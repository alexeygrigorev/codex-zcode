#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;

/// Verify that when the server reports an error, `codex-exec` exits with a
/// non-zero status code so automation can detect failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exits_non_zero_when_server_reports_error() -> anyhow::Result<()> {
    let test = test_codex_exec();

    // Mock a simple Responses API SSE stream that immediately reports a
    // `response.failed` event with an error message.
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": "resp_err_1",
            "error": {"code": "rate_limit_exceeded", "message": "synthetic server error"}
        }
    })]);
    responses::mount_sse_once(&server, body).await;

    // Rate-limit stream errors are retried after a minute or more; disable
    // stream retries so the reported error ends the run.
    let mock_provider = format!(
        "model_providers.mock_provider={{name=\"Mock provider for test\",base_url=\"{}/v1\",wire_api=\"responses\",supports_websockets=false,stream_max_retries=0}}",
        server.uri()
    );
    test.cmd()
        .arg("-c")
        .arg(mock_provider)
        .args(["-c", "model_provider=\"mock_provider\""])
        .arg("--skip-git-repo-check")
        .arg("tell me something")
        .arg("--experimental-json")
        .assert()
        .code(1);

    Ok(())
}
