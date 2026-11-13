//! Trace milestones for every direct or code-mode tool call handled by core.
//! These events contain call identifiers and names, never arguments or output.
//! Code-mode calls reach the broker before the dispatching turn is known.

use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::ThreadId;
use codex_tools::ToolName;

#[derive(Clone, Copy)]
pub(crate) enum Source {
    Direct,
    CodeMode,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::CodeMode => "code_mode",
        }
    }
}

pub(crate) enum Receipt<'a> {
    ModelTurn(&'a str),
    CodeModeBroker {
        cell_id: Option<&'a str>,
        runtime_tool_call_id: Option<&'a str>,
    },
}

pub(crate) fn received(
    thread_id: ThreadId,
    tool_name: &ToolName,
    call_id: &str,
    receipt: Receipt<'_>,
) {
    let (turn_id, source, cell_id, runtime_tool_call_id) = match receipt {
        Receipt::ModelTurn(turn_id) => (Some(turn_id), Source::Direct, None, None),
        Receipt::CodeModeBroker {
            cell_id,
            runtime_tool_call_id,
        } => (None, Source::CodeMode, cell_id, runtime_tool_call_id),
    };
    tracing::event!(
        name: "codex.tool_call_received",
        target: "codex_otel.trace_safe",
        tracing::Level::INFO,
        event.name = "codex.tool_call_received",
        conversation.id = %thread_id,
        turn_id,
        call_id,
        tool_name = tool_name.name.as_str(),
        tool_namespace = namespace(tool_name),
        tool_source = source.as_str(),
        cell.id = cell_id,
        runtime_tool_call_id,
    );
    emit_lifecycle(LifecycleMetadata {
        event: "received",
        thread_id: &thread_id.to_string(),
        turn_id,
        call_id,
        tool_namespace: namespace(tool_name),
        tool_name: tool_name.name.as_str(),
        source: source.as_str(),
        cell_id,
        runtime_tool_call_id,
    });
}

pub(crate) fn result_ready(
    thread_id: ThreadId,
    turn_id: &str,
    tool_name: &ToolName,
    call_id: &str,
    source: Source,
) {
    tracing::event!(
        name: "codex.tool_result_ready",
        target: "codex_otel.trace_safe",
        tracing::Level::INFO,
        event.name = "codex.tool_result_ready",
        conversation.id = %thread_id,
        turn_id,
        call_id,
        tool_name = tool_name.name.as_str(),
        tool_namespace = namespace(tool_name),
        tool_source = source.as_str(),
    );
    emit_lifecycle(LifecycleMetadata {
        event: "result_ready",
        thread_id: &thread_id.to_string(),
        turn_id: Some(turn_id),
        call_id,
        tool_namespace: namespace(tool_name),
        tool_name: tool_name.name.as_str(),
        source: source.as_str(),
        cell_id: None,
        runtime_tool_call_id: None,
    });
}

fn namespace(tool_name: &ToolName) -> &str {
    tool_name
        .namespace
        .as_deref()
        .filter(|namespace| !namespace.is_empty())
        .unwrap_or(DEFAULT_FUNCTION_NAMESPACE)
}

// This budget belongs to this OS process, not a native session or thread. It is
// never reset by a new turn. Existing trace_safe events and execution are not
// subject to this diagnostic budget.
const LIFECYCLE_MAX_RECORD_BYTES: usize = 4096;
const LIFECYCLE_MAX_RECORDS: usize = 256;
const LIFECYCLE_MAX_BYTES: usize = 256 * 1024;
const LIFECYCLE_SUMMARY_RESERVE: usize = 256;

struct LifecycleMetadata<'a> {
    event: &'static str,
    thread_id: &'a str,
    turn_id: Option<&'a str>,
    call_id: &'a str,
    tool_namespace: &'a str,
    tool_name: &'a str,
    source: &'static str,
    cell_id: Option<&'a str>,
    runtime_tool_call_id: Option<&'a str>,
}

