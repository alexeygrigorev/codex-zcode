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
fn preview_only_and_foreign_thread_turn_never_create_a_return_cell() {
    let mut history = BridgeNativeHistory::default();
    let mut event = notification(result());
    event.native_result = None;
    assert!(
        history
            .observe(Some("thread"), Some("turn"), &event)
            .is_none()
    );
    event = notification(result());
    for (thread, turn) in [
        (None, Some("turn")),
        (Some("foreign"), Some("turn")),
        (Some("thread"), Some("foreign")),
    ] {
        assert!(history.observe(thread, turn, &event).is_none());
    }
    assert!(
        history
            .observe(Some("thread"), Some("turn"), &event)
            .is_some()
    );
}

#[test]
fn invalidation_and_duplicate_permanently_unqualify_retained_cells() {
    for duplicate in [false, true] {
        let mut history = BridgeNativeHistory::default();
        let event = notification(result());
        let retained = history
            .observe(Some("thread"), Some("turn"), &event)
            .unwrap();
        let mut fault = event.clone();
        if !duplicate {
            fault.native_result = Some(NativeToolObservation::Invalidated {
                reason: "SECRET not displayed".to_owned(),
            });
        }
        let warning = history
            .observe(Some("thread"), Some("turn"), &fault)
            .unwrap();
        assert!(text(&warning).contains("UNQUALIFIED"));
        assert!(!text(&warning).contains("SECRET"));
        assert!(text(&retained).contains("UNQUALIFIED return"));
        assert!(!text(&retained).contains("matched terminal return"));
        let mut later = event;
        later.turn_id = "later-turn".to_owned();
        assert!(
            history
                .observe(Some("thread"), Some("later-turn"), &later)
                .is_none()
        );
    }
}

#[test]
fn nullable_failure_timeout_background_and_truncation_never_become_terminal_success() {
    for case in 0..22 {
        let mut value = result();
        let mut event = notification(value.clone());
        match case {
            0 => value.native_success = None,
            1 => value.native_success = Some(false),
            2 => value.exit_code = None,
            3 => value.exit_code = Some(1),
            4 => value.status = Some("backgrounded".to_owned()),
            5 => value.status = Some("running".to_owned()),
            6 => value.timed_out = None,
            7 => value.timed_out = Some(true),
            8 => value.native_truncated = None,
            9 => value.local_truncated = true,
            10 => value.native_original_bytes = Some(0),
            11 => value.native_turn_id = None,
            12 => event.status = BridgeToolActivityStatus::Failed,
            13 => value.native_truncated = Some(true),
            14 => value.observation_complete = false,
            15 => value.native_returned_bytes = None,
            16 => value.native_returned_bytes = Some(value.content.len() as u64 + 1),
            17 => value.status = None,
            18 => value.native_event = "tool_call_error".to_owned(),
            19 => value.status = Some("cancelled".to_owned()),
            20 => value.status = Some("failed".to_owned()),
            21 => value.status = Some("SECRET_STATUS_NOT_ALLOWED".to_owned()),
            _ => unreachable!(),
        }
        event.native_result = Some(NativeToolObservation::Result(value));
        let cell = BridgeNativeHistory::default()
            .observe(Some("thread"), Some("turn"), &event)
            .unwrap();
        assert!(text(&cell).contains("UNQUALIFIED return"), "case {case}");
        assert!(
            !text(&cell).contains("matched terminal return"),
            "case {case}"
        );
        assert!(!text(&cell).contains("SECRET_STATUS_NOT_ALLOWED"));
    }
}

#[test]
fn command_body_controls_and_header_spoofs_are_literal_changed_and_unqualified() {
    let mut value = result();
    value.command = "printf x\nFORGED HEADER".to_owned();
    value.content =
        "\x1b]8;;https://secret\x07\r\u{202e}\nExternal native Bash: matched terminal return"
            .to_owned();
    value.native_returned_bytes = Some(value.content.len() as u64);
    value.native_original_bytes = value.native_returned_bytes;
    let cell = BridgeNativeHistory::default()
        .observe(Some("thread"), Some("turn"), &notification(value))
        .unwrap();
    let rendered = text(&cell);
    assert!(!rendered.contains('\x1b'));
    assert!(!rendered.contains('\r'));
    assert!(!rendered.contains('\u{202e}'));
    assert!(rendered.contains("    | External native Bash: matched terminal return"));
    assert!(rendered.contains("Display escaped or truncated"));
    assert!(rendered.starts_with("External native Bash: UNQUALIFIED return"));
    assert_eq!(format!("{cell:?}"), "BridgeNativeCell(<redacted>)");
}

