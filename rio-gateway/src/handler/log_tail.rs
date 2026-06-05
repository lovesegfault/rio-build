//! Per-derivation live-tail subscriptions to rio-store's
//! `LogService.TailLog`.
//!
//! The gateway used to receive build-log lines as `Event::Log` items on
//! the scheduler's `BuildEvent` stream. The log data plane has moved to
//! rio-store: builders stream lines to the store's `AppendLog`, and the
//! gateway pulls them back out by opening one `TailLog(follow: true)`
//! subscription per *building derivation* of a watched build. The
//! subscription tasks feed a single per-build output channel; the
//! build's event loop relays each chunk to the nix client through the
//! same `relay_log_batch` path the `Event::Log` arm used.
//!
//! ## The subscription lifecycle
//!
//! Each rule below exists because the naive version loses or duplicates
//! lines (see `~/tmp/harden-logs/DESIGN.md` §5.3):
//!
//! - **Open on `Started`** at `since_line = 0`. The store serves
//!   history-then-live, so lines that arrived before the gateway
//!   subscribed are not lost. A `Started` with an empty `exec_id`
//!   (a node-vanished race upstream) gets no subscription.
//! - **Re-open on premature end** with `since_line = last_relayed + 1`
//!   after a backoff. Store deploys, replica crashes, and proxy
//!   failures all close the stream; the *client* owns re-subscription.
//!   The store's chunk granularity means the re-opened stream may
//!   resend lines below the cursor — they are trimmed here so the nix
//!   client never sees a line twice.
//! - **Replace on re-dispatch**: a second `Started` with a *different*
//!   exec_id hard-cancels the old subscription (its execution is dead)
//!   and opens a fresh one at `since_line = 0`.
//! - **Drain on terminal**: the per-derivation `Completed`/`Failed`
//!   event does NOT cancel the subscription — the terminal event (via
//!   the scheduler) and the final log lines (via the store) travel on
//!   different network paths, and cancelling on terminal races away
//!   the build error. The task stops *re-opening* and lets the current
//!   stream drain to its natural end, capped at a post-terminal grace.
//! - **Hard-cancel at build terminus** (`LogTailSet::abort_all`).

use std::collections::HashMap;
use std::time::Duration;

use rio_common::transport::{OpenOutcome, bounded_open};
use rio_log_kernel::{ChunkVisit, TailNext, TailStopCause, tail_next, visit_chunk};
use rio_proto::LogServiceClient;
use rio_proto::store::{TailLogChunk, TailLogRequest};
use rio_proto::types;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tonic::transport::Channel;
use tracing::{Instrument, debug, info_span, warn};

/// How long a subscription waits before re-opening a prematurely-ended
/// stream. The store's failure modes here are restart/deploy shaped
/// (the replica serving the stream went away), not congestion shaped,
/// so a fixed backoff is enough; the scheduler-stream reconnect's
/// exponential ladder would just delay the live tail's recovery.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Bound on the `TailLog` *open* itself. A half-open store replica
/// (TCP up, HTTP/2 dead) used to park the subscription in the open
/// await forever — invisible to the drain signal and to the grace
/// clock. The open is raced (via
/// [`rio_common::transport::bounded_open`]) against the drain edge and
/// this bound; `TimedOut` maps to `OpenFailed` and the exit law
/// decides, exactly as for an answered open error.
const TAIL_OPEN_BOUND: Duration = Duration::from_secs(10);

/// How long a subscription keeps draining its current stream after the
/// derivation goes terminal. The builder finishes its log upload (with
/// its own 2 s grace) *before* reporting completion to the scheduler,
/// so by the time the terminal event reaches the gateway the store
/// almost always already holds — and has usually already served — the
/// final lines; this grace covers the tail of that race. Matches the
/// house style for bounded teardown waits.
const TERMINAL_GRACE: Duration = Duration::from_secs(2);

/// Per-build output queue depth. A slow nix client fills the channel
/// and backpressures the subscription tasks' `send().await`, which is
/// correct — the store-side per-subscriber queue sheds load if the
/// gateway itself stops reading, and the gateway's own channel applies
/// backpressure to its own reads.
const OUT_QUEUE_DEPTH: usize = 256;

/// Tuning knobs for [`LogTailSet`], overridable in tests so the
/// grace/backoff tests don't take wall-clock seconds.
#[derive(Clone, Copy, Debug)]
pub(super) struct LogTailConfig {
    pub reconnect_backoff: Duration,
    pub terminal_grace: Duration,
    /// Hard bound on one `TailLog` OPEN await (`TAIL_OPEN_BOUND` in
    /// production). Test-overridable so the hung-open conformance test
    /// does not wall-clock 10 s per cut.
    pub open_bound: Duration,
}

impl Default for LogTailConfig {
    fn default() -> Self {
        Self {
            reconnect_backoff: RECONNECT_BACKOFF,
            terminal_grace: TERMINAL_GRACE,
            open_bound: TAIL_OPEN_BOUND,
        }
    }
}

/// One chunk of log lines from a `TailLog` subscription, tagged with
/// the derivation it belongs to so the build's event loop can attach it
/// to the right `actBuild` activity.
#[derive(Debug)]
pub(super) struct TaggedLogChunk {
    pub derivation_path: String,
    pub first_line_number: u64,
    pub lines: Vec<Vec<u8>>,
}

impl TaggedLogChunk {
    /// Rebuild the `BuildLogBatch` shape `relay_log_batch` consumes.
    /// `executor_id` is debugging metadata the relay never reads.
    pub(super) fn into_batch(self) -> types::BuildLogBatch {
        types::BuildLogBatch {
            derivation_path: self.derivation_path,
            lines: self.lines,
            first_line_number: self.first_line_number,
            executor_id: String::new(),
        }
    }
}

/// One live subscription: the execution it is keyed on, the signal that
/// flips it from "re-open forever" to "drain and exit", and the task
/// handle for the hard-cancel paths.
struct TailHandle {
    exec_id: String,
    drain: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// All live-tail subscriptions for one watched build.
///
/// Owned by `submit_and_process_build` alongside the activity state; it
/// survives scheduler-stream reconnects (the subscriptions are
/// independent of the scheduler connection). Holds a clone of the
/// output sender so the receiver in the event loop can never observe
/// `None` while the set is alive.
pub(super) struct LogTailSet {
    client: LogServiceClient<Channel>,
    /// The watching caller's session tenant token, forwarded on every
    /// `TailLog` open (the store enforces tenant ownership; bug_290).
    /// Snapshot semantics match `with_jwt` on the scheduler stream: a
    /// token that expires mid-build degrades the live tail (opens get
    /// UNAUTHENTICATED and the reconnect loop keeps retrying) without
    /// affecting the build itself.
    jwt_token: Option<String>,
    out_tx: mpsc::Sender<TaggedLogChunk>,
    config: LogTailConfig,
    tasks: HashMap<String, TailHandle>,
}

impl LogTailSet {
    /// Create the set and its output channel. The receiver goes to the
    /// build's event loop; the set keeps a sender clone.
    pub(super) fn new(
        client: LogServiceClient<Channel>,
        jwt_token: Option<String>,
    ) -> (Self, mpsc::Receiver<TaggedLogChunk>) {
        Self::with_config(client, jwt_token, LogTailConfig::default())
    }

    pub(super) fn with_config(
        client: LogServiceClient<Channel>,
        jwt_token: Option<String>,
        config: LogTailConfig,
    ) -> (Self, mpsc::Receiver<TaggedLogChunk>) {
        let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE_DEPTH);
        (
            Self {
                client,
                jwt_token,
                out_tx,
                config,
                tasks: HashMap::new(),
            },
            out_rx,
        )
    }

    /// Test-only visibility: which derivations currently hold a live
    /// tail subscription. Lets sibling-module tests assert the
    /// kind-routing contract (materialization running entries must not
    /// open tails) without reaching into the task map.
    #[cfg(test)]
    pub(super) fn tracked_drvs(&self) -> Vec<String> {
        self.tasks.keys().cloned().collect()
    }

