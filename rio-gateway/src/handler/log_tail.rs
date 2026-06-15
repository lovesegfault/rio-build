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
use std::sync::Arc;
use std::time::Duration;

use rio_common::transport::{OpenDeadline, OpenOutcome, bounded_open_within};
use rio_log_kernel::{ChunkVisit, TailNext, TailStopCause, tail_next, visit_chunk};
use rio_proto::LogServiceClient;
use rio_proto::store::{TailLogChunk, TailLogRequest};
use rio_proto::types;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tonic::transport::Channel;
use tracing::{Instrument, debug, info_span, warn};

use crate::server::session_jwt::{RemintCause, SessionTokenSource};

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

/// R17 (live_062): how long a tail's open-failure episode runs before
/// the ONE user-visible degradation line is injected into the build
/// output. VIOLABLE, hypothesis 30 s — derivation: a routine store
/// rolling deploy re-resolves through the balanced channel in well
/// under 10 s (below the bound, no notice); a full ingest-lease steal
/// window is 45 s and a token blackout is permanent, so both faces the
/// notice exists for clear 30 s comfortably; the floor is one worst-
/// case open cycle (`TAIL_OPEN_BOUND` 10 s + backoff 1 s), so a single
/// half-open replica blip cannot fire it. Raising it trades operator
/// silence for fewer benign lines; the live_062 lesson is that
/// SILENCE escalated a cosmetic degradation into an incident
/// ("stuck builds" — the builds were fine, nobody could see the
/// logs), so the bound stays tight-ish.
const TAIL_DEGRADED_NOTICE_AFTER: Duration = Duration::from_secs(30);

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
    /// Episode age at which the one degradation notice is injected
    /// ([`TAIL_DEGRADED_NOTICE_AFTER`] in production).
    pub degraded_notice_after: Duration,
}

