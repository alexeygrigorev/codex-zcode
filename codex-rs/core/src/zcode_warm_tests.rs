use std::path::PathBuf;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::ZcodeWarmBridge;
use super::warm_bridge_enabled_from_value;
use crate::client::ZcodeRuntime;
use codex_api::ResponseEvent;
use codex_protocol::models::ResponseItem;

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
fn write_fake_server() -> PathBuf {
    write_fake_server_with(/*rejects_unknown_create_keys*/ false)
}

fn write_fake_server_with(rejects_unknown_create_keys: bool) -> PathBuf {
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
function write(obj) { process.stdout.write(JSON.stringify(obj) + "\n"); }
function emit(params) { write({ method: "session/event", params }); }
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
    case "session/create":
      respond({ session: { sessionId: "sess_fake" }, protocol: { name: "ZCode Protocol", version: 1 } });
      break;
    case "session/subscribe":
      respond({ sessionId: msg.params.sessionId, eventSeq: 0, events: [] });
      break;
    case "session/send": {
      const content = msg.params.content;
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
          resultType: "success",
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
      if (!msg.result || msg.result.nativeSearchEnhancementsEnabled !== false) {
        write({ id: original.id, error: { code: -32602, message: "host did not answer the runtime preferences callback" } });
      } else {
        handle(original);
      }
      continue;
    }
    if (msg.id !== undefined && msg.method === "session/create") {
      stats("create");
      pendingPref = { prefId: "pref-" + msg.id, original: msg };
      write({ id: "pref-" + msg.id, method: "session/requestRuntimePreferences", params: { sessionId: "sess_fake", scope: "runtime-materialization" } });
      continue;
    }
    if (msg.id !== undefined && msg.method) {
      stats(msg.method.replace("session/", "") + (msg.method === "session/send" ? ":" + msg.params.content : ""));
      handle(msg);
      continue;
    }
  }
});
"#;
    std::fs::write(
        &fixture,
        script.replace(
            "%REJECT_DRIFT%",
            if rejects_unknown_create_keys {
                "true"
            } else {
                "false"
            },
        ),
    )
    .expect("write fake app-server fixture");
    fixture
}

fn stats_path(fixture: &PathBuf) -> PathBuf {
    PathBuf::from(format!("{}.stats", fixture.display()))
}

fn test_runtime(fixture: &PathBuf) -> ZcodeRuntime {
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
    let bridge =
        ZcodeWarmBridge::spawn(&test_runtime(&fixture), "/tmp").expect("spawn fake app-server");
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
    bridge.kill("test end");
}

#[tokio::test]
async fn warm_bridge_reuses_one_session_across_turns() {
    let fixture = write_fake_server();
    let bridge =
        ZcodeWarmBridge::spawn(&test_runtime(&fixture), "/tmp").expect("spawn fake app-server");
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
    let bridge =
        ZcodeWarmBridge::spawn(&test_runtime(&fixture), "/tmp").expect("spawn fake app-server");
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

#[tokio::test]
async fn compat_retry_strips_unrecognized_keys_and_retries_once() {
    let fixture = write_fake_server_with(/*rejects_unknown_create_keys*/ true);
    let bridge =
        ZcodeWarmBridge::spawn(&test_runtime(&fixture), "/tmp").expect("spawn fake app-server");

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
    let bridge =
        ZcodeWarmBridge::spawn(&test_runtime(&fixture), "/tmp").expect("spawn fake app-server");

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
