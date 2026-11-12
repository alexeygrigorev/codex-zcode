//! A cancelled bridge must reject startup before any untrusted command executes.

use super::*;

#[tokio::test]
async fn cancelled_bridge_before_inner_start_prevents_command_execution() {
    let dir = tempfile::tempdir().expect("test directory");
    let marker = dir.path().join("command-ran");
    let profile = serde_json::to_string(&PermissionProfile::workspace_write())
        .expect("serialize permission profile");
    // Close the bridge endpoint before execing the inner helper. This models
    // cancellation before the helper can rearm PDEATHSIG, without a timing race.
    let script = r#"
import ctypes, json, os, socket, sys
# Match bwrap's capability-free inner stage, including on root test runners.
libc = ctypes.CDLL(None, use_errno=True)
header = (ctypes.c_uint32 * 2)(0x20080522, 0)
capabilities = (ctypes.c_uint32 * 6)()
if (libc.prctl(38, 1, 0, 0, 0) != 0
        or libc.capset(ctypes.byref(header), ctypes.byref(capabilities)) != 0):
    raise OSError(ctypes.get_errno(), 'drop inner-stage capabilities')
control, bridge = socket.socketpair()
bridge.close()
os.set_inheritable(control.fileno(), True)
helper, cwd, profile, marker = sys.argv[1:]
args = [helper, '--sandbox-policy-cwd', cwd, '--permission-profile', profile,
        '--apply-seccomp-then-exec', '--managed-network', '{}',
        '--proxy-route-spec', json.dumps({'routes': [
            {'env_key': 'HTTP_PROXY', 'control_fd': control.fileno()}
        ]}), '--', '/bin/sh', '-c', 'printf ran > "$1"', 'sh', marker]
os.execv(helper, args)
"#;
    let output = tokio::time::timeout(
        OPERATION_TIMEOUT,
        Command::new("python3")
            .args([
                "-c",
                script,
                env!("CARGO_BIN_EXE_codex-linux-sandbox"),
                dir.path().to_str().expect("test directory path"),
                &profile,
                marker.to_str().expect("marker path"),
            ])
            .env_clear()
            .envs(create_env_from_core_vars())
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("cancelled bootstrap must fail promptly")
    .expect("launch inner helper");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error activating Linux proxy routing bridge"),
        "expected rejection during bridge handoff: {stderr}"
    );
    assert!(
        !marker.exists(),
        "cancelled startup must not execute the command"
    );
}
