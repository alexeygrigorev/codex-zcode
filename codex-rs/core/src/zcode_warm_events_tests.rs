use std::collections::HashMap;

use pretty_assertions::assert_eq;

use super::bridge_tool_activity_from_notification;
use codex_protocol::protocol::BridgeToolActivityEvent;
use codex_protocol::protocol::BridgeToolActivityStatus;

fn scheduled_notification(tool_call_id: &str, tool_name: &str, input: serde_json::Value) -> String {
    format!(
        r#"{{"type":"tool_call_scheduled","payload":{{"toolCallId":"{tool_call_id}","toolName":"{tool_name}","input":{input}}}}}"#
    )
}

fn parse(notification: &str) -> serde_json::Value {
    serde_json::from_str(notification).expect("notification fixture parses")
}

fn activity(
    notification: &str,
    tool_names: &mut HashMap<String, String>,
) -> BridgeToolActivityEvent {
    bridge_tool_activity_from_notification(&parse(notification), tool_names)
        .expect("tool notification maps to bridge activity")
}

#[test]
fn scheduled_maps_to_started_with_input_detail() {
    let mut tool_names = HashMap::new();
    assert_eq!(
        activity(
            &scheduled_notification("tc_1", "Bash", serde_json::json!({ "command": "ls /tmp" })),
            &mut tool_names,
        ),
        BridgeToolActivityEvent {
            call_id: "tc_1".to_string(),
            tool: "Bash".to_string(),
            status: BridgeToolActivityStatus::Started,
            detail: Some(r#"{"command":"ls /tmp"}"#.to_string()),
            native_result: None,
        },
    );
    assert_eq!(
        tool_names.get("tc_1").map(String::as_str),
        Some("Bash"),
        "later events need the scheduled name when they omit toolName",
    );
}

#[test]
fn oversized_input_detail_is_capped() {
    let long_command = "x".repeat(500);
    let mut tool_names = HashMap::new();
    let mapped = activity(
        &scheduled_notification(
            "tc_1",
            "Bash",
            serde_json::json!({ "command": long_command }),
        ),
        &mut tool_names,
    );
    let detail = mapped.detail.expect("input detail is present");
    assert_eq!(detail.chars().count(), 200);
}

#[test]
fn successful_result_falls_back_to_scheduled_tool_name() {
    let mut tool_names = HashMap::new();
    activity(
        &scheduled_notification("tc_1", "Bash", serde_json::json!({ "command": "ls" })),
        &mut tool_names,
    );
    assert_eq!(
        activity(
            r#"{"type":"tool_call_result","payload":{"toolCallId":"tc_1","result":{"success":true,"content":"file-a\nfile-b\n"},"duration":12}}"#,
            &mut tool_names,
        ),
        BridgeToolActivityEvent {
            call_id: "tc_1".to_string(),
            tool: "Bash".to_string(),
            status: BridgeToolActivityStatus::Completed,
            detail: Some("file-a\nfile-b\n".to_string()),
            native_result: None,
        },
    );
}

#[test]
fn failed_result_maps_to_failed_status() {
    let mut tool_names = HashMap::new();
    assert_eq!(
        activity(
            r#"{"type":"tool_call_result","payload":{"toolCallId":"tc_1","toolName":"Edit","result":{"success":false,"content":"","error":{"type":"ToolError","message":"patch did not apply"}}}}"#,
            &mut tool_names,
        ),
        BridgeToolActivityEvent {
            call_id: "tc_1".to_string(),
            tool: "Edit".to_string(),
            status: BridgeToolActivityStatus::Failed,
            detail: Some("patch did not apply".to_string()),
            native_result: None,
        },
    );
}

#[test]
fn tool_error_maps_to_failed_status() {
    let mut tool_names = HashMap::new();
    assert_eq!(
        activity(
            r#"{"type":"tool_call_error","payload":{"toolCallId":"tc_1","error":{"type":"ToolError","message":"nope"}}}"#,
            &mut tool_names,
        ),
        BridgeToolActivityEvent {
            call_id: "tc_1".to_string(),
            tool: "unknown".to_string(),
            status: BridgeToolActivityStatus::Failed,
            detail: Some("nope".to_string()),
            native_result: None,
        },
    );
}

#[test]
fn result_without_scheduled_or_name_reports_unknown_tool() {
    let mut tool_names = HashMap::new();
    assert_eq!(
        activity(
            r#"{"type":"tool_call_result","payload":{"toolCallId":"tc_late","result":{"success":true,"content":"ok"}}}"#,
            &mut tool_names,
        )
        .tool,
        "unknown",
    );
}

#[test]
fn non_tool_notifications_do_not_map() {
    let mut tool_names = HashMap::new();
    for notification in [
        r#"{"type":"model.streaming","payload":{"kind":"text_delta","delta":"hi"}}"#,
        r#"{"type":"turn.completed","payload":{"resultType":"success"}}"#,
        r#"{"type":"tool_call_scheduled","payload":{}}"#,
        r#"{"sessionId":"sess_fake"}"#,
    ] {
        assert!(
            bridge_tool_activity_from_notification(&parse(notification), &mut tool_names).is_none(),
            "unexpected mapping for {notification}",
        );
    }
}

fn native_event(kind: &str, payload: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"sessionId":"native-session", "turnId":"native-turn", "type":kind, "payload":payload})
}

