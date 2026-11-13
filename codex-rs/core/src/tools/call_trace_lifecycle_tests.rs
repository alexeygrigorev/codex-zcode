use super::*;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;

fn fixture() -> LifecycleMetadata<'static> {
    LifecycleMetadata {
        event: "received",
        thread_id: "11111111-1111-4111-8111-111111111111",
        turn_id: Some("turn-1"),
        call_id: "call-1",
        tool_namespace: "functions",
        tool_name: "fixture_tool",
        source: "direct",
        cell_id: None,
        runtime_tool_call_id: None,
    }
}

#[test]
fn metadata_schema_is_explicit_and_missing_turn_is_not_invented() {
    let mut metadata = fixture();
    metadata.turn_id = None;
    metadata.source = "code_mode";
    metadata.cell_id = Some("cell-1");
    metadata.runtime_tool_call_id = Some("runtime-1");
    let mut record = lifecycle_record(metadata);
    let timestamp = record
        .as_object_mut()
        .unwrap()
        .remove("timestamp_unix_ms")
        .unwrap();
    assert!(timestamp.is_null() || timestamp.as_u64().is_some());
    assert_eq!(
        record,
        json!({
            "schema_version": 1, "scope": "process", "event": "received", "source": "code_mode",
            "thread_id": {"value": "11111111-1111-4111-8111-111111111111", "omitted": null},
            "turn_id": {"value": null, "omitted": "missing"},
            "call_id": {"value": "call-1", "omitted": null},
            "tool_namespace": {"value": "functions", "omitted": null},
            "tool_name": {"value": "fixture_tool", "omitted": null},
            "cell_id": {"value": "cell-1", "omitted": null},
            "runtime_tool_call_id": {"value": "runtime-1", "omitted": null},
        })
    );
    for forbidden in [
        "arguments",
        "output",
        "command",
        "error",
        "success",
        "exit_code",
        "auth",
    ] {
        assert!(record.get(forbidden).is_none());
    }
}

#[test]
fn invalid_oversized_and_multibyte_fields_cannot_impersonate_full_identity() {
    for (value, limit, reason) in [
        ("a\nb", 128, "invalid"),
        ("a\u{0}b", 128, "invalid"),
        ("", 128, "invalid"),
        ("a\u{2028}b", 128, "invalid"),
        ("a\u{202e}b", 128, "invalid"),
        ("a b", 128, "invalid"),
        ("ééé", 5, "oversized"),
    ] {
        let field = lifecycle_field(Some(value), limit);
        assert_eq!(field["value"], serde_json::Value::Null);
        assert_eq!(field["omitted"], json!(reason));
        assert_eq!(field["original_bytes"], json!(value.len()));
        assert_eq!(field["fingerprint_sha1"].as_str().unwrap().len(), 40);
        assert_eq!(field["fingerprint_scope"], json!("first_1024_bytes_max"));
    }
    assert_eq!(
        lifecycle_field(Some("éé"), 4),
        json!({"value": "éé", "omitted": null})
    );
    let first = "x".repeat(1024) + "suffix-a";
    let second = "x".repeat(1024) + "suffix-b";
    // A prefix fingerprint is intentionally not asserted to be a full ID/hash.
    assert_eq!(
        lifecycle_field(Some(&first), 128),
        lifecycle_field(Some(&second), 128)
    );
}

#[test]
fn count_budget_reserves_one_summary_and_never_heals_on_new_thread() {
    let mut budget = LifecycleBudget::default();
    let mut rows = Vec::new();
    for _ in 0..LIFECYCLE_MAX_RECORDS + 10 {
        if let Some(record) = budget.admit(json!({"event": "received"})) {
            rows.push(serde_json::from_str::<serde_json::Value>(&record).unwrap());
        }
    }
    assert_eq!(rows.len(), LIFECYCLE_MAX_RECORDS);
    assert_eq!(rows.last().unwrap()["event"], json!("trace_limit"));
    assert_eq!(rows.last().unwrap()["reason"], json!("record_count"));
    assert_eq!(rows.last().unwrap()["trace_incomplete"], json!(true));
    assert!(budget.admit(json!({"thread_id": "new-thread"})).is_none());
    assert!(budget.bytes <= LIFECYCLE_MAX_BYTES);
}

#[test]
fn record_and_total_byte_limits_emit_only_one_bounded_summary() {
    let mut budget = LifecycleBudget::default();
    let record = budget.admit(json!({"payload": "x".repeat(5000)})).unwrap();
    assert!(record.len() <= LIFECYCLE_SUMMARY_RESERVE);
    let summary: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(summary["reason"], json!("record_bytes"));
    assert!(budget.admit(json!({})).is_none());

    let mut budget = LifecycleBudget::default();
    let mut rows = Vec::new();
    while let Some(record) = budget.admit(json!({"payload": "x".repeat(3000)})) {
        assert!(record.len() <= LIFECYCLE_MAX_RECORD_BYTES);
        rows.push(record);
    }
    assert!(rows.len() < LIFECYCLE_MAX_RECORDS);
    assert!(rows.iter().map(String::len).sum::<usize>() <= LIFECYCLE_MAX_BYTES);
    let summary: serde_json::Value = serde_json::from_str(rows.last().unwrap()).unwrap();
    assert_eq!(summary["reason"], json!("total_bytes"));
    assert_eq!(summary["trace_incomplete"], json!(true));
}

