use std::sync::Arc;

use super::Buffer;
use super::Emitter;
use super::TRAILING_OUTPUT_GRACE;
use super::resolve_aggregated_output;
use super::spawn_exit_watcher;
use super::start_streaming_output;
use super::utf8_boundary;
use crate::session::tests::make_session_and_context_with_rx;
use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::format_output_omission_marker;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::NoopSpawnLifecycle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use codex_sandboxing::SandboxType;

use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::Instant;

struct StreamingOutputHarness {
    process: Arc<UnifiedExecProcess>,
    stdout_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_tx: tokio::sync::oneshot::Sender<i32>,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    context: UnifiedExecContext,
    rx_event: async_channel::Receiver<Event>,
}

/// Spawn the process and its output producer without starting the live-delta
/// watcher, so a test can control when the watcher subscribes.
async fn unstarted_output_harness() -> anyhow::Result<StreamingOutputHarness> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = Arc::new(
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await?,
    );
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "streaming-output-test".to_string(),
    );
    let transcript = process.transcript();

    Ok(StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    })
}

async fn streaming_output_harness() -> anyhow::Result<StreamingOutputHarness> {
    let harness = unstarted_output_harness().await?;
    start_streaming_output(&harness.process, &harness.context);
    Ok(harness)
}

#[tokio::test]
async fn streaming_output_preserves_multibyte_characters_across_chunks() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    stdout_tx
        .send(vec![0xa9])
        .expect("send UTF-8 continuation byte");
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;

    let event = rx_event.recv().await.expect("receive output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: "é".as_bytes().to_vec(),
        }
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        "é".as_bytes()
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn streaming_output_finishes_on_close_without_waiting_for_grace() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    tokio::time::pause();
    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stdout_tx
            .send(b"LATE-OUTPUT-MARKER\xc3".to_vec())
            .expect("send late output");
    });

    (&mut drained).await;
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < TRAILING_OUTPUT_GRACE,
        "output close should finish before the grace fallback: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"LATE-OUTPUT-MARKER\xc3"
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_keeps_grace_as_fallback_without_close() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    tokio::time::pause();
    let exited_at = Instant::now();
    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= TRAILING_OUTPUT_GRACE
            && elapsed <= TRAILING_OUTPUT_GRACE + Duration::from_millis(10),
        "missing output close should use the grace fallback: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        vec![0xc3]
    );
    let event = rx_event.try_recv().expect("receive final output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: vec![0xc3],
        }
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn exit_watcher_waits_for_late_network_denial_before_classifying_end() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        mut context,
        rx_event,
    } = streaming_output_harness().await?;

    tokio::time::pause();
    let process_for_late_denial = Arc::clone(&process);
    let (late_denial_armed_tx, late_denial_armed_rx) = tokio::sync::oneshot::channel();
    let network_denial_monitor = tokio::spawn(async move {
        let sleep = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(sleep);
        late_denial_armed_tx.send(()).expect("arm late denial");
        sleep.await;
        process_for_late_denial.fail_and_terminate("LATE_DENIAL".to_string());
    });
    late_denial_armed_rx.await.expect("late denial armed");

    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    let step = Arc::get_mut(&mut context.step_context).expect("unshared test step");
    let model_info = Arc::make_mut(&mut Arc::make_mut(&mut step.settings).model_info);
    model_info.truncation_policy = codex_protocol::openai_models::TruncationPolicyConfig {
        mode: codex_protocol::openai_models::TruncationMode::Bytes,
        limit: 4,
    };
    spawn_exit_watcher(
        Arc::clone(&process),
        &context,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        Some(network_denial_monitor),
        /*plugin_metrics_sidecar*/ None,
    );

    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    drop(stdout_tx);

    let event = rx_event.recv().await.expect("command end event");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("expected ItemCompleted");
    };
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(
        (
            item.status,
            item.exit_code,
            item.aggregated_output.as_deref()
        ),
        (
            CommandExecutionStatus::Failed,
            Some(-1),
            Some("LATE_DENIAL")
        )
    );
    assert_eq!(
        item.formatted_output,
        Some(codex_utils_output_truncation::formatted_truncate_text(
            "LATE_DENIAL",
            codex_utils_output_truncation::TruncationPolicy::Bytes(4),
        ))
    );
    assert!(
        elapsed >= Duration::from_millis(10) && elapsed < TRAILING_OUTPUT_GRACE,
        "completion should wait for denial without falling back to the output grace: {elapsed:?}"
    );

    Ok(())
}

#[test]
fn utf8_boundary_preserves_complete_characters() {
    assert_eq!(utf8_boundary(b"hello"), 5);

    let bytes = "ééé".as_bytes();
    assert_eq!(utf8_boundary(&bytes[..3]), 2);

    let bytes = "😀".as_bytes();
    assert_eq!(utf8_boundary(bytes), bytes.len());
    for len in 1..bytes.len() {
        assert_eq!(utf8_boundary(&bytes[..len]), 0);
    }
    assert_eq!(utf8_boundary(&[0xf0, 0x9f, 0x98, 0x80, 0xc3]), 4);
}

#[test]
fn utf8_boundary_batches_malformed_output() {
    assert_eq!(utf8_boundary(&[0xff, b'a', b'b']), 3);
    assert_eq!(utf8_boundary(&[0xff, 0xc3]), 1);
    assert_eq!(utf8_boundary(&[0xff, 0xc3, 0xa9]), 3);
    assert_eq!(utf8_boundary(&[0xe0, 0x80]), 2);

    assert_eq!(utf8_boundary(b"a\xffbbb"), 5);
}

