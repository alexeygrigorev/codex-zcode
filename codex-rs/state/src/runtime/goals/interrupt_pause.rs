//! Ownership record for a goal that was paused by the interrupt of one turn.
//!
//! `thread_goal_interrupt_pauses` holds at most one row per thread. Database triggers delete
//! that row on every insert, update, or delete of the goal row, so the record survives only
//! while the pause written together with it is untouched. That includes same-status writes
//! that land in the same clock tick, which a status or timestamp comparison cannot detect.

use super::GoalStore;
use super::datetime_to_epoch_millis;
use chrono::Utc;
use codex_protocol::ThreadId;

impl GoalStore {
    /// Pauses the active goal `expected_goal_id` because `turn_id` was interrupted and records
    /// that ownership in the same transaction.
    ///
    /// Returns `None` without writing when the goal is gone, replaced, or no longer active.
    pub async fn pause_active_thread_goal_for_interrupted_turn(
        &self,
        thread_id: ThreadId,
        expected_goal_id: &str,
        turn_id: &str,
    ) -> anyhow::Result<Option<crate::ThreadGoal>> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin().await?;
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

        // The update above fired the invalidation trigger, so the record is written after it.
        sqlx::query(
            r#"
INSERT INTO thread_goal_interrupt_pauses (thread_id, goal_id, turn_id)
VALUES (?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    turn_id = excluded.turn_id
            "#,
        )
        .bind(thread_id.to_string())
        .bind(expected_goal_id)
        .bind(turn_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        self.get_thread_goal(thread_id).await
    }

    /// Copies the paused goal of `source_thread_id` to `target_thread_id` as an active goal
    /// with its continuation deferred, but only when the source is still paused by the
    /// interrupt of `interrupted_turn_id` for goal `expected_goal_id`.
    ///
    /// The ownership record is consumed and the copy is written in one transaction. Returns
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
