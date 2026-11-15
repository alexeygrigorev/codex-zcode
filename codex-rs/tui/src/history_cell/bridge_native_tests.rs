//! Pure rendering of actual typed notifications; no provider, artifacts or execution.
use super::*;
use pretty_assertions::assert_eq;

fn result() -> NativeToolResult {
    let content = "AC10_WORK_V2 actual-return".to_owned();
    NativeToolResult {
        native_event: "tool_call_result".to_owned(),
        native_session_id: "native-session".to_owned(),
        native_turn_id: Some("native-turn".to_owned()),
        call_id: "call-1".to_owned(),
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
    }
}

fn notification(result: NativeToolResult) -> BridgeToolActivityNotification {
    BridgeToolActivityNotification {
        native_result: Some(NativeToolObservation::Result(result)),
        thread_id: "thread".to_owned(),
        turn_id: "turn".to_owned(),
        call_id: "call-1".to_owned(),
        tool: "Bash".to_owned(),
        status: BridgeToolActivityStatus::Completed,
        detail: Some("PREVIEW_NOT_RETURNED_TEXT".to_owned()),
    }
}

fn text(cell: &BridgeNativeCell) -> String {
    cell.raw_lines()
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn matched_native_return_renders_literal_command_and_return_not_preview_or_stdout() {
    let mut history = BridgeNativeHistory::default();
    let event = notification(result());
    let cell = history
        .observe(Some("thread"), Some("turn"), &event)
        .unwrap();
    let rendered = text(&cell);
    assert!(rendered.contains("External native Bash: matched terminal return"));
    assert!(rendered.contains("Command: python3 .local/h7_0123456789ab_01.py"));
    assert!(rendered.contains("    | AC10_WORK_V2 actual-return"));
    assert!(rendered.contains("not reconstructed stdout"));
    assert!(!rendered.contains("PREVIEW_NOT_RETURNED_TEXT"));
    assert_eq!(cell.transcript_lines(40), cell.display_lines(40));
    assert!(cell.display_lines(0).is_empty());
    assert!(cell.desired_height(20) > cell.desired_height(120));
}

#[test]
fn late_final_loss_result_invalidates_both_retained_turns_without_stale_rendering() {
    for (local_truncated, observation_complete) in [(true, true), (false, false), (true, false)] {
        let mut history = BridgeNativeHistory::default();
        let first = history
            .observe(Some("thread"), Some("turn"), &notification(result()))
            .unwrap();
        let mut second = notification(result());
        second.turn_id = "next".to_owned();
        let second_cell = history
            .observe(Some("thread"), Some("next"), &second)
            .unwrap();
        let mut loss = notification(result());
        if let Some(NativeToolObservation::Result(value)) = &mut loss.native_result {
            value.local_truncated = local_truncated;
            value.observation_complete = observation_complete;
            value.content = "STALE_RETURN_MUST_NOT_RENDER".to_owned();
        }
        loss.thread_id = "foreign".to_owned();
        assert!(
            history
                .observe(Some("thread"), Some("next"), &loss)
                .is_none()
        );
        loss.thread_id = "thread".to_owned();
        loss.turn_id = "unseen".to_owned();
        assert!(
            history
                .observe(Some("thread"), Some("next"), &loss)
                .is_none()
        );
        assert!(text(&first).contains("matched terminal return"));
        assert!(text(&second_cell).contains("matched terminal return"));
        loss.turn_id = "turn".to_owned();
        let warning = history
            .observe(Some("thread"), Some("next"), &loss)
            .unwrap();
        assert!(warning.value.is_none());
        assert!(!text(&warning).contains("STALE_RETURN_MUST_NOT_RENDER"));
        assert!(text(&first).contains("UNQUALIFIED return"));
        assert!(text(&second_cell).contains("UNQUALIFIED return"));
        assert!(
            history
                .observe(Some("thread"), Some("next"), &second)
                .is_none()
        );
    }
}
