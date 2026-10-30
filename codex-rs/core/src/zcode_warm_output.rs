//! Hard bounds for assistant items retained in history and forked contexts.

use codex_api::ApiError;

// A conservative UTF-8 byte cap avoids depending on a different provider's
// tokenizer and leaves room below the 10K-token context-item limit.
pub(super) const MAX_MESSAGE_BYTES: usize = 8_000;

pub(super) fn check_message_bytes(bytes: usize) -> Result<(), ApiError> {
    if bytes > MAX_MESSAGE_BYTES {
        // A submitted inner turn may already have performed side effects.
        // Do not classify this as a context-window error, which can resubmit.
        Err(ApiError::InvalidRequest {
            message:
                "ZCode assistant message exceeded its history limit; the turn will not be replayed"
                    .to_string(),
        })
    } else {
        Ok(())
    }
}
