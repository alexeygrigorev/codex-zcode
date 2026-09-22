//! Warm-mode goal mirroring over `session/goal` (issue #40).
//!
//! On the ZCode wire the model cannot call Codex-side tools, so the goal
//! continuation's `update_goal` instruction is unusable there; the
//! spawn-per-turn path papers over that with a text-marker protocol, and the
//! warm path had no goal reporting at all. A warm bridge hosts a real ZCode
//! core, and the core owns the better mechanism: the host sets the session
//! goal with `session/goal`, which starts the core's goal loop, and the
//! core's LLM-as-judge completion verifier (a tool-less model call over the
//! full server-side history) drives continuation turns until it proves the
//! goal complete. This module mirrors the Codex-side goal into the core and
//! maps the verified outcome back onto the same synthesized `update_goal`
//! call the marker translation emits, so Codex-side bookkeeping is identical.
//!
//! Opt in with `ZCODE_WARM_GOAL=1`. The marker protocol remains the
//! spawn-per-turn mechanism, and any mirror setup failure falls back to a
//! plain warm turn.

use std::collections::HashMap;
use std::sync::Arc;

use codex_api::ApiError;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::warn;

use codex_tools::ToolSpec;

use crate::client::zcode_failure_message;
use crate::client::zcode_goal_tools_active;
use crate::zcode_warm::ZcodeWarmBridge;
use crate::zcode_warm::assistant_message;
use crate::zcode_warm::event_timeout;
use crate::zcode_warm::stop_turn;
use crate::zcode_warm::token_usage_from_turn_payload;
use crate::zcode_warm::zcode_completed_turn_error;
use crate::zcode_warm_events::bridge_tool_activity_from_notification;

/// Env var enabling goal mirroring for warm sessions (`ZCODE_WARM_GOAL=1`).
const GOAL_MIRROR_ENV_VAR: &str = "ZCODE_WARM_GOAL";

/// Upper bound on core turns one mirrored request may span. The core's
/// verifier re-prompts on every non-passing iteration and has no "blocked"
/// verdict, so an unachievable goal would loop forever; past this cap the
/// bridge stops the run and pauses the core goal without completion, handing
/// control back to Codex's own continuation, budget, and blocked audits.
const MAX_MIRRORED_GOAL_TURNS: u32 = 12;

/// The objective is user-authored data quoted back into a core prompt; cap it
/// so a pathological goal cannot dominate the core's context.
const OBJECTIVE_CHAR_BUDGET: usize = 4_000;

/// Whether goal mirroring was requested via [`GOAL_MIRROR_ENV_VAR`]; unknown
/// values keep the mirror off.
fn goal_mirror_enabled_from_value(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1") | Some("true") | Some("yes"))
}

