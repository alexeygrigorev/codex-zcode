//! Warm app-server bridge for the ZCode wire: one long-lived
//! `zcode.cjs app-server --stdio` child per Codex thread, speaking ZCode
//! Protocol v1 over NDJSON stdin/stdout.
//!
//! The spawn-per-turn bridge pays process startup plus a full-transcript
//! re-send on every turn. The app-server surface keeps server-side history
//! instead: `session/create` once, `session/send` per turn, and the harness
//! streams `session/event` notifications back on a subscription. Measured on
//! the real runtime: a follow-up turn completes in ~2.5s (vs ~8s per spawned
//! turn) with ~99% of input tokens served from the provider cache.
//!
//! This is an opt-in experiment (`ZCODE_WARM=1`) that falls back to the
//! spawn-per-turn bridge on any startup/protocol failure. Unlike the spawn
//! bridge, the warm core executes tool calls inside its own agent loop, so
//! tool execution leaves Codex's sandbox and approval flow; that trade is
//! why the experiment is not the default wire.
//!
//! Envelope (JSON-RPC minus the `jsonrpc` member):
//! `{"id", "method", "params"}` requests, bare `{"method", "params"}`
//! notifications, `{"id", "result"}` / `{"id", "error"}` responses. The core
//! also issues its own requests (callbacks) that the reader answers.

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_api::ApiError;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::warn;

use crate::client::ZCODE_STDERR_TAIL_CAP;
use crate::client::ZcodeRuntime;
use crate::client::zcode_failure_message;
use crate::client::zcode_stderr_tail;
use crate::zcode_process;

/// Env var enabling the warm bridge (`ZCODE_WARM=1`).
const WARM_BRIDGE_ENV_VAR: &str = "ZCODE_WARM";

/// Consecutive startup/protocol failures tolerated before the warm bridge is
/// abandoned for the rest of the Codex thread and every turn falls back to
/// the spawn-per-turn path.
pub(crate) const ZCODE_WARM_MAX_CONSECUTIVE_FAILURES: u32 = 2;

/// Requests are answered or the turn fails quickly: session creation takes
/// ~2s in practice and send acceptance is sub-second, so anything past this
/// window means the child is wedged and the spawn bridge should take over.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the collector waits for session events before declaring the
/// turn stalled. Long tool runs inside the core are legitimate silence, so
/// this reuses the stream idle bound.
fn event_timeout() -> Duration {
    zcode_process::zcode_idle_timeout().unwrap_or(zcode_process::DEFAULT_ZCODE_STREAM_IDLE_TIMEOUT)
}

/// Whether the warm bridge was requested for this invocation.
pub(crate) fn warm_bridge_enabled() -> bool {
    warm_bridge_enabled_from_value(std::env::var(WARM_BRIDGE_ENV_VAR).ok().as_deref())
}

/// Pure core of [`warm_bridge_enabled`] so tests avoid mutating the process
/// environment.
fn warm_bridge_enabled_from_value(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1") | Some("true") | Some("yes"))
}

#[derive(Default)]
struct WarmState {
    next_id: i64,
    pending: HashMap<i64, oneshot::Sender<Result<serde_json::Value, String>>>,
}

/// One warm `app-server --stdio` child plus its per-thread session.
///
/// Shared as [`Arc`] and cached on the Codex thread's client state; dropping
/// the last handle kills the child and its process group (spawned shells
/// included).
pub(crate) struct ZcodeWarmBridge {
    state: StdMutex<WarmState>,
    /// Owned here because writes await; a tokio mutex guard is Send, so the
    /// reader task may hold it across `write_all`.
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    child: StdMutex<Child>,
    /// Saved at spawn: the child leads its own process group, so this id
    /// stays valid for group kills even after the direct child exits.
    process_group_pid: u32,
    stderr_tail: Arc<StdMutex<Vec<u8>>>,
    /// Fan-out of `session/event` notification params to turn collectors.
    events: broadcast::Sender<serde_json::Value>,
    session_id: tokio::sync::Mutex<Option<String>>,
    dead: Arc<AtomicBool>,
    consecutive_failures: AtomicU32,
    request_timeout: Duration,
}