    /// `DerivationEvent::Started` arrived for `derivation_path`.
    ///
    /// - Empty `exec_id` → no subscription (the field is documented as
    ///   possibly empty on an unreachable node-vanished race; without
    ///   an execution there is no log to tail).
    /// - No existing subscription → open one at `since_line = 0`.
    /// - Existing subscription with the *same* exec_id → duplicate
    ///   `Started` (scheduler-stream replay); keep it.
    /// - Existing subscription with a *different* exec_id → the
    ///   derivation was re-dispatched; the old execution's log is dead.
    ///   Hard-cancel and re-open at `since_line = 0` for the new one.
    pub(super) fn on_started(&mut self, derivation_path: &str, exec_id: &str) {
        if exec_id.is_empty() {
            return;
        }
        if let Some(existing) = self.tasks.get(derivation_path) {
            if existing.exec_id == exec_id {
                return;
            }
            debug!(
                drv = %derivation_path,
                old_exec = %existing.exec_id,
                new_exec = %exec_id,
                "re-dispatch: replacing log-tail subscription"
            );
            if let Some(old) = self.tasks.remove(derivation_path) {
                old.task.abort();
            }
        }
        let (drain_tx, drain_rx) = watch::channel(false);
        let span = info_span!(
            "log_tail",
            drv = %derivation_path,
            exec_id = %exec_id,
        );
        let task = tokio::spawn(
            run_tail(
                self.client.clone(),
                self.jwt_token.clone(),
                derivation_path.to_string(),
                exec_id.to_string(),
                self.out_tx.clone(),
                drain_rx,
                self.config,
            )
            .instrument(span),
        );
        self.tasks.insert(
            derivation_path.to_string(),
            TailHandle {
                exec_id: exec_id.to_string(),
                drain: drain_tx,
                task,
            },
        );
    }

    /// The derivation went terminal (`Completed`/`Failed`). Stop the
    /// subscription from re-opening and let its current stream drain.
    /// The entry stays in the map so a later `Started` with a new
    /// exec_id is still recognised as a replacement.
    pub(super) fn on_terminal(&mut self, derivation_path: &str) {
        if let Some(handle) = self.tasks.get(derivation_path) {
            // send_replace never fails (we hold the receiver's peer
            // inside the task); if the task already exited the value
            // is simply never read.
            let _ = handle.drain.send_replace(true);
        }
    }

    /// Build terminus: hard-cancel everything still running. Chunks
    /// already delivered to the output channel are unaffected (the
    /// caller drains that channel before dropping it).
    pub(super) fn abort_all(&mut self) {
        for (_, handle) in self.tasks.drain() {
            handle.task.abort();
        }
    }
}

/// Drop-safety chokepoint (merged_bug_130): EVERY drop path of the set
/// — the session loop's early returns, error exits, panics — aborts
/// every subscription, without enumerating the callers. The kernel's
/// `Orphaned` exit is the defense-in-depth twin for any relay whose
/// abort has not landed yet (or any future ownership shape that drops
/// the drain sender while a task lives): belt at the owner, suspenders
/// in the law.
impl Drop for LogTailSet {
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// Why one driven `TailLog` stream stopped yielding, as observed by
/// [`drive_stream`]. The kernel's [`tail_next`] — not this enum —
/// decides whether the subscription re-opens or exits.
enum DriveEnd {
    /// The stream stopped; `tail_next(cause, ..)` decides what happens.
    Ended(TailStopCause),
    /// The stream jumped past the relay floor and the gap has not had
    /// its one re-open chance yet. The sliced lines are WITHHELD in
    /// the caller's [`PendingGap`] (merged_bug_150: dropping them made
    /// "exit at the grace edge" silently lose fetched lines); nothing
    /// was relayed or advanced; the caller re-opens at the unchanged
    /// floor.
    Gap,
    /// The output channel's receiver is gone — the build's event loop
    /// has exited. Nothing left to relay to.
    OutputClosed,
}

/// A forward jump awaiting its one re-open chance, WITH the lines that
/// arrived past it (merged_bug_150). The pre-fix shape kept only
/// `gap_from` and dropped the chunk — so any exit between the first
/// sighting and the second (grace edge, orphan, terminal-complete)
/// lost both the fetched lines AND the marker. Holding the sliced
/// chunk makes the exit flush total: [`flush_pending_gap`] is the
/// only way out of this state, and every `run_tail` exit path runs it.
struct PendingGap {
    /// First missing line (== the relay floor at sighting time).
    gap_from: u64,
    /// One past the last missing line (the withheld chunk's first).
    gap_until: u64,
    /// The already-sliced lines past the gap, ready to relay.
    withheld: TaggedLogChunk,
    /// The relay watermark to adopt once the withheld lines flush.
    next_line: u64,
}

// r[impl store.log.tail-reconnect]
// r[impl store.log.tail-grace-drain+2]
/// One subscription's lifetime: open → drive → (backoff → re-open)*,
/// with the exit decision delegated to the kernel's [`tail_next`] law:
/// exit only when the post-terminal grace expired, the relay is
/// orphaned (the set's drain sender vanished — no consumer remains),
/// or the stream ended naturally with the derivation terminal and the
/// served log complete.
async fn run_tail(
    mut client: LogServiceClient<Channel>,
    jwt_token: Option<String>,
    derivation_path: String,
    exec_id: String,
    out_tx: mpsc::Sender<TaggedLogChunk>,
    mut drain: watch::Receiver<bool>,
    config: LogTailConfig,
) {
    // The highest line number forwarded to the output channel. `None`
    // until the first line. Survives re-opens — it is the dedup floor
    // that makes the at-least-once store stream exactly-once on the
    // client's wire.
    let mut last_relayed: Option<u64> = None;
    // The most recent message's `is_complete` — at any stream end this
    // is the final message's claim, the store's own statement that
    // everything durable was served. Empty finals carry it too (they
    // are dropped for relay purposes AFTER this is recorded).
    let mut served_complete = false;
    // The post-terminal grace deadline, armed exactly ONCE at the
    // first observation of the drain signal (any path: mid-stream,
    // between streams, during backoff). Re-arming on every stream end
    // would let a terminal subscription ride re-opens forever.
    let mut grace_deadline: Option<Instant> = None;
    // A forward jump observed on the previous stream, awaiting its one
    // re-open chance: the same `gap_from` seen again means the hole is
    // durable (the store re-served the same split) and is accepted
    // with an inline marker. Each distinct gap_from is retried exactly
    // once — the loop cannot ping-pong on one hole. The lines that
    // arrived past the jump ride along (merged_bug_150) so EVERY exit
    // path can flush them with the disclosure.
    let mut pending_gap: Option<PendingGap> = None;
    // Gaps at line numbers below this have already been disclosed with
    // a marker; never re-mark them.
    let mut accepted_gap_floor: Option<u64> = None;
    // Set once the first open-failure of a consecutive run has been
    // logged at `warn!`; reset on a successful open. Without the latch
    // a store that is down for a whole build would emit one warn per
    // second per derivation; without the warn at all, a fleet-wide
    // dead live-tail is indistinguishable from quiet builds at any log
    // level an operator actually runs. The reconnect *counter* below is
    // the alerting signal; the warn is the "which derivation / which
    // status code" breadcrumb next to it.
    let mut warned_open_failure = false;
    loop {
        // An orphaned relay must never open another stream: the drain
        // sender vanishing means the owning set is gone (and with it
        // every consumer), so the law exits unconditionally — proven
        // by `check_tail_next_orphan_always_exits`. Checked BEFORE the
        // open so a death observed during backoff costs zero further
        // store connections (merged_bug_130: this exact shape used to
        // skip every backoff and hot-loop opens at full speed).
        if drain.has_changed().is_err() {
            let verdict = tail_next(
                TailStopCause::Orphaned,
                *drain.borrow(),
                grace_deadline.is_some_and(|d| Instant::now() >= d),
                served_complete,
            );
            debug_assert_eq!(verdict, TailNext::Exit);
            debug!("log tail orphaned (subscription set gone); exiting");
            // Orphan exit still flushes (merged_bug_150): the send
            // fails harmlessly if the consumer is truly gone, but an
            // orphaned WATCH with a live OUTPUT channel must not eat
            // the withheld lines.
            flush_pending_gap(
                &mut pending_gap,
                &out_tx,
                &mut last_relayed,
                &mut accepted_gap_floor,
            )
            .await;
            return;
        }
        arm_grace(&mut grace_deadline, &drain, config.terminal_grace);
        let since_line = last_relayed.map_or(0, |n| n.saturating_add(1));
        let mut request = tonic::Request::new(TailLogRequest {
            derivation: derivation_path.clone(),
            exec_id: exec_id.clone(),
            since_line,
            follow: true,
        });
        // Forward the watching caller's tenant token — the store
        // verifies it and checks build-membership ownership
        // (bug_290; store.log.tail-ownership).
        if let Some(token) = jwt_token.as_deref()
            && let Ok(value) = token.parse()
        {
            request
                .metadata_mut()
                .insert(rio_proto::TENANT_TOKEN_HEADER, value);
        }
        // The open is the one await the drain signal and grace clock
        // cannot see: race it against the drain edge (a signal or
        // sender death mid-open aborts with zero stream consumed; the
        // re-check below reads the fresh watch state) and a hard bound.
        let open = bounded_open(
            async { _ = drain.changed().await },
            config.open_bound,
            client.tail_log(request),
        )
        .await;
        let cause = match open {
            OpenOutcome::Opened(Ok(resp)) => {
                warned_open_failure = false;
                match drive_stream(
                    resp.into_inner(),
                    &derivation_path,
                    &exec_id,
                    &out_tx,
                    &mut drain,
                    &mut last_relayed,
                    &mut served_complete,
                    &mut grace_deadline,
                    &mut pending_gap,
                    &mut accepted_gap_floor,
                    config.terminal_grace,
                    config.reconnect_backoff,
                )
                .await
                {
                    DriveEnd::OutputClosed => return,
                    DriveEnd::Ended(cause) => cause,
                    DriveEnd::Gap => {
                        // The jump (and its withheld lines) is already
                        // recorded in `pending_gap`; the very next
                        // stream gets one chance to serve the missing
                        // span before the gap is accepted and
                        // disclosed.
                        debug!(
                            gap_from = pending_gap.as_ref().map_or(0, |p| p.gap_from),
                            "TailLog stream jumped past the relay floor; re-opening at the gap"
                        );
                        TailStopCause::GapObserved
                    }
                }
            }
            OpenOutcome::Opened(Err(status)) => {
                // Open failed (store unreachable, NotFound because the
                // execution hasn't recorded anything yet, ...). All of
                // these are retryable from the live tail's perspective
                // — the lines are durable in the store regardless, and
                // a reader that gives up early just degrades to the
                // historical read path. After terminal the kernel keeps
                // retrying within the grace budget: the final lines may
                // land on a replica that is restarting right now.
                //
                // Deliberately NOT surfaced to the nix client: a
                // "log tail reconnecting" line in build output is
                // noise the user can't act on, and the lines are
                // durable in the store regardless.
                if warned_open_failure {
                    debug!(code = ?status.code(), "TailLog open failed");
                } else {
                    warned_open_failure = true;
                    warn!(
                        code = ?status.code(),
                        since_line,
                        "TailLog open failed; live tail degraded until the store is reachable \
                         (retrying every {:?})",
                        config.reconnect_backoff
                    );
                }
                TailStopCause::OpenFailed
            }
            OpenOutcome::TimedOut { after } => {
                // A half-open replica: TCP up, nobody home. Same
                // retryability as an answered open error — the lines
                // are durable in the store regardless.
                if warned_open_failure {
                    debug!(?after, "TailLog open timed out");
                } else {
                    warned_open_failure = true;
                    warn!(
                        ?after,
                        since_line,
                        "TailLog open timed out; live tail degraded until the store answers \
                         (retrying every {:?})",
                        config.reconnect_backoff
                    );
                }
                TailStopCause::OpenFailed
            }
            OpenOutcome::Aborted => {
                // The drain watch fired (signal or sender death) while
                // the open was in flight; zero stream consumed.
                // OpenFailed is neutral here — the orphan/terminal
                // re-check directly below re-reads the watch and the
                // exit law decides.
                TailStopCause::OpenFailed
            }
        };
        // The drain signal may have flipped — or its sender may have
        // vanished — while the stream was being driven or opened;
        // observe both before deciding.
        let cause = if drain.has_changed().is_err() {
            TailStopCause::Orphaned
        } else {
            cause
        };
        arm_grace(&mut grace_deadline, &drain, config.terminal_grace);
        let terminal = *drain.borrow();
        let grace_expired = grace_deadline.is_some_and(|d| Instant::now() >= d);
        match tail_next(cause, terminal, grace_expired, served_complete) {
            TailNext::Exit => {
                debug!(
                    ?cause,
                    terminal, grace_expired, served_complete, "log tail finished"
                );
                // THE single exit flush (merged_bug_150): a gap still
                // pending at exit time never got its second chance —
                // accept it now, marker plus withheld lines, through
                // the same path the in-stream accept uses. Exiting
                // with fetched-but-undisclosed lines is unrepresentable.
                flush_pending_gap(
                    &mut pending_gap,
                    &out_tx,
                    &mut last_relayed,
                    &mut accepted_gap_floor,
                )
                .await;
                return;
            }
            TailNext::Reopen => {
                metrics::counter!(
                    "rio_gateway_log_tail_reconnects_total",
                    "reason" => reconnect_reason(cause)
                )
                .increment(1);
                match backoff_capped(&mut drain, config.reconnect_backoff, grace_deadline).await {
                    // The next loop iteration's top-of-loop orphan
                    // check consults the law and exits before any
                    // open — no stream is ever opened for a dead
                    // consumer.
                    BackoffEnd::Orphaned => continue,
                    BackoffEnd::Slept | BackoffEnd::DrainSignal => {}
                }
            }
        }
    }
}

/// The metrics label for one re-open decision.
fn reconnect_reason(cause: TailStopCause) -> &'static str {
    match cause {
        TailStopCause::NaturalEnd | TailStopCause::TransportErr => "stream_ended",
        TailStopCause::OpenFailed => "open_failed",
        TailStopCause::GapObserved => "gap_observed",
        // Orphaned/PermanentErr never reach a Reopen verdict: the law
        // exits on them unconditionally (kani:
        // check_tail_next_no_premature_exit pins both).
        TailStopCause::Orphaned | TailStopCause::PermanentErr => {
            unreachable!("unconditional exits never reopen (kernel law)")
        }
    }
}

