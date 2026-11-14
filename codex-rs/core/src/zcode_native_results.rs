//! Observe already-executed JS-native calls. Never construct a ResponseItem,
//! invoke ToolRouter, read artifacts, or interpret assistant text as execution.
use std::collections::HashMap;
use std::collections::VecDeque;

use codex_protocol::protocol::BridgeToolActivityEvent;
use codex_protocol::protocol::BridgeToolActivityStatus;
use codex_protocol::protocol::NativeToolObservation;
use codex_protocol::protocol::NativeToolResult;
use serde_json::Value;

const MAX_CALLS: usize = 128;
const MAX_ID: usize = 256;
const MAX_COMMAND: usize = 8192;
const MAX_CONTENT: usize = 65536;
const MAX_TOTAL_CONTENT: usize = 262144;

struct Scheduled {
    session: String,
    collector: String,
    turn: Option<String>,
    command: Option<String>,
    finished: bool,
}

/// One bounded collector per actual native stream. Tombstones are never evicted
/// into a fresh state: overflow, replay, orphan, or observed loss is sticky.
#[derive(Default)]
pub(crate) struct NativeResults {
    calls: HashMap<String, Scheduled>,
    names: HashMap<String, String>,
    display_names: HashMap<String, String>,
    display_order: VecDeque<String>,
    invalid: bool,
    content_bytes: usize,
    fault_reported: bool,
}

impl std::fmt::Debug for NativeResults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeResults(<redacted>)")
    }
}

impl NativeResults {
    pub(crate) fn invalidate(&mut self) {
        self.invalid = true;
    }

    pub(crate) fn gap(&mut self) -> Option<BridgeToolActivityEvent> {
        self.invalidate();
        self.fault()
    }

    fn fault(&mut self) -> Option<BridgeToolActivityEvent> {
        if self.fault_reported {
            return None;
        }
        self.fault_reported = true;
        Some(BridgeToolActivityEvent {
            call_id: "native-observer".to_owned(),
            tool: "observer".to_owned(),
            status: BridgeToolActivityStatus::Failed,
            detail: None,
            native_result: Some(NativeToolObservation::Invalidated {
                reason: "native_stream_unqualified".to_owned(),
            }),
        })
    }

    /// Display delivery is independent of sticky proof qualification. A fault
    /// may share the same in-process event as the ordinary UI preview; public
    /// serialization still omits it. No qualified result is recreated after loss.
    pub(crate) fn observe(
        &mut self,
        event: &Value,
        expected_session: &str,
        collector: &str,
    ) -> Option<BridgeToolActivityEvent> {
        let display = self.display(event, expected_session);
        match self.observe_proof(event, expected_session, collector) {
            Some(proof) if proof.native_result.is_some() => {
                if matches!(
                    proof.native_result.as_ref(),
                    Some(NativeToolObservation::Invalidated { .. })
                ) && let Some(mut display) = display
                {
                    display.native_result = proof.native_result;
                    return Some(display);
                }
                Some(proof)
            }
            _ => display,
        }
    }

    fn display(
        &mut self,
        event: &Value,
        expected_session: &str,
    ) -> Option<BridgeToolActivityEvent> {
        if event.get("sessionId").and_then(Value::as_str) != Some(expected_session) {
            return None;
        }
        let kind = match event.get("type")?.as_str()? {
            "tool.updated" => match event.pointer("/payload/kind")?.as_str()? {
                "scheduled" => "tool_call_scheduled",
                "result" => "tool_call_result",
                "error" => "tool_call_error",
                _ => return None,
            },
            "tool_call_scheduled" => "tool_call_scheduled",
            "tool_call_result" => "tool_call_result",
            "tool_call_error" => "tool_call_error",
            _ => return None,
        };
        let id = event.pointer("/payload/toolCallId")?.as_str()?;
        if id.is_empty() || id.len() > MAX_ID {
            return None;
        }
        if let Some(tool) = event.pointer("/payload/toolName").and_then(Value::as_str) {
            if tool.len() > MAX_ID {
                return None;
            }
            if !self.display_names.contains_key(id) {
                if self.display_names.len() == MAX_CALLS
                    && let Some(old) = self.display_order.pop_front()
                {
                    self.display_names.remove(&old);
                }
                self.display_order.push_back(id.to_owned());
            }
            self.display_names.insert(id.to_owned(), tool.to_owned());
        }
        let legacy = preview_notification(event, kind, id);
        crate::zcode_warm_events::bridge_tool_activity_from_notification(
            &legacy,
            &mut self.display_names,
        )
    }

