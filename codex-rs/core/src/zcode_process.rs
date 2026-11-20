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

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::Child;
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

/// How long startup may stay silent before the child emits its first NDJSON
/// stdout line.
///
/// Bounding the pre-first-output phase to 45 seconds ensures silent startup
/// stalls fail and clean up inside a 120-second caller capture budget while
/// preserving the 15-minute idle bound once output begins.
pub(crate) const DEFAULT_ZCODE_STARTUP_TIMEOUT: Duration = Duration::from_secs(45);

/// Grace period between asking the child to shut down and force-killing the
/// process tree.
const TEARDOWN_GRACE_PERIOD: Duration = Duration::from_secs(2);

const STREAM_IDLE_TIMEOUT_ENV_VAR: &str = "ZCODE_STREAM_IDLE_TIMEOUT_SECS";
const STARTUP_TIMEOUT_ENV_VAR: &str = "ZCODE_STARTUP_TIMEOUT_SECS";

/// Resolves the stream idle timeout from `ZCODE_STREAM_IDLE_TIMEOUT_SECS`.
///
/// Unset or invalid values fall back to the default; `0` disables the bound
/// (the turn is then bounded only by the child exiting).
pub(crate) fn zcode_idle_timeout() -> Option<Duration> {
    zcode_idle_timeout_from_value(std::env::var(STREAM_IDLE_TIMEOUT_ENV_VAR).ok().as_deref())
}

/// Resolves the pre-first-output startup timeout from
/// `ZCODE_STARTUP_TIMEOUT_SECS`, capped by `idle_timeout` when both are set.
pub(crate) fn zcode_startup_timeout(idle_timeout: Option<Duration>) -> Option<Duration> {
    zcode_startup_timeout_from_values(
        std::env::var(STARTUP_TIMEOUT_ENV_VAR).ok().as_deref(),
        idle_timeout,
    )
}

/// Pure core of [`zcode_startup_timeout`] so tests avoid mutating the process
/// environment.
///
/// Setting `ZCODE_STARTUP_TIMEOUT_SECS=0` disables the tighter startup limit and
/// falls back to `idle_timeout` so any finite stream idle bound is retained.
fn zcode_startup_timeout_from_values(
    value: Option<&str>,
    idle_timeout: Option<Duration>,
) -> Option<Duration> {
    let startup = match value {
        None => Some(DEFAULT_ZCODE_STARTUP_TIMEOUT),
        Some(value) => match value.trim().parse::<u64>() {
            Ok(0) => idle_timeout,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(_) => {
                warn!("invalid {STARTUP_TIMEOUT_ENV_VAR}; using the default startup timeout");
                Some(DEFAULT_ZCODE_STARTUP_TIMEOUT)
            }
        },
    };
    match (startup, idle_timeout) {
        (Some(startup), Some(idle)) => Some(startup.min(idle)),
        (Some(startup), None) => Some(startup),
        (None, idle) => idle,
    }
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

pub(crate) use codex_file_system::ZcodeSessionPermit;
/// Holds the single in-flight headless ZCode session for one checkout,
/// shared by every ZCode-capable process on the machine.
///
/// `stream_zcode` spawns a new `zcode.cjs` process for every model call,
/// and each process mints its own session (`resume: false`) that can edit
/// the tree. A compaction call, a second thread, the warm bridge, the
/// `zcode` tool, or a second zcodex process pointed at the same checkout
/// is a second writer on the same tree. The permit is an exclusive file
/// lock on a per-checkout lockfile (see `codex-file-system`'s
/// `zcode_session_lock`): the next spawn waits until the current child
/// task drops it, across processes and not just inside this one.
pub(crate) use codex_file_system::acquire_zcode_session;

const TITLE_REQUEST_MARKER: &str = "Generate a concise, single-line task title";
const TITLE_REQUEST_REFUSAL: &str = "Do not answer the request.";

/// Reply for a thread-title request, which must not open a ZCode session.
///
/// Title generation starts a temporary Codex thread in the same checkout as
/// the real turn, a few hundred milliseconds later. On this wire that thread
/// becomes another `zcode.cjs --mode <yolo|build>` process, and the prompt still
/// contains the user's task, so the model does the work. The title is taken
/// from that prompt locally instead.
pub(crate) fn side_request_reply(prompt: &str) -> Option<String> {
    if prompt.contains(TITLE_REQUEST_MARKER) && prompt.contains(TITLE_REQUEST_REFUSAL) {
        return Some(serde_json::json!({ "title": local_thread_title(prompt) }).to_string());
    }
    None
}

/// First few words of the user's own prompt, capped the way the title parser is.
fn local_thread_title(prompt: &str) -> String {
    let source = prompt
        .split_once("User prompt:\n")
        .map(|(_, rest)| rest)
        .or_else(|| {
            prompt
                .split_once("Recent conversation messages:\n")
                .map(|(_, rest)| rest)
        })
        .unwrap_or(prompt);
    let line = source
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with("Attached files:")
                && !line.starts_with('<')
                && !line.starts_with("developer:")
        })
        .unwrap_or("New task");
    let line = line
        .trim_start_matches("user:")
        .trim_start_matches("assistant:")
        .trim();
    let mut title = String::new();
    for (index, word) in line.split_whitespace().take(4).enumerate() {
        if index > 0 {
            title.push(' ');
        }
        if index == 0 {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                title.extend(first.to_uppercase());
                title.push_str(chars.as_str());
            }
        } else {
            title.push_str(word);
        }
    }
    let title: String = title.chars().take(36).collect();
    let title = title
        .trim_end_matches(['.', '?', '!', ','])
        .trim()
        .to_string();
    if title.is_empty() {
        "New task".to_string()
    } else {
        title
    }
}

/// Outcome of racing the child's first stdout line against exit and the
/// pre-first-output startup deadline.
#[derive(Debug)]
pub(crate) enum ZcodeStartupSignal {
    FirstLine(String),
    Exit(io::Result<ExitStatus>),
    Stalled(Duration),
}

/// Races the child's first stdout line against an early exit or a bounded
/// pre-first-output startup deadline.
///
/// If stdout closes or errors while the child is still running, `on_stdout_closed`
/// runs immediately and the wait for `child.wait()` continues under the same
/// absolute startup deadline. `biased` keeps the line branch winning when both
/// line and exit are ready so buffered output is never skipped.
pub(crate) async fn wait_for_first_output<R>(
    child: &mut Child,
    lines: &mut tokio::io::Lines<R>,
    startup_timeout: Option<Duration>,
    on_stdout_closed: impl FnOnce(),
) -> ZcodeStartupSignal
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut on_stdout_closed = Some(on_stdout_closed);
    let mut stdout_closed = false;
    let deadline = startup_timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let sleep = async {
        match (startup_timeout, deadline) {
            (Some(timeout), Some(deadline)) => {
                tokio::time::sleep_until(deadline).await;
                timeout
            }
            _ => {
                std::future::pending::<()>().await;
                unreachable!("pending futures never resolve");
            }
        }
    };
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            biased;
            first_line = lines.next_line(), if !stdout_closed => match first_line {
                Ok(Some(line)) => return ZcodeStartupSignal::FirstLine(line),
                _ => {
                    stdout_closed = true;
                    if let Some(cb) = on_stdout_closed.take() {
                        cb();
                    }
                }
            },
            status = child.wait() => {
                return ZcodeStartupSignal::Exit(status);
            }
            timeout = &mut sleep => {
                return ZcodeStartupSignal::Stalled(timeout);
            }
        }
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
