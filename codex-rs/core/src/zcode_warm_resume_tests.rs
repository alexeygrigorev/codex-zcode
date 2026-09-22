//! Tests for the catch-up diagnostics of the resume path (issue #48): the
//! summary line is what an operator has to reconstruct what the warm core
//! did while no bridge was watching, so its shape is pinned here. The wire
//! behavior itself is covered by the fake-server tests in
//! `zcode_warm_tests.rs`.

use super::missed_events_summary;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn empty_gap_yields_no_summary() {
    assert_eq!(missed_events_summary("sess_fake", 7, &[]), None);
}

#[test]
fn summary_counts_types_and_names_the_latest_terminal_turn() {
    let events = vec![
        json!({"seq": 8, "type": "model.streaming", "payload": {"kind": "text_delta", "delta": "lo"}}),
        json!({"seq": 9, "type": "turn.completed", "payload": {"response": "step", "resultType": "success"}}),
        json!({"seq": 10, "type": "session.updated", "payload": {}}),
        json!({"seq": 11, "type": "turn.completed", "payload": {"response": "retry", "resultType": "budget_exhausted"}}),
    ];
    assert_eq!(
        missed_events_summary("sess_fake", 7, &events),
        Some(
            "ZCode warm session sess_fake: 4 event(s) while disconnected (after seq 7): \
             model.streaming×1, session.updated×1, turn.completed×2; latest terminal turn: \
             turn.completed(budget_exhausted)"
                .to_string()
        )
    );
}

#[test]
fn summary_reports_a_failed_terminal_turn() {
    let events = vec![json!({
        "seq": 8,
        "type": "turn.failed",
        "payload": {"error": {"message": "core restarted"}},
    })];
    assert_eq!(
        missed_events_summary("sess_fake", 7, &events),
        Some(
            "ZCode warm session sess_fake: 1 event(s) while disconnected (after seq 7): \
             turn.failed×1; latest terminal turn: turn.failed(unknown)"
                .to_string()
        )
    );
}

#[test]
fn summary_survives_untyped_events() {
    let events = vec![
        json!({"seq": 8}),
        json!({"seq": 9, "type": "session.updated"}),
    ];
    assert_eq!(
        missed_events_summary("sess_fake", 7, &events),
        Some(
            "ZCode warm session sess_fake: 2 event(s) while disconnected (after seq 7): \
             session.updated×1, unknown×1; latest terminal turn: none"
                .to_string()
        )
    );
}
