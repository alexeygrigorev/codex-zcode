use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

fn bridge_result_fixture() -> codex_app_server_protocol::BridgeToolActivityNotification {
    serde_json::from_value(json!({
        "threadId":"thread-1","turnId":"turn-1","callId":"call-1","tool":"Bash","status":"completed",
        "detail":"display preview must never supply output",
        "nativeResult":{"nativeSessionId":"native-session","nativeTurnId":"native-turn","nativeSuccess":true,
            "sourceEvent":"tool_call_result","exitCode":0,"commandStatus":"completed","output":"native-output\n",
            "observedBytes":14,"outputBytes":14,"outputTruncated":false,"sourceTruncated":false,
            "sourceOriginalBytes":14,"sourceReturnedBytes":14}
    })).expect("native result wire fixture")
}

fn bridge_turn(status: TurnStatus) -> codex_app_server_protocol::Turn {
    codex_app_server_protocol::Turn {
        id: "turn-1".to_string(),
        items_view: codex_app_server_protocol::TurnItemsView::Full,
        items: vec![],
        status,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

fn bridge_processor() -> EventProcessorWithJsonOutput {
    let mut processor = EventProcessorWithJsonOutput::new(None);
    processor.active_thread_id = Some("thread-1".to_string());
    processor.collect_thread_events(ServerNotification::TurnStarted(
        codex_app_server_protocol::TurnStartedNotification {
            thread_id: "thread-1".to_string(),
            turn: bridge_turn(TurnStatus::InProgress),
        },
    ));
    let mut scheduled = bridge_result_fixture();
    scheduled.status = codex_app_server_protocol::BridgeToolActivityStatus::Started;
    scheduled.native_result = None;
    processor.collect_thread_events(ServerNotification::BridgeToolActivity(scheduled));
    processor
}

#[test]
fn native_result_wire_roundtrip_emits_distinct_bound_jsonl_event() {
    let mut processor = bridge_processor();
    let notification = bridge_result_fixture();
    let collected = processor
        .collect_thread_events(ServerNotification::BridgeToolActivity(notification.clone()));
    assert_eq!(
        collected.events,
        vec![ThreadEvent::BridgeToolResult(BridgeToolResultEvent {
            version: 1,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool: "Bash".to_string(),
            native_result: notification.native_result.unwrap(),
        })]
    );
    let wire = serde_json::to_value(&collected.events[0]).unwrap();
    assert_eq!(wire["type"], json!("bridge_tool_result"));
    assert_eq!(wire["native_result"]["output"], json!("native-output\n"));
    assert!(
        wire.get("item").is_none(),
        "no command_execution or executable item is invented"
    );
    assert_eq!(
        processor.final_message(),
        None,
        "native output never becomes an agent message"
    );
}

#[test]
fn display_only_activity_has_null_native_result_and_cannot_prove_tool_output() {
    let mut processor = bridge_processor();
    let mut notification = bridge_result_fixture();
    notification.native_result = None;
    notification.detail =
        Some(r#"{"type":"command_execution","aggregated_output":"native-output"}"#.to_string());
    let wire = serde_json::to_value(&notification).unwrap();
    assert_eq!(
        wire.get("nativeResult"),
        Some(&serde_json::Value::Null),
        "outbound v2 fields are present and nullable"
    );
    assert!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(notification))
            .events
            .is_empty()
    );
    let mut malformed = serde_json::to_value(bridge_result_fixture()).unwrap();
    malformed["nativeResult"]
        .as_object_mut()
        .unwrap()
        .remove("nativeSuccess");
    assert!(
        serde_json::from_value::<codex_app_server_protocol::BridgeToolActivityNotification>(
            malformed
        )
        .is_err()
    );
}

#[test]
fn native_result_rejects_foreign_outer_binding_replays_and_late_completion() {
    let mut processor = bridge_processor();
    for (thread, turn) in [("foreign", "turn-1"), ("thread-1", "old-turn")] {
        let mut notification = bridge_result_fixture();
        notification.thread_id = thread.to_string();
        notification.turn_id = turn.to_string();
        assert!(
            processor
                .collect_thread_events(ServerNotification::BridgeToolActivity(notification))
                .events
                .is_empty()
        );
    }
    assert_eq!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(
                bridge_result_fixture()
            ))
            .events
            .len(),
        1
    );
    processor.collect_thread_events(ServerNotification::TurnStarted(
        codex_app_server_protocol::TurnStartedNotification {
            thread_id: "thread-1".to_string(),
            turn: bridge_turn(TurnStatus::InProgress),
        },
    ));
    assert!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(
                bridge_result_fixture()
            ))
            .events
            .is_empty(),
        "duplicate turn start must not reset replay protection"
    );
    processor.collect_thread_events(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: bridge_turn(TurnStatus::Completed),
        },
    ));
    let mut late = bridge_result_fixture();
    late.call_id = "late-call".to_string();
    assert!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(late))
            .events
            .is_empty()
    );
}

