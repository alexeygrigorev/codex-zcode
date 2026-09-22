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
