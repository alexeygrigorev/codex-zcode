#![allow(clippy::unwrap_used)]

//! End-to-end execution contract: actual permission callbacks, single inner
//! execution, fresh cold lifecycles, warm reuse, and no replay after failure.

use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_model_provider_info::WireApi;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
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
    GoalContinuation,
    Disconnect,
    RejectMode,
    WrongModel,
    LostOutput,
    OversizedGoalStream,
    OversizedGoalFallback,
    InterruptSend,
    InterruptGoal,
    DenyClient,
    UnknownPolicy,
    GatedMode,
    TextSegments,
    Compact,
    CompactThenTurn,
    CompactDisconnect,
    AmbiguousV4,
}

async fn start_probe_turn(
    test: &TestCodex,
    message: &str,
    approval: AskForApproval,
    profile: PermissionProfile,
) {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(profile, test.config.cwd.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: message.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(approval),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
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
    let output_peer = if matches!(
        outcome,
        Outcome::LostOutput
            | Outcome::InterruptSend
            | Outcome::InterruptGoal
            | Outcome::OversizedGoalStream
            | Outcome::OversizedGoalFallback
    ) {
        Some(
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let source = r#"
const fs = require("fs");
const record = (value) => fs.appendFileSync(%LOG%, value + "\n");
const emit = (value) => process.stdout.write(JSON.stringify(value) + "\n");
let sessionId = "sess_" + process.pid;
record("spawn:" + process.pid);
if (!process.argv.includes("app-server")) { record("unexpected-cli"); process.exit(3); }
let turn = 0;
let compacting = false;
let noTools = false;
let holdInput = false;
const modelSettings = { model: { current: { providerId: "zai", modelId: "fixture-before" },
  available: [{ ref: { providerId: "zai", modelId: "fixture-before" }, reasoning: { levels: [{ value: "unused" }], defaultLevel: "unused" } },
  { ref: { providerId: "zai", modelId: "glm-5.3-flash" },
  reasoning: { levels: [{ label: "Fixture", value: "fixture-default" }], defaultLevel: "fixture-default" } }] } };
function permission(tool) {
  emit({ id: "permission-" + tool, method: "interaction/requestPermission", params: {
    sessionId, requestId: "request-" + turn + tool, toolCallId: "tool-" + tool,
    toolName: tool, riskLevel: "high", input: {}, reason: "test execution" } });
}
function holdExecutor(onReady = () => {}) {
  const socket = require("net").connect(%OUTPUT_PORT%, "127.0.0.1", () => { socket.write("R"); onReady(); });
  socket.on("error", () => process.exit(0));
  socket.on("end", () => process.exit(0));
  setTimeout(() => process.exit(0), 30000).unref();
}
require("readline").createInterface({ input: process.stdin }).on("line", (line) => {
  const message = JSON.parse(line);
  if (String(message.id).startsWith("permission-")) {
    const tool = message.id.slice("permission-".length);
    const decision = message.result?.decision;
    record("permission:" + tool + ":" + decision);
    if (compacting) {
      emit({ method: "session/event", params: { sessionId, type: "turn.completed",
        payload: { response: "probe summary", resultType: "success" } } });
      return;
    }
    if (decision !== "allow") {
      emit({ method: "session/event", params: { sessionId, type: "turn.completed",
        payload: { response: "permission denied", resultType: "success" } } });
      return;
    }
    if (tool === "Bash") {
      fs.appendFileSync(%EFFECT%, "inner\n");
      if (holdInput) { holdExecutor(); return; }
      if (%LOST_OUTPUT%) {
        const socket = require("net").connect(%OUTPUT_PORT%, "127.0.0.1", () => {
          socket.write("R"); fs.closeSync(1);
        });
        socket.on("error", () => process.exit(0));
        socket.on("end", () => process.exit(0));
        setTimeout(() => process.exit(0), 30000).unref();
        return;
      }
      if (%DISCONNECT%) process.exit(0);
      if (%TEXT_SEGMENTS%) emit({ method: "session/event", params: { sessionId,
        type: "model.streaming", payload: { kind: "tool_input_start" } } });
      permission("Write");
    } else {
      fs.writeFileSync(%EFFECT% + ".written", "written by inner Write");
      for (const toolName of ["Bash", "Write"]) {
        emit({ method: "session/event", params: { sessionId, type: "tool_call_scheduled",
          payload: { toolCallId: "tool-" + toolName, toolName,
            input: { command: "echo outer >> " + %EFFECT%, file_path: %EFFECT%, content: "outer" } } } });
      }
      if (%TEXT_SEGMENTS% && fs.readFileSync(%EFFECT%, "utf8") === "inner\n") emit({ method: "session/event", params: { sessionId,
        type: "model.streaming", payload: { kind: "text_delta", delta: "Done checking." } } });
      emit({ method: "session/event", params: { sessionId, type: "turn.completed",
        payload: { response: %TEXT_SEGMENTS% ? "Done checking." : "actual client allowed Bash and Write", resultType: "success" } } });
    }
    return;
  }
  const reply = (result) => emit({ id: message.id, result });
  if (message.method === "session/goal") record("goal");
  switch (message.method) {
    case "session/create":
      record("create:" + message.params.mode);
      record("toolAllowlist:" + JSON.stringify(message.params.toolAllowlist ?? null));
      noTools = Array.isArray(message.params.toolAllowlist) && message.params.toolAllowlist.length === 0;
      reply({ session: { sessionId }, settings: modelSettings, protocol: { version: 1 } }); break;
    case "session/list":
      reply({ sessions: message.params.sessionIds.map(sessionId => ({ sessionId })) }); break;
    case "session/resume":
      record("resume"); sessionId = message.params.sessionId;
      record("toolAllowlist:" + JSON.stringify(message.params.toolAllowlist ?? null));
      noTools = Array.isArray(message.params.toolAllowlist) && message.params.toolAllowlist.length === 0;
      reply({ settings: modelSettings, protocol: { version: 1 } }); break;
    case "session/subscribe": reply({ eventSeq: 0, events: [] }); break;
    case "session/setModel":
      record("model:" + message.params.model.providerId + "/" + message.params.model.modelId + ":" + message.params.model.options?.reasoningLevel);
      if (message.params.model.options?.reasoningLevel !== "fixture-default") {
        emit({ id: message.id, error: { code: -32602, message: "reasoning level missing or invalid" } }); break;
      }
      reply({ session: { sessionId }, settings: { model: { current: %WRONG_MODEL%
        ? { providerId: "other-provider", modelId: "other-model" } : message.params.model } } }); break;
    case "session/setMode":
      record("mode:" + message.params.mode);
      if (%REJECT_MODE%) emit({ id: message.id, error: { code: -32000, message: "mode rejected" } });
      else reply({});
      break;
    case "session/send":
      holdInput = %INTERRUPT_SEND% && message.params.content.includes("hold-for-interrupt");
      record("send"); record("content:" + JSON.stringify(message.params.content)); turn++; reply({ accepted: true });
      if (%TEXT_SEGMENTS%) emit({ method: "session/event", params: { sessionId,
        type: "model.streaming", payload: { kind: "text_delta", delta: "Let me check." } } });
      compacting = message.params.content.includes("Summarize the recorded probe without executing tools");
      if (compacting) {
        record("compact");
        if (%COMPACT_DISCONNECT%) { fs.appendFileSync(%EFFECT%, "compact\n"); process.exit(0); }
        if (noTools) {
          emit({ method: "session/event", params: { sessionId, type: "turn.completed",
            payload: { response: "probe summary", resultType: "success" } } });
        } else permission("Bash");
      } else permission("Bash");
      break;
    case "v4/command":
      record("v4-send"); fs.appendFileSync(%EFFECT%, "inner\n");
      reply({ status: "unknown-after-execution" }); break;
    case "session/close": record("close:" + process.pid); reply({ closed: true }); break;
    case "session/goal":
      if (%OVERSIZED_GOAL%) {
        fs.appendFileSync(%EFFECT%, "inner\n");
        holdExecutor(() => {
          reply({ startedTurn: true });
          if (%OVERSIZED_GOAL_STREAM%) {
            for (const delta of ["ok", "x".repeat(7999)]) {
              emit({ method: "session/event", params: { sessionId, type: "model.streaming", payload: { kind: "text_delta", delta } } });
            }
          } else emit({ method: "session/event", params: { sessionId, type: "turn.completed", payload: { response: "x".repeat(8001), resultType: "success" } } });
        });
      } else if (%INTERRUPT_GOAL%) {
        fs.appendFileSync(%EFFECT%, "inner\n"); holdExecutor();
      } else reply({ startedTurn: false });
      break;
    case "session/stop": reply({}); break;
    default: emit({ id: message.id, error: { code: -32601, message: "unsupported " + message.method } });
  }
});
process.stdin.on("end", () => record("eof:" + process.pid));
"#
        .replace("%LOG%", &serde_json::to_string(&log).unwrap())
        .replace("%EFFECT%", &serde_json::to_string(&effect).unwrap())
        .replace("%OVERSIZED_GOAL%", if matches!(outcome, Outcome::OversizedGoalStream | Outcome::OversizedGoalFallback) { "true" } else { "false" })
        .replace("%OVERSIZED_GOAL_STREAM%", if matches!(outcome, Outcome::OversizedGoalStream) { "true" } else { "false" })
        .replace("%TEXT_SEGMENTS%", if matches!(outcome, Outcome::TextSegments) { "true" } else { "false" })
        .replace("%COMPACT_DISCONNECT%", if matches!(outcome, Outcome::CompactDisconnect) { "true" } else { "false" })
        .replace("%DISCONNECT%", if matches!(outcome, Outcome::Disconnect) { "true" } else { "false" })
        .replace("%REJECT_MODE%", if matches!(outcome, Outcome::RejectMode) { "true" } else { "false" });
    let source = source.replace(
        "%WRONG_MODEL%",
        if matches!(outcome, Outcome::WrongModel) {
            "true"
        } else {
            "false"
        },
    );
    let source = source
        .replace(
            "%INTERRUPT_SEND%",
            if matches!(outcome, Outcome::InterruptSend) {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%INTERRUPT_GOAL%",
            if matches!(outcome, Outcome::InterruptGoal) {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%LOST_OUTPUT%",
            if matches!(outcome, Outcome::LostOutput) {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "%OUTPUT_PORT%",
            &output_peer
                .as_ref()
                .map(|peer| peer.local_addr().unwrap().port())
                .unwrap_or_default()
                .to_string(),
        );
    std::fs::write(&cjs, source).unwrap();
    let fake_home = TempDir::new().unwrap();
    let other_home = TempDir::new().unwrap();
    let fixture = serde_json::json!({
        "model": "zai/fixture-before",
        "provider": {"zai": {"models": {"fixture-before": {"name": "preserved"}}}},
        "fixtureUnrelated": {"preserved": true}
    });
    for home in [fake_home.path(), other_home.path()] {
        let directory = home.join(".zcode/cli");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("config.json"), fixture.to_string()).unwrap();
    }
    let _guards = (
        EnvVarGuard::set("ZCODE_CJS", cjs.as_os_str()),
        EnvVarGuard::set(
            "ZCODE_WARM",
            OsStr::new(match lifecycle {
                Lifecycle::Cold => "0",
                Lifecycle::Warm => "1",
            }),
        ),
        EnvVarGuard::set(
            "ZCODE_WARM_MODE",
            OsStr::new(if matches!(outcome, Outcome::GatedMode) {
                "build"
            } else {
                "yolo"
            }),
        ),
        EnvVarGuard::set("ZCODE_WARM_GOAL", OsStr::new("1")),
        EnvVarGuard::set(
            "HOME",
            if cfg!(windows) {
                other_home.path()
            } else {
                fake_home.path()
            }
            .as_os_str(),
        ),
        EnvVarGuard::set("USERPROFILE", fake_home.path().as_os_str()),
    );
    let _v4_guard = if matches!(outcome, Outcome::AmbiguousV4) {
        EnvVarGuard::set("ZCODE_WARM_V4_SEND", OsStr::new("1"))
    } else {
        EnvVarGuard::unset("ZCODE_WARM_V4_SEND")
    };
    let _permission_guard = match outcome {
        Outcome::DenyClient => EnvVarGuard::set("ZCODE_WARM_PERMISSIONS", OsStr::new("deny")),
        Outcome::UnknownPolicy => {
            EnvVarGuard::set("ZCODE_WARM_PERMISSIONS", OsStr::new("unknown-policy"))
        }
        Outcome::Complete
        | Outcome::GoalContinuation
        | Outcome::Disconnect
        | Outcome::RejectMode
        | Outcome::WrongModel
        | Outcome::LostOutput
        | Outcome::OversizedGoalStream
        | Outcome::OversizedGoalFallback
        | Outcome::InterruptSend
        | Outcome::InterruptGoal
        | Outcome::GatedMode
        | Outcome::TextSegments
        | Outcome::Compact
        | Outcome::CompactThenTurn
        | Outcome::CompactDisconnect
        | Outcome::AmbiguousV4 => EnvVarGuard::unset("ZCODE_WARM_PERMISSIONS"),
    };
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.model_provider.wire_api = WireApi::Zcode;
        config.model_provider.name = "ZCode fixture".to_string();
        config.features.disable(Feature::TokenBudget);
        config.model = Some("glm-5.3-flash".to_string());
        config.compact_prompt =
            Some("Summarize the recorded probe without executing tools".to_string());
    });
    let mut test = builder
        .build_with_auto_env(&server)
        .await
        .expect("build test Codex");
    if matches!(
        outcome,
        Outcome::GoalContinuation
            | Outcome::InterruptGoal
            | Outcome::OversizedGoalStream
            | Outcome::OversizedGoalFallback
    ) {
        let started = test
            .thread_manager
            .start_thread(StartThreadOptions {
                dynamic_tools: vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                    name: "update_goal".to_string(),
                    description: "Report goal status.".to_string(),
                    input_schema: serde_json::json!({"type": "object", "properties": {}}),
                    defer_loading: false,
                })],
                ..StartThreadOptions::new(test.config.clone())
            })
            .await
            .expect("start thread with active goal tool");
        test.codex = started.thread;
    }
    let mut interrupted_socket =
        if matches!(outcome, Outcome::InterruptSend | Outcome::InterruptGoal) {
            use tokio::io::AsyncReadExt;
            start_probe_turn(
                &test,
                if matches!(outcome, Outcome::InterruptGoal) {
                    "<objective>hold-for-interrupt</objective>"
                } else {
                    "hold-for-interrupt"
                },
                approval,
                profile.clone(),
            )
            .await;
            let (mut socket, _) = tokio::time::timeout(
                std::time::Duration::from_secs(/*secs*/ 5),
                output_peer.as_ref().unwrap().accept(),
            )
            .await
            .unwrap()
            .unwrap();
            let mut ready = [0];
            tokio::time::timeout(
                std::time::Duration::from_secs(/*secs*/ 5),
                socket.read_exact(&mut ready),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(ready, [b'R']);
            test.codex.submit(Op::Interrupt).await.unwrap();
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnAborted(_))
            })
            .await;
            Some(socket)
        } else {
            None
        };
    for turn in 0..if matches!(
        outcome,
        Outcome::Complete | Outcome::GoalContinuation | Outcome::TextSegments
    ) {
        2
    } else {
        1
    } {
        let message = match (outcome, turn) {
            (Outcome::OversizedGoalStream | Outcome::OversizedGoalFallback, _) => {
                "<objective>Bound output</objective>"
            }
            (Outcome::GoalContinuation, 0) => "Prior user history: migrate records once.",
            (Outcome::GoalContinuation, _) => {
                "New steering: verify the existing receipt; do not migrate again.\n<objective>Finish migration safely</objective>"
            }
            _ => "execute the probe",
        };
        if matches!(
            outcome,
            Outcome::OversizedGoalStream | Outcome::OversizedGoalFallback
        ) {
            start_probe_turn(&test, message, approval, profile.clone()).await;
            let mut errors = Vec::new();
            wait_for_event(&test.codex, |event| {
                if let EventMsg::Error(error) = event {
                    errors.push(error.message.clone());
                }
                matches!(event, EventMsg::TurnComplete(_))
            })
            .await;
            assert_eq!(errors.len(), 1);
            assert!(errors[0].contains("history limit"));
        } else if matches!(outcome, Outcome::InterruptSend | Outcome::InterruptGoal)
            || (approval == AskForApproval::Never
                && profile == PermissionProfile::Disabled
                && matches!(
                    outcome,
                    Outcome::Complete
                        | Outcome::GoalContinuation
                        | Outcome::TextSegments
                        | Outcome::Compact
                        | Outcome::CompactThenTurn
                        | Outcome::CompactDisconnect
                ))
        {
            start_probe_turn(&test, message, approval, profile.clone()).await;
            let mut errors = Vec::new();
            let mut replies = Vec::new();
            wait_for_event(&test.codex, |event| {
                match event {
                    EventMsg::Error(error) => errors.push(error.message.clone()),
                    EventMsg::AgentMessage(message) => replies.push(message.message.clone()),
                    _ => {}
                }
                matches!(event, EventMsg::TurnComplete(_))
            })
            .await;
            assert_eq!(
                (errors, replies),
                (
                    Vec::<String>::new(),
                    if matches!(outcome, Outcome::TextSegments) {
                        vec!["Let me check.".to_string(), "Done checking.".to_string()]
                    } else {
                        vec!["actual client allowed Bash and Write".to_string()]
                    }
                )
            );
        } else {
            test.submit_turn_with_approval_and_permission_profile(
                message,
                approval,
                profile.clone(),
            )
            .await
            .expect("outer turn completes");
        }
        if matches!(
            outcome,
            Outcome::LostOutput | Outcome::OversizedGoalStream | Outcome::OversizedGoalFallback
        ) && let Some(peer) = &output_peer
        {
            use tokio::io::AsyncReadExt;
            let observed =
                tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), async {
                    let (mut socket, _) = peer.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    if let Err(error) = socket.read_to_end(&mut bytes).await {
                        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
                    }
                    bytes
                })
                .await
                .expect("failed turn must retire executor before thread shutdown");
            assert_eq!(observed, vec![b'R']);
        }
        if matches!(
            (lifecycle, outcome),
            (
                Lifecycle::Cold,
                Outcome::Complete | Outcome::GoalContinuation
            )
        ) && approval == AskForApproval::Never
            && profile == PermissionProfile::Disabled
        {
            let lines = std::fs::read_to_string(&log).unwrap();
            assert!(
                lines.lines().last().unwrap().starts_with("eof:"),
                "cold completion must await retirement: {lines}"
            );
        }
    }
    if let Some(socket) = interrupted_socket.as_mut() {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(/*secs*/ 5),
            socket.read_to_end(&mut bytes),
        )
        .await
        .unwrap();
        if let Err(error) = read {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        assert_eq!(bytes, Vec::<u8>::new());
        assert_eq!(
            std::fs::read_to_string(effect.with_extension("written")).unwrap(),
            "written by inner Write"
        );
    }
    if matches!(
        outcome,
        Outcome::Compact | Outcome::CompactThenTurn | Outcome::CompactDisconnect
    ) {
        test.codex
            .submit(Op::Compact)
            .await
            .expect("submit compaction");
        let mut errors = Vec::new();
        wait_for_event(&test.codex, |event| {
            if let EventMsg::Error(error) = event {
                errors.push(error.message.clone());
            }
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert_eq!(
            errors.len(),
            usize::from(matches!(outcome, Outcome::CompactDisconnect))
        );
    }
    if matches!(outcome, Outcome::CompactThenTurn) {
        start_probe_turn(
            &test,
            "Execute the next ordinary turn after compaction",
            approval,
            profile.clone(),
        )
        .await;
        let mut errors = Vec::new();
        let mut replies = Vec::new();
        wait_for_event(&test.codex, |event| {
            match event {
                EventMsg::Error(error) => errors.push(error.message.clone()),
                EventMsg::AgentMessage(message) => replies.push(message.message.clone()),
                _ => {}
            }
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert_eq!(
            (errors, replies),
            (
                Vec::<String>::new(),
                vec!["actual client allowed Bash and Write".to_string()]
            )
        );
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
    if matches!(outcome, Outcome::Complete | Outcome::GoalContinuation)
        && approval == AskForApproval::Never
        && profile == PermissionProfile::Disabled
    {
        assert_eq!(
            std::fs::read_to_string(effect.with_extension("written")).unwrap(),
            "written by inner Write"
        );
    }
    let mut expected = fixture.clone();
    if approval == AskForApproval::Never && profile == PermissionProfile::Disabled {
        expected["model"] = serde_json::json!("zai/glm-5.3-flash");
        expected["provider"]["zai"]["models"]["glm-5.3-flash"] =
            serde_json::json!({"name": "glm-5.3-flash"});
    }
    let read_fixture = |home: &std::path::Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(home.join(".zcode/cli/config.json")).unwrap())
            .unwrap()
    };
    assert_eq!(read_fixture(fake_home.path()), expected);
    assert_eq!(read_fixture(other_home.path()), fixture);
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
async fn zcode_client_binds_requested_model_before_every_cold_or_warm_input() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for lifecycle in [Lifecycle::Cold, Lifecycle::Warm] {
        let (log, effects) = probe(
            lifecycle,
            Outcome::Complete,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "inner\ninner\n");
        assert_eq!(
            log.iter()
                .filter(|line| line.starts_with("model:") || *line == "send")
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "model:zai/glm-5.3-flash:fixture-default",
                "send",
                "model:zai/glm-5.3-flash:fixture-default",
                "send",
            ],
        );
        let (log, effects) = probe(
            lifecycle,
            Outcome::WrongModel,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "");
        assert_eq!(
            log.iter()
                .filter(|line| line.starts_with("model:") || *line == "send")
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["model:zai/glm-5.3-flash:fixture-default"],
        );
    }
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
async fn zcode_client_retires_executor_that_closes_stdout_after_an_effect() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for lifecycle in [Lifecycle::Cold, Lifecycle::Warm] {
        let (log, effects) = probe(
            lifecycle,
            Outcome::LostOutput,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "inner\n");
        assert_eq!(log.iter().filter(|line| *line == "send").count(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_interrupt_then_turn_waits_for_old_executor_retirement() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for outcome in [Outcome::InterruptSend, Outcome::InterruptGoal] {
        let (log, effects) = probe(
            Lifecycle::Warm,
            outcome,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "inner\ninner\n");
        let spawns = log
            .iter()
            .filter(|line| line.starts_with("spawn:"))
            .collect::<Vec<_>>();
        assert_eq!(spawns.len(), 2);
        assert_ne!(spawns[0], spawns[1]);
        assert_eq!(
            log.iter()
                .filter(|line| *line == "permission:Write:allow")
                .count(),
            1
        );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_explicit_build_mode_retains_default_callback_denial() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Warm,
        Outcome::GatedMode,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_cold_preserves_assistant_messages_around_inner_tools() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Cold,
        Outcome::TextSegments,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await;
    assert_eq!(effects, "inner\ninner\n");
    let second_input: String = serde_json::from_str(
        log.iter()
            .filter_map(|line| line.strip_prefix("content:"))
            .nth(1)
            .expect("second cold prompt"),
    )
    .unwrap();
    let assistant_messages = second_input
        .lines()
        .filter(|line| line.starts_with("assistant: "))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(assistant_messages, @"
    assistant: Let me check.
    assistant: Done checking.
    ");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_cold_goal_continuation_sends_history_and_new_steering() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Cold,
        Outcome::GoalContinuation,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await;
    let contents = log
        .iter()
        .filter_map(|line| line.strip_prefix("content:"))
        .map(|content| serde_json::from_str::<String>(content).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        (
            contents.len(),
            log.iter().filter(|line| *line == "send").count(),
            log.iter().filter(|line| *line == "goal").count()
        ),
        (2, 2, 0)
    );
    for text in [
        "Prior user history: migrate records once.",
        "actual client allowed Bash and Write",
        "New steering: verify the existing receipt; do not migrate again.",
        "<objective>Finish migration safely</objective>",
        "[Goal status protocol]",
    ] {
        assert!(
            contents[1].contains(text),
            "missing {text:?} in {}",
            contents[1]
        );
    }
    assert_eq!(effects, "inner\ninner\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_local_compaction_preserves_the_captured_execution_contract() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for lifecycle in [Lifecycle::Cold, Lifecycle::Warm] {
        let (log, effects) = probe(
            lifecycle,
            Outcome::Compact,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "inner\n");
        assert_eq!(
            (
                log.iter().filter(|line| *line == "compact").count(),
                log.iter().filter(|line| *line == "send").count(),
            ),
            (1, 2),
        );
        assert_eq!(
            log.iter()
                .filter(|line| *line == "toolAllowlist:[]")
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_local_compaction_failure_after_effect_is_not_replayed() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for lifecycle in [Lifecycle::Cold, Lifecycle::Warm] {
        let (log, effects) = probe(
            lifecycle,
            Outcome::CompactDisconnect,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "inner\ncompact\n");
        assert_eq!(
            (
                log.iter().filter(|line| *line == "compact").count(),
                log.iter().filter(|line| *line == "send").count(),
            ),
            (1, 2),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_tools_recover_after_compaction_resume() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Warm,
        Outcome::CompactThenTurn,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await;
    assert_eq!(effects, "inner\ninner\n");
    assert_eq!(
        log.iter()
            .filter_map(|line| line.strip_prefix("toolAllowlist:"))
            .collect::<Vec<_>>(),
        vec!["null", "[]", "null"],
    );
    assert_eq!(
        (
            log.iter().filter(|line| *line == "resume").count(),
            log.iter()
                .filter(|line| *line == "permission:Write:allow")
                .count(),
        ),
        (2, 2),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_warm_ambiguous_v4_acknowledgement_does_not_repeat_the_effect() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    let (log, effects) = probe(
        Lifecycle::Warm,
        Outcome::AmbiguousV4,
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await;
    assert_eq!(effects, "inner\n");
    assert_eq!(
        (
            log.iter().filter(|line| *line == "v4-send").count(),
            log.iter().filter(|line| *line == "send").count(),
        ),
        (1, 0),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zcode_mirrored_goal_output_limit_retires_executor_without_replaying() {
    core_test_support::skip_if_remote!("ZCode inner execution is explicitly host-local");
    for outcome in [Outcome::OversizedGoalStream, Outcome::OversizedGoalFallback] {
        let (log, effects) = probe(
            Lifecycle::Warm,
            outcome,
            AskForApproval::Never,
            PermissionProfile::Disabled,
        )
        .await;
        assert_eq!(effects, "inner\n");
        assert_eq!(
            (
                log.iter().filter(|line| line.starts_with("spawn:")).count(),
                log.iter().filter(|line| *line == "goal").count(),
                log.iter()
                    .filter(|line| *line == "send" || *line == "v4-send")
                    .count(),
            ),
            (1, 1, 0)
        );
    }
}
