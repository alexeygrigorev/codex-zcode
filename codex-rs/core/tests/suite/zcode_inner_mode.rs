#![allow(clippy::unwrap_used)]

//! Regression test for the Zcode cold wire: a streamed tool call must have
//! exactly one executor.
//!
//! The client maps the inner ZCode agent's streamed `tool_call` events into
//! Codex `function_call`s and executes them through the outer
//! ToolCallRuntime. The cold spawn previously ran the headless child with
//! `--mode yolo`, so the inner agent executed the same call inside its own
//! loop: every side effect happened twice and the inner output was never
//! recorded. The spawn now uses `--mode build`, which denies inner execution
//! while still streaming the call. The stub cjs here simulates the old
//! behavior — under `--mode yolo` it appends an inner line to the probe file
//! itself — so this test fails on the old argv (two lines) and passes only
//! when the outer runtime is the single executor (one line).

use codex_model_provider_info::WireApi;
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
/// the outer line to the probe file; later invocations (the post-tool
/// continuation request) only stream text so the turn ends. Under the old
/// `--mode yolo` argv the stub also appends an inner line itself,
/// reproducing the double execution this test guards against.
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
// Old inner execution: under yolo the headless agent ran the streamed call
// itself. Under build it must stay idle.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_cold_wire_executes_streamed_tool_call_once() {
    if !node_available() {
        eprintln!("node is not available; skipping the Zcode wire test");
        return;
    }

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

    test.submit_turn_with_policy("run the probe command", SandboxPolicy::DangerFullAccess)
        .await
        .expect("turn completes");

    let probe_lines = std::fs::read_to_string(&probe)
        .unwrap_or_default()
        .lines()
        .count();
    let stub_argv_log =
        std::fs::read_to_string(probe.with_extension("log.invocations.argv")).unwrap_or_default();
    assert_eq!(
        probe_lines, 1,
        "the streamed tool call must be executed exactly once by the outer \
         runtime; two lines mean the inner agent ran it too, zero lines mean \
         the outer execution never happened\n\
         stub spawn argv log:\n{stub_argv_log}"
    );
}
