use crate::runtime::GoalAccountingMode;
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

/// Seeds an active goal and pauses it for the interrupt of `TURN_ID`.
async fn paused_by_interrupt(runtime: &StateRuntime) -> crate::ThreadGoal {
    let goals = runtime.thread_goals();
    let goal = goals
        .replace_thread_goal(
            source_thread_id(),
            "ship the fix",
            crate::ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await
        .expect("goal replacement should succeed");
    goals
        .pause_active_thread_goal_for_interrupted_turn(source_thread_id(), &goal.goal_id, TURN_ID)
        .await
        .expect("interrupt pause should succeed")
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

#[tokio::test]
async fn owned_interrupt_pause_is_inherited_as_active_and_source_stays_paused() {
    let runtime = test_runtime().await;
    let paused = paused_by_interrupt(&runtime).await;
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
    let paused = paused_by_interrupt(&runtime).await;

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
    let paused = paused_by_interrupt(&runtime).await;

    assert!(!inherit(&runtime, &paused.goal_id, "turn-2").await);
    assert!(!inherit(&runtime, "other-goal", TURN_ID).await);
    assert_not_inherited(&runtime).await;
}

#[tokio::test]
async fn pausing_a_goal_that_is_not_active_records_no_ownership() {
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

    assert_eq!(
        None,
        goals
            .pause_active_thread_goal_for_interrupted_turn(
                source_thread_id(),
                &goal.goal_id,
                TURN_ID
            )
            .await
            .expect("interrupt pause should succeed")
    );
    assert!(!inherit(&runtime, &goal.goal_id, TURN_ID).await);
    assert_not_inherited(&runtime).await;
}

#[tokio::test]
async fn every_other_goal_write_invalidates_interrupt_pause_ownership() {
    enum Write {
        SameStatusPause,
        ObjectiveEdit,
        BudgetEdit,
        Block,
        Complete,
        UsageAccounting,
        Replace,
        Snapshot,
        Delete,
    }

    for write in [
        Write::SameStatusPause,
        Write::ObjectiveEdit,
        Write::BudgetEdit,
        Write::Block,
        Write::Complete,
        Write::UsageAccounting,
        Write::Replace,
        Write::Snapshot,
        Write::Delete,
    ] {
        let runtime = test_runtime().await;
        let paused = paused_by_interrupt(&runtime).await;
        let goals = runtime.thread_goals();
        let update = |status, objective: Option<&str>, token_budget| GoalUpdate {
            objective: objective.map(str::to_string),
            status,
            token_budget,
            expected_goal_id: Some(paused.goal_id.clone()),
        };
        match write {
            Write::SameStatusPause => {
                goals
                    .update_thread_goal(
                        source_thread_id(),
                        update(
                            Some(crate::ThreadGoalStatus::Paused),
                            /*objective*/ None,
                            /*token_budget*/ None,
                        ),
                    )
                    .await
                    .expect("pause should succeed");
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
                    .expect("edit should succeed");
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
                    .expect("budget edit should succeed");
            }
            Write::Block => {
                goals
                    .update_thread_goal(
                        source_thread_id(),
                        update(
                            Some(crate::ThreadGoalStatus::Blocked),
                            /*objective*/ None,
                            /*token_budget*/ None,
                        ),
                    )
                    .await
                    .expect("block should succeed");
            }
            Write::Complete => {
                goals
                    .update_thread_goal(
                        source_thread_id(),
                        update(
                            Some(crate::ThreadGoalStatus::Complete),
                            /*objective*/ None,
                            /*token_budget*/ None,
                        ),
                    )
                    .await
                    .expect("completion should succeed");
            }
            Write::UsageAccounting => {
                goals
                    .account_thread_goal_usage(
                        source_thread_id(),
                        /*time_delta_seconds*/ 1,
                        /*token_delta*/ 1,
                        GoalAccountingMode::ActiveOrStopped,
                        Some(&paused.goal_id),
                    )
                    .await
                    .expect("accounting should succeed");
            }
            Write::Replace => {
                goals
                    .replace_thread_goal(
                        source_thread_id(),
                        "replacement",
                        crate::ThreadGoalStatus::Paused,
                        /*token_budget*/ None,
                    )
                    .await
                    .expect("replacement should succeed");
            }
            Write::Snapshot => {
                goals
                    .replace_thread_goal_snapshot(&paused)
                    .await
                    .expect("snapshot should succeed");
            }
            Write::Delete => {
                goals
                    .delete_thread_goal(source_thread_id())
                    .await
                    .expect("clear should succeed");
            }
        }

        assert!(!inherit(&runtime, &paused.goal_id, TURN_ID).await);
        assert_not_inherited(&runtime).await;
    }
}