#[test]
fn concurrent_admission_keeps_process_global_sequence_and_one_limit_summary() {
    let budget = Arc::new(Mutex::new(LifecycleBudget::default()));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let budget = Arc::clone(&budget);
        handles.push(std::thread::spawn(move || {
            let mut rows = Vec::new();
            for _ in 0..100 {
                if let Some(row) = budget.lock().unwrap().admit(json!({"event": "received"})) {
                    rows.push(serde_json::from_str::<serde_json::Value>(&row).unwrap());
                }
            }
            rows
        }));
    }
    let mut rows: Vec<_> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    rows.sort_by_key(|r| r["sequence"].as_u64().unwrap());
    assert_eq!(rows.len(), LIFECYCLE_MAX_RECORDS);
    assert_eq!(
        rows.iter().filter(|r| r["event"] == "trace_limit").count(),
        1
    );
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["sequence"], json!(index + 1));
    }
    assert!(budget.lock().unwrap().bytes <= LIFECYCLE_MAX_BYTES);
}

#[derive(Clone)]
struct Writer(Arc<Mutex<Vec<u8>>>);
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn new_event_is_detached_from_ambient_span_and_existing_target_remains() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Writer(Arc::clone(&bytes));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("fixture", private_payload = "PARENT_SECRET_NEVER_COPY");
        let _entered = span.enter();
        emit_lifecycle(fixture());
    });
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(text.contains("codex_tool_lifecycle"));
    assert!(!text.contains("PARENT_SECRET_NEVER_COPY"));
    assert!(!text.contains("private_payload"));

    bytes.lock().unwrap().clear();
    let writer = Writer(Arc::clone(&bytes));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111").unwrap();
        received(
            thread_id,
            &ToolName::plain("fixture_tool"),
            "call-1",
            Receipt::ModelTurn("turn-1"),
        );
        result_ready(
            thread_id,
            "turn-1",
            &ToolName::plain("fixture_tool"),
            "call-1",
            Source::Direct,
        );
    });
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let old: Vec<_> = text
        .lines()
        .filter(|line| line.contains("codex_otel.trace_safe"))
        .collect();
    assert_eq!(old.len(), 2);
    assert!(old[0].contains("codex.tool_call_received"));
    assert!(old[1].contains("codex.tool_result_ready"));
    assert!(old.iter().all(|line| !line.contains("metadata=")));
    // Ready records have no success/exit promise: abort/failure may be ready too.
    for line in text
        .lines()
        .filter(|line| line.contains("codex_tool_lifecycle"))
    {
        assert!(!line.contains("exit_code"));
        assert!(!line.contains("success"));
    }
}

#[test]
fn paired_filter_collects_only_dedicated_info_keeps_errors_and_drops_broad_target_payload() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Writer(Arc::clone(&bytes));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "error,opentelemetry_sdk=off,opentelemetry_otlp=off,codex_tool_lifecycle=info",
        ))
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        {
            // This span is enabled by the ACTUAL paired filter. Detachment
            // must not succeed merely because filtering disabled the span.
            let span = tracing::info_span!(target: "codex_tool_lifecycle", "fixture_parent",
                private_payload = "PAIRING_PARENT_SECRET_NEVER_COPY");
            let _entered = span.enter();
            tracing::event!(target: "codex_otel.trace_safe", tracing::Level::INFO,
                event.name = "codex.api_request", error.message = "PAIRING_NETWORK_BODY_NEVER_COLLECT");
            tracing::event!(target: "unrelated_provider", tracing::Level::INFO,
                message = "PAIRING_UNRELATED_INFO_NEVER_COLLECT");
            emit_lifecycle(fixture());
        }
        // Preserve the exec default's explicit exporter suppression, even at
        // ERROR level. This is ordinary fixed diagnostic policy, not env mutation.
        tracing::error!(target: "opentelemetry_sdk", "SDK_EXPORTER_ERROR_MUST_STAY_OFF");
        tracing::error!(target: "opentelemetry_otlp", "OTLP_EXPORTER_ERROR_MUST_STAY_OFF");
        // Preserve ERROR behavior outside the secret span; this does not
        // promise arbitrary preexisting ERROR text is redacted.
        tracing::error!(target: "existing_error_target", "PAIRING_EXISTING_ERROR_RETAINED");
    });
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(text.contains("codex_tool_lifecycle"));
    assert!(text.contains("codex.tool_lifecycle"));
    assert!(text.contains("PAIRING_EXISTING_ERROR_RETAINED"));
    assert!(!text.contains("SDK_EXPORTER_ERROR_MUST_STAY_OFF"));
    assert!(!text.contains("OTLP_EXPORTER_ERROR_MUST_STAY_OFF"));
    assert!(!text.contains("PAIRING_NETWORK_BODY_NEVER_COLLECT"));
    assert!(!text.contains("codex_otel.trace_safe"));
    assert!(!text.contains("PAIRING_UNRELATED_INFO_NEVER_COLLECT"));
    assert!(!text.contains("PAIRING_PARENT_SECRET_NEVER_COPY"));
    assert!(!text.contains("private_payload"));
}
