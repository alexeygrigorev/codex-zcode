//! Session establishment for the warm bridge: resume the previous session
//! after an app-server respawn, or create fresh.
//!
//! The client caches one warm bridge per Codex thread and replaces it when
//! the app-server child dies. Replacing the bridge used to abandon the
//! session with it, so every respawn paid a full transcript re-send and lost
//! the provider cache the warm bridge exists to preserve. The core's
//! `session/resume` rebuilds a session in a fresh core process (its schema
//! notes cold resume must re-pin the create-time constraints), so the
//! replacement bridge picks the previous session back up instead (issue #42).
//!
//! Today's `session/create` pins only the workspace, so resume mirrors it
//! with exactly that. The resume schema carries no `mode` member, so gating
//! is not re-asserted either. Subscribing stays live-only on purpose: the
//! interrupted turn's collector is gone by the time the replacement bridge
//! handshakes, so replaying queued events after the last-seen seq could only
//! deliver a stale `turn.completed` into the retry turn.

use tracing::warn;

use crate::zcode_warm::WarmMode;
use crate::zcode_warm::ZcodeWarmBridge;

/// Establishes the bridge's per-thread session: resume `resume_session_id`
/// when the bridge replaced one whose child died, otherwise create; any
/// resume failure falls back to create so a lost session degrades to
/// today's behavior.
pub(crate) async fn establish_session(
    bridge: &ZcodeWarmBridge,
    workspace_path: &str,
    resume_session_id: Option<&str>,
    mode: WarmMode,
) -> Result<String, String> {
    if let Some(session_id) = resume_session_id {
        match resume_session(bridge, session_id, workspace_path).await {
            Ok(()) => return Ok(session_id.to_string()),
            Err(message) => {
                warn!("ZCode warm session resume failed ({message}); creating a fresh session");
            }
        }
    }
    create_session(bridge, workspace_path, mode).await
}

/// Resumes and re-subscribes to a session that outlived the previous child.
async fn resume_session(
    bridge: &ZcodeWarmBridge,
    session_id: &str,
    workspace_path: &str,
) -> Result<(), String> {
    bridge
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

/// Subscribes to the session's live `session/event` stream.
async fn subscribe(bridge: &ZcodeWarmBridge, session_id: &str) -> Result<(), String> {
    bridge
        .request_with_compat_retry(
            "session/subscribe",
            serde_json::json!({
                "sessionId": session_id,
                "deliveryKind": "desktop-continuous",
            }),
        )
        .await
        .map(|_| ())
}