/// Extracts the active objective from the goal continuation template the
/// Codex goal runtime rendered into the newest user message.
fn goal_objective_from_prompt(input: &[ResponseItem]) -> Option<String> {
    let text = input.iter().rev().find_map(|item| match item {
        ResponseItem::Message { role, content, .. } if role.as_str() == "user" => Some(
            content
                .iter()
                .filter_map(|content_item| match content_item {
                    codex_protocol::models::ContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    })?;
    let start = text.find("<objective>")? + "<objective>".len();
    let end = text[start..].find("</objective>")? + start;
    let objective = text[start..end].trim();
    (!objective.is_empty()).then(|| objective.chars().take(OBJECTIVE_CHAR_BUDGET).collect())
}

/// The objective to mirror for this prompt, when the mirror applies: mirroring
/// enabled, a goal active (`update_goal` in the tool list), and the
/// continuation template's objective recovered from the prompt.
pub(crate) fn mirrored_goal_objective(
    tools: &[ToolSpec],
    input: &[ResponseItem],
) -> Option<String> {
    mirrored_goal_objective_for(
        std::env::var(GOAL_MIRROR_ENV_VAR).ok().as_deref(),
        tools,
        input,
    )
}

/// Pure core of [`mirrored_goal_objective`] so tests avoid mutating the
/// process environment.
fn mirrored_goal_objective_for(
    mirror_value: Option<&str>,
    tools: &[ToolSpec],
    input: &[ResponseItem],
) -> Option<String> {
    if !goal_mirror_enabled_from_value(mirror_value) {
        return None;
    }
    if !zcode_goal_tools_active(tools) {
        return None;
    }
    goal_objective_from_prompt(input)
}

/// How the mirrored goal loop ended.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Settle {
    /// The core's verifier proved the goal complete; report it Codex-side.
    Complete,
    /// The loop ended without a verified completion (paused, budget-limited,
    /// cleared, or the turn cap); leave the Codex-side goal decision to the
    /// normal continuation flow.
    WithoutComplete,
}

/// Drives one goal-active turn through the core's own goal loop.
///
/// Sets the session goal (the core starts the goal-continuation turn itself —
/// the flattened Codex transcript is deliberately not sent; the core works
/// from its own server-side history), then collects session events until the
/// goal settles. On a verified completion the stream carries the same
/// synthesized `update_goal` function call the marker translation produces, so
/// the normal registry path records the status change.
pub(crate) async fn turn_mirrored(
    bridge: &Arc<ZcodeWarmBridge>,
    session_id: &str,
    objective: &str,
) -> Result<codex_api::ResponseStream, String> {
    // Subscribe before the request so no goal-loop event can slip past the
    // collector; the broadcast buffer holds anything that arrives early.
    let mut events = bridge.subscribe_events();
    let result = bridge
        .request_with_compat_retry(
            "session/goal",
            serde_json::json!({
                "sessionId": session_id,
                "action": "set",
                "objective": objective,
            }),
        )
        .await?;
    // The core refuses goal management mid-turn and skips the continuation in
    // plan mode; either way there is nothing to collect and the prompt must go
    // out as a normal warm turn.
    if result
        .get("startedTurn")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err("core accepted the goal but did not start the goal loop".to_string());
    }

    let request_id = format!("zcode_warm_{}", uuid::Uuid::new_v4());
    let upstream_request_id = Some(request_id.clone());
    let collector_bridge = Arc::downgrade(bridge);
    let collector_session = session_id.to_string();
    let collector_timeout = event_timeout();
    let (tx, rx_event) =
        mpsc::channel::<std::result::Result<codex_api::ResponseEvent, ApiError>>(64);
    tokio::spawn(async move {
        let mut reply = String::new();
        let mut started_output = false;
        let mut tool_names = HashMap::new();
        let mut turns_seen: u32 = 0;
        let mut usage = TokenUsage::default();
        let mut settle: Option<Settle> = None;
        loop {
            let notification = match tokio::time::timeout(collector_timeout, events.recv()).await {
                Ok(Ok(params)) => params,
                Ok(Err(broadcast::error::RecvError::Lagged(_skipped))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    let _ = tx
                        .send(Err(ApiError::Stream(
                            "ZCode warm app-server event stream closed".to_string(),
                        )))
                        .await;
                    return;
                }
                Err(_elapsed) => {
                    if let Some(bridge) = collector_bridge.upgrade() {
                        bridge.register_failure();
                        bridge.kill("mirrored goal turn stalled");
                    }
                    let _ = tx
                        .send(Err(ApiError::Stream(
                            "ZCode warm goal loop stalled: no session events before the idle \
                         window elapsed"
                                .to_string(),
                        )))
                        .await;
                    return;
                }
            };
            if notification
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                != Some(collector_session.as_str())
            {
                continue;
            }
            match notification.get("type").and_then(serde_json::Value::as_str) {
                Some("model.streaming") => {
                    let payload = notification.get("payload");
                    let kind = payload
                        .and_then(|p| p.get("kind"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let delta = payload
                        .and_then(|p| p.get("delta"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if kind == "text_delta" && !delta.is_empty() {
                        if !started_output {
                            started_output = true;
                            let item = assistant_message(String::new());
                            if tx
                                .send(Ok(codex_api::ResponseEvent::OutputItemAdded(item)))
                                .await
                                .is_err()
                            {
                                stop_turn(&collector_bridge, &collector_session).await;
                                return;
                            }
                        }
                        reply.push_str(delta);
                        if tx
                            .send(Ok(codex_api::ResponseEvent::OutputTextDelta(
                                delta.to_string(),
                            )))
                            .await
                            .is_err()
                        {
                            stop_turn(&collector_bridge, &collector_session).await;
                            return;
                        }
                    }
                }
                Some("tool_call_scheduled" | "tool_call_result" | "tool_call_error") => {
                    if let Some(activity) =
                        bridge_tool_activity_from_notification(&notification, &mut tool_names)
                        && tx
                            .send(Ok(codex_api::ResponseEvent::BridgeToolActivity(activity)))
                            .await
                            .is_err()
                    {
                        stop_turn(&collector_bridge, &collector_session).await;
                        return;
                    }
                }
                Some("turn.failed") => {
                    let payload = notification.get("payload").cloned().unwrap_or_default();
                    let message = zcode_failure_message(&payload)
                        .unwrap_or_else(|| serde_json::to_string(&payload).unwrap_or_default());
                    let _ = tx.send(Err(ApiError::Stream(message))).await;
                    return;
                }
                Some("turn.completed") => {
                    let payload = notification.get("payload").cloned().unwrap_or_default();
                    let result_type = payload
                        .get("resultType")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("success");
                    if let Some(err) = zcode_completed_turn_error(result_type) {
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                    usage = token_usage_from_turn_payload(&payload);
                    let response = if reply.is_empty() {
                        payload
                            .get("response")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    } else {
                        std::mem::take(&mut reply)
                    };
                    if !response.is_empty()
                        && tx
                            .send(Ok(codex_api::ResponseEvent::OutputItemDone(
                                assistant_message(response),
                            )))
                            .await
                            .is_err()
                    {
                        return;
                    }
                    turns_seen += 1;
                    if turns_seen >= MAX_MIRRORED_GOAL_TURNS {
                        warn!(
                            "ZCode warm goal loop hit the {MAX_MIRRORED_GOAL_TURNS}-turn cap; \
                             pausing the core goal without completion"
                        );
                        if let Some(bridge) = collector_bridge.upgrade() {
                            let _ = bridge
                                .request_with_compat_retry(
                                    "session/stop",
                                    serde_json::json!({ "sessionId": collector_session }),
                                )
                                .await;
                            let _ = bridge
                                .request_with_compat_retry(
                                    "session/goal",
                                    serde_json::json!({
                                        "sessionId": collector_session,
                                        "action": "pause",
                                    }),
                                )
                                .await;
                        }
                        settle = Some(Settle::WithoutComplete);
                    }
                    reply = String::new();
                    started_output = false;
                }
                // The legacy wire has no dedicated goal event type: target
                // status transitions and verifier iterations arrive as
                // `session.updated` carrying the core's raw payloads.
                Some("session.updated") => {
                    let payload = notification.get("payload");
                    match payload
                        .and_then(|p| p.pointer("/target/status"))
                        .and_then(serde_json::Value::as_str)
                    {
                        Some("complete") => settle = Some(Settle::Complete),
                        Some("paused" | "budget_limited") => {
                            settle = Some(Settle::WithoutComplete);
                        }
                        _ => {
                            if payload
                                .and_then(|p| p.get("action"))
                                .and_then(serde_json::Value::as_str)
                                == Some("cleared")
                            {
                                settle = Some(Settle::WithoutComplete);
                            }
                        }
                    }
                }
                _ => {}
            }
            if settle.is_some() {
                // A paused or cleared goal can leave a core turn running with
                // no collector left; stop it best-effort before ending.
                if settle == Some(Settle::WithoutComplete) {
                    stop_turn(&collector_bridge, &collector_session).await;
                }
                break;
            }
        }
        if settle == Some(Settle::Complete) {
            let item = ResponseItem::FunctionCall {
                id: None,
                name: "update_goal".to_string(),
                namespace: None,
                arguments: r#"{"status":"complete"}"#.to_string(),
                encrypted_function_args: Some(Vec::new()),
                call_id: format!("zcode_goal_{request_id}"),
                internal_chat_message_metadata_passthrough: None,
            };
            let _ = tx
                .send(Ok(codex_api::ResponseEvent::OutputItemDone(item)))
                .await;
        }
        let _ = tx
            .send(Ok(codex_api::ResponseEvent::Completed {
                response_id: request_id,
                token_usage: Some(usage),
                usage_metadata: None,
                end_turn: Some(true),
            }))
            .await;
    });

    Ok(codex_api::ResponseStream {
        rx_event,
        upstream_request_id,
    })
}

#[cfg(test)]
#[path = "zcode_warm_goal_tests.rs"]
mod tests;