/// Arm the post-terminal grace deadline if the drain signal is set and
/// the deadline has not been armed yet. Idempotent; the deadline is
/// armed at most once per subscription.
fn arm_grace(deadline: &mut Option<Instant>, drain: &watch::Receiver<bool>, grace: Duration) {
    if deadline.is_none() && *drain.borrow() {
        *deadline = Some(Instant::now() + grace);
    }
}

/// How one backoff sleep ended — the caller needs to distinguish a
/// completed sleep / drain flip (keep looping) from the drain SENDER
/// dying (the relay is orphaned; the law exits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackoffEnd {
    /// The sleep ran its full (grace-capped) duration.
    Slept,
    /// The drain signal flipped; wake early so the loop can arm the
    /// grace deadline and consult [`tail_next`].
    DrainSignal,
    /// The drain sender was dropped: the owning set is gone and no
    /// signal can ever arrive. Treating this as a plain wake-up was
    /// the merged_bug_130 hot-loop — every backoff returned
    /// instantly, forever.
    Orphaned,
}

/// Sleep for `backoff`, capped at the remaining grace budget, waking
/// early if the drain signal flips (so the loop can arm the grace
/// deadline and consult [`tail_next`] — going terminal during a
/// backoff no longer exits the subscription by itself).
async fn backoff_capped(
    drain: &mut watch::Receiver<bool>,
    backoff: Duration,
    deadline: Option<Instant>,
) -> BackoffEnd {
    let dur = match deadline {
        Some(d) => backoff.min(d.saturating_duration_since(Instant::now())),
        None => backoff,
    };
    tokio::select! {
        _ = tokio::time::sleep(dur) => BackoffEnd::Slept,
        changed = drain.changed() => match changed {
            Ok(()) => BackoffEnd::DrainSignal,
            Err(_) => BackoffEnd::Orphaned,
        },
    }
}

