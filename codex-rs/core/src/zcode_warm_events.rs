//! Mapping of ZCode `session/event` tool-call notifications to the
//! observational [`BridgeToolActivityEvent`] surfaced through
//! `ResponseEvent::BridgeToolActivity` (issue #38).
//!
//! Tool calls execute inside the warm core's own agent loop, so they must
//! never be re-emitted as `ResponseItem::FunctionCall` — the ToolCallRuntime
//! would execute them again. Display previews remain separate from optional
//! qualified native result metadata.

use std::collections::HashMap;

use codex_protocol::protocol::BridgeNativeToolResult;
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
        native_result: None,
    })
}

/// One live collector's bounded qualification ledger, separate from UI previews.
/// Missing turn/status metadata, orphan results and replayed call ids provide no
/// native evidence. Overflow disables evidence, never tool execution or display.
pub(crate) struct NativeResultTracker {
    session_id: String,
    turn_id: Option<String>,
    calls: HashMap<String, Option<String>>,
    closed: bool,
}

const NATIVE_OUTPUT_LIMIT: usize = 16 * 1024;
const NATIVE_CALL_LIMIT: usize = 128;

impl NativeResultTracker {
    pub(crate) fn new(session_id: String) -> Self {
        Self {
            session_id,
            turn_id: None,
            calls: HashMap::new(),
            closed: false,
        }
    }

    pub(crate) fn observe(
        &mut self,
        notification: &serde_json::Value,
    ) -> Option<BridgeNativeToolResult> {
        if self.closed || notification.get("sessionId").and_then(native_id)? != self.session_id {
            return None;
        }
        let kind = notification.get("type")?.as_str()?;
        if kind == "turn.started" {
            if self.turn_id.is_some() {
                self.closed = true;
            } else if let Some(turn) = notification.get("turnId").and_then(native_id) {
                self.turn_id = Some(turn.to_string());
            } else {
                self.closed = true;
            }
            return None;
        }
        let turn = notification.get("turnId").and_then(native_id)?;
        if Some(turn) != self.turn_id.as_deref() {
            return None;
        }
        if matches!(kind, "turn.completed" | "turn.failed") {
            self.closed = true;
            return None;
        }
        let payload = notification.get("payload")?;
        let call = payload.get("toolCallId").and_then(native_id)?;
        if kind == "tool_call_scheduled" {
            if self.calls.contains_key(call) {
                self.calls.insert(call.to_string(), None);
            } else if self.calls.len() >= NATIVE_CALL_LIMIT {
                self.closed = true;
            } else if let Some(tool) = payload.get("toolName").and_then(native_id) {
                self.calls.insert(call.to_string(), Some(tool.to_string()));
            }
            return None;
        }
        if !matches!(kind, "tool_call_result" | "tool_call_error") {
            return None;
        }
        // Consume before parsing: malformed terminal results cannot later heal.
        let tool = self.calls.get_mut(call)?.take()?;
        if payload
            .get("toolName")
            .is_some_and(|name| name.as_str() != Some(tool.as_str()))
        {
            return None;
        }
        let (success, content, metadata) = if kind == "tool_call_result" {
            let result = payload.get("result")?;
            (
                result.get("success")?.as_bool()?,
                result.get("content")?.as_str()?,
                Some(result),
            )
        } else {
            (false, payload.pointer("/error/message")?.as_str()?, None)
        };
        let observed_bytes = u32::try_from(content.len()).ok()?;
        let source_truncated = match metadata.and_then(|value| value.get("truncated")) {
            Some(value) => Some(value.as_bool()?),
            None => None,
        };
        let source_original_bytes = optional_count(metadata, "originalBytes")?;
        let source_returned_bytes = optional_count(metadata, "returnedBytes")?;
        if source_returned_bytes.is_some_and(|count| count != observed_bytes)
            || source_original_bytes.is_some_and(|count| count < observed_bytes)
            || source_truncated == Some(false)
                && source_original_bytes.is_some_and(|count| count != observed_bytes)
        {
            return None;
        }
        let command = metadata
            .and_then(|result| result.pointer("/perf/detail"))
            .filter(|detail| {
                detail.get("kind").and_then(serde_json::Value::as_str) == Some("command")
            })
            .and_then(|detail| detail.get("command"));
        let exit_code = match command.and_then(|value| value.get("exitCode")) {
            Some(value) => Some(i32::try_from(value.as_i64()?).ok()?),
            None => None,
        };
        let command_status = match command.and_then(|value| value.get("status")) {
            Some(value) => {
                let status = value.as_str()?;
                if !matches!(
                    status,
                    "completed" | "failed" | "timed_out" | "cancelled" | "spawn_error"
                ) {
                    return None;
                }
                Some(status.to_string())
            }
            None => None,
        };
        let mut end = content.len().min(NATIVE_OUTPUT_LIMIT);
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        Some(BridgeNativeToolResult {
            native_session_id: self.session_id.clone(),
            native_turn_id: turn.to_string(),
            native_success: success,
            source_event: kind.to_string(),
            exit_code,
            command_status,
            output: content[..end].to_string(),
            observed_bytes,
            output_bytes: end as u32,
            output_truncated: end != content.len(),
            source_truncated,
            source_original_bytes,
            source_returned_bytes,
        })
    }
}

fn native_id(value: &serde_json::Value) -> Option<&str> {
    value.as_str().filter(|value| {
        !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
    })
}

fn optional_count(value: Option<&serde_json::Value>, key: &str) -> Option<Option<u32>> {
    match value.and_then(|value| value.get(key)) {
        Some(value) => Some(Some(u32::try_from(value.as_u64()?).ok()?)),
        None => Some(None),
    }
}

fn bounded_preview(text: &str) -> String {
    text.chars().take(DETAIL_PREVIEW_CAP).collect()
}

#[cfg(test)]
#[path = "zcode_warm_events_tests.rs"]
mod tests;
