use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

/// Opt-in, bounded observations only; never a provider, exit, or custody proof.
/// One instance belongs to a ModelClientSession (one Codex turn), including
/// warm fallback, retries and multiple model requests within that turn.
pub(super) struct ZcodeBridgeDiagnostics {
    started: Option<Instant>,
    emitted: AtomicU32,
}

#[derive(Clone, Copy)]
pub(super) enum ZcodeDiagnosticBranch {
    Warm,
    Spawn,
}

#[derive(Clone, Copy)]
pub(super) enum ZcodeDiagnosticPhase {
    BranchSelected,
    PermitWait,
    PermitAcquired,
    ChildSpawned,
    ChildReused,
    WarmSessionReturned,
    WarmStreamCreated,
    WarmFallback,
    FirstInnerFrame,
    TerminalEvent,
    StdoutClosedOrReadError,
    ChildWaitPending,
    ChildWaitSettled,
    LimitReached,
}

impl ZcodeBridgeDiagnostics {
    const MAX_RECORDS: u32 = 16;
    const MAX_LINE_BYTES: usize = 384;

    pub(super) fn from_env() -> Self {
        Self::from_value(std::env::var("ZCODE_BRIDGE_DIAGNOSTICS").ok().as_deref())
    }

    fn from_value(value: Option<&str>) -> Self {
        Self {
            started: (value == Some("1")).then(Instant::now),
            emitted: AtomicU32::new(0),
        }
    }

    fn frame_type(kind: &str) -> &'static str {
        match kind {
            "session.created" => "session.created",
            "session.updated" => "session.updated",
            "turn.started" => "turn.started",
            "turn.completed" => "turn.completed",
            "turn.failed" => "turn.failed",
            "session.error" => "session.error",
            "result" => "result",
            _ => "other",
        }
    }

    fn line_at(
        &self,
        branch: ZcodeDiagnosticBranch,
        phase: ZcodeDiagnosticPhase,
        frame: Option<&str>,
        elapsed: Duration,
    ) -> Option<String> {
        self.started.as_ref()?;
        let previous = self
            .emitted
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < Self::MAX_RECORDS).then_some(count + 1)
            })
            .ok()?;
        let limited = previous + 1 == Self::MAX_RECORDS;
        let phase = if limited {
            ZcodeDiagnosticPhase::LimitReached
        } else {
            phase
        };
        let phase = match phase {
            ZcodeDiagnosticPhase::BranchSelected => "branch_selected",
            ZcodeDiagnosticPhase::PermitWait => "permit_wait",
            ZcodeDiagnosticPhase::PermitAcquired => "permit_acquired",
            ZcodeDiagnosticPhase::ChildSpawned => "child_spawned",
            ZcodeDiagnosticPhase::ChildReused => "child_reused",
            ZcodeDiagnosticPhase::WarmSessionReturned => "warm_session_returned",
            ZcodeDiagnosticPhase::WarmStreamCreated => "warm_stream_created",
            ZcodeDiagnosticPhase::WarmFallback => "warm_fallback",
            ZcodeDiagnosticPhase::FirstInnerFrame => "first_inner_frame",
            ZcodeDiagnosticPhase::TerminalEvent => "terminal_event",
            ZcodeDiagnosticPhase::StdoutClosedOrReadError => "stdout_closed_or_read_error",
            ZcodeDiagnosticPhase::ChildWaitPending => "child_wait_pending",
            ZcodeDiagnosticPhase::ChildWaitSettled => "child_wait_settled",
            ZcodeDiagnosticPhase::LimitReached => "limit_reached",
        };
        let branch = match branch {
            ZcodeDiagnosticBranch::Warm => "warm",
            ZcodeDiagnosticBranch::Spawn => "spawn",
        };
        Some(
            serde_json::json!({
                "kind": "zcodex_bridge_phase_v1",
                "seq": previous + 1,
                "branch": branch,
                "phase": phase,
                "frame": if limited { None } else { frame.map(Self::frame_type) },
                "elapsed_ms": elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
                // No already-bound loaded release identity is available here.
                // Do not read paths, argv, configuration or a process environment value.
                "source_release": "unknown",
                // A successful Child::wait proves only its direct child was reaped.
                "scope": "direct_child_only",
            })
            .to_string(),
        )
    }

    pub(super) fn emit(
        &self,
        branch: ZcodeDiagnosticBranch,
        phase: ZcodeDiagnosticPhase,
        frame: Option<&str>,
    ) {
        let Some(started) = self.started.as_ref() else {
            return;
        };
        let Some(mut line) = self.line_at(branch, phase, frame, started.elapsed()) else {
            return;
        };
        if line.len() > Self::MAX_LINE_BYTES {
            return;
        }
        line.push('\n');
        // Fixed metadata bypasses RUST_LOG only when explicitly enabled.
        // Output errors are ignored, without retry, flushing, or changing a turn.
        use std::io::Write as _;
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }
}

#[cfg(test)]
#[path = "zcode_phase_diagnostics_tests.rs"]
mod tests;
