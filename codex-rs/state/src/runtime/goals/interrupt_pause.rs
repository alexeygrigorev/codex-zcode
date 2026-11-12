//! Ownership record for a goal that a fork interrupts together with its running turn.
//!
//! `thread_goal_interrupt_pauses` holds at most one row per thread. A fork first claims the goal
//! that is active right now (`claimed`); the interrupt's pause then turns the claim into
//! `paused` in the same transaction. Database triggers delete the row on every insert, update,
//! or delete of the goal row, so it survives only while nobody else has written the goal since
//! the claim. That includes same-status writes in the same clock tick, which a status or
//! timestamp comparison cannot detect. The single exemption is the usage accounting of the
//! claimed turn itself, which is rewritten inside the transaction that performs it.

use super::GoalAccountingOutcome;
use super::GoalStore;
use super::datetime_to_epoch_millis;
use super::thread_goal_from_row;
use super::usage_update_builder;
use crate::GoalAccountingMode;
use chrono::Utc;
use codex_protocol::ThreadId;

impl GoalStore {
    /// Claims the goal of `thread_id` for a fork that is about to interrupt `turn_id`.
    ///
    /// Returns the claimed goal id, or `None` without writing when the goal is not active right
    /// now. Reading the goal and recording the claim are one statement.
    pub async fn claim_active_thread_goal_for_interrupt(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let goal_id = sqlx::query_scalar(
            r#"
INSERT INTO thread_goal_interrupt_pauses (thread_id, goal_id, turn_id, phase)
SELECT thread_id, goal_id, ?, 'claimed'
FROM thread_goals
WHERE thread_id = ?
  AND status = 'active'
ON CONFLICT(thread_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    turn_id = excluded.turn_id,
    phase = excluded.phase
RETURNING goal_id
            "#,
        )
        .bind(turn_id)
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        Ok(goal_id)
    }

    /// Drops the claim `turn_id` made on `thread_id`, for example when the interrupt did not
    /// happen. A claim that already became a recorded pause is left alone.
    pub async fn release_thread_goal_interrupt_claim(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
DELETE FROM thread_goal_interrupt_pauses
WHERE thread_id = ?
  AND turn_id = ?
  AND phase = 'claimed'
            "#,
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Accounts usage for `turn_id` like [`GoalStore::account_thread_goal_usage`], keeping a
    /// claim that this same turn holds on `expected_goal_id`.
    ///
    /// The claim is taken and restored inside the accounting transaction, so it survives only
    /// if it was still intact when accounting began; any other write has already deleted it and
    /// nothing is restored. No other claim is touched.
    pub async fn account_thread_goal_usage_for_turn(
        &self,
        thread_id: ThreadId,
        time_delta_seconds: i64,
        token_delta: i64,
        mode: GoalAccountingMode,
        expected_goal_id: &str,
        turn_id: &str,
    ) -> anyhow::Result<GoalAccountingOutcome> {
        let time_delta_seconds = time_delta_seconds.max(0);
        let token_delta = token_delta.max(0);
        if time_delta_seconds == 0 && token_delta == 0 {
            return Ok(GoalAccountingOutcome::Unchanged(
                self.get_thread_goal(thread_id).await?,
            ));
        }

        let mut transaction = self.pool.begin().await?;
        let held_claim = sqlx::query(
            r#"
DELETE FROM thread_goal_interrupt_pauses
WHERE thread_id = ?
  AND goal_id = ?
  AND turn_id = ?
  AND phase = 'claimed'
            "#,
        )
        .bind(thread_id.to_string())
        .bind(expected_goal_id)
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            > 0;
        let mut builder = usage_update_builder(
            thread_id,
            time_delta_seconds,
            token_delta,
            mode,
            Some(expected_goal_id),
        );
        let row = builder.build().fetch_optional(&mut *transaction).await?;
        let updated = row.as_ref().map(thread_goal_from_row).transpose()?;
        // Restore the claim only if the very same goal is still active after accounting. A
        // missing row, a budget limit, or any other outcome leaves the claim consumed.
        if held_claim
            && updated.as_ref().is_some_and(|goal| {
                goal.goal_id == expected_goal_id && goal.status == crate::ThreadGoalStatus::Active
            })
        {
            sqlx::query(
                r#"
INSERT INTO thread_goal_interrupt_pauses (thread_id, goal_id, turn_id, phase)
VALUES (?, ?, ?, 'claimed')
ON CONFLICT(thread_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    turn_id = excluded.turn_id,
    phase = excluded.phase
                "#,
            )
            .bind(thread_id.to_string())
            .bind(expected_goal_id)
            .bind(turn_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;

        match updated {
            Some(goal) => Ok(GoalAccountingOutcome::Updated(goal)),
            None => Ok(GoalAccountingOutcome::Unchanged(
                self.get_thread_goal(thread_id).await?,
            )),
        }
    }

    /// Pauses the active goal `expected_goal_id` because `turn_id` was interrupted.
    ///
    /// When `turn_id` still holds an intact claim on that goal, the claim becomes a recorded
    /// pause in the same transaction. Without a claim the goal is paused exactly as before and
    /// nothing is recorded. Returns `None` without writing when the goal is gone, replaced, or
    /// no longer active.
    pub async fn pause_active_thread_goal_for_interrupted_turn(
        &self,
        thread_id: ThreadId,
        expected_goal_id: &str,
        turn_id: &str,
    ) -> anyhow::Result<Option<crate::ThreadGoal>> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin().await?;
        let held_claim = sqlx::query(
            r#"
DELETE FROM thread_goal_interrupt_pauses
WHERE thread_id = ?
  AND goal_id = ?
  AND turn_id = ?
  AND phase = 'claimed'
            "#,
        )
        .bind(thread_id.to_string())
        .bind(expected_goal_id)
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            > 0;
        let result = sqlx::query(
            r#"
UPDATE thread_goals
SET
    status = 'paused',
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND status = 'active'
            "#,
        )
        .bind(now_ms)
        .bind(thread_id.to_string())
        .bind(expected_goal_id)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }

        // The update fired the invalidation trigger, so the recorded pause is written after it.
        if held_claim {
            sqlx::query(
                r#"
INSERT INTO thread_goal_interrupt_pauses (thread_id, goal_id, turn_id, phase)
VALUES (?, ?, ?, 'paused')
ON CONFLICT(thread_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    turn_id = excluded.turn_id,
    phase = excluded.phase
                "#,
            )
            .bind(thread_id.to_string())
            .bind(expected_goal_id)
            .bind(turn_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;

        self.get_thread_goal(thread_id).await
    }

    /// Copies the paused goal of `source_thread_id` to `target_thread_id` as an active goal
    /// with its continuation deferred, but only when the source is still paused by the
    /// interrupt of `interrupted_turn_id` for goal `expected_goal_id`.
    ///
    /// The recorded pause is consumed and the copy is written in one transaction. Returns
    /// `false` and writes nothing when ownership cannot be proven, so callers must then
    /// inherit the unchanged snapshot instead.
    pub async fn inherit_goal_paused_by_interrupted_turn(
        &self,
        source_thread_id: ThreadId,
        target_thread_id: ThreadId,
        expected_goal_id: &str,
        interrupted_turn_id: &str,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let consumed = sqlx::query(
            r#"
DELETE FROM thread_goal_interrupt_pauses
WHERE thread_id = ?
  AND goal_id = ?
  AND turn_id = ?
  AND phase = 'paused'
            "#,
        )
        .bind(source_thread_id.to_string())
        .bind(expected_goal_id)
        .bind(interrupted_turn_id)
        .execute(&mut *transaction)
        .await?;
        if consumed.rows_affected() == 0 {
            return Ok(false);
        }

        let copied = sqlx::query(
            r#"
INSERT INTO thread_goals (
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
)
SELECT
    ?,
    goal_id,
    objective,
    CASE
        WHEN token_budget IS NOT NULL AND tokens_used >= token_budget THEN 'budget_limited'
        ELSE 'active'
    END,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
FROM thread_goals
WHERE thread_id = ?
  AND goal_id = ?
  AND status = 'paused'
ON CONFLICT(thread_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    objective = excluded.objective,
    status = excluded.status,
    token_budget = excluded.token_budget,
    tokens_used = excluded.tokens_used,
    time_used_seconds = excluded.time_used_seconds,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = excluded.updated_at_ms
            "#,
        )
        .bind(target_thread_id.to_string())
        .bind(source_thread_id.to_string())
        .bind(expected_goal_id)
        .execute(&mut *transaction)
        .await?;
        if copied.rows_affected() == 0 {
            return Ok(false);
        }

        sqlx::query(
            r#"
INSERT INTO thread_goal_continuation_deferrals (thread_id)
VALUES (?)
ON CONFLICT(thread_id) DO NOTHING
            "#,
        )
        .bind(target_thread_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        Ok(true)
    }
}

#[cfg(test)]
#[path = "interrupt_pause_tests.rs"]
mod tests;
