//! Turn-input channel for the warm bridge (issue #44).
//!
//! `session/send` is the legacy ZCode Protocol v1 input surface; the v4
//! transport converges mutations into `v4/command`, whose `sendText` kind
//! feeds the same core admission and turn runner and projects the same
//! legacy `session/event` notifications for legacy-created sessions. The
//! vendored zcode.cjs still ships both, so the default stays on the legacy
//! surface; `ZCODE_WARM_V4_SEND=1` opts the bridge's turns into
//! `v4/command sendText` so the removal of `session/send` is a flag flip,
//! not a scramble.
//!
//! The v4 path is soft: any pre-turn failure (older cores without a v4
//! gateway answer method-not-found, schema drift, rejection) falls back to
//! `session/send`. This is deliberately wider than the desktop host, which
//! treats a busy foreground turn as `startNow`-preemptible: the bridge runs
//! one turn at a time and mirrors the legacy rejection semantics through
//! the stream-retry ladder instead.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tracing::warn;

use crate::zcode_warm::ZcodeWarmBridge;

/// Which wire surface carries a turn's user input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnInputChannel {
    /// Legacy `session/send` — today's default.
    LegacySend,
    /// `v4/command sendText` prototype, falling back to legacy on failure.
    V4Command,
}

/// Resolves [`TurnInputChannel`] from the process environment
/// (`ZCODE_WARM_V4_SEND`); unknown values stay on the legacy surface.
pub(crate) fn channel_from_env() -> TurnInputChannel {
    channel_from_value(std::env::var("ZCODE_WARM_V4_SEND").ok().as_deref())
}

/// Pure core of [`channel_from_env`] so tests avoid mutating the process
/// environment.
fn channel_from_value(value: Option<&str>) -> TurnInputChannel {
    match value.map(str::trim) {
        Some("1" | "true" | "yes") => TurnInputChannel::V4Command,
        _ => TurnInputChannel::LegacySend,
    }
}

/// Submits one turn's input on `channel` and verifies the acceptance
/// verdict. The turn's events arrive on the shared session subscription
/// either way.
pub(crate) async fn send_turn_input(
    bridge: &ZcodeWarmBridge,
    session_id: &str,
    content: &str,
    channel: TurnInputChannel,
) -> Result<(), String> {
    match channel {
        TurnInputChannel::LegacySend => legacy_send(bridge, session_id, content).await,
        TurnInputChannel::V4Command => match v4_command_send(bridge, session_id, content).await {
            Ok(()) => Ok(()),
            Err(message) => {
                warn!("v4/command sendText failed ({message}); falling back to session/send");
                legacy_send(bridge, session_id, content).await
            }
        },
    }
}

async fn legacy_send(
    bridge: &ZcodeWarmBridge,
    session_id: &str,
    content: &str,
) -> Result<(), String> {
    let sent = bridge
        .request_with_compat_retry(
            "session/send",
            serde_json::json!({ "sessionId": session_id, "content": content }),
        )
        .await?;
    if sent.get("accepted") != Some(&serde_json::Value::Bool(true)) {
        return Err(format!(
            "session/send was not accepted: {}",
            serde_json::to_string(&sent).unwrap_or_default()
        ));
    }
    Ok(())
}

async fn v4_command_send(
    bridge: &ZcodeWarmBridge,
    session_id: &str,
    content: &str,
) -> Result<(), String> {
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_millis() as u64)
        .unwrap_or_default();
    let command_id = uuid::Uuid::new_v4().simple().to_string();
    let ack = bridge
        .request_with_compat_retry(
            "v4/command",
            serde_json::json!({
                "commandId": command_id,
                "clientId": "codex-zcode-warm",
                "sessionId": session_id,
                "type": "sendText",
                "payload": { "text": content, "requestedDelivery": "startNow" },
                "issuedAt": issued_at,
            }),
        )
        .await?;
    if ack.get("status").and_then(serde_json::Value::as_str) != Some("accepted") {
        return Err(format!(
            "v4/command was not accepted: {}",
            serde_json::to_string(&ack).unwrap_or_default()
        ));
    }
    if let Some(delivery) = ack
        .pointer("/result/delivery")
        .and_then(serde_json::Value::as_str)
        && delivery != "startNow" {
            // The input was admitted into a queue rather than a turn; the
            // events for it may not arrive within this turn's idle window.
            warn!("v4/command sendText was queued rather than started (delivery={delivery})");
        }
    Ok(())
}

#[cfg(test)]
#[path = "zcode_warm_v4_send_tests.rs"]
mod tests;
