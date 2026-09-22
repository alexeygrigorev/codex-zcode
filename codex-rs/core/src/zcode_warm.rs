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
//! `ZCODE_WARM_MODE=build` opts into the gated variant of the experiment:
//! the session is created in `build` mode so the core asks before risky
//! actions via `interaction/*` callbacks. The bridge has no human on the
//! other end, so it denies every request and logs it — measuring how far a
//! gated core gets and what a real approval bridge would need (issue #25).
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
use std::sync::atomic::AtomicU64;
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
use crate::zcode_warm_events::bridge_tool_activity_from_notification;
use crate::zcode_warm_resume;

/// Env var enabling the warm bridge (`ZCODE_WARM=1`).
const WARM_BRIDGE_ENV_VAR: &str = "ZCODE_WARM";

/// Env var selecting the warm session mode (`ZCODE_WARM_MODE`): `yolo` (the
/// default, ungated core-side execution) or `build` (the core asks before
/// risky actions; the bridge denies).
const WARM_MODE_ENV_VAR: &str = "ZCODE_WARM_MODE";

/// How the warm session balances gating against tool access.
///
/// `Yolo` is today's behavior: the core executes everything itself and
/// permission callbacks never fire. `Build` creates the session in `build`
/// mode so the core requests permission through `interaction/*` callbacks;
/// the bridge answers every request with a denial and logs it, since there
/// is no interactive approver on the Codex side of the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WarmMode {
    Yolo,
    Build,
}

impl WarmMode {
    /// The `mode` value sent in `session/create`.
    pub(crate) fn wire_session_mode(self) -> &'static str {
        match self {
            WarmMode::Yolo => "yolo",
            WarmMode::Build => "build",
        }
    }
}

/// Resolves [`WarmMode`] from the `ZCODE_WARM_MODE` value; unknown values
/// stay on the default ungated mode.
pub(crate) fn warm_mode_from_value(value: Option<&str>) -> WarmMode {
    match value.map(str::trim) {
        Some("build") => WarmMode::Build,
        _ => WarmMode::Yolo,
    }
}

/// [`warm_mode_from_value`] against the process environment.
pub(crate) fn warm_mode_from_env() -> WarmMode {
    warm_mode_from_value(std::env::var(WARM_MODE_ENV_VAR).ok().as_deref())
}

/// Consecutive startup/protocol failures tolerated before the warm bridge is
/// abandoned for the rest of the Codex thread and every turn falls back to
/// the spawn-per-turn path.
pub(crate) const ZCODE_WARM_MAX_CONSECUTIVE_FAILURES: u32 = 2;

/// Where a replacement bridge looks for the session to pick back up
/// (issue #42). Both resume seeds fall back to `session/create` when the
/// pick-up fails, so a lost session only ever costs the cache warmth.
pub(crate) enum SessionSeed {
    /// Establish a fresh session.
    Fresh,
    /// The bridge this one replaced died with this session established in
    /// memory, so the session is worth resuming directly.
    Predecessor(String),
    /// A session recorded on disk for this Codex thread by a previous
    /// process: survive a full zcodex restart by confirming the record
    /// still exists server-side (`session/list`) before resuming it.
    Recorded(crate::zcode_warm_store::ZcodeWarmRecord),
}

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

/// A rejected request, keeping the zod issue detail the core attaches to
/// `Invalid params` rejections so version drift can be retried and logged.
#[derive(Debug, Clone)]
pub(crate) struct ProtocolRequestError {
    pub(crate) message: String,
    /// Root-level keys the core flagged as unrecognized, if it did.
    pub(crate) unrecognized_keys: Vec<String>,
}