impl std::fmt::Debug for ZcodeWarmBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZcodeWarmBridge")
            .field("dead", &self.dead.load(Ordering::Relaxed))
            .field(
                "consecutive_failures",
                &self.consecutive_failures.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl ZcodeWarmBridge {
    /// Spawns the app-server child and starts its reader task.
    pub(crate) fn spawn(runtime: &ZcodeRuntime, workspace_path: &str) -> io::Result<Arc<Self>> {
        let mut command = Command::new(&runtime.node);
        command
            .arg(&runtime.cjs)
            .args(["app-server", "--stdio", "--cwd", workspace_path])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group so teardown reaches shells the core spawned.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;

        let process_group_pid = child.id().ok_or_else(|| {
            io::Error::other("warm ZCode app-server has no process id after spawn")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("warm ZCode app-server has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("warm ZCode app-server has no stdout"))?;

        let stderr_tail = Arc::new(StdMutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            let stderr_tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut stderr = stderr;
                let mut chunk = [0u8; 4096];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut tail = stderr_tail
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            tail.extend_from_slice(&chunk[..read]);
                            if tail.len() > ZCODE_STDERR_TAIL_CAP {
                                let excess = tail.len() - ZCODE_STDERR_TAIL_CAP;
                                tail.drain(..excess);
                            }
                        }
                    }
                }
            });
        }

        let (events, _) = broadcast::channel(4096);
        let dead = Arc::new(AtomicBool::new(false));
        let bridge = Arc::new(Self {
            state: StdMutex::new(WarmState::default()),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            child: StdMutex::new(child),
            process_group_pid,
            stderr_tail,
            events,
            session_id: tokio::sync::Mutex::new(None),
            dead: Arc::clone(&dead),
            consecutive_failures: AtomicU32::new(0),
            request_timeout: REQUEST_TIMEOUT,
        });

        // The reader holds the bridge only weakly: when the thread's last
        // handle drops, the child is killed first and this loop drains out.
        let reader_bridge = Arc::downgrade(&bridge);
        let reader_dead = Arc::clone(&dead);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(bridge) = reader_bridge.upgrade() else {
                    break;
                };
                bridge.handle_line(&line).await;
            }
            // Stdout closed: the child is gone; fail all waiters.
            reader_dead.store(true, Ordering::Relaxed);
            if let Some(bridge) = reader_bridge.upgrade() {
                bridge.fail_pending("ZCode warm app-server closed its output");
            }
        });

        Ok(bridge)
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    pub(crate) fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    pub(crate) fn register_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn register_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub(crate) fn stderr_text(&self) -> String {
        zcode_stderr_tail(&self.stderr_tail)
    }

    /// Kills the child and its process group and fails all waiters.
    ///
    /// Used instead of waiting for `Drop` when a turn must give up on the
    /// child while the bridge handle may still be cached by the thread.
    pub(crate) fn kill(&self, reason: &str) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return;
        }
        warn!("ZCode warm app-server killed: {reason}");
        self.fail_pending(reason);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
        codex_utils_pty::process_group::kill_process_group_by_pid(self.process_group_pid)
            .unwrap_or_else(|e| warn!("ZCode warm process group kill failed: {e}"));
    }

    fn fail_pending(&self, reason: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, sender) in state.pending.drain() {
            let _ = sender.send(Err(reason.to_string()));
        }
    }

    /// Routes one NDJSON frame from the core.
    async fn handle_line(&self, line: &str) {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            // Non-JSON noise from nested subprocesses; the spawn bridge
            // tolerates the same.
            return;
        };
        let id = msg.get("id").cloned();
        if let (Some(id), Some(method)) = (id.clone(), msg.get("method").and_then(|m| m.as_str())) {
            self.answer_callback(id, method).await;
            return;
        }
        if let Some(id) = id {
            let Some(sender) = id.as_i64().and_then(|id| {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending
                    .remove(&id)
            }) else {
                return;
            };
            let outcome = match (msg.get("result"), msg.get("error")) {
                (Some(result), _) => Ok(result.clone()),
                (_, Some(error)) => Err(error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown ZCode Protocol error")
                    .to_string()),
                _ => Err(format!("malformed ZCode Protocol response for id {id}")),
            };
            let _ = sender.send(outcome);
            return;
        }
        if msg.get("method").and_then(|m| m.as_str()) == Some("session/event")
            && let Some(params) = msg.get("params")
        {
            let _ = self.events.send(params.clone());
        }
    }

    /// Answers core-to-host callback requests the way the desktop host does;
    /// unknown callbacks get a protocol error so the core fails fast instead
    /// of waiting out its request timeout.
    async fn answer_callback(&self, id: serde_json::Value, method: &str) {
        let frame = match method {
            "session/requestRuntimePreferences" => {
                serde_json::json!({
                    "id": id,
                    "result": {"nativeSearchEnhancementsEnabled": false},
                })
            }
            other => {
                warn!("ZCode warm app-server callback {other} has no host handler; rejecting");
                serde_json::json!({
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("unsupported host callback: {other}"),
                    },
                })
            }
        };
        match serde_json::to_string(&frame) {
            Ok(line) => {
                let _ = self.write_line(line).await;
            }
            Err(e) => warn!("could not serialize callback response: {e}"),
        }
    }

    async fn write_line(&self, mut line: String) -> io::Result<()> {
        if !line.ends_with('\n') {
            line.push('\n');
        }
        let mut stdin = self.stdin.lock().await;
        let Some(stdin) = stdin.as_mut() else {
            return Err(io::Error::other("ZCode warm app-server stdin is closed"));
        };
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if self.is_dead() {
            return Err("ZCode warm app-server is no longer running".to_string());
        }
        let (id, rx) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.next_id += 1;
            let id = state.next_id;
            let (tx, rx) = oneshot::channel();
            state.pending.insert(id, tx);
            (id, rx)
        };
        let frame = serde_json::json!({ "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&frame)
            .map_err(|e| format!("could not serialize {method} request: {e}"))?;
        if let Err(e) = self.write_line(line).await {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .remove(&id);
            return Err(format!(
                "could not send {method} to ZCode warm app-server: {e}"
            ));
        }
        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => Err(format!("{method} failed: {message}")),
            Ok(Err(_dropped)) => Err(format!("{method} wait aborted: reader exited")),
            Err(_) => {
                self.kill(&format!("{method} timed out"));
                Err(format!("{method} timed out"))
            }
        }
    }

    /// Creates (once) and subscribes to the per-thread session.
    pub(crate) async fn ensure_session(&self, workspace_path: &str) -> Result<String, String> {
        let mut cached = self.session_id.lock().await;
        if let Some(session_id) = cached.as_deref() {
            return Ok(session_id.to_string());
        }
        let created = self
            .request(
                "session/create",
                serde_json::json!({
                    "workspace": {
                        "workspacePath": workspace_path,
                        "workspaceKey": workspace_path,
                    },
                    "mode": "yolo",
                    "titleGenerationEnabled": false,
                }),
            )
            .await?;
        let session_id = created
            .pointer("/session/sessionId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "session/create response carried no sessionId: {}",
                    serde_json::to_string(&created).unwrap_or_default()
                )
            })?
            .to_string();
        self.request(
            "session/subscribe",
            serde_json::json!({
                "sessionId": session_id,
                "deliveryKind": "desktop-continuous",
            }),
        )
        .await?;
        *cached = Some(session_id.clone());
        Ok(session_id)
    }

    /// Sends one user turn and returns the mapped Codex event stream.
    ///
    /// Events arrive as `session/event` notifications on the shared
    /// subscription; the collector filters by session, maps text deltas and
    /// the terminal event, and stops the server-side turn if the consumer
    /// goes away.
    pub(crate) fn turn(
        self: &Arc<Self>,
        session_id: &str,
        content: &str,
    ) -> Result<codex_api::ResponseStream, String> {
        let mut events = self.events.subscribe();
        let send_bridge = Arc::clone(self);
        let send_session = session_id.to_string();
        let send_content = content.to_string();
        let (tx, rx_event) =
            mpsc::channel::<std::result::Result<codex_api::ResponseEvent, ApiError>>(64);
        let request_id = format!("zcode_warm_{}", uuid::Uuid::new_v4());
        let upstream_request_id = Some(request_id.clone());
        let collector_bridge = Arc::downgrade(self);
        let collector_session = session_id.to_string();
        let collector_timeout = event_timeout();
        tokio::spawn(async move {
            let send = send_bridge
                .request(
                    "session/send",
                    serde_json::json!({ "sessionId": send_session, "content": send_content }),
                )
                .await;
            let send = match send {
                Ok(result) => result,
                Err(message) => {
                    let _ = tx.send(Err(ApiError::Stream(message))).await;
                    return;
                }
            };
            if send.get("accepted") != Some(&serde_json::Value::Bool(true)) {
                let _ = tx
                    .send(Err(ApiError::Stream(format!(
                        "session/send was not accepted: {}",
                        serde_json::to_string(&send).unwrap_or_default()
                    ))))
                    .await;
                return;
            }

            let mut reply = String::new();
            let mut started_output = false;
            loop {
                let notification =
                    match tokio::time::timeout(collector_timeout, events.recv()).await {
                        Ok(Ok(params)) => params,
                        Ok(Err(broadcast::error::RecvError::Lagged(_skipped))) => continue,
                        Ok(Err(broadcast::error::RecvError::Closed)) => {
                            let _ = tx
                                .send(Err(ApiError::Stream(
                                    "ZCode warm app-server event stream closed".to_string(),
                                )))
                                .await;
                            return;
                        }
                        Err(_elapsed) => {
                            if let Some(bridge) = collector_bridge.upgrade() {
                                bridge.register_failure();
                                bridge.kill("turn events stalled");
                            }
                            let _ = tx
                                .send(Err(ApiError::Stream(
                                    "ZCode warm turn stalled: no session events before the idle \
                                 window elapsed"
                                        .to_string(),
                                )))
                                .await;
                            return;
                        }
                    };
                if notification
                    .get("sessionId")
                    .and_then(serde_json::Value::as_str)
                    != Some(collector_session.as_str())
                {
                    continue;
                }
                match notification.get("type").and_then(serde_json::Value::as_str) {
                    Some("model.streaming") => {
                        let payload = notification.get("payload");
                        let kind = payload
                            .and_then(|p| p.get("kind"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        let delta = payload
                            .and_then(|p| p.get("delta"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        if kind == "text_delta" && !delta.is_empty() {
                            if !started_output {
                                started_output = true;
                                let item = assistant_message(String::new());
                                if tx
                                    .send(Ok(codex_api::ResponseEvent::OutputItemAdded(item)))
                                    .await
                                    .is_err()
                                {
                                    stop_turn(&collector_bridge, &collector_session).await;
                                    return;
                                }
                            }
                            reply.push_str(delta);
                            if tx
                                .send(Ok(codex_api::ResponseEvent::OutputTextDelta(
                                    delta.to_string(),
                                )))
                                .await
                                .is_err()
                            {
                                stop_turn(&collector_bridge, &collector_session).await;
                                return;
                            }
                        }
                    }
                    Some("turn.failed") => {
                        let payload = notification.get("payload").cloned().unwrap_or_default();
                        let message = zcode_failure_message(&payload)
                            .unwrap_or_else(|| serde_json::to_string(&payload).unwrap_or_default());
                        let _ = tx.send(Err(ApiError::Stream(message))).await;
                        return;
                    }
                    Some("turn.completed") => {
                        let payload = notification.get("payload").cloned().unwrap_or_default();
                        // Prefer the streamed text; fall back to the final
                        // response for turns whose deltas never arrived.
                        let response = if reply.is_empty() {
                            payload
                                .get("response")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string()
                        } else {
                            std::mem::take(&mut reply)
                        };
                        if !response.is_empty()
                            && tx
                                .send(Ok(codex_api::ResponseEvent::OutputItemDone(
                                    assistant_message(response),
                                )))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        let token_usage = token_usage_from_turn_payload(&payload);
                        let _ = tx
                            .send(Ok(codex_api::ResponseEvent::Completed {
                                response_id: request_id.clone(),
                                token_usage: Some(token_usage),
                                usage_metadata: None,
                                end_turn: Some(true),
                            }))
                            .await;
                        return;
                    }
                    _ => {}
                }
            }
        });

        Ok(codex_api::ResponseStream {
            rx_event,
            upstream_request_id,
        })
    }
}

fn assistant_message(text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![codex_protocol::models::ContentItem::OutputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

impl Drop for ZcodeWarmBridge {
    fn drop(&mut self) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
        codex_utils_pty::process_group::kill_process_group_by_pid(self.process_group_pid)
            .unwrap_or_else(|e| warn!("ZCode warm process group kill failed on drop: {e}"));
    }
}

async fn stop_turn(bridge: &std::sync::Weak<ZcodeWarmBridge>, session_id: &str) {
    if let Some(bridge) = bridge.upgrade() {
        let _ = bridge
            .request(
                "session/stop",
                serde_json::json!({ "sessionId": session_id }),
            )
            .await;
    }
}

fn token_usage_from_turn_payload(payload: &serde_json::Value) -> TokenUsage {
    let usage = payload.get("usage");
    let field = |name: &str| {
        usage
            .and_then(|usage| usage.get(name))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    TokenUsage {
        input_tokens: field("inputTokens"),
        cached_input_tokens: field("cacheReadTokens"),
        cache_write_input_tokens: field("cacheWriteTokens"),
        output_tokens: field("outputTokens"),
        reasoning_output_tokens: field("reasoningTokens"),
        total_tokens: field("totalTokens"),
        ..TokenUsage::default()
    }
}

#[cfg(test)]
#[path = "zcode_warm_tests.rs"]
mod tests;
