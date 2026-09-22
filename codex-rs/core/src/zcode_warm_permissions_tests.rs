use pretty_assertions::assert_eq;

use super::DENY_ALL_REASON;
use super::PermissionRequest;
use super::PermissionRequestLog;
use super::RiskTier;
use super::WarmPermissionPolicy;

fn request(risk: RiskTier) -> PermissionRequest<'static> {
    PermissionRequest {
        tool_name: "Bash",
        risk,
        session_id: "sess_1",
        request_id: "req_1",
    }
}

#[test]
fn policy_value_parsing_fails_closed() {
    assert_eq!(
        WarmPermissionPolicy::from_value(None),
        super::WarmPermissionPolicy::DenyAll
    );
    assert_eq!(
        WarmPermissionPolicy::from_value(Some("deny")),
        super::WarmPermissionPolicy::DenyAll
    );
    assert_eq!(
        WarmPermissionPolicy::from_value(Some("garbage")),
        super::WarmPermissionPolicy::DenyAll
    );
    assert_eq!(
        WarmPermissionPolicy::from_value(Some("allow-safe")),
        super::WarmPermissionPolicy::AllowSafe
    );
    assert_eq!(
        WarmPermissionPolicy::from_value(Some(" allow-safe ")),
        super::WarmPermissionPolicy::AllowSafe
    );
}

#[test]
fn risk_tier_parsing_covers_the_wire_enum_and_degrades_to_unknown() {
    assert_eq!(RiskTier::from_value(Some("low")), RiskTier::Low);
    assert_eq!(RiskTier::from_value(Some("medium")), RiskTier::Medium);
    assert_eq!(RiskTier::from_value(Some("high")), RiskTier::High);
    assert_eq!(RiskTier::from_value(Some("critical")), RiskTier::Critical);
    assert_eq!(
        RiskTier::from_value(Some("catastrophic")),
        RiskTier::Unknown
    );
    assert_eq!(RiskTier::from_value(None), RiskTier::Unknown);
}

#[test]
fn deny_all_denies_every_risk_tier() {
    for risk in [
        RiskTier::Low,
        RiskTier::Medium,
        RiskTier::High,
        RiskTier::Critical,
        RiskTier::Unknown,
    ] {
        let answer = WarmPermissionPolicy::DenyAll.resolve(&request(risk));
        assert_eq!(answer.decision, "deny", "tier {risk:?}");
        assert_eq!(answer.reason, DENY_ALL_REASON);
    }
}

#[test]
fn allow_safe_approves_only_the_safe_tier() {
    for risk in [RiskTier::Low, RiskTier::Medium] {
        let answer = WarmPermissionPolicy::AllowSafe.resolve(&request(risk));
        assert_eq!(answer.decision, "allow", "tier {risk:?}");
        assert!(answer.reason.contains(risk.wire_name()), "tier {risk:?}");
    }
    for risk in [RiskTier::High, RiskTier::Critical, RiskTier::Unknown] {
        let answer = WarmPermissionPolicy::AllowSafe.resolve(&request(risk));
        assert_eq!(answer.decision, "deny", "tier {risk:?}");
        assert!(answer.reason.contains(risk.wire_name()), "tier {risk:?}");
    }
}

#[test]
fn allow_safe_unreadable_tier_fails_closed() {
    let answer = WarmPermissionPolicy::AllowSafe.resolve(&request(RiskTier::Unknown));
    assert_eq!(answer.decision, "deny");
}

#[test]
fn permission_request_view_degrades_missing_fields() {
    let params = serde_json::json!({
        "toolName": "Write",
        "sessionId": "sess_9",
    });
    let parsed = PermissionRequest::from_params(&params);
    assert_eq!(parsed.tool_name, "Write");
    assert_eq!(parsed.risk, RiskTier::Unknown);
    assert_eq!(parsed.session_id, "sess_9");
    assert_eq!(parsed.request_id, "");
}

#[test]
fn request_log_reports_first_sighting_per_session_and_request() {
    let mut log = PermissionRequestLog::default();
    assert!(log.first_sighting("sess_1", "req_1"));
    assert!(!log.first_sighting("sess_1", "req_1"));
    assert!(log.first_sighting("sess_1", "req_2"));
    assert!(log.first_sighting("sess_2", "req_1"));
}

#[test]
fn request_log_never_dedupes_requests_without_a_request_id() {
    let mut log = PermissionRequestLog::default();
    assert!(log.first_sighting("sess_1", ""));
    assert!(log.first_sighting("sess_1", ""));
}

#[test]
fn request_log_stays_bounded_by_its_cap() {
    let mut log = PermissionRequestLog::default();
    for i in 0..(super::PERMISSION_REQUEST_LOG_CAP * 2) {
        let _ = log.first_sighting("sess_1", &format!("req_{i}"));
    }
    assert_eq!(log.seen.len(), super::PERMISSION_REQUEST_LOG_CAP);
}