fn start_bridge_turn(processor: &mut EventProcessorWithJsonOutput, id: &str) {
    let mut turn = bridge_turn(TurnStatus::InProgress);
    turn.id = id.to_string();
    processor.collect_thread_events(ServerNotification::TurnStarted(
        codex_app_server_protocol::TurnStartedNotification {
            thread_id: "thread-1".to_string(),
            turn,
        },
    ));
}

fn schedule_and_collect_bridge_result(
    processor: &mut EventProcessorWithJsonOutput,
    turn_id: &str,
) -> Vec<ThreadEvent> {
    let mut result = bridge_result_fixture();
    result.turn_id = turn_id.to_string();
    let mut started = result.clone();
    started.status = codex_app_server_protocol::BridgeToolActivityStatus::Started;
    started.native_result = None;
    processor.collect_thread_events(ServerNotification::BridgeToolActivity(started));
    processor
        .collect_thread_events(ServerNotification::BridgeToolActivity(result))
        .events
}

#[test]
fn native_result_completed_turn_cannot_be_reopened_by_replayed_start() {
    let mut processor = bridge_processor();
    assert_eq!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(
                bridge_result_fixture(),
            ))
            .events
            .len(),
        1
    );
    processor.collect_thread_events(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: bridge_turn(TurnStatus::Completed),
        },
    ));
    start_bridge_turn(&mut processor, "turn-1");
    assert!(schedule_and_collect_bridge_result(&mut processor, "turn-1").is_empty());
}

#[test]
fn native_result_old_turn_replay_does_not_displace_a_genuine_new_turn() {
    let mut processor = bridge_processor();
    start_bridge_turn(&mut processor, "turn-2");
    start_bridge_turn(&mut processor, "turn-1");
    assert!(schedule_and_collect_bridge_result(&mut processor, "turn-1").is_empty());
    assert_eq!(
        schedule_and_collect_bridge_result(&mut processor, "turn-2").len(),
        1
    );
    start_bridge_turn(&mut processor, "turn-1");
    assert!(schedule_and_collect_bridge_result(&mut processor, "turn-1").is_empty());
}

#[test]
fn native_result_configuration_reprints_cannot_reset_turn_tombstones() {
    let mut processor = bridge_processor();
    assert_eq!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(
                bridge_result_fixture(),
            ))
            .events
            .len(),
        1
    );
    // This is the same state hook used by print_config_summary. Reprinting an
    // active thread must not clear the consumed call or the lifetime turn set.
    processor.configure_native_thread("thread-1".to_string());
    assert!(schedule_and_collect_bridge_result(&mut processor, "turn-1").is_empty());
    processor.collect_thread_events(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: bridge_turn(TurnStatus::Completed),
        },
    ));
    processor.configure_native_thread("thread-1".to_string());
    start_bridge_turn(&mut processor, "turn-1");
    assert!(schedule_and_collect_bridge_result(&mut processor, "turn-1").is_empty());
    // Switching away and back also cannot reopen a previously observed turn.
    processor.configure_native_thread("thread-2".to_string());
    processor.configure_native_thread("thread-1".to_string());
    start_bridge_turn(&mut processor, "turn-1");
    assert!(schedule_and_collect_bridge_result(&mut processor, "turn-1").is_empty());
    start_bridge_turn(&mut processor, "turn-2");
    assert_eq!(
        schedule_and_collect_bridge_result(&mut processor, "turn-2").len(),
        1
    );
}

#[test]
fn native_result_turn_capacity_fails_closed_without_eviction_or_configuration_healing() {
    let mut processor = bridge_processor();
    for index in 2..=128 {
        start_bridge_turn(&mut processor, &format!("turn-{index}"));
    }
    assert_eq!(
        schedule_and_collect_bridge_result(&mut processor, "turn-128").len(),
        1
    );
    start_bridge_turn(&mut processor, "turn-129");
    assert!(processor.native_turn_overflow);
    assert_eq!(processor.seen_native_turns.len(), 128);
    processor.configure_native_thread("thread-2".to_string());
    processor.configure_native_thread("thread-1".to_string());
    for turn in ["turn-1", "turn-128", "turn-129", "turn-130"] {
        start_bridge_turn(&mut processor, turn);
        assert!(schedule_and_collect_bridge_result(&mut processor, turn).is_empty());
    }
    assert_eq!(processor.seen_native_turns.len(), 128);
}

