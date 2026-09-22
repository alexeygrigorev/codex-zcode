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
use crate::zcode_warm_store::ZcodeWarmRecord;
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
struct FakeServerOptions {
    /// Reject `session/create` with a zod unrecognized-key error to exercise
    /// the compat retry.
    rejects_unknown_create_keys: bool,
    /// Probe the host with `interaction/requestPermission` and
    /// `interaction/requestUserInput` callbacks during the create handshake,
    /// recording each answer in the stats file.
    probes_interactions: bool,
    /// Probe the host with the provider-runtime-headers and official-MCP
    /// auth-headers callbacks during the create handshake, recording each
    /// answer in the stats file.
    probes_runtime_callbacks: bool,
    /// Emit `tool_call_*` session events around the turn's model output.
    emits_tool_events: bool,
    /// `resultType` reported on the `turn.completed` payload.
    turn_result_type: &'static str,
    /// Reject `session/resume` so the bridge's create fallback is exercised.
    fails_resume: bool,
    /// Answer `session/send`, emit one delta, then exit, so the collector is
    /// left waiting when the child's stdout closes.
    exits_mid_turn: bool,
    /// Answer `session/list` with no sessions, so a recorded session that
    /// the server forgot cannot be adopted.
    empty_session_list: bool,
}

impl Default for FakeServerOptions {
    fn default() -> Self {
        Self {
            rejects_unknown_create_keys: false,
            probes_interactions: false,
            probes_runtime_callbacks: false,
            emits_tool_events: false,
            turn_result_type: "success",
            fails_resume: false,
            exits_mid_turn: false,
            empty_session_list: false,
        }
    }
}

fn write_fake_server() -> PathBuf {
    write_fake_server_with(FakeServerOptions::default())
}

fn write_fake_server_with(options: FakeServerOptions) -> PathBuf {
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
    toolName: "Bash", riskLevel: "medium", reason: "probe", input: { command: "ls /tmp" },
  };
  if (kind === "input") params.prompt = "Which directory?";
  write({
    id: probeId,
    method: kind === "input" ? "interaction/requestUserInput" : "interaction/requestPermission",
    params,
  });
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
    case "session/send": {
      const content = msg.params.content;
      if (EXIT_MID_TURN) {
        respond({ accepted: true, sessionId: msg.params.sessionId, stateRevision: 1 });
        emit({ sessionId: msg.params.sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: "pa" } });
        process.exit(0);
      }
      if (TOOL_EVENTS) {
        emit({ sessionId: msg.params.sessionId, type: "tool_call_scheduled", payload: { toolCallId: "tc_1", toolName: "Bash", input: { command: "ls /tmp" } } });
        emit({ sessionId: msg.params.sessionId, type: "tool_call_scheduled", payload: { toolCallId: "tc_2", toolName: "Read", input: { file: "/x" } } });
        emit({ sessionId: msg.params.sessionId, type: "tool_call_result", payload: { toolCallId: "tc_1", result: { success: true, content: "out" }, duration: 3 } });
        emit({ sessionId: msg.params.sessionId, type: "tool_call_error", payload: { toolCallId: "tc_2", error: { type: "ToolError", message: "boom" } } });
      }
      if (!content.includes("nodelta")) {
        emit({ sessionId: msg.params.sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: content.slice(0, 2) } });
        emit({ sessionId: msg.params.sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: content.slice(2) } });
      }
      emit({
        sessionId: msg.params.sessionId,
        type: "turn.completed",
        payload: {
          response: content + "!",
          usage: { inputTokens: 10, outputTokens: 2, totalTokens: 12, cacheReadTokens: 5, reasoningTokens: 1 },
          resultType: "%RESULT_TYPE%",
        },
      });
      respond({ accepted: true, sessionId: msg.params.sessionId, stateRevision: 1 });
      break;
    }
    case "session/stop":
      respond({ sessionId: msg.params.sessionId });
      break;
    default:
      write({ id: msg.id, error: { code: -32601, message: "unhandled " + msg.method } });
  }
}
const DRIFT_KEY = "titleGenerationEnabled";
const REJECT_DRIFT = %REJECT_DRIFT%;
const PROBE_INTERACTIONS = %PROBE_INTERACTIONS%;
const PROBE_RUNTIME = %PROBE_RUNTIME%;
const TOOL_EVENTS = %TOOL_EVENTS%;
const FAIL_RESUME = %FAIL_RESUME%;
const EXIT_MID_TURN = %EXIT_MID_TURN%;
const EMPTY_LIST = %EMPTY_LIST%;
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
        .replace("%RESULT_TYPE%", options.turn_result_type);
    std::fs::write(&fixture, script).expect("write fake app-server fixture");
    fixture
}

fn stats_path(fixture: &Path) -> PathBuf {
    PathBuf::from(format!("{}.stats", fixture.display()))
}

fn test_runtime(fixture: &Path) -> ZcodeRuntime {
    ZcodeRuntime {
        node: "node".to_string(),
        cjs: fixture.to_string_lossy().to_string(),
    }
}

async fn next_event(
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
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge.turn(&session_id, "first").expect("turn starts");
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
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge.turn(&session_id, "toolturn").expect("turn starts");
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
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");

    for prompt in ["first", "second"] {
        let stream = bridge.turn(&session_id, prompt).expect("turn starts");
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
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge
        .turn(&session_id, "nodelta please")
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
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge.turn(&session_id, "nodelta").expect("turn starts");
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
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let previous = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    bridge.kill("simulated child death");

    // The replacement bridge is what the client builds after the child
    // died: same runtime, the predecessor's session id handed over.
    let fixture = write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        SessionSeed::Predecessor(previous.clone()),
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
async fn resume_failure_falls_back_to_creating_a_fresh_session() {
    let fixture = write_fake_server_with(FakeServerOptions {
        fails_resume: true,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        SessionSeed::Predecessor("sess_prev".to_string()),
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
        SessionSeed::Recorded(ZcodeWarmRecord {
            zcode_session_id: "sess_fake".to_string(),
            workspace_path: "/tmp".to_string(),
            last_event_seq: 41,
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
        SessionSeed::Recorded(ZcodeWarmRecord {
            zcode_session_id: "sess_gone".to_string(),
            workspace_path: "/tmp".to_string(),
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
        SessionSeed::Recorded(ZcodeWarmRecord {
            zcode_session_id: "sess_elsewhere".to_string(),
            workspace_path: "/elsewhere".to_string(),
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
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = bridge.turn(&session_id, "dying turn").expect("turn starts");
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
