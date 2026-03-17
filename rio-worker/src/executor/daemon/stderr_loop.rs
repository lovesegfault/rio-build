//! Daemon I/O after spawn: handshake, wopBuildDerivation, STDERR loop, log batching.
// r[impl worker.daemon.stderr-result-logs]

use std::time::Duration;

use tokio::sync::mpsc;

use rio_nix::protocol::build::{BuildMode, BuildResult, BuildStatus};
use rio_nix::protocol::client::{
    StderrMessage, client_handshake, client_set_options, read_stderr_message,
};
use rio_nix::protocol::stderr::ResultField;
use rio_nix::protocol::wire;
use rio_proto::types::{WorkerMessage, worker_message};

use tracing::instrument;

use crate::executor::ExecutorError;
use crate::log_stream::{AddLineResult, BATCH_TIMEOUT, LogBatcher};

use super::DAEMON_SETUP_TIMEOUT;

/// All daemon I/O after spawn: handshake, setOptions, wopBuildDerivation, stderr loop.
///
/// Caller MUST kill the daemon after this returns (whether Ok or Err).
/// This is the only function that should touch daemon stdin/stdout —
/// keeping it isolated ensures the caller's always-kill path is reliable.
///
/// Span duration ≈ actual sandbox build time — this is the hot zone in
/// a trace (e.g., 95% of a slow build's wall time). `build_timeout` in
/// span fields so Tempo can correlate slow builds with timeout config.
// 8 args: daemon handle + drv identity + three timeout/limit knobs +
// log plumbing. Bundling into a struct would obscure the per-knob
// provenance (timeout from config, max_silent_time from assignment).
#[allow(clippy::too_many_arguments)]
#[instrument(
    skip_all,
    fields(
        build_timeout_secs = build_timeout.as_secs(),
        max_silent_secs = max_silent_time,
        build_cores
    )
)]
pub(in crate::executor) async fn run_daemon_build(
    daemon: &mut tokio::process::Child,
    drv_path: &str,
    basic_drv: &rio_nix::derivation::BasicDerivation,
    build_timeout: Duration,
    max_silent_time: u64,
    build_cores: u64,
    batcher: LogBatcher,
    log_tx: &mpsc::Sender<WorkerMessage>,
) -> Result<BuildResult, ExecutorError> {
    let mut stdin = daemon
        .stdin
        .take()
        .ok_or_else(|| ExecutorError::DaemonSetup("failed to get daemon stdin".into()))?;
    let stdout = daemon
        .stdout
        .take()
        .ok_or_else(|| ExecutorError::DaemonSetup("failed to get daemon stdout".into()))?;

    // Handshake + setOptions + send build — all bounded by DAEMON_SETUP_TIMEOUT.
    // All three steps share DAEMON_SETUP_TIMEOUT: a stuck setOptions or stalled
    // write must not hang until build_timeout (potentially hours).
    // We need &mut access to stdout for the setup sequence (handshake reads
    // the daemon's version etc.), but read_build_stderr_loop takes stdout by
    // value for its owned reader task. So: put stdout in an Option, borrow
    // mutably for setup, then take() it for the loop.
    let mut stdout = Some(stdout);
    tokio::time::timeout(DAEMON_SETUP_TIMEOUT, async {
        let stdout_ref = stdout.as_mut().expect("taken only once, after this block");
        let handshake_result = client_handshake(stdout_ref, &mut stdin).await?;

        tracing::debug!(
            version = handshake_result.negotiated_version(),
            "daemon handshake complete"
        );

        client_set_options(stdout_ref, &mut stdin, max_silent_time, build_cores).await?;

        wire::write_u64(
            &mut stdin,
            rio_nix::protocol::opcodes::WorkerOp::BuildDerivation as u64,
        )
        .await?;
        wire::write_string(&mut stdin, drv_path).await?;
        rio_nix::protocol::build::write_basic_derivation(&mut stdin, basic_drv).await?;
        wire::write_u64(&mut stdin, BuildMode::Normal as u64).await?;
        tokio::io::AsyncWriteExt::flush(&mut stdin)
            .await
            .map_err(wire::WireError::from)?;

        Ok::<_, ExecutorError>(())
    })
    .await
    .map_err(|_| ExecutorError::DaemonSetup("daemon setup sequence timed out".into()))??;

    // Read STDERR loop with log streaming (build may run for a long time).
    // stdout is moved into the loop's owned reader task; we don't need it
    // back because the loop itself reads BuildResult after STDERR_LAST.
    let stdout = stdout.take().expect("borrowed above, not yet taken");

    // Timeout is a BUILD OUTCOME, not an executor error. Returning
    // Ok(failure) flows through execute_build's status-mapping path
    // (nix_failure_to_proto → BuildResultStatus::TimedOut), which the
    // scheduler treats as permanent-no-reassign. Returning Err would
    // land in runtime.rs's InfrastructureFailure arm → reassignment
    // storm (same build, same inputs, same timeout, forever).
    //
    // The inner Result<BuildResult, WireError> is different: a wire
    // error mid-STDERR-loop IS an executor fault (daemon died, pipe
    // corrupted) — that `?` stays.
    //
    // r[impl worker.timeout.no-reassign]
    let build_result = match tokio::time::timeout(
        build_timeout,
        read_build_stderr_loop(stdout, batcher, log_tx),
    )
    .await
    {
        Ok(inner) => inner?,
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = build_timeout.as_secs(),
                "build exceeded timeout; reporting TimedOut (no reassignment)"
            );
            BuildResult::failure(
                BuildStatus::TimedOut,
                format!(
                    "build exceeded configured timeout of {}s",
                    build_timeout.as_secs()
                ),
            )
        }
    };

    Ok(build_result)
}