/// Drive one connected stream until it stops yielding, the grace
/// expires, or the output closes. Every chunk steps the kernel cursor
/// ([`visit_chunk`]); the gap variant either ends the drive (first
/// sighting — the caller re-opens at the gap) or, when the same gap
/// survives its re-open chance, is accepted with one synthesized
/// marker line ahead of the chunk.
#[expect(
    clippy::too_many_arguments,
    reason = "the subscription's cursor state lives in run_tail; one drive borrows it all"
)]
async fn drive_stream(
    mut stream: tonic::Streaming<TailLogChunk>,
    derivation_path: &str,
    pinned_exec: &str,
    out_tx: &mpsc::Sender<TaggedLogChunk>,
    drain: &mut watch::Receiver<bool>,
    last_relayed: &mut Option<u64>,
    served_complete: &mut bool,
    grace_deadline: &mut Option<Instant>,
    pending_gap: &mut Option<PendingGap>,
    accepted_gap_floor: &mut Option<u64>,
    terminal_grace: Duration,
    reconnect_backoff: Duration,
) -> DriveEnd {
    loop {
        tokio::select! {
            msg = stream.message() => match msg {
                Ok(Some(chunk)) => {
                    // merged_bug_002 defensive check: this relay PINNED
                    // an execution in its request, so every chunk must
                    // carry it. A foreign exec_id here is a store bug
                    // (the keyed splice belongs to consumers that
                    // follow a derivation across executions — the
                    // dashboard — via the kernel's visit_chunk_keyed);
                    // relaying it would splice another build's lines
                    // into this one's numbering. Skip loudly.
                    if !chunk.exec_id.is_empty() && chunk.exec_id != pinned_exec {
                        warn!(
                            got = %chunk.exec_id,
                            pinned = %pinned_exec,
                            "TailLog chunk from a foreign execution on a pinned stream; skipped"
                        );
                        continue;
                    }
                    // The completeness claim rides every message; the
                    // value at stream end is the final message's — the
                    // one `tail_next` needs. Recorded BEFORE the empty
                    // final is dropped below.
                    *served_complete = chunk.is_complete;
                    let floor = last_relayed.map_or(0, |n| n.saturating_add(1));
                    let first = chunk.first_line_number;
                    match visit_chunk(floor, first, chunk.lines.len() as u64) {
                        ChunkVisit::Skip { .. } => {}
                        ChunkVisit::Serve { yield_from, yield_until, next_line } => {
                            // A pending gap whose first missing line is
                            // now BELOW the relay floor has healed: the
                            // re-open served the span, so the withheld
                            // copy is stale — flushing it later would
                            // disclose a hole that no longer exists and
                            // duplicate its lines (the composed-drive
                            // property test caught exactly that). Void
                            // it; a residual hole re-records fresh.
                            if pending_gap.as_ref().is_some_and(|p| next_line > p.gap_from) {
                                *pending_gap = None;
                            }
                            let tagged =
                                slice_chunk(chunk, derivation_path, yield_from, yield_until);
                            *last_relayed = Some(next_line.saturating_sub(1));
                            // Blocking send: a slow nix client backpressures
                            // this subscription (and, transitively, the
                            // store's reads on our behalf). While blocked the
                            // grace timer is not polled — acceptable, because
                            // a blocked send means the event loop is not
                            // consuming, which means the client write is the
                            // bottleneck and "exit promptly after terminal"
                            // has already lost to "deliver the lines at all".
                            if out_tx.send(tagged).await.is_err() {
                                return DriveEnd::OutputClosed;
                            }
                        }
                        ChunkVisit::GapThenServe {
                            gap_from,
                            gap_until,
                            yield_from,
                            yield_until,
                            next_line,
                        } => {
                            let withheld =
                                slice_chunk(chunk, derivation_path, yield_from, yield_until);
                            let fresh = PendingGap {
                                gap_from,
                                gap_until,
                                withheld,
                                next_line,
                            };
                            let second_sighting =
                                pending_gap.as_ref().is_some_and(|p| p.gap_from == gap_from);
                            // Budget-aware first sighting
                            // (merged_bug_150): withholding only pays
                            // off if there is grace left for the
                            // re-open chance to actually happen. At
                            // the edge — remaining grace at or under
                            // one backoff — accept immediately.
                            let no_budget_for_retry = grace_deadline.is_some_and(|d| {
                                d.saturating_duration_since(Instant::now()) <= reconnect_backoff
                            });
                            if second_sighting || no_budget_for_retry {
                                // Durable hole (the store re-served the
                                // same split) or no budget to find out:
                                // accept and disclose inline (owner
                                // decision Q8: the marker enters
                                // client-visible build output).
                                *pending_gap = Some(fresh);
                                if !flush_pending_gap(
                                    pending_gap,
                                    out_tx,
                                    last_relayed,
                                    accepted_gap_floor,
                                )
                                .await
                                {
                                    return DriveEnd::OutputClosed;
                                }
                            } else {
                                // First sighting with budget: WITHHOLD
                                // the sliced lines and give the store
                                // one re-open at the gap to serve the
                                // span (a transient: mid-flight cut,
                                // replica version skew, racing
                                // manifest read). The withheld copy
                                // makes every later exit total.
                                *pending_gap = Some(fresh);
                                return DriveEnd::Gap;
                            }
                        }
                    }
                }
                Ok(None) => return DriveEnd::Ended(TailStopCause::NaturalEnd),
                Err(status) => {
                    // merged_bug_164's reader half: a status the store
                    // typed as unservable-forever
                    // (x-rio-log-unservable: a hole no manifest row
                    // covers, a corrupt oversized row) will refuse
                    // identically on every future open — re-dialing it
                    // at the backoff cadence was the 1 Hz wedge the
                    // exit law now forbids.
                    if status
                        .metadata()
                        .get(rio_proto::LOG_UNSERVABLE_METADATA_KEY)
                        .is_some()
                    {
                        warn!(
                            code = ?status.code(),
                            msg = %status.message(),
                            "TailLog stream refused as permanently unservable; not retrying"
                        );
                        return DriveEnd::Ended(TailStopCause::PermanentErr);
                    }
                    debug!(code = ?status.code(), "TailLog stream error");
                    return DriveEnd::Ended(TailStopCause::TransportErr);
                }
            },
            // The derivation went terminal while the stream is open:
            // arm the grace deadline (once) and keep draining. Guarded
            // so the branch is only polled until the deadline is armed.
            res = drain.changed(), if grace_deadline.is_none() => {
                // Err = the sender (the LogTailSet entry) is gone; the
                // set aborts this task on removal so this is
                // unreachable, but a closed drain means there is nobody
                // left to flip it — treat as terminal-with-no-grace.
                if res.is_err() {
                    return DriveEnd::Ended(TailStopCause::NaturalEnd);
                }
                arm_grace(grace_deadline, drain, terminal_grace);
            }
            // The post-terminal grace expired with the stream still
            // open: stop waiting for its natural end. The cause value
            // is immaterial — `tail_next` exits on an expired grace
            // regardless of it.
            () = tokio::time::sleep_until(grace_deadline.unwrap_or_else(Instant::now)),
                if grace_deadline.is_some() =>
            {
                debug!("post-terminal grace expired; closing the log tail");
                return DriveEnd::Ended(TailStopCause::NaturalEnd);
            }
        }
    }
}

/// THE accept-and-disclose path (merged_bug_150): marker (floor-gated,
/// never repeated for a span) then the withheld lines, advancing the
/// relay watermark. Every consumer of a pending gap — the in-stream
/// second sighting, the budget-edge immediate accept, and BOTH
/// `run_tail` exit paths — flushes through here, so "exit with
/// fetched-but-undisclosed lines" is not a state the loop can be in.
/// Returns false iff the output channel is closed (nothing left to
/// disclose to).
async fn flush_pending_gap(
    pending_gap: &mut Option<PendingGap>,
    out_tx: &mpsc::Sender<TaggedLogChunk>,
    last_relayed: &mut Option<u64>,
    accepted_gap_floor: &mut Option<u64>,
) -> bool {
    let Some(pending) = pending_gap.take() else {
        return true;
    };
    if accepted_gap_floor.is_none_or(|f| pending.gap_from >= f) {
        *accepted_gap_floor = Some(pending.gap_until);
        let marker = gap_marker(pending.gap_from, pending.gap_until);
        if out_tx
            .send(TaggedLogChunk {
                derivation_path: pending.withheld.derivation_path.clone(),
                first_line_number: pending.gap_from,
                lines: vec![marker],
            })
            .await
            .is_err()
        {
            return false;
        }
    }
    let advanced = pending.next_line.saturating_sub(1);
    if !pending.withheld.lines.is_empty() {
        if out_tx.send(pending.withheld).await.is_err() {
            return false;
        }
        *last_relayed = Some(advanced);
    }
    true
}

/// The synthesized disclosure for an accepted durable gap (owner
/// decision Q8: one marker line in client-visible build output, never
/// repeated for the same span).
fn gap_marker(gap_from: u64, gap_until: u64) -> Vec<u8> {
    format!(
        "*** rio: lines {}-{} missing (durable log gap) ***",
        gap_from,
        gap_until.saturating_sub(1)
    )
    .into_bytes()
}

