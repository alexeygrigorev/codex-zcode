//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` and `followup_task` share the same submission path and differ only in whether the
//! resulting `InterAgentCommunication` should wake the target immediately.

use super::analytics::ToolCallAnalytics;
use super::*;
use crate::agent::control::MessageDeliveryError;
use crate::agent::control::MessageDeliveryMode;
use crate::tools::context::FunctionToolOutput;
use codex_protocol::error::CodexErrorDetails;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `followup_task` tool.
pub(crate) struct FollowupTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
}

pub(super) fn message_content(message: String) -> Result<String, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    Ok(message)
}

/// Handles the shared MultiAgentV2 message flow for both `send_message` and `followup_task`.
pub(super) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    analytics: &mut ToolCallAnalytics,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let message = message_content(message)?;
    let ToolInvocation {
        session,
        turn,
        call_id,
        source,
        ..
    } = invocation;
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    analytics.set_receiver(receiver_thread_id);
    // Models that address agents by invented IDs (observed with the ZCode
    // backend) can only recover if the error names the agents that do exist.
    let message_preview = sub_agent_message_preview(&message);
    let receiver_agent_path = match session
        .services
        .agent_control
        .deliver_message(
            session.thread_id,
            &turn,
            receiver_thread_id,
            agent_message_from_tool(message, &source),
            mode,
        )
        .await
    {
        Ok(receiver_agent_path) => receiver_agent_path,
        Err(MessageDeliveryError::Agent(err))
            if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
        {
            let roster = session
                .services
                .agent_control
                .list_agents(&turn.session_source, /*path_prefix*/ None)
                .await
                .map(|agents| {
                    agents
                        .iter()
                        .map(|agent| agent.agent_name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            return Err(FunctionCallError::RespondToModel(format!(
                "agent with id {receiver_thread_id} not found. Addressable agents (use one as \
                 the target): {roster}"
            )));
        }
        Err(MessageDeliveryError::InvalidRequest(message)) => {
            return Err(FunctionCallError::RespondToModel(message));
        }
        Err(MessageDeliveryError::Agent(err)) => {
            return Err(collab_agent_error(receiver_thread_id, err));
        }
    };
    emit_sub_agent_activity(
        &session,
        &turn,
        SubAgentActivityItem {
            id: call_id,
            agent_thread_id: receiver_thread_id,
            agent_path: receiver_agent_path,
            kind: SubAgentActivityKind::Interacted,
            message_preview,
        },
    )
    .await;

    Ok(FunctionToolOutput::from_text(String::new(), Some(true)))
}
