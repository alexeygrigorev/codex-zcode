use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::SessionSeed;
use super::WarmMode;
use super::ZcodeWarmBridge;
use super::warm_bridge_enabled_from_value;
use super::warm_mode_from_value;
use crate::client::ZcodeRuntime;
use crate::zcode_warm_permissions::PermissionRequest;
use crate::zcode_warm_permissions::RiskTier;
use crate::zcode_warm_permissions::WarmPermissionPolicy;
use crate::zcode_warm_store::ZcodeWarmRecord;
use crate::zcode_warm_v4_send::TurnInputChannel;
use codex_api::ResponseEvent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::BridgeToolActivityEvent;
use codex_protocol::protocol::BridgeToolActivityStatus;

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![codex_protocol::models::ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// Writes the fake app-server fixture and returns its path; the fake appends
/// one line per handled request to `<fixture>.stats` so tests can assert how
/// many sessions were created.
///
/// Before answering `session/create` the fake issues the
/// `session/requestRuntimePreferences` callback the real core sends, and only
/// proceeds once the host reply carries the required flag — a bridge without
/// the callback responder would hang the session handshake and time the test
/// out.
/// Options for the fake app-server fixture.
pub(crate) struct FakeServerOptions {
    /// Reject `session/create` with a zod unrecognized-key error to exercise
    /// the compat retry.
    pub(crate) rejects_unknown_create_keys: bool,
    /// Probe the host with `interaction/requestPermission` and
    /// `interaction/requestUserInput` callbacks during the create handshake,
    /// recording each answer in the stats file.
    pub(crate) probes_interactions: bool,
    /// `riskLevel` carried by the permission probe.
    pub(crate) permission_risk_level: &'static str,
    /// Re-send the permission probe once (same business `requestId`, fresh
    /// protocol id) after the host's first answer, the way the core
    /// re-announces pending interactions.
    pub(crate) reannounces_probe: bool,
    /// Probe the host with the provider-runtime-headers and official-MCP
    /// auth-headers callbacks during the create handshake, recording each
    /// answer in the stats file.
    pub(crate) probes_runtime_callbacks: bool,
    /// Emit `tool_call_*` session events around the turn's model output.
    pub(crate) emits_tool_events: bool,
    /// `resultType` reported on the `turn.completed` payload.
    pub(crate) turn_result_type: &'static str,
    /// Reject `session/resume` so the bridge's create fallback is exercised.
    pub(crate) fails_resume: bool,
    /// Answer `session/send`, emit one delta, then exit, so the collector is
    /// left waiting when the child's stdout closes.
    pub(crate) exits_mid_turn: bool,
    /// Answer `session/list` with no sessions, so a recorded session that
    /// the server forgot cannot be adopted.
    pub(crate) empty_session_list: bool,
    /// Reject `v4/command` with method-not-found, exercising the prototype
    /// channel's legacy fallback.
    pub(crate) fails_v4_command: bool,
    /// Answer `session/events` with three envelopes the previous bridge
    /// never saw, so the resume catch-up has a gap to recover (issue #48).
    pub(crate) missed_events: bool,
    /// Scripted `session/goal` behavior for the goal-mirror tests
    /// (`zcode_warm_goal_tests.rs`).
    pub(crate) goal_loop: GoalLoopMode,
}

/// How the fake app-server answers `session/goal` and what goal-loop events it
/// projects afterwards.
pub(crate) enum GoalLoopMode {
    /// `session/goal` falls through to the default method-not-found rejection.
    None,
    /// Reject the set with the active-turn protocol error.
    FailSet,
    /// Accept the set but report `startedTurn: false`.
    NotStarted,
    /// Run one goal turn, then project a verified completion.
    Complete,
    /// Run one goal turn, then project a paused target instead.
    Paused,
    /// Emit fourteen bare continuation turns and never settle.
    Capped,
}

impl Default for FakeServerOptions {
    fn default() -> Self {
        Self {
            rejects_unknown_create_keys: false,
            probes_interactions: false,
            permission_risk_level: "medium",
            reannounces_probe: false,
            probes_runtime_callbacks: false,
            emits_tool_events: false,
            turn_result_type: "success",
            fails_resume: false,
            exits_mid_turn: false,
            empty_session_list: false,
            fails_v4_command: false,
            missed_events: false,
            goal_loop: GoalLoopMode::None,
        }
    }
}

pub(crate) fn write_fake_server() -> PathBuf {
    write_fake_server_with(FakeServerOptions::default())
}

pub(crate) fn write_fake_server_with(options: FakeServerOptions) -> PathBuf {
    let fixture = std::env::temp_dir().join(format!(
        "zcode-warm-fake-{}.cjs",
        uuid::Uuid::new_v4().simple()
    ));
    let script = r#"
const fs = require("fs");
const statsPath = process.argv[1] + ".stats";
const stats = (line) => fs.appendFileSync(statsPath, line + "\n");
let buffer = "";
let pendingPref = null;
let pendingPerm = null;
let pendingInput = null;
let pendingHeaders = null;
let pendingMcpAuth = null;
// Event envelopes carry a monotonic seq; the subscribe result pins the
// stream head it starts from.
let seqCounter = 7;
function write(obj) { process.stdout.write(JSON.stringify(obj) + "\n"); }
function emit(params) { params.seq = ++seqCounter; write({ method: "session/event", params }); }
function sendProbe(kind, original) {
  const probeId = kind + "-" + original.id;
  const params = {
    sessionId: "sess_fake", turnId: "turn_1", requestId: "req_1", toolCallId: "tc_1",
    toolName: "Bash", riskLevel: PERM_RISK, reason: "probe", input: { command: "ls /tmp" },
  };
  if (kind === "input") params.prompt = "Which directory?";
  write({
    id: probeId,
    method: kind === "input" ? "interaction/requestUserInput" : "interaction/requestPermission",
    params,
  });
  return probeId;
}
// The core re-announces a pending interaction every second with a fresh
// protocol id and the same business requestId; this resends the permission
// probe verbatim so tests can pin the host's reannouncement behavior.
function resendPermissionProbe(original) {
  const probeId = "perm-re-" + original.id;
  const params = {
    sessionId: "sess_fake", turnId: "turn_1", requestId: "req_1", toolCallId: "tc_1",
    toolName: "Bash", riskLevel: PERM_RISK, reason: "probe", input: { command: "ls /tmp" },
  };
  write({ id: probeId, method: "interaction/requestPermission", params });
  return probeId;
}
function sendRuntimeProbe(kind, original) {
  const probeId = kind + "-" + original.id;
  const params = kind === "headers"
    ? { sessionId: "sess_fake", requestId: "req_h", providerId: "p", modelSelection: { modelId: "m", providerId: "p" } }
    : { sessionId: "sess_fake", requestId: "req_m", mcpKey: "k", targetOrigin: "https://example.com", pluginId: "pl" };
  write({
    id: probeId,
    method: kind === "headers"
      ? "interaction/requestProviderRuntimeHeaders"
      : "interaction/requestOfficialMcpAuthHeaders",
    params,
  });
  return probeId;
}
function handle(msg) {
  const respond = (result) => write({ id: msg.id, result });
  if (REJECT_DRIFT && msg.method === "session/create" && msg.params[DRIFT_KEY] !== undefined) {
    stats("drift-reject");
    write({
      id: msg.id,
      error: {
        code: -32602,
        message: "Invalid params",
        data: { name: "ZodError", message: JSON.stringify([
          { code: "unrecognized_keys", path: [], keys: [DRIFT_KEY], message: "Unrecognized key" },
        ]) },
      },
    });
    return;
  }
  switch (msg.method) {
    case "session/list":
      if (EMPTY_LIST) {
        respond({ sessions: [] });
      } else {
        respond({
          sessions: [{
            sessionId: msg.params.sessionIds[0],
            workspace: msg.params.workspace,
            mode: "yolo",
            status: "idle",
            sessionKind: "main",
            title: "",
            createdAt: 0,
            updatedAt: 0,
          }],
        });
      }
      break;
    case "session/resume":
      if (FAIL_RESUME) {
        write({ id: msg.id, error: { code: -32000, message: "session not found: " + msg.params.sessionId } });
      } else {
        respond({ sessionId: msg.params.sessionId, session: { sessionId: msg.params.sessionId }, messages: [], settings: {} });
      }
      break;
    case "session/create":
      respond({ session: { sessionId: "sess_fake" }, protocol: { name: "ZCode Protocol", version: 1 } });
      break;
    case "session/subscribe":
      respond({ sessionId: msg.params.sessionId, eventSeq: 7, events: [] });
      break;
    case "session/events": {
      // Envelopes the previous bridge never read: three events above the
      // subscribe head, so the resume catch-up has a gap to recover.
      const events = MISSED_EVENTS
        ? [
            { seq: ++seqCounter, eventId: "ev_c1", sessionId: msg.params.sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: "lo" } },
            { seq: ++seqCounter, eventId: "ev_c2", sessionId: msg.params.sessionId, type: "turn.completed", payload: { response: "lost tail", resultType: "success" } },
            { seq: ++seqCounter, eventId: "ev_c3", sessionId: msg.params.sessionId, type: "session.updated", payload: { note: "resumed" } },
          ]
        : [];
      respond({ events });
      break;
    }
    case "session/send": {
      const content = msg.params.content;
      if (EXIT_MID_TURN) {
        respond({ accepted: true, sessionId: msg.params.sessionId, stateRevision: 1 });
        emit({ sessionId: msg.params.sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: "pa" } });
        process.exit(0);
      }
      runTurn(msg.params.sessionId, content);
      respond({ accepted: true, sessionId: msg.params.sessionId, stateRevision: 1 });
      break;
    }
    case "v4/command": {
      if (FAIL_V4) {
        write({ id: msg.id, error: { code: -32601, message: "method not found: v4/command" } });
        break;
      }
      stats("v4command:" + JSON.stringify(msg.params));
      runTurn(msg.params.sessionId, msg.params.payload.text);
      respond({
        commandId: msg.params.commandId,
        status: "accepted",
        revisionAtDecision: 1,
        result: { type: "inputAccepted", delivery: "startNow", inputId: msg.params.commandId },
      });
      break;
    }
    case "session/stop":
      respond({ sessionId: msg.params.sessionId });
      break;
    case "session/goal": {
      stats("goal:" + JSON.stringify(msg.params));
      if (msg.params.action === "pause") {
        respond({ response: "", snapshot: {}, startedTurn: false });
        break;
      }
      if (GOAL_LOOP === "failset") {
        write({ id: msg.id, error: { code: -32000, message: "Cannot manage goals while a prompt is running" } });
        break;
      }
      if (GOAL_LOOP === "notstarted") {
        respond({ response: "ok", snapshot: {}, startedTurn: false });
        break;
      }
      const target = { targetId: "tgt_1", objective: msg.params.objective, status: "active" };
      emit({ sessionId: msg.params.sessionId, type: "session.updated", payload: { action: "set", source: "command", target } });
      if (GOAL_LOOP === "capped") {
        for (let i = 0; i < 14; i++) {
          emit({ sessionId: msg.params.sessionId, type: "turn.completed", payload: { response: "step " + i, resultType: "success" } });
        }
        respond({ response: "Goal active", snapshot: {}, startedTurn: true });
        break;
      }
      emit({ sessionId: msg.params.sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: "go" } });
      emit({ sessionId: msg.params.sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: "al" } });
      emit({ sessionId: msg.params.sessionId, type: "turn.completed", payload: { response: "goal work done", usage: { inputTokens: 7, outputTokens: 3, totalTokens: 10 }, resultType: "success" } });
      emit({ sessionId: msg.params.sessionId, type: "session.updated", payload: { verificationId: "v_1", targetId: "tgt_1", status: "completed", goalIteration: 1, verification: { passed: GOAL_LOOP === "complete" } } });
      const finalStatus = GOAL_LOOP === "paused" ? "paused" : "complete";
      emit({ sessionId: msg.params.sessionId, type: "session.updated", payload: { action: "status_updated", source: "runtime", target: { ...target, status: finalStatus } } });
      respond({ response: "Goal active", snapshot: {}, startedTurn: true });
      break;
    }
    default:
      write({ id: msg.id, error: { code: -32601, message: "unhandled " + msg.method } });
  }
}
// Both input surfaces feed the same turn: tool activity, deltas, terminal
// event — only the request/ACK envelope differs.
function runTurn(sessionId, content) {
  if (TOOL_EVENTS) {
    emit({ sessionId: sessionId, type: "tool_call_scheduled", payload: { toolCallId: "tc_1", toolName: "Bash", input: { command: "ls /tmp" } } });
    emit({ sessionId: sessionId, type: "tool_call_scheduled", payload: { toolCallId: "tc_2", toolName: "Read", input: { file: "/x" } } });
    emit({ sessionId: sessionId, type: "tool_call_result", payload: { toolCallId: "tc_1", result: { success: true, content: "out" }, duration: 3 } });
    emit({ sessionId: sessionId, type: "tool_call_error", payload: { toolCallId: "tc_2", error: { type: "ToolError", message: "boom" } } });
  }
  if (!content.includes("nodelta")) {
    emit({ sessionId: sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: content.slice(0, 2) } });
    emit({ sessionId: sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: content.slice(2) } });
  }
  emit({
    sessionId: sessionId,
    type: "turn.completed",
    payload: {
      response: content + "!",
      usage: { inputTokens: 10, outputTokens: 2, totalTokens: 12, cacheReadTokens: 5, reasoningTokens: 1 },
      resultType: "%RESULT_TYPE%",
    },
  });
}
const DRIFT_KEY = "titleGenerationEnabled";
const REJECT_DRIFT = %REJECT_DRIFT%;
const PROBE_INTERACTIONS = %PROBE_INTERACTIONS%;
const PROBE_RUNTIME = %PROBE_RUNTIME%;
const PERM_RISK = "%PERM_RISK%";
const REANNOUNCE_PROBE = %REANNOUNCE_PROBE%;
const TOOL_EVENTS = %TOOL_EVENTS%;
const FAIL_RESUME = %FAIL_RESUME%;
const EXIT_MID_TURN = %EXIT_MID_TURN%;
const EMPTY_LIST = %EMPTY_LIST%;
const FAIL_V4 = %FAIL_V4%;
const MISSED_EVENTS = %MISSED_EVENTS%;
const GOAL_LOOP = %GOAL_LOOP%;
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (!line.trim()) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (pendingPref !== null && msg.id === pendingPref.prefId) {
      // The host answer to our runtime-preferences callback.
      const original = pendingPref.original;
      pendingPref = null;
      if (!msg.result
        || msg.result.nativeSearchEnhancementsEnabled !== false
        || msg.result.askUserQuestionAutoResolutionEnabled !== true
        || msg.result.memoryEnabled !== false) {
        write({ id: original.id, error: { code: -32602, message: "host did not answer the runtime preferences callback" } });
      } else if (PROBE_INTERACTIONS) {
        const probeId = sendProbe("perm", original);
        pendingPerm = { probeId, original };
      } else if (PROBE_RUNTIME) {
        const probeId = sendRuntimeProbe("headers", original);
        pendingHeaders = { probeId, original };
      } else {
        handle(original);
      }
      continue;
    }
    if (pendingHeaders !== null && msg.id === pendingHeaders.probeId) {
      const original = pendingHeaders.original;
      pendingHeaders = null;
      stats("headers:" + (msg.error ? "rejected:" + msg.error.code : JSON.stringify(msg.result)));
      const probeId = sendRuntimeProbe("mcpauth", original);
      pendingMcpAuth = { probeId, original };
      continue;
    }
    if (pendingMcpAuth !== null && msg.id === pendingMcpAuth.probeId) {
      const original = pendingMcpAuth.original;
      pendingMcpAuth = null;
      stats("mcpauth:" + (msg.error ? "rejected:" + msg.error.code : JSON.stringify(msg.result)));
      handle(original);
      continue;
    }
    if (pendingPerm !== null && msg.id === pendingPerm.probeId) {
      const original = pendingPerm.original;
      pendingPerm = null;
      stats("permission:" + (msg.error ? "rejected:" + msg.error.code : JSON.stringify(msg.result)));
      if (REANNOUNCE_PROBE && !original.reannounced) {
        // Re-announce once, like the core does every second while an
        // interaction pends; the second answer resumes the normal chain.
        original.reannounced = true;
        const probeId = resendPermissionProbe(original);
        pendingPerm = { probeId, original };
        continue;
      }
      const probeId = sendProbe("input", original);
      pendingInput = { probeId, original };
      continue;
    }
    if (pendingInput !== null && msg.id === pendingInput.probeId) {
      const original = pendingInput.original;
      pendingInput = null;
      stats("userinput:" + (msg.error ? "rejected:" + msg.error.code : JSON.stringify(msg.result)));
      handle(original);
      continue;
    }
    if (msg.id !== undefined && msg.method === "session/create") {
      stats("create");
      stats("mode:" + msg.params.mode);
      pendingPref = { prefId: "pref-" + msg.id, original: msg };
      write({ id: "pref-" + msg.id, method: "session/requestRuntimePreferences", params: { sessionId: "sess_fake", scope: "runtime-materialization" } });
      continue;
    }
    if (msg.id !== undefined && msg.method) {
      const extra = msg.method === "session/send"
        ? ":" + msg.params.content
        : msg.method === "session/events" ? ":" + msg.params.afterSeq
        : msg.method === "session/list" ? ":" + JSON.stringify(msg.params) : "";
      stats(msg.method.replace("session/", "") + extra);
      handle(msg);
      continue;
    }
  }
});
"#;
    let script = script
        .replace(
            "%REJECT_DRIFT%",
            if options.rejects_unknown_create_keys {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%PROBE_INTERACTIONS%",
            if options.probes_interactions {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%PROBE_RUNTIME%",
            if options.probes_runtime_callbacks {
                "true"
            } else {
                "false"
            },
        )
        .replace("%PERM_RISK%", options.permission_risk_level)
        .replace(
            "%REANNOUNCE_PROBE%",
            if options.reannounces_probe {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%TOOL_EVENTS%",
            if options.emits_tool_events {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%FAIL_RESUME%",
            if options.fails_resume {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%EXIT_MID_TURN%",
            if options.exits_mid_turn {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%EMPTY_LIST%",
            if options.empty_session_list {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%FAIL_V4%",
            if options.fails_v4_command {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%MISSED_EVENTS%",
            if options.missed_events {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%GOAL_LOOP%",
            match options.goal_loop {
                GoalLoopMode::None => "null",
                GoalLoopMode::FailSet => "\"failset\"",
                GoalLoopMode::NotStarted => "\"notstarted\"",
                GoalLoopMode::Complete => "\"complete\"",
                GoalLoopMode::Paused => "\"paused\"",
                GoalLoopMode::Capped => "\"capped\"",
            },
        )
        .replace("%RESULT_TYPE%", options.turn_result_type);
    std::fs::write(&fixture, script).expect("write fake app-server fixture");
    fixture
}

pub(crate) fn stats_path(fixture: &Path) -> PathBuf {
    PathBuf::from(format!("{}.stats", fixture.display()))
}

pub(crate) fn test_runtime(fixture: &Path) -> ZcodeRuntime {
    ZcodeRuntime {
        node: "node".to_string(),
        cjs: fixture.to_string_lossy().to_string(),
    }
}

pub(crate) async fn next_event(
    rx: &mut mpsc::Receiver<Result<ResponseEvent, codex_api::ApiError>>,
) -> ResponseEvent {
    tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("event arrives well within the timeout")
        .expect("channel stays open")
        .expect("turn emits no error")
}

#[test]
fn warm_bridge_flag_parsing() {
    assert!(!warm_bridge_enabled_from_value(None));
    assert!(!warm_bridge_enabled_from_value(Some("0")));
    assert!(!warm_bridge_enabled_from_value(Some("off")));
    assert!(!warm_bridge_enabled_from_value(Some("garbage")));
    assert!(warm_bridge_enabled_from_value(Some("1")));
    assert!(warm_bridge_enabled_from_value(Some("true")));
    assert!(warm_bridge_enabled_from_value(Some(" yes ")));
}

#[tokio::test]
async fn warm_turn_streams_deltas_and_completes_with_usage() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "first", TurnInputChannel::LegacySend)
        .expect("turn starts");
    let mut rx = stream.rx_event;

    match next_event(&mut rx).await {
        ResponseEvent::OutputItemAdded(item) => assert_eq!(item, assistant_message("")),
        other => panic!("expected OutputItemAdded, got {other:?}"),
    }
    match next_event(&mut rx).await {
        ResponseEvent::OutputTextDelta(delta) => assert_eq!(delta, "fi"),
        other => panic!("expected OutputTextDelta, got {other:?}"),
    }
    match next_event(&mut rx).await {
        ResponseEvent::OutputTextDelta(delta) => assert_eq!(delta, "rst"),
        other => panic!("expected OutputTextDelta, got {other:?}"),
    }
    match next_event(&mut rx).await {
        ResponseEvent::OutputItemDone(item) => assert_eq!(item, assistant_message("first")),
        other => panic!("expected OutputItemDone, got {other:?}"),
    }
    match next_event(&mut rx).await {
        ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
            ..
        } => {
            assert!(response_id.starts_with("zcode_warm_"));
            assert_eq!(end_turn, Some(true));
            let usage = token_usage.expect("usage present");
            assert_eq!(
                (
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.total_tokens,
                    usage.cached_input_tokens,
                    usage.reasoning_output_tokens
                ),
                (10, 2, 12, 5, 1)
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    // The fake numbers event envelopes from 8 (subscribe pins head 7): two
    // deltas then turn.completed. The bridge must have tracked the stream.
    assert_eq!(bridge.last_event_seq(), 10);
    // The create handshake reported the fake's protocol version; the drift
    // guard pins it.
    assert_eq!(bridge.protocol_version(), 1);
    bridge.kill("test end");
}

#[tokio::test]
async fn warm_turn_maps_tool_call_events_to_bridge_activity() {
    let fixture = write_fake_server_with(FakeServerOptions {
        emits_tool_events: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "toolturn", TurnInputChannel::LegacySend)
        .expect("turn starts");
    let mut rx = stream.rx_event;

    let expected = [
        BridgeToolActivityEvent {
            call_id: "tc_1".to_string(),
            tool: "Bash".to_string(),
            status: BridgeToolActivityStatus::Started,
            detail: Some(r#"{"command":"ls /tmp"}"#.to_string()),
        },
        BridgeToolActivityEvent {
            call_id: "tc_2".to_string(),
            tool: "Read".to_string(),
            status: BridgeToolActivityStatus::Started,
            detail: Some(r#"{"file":"/x"}"#.to_string()),
        },
        BridgeToolActivityEvent {
            call_id: "tc_1".to_string(),
            tool: "Bash".to_string(),
            status: BridgeToolActivityStatus::Completed,
            detail: Some("out".to_string()),
        },
        BridgeToolActivityEvent {
            call_id: "tc_2".to_string(),
            tool: "Read".to_string(),
            status: BridgeToolActivityStatus::Failed,
            detail: Some("boom".to_string()),
        },
    ];
    for want in expected {
        match next_event(&mut rx).await {
            ResponseEvent::BridgeToolActivity(activity) => assert_eq!(activity, want),
            other => panic!("expected BridgeToolActivity, got {other:?}"),
        }
    }
    // The turn still streams and completes after the tool activity.
    match next_event(&mut rx).await {
        ResponseEvent::OutputItemAdded(item) => assert_eq!(item, assistant_message("")),
        other => panic!("expected OutputItemAdded, got {other:?}"),
    }
    loop {
        match next_event(&mut rx).await {
            ResponseEvent::OutputTextDelta(_) => continue,
            ResponseEvent::OutputItemDone(item) => {
                assert_eq!(item, assistant_message("toolturn"));
                break;
            }
            other => panic!("expected streamed output, got {other:?}"),
        }
    }
    assert!(matches!(
        next_event(&mut rx).await,
        ResponseEvent::Completed { .. }
    ));
    bridge.kill("test end");
}

#[tokio::test]
async fn warm_bridge_reuses_one_session_across_turns() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    for prompt in ["first", "second"] {
        let stream = bridge
            .turn(&session_id, prompt, TurnInputChannel::LegacySend)
            .expect("turn starts");
        let mut rx = stream.rx_event;
        loop {
            if matches!(next_event(&mut rx).await, ResponseEvent::Completed { .. }) {
                break;
            }
        }
    }

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let creates = stats_text.lines().filter(|line| *line == "create").count();
    let sends = stats_text
        .lines()
        .filter(|line| line.starts_with("send:"))
        .count();
    assert_eq!(creates, 1, "one session for the whole thread");
    assert_eq!(sends, 2, "one send per turn");
    bridge.kill("test end");
}

#[tokio::test]
async fn warm_turn_without_deltas_uses_final_response() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "nodelta please", TurnInputChannel::LegacySend)
        .expect("turn starts");
    let mut rx = stream.rx_event;

    let final_text = loop {
        match next_event(&mut rx).await {
            ResponseEvent::OutputItemDone(item) => match item {
                ResponseItem::Message { content, .. } => match &content[0] {
                    codex_protocol::models::ContentItem::OutputText { text } => break text.clone(),
                    other => panic!("expected text content, got {other:?}"),
                },
                other => panic!("expected a message item, got {other:?}"),
            },
            _ => continue,
        }
    };
    assert_eq!(final_text, "nodelta please!");
    bridge.kill("test end");
}

#[test]
fn turn_completed_result_types_classify() {
    use codex_api::ApiError;

    assert!(super::zcode_completed_turn_error("success").is_none());
    for result_type in [
        "cancelled",
        "error_max_turns",
        "error_max_budget",
        "error_max_tool_calls",
    ] {
        assert!(
            matches!(
                super::zcode_completed_turn_error(result_type),
                Some(ApiError::InvalidRequest { .. })
            ),
            "{result_type} must surface as a non-retryable error"
        );
    }
    for result_type in ["error_during_execution", "something_new"] {
        assert!(
            matches!(
                super::zcode_completed_turn_error(result_type),
                Some(ApiError::Stream(_))
            ),
            "{result_type} must surface as a retryable stream error"
        );
    }
}

#[tokio::test]
async fn warm_turn_reports_budget_exhaustion_instead_of_success() {
    let fixture = write_fake_server_with(FakeServerOptions {
        turn_result_type: "error_max_turns",
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "nodelta", TurnInputChannel::LegacySend)
        .expect("turn starts");
    let mut rx = stream.rx_event;

    // The final response is flushed before the error so the partial
    // reply stays in the transcript instead of vanishing with the turn.
    let mut saw_partial_reply = false;
    let err = loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(Ok(ResponseEvent::OutputItemDone(item)))) => {
                assert_eq!(item, assistant_message("nodelta!"));
                saw_partial_reply = true;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(err))) => break err,
            Ok(None) => panic!("stream closed without reporting the failed turn"),
            Err(_elapsed) => panic!("timed out waiting for the turn error"),
        }
    };
    assert!(saw_partial_reply, "partial reply emitted before the error");
    assert!(
        matches!(err, codex_api::ApiError::InvalidRequest { ref message } if message.contains("max turns")),
        "budget exhaustion must be non-retryable, got {err:?}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn compat_retry_strips_unrecognized_keys_and_retries_once() {
    let fixture = write_fake_server_with(FakeServerOptions {
        rejects_unknown_create_keys: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");

    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    assert_eq!(session_id, "sess_fake");

    // The first create was rejected over the dropped flag, the retry passed.
    // The fake counts the attempt in its dispatcher, so the rejected create
    // also logs "create".
    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let rejects = stats_text
        .lines()
        .filter(|line| *line == "drift-reject")
        .count();
    let creates = stats_text.lines().filter(|line| *line == "create").count();
    assert_eq!(rejects, 1, "the drift rejection is retried exactly once");
    assert_eq!(creates, 2, "the retried create succeeds");
    bridge.kill("test end");
}

#[tokio::test]
async fn compat_retry_does_not_retry_unrelated_rejections() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");

    // A failed callback answer forces the handshake error path without any
    // unrecognized keys, so there must be exactly one create attempt.
    bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    assert_eq!(
        stats_text.lines().filter(|line| *line == "create").count(),
        1,
        "healthy handshakes do not retry"
    );
    bridge.kill("test end");
}

#[test]
fn warm_mode_value_parsing() {
    assert_eq!(warm_mode_from_value(None), WarmMode::Yolo);
    assert_eq!(warm_mode_from_value(Some("yolo")), WarmMode::Yolo);
    assert_eq!(warm_mode_from_value(Some("garbage")), WarmMode::Yolo);
    assert_eq!(warm_mode_from_value(Some("build")), WarmMode::Build);
    assert_eq!(warm_mode_from_value(Some(" build ")), WarmMode::Build);
}

#[tokio::test]
async fn gated_bridge_creates_build_session_and_denies_interaction_callbacks() {
    let fixture = write_fake_server_with(FakeServerOptions {
        probes_interactions: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Build,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(lines.contains(&"mode:build"), "stats: {stats_text}");
    assert!(
        lines.contains(&"permission:{\"decision\":\"deny\",\"reason\":\"Denied by the Codex host: the warm ZCode bridge has no interactive approver\"}"),
        "stats: {stats_text}"
    );
    assert!(
        lines.contains(&"userinput:{\"action\":\"decline\",\"reason\":\"Declined by the Codex host: the warm ZCode bridge has no interactive approver\"}"),
        "stats: {stats_text}"
    );
    bridge.kill("test end");
}

/// The stats line the fake server records for the permission probe, computed
/// through the policy itself so the assertion cannot drift from it.
fn permission_stats_line(policy: WarmPermissionPolicy, risk: RiskTier) -> String {
    let answer = policy.resolve(&PermissionRequest {
        tool_name: "Bash",
        risk,
        session_id: "sess_fake",
        request_id: "req_1",
    });
    format!(
        "permission:{}",
        serde_json::json!({ "decision": answer.decision, "reason": answer.reason })
    )
}

#[tokio::test]
async fn allow_safe_policy_auto_approves_medium_risk_requests() {
    let fixture = write_fake_server_with(FakeServerOptions {
        probes_interactions: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Build,
        WarmPermissionPolicy::AllowSafe,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let expected = permission_stats_line(WarmPermissionPolicy::AllowSafe, RiskTier::Medium);
    assert!(
        stats_text.lines().any(|line| line == expected),
        "stats: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn allow_safe_policy_denies_high_risk_requests() {
    let fixture = write_fake_server_with(FakeServerOptions {
        probes_interactions: true,
        permission_risk_level: "high",
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Build,
        WarmPermissionPolicy::AllowSafe,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let expected = permission_stats_line(WarmPermissionPolicy::AllowSafe, RiskTier::High);
    assert!(
        stats_text.lines().any(|line| line == expected),
        "stats: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn reannounced_permission_request_is_answered_each_time() {
    let fixture = write_fake_server_with(FakeServerOptions {
        probes_interactions: true,
        reannounces_probe: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Build,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    // Every reannouncement carries a protocol id the core expects answered;
    // a bridge that deduped answers would wedge the handshake here.
    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let deny_line = permission_stats_line(WarmPermissionPolicy::DenyAll, RiskTier::Medium);
    assert_eq!(
        stats_text.lines().filter(|line| *line == deny_line).count(),
        2,
        "both reannouncements are answered; stats: {stats_text}"
    );
    // The user-input probe only runs after the reannouncement resolves, so
    // reaching it proves the whole handshake survived.
    assert!(
        stats_text
            .lines()
            .any(|line| line.starts_with("userinput:")),
        "stats: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn yolo_bridge_rejects_interaction_callbacks() {
    let fixture = write_fake_server_with(FakeServerOptions {
        probes_interactions: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(lines.contains(&"mode:yolo"), "stats: {stats_text}");
    assert!(
        lines.contains(&"permission:rejected:-32601"),
        "stats: {stats_text}"
    );
    assert!(
        lines.contains(&"userinput:rejected:-32601"),
        "stats: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn bridge_fast_fails_runtime_header_callbacks_with_host_fallback_shapes() {
    let fixture = write_fake_server_with(FakeServerOptions {
        probes_runtime_callbacks: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(
        lines.contains(
            &"headers:{\"errorMessage\":\"Provider request auth is unavailable\",\"headersApplied\":false}"
        ),
        "stats: {stats_text}"
    );
    assert!(
        lines.contains(&"mcpauth:{\"ok\":false,\"reason\":\"official_auth_unavailable\"}"),
        "stats: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn respawned_bridge_resumes_the_previous_session_instead_of_creating() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let previous = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    bridge.kill("simulated child death");

    // The replacement bridge is what the client builds after the child
    // died: same runtime, the predecessor's session id and last observed
    // seq handed over.
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Predecessor {
            session_id: previous.clone(),
            last_event_seq: 7,
        },
    )
    .expect("spawn replacement app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session resumed");
    assert_eq!(session_id, previous);

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(lines.contains(&"resume"), "stats: {stats_text}");
    assert!(
        !lines.contains(&"create"),
        "a resumed bridge must not create a fresh session: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn resumed_bridge_catches_up_events_missed_while_disconnected() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let previous = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    assert_eq!(bridge.last_event_seq(), 7);
    bridge.kill("simulated child death");

    // The replacement resumes from the predecessor's seq; the fake server
    // reports three envelopes the dead bridge never read.
    let fixture = write_fake_server_with(FakeServerOptions {
        missed_events: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Predecessor {
            session_id: previous.clone(),
            last_event_seq: 7,
        },
    )
    .expect("spawn replacement app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session resumed");
    assert_eq!(session_id, previous);
    assert_eq!(
        bridge.last_event_seq(),
        10,
        "replayed envelopes advance the tracked seq"
    );

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    assert!(
        stats_text.lines().any(|line| line == "events:7"),
        "the catch-up must fetch from the predecessor's seq: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn resume_failure_falls_back_to_creating_a_fresh_session() {
    let fixture = write_fake_server_with(FakeServerOptions {
        fails_resume: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Predecessor {
            session_id: "sess_prev".to_string(),
            last_event_seq: 0,
        },
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("fresh session created");
    assert_eq!(session_id, "sess_fake");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(lines.contains(&"resume"), "stats: {stats_text}");
    assert!(lines.contains(&"create"), "stats: {stats_text}");
    bridge.kill("test end");
}

#[tokio::test]
async fn recorded_session_is_adopted_via_list_and_resume() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Recorded(ZcodeWarmRecord {
            zcode_session_id: "sess_fake".to_string(),
            workspace_path: "/tmp".to_string(),
            last_event_seq: 41,
            protocol_version: Some(1),
        }),
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("recorded session adopted");
    assert_eq!(session_id, "sess_fake");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    let listed = lines
        .iter()
        .find(|line| line.starts_with("list:"))
        .expect("adoption must confirm the record through session/list");
    assert!(
        listed.contains("\"sessionIds\":[\"sess_fake\"]") && listed.contains("\"workspace\""),
        "the identity query must pin the recorded session: {listed}"
    );
    assert!(lines.contains(&"resume"), "stats: {stats_text}");
    assert!(
        !lines.contains(&"create"),
        "an adopted session must not create a fresh one: {stats_text}"
    );
    // The recorded seq drives the catch-up fetch even though the subscribe
    // head is behind it.
    assert!(
        stats_text.lines().any(|line| line == "events:41"),
        "the adoption must catch up from the recorded seq: {stats_text}"
    );
    // The subscribe result pins the stream head, and the bridge folds it in.
    assert_eq!(bridge.last_event_seq(), 7);
    bridge.kill("test end");
}

#[tokio::test]
async fn recorded_session_missing_on_the_server_creates_a_fresh_one() {
    let fixture = write_fake_server_with(FakeServerOptions {
        empty_session_list: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Recorded(ZcodeWarmRecord {
            zcode_session_id: "sess_gone".to_string(),
            workspace_path: "/tmp".to_string(),
            protocol_version: None,
            last_event_seq: 0,
        }),
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("fresh session created");
    assert_eq!(session_id, "sess_fake");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(
        lines.iter().any(|line| line.starts_with("list:")),
        "the record must be confirmed through session/list: {stats_text}"
    );
    assert!(
        !lines.contains(&"resume"),
        "a session the server forgot must not be resumed: {stats_text}"
    );
    assert!(lines.contains(&"create"), "stats: {stats_text}");
    bridge.kill("test end");
}

#[tokio::test]
async fn recorded_session_in_another_workspace_creates_a_fresh_one() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Recorded(ZcodeWarmRecord {
            zcode_session_id: "sess_elsewhere".to_string(),
            workspace_path: "/elsewhere".to_string(),
            protocol_version: None,
            last_event_seq: 3,
        }),
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("fresh session created");
    assert_eq!(session_id, "sess_fake");

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(
        !lines.iter().any(|line| line.starts_with("list:")),
        "a workspace-mismatched record must be rejected locally: {stats_text}"
    );
    assert!(!lines.contains(&"resume"), "stats: {stats_text}");
    assert!(lines.contains(&"create"), "stats: {stats_text}");
    bridge.kill("test end");
}

#[tokio::test]
async fn collector_fails_fast_when_the_app_server_exits_mid_turn() {
    let fixture = write_fake_server_with(FakeServerOptions {
        exits_mid_turn: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "dying turn", TurnInputChannel::LegacySend)
        .expect("turn starts");
    let mut rx = stream.rx_event;

    let err = loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(Ok(_event))) => continue,
            Ok(Some(Err(err))) => break err,
            Ok(None) => panic!("stream closed without reporting the dead child"),
            Err(_elapsed) => panic!("collector waited out the idle window instead of failing fast"),
        }
    };
    assert!(
        matches!(err, codex_api::ApiError::Stream(ref message) if message.contains("exited mid-turn")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn v4_send_channel_sends_text_through_v4_command() {
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "v4 hello", TurnInputChannel::V4Command)
        .expect("turn starts");
    let mut rx = stream.rx_event;

    loop {
        match next_event(&mut rx).await {
            ResponseEvent::OutputItemAdded(_) => continue,
            ResponseEvent::OutputTextDelta(delta) => {
                assert_eq!(delta, "v4");
                break;
            }
            other => panic!("expected OutputTextDelta, got {other:?}"),
        }
    }
    loop {
        match next_event(&mut rx).await {
            ResponseEvent::Completed { end_turn, .. } => {
                assert_eq!(end_turn, Some(true));
                break;
            }
            ResponseEvent::OutputItemAdded(_)
            | ResponseEvent::OutputTextDelta(_)
            | ResponseEvent::OutputItemDone(_) => continue,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    let command = lines
        .iter()
        .find(|line| line.starts_with("v4command:"))
        .expect("the input must go out on v4/command");
    assert!(
        command.contains("\"type\":\"sendText\"")
            && command.contains("\"text\":\"v4 hello\"")
            && command.contains("\"sessionId\":\"sess_fake\"")
            && command.contains("\"commandId\"")
            && command.contains("\"issuedAt\""),
        "the envelope must carry the v4 sendText shape: {command}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("send:")),
        "the v4 channel must not touch legacy session/send: {stats_text}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn v4_send_failure_falls_back_to_legacy_send() {
    let fixture = write_fake_server_with(FakeServerOptions {
        fails_v4_command: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "fallback", TurnInputChannel::V4Command)
        .expect("turn starts");
    let mut rx = stream.rx_event;

    loop {
        match next_event(&mut rx).await {
            ResponseEvent::Completed { end_turn, .. } => {
                assert_eq!(end_turn, Some(true));
                break;
            }
            ResponseEvent::OutputItemAdded(_)
            | ResponseEvent::OutputTextDelta(_)
            | ResponseEvent::OutputItemDone(_) => continue,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    let stats_text = std::fs::read_to_string(stats_path(&fixture)).expect("stats written");
    let lines: Vec<&str> = stats_text.lines().collect();
    assert!(
        lines.contains(&"v4/command"),
        "the v4 attempt must have been made: {stats_text}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with("send:fallback")),
        "the rejected v4 command must fall back to session/send: {stats_text}"
    );
    bridge.kill("test end");
}
