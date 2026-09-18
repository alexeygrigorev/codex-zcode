//! Exercises the goal extension's no-progress breaker through the public API.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn continuations_without_successful_tools_block_the_goal_after_three() -> Result<()> {
    // Every continuation produces a non-empty final answer, so the
    // empty-response breaker never applies. Without a single successful tool
    // call, the no-progress breaker must block the goal after three turns.
    let scripts = (1..=3)
        .map(|turn| {
            responses::sse(vec![
                responses::ev_response_created(&format!("response-{turn}")),
                responses::ev_assistant_message(&format!("final-{turn}"), "Still working on it"),
                responses::ev_completed(&format!("response-{turn}")),
            ])
        })
        .collect();
    let server = create_mock_responses_server_sequence(scripts).await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.4")
        .enable_feature(Feature::Goals)
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let request = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = mcp.read_response(request).await?;
    let request = mcp
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId": thread.id, "objective": "Finish the work"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = mcp.read_response(request).await?;

    for _ in 1..=3 {
        let completed: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            mcp.read_notification("turn/completed"),
        )
        .await??;
        assert_eq!(TurnStatus::Completed, completed.turn.status);
        assert_eq!(None, completed.turn.error);
    }
    let request = mcp
        .send_raw_request("thread/goal/get", Some(json!({"threadId": thread.id})))
        .await?;
    let result: ThreadGoalGetResponse = mcp.read_response(request).await?;
    assert_eq!(
        ThreadGoalStatus::Blocked,
        result.goal.expect("goal exists").status
    );
    server.verify().await;
    Ok(())
}