impl ProtocolRequestError {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unrecognized_keys: Vec::new(),
        }
    }

    fn from_protocol_error(error: &serde_json::Value) -> Self {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown ZCode Protocol error");
        let mut parsed = Self::plain(message);
        // Zod rejections embed their issue array as JSON in `data.message`.
        if error.get("code").and_then(serde_json::Value::as_i64) == Some(-32602)
            && let Some(issues) = error
                .pointer("/data/message")
                .and_then(serde_json::Value::as_str)
                .and_then(|issues| serde_json::from_str::<serde_json::Value>(issues).ok())
            && let Some(items) = issues.as_array()
        {
            parsed.unrecognized_keys = items
                .iter()
                .filter(|issue| {
                    issue.get("code").and_then(serde_json::Value::as_str)
                        == Some("unrecognized_keys")
                })
                // Only root-level unknown keys are safely strippable; nested
                // ones point at params we do not rebuild.
                .filter(|issue| {
                    issue
                        .get("path")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(std::vec::Vec::is_empty)
                })
                .filter_map(|issue| issue.get("keys"))
                .filter_map(serde_json::Value::as_array)
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect();
        }
        parsed
    }
}

impl std::fmt::Display for ProtocolRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Default)]
struct WarmState {
    next_id: i64,
    pending: HashMap<i64, oneshot::Sender<Result<serde_json::Value, ProtocolRequestError>>>,
}