/// Read the STDERR loop from the daemon, streaming logs via the batcher.
///
/// The reader is spawned into an owned task that pushes each parsed
/// `StderrMessage` onto an mpsc channel. The main loop `select!`s on that
/// channel and a `BATCH_TIMEOUT` interval, so a partial batch is flushed
/// during silent build periods. Without this, a partial batch would wait
/// for the next STDERR message — for a quiet 60s compile the gateway sees
/// nothing and the build appears hung.
///
/// This approach is cancel-safe: wrapping `read_stderr_message()` directly
/// in `tokio::time::timeout` would drop the read future mid-u64-read,
/// leaving partial bytes consumed from the daemon's stdout pipe and
/// desyncing the Nix STDERR protocol. Spawning the reader into an owned
/// task means the read future is never cancelled; only the `recv()` side
/// of the channel is — and `mpsc::Receiver::recv()` is cancel-safe.
///
/// If the log channel closes during the build, returns `MiscFailure` —
/// the scheduler stream is gone, so there's no way to report completion
/// anyway.
async fn read_build_stderr_loop<R>(
    reader: R,
    mut batcher: LogBatcher,
    log_tx: &mpsc::Sender<WorkerMessage>,
) -> Result<BuildResult, wire::WireError>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    const MAX_BUILD_STDERR_MESSAGES: u64 = 10_000_000;

    /// Helper: send a log batch. Returns false if the channel is closed.
    async fn send_batch(
        log_tx: &mpsc::Sender<WorkerMessage>,
        batch: rio_proto::types::BuildLogBatch,
    ) -> bool {
        let msg = WorkerMessage {
            msg: Some(worker_message::Msg::LogBatch(batch)),
        };
        log_tx.send(msg).await.is_ok()
    }

    let misc_fail = |m: &str| BuildResult::failure(BuildStatus::MiscFailure, m.to_string());

    /// Outcome of handling one log line. `Continue` = keep reading.
    /// `Break(r)` = exit the stderr loop with this result.
    enum LineOutcome {
        Continue,
        Break(Result<Option<BuildResult>, wire::WireError>),
    }

    /// Shared log-line handling for STDERR_NEXT and STDERR_RESULT
    /// BuildLogLine — both need msg_count bound check + batcher.add_line
    /// + AddLineResult dispatch.
    async fn handle_log_line(
        line: Vec<u8>,
        msg_count: &mut u64,
        batcher: &mut LogBatcher,
        log_tx: &mpsc::Sender<WorkerMessage>,
    ) -> LineOutcome {
        *msg_count += 1;
        if *msg_count >= MAX_BUILD_STDERR_MESSAGES {
            return LineOutcome::Break(Err(wire::WireError::Io(std::io::Error::other(
                "exceeded maximum STDERR messages during build",
            ))));
        }
        match batcher.add_line(line) {
            AddLineResult::Buffered => LineOutcome::Continue,
            AddLineResult::BatchReady(batch) => {
                if send_batch(log_tx, batch).await {
                    LineOutcome::Continue
                } else {
                    LineOutcome::Break(Ok(Some(BuildResult::failure(
                        BuildStatus::MiscFailure,
                        "log channel closed during build (scheduler stream gone)".to_string(),
                    ))))
                }
            }
            AddLineResult::LimitExceeded { reason } => {
                // Flush what's buffered so client sees output up to
                // the limit. Best-effort: channel-closed is moot,
                // we're breaking with LogLimitExceeded anyway.
                if batcher.has_pending() {
                    let _ = send_batch(log_tx, batcher.flush()).await;
                }
                tracing::warn!(reason = %reason, "build log limit exceeded, aborting");
                LineOutcome::Break(Ok(Some(BuildResult::failure(
                    BuildStatus::LogLimitExceeded,
                    reason,
                ))))
            }
        }
    }

    // Spawn the owned reader task. It reads one StderrMessage at a time and
    // pushes to `msg_tx`. Terminal messages (Last, Error, wire Err) break the
    // loop after being pushed, so the task returns the reader for the caller's
    // post-loop read_build_result(). Backpressure: channel has a small buffer;
    // if the main loop falls behind (shouldn't — it does little work per msg),
    // the reader task naturally blocks on send().
    let (msg_tx, mut msg_rx) = mpsc::channel::<Result<StderrMessage, wire::WireError>>(32);
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            let msg = read_stderr_message(&mut reader).await;
            let is_terminal = matches!(
                &msg,
                Ok(StderrMessage::Last) | Ok(StderrMessage::Error(_)) | Err(_)
            );
            // If the main loop has exited (receiver dropped), stop reading.
            // Otherwise push and continue unless this was a terminal message.
            if msg_tx.send(msg).await.is_err() || is_terminal {
                break;
            }
        }
        reader
    });

    // Abort guard: if the main loop early-returns (misc_fail paths, channel
    // closed, msg-count bound), the reader task would otherwise leak, blocked
    // forever on read() of the daemon's stdout. scopeguard::guard runs on all
    // exit paths including panic. `.abort()` on a completed task is a no-op.
    let reader_abort = reader_task.abort_handle();
    let _abort_guard = scopeguard::guard((), move |()| reader_abort.abort());

    let mut flush_tick = tokio::time::interval(BATCH_TIMEOUT);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; consume it so the first real flush waits
    // the full interval.
    flush_tick.tick().await;

    let mut msg_count: u64 = 0;

    // Outcome of the select! loop. Ok(None) = saw STDERR_LAST, proceed to
    // read BuildResult. Ok(Some(fail)) = return that failure without reading
    // BuildResult. Err(e) = wire error, propagate.
    let outcome: Result<Option<BuildResult>, wire::WireError> = loop {
        tokio::select! {
            // `biased` prioritizes the message arm over the tick arm when
            // both are ready. Under heavy log spew we want to drain messages
            // before doing a tick-driven flush (the 64-line batch-full
            // trigger in add_line already handles chatty builds).
            biased;

            maybe = msg_rx.recv() => {
                match maybe {
                    Some(Ok(StderrMessage::Last)) => break Ok(None),
                    Some(Ok(StderrMessage::Error(e))) => break Ok(Some(misc_fail(&e.message))),
                    Some(Ok(StderrMessage::Next(line))) => {
                        match handle_log_line(
                            line.into_bytes(),
                            &mut msg_count,
                            &mut batcher,
                            log_tx,
                        )
                        .await
                        {
                            LineOutcome::Continue => {}
                            LineOutcome::Break(r) => break r,
                        }
                    }
                    Some(Ok(StderrMessage::Read(_))) => {
                        break Ok(Some(misc_fail("daemon sent STDERR_READ, not supported")));
                    }
                    // STDERR_RESULT with result_type 101 (BuildLogLine) or
                    // 107 (PostBuildLogLine): this is how modern nix-daemon
                    // sends builder stderr output. It DOES NOT come as raw
                    // STDERR_NEXT — that's only for daemon chatter.
                    //
                    // This was a latent phase2a bug: we were silently dropping
                    // all build output. It never mattered because phase2a
                    // didn't assert on log content. vm-phase2b's log-pipeline
                    // assertion caught it — exactly what milestone VM tests
                    // are for.
                    //
                    // fields[0] is the log line (String). Same batching +
                    // limit logic as STDERR_NEXT.
                    Some(Ok(StderrMessage::Result { result_type, fields, .. }))
                        if (result_type == 101 || result_type == 107)
                            && matches!(fields.first(), Some(ResultField::String(_))) =>
                    {
                        // Safe: matches! above verified it's Some(String(_)).
                        let ResultField::String(line) = &fields[0] else {
                            unreachable!("match guard above proved String")
                        };
                        match handle_log_line(
                            line.as_bytes().to_vec(),
                            &mut msg_count,
                            &mut batcher,
                            log_tx,
                        )
                        .await
                        {
                            LineOutcome::Continue => {}
                            LineOutcome::Break(r) => break r,
                        }
                    }
                    // Other Result types (Progress, SetExpected, etc.) and
                    // activity lifecycle messages — discard.
                    Some(Ok(
                        StderrMessage::Write(_)
                        | StderrMessage::StartActivity { .. }
                        | StderrMessage::StopActivity { .. }
                        | StderrMessage::Result { .. },
                    )) => {}
                    Some(Err(e)) => break Err(e),
                    None => {
                        // Reader task exited without a terminal message (it
                        // dropped msg_tx). This means the main loop dropped
                        // msg_rx first (can't happen here) or the task
                        // panicked. Treat as a protocol error.
                        break Err(wire::WireError::Io(std::io::Error::other(
                            "stderr reader task exited without terminal message",
                        )));
                    }
                }
            }

            _ = flush_tick.tick() => {
                // The interval itself IS the 100ms gate: if a tick fired and
                // there's anything pending, flush unconditionally. (We don't
                // use batcher.maybe_flush() here — that checks against
                // std::time::Instant, which doesn't advance under tokio's
                // paused-time test mode. The tick already proved 100ms of
                // tokio-time elapsed.)
                if batcher.has_pending() && !send_batch(log_tx, batcher.flush()).await {
                    break Ok(Some(misc_fail(
                        "log channel closed during build (scheduler stream gone)",
                    )));
                }
            }
        }
    };

    // Final flush: the loop owns the batcher (by-value), so
    // any partial batch must be drained here. Best-effort — the build
    // result is already determined; if the log channel is closed, just drop.
    if batcher.has_pending() {
        let _ = send_batch(log_tx, batcher.flush()).await;
    }

    // Terminal-message paths (Last/Error) fell through here: recover the reader
    // so we can read the BuildResult that follows STDERR_LAST. For the other
    // outcomes (misc_fail, wire Err), the abort guard fires on return and
    // cleans up the reader task; we don't need the reader back.
    match outcome? {
        Some(fail) => Ok(fail),
        None => {
            // Reader task has already returned (it pushed STDERR_LAST and
            // broke its loop). `.await` here does not block; it just
            // collects the return value.
            let mut reader = reader_task.await.map_err(|e| {
                wire::WireError::Io(std::io::Error::other(format!(
                    "stderr reader task join failed: {e}"
                )))
            })?;
            rio_nix::protocol::build::read_build_result(&mut reader).await
        }
    }
}

