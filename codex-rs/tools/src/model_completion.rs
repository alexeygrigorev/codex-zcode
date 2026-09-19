use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Request for one bounded, tool-less host model completion.
#[derive(Clone, Debug)]
pub struct CompletionRequest {
    /// Single user message presented to the model. Implementations reject
    /// requests above their own hard cap, so callers must bound the input
    /// before building the request.
    pub input: String,
}

/// Text output of one tool-less host model completion.
#[derive(Clone, Debug)]
pub struct CompletionOutput {
    /// Final assistant message text.
    pub message: String,
}

/// Future returned by [`ModelCompletion::complete`].
pub type ModelCompletionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletionOutput, String>> + Send + 'a>>;

/// Host capability for one-off model completions with no tools attached.
///
/// Implementations run the request without any tool surface, so the model can
/// only answer with text; callers use this for judge or verification passes
/// that must never act. The capability is captured per tool call because it
/// borrows the executing session's model client and turn settings.
pub trait ModelCompletion: Send + Sync {
    /// Runs one completion and returns the final assistant message text.
    fn complete<'a>(&'a self, request: CompletionRequest) -> ModelCompletionFuture<'a>;
}

/// Shared handle used by tool calls to reach the host completion capability.
pub type SharedModelCompletion = Arc<dyn ModelCompletion>;
/// Completion capability for hosts and tests with no model access.
///
/// It fails closed: every request returns an error so callers never mistake a
/// missing capability for a verified result.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopModelCompletion;

impl ModelCompletion for NoopModelCompletion {
    fn complete<'a>(&'a self, _request: CompletionRequest) -> ModelCompletionFuture<'a> {
        Box::pin(std::future::ready(Err(
            "host model completion is unavailable".to_string(),
        )))
    }
}
