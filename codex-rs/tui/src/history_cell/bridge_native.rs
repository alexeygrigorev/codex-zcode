//! Local UI for already-executed native returns. Never an execution request.
use super::*;
use codex_app_server_protocol::BridgeToolActivityNotification;
use codex_app_server_protocol::BridgeToolActivityStatus;
use codex_protocol::protocol::NativeToolObservation;
use codex_protocol::protocol::NativeToolResult;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

const MAX_CELLS: usize = 128;
const MAX_ID: usize = 256;
const MAX_TEXT: usize = 8192;
const MAX_BYTES: usize = 262144;

/// Widget-lifetime loss/tombstones; a new turn cannot heal a lost observation.
#[derive(Default)]
pub(crate) struct BridgeNativeHistory {
    seen: HashSet<(String, String, String, String)>,
    bytes: usize,
    invalid: Arc<AtomicBool>,
    fault_rendered: bool,
}

impl std::fmt::Debug for BridgeNativeHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeNativeHistory(<redacted>)")
    }
}

fn identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID && value.chars().all(|c| !unsafe_control(c))
}

fn unsafe_control(c: char) -> bool {
    // Conservative union of Unicode format/default-ignorable ranges. Zero-width
    // characters (including combining marks) are also escaped, never silently
    // normalized. Ordinary visible Unicode such as CJK remains literal.
    c.is_control()
        || unicode_width::UnicodeWidthChar::width(c).is_none_or(|width| width == 0)
        || matches!(c,
            '\u{00ad}' | '\u{034f}' | '\u{0600}'..='\u{0605}' | '\u{061c}' |
            '\u{06dd}' | '\u{070f}' | '\u{0890}'..='\u{0891}' | '\u{08e2}' |
            '\u{115f}'..='\u{1160}' | '\u{17b4}'..='\u{17b5}' | '\u{180b}'..='\u{180f}' |
            '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' |
            '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{feff}' | '\u{ffa0}' |
            '\u{fff0}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' |
            '\u{13430}'..='\u{13455}' | '\u{1bca0}'..='\u{1bca3}' |
            '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e0fff}')
}

/// No ANSI/OSC, Markdown parsing, or silent conversion into exact native text.
fn literal(value: &str, multiline: bool) -> (String, bool) {
    let mut text = String::new();
    let mut changed = false;
    for c in value.chars() {
        let next = if unsafe_control(c) && !(multiline && c == '\n') {
            changed = true;
            c.escape_default().to_string()
        } else {
            c.to_string()
        };
        if text.len() + next.len() > MAX_TEXT {
            changed = true;
            break;
        }
        text.push_str(&next);
    }
    (text, changed)
}

fn explicit_terminal(result: &NativeToolResult) -> bool {
    result.native_event == "tool_call_result"
        && result.native_success == Some(true)
        && result.exit_code == Some(0)
        && result.status.as_deref() == Some("completed")
        && result.timed_out == Some(false)
        && result.native_truncated == Some(false)
        && !result.local_truncated
        && result.observation_complete
        && result.native_turn_id.as_deref().is_some_and(identity)
        && result.native_returned_bytes == Some(result.content.len() as u64)
        && result.native_original_bytes == result.native_returned_bytes
}

impl BridgeNativeHistory {
    fn fault(&mut self) -> Option<BridgeNativeCell> {
        self.invalid.store(true, Ordering::Release);
        if self.fault_rendered {
            return None;
        }
        self.fault_rendered = true;
        Some(BridgeNativeCell {
            invalid: Arc::clone(&self.invalid),
            value: None,
        })
    }

