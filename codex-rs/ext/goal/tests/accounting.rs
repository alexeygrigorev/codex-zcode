#![allow(dead_code)]

#[path = "../src/accounting.rs"]
mod accounting;

use accounting::BudgetLimitedGoalDisposition;
use accounting::GoalAccountingState;
use accounting::GoalIterationVerdict;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolName;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::TokenUsage;
use codex_state::ThreadGoalStatus;
use pretty_assertions::assert_eq;

#[test]
fn goal_accounting_uses_turn_start_baseline_for_exact_deltas() {
    let state = GoalAccountingState::default();
    state.start_turn(
        "turn-1",
        ModeKind::Default,
        &token_usage(
            /*input_tokens*/ 100, /*cached_input_tokens*/ 10, /*output_tokens*/ 30,
            /*reasoning_output_tokens*/ 5, /*total_tokens*/ 135,
        ),
    );

    let recorded = state
        .record_token_usage(
            "turn-1",
            &token_usage(
                /*input_tokens*/ 120, /*cached_input_tokens*/ 14,
                /*output_tokens*/ 42, /*reasoning_output_tokens*/ 8,
                /*total_tokens*/ 162,
            ),
        )
        .expect("token delta should be recorded");

    assert_eq!(28, recorded.turn_delta);
    assert_eq!(28, recorded.thread_unflushed_delta);
}

#[test]
fn goal_accounting_ignores_plan_mode_turns() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Plan, &TokenUsage::default());

    let recorded = state.record_token_usage(
        "turn-1",
        &token_usage(
            /*input_tokens*/ 20, /*cached_input_tokens*/ 5, /*output_tokens*/ 8,
            /*reasoning_output_tokens*/ 2, /*total_tokens*/ 30,
        ),
    );

    assert_eq!(None, recorded);
}

#[test]
fn empty_continuations_require_three_turns_without_activity_or_goal_changes() {
    let empty_final = TurnItem::AgentMessage(AgentMessageItem {
        id: "empty".into(),
        content: vec![AgentMessageContent::Text { text: " \n".into() }],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    });
    for (interruption, blocking_turn) in [
        ("none", 3),
        ("user", 6),
        ("tool", 6),
        ("goal", 5),
        ("reset", 6),
        ("missing final", 6),
    ] {
        let state = GoalAccountingState::default();
        for turn in 1..=blocking_turn {
            let id = turn.to_string();
            let goal_id = if interruption == "goal" && turn >= 3 {
                "new"
            } else {
                "goal"
            };
            state.start_turn(&id, ModeKind::Default, &TokenUsage::default());
            state.mark_turn_goal_active(&id, goal_id);
            if !(turn == 3 && interruption == "missing final") {
                state.record_item(&id, &empty_final);
            }
            // Admission can finish after output arrives, but before turn-stop evaluation.
            if !(turn == 3 && interruption == "user") {
                state.mark_goal_continuation(id.clone());
            }
            if turn == 3 && interruption == "tool" {
                state.record_tool_outcome(
                    &id,
                    &ToolName::plain("shell"),
                    ToolCallOutcome::Completed { success: false },
                );
            }
            if turn == 3 && interruption == "reset" {
                state.reset_empty_responses();
            }
            assert_eq!(
                (turn == blocking_turn).then(|| goal_id.to_string()),
                state.empty_response_goal(&id),
                "interruption: {interruption}, turn: {turn}",
            );
            state.finish_turn(&id);
        }
    }
}

#[test]
fn execution_failures_do_not_transfer_to_a_replacement_goal() {
    let state = GoalAccountingState::default();

    for (turn, goal_id, replacement_goal_id) in [
        (1, "first-goal", Some("second-goal")),
        (2, "second-goal", None),
        (3, "second-goal", None),
        (4, "second-goal", None),
    ] {
        let turn_id = format!("turn-{turn}");
        state.start_turn(&turn_id, ModeKind::Default, &TokenUsage::default());
        state.mark_turn_goal_active(&turn_id, goal_id);
        state.record_tool_outcome(
            &turn_id,
            &ToolName::plain("exec"),
            ToolCallOutcome::Failed {
                handler_executed: true,
            },
        );
        if let Some(replacement_goal_id) = replacement_goal_id {
            state.mark_current_turn_goal_active(replacement_goal_id);
        }

        assert_eq!(
            (turn == 4).then(|| "second-goal".to_string()),
            state.execution_failure_goal(&turn_id)
        );
        state.finish_turn(&turn_id);
        if turn == 3 {
            state.reset_idle_progress_baseline_and_clear_active_goal();
        }
    }
}

