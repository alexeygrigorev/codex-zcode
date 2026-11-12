use crate::runtime::GoalAccountingMode;
use crate::runtime::GoalAccountingOutcome;
use crate::runtime::GoalUpdate;
use crate::runtime::StateRuntime;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

const TURN_ID: &str = "turn-1";

fn source_thread_id() -> ThreadId {
    ThreadId::from_string("00000000-0000-0000-0000-000000000123").expect("valid thread id")
}

fn target_thread_id() -> ThreadId {
    ThreadId::from_string("00000000-0000-0000-0000-000000000456").expect("valid thread id")
}

async fn test_runtime() -> std::sync::Arc<StateRuntime> {
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(unique_temp_dir().as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state db should initialize");
    let sqlite_home = runtime.sqlite().home();
    for thread_id in [source_thread_id(), target_thread_id()] {
        let metadata = test_thread_metadata(sqlite_home, thread_id, sqlite_home.join("workspace"));
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("test thread should be upserted");
    }
    runtime
}

async fn active_goal(runtime: &StateRuntime) -> crate::ThreadGoal {
    runtime
        .thread_goals()
        .replace_thread_goal(
            source_thread_id(),
            "ship the fix",
            crate::ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await
        .expect("goal replacement should succeed")
}

async fn claim(runtime: &StateRuntime) -> Option<String> {
    runtime
        .thread_goals()
        .claim_active_thread_goal_for_interrupt(source_thread_id(), TURN_ID)
        .await
        .expect("claim should not error")
}

async fn pause(runtime: &StateRuntime, goal_id: &str) -> Option<crate::ThreadGoal> {
    runtime
        .thread_goals()
        .pause_active_thread_goal_for_interrupted_turn(source_thread_id(), goal_id, TURN_ID)
        .await
        .expect("interrupt pause should not error")
}

/// Seeds an active goal, claims it, and pauses it for the interrupt of `TURN_ID`.
async fn paused_by_owned_interrupt(runtime: &StateRuntime) -> crate::ThreadGoal {
    let goal = active_goal(runtime).await;
    assert_eq!(Some(goal.goal_id.clone()), claim(runtime).await);
    pause(runtime, &goal.goal_id)
        .await
        .expect("active goal should be paused")
}

async fn inherit(runtime: &StateRuntime, goal_id: &str, turn_id: &str) -> bool {
    runtime
        .thread_goals()
        .inherit_goal_paused_by_interrupted_turn(
            source_thread_id(),
            target_thread_id(),
            goal_id,
            turn_id,
        )
        .await
        .expect("inheritance should not error")
}

async fn assert_not_inherited(runtime: &StateRuntime) {
    assert_eq!(
        None,
        runtime
            .thread_goals()
            .get_thread_goal(target_thread_id())
            .await
            .expect("goal read should succeed")
    );
}

#[derive(Clone, Copy, Debug)]
enum Write {
    SameStatusActive,
    SameStatusPause,
    PauseThenResume,
    ObjectiveEdit,
    BudgetEdit,
    Block,
    Complete,
    UsageAccountingWithoutTurn,
    Replace,
    Snapshot,
    Delete,
}

const ALL_WRITES: [Write; 11] = [
    Write::SameStatusActive,
    Write::SameStatusPause,
    Write::PauseThenResume,
    Write::ObjectiveEdit,
    Write::BudgetEdit,
    Write::Block,
    Write::Complete,
    Write::UsageAccountingWithoutTurn,
    Write::Replace,
    Write::Snapshot,
    Write::Delete,
];

async fn apply(runtime: &StateRuntime, goal: &crate::ThreadGoal, write: Write) {
    let goals = runtime.thread_goals();
    let update = |status, objective: Option<&str>, token_budget| GoalUpdate {
        objective: objective.map(str::to_string),
        status,
        token_budget,
        expected_goal_id: Some(goal.goal_id.clone()),
    };
    let status_update = |status| {
        update(
            Some(status),
            /*objective*/ None,
            /*token_budget*/ None,
        )
    };
    match write {
        Write::SameStatusActive => {
            goals
                .update_thread_goal(
                    source_thread_id(),
                    status_update(crate::ThreadGoalStatus::Active),
                )
                .await
                .expect("write should succeed");
        }
        Write::SameStatusPause => {
            goals
                .update_thread_goal(
                    source_thread_id(),
                    status_update(crate::ThreadGoalStatus::Paused),
                )
                .await
                .expect("write should succeed");
        }
        Write::PauseThenResume => {
            for status in [
                crate::ThreadGoalStatus::Paused,
                crate::ThreadGoalStatus::Active,
            ] {
                goals
                    .update_thread_goal(source_thread_id(), status_update(status))
                    .await
                    .expect("write should succeed");
            }
        }
        Write::ObjectiveEdit => {
            goals
                .update_thread_goal(
                    source_thread_id(),
                    update(
                        /*status*/ None,
                        Some("edited objective"),
                        /*token_budget*/ None,
                    ),
                )
                .await
                .expect("write should succeed");
        }
        Write::BudgetEdit => {
            goals
                .update_thread_goal(
                    source_thread_id(),
                    update(
                        /*status*/ None,
                        /*objective*/ None,
                        Some(Some(1_000_000)),
                    ),
                )
                .await
                .expect("write should succeed");
        }
        Write::Block => {
            goals
                .update_thread_goal(
                    source_thread_id(),
                    status_update(crate::ThreadGoalStatus::Blocked),
                )
                .await
                .expect("write should succeed");
        }
        Write::Complete => {
            goals
                .update_thread_goal(
                    source_thread_id(),
                    status_update(crate::ThreadGoalStatus::Complete),
                )
                .await
                .expect("write should succeed");
        }
        Write::UsageAccountingWithoutTurn => {
            goals
                .account_thread_goal_usage(
                    source_thread_id(),
                    /*time_delta_seconds*/ 1,
                    /*token_delta*/ 1,
                    GoalAccountingMode::ActiveOrStopped,
                    Some(&goal.goal_id),
                )
                .await
                .expect("write should succeed");
        }
        Write::Replace => {
            goals
                .replace_thread_goal(
                    source_thread_id(),
                    "replacement",
                    crate::ThreadGoalStatus::Active,
                    /*token_budget*/ None,
                )
                .await
                .expect("write should succeed");
        }
        Write::Snapshot => {
            goals
                .replace_thread_goal_snapshot(goal)
                .await
                .expect("write should succeed");
        }
        Write::Delete => {
            goals
                .delete_thread_goal(source_thread_id())
                .await
                .expect("write should succeed");
        }
    }
}

#[tokio::test]
async fn owned_interrupt_pause_is_inherited_as_active_and_source_stays_paused() {
    let runtime = test_runtime().await;
    let paused = paused_by_owned_interrupt(&runtime).await;
    assert_eq!(crate::ThreadGoalStatus::Paused, paused.status);

    assert!(inherit(&runtime, &paused.goal_id, TURN_ID).await);

    let goals = runtime.thread_goals();
    assert_eq!(
        Some(paused.clone()),
        goals
            .get_thread_goal(source_thread_id())
            .await
            .expect("goal read should succeed")
    );
    assert_eq!(
        Some(crate::ThreadGoal {
            thread_id: target_thread_id(),
            status: crate::ThreadGoalStatus::Active,
            ..paused
        }),
        goals
            .get_thread_goal(target_thread_id())
            .await
            .expect("goal read should succeed")
    );
    assert!(
        goals
            .has_thread_goal_continuation_deferral(target_thread_id())
            .await
            .expect("deferral read should succeed")
    );
}

#[tokio::test]
async fn owned_interrupt_pause_is_consumed_by_the_first_inheritance() {
    let runtime = test_runtime().await;
    let paused = paused_by_owned_interrupt(&runtime).await;

    assert!(inherit(&runtime, &paused.goal_id, TURN_ID).await);
    runtime
        .thread_goals()
        .delete_thread_goal(target_thread_id())
        .await
        .expect("target goal delete should succeed");

    assert!(!inherit(&runtime, &paused.goal_id, TURN_ID).await);
    assert_not_inherited(&runtime).await;
}

#[tokio::test]
async fn interrupt_pause_of_another_turn_or_goal_is_not_inherited() {
    let runtime = test_runtime().await;
    let paused = paused_by_owned_interrupt(&runtime).await;

    assert!(!inherit(&runtime, &paused.goal_id, "turn-2").await);
    assert!(!inherit(&runtime, "other-goal", TURN_ID).await);
    assert_not_inherited(&runtime).await;
}

#[tokio::test]
async fn interrupt_without_a_claim_pauses_the_goal_but_records_no_ownership() {
    let runtime = test_runtime().await;
    let goal = active_goal(&runtime).await;

    let paused = pause(&runtime, &goal.goal_id)
        .await
        .expect("active goal should be paused");

    assert_eq!(crate::ThreadGoalStatus::Paused, paused.status);
    assert!(!inherit(&runtime, &goal.goal_id, TURN_ID).await);
    assert_not_inherited(&runtime).await;
}

#[tokio::test]
async fn goal_that_is_not_active_cannot_be_claimed_or_paused() {
    let runtime = test_runtime().await;
    let goals = runtime.thread_goals();
    let goal = goals
        .replace_thread_goal(
            source_thread_id(),
            "already paused",
            crate::ThreadGoalStatus::Paused,
            /*token_budget*/ None,
        )
        .await
        .expect("goal replacement should succeed");

    assert_eq!(None, claim(&runtime).await);
    assert_eq!(None, pause(&runtime, &goal.goal_id).await);
    assert!(!inherit(&runtime, &goal.goal_id, TURN_ID).await);
    assert_not_inherited(&runtime).await;
}

#[tokio::test]
async fn every_write_between_the_claim_and_the_pause_refuses_ownership() {
    for write in ALL_WRITES {
        let runtime = test_runtime().await;
        let goal = active_goal(&runtime).await;
        assert_eq!(Some(goal.goal_id.clone()), claim(&runtime).await);

        apply(&runtime, &goal, write).await;
        // Whatever is left of the goal is paused by the interrupt as usual.
        let _ = pause(&runtime, &goal.goal_id).await;

        assert!(
            !inherit(&runtime, &goal.goal_id, TURN_ID).await,
            "{write:?}"
        );
        assert_not_inherited(&runtime).await;
    }
}

#[tokio::test]
async fn every_write_after_the_pause_invalidates_ownership() {
    for write in ALL_WRITES {
        let runtime = test_runtime().await;
        let paused = paused_by_owned_interrupt(&runtime).await;

        apply(&runtime, &paused, write).await;

        assert!(
            !inherit(&runtime, &paused.goal_id, TURN_ID).await,
            "{write:?}"
        );
        assert_not_inherited(&runtime).await;
    }
}

#[tokio::test]
async fn usage_accounting_of_the_claimed_turn_keeps_the_claim() {
    let runtime = test_runtime().await;
    let goal = active_goal(&runtime).await;
    assert_eq!(Some(goal.goal_id.clone()), claim(&runtime).await);

    runtime
        .thread_goals()
        .account_thread_goal_usage_for_turn(
            source_thread_id(),
            /*time_delta_seconds*/ 3,
            /*token_delta*/ 7,
            GoalAccountingMode::ActiveOnly,
            &goal.goal_id,
            TURN_ID,
        )
        .await
        .expect("accounting should succeed");
    let paused = pause(&runtime, &goal.goal_id)
        .await
        .expect("active goal should be paused");

    assert_eq!((7, 3), (paused.tokens_used, paused.time_used_seconds));
    assert!(inherit(&runtime, &goal.goal_id, TURN_ID).await);
}

#[tokio::test]
async fn usage_accounting_of_another_turn_does_not_keep_the_claim() {
    let runtime = test_runtime().await;
    let goal = active_goal(&runtime).await;
    assert_eq!(Some(goal.goal_id.clone()), claim(&runtime).await);

    runtime
        .thread_goals()
        .account_thread_goal_usage_for_turn(
            source_thread_id(),
            /*time_delta_seconds*/ 3,
            /*token_delta*/ 7,
            GoalAccountingMode::ActiveOnly,
            &goal.goal_id,
            "turn-2",
        )
        .await
        .expect("accounting should succeed");
    pause(&runtime, &goal.goal_id)
        .await
        .expect("active goal should be paused");

    assert!(!inherit(&runtime, &goal.goal_id, TURN_ID).await);
}

#[tokio::test]
async fn released_claim_records_no_ownership_and_a_recorded_pause_survives_release() {
    let runtime = test_runtime().await;
    let goal = active_goal(&runtime).await;
    assert_eq!(Some(goal.goal_id.clone()), claim(&runtime).await);
    runtime
        .thread_goals()
        .release_thread_goal_interrupt_claim(source_thread_id(), TURN_ID)
        .await
        .expect("release should succeed");
    pause(&runtime, &goal.goal_id)
        .await
        .expect("active goal should be paused");
    assert!(!inherit(&runtime, &goal.goal_id, TURN_ID).await);

    let runtime = test_runtime().await;
    let paused = paused_by_owned_interrupt(&runtime).await;
    runtime
        .thread_goals()
        .release_thread_goal_interrupt_claim(source_thread_id(), TURN_ID)
        .await
        .expect("release should succeed");
    assert!(inherit(&runtime, &paused.goal_id, TURN_ID).await);
}

#[tokio::test]
async fn usage_accounting_that_exhausts_the_budget_does_not_keep_the_claim() {
    let runtime = test_runtime().await;
    let goals = runtime.thread_goals();
    let goal = goals
        .replace_thread_goal(
            source_thread_id(),
            "ship the fix",
            crate::ThreadGoalStatus::Active,
            /*token_budget*/ Some(5),
        )
        .await
        .expect("goal replacement should succeed");
    assert_eq!(Some(goal.goal_id.clone()), claim(&runtime).await);

    let outcome = goals
        .account_thread_goal_usage_for_turn(
            source_thread_id(),
            /*time_delta_seconds*/ 1,
            /*token_delta*/ 9,
            GoalAccountingMode::ActiveOnly,
            &goal.goal_id,
            TURN_ID,
        )
        .await
        .expect("accounting should succeed");

    let GoalAccountingOutcome::Updated(limited) = outcome else {
        panic!("accounting should update the goal");
    };
    assert_eq!(crate::ThreadGoalStatus::BudgetLimited, limited.status);
    assert_eq!(None, pause(&runtime, &goal.goal_id).await);
    assert!(!inherit(&runtime, &goal.goal_id, TURN_ID).await);
}
