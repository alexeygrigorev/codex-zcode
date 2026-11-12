#![cfg(unix)]
//! `thread/fork` with `interruptTurnId`: the server interrupts the running source turn and
//! carries its goal into the fork as active only when that interrupt paused it.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_command_execution_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_features::Feature;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
const OBJECTIVE: &str = "keep going after the retry";

/// A source thread whose goal-driven turn is blocked in a long-running command.
struct RunningGoalTurn {
    mcp: TestAppServer,
    thread_id: String,
    turn_id: String,
    _server: MockServer,
    _tmp: TempDir,
}

impl RunningGoalTurn {
    async fn start() -> Result<Self> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path().join("codex_home");
        std::fs::create_dir(&codex_home)?;
        let working_directory = tmp.path().join("workdir");
        std::fs::create_dir(&working_directory)?;
        let server = create_mock_responses_server_sequence_unchecked(vec![
            create_command_execution_sse_response(
                vec!["sleep".to_string(), "10".to_string()],
                Some(&working_directory),
                Some(10_000),
                "call_sleep",
            )?,
        ])
        .await;
        MockResponsesConfig::new(&server.uri())
            .with_model("gpt-5.4")
            .with_sandbox_mode("workspace-write")
            .enable_feature(Feature::Goals)
            .with_root_config(r#"approvals_reviewer = "user""#)
            .write(&codex_home)?;
        let mut mcp = TestAppServer::builder()
            .with_codex_home(&codex_home)
            .without_managed_config()
            .build_initialized()
            .await?;
        let request = mcp
            .send_thread_start_request_with_auto_env(ThreadStartParams {
                cwd: Some(working_directory.to_string_lossy().to_string()),
                ..Default::default()
            })
            .await?;
        let ThreadStartResponse { thread, .. } = mcp.read_response(request).await?;

        // An active goal on an idle thread starts the turn that runs the command.
        let request = mcp
            .send_raw_request(
                "thread/goal/set",
                Some(json!({"threadId": thread.id, "objective": OBJECTIVE})),
            )
            .await?;
        let _: ThreadGoalSetResponse = mcp.read_response(request).await?;
        let started: TurnStartedNotification =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_notification("turn/started")).await??;
        Ok(Self {
            mcp,
            thread_id: thread.id,
            turn_id: started.turn.id,
            _server: server,
            _tmp: tmp,
        })
    }

    async fn goal(&mut self, thread_id: &str) -> Result<Option<ThreadGoal>> {
        let request = self
            .mcp
            .send_raw_request("thread/goal/get", Some(json!({"threadId": thread_id})))
            .await?;
        let response: ThreadGoalGetResponse = self.mcp.read_response(request).await?;
        Ok(response.goal)
    }

    async fn set_goal(&mut self, params: Value) -> Result<()> {
        let request = self
            .mcp
            .send_raw_request("thread/goal/set", Some(params))
            .await?;
        let _: ThreadGoalSetResponse = self.mcp.read_response(request).await?;
        Ok(())
    }

    fn fork_params(&self, interrupt_turn_id: &str) -> ThreadForkParams {
        ThreadForkParams {
            thread_id: self.thread_id.clone(),
            before_turn_id: Some(self.turn_id.clone()),
            interrupt_turn_id: Some(interrupt_turn_id.to_string()),
            defer_goal_continuation: true,
            exclude_turns: true,
            ..Default::default()
        }
    }

    async fn fork_interrupting(&mut self, interrupt_turn_id: &str) -> Result<String> {
        let params = self.fork_params(interrupt_turn_id);
        let request = self.mcp.send_thread_fork_request(params).await?;
        let response: ThreadForkResponse =
            timeout(DEFAULT_READ_TIMEOUT, self.mcp.read_response(request)).await??;
        // The response acknowledges exactly the interrupt this request performed.
        assert_eq!(
            Some(interrupt_turn_id.to_string()),
            response.interrupted_turn_id
        );
        Ok(response.thread.id)
    }

    async fn fork_interrupting_error(
        &mut self,
        interrupt_turn_id: &str,
    ) -> Result<JSONRPCErrorError> {
        let params = self.fork_params(interrupt_turn_id);
        let request = self.mcp.send_thread_fork_request(params).await?;
        let error = timeout(
            DEFAULT_READ_TIMEOUT,
            self.mcp
                .read_stream_until_error_message(RequestId::Integer(request)),
        )
        .await??;
        Ok(error.error)
    }
}

fn expected_goal(goal: &ThreadGoal, thread_id: &str, status: ThreadGoalStatus) -> ThreadGoal {
    ThreadGoal {
        thread_id: thread_id.to_string(),
        status,
        ..goal.clone()
    }
}

#[tokio::test]
async fn fork_interrupt_carries_the_goal_it_paused_as_active_and_keeps_the_source_paused()
-> Result<()> {
    skip_if_remote!(
        Ok(()),
        "uses a host-local command and cwd fixture unavailable to remote executors"
    );
    let mut source = RunningGoalTurn::start().await?;
    let before = source
        .goal(&source.thread_id.clone())
        .await?
        .expect("source goal");
    assert_eq!(ThreadGoalStatus::Active, before.status);

    let fork_id = source.fork_interrupting(&source.turn_id.clone()).await?;

    let source_goal = source
        .goal(&source.thread_id.clone())
        .await?
        .expect("source goal");
    let fork_goal = source.goal(&fork_id).await?.expect("fork goal");
    assert_eq!(ThreadGoalStatus::Paused, source_goal.status);
    assert_eq!(OBJECTIVE, source_goal.objective);
    assert_eq!(
        expected_goal(&source_goal, &fork_id, ThreadGoalStatus::Active),
        fork_goal
    );
    Ok(())
}