#[test]
fn script_errors_and_failures_before_execution_do_not_block_goals() {
    for outcome in [
        ToolCallOutcome::Completed { success: false },
        ToolCallOutcome::Failed {
            handler_executed: false,
        },
    ] {
        let state = GoalAccountingState::default();
        for turn in 1..=3 {
            let turn_id = format!("turn-{turn}");
            state.start_turn(&turn_id, ModeKind::Default, &TokenUsage::default());
            state.mark_turn_goal_active(&turn_id, "goal");
            state.record_tool_outcome(&turn_id, &ToolName::plain("exec"), outcome);

            assert_eq!(None, state.execution_failure_goal(&turn_id));
            state.finish_turn(&turn_id);
        }
    }
}

#[test]
fn successful_tool_resets_failures_before_an_interrupted_turn_ends() {
    let state = GoalAccountingState::default();

    for (turn, tool_name, outcome) in [
        (
            1,
            "exec",
            ToolCallOutcome::Failed {
                handler_executed: true,
            },
        ),
        (
            2,
            "exec",
            ToolCallOutcome::Failed {
                handler_executed: true,
            },
        ),
        (3, "shell", ToolCallOutcome::Completed { success: true }),
        (
            4,
            "exec",
            ToolCallOutcome::Failed {
                handler_executed: true,
            },
        ),
    ] {
        let turn_id = format!("turn-{turn}");
        state.start_turn(&turn_id, ModeKind::Default, &TokenUsage::default());
        state.mark_turn_goal_active(&turn_id, "goal");
        state.record_tool_outcome(&turn_id, &ToolName::plain(tool_name), outcome);
        if turn != 3 {
            assert_eq!(None, state.execution_failure_goal(&turn_id));
        }
        state.finish_turn(&turn_id);
    }
}

#[test]
fn goal_accounting_preserves_concurrent_descendant_usage_across_checkpoints() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_current_turn_goal_active("goal-1");
    let first_usage = token_usage(
        /*input_tokens*/ 20, /*cached_input_tokens*/ 5, /*output_tokens*/ 8,
        /*reasoning_output_tokens*/ 0, /*total_tokens*/ 28,
    );
    let second_usage = token_usage(
        /*input_tokens*/ 8, /*cached_input_tokens*/ 2, /*output_tokens*/ 4,
        /*reasoning_output_tokens*/ 0, /*total_tokens*/ 12,
    );
    std::thread::scope(|scope| {
        scope.spawn(|| state.record_descendant_token_usage(&first_usage));
        scope.spawn(|| state.record_descendant_token_usage(&second_usage));
    });

    let first = state
        .progress_snapshot("turn-1")
        .expect("descendant usage should create a progress snapshot");
    assert_eq!(33, first.token_delta);

    state.record_descendant_token_usage(&token_usage(
        /*input_tokens*/ 6, /*cached_input_tokens*/ 1, /*output_tokens*/ 3,
        /*reasoning_output_tokens*/ 0, /*total_tokens*/ 9,
    ));
    state.mark_progress_accounted_for_status(
        "turn-1",
        &first,
        ThreadGoalStatus::Active,
        BudgetLimitedGoalDisposition::KeepActive,
    );

    let second = state
        .progress_snapshot("turn-1")
        .expect("usage received during accounting should remain pending");
    assert_eq!(8, second.token_delta);
}

fn token_usage(
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    total_tokens: i64,
) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        codex_rollout_budget_units: None,
    }
}

#[test]
fn no_progress_continuations_block_after_three_turns_without_successful_tools() {
    let text_final = TurnItem::AgentMessage(AgentMessageItem {
        id: "text".into(),
        content: vec![AgentMessageContent::Text {
            text: "Still working on it".into(),
        }],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    });
    for (interruption, blocking_turn) in [
        ("none", Some(3)),
        ("user", Some(6)),
        ("tool", Some(6)),
        ("failed tool", Some(3)),
        ("goal", Some(5)),
        ("manual", None),
    ] {
        let state = GoalAccountingState::default();
        let turns = blocking_turn.unwrap_or(6);
        let mut streak: u8 = 0;
        for turn in 1..=turns {
            let turn_id = format!("turn-{turn}");
            let goal_id = if interruption == "goal" && turn >= 3 {
                "new"
            } else {
                "goal"
            };
            let goal_changed = interruption == "goal" && turn == 3;
            let tool_success = interruption == "tool" && turn == 3;
            state.start_turn(&turn_id, ModeKind::Default, &TokenUsage::default());
            state.mark_turn_goal_active(&turn_id, goal_id);
            state.record_item(&turn_id, &text_final);
            if interruption == "failed tool" {
                state.record_tool_outcome(
                    &turn_id,
                    &ToolName::plain("exec"),
                    ToolCallOutcome::Failed {
                        handler_executed: true,
                    },
                );
            }
            if tool_success {
                state.record_tool_outcome(
                    &turn_id,
                    &ToolName::plain("shell"),
                    ToolCallOutcome::Completed { success: true },
                );
            }
            // Admission can finish after output arrives, but before turn-stop evaluation.
            let automatic = interruption != "manual" && !(interruption == "user" && turn == 3);
            if automatic {
                state.mark_goal_continuation(turn_id.clone());
            }

            if goal_changed {
                streak = 0;
            }
            if tool_success || !automatic {
                streak = 0;
            } else {
                streak += 1;
            }
            let expected = (automatic && streak >= 3).then(|| goal_id.to_string());
            assert_eq!(
                expected,
                state.no_progress_goal(&turn_id),
                "interruption: {interruption}, turn: {turn}",
            );
            state.finish_turn(&turn_id);
        }
    }
}

