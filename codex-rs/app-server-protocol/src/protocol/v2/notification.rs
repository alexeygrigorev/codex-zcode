use super::TurnError;
use crate::JsonSchema;
use crate::RequestId;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Lifecycle of one tool call executing inside a bridged external agent core.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum BridgeToolActivityStatus {
    /// The core scheduled the call; `detail` summarizes the input.
    Started,
    /// The call finished successfully; `detail` carries an output preview.
    Completed,
    /// The call failed; `detail` carries the error message.
    Failed,
}

/// Live status of one tool call executing inside a bridged external agent
/// core (the ZCode warm bridge). Display only: the tool ran on the far side
/// of the bridge, so it has no matching item in the thread history.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct BridgeToolActivityNotification {
    pub thread_id: String,
    pub turn_id: String,
    /// The external core's id for the tool call.
    pub call_id: String,
    pub tool: String,
    pub status: BridgeToolActivityStatus,
    /// Bounded human-readable detail (input summary, output preview, or
    /// error message), capped by the bridge before emission.
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AuthRecoveryNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub provider: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct DeprecationNoticeNotification {
    /// Concise summary of what is deprecated.
    pub summary: String,
    /// Optional extra guidance, such as migration steps or rationale.
    pub details: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WarningNotification {
    /// Optional thread target when the warning applies to a specific thread.
    pub thread_id: Option<String>,
    /// Concise warning message for the user.
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GuardianWarningNotification {
    /// Thread target for the guardian warning.
    pub thread_id: String,
    /// Concise guardian warning message for the user.
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct StrictReviewRequiredNotification {
    pub thread_id: String,
    pub turn_id: String,
    /// Unix timestamp (in milliseconds) when this review started.
    #[ts(type = "number")]
    pub started_at_ms: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ErrorNotification {
    pub error: TurnError,
    // Set to true if the error is transient and the app-server process will automatically retry.
    // If true, this will not interrupt a turn.
    pub will_retry: bool,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ServerRequestResolvedNotification {
    pub thread_id: String,
    pub request_id: RequestId,
}
