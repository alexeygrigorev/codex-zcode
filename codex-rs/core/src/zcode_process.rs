//! Stall bounding and staged teardown for the ZCode spawn-per-turn bridge.
//!
//! The bridge spawns one headless CLI per turn and trusts it until exit, so a
//! child that wedges without exiting stalls the turn with no ceiling, and
//! killing only the direct child on abort can strand shells the child
//! spawned. This module borrows the two patterns the ZCode desktop host uses
//! for the same binary: a timeout on the stream (`ZCodeProtocolClient`'s
//! default 3-minute RPC bound) and the staged `disposeAndWaitOnce()`
//! teardown (graceful shutdown request, grace period, then force-kill
//! whatever is left of the process tree).

use std::collections::HashMap;
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tracing::warn;

use codex_utils_pty::process_group::kill_child_process_group;
#[cfg(unix)]
use codex_utils_pty::process_group::terminate_process_group;

/// How long the NDJSON stream may stay silent before the turn is failed as
/// stalled.
///
/// The child emits nothing while it runs a tool, and a tool command can
/// legitimately stay silent for its whole timeout (up to ten minutes for a
/// maxed-out shell call), so the default clears that bar with headroom
/// instead of matching the desktop host's 3-minute per-RPC bound.
pub(crate) const DEFAULT_ZCODE_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Grace period between asking the child to shut down and force-killing the
/// process tree.
const TEARDOWN_GRACE_PERIOD: Duration = Duration::from_secs(2);

const STREAM_IDLE_TIMEOUT_ENV_VAR: &str = "ZCODE_STREAM_IDLE_TIMEOUT_SECS";

/// Resolves the stream idle timeout from `ZCODE_STREAM_IDLE_TIMEOUT_SECS`.
///
/// Unset or invalid values fall back to the default; `0` disables the bound
/// (the turn is then bounded only by the child exiting).
pub(crate) fn zcode_idle_timeout() -> Option<Duration> {
    zcode_idle_timeout_from_value(std::env::var(STREAM_IDLE_TIMEOUT_ENV_VAR).ok().as_deref())
}

/// Pure core of [`zcode_idle_timeout`] so tests avoid mutating the process
/// environment.
fn zcode_idle_timeout_from_value(value: Option<&str>) -> Option<Duration> {
    match value {
        None => Some(DEFAULT_ZCODE_STREAM_IDLE_TIMEOUT),
        Some(value) => match value.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(_) => {
                warn!(
                    "invalid {STREAM_IDLE_TIMEOUT_ENV_VAR}={value:?}; using the default idle timeout"
                );
                Some(DEFAULT_ZCODE_STREAM_IDLE_TIMEOUT)
            }
        },
    }
}

/// Future that resolves once the idle window elapses, or never resolves when
/// the timeout is disabled. Yields the enforced window so stall reporting can
/// include it.
pub(crate) async fn idle_elapsed(idle_timeout: Option<Duration>) -> Duration {
    let Some(idle_timeout) = idle_timeout else {
        std::future::pending::<()>().await;
        unreachable!("pending futures never resolve");
    };
    tokio::time::sleep(idle_timeout).await;
    idle_timeout
}

/// Holds the single in-flight headless ZCode session for one working directory.
///
/// `stream_zcode` spawns a new `zcode.cjs` process for every model call, and
/// each process mints its own session (`resume: false`) that can edit the
/// tree. A compaction call or a second thread that starts while the first
/// child is still running is a second writer on the same checkout. This
/// permit is that checkout's lock: the next spawn waits until the current
/// child task drops it.
pub(crate) struct WorkspaceSessionPermit {
    _guard: OwnedMutexGuard<()>,
}

struct WorkspaceSessionGates {
    locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

fn workspace_session_gates() -> &'static WorkspaceSessionGates {
    static GATES: OnceLock<WorkspaceSessionGates> = OnceLock::new();
    GATES.get_or_init(|| WorkspaceSessionGates {
        locks: StdMutex::new(HashMap::new()),
    })
}

