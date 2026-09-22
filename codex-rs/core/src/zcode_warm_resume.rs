//! Session establishment for the warm bridge: pick the previous session
//! back up, or create fresh.
//!
//! The client caches one warm bridge per Codex thread and replaces it when
//! the app-server child dies. Replacing the bridge used to abandon the
//! session with it, so every respawn paid a full transcript re-send and lost
//! the provider cache the warm bridge exists to preserve. The core's
//! `session/resume` rebuilds a session in a fresh core process (its schema
//! notes cold resume must re-pin the create-time constraints), so the
//! replacement bridge picks the previous session back up instead (issue #42).
//!
//! A full zcodex restart drops the in-memory handoff, so a restart would
//! still have paid the re-send: `zcode_warm_store` persists the thread's
//! session id, and the [`SessionSeed::Recorded`] path confirms it still
//! exists server-side via `session/list` (identity queries read persisted
//! sessions) before resuming it.
//!
//! Today's `session/create` pins only the workspace, so resume mirrors it
//! with exactly that. The resume schema carries no `mode` member, so gating
//! is not re-asserted either. Subscribing stays live-only on purpose: the
//! core returns an empty replay gap unless `afterSeq` is passed, and no
//! collector is waiting during the handshake — replaying queued events
//! could only deliver a stale `turn.completed` into a retry turn.

use tracing::warn;

use crate::zcode_warm::SessionSeed;
use crate::zcode_warm::WarmMode;
use crate::zcode_warm::ZcodeWarmBridge;

/// Establishes the bridge's per-thread session: resume the seeded session
/// when there is one, otherwise create; any pick-up failure falls back to
/// create so a lost session degrades to today's behavior.
pub(crate) async fn establish_session(
    bridge: &ZcodeWarmBridge,
    workspace_path: &str,
    seed: &SessionSeed,
    mode: WarmMode,
) -> Result<String, String> {
    let resume_session_id = match seed {
        SessionSeed::Fresh => None,
        SessionSeed::Predecessor(session_id) => Some(session_id.clone()),
        SessionSeed::Recorded(record) => {
            if record.workspace_path != workspace_path {
                warn!(
                    "ZCode warm session record points at {} (now {workspace_path}); creating a \
                     fresh session",
                    record.workspace_path
                );
                None
            } else if listed_session_exists(bridge, &record.zcode_session_id, workspace_path).await
            {
                Some(record.zcode_session_id.clone())
            } else {
                warn!(
                    "ZCode warm session record {} no longer exists server-side; creating a \
                     fresh session",
                    record.zcode_session_id
                );
                None
            }
        }
    };
    if let Some(session_id) = resume_session_id {
        match resume_session(bridge, &session_id, workspace_path).await {
            Ok(()) => return Ok(session_id),
            Err(message) => {
                warn!("ZCode warm session resume failed ({message}); creating a fresh session");
            }
        }
    }
    create_session(bridge, workspace_path, mode).await
}

/// Whether `session/list` still knows the recorded session in this
/// workspace. A list failure counts as "not found": the fallback create is
/// cheaper than probing why the query failed.
async fn listed_session_exists(
    bridge: &ZcodeWarmBridge,
    session_id: &str,
    workspace_path: &str,
) -> bool {
    bridge
        .request_with_compat_retry(
            "session/list",
            serde_json::json!({
                "sessionIds": [session_id],
                "workspace": {
                    "workspacePath": workspace_path,
                    "workspaceKey": workspace_path,
                },
            }),
        )
        .await
        .is_ok_and(|result| {
            result
                .pointer("/sessions/0/sessionId")
                .and_then(serde_json::Value::as_str)
                == Some(session_id)
        })
}

/// Resumes and re-subscribes to a session that outlived the previous child.
async fn resume_session(
    bridge: &ZcodeWarmBridge,
    session_id: &str,
    workspace_path: &str,
) -> Result<(), String> {
    let resumed = bridge
        .request_with_compat_retry(
            "session/resume",
            serde_json::json!({
                "sessionId": session_id,
                "workspace": {
                    "workspacePath": workspace_path,
                    "workspaceKey": workspace_path,
                },
            }),
        )
        .await?;
    observe_protocol_version(bridge, &resumed);
    subscribe(bridge, session_id).await
}

/// Creates the fresh-session fallback and subscribes to it.
async fn create_session(
    bridge: &ZcodeWarmBridge,
    workspace_path: &str,
    mode: WarmMode,
) -> Result<String, String> {
    let created = bridge
        .request_with_compat_retry(
            "session/create",
            serde_json::json!({
                "workspace": {
                    "workspacePath": workspace_path,
                    "workspaceKey": workspace_path,
                },
                "mode": mode.wire_session_mode(),
                "titleGenerationEnabled": false,
            }),
        )
        .await?;
    observe_protocol_version(bridge, &created);
    let session_id = created
        .pointer("/session/sessionId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "session/create response carried no sessionId: {}",
                serde_json::to_string(&created).unwrap_or_default()
            )
        })?
        .to_string();
    subscribe(bridge, &session_id).await?;
    Ok(session_id)
}

/// Pins the core's reported `protocol.version` onto the bridge so the
/// persisted record can warn when a zcode.cjs update shifts it (issue #44).
fn observe_protocol_version(bridge: &ZcodeWarmBridge, handshake_result: &serde_json::Value) {
    if let Some(version) = handshake_result
        .pointer("/protocol/version")
        .and_then(serde_json::Value::as_u64)
    {
        bridge.observe_protocol_version(version);
    }
}

/// Subscribes to the session's live `session/event` stream.
async fn subscribe(bridge: &ZcodeWarmBridge, session_id: &str) -> Result<(), String> {
    let subscribed = bridge
        .request_with_compat_retry(
            "session/subscribe",
            serde_json::json!({
                "sessionId": session_id,
                "deliveryKind": "desktop-continuous",
            }),
        )
        .await?;
    // The subscribe result pins the stream's current head; fold it into the
    // bridge's seq tracking so the persisted record stays meaningful even
    // for sessions that never streamed an event this process.
    if let Some(seq) = subscribed
        .pointer("/eventSeq")
        .and_then(serde_json::Value::as_u64)
    {
        bridge.observe_event_seq(seq);
    }
    Ok(())
}