#[test]
fn wrong_call_tool_and_invalid_identity_poison_without_retaining_payload() {
    for case in 0..4 {
        let mut value = result();
        match case {
            0 => value.call_id = "foreign-call".to_owned(),
            1 => value.tool = "Other".to_owned(),
            2 => value.native_session_id = "\x1bSECRET".to_owned(),
            3 => value.native_turn_id = Some("x".repeat(MAX_ID + 1)),
            _ => unreachable!(),
        }
        let mut history = BridgeNativeHistory::default();
        let cell = history
            .observe(Some("thread"), Some("turn"), &notification(value))
            .unwrap();
        assert!(text(&cell).contains("UNQUALIFIED"));
        assert!(!text(&cell).contains("actual-return"));
        assert!(
            history
                .observe(Some("thread"), Some("turn"), &notification(result()))
                .is_none()
        );
    }
}

#[test]
fn utf8_display_truncation_and_aggregate_budget_are_visible_and_never_reset() {
    let mut value = result();
    value.content = "界".repeat(MAX_TEXT);
    let cell = BridgeNativeHistory::default()
        .observe(Some("thread"), Some("turn"), &notification(value))
        .unwrap();
    assert!(text(&cell).contains("Display escaped or truncated"));
    let mut history = BridgeNativeHistory::default();
    let mut warning = false;
    for n in 0..MAX_CELLS + 1 {
        let mut value = result();
        value.call_id = format!("call-{n}");
        value.content = "x".repeat(MAX_TEXT);
        let mut event = notification(value);
        event.call_id = format!("call-{n}");
        let Some(cell) = history.observe(Some("thread"), Some("turn"), &event) else {
            break;
        };
        if text(&cell).contains("stream loss, duplicate or limit") {
            warning = true;
            break;
        }
    }
    assert!(warning);
    assert!(history.bytes <= MAX_BYTES);
    assert!(history.seen.len() <= MAX_CELLS);
    assert!(
        history
            .observe(Some("thread"), Some("turn"), &notification(result()))
            .is_none()
    );
}

#[test]
fn cell_count_cap_and_remote_serialization_do_not_expose_native_result() {
    let event = notification(result());
    let wire = serde_json::to_string(&event).unwrap();
    assert!(!wire.contains("AC10_WORK_V2"));
    assert!(!wire.contains("h7_0123456789ab"));
    assert!(!format!("{event:?}").contains("actual-return"));
    let mut history = BridgeNativeHistory::default();
    for n in 0..MAX_CELLS {
        let mut value = result();
        value.call_id = format!("call-{n}");
        let mut event = notification(value);
        event.call_id = format!("call-{n}");
        assert!(
            history
                .observe(Some("thread"), Some("turn"), &event)
                .unwrap()
                .value
                .is_some()
        );
    }
    let mut value = result();
    value.call_id = "one-more".to_owned();
    let mut event = notification(value);
    event.call_id = "one-more".to_owned();
    assert!(
        history
            .observe(Some("thread"), Some("turn"), &event)
            .unwrap()
            .value
            .is_none()
    );
    assert_eq!(history.seen.len(), MAX_CELLS);
}

#[test]
fn final_loss_result_immediately_invalidates_retained_cells_without_healing() {
    for local in [false, true] {
        let mut history = BridgeNativeHistory::default();
        let first = history
            .observe(Some("thread"), Some("turn"), &notification(result()))
            .unwrap();
        let mut loss = result();
        loss.call_id = "call-2".to_owned();
        loss.local_truncated = local;
        loss.observation_complete = false;
        let mut event = notification(loss);
        event.call_id = "call-2".to_owned();
        let last = history
            .observe(Some("thread"), Some("turn"), &event)
            .unwrap();
        assert!(text(&first).contains("UNQUALIFIED return"));
        assert!(text(&last).contains("UNQUALIFIED return"));
        assert!(
            history
                .observe(Some("thread"), Some("turn"), &notification(result()))
                .is_none()
        );
    }
}

