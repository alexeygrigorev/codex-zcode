//! One headless ZCode child per ZCode session and checkout, machine-wide.
//!
//! The spawn-per-turn bridge mints a headless `zcode.cjs` session per model
//! call, and every session can edit the checkout it runs in. A single ZCode
//! session must never run as two concurrent children: separate `zcodex`
//! processes (a handoff restarting the same conversation, a retried turn)
//! previously had no shared gate, and the warm bridge and the `zcode`
//! extension tool spawned outside the in-process gate entirely.
//!
//! This module is the single gate every ZCode spawn path takes: an
//! exclusive, advisory `std::fs` file lock keyed by the checkout and the
//! ZCode session identity under the Codex home. The lock is held from
//! before the child spawns until its teardown, so invocations of the same
//! session queue instead of racing, whatever process or code path they
//! belong to. Deliberately NOT covered: different ZCode sessions in the
//! same checkout run concurrently — running several sessions in one
//! project in parallel is a supported workflow, not a race.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use codex_utils_home_dir::find_codex_home;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;

/// How long an acquisition may wait before a wait is logged, so a turn
/// queued behind another invocation is diagnosable instead of looking like
/// a hang.
const LONG_WAIT_LOG_THRESHOLD: Duration = Duration::from_secs(30);

/// How often a blocked acquisition re-polls the lock. `File::try_lock` is
/// polled instead of blocking inside `File::lock` so a cancelled acquirer
/// never leaves a thread stuck in the kernel call.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

const SESSION_LOCK_DIR_NAME: &str = "zcode-session-locks";

/// Holds the single ZCode child slot for one session in one checkout.
///
/// The lock lives on the file descriptor: dropping the permit closes the
/// file and releases it, on every drop path and in every drop order around
/// the child teardown.
#[must_use = "the session slot is only reserved while the permit is held"]
pub struct ZcodeSessionPermit {
    _lock_file: fs::File,
}

impl std::fmt::Debug for ZcodeSessionPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZcodeSessionPermit").finish_non_exhaustive()
    }
}

/// Waits until no other child of this ZCode session is running in `cwd`,
/// then holds that session's slot until the returned permit is dropped.
///
/// `session_key` is the stable identity of the ZCode session across
/// processes (for the model-call bridges, the Codex thread id; for the
/// `zcode` tool, the resumed session id). Invocations that pass different
/// keys never block each other.
pub async fn acquire_zcode_session(cwd: &str, session_key: &str) -> io::Result<ZcodeSessionPermit> {
    acquire_zcode_session_in(session_lock_dir()?, cwd, session_key).await
}

/// [`acquire_zcode_session`] against an explicit lock directory, so tests
/// stay out of the real Codex home.
pub async fn acquire_zcode_session_in(
    lock_dir: PathBuf,
    cwd: &str,
    session_key: &str,
) -> io::Result<ZcodeSessionPermit> {
    let lock_path = session_lock_path(&lock_dir, cwd, session_key);
    let started = Instant::now();
    let permit = acquire_lock_file(lock_path).await?;
    let waited = started.elapsed();
    if waited >= LONG_WAIT_LOG_THRESHOLD {
        warn!(
            "ZCode session was busy for {waited:?}; resuming now that the \
             other invocation finished"
        );
    }
    Ok(permit)
}

async fn acquire_lock_file(lock_path: PathBuf) -> io::Result<ZcodeSessionPermit> {
    // Created once and reused: deleting a lockfile would break mutual
    // exclusion (a holder keeps locking the unlinked inode while the next
    // acquirer creates a fresh one), so the files are left in place.
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(ZcodeSessionPermit { _lock_file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(LOCK_POLL_INTERVAL).await;
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

/// The lock directory shared by every ZCode-capable process on the machine.
fn session_lock_dir() -> io::Result<PathBuf> {
    let dir = find_codex_home()?.to_path_buf().join(SESSION_LOCK_DIR_NAME);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn session_lock_path(lock_dir: &Path, cwd: &str, session_key: &str) -> PathBuf {
    lock_dir.join(format!("{}.lock", session_lock_key(cwd, session_key)))
}

/// Stable key for one session in one checkout: the checkout is
/// canonicalized so the same directory spelled two ways (symlink, trailing
/// slash) lands on one lock, with the raw spelling as the fallback for
/// paths that do not exist yet.
fn session_lock_key(cwd: &str, session_key: &str) -> String {
    let checkout = match fs::canonicalize(cwd) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => fallback_checkout_key(cwd),
    };
    let digest = Sha256::digest(format!("{checkout}\0{session_key}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Same directory even when one caller has a trailing slash and the other
/// does not.
fn fallback_checkout_key(cwd: &str) -> String {
    let trimmed = cwd.trim();
    if trimmed.len() > 1 {
        trimmed.trim_end_matches('/').to_string()
    } else {
        trimmed.to_string()
    }
}
