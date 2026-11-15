//! Bridged external-core tool activity drives the status indicator.

use super::*;
use codex_app_server_protocol::BridgeToolActivityNotification;
use codex_app_server_protocol::BridgeToolActivityStatus as AppServerBridgeToolActivityStatus;
use pretty_assertions::assert_eq;

fn activity(
    chat: &mut ChatWidget,
    call_id: &str,
    status: AppServerBridgeToolActivityStatus,
    detail: Option<&str>,
) {
    chat.handle_server_notification(
        ServerNotification::BridgeToolActivity(BridgeToolActivityNotification {
            native_result: None,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: call_id.to_string(),
            tool: "Bash".to_string(),
            status,
            detail: detail.map(str::to_string),
        }),
        /*replay_kind*/ None,
    );
}

#[tokio::test]
async fn bridge_tool_activity_drives_the_status_indicator() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    activity(
        &mut chat,
        "tc_1",
        AppServerBridgeToolActivityStatus::Started,
        Some("ls /tmp"),
    );
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("running status while the bridged tool executes");
    assert_eq!(status.header(), "Running Bash");
    assert_eq!(status.details(), Some("ls /tmp"));

    activity(
        &mut chat,
        "tc_1",
        AppServerBridgeToolActivityStatus::Completed,
        Some("file-a"),
    );
    assert_eq!(
        chat.bottom_pane
            .status_widget()
            .expect("generic working status after completion")
            .header(),
        "Working"
    );

    activity(
        &mut chat,
        "tc_2",
        AppServerBridgeToolActivityStatus::Started,
        Some(r#"{"command":"sleep 5"}"#),
    );
    activity(
        &mut chat,
        "tc_2",
        AppServerBridgeToolActivityStatus::Failed,
        Some("boom"),
    );
    assert_eq!(
        chat.bottom_pane
            .status_widget()
            .expect("generic working status after failure")
            .header(),
        "Working"
    );
}

#[tokio::test]
async fn bridge_tool_activity_running_row_renders() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    activity(
        &mut chat,
        "tc_1",
        AppServerBridgeToolActivityStatus::Started,
        Some("ls /tmp"),
    );

    let mut timer = crate::status_indicator_widget::StatusTimer::default();
    timer.pause_at(std::time::Instant::now());
    timer.reset(std::time::Duration::from_secs(/*secs*/ 7));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
        /*width*/ 80, /*height*/ 1,
    ))
    .expect("terminal");
    terminal
        .draw(|frame| {
            chat.bottom_pane
                .status_widget()
                .expect("running status")
                .with_timer(&timer)
                .render(frame.area(), frame.buffer_mut());
        })
        .expect("render bridge tool status");
    assert_chatwidget_snapshot!("bridge_tool_activity_row", terminal.backend());
}

#[tokio::test]
async fn genuine_in_process_native_return_inserts_ui_history_without_execution_request() {
    use codex_protocol::protocol::NativeToolObservation;
    use codex_protocol::protocol::NativeToolResult;
    let (mut chat, mut events, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(codex_protocol::ThreadId::new());
    chat.turn_lifecycle.last_turn_id = Some("owned-turn".to_owned());
    while events.try_recv().is_ok() {}
    while ops.try_recv().is_ok() {}
    let content = "native-only-marker".to_owned();
    chat.on_bridge_tool_activity(BridgeToolActivityNotification {
        native_result: Some(NativeToolObservation::Result(NativeToolResult {
            native_event: "tool_call_result".to_owned(),
            native_session_id: "native-session".to_owned(),
            native_turn_id: Some("native-turn".to_owned()),
            call_id: "owned-call".to_owned(),
            tool: "Bash".to_owned(),
            command: "python3 .local/h7_0123456789ab_01.py".to_owned(),
            native_original_bytes: Some(content.len() as u64),
            native_returned_bytes: Some(content.len() as u64),
            content,
            native_success: Some(true),
            exit_code: Some(0),
            status: Some("completed".to_owned()),
            timed_out: Some(false),
            native_truncated: Some(false),
            local_truncated: false,
            observation_complete: true,
        })),
        thread_id: chat.thread_id().unwrap().to_string(),
        turn_id: "owned-turn".to_owned(),
        call_id: "owned-call".to_owned(),
        tool: "Bash".to_owned(),
        status: AppServerBridgeToolActivityStatus::Completed,
        detail: Some("preview-must-not-replace-return".to_owned()),
    });
    let mut native_cells = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            let text = cell
                .raw_lines()
                .into_iter()
                .map(|line| {
                    line.spans
                        .into_iter()
                        .map(|span| span.content.into_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.starts_with("External native Bash:") {
                native_cells.push(text);
            }
        }
    }
    assert_eq!(native_cells.len(), 1);
    let snapshot = native_cells[0].replace(&chat.thread_id().unwrap().to_string(), "THREAD");
    insta::assert_snapshot!("bridge_native_actual_notification_history", snapshot, @r"
External native Bash: matched terminal return
  Thread: THREAD / turn: owned-turn
  Native session: native-session / turn: native-turn
  Call: owned-call
  Command: python3 .local/h7_0123456789ab_01.py
  Status: completed / exit: 0 / success: true / timeout: false
  Truncation: native=false local=false / observation_complete=true
  Serialized bytes: original=18 returned=18
  Native returned text (not reconstructed stdout):
    | native-only-marker
");
    assert!(native_cells[0].contains("    | native-only-marker"));
    assert!(!native_cells[0].contains("preview-must-not-replace-return"));
    assert!(
        ops.try_recv().is_err(),
        "rendering must not schedule core execution"
    );
}
