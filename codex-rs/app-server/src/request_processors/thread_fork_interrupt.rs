//! Server-owned turn interruption for `thread/fork`.
//!
//! A client that retries a running turn on a fork must not interrupt the turn itself: the
//! interrupt pauses the turn's goal, and only the server can prove that pause belongs to the
//! fork's own interrupt rather than to a user's earlier or concurrent one.

use super::thread_fork_goal::InterruptedActiveGoal;
use super::thread_processor::ThreadRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_protocol::ThreadId;

impl ThreadRequestProcessor {
    /// Interrupts `turn_id` on behalf of a fork, but only if it is the source thread's currently
    /// running turn, and returns once the turn's abort lifecycle (including the goal pause) is
    /// complete.
    ///
    /// The turn is selected and taken in one core step, so a successor turn is never touched and
    /// a concurrent manual interrupt of the same turn either wins before this call, which then
    /// fails without interrupting, or finds the turn already taken. When `track_goal` is set,
    /// the returned claim is what a fork needs to later prove that this very interrupt paused
    /// the goal that was active when it began.
    pub(super) async fn interrupt_turn_for_fork(
        &self,
        source_thread_id: ThreadId,
        turn_id: &str,
        track_goal: bool,
    ) -> Result<Option<InterruptedActiveGoal>, JSONRPCErrorError> {
        let thread = self
            .thread_manager
            .get_thread(source_thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {source_thread_id}")))?;
        let state_db = thread.state_db().or_else(|| self.state_db.clone());
        let claim = match state_db.as_deref() {
            Some(state_db) if track_goal => {
                InterruptedActiveGoal::claim(state_db, source_thread_id, turn_id)
                    .await
                    .map_err(|err| {
                        internal_error(format!("failed to read source thread goal: {err}"))
                    })?
            }
            _ => None,
        };

        if thread.interrupt_turn_if_active(turn_id).await {
            return Ok(claim);
        }
        if let (Some(claim), Some(state_db)) = (claim.as_ref(), state_db.as_deref())
            && let Err(err) = claim.release(state_db, source_thread_id).await
        {
            tracing::warn!(%source_thread_id, "failed to release goal interrupt claim: {err}");
        }
        Err(invalid_request(format!(
            "turn {turn_id} is not the currently running turn"
        )))
    }
}
