//! Extraction of the user's actual request from prompts that carry an
//! IDE-style context prefix.
//!
//! The desktop app and IDE extension serialize context into the raw prompt
//! before a fixed delimiter, then transcript rendering strips back to the
//! request after the last delimiter. Keeping the same delimiter and stripping
//! semantics lets threads created with context in one surface replay cleanly
//! in the others.

const PROMPT_REQUEST_BEGIN: &str = "## My request for Codex:";

pub(crate) fn extract_prompt_request_with_offset(message: &str) -> (&str, usize) {
    let Some((before_request, request)) = message.rsplit_once(PROMPT_REQUEST_BEGIN) else {
        return (message, 0);
    };

    let request_start = before_request.len() + PROMPT_REQUEST_BEGIN.len();
    let trimmed_request = request.trim();
    let leading_trimmed_len = request.len() - request.trim_start().len();
    (trimmed_request, request_start + leading_trimmed_len)
}

#[cfg(test)]
#[path = "prompt_request_tests.rs"]
mod tests;