// Hashes are bounded prefix fingerprints, NOT anonymization or identity proof.
// An omitted field has no textual value, so a fingerprint cannot impersonate a
// fully recorded identifier. Missing and invalid/oversized fields stay distinct.
fn lifecycle_field(value: Option<&str>, max_bytes: usize) -> serde_json::Value {
    use sha1::Digest;
    let Some(value) = value else {
        return serde_json::json!({"value": null, "omitted": "missing"});
    };
    let reason = if value.len() > max_bytes {
        Some("oversized")
    } else if value.is_empty()
        || value
            .chars()
            .any(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/' | '@')))
    {
        Some("invalid")
    } else {
        None
    };
    match reason {
        None => serde_json::json!({"value": value, "omitted": null}),
        Some(reason) => {
            let prefix = &value.as_bytes()[..value.len().min(1024)];
            let hash = format!("{:x}", sha1::Sha1::digest(prefix));
            serde_json::json!({
                "value": null, "omitted": reason, "original_bytes": value.len(),
                "fingerprint_sha1": hash, "fingerprint_scope": "first_1024_bytes_max",
            })
        }
    }
}

fn lifecycle_record(metadata: LifecycleMetadata<'_>) -> serde_json::Value {
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    serde_json::json!({
        "schema_version": 1, "scope": "process", "event": metadata.event,
        "timestamp_unix_ms": timestamp_ms,
        "thread_id": lifecycle_field(Some(metadata.thread_id), 64),
        "turn_id": lifecycle_field(metadata.turn_id, 128),
        "call_id": lifecycle_field(Some(metadata.call_id), 128),
        "tool_namespace": lifecycle_field(Some(metadata.tool_namespace), 128),
        "tool_name": lifecycle_field(Some(metadata.tool_name), 128),
        "source": metadata.source,
        "cell_id": lifecycle_field(metadata.cell_id, 128),
        "runtime_tool_call_id": lifecycle_field(metadata.runtime_tool_call_id, 128),
    })
}

#[derive(Default)]
struct LifecycleBudget {
    records: usize,
    bytes: usize,
    limited: bool,
}

impl LifecycleBudget {
    // A bounded summary slot/bytes are reserved before normal admissions. Once
    // limited, no later event heals the incomplete trace or resets the budget.
    fn admit(&mut self, mut record: serde_json::Value) -> Option<String> {
        if self.limited {
            return None;
        }
        record["sequence"] = serde_json::json!(self.records + 1);
        let encoded = serde_json::to_string(&record).ok()?;
        let reason = if encoded.len() > LIFECYCLE_MAX_RECORD_BYTES {
            Some("record_bytes")
        } else if self.records + 1 >= LIFECYCLE_MAX_RECORDS {
            Some("record_count")
        } else if encoded.len() > LIFECYCLE_MAX_BYTES - LIFECYCLE_SUMMARY_RESERVE - self.bytes {
            Some("total_bytes")
        } else {
            None
        };
        let encoded = match reason {
            None => encoded,
            Some(reason) => {
                self.limited = true;
                serde_json::to_string(&serde_json::json!({
                    "schema_version": 1, "scope": "process", "event": "trace_limit",
                    "sequence": self.records + 1, "reason": reason,
                    "records_before_limit": self.records, "trace_incomplete": true,
                }))
                .ok()?
            }
        };
        self.records += 1;
        self.bytes += encoded.len();
        Some(encoded)
    }
}

fn emit_lifecycle(metadata: LifecycleMetadata<'_>) {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    // No formatting/fingerprinting or accounting when the dedicated target is off.
    if !tracing::enabled!(target: "codex_tool_lifecycle", tracing::Level::INFO) {
        return;
    }
    static BUDGET: OnceLock<Mutex<LifecycleBudget>> = OnceLock::new();
    let record = lifecycle_record(metadata);
    let Some(encoded) = BUDGET
        .get_or_init(|| Mutex::new(LifecycleBudget::default()))
        .lock()
        .ok()
        .and_then(|mut budget| budget.admit(record))
    else {
        return;
    };
    // parent=None prevents arbitrary ambient span fields from being collected.
    // The payload is structured/escaped JSON, with no arguments/output/errors.
    tracing::event!(
        target: "codex_tool_lifecycle", parent: None, tracing::Level::INFO,
        event.name = "codex.tool_lifecycle", metadata = %encoded,
    );
}

#[cfg(test)]
#[path = "call_trace_lifecycle_tests.rs"]
mod lifecycle_tests;
