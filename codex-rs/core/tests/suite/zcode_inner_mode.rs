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
const writeProbe = `${{probe}}.write`;
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
  fs.writeFileSync(writeProbe, "inner write\n");
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
  emit({{
    type: "model.streaming",
    payload: {{
      kind: "tool_call",
      toolCallId: "stub-write-1",
      toolName: "Write",
      input: {{ file_path: writeProbe, content: "outer write\n" }},
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
    written_content: String,
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
        written_content: std::fs::read_to_string(probe.with_extension("log.write"))
            .unwrap_or_default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_cold_wire_executes_streamed_tool_call_once() {
    if !node_available() {
        eprintln!("node is not available; skipping the Zcode wire test");
        return;
    }

    let probe = run_inner_mode_probe(AskForApproval::Never).await;
    assert_eq!(probe.written_content, "inner write\n");
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
    assert_eq!(
        probe.argv_log, "",
        "unsupported approval must fail before spawning"
    );
    assert_eq!(
        probe.probe_lines, 0,
        "inner `--mode build` must not execute the streamed tool call; the \
         cold spawn has no permission client\n\
         stub spawn argv log:\n{}",
        probe.argv_log
    );
}

#[derive(Clone, Copy)]
enum ModeAcknowledgement {
    Accept,
    Reject,
}

async fn run_warm_mode_probe(
    acknowledgement: ModeAcknowledgement,
    approval_policy: AskForApproval,
    warm_mode: &str,
) -> Vec<String> {
    let work = TempDir::new().unwrap();
    let log = work.path().join("warm.log");
    let cjs = work.path().join("warm-stub.cjs");
    let source = r#"
const fs = require("fs");
const log = (value) => fs.appendFileSync(%LOG%, value + "\n");
const emit = (value) => process.stdout.write(JSON.stringify(value) + "\n");
if (!process.argv.includes("app-server")) {
  const index = process.argv.indexOf("--mode");
  log("cold:" + process.argv[index + 1]);
  emit({ type: "result", response: "cold fallback denied effects" });
} else {
  require("readline").createInterface({ input: process.stdin }).on("line", (line) => {
    const message = JSON.parse(line);
    const reply = (result) => emit({ id: message.id, result });
    switch (message.method) {
      case "session/create":
        log("create:" + message.params.mode);
        reply({ session: { sessionId: "sess_warm_policy" }, protocol: { version: 1 } });
        break;
      case "session/subscribe":
        reply({ eventSeq: 0, events: [] });
        break;
      case "session/setMode":
        log("setMode:" + message.params.mode);
        if (%REJECT%) emit({ id: message.id, error: { code: -32000, message: "mode rejected" } });
        else reply({});
        break;
      case "session/send":
        log("send");
        reply({ accepted: true });
        emit({ method: "session/event", params: { sessionId: message.params.sessionId, type: "turn.completed", payload: { response: "warm policy honored", resultType: "success" } } });
        break;
      default:
        emit({ id: message.id, error: { code: -32601, message: "unsupported " + message.method } });
    }
  });
}
"#
        .replace("%LOG%", &serde_json::to_string(&log).unwrap())
        .replace("%REJECT%", match acknowledgement { ModeAcknowledgement::Accept => "false", ModeAcknowledgement::Reject => "true" });
    std::fs::write(&cjs, source).unwrap();
    let fake_home = TempDir::new().unwrap();
    let _env_guards = (
        EnvVarGuard::set("ZCODE_CJS", cjs.as_os_str()),
        EnvVarGuard::set("ZCODE_WARM", OsStr::new("1")),
        EnvVarGuard::set("ZCODE_WARM_MODE", OsStr::new(warm_mode)),
        EnvVarGuard::set("HOME", fake_home.path().as_os_str()),
    );
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.model_provider.wire_api = WireApi::Zcode;
    });
    let test = builder
        .build_with_auto_env(&server)
        .await
        .expect("build test Codex");
    test.submit_turn_with_policies(
        "report the enforced policy",
        approval_policy,
        SandboxPolicy::DangerFullAccess,
    )
    .await
    .expect("turn completes");
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_wire_enforces_approval_policy_before_send() {
    if !node_available() {
        return;
    }
    assert_eq!(
        run_warm_mode_probe(ModeAcknowledgement::Accept, AskForApproval::Never, "build").await,
        vec!["create:build", "setMode:build", "send"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_wire_rejected_mode_falls_back_without_sending() {
    if !node_available() {
        return;
    }
    assert_eq!(
        run_warm_mode_probe(ModeAcknowledgement::Reject, AskForApproval::Never, "build").await,
        vec!["create:build", "setMode:build", "cold:build"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_fallback_retains_stricter_environment_mode() {
    if !node_available() {
        return;
    }
    assert_eq!(
        run_warm_mode_probe(ModeAcknowledgement::Reject, AskForApproval::Never, "build").await,
        vec!["create:build", "setMode:build", "cold:build"]
    );
}