fn native_tracker() -> super::NativeResultTracker {
    let mut tracker = super::NativeResultTracker::new("native-session".to_string());
    assert!(
        tracker
            .observe(&native_event("turn.started", serde_json::json!({})))
            .is_none()
    );
    tracker
}

fn schedule_native(tracker: &mut super::NativeResultTracker, call: &str) {
    assert!(tracker.observe(&native_event("tool_call_scheduled", serde_json::json!({
        "toolCallId":call,"toolName":"Bash","input":{"command":"do not execute this fixture"}
    }))).is_none());
}

fn result_native(call: &str) -> serde_json::Value {
    native_event(
        "tool_call_result",
        serde_json::json!({"toolCallId":call,"result":{
            "success":true,"content":"native-output\n","truncated":false,"originalBytes":14,"returnedBytes":14,
            "perf":{"detail":{"kind":"command","command":{"status":"completed","exitCode":0}}}
        }}),
    )
}

#[test]
fn native_result_preserves_output_identities_and_explicit_command_metadata() {
    let mut tracker = native_tracker();
    schedule_native(&mut tracker, "call-1");
    let result = tracker
        .observe(&result_native("call-1"))
        .expect("qualified native result");
    assert_eq!(
        result,
        codex_protocol::protocol::BridgeNativeToolResult {
            native_session_id: "native-session".to_string(),
            native_turn_id: "native-turn".to_string(),
            native_success: true,
            source_event: "tool_call_result".to_string(),
            exit_code: Some(0),
            command_status: Some("completed".to_string()),
            output: "native-output\n".to_string(),
            observed_bytes: 14,
            output_bytes: 14,
            output_truncated: false,
            source_truncated: Some(false),
            source_original_bytes: Some(14),
            source_returned_bytes: Some(14),
        }
    );
    assert!(
        tracker.observe(&result_native("call-1")).is_none(),
        "duplicate completion is not fresh evidence"
    );
}

#[test]
fn native_tool_success_does_not_invent_command_success_or_known_truncation() {
    for success in [true, false] {
        let mut tracker = native_tracker();
        schedule_native(&mut tracker, "call-1");
        let mut event = result_native("call-1");
        event["payload"]["result"] =
            serde_json::json!({"success":success,"content":"native-output\n"});
        let result = tracker
            .observe(&event)
            .expect("native tool result without command metadata");
        assert_eq!(result.native_success, success);
        assert_eq!(
            (
                result.exit_code,
                result.command_status,
                result.source_truncated,
                result.source_original_bytes,
                result.source_returned_bytes
            ),
            (None, None, None, None, None)
        );
    }
    let mut tracker = native_tracker();
    schedule_native(&mut tracker, "call-1");
    let mut event = result_native("call-1");
    event["payload"]["result"]["perf"]["detail"]["command"]["exitCode"] = serde_json::json!(9);
    let result = tracker.observe(&event).expect("nonzero exit preserved");
    assert_eq!((result.native_success, result.exit_code), (true, Some(9)));
}