    fn observe_proof(
        &mut self,
        event: &Value,
        expected_session: &str,
        collector: &str,
    ) -> Option<BridgeToolActivityEvent> {
        if self.invalid {
            return self.fault();
        }
        let kind = match event.get("type")?.as_str()? {
            "tool.updated" => match event.pointer("/payload/kind").and_then(Value::as_str) {
                Some("scheduled") => "tool_call_scheduled",
                Some("result") => "tool_call_result",
                Some("error") => "tool_call_error",
                // Documented native activity, not a terminal proof event.
                Some("started" | "progress" | "batch") => return None,
                _ => {
                    self.invalidate();
                    return self.fault();
                }
            },
            "tool_call_scheduled" => "tool_call_scheduled",
            "tool_call_result" => "tool_call_result",
            "tool_call_error" => "tool_call_error",
            _ => return None,
        };
        let session = event.get("sessionId").and_then(Value::as_str);
        if expected_session.is_empty()
            || session != Some(expected_session)
            || expected_session.len() > MAX_ID
        {
            self.invalidate();
            return self.fault();
        }
        let id = event.pointer("/payload/toolCallId").and_then(Value::as_str);
        let Some(id) = id.filter(|id| !id.is_empty() && id.len() <= MAX_ID) else {
            self.invalidate();
            return self.fault();
        };
        let turn = match event.get("turnId") {
            None | Some(Value::Null) => None,
            Some(Value::String(turn)) if !turn.is_empty() && turn.len() <= MAX_ID => {
                Some(turn.clone())
            }
            _ => {
                self.invalidate();
                return self.fault();
            }
        };
        if kind == "tool_call_scheduled" {
            if self.calls.contains_key(id) || self.calls.len() >= MAX_CALLS {
                self.invalidate();
                return self.fault();
            }
            let Some(tool) = event
                .pointer("/payload/toolName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= MAX_ID)
            else {
                self.invalidate();
                return self.fault();
            };
            let command = if tool == "Bash" {
                match event
                    .pointer("/payload/input/command")
                    .and_then(Value::as_str)
                    .filter(|command| !command.is_empty() && command.len() <= MAX_COMMAND)
                {
                    Some(command) => Some(command.to_owned()),
                    None => {
                        self.invalidate();
                        return self.fault();
                    }
                }
            } else {
                None
            };
            self.calls.insert(
                id.to_owned(),
                Scheduled {
                    session: expected_session.to_owned(),
                    collector: collector.to_owned(),
                    turn: turn.clone(),
                    command,
                    finished: false,
                },
            );
        } else {
            let Some(call) = self.calls.get_mut(id) else {
                self.invalidate();
                return self.fault();
            };
            if call.finished
                || call.session != expected_session
                || call.collector != collector
                || call.turn != turn
            {
                self.invalidate();
                return self.fault();
            }
            // A result cannot rename the scheduled tool.
            if event
                .pointer("/payload/toolName")
                .is_some_and(|value| value.as_str() != self.names.get(id).map(String::as_str))
            {
                self.invalidate();
                return self.fault();
            }
            call.finished = true;
        }
        let legacy = preview_notification(event, kind, id);
        let mut activity = crate::zcode_warm_events::bridge_tool_activity_from_notification(
            &legacy,
            &mut self.names,
        )?;
        if kind == "tool_call_scheduled" {
            return Some(activity);
        }
        let call = self.calls.get(id)?;
        let Some(command) = call.command.clone() else {
            return Some(activity);
        };
        let result = event.pointer("/payload/result");
        let content = result
            .and_then(|r| r.get("content"))
            .and_then(Value::as_str);
        // Project native serialized content exactly, never display previews or artifacts.
        let original = content.unwrap_or_default();
        let available = MAX_CONTENT.min(MAX_TOTAL_CONTENT.saturating_sub(self.content_bytes));
        let end = utf8_prefix(original, available);
        let local_truncated = end < original.len();
        self.content_bytes += end;
        if local_truncated {
            self.invalid = true;
        }
        let command_meta = result
            .and_then(|r| r.pointer("/perf/detail"))
            .filter(|detail| detail.get("kind").and_then(Value::as_str) == Some("command"))
            .and_then(|detail| detail.get("command"));
        let status = command_meta
            .and_then(|c| c.get("status"))
            .and_then(Value::as_str)
            .filter(|status| {
                matches!(
                    *status,
                    "completed" | "failed" | "timed_out" | "cancelled" | "running" | "backgrounded"
                )
            })
            .map(str::to_owned);
        let truncated = result
            .and_then(|r| r.get("truncated"))
            .and_then(Value::as_bool);
        let observation_complete = !self.invalid
            && turn.is_some()
            && content.is_some()
            && truncated == Some(false)
            && result
                .and_then(|r| r.get("originalBytes"))
                .and_then(Value::as_u64)
                .is_some()
            && result
                .and_then(|r| r.get("returnedBytes"))
                .and_then(Value::as_u64)
                == Some(original.len() as u64);
        activity.native_result = Some(NativeToolObservation::Result(NativeToolResult {
            native_event: kind.to_owned(),
            native_session_id: expected_session.to_owned(),
            native_turn_id: turn,
            call_id: id.to_owned(),
            tool: activity.tool.clone(),
            command,
            content: original[..end].to_owned(),
            native_success: result
                .and_then(|r| r.get("success"))
                .and_then(Value::as_bool),
            exit_code: command_meta
                .and_then(|c| c.get("exitCode"))
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok()),
            status,
            timed_out: command_meta
                .and_then(|c| c.get("timedOut"))
                .and_then(Value::as_bool),
            native_truncated: truncated,
            native_original_bytes: result
                .and_then(|r| r.get("originalBytes"))
                .and_then(Value::as_u64),
            native_returned_bytes: result
                .and_then(|r| r.get("returnedBytes"))
                .and_then(Value::as_u64),
            local_truncated,
            observation_complete,
        }));
        Some(activity)
    }
}

fn preview_notification(event: &Value, kind: &str, id: &str) -> Value {
    // Do not clone an arbitrarily large result just to make the 200-character UI preview.
    let bounded = |value: Option<&Value>| {
        Value::String(
            value
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
        )
    };
    let mut preview = serde_json::json!({"toolCallId": id});
    if kind == "tool_call_scheduled" {
        preview["toolName"] = event["payload"]["toolName"].clone();
        // Preserve existing preview semantics without retaining inputs in observation state.
        preview["input"] = event["payload"]["input"].clone();
    } else {
        preview["result"] = serde_json::json!({
            "success": event.pointer("/payload/result/success").and_then(Value::as_bool),
            "content": bounded(event.pointer("/payload/result/content")),
            "error": {"message": bounded(event.pointer("/payload/result/error/message"))},
        });
        preview["error"] =
            serde_json::json!({"message": bounded(event.pointer("/payload/error/message"))});
    }
    serde_json::json!({"type":kind,"payload":preview})
}

fn utf8_prefix(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
#[path = "zcode_native_results_tests.rs"]
mod tests;