impl Default for LogTailConfig {
    fn default() -> Self {
        Self {
            reconnect_backoff: RECONNECT_BACKOFF,
            terminal_grace: TERMINAL_GRACE,
            open_bound: TAIL_OPEN_BOUND,
            degraded_notice_after: TAIL_DEGRADED_NOTICE_AFTER,
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

/// The typed abort disposition shared between a relay's owner and its
/// [`PendingGapCell`] (`sys.epilogue.supersession`, bug_168). Drop-time
/// disclosure is the TERMINUS posture and the default: a
/// non-cooperative abort still owes the consumer the withheld lines
/// plus the gap marker (merged_bug_111). SUPERSESSION must DISCARD:
/// the successor relay owns the output channel, and the dead
/// execution's withheld state would splice into the retry's
/// client-visible stream as stale lines plus a FALSE "durable log gap"
/// marker — `TaggedLogChunk` carries no exec_id, so no downstream
/// consumer can filter the splice out. The owner marks the disposition
/// BEFORE aborting; the cell consults it before unwinding, so the
/// context-free Drop backstop is no longer the decision-maker.
///
/// (The exec_id-stamping alternative — tag every chunk and filter at
/// the consumer — is recorded REJECTED: the consumer-filter form
/// leaves the splice window open until the filter, and every consumer
/// must then carry the filter forever; the typed disposition kills the
/// splice at its source.)
#[derive(Clone)]
struct RelayDisposition(Arc<std::sync::atomic::AtomicBool>);

impl RelayDisposition {
    /// The default posture: disclose at drop (the terminus law).
    fn disclose_at_drop() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }

    /// Flip to must-discard. Called by the OWNER, before the abort —
    /// the ordering is the protocol: mark, abort, bound-join.
    fn mark_superseded(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    fn must_discard(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// How long the SUCCESSOR relay waits for the superseded relay's task
/// to unwind before producing its first byte. Mirrors the terminus
/// bound-join discipline (build.rs, the abort_all + 250 ms join at
/// stream end): an aborted task resolves at its next yield point
/// (typically <1 ms); the bound only caps a pathological scheduler
/// stall, and a straggler past it can no longer splice DISCLOSURES —
/// the disposition is already must-discard AND every awaited armed
/// discharge consults it post-reserve at the gap module's one
/// disclosure chokepoint (bug_040: pre-fix only Drop consulted, so an
/// in-flight poll or a sync-stalled straggler could still push a dead
/// execution's marker or stale withheld lines through the armed
/// sends) — only an in-flight regular send remains, the same priced
/// residual the terminus form carries.
const SUPERSEDED_JOIN_BOUND: Duration = Duration::from_millis(250);

/// One live subscription: the execution it is keyed on, the signal that
/// flips it from "re-open forever" to "drain and exit", the task
/// handle for the hard-cancel paths, and the abort disposition the
/// owner sets before superseding it.
struct TailHandle {
    exec_id: String,
    drain: watch::Sender<bool>,
    task: JoinHandle<()>,
    disposition: RelayDisposition,
}

/// The owner→relay wiring minted per subscription: the output channel,
/// the drain signal, and the abort disposition (the supersession
/// protocol's carrier). One struct so the relay's signature names the
/// owner contract once.
struct RelayWiring {
    out_tx: mpsc::Sender<TaggedLogChunk>,
    drain: watch::Receiver<bool>,
    disposition: RelayDisposition,
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
    /// The watching caller's session token SOURCE, consulted fresh on
    /// every `TailLog` open (the store enforces tenant ownership;
    /// bug_290). live_062 retired the previous `Option<String>`
    /// snapshot here: its own doc admitted "a token that expires
    /// mid-build degrades the live tail … the reconnect loop keeps
    /// retrying" — which at mint+65min was a TOTAL tail blackout for
    /// the rest of the build (every re-open UNAUTHENTICATED with the
    /// frozen string, while the build itself ran fine). The source
    /// re-mints near expiry per open and force-re-mints once after an
    /// UNAUTHENTICATED rejection it can locally verify as expiry
    /// (`r[gw.jwt.remint-local-expiry-only]`), so a tail outliving
    /// `JWT_SESSION_TTL_SECS` keeps reading.
    jwt: SessionTokenSource,
    out_tx: mpsc::Sender<TaggedLogChunk>,
    config: LogTailConfig,
    tasks: HashMap<String, TailHandle>,
}

impl LogTailSet {
    /// Create the set and its output channel. The receiver goes to the
    /// build's event loop; the set keeps a sender clone.
    pub(super) fn new(
        client: LogServiceClient<Channel>,
        jwt: SessionTokenSource,
    ) -> (Self, mpsc::Receiver<TaggedLogChunk>) {
        Self::with_config(client, jwt, LogTailConfig::default())
    }

    pub(super) fn with_config(
        client: LogServiceClient<Channel>,
        jwt: SessionTokenSource,
        config: LogTailConfig,
    ) -> (Self, mpsc::Receiver<TaggedLogChunk>) {
        let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE_DEPTH);
        (
            Self {
                client,
                jwt,
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

    /// Test-only visibility: whether `derivation_path`'s subscription
    /// has been flipped to drain-and-exit by [`Self::on_terminal`]
    /// (`None` = no subscription was ever opened). Lets sibling-module
    /// tests assert a display-family flip cut the dead execution's
    /// tail without timing on task exit.
    #[cfg(test)]
    pub(super) fn draining(&self, derivation_path: &str) -> Option<bool> {
        self.tasks
            .get(derivation_path)
            .map(|handle| *handle.drain.borrow())
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
        let mut superseded: Option<JoinHandle<()>> = None;
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
            // r[impl sys.epilogue.supersession]
            // The supersession protocol (bug_168): mark the typed
            // discard disposition FIRST — the old relay's
            // PendingGapCell consults it before unwinding, so its Drop
            // cannot splice the dead execution's withheld lines or a
            // false gap marker into the successor's stream — then
            // abort, then hand the JoinHandle to the successor, whose
            // FIRST act is the bounded join (the build.rs terminus
            // bound-join discipline, mirrored): the successor
            // structurally cannot produce output before the
            // predecessor unwound.
            if let Some(old) = self.tasks.remove(derivation_path) {
                old.disposition.mark_superseded();
                old.task.abort();
                superseded = Some(old.task);
            }
        }
        let (drain_tx, drain_rx) = watch::channel(false);
        let disposition = RelayDisposition::disclose_at_drop();
        let span = info_span!(
            "log_tail",
            drv = %derivation_path,
            exec_id = %exec_id,
        );
        let relay = run_tail(
            self.client.clone(),
            self.jwt.clone(),
            derivation_path.to_string(),
            exec_id.to_string(),
            RelayWiring {
                out_tx: self.out_tx.clone(),
                drain: drain_rx,
                disposition: disposition.clone(),
            },
            self.config,
        );
        let task = tokio::spawn(
            async move {
                if let Some(pred) = superseded {
                    // Bound-join the superseded relay before the new
                    // execution's relay produces a byte (the splice
                    // window closes here; the bound caps a
                    // pathological stall only).
                    let _ = tokio::time::timeout(SUPERSEDED_JOIN_BOUND, pred).await;
                }
                relay.await
            }
            .instrument(span),
        );
        self.tasks.insert(
            derivation_path.to_string(),
            TailHandle {
                exec_id: exec_id.to_string(),
                drain: drain_tx,
                task,
                disposition,
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

    /// Build terminus: hard-cancel everything still running. Every
    /// aborted task's pending-gap disclosure lands in the output
    /// channel regardless of residency (merged_bug_111; bug_122): the
    /// [`PendingGapCell`] Drop covers recorded state, and the
    /// [`ArmedGap`] guard covers a consume the abort caught
    /// mid-flight (taken state stays Drop-armed across every send) —
    /// the returned `JoinHandle`s let the terminus bound-join so
    /// those disclosures land BEFORE its final channel drain. Chunks
    /// already delivered to the output channel are unaffected. Priced
    /// residuals (disclosed at the gap module's law): a drop-time
    /// channel without enough free slots, a closed channel, and a
    /// regular serve send in flight at the abort (never-withheld
    /// content outside the gap obligation).
    pub(super) fn abort_all(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        self.tasks
            .drain()
            .map(|(_, handle)| {
                handle.task.abort();
                handle.task
            })
            .collect()
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
        // Drop cannot await the join handles; the aborted tasks'
        // cells still Drop-disclose into out_tx (merged_bug_111) —
        // on these paths (session early-exit, error, panic) the
        // consumer is usually gone too, the law's vacuous case.
        let _ = self.abort_all();
    }
}

/// What one [`drive_stream`] call observed: whether ANY chunk reached — quantifier: census(quint: tailReaderLoop.degradedImpliesNoRelaySinceArm) —
/// the output channel (`relayed_any` — the `r[sys.recovery.witnessed-clear]`
/// witnessed-work bit) and how the stream stopped (`end`).
///
/// merged_bug_006 (the §Multi-axis-fn sibling-arm hole): the
/// witnessed-work bit was a per-arm `last_relayed != relayed_before`
/// diff that the `DriveEnd::Gap` arm never computed — a drive that
/// served lines and then withheld at a forward jump bypassed the
/// degradation clear. `relayed_any` is constructed at exactly ONE
/// site (the function tail, after the labeled-break loop), so a
/// future 13th `DriveEnd` exit cannot hand-roll `relayed_any: false`
/// — there is no construction site for it.
#[must_use]
struct DriveOutcome {
    relayed_any: bool,
    end: DriveEnd,
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
    /// "exit at the grace edge" silently lose fetched lines). Earlier
    /// `Serve` chunks in the same drive may well have advanced the
    /// floor — only the gap chunk's own contribution is withheld; the
    /// caller re-opens at the (possibly advanced) floor and consults
    /// [`DriveOutcome::relayed_any`] for the witnessed-work clear
    /// (merged_bug_006: the prior doc claimed "nothing was relayed or
    /// advanced", which mis-stated exactly the serve-then-gap case the
    /// Gap arm bypassed).
    Gap,
    /// The output channel's receiver is gone — the build's event loop
    /// has exited. Nothing left to relay to.
    OutputClosed,
}

// r[impl store.log.tail-grace-drain+2]
// r[impl gw.tail.disclosure-linear]
/// The linear withheld-gap home (merged_bug_023; bug_122/R32 made the
/// linearity a MODULE BOUNDARY). Recorded state lives in
/// [`PendingGapCell`]; taken-in-flight state lives in [`ArmedGap`],
/// the disclosure-on-drop guard every take returns. Both are
/// Drop-armed with ONE disclosure implementation (the guard's), and
/// the state fields are private to this module — so a consume that
/// would hold withheld lines or an undisclosed hole in a bare local
/// across an await has no API to write: `take` without arming does
/// not exist, and each completed send defuses exactly the part it
/// delivered (no await sits between a payload leaving the guard and
/// the defuse — async cancellation lands only at awaits, so
/// taken-but-unsent is not a reachable residency).
///
/// merged_bug_111 carried: any termination that destroys a relay
/// future — the re-dispatch/terminus `JoinHandle::abort()`, or any
/// future abort site — reaches a Drop (cell or guard) that
/// `try_reserve`-sends the disclosure into the output channel.
/// bug_123 carried: a two-message disclosure reserves BOTH permits
/// before either message sends (both-or-neither). Honest residuals,
/// priced: a drop-time channel without enough free slots drops the
/// WHOLE disclosure (256 slots of backlog mean the consumer is the
/// bottleneck); a CLOSED channel means no consumer remains; a regular
/// serve send already in flight at abort time is outside the gap
/// obligation (it was never withheld); and `mem::forget` of a guard
/// is representable but unwritten.
mod gap {
    use tokio::sync::mpsc;

    use super::{RelayDisposition, TaggedLogChunk, gap_marker};

    /// A forward jump awaiting its one re-open chance, WITH the lines
    /// that arrived past it (merged_bug_150). The pre-fix shape kept
    /// only `gap_from` and dropped the chunk — so any exit between the
    /// first sighting and the second (grace edge, orphan,
    /// terminal-complete) lost both the fetched lines AND the marker.
    /// This is the cell's INPUT record; once recorded, the state is
    /// reachable only through the cell's typed operations.
    pub(super) struct PendingGap {
        /// First missing line (== the relay floor at sighting time).
        pub(super) gap_from: u64,
        /// One past the last missing line (the withheld chunk's first).
        pub(super) gap_until: u64,
        /// The already-sliced lines past the gap, ready to relay.
        pub(super) withheld: TaggedLogChunk,
        /// The relay watermark to adopt once the withheld lines flush.
        pub(super) next_line: u64,
    }

    /// What a `GapThenServe` sighting means against the cell's state.
    pub(super) enum Sighting {
        /// Nothing pending: this is the hole's first observation.
        First,
        /// The store re-served the SAME split — the hole is durable.
        SameSpan,
        /// The store's view changed across the re-open (a different
        /// split): the OLD pending must flush before the chunk is
        /// re-examined against the advanced floor.
        Divergent,
    }

    /// The deferred partial-heal floor (merged_bug_025): proof
    /// material that a serve send covering the hole prefix
    /// `[gap_from, floor)` is in flight. Minted ONLY by — quantifier: census(test: sealed_gap_lint_is_clean_at_head)
    /// [`PendingGapCell::on_serve`]'s partial arm; consumed ONLY by — quantifier: census(test: sealed_gap_lint_is_clean_at_head)
    /// [`PendingGapCell::discharge_served_prefix`], which the caller
    /// invokes AFTER the serve send resolves Ok. An abort parked in
    /// the send drops this token undischarged and the cell still
    /// records the FULL hole — Drop (or the terminus flush) discloses
    /// `[old gap_from, gap_until)`, never the shrunk span. Field
    /// private to `mod gap`: the reduction has no other writer.
    #[must_use = "a partial heal reduces the hole only at the post-send discharge"]
    pub(super) struct ServedPrefix {
        floor: u64,
    }

    /// How a `Serve` interacted with a pending hole.
    pub(super) enum ServeHeal {
        /// No pending, or the serve did not reach into the hole.
        Untouched,
        /// Partial heal: the serve chunk now in flight covers the
        /// hole prefix. The payload is the DEFERRED floor — the hole
        /// shrinks only at the post-send
        /// [`PendingGapCell::discharge_served_prefix`], exactly as
        /// the full-heal arm defuses only at post-send
        /// [`ArmedGap::note_delivered`]; the withheld lines stay
        /// recorded for the eventual flush either way.
        Shrunk(ServedPrefix),
        /// The hole fully healed — the serve chunk now in flight
        /// covers it. The payload is the ARMED remainder: the hole
        /// stays marker-armed (if the serve send aborts, the client
        /// never received the covering lines and the hole must still
        /// be disclosed) plus the full withheld lines. After the
        /// serve send completes the caller calls
        /// [`ArmedGap::note_delivered`], which defuses the hole and
        /// trims the duplicate prefix, then [`ArmedGap::send_lines`]
        /// relays the true continuation (a no-op for the
        /// all-duplicates case).
        Healed(ArmedGap),
    }

    /// merged_bug_023: the linear withheld-gap cell. The recorded
    /// state's only operations are record / same-span merge / heal /
    /// armed-take — a plain assignment that destroys
    /// fetched-but-undisclosed lines is unrepresentable, and (bug_122)
    /// so is a take that strips the disclosure obligation off the
    /// lines: [`Self::take_armed`] is the only take and it returns the
    /// Drop-armed [`ArmedGap`].
    pub(super) struct PendingGapCell {
        state: Option<PendingGap>,
        out_tx: mpsc::Sender<TaggedLogChunk>,
        /// The owner-set abort disposition the Drops consult
        /// (`sys.epilogue.supersession`): disclose at terminus,
        /// discard at supersession.
        disposition: RelayDisposition,
    }

    impl Drop for PendingGapCell {
        fn drop(&mut self) {
            // One disclosure implementation: arm the recorded state
            // and let the guard's Drop disclose it. The pending's
            // span is unmarked by invariant: a recorded pending is
            // exactly a hole whose disclosure has not flushed (every
            // accepted/flushed span empties the cell and raises the
            // accepted floor before a new pending can record at or
            // above it), so no dedup consultation is needed here.
            if let Some(p) = self.state.take() {
                drop(self.arm(p));
            }
        }
    }

    impl PendingGapCell {
        pub(super) fn new(
            out_tx: mpsc::Sender<TaggedLogChunk>,
            disposition: RelayDisposition,
        ) -> Self {
            Self {
                state: None,
                out_tx,
                disposition,
            }
        }

        pub(super) fn gap_from(&self) -> Option<u64> {
            self.state.as_ref().map(|p| p.gap_from)
        }

        /// The pending hole's withheld-start (`gap_until`) — the
        /// divergent-backfill discriminant (merged_bug_020): a fresh
        /// sighting beginning below it carries lines that HEAL part
        /// of the recorded hole.
        pub(super) fn pending_until(&self) -> Option<u64> {
            self.state.as_ref().map(|p| p.gap_until)
        }

        /// THE take (bug_122): every consume of recorded state leaves
        /// through here and receives the obligation ARMED — the
        /// returned guard Drop-discloses whatever part of the hole
        /// and the withheld lines has not been delivered or defused
        /// by the time it is destroyed.
        pub(super) fn take_armed(&mut self) -> Option<ArmedGap> {
            self.state.take().map(|p| self.arm(p))
        }

        fn arm(&self, p: PendingGap) -> ArmedGap {
            ArmedGap {
                hole: Some((p.gap_from, p.gap_until)),
                lines_from: p.withheld.first_line_number,
                lines: p.withheld.lines,
                derivation_path: p.withheld.derivation_path,
                next_line: p.next_line,
                out_tx: self.out_tx.clone(),
                disposition: self.disposition.clone(),
            }
        }

        pub(super) fn classify(&self, gap_from: u64, gap_until: u64) -> Sighting {
            match &self.state {
                None => Sighting::First,
                Some(p) if p.gap_from == gap_from && p.gap_until == gap_until => Sighting::SameSpan,
                Some(_) => Sighting::Divergent,
            }
        }

        /// First sighting: record the hole and its withheld lines.
        pub(super) fn record_first(&mut self, fresh: PendingGap) {
            debug_assert!(self.state.is_none(), "record_first on a non-empty cell");
            self.state = Some(fresh);
        }

        /// Same-span second sighting: keep whichever copy covers MORE
        /// (the chunks carry identical numbering, so the longer one is
        /// a superset — replacing with a shorter fresh copy would drop
        /// the old tail).
        pub(super) fn merge_same_span(&mut self, fresh: PendingGap) {
            match self.state.as_mut() {
                None => self.state = Some(fresh),
                Some(old) => {
                    if fresh.next_line > old.next_line {
                        *old = fresh;
                    }
                }
            }
        }

        /// A `Serve` advanced the floor to `next_line`: shrink, keep,
        /// or heal the pending hole. Healing returns the ARMED
        /// remainder (see [`ServeHeal::Healed`]) — never a bare chunk
        /// a parked send could silently destroy. A PARTIAL heal
        /// returns the DEFERRED floor (see [`ServedPrefix`]) and
        /// mutates NOTHING here (merged_bug_025): the hole shrinks
        /// only at the post-send discharge, exactly as the full-heal
        /// arm defuses only at post-send `note_delivered` — an abort
        /// parked in the serve send finds the recorded state still
        /// covering the FULL hole, healing prefix included.
        pub(super) fn on_serve(&mut self, next_line: u64) -> ServeHeal {
            let Some(p) = self.state.as_mut() else {
                return ServeHeal::Untouched;
            };
            if next_line <= p.gap_from {
                return ServeHeal::Untouched;
            }
            if next_line < p.gap_until {
                // Partial heal: the served prefix [gap_from,
                // next_line) cannot duplicate the withheld lines
                // (they start at gap_until); only the residual hole
                // will remain ONCE THE SERVE SEND COMPLETES. The
                // reduction itself is deferred through the typed
                // token — a bare `p.gap_from = next_line` here was
                // the R32 discharge escape: it destroyed the healing
                // prefix's disclosure under an abort parked in the
                // send, and the Drop marker misstated the span.
                return ServeHeal::Shrunk(ServedPrefix { floor: next_line });
            }
            // Full heal: the serve covers [.., next_line) ⊇ the hole.
            let p = self.state.take().expect("checked above");
            ServeHeal::Healed(self.arm(p))
        }

        /// The partial-heal obligation reduction (merged_bug_025): the
        /// ONLY writer of a recorded hole's `gap_from` — quantifier: census(test: sealed_gap_lint_is_clean_at_head). Consumes the
        /// [`ServedPrefix`] token minted by [`Self::on_serve`]'s
        /// partial arm — the caller invokes this AFTER the serve send
        /// resolved Ok, so the hole shrinks exactly when the covering
        /// lines were handed to the channel (the same priced in-flight
        /// sliver as every completed send). Total over cell states: a
        /// displaced or already-healed cell ignores a stale floor
        /// (`max`), and a floor past `gap_until` cannot be minted.
        pub(super) fn discharge_served_prefix(&mut self, served: ServedPrefix) {
            if let Some(p) = self.state.as_mut()
                && served.floor < p.gap_until
            {
                p.gap_from = p.gap_from.max(served.floor);
            }
        }
    }

    /// The disclosure-on-drop guard (bug_122, the R32 banner
    /// instance): the taken-in-flight residency of a pending gap. The
    /// obligation travels WITH the lines — a consume holds this guard
    /// (never bare lines) across its awaits, defusing each part as
    /// the matching send completes:
    ///
    /// - [`Self::disclose_hole_until`] sends a marker for a hole
    ///   prefix and shrinks the armed hole past it;
    /// - [`Self::note_delivered`] defuses what a CALLER-sent chunk
    ///   delivered (hole coverage and duplicate line prefixes);
    /// - [`Self::send_lines`] relays the remaining withheld lines and
    ///   leaves the guard fully defused;
    /// - [`Self::suppress_hole`]/[`Self::suppress_hole_until`] defuse
    ///   a marker the accepted-gap floor proves ALREADY disclosed by
    ///   an earlier flush (typed discharge-by-prior-disclosure, never
    ///   a silent drop).
    ///
    /// Every async wait is a `reserve().await` with the payload still
    /// armed; the payload leaves the guard only in the synchronous
    /// permit-send that follows, and the defuse is the next statement
    /// — an abort at ANY await point therefore finds the undelivered — quantifier: census(the W12-AT abort-window battery)
    /// remainder still armed, and [`Drop`] discloses it (marker
    /// and/or lines, both-permits law) subject to the supersession
    /// disposition and the priced channel-full/closed residuals.
    pub(super) struct ArmedGap {
        /// The still-undisclosed, still-undelivered hole
        /// `[from, until)`. `None` once disclosed, delivered, or
        /// floor-suppressed.
        hole: Option<(u64, u64)>,
        /// First line number of `lines`.
        lines_from: u64,
        /// The withheld lines not yet relayed (trimmed as coverage
        /// lands; empty once relayed).
        lines: Vec<Vec<u8>>,
        derivation_path: String,
        /// The relay watermark to adopt when the lines land.
        next_line: u64,
        out_tx: mpsc::Sender<TaggedLogChunk>,
        disposition: RelayDisposition,
    }

    impl ArmedGap {
        /// The armed hole, if any remains.
        pub(super) fn hole(&self) -> Option<(u64, u64)> {
            self.hole
        }

        /// Typed marker discharge WITHOUT a send: the accepted-gap
        /// floor proves the whole span was already disclosed by an
        /// earlier flush (markers are never repeated for a span).
        pub(super) fn suppress_hole(&mut self) {
            self.hole = None;
        }

        /// Prefix form of [`Self::suppress_hole`]: the span below
        /// `until` was already disclosed; the remainder stays armed.
        pub(super) fn suppress_hole_until(&mut self, until: u64) {
            if let Some((from, hole_until)) = self.hole {
                self.hole = if until >= hole_until {
                    None
                } else {
                    Some((from.max(until), hole_until))
                };
            }
        }

        /// Disclose the armed hole up to `until` (clamped to the
        /// hole): one marker send, then the hole shrinks past it. The
        /// await is the permit reservation — the hole stays armed
        /// through it, and the disposition is consulted post-reserve
        /// (bug_040). Returns `false` iff the relay must stop (channel
        /// closed or superseded-discard).
        pub(super) async fn disclose_hole_until(&mut self, until: u64) -> bool {
            let Some((from, hole_until)) = self.hole else {
                return true;
            };
            let until = until.min(hole_until);
            if until <= from {
                return true;
            }
            let Some(permit) = reserve_disclosure(&self.out_tx, &self.disposition).await else {
                return false;
            };
            permit.send(TaggedLogChunk {
                derivation_path: self.derivation_path.clone(),
                first_line_number: from,
                lines: vec![gap_marker(from, until)],
            });
            self.hole = if until < hole_until {
                Some((until, hole_until))
            } else {
                None
            };
            true
        }

        /// A caller-sent chunk delivered everything below `next`:
        /// defuse the covered hole prefix and trim the now-duplicate
        /// withheld prefix. Synchronous — called immediately after
        /// the caller's send completes.
        pub(super) fn note_delivered(&mut self, next: u64) {
            if let Some((from, until)) = self.hole {
                self.hole = if next >= until {
                    None
                } else {
                    Some((from.max(next), until))
                };
            }
            if next > self.lines_from {
                let skip = usize::try_from(next - self.lines_from).unwrap_or(usize::MAX);
                if skip >= self.lines.len() {
                    self.lines.clear();
                } else {
                    self.lines.drain(..skip);
                }
                self.lines_from = next;
            }
        }

        /// Relay the remaining withheld lines as one chunk and adopt
        /// the watermark; a no-op when nothing remains (the
        /// all-duplicates heal). The lines leave the guard only in
        /// the synchronous permit-send after the reservation resolves
        /// — and only if the disposition still permits disclosure
        /// (bug_040). Returns `false` iff the relay must stop (channel
        /// closed or superseded-discard).
        pub(super) async fn send_lines(&mut self, last_relayed: &mut Option<u64>) -> bool {
            if self.lines.is_empty() {
                return true;
            }
            let Some(permit) = reserve_disclosure(&self.out_tx, &self.disposition).await else {
                return false;
            };
            let lines = std::mem::take(&mut self.lines);
            permit.send(TaggedLogChunk {
                derivation_path: self.derivation_path.clone(),
                first_line_number: self.lines_from,
                lines,
            });
            *last_relayed = Some(self.next_line.saturating_sub(1));
            true
        }
    }

    /// Reserve an output permit AND consult the abort disposition —
    /// the ONE chokepoint every AWAITED armed discharge routes
    /// through (bug_040; the sealed-gap lint pins it: no other
    /// `reserve()` exists in this module). The consult happens AFTER
    /// the reservation resolves — the latest synchronous point before
    /// the enqueue — so a relay superseded while the reserve was
    /// parked (the in-progress-poll window) or while a sync-stalled
    /// straggler slept past the join bound refuses the send instead
    /// of splicing a dead execution's marker or stale lines into the
    /// successor's stream. `None` = stop relaying: the channel is
    /// closed OR this relay must discard; either way the caller exits
    /// and the guard's Drop (whose own consult is the backstop)
    /// settles the remainder. A free fn over the two fields so the
    /// permit borrow stays field-scoped.
    async fn reserve_disclosure<'a>(
        out_tx: &'a mpsc::Sender<TaggedLogChunk>,
        disposition: &RelayDisposition,
    ) -> Option<mpsc::Permit<'a, TaggedLogChunk>> {
        let Ok(permit) = out_tx.reserve().await else {
            return None;
        };
        if disposition.must_discard() {
            return None;
        }
        Some(permit)
    }

    impl Drop for ArmedGap {
        fn drop(&mut self) {
            let marker = self.hole.take();
            let lines = std::mem::take(&mut self.lines);
            if marker.is_none() && lines.is_empty() {
                // Defused: every part was delivered, disclosed, or
                // floor-suppressed.
                return;
            }
            if self.disposition.must_discard() {
                // Superseded (bug_168): the successor relay owns the
                // output channel now. The dead execution's withheld
                // lines are stale content and its hole is not a hole
                // in the NEW execution's log — relaying either would
                // splice into the retry's client-visible stream. No
                // try_send, no marker.
                return;
            }
            // bug_123 (R27 lens), carried: a two-message disclosure
            // is TRANSACTIONAL — both permits reserved before either
            // message sends (both-or-neither; permits send in
            // acquisition order on the same channel). The whole-drop
            // channel-full case remains the priced residual.
            let marker_chunk = marker.map(|(from, until)| TaggedLogChunk {
                derivation_path: self.derivation_path.clone(),
                first_line_number: from,
                lines: vec![gap_marker(from, until)],
            });
            let lines_chunk = (!lines.is_empty()).then(|| TaggedLogChunk {
                derivation_path: self.derivation_path.clone(),
                first_line_number: self.lines_from,
                lines,
            });
            match (marker_chunk, lines_chunk) {
                (Some(only), None) | (None, Some(only)) => {
                    if let Ok(permit) = self.out_tx.try_reserve() {
                        permit.send(only);
                    }
                }
                (Some(marker_chunk), Some(lines_chunk)) => {
                    if let (Ok(marker_permit), Ok(lines_permit)) =
                        (self.out_tx.try_reserve(), self.out_tx.try_reserve())
                    {
                        marker_permit.send(marker_chunk);
                        lines_permit.send(lines_chunk);
                    }
                }
                (None, None) => unreachable!("the defused case returned above"),
            }
        }
    }
}

use gap::{ArmedGap, PendingGap, PendingGapCell, ServeHeal, Sighting};

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
    jwt: SessionTokenSource,
    derivation_path: String,
    exec_id: String,
    wiring: RelayWiring,
    config: LogTailConfig,
) {
    let RelayWiring {
        out_tx,
        mut drain,
        disposition,
    } = wiring;
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
    let mut pending_gap = PendingGapCell::new(out_tx.clone(), disposition);
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
    // live_062: the degradation EPISODE clock — armed at the first failed/timed-out open OR the
    // first zero-relayed in-stream refusal (the store's err_stream
    // channel) of a consecutive run, cleared only on the first RELAYED
    // CHUNK (R34-w(i): the clearing event is witnessed work, never a
    // successful open — merged_bug_003). When an episode outlives
    // `config.degraded_notice_after`, exactly ONE typed notice line is
    // injected into the build output (the user-visible half the warn
    // latch above structurally cannot provide — operators read pod
    // logs, users read build output, and live_062's users diagnosed
    // "stuck builds" from a silent dark tail). `degraded_lane` is the
    // last failure's binary diagnosis for the notice text.
    //
    // R29' clock row (`tail-degraded-notice`): the gate is
    // DEADLINE-SHAPED on the episode's own arming stamp — `(armed,
    // armed + degraded_notice_after)` minted together at the first
    // failure, compared as `Instant::now() >= deadline` (the
    // grace_deadline idiom below; same Instant domain, no
    // conversion). The episode's age IS the user-facing evidence
    // here: nothing else refreshes or re-arms the deadline, so the
    // notice cannot be starved by activity on any other clock.
    let mut degraded: Option<(Instant, Instant)> = None;
    let mut degraded_notice_sent = false;
    let mut degraded_lane: &'static str = open_failed_lane(tonic::Code::Unavailable);
    loop {
        // An orphaned relay must never open another stream: the drain
        // sender vanishing means the owning set is gone, so the law
        // exits unconditionally — proven by
        // `check_tail_next_orphan_always_exits`. Checked BEFORE the
        // open so a death observed during backoff costs zero further
        // store connections (merged_bug_130: this exact shape used to
        // skip every backoff and hot-loop opens at full speed).
        // merged_bug_007: the orphaned WATCH does not entail a dead
        // OUTPUT channel — consumer liveness is its own input, and
        // the verdict's disclosure obligation is DISCHARGED here
        // exactly as on the post-drive path (the pre-fix fast path
        // matched `Exit { .. }` and returned, silently discarding a
        // disclose_truncation the kernel had computed).
        if drain.has_changed().is_err() {
            // Bound BEFORE the match: the watch read-guard inside a
            // scrutinee temporary must not live across the discharge
            // await below.
            let terminal = *drain.borrow();
            let verdict = tail_next(
                TailStopCause::Orphaned,
                terminal,
                grace_deadline.is_some_and(|d| Instant::now() >= d),
                served_complete,
                !out_tx.is_closed(),
            );
            match verdict {
                TailNext::Reopen => {
                    // Orphaned never reopens (kernel law, kani:
                    // check_tail_next_orphan_always_exits).
                    unreachable!("an orphaned relay never reopens (kernel law)")
                }
                TailNext::Exit(verdict) => {
                    debug!("log tail orphaned (subscription set gone); exiting");
                    finish_exit(
                        verdict,
                        &derivation_path,
                        &out_tx,
                        &mut pending_gap,
                        &mut last_relayed,
                        &mut accepted_gap_floor,
                    )
                    .await;
                    return;
                }
            }
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
        // (bug_290; store.log.tail-ownership). Fresh PER OPEN
        // (live_062): the source re-mints near expiry, so a tail that
        // outlives the session TTL keeps opening with a live token
        // instead of replaying a frozen snapshot into UNAUTHENTICATED
        // forever.
        if let Some(token) = jwt.fresh()
            && let Ok(value) = token.parse()
        {
            request
                .metadata_mut()
                .insert(rio_proto::TENANT_TOKEN_HEADER, value);
        }
        // The open is raced against the drain edge (a signal or
        // sender death mid-open aborts with zero stream consumed; the
        // re-check below reads the fresh watch state) and a
        // DEADLINE-TYPED bound (bug_038, R34(iv)): the per-attempt
        // open bound clamped to whatever remains of the armed grace
        // envelope, consulted BEFORE the open arms. Pre-fix the open
        // was the ONE await in this loop the grace clock could not
        // see — a hung re-open against a half-open replica ran the
        // full fixed bound (prod 10 s vs the 2 s grace), stretching
        // exit-at-expiry ~5-6x past the spec'd budget and delaying
        // the truncation disclosure; the backoff one await below was
        // already grace-capped.
        let open = bounded_open_within(
            async { _ = drain.changed().await },
            OpenDeadline::within(config.open_bound, grace_deadline),
            client.tail_log(request),
        )
        .await;
        let cause = match open {
            OpenOutcome::Opened(Ok(resp)) => {
                warned_open_failure = false;
                // R34-w (merged_bug_003): the episode does NOT end
                // here. A successful open is connection
                // establishment, not witnessed work — the store routes
                // every application refusal (authorize_tail NotFound,
                // missing exec row, PG errors) through err_stream so
                // the open succeeds and the in-body error becomes
                // TransportErr->Reopen. Clearing on the open
                // (merged_bug_003) reset the clock every 1s cycle on a
                // persistently-refusing peer, and neither the 30s
                // notice nor the per-episode warn could ever fire. The
                // clear moves below, gated on the FIRST RELAYED CHUNK.
                let DriveOutcome { relayed_any, end } = drive_stream(
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
                .await;
                // r[impl sys.recovery.witnessed-clear]
                // The episode-ending event is WITNESSED WORK:
                // `degraded` clears only on the first relayed chunk.
                // Hoisted ABOVE the `DriveEnd` match (merged_bug_006,
                // the §Multi-axis-fn hoist-the-guard template): EVERY — quantifier: census(quint: tailReaderLoop.degradedImpliesNoRelaySinceArm) —
                // drive that relayed lines clears the episode — Ended,
                // Gap, AND any future variant. A drive that ends with
                // zero lines relayed via err_stream (TransportErr)
                // ARMS the episode — that is the store's primary
                // refusal channel and exactly the dark-tail shape the
                // 30s notice exists for. A drive that ends NaturalEnd
                // with zero new lines is an idle-but-healthy tail (a
                // quiet build) and neither clears nor arms — the
                // conservative form (a held-open stream that yields
                // nothing is not degradation, per the WO derivation).
                if relayed_any {
                    degraded = None;
                    degraded_notice_sent = false;
                }
                match end {
                    DriveEnd::OutputClosed => return,
                    DriveEnd::Ended(cause) => {
                        if !relayed_any && matches!(cause, TailStopCause::TransportErr) {
                            let was_armed = degraded.is_some();
                            degraded.get_or_insert_with(|| {
                                let armed = Instant::now();
                                (armed, armed + config.degraded_notice_after)
                            });
                            degraded_lane = "store refused in-stream";
                            if !was_armed {
                                warn!(
                                    since_line,
                                    "TailLog stream opened but the store refused in-stream \
                                     before any line was relayed; live tail degraded \
                                     (retrying every {:?})",
                                    config.reconnect_backoff
                                );
                            }
                        }
                        cause
                    }
                    DriveEnd::Gap => {
                        // The jump (and its withheld lines) is already
                        // recorded in `pending_gap`; the very next
                        // stream gets one chance to serve the missing
                        // span before the gap is accepted and
                        // disclosed.
                        debug!(
                            gap_from = pending_gap.gap_from().unwrap_or(0),
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
                // UNAUTHENTICATED is the live_062 face: the token the
                // open carried is no longer valid. The source classifies
                // against the gateway's OWN clock and arms a re-mint
                // only for LocalExpiry (R34-w(iii), merged_bug_005): a
                // token well within its TTL was rejected for a cause a
                // re-mint cannot heal (revoked jti, unknown verify
                // key), and re-minting around it would silently
                // override an operator denial every cycle. The lane
                // text below carries the typed cause so the user-facing
                // notice and the warn name auth (not reachability).
                let unauth_cause = if status.code() == tonic::Code::Unauthenticated {
                    Some(jwt.note_rejected())
                } else {
                    None
                };
                // live_062 retired the "deliberately NOT surfaced"
                // posture that used to live here: a SHORT blip is
                // still silent (noise the user can't act on), but a
                // SUSTAINED episode now injects one typed notice line
                // below — silence escalated a cosmetic degradation
                // into a "stuck builds" incident while every build
                // was succeeding. The warn text forks by lane: the
                // pre-fix text blamed store reachability for token
                // rejections too, which misdiagnosed the 65-min
                // blackout in the one log line operators had.
                let lane = open_failed_lane(status.code());
                degraded.get_or_insert_with(|| {
                    let armed = Instant::now();
                    (armed, armed + config.degraded_notice_after)
                });
                degraded_lane = lane;
                if warned_open_failure {
                    debug!(code = ?status.code(), "TailLog open failed");
                } else {
                    warned_open_failure = true;
                    match unauth_cause {
                        Some(RemintCause::LocalExpiry) => warn!(
                            code = ?status.code(),
                            since_line,
                            "TailLog open rejected UNAUTHENTICATED; the carried token is past \
                             local expiry, re-minting and retrying (every {:?})",
                            config.reconnect_backoff
                        ),
                        Some(RemintCause::NotLocallyHealable) => warn!(
                            code = ?status.code(),
                            since_line,
                            "TailLog open rejected UNAUTHENTICATED with the carried token well \
                             within its TTL (revoked jti or unknown verify key); not re-minting \
                             (retrying every {:?})",
                            config.reconnect_backoff
                        ),
                        None => warn!(
                            code = ?status.code(),
                            since_line,
                            "TailLog open failed; live tail degraded until the store is reachable \
                             (retrying every {:?})",
                            config.reconnect_backoff
                        ),
                    }
                }
                TailStopCause::OpenFailed
            }
            OpenOutcome::TimedOut { after } => {
                // A half-open replica: TCP up, nobody home. Same
                // retryability as an answered open error — the lines
                // are durable in the store regardless.
                degraded.get_or_insert_with(|| {
                    let armed = Instant::now();
                    (armed, armed + config.degraded_notice_after)
                });
                degraded_lane = open_failed_lane(tonic::Code::Unavailable);
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
        // live_062: the one degradation notice per episode. try_send,
        // not send: a full output queue means the consumer has plenty
        // to read already and the notice is the least important line
        // in it — never backpressure the relay on it (the
        // `degraded_notice_sent` latch still flips, matching the
        // warn latch's one-per-episode law).
        if let Some((armed, deadline)) = degraded
            && !degraded_notice_sent
            && Instant::now() >= deadline
        {
            degraded_notice_sent = true;
            let _ = out_tx.try_send(TaggedLogChunk {
                derivation_path: derivation_path.clone(),
                first_line_number: last_relayed.map_or(0, |n| n.saturating_add(1)),
                lines: vec![degraded_notice(armed.elapsed(), degraded_lane)],
            });
        }
        arm_grace(&mut grace_deadline, &drain, config.terminal_grace);
        let terminal = *drain.borrow();
        let grace_expired = grace_deadline.is_some_and(|d| Instant::now() >= d);
        match tail_next(
            cause,
            terminal,
            grace_expired,
            served_complete,
            !out_tx.is_closed(),
        ) {
            TailNext::Exit(verdict) => {
                debug!(
                    ?cause,
                    terminal, grace_expired, served_complete, "log tail finished"
                );
                finish_exit(
                    verdict,
                    &derivation_path,
                    &out_tx,
                    &mut pending_gap,
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

/// THE exit epilogue, shared by BOTH `run_tail` exit paths
/// (merged_bug_007): the single exit flush (merged_bug_150 — a gap
/// still pending at exit time never got its second chance; accept it
/// now, marker plus withheld lines, through the same path the
/// in-stream accept uses), then the typed disclosure obligation is
/// DISCHARGED — `ExitVerdict` is consumable only through its
/// discharge closure, so an exit path that ignores the verdict no
/// longer typechecks (the pre-fix orphan fast path matched
/// `Exit { .. }` and returned without reading `disclose_truncation`).
///
/// bug_121 carried: the disclosure obligation quantifies over the LOSS — quantifier: census(grace_exit_discloses_store_served_truncation)
/// surface, not the fetch surface — the verdict carries the
/// obligation typed; the truncation marker is the FINAL write. The
/// spec's exit-exactly-at-expiry law is untouched (disclosure rides
/// the close; the grace is not extended). A send into a closed
/// channel fails harmlessly (the consumer raced away after the
/// liveness read — the verdict was honest at decision time).
// r[impl gw.tail.truncation-disclosed+2]
async fn finish_exit(
    verdict: rio_log_kernel::ExitVerdict,
    derivation_path: &str,
    out_tx: &mpsc::Sender<TaggedLogChunk>,
    pending_gap: &mut PendingGapCell,
    last_relayed: &mut Option<u64>,
    accepted_gap_floor: &mut Option<u64>,
) {
    flush_pending_gap(pending_gap, last_relayed, accepted_gap_floor).await;
    let from = last_relayed.map_or(0, |n| n.saturating_add(1));
    let marker = verdict.discharge(|disclose| {
        disclose.then(|| TaggedLogChunk {
            derivation_path: derivation_path.to_string(),
            first_line_number: from,
            lines: vec![truncation_marker(from)],
        })
    });
    if let Some(marker) = marker {
        let _ = out_tx.send(marker).await;
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
    // ONE producer of the envelope-clamp arithmetic (bug_038): the
    // backoff and the open consume the same `OpenDeadline::within`,
    // so the two grace-capped awaits in this loop cannot drift.
    let dur = OpenDeadline::within(backoff, deadline).bound();
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
    pending_gap: &mut PendingGapCell,
    accepted_gap_floor: &mut Option<u64>,
    terminal_grace: Duration,
    reconnect_backoff: Duration,
) -> DriveOutcome {
    let entry_floor = *last_relayed;
    // merged_bug_006: every former `return DriveEnd::X` is a
    // `break 'drive DriveEnd::X` so `relayed_any` is computed at
    // exactly ONE site — the function tail. A future exit cannot
    // bypass it.
    let end: DriveEnd = 'drive: loop {
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
                    // merged_bug_023: a divergent second sighting
                    // flushes the OLD pending and re-examines the SAME
                    // chunk against the advanced floor — at most one
                    // extra pass (the flush empties the cell, so the
                    // next classify is First).
                    loop {
                        let floor = last_relayed.map_or(0, |n| n.saturating_add(1));
                        let first = chunk.first_line_number;
                        match visit_chunk(floor, first, chunk.lines.len() as u64) {
                        ChunkVisit::Skip { .. } => break,
                        ChunkVisit::Serve { yield_from, yield_until, next_line } => {
                            // Heal-aware (merged_bug_023): a serve
                            // reaching INTO the hole shrinks it (the
                            // withheld lines stay); a serve covering
                            // the hole heals it — the withheld
                            // continuation relays (trimmed of any
                            // duplicate prefix), with NO marker, since
                            // no hole remains. The pre-fix void fired
                            // on any next_line > gap_from, destroying
                            // withheld lines on partial heals.
                            // The heal verdict is ARMED or DEFERRED
                            // (bug_122/R32; merged_bug_025): on a full
                            // heal the hole and the withheld
                            // continuation ride a disclosure-on-drop
                            // guard ACROSS the serve send below; on a
                            // partial heal the hole reduction rides
                            // the typed deferred floor. An abort
                            // parked in the send finds the obligation
                            // intact either way and Drop discloses the
                            // FULL recorded state; the completed send
                            // discharges exactly what it delivered.
                            let heal = pending_gap.on_serve(next_line);
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
                                break 'drive DriveEnd::OutputClosed;
                            }
                            match heal {
                                ServeHeal::Untouched => {}
                                ServeHeal::Shrunk(served) => {
                                    pending_gap.discharge_served_prefix(served);
                                }
                                ServeHeal::Healed(mut armed) => {
                                    armed.note_delivered(next_line);
                                    if !armed.send_lines(last_relayed).await {
                                        break 'drive DriveEnd::OutputClosed;
                                    }
                                }
                            }
                            break;
                        }
                        ChunkVisit::GapThenServe {
                            gap_from,
                            gap_until,
                            yield_from,
                            yield_until,
                            next_line,
                        } => {
                            match pending_gap.classify(gap_from, gap_until) {
                                Sighting::Divergent => {
                                    // merged_bug_020: when the fresh
                                    // sighting begins INSIDE the
                                    // pending hole (partial backfill),
                                    // it carries lines that HEAL part
                                    // of the recorded span — flushing
                                    // the old pending first would
                                    // advance the floor past them and
                                    // the re-visit would skip them.
                                    // Reconcile BEFORE any floor
                                    // advancement: residual marker,
                                    // healing lines, trimmed withheld.
                                    if let Some(pending_until) =
                                        pending_gap.pending_until()
                                        && first < pending_until
                                    {
                                        let armed = pending_gap
                                            .take_armed()
                                            .expect("pending_until implies pending");
                                        let fresh = slice_chunk(
                                            chunk,
                                            derivation_path,
                                            yield_from,
                                            yield_until,
                                        );
                                        if !reconcile_backfill(
                                            armed,
                                            fresh,
                                            next_line,
                                            out_tx,
                                            last_relayed,
                                            accepted_gap_floor,
                                        )
                                        .await
                                        {
                                            break 'drive DriveEnd::OutputClosed;
                                        }
                                        break;
                                    }
                                    // The fresh sighting begins at or
                                    // past the withheld start: no
                                    // healing lines exist below the
                                    // flush's floor advance. The old
                                    // withheld lines may be the only
                                    // copy of their span anywhere:
                                    // flush them (marker + lines),
                                    // then re-visit this chunk against
                                    // the advanced floor — the fresh
                                    // split's true residual (possibly
                                    // none) falls out of the
                                    // re-examination.
                                    if !flush_pending_gap(
                                        pending_gap,
                                        last_relayed,
                                        accepted_gap_floor,
                                    )
                                    .await
                                    {
                                        break 'drive DriveEnd::OutputClosed;
                                    }
                                    continue;
                                }
                                Sighting::SameSpan => {
                                    // Durable hole (the store re-served
                                    // the same split): accept and
                                    // disclose inline (owner decision
                                    // Q8: the marker enters
                                    // client-visible build output).
                                    // Merge keeps the longer coverage —
                                    // a shorter fresh copy must not
                                    // drop the old tail.
                                    let withheld = slice_chunk(
                                        chunk,
                                        derivation_path,
                                        yield_from,
                                        yield_until,
                                    );
                                    pending_gap.merge_same_span(PendingGap {
                                        gap_from,
                                        gap_until,
                                        withheld,
                                        next_line,
                                    });
                                    if !flush_pending_gap(
                                        pending_gap,
                                        last_relayed,
                                        accepted_gap_floor,
                                    )
                                    .await
                                    {
                                        break 'drive DriveEnd::OutputClosed;
                                    }
                                    break;
                                }
                                Sighting::First => {
                                    // Budget-aware first sighting
                                    // (merged_bug_150): withholding only
                                    // pays off if there is grace left
                                    // for the re-open chance to actually
                                    // happen. At the edge — remaining
                                    // grace at or under one backoff —
                                    // accept immediately.
                                    let no_budget_for_retry =
                                        grace_deadline.is_some_and(|d| {
                                            d.saturating_duration_since(Instant::now())
                                                <= reconnect_backoff
                                        });
                                    let withheld = slice_chunk(
                                        chunk,
                                        derivation_path,
                                        yield_from,
                                        yield_until,
                                    );
                                    pending_gap.record_first(PendingGap {
                                        gap_from,
                                        gap_until,
                                        withheld,
                                        next_line,
                                    });
                                    if no_budget_for_retry {
                                        if !flush_pending_gap(
                                            pending_gap,
                                            last_relayed,
                                            accepted_gap_floor,
                                        )
                                        .await
                                        {
                                            break 'drive DriveEnd::OutputClosed;
                                        }
                                        break;
                                    }
                                    // First sighting with budget:
                                    // WITHHOLD the sliced lines and give
                                    // the store one re-open at the gap
                                    // (a transient: mid-flight cut,
                                    // replica version skew, racing
                                    // manifest read). The withheld copy
                                    // makes every later exit total.
                                    break 'drive DriveEnd::Gap;
                                }
                            }
                        }
                        }
                    }
                }
                Ok(None) => break 'drive DriveEnd::Ended(TailStopCause::NaturalEnd),
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
                        break 'drive DriveEnd::Ended(TailStopCause::PermanentErr);
                    }
                    debug!(code = ?status.code(), "TailLog stream error");
                    break 'drive DriveEnd::Ended(TailStopCause::TransportErr);
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
                    break 'drive DriveEnd::Ended(TailStopCause::NaturalEnd);
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
                break 'drive DriveEnd::Ended(TailStopCause::NaturalEnd);
            }
        }
    };
    DriveOutcome {
        relayed_any: *last_relayed != entry_floor,
        end,
    }
}

/// THE accept-and-disclose path (merged_bug_150): marker (floor-gated,
/// never repeated for a span) then the withheld lines, advancing the
/// relay watermark. Every consumer of a pending gap — the in-stream
/// second sighting, the budget-edge immediate accept, and BOTH
/// `run_tail` exit paths — flushes through here, so "exit with
/// fetched-but-undisclosed lines" is not a state the loop can be in.
/// The consume rides the armed take (bug_122/R32): the marker and the
/// lines stay Drop-armed across both sends, and each completed send
/// defuses its part — an abort parked at EITHER send still discloses
/// the remainder. Returns false iff the output channel is closed
/// (nothing left to disclose to).
async fn flush_pending_gap(
    pending_gap: &mut PendingGapCell,
    last_relayed: &mut Option<u64>,
    accepted_gap_floor: &mut Option<u64>,
) -> bool {
    let Some(mut armed) = pending_gap.take_armed() else {
        return true;
    };
    if let Some((from, until)) = armed.hole() {
        if accepted_gap_floor.is_none_or(|f| from >= f) {
            *accepted_gap_floor = Some(until);
            if !armed.disclose_hole_until(until).await {
                return false;
            }
        } else {
            // Below the accepted floor: an earlier flush already
            // disclosed this span — typed discharge, never a re-mark.
            armed.suppress_hole();
        }
    }
    armed.send_lines(last_relayed).await
}

/// merged_bug_020: the divergent-backfill reconcile. The fresh
/// divergent chunk begins INSIDE the pending hole
/// (`fresh.first < pending.gap_until`): flushing the old pending
/// FIRST would advance the floor past the healing lines, and the
/// re-visit would skip them above it — fetched content silently
/// destroyed by the disclosure choreography itself. Reconcile BEFORE
/// any floor advancement, in line order:
///
/// 1. residual prefix marker `[gap_from, fresh.first)` — both store
///    views agree the span is missing and the one-retry budget is
///    spent;
/// 2. the fresh healing slice (every line the store just served);
/// 3. the withheld continuation trimmed of the fresh coverage — or,
///    when the fresh stopped short of the withheld start, the second
///    residual marker `[fresh_next, pending.gap_until)` and then the
///    whole withheld.
///
/// Every fresh/withheld line is relayed exactly once or proven a
/// duplicate of relayed coverage; destruction has no path (the
/// PendingGapCell law, extended over this consume — bug_122/R32: the
/// pending arrives ARMED, every marker/line stays Drop-armed until
/// the send that delivers it completes, and the fresh chunk's landing
/// defuses exactly the coverage it delivered). Returns false iff the
/// output channel closed.
async fn reconcile_backfill(
    mut armed: ArmedGap,
    fresh: TaggedLogChunk,
    fresh_next: u64,
    out_tx: &mpsc::Sender<TaggedLogChunk>,
    last_relayed: &mut Option<u64>,
    accepted_gap_floor: &mut Option<u64>,
) -> bool {
    let fresh_first = fresh.first_line_number;
    let Some((gap_from, _)) = armed.hole() else {
        debug_assert!(false, "a backfill reconcile starts with an armed hole");
        return armed.send_lines(last_relayed).await;
    };
    // 1. Residual prefix hole, if any survives the marker dedup.
    if fresh_first > gap_from {
        if accepted_gap_floor.is_none_or(|f| gap_from >= f) {
            *accepted_gap_floor = Some(fresh_first);
            if !armed.disclose_hole_until(fresh_first).await {
                return false;
            }
        } else {
            // Already disclosed by an earlier flush — typed
            // discharge of the prefix, never a re-mark.
            armed.suppress_hole_until(fresh_first);
        }
    }
    // 2. The healing lines themselves — BEFORE any floor advancement
    // could hide them. (The armed remainder covers the hole and the
    // withheld lines if this send aborts mid-flight.)
    if !fresh.lines.is_empty() {
        if out_tx.send(fresh).await.is_err() {
            return false;
        }
        *last_relayed = Some(fresh_next.saturating_sub(1));
    }
    // The fresh coverage landed: defuse the hole span it delivered
    // and trim any withheld prefix it duplicated (3a's skip).
    armed.note_delivered(fresh_next);
    // 3b. The fresh stopped short of the withheld start: a second
    // residual hole remains. Disclose it, then the withheld whole —
    // the retry budget for this hole is spent.
    if let Some((from, until)) = armed.hole() {
        if accepted_gap_floor.is_none_or(|f| from >= f) {
            *accepted_gap_floor = Some(until);
            if !armed.disclose_hole_until(until).await {
                return false;
            }
        } else {
            armed.suppress_hole();
        }
    }
    // 3a/3b tail: the non-duplicate withheld remainder.
    armed.send_lines(last_relayed).await
}

/// bug_121: the truncation disclosure for a cut exit — the
/// gap-vocabulary form with the un-served tail open-ended (the relay
/// cannot know the extent it never fetched), one marker line in
/// client-visible build output (the owner-decision Q8 vocabulary).
///
/// bug_018 (R26 — absence is not evidence): the text is denominated
/// in the verdict the relay actually holds. The marker fires exactly
/// when `served_complete` was never observed true — the store's
/// completeness predicate DECLINED to confirm — and that preimage has
/// two faces the 1-bit wire claim cannot distinguish: cut-mid-replay
/// durable residue (retrievable from the store) and a never-uploaded
/// tail (builder Detached/DeadlineExpired un-acked loss; manifest
/// holes are "genuine storage loss" store-side). The pre-fix text
/// asserted "the full log is durable in the store" unconditionally —
/// narrating the ABSENCE of the completeness confirmation as positive
/// durability, in precisely the loss subset where a user following it
/// finds nothing. The text now states both faces, keeping the
/// durable face's actionable pointer conditional; the confirmed face
/// (`served_complete == true`) stays the honest no-marker exit.
// r[impl gw.tail.truncation-disclosed+2]
// r[impl gw.tail.two-face-truncation]
fn truncation_marker(from: u64) -> Vec<u8> {
    format!(
        "*** rio: lines {from}- not served before the live tail closed \
(the store did not confirm the log complete: a stored remainder is \
retrievable from the store; a tail never uploaded by the builder is \
lost) ***"
    )
    .into_bytes()
}

fn gap_marker(gap_from: u64, gap_until: u64) -> Vec<u8> {
    format!(
        "*** rio: lines {}-{} missing (durable log gap) ***",
        gap_from,
        gap_until.saturating_sub(1)
    )
    .into_bytes()
}

/// live_062: the binary user-facing diagnosis of an open failure. The
/// projection is TOTAL by design over exactly two lanes — the token
/// lane (UNAUTHENTICATED: the gateway re-mints and retries; with a
/// healthy key this self-heals on the next open) and the reachability
/// lane (everything else: the store is down/deploying/half-open; the
/// reconnect loop rides it out). The pre-fix warn text folded both
/// into "until the store is reachable", which misdiagnosed the
/// token-expiry blackout in the only line operators had.
fn open_failed_lane(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Unauthenticated => "session token rejected",
        _ => "store unreachable",
    }
}

/// live_062: the ONE user-visible line of a sustained tail
/// degradation (the `***`-marker house form, same family as the gap
/// and truncation markers). States the three things the user can act
/// on: how long it has been dark, the lane, and that the build itself
/// is unaffected with the log durable for later.
fn degraded_notice(elapsed: Duration, lane: &str) -> Vec<u8> {
    format!(
        "*** rio: live log tail degraded for {}s ({lane}); the build is \
         unaffected and the full log stays readable from the store after \
         completion ***",
        elapsed.as_secs()
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

    use super::{LogTailConfig, LogTailSet, SessionTokenSource, TaggedLogChunk, TailStopCause};

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
        /// The `x-rio-tenant-token` metadata observed per `tail_log`
        /// call, in arrival order (`None` = no header). live_062: the
        /// refresh-per-open contract is asserted against THESE — the
        /// store-visible tokens, not the source's internal state.
        tokens: Mutex<Vec<Option<String>>>,
        /// How many upcoming `tail_log` calls fail at the open itself
        /// (UNAVAILABLE). The request is still recorded — the counter
        /// counts attempts.
        fail_opens: Mutex<u32>,
        /// How many upcoming `tail_log` calls fail at the open with
        /// UNAUTHENTICATED (live_062: a verifier-side token
        /// rejection). The request and its token are still recorded.
        unauth_opens: Mutex<u32>,
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

        /// Fail the next `n` `tail_log` opens with UNAUTHENTICATED
        /// (live_062: what the store answers when the carried token
        /// is rejected by the verifier — the gateway classifies the
        /// cause locally per `r[gw.jwt.remint-local-expiry-only]`).
        fn unauth_next_opens(&self, n: u32) {
            *self.inner.unauth_opens.lock().unwrap() = n;
        }

        /// The tenant-token header observed per open, arrival order.
        fn tokens(&self) -> Vec<Option<String>> {
            self.inner.tokens.lock().unwrap().clone()
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
            let token = request
                .metadata()
                .get(rio_proto::TENANT_TOKEN_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            self.inner.tokens.lock().unwrap().push(token);
            let req = request.into_inner();
            self.inner.requests.lock().unwrap().push(req);
            {
                let mut unauths = self.inner.unauth_opens.lock().unwrap();
                if *unauths > 0 {
                    *unauths -= 1;
                    return Err(Status::unauthenticated("scripted: token rejected"));
                }
            }
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
    ///
    /// PRODUCTION PARAMETER ORDERING (bug_038, R31'(v)): `open_bound`
    /// is LARGER than `terminal_grace` (2000 ms vs 400 ms — the same
    /// 5x ratio as the shipped 10 s vs 2 s). The pre-fix fixture
    /// inverted the ratio (open 100 ms < grace 400 ms), so the
    /// missing open-vs-grace clamp was structurally untestable: every
    /// hung open was cut by its own bound well inside the grace, and
    /// the clamp assertion was vacuous. A grace-conformance fixture
    /// must preserve the production ordering or it certifies nothing.
    fn test_config() -> LogTailConfig {
        LogTailConfig {
            reconnect_backoff: Duration::from_millis(50),
            terminal_grace: Duration::from_millis(400),
            open_bound: Duration::from_millis(2000),
            // Big enough that the non-notice tests never trip it
            // through their scripted single failures; the notice test
            // overrides it down.
            degraded_notice_after: Duration::from_secs(30),
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
        let (set, out_rx) = LogTailSet::with_config(client, SessionTokenSource::none(), config);
        Harness {
            mock,
            set,
            out_rx,
            _server: server,
        }
    }

    /// Harness with a REAL token source (live_062): an expired cached
    /// mint + the signing key, so the refresh-per-open contract is
    /// observable through the mock's captured `x-rio-tenant-token`
    /// headers.
    async fn harness_with_jwt(config: LogTailConfig, jwt: SessionTokenSource) -> Harness {
        let mock = MockTail::default();
        let router = Server::builder().add_service(LogServiceServer::new(mock.clone()));
        let (addr, server) = spawn_grpc_server(router).await;
        let client = rio_proto::LogServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("connect to the mock LogService");
        let (set, out_rx) = LogTailSet::with_config(client, jwt, config);
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

    /// live_062 (W13-S10-A, red-first): every `TailLog` open carries a
    /// FRESH token, never the construction-time snapshot. The source
    /// is built over an EXPIRED cached mint (`"stale-token"`,
    /// exp = now−10 — the mint+65min shape); the store-visible header
    /// must be a re-mint on the FIRST open and on the re-open after a
    /// scripted UNAVAILABLE.
    ///
    /// Pre-fix RED (the `Option<String>` snapshot design): the type
    /// cannot host this witness at all — `LogTailSet` froze the string
    /// at construction, so every captured header reads `stale-token`
    /// verbatim (the field's own doc conceded it: "a token that
    /// expires mid-build degrades the live tail … the reconnect loop
    /// keeps retrying"). Strawman disclosure per §1.5-4: the red is
    /// the retired design's documented behavior, quoted; the witness
    /// is expressible only post-redesign.
    #[tokio::test]
    async fn tail_opens_carry_refreshed_token_not_the_snapshot() {
        use ed25519_dalek::SigningKey;
        use rio_auth::jwt::TenantClaims;
        let key = SigningKey::from_bytes(&[0x42; 32]);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let stale = TenantClaims {
            sub: uuid::Uuid::from_u128(0xCAFE),
            iat: now - 4000,
            exp: now - 10,
            jti: "stale-jti".into(),
        };
        let source =
            SessionTokenSource::new(Some(("stale-token".into(), stale)), Some(Arc::new(key)));
        let mut h = harness_with_jwt(test_config(), source).await;

        h.mock.fail_next_opens(1);
        h.mock.push_script(vec![chunk(0, 1)], SessionEnd::Hold);
        h.set.on_started(DRV, EXEC_A);
        let mock = h.mock.clone();
        wait_for("two opens (failed + retried)", || mock.request_count() >= 2).await;

        let tokens = h.mock.tokens();
        for (i, t) in tokens.iter().take(2).enumerate() {
            let t = t.as_deref().expect("token header present");
            assert_ne!(
                t, "stale-token",
                "open {i} replayed the frozen snapshot (the live_062 blackout)"
            );
        }
    }

    /// W14-A1 + W14-A1b (merged_bug_005 red-first, the
    /// `NotLocallyHealable` face): an UNAUTHENTICATED open on a token
    /// WELL WITHIN its TTL does NOT re-mint — the verifier rejected it
    /// for a cause re-signing with the same key cannot heal (revoked
    /// jti, the operator denial; or unknown verify key, the rotation
    /// claim's runtime negative). The re-open carries the same token
    /// and the rejection surfaces honestly through the auth lane.
    ///
    /// Pre-fix RED (the wave-13 unconditional `note_rejected`): the
    /// re-open carried a FRESH jti and the operator's per-jti
    /// revocation was healed around in one cycle.
    #[tokio::test]
    async fn unauthenticated_open_on_healthy_token_does_not_remint() {
        use ed25519_dalek::SigningKey;
        let key = SigningKey::from_bytes(&[0x56; 32]);
        let (token, claims) =
            crate::server::session_jwt::mint_session_jwt(uuid::Uuid::from_u128(0xF00E), &key)
                .expect("mint");
        let source = SessionTokenSource::new(Some((token, claims)), Some(Arc::new(key)));
        let mut h = harness_with_jwt(test_config(), source).await;

        h.mock.unauth_next_opens(2);
        h.mock.push_script(vec![chunk(0, 1)], SessionEnd::Hold);
        h.set.on_started(DRV, EXEC_A);
        let mock = h.mock.clone();
        wait_for("three opens (rejected, rejected, served)", || {
            mock.request_count() >= 3
        })
        .await;

        let tokens = h.mock.tokens();
        let first = tokens[0].as_deref().expect("first open carried a token");
        let second = tokens[1].as_deref().expect("re-open carried a token");
        assert_eq!(
            first, second,
            "a NotLocallyHealable rejection must NOT re-mint: pre-fix this minted a fresh \
             jti and silently healed around the operator's per-jti revocation (merged_bug_005)"
        );
    }

    /// live_062 (WO-S10-6, red-first): a SUSTAINED open-failure
    /// episode injects exactly ONE user-visible notice line into the
    /// build output after `degraded_notice_after`; further failures in
    /// the same episode stay silent (the warn latch's law, mirrored).
    ///
    /// Pre-fix RED (the design's own text, quoted from the deleted
    /// comment at the open-failure arm): "Deliberately NOT surfaced to
    /// the nix client" — no notice existed at any duration; live_062's
    /// users diagnosed "stuck builds" from a silently dark tail while
    /// every build succeeded. Strawman disclosure per the
    /// order-infeasible form (the notice machinery and its witness
    /// land together).
    #[tokio::test]
    async fn sustained_degradation_notices_user_once_per_episode() {
        let mut config = test_config();
        config.degraded_notice_after = Duration::from_millis(150);
        let mut h = harness_with(config).await;
        // Every open fails for the whole test: one episode, many
        // cycles (50 ms backoff → ~3 cycles before the threshold,
        // many after).
        h.mock.fail_next_opens(200);
        h.set.on_started(DRV, EXEC_A);

        let notice = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect("the degradation notice must arrive within the bound")
            .expect("output channel open");
        let text = String::from_utf8(notice.lines[0].clone()).expect("marker is UTF-8");
        assert!(
            text.contains("live log tail degraded for"),
            "notice text: {text}"
        );
        assert!(
            text.contains("store unreachable"),
            "UNAVAILABLE opens are the reachability lane: {text}"
        );

        // One per episode: the episode persists (opens keep failing),
        // so no second notice may arrive.
        let second = tokio::time::timeout(Duration::from_millis(400), h.out_rx.recv()).await;
        assert!(
            second.is_err(),
            "a second notice arrived in the same episode: {second:?}"
        );
    }

    // r[verify sys.recovery.witnessed-clear]
    /// W14-A4 (merged_bug_003 red-first, R34-w(i) — the err_stream
    /// silent dark tail): the store opens successfully and answers
    /// every cycle through err_stream (an in-body Status — the
    /// production refusal design for authorize_tail NotFound, missing
    /// exec row, PG errors). Pre-fix RED: `degraded` was
    /// unconditionally cleared on every successful open
    /// (log_tail.rs:1113-1114), so the err_stream refusal reset the
    /// episode every 1s cycle and neither the 30s notice nor the
    /// per-episode warn could ever fire — a persistently-dark tail
    /// with the open succeeding and zero notice/warn. The test
    /// ASSERTING the notice DOES fire is the RED.
    ///
    /// Post-fix: the episode arms on the first zero-relayed
    /// TransportErr drive, holds across the open-succeeds cycles, and
    /// the notice fires once at the bound with the in-stream-refusal
    /// lane.
    #[tokio::test]
    async fn err_stream_refusal_arms_degradation_and_notices_once() {
        let mut config = test_config();
        config.degraded_notice_after = Duration::from_millis(150);
        let mut h = harness_with(config).await;
        // Every open SUCCEEDS; every stream answers an in-body error
        // before any line — the store's err_stream channel.
        for _ in 0..200 {
            h.mock
                .push_script(vec![], SessionEnd::Error(tonic::Code::NotFound));
        }
        h.set.on_started(DRV, EXEC_A);

        let notice = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect("the err_stream-armed notice must arrive within the bound (merged_bug_003)")
            .expect("output channel open");
        let text = String::from_utf8(notice.lines[0].clone()).expect("marker is UTF-8");
        assert!(
            text.contains("live log tail degraded for"),
            "notice text: {text}"
        );
        assert!(
            text.contains("store refused in-stream"),
            "the err_stream lane must be named: {text}"
        );

        // One per episode: the in-stream refusal persists, no second
        // notice.
        let second = tokio::time::timeout(Duration::from_millis(400), h.out_rx.recv()).await;
        assert!(
            second.is_err(),
            "a second notice arrived in the same episode: {second:?}"
        );
    }

    // r[verify sys.recovery.witnessed-clear]
    /// W14-A5 (merged_bug_003, the occupancy witness + the
    /// false-positive guard): (a) after an err_stream-armed
    /// degradation, the FIRST genuinely relayed line clears the
    /// episode — the next degradation gets its own notice; (b) an
    /// idle-but-healthy open stream (NaturalEnd, zero new lines) does
    /// NOT arm — a quiet build is not degradation.
    #[tokio::test]
    async fn relayed_line_clears_degradation_and_idle_healthy_does_not_arm() {
        let mut config = test_config();
        config.degraded_notice_after = Duration::from_millis(100);
        config.reconnect_backoff = Duration::from_millis(20);
        let mut h = harness_with(config).await;
        // (b) Idle-but-healthy first: a few opens that close cleanly
        // with zero lines (a quiet build between cuts). These must
        // neither arm nor clear — `degraded` stays None.
        for _ in 0..3 {
            h.mock.push_script(vec![], SessionEnd::Close);
        }
        // (a) Then degrade: err_stream refusals arm the episode.
        for _ in 0..12 {
            h.mock
                .push_script(vec![], SessionEnd::Error(tonic::Code::Internal));
        }
        // Then a relayed line clears it.
        h.mock.push_script(vec![chunk(0, 1)], SessionEnd::Close);
        // Then a SECOND degradation episode (must get its own notice).
        for _ in 0..50 {
            h.mock
                .push_script(vec![], SessionEnd::Error(tonic::Code::Internal));
        }
        h.set.on_started(DRV, EXEC_A);

        // First emission must be the notice (idle-healthy did not arm,
        // so the err_stream refusals start the clock; the relayed
        // line below comes AFTER the first notice in this scripting).
        let first = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect("first notice")
            .expect("channel open");
        let text = String::from_utf8(first.lines[0].clone()).unwrap();
        assert!(
            text.contains("live log tail degraded"),
            "first emission must be the degradation notice (idle-healthy must not \
             have armed earlier): {text}"
        );
        // The relayed chunk (clears the episode).
        let lines = recv_lines(&mut h.out_rx, 1).await;
        assert_eq!(lines[0].0, 0, "the relayed line: {lines:?}");
        // The SECOND notice — proves the relayed line cleared the
        // episode (R34-w: the clear is occupancy, not the open).
        let second = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect("second notice (the relayed line cleared the episode)")
            .expect("channel open");
        let text2 = String::from_utf8(second.lines[0].clone()).unwrap();
        assert!(text2.contains("live log tail degraded"), "second: {text2}");
    }

    // r[verify sys.recovery.witnessed-clear]
    /// merged_bug_006 (the §Multi-axis-fn sibling-arm hole next to
    /// W14-A5): a drive that RELAYS lines and then withholds at a
    /// forward jump (`DriveEnd::Gap`) is witnessed work — the
    /// degradation episode MUST clear. Pre-fix the witnessed-work
    /// clear lived only in the `DriveEnd::Ended` arm; the `Gap` arm
    /// left `degraded_notice_sent = true`, so the second episode below
    /// was muted forever (timeout on the second notice).
    #[tokio::test]
    async fn serve_then_gap_drive_clears_degradation_episode() {
        let mut config = test_config();
        config.degraded_notice_after = Duration::from_millis(100);
        config.reconnect_backoff = Duration::from_millis(20);
        // The serve-then-gap drive must take the WITHHOLD branch (the
        // `Sighting::First` re-open chance, not the budget-edge
        // immediate accept) — the grace clock is unarmed in this test,
        // but pin a generous budget so that stays the only reason.
        config.terminal_grace = Duration::from_secs(5);
        let mut h = harness_with(config).await;
        // Episode 1: err_stream refusals arm the clock.
        for _ in 0..12 {
            h.mock
                .push_script(vec![], SessionEnd::Error(tonic::Code::Internal));
        }
        // The serve-then-gap drive: lines 0-4 reach the output (the
        // user sees them), then a forward jump to line 100 returns
        // `DriveEnd::Gap`. THIS is the witnessed-work clear.
        h.mock
            .push_script(vec![chunk(0, 5), chunk(100, 5)], SessionEnd::Hold);
        // Episode 2: a fresh degradation that MUST get its own notice.
        for _ in 0..50 {
            h.mock
                .push_script(vec![], SessionEnd::Error(tonic::Code::Internal));
        }
        h.set.on_started(DRV, EXEC_A);

        let first = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect("first notice")
            .expect("channel open");
        let text = String::from_utf8(first.lines[0].clone()).unwrap();
        assert!(
            text.contains("live log tail degraded"),
            "first emission must be the degradation notice: {text}"
        );
        // The relayed lines from the serve-then-gap drive.
        let lines = recv_lines(&mut h.out_rx, 5).await;
        assert_eq!(lines.last().map(|l| l.0), Some(4), "lines 0-4: {lines:?}");
        // The SECOND notice — proves a relayed-then-Gap drive cleared
        // the episode (merged_bug_006: the Gap arm bypassed the clear
        // and `degraded_notice_sent` stayed latched true).
        let second = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect(
                "second notice within 2s (a serve-then-gap drive is witnessed \
                 work and MUST clear the degradation episode — merged_bug_006)",
            )
            .expect("channel open");
        let text2 = String::from_utf8(second.lines[0].clone()).unwrap();
        assert!(text2.contains("live log tail degraded"), "second: {text2}");
    }

    /// live_062: the user-facing open-failure lane projection is the
    /// closed two-letter alphabet — UNAUTHENTICATED is the token lane
    /// (the pre-fix text blamed store reachability for it, which
    /// misdiagnosed the blackout), everything else the reachability
    /// lane.
    #[test]
    fn open_failed_lane_cells() {
        use super::open_failed_lane;
        assert_eq!(
            open_failed_lane(tonic::Code::Unauthenticated),
            "session token rejected"
        );
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::NotFound,
            tonic::Code::DeadlineExceeded,
            tonic::Code::Internal,
        ] {
            assert_eq!(open_failed_lane(code), "store unreachable", "{code:?}");
        }
    }

    /// live_062 ((lllll) + the axis pin): `TAIL_RECONNECT_REASONS` —
    /// the boot-seed axis behind RioGatewayLogTailDegraded — IS the
    /// image of `reconnect_reason` over every cause that can reach a
    /// Reopen verdict. A new stop cause that mints a new reason string
    /// fails here instead of shipping a birth-gapped series.
    #[test]
    fn tail_reconnect_reason_axis_matches_the_emit_law() {
        use super::reconnect_reason;
        // The reopen-reachable causes (Orphaned/PermanentErr exit
        // unconditionally — kernel law, kani-pinned).
        let image: std::collections::BTreeSet<&str> = [
            TailStopCause::NaturalEnd,
            TailStopCause::TransportErr,
            TailStopCause::OpenFailed,
            TailStopCause::GapObserved,
        ]
        .into_iter()
        .map(reconnect_reason)
        .collect();
        let seeded: std::collections::BTreeSet<&str> =
            crate::TAIL_RECONNECT_REASONS.iter().copied().collect();
        assert_eq!(
            image, seeded,
            "the seeded reason axis must equal reconnect_reason's image"
        );
    }

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

    /// Collect every tagged chunk that arrives within `quiet` of the
    /// last one (or the start), flattened to `(line_number, text)`.
    /// For stream-content assertions where ABSENCE matters (W10-BF):
    /// the window bounds how long a straggler has to splice.
    async fn collect_window(
        rx: &mut mpsc::Receiver<TaggedLogChunk>,
        quiet: Duration,
    ) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        while let Ok(Some(tagged)) = tokio::time::timeout(quiet, rx.recv()).await {
            for (i, line) in tagged.lines.iter().enumerate() {
                out.push((
                    tagged.first_line_number + i as u64,
                    // Fixture lines and markers are always UTF-8.
                    String::from_utf8(line.clone()).expect("test lines are UTF-8"),
                ));
            }
        }
        out
    }

    /// W10-BF (bug_168, sys.epilogue.supersession): a re-dispatch
    /// replaces the relay for a NEW execution on the SAME output
    /// channel. The superseded relay's withheld-gap state must be
    /// DISCARDED — pre-fix its `PendingGapCell::drop` try_sent the
    /// dead execution's withheld lines plus a false "durable log gap"
    /// marker into the retry's client-visible stream at an arbitrary
    /// later poll (`TaggedLogChunk` carries no exec_id, so the
    /// consumer cannot filter). The must-DISCLOSE face is asserted in
    /// the same test: at build terminus the new relay's own pending
    /// gap still discloses (marker + withheld lines) — supersession
    /// narrows the disposition, it does not erase the law.
    // r[verify sys.epilogue.supersession]
    #[tokio::test]
    async fn redispatch_discards_superseded_relay_state_terminus_still_discloses() {
        let mut h = harness().await;
        // EXEC_A session 1: a chunk past the floor (lines 5-6) — the
        // jump records a pending gap with the lines WITHHELD, then the
        // subscription re-opens at the unchanged floor.
        h.mock.push_script(vec![chunk(5, 2)], SessionEnd::Hold);
        // EXEC_A session 2 (the gap's one re-open chance): opens and
        // serves NOTHING (held) — the pending stays recorded.
        h.mock.push_script(vec![], SessionEnd::Hold);
        h.set.on_started(DRV, EXEC_A);
        wait_for("the gap re-open to be requested", || {
            h.mock.request_count() == 2
        })
        .await;

        // Re-dispatch: the derivation restarts under EXEC_B. The old
        // relay holds withheld lines 5-6 and an undisclosed 0..5 hole.
        // (EXEC_B's chunks are stamped with ITS exec — the relay drops
        // foreign-exec chunks, which is orthogonal to this law: the
        // splice rides the OUTPUT channel, past that filter.)
        let chunk_b = TailLogChunk {
            exec_id: EXEC_B.to_string(),
            ..chunk(0, 2)
        };
        h.mock.push_script(vec![chunk_b], SessionEnd::Hold);
        h.set.on_started(DRV, EXEC_B);
        wait_for("the replacement subscription to open", || {
            h.mock.request_count() == 3
        })
        .await;

        // The retry stream: exactly EXEC_B's lines. No superseded
        // lines (numbers >= 5), no gap marker — pre-fix the aborted
        // relay's Drop spliced both in (the red).
        let lines = collect_window(&mut h.out_rx, Duration::from_millis(400)).await;
        let numbers: Vec<u64> = lines.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            numbers,
            vec![0, 1],
            "the retry stream must carry ONLY the new execution's lines; \
             superseded withheld lines / a false gap marker spliced in \
             (the bug_168 red): {lines:?}"
        );
        assert!(
            !lines.iter().any(|(_, t)| t.contains("durable log gap")),
            "no gap marker may be minted for the superseded execution: {lines:?}"
        );

        // The must-DISCLOSE face: EXEC_B's own stream now jumps (lines
        // 10), recording a fresh pending gap...
        h.mock.push_script(vec![chunk(10, 1)], SessionEnd::Hold);
        // Session 3 (EXEC_B) is HELD with nothing withheld, so a
        // second re-dispatch (B -> A') is the cheapest path to a relay
        // that holds a fresh pending gap at terminus: it also
        // exercises supersession over an EMPTY cell (a no-op discard).
        h.set.on_started(DRV, EXEC_A);
        wait_for("the disclosure-face subscription to open", || {
            h.mock.request_count() == 4
        })
        .await;
        // Session 4 served chunk(10,1) against a fresh floor 0: gap
        // 0..10 recorded, line 10 withheld; session 5 (the re-open) is
        // unscripted (served as an empty close by the mock).
        wait_for("the gap re-open of the disclosure face", || {
            h.mock.request_count() >= 5
        })
        .await;

        // Build terminus: hard-cancel + bound-join (the build.rs
        // discipline). The pending gap MUST disclose into the channel.
        let handles = h.set.abort_all();
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            futures_util::future::join_all(handles),
        )
        .await;
        let disclosed = collect_window(&mut h.out_rx, Duration::from_millis(400)).await;
        assert!(
            disclosed.iter().any(|(_, t)| t.contains("durable log gap")),
            "terminus must still disclose the live relay's pending gap \
             (the must-disclose face survives the supersession close): {disclosed:?}"
        );
        assert!(
            disclosed.iter().any(|(n, _)| *n == 10),
            "terminus must still flush the live relay's withheld lines: {disclosed:?}"
        );
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
    // r[verify gw.tail.truncation-disclosed+2]
    /// W11-BJ (bug_121): disclosure totality quantifies over the LOSS
    /// surface, not the fetch surface — the spec-mandated hard exit at
    /// grace expiry (`tail-grace-drain`: exit exactly at expiry) cuts
    /// store-served backlog mid-replay, and pre-fix that was the ONE
    /// loss path in the module with zero disclosure (the exit flush is
    /// scoped to fetched-but-withheld pending-gap lines by contract).
    /// The kernel holds `served_complete` at the grace exit, so the
    /// exit verdict carries the truncation obligation typed:
    /// grace-expiry with `served_complete == false` writes the
    /// truncation marker (gap-vocabulary form) as the FINAL write
    /// before closing. The grace itself is NOT extended.
    ///
    /// Pre-fix red: the post-exit recv times out — close with no
    /// marker and lines undelivered (`panicked at 'the grace exit
    /// must disclose the store-served truncation as its final write
    /// (pre-fix: silent cut)'`).
    #[tokio::test]
    async fn grace_exit_discloses_store_served_truncation() {
        let mut h = harness().await;
        // Two lines served, then the stream holds forever with the
        // served log INCOMPLETE (no final chunk, is_complete never
        // true): a wedged replica mid-replay of durable backlog.
        h.mock.push_script(vec![chunk(0, 2)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);
        let _ = recv_lines(&mut h.out_rx, 2).await;
        h.set.on_terminal(DRV);

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

        // The truncation disclosure is the final write.
        let marker = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect(
                "the grace exit must disclose the store-served truncation \
                 as its final write (pre-fix: silent cut)",
            )
            .expect("output channel open");
        assert_eq!(
            marker.first_line_number, 2,
            "the marker names the first unserved line"
        );
        let text = String::from_utf8(marker.lines[0].clone()).expect("marker is UTF-8");
        assert!(
            text.contains("rio:") && text.contains("not served"),
            "gap-vocabulary truncation marker; got {text:?}"
        );

        h.set.abort_all();
    }

    /// W11-BJ companion (the negative cell): a grace exit whose served
    /// log IS complete (the store's own claim) owes no truncation
    /// marker — nothing was cut.
    // r[verify gw.tail.truncation-disclosed+2]
    #[tokio::test]
    async fn grace_exit_with_complete_serve_stays_silent() {
        let mut h = harness().await;
        // Two lines + the completeness claim, then hold (the stream
        // outlives its own final message; the grace still cuts it).
        h.mock
            .push_script(vec![chunk(0, 2), final_chunk(2, true)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);
        let _ = recv_lines(&mut h.out_rx, 2).await;
        h.set.on_terminal(DRV);

        let handle = h
            .set
            .tasks
            .get(DRV)
            .expect("the subscription is still tracked")
            .task
            .abort_handle();
        wait_for("the subscription task to exit", || handle.is_finished()).await;

        // No further writes: a complete serve cut at grace owes
        // nothing.
        let extra = tokio::time::timeout(Duration::from_millis(300), h.out_rx.recv()).await;
        assert!(
            extra.is_err(),
            "no truncation marker for a served-complete exit; got {extra:?}"
        );

        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_194's dedicated gateway twin + W13-AA (bug_038), at
    /// PRODUCTION PARAMETER ORDERING (open_bound 2000 ms > grace
    /// 400 ms — the shipped 5x ratio): a store that ACCEPTS the TCP
    /// connection but never answers `TailLog` (the open future itself
    /// hangs) must not park the relay past the drain deadline. The
    /// first hung open is cut by the drain EDGE (terminal signal,
    /// raced via the bounded open's abort arm); each subsequent hung
    /// open is cut by the DEADLINE-TYPED bound — the per-attempt
    /// open_bound clamped to the REMAINING GRACE — so exit-at-expiry
    /// holds even though one bare open_bound is 5x the whole grace.
    ///
    /// RECORDED RED (pre-C1 shape, `bounded_open` neutered to a bare
    /// `client.tail_log(request).await`): the task never finishes —
    /// the relay hung on the open await forever.
    ///
    /// W13-AA RED (2026-06-12, the clamp strawmanned to
    /// `OpenDeadline::within(config.open_bound, None)` — the pre-fix
    /// bare fixed bound, at production ordering): the post-edge
    /// re-open ran its FULL 2000 ms bound — 5x past the 400 ms grace
    /// — and `wait_for("the relay to exit by the drain deadline")`
    /// panicked at its 2 s cap with the relay still parked in the
    /// open. POST-FIX GREEN: exit at ~450 ms past the edge, strictly
    /// inside one open_bound (the structural separation below).
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
        // at min(open_bound, remaining grace) along the way.
        h.set.on_terminal(DRV);
        let edge_at = std::time::Instant::now();
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
        // W13-AA, the budget separation (structural, 4x headroom
        // under load): the exit lands STRICTLY inside one bare
        // open_bound past the edge. Pre-fix this is impossible — the
        // post-edge hung open alone holds the loop for the full
        // 2000 ms before the grace expiry can even be consulted (the
        // 5-6x exit-at-expiry breach the spec'd budget forbids).
        let exited_after = edge_at.elapsed();
        assert!(
            exited_after < Duration::from_millis(2000),
            "exit-at-expiry must not wait out a bare open_bound: the hung re-open ran unclamped past the remaining grace (exited {exited_after:?} after the drain edge)"
        );

        // The exit came from the law (grace expiry), not from a lucky
        // single attempt: the hung opens were retried and cut at least
        // once after the drain edge (grace 400 ms with backoff 50 ms
        // and the grace-clamped open leaves room for >=2 further
        // attempts).
        assert!(
            h.mock.request_count() >= 2,
            "expected re-opens after the drain edge, got {}",
            h.mock.request_count()
        );
        // And no log LINE was ever relayed — the store never served a
        // chunk. bug_121: the grace exit over the never-served (hence
        // incomplete) log now writes exactly one truncation
        // disclosure as its final write — the dark live tail is
        // disclosed, not silent.
        let only = h.out_rx.try_recv().expect("the truncation disclosure");
        let text = String::from_utf8(only.lines[0].clone()).expect("marker is UTF-8");
        assert!(
            text.contains("rio:") && text.contains("not served"),
            "the one write is the truncation marker; got {text:?}"
        );
        assert!(
            h.out_rx.try_recv().is_err(),
            "nothing beyond the disclosure"
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
            degraded_notice_after: Duration::from_secs(30),
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
            degraded_notice_after: Duration::from_secs(30),
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

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_023 leg 1 (divergent-split second sighting): the
    /// retry stream re-serves the SAME hole start but a DIFFERENT
    /// split ([25,35) instead of [20,30)). The first sighting's
    /// withheld lines [20,25) — which the gateway fetched and may be
    /// the only remaining copy anywhere — must flush before the new
    /// shape is adopted, not be overwritten and then affirmatively
    /// declared "missing" by a widened marker. Recorded red (pre-fix):
    /// line 20 never reached the output; the marker claimed
    /// `lines 10-24 missing` over content the relay held in hand.
    #[tokio::test]
    async fn divergent_second_sighting_flushes_old_withheld_lines() {
        let mut h = harness().await;
        // Stream 1: prefix [0,10) then jump to [20,30) -> withhold.
        h.mock
            .push_script(vec![chunk(0, 10), chunk(20, 10)], SessionEnd::Close);
        // Stream 2 (the one re-open chance): the store's view changed —
        // only [25,35) is durable now; gap_from is still 10.
        h.mock.push_script(vec![chunk(25, 10)], SessionEnd::Close);
        h.set.on_started(DRV, EXEC_A);

        let prefix = recv_lines(&mut h.out_rx, 10).await;
        assert_eq!(prefix[9].0, 9, "prefix intact");

        h.set.on_terminal(DRV);

        // Post-fix order: marker 10-19, old withheld 20-29, then the
        // re-visited fresh chunk's residual 30-34.
        let rest = recv_lines(&mut h.out_rx, 16).await;
        assert!(
            rest.iter().any(|(n, _)| *n == 20),
            "the first sighting's withheld lines must not be destroyed: {rest:?}"
        );
        assert!(
            rest[0].1.contains("lines 10-19 missing"),
            "the marker covers only the REAL hole, not fetched content: {:?}",
            rest[0]
        );
        assert!(
            rest.iter().any(|(n, _)| *n == 34),
            "the divergent chunk's residual still relays: {rest:?}"
        );
        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// RED (merged_bug_111): a NON-COOPERATIVE termination — the
    /// build terminus' / re-dispatch's `JoinHandle::abort()` — lands
    /// while withheld PendingGap lines are recorded. Pre-fix, the
    /// abort dropped the run_tail future mid-await and the cell's
    /// state simply ceased to exist: no marker, no lines, no
    /// disclosure (the flush-total law enumerated only cooperative
    /// exits; recorded red: `timed out after 0 of 6 lines`). The
    /// Drop-disclosure backstop now fires on EVERY cancellation —
    /// including any future abort site — and try_sends marker +
    /// withheld into the still-open output channel.
    #[tokio::test]
    async fn abort_discloses_withheld_gap_lines() {
        let mut h = harness().await;
        // Stream 1 serves the prefix and a forward jump (withhold),
        // then parks. The gap's one re-open chance (request 2) hits
        // the exhausted-script empty close, so the pending stays
        // recorded while the subscription cycles its backoff loop.
        h.mock
            .push_script(vec![chunk(0, 5), chunk(100, 5)], SessionEnd::Hold);
        h.set.on_started(DRV, EXEC_A);

        let prefix = recv_lines(&mut h.out_rx, 5).await;
        assert_eq!(prefix[4].0, 4, "prefix intact");
        // The re-open proves the pending gap is recorded (the drive
        // returned DriveEnd::Gap and run_tail re-opened at the gap).
        let mock = h.mock.clone();
        wait_for("the gap re-open request", || mock.request_count() >= 2).await;

        // NON-cooperative termination: no drain signal, no grace —
        // the hard abort every cooperative-exit law is blind to.
        h.set.abort_all();

        let rest = recv_lines(&mut h.out_rx, 6).await;
        assert_eq!(
            rest[0],
            (
                5,
                "*** rio: lines 5-99 missing (durable log gap) ***".to_string()
            ),
            "the abort backstop discloses the hole: {rest:?}"
        );
        assert_eq!(rest[1].0, 100, "the withheld lines reach the output");
        assert_eq!(rest[5].0, 104);
    }

    // r[verify store.log.tail-grace-drain+2]
    /// RED (merged_bug_020): the divergent second sighting begins
    /// INSIDE the pending hole (partial backfill — fresh@12 against
    /// pending [10,20) with withheld [20,25)). Pre-fix, the Divergent
    /// arm flushed the OLD pending FIRST (full-span marker 10-19,
    /// floor advanced to 25) and only THEN re-visited the fresh chunk
    /// — wholly below the advanced floor, so the healed lines 12-19
    /// were silently skipped (`timed out after 7 of 15 lines`; the
    /// marker also over-claimed a span the store had just served).
    /// The reconcile now runs BEFORE any floor advancement: residual
    /// marker 10-11, the healing lines 12-24, withheld proven
    /// duplicate.
    #[tokio::test]
    async fn divergent_backfill_relays_healed_lines_before_floor_advance() {
        let mut h = harness().await;
        // Stream 1: prefix [0,10) then jump to [20,25) -> withhold.
        h.mock
            .push_script(vec![chunk(0, 10), chunk(20, 5)], SessionEnd::Close);
        // Stream 2 (the one re-open chance): the store's view now
        // starts INSIDE the old hole and covers through the withheld
        // span: [12,25).
        h.mock.push_script(vec![chunk(12, 13)], SessionEnd::Close);
        h.set.on_started(DRV, EXEC_A);

        let prefix = recv_lines(&mut h.out_rx, 10).await;
        assert_eq!(prefix[9].0, 9, "prefix intact");

        h.set.on_terminal(DRV);

        // marker 10-11 + lines 12..25 = 14 entries.
        let rest = recv_lines(&mut h.out_rx, 14).await;
        assert_eq!(
            rest[0],
            (
                10,
                "*** rio: lines 10-11 missing (durable log gap) ***".to_string()
            ),
            "the marker covers only the residual prefix, not healed content: {rest:?}"
        );
        assert!(
            rest.iter().any(|(n, _)| *n == 12),
            "the healing lines inside the old hole must relay: {rest:?}"
        );
        assert!(
            rest.iter().any(|(n, _)| *n == 24),
            "the tail relays once (withheld proven duplicate): {rest:?}"
        );
        let line_12_count = rest.iter().filter(|(n, _)| *n == 12).count();
        let line_20_count = rest.iter().filter(|(n, _)| *n == 20).count();
        assert_eq!(line_12_count, 1, "no double relay: {rest:?}");
        assert_eq!(
            line_20_count, 1,
            "withheld overlap trimmed, not re-sent: {rest:?}"
        );
        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_020 short-backfill leg: the fresh divergent chunk
    /// heals only [12,16) of the pending [10,20) — it stops SHORT of
    /// the withheld start. Order owed: residual marker 10-11, healing
    /// lines 12-15, second residual marker 16-19, withheld 20-24.
    /// Every fetched line relays; both residual holes get exactly one
    /// marker each.
    #[tokio::test]
    async fn divergent_short_backfill_marks_both_residual_holes() {
        let mut h = harness().await;
        h.mock
            .push_script(vec![chunk(0, 10), chunk(20, 5)], SessionEnd::Close);
        // Fresh covers [12,16): inside the hole, short of the withheld.
        h.mock.push_script(vec![chunk(12, 4)], SessionEnd::Close);
        h.set.on_started(DRV, EXEC_A);

        let prefix = recv_lines(&mut h.out_rx, 10).await;
        assert_eq!(prefix[9].0, 9, "prefix intact");

        h.set.on_terminal(DRV);

        // marker + 4 lines + marker + 5 lines = 11 entries.
        let rest = recv_lines(&mut h.out_rx, 11).await;
        assert_eq!(
            rest[0],
            (
                10,
                "*** rio: lines 10-11 missing (durable log gap) ***".to_string()
            ),
            "first residual marker: {rest:?}"
        );
        assert_eq!(rest[1].0, 12, "healing lines follow: {rest:?}");
        assert_eq!(rest[4].0, 15);
        assert_eq!(
            rest[5],
            (
                16,
                "*** rio: lines 16-19 missing (durable log gap) ***".to_string()
            ),
            "second residual marker between fresh end and withheld: {rest:?}"
        );
        assert_eq!(rest[6].0, 20, "withheld flushes after its marker: {rest:?}");
        assert_eq!(rest[10].0, 24);
        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_023 leg 2 (partial-heal void): the retry stream
    /// serves only PART of the hole ([5,7) of [5,10)). The hole
    /// shrinks; the withheld lines [10,15) must survive to the exit
    /// flush with a marker covering the residual hole [7,10).
    /// Recorded red (pre-fix): the log silently truncated at line 6 —
    /// neither the withheld lines nor any marker reached the output
    /// (`timed out after 7 of 13 lines`).
    #[tokio::test]
    async fn partial_heal_keeps_withheld_lines_and_residual_marker() {
        let mut h = harness().await;
        // Stream 1: prefix [0,5) then jump to [10,15) -> withhold.
        h.mock
            .push_script(vec![chunk(0, 5), chunk(10, 5)], SessionEnd::Close);
        // Stream 2: partial heal — only [5,7) is served.
        h.mock.push_script(vec![chunk(5, 2)], SessionEnd::Close);
        h.set.on_started(DRV, EXEC_A);

        let head = recv_lines(&mut h.out_rx, 7).await;
        assert_eq!(head[6].0, 6, "prefix + partial heal relayed");

        h.set.on_terminal(DRV);

        let rest = recv_lines(&mut h.out_rx, 6).await;
        assert!(
            rest[0].1.contains("lines 7-9 missing"),
            "the residual hole is disclosed, shrunk past the heal: {:?}",
            rest[0]
        );
        assert!(
            rest.iter().any(|(n, _)| *n == 10) && rest.iter().any(|(n, _)| *n == 14),
            "the withheld lines survive a partial heal: {rest:?}"
        );
        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_023 conservation corollary (exact heal): the retry
    /// stream serves the hole EXACTLY ([5,10)). No hole remains, so
    /// the withheld lines relay as the ordinary continuation — no
    /// marker, nothing lost, nothing duplicated.
    #[tokio::test]
    async fn exact_heal_relays_withheld_without_marker() {
        let mut h = harness().await;
        h.mock
            .push_script(vec![chunk(0, 5), chunk(10, 5)], SessionEnd::Close);
        h.mock.push_script(vec![chunk(5, 5)], SessionEnd::Close);
        h.set.on_started(DRV, EXEC_A);
        h.set.on_terminal(DRV);

        let all = recv_lines(&mut h.out_rx, 15).await;
        assert_eq!(
            all.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            (0..15).collect::<Vec<_>>(),
            "exact heal: contiguous relay, no loss, no duplicates"
        );
        assert!(
            all.iter().all(|(_, t)| !t.contains("missing")),
            "no marker for a healed hole: {all:?}"
        );
        h.set.abort_all();
    }

    // r[verify store.log.tail-grace-drain+2]
    /// merged_bug_023 conservation corollary (over-heal): the retry
    /// stream serves PAST the withheld start ([5,12) over a [5,10)
    /// hole with withheld [10,15)). The duplicate withheld prefix is
    /// trimmed; the suffix [12,15) relays; no marker, no regression
    /// of the dedup floor.
    #[tokio::test]
    async fn over_heal_trims_withheld_prefix_and_relays_suffix() {
        let mut h = harness().await;
        h.mock
            .push_script(vec![chunk(0, 5), chunk(10, 5)], SessionEnd::Close);
        h.mock.push_script(vec![chunk(5, 7)], SessionEnd::Close);
        h.set.on_started(DRV, EXEC_A);
        h.set.on_terminal(DRV);

        let all = recv_lines(&mut h.out_rx, 15).await;
        assert_eq!(
            all.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            (0..15).collect::<Vec<_>>(),
            "over-heal: contiguous, the overlap deduplicated exactly once"
        );
        assert!(
            all.iter().all(|(_, t)| !t.contains("missing")),
            "no marker when the store served the whole hole: {all:?}"
        );
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
                        degraded_notice_after: Duration::from_secs(30),
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
            SessionTokenSource::none(),
            DRV.to_string(),
            EXEC_A.to_string(),
            super::RelayWiring {
                out_tx,
                drain: drain_rx,
                disposition: super::RelayDisposition::disclose_at_drop(),
            },
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
    /// W11-BK (bug_123, the R27 lens extension): the non-cooperative
    /// Drop epilogue's two-message disclosure is TRANSACTIONAL —
    /// both permits reserved before either message sends. Pre-fix the
    /// two independent try_sends made the partial-failure lattice
    /// reachable: at the lattice's own cell (exactly one slot free)
    /// the marker landed and the withheld chunk vanished — and under
    /// a consumer recv between the two sends, the inverse split
    /// (withheld-without-marker: the hole never disclosed) landed.
    /// Post-fix the cell yields NEITHER (the whole-drop residual the
    /// doc already prices) and two free slots yield BOTH.
    ///
    /// Pre-fix red, verbatim:
    ///   one-slot-free drop must send both-or-neither (pre-fix: the
    ///   partial split — 1 message)
    #[tokio::test]
    async fn drop_disclosure_is_both_or_neither() {
        let pending = || super::PendingGap {
            gap_from: 5,
            gap_until: 8,
            withheld: super::TaggedLogChunk {
                derivation_path: DRV.to_string(),
                first_line_number: 8,
                lines: vec![b"line-8".to_vec()],
            },
            next_line: 9,
        };

        // Cell 1: exactly ONE slot free — both-or-neither means
        // NEITHER message lands (the priced whole-drop residual).
        let (out_tx, mut out_rx) = mpsc::channel(2);
        out_tx
            .try_send(super::TaggedLogChunk {
                derivation_path: DRV.to_string(),
                first_line_number: 0,
                lines: vec![b"filler".to_vec()],
            })
            .expect("fill to one-slot-free");
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(pending());
        drop(cell);
        drop(out_tx);
        let mut got = Vec::new();
        while let Some(c) = out_rx.recv().await {
            got.push(c);
        }
        assert_eq!(
            got.len() - 1, // minus the filler
            0,
            "one-slot-free drop must send both-or-neither (pre-fix: the \
             partial split — {} message)",
            got.len() - 1
        );

        // Cell 2: TWO slots free — both messages land, in order
        // (marker then withheld).
        let (out_tx, mut out_rx) = mpsc::channel(2);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(pending());
        drop(cell);
        drop(out_tx);
        let marker = out_rx.recv().await.expect("the marker");
        assert!(
            String::from_utf8(marker.lines[0].clone())
                .expect("utf8")
                .contains("missing"),
            "marker first"
        );
        let withheld = out_rx.recv().await.expect("the withheld lines");
        assert_eq!(withheld.first_line_number, 8);
        assert!(out_rx.recv().await.is_none());

        // Cell 3: marker-only shape (no withheld lines) needs only
        // one permit — one free slot serves it.
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        let mut p = pending();
        p.withheld.lines.clear();
        cell.record_first(p);
        drop(cell);
        drop(out_tx);
        let marker = out_rx.recv().await.expect("the lone marker");
        assert_eq!(marker.first_line_number, 5);
        assert!(out_rx.recv().await.is_none());
    }

    // ------------------------------------------------------------------
    // W12-AT (bug_122, R32 banner): the taken-in-flight residency
    // ------------------------------------------------------------------

    /// Drive `fut` exactly one `poll` with a no-op waker: the future
    /// advances to its first pending await and STOPS — the
    /// deterministic stand-in for "a terminus `abort_all` lands while
    /// the consume is parked at this send". Dropping the future after
    /// a single poll IS the cancellation (tokio aborts by dropping the
    /// task's future at a yield point).
    fn poll_once<F: std::future::Future>(fut: &mut std::pin::Pin<Box<F>>) {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "the consume must park at the choreographed send"
        );
    }

    fn taken_window_pending(lines: &[&str]) -> super::PendingGap {
        super::PendingGap {
            gap_from: 5,
            gap_until: 8,
            withheld: super::TaggedLogChunk {
                derivation_path: DRV.to_string(),
                first_line_number: 8,
                lines: lines.iter().map(|l| l.as_bytes().to_vec()).collect(),
            },
            next_line: 8 + lines.len() as u64,
        }
    }

    /// W12-AT window 1 — the flush consume, aborted at the WITHHELD
    /// send (the worst case: marker delivered, lines gone).
    /// Proposition: no abort window exists in which withheld lines die
    /// undisclosed — the disclosure obligation travels WITH the lines
    /// (in-cell before the take, Drop-armed while taken-in-flight).
    ///
    /// Choreography (fully deterministic, no runtime races): a 1-slot
    /// channel parks the flush at its second send with the marker
    /// already delivered; the test then frees the slot WITHOUT polling
    /// the consume again and drops the parked future — exactly what a
    /// terminus `abort_all` does to the relay task.
    ///
    /// Pre-fix red (the wave-12 W12-AT capture): the taken
    /// `PendingGap` is a plain local of the dropped future and
    /// `PendingGapCell::drop` sees `None` —
    ///   `marker-delivered-lines-gone: the withheld lines must survive
    ///   an abort parked at the withheld send (got [])`.
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn flush_abort_at_withheld_send_still_discloses_lines() {
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(taken_window_pending(&["line-8", "line-9"]));
        let mut last_relayed: Option<u64> = None;
        let mut floor: Option<u64> = None;
        {
            let mut fut = Box::pin(super::flush_pending_gap(
                &mut cell,
                &mut last_relayed,
                &mut floor,
            ));
            // One poll: the marker send completes synchronously (one
            // slot free), the withheld send parks (channel full).
            poll_once(&mut fut);
            // Free the slot, then "abort": drop the parked consume.
            let marker = out_rx.try_recv().expect("the marker landed first");
            assert!(
                String::from_utf8(marker.lines[0].clone())
                    .expect("utf8")
                    .contains("missing"),
                "the first write is the gap marker"
            );
        }
        // The cell survived the abort (it lives in `run_tail`, not the
        // consume) — at terminus it drops too.
        drop(cell);
        drop(out_tx);
        let mut got = Vec::new();
        while let Some(c) = out_rx.recv().await {
            got.extend(
                c.lines
                    .iter()
                    .map(|l| String::from_utf8(l.clone()).expect("test lines are UTF-8")),
            );
        }
        assert_eq!(
            got,
            vec!["line-8".to_string(), "line-9".to_string()],
            "marker-delivered-lines-gone: the withheld lines must survive an \
             abort parked at the withheld send (got {got:?})"
        );
    }

    /// W12-AT window 2 — the divergent-backfill consume, aborted
    /// mid-choreography (after the residual marker and the healing
    /// lines, parked at the withheld continuation). Same proposition,
    /// at the four-send consume's deepest window.
    ///
    /// Pre-fix red: `reconcile_backfill` owns the pending BY VALUE —
    /// dropping the parked future destroys the withheld lines and the
    /// un-disclosed second residual; the channel drains to the two
    /// delivered messages only.
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn backfill_abort_at_withheld_send_still_discloses_remainder() {
        // Pending hole [5, 8) with withheld [8, 10); a divergent fresh
        // sighting serves [6, 7) — partial backfill: marker [5, 6),
        // fresh line 6, then (parked) the second residual [7, 8) and
        // the withheld lines.
        let (out_tx, mut out_rx) = mpsc::channel(2);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(taken_window_pending(&["line-8", "line-9"]));
        let armed = cell.take_armed().expect("recorded");
        let fresh = super::TaggedLogChunk {
            derivation_path: DRV.to_string(),
            first_line_number: 6,
            lines: vec![b"line-6".to_vec()],
        };
        let mut last_relayed: Option<u64> = None;
        let mut floor: Option<u64> = None;
        {
            let mut fut = Box::pin(super::reconcile_backfill(
                armed,
                fresh,
                7,
                &out_tx,
                &mut last_relayed,
                &mut floor,
            ));
            // One poll: residual marker [5,6) and the healing line
            // fill both slots; the consume parks at the second
            // residual marker send.
            poll_once(&mut fut);
            let m1 = out_rx.try_recv().expect("residual prefix marker");
            assert_eq!(m1.first_line_number, 5);
            let healing = out_rx.try_recv().expect("the healing line");
            assert_eq!(healing.first_line_number, 6);
        }
        drop(cell);
        drop(out_tx);
        let mut got = Vec::new();
        while let Some(c) = out_rx.recv().await {
            for l in &c.lines {
                got.push(String::from_utf8(l.clone()).expect("test lines are UTF-8"));
            }
        }
        assert!(
            got.iter().any(|l| l.contains("lines 7-7 missing"))
                && got.iter().any(|l| l == "line-8")
                && got.iter().any(|l| l == "line-9"),
            "an abort parked mid-backfill must still disclose the residual \
             hole and the withheld lines (got {got:?})"
        );
    }

    /// W12-AT window 3 — the full-heal consume: `on_serve` takes the
    /// pending and hands the withheld continuation to the caller, who
    /// holds it across the serve-chunk send. An abort parked there
    /// destroyed the continuation while the cell's Drop saw `None`.
    ///
    /// Pre-fix red: the heal returns the suffix as a BARE chunk; the
    /// simulated abort (dropping it) leaves nothing armed —
    ///   `the heal continuation must survive an abort at the serve
    ///   send (got [])`.
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn heal_abort_at_serve_send_still_discloses_continuation() {
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(taken_window_pending(&["line-8", "line-9"]));
        // A serve advances the floor to 9: covers the hole [5,8) and
        // the first withheld line — the continuation is line-9.
        let heal = cell.on_serve(9);
        match heal {
            super::ServeHeal::Healed(taken) => {
                // The simulated abort: the caller's local (the taken
                // continuation) is destroyed at the parked serve send.
                drop(taken);
            }
            _ => panic!("a floor at 9 fully heals the [5,8) hole"),
        }
        drop(cell);
        drop(out_tx);
        let mut got = Vec::new();
        while let Some(c) = out_rx.recv().await {
            for l in &c.lines {
                got.push(String::from_utf8(l.clone()).expect("test lines are UTF-8"));
            }
        }
        assert!(
            got.iter().any(|l| l == "line-9"),
            "the heal continuation must survive an abort at the serve send \
             (got {got:?})"
        );
    }

    /// W13-Z window 4 — the PARTIAL-heal consume (merged_bug_025, the
    /// R32 discharge escape repaired): `on_serve`'s partial arm used
    /// to discharge the hole-prefix obligation by bare field mutation
    /// (`p.gap_from = next_line`) BEFORE the caller's serve send,
    /// while the sibling full-heal arm stays armed across that exact
    /// send. An abort parked in the send destroyed the healing prefix
    /// `[5,6)` with Drop disclosing only the SHRUNK hole — lines
    /// inside a recorded hole silently lost AND the emitted marker
    /// actively misstating the missing span.
    ///
    /// Pre-fix red (2026-06-12, strawman: the pre-send
    /// `p.gap_from = next_line` mutation restored in the partial
    /// arm): `the partial-heal abort must disclose the FULL recorded
    /// hole [5,8) — got marker "*** rio: lines 6-7 missing (durable
    /// log gap) ***" (the shrunk misstatement; the healing prefix
    /// [5,6) was never delivered and is now undisclosed)`.
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn partial_heal_abort_at_serve_send_discloses_full_hole() {
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(taken_window_pending(&["line-8", "line-9"]));
        // A serve advances the floor to 6: INTO the hole [5,8) —
        // a partial heal of [5,6).
        let heal = cell.on_serve(6);
        match heal {
            super::ServeHeal::Shrunk(served) => {
                // The simulated abort: the caller's local (the
                // deferred floor) is destroyed at the parked serve
                // send — the discharge never runs.
                drop(served);
            }
            _ => panic!("a floor at 6 partially heals the [5,8) hole"),
        }
        drop(cell);
        drop(out_tx);
        let mut markers = Vec::new();
        let mut lines = Vec::new();
        while let Some(c) = out_rx.recv().await {
            for l in &c.lines {
                let s = String::from_utf8(l.clone()).expect("test lines are UTF-8");
                if s.contains("missing (durable log gap)") {
                    markers.push((c.first_line_number, s.clone()));
                } else {
                    lines.push(s);
                }
            }
        }
        assert_eq!(
            markers.len(),
            1,
            "exactly one disclosure marker (got {markers:?})"
        );
        let (marker_from, marker_text) = &markers[0];
        assert_eq!(
            (*marker_from, marker_text.as_str()),
            (5, "*** rio: lines 5-7 missing (durable log gap) ***"),
            "the partial-heal abort must disclose the FULL recorded hole [5,8) \
             — the serve send never completed, so the healing prefix [5,6) was \
             never delivered and a shrunk marker would misstate the span"
        );
        assert!(
            lines.iter().any(|l| l == "line-8") && lines.iter().any(|l| l == "line-9"),
            "the withheld lines still disclose (got {lines:?})"
        );
    }

    /// W13-Z2 — the happy partial-heal path is byte-stable: a serve
    /// send that COMPLETES discharges the deferred floor, the hole
    /// shrinks to exactly the residual `[6,8)`, and the eventual
    /// flush discloses the shrunk marker plus the withheld lines —
    /// identical bytes to the pre-fix happy path.
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn partial_heal_completed_send_shrinks_at_discharge() {
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(taken_window_pending(&["line-8", "line-9"]));
        match cell.on_serve(6) {
            super::ServeHeal::Shrunk(served) => {
                // The serve send completed: the post-send discharge
                // is the one writer of the reduction.
                cell.discharge_served_prefix(served);
            }
            _ => panic!("a floor at 6 partially heals the [5,8) hole"),
        }
        let mut last_relayed: Option<u64> = None;
        let mut floor: Option<u64> = None;
        assert!(
            super::flush_pending_gap(&mut cell, &mut last_relayed, &mut floor).await,
            "roomy channel: the flush completes"
        );
        drop(cell);
        drop(out_tx);
        let mut got = Vec::new();
        while let Some(c) = out_rx.recv().await {
            for l in &c.lines {
                got.push((
                    c.first_line_number,
                    String::from_utf8(l.clone()).expect("test lines are UTF-8"),
                ));
            }
        }
        assert!(
            got.iter()
                .any(|(from, s)| *from == 6
                    && s == "*** rio: lines 6-7 missing (durable log gap) ***"),
            "the completed partial heal flushes the RESIDUAL hole [6,8) \
             (got {got:?})"
        );
        assert!(
            got.iter().any(|(_, s)| s == "line-8"),
            "the withheld lines relay (got {got:?})"
        );
    }

    /// W13-Z battery, the Untouched window: a serve that never
    /// reaches into the hole arms nothing and defers nothing — an
    /// abort parked at its send leaves the recorded state intact and
    /// Drop discloses the full hole plus lines, exactly as if the
    /// serve had never happened.
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn untouched_serve_abort_changes_nothing() {
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(taken_window_pending(&["line-8"]));
        // The floor stops AT the hole's start: no heal of any kind.
        match cell.on_serve(5) {
            super::ServeHeal::Untouched => {}
            _ => panic!("a floor at 5 does not reach into the [5,8) hole"),
        }
        drop(cell);
        drop(out_tx);
        let mut got = Vec::new();
        while let Some(c) = out_rx.recv().await {
            for l in &c.lines {
                got.push(String::from_utf8(l.clone()).expect("test lines are UTF-8"));
            }
        }
        assert!(
            got.iter()
                .any(|s| s == "*** rio: lines 5-7 missing (durable log gap) ***"),
            "the untouched window discloses the full hole (got {got:?})"
        );
        assert!(
            got.iter().any(|s| s == "line-8"),
            "the withheld lines disclose (got {got:?})"
        );
    }

    /// [GEN-SET] The abort-window battery census (merged_bug_025,
    /// R31′(ii)): the battery population derives from the `ServeHeal`
    /// variant ALPHABET, never a hand list — this match is exhaustive
    /// with NO wildcard arm, so adding a variant refuses to compile
    /// until its abort-parked-in-send battery member is named here,
    /// and the named members are pinned as real test fns by the
    /// function references below (a renamed or deleted test breaks
    /// the build, not just the census). The wave-12 battery was
    /// hand-enumerated to three windows and missed the fourth variant
    /// (`Shrunk`) — exactly the gap this derivation closes.
    #[test]
    fn serve_heal_abort_battery_is_variant_total() {
        fn battery_member(h: &super::ServeHeal) -> &'static str {
            match h {
                super::ServeHeal::Untouched => "untouched_serve_abort_changes_nothing",
                super::ServeHeal::Shrunk(_) => {
                    "partial_heal_abort_at_serve_send_discloses_full_hole"
                }
                super::ServeHeal::Healed(_) => {
                    "heal_abort_at_serve_send_still_discloses_continuation"
                }
            }
        }
        // The named members are REAL tests in this module: referencing
        // them makes the census structurally un-rottable.
        let _members: [fn(); 3] = [
            untouched_serve_abort_changes_nothing,
            partial_heal_abort_at_serve_send_discloses_full_hole,
            heal_abort_at_serve_send_still_discloses_continuation,
        ];
        // Construct every variant through the PRODUCTION constructor
        // (`on_serve` on a recorded [5,8) hole — R13 provenance), and
        // pin each to its battery member by name.
        let (out_tx, _out_rx) = mpsc::channel(4);
        let mk = || {
            let mut cell = super::PendingGapCell::new(
                out_tx.clone(),
                super::RelayDisposition::disclose_at_drop(),
            );
            cell.record_first(taken_window_pending(&["line-8"]));
            cell
        };
        assert_eq!(
            battery_member(&mk().on_serve(5)),
            "untouched_serve_abort_changes_nothing"
        );
        assert_eq!(
            battery_member(&mk().on_serve(6)),
            "partial_heal_abort_at_serve_send_discloses_full_hole"
        );
        assert_eq!(
            battery_member(&mk().on_serve(9)),
            "heal_abort_at_serve_send_still_discloses_continuation"
        );
    }

    /// W12-AT2 — the defuse arm: a consume that runs to COMPLETION
    /// fires no disclosure from any Drop (the guard is not a blanket
    /// downgrade; each send defuses exactly the part it delivered).
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn completed_flush_fires_no_drop_disclosure() {
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut cell =
            super::PendingGapCell::new(out_tx.clone(), super::RelayDisposition::disclose_at_drop());
        cell.record_first(taken_window_pending(&["line-8"]));
        let mut last_relayed: Option<u64> = None;
        let mut floor: Option<u64> = None;
        assert!(
            super::flush_pending_gap(&mut cell, &mut last_relayed, &mut floor).await,
            "roomy channel: the flush completes"
        );
        assert_eq!(last_relayed, Some(8));
        drop(cell);
        drop(out_tx);
        let mut msgs = 0;
        while out_rx.recv().await.is_some() {
            msgs += 1;
        }
        assert_eq!(
            msgs, 2,
            "exactly the marker and the lines — a completed consume leaves \
             nothing armed, so no Drop re-discloses"
        );
    }
    /// W13-AB (bug_040): the disposition gates EVERY awaited armed — quantifier: census(test: sealed_gap_lint_is_clean_at_head)
    /// discharge, not just Drop. The two REAL windows (the refuted
    /// parked-then-resumed third window is recorded ABSENT per the
    /// triage — tokio cancels queued-aborted tasks at dequeue):
    /// (1) the in-progress poll — the owner marks-then-aborts while
    /// the straggler's reserve is parked; (2) the sync-stalled
    /// straggler past the 250 ms join bound whose next act is the
    /// armed send. Both resolve at the same point: the post-reserve
    /// consult inside the module's one disclosure chokepoint.
    ///
    /// Choreography (deterministic): a 1-slot channel pre-filled so
    /// the armed send parks at its reserve; one poll parks it (the
    /// in-flight poll); the owner marks the disposition; the
    /// successor frees the slot; the straggler resumes.
    ///
    /// Pre-fix red (2026-06-12, strawman: the chokepoint consult
    /// neutralized via `if false && disposition.must_discard()`):
    ///   `the dead execution's withheld lines must not splice into
    ///   the successor's stream (got ["line-8"])`.
    /// Post-fix: the consult refuses the send (the resumed future
    /// returns false), nothing splices, and the guard's Drop —
    /// consulting the same disposition — discards silently.
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn superseded_armed_send_refuses_at_the_consult() {
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let disposition = super::RelayDisposition::disclose_at_drop();
        let mut cell = super::PendingGapCell::new(out_tx.clone(), disposition.clone());
        cell.record_first(taken_window_pending(&["line-8"]));
        let super::ServeHeal::Healed(mut armed) = cell.on_serve(9) else {
            panic!("a floor at 9 fully heals the [5,8) hole");
        };
        // Fill the only slot: the armed send's reserve parks.
        out_tx
            .try_send(super::TaggedLogChunk {
                derivation_path: DRV.to_string(),
                first_line_number: 0,
                lines: vec![b"successor-traffic".to_vec()],
            })
            .expect("one slot free");
        let mut last_relayed: Option<u64> = None;
        let refused = {
            let mut fut = Box::pin(armed.send_lines(&mut last_relayed));
            // The in-flight poll: parked at the reserve.
            poll_once(&mut fut);
            // The owner supersedes WHILE the poll is in flight (the
            // mark-then-abort protocol's mark; the abort has not
            // landed yet).
            disposition.mark_superseded();
            // The successor consumes the slot; the straggler resumes.
            let freed = out_rx.recv().await.expect("the successor's own traffic");
            assert_eq!(freed.lines[0], b"successor-traffic".to_vec());
            !fut.await
        };
        drop(armed);
        drop(cell);
        drop(out_tx);
        let mut got = Vec::new();
        while let Some(c) = out_rx.recv().await {
            for l in &c.lines {
                got.push(String::from_utf8(l.clone()).expect("test lines are UTF-8"));
            }
        }
        // The proposition FIRST: nothing from the dead execution
        // reaches the successor's stream — neither the resumed armed
        // send (the splice) nor the guard's Drop (its consult is the
        // backstop).
        assert!(
            got.is_empty(),
            "the dead execution's withheld lines must not splice into the \
             successor's stream (got {got:?})"
        );
        assert!(
            refused,
            "the resumed armed send must refuse at the post-reserve consult"
        );
    }

    /// W13-AB2 — the control: the same choreography without the
    /// supersession mark relays normally once the slot frees (the
    /// consult is not a blanket downgrade; non-superseded sends are
    /// byte-stable).
    // r[verify gw.tail.disclosure-linear]
    #[tokio::test]
    async fn non_superseded_armed_send_relays_after_the_park() {
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let disposition = super::RelayDisposition::disclose_at_drop();
        let mut cell = super::PendingGapCell::new(out_tx.clone(), disposition.clone());
        cell.record_first(taken_window_pending(&["line-8"]));
        let super::ServeHeal::Healed(mut armed) = cell.on_serve(9) else {
            panic!("a floor at 9 fully heals the [5,8) hole");
        };
        out_tx
            .try_send(super::TaggedLogChunk {
                derivation_path: DRV.to_string(),
                first_line_number: 0,
                lines: vec![b"consumer-traffic".to_vec()],
            })
            .expect("one slot free");
        let mut last_relayed: Option<u64> = None;
        let sent = {
            let mut fut = Box::pin(armed.send_lines(&mut last_relayed));
            poll_once(&mut fut);
            let _ = out_rx.recv().await.expect("the consumer's own traffic");
            fut.await
        };
        assert!(sent, "an un-superseded armed send completes");
        let relayed = out_rx.recv().await.expect("the withheld lines");
        assert_eq!(relayed.lines[0], b"line-8".to_vec());
    }

    // ------------------------------------------------------------------
    // The widened sealed-gap lint (bug_040 + merged_bug_025; the
    // prefiled-#5 reconciliation, OQ-14 verdict: IN-CRATE TEST TIER —
    // the grammar is module-local to `mod gap`, so the lint lives in
    // the take_armed family's own file and needs no staged workspace)
    // ------------------------------------------------------------------

    /// The lint's grammar selector — and its OWN K-mutation surface
    /// (R31′(iii)): each narrowed variant is a seeded mutation of the
    /// artifact's control flow, and the self-test below proves every
    /// plant DIES under the mutation that disables its rule. A lint
    /// whose plants survive its own narrowing would be the bug_047
    /// born-broken shape.
    #[derive(Clone, Copy, PartialEq)]
    enum SealedGapGrammar {
        /// The widened grammar: the sealed take set AND
        /// obligation-reducing mutations AND the reservation
        /// chokepoint.
        Full,
        /// K-mutation 1 — the original prefiled take-only candidate:
        /// must MISS the merged_bug_025 mutate-then-await plant.
        TakeOnly,
        /// K-mutation 2 — the chokepoint rule dropped: must MISS the
        /// bug_040 unconsulted-reservation plant.
        NoChokepointRule,
    }

    /// The sealed-gap census over `mod gap`'s production text:
    /// comment-stripped, whitespace-stripped, count-pinned needles.
    /// Every count names its sanctioned sites — a NEW take, a NEW
    /// obligation-reducing mutation outside the typed discharges, or
    /// a NEW reservation outside the disposition-consulting
    /// chokepoint changes a count and reds the lint.
    fn sealed_gap_violations(gap_src: &str, grammar: SealedGapGrammar) -> Vec<String> {
        let stripped: String = gap_src
            .lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .concat()
            .split_whitespace()
            .collect();
        let mut v = Vec::new();
        let mut pin = |got: usize, needle: &str, want: usize, why: &str| {
            if got != want {
                v.push(format!("{needle}: {got} != {want} ({why})"));
            }
        };
        let count = |needle: &str| stripped.matches(needle).count();
        // The sealed take set (every grammar): cell Drop, take_armed,
        // on_serve's full-heal arm.
        pin(
            count("state.take()"),
            "state.take()",
            3,
            "the sealed take set: PendingGapCell::drop, take_armed, on_serve full heal",
        );
        if grammar != SealedGapGrammar::TakeOnly {
            // Obligation-reducing mutations live ONLY in the typed — quantifier: census(test: sealed_gap_lint_catches_both_historical_shapes)
            // discharge fns (the merged_bug_025 lesson: a bare
            // pre-send mutation is the R32 escape). ASSIGNMENT only:
            // the classify arm's `gap_from ==` equality is excluded
            // by subtracting the double-equals matches.
            pin(
                count("gap_from=") - count("gap_from=="),
                "gap_from=",
                1,
                "discharge_served_prefix is the only recorded-hole floor writer",
            );
            pin(
                count("self.hole="),
                "self.hole=",
                4,
                "suppress_hole, suppress_hole_until, disclose_hole_until \
                 (post-send), note_delivered",
            );
            pin(
                count("mem::take(&mutself.lines)"),
                "mem::take(&mutself.lines)",
                2,
                "send_lines' permit-send and the Drop disclosure",
            );
            pin(
                count(".lines.clear()"),
                ".lines.clear()",
                1,
                "note_delivered's full-coverage trim",
            );
            pin(
                count(".lines.drain("),
                ".lines.drain(",
                1,
                "note_delivered's prefix trim",
            );
        }
        if grammar != SealedGapGrammar::NoChokepointRule {
            // The reservation chokepoint (the bug_040 lesson: a send
            // site that does not consult the disposition splices).
            pin(
                count("fnreserve_disclosure"),
                "fnreserve_disclosure",
                1,
                "the one disposition-consulting reservation chokepoint exists",
            );
            pin(
                count(".reserve().await"),
                ".reserve().await",
                1,
                "every AWAITED reservation routes through reserve_disclosure",
            );
            pin(
                count("must_discard()"),
                "must_discard()",
                2,
                "the chokepoint consult + the ArmedGap::drop backstop",
            );
            pin(
                count("try_reserve()"),
                "try_reserve()",
                3,
                "Drop's transactional sends only (one single + one pair)",
            );
        }
        v
    }

    /// Extract `mod gap`'s text from this file (the embedded census
    /// universe — compile-time `include_str!`, never a tree walk).
    fn gap_module_src() -> &'static str {
        let src = include_str!("log_tail.rs");
        let start = src.find("mod gap {").expect("mod gap exists");
        let end = src
            .find("use gap::{")
            .expect("the module-close anchor exists");
        &src[start..end]
    }

    /// The lint at head: zero violations over the real module text.
    #[test]
    fn sealed_gap_lint_is_clean_at_head() {
        let v = sealed_gap_violations(gap_module_src(), SealedGapGrammar::Full);
        assert!(v.is_empty(), "sealed-gap violations: {v:#?}");
    }

    /// W13-AB3 — both historical shapes planted INSIDE the census
    /// population and caught: the merged_bug_025 mutate-then-await
    /// (a bare `gap_from =` outside the typed discharge) and the
    /// bug_040 unconsulted reservation (a second `.reserve().await`
    /// outside the chokepoint).
    #[test]
    fn sealed_gap_lint_catches_both_historical_shapes() {
        let mutate_plant = format!(
            "{}\nfn evil_shrink(p: &mut PendingGap, next: u64) {{ p.gap_from = next; }}\n",
            gap_module_src()
        );
        let v = sealed_gap_violations(&mutate_plant, SealedGapGrammar::Full);
        assert!(
            v.iter().any(|x| x.contains("gap_from=")),
            "the mutate-then-await plant must red the widened lint (got {v:?})"
        );

        let reserve_plant = format!(
            "{}\nasync fn evil_send(tx: &mpsc::Sender<TaggedLogChunk>) {{ \
             let _ = tx.reserve().await; }}\n",
            gap_module_src()
        );
        let v = sealed_gap_violations(&reserve_plant, SealedGapGrammar::Full);
        assert!(
            v.iter().any(|x| x.contains(".reserve().await")),
            "the unconsulted-reservation plant must red the widened lint \
             (got {v:?})"
        );
    }

    /// The K-mutation self-test (R31′(iii)): each planted red DIES
    /// under the seeded mutation that disables its rule — proving
    /// both widenings are load-bearing, not decorative. A third
    /// mutation (the population walk emptied) is caught by the
    /// definition pins: an empty universe is a missing chokepoint,
    /// never a vacuous pass.
    #[test]
    fn sealed_gap_lint_k_mutations_kill_the_plants() {
        // K1: the take-only narrowing misses the mutate plant — the
        // exact blindness the prefiled-#5 candidate had.
        let mutate_plant = format!(
            "{}\nfn evil_shrink(p: &mut PendingGap, next: u64) {{ p.gap_from = next; }}\n",
            gap_module_src()
        );
        let v = sealed_gap_violations(&mutate_plant, SealedGapGrammar::TakeOnly);
        assert!(
            !v.iter().any(|x| x.contains("gap_from=")),
            "under the take-only narrowing the mutate plant's red must DIE \
             (got {v:?}) — otherwise the widening proves nothing"
        );

        // K2: dropping the chokepoint rule misses the reserve plant.
        let reserve_plant = format!(
            "{}\nasync fn evil_send(tx: &mpsc::Sender<TaggedLogChunk>) {{ \
             let _ = tx.reserve().await; }}\n",
            gap_module_src()
        );
        let v = sealed_gap_violations(&reserve_plant, SealedGapGrammar::NoChokepointRule);
        assert!(
            !v.iter().any(|x| x.contains(".reserve().await")),
            "under the no-chokepoint mutation the reservation plant's red \
             must DIE (got {v:?})"
        );

        // K3: the emptied population walk is itself a red (the
        // definition pins demand the chokepoint EXISTS).
        let v = sealed_gap_violations("", SealedGapGrammar::Full);
        assert!(
            !v.is_empty(),
            "an empty census universe must red, never vacuously pass"
        );
    }

    // ------------------------------------------------------------------
    // W12-AU / W12-AU2 (merged_bug_007, R32): every exit discharges
    // ------------------------------------------------------------------

    /// W12-AU — the orphan fast path discharges the disclosure
    /// obligation exactly as the post-drive path. Proposition: every
    /// `Exit` verdict is discharged on every path, and
    /// `disclose_truncation` is true iff (cut AND consumer-alive) —
    /// consumer liveness is its own input, never inferred from the
    /// stop cause.
    ///
    /// Choreography: the relay loses its stream to a transport error
    /// (incomplete serve), then during the reconnect backoff the
    /// drain flips terminal and the SENDER dies — the loop-top orphan
    /// fast path observes the orphan with the OUTPUT channel still
    /// open and a cut serve. Pre-fix the fast path computed the
    /// verdict, `debug_assert`-matched `Exit { .. }`, and returned
    /// without reading the disclosure (and the kernel's orphan arm
    /// hard-coded false regardless); post-fix the discharge sends the
    /// truncation marker through the same epilogue the post-drive
    /// path uses.
    ///
    /// Pre-fix red (the wave-12 W12-AU capture): the post-orphan recv
    /// times out — `the orphan exit must discharge the cut disclosure
    /// to the live output channel (pre-fix: verdict discarded)`.
    // r[verify gw.tail.truncation-disclosed+2]
    #[tokio::test]
    async fn orphan_exit_with_cut_discloses_to_live_consumer() {
        let mock = MockTail::default();
        let router = Server::builder().add_service(LogServiceServer::new(mock.clone()));
        let (addr, _server) = spawn_grpc_server(router).await;
        let client = rio_proto::LogServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("connect to the mock LogService");
        // Two lines then a transport error: the serve is INCOMPLETE
        // and the relay heads into its reconnect loop.
        mock.push_script(
            vec![chunk(0, 2)],
            SessionEnd::Error(tonic::Code::Unavailable),
        );

        let (out_tx, mut out_rx) = mpsc::channel(8);
        let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
        let disposition = super::RelayDisposition::disclose_at_drop();
        let relay = tokio::spawn(super::run_tail(
            client,
            SessionTokenSource::none(),
            DRV.to_string(),
            EXEC_A.to_string(),
            super::RelayWiring {
                out_tx: out_tx.clone(),
                drain: drain_rx,
                disposition,
            },
            test_config(),
        ));

        let lines = recv_lines(&mut out_rx, 2).await;
        assert_eq!(lines.len(), 2);
        // Terminal, then the watch sender dies: the relay is orphaned
        // mid-backoff while the OUTPUT channel stays open — the
        // loop-top fast path decides the exit.
        drain_tx.send(true).expect("relay alive");
        drop(drain_tx);

        let marker = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect(
                "the orphan exit must discharge the cut disclosure to the \
                 live output channel (pre-fix: verdict discarded)",
            )
            .expect("output channel open");
        let text = String::from_utf8(marker.lines[0].clone()).expect("marker is UTF-8");
        assert!(
            text.contains("rio:") && marker.first_line_number == 2,
            "the truncation disclosure names the first unserved line; got {text:?}"
        );
        let _ = relay.await;
    }

    /// W12-AU2 — the PermanentErr-with-live-consumer face: a stream
    /// the store types permanently unservable MID-BUILD (no terminal,
    /// grace never armed) stops the relay while the nix client is
    /// alive and reading. The kernel's pre-fix arm hard-coded
    /// `disclose_truncation: false` on a "no consumer remains"
    /// premise that is false here — the client's log silently
    /// stopped. Post-fix the (cut x consumer-alive) product
    /// discloses.
    ///
    /// Pre-fix red: the post-lines recv times out — `a typed-permanent
    /// refusal with a live consumer must disclose the cut (pre-fix:
    /// hard-coded silent)`.
    // r[verify gw.tail.truncation-disclosed+2]
    #[tokio::test]
    async fn permanent_err_with_live_consumer_discloses_cut() {
        let mut h = harness().await;
        h.mock
            .push_script(vec![chunk(0, 2)], SessionEnd::ErrorUnservable);

        h.set.on_started(DRV, EXEC_A);
        let lines = recv_lines(&mut h.out_rx, 2).await;
        assert_eq!(lines.len(), 2);

        let marker = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect(
                "a typed-permanent refusal with a live consumer must disclose \
                 the cut (pre-fix: hard-coded silent)",
            )
            .expect("output channel open");
        let text = String::from_utf8(marker.lines[0].clone()).expect("marker is UTF-8");
        assert!(
            text.contains("rio:") && marker.first_line_number == 2,
            "the truncation disclosure names the first unserved line; got {text:?}"
        );

        h.set.abort_all();
    }
    /// W12-AV (bug_018, the R26 instance repaired): the truncation
    /// disclosure is denominated in the store's own verdict. It fires
    /// exactly when the store DECLINED to confirm the served log
    /// complete (`is_complete` never observed true) --- a preimage
    /// with two faces the 1-bit wire claim cannot distinguish:
    /// cut-mid-replay durable residue (retrievable from the store)
    /// and a never-uploaded tail (builder died un-acked; manifest
    /// holes are "genuine storage loss" --- nothing to retrieve). The
    /// marker text MUST NOT pick a face: the pre-fix text asserted
    /// "the full log is durable in the store" unconditionally --- a
    /// user following it on the loss face finds the tail absent.
    ///
    /// This fixture drives the never-durable face: the builder dies
    /// mid-build (stream cut, no final claim ever), the derivation
    /// goes terminal, the grace expires. Pre-fix red:
    ///   `the truncation marker must not claim unconditional
    ///   durability on the never-durable face (got "*** rio: lines
    ///   2- not served before the post-terminal grace expired (live
    ///   tail truncated; the full log is durable in the store) ***")`
    /// The durable face's actionable pointer survives CONDITIONALLY
    /// (the "stored remainder" clause); the served-complete exit
    /// (`grace_exit_with_complete_serve_stays_silent`) remains the
    /// honest treatment of the confirmed face: no marker at all.
    // r[verify gw.tail.two-face-truncation]
    #[tokio::test]
    async fn truncation_marker_states_both_faces_never_claiming_durability() {
        let mut h = harness().await;
        // Two lines, then the stream holds forever with no
        // completeness claim: an un-acked tail (the builder died
        // before uploading the rest -- the store has nothing more).
        h.mock.push_script(vec![chunk(0, 2)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);
        let _ = recv_lines(&mut h.out_rx, 2).await;
        h.set.on_terminal(DRV);

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

        let marker = tokio::time::timeout(Duration::from_secs(2), h.out_rx.recv())
            .await
            .expect("the grace exit must disclose the truncation")
            .expect("output channel open");
        assert_eq!(marker.first_line_number, 2);
        let text = String::from_utf8(marker.lines[0].clone()).expect("marker is UTF-8");
        assert!(
            !text.contains("the full log is durable in the store"),
            "the truncation marker must not claim unconditional durability \
             on the never-durable face (got {text:?})"
        );
        assert!(
            text.contains("did not confirm")
                && text.contains("stored remainder")
                && text.contains("never uploaded"),
            "the marker states both faces of the unconfirmed verdict; got {text:?}"
        );

        h.set.abort_all();
    }
}
