//! Per-build `AppendLog` client: streams log batches to rio-store with an
//! in-memory ack-trimmed retransmit buffer.
//!
//! One [`LogUploader`] is spawned per build attempt. The stderr loop feeds
//! it [`BuildLogBatch`]es through a bounded channel; the uploader pushes
//! each onto a retransmit buffer and sends it on an open
//! `LogService.AppendLog` stream to rio-store. The store acks
//! `durable_through_line` after each chunk it durably commits (S3 PUT +
//! manifest INSERT); the uploader trims every buffered frame whose last
//! line is covered by an ack. On any stream failure the uploader reconnects
//! (a fresh session server-side) and retransmits everything still in the
//! buffer — the store's read path deduplicates overlapping session chunks
//! by line number, so at-least-once delivery here is safe.
//!
//! The buffer is in-memory only (no on-disk WAL): a non-fsynced file on
//! emptyDir that is never read across a process boundary has exactly the
//! durability of the heap it would be copied from. The buffer's worst case
//! is the per-build log size cap (the store unreachable for the whole
//! build); its steady state is one store-side chunk interval's worth of
//! lines. The cap counts content bytes, but each buffered line is its own
//! `Vec<u8>` (24-byte header + allocator slack), so the resident worst
//! case runs ~25–30% over the cap for ordinary lines — and more for
//! pathologically short ones, which the upstream rate limiter keeps
//! unreachable for non-malicious builds. The real bound is the pod's own
//! memory limit, not this comment's arithmetic.
//!
//! Wired into `spawn_build_task` in the next change. Until then this module
//! has no production caller.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Code, Request, Status, Streaming};
use tracing::Instrument;

use rio_proto::store::append_log_request::Msg;
use rio_proto::store::log_service_client::LogServiceClient;
use rio_proto::store::{AppendLogAck, AppendLogHeader, AppendLogRequest};
use rio_proto::types::BuildLogBatch;

use crate::upload::common::attach_assignment_token;

/// Capacity of the stderr-loop → uploader channel. Matches the old
/// scheduler-bound permanent sink: at the 100 ms batch-flush cadence a full
/// channel represents ~25 s of log output, after which the stderr loop's
/// `send().await` blocks and backpressure propagates to the build's stdout
/// pipe. The uploader drains this channel into its (unbounded, but
/// log-size-capped) retransmit buffer even while the store is unreachable,
/// so a store outage does not stall the build.
const INPUT_QUEUE_DEPTH: usize = 256;

/// Capacity of the per-stream outbound request channel. Small: the buffer
/// of record is the retransmit `VecDeque`, not this channel — anything
/// sitting here un-sent is also still in the buffer and will be replayed
/// on the next connection if this one dies.
const OUTBOUND_QUEUE_DEPTH: usize = 16;

/// How long a finished build's detached uploader keeps reconnecting and
/// replaying before giving up on un-acked lines. This is the bound on "the
/// store was down when the build completed": any outage shorter than this
/// loses nothing, because the build slot is freed at `finish()`'s grace
/// timeout while this task keeps draining in the background. Expiry is
/// recorded as `rio_builder_log_drain_abandoned_total` — that counter
/// incrementing means log lines were durably lost.
const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(600);

/// Fixed delay between reconnect attempts. The store's failure modes are
/// restart/deploy shaped (seconds of total unavailability), not congestion
/// shaped, so exponential backoff buys nothing over the scheduler stream's
/// established 1 s constant.
const DEFAULT_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Tuning for [`LogUploader::spawn_with_config`]. Production callers use
/// [`Default`]; tests compress the timings so the slowest case stays under
/// a second of wall clock.
#[derive(Debug, Clone, Copy)]
pub struct LogUploaderConfig {
    /// Delay between reconnect attempts after a stream failure.
    pub reconnect_backoff: Duration,
    /// How long the detached task keeps draining after the input closes.
    pub drain_deadline: Duration,
}

impl Default for LogUploaderConfig {
    fn default() -> Self {
        Self {
            reconnect_backoff: DEFAULT_RECONNECT_BACKOFF,
            drain_deadline: DEFAULT_DRAIN_DEADLINE,
        }
    }
}

/// A snapshot of the uploader's durability progress, published on a watch
/// channel so [`LogUploader::finish`] can report the state of a drain it
/// did not wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// The highest line number the store has acknowledged as durably
    /// committed. `None` until the first ack.
    pub last_acked_line: Option<u64>,
    /// Lines accepted from the stderr loop but not yet covered by an ack.
    pub unacked_lines: u64,
    /// The upload task has exited (drained, abandoned, or permanently
    /// rejected).
    pub done: bool,
}

/// Why an upload ended with lines still un-acked. Every variant names a
/// distinct disposition of those lines; `lost_lines` is computed FROM
/// the variant, so "is this loss?" is a property of the type, not a
/// judgment call repeated at call sites (`builder.log.loss-disclosure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbandonReason {
    /// The drain deadline expired on lines the store would have
    /// accepted. Real, durable loss.
    DeadlineExpired,
    /// The store already holds a complete `[0, final_line_count)` log
    /// for this execution (`x-rio-log-reject: complete`, or bare
    /// `FAILED_PRECONDITION` from a pre-metadata store). The un-acked
    /// lines are a replay of content the store provably has: lost: 0.
    CompleteLog,
    /// The execution was superseded (`x-rio-log-reject: superseded`,
    /// or bare `PERMISSION_DENIED`). THIS execution's un-sent tail is
    /// durably gone — the superseding attempt produces its own log,
    /// not these lines.
    Superseded,
    /// The execution hit its per-execution byte/chunk cap
    /// (`x-rio-log-reject: cap`). Everything past the cap is
    /// deliberately discarded — but it is still loss, and it is
    /// disclosed as such.
    CapExhausted,
    /// The upload task panicked mid-flight (counted by `LossGuard`
    /// during unwind, so even a never-awaited detached task
    /// discloses).
    Panicked,
}

impl AbandonReason {
    /// The `reason` label value on
    /// `rio_builder_log_drain_abandoned_total`.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::DeadlineExpired => "deadline_expired",
            Self::CompleteLog => "complete_log",
            Self::Superseded => "superseded",
            Self::CapExhausted => "cap_exhausted",
            Self::Panicked => "panic",
        }
    }
}

/// How many of `unacked` lines are durably lost under `reason`. Zero
/// ONLY for [`AbandonReason::CompleteLog`] — the one case where the
/// store provably holds every line of the finished log.
fn lost_lines(reason: AbandonReason, unacked: u64) -> u64 {
    match reason {
        AbandonReason::CompleteLog => 0,
        AbandonReason::DeadlineExpired
        | AbandonReason::Superseded
        | AbandonReason::CapExhausted
        | AbandonReason::Panicked => unacked,
    }
}