#[test]
fn goal_iterations_record_verdicts_with_per_cycle_cost() {
    let state = GoalAccountingState::default();
    state.start_turn(
        "turn-1",
        ModeKind::Default,
        &token_usage(
            /*input_tokens*/ 100, /*cached_input_tokens*/ 0, /*output_tokens*/ 0,
            /*reasoning_output_tokens*/ 0, /*total_tokens*/ 100,
        ),
    );
    state.mark_current_turn_goal_active("goal-1");

    // First iteration: two attributed tool calls and a token delta.
    state.record_tool_outcome(
        "turn-1",
        &ToolName::plain("exec"),
        ToolCallOutcome::Completed { success: true },
    );
    state.record_tool_outcome(
        "turn-1",
        &ToolName::plain("shell"),
        ToolCallOutcome::Completed { success: true },
    );
    state
        .record_token_usage(
            "turn-1",
            &token_usage(
                /*input_tokens*/ 160, /*cached_input_tokens*/ 10,
                /*output_tokens*/ 40, /*reasoning_output_tokens*/ 0,
                /*total_tokens*/ 190,
            ),
        )
        .expect("delta recorded");
    let first = state.record_goal_iteration(GoalIterationVerdict::NotPassed);
    assert_eq!(first.index, 1);
    assert_eq!(first.verdict, GoalIterationVerdict::NotPassed);
    assert_eq!(first.tool_calls, 2);
    // Usage delta (60 input, 10 cached, 40 output) counts
    // (60 - 10) non-cached input plus 40 output = 90 goal tokens.
    assert_eq!(first.token_delta, 90);

    // Second iteration accrues only what came after the first verification.
    state
        .record_token_usage(
            "turn-1",
            &token_usage(
                /*input_tokens*/ 170, /*cached_input_tokens*/ 20,
                /*output_tokens*/ 45, /*reasoning_output_tokens*/ 0,
                /*total_tokens*/ 195,
            ),
        )
        .expect("delta recorded");
    let second = state.record_goal_iteration(GoalIterationVerdict::Passed);
    assert_eq!(second.index, 2);
    assert_eq!(second.verdict, GoalIterationVerdict::Passed);
    // Usage delta (10 input, 10 cached, 5 output) counts
    // (10 - 10) + 5 = 5 goal tokens.
    assert_eq!(second.token_delta, 5);
    assert_eq!(second.tool_calls, 0);

    let (count, iterations) = state.goal_iterations();
    assert_eq!(count, 2);
    assert_eq!(iterations, vec![first, second]);
}

#[test]
fn goal_iterations_reset_when_the_goal_clears() {
    let state = GoalAccountingState::default();
    state.start_turn(
        "turn-1",
        ModeKind::Default,
        &token_usage(
            /*input_tokens*/ 100, /*cached_input_tokens*/ 0, /*output_tokens*/ 0,
            /*reasoning_output_tokens*/ 0, /*total_tokens*/ 100,
        ),
    );
    state.mark_current_turn_goal_active("goal-1");
    state
        .record_token_usage(
            "turn-1",
            &token_usage(
                /*input_tokens*/ 150, /*cached_input_tokens*/ 0,
                /*output_tokens*/ 20, /*reasoning_output_tokens*/ 0,
                /*total_tokens*/ 170,
            ),
        )
        .expect("delta recorded");
    state.record_goal_iteration(GoalIterationVerdict::Error);

    state.clear_current_turn_goal();

    assert_eq!((0, Vec::new()), state.goal_iterations());
}

#[test]
fn goal_iterations_history_stays_bounded() {
    let state = GoalAccountingState::default();
    state.start_turn(
        "turn-1",
        ModeKind::Default,
        &token_usage(
            /*input_tokens*/ 0, /*cached_input_tokens*/ 0, /*output_tokens*/ 0,
            /*reasoning_output_tokens*/ 0, /*total_tokens*/ 0,
        ),
    );
    state.mark_current_turn_goal_active("goal-1");
    for _ in 0..12 {
        state.record_goal_iteration(GoalIterationVerdict::NotPassed);
    }

    let (count, iterations) = state.goal_iterations();
    assert_eq!(count, 12);
    assert_eq!(iterations.len(), accounting::MAX_RECORDED_GOAL_ITERATIONS);
    assert_eq!(iterations.first().expect("non-empty").index, 3);
    assert_eq!(iterations.last().expect("non-empty").index, 12);
}
