use pretty_assertions::assert_eq;

// Authored for the opt-in diagnostic boundary. No provider/runtime is needed.
#[test]
fn zcode_bridge_diagnostics_default_off_and_non_one_flags_emit_nothing() {
    use super::ZcodeBridgeDiagnostics;
    use super::ZcodeDiagnosticBranch;
    use super::ZcodeDiagnosticPhase;
    for flag in [
        None,
        Some("0"),
        Some("true"),
        Some(" 1"),
        Some("TOKEN_PRIVATE_SENTINEL"),
    ] {
        let diagnostics = ZcodeBridgeDiagnostics::from_value(flag);
        assert_eq!(
            diagnostics.line_at(
                ZcodeDiagnosticBranch::Spawn,
                ZcodeDiagnosticPhase::PermitWait,
                Some("TOKEN_PRIVATE_SENTINEL"),
                std::time::Duration::from_millis(17),
            ),
            None,
        );
        assert_eq!(
            diagnostics
                .emitted
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        // The real writer must return before touching stderr when default-OFF.
        diagnostics.emit(
            ZcodeDiagnosticBranch::Spawn,
            ZcodeDiagnosticPhase::PermitWait,
            None,
        );
    }
}

#[test]
fn zcode_bridge_diagnostics_enabled_has_exact_closed_schema() {
    use super::ZcodeBridgeDiagnostics;
    use super::ZcodeDiagnosticBranch;
    use super::ZcodeDiagnosticPhase;
    let diagnostics = ZcodeBridgeDiagnostics::from_value(Some("1"));
    let line = diagnostics
        .line_at(
            ZcodeDiagnosticBranch::Spawn,
            ZcodeDiagnosticPhase::PermitAcquired,
            None,
            std::time::Duration::from_millis(17),
        )
        .expect("enabled fixed record");
    assert!(line.len() <= ZcodeBridgeDiagnostics::MAX_LINE_BYTES);
    let record: serde_json::Value = serde_json::from_str(&line).expect("fixed JSON");
    assert_eq!(
        record,
        serde_json::json!({
            "kind": "zcodex_bridge_phase_v1", "seq": 1, "branch": "spawn",
            "phase": "permit_acquired", "frame": null, "elapsed_ms": 17,
            "source_release": "unknown", "scope": "direct_child_only",
        })
    );
    // Both existing hooks collapse EOF and read errors; report only that union.
    let closed = diagnostics
        .line_at(
            ZcodeDiagnosticBranch::Spawn,
            ZcodeDiagnosticPhase::StdoutClosedOrReadError,
            None,
            std::time::Duration::from_millis(18),
        )
        .expect("closed disjunctive phase");
    let closed: serde_json::Value = serde_json::from_str(&closed).expect("fixed JSON");
    assert_eq!(closed["phase"], "stdout_closed_or_read_error");
    assert_eq!(closed["frame"], serde_json::Value::Null);
    assert_eq!(closed.as_object().expect("object").len(), 8);
    assert!(!closed.to_string().contains("stdout_eof"));
}

#[test]
fn zcode_bridge_diagnostics_warm_fallback_shares_one_sixteen_record_budget() {
    use super::ZcodeBridgeDiagnostics;
    use super::ZcodeDiagnosticBranch;
    use super::ZcodeDiagnosticPhase;
    let warm = std::sync::Arc::new(ZcodeBridgeDiagnostics::from_value(Some("1")));
    let spawn = std::sync::Arc::clone(&warm);
    let mut records = Vec::new();
    for index in 0..24 {
        let (diagnostics, branch) = if index < 8 {
            (&warm, ZcodeDiagnosticBranch::Warm)
        } else {
            (&spawn, ZcodeDiagnosticBranch::Spawn)
        };
        if let Some(line) = diagnostics.line_at(
            branch,
            ZcodeDiagnosticPhase::ChildSpawned,
            Some("result"),
            std::time::Duration::from_millis(index),
        ) {
            assert!(line.len() <= ZcodeBridgeDiagnostics::MAX_LINE_BYTES);
            records.push(serde_json::from_str::<serde_json::Value>(&line).expect("fixed JSON"));
        }
    }
    assert_eq!(records.len(), 16);
    assert_eq!(records[0]["branch"], "warm");
    assert_eq!(records[8]["branch"], "spawn");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["seq"], (index + 1) as u64);
        assert_eq!(record["elapsed_ms"], index as u64);
    }
    assert_eq!(records[15]["phase"], "limit_reached");
    assert_eq!(records[15]["frame"], serde_json::Value::Null);
    assert_eq!(spawn.emitted.load(std::sync::atomic::Ordering::Relaxed), 16);
}

#[test]
fn zcode_bridge_diagnostics_unknown_frame_never_echoes_payload_or_secrets() {
    use super::ZcodeBridgeDiagnostics;
    use super::ZcodeDiagnosticBranch;
    use super::ZcodeDiagnosticPhase;
    let diagnostics = ZcodeBridgeDiagnostics::from_value(Some("1"));
    let hostile = "turn.failed\nTOKEN_PRIVATE_SENTINEL prompt=PRIVATE_PROMPT auth=PRIVATE_AUTH";
    let line = diagnostics
        .line_at(
            ZcodeDiagnosticBranch::Spawn,
            ZcodeDiagnosticPhase::FirstInnerFrame,
            Some(hostile),
            std::time::Duration::from_millis(2),
        )
        .expect("bounded other frame");
    let record: serde_json::Value = serde_json::from_str(&line).expect("fixed JSON");
    assert_eq!(record["frame"], "other");
    assert_eq!(record.as_object().expect("object").len(), 8);
    for private in [
        hostile,
        "TOKEN_PRIVATE_SENTINEL",
        "PRIVATE_PROMPT",
        "PRIVATE_AUTH",
    ] {
        assert!(!line.contains(private));
    }
    for known in [
        "session.created",
        "session.updated",
        "turn.started",
        "turn.completed",
        "turn.failed",
        "session.error",
        "result",
    ] {
        assert_eq!(ZcodeBridgeDiagnostics::frame_type(known), known);
    }
}

#[test]
fn zcode_bridge_diagnostics_child_wait_is_not_descendant_or_provider_success() {
    use super::ZcodeBridgeDiagnostics;
    use super::ZcodeDiagnosticBranch;
    use super::ZcodeDiagnosticPhase;
    let diagnostics = ZcodeBridgeDiagnostics::from_value(Some("1"));
    let line = diagnostics
        .line_at(
            ZcodeDiagnosticBranch::Spawn,
            ZcodeDiagnosticPhase::ChildWaitSettled,
            None,
            std::time::Duration::from_millis(10),
        )
        .expect("direct child observation");
    let record: serde_json::Value = serde_json::from_str(&line).expect("fixed JSON");
    assert_eq!(record["phase"], "child_wait_settled");
    assert_eq!(record["scope"], "direct_child_only");
    for forbidden in [
        "success",
        "exit_code",
        "containment_empty",
        "provider_accepted",
        "prompt",
        "token",
        "auth",
        "pid",
        "argv",
        "path",
        "config",
        "env",
    ] {
        assert!(!record.as_object().expect("object").contains_key(forbidden));
    }
}