// r[impl builder.log.loss-disclosure]
/// THE single disclosure site: the only place
/// `rio_builder_log_drain_abandoned_total` is incremented.
/// Counter ⟺ `lost_lines > 0`, by construction — a zero-loss abandon
/// (CompleteLog, or any reason with nothing un-acked) logs at `debug!`
/// and fires nothing.
fn disclose(reason: AbandonReason, unacked_lines: u64, last_acked_line: Option<u64>) {
    let lost = lost_lines(reason, unacked_lines);
    if lost == 0 {
        tracing::debug!(
            reason = reason.as_label(),
            "log upload abandoned with nothing durably lost"
        );
        return;
    }
    metrics::counter!(
        "rio_builder_log_drain_abandoned_total",
        "reason" => reason.as_label(),
    )
    .increment(1);
    tracing::error!(
        reason = reason.as_label(),
        lost_lines = lost,
        ?last_acked_line,
        "log upload abandoned with un-acked lines: those lines were \
         never durably stored and will be missing from the build log"
    );
}

/// Drop-guard that discloses a panic-shaped loss. Armed before the
/// upload loop runs, defused on every normal exit; if the task unwinds
/// (or is torn down without reaching its exit path), the guard's `Drop`
/// reads the last published [`Progress`] and routes through
/// `disclose` with [`AbandonReason::Panicked`]. This covers the
/// `JoinError` path AND a post-detach panic — nobody has to await the
/// handle for the loss to be counted.
struct LossGuard {
    progress: watch::Receiver<Progress>,
    armed: bool,
}

impl LossGuard {
    fn new(progress: watch::Receiver<Progress>) -> Self {
        Self {
            progress,
            armed: true,
        }
    }

    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for LossGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let p = *self.progress.borrow();
        disclose(AbandonReason::Panicked, p.unacked_lines, p.last_acked_line);
    }
}

/// The terminal state of a build's log upload, returned by
/// [`LogUploader::finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainStatus {
    /// Every accepted line was acked durable.
    Drained {
        /// The last line the store acknowledged. `None` iff the build
        /// produced no log lines at all.
        final_acked_line: Option<u64>,
    },
    /// The grace period expired with lines still un-acked. The upload task
    /// keeps running in the background (reconnecting and replaying) until
    /// everything is acked or the drain deadline expires; the caller should
    /// proceed with its `CompletionReport` — a build is never failed or
    /// delayed because its log could not be persisted yet.
    Detached {
        last_acked_line: Option<u64>,
        unacked_lines: u64,
    },
    /// The upload ended with lines still un-acked. Whether that is loss
    /// — and whether the loss counter fired — is decided by `reason`
    /// (see `lost_lines`); the disclosure itself happened at the
    /// task's single `disclose` site before this status was returned.
    Abandoned {
        last_acked_line: Option<u64>,
        unacked_lines: u64,
        reason: AbandonReason,
    },
}

/// Handle to a per-build `AppendLog` upload task. See the module doc.
pub struct LogUploader {
    /// The uploader's own copy of the input sender. The input channel
    /// closes — and the drain begins — only once this AND every clone
    /// handed out by [`Self::sender`] have been dropped.
    tx: Option<mpsc::Sender<BuildLogBatch>>,
    /// The monitored task handle. Held for liveness only — the terminal
    /// status arrives on `status`, and a panic is disclosed by the
    /// task's own `LossGuard` (plus `spawn_monitored`'s `error!`),
    /// not by awaiting this.
    _handle: JoinHandle<()>,
    /// One-shot carrying the task's terminal [`DrainStatus`]. Dropped
    /// without a send iff the task panicked.
    status: oneshot::Receiver<DrainStatus>,
    progress: watch::Receiver<Progress>,
}

impl LogUploader {
    /// Spawn the upload task for one build attempt with production tuning.
    ///
    /// `client` is a clone of the builder's existing rio-store channel
    /// (tonic channels are cheaply cloneable). `exec_id` and
    /// `derivation_path` come from the `WorkAssignment`; `token` is the
    /// assignment token, attached as `x-rio-assignment-token` metadata on
    /// every stream open (empty = dev mode, no header).
    pub fn spawn(
        client: LogServiceClient<Channel>,
        exec_id: String,
        derivation_path: String,
        token: String,
    ) -> Self {
        Self::spawn_with_config(
            client,
            exec_id,
            derivation_path,
            token,
            LogUploaderConfig::default(),
        )
    }

    /// [`Self::spawn`] with explicit tuning. Tests use this to compress the
    /// reconnect backoff and the drain deadline.
    pub fn spawn_with_config(
        client: LogServiceClient<Channel>,
        exec_id: String,
        derivation_path: String,
        token: String,
        config: LogUploaderConfig,
    ) -> Self {
        let (tx, rx) = mpsc::channel(INPUT_QUEUE_DEPTH);
        let (progress_tx, progress_rx) = watch::channel(Progress::default());
        // The span every log line in the task inherits. Without it the
        // task's `error!`/`warn!` events ("log upload abandoned with
        // un-acked lines", the reconnect warnings) would name no build —
        // tokio does not propagate the spawner's span into a spawned task.
        // Mirrors `executor_future.instrument(build_span)` in
        // runtime/mod.rs.
        let span = tracing::info_span!(
            "log_upload",
            exec_id = %exec_id,
            drv_path = %derivation_path,
        );
        let task = UploadTask {
            client,
            header: AppendLogHeader {
                derivation_path,
                exec_id,
            },
            token,
            config,
            input: rx,
            input_open: true,
            buffer: VecDeque::new(),
            unacked_lines: 0,
            last_acked_line: None,
            deadline: None,
            progress: progress_tx,
        };
        let (status_tx, status_rx) = oneshot::channel();
        // `spawn_monitored`, not a raw `tokio::spawn`: a panic in the
        // upload loop is logged with the task name even if nobody ever
        // awaits the handle (the detached-drain case). The loss itself
        // is disclosed by the task's `LossGuard` during unwind.
        let handle =
            rio_common::task::spawn_monitored("log_upload", task.run(status_tx).instrument(span));
        Self {
            tx: Some(tx),
            _handle: handle,
            status: status_rx,
            progress: progress_rx,
        }
    }

    /// A clone of the input sender for the stderr loop. The channel is
    /// bounded (`INPUT_QUEUE_DEPTH`); a full channel blocks the sender,
    /// which is the designed backpressure path from the store all the way
    /// back to the build's stdout pipe.
    pub fn sender(&self) -> mpsc::Sender<BuildLogBatch> {
        self.tx
            .as_ref()
            .expect("sender() called after finish()")
            .clone()
    }

    /// A watch on the upload's durability progress. Useful for logging the
    /// state of a detached drain after [`Self::finish`] has returned.
    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// Half-close the input and wait up to `grace` for every accepted line
    /// to be acked durable.
    ///
    /// The input channel closes only once every sender clone handed out by
    /// [`Self::sender`] has also been dropped — call this after the stderr
    /// loop has exited. If the drain does not complete within `grace`, the
    /// upload task is detached: it keeps reconnecting and replaying in the
    /// background for up to the drain deadline while the caller proceeds
    /// (sends its `CompletionReport`, frees the build slot). A build is
    /// never failed or delayed because its log could not be persisted.
    pub async fn finish(mut self, grace: Duration) -> DrainStatus {
        // Drop our copy of the sender. Once the stderr loop's clones are
        // gone too, the task's input channel yields `None` and the drain
        // deadline starts.
        self.tx = None;
        match tokio::time::timeout(grace, &mut self.status).await {
            Ok(Ok(status)) => status,
            Ok(Err(_recv)) => {
                // The status sender dropped without a send: the task
                // panicked. ONLY log here — the loss was already
                // disclosed (counted, error!-ed) by the task's
                // LossGuard during unwind, and `spawn_monitored` logged
                // the panic itself; a second disclosure here would
                // double-count.
                tracing::error!("log upload task exited without reporting (panic)");
                let p = *self.progress.borrow();
                DrainStatus::Abandoned {
                    last_acked_line: p.last_acked_line,
                    unacked_lines: p.unacked_lines,
                    reason: AbandonReason::Panicked,
                }
            }
            Err(_elapsed) => {
                // Detach: dropping the receiver does NOT abort the task;
                // it keeps draining until the deadline.
                let p = *self.progress.borrow();
                DrainStatus::Detached {
                    last_acked_line: p.last_acked_line,
                    unacked_lines: p.unacked_lines,
                }
            }
        }
    }
}