// r[verify worker.daemon.stderr-result-logs]
// r[verify worker.daemon.stdio-client]
#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // read_build_stderr_loop — pure async, no daemon process needed
    // -----------------------------------------------------------------------

    use tokio::io::AsyncWriteExt;

    use rio_nix::protocol::build::write_build_result;
    use rio_nix::protocol::stderr::{STDERR_READ, STDERR_WRITE, StderrError, StderrWriter};

    /// Serialize a BuildResult to wire bytes.
    async fn build_result_bytes(r: &BuildResult) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::new();
        write_build_result(&mut buf, r).await?;
        Ok(buf)
    }

    /// Run read_build_stderr_loop against a Cursor of `input` bytes with a
    /// fresh batcher. Returns (result, all batches received on log_rx).
    async fn run_loop(
        input: Vec<u8>,
    ) -> (Result<BuildResult, wire::WireError>, Vec<WorkerMessage>) {
        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (tx, mut rx) = mpsc::channel(128);
        let cursor = std::io::Cursor::new(input);
        let result = read_build_stderr_loop(cursor, batcher, &tx).await;
        drop(tx);
        let mut batches = Vec::new();
        while let Some(m) = rx.recv().await {
            batches.push(m);
        }
        (result, batches)
    }

    /// Count total log lines across all received BuildLogBatch messages.
    fn count_log_lines(batches: &[WorkerMessage]) -> usize {
        batches
            .iter()
            .filter_map(|m| match &m.msg {
                Some(worker_message::Msg::LogBatch(b)) => Some(b.lines.len()),
                _ => None,
            })
            .sum()
    }

    /// Happy path: STDERR_NEXT ×2, STDERR_LAST, BuildResult{Built}.
    #[tokio::test]
    async fn test_stderr_loop_next_then_success() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("line1").await?;
            w.log("line2").await?;
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, batches) = run_loop(buf).await;
        let br = result.expect("loop should succeed");
        assert_eq!(br.status, BuildStatus::Built);
        // The loop owns the batcher and does a final flush after
        // STDERR_LAST, so both lines are deterministically in `batches`.
        assert_eq!(count_log_lines(&batches), 2);
        Ok(())
    }

    /// Daemon sends STDERR_ERROR → loop returns Ok(MiscFailure), NOT Err.
    /// This is how build-time errors (compile failures etc.) propagate.
    #[tokio::test]
    async fn test_stderr_loop_daemon_error_returns_misc_failure() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("compiling").await?;
            w.error(&StderrError::simple("Build", "compile error: missing ;"))
                .await?;
            // NO finish() — STDERR_ERROR terminates the loop.
        }

        let (result, _batches) = run_loop(buf).await;
        let br = result.expect("daemon error is Ok(failure), not Err");
        assert_eq!(br.status, BuildStatus::MiscFailure);
        assert_eq!(br.error_msg, "compile error: missing ;");
        Ok(())
    }

    /// STDERR_READ is not supported in build-stderr context → MiscFailure.
    #[tokio::test]
    async fn test_stderr_loop_read_not_supported() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_READ).await?;
        wire::write_u64(&mut buf, 42).await?;

        let (result, _) = run_loop(buf).await;
        let br = result.expect("STDERR_READ is Ok(failure), not Err");
        assert_eq!(br.status, BuildStatus::MiscFailure);
        assert!(
            br.error_msg.contains("STDERR_READ") && br.error_msg.contains("not supported"),
            "error_msg: {}",
            br.error_msg
        );
        Ok(())
    }

    /// Log channel closed mid-build → MiscFailure. The scheduler stream is
    /// gone so there's no way to report completion anyway.
    #[tokio::test]
    async fn test_stderr_loop_log_channel_closed_returns_failure() -> anyhow::Result<()> {
        // Feed enough STDERR_NEXT to force a batch flush (MAX_BATCH_LINES=64).
        // The 65th add_line returns Some(batch); send_batch fails because rx
        // is dropped.
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            for i in 0..65 {
                w.log(&format!("line {i}")).await?;
            }
        }

        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (tx, rx) = mpsc::channel(1);
        drop(rx); // channel closed before loop starts
        let cursor = std::io::Cursor::new(buf);

        let result = read_build_stderr_loop(cursor, batcher, &tx).await;
        let br = result.expect("channel-closed is Ok(failure)");
        assert_eq!(br.status, BuildStatus::MiscFailure);
        assert!(
            br.error_msg.contains("log channel closed"),
            "got: {}",
            br.error_msg
        );
        Ok(())
    }

    /// Activity/Write/Result messages are silently discarded.
    #[tokio::test]
    async fn test_stderr_loop_discards_activity_and_write() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            let aid = w
                .start_activity(
                    rio_nix::protocol::stderr::ActivityType::Build,
                    "building",
                    0,
                    0,
                )
                .await?;
            w.stop_activity(aid).await?;
        }
        // STDERR_WRITE (raw, StderrWriter doesn't have a generic write)
        wire::write_u64(&mut buf, STDERR_WRITE).await?;
        wire::write_bytes(&mut buf, b"some data").await?;
        // STDERR_RESULT: activity_id + result_type + 0 fields
        wire::write_u64(&mut buf, rio_nix::protocol::stderr::STDERR_RESULT).await?;
        wire::write_u64(&mut buf, 1).await?; // activity_id
        wire::write_u64(&mut buf, 0).await?; // result_type
        wire::write_u64(&mut buf, 0).await?; // field count
        // STDERR_LAST + success
        {
            let mut w = StderrWriter::new(&mut buf);
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, batches) = run_loop(buf).await;
        let br = result.expect("should succeed");
        assert_eq!(br.status, BuildStatus::Built);
        // None of these messages produce log lines.
        assert_eq!(count_log_lines(&batches), 0);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Cancel-safety + silent-period flush
    // -----------------------------------------------------------------------

    /// A build that goes silent for >100ms MUST flush its partial batch via
    /// the interval tick, not wait for the next STDERR message. This is the
    /// observability spec's "64 lines / 100ms" guarantee.
    ///
    /// Uses `start_paused = true` + `tokio::io::duplex` so the flush tick
    /// fires deterministically under paused tokio-time. The tick arm uses
    /// `has_pending() + flush()` (not `maybe_flush()`) specifically because
    /// `maybe_flush()` checks `std::time::Instant`, which does NOT advance
    /// under paused tokio-time.
    #[tokio::test(start_paused = true)]
    async fn test_silent_period_triggers_flush() -> anyhow::Result<()> {
        let (mut write_half, read_half) = tokio::io::duplex(4096);
        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (log_tx, mut log_rx) = mpsc::channel(8);

        // Spawn the loop. It will read from read_half (which we write to).
        let loop_handle =
            tokio::spawn(async move { read_build_stderr_loop(read_half, batcher, &log_tx).await });

        // Send exactly one STDERR_NEXT line, then go silent.
        {
            let mut w = StderrWriter::new(&mut write_half);
            w.log("line before silence").await?;
        }
        // Let the reader task drain the channel and the main loop add_line().
        // Under paused time, auto-advance doesn't kick in while tasks are
        // ready to run, so yield a few times to let the mpsc drain.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        // Before the tick: no batch yet (1 line < 64, and no tick fired yet).
        assert!(
            log_rx.try_recv().is_err(),
            "no batch should be sent before BATCH_TIMEOUT elapses"
        );

        // Advance past BATCH_TIMEOUT. The interval tick fires, the tick arm
        // sees has_pending() → flush() → send_batch.
        tokio::time::advance(BATCH_TIMEOUT + Duration::from_millis(10)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        // After the tick: batch with our one line should have arrived.
        let msg = log_rx
            .try_recv()
            .expect("batch should be flushed by the interval tick during silence");
        let Some(worker_message::Msg::LogBatch(batch)) = msg.msg else {
            panic!("expected LogBatch, got {:?}", msg.msg);
        };
        assert_eq!(batch.lines.len(), 1);
        assert_eq!(batch.lines[0], b"line before silence");

        // Cleanup: send STDERR_LAST + BuildResult so the loop terminates cleanly.
        {
            let mut w = StderrWriter::new(&mut write_half);
            w.finish().await?;
        }
        write_half
            .write_all(&build_result_bytes(&BuildResult::success()).await?)
            .await?;
        drop(write_half); // EOF

        let result = loop_handle.await?;
        let br = result.expect("loop should complete successfully");
        assert_eq!(br.status, BuildStatus::Built);
        Ok(())
    }

    /// Cancel-safety: a STDERR message that arrives in two halves (partial
    /// write, tick fires, rest of write) must NOT desync the protocol.
    ///
    /// This is the core cancel-safety proof: the naive approach of wrapping
    /// `read_stderr_message()` in `tokio::time::timeout` would drop the read
    /// future after consuming the first 4 bytes of the u64 tag, leaving the
    /// pipe at a non-message-boundary. The owned-task approach keeps the read
    /// future alive across ticks — only `msg_rx.recv()` is cancelled, and
    /// mpsc recv is cancel-safe.
    #[tokio::test(start_paused = true)]
    async fn test_reader_not_desynced_across_tick() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::STDERR_NEXT;

        let (mut write_half, read_half) = tokio::io::duplex(4096);
        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (log_tx, mut log_rx) = mpsc::channel(8);

        let loop_handle =
            tokio::spawn(async move { read_build_stderr_loop(read_half, batcher, &log_tx).await });

        // Write the first 4 bytes of the STDERR_NEXT u64 tag. This leaves
        // the reader task blocked mid-read_u64.
        let tag_bytes = STDERR_NEXT.to_le_bytes();
        write_half.write_all(&tag_bytes[..4]).await?;
        write_half.flush().await?;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        // Tick fires mid-read. In the BUGGY timeout-wrap approach, this would
        // cancel the read future; the 4 already-consumed bytes are gone, and
        // the NEXT read_u64 starts from byte 5 of the original message →
        // garbage tag → protocol desync. In the owned-task approach, the
        // read future is NOT cancelled; only msg_rx.recv() is.
        tokio::time::advance(BATCH_TIMEOUT + Duration::from_millis(10)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        // No batch (nothing pending — we haven't gotten a full Next yet).
        assert!(log_rx.try_recv().is_err());

        // Now send the REST of the tag + the payload. If the protocol is
        // intact, the reader task completes read_stderr_message() and pushes
        // a Next("intact-payload") onto msg_rx.
        write_half.write_all(&tag_bytes[4..]).await?;
        // STDERR_NEXT payload is a length-prefixed string (u64 len + bytes + padding)
        wire::write_string(&mut write_half, "intact-payload").await?;
        write_half.flush().await?;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        // One more tick to flush the single line.
        tokio::time::advance(BATCH_TIMEOUT + Duration::from_millis(10)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        let msg = log_rx.try_recv().expect(
            "protocol should be intact: partial-then-complete write across a tick must not desync",
        );
        let Some(worker_message::Msg::LogBatch(batch)) = msg.msg else {
            panic!("expected LogBatch, got {:?}", msg.msg);
        };
        assert_eq!(batch.lines.len(), 1);
        assert_eq!(
            batch.lines[0], b"intact-payload",
            "payload reassembled correctly across the mid-read tick"
        );

        // Cleanup.
        {
            let mut w = StderrWriter::new(&mut write_half);
            w.finish().await?;
        }
        write_half
            .write_all(&build_result_bytes(&BuildResult::success()).await?)
            .await?;
        drop(write_half);

        let result = loop_handle.await?;
        let br = result.expect("loop should complete successfully after reassembly");
        assert_eq!(br.status, BuildStatus::Built);
        Ok(())
    }

    /// The final flush (inside the loop, after STDERR_LAST) must drain any
    /// partial batch. The loop owns the batcher by-value, so draining is
    /// its responsibility.
    #[tokio::test]
    async fn test_final_flush_after_last() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // 3 lines < 64: these stay in the partial batch until final flush.
            w.log("tail-1").await?;
            w.log("tail-2").await?;
            w.log("tail-3").await?;
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, batches) = run_loop(buf).await;
        result.expect("should succeed");
        // The 3 lines MUST be in the batches (final flush fired).
        assert_eq!(
            count_log_lines(&batches),
            3,
            "final flush should drain the 3-line partial batch"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Log limiting (LogLimitExceeded path through the stderr loop)
    // -----------------------------------------------------------------------

    /// Run the stderr loop with a batcher that has the given limits.
    /// Same as `run_loop` but with configurable limits.
    async fn run_loop_with_limits(
        input: Vec<u8>,
        limits: crate::log_stream::LogLimits,
    ) -> (Result<BuildResult, wire::WireError>, Vec<WorkerMessage>) {
        let batcher = LogBatcher::new("/nix/store/test.drv".into(), "test-worker".into(), limits);
        let (tx, mut rx) = mpsc::channel(128);
        let cursor = std::io::Cursor::new(input);
        let result = read_build_stderr_loop(cursor, batcher, &tx).await;
        drop(tx);
        let mut batches = Vec::new();
        while let Some(m) = rx.recv().await {
            batches.push(m);
        }
        (result, batches)
    }

    /// Rate limit trip → BuildStatus::LogLimitExceeded (terminal, non-retry).
    /// The pre-trip lines must be flushed to the scheduler — the user wants
    /// to see output right up to the limit, not have 5 lines disappear.
    #[tokio::test]
    async fn test_stderr_loop_rate_limit_exceeded() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // 10 lines: 5 accepted, 6th trips a rate_limit=5.
            for i in 0..10 {
                w.log(&format!("spew {i}")).await?;
            }
            // No finish() — loop breaks on LimitExceeded before reaching here.
        }

        let (result, batches) = run_loop_with_limits(
            buf,
            crate::log_stream::LogLimits {
                rate_lines_per_sec: 5,
                total_bytes: 0,
            },
        )
        .await;

        let br = result.expect("limit-exceeded is Ok(failure), not Err");
        assert_eq!(
            br.status,
            BuildStatus::LogLimitExceeded,
            "should map to Nix-native BuildStatus=11"
        );
        assert!(
            br.error_msg.contains("log_rate_limit"),
            "error_msg should name the limit: {}",
            br.error_msg
        );
        // The 5 pre-trip lines MUST have been flushed. The LimitExceeded arm
        // in the loop explicitly flushes before breaking — if it didn't, a
        // build that trips at line 63 would lose lines 0-62.
        assert_eq!(
            count_log_lines(&batches),
            5,
            "pre-trip lines must be flushed; the tripping line is NOT buffered"
        );
        Ok(())
    }

    /// Size limit trip → same BuildStatus::LogLimitExceeded, different reason.
    #[tokio::test]
    async fn test_stderr_loop_size_limit_exceeded() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // Each "line-N" is 6 bytes. 3 lines = 18 bytes accepted.
            // 4th line (18+6=24 > 20) trips.
            w.log("line-0").await?;
            w.log("line-1").await?;
            w.log("line-2").await?;
            w.log("line-3").await?;
        }

        let (result, batches) = run_loop_with_limits(
            buf,
            crate::log_stream::LogLimits {
                rate_lines_per_sec: 0,
                total_bytes: 20,
            },
        )
        .await;

        let br = result.expect("limit-exceeded is Ok(failure)");
        assert_eq!(br.status, BuildStatus::LogLimitExceeded);
        assert!(
            br.error_msg.contains("log_size_limit"),
            "got: {}",
            br.error_msg
        );
        assert_eq!(count_log_lines(&batches), 3, "3 pre-trip lines flushed");
        Ok(())
    }

    /// With unlimited limits (the `run_loop` helper's default), the loop
    /// proceeds normally even under heavy log volume. Regression guard:
    /// making sure UNLIMITED really is a no-op.
    #[tokio::test]
    async fn test_stderr_loop_unlimited_does_not_trip() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // 200 lines in rapid succession — would trip any finite rate.
            for i in 0..200 {
                w.log(&format!("heavy {i}")).await?;
            }
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        // run_loop uses UNLIMITED internally.
        let (result, batches) = run_loop(buf).await;
        let br = result.expect("should succeed");
        assert_eq!(br.status, BuildStatus::Built);
        assert_eq!(count_log_lines(&batches), 200);
        Ok(())
    }

    /// STDERR_RESULT with result_type=101 (BuildLogLine) is captured as a
    /// log line. This is how modern nix-daemon actually sends builder
    /// output — NOT as raw STDERR_NEXT. Latent phase2a bug caught by
    /// vm-phase2b's log-pipeline assertion.
    #[tokio::test]
    async fn test_stderr_loop_result_build_log_line_captured() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::ResultField;
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // result_type 101 = BuildLogLine. First field is the line string.
            // activity_id 42 is arbitrary (we don't use it).
            w.result(
                42,
                101,
                &[ResultField::String("compiling foo.c".to_string())],
            )
            .await?;
            w.result(42, 101, &[ResultField::String("linking foo".to_string())])
                .await?;
            // result_type 107 = PostBuildLogLine — also captured.
            w.result(
                42,
                107,
                &[ResultField::String("post-build hook output".to_string())],
            )
            .await?;
            // result_type 105 = Progress — discarded (not a log line).
            w.result(42, 105, &[ResultField::Int(50), ResultField::Int(100)])
                .await?;
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, batches) = run_loop(buf).await;
        let br = result.expect("should succeed");
        assert_eq!(br.status, BuildStatus::Built);
        // 3 log lines captured (101×2 + 107×1). Progress (105) NOT captured.
        assert_eq!(
            count_log_lines(&batches),
            3,
            "101 + 107 captured, 105 discarded"
        );
        Ok(())
    }

    /// BuildLogLine via STDERR_RESULT is subject to the same rate/size
    /// limits as STDERR_NEXT.
    #[tokio::test]
    async fn test_stderr_loop_result_build_log_line_subject_to_limits() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::ResultField;
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            for i in 0..10 {
                w.result(
                    1,
                    101,
                    &[ResultField::String(format!("spew-via-result {i}"))],
                )
                .await?;
            }
        }

        let (result, batches) = run_loop_with_limits(
            buf,
            crate::log_stream::LogLimits {
                rate_lines_per_sec: 5,
                total_bytes: 0,
            },
        )
        .await;

        let br = result.expect("limit-exceeded is Ok(failure)");
        assert_eq!(br.status, BuildStatus::LogLimitExceeded);
        assert_eq!(count_log_lines(&batches), 5, "pre-trip lines flushed");
        Ok(())
    }
}