/// One warm `app-server --stdio` child plus its per-thread session.
///
/// Shared as [`Arc`] and cached on the Codex thread's client state; dropping
/// the last handle kills the child and its process group (spawned shells
/// included).
pub(crate) struct ZcodeWarmBridge {
    state: StdMutex<WarmState>,
    /// Frames for the child's stdin. A dedicated writer task owns the pipe
    /// and is the only place that awaits on writes, so no mutex guard is
    /// ever held across an await point (workspace clippy denies that).
    stdin: mpsc::UnboundedSender<String>,
    child: StdMutex<Child>,
    /// Saved at spawn: the child leads its own process group, so this id
    /// stays valid for group kills even after the direct child exits.
    process_group_pid: u32,
    stderr_tail: Arc<StdMutex<Vec<u8>>>,
    /// Fan-out of `session/event` notification params to turn collectors.
    events: broadcast::Sender<serde_json::Value>,
    /// Created once per bridge; concurrent first turns share the handshake
    /// and failed attempts leave the cell empty for a retry.
    session_id: tokio::sync::OnceCell<String>,
    /// Where this bridge looks for a session to pick back up in
    /// [`Self::ensure_session`] (issue #42): a dead predecessor's session,
    /// a record persisted by an earlier process, or nothing.
    resume_seed: SessionSeed,
    /// Highest `seq` observed on this session's event stream; persisted
    /// with the session id so a later process knows where catch-up would
    /// start from.
    last_event_seq: AtomicU64,
    dead: Arc<AtomicBool>,
    consecutive_failures: AtomicU32,
    request_timeout: Duration,
    mode: WarmMode,
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
    pub(crate) fn spawn(
        runtime: &ZcodeRuntime,
        workspace_path: &str,
        mode: WarmMode,
        resume_seed: SessionSeed,
    ) -> io::Result<Arc<Self>> {
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
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        // The writer task owns the child's stdin for its whole lifetime;
        // losing the pipe only surfaces to senders on the next frame.
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = stdin_rx.recv().await {
                let write_error = match stdin.write_all(line.as_bytes()).await {
                    Err(e) => Some(e),
                    Ok(()) => stdin.flush().await.err(),
                };
                if let Some(e) = write_error {
                    warn!("ZCode warm app-server stdin write failed: {e}");
                    break;
                }
            }
        });
        let bridge = Arc::new(Self {
            state: StdMutex::new(WarmState::default()),
            stdin: stdin_tx,
            child: StdMutex::new(child),
            process_group_pid,
            stderr_tail,
            events,
            session_id: tokio::sync::OnceCell::new(),
            resume_seed,
            last_event_seq: AtomicU64::new(0),
            dead: Arc::clone(&dead),
            consecutive_failures: AtomicU32::new(0),
            request_timeout: REQUEST_TIMEOUT,
            mode,
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
                bridge.fail_collectors(
                    "ZCode warm app-server exited mid-turn; the turn will be retried \
                     in the same session",
                );
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

    /// The established session id, if the handshake completed.
    pub(crate) fn session_id(&self) -> Option<String> {
        self.session_id.get().cloned()
    }

    /// Highest `seq` seen on the session's event stream, or 0 before any.
    pub(crate) fn last_event_seq(&self) -> u64 {
        self.last_event_seq.load(Ordering::Relaxed)
    }

    /// Folds an observed sequence number into [`Self::last_event_seq`].
    pub(crate) fn observe_event_seq(&self, seq: u64) {
        self.last_event_seq.fetch_max(seq, Ordering::Relaxed);
    }

    /// Folds a `session/event` envelope into [`Self::last_event_seq`].
    ///
    /// Older cores may omit `seq` on the envelope; those events are simply
    /// not tracked.
    fn record_event_seq(&self, event: &serde_json::Value) {
        if let Some(seq) = event.get("seq").and_then(serde_json::Value::as_u64) {
            self.observe_event_seq(seq);
        }
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
        self.fail_collectors(reason);
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
            let _ = sender.send(Err(ProtocolRequestError::plain(reason)));
        }
    }

    /// Fails any turn collector waiting on session events with a synthetic
    /// `turn.failed` for the active session.
    ///
    /// Requests get their waiters failed directly, but a collector whose
    /// `session/send` was already answered only watches the event stream;
    /// without this it would wait out the whole idle window after the child
    /// died instead of erroring into the stream-retry ladder immediately.
    fn fail_collectors(&self, reason: &str) {
        if let Some(session_id) = self.session_id.get() {
            let _ = self.events.send(serde_json::json!({
                "sessionId": session_id,
                "type": "turn.failed",
                "payload": { "error": { "message": reason } },
            }));
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
            let params = msg.get("params").unwrap_or(&serde_json::Value::Null);
            self.answer_callback(id, method, params).await;
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
                (_, Some(error)) => Err(ProtocolRequestError::from_protocol_error(error)),
                _ => Err(ProtocolRequestError::plain(format!(
                    "malformed ZCode Protocol response for id {id}"
                ))),
            };
            let _ = sender.send(outcome);
            return;
        }
        if msg.get("method").and_then(|m| m.as_str()) == Some("session/event")
            && let Some(params) = msg.get("params")
        {
            self.record_event_seq(params);
            let _ = self.events.send(params.clone());
        }
    }

    /// Answers core-to-host callback requests the way the desktop host does;
    /// unknown callbacks get a protocol error so the core fails fast instead
    /// of waiting out its request timeout.
    ///
    /// In gated mode the `interaction/*` requests are answered with denials
    /// (the wire result schemas are strict: permission answers carry
    /// `decision`, user-input answers carry `action`), and each request is
    /// logged with a bounded input preview so the experiment shows exactly
    /// what the gated core wanted to do.
    async fn answer_callback(
        &self,
        id: serde_json::Value,
        method: &str,
        params: &serde_json::Value,
    ) {
        let frame = match method {
            "session/requestRuntimePreferences" => {
                // Host-parity answer shape (#36). nativeSearchEnhancements
                // stays off on purpose: the core gates its search runner's
                // find/grep backend on it and our headless host has none.
                // The other two fields match what the core's zod schema
                // already defaults to when missing, so this is explicit
                // parity rather than a behavior change.
                serde_json::json!({
                    "id": id,
                    "result": {
                        "askUserQuestionAutoResolutionEnabled": true,
                        "nativeSearchEnhancementsEnabled": false,
                        "memoryEnabled": false,
                    },
                })
            }
            // Newer cores ask the host for provider/MCP auth material. The
            // real host fast-fails when no resolver is registered instead of
            // leaving the CLI side hanging toward its 180 s timeout; mirror
            // the host's exact fallback shapes (issue #34).
            "interaction/requestProviderRuntimeHeaders" => {
                warn!(
                    "ZCode warm provider runtime-headers callback: no auth resolver, failing fast"
                );
                serde_json::json!({
                    "id": id,
                    "result": {
                        "headersApplied": false,
                        "errorMessage": "Provider request auth is unavailable",
                    },
                })
            }
            "interaction/requestOfficialMcpAuthHeaders" => {
                warn!("ZCode warm official MCP auth-headers callback: no resolver, failing fast");
                serde_json::json!({
                    "id": id,
                    "result": {
                        "ok": false,
                        "reason": "official_auth_unavailable",
                    },
                })
            }
            "interaction/requestPermission" if self.mode == WarmMode::Build => {
                let input_preview = params
                    .get("input")
                    .map(serde_json::to_string)
                    .and_then(Result::ok)
                    .map(|input| input.chars().take(200).collect::<String>())
                    .unwrap_or_default();
                warn!(
                    "ZCode warm permission request denied: tool={} risk={} reason={} input={}",
                    params
                        .get("toolName")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown"),
                    params
                        .get("riskLevel")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown"),
                    params
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(""),
                    input_preview,
                );
                serde_json::json!({
                    "id": id,
                    "result": {
                        "decision": "deny",
                        "reason": "Denied by the Codex host: the warm ZCode bridge has no \
                                   interactive approver",
                    },
                })
            }
            "interaction/requestUserInput" if self.mode == WarmMode::Build => {
                let prompt = params
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>();
                warn!("ZCode warm user-input request declined: prompt={prompt}");
                serde_json::json!({
                    "id": id,
                    "result": {
                        "action": "decline",
                        "reason": "Declined by the Codex host: the warm ZCode bridge has no \
                                   interactive approver",
                    },
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
                let _ = self.write_line(line);
            }
            Err(e) => warn!("could not serialize callback response: {e}"),
        }
    }

    fn write_line(&self, mut line: String) -> io::Result<()> {
        if !line.ends_with('\n') {
            line.push('\n');
        }
        self.stdin
            .send(line)
            .map_err(|_| io::Error::other("ZCode warm app-server stdin is closed"))
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ProtocolRequestError> {
        if self.is_dead() {
            return Err(ProtocolRequestError::plain(
                "ZCode warm app-server is no longer running",
            ));
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
        let line = serde_json::to_string(&frame).map_err(|e| {
            ProtocolRequestError::plain(format!("could not serialize {method} request: {e}"))
        })?;
        if let Err(e) = self.write_line(line) {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .remove(&id);
            return Err(ProtocolRequestError::plain(format!(
                "could not send {method} to ZCode warm app-server: {e}"
            )));
        }
        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(mut error))) => {
                error.message = format!("{method} failed: {}", error.message);
                Err(error)
            }
            Ok(Err(_dropped)) => Err(ProtocolRequestError::plain(format!(
                "{method} wait aborted: reader exited"
            ))),
            Err(_) => {
                self.kill(&format!("{method} timed out"));
                Err(ProtocolRequestError::plain(format!("{method} timed out")))
            }
        }
    }

    /// Sends a request, retrying once without unrecognized keys when the
    /// core's zod schema rejects our params.
    ///
    /// The desktop app auto-updates zcode.cjs under us, so its protocol
    /// schema can drift without a code change on our side; the desktop host
    /// answers the same drift by stripping the flagged fields and retrying.
    /// Drift is logged loudly so the protocol change gets noticed.
    pub(crate) async fn request_with_compat_retry(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match self.request(method, params.clone()).await {
            Ok(result) => Ok(result),
            Err(error) if !error.unrecognized_keys.is_empty() => {
                let mut stripped = params;
                if let Some(object) = stripped.as_object_mut() {
                    for key in &error.unrecognized_keys {
                        object.remove(key);
                    }
                }
                warn!(
                    "ZCode Protocol drift: {method} rejected unrecognized keys {:?}; \
                     retrying without them",
                    error.unrecognized_keys
                );
                self.request(method, stripped)
                    .await
                    .map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Establishes (once) and subscribes to the per-thread session, picking
    /// up the predecessor's session after an app-server respawn or the
    /// recorded session after a full zcodex restart (issue #42).
    ///
    /// The `OnceCell` makes concurrent first turns share one handshake while
    /// keeping no lock guard across the awaited requests.
    pub(crate) async fn ensure_session(&self, workspace_path: &str) -> Result<String, String> {
        self.session_id
            .get_or_try_init(|| {
                zcode_warm_resume::establish_session(
                    self,
                    workspace_path,
                    &self.resume_seed,
                    self.mode,
                )
            })
            .await
            .cloned()
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
                .request_with_compat_retry(
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
            let mut tool_names = HashMap::new();
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
                    Some("tool_call_scheduled" | "tool_call_result" | "tool_call_error") => {
                        // Display-only tool activity from the core's own
                        // agent loop (issue #38): surfaced as an event, never
                        // as a ResponseItem the ToolCallRuntime would run.
                        if let Some(activity) =
                            bridge_tool_activity_from_notification(&notification, &mut tool_names)
                            && tx
                                .send(Ok(codex_api::ResponseEvent::BridgeToolActivity(activity)))
                                .await
                                .is_err()
                        {
                            stop_turn(&collector_bridge, &collector_session).await;
                            return;
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
                        let result_type = payload
                            .get("resultType")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("success");
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
                        match zcode_completed_turn_error(result_type) {
                            Some(err) => {
                                let _ = tx.send(Err(err)).await;
                            }
                            None => {
                                let token_usage = token_usage_from_turn_payload(&payload);
                                let _ = tx
                                    .send(Ok(codex_api::ResponseEvent::Completed {
                                        response_id: request_id.clone(),
                                        token_usage: Some(token_usage),
                                        usage_metadata: None,
                                        end_turn: Some(true),
                                    }))
                                    .await;
                            }
                        }
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

/// Classifies the `resultType` carried by a `turn.completed` payload into
/// the API error Codex should act on, or `None` when the turn finished
/// normally. ZCode reports turn-end reasons on the completed event itself;
/// treating every completion as success made failed turns look clean.
///
/// Budget stops (`error_max_turns`, `error_max_budget`,
/// `error_max_tool_calls`) and cancellations are deterministic: re-sending
/// the identical prompt cannot succeed differently, so they surface as
/// non-retryable invalid-request errors rather than entering the
/// session-level stream retry ladder. `error_during_execution` stays a
/// retryable stream error, matching the `turn.failed` path, because
/// provider-side execution failures are often transient. Unknown values
/// (from newer cores) take the same conservative-as-transient path.
fn zcode_completed_turn_error(result_type: &str) -> Option<ApiError> {
    match result_type {
        "success" => None,
        "cancelled" => Some(ApiError::InvalidRequest {
            message: "ZCode warm turn was cancelled".to_string(),
        }),
        "error_max_turns" => Some(ApiError::InvalidRequest {
            message: "ZCode warm turn stopped: max turns exhausted".to_string(),
        }),
        "error_max_budget" => Some(ApiError::InvalidRequest {
            message: "ZCode warm turn stopped: turn budget exhausted".to_string(),
        }),
        "error_max_tool_calls" => Some(ApiError::InvalidRequest {
            message: "ZCode warm turn stopped: max tool calls exhausted".to_string(),
        }),
        "error_during_execution" => Some(ApiError::Stream(
            "ZCode warm turn failed during execution".to_string(),
        )),
        other => Some(ApiError::Stream(format!(
            "ZCode warm turn ended with unknown resultType {other}"
        ))),
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