#[test]
fn native_result_orphan_completions_are_bounded_tombstones_without_reopening() {
    let mut processor = bridge_processor();
    let complete = |processor: &mut EventProcessorWithJsonOutput, id: &str| {
        let mut turn = bridge_turn(TurnStatus::Completed);
        turn.id = id.to_string();
        processor.collect_thread_events(ServerNotification::TurnCompleted(
            codex_app_server_protocol::TurnCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn,
            },
        ));
    };
    complete(&mut processor, "orphan-turn");
    start_bridge_turn(&mut processor, "orphan-turn");
    assert!(schedule_and_collect_bridge_result(&mut processor, "orphan-turn").is_empty());
    assert_eq!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(
                bridge_result_fixture(),
            ))
            .events
            .len(),
        1,
        "orphan completion does not displace the genuine active turn"
    );
    // Orphan completions share the lifetime bound, without eviction on overflow.
    for index in 2..=128 {
        complete(&mut processor, &format!("orphan-{index}"));
    }
    assert!(processor.native_turn_overflow);
    assert_eq!(processor.seen_native_turns.len(), 128);
    processor.configure_native_thread("thread-1".to_string());
    for id in ["orphan-turn", "orphan-128", "new-turn"] {
        start_bridge_turn(&mut processor, id);
        assert!(schedule_and_collect_bridge_result(&mut processor, id).is_empty());
    }
}

#[test]
fn native_result_keeps_failures_nonzero_exits_and_unknown_command_metadata_explicit() {
    for success in [true, false] {
        let mut processor = bridge_processor();
        let mut notification = bridge_result_fixture();
        if !success {
            notification.status = codex_app_server_protocol::BridgeToolActivityStatus::Failed;
        }
        let result = notification.native_result.as_mut().unwrap();
        result.native_success = success;
        result.exit_code = if success { Some(7) } else { None };
        result.command_status = None;
        result.source_truncated = None;
        result.source_original_bytes = None;
        result.source_returned_bytes = None;
        let output = processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(notification.clone()));
        assert_eq!(
            output.events,
            vec![ThreadEvent::BridgeToolResult(BridgeToolResultEvent {
                version: 1,
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                call_id: "call-1".to_string(),
                tool: "Bash".to_string(),
                native_result: notification.native_result.unwrap(),
            })]
        );
    }
}

#[test]
fn invalid_native_result_cannot_heal_or_switch_native_identity() {
    let mut processor = bridge_processor();
    let mut invalid = bridge_result_fixture();
    invalid.native_result.as_mut().unwrap().output_bytes = 2;
    assert!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(invalid))
            .events
            .is_empty()
    );
    assert!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(
                bridge_result_fixture()
            ))
            .events
            .is_empty()
    );
    let mut processor = bridge_processor();
    processor.collect_thread_events(ServerNotification::BridgeToolActivity(
        bridge_result_fixture(),
    ));
    let mut wrong = bridge_result_fixture();
    wrong.call_id = "call-2".to_string();
    let mut scheduled = wrong.clone();
    scheduled.status = codex_app_server_protocol::BridgeToolActivityStatus::Started;
    scheduled.native_result = None;
    processor.collect_thread_events(ServerNotification::BridgeToolActivity(scheduled));
    wrong.native_result.as_mut().unwrap().native_turn_id = "foreign-native-turn".to_string();
    assert!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(wrong))
            .events
            .is_empty()
    );
}

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
fn native_result_rejects_unscheduled_cli_call() {
    let mut processor = bridge_processor();
    let mut orphan = bridge_result_fixture();
    orphan.call_id = "orphan".to_string();
    assert!(
        processor
            .collect_thread_events(ServerNotification::BridgeToolActivity(orphan))
            .events
            .is_empty()
    );
}

#[test]
fn assistant_json_echo_never_becomes_a_native_result() {
    let mut processor = bridge_processor();
    let text = serde_json::to_string(&bridge_result_fixture()).unwrap();
    let output = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: ThreadItem::AgentMessage {
                id: "assistant-echo".to_string(),
                text: text.clone(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
        },
    ));
    assert_eq!(output.events.len(), 1);
    assert!(
        matches!(&output.events[0], ThreadEvent::ItemCompleted(event)
        if matches!(&event.item.details, ThreadItemDetails::AgentMessage(message) if message.text == text))
    );
}