/// Why the current `AppendLog` session stopped being usable.
enum SessionEnd {
    /// The stream died (send failed, the ack stream errored with a
    /// retryable status or ended with lines still un-acked). Reconnect
    /// and replay.
    Reconnect,
    /// Everything accepted has been acked and the input is closed.
    Drained,
    /// The drain deadline expired with lines still un-acked.
    DeadlineExpired,
    /// The store rejected the stream MID-FLIGHT with a permanent status
    /// (a cap trip, the completeness seal landing, a supersession).
    /// Reconnecting can never succeed — without this arm the uploader
    /// re-opened against the same rejection at 1 Hz until the drain
    /// deadline (bug_248), burning a connection per second for up to
    /// ten minutes per finished build.
    PermanentReject(Status),
}

/// Why an open attempt did not produce a usable session.
enum OpenFailure {
    /// Transport error or a retryable status — back off and try again.
    Retryable(Status),
    /// The store will never accept this stream (the log is already
    /// complete, or the derivation has been re-assigned to another
    /// executor). Stop trying; keep draining the input so the stderr loop
    /// never blocks.
    Permanent(Status),
}

struct UploadTask {
    client: LogServiceClient<Channel>,
    header: AppendLogHeader,
    token: String,
    config: LogUploaderConfig,

    input: mpsc::Receiver<BuildLogBatch>,
    /// False once the input channel has yielded `None`.
    input_open: bool,
    /// Every accepted batch not yet covered by an ack, in line order.
    /// Retransmitted in full on every reconnect.
    buffer: VecDeque<BuildLogBatch>,
    /// Cached `Σ lines.len()` over `buffer`, so progress publication does
    /// not walk the deque.
    unacked_lines: u64,
    last_acked_line: Option<u64>,
    /// Set when the input closes; expiring it abandons the drain.
    deadline: Option<Instant>,
    progress: watch::Sender<Progress>,
}

impl UploadTask {
    async fn run(mut self, status_tx: oneshot::Sender<DrainStatus>) {
        // Armed across the whole loop: an unwind anywhere below
        // discloses {reason="panic"} from the last published progress.
        let guard = LossGuard::new(self.progress.subscribe());
        let status = self.run_inner().await;
        guard.defuse();
        // Publish the terminal state so a detached `finish()` caller's
        // progress watch observes the exit.
        self.publish(true);
        match &status {
            DrainStatus::Drained { final_acked_line } => {
                tracing::debug!(?final_acked_line, "log upload drained");
            }
            // The single disclosure site decides loss-or-not from the
            // reason: CompleteLog (the store provably holds the full
            // log) logs at debug and fires nothing; every other reason
            // counts its un-acked lines as durable loss, reason-labeled.
            DrainStatus::Abandoned {
                unacked_lines,
                last_acked_line,
                reason,
            } => disclose(*reason, *unacked_lines, *last_acked_line),
            // Unreachable from run_inner (Detached is only constructed by
            // finish()), but harmless.
            DrainStatus::Detached { .. } => {}
        }
        let _ = status_tx.send(status);
    }

    async fn run_inner(&mut self) -> DrainStatus {
        loop {
            // Exit conditions, checked between sessions.
            if !self.input_open && self.buffer.is_empty() {
                return DrainStatus::Drained {
                    final_acked_line: self.last_acked_line,
                };
            }
            if self.deadline_expired() {
                return self.abandoned();
            }

            // Open a session. `open()` drains the input into the buffer
            // while it works, so a store outage never blocks the stderr
            // loop on a full input channel.
            let (out, acks) = match self.open().await {
                Ok(session) => session,
                Err(OpenFailure::Permanent(status)) => {
                    tracing::warn!(
                        code = ?status.code(),
                        message = status.message(),
                        "store permanently rejected the log stream; discarding \
                         this build's log output"
                    );
                    return self
                        .reject_permanently(abandon_reason_for_rejection(&status))
                        .await;
                }
                Err(OpenFailure::Retryable(status)) => {
                    tracing::debug!(
                        code = ?status.code(),
                        message = status.message(),
                        "log stream open failed; backing off"
                    );
                    self.backoff().await;
                    continue;
                }
            };

            // Drive the session until it dies or drains.
            match self.drive(out, acks).await {
                SessionEnd::Drained => {
                    return DrainStatus::Drained {
                        final_acked_line: self.last_acked_line,
                    };
                }
                SessionEnd::DeadlineExpired => return self.abandoned(),
                SessionEnd::PermanentReject(status) => {
                    tracing::warn!(
                        code = ?status.code(),
                        message = status.message(),
                        "store permanently rejected the log stream mid-flight; \
                         discarding the rest of this build's log output"
                    );
                    return self
                        .reject_permanently(abandon_reason_for_rejection(&status))
                        .await;
                }
                SessionEnd::Reconnect => {
                    metrics::counter!("rio_builder_log_append_reconnects_total").increment(1);
                    tracing::warn!(
                        unacked_lines = self.unacked_lines,
                        "log stream to the store ended; reconnecting and \
                         replaying the un-acked tail"
                    );
                    self.backoff().await;
                }
            }
        }
    }

    /// Open an `AppendLog` stream: buffer the header into the request
    /// stream, then await the call.
    ///
    /// The header MUST be written before the call is awaited — the server
    /// does not return response headers until it has read and validated the
    /// header, so the reverse order deadlocks (the client waits for
    /// response headers the server will not send until it receives the
    /// header the client has not sent). Every open-time rejection therefore
    /// surfaces as an error from the call itself, before the ack stream
    /// exists. See the `AppendLog` RPC comment in `store.proto`.
    async fn open(
        &mut self,
    ) -> Result<(mpsc::Sender<AppendLogRequest>, Streaming<AppendLogAck>), OpenFailure> {
        let (out_tx, out_rx) = mpsc::channel::<AppendLogRequest>(OUTBOUND_QUEUE_DEPTH);
        out_tx
            .try_send(AppendLogRequest {
                msg: Some(Msg::Header(self.header.clone())),
            })
            .expect("the header always fits in a fresh outbound channel");

        let mut req = Request::new(ReceiverStream::new(out_rx));
        attach_assignment_token(&mut req, &self.token).map_err(OpenFailure::Permanent)?;

        let mut client = self.client.clone();
        let call = client.append_log(req);
        match self.await_while_buffering(call).await {
            Some(Ok(resp)) => Ok((out_tx, resp.into_inner())),
            Some(Err(status)) => Err(classify_open_failure(status)),
            // Drain deadline expired while the open hung: surface as
            // retryable — run_inner's loop-top deadline check converts
            // it to the counted Abandoned exit.
            None => Err(OpenFailure::Retryable(Status::deadline_exceeded(
                "drain deadline expired while AppendLog open was awaited",
            ))),
        }
    }

