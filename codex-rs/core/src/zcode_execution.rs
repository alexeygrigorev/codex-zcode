//! Policy admission and a fresh app-server permission client for every cold turn.
//! Inner tools execute only in ZCode; streamed tool events remain display-only.

use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_api::ResponseStream;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc;

use super::ZcodeRuntime;
use crate::zcode_process;
use crate::zcode_warm::SessionSeed;
use crate::zcode_warm::WarmMode;
use crate::zcode_warm::ZcodeWarmBridge;
use crate::zcode_warm_permissions::WarmPermissionPolicy;
use crate::zcode_warm_v4_send::TurnInputChannel;

#[derive(Debug, Clone)]
pub(crate) struct ZcodeExecutionPolicy {
    pub(crate) approval_policy: AskForApproval,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) workspace: Option<AbsolutePathBuf>,
}

impl Default for ZcodeExecutionPolicy {
    fn default() -> Self {
        Self {
            approval_policy: AskForApproval::UnlessTrusted,
            permission_profile: PermissionProfile::read_only(),
            workspace: None,
        }
    }
}

impl ZcodeExecutionPolicy {
    pub(crate) fn admitted_workspace(&self) -> Result<&AbsolutePathBuf, String> {
        if self.approval_policy != AskForApproval::Never
            || self.permission_profile != PermissionProfile::Disabled
        {
            return Err("ZCode inner execution requires an explicitly unrestricted permission profile and approval policy never; restricted sandbox or interactive approval enforcement is unavailable".to_string());
        }
        self.workspace.as_ref().ok_or_else(|| {
            "ZCode inner execution requires exactly one ready host-local environment".to_string()
        })
    }
}

/// Tool-free requests, including compaction, never grant inner tool execution.
pub(super) fn permission_policy(prompt: &crate::Prompt) -> WarmPermissionPolicy {
    if prompt.tools.is_empty() {
        WarmPermissionPolicy::NoTools
    } else {
        WarmPermissionPolicy::for_unrestricted_execution()
    }
}

/// Clears only this in-flight claim if cancellation drops its owning future.
pub(super) struct WarmClaim<'a> {
    pub(super) slot: &'a Mutex<Option<super::ZcodeWarmSlot>>,
    pub(super) owner: Arc<AtomicBool>,
}

impl Drop for WarmClaim<'_> {
    fn drop(&mut self) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(slot.as_ref(), Some(super::ZcodeWarmSlot::Claiming(owner)) if Arc::ptr_eq(owner, &self.owner))
        {
            *slot = None;
        }
    }
}

/// Prevent the outer sampling retry ladder from replaying an inner agent turn.
pub(crate) fn no_replay(stream: ResponseStream) -> ResponseStream {
    let upstream_request_id = stream.upstream_request_id;
    let mut incoming = stream.rx_event;
    let (tx, rx_event) = mpsc::channel(/*buffer*/ 64);
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                _ = tx.closed() => return,
                event = incoming.recv() => event,
            };
            let Some(event) = event else {
                break;
            };
            let event = event.map_err(|error| match error {
                ApiError::ContextWindowExceeded => ApiError::ContextWindowExceeded,
                ApiError::Stream(ref message)
                    if message.contains(super::ZCODE_OUTPUT_LIMIT_CODE) =>
                {
                    ApiError::ContextWindowExceeded
                }
                error => ApiError::InvalidRequest {
                    message: format!(
                        "ZCode execution failed; the turn will not be replayed: {error}"
                    ),
                },
            });
            let terminal = matches!(&event, Err(_) | Ok(ResponseEvent::Completed { .. }));
            if tx.send(event).await.is_err() || terminal {
                return;
            }
        }
        let _ = tx
            .send(Err(ApiError::InvalidRequest {
                message: "ZCode execution ended without completion; the turn will not be replayed"
                    .to_string(),
            }))
            .await;
    });
    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

