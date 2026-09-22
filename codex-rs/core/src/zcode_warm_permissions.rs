//! Permission policy for the warm ZCode bridge's `interaction/requestPermission`
//! callbacks (issue #25).
//!
//! In `build` mode the core asks the host before risky actions, and the bridge
//! has no interactive approver on the Codex side of the wire — the policy here
//! answers programmatically. The wire contract (verified against the
//! open-sourced core) is a strict `{decision: "allow"|"deny"|"escalate"|
//! "modify", reason?, modifiedInput?, permissionUpdates?}` result, and the
//! deny `reason` is load-bearing: the core appends it to the model-visible
//! denial content, so it must state the cause and what to do instead, or the
//! model keeps retrying around the block.
//!
//! [`WarmPermissionPolicy::AllowSafe`] answers with bare `"allow"` —
//! allow-once semantics: no `permissionUpdates`, so nothing is remembered and
//! every later occurrence asks again.
//!
//! The core re-announces a pending interaction every second with a fresh
//! protocol id and the same business `requestId`, and drops answers for
//! already-resolved interactions idempotently. Every reannouncement is
//! therefore answered; only the log line is deduped.

use std::collections::HashMap;

/// Env var selecting the permission policy (`ZCODE_WARM_PERMISSIONS`):
/// `deny` (the default) or `allow-safe`.
const WARM_PERMISSIONS_ENV_VAR: &str = "ZCODE_WARM_PERMISSIONS";

/// Risk tiers the core attaches to permission request params
/// (`low|medium|high|critical`); anything else parses as [`RiskTier::Unknown`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RiskTier {
    Low,
    Medium,
    High,
    Critical,
    Unknown,
}

impl RiskTier {
    fn from_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("low") => RiskTier::Low,
            Some("medium") => RiskTier::Medium,
            Some("high") => RiskTier::High,
            Some("critical") => RiskTier::Critical,
            _ => RiskTier::Unknown,
        }
    }

    /// The wire value, for log lines and denial reasons.
    fn wire_name(self) -> &'static str {
        match self {
            RiskTier::Low => "low",
            RiskTier::Medium => "medium",
            RiskTier::High => "high",
            RiskTier::Critical => "critical",
            RiskTier::Unknown => "unknown",
        }
    }

    /// The tier [`WarmPermissionPolicy::AllowSafe`] auto-approves. Read-only
    /// actions rarely reach the callback at all (the core's build mode allows
    /// them itself), and medium covers the small side effects — file writes,
    /// single commands — a normal coding turn needs. High (e.g. piped shell
    /// pipelines) and critical stay gated.
    fn is_safe(self) -> bool {
        matches!(self, RiskTier::Low | RiskTier::Medium)
    }
}

/// How `interaction/requestPermission` callbacks are answered in `build`
/// mode. `yolo` sessions never see these callbacks, so the policy is moot
/// there.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum WarmPermissionPolicy {
    /// Deny every request with a reason; the gated core measures what a real
    /// approval bridge would need.
    #[default]
    DenyAll,
    /// Auto-approve low/medium risk with allow-once semantics; deny
    /// high/critical (and anything with an unreadable tier) with a reason
    /// naming the policy.
    AllowSafe,
}

impl WarmPermissionPolicy {
    /// Resolves the policy from a `ZCODE_WARM_PERMISSIONS` value; unknown
    /// values stay on the fail-closed default.
    pub(crate) fn from_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("allow-safe") => WarmPermissionPolicy::AllowSafe,
            _ => WarmPermissionPolicy::DenyAll,
        }
    }

    /// [`WarmPermissionPolicy::from_value`] against the process environment.
    pub(crate) fn from_env() -> Self {
        Self::from_value(std::env::var(WARM_PERMISSIONS_ENV_VAR).ok().as_deref())
    }

    /// Decides a permission request. The returned `decision` is the wire
    /// value ("allow" or "deny"); `reason` rides along on both — required on
    /// denials, informative on approvals.
    pub(crate) fn resolve(self, request: &PermissionRequest<'_>) -> PermissionAnswer {
        match self {
            WarmPermissionPolicy::DenyAll => PermissionAnswer {
                decision: "deny",
                reason: DENY_ALL_REASON.to_string(),
            },
            WarmPermissionPolicy::AllowSafe if request.risk.is_safe() => PermissionAnswer {
                decision: "allow",
                reason: format!(
                    "Auto-approved by the Codex host policy: risk level {} is in the safe tier",
                    request.risk.wire_name()
                ),
            },
            WarmPermissionPolicy::AllowSafe => PermissionAnswer {
                decision: "deny",
                reason: format!(
                    "Denied by the Codex host policy: risk level {} is not auto-approved by \
                     the warm ZCode bridge; use a lower-risk approach",
                    request.risk.wire_name()
                ),
            },
        }
    }
}

/// The [`WarmPermissionPolicy::DenyAll`] denial reason.
pub(crate) const DENY_ALL_REASON: &str =
    "Denied by the Codex host: the warm ZCode bridge has no interactive approver";

/// The fields of an `interaction/requestPermission` params object the policy
/// and the log line need. Unreadable fields degrade to `""`/[`RiskTier::Unknown`]
/// rather than failing the callback: the core blocks on an answer.
pub(crate) struct PermissionRequest<'a> {
    pub(crate) tool_name: &'a str,
    pub(crate) risk: RiskTier,
    pub(crate) session_id: &'a str,
    pub(crate) request_id: &'a str,
}

impl<'a> PermissionRequest<'a> {
    pub(crate) fn from_params(params: &'a serde_json::Value) -> Self {
        let str_field = |name: &str| {
            params
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        };
        Self {
            tool_name: str_field("toolName"),
            risk: RiskTier::from_value(params.get("riskLevel").and_then(serde_json::Value::as_str)),
            session_id: str_field("sessionId"),
            request_id: str_field("requestId"),
        }
    }
}

/// A decided permission request: the wire `decision` plus its reason.
pub(crate) struct PermissionAnswer {
    pub(crate) decision: &'static str,
    pub(crate) reason: String,
}

/// Cap on remembered (session, request) pairs; a long-lived bridge clears the
/// set rather than growing without bound. Worst case after a clear is one
/// duplicate log line per pending interaction.
const PERMISSION_REQUEST_LOG_CAP: usize = 512;

/// Remembers which (session, request) pairs have already been logged so the
/// core's 1s reannouncement of a pending interaction does not spam the log.
/// Answers are never deduped: each reannouncement carries a fresh protocol id
/// the core expects answered.
#[derive(Default)]
pub(crate) struct PermissionRequestLog {
    seen: HashMap<(String, String), ()>,
}

impl PermissionRequestLog {
    /// Returns whether this (session, request) pair is new. Requests without
    /// a business `requestId` are always reported new: they cannot be told
    /// apart, so silencing them could hide real requests.
    pub(crate) fn first_sighting(&mut self, session_id: &str, request_id: &str) -> bool {
        if request_id.is_empty() {
            return true;
        }
        if self.seen.len() >= PERMISSION_REQUEST_LOG_CAP {
            self.seen.clear();
        }
        self.seen
            .insert((session_id.to_string(), request_id.to_string()), ())
            .is_none()
    }
}

#[cfg(test)]
#[path = "zcode_warm_permissions_tests.rs"]
mod tests;
