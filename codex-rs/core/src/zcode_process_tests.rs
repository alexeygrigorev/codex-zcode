use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Command;

use super::DEFAULT_ZCODE_STREAM_IDLE_TIMEOUT;
use super::ZcodeStreamLine;
use super::dispose_and_wait_once;
use super::next_stream_line;
use super::zcode_idle_timeout_from_value;

fn test_command() -> Command {
    // Own process group, matching what the bridge does at spawn time, so
    // teardown exercises real group semantics.
    let mut command = Command::new("sh");
    command.process_group(0);
    command
}

#[test]
fn idle_timeout_defaults_when_env_is_unset() {
    assert_eq!(
        zcode_idle_timeout_from_value(None),
        Some(DEFAULT_ZCODE_STREAM_IDLE_TIMEOUT)
    );
}

#[test]
fn idle_timeout_zero_disables_the_bound() {
    assert_eq!(zcode_idle_timeout_from_value(Some("0")), None);
}

#[test]
fn idle_timeout_parses_seconds_and_falls_back_on_junk() {
    assert_eq!(
        zcode_idle_timeout_from_value(Some("45")),
        Some(Duration::from_secs(45))
    );
    assert_eq!(
        zcode_idle_timeout_from_value(Some(" 120 ")),
        Some(Duration::from_secs(120))
    );
    assert_eq!(
        zcode_idle_timeout_from_value(Some("not-a-number")),
        Some(DEFAULT_ZCODE_STREAM_IDLE_TIMEOUT)
    );
}

#[tokio::test]
async fn dispose_reaps_a_child_that_exits_on_its_own() {
    // Ignore TERM so the graceful stage cannot kill the child, and prove the
    // trap is installed via a readiness line before signaling; the child
    // must then exit by itself (0.2s) well inside the grace period.
    let mut child = test_command()
        .arg("-c")
        .arg("trap '' TERM; echo ready; sleep 0.2")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn test child");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    lines
        .next_line()
        .await
        .expect("readiness line")
        .expect("line is present");
    drop(lines);

    let started = Instant::now();
    let status = dispose_and_wait_once(&mut child)
        .await
        .expect("child wait succeeds");
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn dispose_force_kills_a_child_that_ignores_termination() {
    let mut child = test_command()
        .arg("-c")
        // `echo ready` proves the trap is installed before the teardown can
        // signal, so the child ignoring TERM is deterministic rather than a
        // race against shell startup.
        .arg("trap '' TERM; echo ready; sleep 30")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn test child");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    lines
        .next_line()
        .await
        .expect("readiness line")
        .expect("line is present");
    drop(lines);

    let started = Instant::now();
    let status = dispose_and_wait_once(&mut child)
        .await
        .expect("child wait succeeds");
    // The child survives the graceful stage and the grace period, so the
    // teardown must escalate instead of waiting out the 30s sleep.
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!status.success());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dispose_kills_grandchildren_in_the_child_process_group() {
    let mut child = test_command()
        .arg("-c")
        .arg("trap '' TERM; sleep 30 & echo $!; wait")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn test child");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    let grandchild_pid: i32 = lines
        .next_line()
        .await
        .expect("grandchild pid line")
        .expect("line is present")
        .parse()
        .expect("grandchild pid parses");
    drop(lines);

    let status = dispose_and_wait_once(&mut child)
        .await
        .expect("child wait succeeds");
    assert!(!status.success());

    // The grandchild inherits the ignored TERM disposition, so only the
    // group SIGKILL reaches it. After SIGKILL it can linger briefly as a
    // zombie until its new parent reaps it — `kill(pid, 0)` still succeeds
    // on zombies, so consult /proc/<pid>/stat and treat a vanished or
    // zombie process as dead.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = std::fs::read_to_string(format!("/proc/{grandchild_pid}/stat"))
            .ok()
            .and_then(|stat| {
                let after_comm = stat.rsplit_once(')')?.1;
                after_comm.split_whitespace().next().map(str::to_string)
            });
        if state.is_none() || state.as_deref() == Some("Z") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "grandchild should have been killed with the process tree"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn next_stream_line_reads_lines_then_eof() {
    let mut child = test_command()
        .arg("-c")
        .arg("echo hello")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn test child");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    assert_eq!(
        next_stream_line(&mut lines, Some(Duration::from_secs(5))).await,
        ZcodeStreamLine::Line("hello".to_string())
    );
    assert_eq!(
        next_stream_line(&mut lines, Some(Duration::from_secs(5))).await,
        ZcodeStreamLine::Eof
    );
    child.wait().await.expect("child wait succeeds");
}

#[tokio::test]
async fn next_stream_line_reads_eof_without_a_timeout() {
    let mut child = test_command()
        .arg("-c")
        .arg("exit 0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn test child");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    assert_eq!(
        next_stream_line(&mut lines, None).await,
        ZcodeStreamLine::Eof
    );
    child.wait().await.expect("child wait succeeds");
}

#[tokio::test]
async fn next_stream_line_reports_a_stall_when_nothing_arrives() {
    let mut child = test_command()
        .arg("-c")
        .arg("sleep 30")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn test child");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    let started = Instant::now();
    assert_eq!(
        next_stream_line(&mut lines, Some(Duration::from_millis(100))).await,
        ZcodeStreamLine::Stalled(Duration::from_millis(100))
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // A bare kill would strand the `sleep` grandchild (the exact problem
    // this module exists to prevent); use the staged teardown.
    dispose_and_wait_once(&mut child)
        .await
        .expect("dispose test child");
}
