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
