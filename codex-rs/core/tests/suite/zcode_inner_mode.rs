#![allow(clippy::unwrap_used)]

//! End-to-end execution contract: actual permission callbacks, single inner
//! execution, fresh cold lifecycles, warm reuse, and no replay after failure.

use codex_model_provider_info::WireApi;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::ffi::OsString;
use tempfile::TempDir;

struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}
impl EnvVarGuard {
    fn unset(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, original }
    }
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

#[derive(Clone, Copy)]
enum Lifecycle {
    Cold,
    Warm,
}
#[derive(Clone, Copy)]
enum Outcome {
    Complete,
    Disconnect,
    RejectMode,
    DenyClient,
    UnknownPolicy,
}

async fn probe(
    lifecycle: Lifecycle,
    outcome: Outcome,
    approval: AskForApproval,
    profile: PermissionProfile,
) -> (Vec<String>, String) {
    let work = TempDir::new().unwrap();
    let cjs = work.path().join("stub.cjs");
    let log = work.path().join("protocol.log");
    let effect = work.path().join("effects");
    let source = r#"
const fs = require("fs");
const record = (value) => fs.appendFileSync(%LOG%, value + "\n");
const emit = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const sessionId = "sess_" + process.pid;
record("spawn:" + process.pid);
if (!process.argv.includes("app-server")) { record("unexpected-cli"); process.exit(3); }
let turn = 0;
function permission(tool) {
  emit({ id: "permission-" + tool, method: "interaction/requestPermission", params: {
    sessionId, requestId: "request-" + turn + tool, toolCallId: "tool-" + tool,
    toolName: tool, riskLevel: "high", input: {}, reason: "test execution" } });
}
require("readline").createInterface({ input: process.stdin }).on("line", (line) => {
  const message = JSON.parse(line);
  if (String(message.id).startsWith("permission-")) {
    const tool = message.id.slice("permission-".length);
    const decision = message.result?.decision;
    record("permission:" + tool + ":" + decision);
    if (decision !== "allow") {
      emit({ method: "session/event", params: { sessionId, type: "turn.completed",
        payload: { response: "permission denied", resultType: "success" } } });
      return;
    }
    if (tool === "Bash") {
      fs.appendFileSync(%EFFECT%, "inner\n");
      if (%DISCONNECT%) process.exit(0);
      permission("Write");
    } else {
      fs.writeFileSync(%EFFECT% + ".written", "written by inner Write");
      for (const toolName of ["Bash", "Write"]) {
        emit({ method: "session/event", params: { sessionId, type: "tool_call_scheduled",
          payload: { toolCallId: "tool-" + toolName, toolName,
            input: { command: "echo outer >> " + %EFFECT%, file_path: %EFFECT%, content: "outer" } } } });
      }
      emit({ method: "session/event", params: { sessionId, type: "turn.completed",
        payload: { response: "actual client allowed Bash and Write", resultType: "success" } } });
    }
    return;
  }
  const reply = (result) => emit({ id: message.id, result });
  switch (message.method) {
    case "session/create": record("create:" + message.params.mode); reply({ session: { sessionId }, protocol: { version: 1 } }); break;
    case "session/subscribe": reply({ eventSeq: 0, events: [] }); break;
    case "session/setMode":
      record("mode:" + message.params.mode);
      if (%REJECT_MODE%) emit({ id: message.id, error: { code: -32000, message: "mode rejected" } });
      else reply({});
      break;
    case "session/send": record("send"); turn++; reply({ accepted: true }); permission("Bash"); break;
    case "session/close": record("close:" + process.pid); reply({ closed: true }); break;
    case "session/stop": reply({}); break;
    default: emit({ id: message.id, error: { code: -32601, message: "unsupported " + message.method } });
  }
});
process.stdin.on("end", () => record("eof:" + process.pid));
"#
        .replace("%LOG%", &serde_json::to_string(&log).unwrap())
        .replace("%EFFECT%", &serde_json::to_string(&effect).unwrap())
        .replace("%DISCONNECT%", if matches!(outcome, Outcome::Disconnect) { "true" } else { "false" })
        .replace("%REJECT_MODE%", if matches!(outcome, Outcome::RejectMode) { "true" } else { "false" });
    std::fs::write(&cjs, source).unwrap();
    let fake_home = TempDir::new().unwrap();
    let _guards = (
        EnvVarGuard::set("ZCODE_CJS", cjs.as_os_str()),
        EnvVarGuard::set(
            "ZCODE_WARM",
            OsStr::new(match lifecycle {
                Lifecycle::Cold => "0",
                Lifecycle::Warm => "1",
            }),
        ),
        EnvVarGuard::set("ZCODE_WARM_MODE", OsStr::new("yolo")),
        EnvVarGuard::set("HOME", fake_home.path().as_os_str()),
    );
    let _permission_guard = match outcome {
        Outcome::DenyClient => EnvVarGuard::set("ZCODE_WARM_PERMISSIONS", OsStr::new("deny")),
        Outcome::UnknownPolicy => {
            EnvVarGuard::set("ZCODE_WARM_PERMISSIONS", OsStr::new("unknown-policy"))
        }
        Outcome::Complete | Outcome::Disconnect | Outcome::RejectMode => {
            EnvVarGuard::unset("ZCODE_WARM_PERMISSIONS")
        }
    };
    let server = start_mock_server().await;
    let mut builder =
        test_codex().with_config(|config| config.model_provider.wire_api = WireApi::Zcode);
    let test = builder
        .build_with_auto_env(&server)
        .await
        .expect("build test Codex");
    for _ in 0..if matches!(outcome, Outcome::Complete) {
        2
    } else {
        1
    } {
        test.submit_turn_with_approval_and_permission_profile(
            "execute the probe",
            approval,
            profile.clone(),
        )
        .await
        .expect("outer turn completes");
        if matches!((lifecycle, outcome), (Lifecycle::Cold, Outcome::Complete))
            && approval == AskForApproval::Never
            && profile == PermissionProfile::Disabled
        {
            let lines = std::fs::read_to_string(&log).unwrap();
            assert!(
                lines.lines().last().unwrap().starts_with("eof:"),
                "cold completion must await retirement: {lines}"
            );
        }
    }
    test.codex
        .shutdown_and_wait()
        .await
        .expect("stop test thread");
    let lines = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let effects = std::fs::read_to_string(&effect).unwrap_or_default();
    if matches!(outcome, Outcome::Complete)
        && approval == AskForApproval::Never
        && profile == PermissionProfile::Disabled
    {
        assert_eq!(
            std::fs::read_to_string(effect.with_extension("written")).unwrap(),
            "written by inner Write"
        );
    }
    (lines, effects)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_cold_client_executes_once_and_retires_each_fresh_process() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Cold,
        Outcome::Complete,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await;
    assert_eq!(effects, "inner\ninner\n");
    assert_eq!(
        log.iter().filter(|line| line.starts_with("spawn:")).count(),
        2
    );
    assert_eq!(
        log.iter()
            .filter(|line| line.starts_with("permission:"))
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "permission:Bash:allow",
            "permission:Write:allow",
            "permission:Bash:allow",
            "permission:Write:allow"
        ]
    );
    assert_eq!(
        log.iter()
            .filter(|line| line.starts_with("create:"))
            .cloned()
            .collect::<Vec<_>>(),
        vec!["create:build", "create:build"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_client_reuses_process_with_real_callbacks() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Warm,
        Outcome::Complete,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await;
    assert_eq!(effects, "inner\ninner\n");
    assert_eq!(
        log.iter().filter(|line| line.starts_with("spawn:")).count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|line| *line == "permission:Write:allow")
            .count(),
        2
    );
    assert_eq!(log.iter().filter(|line| *line == "mode:build").count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_client_rejects_restricted_contract_before_spawn() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for (approval, profile) in [
        (AskForApproval::OnRequest, PermissionProfile::Disabled),
        (AskForApproval::Never, PermissionProfile::read_only()),
    ] {
        let (log, effects) = probe(Lifecycle::Cold, Outcome::Complete, approval, profile).await;
        assert_eq!((log, effects), (Vec::<String>::new(), String::new()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_client_disconnect_after_effect_is_not_replayed() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for lifecycle in [Lifecycle::Cold, Lifecycle::Warm] {
        let (log, effects) = probe(
            lifecycle,
            Outcome::Disconnect,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "inner\n");
        assert_eq!(
            log.iter().filter(|line| line.starts_with("spawn:")).count(),
            1
        );
        assert_eq!(log.iter().filter(|line| *line == "send").count(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_client_rejected_mode_does_not_send_or_fallback() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Cold,
        Outcome::RejectMode,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await;
    assert_eq!(effects, "");
    assert_eq!(
        log.iter().filter(|line| line.starts_with("spawn:")).count(),
        1
    );
    assert!(
        !log.iter()
            .any(|line| line == "send" || line == "unexpected-cli")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_client_preserves_explicit_permission_denials() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for lifecycle in [Lifecycle::Cold, Lifecycle::Warm] {
        for policy in [Outcome::DenyClient, Outcome::UnknownPolicy] {
            let (log, effects) = probe(
                lifecycle,
                policy,
                AskForApproval::Never,
                PermissionProfile::Disabled,
            )
            .await;
            assert_eq!(effects, "");
            assert_eq!(
                log.iter()
                    .filter(|line| line.starts_with("permission:"))
                    .cloned()
                    .collect::<Vec<_>>(),
                vec!["permission:Bash:deny"]
            );
        }
    }
}
