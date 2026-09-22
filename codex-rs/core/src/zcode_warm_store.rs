//! On-disk record binding each Codex thread to the ZCode session that backs
//! its warm bridge (issue #42 stage 2).
//!
//! The in-memory handoff only survives app-server respawns: a full zcodex
//! restart drops the thread's warm bridge together with the session id only
//! it knew. Storing the id per thread lets the next process adopt the same
//! ZCode session — server-side history and provider cache intact — instead
//! of paying a full transcript re-send through `session/create`.
//!
//! Records live in `$CODEX_HOME/zcode-warm/<thread_id>.json`, one file per
//! thread so concurrent Codex sessions never clobber each other. Every
//! operation is best-effort: persistence only upgrades the next startup, so
//! any failure degrades to creating a fresh session and is logged, never
//! surfaced to the turn.

use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use tracing::warn;

use crate::config::find_codex_home;

/// One thread's adopted ZCode session.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct ZcodeWarmRecord {
    pub(crate) zcode_session_id: String,
    pub(crate) workspace_path: String,
    /// Last `session/event` seq this thread observed. The subscription
    /// starts live-only, so adoption never replays this far; the value is
    /// what makes an interrupted turn recoverable later through
    /// `session/events` catch-up.
    #[serde(default)]
    pub(crate) last_event_seq: u64,
    /// `protocol.version` the core reported at the last handshake. The
    /// desktop app auto-updates zcode.cjs under us, so a change between
    /// processes is the visible edge of a protocol drift and gets a loud
    /// warning (issue #44).
    #[serde(default)]
    pub(crate) protocol_version: Option<u32>,
}

/// Reads the thread's record, or `None` when absent or unreadable.
pub(crate) fn load_record(thread_id: &ThreadId) -> Option<ZcodeWarmRecord> {
    match find_codex_home() {
        Ok(codex_home) => load_record_from(&record_dir(&codex_home), thread_id),
        Err(e) => {
            warn!("ZCode warm session record unavailable (no codex home: {e})");
            None
        }
    }
}

/// Persists the thread's record, replacing any previous one; a changed
/// `protocol_version` warns loudly, since the vendored binary drifted under
/// us and the warm bridge's wire assumptions are worth re-checking.
pub(crate) fn store_record(thread_id: &ThreadId, record: &ZcodeWarmRecord) {
    if let Ok(codex_home) = find_codex_home() {
        let dir = record_dir(&codex_home);
        if let Some(previous) = load_record_from(&dir, thread_id)
            && let Some(message) = protocol_drift_message(&previous, record)
        {
            warn!("{message}");
        }
        store_record_to(&dir, thread_id, record);
    }
}

/// The drift warning for `previous` → `next`, or `None` when the recorded
/// protocol version did not change.
fn protocol_drift_message(previous: &ZcodeWarmRecord, next: &ZcodeWarmRecord) -> Option<String> {
    match (previous.protocol_version, next.protocol_version) {
        (Some(old), Some(new)) if old != new => Some(format!(
            "ZCode Protocol version drift: zcode.cjs moved from v{old} to v{new} for session \
             {}; re-verify the warm bridge",
            next.zcode_session_id
        )),
        _ => None,
    }
}

fn record_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("zcode-warm")
}

fn record_path(dir: &Path, thread_id: &ThreadId) -> PathBuf {
    dir.join(format!("{thread_id}.json"))
}

fn load_record_from(dir: &Path, thread_id: &ThreadId) -> Option<ZcodeWarmRecord> {
    let path = record_path(dir, thread_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(
                "ZCode warm session record unreadable at {}: {e}",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_slice::<ZcodeWarmRecord>(&bytes) {
        Ok(record) => Some(record),
        Err(e) => {
            warn!(
                "ZCode warm session record unparsable at {}: {e}",
                path.display()
            );
            None
        }
    }
}

fn store_record_to(dir: &Path, thread_id: &ThreadId, record: &ZcodeWarmRecord) {
    let path = record_path(dir, thread_id);
    if let Err(e) = std::fs::create_dir_all(dir) {
        warn!(
            "could not create {} for the ZCode warm session record: {e}",
            dir.display()
        );
        return;
    }
    let serialized = match serde_json::to_vec_pretty(record) {
        Ok(serialized) => serialized,
        Err(e) => {
            warn!("could not serialize the ZCode warm session record: {e}");
            return;
        }
    };
    // Write-then-rename so a crash mid-write cannot leave a half record.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, serialized) {
        warn!(
            "could not write the ZCode warm session record to {}: {e}",
            tmp.display()
        );
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!(
            "could not persist the ZCode warm session record at {}: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
#[path = "zcode_warm_store_tests.rs"]
mod tests;
