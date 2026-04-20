//! Daemon I/O after spawn: handshake, wopBuildDerivation, STDERR loop, log batching.
// r[impl builder.daemon.stderr-result-logs]

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use rio_nix::protocol::build::{BuildMode, BuildResult, BuildStatus};
use rio_nix::protocol::client::{
    StderrMessage, client_handshake, client_send_build_derivation, client_set_options,
    read_stderr_message,
};
use rio_nix::protocol::stderr::ResultField;
use rio_nix::protocol::wire;
use rio_proto::types::{ExecutorMessage, executor_message};

use tracing::instrument;

use crate::executor::ExecutorError;
use crate::log_stream::{AddLineResult, BATCH_TIMEOUT, LogBatcher};

use super::DAEMON_SETUP_TIMEOUT;

/// Per-build option triple passed to the daemon. Bundled so
/// [`run_daemon_build`] stays under the `too_many_arguments` threshold;
/// per-field provenance (timeout from config, max_silent_time from
/// assignment, build_cores from cgroup clamp) is documented at
/// `executor::resolve_build_opts`.
pub(in crate::executor) struct DaemonBuildOpts {
    pub build_timeout: Duration,
    pub max_silent_time: u64,
    pub build_cores: u64,
}

/// All daemon I/O after spawn: handshake, setOptions, wopBuildDerivation, stderr loop.
///
/// Caller MUST kill the daemon after this returns (whether Ok or Err).
/// This is the only function that should touch daemon stdin/stdout —
/// keeping it isolated ensures the caller's always-kill path is reliable.
///
/// Span duration ≈ actual sandbox build time — this is the hot zone in
/// a trace (e.g., 95% of a slow build's wall time). `build_timeout` in
/// span fields so Tempo can correlate slow builds with timeout config.
// r[impl builder.daemon.stdio-client]
#[instrument(
    skip_all,
    fields(
        build_timeout_secs = opts.build_timeout.as_secs(),
        max_silent_secs = opts.max_silent_time,
        build_cores = opts.build_cores,
    )
)]
pub(in crate::executor) async fn run_daemon_build(
    daemon: &mut tokio::process::Child,
    drv_path: &str,
    basic_drv: &rio_nix::derivation::BasicDerivation,
    opts: DaemonBuildOpts,
    batcher: LogBatcher,
    log_tx: &mpsc::Sender<ExecutorMessage>,
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
    let negotiated_version = tokio::time::timeout(DAEMON_SETUP_TIMEOUT, async {
        let stdout_ref = stdout.as_mut().expect("taken only once, after this block");
        let handshake_result = client_handshake(stdout_ref, &mut stdin).await?;
        let negotiated = handshake_result.negotiated_version();
        tracing::debug!(version = negotiated, "daemon handshake complete");

        client_set_options(
            stdout_ref,
            &mut stdin,
            opts.max_silent_time,
            opts.build_cores,
        )
        .await?;

        client_send_build_derivation(&mut stdin, drv_path, basic_drv, BuildMode::Normal).await?;

        Ok::<_, ExecutorError>(negotiated)
    })
    .await
    .map_err(|_| ExecutorError::DaemonSetup("daemon setup sequence timed out".into()))??;

    // Read STDERR loop with log streaming (build may run for a long time).
    // stdout is moved into the loop's owned reader task; we don't need it
    // back because the loop itself reads BuildResult after STDERR_LAST.
    let stdout = stdout.take().expect("borrowed above, not yet taken");

    // All four limit-class terminations (build_timeout, max_silent_time,
    // byte-limit, msg-cap) live INSIDE read_build_stderr_loop's select!
    // and `break Ok(Some(BuildResult::failure(..)))`. That converges
    // them on `state.final_flush()` (so the trailing ≤63 buffered lines
    // reach log_tx) AND maps each to a non-reassignable proto status.
    // Timeout is a BUILD OUTCOME, not an executor error: returning Err
    // would land in runtime.rs's InfrastructureFailure arm →
    // reassignment storm (same build, same inputs, same timeout,
    // forever). The Result<_, WireError> here is different — a wire
    // error mid-STDERR-loop IS an executor fault (daemon died, pipe
    // corrupted); that `?` stays.
    //
    // The local nix-daemon MAY also enforce maxSilentTime itself
    // (forwarded via client_set_options above) — but rio-side is the
    // authoritative backstop: we get a correct TimedOut regardless of
    // what the daemon does, and the caller's unconditional
    // cgroup.kill() (executor/mod.rs) reaps the tree.
    //
    // r[impl builder.timeout.no-reassign]
    // r[impl builder.daemon.negotiated-version]
    let build_result = read_build_stderr_loop(
        stdout,
        opts.max_silent_time,
        opts.build_timeout,
        negotiated_version,
        batcher,
        log_tx,
    )
    .await?;

    Ok(build_result)
}

/// Hard cap on STDERR-protocol messages per build. Distinct from the
/// log-byte limit (`LogLimits.total_bytes`): a malformed daemon could
/// emit millions of zero-length `STDERR_NEXT` frames without ever
/// tripping the byte limit. Generous (10M) — a real build emitting that
/// many lines is itself pathological.
const MAX_BUILD_STDERR_MESSAGES: u64 = 10_000_000;

/// Outcome of one STDERR-loop event. `Ok(None)` = saw `STDERR_LAST`,
/// proceed to read `BuildResult`. `Ok(Some(fail))` = return that failure
/// without reading `BuildResult`. `Err(e)` = wire error, propagate.
type LoopOutcome = Result<Option<BuildResult>, wire::WireError>;