    /// Run one open session: transmit the buffer (a replay on a reconnect,
    /// a no-op on the first connection of an idle build), forward new input
    /// batches, and trim the buffer as acks arrive.
    ///
    /// `sent` indexes the first buffer entry not yet transmitted on THIS
    /// connection. A fresh connection starts at 0, so the whole un-acked
    /// buffer is retransmitted; an ack pops entries off the front and
    /// decrements `sent` to match. New input is pushed onto the back and
    /// picked up by the same send arm — there is no separate "replay
    /// phase", which is what makes input arriving mid-replay safe.
    async fn drive(
        &mut self,
        out: mpsc::Sender<AppendLogRequest>,
        mut acks: Streaming<AppendLogAck>,
    ) -> SessionEnd {
        let mut out = Some(out);
        let mut sent: usize = 0;

        loop {
            // Half-close the outbound once everything accepted has been
            // transmitted and no more input is coming: the server runs its
            // final drain (cutting every remaining contiguous run) and acks
            // it, then ends the ack stream.
            if !self.input_open && sent == self.buffer.len() && out.is_some() {
                out = None;
            }

            let deadline = self.deadline_sleep();
            tokio::pin!(deadline);

            tokio::select! {
                // Acks first: trimming the buffer is what lets everything
                // else make progress, and a biased select keeps the
                // drain-completion check prompt.
                biased;

                ack = acks.message() => match ack {
                    Ok(Some(ack)) => {
                        let popped = self.trim(ack.durable_through_line);
                        sent = sent.saturating_sub(popped);
                        if !self.input_open && self.buffer.is_empty() {
                            return SessionEnd::Drained;
                        }
                    }
                    // The server ended the ack stream. If everything is
                    // acked this is the clean end of a drained session;
                    // otherwise the server went away mid-stream and the
                    // un-acked tail must be replayed elsewhere.
                    Ok(None) => {
                        if !self.input_open && self.buffer.is_empty() {
                            return SessionEnd::Drained;
                        }
                        return SessionEnd::Reconnect;
                    }
                    Err(status) => {
                        // Classify before reconnecting (bug_248): a
                        // permanent code or an `x-rio-log-reject` class
                        // landing MID-STREAM (the cap tripping, the seal
                        // landing, a supersession) will reject every
                        // future open identically — reconnecting against
                        // it is a 1 Hz storm until the drain deadline.
                        if is_permanent_rejection(&status) {
                            return SessionEnd::PermanentReject(status);
                        }
                        tracing::debug!(code = ?status.code(), message = status.message(),
                            "log ack stream errored");
                        return SessionEnd::Reconnect;
                    }
                },

                // Transmit the next un-sent buffered batch. `reserve()` is
                // cancel-safe (an unused permit is released on drop) and
                // lets the batch be cloned only once a slot is guaranteed.
                permit = async { out.as_ref().expect("guarded by the if").reserve().await },
                    if out.is_some() && sent < self.buffer.len() =>
                {
                    match permit {
                        Ok(permit) => {
                            permit.send(AppendLogRequest {
                                msg: Some(Msg::Batch(self.buffer[sent].clone())),
                            });
                            sent += 1;
                        }
                        // The request stream's receiver is gone: the server
                        // tore the stream down.
                        Err(_) => return SessionEnd::Reconnect,
                    }
                }

                // Accept new input. Always enabled while the input is open,
                // even when the outbound is saturated — the buffer (not the
                // outbound channel) is the backpressure absorber.
                batch = self.input.recv(), if self.input_open => {
                    self.accept(batch);
                }

                _ = &mut deadline, if self.deadline.is_some() => {
                    return SessionEnd::DeadlineExpired;
                }
            }
        }
    }

    /// Await `fut` while continuing to drain the input channel into the
    /// retransmit buffer. Used for the open call and the reconnect backoff
    /// so that a store outage never stalls the stderr loop behind a full
    /// input channel.
    ///
    /// Returns `None` when the drain deadline expires while awaiting
    /// (merged_bug_181): the deadline used to be enforced only inside an
    /// OPEN session and at the loop top, so a hung `open()` — a server
    /// that accepts the connection but withholds response headers —
    /// parked the drain forever past its 600 s bound.
    async fn await_while_buffering<F>(&mut self, fut: F) -> Option<F::Output>
    where
        F: std::future::Future,
    {
        tokio::pin!(fut);
        loop {
            // Re-created per iteration (the deadline_sleep pattern):
            // the deadline arms when accept(None) observes the input
            // close, which can happen INSIDE this loop — a snapshot at
            // entry would miss it.
            let expiry = self.deadline_sleep();
            tokio::select! {
                out = &mut fut => return Some(out),
                batch = self.input.recv(), if self.input_open => self.accept(batch),
                _ = expiry, if self.deadline.is_some() => return None,
            }
        }
    }

    /// Sleep the reconnect backoff (still draining the input). Returns
    /// early when the drain deadline expires mid-backoff.
    async fn backoff(&mut self) {
        let sleep = tokio::time::sleep(self.config.reconnect_backoff);
        let _ = self.await_while_buffering(sleep).await;
    }

    /// Record one input-channel recv result: push a batch onto the
    /// retransmit buffer, or mark the input closed and start the drain
    /// deadline.
    fn accept(&mut self, batch: Option<BuildLogBatch>) {
        match batch {
            Some(batch) => {
                if batch.lines.is_empty() {
                    // The batcher never emits empty batches; an empty batch
                    // has no last line to ack against, so drop it rather
                    // than poison the trim arithmetic.
                    return;
                }
                self.unacked_lines += batch.lines.len() as u64;
                self.buffer.push_back(batch);
                self.publish(false);
            }
            None => {
                self.input_open = false;
                self.deadline = Some(Instant::now() + self.config.drain_deadline);
            }
        }
    }

    /// Pop every buffered frame whose last line is covered by
    /// `durable_through_line`. Returns the number of frames popped.
    fn trim(&mut self, durable_through_line: u64) -> usize {
        let mut popped = 0;
        while let Some(front) = self.buffer.front() {
            // checked_add, not `+`: a frame whose line numbers overflow
            // u64 is a protocol violation (LogBatcher numbers lines
            // monotonically from 0), and the failure mode must be the
            // same in every build profile — debug `+` panics on
            // overflow but release WRAPS, which would silently corrupt
            // the ack trim (and let the panic-disclosure test pass in
            // dev while never firing under the release-built CI gate,
            // which is exactly how this line was found). The panic is
            // caught by the upload task's LossGuard and disclosed as
            // reason=panic.
            let last_line = front
                .first_line_number
                .checked_add(front.lines.len() as u64 - 1)
                .expect("log frame line numbers overflow u64 (protocol violation)");
            if last_line > durable_through_line {
                break;
            }
            self.unacked_lines = self.unacked_lines.saturating_sub(front.lines.len() as u64);
            self.buffer.pop_front();
            popped += 1;
        }
        if popped > 0 {
            self.last_acked_line = Some(
                self.last_acked_line
                    .map_or(durable_through_line, |l| l.max(durable_through_line)),
            );
            self.publish(false);
        }
        popped
    }

