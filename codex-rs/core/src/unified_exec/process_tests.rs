use super::process::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_error: Option<String>,
    wake_tx: watch::Sender<u64>,
}

impl MockExecProcess {
    async fn read(&self) -> Result<ReadResponse, ExecServerError> {
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

pub(super) async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    sandbox_type: codex_sandboxing::SandboxType,
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error,
            wake_tx,
        }),
        sandbox_type: Some(sandbox_type),
    };

    UnifiedExecProcess::from_exec_server_started(started)
        .await
        .expect("remote process should start")
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(
        WriteStatus::UnknownProcess,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(
        WriteStatus::StdinClosed,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
}

#[tokio::test]
async fn remote_terminate_confirmed_updates_state_on_success_only() {
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());

    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    process
        .terminate_confirmed()
        .await
        .expect("terminate should succeed");

    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_process_preserves_executor_sandbox_type() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::LinuxSeccomp,
    )
    .await;

    assert_eq!(
        process.sandbox_type(),
        codex_sandboxing::SandboxType::LinuxSeccomp
    );
}

/// Wait until the canonical transcript has recorded `total` bytes.
async fn wait_for_transcript_bytes(process: &UnifiedExecProcess, total: usize) {
    let output = process.output_handles().clone();
    let transcript = process.transcript();
    loop {
        let recorded = output.output_notify.notified();
        tokio::pin!(recorded);
        recorded.as_mut().enable();
        if transcript.lock().await.total_bytes() >= total {
            return;
        }
        recorded.await;
    }
}

/// A stalled canonical consumer must hold the process readers back through
/// the bounded channels rather than skip chunks, and output must stay open
/// until both stdout and stderr have ended.
#[tokio::test]
async fn local_output_backpressures_a_stalled_consumer_without_losing_chunks() -> anyhow::Result<()>
{
    use super::head_tail_buffer::HeadTailBuffer;
    use super::process::NoopSpawnLifecycle;
    use codex_sandboxing::SandboxType;
    use std::sync::atomic::Ordering;

    const CHANNEL_CAPACITY: usize = 256;
    // More chunks than the channel and the former broadcast hop could hold.
    const STDOUT_CHUNKS: usize = 600;
    // Eight-byte records keep every chunk distinct.
    const RECORDS_PER_CHUNK: usize = 512;
    const CHUNK_BYTES: usize = RECORDS_PER_CHUNK * 8;

    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (_driver_stdout_tx, driver_stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(1);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let mut spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx: driver_stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    // Feed the process streams directly so the test is the bounded reader.
    let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let (stderr_tx, stderr_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    spawned.stdout_rx = stdout_rx;
    spawned.stderr_rx = stderr_rx;
    let process =
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await?;
    let output = process.output_handles().clone();
    let transcript = process.transcript();

    let chunks: Vec<Vec<u8>> = (0..STDOUT_CHUNKS)
        .map(|index| {
            format!("{index:07}\n")
                .repeat(RECORDS_PER_CHUNK)
                .into_bytes()
        })
        .collect();
    let stderr_chunk = b"stderr-after-stdout-eof\n".to_vec();

    // Stall the canonical consumer. Without yielding, the bounded channel
    // accepts exactly its capacity and then pushes back.
    let stalled = transcript.lock().await;
    let mut accepted = 0;
    while stdout_tx.try_send(chunks[accepted].clone()).is_ok() {
        accepted += 1;
    }
    assert_eq!(accepted, CHANNEL_CAPACITY);

    // The rest must wait for the consumer, however long it stays stalled.
    let remaining = chunks[accepted..].to_vec();
    let sender = tokio::spawn(async move {
        for chunk in remaining {
            stdout_tx.send(chunk).await?;
        }
        anyhow::Ok(())
    });
    for _ in 0..CHANNEL_CAPACITY {
        tokio::task::yield_now().await;
    }
    assert!(
        !sender.is_finished(),
        "stalled consumer must hold the sender back"
    );
    drop(stalled);
    sender.await??;

    // Stdout has ended, but stderr is still open: output must not close early.
    wait_for_transcript_bytes(&process, STDOUT_CHUNKS * CHUNK_BYTES).await;
    stderr_tx.send(stderr_chunk.clone()).await?;
    wait_for_transcript_bytes(&process, STDOUT_CHUNKS * CHUNK_BYTES + stderr_chunk.len()).await;
    assert!(!output.output_closed.load(Ordering::Acquire));

    drop(stderr_tx);
    loop {
        let closed = output.output_closed_notify.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if output.output_closed.load(Ordering::Acquire) {
            break;
        }
        closed.await;
    }

    let mut expected = HeadTailBuffer::default();
    for chunk in &chunks {
        expected.push_chunk(chunk);
    }
    expected.push_chunk(&stderr_chunk);
    assert_eq!(*transcript.lock().await, expected);

    exit_tx
        .send(0)
        .map_err(|_| anyhow::anyhow!("exit receiver dropped"))?;
    Ok(())
}
