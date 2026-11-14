use super::*;
use codex_protocol::protocol::NativeToolObservation;
use serde_json::json;

fn event(modern: bool, kind: &str, id: &str, result: Value) -> Value {
    let mut payload = json!({"toolCallId": id});
    if kind == "scheduled" {
        payload["toolName"] = json!("Bash");
        payload["input"] = json!({"command":"printf 'actual-nonce'"});
    } else if kind == "result" {
        payload["result"] = result;
    }
    let ty = if modern {
        payload["kind"] = json!(kind);
        "tool.updated".to_owned()
    } else {
        format!("tool_call_{kind}")
    };
    json!({"type":ty,"sessionId":"native-session","turnId":"native-turn","payload":payload})
}

fn returned() -> Value {
    json!({"success":true,"content":"actual-nonce\n","truncated":false,
        "originalBytes":13,"returnedBytes":13,
        "perf":{"detail":{"kind":"command","command":{"exitCode":0,"status":"completed","timedOut":false}}}})
}

fn observe(state: &mut NativeResults, event: &Value) -> Option<BridgeToolActivityEvent> {
    state.observe(event, "native-session", "outer-turn")
}

fn result(activity: BridgeToolActivityEvent) -> NativeToolResult {
    match activity.native_result.expect("native observation") {
        NativeToolObservation::Result(result) => result,
        NativeToolObservation::Invalidated { .. } => panic!("expected actual native return"),
    }
}

#[test]
fn both_native_schemas_preserve_actual_matched_result_without_dispatch() {
    for modern in [false, true] {
        let mut state = NativeResults::default();
        let assistant = json!({"type":"model.streaming","sessionId":"native-session",
            "payload":{"kind":"text_delta","delta":"actual-nonce fake Bash exit0"}});
        assert!(observe(&mut state, &assistant).is_none());
        let started =
            observe(&mut state, &event(modern, "scheduled", "call", Value::Null)).unwrap();
        assert!(started.native_result.is_none());
        let observed =
            result(observe(&mut state, &event(modern, "result", "call", returned())).unwrap());
        assert_eq!(
            observed,
            NativeToolResult {
                native_event: "tool_call_result".to_owned(),
                native_session_id: "native-session".to_owned(),
                native_turn_id: Some("native-turn".to_owned()),
                call_id: "call".to_owned(),
                tool: "Bash".to_owned(),
                command: "printf 'actual-nonce'".to_owned(),
                content: "actual-nonce\n".to_owned(),
                native_success: Some(true),
                exit_code: Some(0),
                status: Some("completed".to_owned()),
                timed_out: Some(false),
                native_truncated: Some(false),
                native_original_bytes: Some(13),
                native_returned_bytes: Some(13),
                local_truncated: false,
                observation_complete: true,
            }
        );
    }
}

#[test]
fn call_cap_lag_and_missing_native_turn_fail_closed() {
    let mut state = NativeResults::default();
    for index in 0..MAX_CALLS {
        observe(
            &mut state,
            &event(
                /*modern*/ true,
                "scheduled",
                &index.to_string(),
                Value::Null,
            ),
        );
    }
    let overflow = observe(
        &mut state,
        &event(/*modern*/ true, "scheduled", "overflow", Value::Null),
    )
    .unwrap();
    assert!(matches!(
        overflow.native_result,
        Some(NativeToolObservation::Invalidated { .. })
    ));
    assert_eq!(state.calls.len(), MAX_CALLS);
    let mut state = NativeResults::default();
    assert!(state.gap().is_some());
    assert!(state.gap().is_none());
    let mut state = NativeResults::default();
    let mut scheduled = event(/*modern*/ true, "scheduled", "call", Value::Null);
    scheduled.as_object_mut().unwrap().remove("turnId");
    observe(&mut state, &scheduled);
    let mut native = event(/*modern*/ true, "result", "call", returned());
    native.as_object_mut().unwrap().remove("turnId");
    let observed = result(observe(&mut state, &native).unwrap());
    assert_eq!(observed.native_turn_id, None);
    assert!(!observed.observation_complete);
}

