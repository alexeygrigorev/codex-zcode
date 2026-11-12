use codex_protocol::ThreadId;
use codex_protocol::protocol::validate_thread_goal_objective;
use codex_state::StateRuntime;
use codex_state::ThreadGoalStatus;

/// A goal that was active when a fork began to interrupt `turn_id`.
///
/// This is only a claim: the source goal is carried into the fork as active only if the
/// interrupt of exactly this turn is still recorded as the cause of its pause.
pub(super) struct InterruptedActiveGoal {
    goal_id: String,
    turn_id: String,
}

impl InterruptedActiveGoal {
    /// Returns the claim when the thread's goal is active right now, before the interrupt.
    pub(super) async fn capture(
        state_db: &StateRuntime,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Option<Self>> {
        Ok(state_db
            .thread_goals()
            .get_thread_goal(thread_id)
            .await?
            .filter(|goal| goal.status == ThreadGoalStatus::Active)
            .map(|goal| Self {
                goal_id: goal.goal_id,
                turn_id: turn_id.to_string(),
            }))
    }
}

/// Copies the source goal into the fork and defers its continuation to the next turn.
///
/// The goal keeps the source status unless `interrupted_goal` is proven: the source goal is
/// still the one that was active before the fork's own interrupt and that interrupt paused it,
/// with no write to the goal since. Then, and only then, the fork starts out active. The source
/// goal is never changed.
pub(super) async fn inherit_thread_goal_snapshot(
    state_db: &StateRuntime,
    source_thread_id: ThreadId,
    target_thread_id: ThreadId,
    interrupted_goal: Option<&InterruptedActiveGoal>,
) -> anyhow::Result<bool> {
    let Some(mut goal) = state_db
        .thread_goals()
        .get_thread_goal(source_thread_id)
        .await?
    else {
        return Ok(false);
    };
    if let Err(err) = validate_thread_goal_objective(&goal.objective) {
        tracing::warn!(%source_thread_id, "skipping invalid inherited thread goal: {err}");
        return Ok(false);
    }

    if let Some(interrupted_goal) = interrupted_goal
        && state_db
            .thread_goals()
            .inherit_goal_paused_by_interrupted_turn(
                source_thread_id,
                target_thread_id,
                &interrupted_goal.goal_id,
                &interrupted_goal.turn_id,
            )
            .await?
    {
        return Ok(true);
    }

    goal.thread_id = target_thread_id;
    state_db
        .thread_goals()
        .replace_thread_goal_snapshot(&goal)
        .await?;
    Ok(true)
}

#[cfg(test)]
#[path = "thread_fork_goal_tests.rs"]
mod tests;