    /// The store will never accept this stream. Drop the buffer and keep
    /// draining the input to /dev/null until the build finishes, so the
    /// stderr loop never blocks on a log the store will not take.
    ///
    /// `reason` carries WHICH permanent class fired; whether the
    /// discarded lines count as loss is `lost_lines`'s decision at
    /// the single `disclose` site (CompleteLog: no — the store
    /// provably holds the finished log; Superseded/CapExhausted: yes).
    async fn reject_permanently(&mut self, reason: AbandonReason) -> DrainStatus {
        self.buffer.clear();
        // `unacked_lines` deliberately keeps counting: the Abandoned status
        // reports how many lines were discarded.
        while self.input_open {
            let batch = self.input.recv().await;
            match batch {
                Some(b) => {
                    self.unacked_lines += b.lines.len() as u64;
                    self.publish(false);
                }
                None => self.input_open = false,
            }
        }
        DrainStatus::Abandoned {
            last_acked_line: self.last_acked_line,
            unacked_lines: self.unacked_lines,
            reason,
        }
    }

    fn abandoned(&self) -> DrainStatus {
        DrainStatus::Abandoned {
            reason: AbandonReason::DeadlineExpired,
            last_acked_line: self.last_acked_line,
            unacked_lines: self.unacked_lines,
        }
    }

    fn deadline_expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// A sleep-until-the-drain-deadline future, or a far-future sleep when
    /// the input is still open. Re-created per select iteration; cheap.
    fn deadline_sleep(&self) -> tokio::time::Sleep {
        match self.deadline {
            Some(d) => tokio::time::sleep_until(d),
            // Never polled: the select arm is gated on
            // `self.deadline.is_some()`.
            None => tokio::time::sleep(Duration::from_secs(3600)),
        }
    }

    fn publish(&self, done: bool) {
        self.progress.send_replace(Progress {
            last_acked_line: self.last_acked_line,
            unacked_lines: self.unacked_lines,
            done,
        });
    }
}

/// Classify an open-time rejection.
///
/// `FAILED_PRECONDITION` = the execution's log is already complete (the
/// completeness seal); `PERMISSION_DENIED` = the token no longer matches
/// the derivation's latest assignment (re-dispatched to another executor).
/// Neither will ever succeed on retry. Everything else — transport errors,
/// `UNAVAILABLE`, `UNAUTHENTICATED` (a clock-skewed expiry check),
/// `ALREADY_EXISTS` (another live session holds the lease, e.g. a
/// reconnect racing the server's teardown of the previous stream),
/// `RESOURCE_EXHAUSTED` (the replica is at capacity) — is worth retrying:
/// the next attempt may land on a different replica or after the
/// conflicting state has cleared.
fn classify_open_failure(status: Status) -> OpenFailure {
    match status.code() {
        Code::FailedPrecondition | Code::PermissionDenied => OpenFailure::Permanent(status),
        _ => OpenFailure::Retryable(status),
    }
}

/// Is this status one the store will return identically on every future
/// open? Permanent codes, or any status the store explicitly classed
/// via `x-rio-log-reject` (the metadata is only ever attached to
/// permanent rejections).
fn is_permanent_rejection(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::FailedPrecondition | Code::PermissionDenied
    ) || status
        .metadata()
        .get(rio_proto::LOG_REJECT_METADATA_KEY)
        .is_some()
}

