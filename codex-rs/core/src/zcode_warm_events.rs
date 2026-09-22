//! Mapping of ZCode `session/event` tool-call notifications to the
//! display-only [`BridgeToolActivityEvent`] surfaced through
//! `ResponseEvent::BridgeToolActivity` (issue #38).
//!
//! Tool calls execute inside the warm core's own agent loop, so they must
//! never be re-emitted as `ResponseItem::FunctionCall` — the ToolCallRuntime
//! would execute them again. These events only carry live status for the UI.

use std::collections::HashMap;

use codex_protocol::protocol::BridgeToolActivityEvent;
use codex_protocol::protocol::BridgeToolActivityStatus;

/// Detail previews are display-only; cap them so a chatty tool output or a
/// large tool input cannot flood the event stream.
const DETAIL_PREVIEW_CAP: usize = 200;

/// Extracts the bridge tool activity carried by one `session/event`
/// notification, or `None` when it is not a mapped tool-call event.
///
/// Only `tool_call_scheduled` reliably names the tool, so [`HashMap`]
/// `tool_names` remembers each call's name for the later result/error
/// events, which may omit `toolName`.
pub(crate) fn bridge_tool_activity_from_notification(
    notification: &serde_json::Value,
    tool_names: &mut HashMap<String, String>,
) -> Option<BridgeToolActivityEvent> {
    let event_type = notification
        .get("type")
        .and_then(serde_json::Value::as_str)?;
    let payload = notification.get("payload")?;
    let call_id = payload
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    if let Some(tool) = payload.get("toolName").and_then(serde_json::Value::as_str) {
        tool_names.insert(call_id.clone(), tool.to_string());
    }
    let tool = payload
        .get("toolName")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| tool_names.get(&call_id).cloned())
        .unwrap_or_else(|| "unknown".to_string());
    let (status, detail) = match event_type {
        "tool_call_scheduled" => (
            BridgeToolActivityStatus::Started,
            payload
                .get("input")
                .and_then(|input| serde_json::to_string(input).ok())
                .map(|input| bounded_preview(&input))
                .filter(|detail| !detail.is_empty()),
        ),
        "tool_call_result" => {
            let success = payload
                .pointer("/result/success")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let content = payload
                .pointer("/result/content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let detail = if content.is_empty() {
                payload
                    .pointer("/result/error/message")
                    .and_then(serde_json::Value::as_str)
                    .map(bounded_preview)
            } else {
                Some(bounded_preview(content))
            };
            if success {
                (BridgeToolActivityStatus::Completed, detail)
            } else {
                (BridgeToolActivityStatus::Failed, detail)
            }
        }
        "tool_call_error" => (
            BridgeToolActivityStatus::Failed,
            payload
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(bounded_preview),
        ),
        _ => return None,
    };
    Some(BridgeToolActivityEvent {
        call_id,
        tool,
        status,
        detail,
    })
}

fn bounded_preview(text: &str) -> String {
    text.chars().take(DETAIL_PREVIEW_CAP).collect()
}

#[cfg(test)]
#[path = "zcode_warm_events_tests.rs"]
mod tests;
