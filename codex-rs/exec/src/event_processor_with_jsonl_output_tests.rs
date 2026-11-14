use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn failed_turn_does_not_overwrite_output_last_message_file() {
    let tempdir = tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("last-message.txt");
    std::fs::write(&output_path, "keep existing contents").expect("seed output file");

    let mut processor = EventProcessorWithJsonOutput::new(Some(output_path.clone()));

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "msg-1".to_string(),
                text: "partial answer".to_string(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(processor.final_message(), Some("partial answer"));

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: Some(codex_app_server_protocol::TurnError {
                    misalignment: None,
                    message: "turn failed".to_string(),
                    additional_details: None,
                    codex_error_info: None,
                }),
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(status, CodexStatus::InitiateShutdown);
    assert_eq!(processor.final_message(), None);

    EventProcessor::print_final_output(&mut processor);

    assert_eq!(
        std::fs::read_to_string(&output_path).expect("read output file"),
        "keep existing contents"
    );
}

#[test]
fn runtime_warning_emits_a_non_fatal_error_item() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::Warning(
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-1".to_string()),
            message: "invalid global instructions".to_string(),
        },
    ));

    assert_eq!(
        collected,
        CollectedThreadEvents {
            events: vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ExecThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::Error(ErrorItem {
                        message: "invalid global instructions".to_string(),
                    }),
                },
            })],
            status: CodexStatus::Running,
        }
    );
}

#[test]
fn mcp_tool_call_result_preserves_meta_in_jsonl_event() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::McpToolCall {
                id: "mcp-1".to_string(),
                server: "search service".to_string(),
                tool: "web_run".to_string(),
                status: McpToolCallStatus::Completed,
                arguments: json!({"search_query": [{"q": "OpenAI Codex CLI documentation"}]}),
                app_context: None,
                mcp_app_resource_uri: None,
                mcp_app_ui: None,
                plugin_id: None,
                read_only_hint: None,
                result: Some(Box::new(codex_app_server_protocol::McpToolCallResult {
                    content: vec![json!({"type": "text", "text": "search result"})],
                    structured_content: None,
                    meta: Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]})),
                })),
                error: None,
                duration_ms: Some(42),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(collected.events.len(), 1);

    let ThreadEvent::ItemCompleted(ItemCompletedEvent { item }) = &collected.events[0] else {
        panic!("expected item.completed event");
    };
    let ThreadItemDetails::McpToolCall(item) = &item.details else {
        panic!("expected MCP tool call item");
    };
    let result = item.result.as_ref().expect("expected MCP tool result");
    assert_eq!(
        result.meta,
        Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]}))
    );

    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize event");
    assert_eq!(
        serialized["item"]["result"]["_meta"],
        json!({"raw_messages": [{"ref_id": "turn0search0"}]})
    );
    assert!(serialized["item"]["result"].get("meta").is_none());
}

#[test]
fn native_result_is_owned_json_only_and_never_a_command_execution() {
    use codex_protocol::protocol::NativeToolObservation;
    use codex_protocol::protocol::NativeToolResult;
    let observation = NativeToolObservation::Result(NativeToolResult {
        native_event: "tool_call_result".to_owned(),
        native_session_id: "native-session".to_owned(),
        native_turn_id: Some("native-turn".to_owned()),
        call_id: "actual-call".to_owned(),
        tool: "Bash".to_owned(),
        command: "printf nonce".to_owned(),
        content: "nonce".to_owned(),
        native_success: Some(true),
        exit_code: Some(0),
        status: Some("completed".to_owned()),
        timed_out: Some(false),
        native_truncated: Some(false),
        native_original_bytes: Some(5),
        native_returned_bytes: Some(5),
        local_truncated: false,
        observation_complete: true,
    });
    let activity = codex_app_server_protocol::BridgeToolActivityNotification {
        native_result: Some(observation.clone()),
        thread_id: "thread-1".to_owned(),
        turn_id: "turn-1".to_owned(),
        call_id: "actual-call".to_owned(),
        tool: "Bash".to_owned(),
        status: codex_app_server_protocol::BridgeToolActivityStatus::Completed,
        detail: Some("preview".to_owned()),
    };
    let public = serde_json::to_value(&activity).unwrap();
    assert_eq!(
        public,
        json!({"threadId":"thread-1","turnId":"turn-1","callId":"actual-call","tool":"Bash","status":"completed","detail":"preview"})
    );
    assert!(!format!("{activity:?}").contains("printf nonce"));
    assert!(!format!("{activity:?}").contains("content:"));
    let deserialized: codex_app_server_protocol::BridgeToolActivityNotification =
        serde_json::from_value(public).unwrap();
    assert_eq!(deserialized.native_result, None);
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    let collected =
        processor.collect_thread_events(ServerNotification::BridgeToolActivity(activity));
    assert_eq!(collected.events.len(), 1);
    let encoded = serde_json::to_value(&collected.events[0]).unwrap();
    assert_eq!(encoded["type"], "item.completed");
    assert_eq!(encoded["item"]["type"], "native_tool_result");
    assert_eq!(encoded["item"]["thread_id"], "thread-1");
    assert_eq!(encoded["item"]["turn_id"], "turn-1");
    assert_eq!(encoded["item"]["observation"]["content"], "nonce");
    assert_eq!(encoded["item"]["observation"]["kind"], "result");
    assert!(!encoded.to_string().contains("command_execution"));
}

#[test]
fn native_stream_fault_is_explicit_owned_json_not_silent_loss() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    let collected = processor.collect_thread_events(ServerNotification::BridgeToolActivity(
        codex_app_server_protocol::BridgeToolActivityNotification {
            native_result: Some(
                codex_protocol::protocol::NativeToolObservation::Invalidated {
                    reason: "native_stream_unqualified".to_owned(),
                },
            ),
            thread_id: "t".to_owned(),
            turn_id: "u".to_owned(),
            call_id: "native-observer".to_owned(),
            tool: "observer".to_owned(),
            status: codex_app_server_protocol::BridgeToolActivityStatus::Failed,
            detail: None,
        },
    ));
    let encoded = serde_json::to_value(&collected.events[0]).unwrap();
    assert_eq!(
        encoded["item"]["observation"],
        json!({"kind":"invalidated","reason":"native_stream_unqualified"})
    );
}