/// Tag the kernel-chosen `[yield_from, yield_until)` slice of `chunk`
/// for relay. The slice bounds come from [`visit_chunk`], which
/// guarantees they lie inside the chunk.
fn slice_chunk(
    chunk: TailLogChunk,
    derivation_path: &str,
    yield_from: u64,
    yield_until: u64,
) -> TaggedLogChunk {
    let first = chunk.first_line_number;
    let skip = usize::try_from(yield_from.saturating_sub(first)).unwrap_or(usize::MAX);
    let take = usize::try_from(yield_until.saturating_sub(yield_from)).unwrap_or(usize::MAX);
    let mut lines = chunk.lines;
    if skip > 0 {
        lines.drain(..skip.min(lines.len()));
    }
    lines.truncate(take);
    TaggedLogChunk {
        derivation_path: derivation_path.to_string(),
        first_line_number: yield_from,
        lines,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rio_proto::store::log_service_server::{LogService, LogServiceServer};
    use rio_proto::store::{AppendLogAck, AppendLogRequest, TailLogChunk, TailLogRequest};
    use rio_test_support::grpc::spawn_grpc_server;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::transport::Server;
    use tonic::{Request, Response, Status, Streaming};

    use super::{LogTailConfig, LogTailSet, TaggedLogChunk};

    // ------------------------------------------------------------------
    // The mock LogService
    // ------------------------------------------------------------------

    /// What one accepted `tail_log` call does after serving its scripted
    /// chunks.
    enum SessionEnd {
        /// End the stream cleanly (the ingest session closed).
        Close,
        /// Keep the stream open until the test drops the guard sender or
        /// the client disconnects. Models a quiet live build.
        Hold,
        /// End the stream with an in-stream error (a store replica
        /// dying mid-serve / a proxy reset).
        Error(tonic::Code),
        /// End the stream with a TYPED-permanent unservable error (the
        /// store's `x-rio-log-unservable` metadata: an uncovered hole,
        /// a corrupt oversized row).
        ErrorUnservable,
    }

    /// One scripted `tail_log` response.
    struct SessionScript {
        chunks: Vec<TailLogChunk>,
        end: SessionEnd,
    }

    #[derive(Clone, Default)]
    struct MockTail {
        inner: Arc<MockTailInner>,
    }

    #[derive(Default)]
    struct MockTailInner {
        /// Every `TailLogRequest` received, in arrival order.
        requests: Mutex<Vec<TailLogRequest>>,
        /// Scripts consumed in order, one per `tail_log` call. An
        /// exhausted script list serves an empty close (so a stray
        /// re-subscription doesn't hang a test).
        scripts: Mutex<VecDeque<SessionScript>>,
        /// Senders keeping `Hold` sessions open. Dropping the mock (end
        /// of test) closes them.
        holds: Mutex<Vec<mpsc::Sender<Result<TailLogChunk, Status>>>>,
        /// Set while a `tail_log` call is being held open *before*
        /// serving its chunks (see `gate_next_session`). The test uses
        /// this to deterministically interleave "the subscription is
        /// open" with "the terminal signal fires".
        gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
        /// How many upcoming `tail_log` calls fail at the open itself
        /// (UNAVAILABLE). The request is still recorded — the counter
        /// counts attempts.
        fail_opens: Mutex<u32>,
        /// How many upcoming `tail_log` calls HANG at the open itself
        /// (the future never resolves — a wedged store accepting TCP
        /// but never answering the RPC). The request is still
        /// recorded.
        hang_opens: Mutex<u32>,
    }

    impl MockTail {
        fn push_script(&self, chunks: Vec<TailLogChunk>, end: SessionEnd) {
            self.inner
                .scripts
                .lock()
                .unwrap()
                .push_back(SessionScript { chunks, end });
        }

        /// Fail the next `n` `tail_log` opens with UNAVAILABLE.
        fn fail_next_opens(&self, n: u32) {
            *self.inner.fail_opens.lock().unwrap() = n;
        }

        /// Hang the next `n` `tail_log` opens forever (the open future
        /// never resolves). For the bounded-open conformance tests.
        fn hang_next_opens(&self, n: u32) {
            *self.inner.hang_opens.lock().unwrap() = n;
        }

        /// The next `tail_log` call records its request, then parks
        /// until the returned `Notify` is notified, then serves its
        /// script. Lets a test assert "the request arrived" and flip
        /// external state before any chunk is served.
        fn gate_next_session(&self) -> Arc<tokio::sync::Notify> {
            let notify = Arc::new(tokio::sync::Notify::new());
            *self.inner.gate.lock().unwrap() = Some(notify.clone());
            notify
        }

        fn requests(&self) -> Vec<TailLogRequest> {
            self.inner.requests.lock().unwrap().clone()
        }

        fn request_count(&self) -> usize {
            self.inner.requests.lock().unwrap().len()
        }
    }

    #[tonic::async_trait]
    impl LogService for MockTail {
        type AppendLogStream = ReceiverStream<Result<AppendLogAck, Status>>;
        type TailLogStream = ReceiverStream<Result<TailLogChunk, Status>>;

        async fn append_log(
            &self,
            _request: Request<Streaming<AppendLogRequest>>,
        ) -> Result<Response<Self::AppendLogStream>, Status> {
            Err(Status::unimplemented("mock tail does not accept appends"))
        }

        async fn tail_log(
            &self,
            request: Request<TailLogRequest>,
        ) -> Result<Response<Self::TailLogStream>, Status> {
            let req = request.into_inner();
            self.inner.requests.lock().unwrap().push(req);
            {
                let mut fails = self.inner.fail_opens.lock().unwrap();
                if *fails > 0 {
                    *fails -= 1;
                    return Err(Status::unavailable("scripted open failure"));
                }
            }
            let hang = {
                let mut hangs = self.inner.hang_opens.lock().unwrap();
                if *hangs > 0 {
                    *hangs -= 1;
                    true
                } else {
                    false
                }
            };
            if hang {
                // The open future never resolves: the caller's bound
                // (drain race / open_bound) is the only way out.
                std::future::pending::<()>().await;
            }
            let gate = self.inner.gate.lock().unwrap().take();
            let script = self
                .inner
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(SessionScript {
                    chunks: vec![],
                    end: SessionEnd::Close,
                });
            let (tx, rx) = mpsc::channel(64);
            let inner = self.inner.clone();
            tokio::spawn(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                for chunk in script.chunks {
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                match script.end {
                    SessionEnd::Close => {}
                    SessionEnd::Hold => {
                        // Park the sender so the stream stays open until
                        // the mock is dropped or the client disconnects.
                        inner.holds.lock().unwrap().push(tx);
                    }
                    SessionEnd::Error(code) => {
                        let _ = tx
                            .send(Err(Status::new(code, "scripted stream error")))
                            .await;
                    }
                    SessionEnd::ErrorUnservable => {
                        let mut status =
                            Status::internal("scripted: chunk is permanently unservable");
                        status.metadata_mut().insert(
                            rio_proto::LOG_UNSERVABLE_METADATA_KEY,
                            tonic::metadata::MetadataValue::from_static("short_object"),
                        );
                        let _ = tx.send(Err(status)).await;
                    }
                }
            });
            Ok(Response::new(ReceiverStream::new(rx)))
        }
    }

    // ------------------------------------------------------------------
    // Harness
    // ------------------------------------------------------------------

    const DRV: &str = "/nix/store/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-tailed.drv";
    const EXEC_A: &str = "01900000-0000-7000-8000-00000000000a";
    const EXEC_B: &str = "01900000-0000-7000-8000-00000000000b";

    /// Test-scale timings: large enough that a loaded runner still
    /// observes the grace *window* (lines served inside it are
    /// relayed), small enough that the grace-expiry test stays fast.
    fn test_config() -> LogTailConfig {
        LogTailConfig {
            reconnect_backoff: Duration::from_millis(50),
            terminal_grace: Duration::from_millis(400),
            open_bound: Duration::from_millis(100),
        }
    }

    struct Harness {
        mock: MockTail,
        set: LogTailSet,
        out_rx: mpsc::Receiver<TaggedLogChunk>,
        _server: tokio::task::JoinHandle<()>,
    }

    async fn harness() -> Harness {
        harness_with(test_config()).await
    }

    async fn harness_with(config: LogTailConfig) -> Harness {
        let mock = MockTail::default();
        let router = Server::builder().add_service(LogServiceServer::new(mock.clone()));
        let (addr, server) = spawn_grpc_server(router).await;
        let client = rio_proto::LogServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("connect to the mock LogService");
        let (set, out_rx) = LogTailSet::with_config(client, None, config);
        Harness {
            mock,
            set,
            out_rx,
            _server: server,
        }
    }

    fn chunk(first_line: u64, n: usize) -> TailLogChunk {
        TailLogChunk {
            exec_id: EXEC_A.to_string(),
            lines: (0..n)
                .map(|i| format!("line-{:05}", first_line + i as u64).into_bytes())
                .collect(),
            first_line_number: first_line,
            is_complete: false,
        }
    }

    /// A final, possibly-empty chunk carrying the store's completeness
    /// claim (`send_final` / the last message of a session).
    fn final_chunk(next_line: u64, complete: bool) -> TailLogChunk {
        TailLogChunk {
            exec_id: EXEC_A.to_string(),
            lines: Vec::new(),
            first_line_number: next_line,
            is_complete: complete,
        }
    }

    /// Receive tagged chunks from `out_rx` until `n` total lines have
    /// arrived (or ~2 s elapse). Returns the flattened
    /// `(line_number, text)` pairs in arrival order.
    async fn recv_lines(rx: &mut mpsc::Receiver<TaggedLogChunk>, n: usize) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        while out.len() < n {
            let tagged = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out after {} of {n} lines", out.len()))
                .expect("output channel closed early");
            for (i, line) in tagged.lines.iter().enumerate() {
                out.push((
                    tagged.first_line_number + i as u64,
                    // Test fixture lines are always UTF-8 (the `chunk`
                    // helper formats them); a hard failure here is a
                    // test bug, not a display concern.
                    String::from_utf8(line.clone()).expect("test lines are UTF-8"),
                ));
            }
        }
        out
    }

    /// Poll `cond` every 10 ms until it returns true or ~2 s elapse.
    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    /// Rule 1 + 5: a `Started` opens a `TailLog(since_line: 0,
    /// follow: true)` subscription and the served chunks arrive on the
    /// output channel tagged with the derivation path, in order.
    #[tokio::test]
    async fn subscribes_on_started_and_relays_lines() {
        let mut h = harness().await;
        h.mock
            .push_script(vec![chunk(0, 3), chunk(3, 2)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);

        let lines = recv_lines(&mut h.out_rx, 5).await;
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4],
            "lines arrive in order with contiguous numbering"
        );
        assert_eq!(lines[0].1, "line-00000");
        assert_eq!(lines[4].1, "line-00004");

        let reqs = h.mock.requests();
        assert_eq!(reqs.len(), 1, "exactly one subscription opened");
        assert_eq!(reqs[0].derivation, DRV);
        assert_eq!(reqs[0].exec_id, EXEC_A);
        assert_eq!(reqs[0].since_line, 0);
        assert!(reqs[0].follow, "the live tail is a follow subscription");

        h.set.abort_all();
    }

    // r[verify store.log.tail-reconnect]
    /// Rule 2: a stream that ends while the derivation is not terminal
    /// is re-opened with `since_line = last_relayed + 1`, and lines the
    /// store resends below that cursor (chunk granularity) are not
    /// relayed twice.
    #[tokio::test]
    async fn resubscribes_on_premature_stream_end_with_cursor() {
        let mut h = harness().await;
        // Session 1 serves lines 0..50 and closes (a store deploy).
        h.mock.push_script(vec![chunk(0, 50)], SessionEnd::Close);
        // Session 2 resends the whole containing chunk (lines 0..60) —
        // the store's chunk granularity means a since_line=50 read can
        // legally return lines starting at 0.
        h.mock.push_script(vec![chunk(0, 60)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);

        let lines = recv_lines(&mut h.out_rx, 60).await;
        let numbers: Vec<u64> = lines.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            numbers,
            (0..60).collect::<Vec<u64>>(),
            "every line exactly once, in order, across the reconnect"
        );

        let reqs = h.mock.requests();
        assert_eq!(reqs.len(), 2, "the subscription re-opened once");
        assert_eq!(reqs[0].since_line, 0);
        assert_eq!(
            reqs[1].since_line, 50,
            "the re-open resumes at last_relayed + 1"
        );
        assert_eq!(reqs[1].exec_id, EXEC_A, "same execution across re-opens");

        h.set.abort_all();
    }

    /// Rule 3: a second `Started` with a different exec_id replaces the
    /// subscription (new request at since_line=0 for the new exec); a
    /// second `Started` with the same exec_id is a duplicate and does
    /// nothing.
    #[tokio::test]
    async fn replaces_subscription_on_redispatch() {
        let mut h = harness().await;
        h.mock.push_script(vec![chunk(0, 2)], SessionEnd::Hold);
        h.mock.push_script(vec![chunk(0, 1)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the first subscription to open", || {
            h.mock.request_count() == 1
        })
        .await;
        // Drain the first session's lines so the channel ordering in the
        // assertion below is unambiguous.
        let _ = recv_lines(&mut h.out_rx, 2).await;

        // Duplicate Started, same exec: no new subscription.
        h.set.on_started(DRV, EXEC_A);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            h.mock.request_count(),
            1,
            "a duplicate Started with the same exec_id must not re-subscribe"
        );

        // Re-dispatch: new exec_id.
        h.set.on_started(DRV, EXEC_B);
        wait_for("the replacement subscription to open", || {
            h.mock.request_count() == 2
        })
        .await;

        let reqs = h.mock.requests();
        assert_eq!(
            reqs[1].exec_id, EXEC_B,
            "the new subscription is for the new execution"
        );
        assert_eq!(
            reqs[1].since_line, 0,
            "a re-dispatched execution's log starts over at line 0"
        );

        // An empty exec_id never subscribes (rule 1's guard).
        h.set.on_started("/nix/store/other-thing.drv", "");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            h.mock.request_count(),
            2,
            "an empty exec_id must not open a subscription"
        );

        h.set.abort_all();
    }

    /// Rule 4, first half: the terminal signal arriving while the store
    /// still has lines buffered does not race them away — they are
    /// relayed before the subscription closes.
    #[tokio::test]
    async fn drains_to_natural_end_on_terminal() {
        let mut h = harness().await;
        // The mock parks the session before serving anything, so the
        // interleaving "subscription open → terminal signal → lines
        // served" is deterministic, not a sleep race.
        let gate = h.mock.gate_next_session();
        h.mock.push_script(vec![chunk(0, 50)], SessionEnd::Close);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the subscription to open", || h.mock.request_count() == 1).await;

        // The derivation goes terminal while the mock still holds all
        // 50 lines.
        h.set.on_terminal(DRV);
        gate.notify_one();

        let lines = recv_lines(&mut h.out_rx, 50).await;
        assert_eq!(
            lines.len(),
            50,
            "every buffered line is relayed after the terminal signal"
        );
        assert_eq!(lines.last().unwrap().0, 49);

        h.set.abort_all();
    }

    /// Rule 4, second half: a stream that never ends after the terminal
    /// signal is cut off at the post-terminal grace cap (the task
    /// exits; it does not wait forever).
    #[tokio::test]
    async fn terminal_grace_cap_closes_a_stuck_stream() {
        let mut h = harness().await;
        // The session serves 2 lines and then holds the stream open
        // forever (a wedged store replica / a follow stream whose
        // ingest session never closes).
        h.mock.push_script(vec![chunk(0, 2)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);
        let _ = recv_lines(&mut h.out_rx, 2).await;

        h.set.on_terminal(DRV);

        // The task must exit within the grace (400 ms test-scale) plus
        // slack — NOT hang forever on the held-open stream.
        let handle = h
            .set
            .tasks
            .get(DRV)
            .expect("the subscription is still tracked")
            .task
            .abort_handle();
        wait_for("the subscription task to exit at the grace cap", || {
            handle.is_finished()
        })
        .await;

        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_194's dedicated gateway twin: a store that ACCEPTS
    /// the TCP connection but never answers `TailLog` (the open future
    /// itself hangs) must not park the relay past the drain deadline.
    /// The first hung open is cut by the drain EDGE (terminal signal,
    /// raced via `bounded_open`'s abort arm); subsequent hung opens
    /// are cut by `open_bound`; the grace budget then expires and the
    /// law exits.
    ///
    /// RECORDED RED (pre-C1 shape, `bounded_open` neutered to a bare
    /// `client.tail_log(request).await`): the task never finishes —
    /// `wait_for("the relay to exit by the drain deadline")` panics at
    /// its 2 s cap with the open still parked (the pre-fix relay hung
    /// on the open await forever; only the unbounded open changed).
    #[tokio::test]
    async fn hung_open_abandons_at_drain_deadline() {
        let mut h = harness().await;
        // EVERY open hangs: the relay gets no stream, ever.
        h.mock.hang_next_opens(u32::MAX);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the first (hung) open to arrive", || {
            h.mock.request_count() == 1
        })
        .await;

        // Terminal while the open is parked: the drain edge must abort
        // the open (not wait for it), arm the grace, and the loop must
        // exit once the grace expires — cutting each further hung open
        // at open_bound (100 ms test scale) along the way.
        h.set.on_terminal(DRV);
        let handle = h
            .set
            .tasks
            .get(DRV)
            .expect("the subscription is still tracked")
            .task
            .abort_handle();
        wait_for("the relay to exit by the drain deadline", || {
            handle.is_finished()
        })
        .await;

        // The exit came from the law (grace expiry), not from a lucky
        // single attempt: the hung opens were retried and cut at least
        // once after the drain edge (grace 400 ms / open_bound 100 ms
        // + backoff 50 ms leaves room for ≥2 further attempts).
        assert!(
            h.mock.request_count() >= 2,
            "expected re-opens after the drain edge, got {}",
            h.mock.request_count()
        );
        // And nothing was ever relayed — the store never served a
        // chunk.
        assert!(
            h.out_rx.try_recv().is_err(),
            "no chunk should have been relayed from hung opens"
        );
        h.set.abort_all();
    }

    /// Rule 1's guard as its own test: a `Started` with an empty
    /// exec_id opens no subscription at all.
    #[tokio::test]
    async fn empty_exec_id_does_not_subscribe() {
        let h = harness().await;
        let mut set = h.set;
        set.on_started(DRV, "");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.mock.request_count(),
            0,
            "no TailLog request for an execution-less Started"
        );
        assert!(set.tasks.is_empty(), "no task spawned");
    }

    // r[verify store.log.tail-grace-drain+2]
    /// A transport error after the terminal signal does not end the
    /// subscription while grace budget remains — the final lines may be
    /// on a replica that is restarting right now.
    #[tokio::test]
    async fn transport_error_after_terminal_reopens_within_grace() {
        let mut h = harness().await;
        let gate = h.mock.gate_next_session();
        h.mock.push_script(
            vec![chunk(0, 2)],
            SessionEnd::Error(tonic::Code::Unavailable),
        );
        h.mock
            .push_script(vec![final_chunk(2, true)], SessionEnd::Close);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the subscription to open", || h.mock.request_count() == 1).await;
        h.set.on_terminal(DRV);
        gate.notify_one();

        let lines = recv_lines(&mut h.out_rx, 2).await;
        assert_eq!(lines.len(), 2);
        wait_for(
            "a re-open after the post-terminal transport error (grace unspent)",
            || h.mock.request_count() == 2,
        )
        .await;
        h.set.abort_all();
    }

    /// An open failure after the terminal signal keeps retrying within
    /// the grace budget instead of giving up with zero attempts.
    #[tokio::test]
    async fn open_failure_at_terminal_retries_within_grace() {
        let mut h = harness().await;
        h.mock.fail_next_opens(u32::MAX);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the first failed open", || h.mock.request_count() >= 1).await;
        h.set.on_terminal(DRV);
        let snapshot = h.mock.request_count();
        // The old code exits on the first post-terminal check; the
        // kernel keeps retrying every backoff tick until the grace
        // expires (~8 attempts at test scale). Two more attempts is
        // race-proof in both directions.
        wait_for("post-terminal open retries within the grace budget", || {
            h.mock.request_count() >= snapshot + 2
        })
        .await;
        h.set.abort_all();
    }

    /// The terminal signal landing during a between-streams backoff
    /// does not exit the subscription — the loop wakes, arms the grace,
    /// and re-opens to drain the final lines.
    #[tokio::test]
    async fn terminal_during_backoff_still_reopens() {
        // A long backoff so the terminal signal deterministically lands
        // inside the backoff window.
        let mut h = harness_with(LogTailConfig {
            reconnect_backoff: Duration::from_millis(300),
            terminal_grace: Duration::from_millis(800),
            open_bound: Duration::from_millis(100),
        })
        .await;
        h.mock.push_script(vec![chunk(0, 2)], SessionEnd::Close);
        h.mock
            .push_script(vec![final_chunk(2, true)], SessionEnd::Close);

        h.set.on_started(DRV, EXEC_A);
        let _ = recv_lines(&mut h.out_rx, 2).await;
        h.set.on_terminal(DRV);
        wait_for("the post-terminal re-open after the backoff", || {
            h.mock.request_count() == 2
        })
        .await;
        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// A natural stream end after terminal with the store NOT claiming
    /// completeness re-opens (the served log is incomplete and there is
    /// budget left); the re-opened stream's complete final exits it.
    #[tokio::test]
    async fn natural_end_incomplete_reopens_until_grace() {
        let mut h = harness().await;
        let gate = h.mock.gate_next_session();
        // Session 1 ends naturally but its final says incomplete.
        h.mock
            .push_script(vec![chunk(0, 3), final_chunk(3, false)], SessionEnd::Close);
        // Session 2 serves nothing new and claims complete.
        h.mock
            .push_script(vec![final_chunk(3, true)], SessionEnd::Close);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the subscription to open", || h.mock.request_count() == 1).await;
        h.set.on_terminal(DRV);
        gate.notify_one();
        let _ = recv_lines(&mut h.out_rx, 3).await;

        wait_for("an incomplete natural end re-opens", || {
            h.mock.request_count() == 2
        })
        .await;
        // The complete final exits the subscription well before the
        // grace cap.
        let handle = h
            .set
            .tasks
            .get(DRV)
            .expect("the subscription is still tracked")
            .task
            .abort_handle();
        wait_for("exit on served-complete", || handle.is_finished()).await;
        h.set.abort_all();
    }

    /// A forward jump in the served stream is NOT silently relayed: the
    /// subscription re-opens once at the gap, and only when the store
    /// re-serves the same split is the gap accepted — disclosed with
    /// exactly one synthesized marker line ahead of the chunk.
    #[tokio::test]
    async fn gap_reopened_once_then_marked() {
        let mut h = harness().await;
        // Session 1: lines 0..50, then a jump to 100 (a durable hole).
        h.mock
            .push_script(vec![chunk(0, 50), chunk(100, 5)], SessionEnd::Hold);
        // Session 2: the store re-serves the same shape.
        h.mock.push_script(vec![chunk(100, 5)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);

        let lines = recv_lines(&mut h.out_rx, 56).await;
        let reqs = h.mock.requests();
        assert_eq!(reqs.len(), 2, "the gap re-opened exactly once");
        assert_eq!(
            reqs[1].since_line, 50,
            "the re-open lands at the gap, not past it"
        );
        assert_eq!(lines[49].0, 49, "the contiguous prefix is intact");
        assert_eq!(
            lines[50],
            (
                50,
                "*** rio: lines 50-99 missing (durable log gap) ***".to_string()
            ),
            "the durable gap is disclosed inline exactly once"
        );
        assert_eq!(
            lines[51].0, 100,
            "the gapped chunk is relayed after the marker"
        );
        assert_eq!(lines[55].0, 104);
        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_150's recorded red: a forward jump sighted at the
    /// grace edge — no budget for the one re-open chance — used to
    /// DROP the fetched lines and exit without a marker (the pre-fix
    /// run timed out waiting for line 100: neither the withheld lines
    /// nor the disclosure ever reached the output). Now the
    /// budget-aware first sighting accepts immediately: marker plus
    /// the withheld lines flush through the same accept_and_disclose
    /// path every other exit uses, on the ONE open (no re-open burned
    /// against a budget that cannot fund it).
    #[tokio::test]
    async fn gap_at_grace_edge_flushes_withheld_lines() {
        let mut h = harness_with(LogTailConfig {
            reconnect_backoff: Duration::from_millis(50),
            terminal_grace: Duration::from_millis(150),
            open_bound: Duration::from_millis(100),
        })
        .await;
        // One stream serves the prefix and the jump, then closes. The
        // store NEVER re-serves the missing span (every later open is
        // the mock's exhausted-scripts empty close), so the ONLY way
        // lines 100.. and the marker reach the output is the exit
        // flush of the withheld copy.
        h.mock
            .push_script(vec![chunk(0, 5), chunk(100, 5)], SessionEnd::Close);
        h.set.on_started(DRV, EXEC_A);

        // The prefix relays before the sighting withholds.
        let prefix = recv_lines(&mut h.out_rx, 5).await;
        assert_eq!(prefix[4].0, 4, "prefix intact");

        // NOW the derivation goes terminal: the grace clock starts,
        // empty closes burn it down, and the exit fires with the gap
        // still pending — its one re-open chance never served the span.
        h.set.on_terminal(DRV);

        let rest = recv_lines(&mut h.out_rx, 6).await;
        assert_eq!(
            rest[0],
            (
                5,
                "*** rio: lines 5-99 missing (durable log gap) ***".to_string()
            ),
            "the exit flush discloses the gap"
        );
        assert_eq!(rest[1].0, 100, "the withheld lines flushed at exit");
        assert_eq!(rest[5].0, 104);
        h.set.abort_all();
    }

    /// merged_bug_164's reader half (recorded red: pre-fix the relay
    /// re-dialed the unservable stream once per backoff until grace —
    /// the request count climbed past 1 while the law had no
    /// PermanentErr vocabulary). A status carrying
    /// `x-rio-log-unservable` exits after ONE open, immediately,
    /// relaying what arrived before the refusal.
    #[tokio::test]
    async fn unservable_stream_exits_without_redial() {
        let mut h = harness().await;
        h.mock
            .push_script(vec![chunk(0, 3)], SessionEnd::ErrorUnservable);
        h.set.on_started(DRV, EXEC_A);

        let lines = recv_lines(&mut h.out_rx, 3).await;
        assert_eq!(lines[2].0, 2, "lines before the refusal relay");
        // Give a would-be re-dial loop several backoffs to show itself.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            h.mock.request_count(),
            1,
            "a typed-permanent refusal is never re-dialed"
        );
        h.set.abort_all();
    }

    /// merged_bug_002's gateway leg: this relay pins ONE execution; a
    /// chunk tagged with a different exec_id is a store bug and must
    /// be skipped (relaying it would splice a different build's
    /// numbering into this stream), without disturbing the pinned
    /// stream's dedup floor.
    #[tokio::test]
    async fn foreign_exec_chunk_skipped() {
        let mut h = harness().await;
        let mut foreign = chunk(10, 3);
        foreign.exec_id = EXEC_B.to_string();
        h.mock
            .push_script(vec![chunk(0, 2), foreign, chunk(2, 2)], SessionEnd::Hold);
        h.set.on_started(DRV, EXEC_A);

        let lines = recv_lines(&mut h.out_rx, 4).await;
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "the foreign-exec chunk is skipped, not spliced"
        );
        h.set.abort_all();
    }

    /// merged_bug_150's composed-drive property over a bounded grid
    /// (gap position x budget regime x whether the store ever re-serves
    /// the span): EVERY line the store served above the relay floor is
    /// either relayed or covered by a disclosed gap-marker span —
    /// "silently discarded" is not an outcome any cell can produce.
    #[tokio::test]
    async fn composed_drive_never_silently_discards() {
        for gap_at in [1u64, 7, 31] {
            for (backoff_ms, grace_ms) in [(400u64, 150u64), (50, 800)] {
                for reserves in [false, true] {
                    let mut h = harness_with(LogTailConfig {
                        reconnect_backoff: Duration::from_millis(backoff_ms),
                        terminal_grace: Duration::from_millis(grace_ms),
                        open_bound: Duration::from_millis(100),
                    })
                    .await;
                    let tail = chunk(100, 2);
                    h.mock
                        .push_script(vec![chunk(0, gap_at as usize), tail], SessionEnd::Hold);
                    if reserves {
                        // The re-open serves the missing span, then the
                        // tail again.
                        h.mock.push_script(
                            vec![chunk(gap_at, (100 - gap_at) as usize), chunk(100, 2)],
                            SessionEnd::Hold,
                        );
                    } else {
                        h.mock.push_script(vec![chunk(100, 2)], SessionEnd::Hold);
                    }
                    h.set.on_started(DRV, EXEC_A);
                    // Terminal after the prefix has had a moment to
                    // relay: the grace clock starts somewhere between
                    // the sighting and the later opens — WHICH accept
                    // path runs (second sighting, budget edge, exit
                    // flush) varies by cell and scheduling; the
                    // property must hold on all of them.
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    h.set.on_terminal(DRV);

                    // Drain until quiet: the set keeps a sender
                    // clone, so the channel never closes — a full
                    // second past every cell's grace+backoff horizon
                    // with no output means the relay is done.
                    let mut got: Vec<(u64, String)> = Vec::new();
                    while let Ok(Some(tagged)) =
                        tokio::time::timeout(Duration::from_secs(1), h.out_rx.recv()).await
                    {
                        for (i, line) in tagged.lines.iter().enumerate() {
                            got.push((
                                tagged.first_line_number + i as u64,
                                String::from_utf8(line.clone()).unwrap(),
                            ));
                        }
                    }
                    // The property: lines 0..gap_at always relay;
                    // 100..102 relay (withheld or fresh); the span
                    // between is either RELAYED (reserves && budget) or
                    // covered by a marker row at gap_at.
                    let nums: Vec<u64> = got.iter().map(|(n, _)| *n).collect();
                    for want in (0..gap_at).chain([100, 101]) {
                        assert!(
                            nums.contains(&want)
                                || got.iter().any(|(n, l)| *n <= want && l.contains("missing")),
                            "cell(gap_at={gap_at}, backoff={backoff_ms}, grace={grace_ms}, \
                             reserves={reserves}): line {want} neither relayed nor covered \
                             by a disclosure (got {nums:?})"
                        );
                    }
                    let served_span = nums.windows(2).all(|w| w[1] >= w[0]);
                    assert!(served_span, "relay order is monotone: {nums:?}");
                    h.set.abort_all();
                }
            }
        }
    }

    // r[verify store.log.tail-grace-drain+2]
    /// An orphaned relay — the drain sender gone without an abort —
    /// exits via the kernel law instead of hot-looping stream opens
    /// (merged_bug_130). Pre-fix the dead watch channel turned every
    /// backoff into an instant wake: this test observed the open
    /// counter climbing unboundedly at zero backoff.
    #[tokio::test]
    async fn orphaned_relay_exits_without_reopening() {
        let mock = MockTail::default();
        let router = Server::builder().add_service(LogServiceServer::new(mock.clone()));
        let (addr, _server) = spawn_grpc_server(router).await;
        let client = rio_proto::LogServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("connect to the mock LogService");
        let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
        // Orphan the relay before it ever runs: the owning set is gone.
        drop(drain_tx);
        let (out_tx, _out_rx) = mpsc::channel(8);
        let task = tokio::spawn(super::run_tail(
            client,
            None,
            DRV.to_string(),
            EXEC_A.to_string(),
            out_tx,
            drain_rx,
            test_config(),
        ));
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("orphaned relay exited (pre-fix: hot-looped forever)")
            .expect("relay task completed cleanly");
        assert_eq!(
            mock.request_count(),
            0,
            "an orphaned relay must never open a stream"
        );
    }

    /// Dropping the set — ANY drop path: session-loop early return,
    /// error exit, panic unwind — aborts every subscription without
    /// the caller remembering to call `abort_all` (the merged_bug_130
    /// ownership chokepoint).
    #[tokio::test]
    async fn dropping_the_set_aborts_subscriptions() {
        let mut h = harness().await;
        // A held-open session: the subscription would otherwise live
        // (and re-open) indefinitely.
        h.mock.push_script(vec![chunk(0, 1)], SessionEnd::Hold);
        h.set.on_started(DRV, EXEC_A);
        let _ = recv_lines(&mut h.out_rx, 1).await;
        let count_at_drop = h.mock.request_count();
        drop(h.set);
        // Give an un-aborted task ample room to re-open (pre-fix the
        // drop did nothing: the orphaned task kept opening streams).
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            h.mock.request_count(),
            count_at_drop,
            "dropping the set must abort its subscriptions (no further opens)"
        );
    }
}