/// Map a permanent store rejection onto its [`AbandonReason`]: the
/// `x-rio-log-reject` class when present (`cap`/`complete`/
/// `superseded`), else the bare-code fallback for pre-metadata stores —
/// `FAILED_PRECONDITION` was historically the completeness seal,
/// `PERMISSION_DENIED` the supersession.
fn abandon_reason_for_rejection(status: &Status) -> AbandonReason {
    match status
        .metadata()
        .get(rio_proto::LOG_REJECT_METADATA_KEY)
        .and_then(|v| v.to_str().ok())
    {
        Some("cap") => AbandonReason::CapExhausted,
        Some("complete") => AbandonReason::CompleteLog,
        Some("superseded") => AbandonReason::Superseded,
        // An unknown class is still a permanent rejection; Superseded's
        // disposition (count the loss) is the conservative one.
        Some(_) => AbandonReason::Superseded,
        None => match status.code() {
            Code::FailedPrecondition => AbandonReason::CompleteLog,
            _ => AbandonReason::Superseded,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::transport::Server;
    use tonic::{Request, Response, Status, Streaming};

    use rio_proto::store::append_log_request::Msg;
    use rio_proto::store::log_service_server::{LogService, LogServiceServer};
    use rio_proto::store::{AppendLogAck, AppendLogRequest, TailLogChunk, TailLogRequest};
    use rio_proto::types::BuildLogBatch;
    use rio_test_support::grpc::spawn_grpc_server;
    use rio_test_support::metrics::CountingRecorder;

    use super::{AbandonReason, DrainStatus, LogUploader, LogUploaderConfig};

    /// A permanent rejection carrying the store's `x-rio-log-reject`
    /// class, exactly as `gate::reject_permanent` constructs it.
    fn rejection_with_class(class: &'static str, msg: &str) -> Status {
        let mut status = Status::failed_precondition(msg.to_string());
        status.metadata_mut().insert(
            rio_proto::LOG_REJECT_METADATA_KEY,
            tonic::metadata::MetadataValue::from_static(class),
        );
        status
    }

    // ------------------------------------------------------------------
    // The mock LogService
    // ------------------------------------------------------------------

    /// One `AppendLog` stream the mock accepted: every request message it
    /// received (in order) and the handle the test uses to ack or close it.
    struct MockSession {
        messages: Mutex<Vec<AppendLogRequest>>,
        /// `Some` until the test closes the session; sending on it pushes an
        /// ack onto the response stream.
        ack_tx: Mutex<Option<mpsc::Sender<Result<AppendLogAck, Status>>>>,
    }

    impl MockSession {
        fn message_count(&self) -> usize {
            self.messages.lock().unwrap().len()
        }

        /// The received messages, decomposed: `(header_count, batches)`.
        /// Batches are returned in arrival order.
        fn split(&self) -> (usize, Vec<BuildLogBatch>) {
            let msgs = self.messages.lock().unwrap();
            let mut headers = 0;
            let mut batches = Vec::new();
            for m in msgs.iter() {
                match &m.msg {
                    Some(Msg::Header(_)) => headers += 1,
                    Some(Msg::Batch(b)) => batches.push(b.clone()),
                    None => panic!("mock received an empty AppendLogRequest"),
                }
            }
            (headers, batches)
        }

        /// True iff the first received message is the header.
        fn header_first(&self) -> bool {
            matches!(
                self.messages.lock().unwrap().first(),
                Some(AppendLogRequest {
                    msg: Some(Msg::Header(_))
                })
            )
        }

        /// Push an ack onto the response stream.
        async fn ack(&self, durable_through_line: u64) {
            let tx = self
                .ack_tx
                .lock()
                .unwrap()
                .clone()
                .expect("acking a closed session");
            tx.send(Ok(AppendLogAck {
                durable_through_line,
            }))
            .await
            .expect("ack send");
        }

        /// End the response stream (the client sees `Ok(None)` on its ack
        /// stream — the server went away).
        fn close(&self) {
            self.ack_tx.lock().unwrap().take();
        }

        /// Fail the ack stream with `status` (the client sees
        /// `Err(status)` mid-stream — the shape of a cap trip, the
        /// completeness seal landing, or a supersession arriving while
        /// the stream is open).
        async fn fail(&self, status: Status) {
            let tx = self
                .ack_tx
                .lock()
                .unwrap()
                .clone()
                .expect("failing a closed session");
            tx.send(Err(status)).await.expect("fail send");
        }
    }

    /// Scriptable mock `LogService`. Records every session in order; the
    /// test inspects/acks/closes sessions by index.
    #[derive(Clone, Default)]
    struct MockLogService {
        inner: Arc<MockInner>,
    }

    #[derive(Default)]
    struct MockInner {
        /// When true, `append_log` reads the header then withholds the
        /// response headers forever (the hung-open shape of
        /// merged_bug_181).
        hang_open: std::sync::atomic::AtomicBool,
        sessions: Mutex<Vec<Arc<MockSession>>>,
        /// If `Some`, every `append_log` open is rejected with this status
        /// (cloned). The open never produces a session record.
        reject_open: Mutex<Option<Status>>,
        /// Number of opens attempted (including rejected ones).
        opens: Mutex<usize>,
    }

    impl MockLogService {
        fn reject_opens_with(&self, status: Status) {
            *self.inner.reject_open.lock().unwrap() = Some(status);
        }

        fn hang_opens(&self) {
            self.inner
                .hang_open
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn session(&self, idx: usize) -> Arc<MockSession> {
            self.inner
                .sessions
                .lock()
                .unwrap()
                .get(idx)
                .cloned()
                .unwrap_or_else(|| panic!("mock has no session #{idx}"))
        }

        fn session_count(&self) -> usize {
            self.inner.sessions.lock().unwrap().len()
        }

        fn open_count(&self) -> usize {
            *self.inner.opens.lock().unwrap()
        }
    }

    #[tonic::async_trait]
    impl LogService for MockLogService {
        type AppendLogStream = ReceiverStream<Result<AppendLogAck, Status>>;
        type TailLogStream = ReceiverStream<Result<TailLogChunk, Status>>;

        async fn append_log(
            &self,
            request: Request<Streaming<AppendLogRequest>>,
        ) -> Result<Response<Self::AppendLogStream>, Status> {
            *self.inner.opens.lock().unwrap() += 1;
            if let Some(status) = self.inner.reject_open.lock().unwrap().clone() {
                return Err(status);
            }

            // Mirror the real server's open contract: read the first
            // message (the header) before returning response headers, so a
            // client that awaits the call before buffering the header
            // deadlocks against this mock exactly as it would against the
            // real store.
            let mut inbound = request.into_inner();
            let first = inbound
                .message()
                .await?
                .ok_or_else(|| Status::invalid_argument("stream ended before the header"))?;

            if self
                .inner
                .hang_open
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                // Withhold response headers forever: the open is
                // accepted at the transport level but never answered.
                std::future::pending::<()>().await;
            }

            let (ack_tx, ack_rx) = mpsc::channel::<Result<AppendLogAck, Status>>(16);
            let session = Arc::new(MockSession {
                messages: Mutex::new(vec![first]),
                ack_tx: Mutex::new(Some(ack_tx)),
            });
            self.inner
                .sessions
                .lock()
                .unwrap()
                .push(Arc::clone(&session));

            // Reader task: append every subsequent message to the session
            // record. Exits when the client half-closes or the transport
            // drops.
            tokio::spawn(async move {
                while let Ok(Some(msg)) = inbound.message().await {
                    session.messages.lock().unwrap().push(msg);
                }
            });

            Ok(Response::new(ReceiverStream::new(ack_rx)))
        }

        async fn tail_log(
            &self,
            _request: Request<TailLogRequest>,
        ) -> Result<Response<Self::TailLogStream>, Status> {
            Err(Status::unimplemented("mock"))
        }
    }

    // ------------------------------------------------------------------
    // Harness
    // ------------------------------------------------------------------

    struct Harness {
        mock: MockLogService,
        uploader: LogUploader,
        _server: tokio::task::JoinHandle<()>,
    }

    /// Test-scale timings: a 50 ms reconnect backoff and a 400 ms drain
    /// deadline keep the slowest test under a second of wall clock while
    /// preserving the ordering relationships the production constants have
    /// (backoff < grace < deadline).
    fn test_config() -> LogUploaderConfig {
        LogUploaderConfig {
            reconnect_backoff: Duration::from_millis(50),
            drain_deadline: Duration::from_millis(400),
        }
    }

    async fn harness() -> Harness {
        harness_with(test_config()).await
    }

    async fn harness_with(config: LogUploaderConfig) -> Harness {
        let mock = MockLogService::default();
        let router = Server::builder().add_service(LogServiceServer::new(mock.clone()));
        let (addr, server) = spawn_grpc_server(router).await;
        let client = super::LogServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("connect to the mock LogService");
        let uploader = LogUploader::spawn_with_config(
            client,
            "01900000-0000-7000-8000-000000000001".to_string(),
            "/nix/store/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-test.drv".to_string(),
            // An empty token exercises the "dev mode / no token" arm of
            // attach_assignment_token; the token plumbing itself is
            // byte-identical to the NAR upload path's and is covered there.
            String::new(),
            config,
        );
        Harness {
            mock,
            uploader,
            _server: server,
        }
    }

    fn batch(first_line: u64, n_lines: usize) -> BuildLogBatch {
        BuildLogBatch {
            derivation_path: String::new(),
            lines: (0..n_lines)
                .map(|i| format!("line-{:05}", first_line + i as u64).into_bytes())
                .collect(),
            first_line_number: first_line,
            executor_id: String::new(),
        }
    }

    /// Poll `cond` every 10 ms until it returns true or ~30 s elapse.
    /// The established shape for "wait for a fire-and-forget task to land"
    /// in this workspace's test suites. The budget is deliberately wide:
    /// green runs return in milliseconds (the poll exits on the first
    /// true), and the panic-unwind + Drop disclosure path has timed out
    /// at a 2 s budget under full-gate builder contention (documented
    /// wall-clock-under-load flake class) — the wide bound buys tail
    /// headroom without slowing anything that works.
    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..3000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    // ------------------------------------------------------------------
    // 1. The open contract and message ordering
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn sends_header_then_batches() {
        let h = harness().await;
        let tx = h.uploader.sender();
        tx.send(batch(0, 2)).await.unwrap();
        tx.send(batch(2, 1)).await.unwrap();

        wait_for("the mock to receive 3 messages", || {
            h.mock.session_count() == 1 && h.mock.session(0).message_count() == 3
        })
        .await;

        let session = h.mock.session(0);
        assert!(
            session.header_first(),
            "the first message on the stream must be the AppendLogHeader"
        );
        let (headers, batches) = session.split();
        assert_eq!(headers, 1, "exactly one header per session");
        assert_eq!(
            batches
                .iter()
                .map(|b| b.first_line_number)
                .collect::<Vec<_>>(),
            vec![0, 2],
            "batches arrive in send order"
        );
    }

    // ------------------------------------------------------------------
    // 2. Ack trimming
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn trims_buffer_on_ack() {
        let h = harness().await;
        let tx = h.uploader.sender();
        // Two frames: lines 0..=99 and 100..=149.
        tx.send(batch(0, 100)).await.unwrap();
        tx.send(batch(100, 50)).await.unwrap();
        wait_for("the mock to receive both batches", || {
            h.mock.session_count() == 1 && h.mock.session(0).message_count() == 3
        })
        .await;

        let progress = h.uploader.progress();
        assert_eq!(
            progress.borrow().unacked_lines,
            150,
            "everything is unacked before the first ack"
        );

        // Ack line 99: the first frame (ending at 99) is trimmed, the
        // second (ending at 149) is retained.
        h.mock.session(0).ack(99).await;
        wait_for("the ack to trim the first frame", || {
            h.uploader.progress().borrow().unacked_lines == 50
        })
        .await;
        assert_eq!(h.uploader.progress().borrow().last_acked_line, Some(99));
    }

    // ------------------------------------------------------------------
    // 3. Reconnect + replay
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn replays_unacked_tail_on_reconnect() {
        let h = harness().await;
        let tx = h.uploader.sender();
        // Two frames: 0..=49 and 50..=99.
        tx.send(batch(0, 50)).await.unwrap();
        tx.send(batch(50, 50)).await.unwrap();
        wait_for("session 0 to receive both batches", || {
            h.mock.session_count() == 1 && h.mock.session(0).message_count() == 3
        })
        .await;

        // Ack the first frame, then kill the stream.
        h.mock.session(0).ack(49).await;
        wait_for("the ack to land", || {
            h.uploader.progress().borrow().last_acked_line == Some(49)
        })
        .await;
        h.mock.session(0).close();

        // The uploader reconnects (a fresh header) and replays only the
        // un-acked tail: the frame starting at line 50. Not line 0 (that
        // frame was acked durable) and not nothing.
        wait_for("a second session with the replayed tail", || {
            h.mock.session_count() == 2 && h.mock.session(1).message_count() >= 2
        })
        .await;
        let session = h.mock.session(1);
        assert!(session.header_first(), "the reconnect re-sends the header");
        let (headers, batches) = session.split();
        assert_eq!(headers, 1);
        assert_eq!(
            batches
                .iter()
                .map(|b| b.first_line_number)
                .collect::<Vec<_>>(),
            vec![50],
            "only the un-acked tail is replayed"
        );
    }

    // ------------------------------------------------------------------
    // 4. Detach-and-drain
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn detaches_and_drains_after_completion() {
        // A long drain deadline so the detached task is still trying when
        // the delayed ack finally arrives.
        let h = harness_with(LogUploaderConfig {
            reconnect_backoff: Duration::from_millis(50),
            drain_deadline: Duration::from_secs(30),
        })
        .await;
        let tx = h.uploader.sender();
        tx.send(batch(0, 100)).await.unwrap();
        wait_for("the mock to receive the batch", || {
            h.mock.session_count() == 1 && h.mock.session(0).message_count() == 2
        })
        .await;

        // Watch the task's progress across the detach.
        let progress = h.uploader.progress();

        // The mock has NOT acked. finish() must come back at the grace
        // timeout with the task still running, not block until the ack.
        let started = std::time::Instant::now();
        drop(tx);
        let status = h.uploader.finish(Duration::from_millis(200)).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "finish() must return at the grace timeout, not wait for the ack \
             (took {:?})",
            started.elapsed()
        );
        match status {
            DrainStatus::Detached { unacked_lines, .. } => {
                assert_eq!(unacked_lines, 100, "nothing was acked before the detach")
            }
            other => panic!("expected Detached at the grace timeout, got {other:?}"),
        }

        // The detached task is still running: the delayed ack drains it.
        h.mock.session(0).ack(99).await;
        wait_for("the detached task to drain and exit", || {
            let p = *progress.borrow();
            p.done && p.unacked_lines == 0
        })
        .await;
    }

    // ------------------------------------------------------------------
    // 5. The drain deadline
    // ------------------------------------------------------------------

    /// merged_bug_181 (red-first): the store accepts the AppendLog
    /// connection but withholds response headers — `open()` hangs. The
    /// drain deadline must still fire (third select arm in
    /// await_while_buffering) and the task must exit Abandoned with the
    /// loss counted, instead of parking forever.
    #[tokio::test]
    async fn hung_open_abandons_at_drain_deadline() {
        let h = harness().await;
        h.mock.hang_opens();
        let tx = h.uploader.sender();
        tx.send(batch(0, 25)).await.unwrap();
        wait_for("the hung open to be attempted", || h.mock.open_count() >= 1).await;

        let started = std::time::Instant::now();
        drop(tx); // input closes → the 400 ms drain deadline arms
        let status = h.uploader.finish(Duration::from_secs(5)).await;
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the drain must abandon at the deadline while open() is awaited, \
             not park forever (took {:?})",
            started.elapsed()
        );
        match status {
            DrainStatus::Abandoned {
                unacked_lines,
                last_acked_line,
                reason,
            } => {
                assert_eq!(unacked_lines, 25, "every line is un-acked loss");
                assert_eq!(last_acked_line, None);
                assert_eq!(
                    reason,
                    AbandonReason::DeadlineExpired,
                    "a hung open is deadline loss, not a store rejection"
                );
            }
            other => panic!("expected Abandoned at the deadline, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gives_up_after_drain_deadline() {
        // 400 ms deadline, and the mock never acks.
        let h = harness().await;
        let tx = h.uploader.sender();
        tx.send(batch(0, 10)).await.unwrap();
        wait_for("the mock to receive the batch", || {
            h.mock.session_count() == 1 && h.mock.session(0).message_count() == 2
        })
        .await;

        let progress = h.uploader.progress();
        drop(tx);
        // A grace longer than the deadline: finish() observes the task give
        // up rather than detaching from it.
        let status = h.uploader.finish(Duration::from_secs(5)).await;
        match status {
            DrainStatus::Abandoned {
                unacked_lines,
                last_acked_line,
                reason,
            } => {
                assert_eq!(unacked_lines, 10);
                assert_eq!(last_acked_line, None);
                assert_eq!(
                    reason,
                    AbandonReason::DeadlineExpired,
                    "a deadline expiry is real data loss — the case that fires \
                     rio_builder_log_drain_abandoned_total{{reason=deadline_expired}}"
                );
            }
            other => panic!("expected Abandoned after the drain deadline, got {other:?}"),
        }
        assert!(progress.borrow().done, "the task exited");
    }

    // ------------------------------------------------------------------
    // 6. Permanent open-time rejection
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn permanent_rejection_stops_retrying() {
        let h = harness().await;
        h.mock
            .reject_opens_with(Status::failed_precondition("log already complete"));

        let tx = h.uploader.sender();
        tx.send(batch(0, 1)).await.unwrap();

        // The uploader attempts the open, gets FAILED_PRECONDITION, and
        // stops. Give it a few backoff intervals' worth of wall clock to
        // prove it is not retrying.
        wait_for("the first (rejected) open", || h.mock.open_count() >= 1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            h.mock.open_count(),
            1,
            "a permanent rejection must not be retried"
        );
        assert_eq!(h.mock.session_count(), 0, "no session was established");

        // The input channel keeps accepting (and discarding) batches
        // without blocking — the stderr loop must never wedge on a build
        // whose log the store will not take. 256 sends would block forever
        // on a 256-cap channel if nothing were draining it.
        for i in 0..512u64 {
            tokio::time::timeout(Duration::from_secs(1), tx.send(batch(i + 1, 1)))
                .await
                .expect("send into a permanently-rejected uploader must not block")
                .expect("the input channel must stay open");
        }

        drop(tx);
        let status = h.uploader.finish(Duration::from_secs(5)).await;
        // A bare FAILED_PRECONDITION (a pre-metadata store's
        // completeness seal) maps to CompleteLog: the store provably
        // holds the finished log, so `lost_lines` is 0 and the loss
        // counter stays silent — a routine late replay must be
        // distinguishable from real abandonment, or every such race
        // pages an operator.
        assert!(
            matches!(
                status,
                DrainStatus::Abandoned {
                    reason: AbandonReason::CompleteLog,
                    ..
                }
            ),
            "a bare FAILED_PRECONDITION rejection reports CompleteLog, got {status:?}"
        );
    }

    // ------------------------------------------------------------------
    // 7. Loss disclosure (merged_bug_360, bug_248,
    //    builder.log.loss-disclosure)
    // ------------------------------------------------------------------

    // r[verify builder.log.loss-disclosure]
    /// merged_bug_360 (red-first): PERMISSION_DENIED with un-acked lines
    /// is durable loss — the superseding attempt produces ITS OWN log,
    /// not these lines. Pre-fix, `rejected: true` suppressed the
    /// counter for every permanent rejection alike; the recorded red
    /// was `reason=superseded` count 0.
    #[tokio::test]
    async fn permission_denied_with_unacked_fires_loss_counter() {
        let rec = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let h = harness().await;
        h.mock
            .reject_opens_with(Status::permission_denied("superseded"));
        let tx = h.uploader.sender();
        tx.send(batch(0, 9)).await.unwrap();
        wait_for("the rejected open", || h.mock.open_count() >= 1).await;

        drop(tx);
        let status = h.uploader.finish(Duration::from_secs(5)).await;
        assert!(
            matches!(
                status,
                DrainStatus::Abandoned {
                    reason: AbandonReason::Superseded,
                    unacked_lines: 9,
                    ..
                }
            ),
            "got {status:?}"
        );
        assert_eq!(
            rec.get("rio_builder_log_drain_abandoned_total{reason=superseded}"),
            1,
            "9 un-acked lines died with the superseded execution and must \
             be disclosed (saw keys: {:?})",
            rec.all_keys()
        );
    }

    // r[verify builder.log.loss-disclosure]
    /// merged_bug_360 (red-first): a panic in the upload task must
    /// disclose the un-acked lines. Pre-fix the counter fired only at
    /// `run()`'s normal exit, which a panicking task never reaches —
    /// the recorded red was `reason=panic` count 0. The panic seam is
    /// real arithmetic: a batch at `u64::MAX` overflows the trim's
    /// last-line computation in debug builds.
    #[tokio::test]
    async fn panic_in_upload_task_fires_counter() {
        let rec = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let h = harness().await;
        let tx = h.uploader.sender();
        // first_line_number = u64::MAX with 2 lines: the ack-trim
        // computes MAX + 1 and panics (debug overflow) inside the task.
        // Built inline — the batch() helper's line formatting would
        // overflow in the TEST thread instead.
        tx.send(BuildLogBatch {
            derivation_path: String::new(),
            lines: vec![b"a".to_vec(), b"b".to_vec()],
            first_line_number: u64::MAX,
            executor_id: String::new(),
        })
        .await
        .unwrap();
        wait_for("the batch to reach the mock", || {
            h.mock.session_count() == 1 && h.mock.session(0).message_count() == 2
        })
        .await;
        h.mock.session(0).ack(0).await;

        wait_for("the LossGuard to disclose the panic", || {
            rec.get("rio_builder_log_drain_abandoned_total{reason=panic}") == 1
        })
        .await;

        drop(tx);
        let status = h.uploader.finish(Duration::from_secs(5)).await;
        assert!(
            matches!(
                status,
                DrainStatus::Abandoned {
                    reason: AbandonReason::Panicked,
                    ..
                }
            ),
            "got {status:?}"
        );
        assert_eq!(
            rec.get("rio_builder_log_drain_abandoned_total{reason=panic}"),
            1,
            "the disclosure fires exactly once (the guard, not finish())"
        );
    }

    // r[verify builder.log.loss-disclosure]
    /// bug_248 (red-first): a permanent rejection arriving MID-STREAM
    /// (the cap tripping while the stream is open) must stop the
    /// session loop — pre-fix it was classified Reconnect and the
    /// uploader re-opened against the identical rejection at 1 Hz until
    /// the drain deadline (the recorded red: open_count kept growing).
    #[tokio::test]
    async fn midstream_cap_routes_to_permanent() {
        let rec = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let h = harness().await;
        let tx = h.uploader.sender();
        tx.send(batch(0, 4)).await.unwrap();
        wait_for("the open to land", || {
            h.mock.session_count() == 1 && h.mock.session(0).message_count() == 2
        })
        .await;

        // The cap trips mid-stream.
        h.mock
            .session(0)
            .fail(rejection_with_class("cap", "per-execution byte cap"))
            .await;

        drop(tx);
        let status = h.uploader.finish(Duration::from_secs(5)).await;
        assert!(
            matches!(
                status,
                DrainStatus::Abandoned {
                    reason: AbandonReason::CapExhausted,
                    ..
                }
            ),
            "got {status:?}"
        );
        assert_eq!(
            h.mock.open_count(),
            1,
            "a mid-stream permanent rejection must not trigger a single \
             reconnect — pre-fix this was a 1 Hz open storm"
        );
        assert_eq!(
            rec.get("rio_builder_log_drain_abandoned_total{reason=cap_exhausted}"),
            1,
            "capped overflow is discarded by design but still disclosed"
        );
    }

    // r[verify builder.log.loss-disclosure]
    /// Polarity guard: a `complete` rejection (the store provably holds
    /// the full `[0, final)` log) is the ONE zero-loss abandon — no
    /// counter, under any reason label.
    #[tokio::test]
    async fn complete_log_rejection_stays_silent() {
        let rec = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let h = harness().await;
        h.mock
            .reject_opens_with(rejection_with_class("complete", "log already complete"));
        let tx = h.uploader.sender();
        tx.send(batch(0, 3)).await.unwrap();
        wait_for("the rejected open", || h.mock.open_count() >= 1).await;

        drop(tx);
        let status = h.uploader.finish(Duration::from_secs(5)).await;
        assert!(
            matches!(
                status,
                DrainStatus::Abandoned {
                    reason: AbandonReason::CompleteLog,
                    ..
                }
            ),
            "got {status:?}"
        );
        for label in [
            "deadline_expired",
            "complete_log",
            "superseded",
            "cap_exhausted",
            "panic",
        ] {
            assert_eq!(
                rec.get(&format!(
                    "rio_builder_log_drain_abandoned_total{{reason={label}}}"
                )),
                0,
                "a complete-log replay loses nothing; the counter must stay \
                 silent (label {label}; saw keys: {:?})",
                rec.all_keys()
            );
        }
    }
}
