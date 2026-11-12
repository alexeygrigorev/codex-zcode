//! Server-owned turn interruption for `thread/fork`.
//!
//! A client that retries a running turn on a fork must not interrupt the turn itself: the
//! interrupt pauses the turn's goal, and only the server can prove that pause belongs to the
//! fork's own interrupt rather than to a user's earlier or concurrent one.

use super::thread_fork_goal::InterruptedActiveGoal;
use super::thread_processor::ThreadRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::thread_state::PendingInterrupt;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Op;

impl ThreadRequestProcessor {
    /// Interrupts `turn_id` on behalf of a fork and waits until the turn has finished aborting,
    /// including the goal extension's pause of the turn's goal.
    ///
    /// Fails before interrupting unless `turn_id` is the source thread's currently running turn
    /// and no other interrupt of it is already pending. When `track_goal` is set, the returned
    /// value records the goal that was active when the interrupt began, which is what a fork
    /// needs to later prove that the interrupt itself paused that goal.
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
        let thread_state = self
            .thread_state_manager
            .thread_state(source_thread_id)
            .await;
        let active_goal = if track_goal {
            match thread.state_db().or_else(|| self.state_db.clone()) {
                Some(state_db) => {
                    InterruptedActiveGoal::capture(&state_db, source_thread_id, turn_id)
                        .await
                        .map_err(|err| {
                            internal_error(format!("failed to read source thread goal: {err}"))
                        })?
                }
                None => None,
            }
        } else {
            None
        };

        // Core's active turn is the source of truth. Query it before taking the thread-state
        // lock, as `turn/interrupt` does, so no guard is held across the core query.
        if thread.active_turn_root(turn_id).await.is_none() {
            return Err(invalid_request(format!(
                "turn {turn_id} is not the currently running turn"
            )));
        }
        let (waiter, interrupted) = tokio::sync::oneshot::channel();
        {
            let mut thread_state = thread_state.lock().await;
            if let Some(tracked_turn_id) = thread_state.active_turn_id()
                && tracked_turn_id != turn_id
            {
                return Err(invalid_request(format!(
                    "expected active turn id {turn_id} but found {tracked_turn_id}"
                )));
            }
            // Another pending interrupt means a pause of this turn may not be ours.
            if !thread_state.pending_interrupts.is_empty() {
                return Err(invalid_request(format!(
                    "turn {turn_id} is already being interrupted"
                )));
            }
            thread_state
                .pending_interrupts
                .push(PendingInterrupt::Waiter(waiter));
        }

        if let Err(err) = thread.submit(Op::Interrupt).await {
            // Close our receiver so the retain below can recognize and drop our waiter.
            drop(interrupted);
            let mut thread_state = thread_state.lock().await;
            thread_state
                .pending_interrupts
                .retain(|pending| match pending {
                    PendingInterrupt::Request(_) => true,
                    PendingInterrupt::Waiter(waiter) => !waiter.is_closed(),
                });
            return Err(internal_error(format!("failed to interrupt turn: {err}")));
        }
        interrupted.await.map_err(|_| {
            internal_error(format!(
                "turn {turn_id} was not acknowledged as interrupted before the thread closed"
            ))
        })?;
        Ok(active_goal)
    }
}