/// Per-message dispatch state for [`read_build_stderr_loop`]'s `select!`.
///
/// Holds the batcher + log channel + book-keeping that every match arm
/// touches. Methods correspond to the loop's phases: classify a message,
/// forward a log line, forward a phase, time-driven flush. Splitting the
/// 240-line `select!` body into methods keeps each arm at the call site
/// (where the cancel-safety / `biased;` ordering comments belong) while
/// the actual work lives next to the state it mutates.
struct StderrLoop<'a> {
    batcher: LogBatcher,
    log_tx: &'a mpsc::Sender<ExecutorMessage>,
    /// Count of STDERR messages seen so far; bounded by
    /// [`MAX_BUILD_STDERR_MESSAGES`].
    msg_count: u64,
    /// Last instant an OUTPUT-producing message arrived (Next or
    /// Result{101,107}). Progress chatter does NOT bump this — see
    /// `r[builder.silence.timeout-kill]`.
    last_output: Instant,
    /// `max_silent_time` as a Duration, for the silence-deadline arm.
    silence: Duration,
}

impl<'a> StderrLoop<'a> {
    fn new(
        batcher: LogBatcher,
        log_tx: &'a mpsc::Sender<ExecutorMessage>,
        silence: Duration,
    ) -> Self {
        Self {
            batcher,
            log_tx,
            msg_count: 0,
            last_output: Instant::now(),
            silence,
        }
    }

    /// Shared log-line handling for `STDERR_NEXT` and `STDERR_RESULT`
    /// `BuildLogLine` — `batcher.add_line` + `AddLineResult` dispatch.
    /// On `Continue`, bumps `last_output`.
    async fn on_log_line(&mut self, line: Vec<u8>) -> std::ops::ControlFlow<LoopOutcome> {
        use std::ops::ControlFlow::*;
        match self.batcher.add_line(line) {
            AddLineResult::Buffered => {
                self.last_output = Instant::now();
                Continue(())
            }
            AddLineResult::BatchReady(batch) => {
                if send_batch(self.log_tx, batch).await {
                    self.last_output = Instant::now();
                    Continue(())
                } else {
                    Break(Ok(Some(misc_fail(
                        "log channel closed during build (scheduler stream gone)",
                    ))))
                }
            }
            // r[impl builder.log-limit+2]
            AddLineResult::LimitExceeded { reason } => {
                // Flush what's buffered so client sees output up to
                // the limit. final_flush() also drains
                // lines_dropped_this_window (has_pending() doesn't
                // cover that). Best-effort: channel-closed is moot,
                // we're breaking anyway.
                let batch = self.batcher.final_flush();
                if !batch.lines.is_empty() {
                    let _ = send_batch(self.log_tx, batch).await;
                }
                tracing::warn!(reason = %reason, "build log limit exceeded, aborting");
                Break(Ok(Some(BuildResult::failure(
                    BuildStatus::LogLimitExceeded,
                    reason,
                ))))
            }
        }
    }

    // r[impl builder.stderr.forward-set-phase]
    /// `STDERR_RESULT{104 SetPhase}`: forward as `BuildPhase` so the
    /// gateway can emit `resSetPhase` against the per-drv activity → nom
    /// shows "buildPhase"/"installPhase". NOT batched (state edge, not
    /// spew); does NOT reset the silence deadline (a build that only
    /// emits phase markers but no actual output is still "silent" by
    /// the maxSilentTime contract — same rule as Progress chatter).
    ///
    /// Flushes any buffered log lines first: `BuildPhase` carries no
    /// `first_line_number`, so if the phase marker overtook the ≤63
    /// lines awaiting `flush_tick` on `log_tx` the gateway/nom could
    /// not reorder them. This is the only `Continue` path that sends
    /// directly on `log_tx` instead of going through the batcher; the
    /// pre-flush keeps the channel's order monotone.
    async fn forward_phase(&mut self, phase: &str) -> std::ops::ControlFlow<LoopOutcome> {
        if self.batcher.has_pending() && !send_batch(self.log_tx, self.batcher.flush()).await {
            return std::ops::ControlFlow::Break(Ok(Some(misc_fail(
                "log channel closed during build (scheduler stream gone)",
            ))));
        }
        let msg = ExecutorMessage {
            msg: Some(executor_message::Msg::Phase(rio_proto::types::BuildPhase {
                derivation_path: self.batcher.drv_path().to_owned(),
                phase: phase.to_owned(),
            })),
        };
        if self.log_tx.send(msg).await.is_err() {
            std::ops::ControlFlow::Break(Ok(Some(misc_fail(
                "log channel closed during build (scheduler stream gone)",
            ))))
        } else {
            std::ops::ControlFlow::Continue(())
        }
    }

    /// Classify one parsed `StderrMessage` and dispatch to the
    /// appropriate handler. Called from the `msg_rx.recv()` arm.
    ///
    /// Every parsed message flows through here, so this is where
    /// [`MAX_BUILD_STDERR_MESSAGES`] is enforced — a single choke
    /// point that no arm (including `Result{104}` → `forward_phase`
    /// and the discard arm) can bypass. Counting only in
    /// `on_log_line` let a malicious build flood the scheduler with
    /// `BuildPhase` frames via `@nix {"action":"setPhase",...}` on
    /// `NIX_LOG_FD` for the full `build_timeout`.
    async fn dispatch(&mut self, msg: StderrMessage) -> std::ops::ControlFlow<LoopOutcome> {
        use std::ops::ControlFlow::*;
        // r[impl builder.stderr.msg-cap]
        self.msg_count += 1;
        if self.msg_count >= MAX_BUILD_STDERR_MESSAGES {
            tracing::warn!("build exceeded MAX_BUILD_STDERR_MESSAGES, aborting");
            return Break(Ok(Some(BuildResult::failure(
                BuildStatus::LogLimitExceeded,
                format!("build exceeded {MAX_BUILD_STDERR_MESSAGES} STDERR-protocol messages"),
            ))));
        }
        match msg {
            StderrMessage::Last => Break(Ok(None)),
            StderrMessage::Error(e) => Break(Ok(Some(misc_fail(&e.message)))),
            StderrMessage::Next(line) => self.on_log_line(line.into_bytes()).await,
            StderrMessage::Read(_) => Break(Ok(Some(misc_fail(
                "daemon sent STDERR_READ, not supported",
            )))),
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
            StderrMessage::Result {
                result_type,
                fields,
                ..
            } if (result_type == 101 || result_type == 107)
                && matches!(fields.first(), Some(ResultField::String(_))) =>
            {
                // Safe: matches! above verified it's Some(String(_)).
                let ResultField::String(line) = &fields[0] else {
                    unreachable!("match guard above proved String")
                };
                self.on_log_line(line.as_bytes().to_vec()).await
            }
            StderrMessage::Result {
                result_type,
                fields,
                ..
            } if result_type == 104 && matches!(fields.first(), Some(ResultField::String(_))) => {
                let ResultField::String(phase) = &fields[0] else {
                    unreachable!("match guard above proved String")
                };
                self.forward_phase(phase).await
            }
            // Other Result types (Progress, SetExpected, etc.) and
            // activity lifecycle messages — discard.
            StderrMessage::Write(_)
            | StderrMessage::StartActivity { .. }
            | StderrMessage::StopActivity { .. }
            | StderrMessage::Result { .. } => Continue(()),
        }
    }