#[test]
fn malformed_terminal_metadata_cannot_heal_or_become_success() {
    for (field, value) in [
        ("success", serde_json::Value::Null),
        ("success", serde_json::json!("true")),
        ("content", serde_json::json!(true)),
        ("truncated", serde_json::json!("false")),
        ("originalBytes", serde_json::json!(2)),
        ("returnedBytes", serde_json::json!(13)),
    ] {
        let mut tracker = native_tracker();
        schedule_native(&mut tracker, "call-1");
        let mut event = result_native("call-1");
        event["payload"]["result"][field] = value;
        assert!(tracker.observe(&event).is_none(), "invalid {field}");
        assert!(
            tracker.observe(&result_native("call-1")).is_none(),
            "terminal corruption cannot heal"
        );
    }
    let mut tracker = native_tracker();
    schedule_native(&mut tracker, "call-1");
    let mut event = result_native("call-1");
    event["payload"]["result"]
        .as_object_mut()
        .unwrap()
        .remove("success");
    assert!(
        tracker.observe(&event).is_none(),
        "absent success is not true"
    );
}

#[test]
fn native_results_reject_orphans_wrong_identity_late_and_duplicate_schedules() {
    let mut tracker = native_tracker();
    assert!(tracker.observe(&result_native("orphan")).is_none());
    schedule_native(&mut tracker, "call-1");
    for (field, value) in [("sessionId", "foreign-session"), ("turnId", "foreign-turn")] {
        let mut event = result_native("call-1");
        event[field] = serde_json::json!(value);
        assert!(tracker.observe(&event).is_none());
    }
    assert!(tracker.observe(&result_native("call-1")).is_some());
    schedule_native(&mut tracker, "duplicate");
    schedule_native(&mut tracker, "duplicate");
    assert!(tracker.observe(&result_native("duplicate")).is_none());
    schedule_native(&mut tracker, "late");
    tracker.observe(&native_event("turn.completed", serde_json::json!({})));
    assert!(tracker.observe(&result_native("late")).is_none());
    tracker.observe(&native_event("turn.started", serde_json::json!({})));
    assert!(
        tracker.observe(&result_native("late")).is_none(),
        "closed collector never restarts"
    );
}

#[test]
fn native_output_is_byte_bounded_without_splitting_utf8_and_preserves_source_truncation() {
    let mut tracker = native_tracker();
    schedule_native(&mut tracker, "call-1");
    let content = "€".repeat(6000);
    let mut event = result_native("call-1");
    event["payload"]["result"] = serde_json::json!({
        "success":true,"content":content,"truncated":true,"originalBytes":20000,"returnedBytes":18000});
    let result = tracker.observe(&event).expect("bounded native output");
    assert_eq!(result.output, "€".repeat(5461));
    assert_eq!(
        (
            result.observed_bytes,
            result.output_bytes,
            result.output_truncated,
            result.source_truncated,
            result.source_original_bytes,
            result.source_returned_bytes
        ),
        (18000, 16383, true, Some(true), Some(20000), Some(18000))
    );
}

#[test]
fn native_tracker_overflow_and_missing_turn_never_produce_evidence() {
    let mut tracker = super::NativeResultTracker::new("native-session".to_string());
    schedule_native(&mut tracker, "before-start");
    assert!(tracker.observe(&result_native("before-start")).is_none());
    let mut tracker = native_tracker();
    for i in 0..=super::NATIVE_CALL_LIMIT {
        schedule_native(&mut tracker, &format!("call-{i}"));
    }
    assert!(
        tracker.observe(&result_native("call-0")).is_none(),
        "overflow stays unknown"
    );
    assert!(tracker.calls.len() <= super::NATIVE_CALL_LIMIT);
}

#[test]
fn typed_native_error_is_a_failure_and_never_a_successful_command() {
    let mut tracker = native_tracker();
    schedule_native(&mut tracker, "call-1");
    let result = tracker
        .observe(&native_event(
            "tool_call_error",
            serde_json::json!({
        "toolCallId":"call-1","error":{"message":"native failure"}}),
        ))
        .expect("typed native failure");
    assert_eq!(
        (
            result.source_event.as_str(),
            result.native_success,
            result.exit_code,
            result.command_status
        ),
        ("tool_call_error", false, None, None)
    );
    assert_eq!(result.output, "native failure");
    assert!(tracker.observe(&result_native("call-1")).is_none());
}