#[tokio::test]
async fn streaming_output_bounds_invalid_bytes_in_deltas() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let mut output = Buffer::<8> {
        pending: Vec::new(),
        emitter: Emitter {
            remaining_deltas: 2,
            session,
            turn,
            call_id: "bounded-output-test".to_string(),
        },
    };

    // The first frame splits 😀; the last allowed frame leaves é incomplete.
    let bytes = b"\xff\xff\xff\xff\xff\xff\xf0\x9f\x98\x80\xff\xff\xff\xc3\xa9";
    output.push(bytes.to_vec()).await;
    output.push(vec![0xfe, 0xfe]).await;
    output.finish().await;

    let mut chunks = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        chunks.push(delta.chunk);
    }
    assert_eq!(
        chunks,
        vec![
            b"\xff\xff\xff\xff\xff\xff".to_vec(),
            b"\xf0\x9f\x98\x80\xff\xff\xff".to_vec(),
        ]
    );
}

/// The live-delta watcher subscribes only after more than a broadcast
/// channel's worth of chunks and later falls behind again. Both lags must cost
/// only live deltas: the aggregated transcript keeps the exact head, tail and
/// omission count, with no duplicated bytes.
#[test]
fn transcript_is_exact_after_delayed_subscription_and_watcher_lag() -> anyhow::Result<()> {
    // Capacity of the process output broadcast channel.
    const BROADCAST_CAPACITY: usize = 64;
    // More chunks than the broadcast channel holds, published per phase.
    const CHUNKS_PER_PHASE: usize = 100;
    // Eight-byte records, so every chunk is distinct ASCII and fits one delta.
    const RECORDS_PER_CHUNK: usize = 1024;
    const CHUNK_BYTES: usize = RECORDS_PER_CHUNK * 8;

    // The producer side runs on worker threads that always make progress. The
    // watcher runs on a runtime that advances only while this test drives it,
    // so its lag is forced rather than raced.
    let producer_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let watcher_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let chunks: Vec<Vec<u8>> = (0..2 * CHUNKS_PER_PHASE)
        .map(|index| {
            format!("{index:07}\n")
                .repeat(RECORDS_PER_CHUNK)
                .into_bytes()
        })
        .collect();
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    } = producer_rt.block_on(unstarted_output_harness())?;
    let output = process.output_handles().clone();
    // Publish one chunk at a time and wait until the producer has recorded it,
    // so the producer itself never lags its input.
    let publish = |batch: &[Vec<u8>]| {
        producer_rt.block_on(async {
            for chunk in batch {
                let published = output.output_notify.notified();
                tokio::pin!(published);
                published.as_mut().enable();
                stdout_tx.send(chunk.clone())?;
                published.await;
            }
            Ok::<(), anyhow::Error>(())
        })
    };

    // Delayed subscription: the first phase is published before the watcher
    // exists, overflowing the receiver retained for it.
    publish(&chunks[..CHUNKS_PER_PHASE])?;
    {
        let _watcher_runtime = watcher_rt.enter();
        start_streaming_output(&process, &context);
    }
    let mut deltas = Vec::new();
    watcher_rt.block_on(async {
        while deltas.len() < BROADCAST_CAPACITY {
            if let EventMsg::ExecCommandOutputDelta(delta) = rx_event.recv().await?.msg {
                deltas.push(delta.chunk);
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // Subsequent lag: the caught-up watcher is not driven while the second
    // phase overflows the channel again.
    publish(&chunks[CHUNKS_PER_PHASE..])?;
    drop(stdout_tx);
    exit_tx
        .send(0)
        .map_err(|_| anyhow::anyhow!("exit receiver dropped"))?;
    producer_rt.block_on(async {
        output.cancellation_token.cancelled().await;
        loop {
            let closed = output.output_closed_notify.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            if output
                .output_closed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
            closed.await;
        }
    });
    let output_drained = process.output_drained_notify();
    watcher_rt.block_on(output_drained.notified());
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::ExecCommandOutputDelta(delta) = event.msg {
            deltas.push(delta.chunk);
        }
    }

    // Both lags really happened: the watcher saw only what each overflowed
    // channel still held.
    let delta_indices: Vec<Option<usize>> = deltas
        .iter()
        .map(|delta| chunks.iter().position(|chunk| chunk == delta))
        .collect();
    let expected_indices: Vec<Option<usize>> = (CHUNKS_PER_PHASE - BROADCAST_CAPACITY
        ..CHUNKS_PER_PHASE)
        .chain(2 * CHUNKS_PER_PHASE - BROADCAST_CAPACITY..2 * CHUNKS_PER_PHASE)
        .map(Some)
        .collect();
    assert_eq!(delta_indices, expected_indices);

    let mut expected_transcript = HeadTailBuffer::default();
    for chunk in &chunks {
        expected_transcript.push_chunk(chunk);
    }
    // One mebibyte keeps 64 head and 64 tail chunks; the middle 72 are omitted.
    let head_chunks = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 2 / CHUNK_BYTES;
    let tail_start = chunks.len() - head_chunks;
    let expected_output = format!(
        "{}\n{}\n{}",
        String::from_utf8(chunks[..head_chunks].concat())?,
        format_output_omission_marker((tail_start - head_chunks) * CHUNK_BYTES),
        String::from_utf8(chunks[tail_start..].concat())?,
    );
    producer_rt.block_on(async {
        assert_eq!(*transcript.lock().await, expected_transcript);
        // The complete transcript wins over a partial independently collected
        // fallback.
        assert_eq!(
            resolve_aggregated_output(&transcript, "partial fallback".to_string()).await,
            expected_output
        );
    });

    let _producer_runtime = producer_rt.enter();
    drop((process, context, rx_event, transcript));
    Ok(())
}