    /// Tick-driven flush: if the batcher has pending lines, flush them.
    /// The interval itself IS the 100ms gate; we don't use
    /// `batcher.maybe_flush()` here — that checks against
    /// `std::time::Instant`, which doesn't advance under tokio's
    /// paused-time test mode. The tick already proved 100ms of
    /// tokio-time elapsed.
    async fn flush_tick(&mut self) -> std::ops::ControlFlow<LoopOutcome> {
        if self.batcher.has_pending() && !send_batch(self.log_tx, self.batcher.flush()).await {
            std::ops::ControlFlow::Break(Ok(Some(misc_fail(
                "log channel closed during build (scheduler stream gone)",
            ))))
        } else {
            std::ops::ControlFlow::Continue(())
        }
    }

    /// Best-effort final flush after the loop exits. The build result is
    /// already determined; if the log channel is closed, just drop.
    async fn final_flush(&mut self) {
        // Unconditional: final_flush() drains lines_dropped_this_window
        // even when self.lines is empty (the case has_pending()
        // deliberately doesn't cover — see its doc). Skip the send if
        // nothing came out.
        let batch = self.batcher.final_flush();
        if !batch.lines.is_empty() {
            let _ = send_batch(self.log_tx, batch).await;
        }
    }
}

/// Send a log batch on the executor stream. Returns `false` if the
/// channel is closed (scheduler stream gone). Free fn (not a method on
/// `StderrLoop`) so callers can `batcher.flush()` (mutable self.batcher)
/// in the same expression without a split-borrow dance.
async fn send_batch(
    log_tx: &mpsc::Sender<ExecutorMessage>,
    batch: rio_proto::types::BuildLogBatch,
) -> bool {
    let msg = ExecutorMessage {
        msg: Some(executor_message::Msg::LogBatch(batch)),
    };
    log_tx.send(msg).await.is_ok()
}

fn misc_fail(m: &str) -> BuildResult {
    BuildResult::failure(BuildStatus::MiscFailure, m.to_string())
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
    max_silent_time: u64,
    build_timeout: Duration,
    negotiated_version: u64,
    batcher: LogBatcher,
    log_tx: &mpsc::Sender<ExecutorMessage>,
) -> Result<BuildResult, wire::WireError>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    // r[impl builder.silence.timeout-kill]
    // Silence deadline: fires when last_output + max_silent_time is
    // reached without an intervening output-producing message. Uses
    // tokio::time::Instant (NOT std) so paused-time tests work — same
    // constraint as the flush_tick arm below. max_silent_time == 0
    // disables (select! arm guard). Reset only in the Next and
    // Result{101,107} arms: those are the output-producing paths.
    // Activity/Write/Progress chatter does NOT reset — a build that
    // spins sending progress updates but no actual log lines is still
    // "silent" by the maxSilentTime contract (builder stderr quiescence).
    //
    // Build deadline: absolute hard cap, never resets. Computed here
    // (not by caller) so paused-time tests advance it via
    // tokio::time::advance and the error message can name the
    // configured seconds.
    let build_deadline = Instant::now() + build_timeout;
    let mut state = StderrLoop::new(batcher, log_tx, Duration::from_secs(max_silent_time));

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

    let outcome: LoopOutcome = loop {
        tokio::select! {
            // `biased` prioritizes the message arm over the tick arm when
            // both are ready. Under heavy log spew we want to drain messages
            // before doing a tick-driven flush (the 64-line batch-full
            // trigger in add_line already handles chatty builds).
            biased;

            maybe = msg_rx.recv() => {
                match maybe {
                    Some(Ok(msg)) => {
                        if let std::ops::ControlFlow::Break(r) = state.dispatch(msg).await {
                            break r;
                        }
                    }
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
                if let std::ops::ControlFlow::Break(r) = state.flush_tick().await {
                    break r;
                }
            }

            // Silence deadline. biased; above ensures a pending message is
            // always consumed (and resets last_output) before this arm can
            // fire — a chatty build never triggers the silence kill. A
            // fresh sleep_until future is created each iteration with the
            // current last_output; when last_output is reset the NEXT
            // iteration's deadline is pushed out. sleep_until with a past
            // deadline fires immediately, which is what we want after a
            // long msg_rx.recv() await where no output arrived.
            _ = tokio::time::sleep_until(state.last_output + state.silence),
                if max_silent_time > 0
            => {
                tracing::warn!(
                    max_silent_time,
                    "build silent for maxSilentTime; reporting TimedOut (no reassignment)"
                );
                break Ok(Some(BuildResult::failure(
                    BuildStatus::TimedOut,
                    format!("no output for {max_silent_time}s (maxSilentTime)"),
                )));
            }

            // Hard build deadline. biased; above means a pending message
            // (or the silence arm) wins on the same iteration; this is
            // the absolute backstop. Breaking out of the loop reaches
            // final_flush below — symmetric with the silence arm so the
            // last ≤63 buffered lines reach log_tx instead of being
            // dropped with the future (the previous outer-timeout shape).
            _ = tokio::time::sleep_until(build_deadline) => {
                tracing::warn!(
                    timeout_secs = build_timeout.as_secs(),
                    "build exceeded timeout; reporting TimedOut (no reassignment)"
                );
                break Ok(Some(BuildResult::failure(
                    BuildStatus::TimedOut,
                    format!(
                        "build exceeded configured timeout of {}s",
                        build_timeout.as_secs()
                    ),
                )));
            }
        }
    };

    // Final flush: the loop owns the batcher (by-value), so any partial
    // batch must be drained here.
    state.final_flush().await;

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
            rio_nix::protocol::build::read_build_result(&mut reader, negotiated_version).await
        }
    }
}