pub(crate) async fn stream_cold(
    runtime: ZcodeRuntime,
    prompt: &crate::Prompt,
    thread_id: &str,
) -> Result<ResponseStream, String> {
    let workspace = prompt
        .zcode_execution
        .admitted_workspace()?
        .to_string_lossy()
        .into_owned();
    let permit = zcode_process::acquire_zcode_session(&workspace, thread_id)
        .await
        .map_err(|error| error.to_string())?;
    let permission_policy = permission_policy(prompt);
    let bridge = ZcodeWarmBridge::spawn(
        &runtime,
        &workspace,
        WarmMode::Build,
        permission_policy,
        SessionSeed::Fresh,
    )
    .map_err(|error| error.to_string())?;
    let mut lease = super::ZcodeWarmSlotEntry {
        workspace: prompt.zcode_execution.admitted_workspace()?.clone(),
        permission_policy,
        bridge: std::sync::Arc::clone(&bridge),
        checkout: Some(permit),
    };
    let mut content = super::zcode_flatten_transcript(&prompt.input);
    let goal_protocol = super::zcode_goal_status_protocol(&prompt.tools);
    if let Some(note) = &goal_protocol {
        content.push('\n');
        content.push_str(note);
    }
    let goal_already_reported = super::zcode_goal_already_reported_in_current_turn(&prompt.input);
    content.push_str(super::ZCODE_REPLY_BUDGET_NOTE);
    let prompt_bytes = content.len();
    let (tx, rx_event) = mpsc::channel(/*buffer*/ 64);
    tokio::spawn(async move {
        let prepared = tokio::select! {
            _ = tx.closed() => None,
            prepared = async {
            let session_id = bridge.prepare_session(&workspace, WarmMode::Build).await?;
            // A fresh cold session has no server-side history for goal mirroring.
            bridge.turn(&session_id, &content, TurnInputChannel::LegacySend)
            } => Some(prepared),
        };
        let mut goal_status = None;
        let mut reply_bytes = 0;
        let terminal = match prepared {
            None => None,
            Some(Err(error)) => Some(Err(ApiError::InvalidRequest { message: error })),
            Some(Ok(mut stream)) => loop {
                let event = tokio::select! {
                    _ = tx.closed() => break None,
                    event = stream.rx_event.recv() => event,
                };
                match event {
                    Some(Ok(ResponseEvent::Completed {
                        response_id,
                        token_usage,
                        usage_metadata,
                        end_turn,
                    })) => {
                        let mut usage = token_usage.unwrap_or_default();
                        if usage.total_tokens <= 0 {
                            let fallback =
                                super::zcode_token_usage(None, prompt_bytes, reply_bytes);
                            if usage.input_tokens <= 0 {
                                usage.input_tokens = fallback.input_tokens;
                            }
                            if usage.output_tokens <= 0 {
                                usage.output_tokens = fallback.output_tokens;
                            }
                            usage.total_tokens =
                                usage.input_tokens.saturating_add(usage.output_tokens);
                        }
                        break Some(Ok(ResponseEvent::Completed {
                            response_id,
                            token_usage: Some(usage),
                            usage_metadata,
                            end_turn,
                        }));
                    }
                    Some(event @ Err(_)) => {
                        break Some(event);
                    }
                    Some(Ok(ResponseEvent::OutputItemDone(mut item))) => {
                        if let ResponseItem::Message { content, .. } = &item {
                            reply_bytes += content
                                .iter()
                                .filter_map(|chunk| match chunk {
                                    ContentItem::OutputText { text } => Some(text.len()),
                                    _ => None,
                                })
                                .sum::<usize>();
                        }
                        if goal_protocol.is_some()
                            && let ResponseItem::Message { content, phase, .. } = &mut item
                        {
                            for chunk in content {
                                if let ContentItem::OutputText { text } = chunk
                                    && let Some(status) = super::zcode_goal_status_from_reply(text)
                                {
                                    if !goal_already_reported
                                        && *phase != Some(MessagePhase::Commentary)
                                    {
                                        goal_status = Some(status);
                                    }
                                    *text = super::zcode_strip_goal_status_marker(text);
                                }
                            }
                        }
                        if tx
                            .send(Ok(ResponseEvent::OutputItemDone(item)))
                            .await
                            .is_err()
                        {
                            break None;
                        }
                    }
                    Some(event) => {
                        if tx.send(event).await.is_err() {
                            break None;
                        }
                    }
                    None => {
                        break Some(Err(ApiError::InvalidRequest {
                            message: "ZCode cold session ended without completion".to_string(),
                        }));
                    }
                }
            },
        };
        let retired = bridge.retire().await;
        drop(lease.checkout.take());
        let terminal = match retired {
            Ok(()) => terminal,
            Err(error) => Some(Err(ApiError::InvalidRequest { message: error })),
        };
        if matches!(&terminal, Some(Ok(ResponseEvent::Completed { .. })))
            && let Some(status) = goal_status
        {
            let item = ResponseItem::FunctionCall {
                id: None,
                name: "update_goal".to_string(),
                namespace: None,
                arguments: format!(r#"{{"status":"{status}"}}"#),
                encrypted_function_args: Some(Vec::new()),
                call_id: format!("zcode_goal_{}", uuid::Uuid::new_v4()),
                internal_chat_message_metadata_passthrough: None,
            };
            let _ = tx.send(Ok(ResponseEvent::OutputItemDone(item))).await;
        }
        if let Some(event) = terminal {
            let _ = tx.send(event).await;
        }
    });
    Ok(no_replay(ResponseStream {
        rx_event,
        upstream_request_id: None,
    }))
}

#[cfg(test)]
#[path = "zcode_execution_tests.rs"]
mod tests;
