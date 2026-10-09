#![allow(clippy::unwrap_used)]

//! Regression test for the Zcode cold wire: a streamed tool call must have
//! exactly one executor.
//!
//! Inner `--mode` follows Codex `/permissions`: `AskForApproval::Never`
//! (YOLO) is that executor. Relaying streamed `tool_call` events into Codex
//! `function_call`s made ToolCallRuntime run the same side effect again.
//! Other approval policies spawn `--mode build`, which denies writes with
//! "No permission client configured" because the cold spawn never attaches
//! a permission client.
//!
//! The stub appends an inner line under `--mode yolo` and still streams a
//! Bash `tool_call` whose command would append an outer line if Codex
//! executed it. The probe must record exactly one line (inner).

use codex_model_provider_info::WireApi;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

/// Restores a process environment variable on drop. Safe under nextest,
/// where every test runs in its own process; under plain `cargo test` the
/// drop restore keeps the mutation window bounded to this test.
struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &OsStr) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Writes the fake `zcode.cjs` the client spawns through `ZCODE_CJS`.
///
/// The stub replays the wire contract `stream_zcode` parses: a
/// `session.updated` line, a `model.streaming` stream, and a final `result`.
/// The first invocation streams one Bash `tool_call` whose command appends
/// the outer line to the probe file if Codex executed it; later
/// invocations only stream text so a continuation turn would still end.
/// Under `--mode yolo` the stub also appends an inner line itself, which
/// is the sole intended execution.
fn write_stub_cjs(dir: &Path, probe: &Path) -> PathBuf {
    let outer_command = format!("echo outer >> {}", probe.display());
    let outer_command_json = serde_json::to_string(&outer_command).unwrap();
    let probe_json = serde_json::to_string(&probe.display().to_string()).unwrap();
    let cjs = dir.join("zcode-stub.cjs");
    let source = format!(
        r#"const fs = require("fs");
const argv = process.argv;
const modeIndex = argv.indexOf("--mode");
const mode = modeIndex >= 0 ? argv[modeIndex + 1] : "";
const probe = {probe_json};
const counter = `${{probe}}.invocations`;
let seen = 0;
try {{
  seen = Number(fs.readFileSync(counter, "utf8"));
}} catch {{
  // First invocation: no counter file yet.
}}
fs.writeFileSync(counter, String(seen + 1));
fs.appendFileSync(counter + ".argv", JSON.stringify(argv) + "\n");
// Inner execution: `--mode yolo` is the sole executor. `--mode build`
// must not append (that argv is the write-blocker regression).
if (mode === "yolo") {{
  fs.appendFileSync(probe, "inner\n");
}}
const emit = (value) => process.stdout.write(`${{JSON.stringify(value)}}\n`);
emit({{ type: "session.updated", sessionId: "stub-session" }});
if (seen === 0) {{
  emit({{
    type: "model.streaming",
    payload: {{ kind: "tool_input_start", toolCallId: "stub-tool-1", toolName: "Bash" }},
  }});
  emit({{
    type: "model.streaming",
    payload: {{
      kind: "tool_call",
      toolCallId: "stub-tool-1",
      toolName: "Bash",
      input: {{ command: {outer_command_json} }},
    }},
  }});
}} else {{
  emit({{
    type: "model.streaming",
    payload: {{ kind: "text_delta", delta: "done" }},
  }});
}}
emit({{ type: "result", response: seen === 0 ? "tool turn" : "done" }});
"#
    );
    std::fs::write(&cjs, source).unwrap();
    cjs
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

struct InnerModeProbe {
    argv_log: String,
    probe_lines: usize,
}

async fn run_inner_mode_probe(approval_policy: AskForApproval) -> InnerModeProbe {
    let work = TempDir::new().unwrap();
    let probe = work.path().join("probe.log");
    let cjs = write_stub_cjs(work.path(), &probe);
    // `HOME` must not be the real one: the Zcode dispatch installs a model
    // override into `~/.zcode/cli/config.json` before spawning.
    let fake_home = TempDir::new().unwrap();

    let _env_guards = (
        EnvVarGuard::set("ZCODE_CJS", cjs.as_os_str()),
        // Pin the spawn-per-turn path regardless of the ambient environment.
        EnvVarGuard::set("ZCODE_WARM", OsStr::new("0")),
        EnvVarGuard::set("HOME", fake_home.path().as_os_str()),
    );

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.model_provider.wire_api = WireApi::Zcode;
    });
    let test = builder.build(&server).await.expect("build test Codex");

    test.submit_turn_with_policies(
        "run the probe command",
        approval_policy,
        SandboxPolicy::DangerFullAccess,
    )
    .await
    .expect("turn completes");

    let probe_lines = std::fs::read_to_string(&probe)
        .unwrap_or_default()
        .lines()
        .count();
    let argv_log =
        std::fs::read_to_string(probe.with_extension("log.invocations.argv")).unwrap_or_default();
    InnerModeProbe {
        argv_log,
        probe_lines,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_cold_wire_executes_streamed_tool_call_once() {
    if !node_available() {
        eprintln!("node is not available; skipping the Zcode wire test");
        return;
    }

    let probe = run_inner_mode_probe(AskForApproval::Never).await;
    assert!(
        probe.argv_log.contains("\"--mode\",\"yolo\""),
        "Codex YOLO (`AskForApproval::Never`) must spawn `--mode yolo` so \
         inner writes are allowed\n\
         stub spawn argv log:\n{}",
        probe.argv_log
    );
    assert_eq!(
        probe.probe_lines, 1,
        "the streamed tool call must be executed exactly once by the inner \
         yolo agent; two lines mean Codex ToolCallRuntime ran it too, zero \
         lines mean inner execution never happened (`--mode build`)\n\
         stub spawn argv log:\n{}",
        probe.argv_log
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_cold_wire_follows_codex_approval_policy() {
    if !node_available() {
        eprintln!("node is not available; skipping the Zcode wire test");
        return;
    }

    let probe = run_inner_mode_probe(AskForApproval::OnRequest).await;
    assert!(
        probe.argv_log.contains("\"--mode\",\"build\""),
        "non-YOLO Codex approval must spawn `--mode build`\n\
         stub spawn argv log:\n{}",
        probe.argv_log
    );
    assert_eq!(
        probe.probe_lines, 0,
        "inner `--mode build` must not execute the streamed tool call; the \
         cold spawn has no permission client\n\
         stub spawn argv log:\n{}",
        probe.argv_log
    );
}