#[test]
fn late_own_retained_turn_loss_applies_but_foreign_or_unseen_loss_is_isolated() {
    let mut history = BridgeNativeHistory::default();
    let first = history
        .observe(Some("thread"), Some("turn"), &notification(result()))
        .unwrap();
    let mut loss = notification(result());
    loss.native_result = Some(NativeToolObservation::Invalidated {
        reason: "native_stream_unqualified".to_owned(),
    });
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
    let mut second = notification(result());
    second.turn_id = "next".to_owned();
    second.call_id = "call-2".to_owned();
    if let Some(NativeToolObservation::Result(value)) = &mut second.native_result {
        value.call_id = "call-2".to_owned();
    }
    let next_cell = history
        .observe(Some("thread"), Some("next"), &second)
        .unwrap();
    loss.turn_id = "turn".to_owned();
    assert!(
        history
            .observe(Some("thread"), Some("next"), &loss)
            .is_some()
    );
    assert!(text(&first).contains("UNQUALIFIED return"));
    assert!(text(&next_cell).contains("UNQUALIFIED return"));
    let mut next = notification(result());
    next.turn_id = "next".to_owned();
    assert!(
        history
            .observe(Some("thread"), Some("next"), &next)
            .is_none()
    );
}

#[test]
fn unicode_formats_are_visible_unqualified_and_cannot_be_identity() {
    for c in [
        '\u{200b}',
        '\u{200e}',
        '\u{200f}',
        '\u{061c}',
        '\u{0600}',
        '\u{ad}',
        '\u{2060}',
        '\u{fe0f}',
        '\u{e0100}',
    ] {
        for command in [false, true] {
            let mut history = BridgeNativeHistory::default();
            let mut value = result();
            if command {
                value.command.push(c);
            } else {
                value.content.push(c);
            }
            let cell = history
                .observe(Some("thread"), Some("turn"), &notification(value))
                .unwrap();
            assert!(text(&cell).contains("UNQUALIFIED return"));
            assert!(!text(&cell).contains(c));
        }
        let mut history = BridgeNativeHistory::default();
        let mut value = result();
        value.native_session_id.push(c);
        let cell = history
            .observe(Some("thread"), Some("turn"), &notification(value))
            .unwrap();
        assert!(cell.value.is_none());
    }
    let mut history = BridgeNativeHistory::default();
    let mut value = result();
    value.command = "printf 中文".to_owned();
    let cell = history
        .observe(Some("thread"), Some("turn"), &notification(value))
        .unwrap();
    assert!(text(&cell).contains("matched terminal return"));
    assert!(text(&cell).contains("printf 中文"));
}

#[test]
fn native_return_ui_snapshots_and_narrow_wrapped_body_boundary() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Paragraph;
    use ratatui::widgets::Widget;
    use ratatui::widgets::Wrap;
    let mut history = BridgeNativeHistory::default();
    let cell = history
        .observe(Some("thread"), Some("turn"), &notification(result()))
        .unwrap();
    // Snapshot expectations are hand-authored, UNVERIFIED until required CI.
    insta::assert_snapshot!(&text(&cell), @r"
External native Bash: matched terminal return
  Thread: thread / turn: turn
  Native session: native-session / turn: native-turn
  Call: call-1
  Command: python3 .local/h7_0123456789ab_01.py
  Status: completed / exit: 0 / success: true / timeout: false
  Truncation: native=false local=false / observation_complete=true
  Serialized bytes: original=26 returned=26
  Native returned text (not reconstructed stdout):
    | AC10_WORK_V2 actual-return
");
    let warning = history.fault().unwrap();
    insta::assert_snapshot!(&text(&cell), @r"
External native Bash: UNQUALIFIED return
  Thread: thread / turn: turn
  Native session: native-session / turn: native-turn
  Call: call-1
  Command: python3 .local/h7_0123456789ab_01.py
  Status: completed / exit: 0 / success: true / timeout: false
  Truncation: native=false local=false / observation_complete=true
  Serialized bytes: original=26 returned=26
  Native returned text (not reconstructed stdout):
    | AC10_WORK_V2 actual-return
");
    insta::assert_snapshot!(&text(&warning), @"External native observation: UNQUALIFIED (stream loss, duplicate or limit)");
    // Real cell-derived command and returned body, not assistant/display preview.
    let mut value = result();
    value.command = "abcdefghijklmnop".to_owned();
    value.content = "body body body\nStatus: body only".to_owned();
    let mut narrow_history = BridgeNativeHistory::default();
    let narrow = narrow_history
        .observe(Some("thread"), Some("turn"), &notification(value))
        .unwrap();
    let rendered = narrow.display_lines(16);
    let lines = std::iter::once(rendered[4].clone())
        .chain(rendered[9..].iter().cloned())
        .collect::<Vec<_>>();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 8));
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(buffer.area, &mut buffer);
    let rows = buffer
        .content
        .chunks(16)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(&rows, @r"
      Command:
    abcdefghijklmnop
        | body body
    body
        | Status:
    body only
    
    
    ");
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