// r[verify builder.daemon.stderr-result-logs]
// r[verify builder.daemon.stdio-client]
#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // read_build_stderr_loop — pure async, no daemon process needed
    // -----------------------------------------------------------------------

    use tokio::io::AsyncWriteExt;

    use rio_nix::protocol::build::write_build_result;
    use rio_nix::protocol::handshake::{PROTOCOL_VERSION, encode_version};
    use rio_nix::protocol::stderr::{STDERR_READ, STDERR_WRITE, StderrError, StderrWriter};

    /// Effectively-disabled build_timeout for tests that don't exercise
    /// the build-deadline arm. Far enough out that no
    /// `tokio::time::advance` in this module reaches it.
    const NO_BUILD_TIMEOUT: Duration = Duration::from_secs(86400 * 365);

    /// Serialize a BuildResult to wire bytes at `PROTOCOL_VERSION` (test
    /// controls both write and read sides, so the constant is correct).
    async fn build_result_bytes(r: &BuildResult) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::new();
        write_build_result(&mut buf, r, PROTOCOL_VERSION).await?;
        Ok(buf)
    }

    /// Run read_build_stderr_loop against a Cursor of `input` bytes with a
    /// fresh batcher. Returns (result, all batches received on log_rx).
    async fn run_loop(
        input: Vec<u8>,
    ) -> (Result<BuildResult, wire::WireError>, Vec<ExecutorMessage>) {
        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (tx, mut rx) = mpsc::channel(128);
        let cursor = std::io::Cursor::new(input);
        let result =
            read_build_stderr_loop(cursor, 0, NO_BUILD_TIMEOUT, PROTOCOL_VERSION, batcher, &tx)
                .await;
        drop(tx);
        let mut batches = Vec::new();
        while let Some(m) = rx.recv().await {
            batches.push(m);
        }
        (result, batches)
    }

    /// Count total log lines across all received BuildLogBatch messages.
    fn count_log_lines(batches: &[ExecutorMessage]) -> usize {
        batches
            .iter()
            .filter_map(|m| match &m.msg {
                Some(executor_message::Msg::LogBatch(b)) => Some(b.lines.len()),
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

        let result =
            read_build_stderr_loop(cursor, 0, NO_BUILD_TIMEOUT, PROTOCOL_VERSION, batcher, &tx)
                .await;
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
                    &[],
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
        let loop_handle = tokio::spawn(async move {
            read_build_stderr_loop(
                read_half,
                0,
                NO_BUILD_TIMEOUT,
                PROTOCOL_VERSION,
                batcher,
                &log_tx,
            )
            .await
        });

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
        let Some(executor_message::Msg::LogBatch(batch)) = msg.msg else {
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

        let loop_handle = tokio::spawn(async move {
            read_build_stderr_loop(
                read_half,
                0,
                NO_BUILD_TIMEOUT,
                PROTOCOL_VERSION,
                batcher,
                &log_tx,
            )
            .await
        });

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
        let Some(executor_message::Msg::LogBatch(batch)) = msg.msg else {
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

    // -----------------------------------------------------------------------
    // maxSilentTime enforcement
    // -----------------------------------------------------------------------

    /// Spawn read_build_stderr_loop with the given silence timeout.
    /// Returns (write_half, log_rx, loop_handle) for paused-time
    /// orchestration.
    #[allow(clippy::type_complexity)]
    fn spawn_loop_with_silence(
        max_silent_time: u64,
    ) -> (
        tokio::io::DuplexStream,
        mpsc::Receiver<ExecutorMessage>,
        tokio::task::JoinHandle<Result<BuildResult, wire::WireError>>,
    ) {
        let (write_half, read_half) = tokio::io::duplex(4096);
        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (log_tx, log_rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move {
            read_build_stderr_loop(
                read_half,
                max_silent_time,
                NO_BUILD_TIMEOUT,
                PROTOCOL_VERSION,
                batcher,
                &log_tx,
            )
            .await
        });
        (write_half, log_rx, handle)
    }

    /// Yield a few times so spawned tasks drain mpsc channels. Under
    /// paused time, auto-advance doesn't kick in while tasks are ready
    /// to run.
    async fn drain_tasks() {
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    /// r[verify builder.silence.timeout-kill]
    /// One output line, then silence past max_silent_time → TimedOut.
    /// The silence arm is the THIRD select! branch; biased; means a
    /// pending message always wins over the silence fire, so this test
    /// proves the silence arm fires when NO message is pending.
    #[tokio::test(start_paused = true)]
    async fn test_silence_timeout_fires_after_max_silent_time() -> anyhow::Result<()> {
        let (mut write_half, _log_rx, loop_handle) = spawn_loop_with_silence(5);

        // One STDERR_NEXT: resets last_output to "now" (paused time t=0).
        {
            let mut w = StderrWriter::new(&mut write_half);
            w.log("starting").await?;
        }
        drain_tasks().await;

        // Advance 3s. Below the 5s deadline — loop should still be
        // running (flush_tick has fired 30× but silence arm hasn't).
        tokio::time::advance(Duration::from_secs(3)).await;
        drain_tasks().await;
        assert!(
            !loop_handle.is_finished(),
            "loop should not exit at 3s < 5s"
        );

        // Advance past 5s total. Silence arm fires → TimedOut.
        tokio::time::advance(Duration::from_secs(3)).await;
        drain_tasks().await;

        let br = loop_handle
            .await?
            .expect("silence timeout is Ok(TimedOut), not Err");
        assert_eq!(
            br.status,
            BuildStatus::TimedOut,
            "silence past maxSilentTime must be TimedOut (no-reassign status)"
        );
        assert!(
            br.error_msg.contains("5s") && br.error_msg.contains("maxSilentTime"),
            "error_msg should name the limit and cause: {}",
            br.error_msg
        );
        Ok(())
    }

    /// Output arriving before the deadline RESETS last_output. Build
    /// that sends a line every 3s with a 5s maxSilentTime must NOT
    /// time out — each line pushes the deadline 5s forward.
    #[tokio::test(start_paused = true)]
    async fn test_silence_timeout_resets_on_output() -> anyhow::Result<()> {
        let (mut write_half, _log_rx, loop_handle) = spawn_loop_with_silence(5);

        // Send one line, advance 3s, repeat ×3. Each 3s is < 5s, so
        // the deadline never fires. Total elapsed = 9s, well past 5s
        // absolute — proves the timer is a MOVING window, not absolute.
        for i in 0..3 {
            {
                let mut w = StderrWriter::new(&mut write_half);
                w.log(&format!("tick {i}")).await?;
            }
            drain_tasks().await;
            tokio::time::advance(Duration::from_secs(3)).await;
            drain_tasks().await;
            assert!(
                !loop_handle.is_finished(),
                "loop should still be running at iteration {i} (3s < 5s since last output)"
            );
        }

        // Clean termination: STDERR_LAST + BuildResult. Proves the
        // silence arm didn't somehow preempt a pending terminal message.
        {
            let mut w = StderrWriter::new(&mut write_half);
            w.finish().await?;
        }
        write_half
            .write_all(&build_result_bytes(&BuildResult::success()).await?)
            .await?;
        drop(write_half);

        let br = loop_handle.await?.expect("should complete Built");
        assert_eq!(
            br.status,
            BuildStatus::Built,
            "output every 3s with 5s maxSilentTime should never trip"
        );
        Ok(())
    }

    /// max_silent_time = 0 disables the silence arm entirely. A build
    /// that goes silent for 100s with silence=0 must NOT time out.
    /// This is the select! arm's `if max_silent_time > 0` guard.
    #[tokio::test(start_paused = true)]
    async fn test_silence_timeout_disabled_when_zero() -> anyhow::Result<()> {
        let (mut write_half, _log_rx, loop_handle) = spawn_loop_with_silence(0);

        {
            let mut w = StderrWriter::new(&mut write_half);
            w.log("only line").await?;
        }
        drain_tasks().await;

        // Advance 100s. If the guard were wrong (e.g. checked for >= 0
        // or the arm lacked a guard), sleep_until(t0 + 0s) would have
        // fired immediately on the first iteration.
        tokio::time::advance(Duration::from_secs(100)).await;
        drain_tasks().await;
        assert!(
            !loop_handle.is_finished(),
            "max_silent_time=0 must disable the silence arm (100s elapsed)"
        );

        // Clean termination.
        {
            let mut w = StderrWriter::new(&mut write_half);
            w.finish().await?;
        }
        write_half
            .write_all(&build_result_bytes(&BuildResult::success()).await?)
            .await?;
        drop(write_half);
        let br = loop_handle.await?.expect("should complete Built");
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
    // Log limiting (rate suppression + size LogLimitExceeded)
    // -----------------------------------------------------------------------

    /// Run the stderr loop with a batcher that has the given limits.
    /// Same as `run_loop` but with configurable limits.
    async fn run_loop_with_limits(
        input: Vec<u8>,
        limits: crate::log_stream::LogLimits,
    ) -> (Result<BuildResult, wire::WireError>, Vec<ExecutorMessage>) {
        let batcher = LogBatcher::new("/nix/store/test.drv".into(), "test-worker".into(), limits);
        let (tx, mut rx) = mpsc::channel(128);
        let cursor = std::io::Cursor::new(input);
        let result =
            read_build_stderr_loop(cursor, 0, NO_BUILD_TIMEOUT, PROTOCOL_VERSION, batcher, &tx)
                .await;
        drop(tx);
        let mut batches = Vec::new();
        while let Some(m) = rx.recv().await {
            batches.push(m);
        }
        (result, batches)
    }

    /// Rate limit suppresses excess lines; build continues to completion.
    #[tokio::test]
    async fn test_stderr_loop_rate_limit_suppresses() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // 10 lines: 5 accepted, 5 dropped (rate=5, single window).
            for i in 0..10 {
                w.log(&format!("spew {i}")).await?;
            }
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, batches) = run_loop_with_limits(
            buf,
            crate::log_stream::LogLimits {
                rate_lines_per_sec: 5,
                total_bytes: 0,
            },
        )
        .await;

        let br = result.expect("rate suppression is not a failure");
        assert_eq!(
            br.status,
            BuildStatus::Built,
            "rate limit drops lines, does NOT abort the build"
        );
        // 5 buffered, 5 dropped → final_flush emits the suppression marker
        // even though the window never reset (test completes in <1s).
        assert_eq!(
            count_log_lines(&batches),
            6,
            "5 lines accepted + 1 suppression marker on final flush"
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

    /// r[verify builder.fod.verify-hash]
    ///
    /// P0308 T1: FOD-failure BuildResult propagation — worker-side isolation.
    ///
    /// Reproduces the netpol.nix (pre-ADR-019):285 denied-case WIRE SEQUENCE as seen by
    /// the worker's stderr loop: `STDERR_RESULT{101, "wget: 403..."}` (how
    /// modern nix-daemon forwards builder output) followed by `STDERR_LAST`
    /// followed by `BuildResult{PermanentFailure}`. This is the Nix daemon's
    /// contract for `wopBuildDerivation` — the daemon catches build errors
    /// INTERNALLY and returns them via BuildResult, not via STDERR_ERROR.
    ///
    /// **What this test proves:** IF the daemon writes these bytes to its
    /// stdout, the worker's stderr loop receives the PermanentFailure
    /// correctly and promptly. The loop does not branch on BuildResult.status
    /// for parsing (read_build_result reads the same fields regardless), so
    /// the success and failure wire shapes are identical.
    ///
    /// **What this test localizes:** The netpol.nix (pre-ADR-019):285 hang is NOT in
    /// the worker's stderr parsing. The daemon is not sending these bytes —
    /// it is blocked inside `buildDerivation()` before reaching the
    /// `logger->stopWork()` (STDERR_LAST) call. Root cause: the daemon's
    /// post-fail cleanup probes the never-created `$out` at
    /// `/nix/store/<fod-output>`, which falls through the overlay
    /// (upper → host → FUSE) to a FUSE `lookup()`. JIT FUSE lookup
    /// classifies output basenames as `JitClass::NotInput` → ENOENT
    /// without store contact, so the daemon's stat returns immediately.
    #[tokio::test]
    async fn test_fod_failure_buildresult_propagates_promptly() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::ResultField;

        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // Daemon's pre-build chatter: STDERR_START_ACTIVITY. Discarded
            // by the loop; included to match the real wire shape.
            let aid = w
                .start_activity(
                    rio_nix::protocol::stderr::ActivityType::Build,
                    "building '/nix/store/xxx-rio-fod-fetch.drv'",
                    0,
                    0,
                    &[],
                )
                .await?;
            // Builder output: wget's 403 line, forwarded as
            // STDERR_RESULT{101}. This is the line P0243 CONFIRMED
            // arrives at nix-build — so we know the daemon writes at
            // least up to here, and the worker forwards it.
            w.result(
                aid,
                rio_nix::protocol::stderr::ResultType::BuildLogLine,
                &[ResultField::String(
                    "wget: server returned error: HTTP/1.1 403 Forbidden".into(),
                )],
            )
            .await?;
            // Builder exits 1. Daemon's buildDone() cleanup runs — THIS
            // is where the real daemon hangs (overlay→FUSE stat of the
            // never-created $out). In this test the bytes continue:
            w.stop_activity(aid).await?;
            // logger->stopWork() → STDERR_LAST. The daemon DOES send
            // this on both success and failure paths (daemon.cc's
            // wopBuildDerivation handler runs stopWork unconditionally
            // before writing BuildResult).
            w.finish().await?;
        }
        // BuildResult with PermanentFailure. Same wire format as
        // success — read_build_result reads status + errorMsg +
        // timesBuilt + isNonDet + startTime + stopTime + cpuUser +
        // cpuSystem (≥1.37) + builtOutputs. builtOutputs is empty on
        // failure (output_count=0 → loop body skipped).
        buf.extend(
            build_result_bytes(&BuildResult::failure(
                BuildStatus::PermanentFailure,
                "builder for '/nix/store/xxx-rio-fod-fetch.drv' failed with exit code 1",
            ))
            .await?,
        );

        // tokio::time::timeout on a Cursor-backed future is just a
        // safety rail — the loop resolves in microseconds. If THIS
        // times out, the stderr loop has a parsing bug that only
        // triggers on the failure shape (which we've argued is
        // impossible since the parser is status-agnostic, but the
        // test makes that argument executable).
        let (result, batches) = tokio::time::timeout(Duration::from_secs(5), run_loop(buf))
            .await
            .expect("stderr loop should return promptly on failure BuildResult, not hang");

        let br = result.expect("loop should return Ok(BuildResult), not WireError");
        assert_eq!(
            br.status,
            BuildStatus::PermanentFailure,
            "failure status should propagate unchanged"
        );
        assert!(
            br.error_msg.contains("exit code 1"),
            "daemon's error_msg should propagate: {}",
            br.error_msg
        );
        // The wget 403 line was a STDERR_RESULT{101} → captured as a log line.
        assert_eq!(
            count_log_lines(&batches),
            1,
            "wget 403 line captured (proves the full wire sequence parsed cleanly)"
        );
        Ok(())
    }

    // r[verify builder.stderr.forward-set-phase]
    /// STDERR_RESULT{104 SetPhase} is forwarded as a `BuildPhase`
    /// `ExecutorMessage` (not batched, not a log line).
    #[tokio::test]
    async fn test_stderr_loop_forwards_set_phase() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::{ResultField, ResultType};
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.result(
                42,
                ResultType::SetPhase,
                &[ResultField::String("buildPhase".into())],
            )
            .await?;
            w.result(
                42,
                ResultType::BuildLogLine,
                &[ResultField::String("compiling".into())],
            )
            .await?;
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, msgs) = run_loop(buf).await;
        result.expect("should succeed");
        // Phase arrives as its own ExecutorMessage::Phase, separate from
        // the LogBatch containing "compiling".
        let phases: Vec<_> = msgs
            .iter()
            .filter_map(|m| match &m.msg {
                Some(executor_message::Msg::Phase(p)) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(phases.len(), 1, "msgs: {msgs:?}");
        assert_eq!(phases[0].phase, "buildPhase");
        assert_eq!(phases[0].derivation_path, "/nix/store/test.drv");
        // The log line still lands in a LogBatch.
        assert_eq!(count_log_lines(&msgs), 1);
        Ok(())
    }

    /// STDERR_RESULT with result_type=101 (BuildLogLine) is captured as a
    /// log line. This is how modern nix-daemon actually sends builder
    /// output — NOT as raw STDERR_NEXT. Latent phase2a bug caught by
    /// vm-phase2b's log-pipeline assertion.
    #[tokio::test]
    async fn test_stderr_loop_result_build_log_line_captured() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::{ResultField, ResultType};
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            // result_type 101 = BuildLogLine. First field is the line string.
            // activity_id 42 is arbitrary (we don't use it).
            w.result(
                42,
                ResultType::BuildLogLine,
                &[ResultField::String("compiling foo.c".to_string())],
            )
            .await?;
            w.result(
                42,
                ResultType::BuildLogLine,
                &[ResultField::String("linking foo".to_string())],
            )
            .await?;
            // result_type 107 = PostBuildLogLine — also captured.
            w.result(
                42,
                ResultType::PostBuildLogLine,
                &[ResultField::String("post-build hook output".to_string())],
            )
            .await?;
            // result_type 105 = Progress — discarded (not a log line).
            w.result(
                42,
                ResultType::Progress,
                &[ResultField::Int(50), ResultField::Int(100)],
            )
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

    /// BuildLogLine via STDERR_RESULT is subject to the same rate
    /// suppression as STDERR_NEXT.
    #[tokio::test]
    async fn test_stderr_loop_result_build_log_line_subject_to_limits() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::{ResultField, ResultType};
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            for i in 0..10 {
                w.result(
                    1,
                    ResultType::BuildLogLine,
                    &[ResultField::String(format!("spew-via-result {i}"))],
                )
                .await?;
            }
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, batches) = run_loop_with_limits(
            buf,
            crate::log_stream::LogLimits {
                rate_lines_per_sec: 5,
                total_bytes: 0,
            },
        )
        .await;

        let br = result.expect("rate suppression is not a failure");
        assert_eq!(br.status, BuildStatus::Built);
        assert_eq!(
            count_log_lines(&batches),
            6,
            "5 within rate + 1 suppression marker on final flush"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // dispatch() msg_count cap — single choke point for ALL StderrMessage
    // variants. Regression: forward_phase / discard arm previously bypassed.
    // -----------------------------------------------------------------------

    fn mk_stderr_loop(tx: &mpsc::Sender<ExecutorMessage>) -> StderrLoop<'_> {
        let batcher = LogBatcher::new(
            "/nix/store/t.drv".into(),
            "w".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        StderrLoop::new(batcher, tx, Duration::ZERO)
    }

    /// Hitting the cap MUST be a build outcome (`LogLimitExceeded`),
    /// NOT `Err(WireError)`. `Err` would propagate via `inner?` →
    /// `ExecutorError::Wire` → `is_daemon_transient()=false` /
    /// `is_permanent()=false` → `InfrastructureFailure` → reassignment
    /// storm (same build emits 10M messages on every worker).
    fn assert_msg_cap_outcome(r: std::ops::ControlFlow<LoopOutcome>) {
        let std::ops::ControlFlow::Break(Ok(Some(br))) = r else {
            panic!("msg-cap MUST Break(Ok(Some(LogLimitExceeded))), got {r:?}");
        };
        assert_eq!(
            br.status,
            BuildStatus::LogLimitExceeded,
            "msg-cap is terminal non-retryable, same semantics as byte-limit"
        );
    }

    /// r[verify builder.stderr.msg-cap]
    /// `Result{104}` (SetPhase) MUST count toward `MAX_BUILD_STDERR_MESSAGES`.
    /// Previously `forward_phase` bypassed `msg_count` entirely → a malicious
    /// build could flood unbounded `BuildPhase` frames for `build_timeout`.
    #[tokio::test]
    async fn test_dispatch_msg_cap_covers_set_phase() {
        use std::ops::ControlFlow::*;
        let (tx, _rx) = mpsc::channel(8);
        let mut state = mk_stderr_loop(&tx);
        state.msg_count = MAX_BUILD_STDERR_MESSAGES - 2;
        let phase_msg = || StderrMessage::Result {
            activity_id: 0,
            result_type: 104,
            fields: vec![ResultField::String("x".into())],
        };
        assert!(matches!(state.dispatch(phase_msg()).await, Continue(())));
        assert_msg_cap_outcome(state.dispatch(phase_msg()).await);
    }

    /// Discard arm (`StartActivity` / `StopActivity` / `Write` / other
    /// `Result`) MUST also count — zero-length `StartActivity` spam is the
    /// same threat as zero-length `STDERR_NEXT` spam the cap was added for.
    #[tokio::test]
    async fn test_dispatch_msg_cap_covers_discard_arm() {
        use std::ops::ControlFlow::*;
        let (tx, _rx) = mpsc::channel(8);
        let mut state = mk_stderr_loop(&tx);
        state.msg_count = MAX_BUILD_STDERR_MESSAGES - 2;
        let act_msg = || StderrMessage::StartActivity {
            id: 1,
            level: 0,
            activity_type: 0,
            text: String::new(),
            fields: vec![],
            parent_id: 0,
        };
        assert!(matches!(state.dispatch(act_msg()).await, Continue(())));
        assert_msg_cap_outcome(state.dispatch(act_msg()).await);
    }

    /// Sensitivity: the original `Next` path still hits the cap after
    /// the check moved from `on_log_line` to `dispatch`.
    #[tokio::test]
    async fn test_dispatch_msg_cap_covers_next() {
        use std::ops::ControlFlow::*;
        let (tx, _rx) = mpsc::channel(8);
        let mut state = mk_stderr_loop(&tx);
        state.msg_count = MAX_BUILD_STDERR_MESSAGES - 2;
        assert!(matches!(
            state.dispatch(StderrMessage::Next("a".into())).await,
            Continue(())
        ));
        assert_msg_cap_outcome(state.dispatch(StderrMessage::Next("b".into())).await);
    }

    // -----------------------------------------------------------------------
    // build_timeout select! arm — symmetric with silence: drains final_flush
    // -----------------------------------------------------------------------

    /// The build-deadline arm MUST `break` the select! loop so
    /// `final_flush` runs — same as the silence arm. Regression: when
    /// `build_timeout` was an outer `tokio::time::timeout` wrapper, the
    /// inner future was DROPPED mid-select on elapse, discarding the
    /// owned `LogBatcher` (≤63 buffered lines) without flush.
    ///
    /// `build_timeout` is set to `< BATCH_TIMEOUT` (50ms < 100ms) so
    /// `flush_tick` is structurally Pending when the deadline fires.
    /// With any 100ms-multiple timeout, `biased;` lets the co-scheduled
    /// `flush_tick` win first and the line-count assertion passes for
    /// the wrong reason — `final_flush()` then runs on an empty batcher
    /// and the assertion would not catch its removal.
    #[tokio::test(start_paused = true)]
    async fn test_build_deadline_flushes_buffered_lines() -> anyhow::Result<()> {
        let (mut write_half, read_half) = tokio::io::duplex(4096);
        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (log_tx, mut log_rx) = mpsc::channel(8);
        let loop_handle = tokio::spawn(async move {
            read_build_stderr_loop(
                read_half,
                0,
                Duration::from_millis(50),
                PROTOCOL_VERSION,
                batcher,
                &log_tx,
            )
            .await
        });

        // 3 lines (< MAX_BATCH_LINES=64): stay buffered, no flush yet.
        {
            let mut w = StderrWriter::new(&mut write_half);
            w.log("tail-a").await?;
            w.log("tail-b").await?;
            w.log("tail-c").await?;
        }
        drain_tasks().await;
        assert!(
            log_rx.try_recv().is_err(),
            "3 lines < 64, no flush_tick yet → batcher still holding them"
        );

        // Advance past build_timeout (50ms) but not BATCH_TIMEOUT (100ms):
        // flush_tick is Pending, deadline arm fires, final_flush is the
        // ONLY path that can have sent the 3 lines.
        tokio::time::advance(Duration::from_millis(60)).await;
        drain_tasks().await;

        let br = loop_handle
            .await?
            .expect("build-deadline is Ok(TimedOut), not Err");
        assert_eq!(br.status, BuildStatus::TimedOut);
        assert!(
            br.error_msg.contains("0s"),
            "error_msg names the configured timeout: {}",
            br.error_msg
        );

        let mut received = Vec::new();
        while let Ok(m) = log_rx.try_recv() {
            received.push(m);
        }
        assert_eq!(
            count_log_lines(&received),
            3,
            "build-deadline arm MUST reach final_flush (was dropped under outer-timeout shape)"
        );
        Ok(())
    }

    // r[verify builder.stderr.forward-set-phase]
    /// A `SetPhase` arriving immediately after a buffered log line MUST
    /// NOT overtake it on `log_tx`: `forward_phase` flushes the batcher
    /// first. Regression: previously the line stayed in the batcher
    /// until `final_flush` (after STDERR_LAST), so the receiver saw
    /// `Phase` at index 0 and `LogBatch` at index 1.
    #[tokio::test]
    async fn test_stderr_loop_phase_after_buffered_line_preserves_order() -> anyhow::Result<()> {
        use rio_nix::protocol::stderr::{ResultField, ResultType};
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.result(
                42,
                ResultType::BuildLogLine,
                &[ResultField::String("configuring...".into())],
            )
            .await?;
            w.result(
                42,
                ResultType::SetPhase,
                &[ResultField::String("buildPhase".into())],
            )
            .await?;
            w.finish().await?;
        }
        buf.extend(build_result_bytes(&BuildResult::success()).await?);

        let (result, msgs) = run_loop(buf).await;
        result.expect("should succeed");
        assert_eq!(msgs.len(), 2, "one LogBatch + one Phase: {msgs:?}");
        assert!(
            matches!(
                &msgs[0].msg,
                Some(executor_message::Msg::LogBatch(b)) if b.lines == [b"configuring...".to_vec()]
            ),
            "msgs[0] must be the buffered log line, got: {:?}",
            msgs[0]
        );
        assert!(
            matches!(
                &msgs[1].msg,
                Some(executor_message::Msg::Phase(p)) if p.phase == "buildPhase"
            ),
            "msgs[1] must be the phase marker, got: {:?}",
            msgs[1]
        );
        Ok(())
    }

    // r[verify builder.daemon.negotiated-version]
    /// `read_build_stderr_loop` MUST decode `BuildResult` at the
    /// negotiated version, not `PROTOCOL_VERSION`. Regression: the
    /// negotiated value from `client_handshake` was logged then
    /// discarded; `read_build_result` was called with the hardcoded
    /// constant, so a daemon at 1.36 (no cpu fields) would desync.
    #[tokio::test]
    async fn test_stderr_loop_uses_negotiated_version_for_build_result() -> anyhow::Result<()> {
        let v136 = encode_version(1, 36);
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.finish().await?;
        }
        // BuildResult serialized at 1.36 — NO cpu_user/cpu_system fields.
        write_build_result(&mut buf, &BuildResult::success(), v136).await?;

        let batcher = LogBatcher::new(
            "/nix/store/test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (tx, _rx) = mpsc::channel(8);
        let cursor = std::io::Cursor::new(buf);
        let br = read_build_stderr_loop(cursor, 0, NO_BUILD_TIMEOUT, v136, batcher, &tx)
            .await
            .expect("1.36-serialized BuildResult must parse at negotiated_version=1.36");
        assert_eq!(br.status, BuildStatus::Built);
        assert_eq!(br.times_built, 1);
        Ok(())
    }
}