#[test]
fn malformed_modern_kind_cannot_hide_between_schedule_and_matching_result() {
    for kind in [
        None,
        Some(Value::Null),
        Some(json!(true)),
        Some(json!(42)),
        Some(json!("unknown-kind")),
    ] {
        let mut state = NativeResults::default();
        observe(
            &mut state,
            &event(/*modern*/ true, "scheduled", "call", Value::Null),
        );
        let mut malformed = json!({"type":"tool.updated","sessionId":"native-session","turnId":"native-turn","payload":{"toolCallId":"call"}});
        if let Some(kind) = kind {
            malformed["payload"]["kind"] = kind;
        }
        let fault = observe(&mut state, &malformed)
            .expect("known envelope with invalid kind must emit a fault");
        assert!(matches!(
            fault.native_result,
            Some(NativeToolObservation::Invalidated { .. })
        ));
        assert!(
            observe(
                &mut state,
                &event(/*modern*/ true, "result", "call", returned())
            )
            .unwrap()
            .native_result
            .is_none()
        );
        assert!(state.invalid);
    }
}

#[test]
fn proof_fault_never_suppresses_later_ordinary_display_or_heals_proof() {
    let mut state = NativeResults::default();
    let bad = serde_json::json!({"type":"tool.updated","sessionId":"native-session","payload":{"kind":"unknown"}});
    let fault = observe(&mut state, &bad).unwrap();
    assert!(matches!(
        fault.native_result,
        Some(NativeToolObservation::Invalidated { .. })
    ));
    let started = observe(
        &mut state,
        &event(/*modern*/ true, "scheduled", "healthy", Value::Null),
    )
    .unwrap();
    assert_eq!(
        started,
        BridgeToolActivityEvent {
            native_result: None,
            call_id: "healthy".to_owned(),
            tool: "Bash".to_owned(),
            status: BridgeToolActivityStatus::Started,
            detail: Some(r#"{"command":"printf 'actual-nonce'"}"#.to_owned()),
        }
    );
    let finished = observe(
        &mut state,
        &event(/*modern*/ true, "result", "healthy", returned()),
    )
    .unwrap();
    assert_eq!(
        finished,
        BridgeToolActivityEvent {
            native_result: None,
            call_id: "healthy".to_owned(),
            tool: "Bash".to_owned(),
            status: BridgeToolActivityStatus::Completed,
            detail: Some("actual-nonce\n".to_owned()),
        }
    );
    assert!(state.invalid);
}

#[test]
fn proof_cap_keeps_display_bounded_and_later_turn_progress_visible() {
    let mut state = NativeResults::default();
    for i in 0..MAX_CALLS {
        observe(
            &mut state,
            &event(
                /*modern*/ false,
                "scheduled",
                &i.to_string(),
                Value::Null,
            ),
        );
    }
    let at_cap = observe(
        &mut state,
        &event(/*modern*/ false, "scheduled", "over-cap", Value::Null),
    )
    .unwrap();
    assert_eq!(at_cap.status, BridgeToolActivityStatus::Started);
    assert!(matches!(
        at_cap.native_result,
        Some(NativeToolObservation::Invalidated { .. })
    ));
    let started = state
        .observe(
            &event(
                /*modern*/ false,
                "scheduled",
                "later-turn",
                Value::Null,
            ),
            "native-session",
            "later-collector",
        )
        .unwrap();
    assert_eq!(started.call_id, "later-turn");
    assert_eq!(started.tool, "Bash");
    assert_eq!(started.status, BridgeToolActivityStatus::Started);
    assert!(started.native_result.is_none());
    let finished = state
        .observe(
            &event(/*modern*/ false, "result", "later-turn", returned()),
            "native-session",
            "later-collector",
        )
        .unwrap();
    assert_eq!(finished.status, BridgeToolActivityStatus::Completed);
    assert_eq!(finished.detail.as_deref(), Some("actual-nonce\n"));
    assert!(finished.native_result.is_none());
    assert!(state.invalid);
    assert_eq!(state.calls.len(), MAX_CALLS);
    assert_eq!(state.display_names.len(), MAX_CALLS);
    assert_eq!(state.display_order.len(), MAX_CALLS);
}

#[test]
fn orphan_result_display_cache_cannot_grow_after_proof_failure() {
    let mut state = NativeResults::default();
    for i in 0..MAX_CALLS * 3 {
        let mut native = event(/*modern*/ true, "result", &i.to_string(), returned());
        native["payload"]["toolName"] = json!("Bash");
        let activity = observe(&mut state, &native).unwrap();
        assert_eq!(activity.tool, "Bash");
        assert_eq!(activity.status, BridgeToolActivityStatus::Completed);
        assert!(!matches!(
            activity.native_result,
            Some(NativeToolObservation::Result(_))
        ));
        assert!(state.display_names.len() <= MAX_CALLS);
        assert_eq!(state.display_order.len(), state.display_names.len());
    }
    assert!(state.invalid);
}