/// Same directory even when one caller has a trailing slash and the other does not.
fn workspace_session_key(cwd: &str) -> String {
    let trimmed = cwd.trim();
    if trimmed.len() > 1 {
        trimmed.trim_end_matches('/').to_string()
    } else {
        trimmed.to_string()
    }
}

/// Waits until no other spawn-per-turn ZCode child is running in `cwd`.
pub(crate) async fn acquire_workspace_session(cwd: &str) -> WorkspaceSessionPermit {
    let mutex = {
        let mut locks = workspace_session_gates()
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(workspace_session_key(cwd))
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    };
    WorkspaceSessionPermit {
        _guard: mutex.lock_owned().await,
    }
}

/// One NDJSON line from the ZCode stream, bounded by the idle timeout.
#[derive(Debug, PartialEq)]
pub(crate) enum ZcodeStreamLine {
    Line(String),
    /// Stdout closed or errored; treated as end of stream.
    Eof,
    /// No line arrived within the window; the child is presumed wedged.
    Stalled(Duration),
}

/// Reads the next line, failing the stream as stalled when nothing arrives
/// within the window. Read errors collapse into [`ZcodeStreamLine::Eof`],
/// matching the bridge's pre-existing tolerance of noisy stdout.
pub(crate) async fn next_stream_line<R>(
    lines: &mut tokio::io::Lines<R>,
    idle_timeout: Option<Duration>,
) -> ZcodeStreamLine
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let read = lines.next_line();
    match idle_timeout {
        None => match read.await {
            Ok(Some(line)) => ZcodeStreamLine::Line(line),
            Ok(None) | Err(_) => ZcodeStreamLine::Eof,
        },
        Some(idle_timeout) => match tokio::time::timeout(idle_timeout, read).await {
            Ok(Ok(Some(line))) => ZcodeStreamLine::Line(line),
            Ok(_) => ZcodeStreamLine::Eof,
            Err(_elapsed) => ZcodeStreamLine::Stalled(idle_timeout),
        },
    }
}

/// Staged teardown mirroring the desktop host's `disposeAndWaitOnce()`.
///
/// Stage 1 (end stdin) is satisfied statically: the child is spawned with
/// stdin at EOF, so nothing can block on stdin. Stage 2 asks TERM-aware
/// processes to shut down and waits through the grace period. Stage 3
/// force-kills the whole process tree — the child runs in its own process
/// group, so shells it spawned die with it instead of stranding.
pub(crate) async fn dispose_and_wait_once(child: &mut Child) -> io::Result<ExitStatus> {
    terminate_child_process_tree(child);
    match tokio::time::timeout(TEARDOWN_GRACE_PERIOD, child.wait()).await {
        Ok(status) => status,
        Err(_elapsed) => {
            warn!("ZCode child survived the teardown grace period; killing the process tree");
            force_kill_process_tree(child)?;
            child.wait().await
        }
    }
}

/// Asks the child's process group to terminate, leaving the force-kill as the
/// backstop for anything that ignores the request.
#[cfg(unix)]
fn terminate_child_process_tree(child: &Child) {
    if let Some(pid) = child.id()
        && let Err(e) = terminate_process_group(pid)
    {
        warn!("could not signal the ZCode process group for termination: {e}");
    }
}

#[cfg(not(unix))]
fn terminate_child_process_tree(_child: &Child) {}

/// Force-kills the child and every process it spawned.
#[cfg(unix)]
fn force_kill_process_tree(child: &mut Child) -> io::Result<()> {
    kill_child_process_group(child)
}

#[cfg(windows)]
fn force_kill_process_tree(child: &mut Child) -> io::Result<()> {
    // Process groups do not exist on Windows, so the tree walk goes through
    // `taskkill /T /F`, the same force-kill the desktop host uses.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Some(pid) = child.id() {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    child.start_kill()
}

#[cfg(test)]
#[path = "zcode_process_tests.rs"]
mod tests;