#[tokio::test]
async fn fork_interrupt_never_reactivates_a_goal_that_was_not_active() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "uses a host-local command and cwd fixture unavailable to remote executors"
    );
    for status in ["paused", "blocked", "complete"] {
        let mut source = RunningGoalTurn::start().await?;
        let thread_id = source.thread_id.clone();
        source
            .set_goal(json!({"threadId": thread_id, "status": status}))
            .await?;
        let before = source.goal(&thread_id).await?.expect("source goal");

        let fork_id = source.fork_interrupting(&source.turn_id.clone()).await?;

        let source_goal = source.goal(&thread_id).await?.expect("source goal");
        let fork_goal = source.goal(&fork_id).await?.expect("fork goal");
        assert_eq!(
            before.status, source_goal.status,
            "source status for {status}"
        );
        assert_eq!(
            expected_goal(&source_goal, &fork_id, before.status),
            fork_goal,
            "fork goal for {status}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn fork_interrupt_of_a_cleared_goal_forks_without_a_goal() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "uses a host-local command and cwd fixture unavailable to remote executors"
    );
    let mut source = RunningGoalTurn::start().await?;
    let thread_id = source.thread_id.clone();
    let request = source
        .mcp
        .send_raw_request("thread/goal/clear", Some(json!({"threadId": thread_id})))
        .await?;
    let _: Value = source.mcp.read_response(request).await?;

    let fork_id = source.fork_interrupting(&source.turn_id.clone()).await?;

    assert_eq!(None, source.goal(&thread_id).await?);
    assert_eq!(None, source.goal(&fork_id).await?);
    Ok(())
}

#[tokio::test]
async fn fork_interrupt_after_a_manual_interrupt_of_the_same_turn_is_rejected_and_stays_paused()
-> Result<()> {
    skip_if_remote!(
        Ok(()),
        "uses a host-local command and cwd fixture unavailable to remote executors"
    );
    let mut source = RunningGoalTurn::start().await?;
    let thread_id = source.thread_id.clone();
    let turn_id = source.turn_id.clone();
    let request = source
        .mcp
        .request(
            |request_id| codex_app_server_protocol::ClientRequest::TurnInterrupt {
                request_id,
                params: TurnInterruptParams {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                },
            },
        )
        .await;
    let _: TurnInterruptResponse = request?;
    let paused = source.goal(&thread_id).await?.expect("source goal");
    assert_eq!(ThreadGoalStatus::Paused, paused.status);

    let error = source.fork_interrupting_error(&turn_id).await?;

    assert_eq!(
        JSONRPCErrorError {
            code: INVALID_REQUEST_ERROR_CODE,
            message: format!("turn {turn_id} is not the currently running turn"),
            data: None,
        },
        error
    );
    assert_eq!(Some(paused), source.goal(&thread_id).await?);
    Ok(())
}

#[tokio::test]
async fn fork_interrupt_of_a_turn_that_is_not_running_does_not_interrupt_the_source() -> Result<()>
{
    skip_if_remote!(
        Ok(()),
        "uses a host-local command and cwd fixture unavailable to remote executors"
    );
    let mut source = RunningGoalTurn::start().await?;
    let thread_id = source.thread_id.clone();
    let before = source.goal(&thread_id).await?.expect("source goal");

    let error = source
        .fork_interrupting_error("not-the-running-turn")
        .await?;

    assert_eq!(
        JSONRPCErrorError {
            code: INVALID_REQUEST_ERROR_CODE,
            message: "turn not-the-running-turn is not the currently running turn".to_string(),
            data: None,
        },
        error
    );
    assert_eq!(Some(before), source.goal(&thread_id).await?);
    Ok(())
}

#[tokio::test]
async fn fork_interrupt_requires_the_canonical_thread_id_and_no_path() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "uses a host-local command and cwd fixture unavailable to remote executors"
    );
    let mut source = RunningGoalTurn::start().await?;
    let thread_id = source.thread_id.clone();
    let before = source.goal(&thread_id).await?.expect("source goal");
    let expected = JSONRPCErrorError {
        code: INVALID_REQUEST_ERROR_CODE,
        message:
            "`interruptTurnId` requires the canonical `threadId` and cannot be combined with `path`"
                .to_string(),
        data: None,
    };

    for params in [
        ThreadForkParams {
            path: Some(std::path::PathBuf::from("rollout.jsonl")),
            ..source.fork_params(&source.turn_id.clone())
        },
        ThreadForkParams {
            thread_id: thread_id.to_uppercase(),
            ..source.fork_params(&source.turn_id.clone())
        },
    ] {
        let request = source.mcp.send_thread_fork_request(params).await?;
        let error = timeout(
            DEFAULT_READ_TIMEOUT,
            source
                .mcp
                .read_stream_until_error_message(RequestId::Integer(request)),
        )
        .await??;
        assert_eq!(expected, error.error);
    }

    // Nothing was claimed or interrupted: the turn is still running and the goal untouched.
    assert_eq!(Some(before), source.goal(&thread_id).await?);
    Ok(())
}
