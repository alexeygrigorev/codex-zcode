//! Completion verification for `update_goal(Complete)` requests.
//!
//! The verification pass runs one tool-less host model completion over a
//! bounded transcript, so the model cannot grant itself goal completion
//! without conversational evidence. Every ambiguous outcome fails closed: the
//! goal stays active and the reason is surfaced back to the model.

use codex_extension_api::CompletionRequest;
use codex_extension_api::ConversationHistory;
use codex_extension_api::SharedModelCompletion;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;

/// Hard cap on transcript characters included in one verification request.
const TRANSCRIPT_CHAR_BUDGET: usize = 16_000;
/// Hard cap applied to any single rendered history item.
const ITEM_CHAR_BUDGET: usize = 4_000;
/// Hard cap on the failure reason quoted back to the model.
const REASON_CHAR_BUDGET: usize = 500;

/// Outcome of one goal-completion verification pass.
pub(crate) enum CompletionVerdict {
    /// The judge found conversational evidence that the goal was achieved.
    Passed,
    /// The judge judged the goal incomplete and returned a reason.
    NotPassed(String),
}

/// Judges whether the conversation shows the goal objective was achieved.
pub(crate) async fn verify_goal_completion(
    model_completion: &SharedModelCompletion,
    objective: &str,
    history: &ConversationHistory,
) -> Result<CompletionVerdict, String> {
    let input = build_judge_input(objective, render_transcript(history.items()));
    let output = model_completion
        .complete(CompletionRequest { input })
        .await
        .map_err(|err| format!("verification model call failed: {err}"))?;
    parse_verdict(&output.message).ok_or_else(|| {
        format!(
            "verification model answer was not PASS or FAIL: {}",
            truncate_chars(output.message.trim(), REASON_CHAR_BUDGET)
        )
    })
}

fn build_judge_input(objective: &str, transcript: String) -> String {
    let instructions = "You are verifying whether a coding agent achieved its assigned goal before it marks the goal complete. Answer with exactly one line: \"PASS\" if the transcript shows the goal was fully achieved, or \"FAIL: <reason>\" if it was not. Do not credit plans, partial progress, or promises of future work.";
    format!("{instructions}\n\nGoal: {objective}\n\nConversation transcript:\n{transcript}")
}

fn render_transcript(items: &[ResponseItem]) -> String {
    let rendered: Vec<String> = items.iter().filter_map(render_item).collect();
    let mut kept: Vec<&String> = Vec::new();
    let mut total_chars = 0;
    for line in rendered.iter().rev() {
        let line_chars = line.chars().count();
        if total_chars + line_chars > TRANSCRIPT_CHAR_BUDGET {
            break;
        }
        total_chars += line_chars;
        kept.push(line);
    }
    kept.reverse();
    kept.into_iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_item(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let text = content
                .iter()
                .filter_map(|content_item| match content_item {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            (!text.trim().is_empty())
                .then(|| truncate_chars(&format!("{role}: {text}"), ITEM_CHAR_BUDGET))
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => Some(truncate_chars(
            &format!("assistant invoked tool {name} with arguments: {arguments}"),
            ITEM_CHAR_BUDGET,
        )),
        ResponseItem::FunctionCallOutput { output, .. } => {
            let text = match &output.body {
                FunctionCallOutputBody::Text(text) => text.clone(),
                FunctionCallOutputBody::ContentItems(items) => items
                    .iter()
                    .filter_map(|item| match item {
                        FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                        FunctionCallOutputContentItem::InputImage { .. }
                        | FunctionCallOutputContentItem::InputAudio { .. }
                        | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            (!text.trim().is_empty())
                .then(|| truncate_chars(&format!("tool output: {text}"), ITEM_CHAR_BUDGET))
        }
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ConfigurationUpdate { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => None,
    }
}

fn parse_verdict(message: &str) -> Option<CompletionVerdict> {
    let trimmed = message.trim();
    let (first_word, rest) = trimmed
        .split_once([' ', '\n', '\t', ':'])
        .unwrap_or((trimmed, ""));
    match first_word
        .trim_end_matches([':', '.', '!'])
        .to_ascii_uppercase()
        .as_str()
    {
        "PASS" | "PASSED" => Some(CompletionVerdict::Passed),
        "FAIL" | "FAILED" => {
            let reason = rest.trim().trim_start_matches(':').trim();
            let reason = if reason.is_empty() {
                trimmed.to_string()
            } else {
                truncate_chars(reason, REASON_CHAR_BUDGET)
            };
            Some(CompletionVerdict::NotPassed(reason))
        }
        _ => None,
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}\u{2026}")
}