    pub(crate) fn observe(
        &mut self,
        thread: Option<&str>,
        turn: Option<&str>,
        event: &BridgeToolActivityNotification,
    ) -> Option<BridgeNativeCell> {
        // Foreign events neither expose text nor poison this widget's stream.
        if thread != Some(event.thread_id.as_str()) {
            return None;
        }
        match event.native_result.as_ref()? {
            NativeToolObservation::Invalidated { .. } => {
                let retained = self.seen.iter().any(|(thread, turn, _, _)| {
                    thread == &event.thread_id && turn == &event.turn_id
                });
                if turn == Some(event.turn_id.as_str()) || retained {
                    self.fault()
                } else {
                    None
                }
            }
            NativeToolObservation::Result(result) => {
                if turn != Some(event.turn_id.as_str()) {
                    let retained = self.seen.iter().any(|(thread, turn, _, _)| {
                        thread == &event.thread_id && turn == &event.turn_id
                    });
                    // Late final loss still applies to retained own turns. Never
                    // render a stale return or let advancing the UI heal loss.
                    if retained && (result.local_truncated || !result.observation_complete) {
                        return self.fault();
                    }
                    return None;
                }
                if self.invalid.load(Ordering::Acquire) {
                    return None;
                }
                if event.status == BridgeToolActivityStatus::Started
                    || result.call_id != event.call_id
                    || result.tool != event.tool
                    || result.tool != "Bash"
                    || ![
                        event.thread_id.as_str(),
                        event.turn_id.as_str(),
                        result.native_session_id.as_str(),
                        result.call_id.as_str(),
                    ]
                    .into_iter()
                    .all(identity)
                    || result
                        .native_turn_id
                        .as_deref()
                        .is_some_and(|id| !identity(id))
                    || result.command.is_empty()
                    || result.command.len() > MAX_TEXT
                {
                    return self.fault();
                }
                let key = (
                    event.thread_id.clone(),
                    event.turn_id.clone(),
                    result.native_session_id.clone(),
                    result.call_id.clone(),
                );
                if self.seen.contains(&key) || self.seen.len() >= MAX_CELLS {
                    return self.fault();
                }
                let (command, command_changed) = literal(&result.command, /*multiline*/ false);
                let (content, content_changed) = literal(&result.content, /*multiline*/ true);
                let size = command.len()
                    + content.len()
                    + result.native_session_id.len()
                    + result.native_turn_id.as_ref().map_or(0, String::len)
                    + result.call_id.len()
                    + event.thread_id.len()
                    + event.turn_id.len();
                if size > MAX_BYTES.saturating_sub(self.bytes) {
                    return self.fault();
                }
                self.bytes += size;
                self.seen.insert(key);
                // A final lossy Result may never be followed by Invalidated.
                // Preserve its useful text, but immediately unqualify all cells.
                if result.local_truncated || !result.observation_complete {
                    self.invalid.store(true, Ordering::Release);
                }
                // Only closed metadata labels are rendered, never free-form errors/status.
                let status = match result.status.as_deref() {
                    Some(
                        "completed" | "failed" | "timed_out" | "cancelled" | "running"
                        | "backgrounded",
                    ) => result.status.clone().unwrap_or_default(),
                    _ => "unknown".to_owned(),
                };
                Some(BridgeNativeCell {
                    invalid: Arc::clone(&self.invalid),
                    value: Some(RenderedReturn {
                        thread: event.thread_id.clone(),
                        turn: event.turn_id.clone(),
                        session: result.native_session_id.clone(),
                        native_turn: result.native_turn_id.clone(),
                        call: result.call_id.clone(),
                        command,
                        content,
                        status,
                        success: result.native_success,
                        exit: result.exit_code,
                        timeout: result.timed_out,
                        terminal: event.status == BridgeToolActivityStatus::Completed
                            && explicit_terminal(result)
                            && !command_changed
                            && !content_changed,
                        native_truncated: result.native_truncated,
                        local_truncated: result.local_truncated,
                        observation_complete: result.observation_complete,
                        original_bytes: result.native_original_bytes,
                        returned_bytes: result.native_returned_bytes,
                        changed: command_changed || content_changed,
                    }),
                })
            }
        }
    }
}

struct RenderedReturn {
    thread: String,
    turn: String,
    session: String,
    native_turn: Option<String>,
    call: String,
    command: String,
    content: String,
    status: String,
    success: Option<bool>,
    exit: Option<i32>,
    timeout: Option<bool>,
    terminal: bool,
    native_truncated: Option<bool>,
    local_truncated: bool,
    observation_complete: bool,
    original_bytes: Option<u64>,
    returned_bytes: Option<u64>,
    changed: bool,
}

pub(crate) struct BridgeNativeCell {
    invalid: Arc<AtomicBool>,
    value: Option<RenderedReturn>,
}

impl std::fmt::Debug for BridgeNativeCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeNativeCell(<redacted>)")
    }
}

impl BridgeNativeCell {
    fn lines(&self) -> Vec<Line<'static>> {
        let Some(value) = &self.value else {
            return vec![Line::from(
                "External native observation: UNQUALIFIED (stream loss, duplicate or limit)"
                    .yellow(),
            )];
        };
        let terminal = value.terminal && !self.invalid.load(Ordering::Acquire);
        let label = if terminal {
            "matched terminal return"
        } else {
            "UNQUALIFIED return"
        };
        let boolean = |v: Option<bool>| v.map_or_else(|| "unknown".to_owned(), |v| v.to_string());
        let mut lines = vec![
            Line::from(format!("External native Bash: {label}").bold()),
            Line::from(format!("  Thread: {} / turn: {}", value.thread, value.turn).dim()),
            Line::from(
                format!(
                    "  Native session: {} / turn: {}",
                    value.session,
                    value.native_turn.as_deref().unwrap_or("unknown")
                )
                .dim(),
            ),
            Line::from(format!("  Call: {}", value.call).dim()),
            Line::from(format!("  Command: {}", value.command)),
            Line::from(format!(
                "  Status: {} / exit: {} / success: {} / timeout: {}",
                value.status,
                value
                    .exit
                    .map_or_else(|| "unknown".to_owned(), |v| v.to_string()),
                boolean(value.success),
                boolean(value.timeout)
            )),
            Line::from(format!(
                "  Truncation: native={} local={} / observation_complete={}",
                boolean(value.native_truncated),
                value.local_truncated,
                value.observation_complete
            )),
            Line::from(format!(
                "  Serialized bytes: original={} returned={}",
                value
                    .original_bytes
                    .map_or_else(|| "unknown".to_owned(), |v| v.to_string()),
                value
                    .returned_bytes
                    .map_or_else(|| "unknown".to_owned(), |v| v.to_string())
            )),
            Line::from("  Native returned text (not reconstructed stdout):".dim()),
        ];
        lines.extend(
            value
                .content
                .split('\n')
                .map(|line| Line::from(format!("    | {line}"))),
        );
        if value.changed {
            lines.push(Line::from(
                "  Display escaped or truncated; exact native text proof unavailable".yellow(),
            ));
        }
        lines
    }
}

impl HistoryCell for BridgeNativeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 { Vec::new() } else { self.lines() }
    }
    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.lines()
    }
}

#[cfg(test)]
#[path = "bridge_native_tests.rs"]
mod tests;
