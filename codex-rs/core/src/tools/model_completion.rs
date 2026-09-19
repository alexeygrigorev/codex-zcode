//! Host adapter backing extension model completions with the session's model client.

use std::sync::Weak;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::CompletionOutput;
use codex_tools::CompletionRequest;
use codex_tools::ModelCompletion;
use codex_tools::ModelCompletionFuture;
use futures::StreamExt;

use crate::Prompt;
use crate::client_common::ResponseEvent;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::RequestEffortUsage;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// Hard cap on the single user message accepted for one host model completion.
const MODEL_COMPLETION_INPUT_CHAR_BUDGET: usize = 32_768;

/// Session-backed [`ModelCompletion`] handed to extension tool calls.
///
/// Requests run through the capturing turn's model client with no tool
/// surface, and nothing is recorded into the conversation, so a judge
/// completion can neither act nor leak into the model-visible history.
pub(crate) struct CoreModelCompletion {
    pub(crate) session: Weak<Session>,
    pub(crate) turn: Weak<TurnContext>,
}

impl ModelCompletion for CoreModelCompletion {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ModelCompletionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return Err("session ended before the model completion ran".to_string());
            };
            run_model_completion(&session, &turn, request).await
        })
    }
}

async fn run_model_completion(
    session: &Session,
    turn: &TurnContext,
    request: CompletionRequest,
) -> Result<CompletionOutput, String> {
    if request.input.len() > MODEL_COMPLETION_INPUT_CHAR_BUDGET {
        return Err(format!(
            "model completion input exceeded the {MODEL_COMPLETION_INPUT_CHAR_BUDGET} character budget"
        ));
    }
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: request.input,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        ..Default::default()
    };
    let responses_metadata = turn.turn_metadata_state.to_responses_metadata(
        session.installation_id.clone(),
        session.current_window_id().await,
        CodexResponsesRequestKind::Turn,
    );
    let mut client_session = session.services.model_client.new_session();
    let mut stream = client_session
        .stream(
            &prompt,
            turn.model_info(),
            &turn.session_telemetry,
            session
                .reasoning_effort_for_request(
                    &turn.initial_settings,
                    RequestEffortUsage::Compaction,
                )
                .await,
            turn.reasoning_summary(),
            turn.config.service_tier.clone(),
            &responses_metadata,
            &InferenceTraceContext::disabled(),
        )
        .await
        .map_err(|err| format!("model completion stream failed: {err}"))?;
    let mut message: Option<String> = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                if let ResponseItem::Message { role, content, .. } = item
                    && role == "assistant"
                {
                    let text = content
                        .iter()
                        .filter_map(|content_item| match content_item {
                            ContentItem::OutputText { text } => Some(text.as_str()),
                            ContentItem::InputText { .. }
                            | ContentItem::InputImage { .. }
                            | ContentItem::InputAudio { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.trim().is_empty() {
                        message = Some(text);
                    }
                }
            }
            Ok(ResponseEvent::Completed { .. }) => {
                return message
                    .map(|message| CompletionOutput { message })
                    .ok_or_else(|| {
                        "model completion ended without an assistant message".to_string()
                    });
            }
            Ok(_) => {}
            Err(err) => return Err(format!("model completion stream error: {err}")),
        }
    }
    Err("model completion stream closed before completing".to_string())
}
