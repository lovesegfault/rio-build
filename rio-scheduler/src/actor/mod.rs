//! DAG actor: single Tokio task owning all mutable scheduler state.
//!
//! All gRPC handlers communicate with the actor via an mpsc command channel.
//! The actor processes commands serially, ensuring deterministic ordering
//! and eliminating lock contention.
// r[impl sched.actor.single-owner]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

// `broadcast` and the `crate::state` heartbeat/poison constants below
// are not used by mod.rs directly — they're pulled through `use
// super::*` by `recovery.rs` / `tests/` (b03 scope, glob-import
// retained). Once those modules switch to explicit imports, drop
// these.
#[allow(unused_imports)]
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use rio_proto::StoreServiceClient;
use rio_proto::types::FindMissingPathsRequest;

use crate::dag::DerivationDag;
use crate::db::SchedulerDb;
use crate::lease::LeaderState;
#[allow(unused_imports)]
use crate::state::{
    BuildInfo, BuildState, BuildStateExt, DerivationStatus, DrvHash, ExecutorId, POISON_TTL,
    PoisonConfig, RetryPolicy,
};

// `impl DagActor` is sharded across these submodules by concern.
// Cohesive field clusters live in sub-structs (`events: BuildEventBus`,
// `leader: LeaderState`); the genuinely
// cross-cutting fields (`dag`, `builds`, `db`) remain
// flat — every handler reads/writes them. Keep
// ALL `mod` decls here so the submodule list is discoverable in one
// place.
mod breaker;
mod build;
mod command;
mod completion;
mod config;
mod dispatch;
mod event;
mod executor;
pub(crate) mod floor;
mod handle;
mod housekeeping;
pub(crate) use housekeeping::describe_housekeeping_metrics;
pub(crate) mod materialize;
mod merge;
pub(crate) mod pull;
mod recovery;
mod report_ctx;
mod snapshot;

pub(super) use breaker::CacheCheckBreaker;
pub use command::*;
pub use config::{DagActorConfig, DagActorPlumbing};
use event::BuildEventBus;
pub use event::BuildEventReceivers;
pub use handle::ActorHandle;
#[cfg(test)]
pub(crate) use handle::DebugDerivationInfo;
pub use pull::{PullOutcome, PullRejection};

#[cfg(test)]
mod debug;
#[cfg(test)]
pub(crate) mod tests;

/// Channel capacity for the actor command channel.
pub(crate) const ACTOR_CHANNEL_CAPACITY: usize = 10_000;

/// Backpressure: reject new work above this fraction of channel capacity.
const BACKPRESSURE_HIGH_WATERMARK: f64 = 0.80;

/// Backpressure: resume accepting work below this fraction.
const BACKPRESSURE_LOW_WATERMARK: f64 = 0.60;

/// The B8 admin-latency SLO (round-9): a Fast-lane admin command
/// ([`AdminQuery::lane`]) completes within this budget regardless of
/// Tick cost. The value MIRRORS the controller's client-side deadline
/// (`rio-controller/src/reconcilers/mod.rs` `ADMIN_RPC_TIMEOUT = 5s`)
/// — a doc-pinned mirror, not a shared symbol: minting a shared
/// constant would edit the controller's cite surface, which belongs
/// to another round-9 slot (divergence recorded in the WO-S2-10
/// commit). Violable (R17): if the controller's deadline moves, this
/// moves with it — the W9-AG witness pins the LAW (delivery bounded
/// by the largest indivisible work slice, not the whole tick), not
/// the number.
pub(crate) const ADMIN_FAST_DELIVERY_SLO: std::time::Duration = std::time::Duration::from_secs(5);

/// Capacity of the admin fast lane (round-9 B8). Sized for its real
/// producers — the controller's per-pool reconcile (one mint per
/// GetSpawnIntents poll, sequential per pool) plus dashboard/CLI SLA
/// reads — with two orders of magnitude headroom. A FULL fast lane
/// fails OPEN: the sender falls back to the main mailbox (the
/// pre-B8 FIFO path), never drops. Violable (R17): population axis
/// only; the fallback is what makes any value safe.
pub(crate) const ADMIN_FAST_LANE_CAPACITY: usize = 256;

/// Work-per-turn quota for fast-lane service (round-9 B8, the A-1
/// discipline applied to the lane itself): at most this many fast
/// commands are served per drain visit (one Tick phase boundary or
/// one main-loop turn run of consecutive fast arms) before yielding
/// back to the main mailbox / the next phase. Bounds both the
/// per-boundary Tick inflation and biased-select starvation of the
/// main mailbox under a fast flood (each serve is one bounded
/// `handle_admin` call; producers are authenticated admin surfaces).
/// Violable (R17): time axis rides the per-command handler bound; 16
/// covers every observed legitimate burst (≤ 2 mints + a handful of
/// dashboard reads per second).
pub(crate) const ADMIN_FAST_LANE_DRAIN_QUOTA: usize = 16;

/// EWMA weight for the per-turn cost estimate feeding the cost-axis
/// backpressure law (round-9 B6, re-derived sh-024 §S2). The EWMA is
/// FED at fast-lane rate ([`DagActor::note_turn_cost`] from BOTH the
/// main-mailbox path AND `serve_fast_admin`, up to
/// [`ADMIN_FAST_LANE_DRAIN_QUOTA`] = 16 feeds between consecutive
/// main-loop [`DagActor::update_backpressure`] evaluations — the
/// fast-lane work prices into drain by design, review (e)) but
/// EVALUATED only per main-mailbox dequeue: at the prior 0.3 a single
/// 1.3 s spike (ewma 0.39, drain 44.5 s) activated and the 16 × 5 µs
/// fast feeds (× 0.7¹⁶ ≈ 0.003) released it 80 µs later — sh-024 saw
/// `queue_backpressure` +24 in 120 s. 0.05 is sized so the worst-case
/// inter-evaluation decay 0.95^QUOTA ≈ 0.44 keeps a genuine
/// pathological turn engaged (the live_053 140 s command: one
/// observation lands 0.05 × 140 = 7 s, drain @ q=100 = 700 s ≫ HIGH;
/// after 16 fast decays drain = 308 s — STAYS active) while a single
/// 1.3 s spike never reaches HIGH (ewma 0.066, drain 7.5 s @ q=114).
/// Release after ≈83 cheap feeds (≈5 main-loop evaluates with a full
/// fast lane, ≈83 without; ≤ ~80 ms wall-clock at sub-ms turns —
/// review (d)). The trade is sustained-overload engage latency: a
/// 0.5 s/turn stream at q=100 reaches drain=30 s after ~18 turns
/// (≈9 s) vs ~4 turns at 0.3 — bounded under the 30 s caller-deadline
/// derivation at [`BACKPRESSURE_DRAIN_HIGH_SECS`]. Violable (R17):
/// the law W9-AH pins is engage-on-cost / release-on-decay, not the
/// weight.
const BACKPRESSURE_COST_EWMA_ALPHA: f64 = 0.05;

/// Cost-axis ENGAGE bound (round-9 B6): backpressure activates when
/// projected mailbox drain time (queue depth × per-turn cost EWMA)
/// reaches this many seconds, regardless of depth fraction.
/// Derivation: 30s is the submit-side caller deadline class
/// (`grpc_timeout` — the gateway returns DEADLINE_EXCEEDED past it);
/// a mailbox that cannot drain within the deadline of the callers
/// queued in it is already failing all of them, so shedding NEW work
/// is strictly better than accepting it. Violable (R17): the paired
/// witness pins the inversion (engage while depth ≪ watermark), not
/// the number.
const BACKPRESSURE_DRAIN_HIGH_SECS: f64 = 30.0;

/// Cost-axis RELEASE bound (round-9 B6): the cost term clears when
/// projected drain falls to this many seconds. 3× under the engage
/// bound — the same anti-flap band shape as the 0.80/0.60 depth pair
/// — and far above any healthy steady state (sub-ms turns × even a
/// full mailbox ≈ single-digit seconds). Release requires BOTH axes
/// low (depth AND cost): a one-axis release would re-admit work the
/// other axis is still drowning under.
const BACKPRESSURE_DRAIN_LOW_SECS: f64 = 10.0;

/// Phase 1b: how many times a failure completion whose appending
/// transaction failed is re-delivered to the actor's own mailbox before
/// the event is dropped. The derivation stays in its pre-report state
/// either way (the worker never re-sends a `CompletionReport`); past the
/// cap the backstop sweep is what eventually re-drives it — the trade
/// the design makes explicitly: a PG outage stalls failure accounting
/// instead of silently under-counting it.
const MAX_ATTEMPT_RECORD_REDELIVERIES: u32 = 3;

/// Delay before a re-delivered failure completion is pushed back onto
/// the actor mailbox, so a brief PG blip has a chance to clear instead
/// of burning the whole re-delivery budget within one event-loop turn.
const ATTEMPT_RECORD_REDELIVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Phase 1b bounded re-enqueue support (merged_bug_003): the
/// ORIGINAL `ProcessCompletion` payload, captured by value at the
/// routing match (failure-gated clone — the Built hot path never
/// clones) so a failed appending transaction re-delivers the
/// PRISTINE wire report: proto `BuildResult` (status, message,
/// `store_degraded`, outputs, timestamps), node identity, and
/// resource telemetry all survive the mailbox roundtrip.
pub(super) struct CompletionEcho {
    pub(super) result: rio_proto::types::BuildResult,
    pub(super) peak_memory_bytes: u64,
    pub(super) peak_cpu_cores: f64,
    pub(super) node_name: Option<String>,
    pub(super) hw_class: Option<String>,
    pub(super) final_resources: Option<rio_proto::types::ResourceUsage>,
    pub(super) final_line_count: u64,
}

/// Number of state events to retain in each build's broadcast ring for
/// late subscribers.
///
/// 4096 (was 1024 — I-144): `handle_merge_dag` used to dispatch ready
/// nodes BEFORE returning `event_rx`, so the initial event burst (one
/// Derivation::Started per ready node) landed in the ring before the
/// SubmitBuild bridge started draining; pull-mode attempt opens emit
/// the same Started events asynchronously. A 153k-node submission with ~500
/// ready nodes plus Progress emitted ~1.3k events synchronously → the
/// bridge's first `recv()` was `Lagged`. 4096 gives headroom for the
/// initial burst; the bridge now also continues across `Lagged` instead
/// of dropping the receiver (see `bridge_build_events`).
///
/// Display-only events are NOT routed through this channel — they have
/// their own [`LOG_EVENT_BUFFER_SIZE`]-sized ring so display volume
/// cannot evict state-transition events (`r[gw.activity.stop-parity]`).
pub(super) const BUILD_EVENT_BUFFER_SIZE: usize = 4096;

/// Display-only broadcast ring size, per build
/// (`Event::SubstituteProgress`). Separate from
/// [`BUILD_EVENT_BUFFER_SIZE`] so chatty parallel builds cannot lag the
/// state-event
/// channel and drop `DerivationEvent::Completed`. The Apr-7 large-shallow
/// repro had 44 `start_activity` but only 34 `stop` on the wire —
/// `Lagged` skip-and-continue silently dropped 10 completions. Display
/// loss is acceptable (the terminal Cached/Completed covers a dropped
/// progress emit); state loss is not.
///
/// This ring used to also carry `Event::Log` (the scheduler-relayed live
/// log tail); the build-log data-plane cutover moved log batches to
/// rio-store's `LogService` and deleted the variant. Substitute download
/// progress bars (`r[gw.activity.subst-progress]` — fed by the store
/// replicas' materialization progress reports) still ride this ring.
/// The only consumer-side split is the `display_only` `matches!` in
/// `BuildEventBus::emit`. The 1024 size was chosen for log volume and is
/// now generous (SubstituteProgress is throttled per-path); it is not
/// worth shrinking.
pub(crate) const LOG_EVENT_BUFFER_SIZE: usize = 1024;

/// Expiry for the dispatch-probe service token
/// (`dispatch::DagActor::probe_service_meta` — the `FindMissingPaths`
/// tenant-context mint). Inherited unchanged from the deleted walk
/// auth's token expiry (the probe used the same mint); generous for a
/// single bounded probe call, deliberately not tightened in the
/// deletion commit.
pub const PROBE_TOKEN_EXPIRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// LRU cap for [`DagActor::unroutable_features_warned`] (mb_001). The
/// key's second component is tenant-controlled `requiredSystemFeatures`;
/// 1024 distinct `(tenant, features)` tuples is generous for legitimate
/// misconfiguration (a tenant's drv-set has a small, repeated set of
/// feature combinations) while bounding the actor heap to ~2 MiB worst
/// case (1024 × ~2 KiB clamped key). Eviction re-arms the warn —
/// fail-safe over-emit.
pub(super) const UNROUTABLE_FEATURES_WARNED_CAP: usize = 1024;
/// LRU cap for [`DagActor::cap_mismatch_warned`] (mb_003 / r31 A3).
/// Key cardinality is `|tenants| × |overridden pnames| × |CapacityType|`
/// — operator-configured overrides are sparse (a handful at a time) so
/// 1024 is far above any legitimate steady-state.
pub(super) const CAP_MISMATCH_WARNED_CAP: usize = 1024;
/// LRU cap for [`DagActor::forecast_dropped_warned`] (r34 bug_018).
/// Key cardinality is `|Queued-with-incomplete-deps drvs| × |reason|`,
/// where the `reason` alphabet is the registered label set of
/// `rio_scheduler_sla_forecast_dropped_total` in
/// `crate::sla::metrics::SLA_LABELED_METRICS` — the cap derives from
/// THAT row, never from a member list restated here (merged_bug_149:
/// the restated `|reason| = 2` went stale the moment wave-9 appended
/// `substituting_pacing`; the registered slice is the single source
/// and the label-extension lane grows it). Bounded by the DAG width;
/// 4096 covers the largest observed Queued frontier (e.g.
/// nixpkgs-cross release closures) times any registered reason
/// cardinality. Eviction re-arms the warn — fail-safe over-emit,
/// bounded by cap × eviction churn.
///
/// STRIKE-3 on the ONCE_PER_MISS contract (merged_bug_001/r3-BLOCKED →
/// `unroutable_features_warned` → `cap_mismatch_warned` → this). r35
/// tripwire: a 4th `Mutex<LruCache>` debounce field warrants a
/// `DebouncedCounter<K>` newtype that enforces the gate at the type
/// level (so `.increment(key)` cannot bypass it) plus a lint-test
/// asserting no raw `::metrics::counter!` appears in
/// `compute_spawn_intents`.
pub(super) const FORECAST_DROPPED_WARNED_CAP: usize = 4096;

/// Timeout for the merge-time `FindMissingPaths` only
/// (`find_missing_with_breaker`). Separate from `grpc_timeout` (30s):
/// with the store-side probe-truncation cap removed
/// (`r[store.substitute.probe-bounded+4]`), `check_available` runs the
/// FULL uncached set at 128-wide. Envelope: `⌈N_uncached/128⌉ × RTT` —
/// 153k paths at 30ms ≈ 36s, which the default 30s would clip. 90s
/// covers that with headroom for one 429-retry sleep; the merge phase
/// already sits inside the actor for a 153k-node submission, so the
/// extra 60s is acceptable for that (rare, inherently slow) shape.
/// Dispatch-time FMP stays on `grpc_timeout` (its batch is bounded by
/// `DISPATCH_PROBE_TICK_QUOTA`).
pub const MERGE_FMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Delay before cleaning up terminal build state. Keeps the build
/// resident (and its broadcast channels alive) so late WatchBuild
/// subscribers can still attach and learn the outcome from their
/// snapshot (`r[sched.watch.snapshot-first]`).
const TERMINAL_CLEANUP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Per-tick admission quota for the ready-set store short-circuit
/// (`sweep_ready_cached`): across ALL inline sweeps within one
/// `probe_generation` (after merges, after completion cascades, plus
/// the Tick's own sweep), at most this many Ready candidates are
/// admitted to `FindMissingPaths`. Round-9 B7 promoted the old
/// per-SWEEP batch cap to this deliberate per-tick quota: the cap's
/// pacing effect was incidental — every same-tick re-sweep advanced
/// the truncate window and admitted another batch, so per-tick
/// admissions were bounded by (cap × sweeps/tick), unbounded in
/// completion-cascade count.
///
/// Typed envelope (R17), all axes:
/// - population: ≤ 2048 candidates admitted per generation; the
///   unserved tail carries to the NEXT generation (never re-granted
///   within the same one) and is served oldest-`probed_generation`
///   first, so the window self-heals to full coverage instead of
///   starving behind same-age re-probes.
/// - time: each sweep's FMP batch is ≤ the quota remainder ≤ 2048
///   paths — the same single-RPC bound the old per-sweep cap enforced
///   (in the actor's ~100ms budget under `grpc_timeout` (30s), not
///   [`MERGE_FMP_TIMEOUT`]).
/// - VIOLABLE: 2048 is the carried wave-8 value (derived from the FMP
///   RPC budget above, far under the wire's `max_batch_paths`); no
///   production capture has contradicted it. Retune on evidence of
///   starved wide layers — the quota law (≤ quota/tick, next-tick
///   self-heal) is what tests pin, not the number.
pub(crate) const DISPATCH_PROBE_TICK_QUOTA: usize = 2048;

/// Wall-clock ceiling for the phase-17 ready-cache sweep's on-actor
/// `FindMissingPaths`. Deliberately NOT applied to the phase-12
/// orphan-output probe (breaker-feed semantics own that timeout — a
/// 5.5 s cap there reclassifies a slow-but-healthy store as a breaker
/// failure) or the per-outcome `reprobe_live_wanted_paths` (per-RPC
/// settlement, not per-tick; a 5.5 s shared serial budget breaks
/// 3-tenant 2.5 s/ea into a bare-re-arm livelock until age-out).
/// Named `SELF_FENCE_AFTER/2` (= 5.5 s): the threshold past which the
/// pre-guard-isolated lease shape would have starved a renew, and the
/// budget the phase-17 WARN names. With sh-044's candidate filter the
/// steady-state set is the freshly-Ready frontier only (≪ 1047), so
/// this is defense-in-depth — it bounds the cold-merge / store-slow
/// tail at the same threshold the existing skip already guards. On
/// expiry the unprobed tail fails open: the next `probe_generation`
/// re-admits it (Ready dispatches from source via the normal drain).
///
/// `checked_div(2).unwrap()` is const-stable since 1.58 / `unwrap`
/// since 1.83; `SELF_FENCE_AFTER` = 11 s → 5_500_000_000 ns exactly.
pub(crate) const DISPATCH_PROBE_SWEEP_BUDGET: std::time::Duration =
    rio_lease::SELF_FENCE_AFTER.checked_div(2).unwrap();

/// Concurrency ceiling for the per-tenant store-probe fan-out (the
/// sweep's wall-clock is owned by its `AttemptBudget`, not this knob;
/// this only caps simultaneous in-flight FindMissingPaths RPCs so a
/// many-tenant sweep cannot dogpile the store).
pub(crate) const MAX_PROBE_CONCURRENCY: usize = 8;

/// Generation-keyed admission ledger for
/// [`DISPATCH_PROBE_TICK_QUOTA`]. `admitted` counts candidates this
/// `generation` has granted to FindMissingPaths across every sweep;
/// the sweep resets the count itself when it observes a NEWER
/// `probe_generation` (structural expiry — no tick-site reset to
/// bypass, the merged_bug_033 lesson). Default `generation: 0` is
/// strictly older than the actor's starting `probe_generation` (1), so
/// the very first sweep begins with a fresh budget.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProbeQuotaLedger {
    pub(crate) generation: u64,
    pub(crate) admitted: usize,
}

/// Entry in [`DagActor::authoritative_binding`]: kube-authoritative
/// `spec.nodeName` from the controller's pod informer, plus the
/// scheduler-side tenant attribution captured at Ack time. Bundled so
/// every reader sees both off ONE map entry — `node` and `tenant`
/// structurally cannot diverge (mb_012).
#[derive(Debug, Clone)]
pub(crate) struct AuthBinding {
    pub node: String,
    pub tenant: Option<Uuid>,
    /// The rendered deadline the bound pod was dispatched under
    /// (`BoundIntent.deadline_secs`; `None` = wire `0` = absent —
    /// pre-upgrade controller or unparseable annotation). The build
    /// mint floors its persisted deadline at this value (bug_106): the
    /// establishment window must never shrink below the deadline the
    /// pod is REALLY running under, whatever the mint-time re-solve
    /// says.
    pub deadline_secs: Option<u32>,
}

/// One failed status-batch persist, owned for the outbox. EVERY
/// `persist_status_batch` status latches on failure — Cancelled and
/// DependencyFailed (cancel paths), Completed, Ready, Queued (the
/// dispatch/merge batch persists) — not just terminal cancels.
/// See `DagActor::status_outbox`.
#[derive(Debug)]
pub(super) struct StatusBatch {
    pub(super) drv_hashes: Vec<String>,
    pub(super) status: crate::state::DerivationStatus,
    /// The batch derivations' ACTIVE exec_ids, latched at
    /// persist-failure time from the in-memory DAG (the only source —
    /// PG was down). The replay's assignment close is scoped to
    /// exactly these (`WHERE exec_id = ANY(..) AND status IN
    /// ('pending','acknowledged')`): a successor attempt minted after
    /// the latch carries a different exec_id, so the replay cannot
    /// touch it by construction (merged_bug_011).
    pub(super) exec_ids: Vec<Uuid>,
    pub(super) enqueued_at: std::time::Instant,
    /// Wall-clock latch instant (epoch seconds, POD `SystemTime` —
    /// NOT the clock that stamps `derivations.status_changed_at`,
    /// which is PG `now()`). DIAGNOSTIC ONLY (merged_bug_017): logged
    /// in the flusher's refusal warn for skew forensics, never
    /// compared against any PG-stamped column. The replay's
    /// precedence anchor (merged_bug_025) is the MONOTONIC
    /// `enqueued_at` above, mapped into the PG domain INSIDE the
    /// replay transaction as a latch AGE (the boundary-witnessed
    /// `db::LatchAge::at_replay_boundary`, merged_bug_004 →
    /// `status_changed_at <= now() - make_interval(secs => age)`), so
    /// the comparison lives entirely in one clock domain and the cut
    /// can only trail the enqueue instant.
    pub(super) latched_at_epoch: f64,
}

/// Zero-sized authority witness: the in-memory DAG reflects PG for
/// THIS tenure (merged_bug_210). Mintable only through
/// [`DagActor::dag_authority`] — the one constructor expression reads
/// `dag_authoritative` (policy-pinned to a single production site).
/// Every destructive housekeeping tick takes `&DagAuthority`, so a
/// tick that closes/charges/cancels/GCs from DAG-derived staleness
/// inferences cannot be CALLED while the DAG is empty-but-not-
/// authoritative (the pre-recovery window, or a tenure whose recovery
/// failed): adding a future destructive tick outside the gate is a
/// compile error, not a review item. Deliberately not `Clone`/`Copy`
/// and never stored — minted per tick, dropped at tick end.
pub(crate) struct DagAuthority(());

impl DagActor {
    /// Mint the tick-scoped DAG-authority witness, or `None` when the
    /// DAG must not be treated as ground truth (see
    /// [`DagActor::dag_authoritative`]).
    pub(super) fn dag_authority(&self) -> Option<DagAuthority> {
        self.dag_authoritative.then_some(DagAuthority(()))
    }

    // r[impl sched.attempt.cancel-close-driven+3]
    /// The ONLY production latch site for the status outbox
    /// (merged_bug_004 hole 1): per-drv supersession — each drv the
    /// new batch names is stripped from every queued batch's
    /// `drv_hashes`, so at most ONE pending status exists per drv
    /// queue-wide and FIFO can never replay an older same-drv truth
    /// whose own stamp would refuse the newer one (two terminal
    /// latches for one reaped drv: the older replayed first, stamped
    /// the comparand, and inverted the durable row while the warn
    /// claimed "the newer rows stand"). The flush-time re-derivation
    /// already enforces newest-wins for DAG-PRESENT nodes; the
    /// DAG-absent terminal pair was the unguarded cell.
    ///
    /// `exec_ids` STAY on the superseded batch (bug_158: the close is
    /// exec-scoped and unconditional — those attempts ended at THEIR
    /// latch instant whatever the drv's newest truth is; an emptied
    /// batch still flushes close-only).
    ///
    /// Interleaving: the actor is single-threaded, and the flusher's
    /// Fenced/Err `push_front` re-queues are pop-returns (the batch
    /// re-enters at the position it left, with its drv set already
    /// superseded by any later latch) — no new-latch call can run
    /// between the flusher's pop and its re-queue, so the at-most-one
    /// invariant holds across flush ticks by construction. Push-site
    /// census rides the owning commit ([GEN-SET]:
    /// `rg -n 'status_outbox\.push_back' rio-scheduler/src/` = this
    /// chokepoint + the two direct-latch test pushes).
    pub(super) fn latch_status_batch(&mut self, batch: StatusBatch) {
        for queued in &mut self.status_outbox {
            queued
                .drv_hashes
                .retain(|h| !batch.drv_hashes.iter().any(|b| b == h));
        }
        self.status_outbox.push_back(batch);
        metrics::gauge!("rio_scheduler_status_outbox_depth").set(self.status_outbox.len() as f64);
    }
}

/// One controller-witnessed terminal mark (see
/// [`DagActor::witnessed_terminal`]): when the controller FIRST
/// witnessed the attempt's pod terminal, and the letter it classified
/// it with. The clock anchors the establishment window for the marked
/// attempt; the letter is disclosed at establishment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WitnessedTerminal {
    /// Epoch seconds of the FIRST witnessing report. Level-triggered
    /// re-reports never advance it — the establishment window
    /// (`witnessed_at + establishment_report_slack`) anchors here.
    pub(crate) witnessed_at: f64,
    /// The wire letter as witnessed (`AttemptTerminalReason`).
    pub(crate) reason: rio_proto::types::AttemptTerminalReason,
}

pub struct DagActor {
    /// The global derivation DAG.
    dag: DerivationDag,
    /// Active builds indexed by build_id.
    builds: HashMap<Uuid, BuildInfo>,
    /// Per-build event broadcast channels + progress-debounce state.
    /// See [`BuildEventBus`].
    events: BuildEventBus,
    /// Kube-authoritative `drv_hash → (spec.nodeName, tenant)` from
    /// the controller's pod informer (`AckSpawnedIntents.bound_intents`).
    /// Read by the pull mint (AD2c `source_node` attribution) and the
    /// establishment sweep's charge row; `node` is
    /// controller-authoritative — worker-supplied identity is never
    /// trusted for node attribution. `tenant` is captured
    /// scheduler-side at Ack time from `dag.node(h)?.attributed_tenant()`
    /// when DAG-present, else carried forward — once DAG-absent the
    /// last DAG-present value sticks (mb_012: both projections come off
    /// one entry so they structurally cannot diverge).
    ///
    /// Wholesale-rebuilt on Acks whose `binding_snapshot` is PRESENT
    /// (~10s; the nodeclaim_pool reconciler attaches the full set
    /// every tick — present-and-EMPTY clears the map: scale-to-zero,
    /// C2/285) — entries for deleted pods drop naturally; no separate
    /// sweep. The per-pool reconciler sends no snapshot (it only arms
    /// `dispatched_cells`); absent = "this Ack carries no binding
    /// snapshot" = no-op on this map (and the legacy non-empty
    /// `bound_intents` arm keeps pre-upgrade controllers working —
    /// R9 read-side back-compat, never dual-written). READ by: the
    /// pull mint (`pull.rs` AD2c source-node attribution + the
    /// rendered-deadline floor), the completion fold
    /// (`completion.rs`), and the establishment sweep
    /// (`housekeeping.rs`) — the entries are live attribution inputs,
    /// which is exactly why a stale scale-to-zero map mis-attributed
    /// the next re-dispatch (bug_285).
    pub(crate) authoritative_binding: HashMap<DrvHash, AuthBinding>,
    /// 124(d): `drv_hash → epoch_secs` of the most recent controller
    /// `AckSpawnedIntents{spawned}` covering that intent — the "a Job
    /// was just created for this" witness. A `NoEligibleSource`
    /// verdict arriving within [`pull::ACKED_SPAWNED_DEFER_SECS`] of
    /// the ack is deferred (the verdict raced its own spawn: the gate
    /// evaluated a tick where the Job did not exist yet). Written for
    /// EVERY spawned intent (unlike `dispatched_cells`, which only
    /// arms band-targeted ones); entries older than twice the defer
    /// window are pruned opportunistically at the ack site; cleared
    /// in the leadership wipe (stale hashes must not defer a
    /// successor-generation verdict).
    pub(crate) acked_spawned: HashMap<DrvHash, f64>,
    /// Phase 1b: per-derivation count of re-deliveries of a failure
    /// completion whose appending transaction (attempt row, `decide()`,
    /// status persist) failed. Bounded by
    /// [`MAX_ATTEMPT_RECORD_REDELIVERIES`]; cleared when a later
    /// delivery for the derivation is handled. In-memory only — after a
    /// restart the derivation is still in its pre-report state and the
    /// backstop sweep re-drives it.
    pub(crate) attempt_record_retries: HashMap<DrvHash, u32>,
    /// live_058-c: controller-witnessed terminal marks for OPEN pull
    /// attempts, keyed by exec_id — the establishment sweep's
    /// witnessed-clock anchor. Minted by the `ReportAttemptOutcome`
    /// unclassified-open arm (pull.rs): a pod-terminal letter for an
    /// open attempt with no worker classification row means the pod
    /// is GONE — the worker's own report can only still be IN FLIGHT,
    /// never future — so the sweep establishes marked attempts at
    /// `witnessed_at + establishment_report_slack` instead of
    /// dead-waiting the dispatch deadline (the live incident's
    /// deadline=9803s solve dead-waited ≈2h45m per re-OOM loop
    /// iteration). Idempotent under the controller's level-triggered
    /// re-reports (one per tick while the pod stays listable,
    /// `report_terminated_pods`): first-witnessed-wins — a re-report
    /// never advances the clock (advancing would indefinitely defer
    /// establishment) and never re-fires anything; it re-creates the
    /// mark only when absent (the post-failover re-arm). In-memory BY
    /// DESIGN (the wave's single DDL allocation is spoken for; the
    /// durable-column arm is the recorded rejected alternative): lost
    /// on scheduler failover, re-armed by the next level-triggered
    /// re-report while the pod stays listable; beyond that window the
    /// deadline anchor backstops — degradation is bounded by the OLD
    /// behavior, never worse. Pruned at establishment (the sweep
    /// consumes the mark) and structurally against the open-attempt
    /// view every sweep (a mark whose attempt resolved through any
    /// other path dies on the next pass).
    pub(crate) witnessed_terminal: HashMap<Uuid, WitnessedTerminal>,

    /// bughunt-2 slot 3 C2 (merged_bug_032), re-keyed round 3
    /// (merged_bug_013): corroboration evidence for worker-supplied
    /// `store_degraded` flags — most-recent flagged report per
    /// CONTROLLER-AUTHORITATIVE node binding (AD2c: the worker cannot
    /// forge its corroboration identity). Keyed by NODE, non-optional:
    /// a binding-less report is NOT inserted at all — the pre-fix
    /// `Option<String>` key let one node's pre-binding `None` sighting
    /// plus its later attributed `Some` sighting count as 2 "distinct
    /// nodes" and self-corroborate into the uncharged Paced lane.
    /// Binding-less evidence rides only the store-health leg.
    /// In-memory only: forgotten on failover, self-healing (the gate
    /// re-corroborates within one window).
    pub(crate) store_degraded_sightings: HashMap<String, Instant>,
    /// The scheduler-side store-health OR-leg: the last time one of the
    /// scheduler's OWN store RPCs (the dispatch-time FindMissingPaths
    /// probes) failed or timed out. Covers the fleet-of-one case where
    /// two distinct nodes can never be observed.
    /// merged_bug_179: store-health evidence for the corroboration
    /// gate. Written ONLY through `note_issued_store_rpc_failure` —
    /// the issued-RPC chokepoint.
    pub(crate) last_store_rpc_failure: Option<Instant>,
    /// Retry policy.
    retry_policy: RetryPolicy,
    /// Establishment report slack for open pull-mode attempts
    /// (config `establishment_report_slack_secs`, default 120 s): how
    /// long past the intent deadline the establishment sweep waits for
    /// a terminal report before establishing the attempt.
    establishment_report_slack: std::time::Duration,
    /// Retention (days) for terminal unreferenced drv_executions rows
    /// (`store.log.sweep-ownership` second deleter).
    exec_retention_days: u32,
    /// Poison threshold + distinct-workers config. Replaces the
    /// former `POISON_THRESHOLD` const (3). Default matches prior
    /// behavior: 3 distinct workers.
    poison_config: PoisonConfig,
    /// Substitution-replacement materialization config
    /// (`[materialization]` table, config `materialization`). Phase A
    /// deployed state is `enabled = false`: the pull-admission shim
    /// passes the flag to the kernel's kinded wrapper (bit-identical to
    /// as-built when off), and every other consumer (job creation,
    /// consumption, establishment) is flag-gated on it.
    materialization_cfg: crate::config::MaterializationConfig,
    /// In-memory materialization-job view: `drv_hash → JobViewEntry`
    /// (substitution-replacement Phase A). A droppable cache — never
    /// written back, populated only by the flag-gated creation paths
    /// (so flag-off it is permanently empty), consulted by pull
    /// admission's job-view projection. Authority lives in PG
    /// (`materialization_jobs` + the partial-unique dedup index);
    /// recovery rebuild is Phase B.
    materialization_jobs: materialize::JobViewState,
    // r[impl sched.attempt.cancel-close-driven+3]
    /// Terminal-status batches whose persist FAILED, latched for the
    /// housekeeping tick to re-drive until a persist succeeds ("latch
    /// on Ok only"). The persist is what closes the batch's assignment
    /// rows — losing it left cancelled derivations' attempts open
    /// forever, to be mis-charged as executor crashes by the
    /// establishment sweep (bug_347). Leader-scoped: cleared on
    /// leadership loss (the new leader's recovery + the charge-free
    /// establishment arm own the rows).
    status_outbox: std::collections::VecDeque<StatusBatch>,
    /// Realized-path carriers whose stale-reset job creation came back
    /// fenced/failed (merged_bug_257), retried by the housekeeping
    /// tick until the row applies or the node goes terminal/gone.
    /// Leader-scoped IN-MEMORY by design (do not durable-table this):
    /// a leadership-loss drop is the PG-authority class — any APPLIED
    /// creation already persisted the carrier on the job row, and the
    /// successor re-derives from durable state; only the
    /// never-applied window is lost, counted by
    /// `rio_scheduler_materialization_carrier_dropped_total`.
    pending_carriers: Vec<(crate::state::DrvHash, Vec<String>)>,
    /// sh-002 row 4 (coalesce-outcomes), the FIRST-level accumulator:
    /// queued `ActorCommand::ReportPullOutcome` commands — exec_id,
    /// auth_intent, payload, AND the reply sender — held between
    /// `handle_report_outcome`'s push and the
    /// [`flush_pending_pull_outcomes`](Self::flush_pending_pull_outcomes)
    /// that runs every per-item consumption then sends every reply
    /// only after the batched completion commits (the
    /// ack-after-durable contract). Leader-scoped:
    /// `handle_leader_lost` DRAINS it sending `Err(NotLeader)` on
    /// every held reply (never `clear()` — a silently dropped reply
    /// would leave store-side `report_until_acked` believing
    /// ack'd-then-lost). Listed in `clear_persisted_state` only to
    /// satisfy the exhaustive destructure; the drain runs in
    /// `handle_leader_lost` BEFORE the wipe.
    pending_pull_outcomes: Vec<pull::PendingReport>,
    /// sh-027 §3, flush trigger (iv): the
    /// [`REPORT_OUTCOME_FLUSH_DEADLINE`](pull::REPORT_OUTCOME_FLUSH_DEADLINE)
    /// (250ms) select! arm — bounds ack latency for sub-BATCH_MAX
    /// inbound rates without coupling to `tick_interval`. Replaces
    /// the retired mailbox-empty signal (sh-002 trigger iv), which
    /// degraded N̄ to ~5.5 under interleaving with
    /// `ListMaterializationJobs`/`SubstituteProgress`.
    /// `MissedTickBehavior::Delay` (house style — every long-lived
    /// Interval in-tree sets it explicitly); `reset()` on the
    /// empty→nonempty transition (`handle_report_outcome`) so an
    /// idle-stale deadline cannot flush the first report at N=1.
    pull_flush_deadline: tokio::time::Interval,
    /// sh-002 row 4, the SECOND-level (flush-scoped) accumulator:
    /// `(drv_hash, WalkVerified(..))` pairs the Success consumption
    /// arm pushes INSTEAD of calling
    /// `complete_ready_from_store_batch(len=1)` inline. Drained at
    /// the tail of every `flush_pending_pull_outcomes` into ONE
    /// batched call. Always empty between flushes; the carried-paths
    /// `output_paths` stamp runs per-item BEFORE the push (Hazard Q
    /// — `dispatch.rs`'s `output_paths.is_empty()` back-fill must
    /// see the realized floating-CA path).
    pending_walk_completed: Vec<(crate::state::DrvHash, crate::db::live_pins::StampProvenance)>,
    /// Database handle.
    db: SchedulerDb,
    /// Store service client for scheduler-side cache checks. `None` in tests
    /// that don't need the store (cache check is then skipped).
    store_client: Option<StoreServiceClient<Channel>>,
    /// Timeout for metadata gRPC calls to the store (FindMissingPaths,
    /// QueryPathInfo). Defaults to [`rio_common::grpc::DEFAULT_GRPC_TIMEOUT`]
    /// (30s). Tests that arm a hung MockStore to prove the timeout wrapper
    /// exists override to 3s via
    /// `with_grpc_timeout` — same
    /// wrapper-exists proof at 10× less wall-clock. Plumbed as a field
    /// (not `cfg(test)` on the const) because `cfg(test)` is per-crate:
    /// rio-scheduler's test build links against rio-common built WITHOUT
    /// `cfg(test)`, so a test-gated constant there is invisible here.
    grpc_timeout: std::time::Duration,
    /// Circuit breaker for the cache-check FindMissingPaths call. Owned by
    /// the actor (single-threaded, no lock needed). Checked/updated in
    /// `merge.rs::check_cached_outputs`.
    cache_breaker: CacheCheckBreaker,
    /// ADR-023 per-`(pname, system, tenant)` fitted curves. Feeds
    /// `compute_spawn_intents` (SpawnIntent population) and
    /// dispatch's resource-fit filter via [`crate::sla::solve::intent_for`].
    /// Shared with [`crate::sla::estimator_poller`] (sh-018b: refresh
    /// runs OFF the actor turn); the actor only reads — every
    /// `SlaEstimator` field is interior-`RwLock` and the per-key
    /// `cache.write()` hold is sub-µs.
    pub(crate) sla_estimator: Arc<crate::sla::SlaEstimator>,
    /// Tier ladder from `cfg.sla.solve_tiers()` (sorted tightest-first).
    /// Shared between the tick `refresh()` (Schmitt-trigger reassign)
    /// and `solve_intent_for` so both see the SAME ladder.
    pub(crate) sla_tiers: Vec<crate::sla::solve::Tier>,
    /// §13c-3: hard ceilings from `Ceilings::from_resolved(&cfg.sla,
    /// cost_table.read().resolved_global())` — the boot-resolved
    /// (catalog-derived under Spot) global, not the configured
    /// `Option<>` override. Set once at spawn.
    pub(crate) sla_ceilings: crate::sla::solve::Ceilings,
    /// Full `[sla]` config — feeds [`crate::sla::explore::next`]'s
    /// probe shape and feature overrides.
    pub(crate) sla_config: crate::sla::config::SlaConfig,
    /// ADR-023 phase-13 hw-band cost table — `$/vCPU·hr` per
    /// `(band, cap)` + per-band λ. `Arc<RwLock<_>>` shared with
    /// `spot_price_poller` (lease-gated, 10min tick); the actor reads a
    /// snapshot per `solve_intent_for` call. Seed-backed Default →
    /// `solve_full` always has a comparable scalar even before the
    /// first poll.
    pub(crate) cost_table: Arc<parking_lot::RwLock<crate::sla::cost::CostTable>>,
    /// Shared edge-reload latch — see [`DagActorPlumbing::cost_was_leader`].
    /// `handle_ack_spawned_intents` gates its `cost_table.write()` on
    /// this so observations don't land on the pre-reload table and get
    /// clobbered by `interrupt_housekeeping`'s `*cost = load()`.
    pub(crate) cost_was_leader: Arc<AtomicBool>,
    /// `interrupt_housekeeping` wake-up — see
    /// [`DagActorPlumbing::cost_reload_notify`].
    pub(crate) cost_reload_notify: Arc<tokio::sync::Notify>,
    /// In-process insufficient-capacity mask. `handle_ack_spawned_intents`
    /// marks cells the controller reported `unfulfillable`; the
    /// per-dispatch read-time mask (`A \ masked`) is applied in
    /// `solve_intent_for` AFTER reading the memo, so unmasking is free.
    pub(crate) ice: Arc<crate::sla::cost::IceBackoff>,
    /// §13a interim ICE-clear path: `handle_ack_spawned_intents`
    /// records the FULL A' cell-set of the controller-acked
    /// `SpawnIntent` per drv (arm-on-**ack**, not arm-on-emit —
    /// `solve_intent_for` is read-only so dashboard/CLI polls don't
    /// leak entries); the first successful pull (the mint in
    /// `actor/pull.rs`) looks it up and `ice.clear()`s **iff
    /// `len()==1`** (a delivered pull ⇒ pod scheduled ⇒ ∃ cell ∈ A'
    /// with capacity; for `|A'|>1` it identifies none — bug_030).
    /// Removed on that edge or the `handle_tick` DAG-state sweep
    /// (cancel/substitute/terminal). §13b's
    /// `AckSpawnedIntents.registered_cells` (NodeClaim watcher)
    /// supersedes this once wired. SmallVec:
    /// `|A'| ≤ |H|×|CapacityType::ALL|` (= |H|×2; 4 typical at 2
    /// hw_classes; spills at |H|≥3).
    pub(crate) dispatched_cells:
        dashmap::DashMap<DrvHash, smallvec::SmallVec<[crate::sla::config::Cell; 4]>>,
    /// Per-key admissible-set memo. Keyed on `(model_key_hash,
    /// override_hash)`; `(inputs_gen, fit_content_hash)` are staleness
    /// fields, so most `compute_spawn_intents` ticks are pure cache
    /// hits (ADR-023 L616). `inputs_gen` is **derived** from the
    /// `(HwTable, CostTable)` solve-relevant projection at poll time
    /// via [`crate::sla::solve::SolveInputs::inputs_gen`] — nobody bumps; the
    /// pollers just write to the tables.
    pub(crate) solve_cache: Arc<crate::sla::solve::SolveCache>,
    /// Tick counter for periodic tasks that run less often than every
    /// Tick (e.g., estimator refresh every ~60s with a 10s tick interval).
    /// Wraps at u64::MAX — harmless, just means the 60s cadence drifts
    /// by one tick after ~5.8 billion years.
    tick_count: u64,
    /// Whether backpressure is currently active. Shared with ActorHandle
    /// so hysteresis (80%/60%) is honored by send() instead of a simple
    /// threshold check. `Arc<AtomicBool>` for lock-free reads on the hot path.
    backpressure_active: Arc<AtomicBool>,
    /// Leader-election shared state: `generation` (assignment-token /
    /// stale-work nonce), `is_leader` (dispatch gate), `recovery_complete`
    /// (dispatch gate). Same Arcs as the lease task and `ActorHandle` —
    /// the lease task writes `is_leader`/`generation`/
    /// `acquired_transitions` via [`LeaderState::on_acquire`]/
    /// [`LeaderState::on_rebound`]/[`LeaderState::on_lose`] (the latter
    /// two also clear the completion stamp); the actor raises
    /// `generation` during recovery via
    /// [`LeaderState::seed_generation_from`] (the PG-floor seed — a
    /// `fetch_max` placed after the TOCTOU gate) and writes the
    /// epoch-keyed completion stamp via
    /// [`LeaderState::set_recovery_complete`] /
    /// [`LeaderState::invalidate_recovery_completion`]; the lease task
    /// additionally advances the renew-round counters
    /// ([`LeaderState::begin_renew_round`] /
    /// [`LeaderState::confirm_leading_round`]) that the
    /// bump-confirmation wait reads back; everything else is
    /// `SeqCst`/`Acquire` reads. See [`LeaderState`] for the
    /// multi-field ordering rationale.
    ///
    /// u64 generation, not i64: the proto is `uint64`
    /// (`WorkAssignment.generation`). PG's `assignments.generation` is
    /// BIGINT (signed); cast `u64 as i64` at THAT single boundary
    /// instead of at every proto-encode site.
    leader: LeaderState,
    // r[impl sched.evidence.durability+4]
    /// The lease generation of the tenure that built the CURRENT
    /// in-memory DAG state — what every claims-floor-fenced evidence
    /// write carries as its serving generation. Written in exactly two
    /// places: [`DagActor::new`] (initial lease read) and
    /// `handle_leader_acquired` (re-stamped to the claim target
    /// immediately after the generation claim succeeds, BEFORE any of
    /// the new tenure's evidence writes run). NEVER re-read from
    /// `self.leader.generation()` per command: the lease task can bump
    /// that atomic mid-mailbox (a same-process cross-epoch re-acquire),
    /// and a command queued under the old tenure must keep carrying the
    /// old tenure's number so the claims-floor fence refuses its writes
    /// instead of letting them ride the new tenure's authority while
    /// operating on the old tenure's DAG. `handle_leader_lost`
    /// deliberately leaves the field at the deposed tenure's value:
    /// stale-by-construction, it sits below any successor's claimed
    /// floor, which is exactly what makes the fence reject the deposed
    /// replica's queued-command writes.
    ///
    /// i64 (not u64 like the lease atomic): this value exists solely to
    /// be compared against PG BIGINT floors; converting once at the
    /// stamp sites keeps every fence comparison cast-free.
    serving_generation: crate::db::ServingGeneration,
    /// Cached [`SchedulerDb::max_known_generation`] for the per-pull
    /// generation fence (sh-007 row 1). Refreshed at exactly two
    /// sites: `handle_leader_acquired` (set to the freshly-claimed
    /// generation — the claim row is the post-claim floor) and the
    /// `handle_tick` head (one PG read per tick interval). The
    /// per-pull comparison reads this field instead of issuing a PG
    /// round-trip; the cached value is at most one tick interval
    /// stale, so a deposed replica's `serving_generation < floor`
    /// self-reject lags the durable floor by at most one tick — the
    /// PG-side `FencedTx` check at `mint_pull_attempt_fenced` remains
    /// the hard gate, this advisory check delays the soft self-reject
    /// only. `None` = fresh cluster (no claims, no assignments) — same
    /// shape as the PG read it caches.
    generation_floor_cached: Option<i64>,
    /// Ordering tripwire for the claim-before-recovery-writes invariant
    /// (`sched.evidence.durability`): false at `handle_leader_acquired`
    /// entry, true once the generation claim has stamped
    /// `serving_generation`. The recovery-time fenced-write site (the
    /// expired-at-load poison clear in `load_dag_from_rows`)
    /// `debug_assert!`s it so a future re-ordering of
    /// `handle_leader_acquired` (claim moved back after
    /// `recover_from_pg`) fails loudly in tests instead of silently
    /// re-introducing self-fenced recovery writes in the saturated-floor
    /// regime.
    recovery_claim_stamped: bool,
    /// True only while `self.dag` reflects PG: set in
    /// `complete_tenure` (sole writer, reachable only with the
    /// `RecoveredDag` witness from this tenure's successful
    /// `recover_from_pg`), cleared by every
    /// [`clear_persisted_state`](Self::clear_persisted_state) caller
    /// (LeaderLost, recovery start, the TOCTOU flap discard, and the
    /// failed-recovery Err arm).
    ///
    /// NOT the same thing as [`LeaderState::recovery_complete`]:
    /// since bug_155 both are written only by `complete_tenure` (the
    /// `RecoveredDag` witness path — a failed recovery completes
    /// nothing and requests a step-down per
    /// `sched.recovery.step-down`), but they still differ on CLEAR:
    /// `recovery_complete` is epoch-keyed lease state while this bit
    /// tracks the in-memory DAG, cleared by every
    /// `clear_persisted_state` caller. Destructive consumers that
    /// infer "stale" from "not in the DAG" take the `DagAuthority`
    /// witness (minted from THIS bit) — never `recovery_complete`.
    ///
    /// Initialized from the `LeaderState` constructor semantics
    /// (`plumbing.leader.recovery_complete()`): `always_leader`
    /// (non-K8s / single-scheduler / test default) starts true — no
    /// lease loop ever sends `LeaderAcquired` there, and the never-
    /// cleared DAG is all the state that exists; `pending` (K8s mode)
    /// starts false until the first successful recovery.
    dag_authoritative: bool,
    /// Lease holder identity (the pod name), recorded on
    /// `leader_generation_claims` rows and compared against them on
    /// re-acquire — see [`DagActorPlumbing::holder_id`]. Empty in
    /// non-K8s/test mode.
    holder_id: String,
    /// Weak clone of the actor's own command sender, for scheduling delayed
    /// internal commands (e.g., terminal build cleanup). Weak so the actor
    /// doesn't prevent channel close when all external handles are dropped.
    /// `None` if spawned via bare `run()` (no delayed scheduling).
    self_tx: Option<mpsc::WeakSender<ActorCommand>>,
    /// I-204: capability-hint features stripped at DAG insertion.
    /// Stored on the actor (not just the DAG) because
    /// `clear_persisted_state` replaces `self.dag` on every leader
    /// transition — this copy is what survives.
    pub(crate) soft_features: Vec<String>,
    /// HMAC signer for assignment tokens. When Some, dispatch
    /// signs a Claims { executor_id, drv_hash, expected_output_paths,
    /// expiry } into WorkAssignment.assignment_token. The store
    /// verifies on PutPath — a worker can only upload outputs
    /// matching a valid assignment.
    ///
    /// None = tokens are the legacy format-string (unsigned).
    /// Store with hmac_verifier=None accepts both (dev mode).
    /// Arc because assign_to_worker is hot path and cloning the
    /// underlying key Vec on every dispatch would allocate.
    hmac_signer: Option<Arc<rio_auth::hmac::HmacSigner>>,
    /// HMAC signer for `x-rio-service-token`. When Some, the
    /// dispatch-time store-check
    /// (`dispatch::DagActor::batch_probe_cached_ready`) sets
    /// `x-rio-service-token` + `x-rio-probe-tenant-id` so the store's
    /// upstream-substitution probe fires —
    /// `r[sched.dispatch.fod-substitute]`. None = local-presence-only
    /// (the pre-fix behaviour).
    service_signer: Option<Arc<rio_auth::hmac::HmacSigner>>,
    /// Shutdown token. When cancelled (SIGTERM via `shutdown_signal`),
    /// the run loop breaks. Without
    /// this, `serve_with_shutdown` deadlocks on open bidi streams
    /// because the `SchedulerGrpc` that holds an `ActorHandle`
    /// (sender) is itself held by the server's handler registry —
    /// circular wait.
    ///
    /// Default (from `new()`) is a fresh never-cancelled token →
    /// tests and non-production constructors are unchanged.
    shutdown: rio_common::signal::Token,
    /// `(tenant, required_features)` tuples already counted and WARNed
    /// as unroutable in `solve_intent_for` (no hwClass `provides_features`
    /// hosts the tuple). Debounces `rio_scheduler_unroutable_features_total`
    /// and the companion `warn!` to once-per-edge — without it the emit
    /// fires per-drv per-`GetSpawnIntents` poll (~hundreds/min), and is
    /// also the only emit in `solve_intent_for` that sits BEFORE
    /// `was_miss` is declared so none of the memo-anchored debounce
    /// gates apply (mb_031). `Mutex` because `solve_intent_for` is
    /// `&self`; the actor is single-threaded so contention is zero.
    ///
    /// `LruCache` capped at [`UNROUTABLE_FEATURES_WARNED_CAP`] (mb_001):
    /// the key's second component is tenant-controlled
    /// (`requiredSystemFeatures` verbatim from the drv), so a `HashSet`
    /// — even with the per-entry 64×32 ASCII clamp — grows unbounded in
    /// distinct keys (`["x-1"]`, `["x-2"]`, …). The clamp bounds bytes
    /// per entry (~2 KiB); the LRU bounds entry COUNT. Eviction under
    /// pressure re-arms the warn for the evicted key — fail-safe
    /// over-emit, bounded by cap × eviction churn. The metric carries
    /// only `tenant` (bounded by `Claims.sub`). Re-arms on pod restart
    /// (config change rolls the pod via the scheduler.yaml checksum).
    unroutable_features_warned: parking_lot::Mutex<lru::LruCache<(String, Vec<String>), ()>>,
    /// `(tenant, pname, cap)` tuples already WARNed as a `--capacity`
    /// override pin the reference class doesn't host (mb_003 / r31 A3).
    /// The bypass-path `Some(cap)` arm in [`DagActor::bypass_cells`]
    /// drops the cell when `cap ∉ capacity_types_for(h)` — without the
    /// warn, the operator's pin is silently ignored for the override
    /// TTL with no signal. Debounced once-per-edge so the per-drv
    /// per-poll cadence doesn't flood. Same `LruCache` shape as
    /// `unroutable_features_warned`: `pname` is tenant-controlled.
    /// Retained across `clear_persisted_state` for the same reason
    /// (override config doesn't change on leader transition).
    cap_mismatch_warned:
        parking_lot::Mutex<lru::LruCache<(String, String, crate::sla::config::CapacityType), ()>>,
    /// `(drv_hash, reason)` pairs already counted in
    /// `forecast_dropped_total` (r34 bug_018). The forecast loop in
    /// `compute_spawn_intents` runs once per poll; without a debounce
    /// a Queued drv with a slow Running dep increments the counter on
    /// every controller poll + scheduler tick + dashboard refresh —
    /// `(poll_rate)×(stuck drvs)`, not `(drop events)`. Debounced
    /// once-per-edge so the metric means "unique drop events" as
    /// documented in [`crate::sla::metrics::describe_all`]. Same
    /// `LruCache` shape as `unroutable_features_warned` /
    /// `cap_mismatch_warned`. Re-arms on eviction (fail-safe over-emit)
    /// and on pod restart. Retained across `clear_persisted_state` —
    /// the drv set doesn't change on leader transition.
    forecast_dropped_warned: parking_lot::Mutex<lru::LruCache<(String, &'static str), ()>>,
    /// live_050(e)/live_051(c): supply-revalidation state — the
    /// per-drv consecutive no-hosting-class verdict counters
    /// (tenure-scoped) + the emission-revalidation disclosure
    /// debounce, one typed home with a type-enforced once-per-edge
    /// gate (the r35 STRIKE-3 tripwire's asked-for shape, scoped to
    /// this family — see [`snapshot::SupplyRevalidation`]).
    supply_reval: snapshot::SupplyRevalidation,
    /// Advances once per `handle_tick`. The ready-set store
    /// short-circuit (`sweep_ready_cached`) stamps each ADMITTED
    /// node's `probed_generation` with this value and skips
    /// already-stamped nodes within the same generation, so no node is
    /// FMP-probed twice per tick; [`DagActor::probe_quota`] keys on
    /// the same value, so per-tick admissions are bounded by
    /// [`DISPATCH_PROBE_TICK_QUOTA`] in aggregate and the truncated
    /// tail advances on the NEXT tick (oldest-generation first), never
    /// within-tick. Starts at 1 so freshly-inserted nodes
    /// (`probed_generation: 0`) are immediately eligible.
    probe_generation: u64,
    /// Per-generation probe-admission ledger for
    /// [`DISPATCH_PROBE_TICK_QUOTA`]. Expires STRUCTURALLY by key: the
    /// first sweep of a new `probe_generation` observes the stale
    /// generation and resets the count — no tick-site reset code path
    /// to bypass (the merged_bug_033 early-exit-ledger shape is
    /// unrepresentable).
    probe_quota: ProbeQuotaLedger,
    /// Receiver half of the admin fast lane (round-9 B8). Drained with
    /// priority between mailbox commands (the biased select arm in
    /// `run_inner`) and at every Tick phase boundary (the `phase!`
    /// macro), so Fast-lane admin delivery is bounded by the largest
    /// indivisible work slice instead of the whole mailbox FIFO. The
    /// actor owns the receiver as a FIELD precisely so `handle_tick`
    /// can drain mid-Tick.
    admin_fast_rx: mpsc::Receiver<FastAdmin>,
    /// Sender template for the fast lane — cloned into [`ActorHandle`]
    /// at spawn via [`DagActor::admin_fast_sender`]. The actor holding
    /// a sender never wedges shutdown (the loop exits via the
    /// cancellation token, not channel closure).
    admin_fast_tx: mpsc::Sender<FastAdmin>,
    /// Per-turn work-cost EWMA in seconds (round-9 B6) — fed by
    /// [`DagActor::note_turn_cost`] after every mailbox command and
    /// every fast-lane serve; consumed by `update_backpressure`'s
    /// cost axis (projected drain = depth × this). Plain `f64`: only
    /// the single-threaded actor loop reads/writes it.
    turn_cost_ewma_secs: f64,
    /// Last [`ClusterSnapshot`] published by `handle_tick`. The
    /// AdminService `cluster_status` handler reads `snapshot_tx.
    /// subscribe().borrow()` via [`ActorHandle::cluster_snapshot_cached`]
    /// — a watch-channel cache, not a mailbox round-trip — so `xtask
    /// status` / autoscaler polls stay alive regardless of mailbox
    /// depth (I-163: 30s timeouts when 9.5k commands queued ahead of a
    /// 37µs handler). Up to one Tick stale.
    // r[impl sched.admin.snapshot-cached]
    snapshot_tx: watch::Sender<Arc<ClusterSnapshot>>,
    /// Test-only: oneshot pair for deterministic interleaving in
    /// `handle_leader_acquired`. When set, the actor sends on `.0`
    /// after `recover_from_pg()` returns, then awaits `.1` before
    /// the gen re-check. Lets the TOCTOU test bump `generation`
    /// between recovery completion and the staleness check —
    /// simulating a lease flap mid-recovery without mocking PG.
    #[cfg(test)]
    recovery_toctou_gate: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    /// Test-only: fail the next `recover_from_pg()` DAG-load phase up
    /// front (the independent PG-floor read is unaffected). See
    /// `DagActorPlumbing::fail_next_recovery_load`.
    #[cfg(test)]
    fail_next_recovery_load: bool,
    /// Test-only: fail the next independent PG-floor read (the DAG
    /// load is unaffected). See
    /// `DagActorPlumbing::fail_next_floor_read`.
    #[cfg(test)]
    fail_next_floor_read: bool,
    /// `DagActorPlumbing::bump_claims_floor_before_fence_write`
    /// (bug_015's deposed-writer injection hook).
    #[cfg(test)]
    bump_claims_floor_before_fence_write: bool,
    /// Test-only: fail the next infra-failure appending transaction.
    /// See `DagActorPlumbing::fail_next_attempt_append`.
    #[cfg(test)]
    fail_next_attempt_append: bool,
    /// Test-only: fail the next job-view load inside
    /// `recover_from_pg()` (after the DAG and builds loaded) — the
    /// merged_bug_246 required-load arm. See
    /// `DagActorPlumbing::fail_next_job_view_load`.
    #[cfg(test)]
    fail_next_job_view_load: bool,
    /// Test-only structural counters. Asserting on these (rather than
    /// wall-clock or absence-of-side-effect) makes the I-163 / I-139
    /// regression tests fail under their target mutation.
    #[cfg(test)]
    pub(crate) test_counters: TestCounters,
    /// Test-only: synthetic long-Tick hook (W9-AG). `Some((n, d))` —
    /// the next `n` phase bodies of `handle_tick` each sleep `d` of
    /// REAL time before completing. Armed via
    /// [`DebugCmd::StallTickPhases`]; consumed (decremented) by the
    /// `phase!` boundary in `handle_tick`.
    #[cfg(test)]
    tick_phase_stall: Option<(u32, std::time::Duration)>,
}

/// Per-actor `#[cfg(test)]` call counters. Incremented at the top of
/// the named method; read via [`DebugCmd::Counters`]. Atomics so
/// `&self` callsites (e.g. `persist_status`) can increment without
/// changing the borrow signature.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct TestCounters {
    /// Incremented on every singular `persist_status` call (NOT the
    /// batch variant). Asserts on the I-139 rule: a batched completion
    /// path must NOT touch the per-row helper.
    pub persist_status_calls: std::sync::atomic::AtomicU64,
    /// Incremented on every `solve_inputs()` call. Asserts on the r33
    /// bug_013 hoist: one `compute_spawn_intents` pass over N Ready
    /// drvs must snapshot the solve inputs exactly ONCE, not
    /// once-per-drv (the per-drv re-read is both the §13c-2 gauge spam
    /// and a TOCTOU at the same `inputs_gen` if `spot_price_poller`
    /// writes mid-pass).
    pub solve_inputs_calls: std::sync::atomic::AtomicU64,
    /// Incremented on every `note_fenced_evidence_write` call — the
    /// test-readable mirror of `rio_scheduler_evidence_write_fenced_total`
    /// (`sched.evidence.durability`). The deposed-actor fencing test
    /// asserts it moved; the single-leader batteries assert it stayed 0
    /// (the fence must never reject a live leader's writes).
    pub evidence_writes_fenced: std::sync::atomic::AtomicU64,
    /// Incremented at the head of every
    /// [`complete_ready_from_store_batch`](DagActor::complete_ready_from_store_batch)
    /// call (sh-002 row 4): asserts on the report-coalesce rule — N
    /// queued Success materialization reports must drive ONE batched
    /// completion call, not N per-item ones.
    pub complete_ready_batch_calls: std::sync::atomic::AtomicU64,
    /// Incremented at every actor-level
    /// [`SchedulerDb::max_known_generation`] call site (sh-007 row 1).
    /// Asserts on the cached-floor rule: N PullAssignments drive at
    /// most one Tick-head refresh, not N per-pull PG round-trips.
    pub max_known_generation_reads: std::sync::atomic::AtomicU64,
    /// Incremented at every actor-level `persist_build_counts*` PG
    /// round-trip (sh-007c S5). Asserts on the per-build-tail batch
    /// rule: `complete_ready_from_store_batch` over N interested
    /// builds must drive ≤1 batched counts write, not N serial
    /// `persist_build_counts` awaits inside `update_build_counts_with`.
    pub persist_build_counts_calls: std::sync::atomic::AtomicU64,
    /// Incremented at every consumption-path `begin_fenced` call site
    /// (sh-007c S6): `close_materialization_attempt` and the batched
    /// `close_and_resolve_materialization_batch` wrapper. Asserts on
    /// the O(1)-PG-per-flush rule: N queued materialization reports
    /// must drive ONE fenced close+resolve transaction, not N
    /// per-item `close_materialization_attempt` calls.
    pub begin_fenced_calls: std::sync::atomic::AtomicU64,
    /// Incremented at every per-item
    /// [`companion_release`](DagActor::companion_release) await
    /// (sh-027 §3 phase-D batch). Asserts on the deferred-release
    /// rule: N batched `BatchedCompanion::Release` intents must
    /// drive ZERO per-item `companion_release` awaits — the phase-D
    /// loop collects [`DeferredRelease`](materialize::DeferredRelease)
    /// and runs ONE `companion_release_batch` after.
    pub companion_release_awaits: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl TestCounters {
    pub(crate) fn snapshot(&self) -> TestCountersSnapshot {
        use std::sync::atomic::Ordering::SeqCst;
        TestCountersSnapshot {
            persist_status_calls: self.persist_status_calls.load(SeqCst),
            solve_inputs_calls: self.solve_inputs_calls.load(SeqCst),
            evidence_writes_fenced: self.evidence_writes_fenced.load(SeqCst),
            complete_ready_batch_calls: self.complete_ready_batch_calls.load(SeqCst),
            max_known_generation_reads: self.max_known_generation_reads.load(SeqCst),
            persist_build_counts_calls: self.persist_build_counts_calls.load(SeqCst),
            begin_fenced_calls: self.begin_fenced_calls.load(SeqCst),
            companion_release_awaits: self.companion_release_awaits.load(SeqCst),
        }
    }
}

/// Plain-data snapshot of [`TestCounters`] for the
/// [`DebugCmd::Counters`] reply. `pub` (not `pub(crate)`) only because
/// `DebugCmd` is `pub` and the private-interfaces lint denies the
/// mismatch; `cfg(test)` keeps it out of real builds either way.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TestCountersSnapshot {
    pub persist_status_calls: u64,
    pub solve_inputs_calls: u64,
    /// Mirror of `rio_scheduler_evidence_write_fenced_total` — see
    /// [`TestCounters::evidence_writes_fenced`].
    pub evidence_writes_fenced: u64,
    /// See [`TestCounters::complete_ready_batch_calls`].
    pub complete_ready_batch_calls: u64,
    /// See [`TestCounters::max_known_generation_reads`].
    pub max_known_generation_reads: u64,
    /// See [`TestCounters::persist_build_counts_calls`].
    pub persist_build_counts_calls: u64,
    /// See [`TestCounters::begin_fenced_calls`].
    pub begin_fenced_calls: u64,
    /// See [`TestCounters::companion_release_awaits`].
    pub companion_release_awaits: u64,
}

impl DagActor {
    /// Create a new actor.
    ///
    /// `cfg` holds operator deploy config (scheduler.toml / env);
    /// `plumbing` holds runtime channels and shared leader state. Both
    /// are `Default`-able — tests / non-K8s spawns can
    /// `..Default::default()` and override one or two fields.
    pub fn new(db: SchedulerDb, cfg: DagActorConfig, plumbing: DagActorPlumbing) -> Self {
        let mut dag = DerivationDag::new();
        dag.set_soft_features(cfg.soft_features.clone());
        let max_lead_time = cfg.sla.max_lead_time;
        // Admin fast lane (B8): the actor owns the receiver (field —
        // handle_tick drains it at phase boundaries); the sender is
        // cloned into ActorHandle at spawn.
        let (admin_fast_tx, admin_fast_rx) = mpsc::channel(ADMIN_FAST_LANE_CAPACITY);
        // §13c-3: in production, `main.rs` calls `set_resolved_global`
        // before actor spawn (after `validate_resolved`). Test/non-K8s
        // code that constructs the actor directly with a fresh
        // `DagActorPlumbing::default()` cost_table → wire it up from
        // `cfg.sla`'s `Some(...)` (test fixtures always set both).
        // A half-set fixture is a fixture bug — `validate_shape()`
        // rejects it.
        {
            let mut ct = plumbing.cost_table.write();
            if !ct.has_resolved_global()
                && let (Some(c), Some(m)) = (cfg.sla.max_cores, cfg.sla.max_mem)
            {
                ct.set_resolved_global((c as u32, m));
            }
            // bug_119 (sched.sla.class-membership): install the
            // configured-set membership snapshot on the shared table
            // (unconditional — cfg.sla is process-immutable; same
            // wiring block as the resolved-global above).
            ct.set_member_classes(cfg.sla.hw_classes.keys().cloned());
        }
        // bug_119: snapshot the configured classes BEFORE `cfg.sla`
        // moves into the struct literal below (the ICE mask's gate
        // consumes it after the move point).
        let member_classes: Vec<crate::sla::config::HwClassName> =
            cfg.sla.hw_classes.keys().cloned().collect();
        let sla_ceilings = crate::sla::solve::Ceilings::from_resolved(
            &cfg.sla,
            plumbing.cost_table.read().resolved_global(),
        );
        // Mirror the LeaderState constructor semantics (see the field
        // doc): always_leader starts with recovery_complete=true and no
        // recovery will ever run (no lease loop → no LeaderAcquired),
        // so its DAG counts as authoritative from the start; pending
        // starts false until the first successful recovery's Ok arm.
        let dag_authoritative = plumbing.leader.recovery_complete();

        Self {
            dag,
            builds: HashMap::new(),
            events: BuildEventBus::new(),
            authoritative_binding: HashMap::new(),
            acked_spawned: HashMap::new(),
            attempt_record_retries: HashMap::new(),
            witnessed_terminal: HashMap::new(),
            store_degraded_sightings: HashMap::new(),
            last_store_rpc_failure: None,
            retry_policy: cfg.retry_policy,
            poison_config: cfg.poison,
            establishment_report_slack: cfg.establishment_report_slack,
            exec_retention_days: cfg.exec_retention_days,
            materialization_cfg: cfg.materialization,
            // K8s mode: Unavailable until the first successful
            // recovery hydrates it (merged_bug_246: never
            // Hydrated(empty) over a live DAG). always_leader mode
            // mirrors `dag_authoritative` above — no lease loop ever
            // sends LeaderAcquired, so no recovery will ever hydrate
            // the view; it starts hydrated-empty, which IS the
            // faithful projection there (the DAG is empty at
            // construction and moves with the view; the 246 hole
            // needs a populated DAG over a fabricated-empty view).
            // The dedup re-feed ([E.2]) rehydrates any pre-restart PG
            // rows on first re-encounter; the backstop sweep cancels
            // orphans. Tests start hydrated by default — the
            // direct-setup harness serves without driving recovery
            // (`DagActorPlumbing::start_hydrated_job_view`).
            #[cfg(not(test))]
            materialization_jobs: if dag_authoritative {
                let mut v = materialize::JobViewState::default();
                v.rebuild(std::iter::empty());
                v
            } else {
                materialize::JobViewState::default()
            },
            #[cfg(test)]
            materialization_jobs: if plumbing.start_hydrated_job_view {
                let mut v = materialize::JobViewState::default();
                v.rebuild(std::iter::empty());
                v
            } else {
                materialize::JobViewState::default()
            },
            pending_carriers: Vec::new(),
            pending_pull_outcomes: Vec::new(),
            pull_flush_deadline: {
                let mut iv = tokio::time::interval(pull::REPORT_OUTCOME_FLUSH_DEADLINE);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                iv
            },
            pending_walk_completed: Vec::new(),
            status_outbox: std::collections::VecDeque::new(),
            db,
            store_client: plumbing.store_client,
            grpc_timeout: cfg.grpc_timeout,
            cache_breaker: CacheCheckBreaker::with_mirror(plumbing.breaker_open),
            // sh-018b: shared with `estimator_poller` when main.rs
            // wires it; tests / non-K8s spawns leave plumbing at `None`
            // → constructed from `cfg.sla` here so tests that customize
            // `cfg.sla` keep their config-matched estimator.
            sla_estimator: plumbing
                .sla_estimator
                .unwrap_or_else(|| Arc::new(crate::sla::SlaEstimator::new(&cfg.sla))),
            sla_tiers: cfg.sla.solve_tiers(),
            sla_ceilings,
            sla_config: cfg.sla,
            cost_table: plumbing.cost_table,
            cost_was_leader: plumbing.cost_was_leader,
            cost_reload_notify: plumbing.cost_reload_notify,
            // bug_119: the ICE mask's wire growth seams gate on the
            // same configured-set membership.
            ice: Arc::new(
                crate::sla::cost::IceBackoff::new(max_lead_time).with_members(member_classes),
            ),
            dispatched_cells: dashmap::DashMap::new(),
            solve_cache: plumbing.solve_cache.unwrap_or_default(),
            tick_count: 0,
            backpressure_active: Arc::new(AtomicBool::new(false)),
            // The initial tenure stamp (see the field doc): the lease
            // read at construction time. K8s-mode actors re-stamp it at
            // every LeaderAcquired's generation claim; always-leader
            // actors keep this value for the process lifetime.
            serving_generation: crate::db::ServingGeneration::stamp_from_claim(
                plumbing.leader.generation(),
            ),
            // No floor read yet: the first LeaderAcquired (k8s mode) or
            // first Tick (always-leader) seeds it. None == the PG
            // read's fresh-cluster shape, so the kernel's floor arm is
            // a no-op until then — same as a fresh-DB live read.
            generation_floor_cached: None,
            // No claim has run for this construction-time stamp; the
            // first handle_leader_acquired sets it before recovery.
            recovery_claim_stamped: false,
            leader: plumbing.leader,
            dag_authoritative,
            holder_id: plumbing.holder_id,
            self_tx: None,
            soft_features: cfg.soft_features,
            hmac_signer: plumbing.hmac_signer,
            service_signer: plumbing.service_signer,
            shutdown: plumbing.shutdown,
            unroutable_features_warned: parking_lot::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(UNROUTABLE_FEATURES_WARNED_CAP).unwrap(),
            )),
            cap_mismatch_warned: parking_lot::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(CAP_MISMATCH_WARNED_CAP).unwrap(),
            )),
            forecast_dropped_warned: parking_lot::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(FORECAST_DROPPED_WARNED_CAP).unwrap(),
            )),
            supply_reval: Default::default(),
            probe_generation: 1,
            probe_quota: ProbeQuotaLedger::default(),
            admin_fast_rx,
            admin_fast_tx,
            turn_cost_ewma_secs: 0.0,
            snapshot_tx: watch::channel(Arc::new(ClusterSnapshot::default())).0,
            #[cfg(test)]
            recovery_toctou_gate: plumbing.recovery_toctou_gate,
            #[cfg(test)]
            fail_next_recovery_load: plumbing.fail_next_recovery_load,
            #[cfg(test)]
            fail_next_job_view_load: plumbing.fail_next_job_view_load,
            #[cfg(test)]
            fail_next_floor_read: plumbing.fail_next_floor_read,
            #[cfg(test)]
            bump_claims_floor_before_fence_write: plumbing.bump_claims_floor_before_fence_write,
            #[cfg(test)]
            fail_next_attempt_append: plumbing.fail_next_attempt_append,
            #[cfg(test)]
            test_counters: TestCounters::default(),
            #[cfg(test)]
            tick_phase_stall: None,
        }
    }

    /// Receiver for the cached [`ClusterSnapshot`]. Called once by
    /// `ActorHandle::spawn` (and the test helper) before
    /// `run_with_self_tx` — same pattern as `backpressure_flag` /
    /// `generation_reader`. Additional subscribers are fine
    /// (`watch::Sender::subscribe` is cheap, single-slot).
    pub fn snapshot_receiver(&self) -> watch::Receiver<Arc<ClusterSnapshot>> {
        self.snapshot_tx.subscribe()
    }

    // r[impl sched.evidence.durability+4]
    /// The serving generation every claims-floor-fenced evidence write
    /// of this tenure carries — the tenure-tracking field, never a
    /// fresh `self.leader.generation()` read (see the field doc for why
    /// the distinction is load-bearing).
    pub(super) fn serving_generation(&self) -> crate::db::ServingGeneration {
        self.serving_generation
    }

    // r[impl sched.evidence.durability+4]
    /// Shared posture for a [`crate::db::FencedOutcome::Fenced`] outcome
    /// on a best-effort evidence write: warn (with the serving-tenure
    /// context an operator needs), count it, and continue. A fenced
    /// write is NOT an error — it is the claims-floor fence refusing a
    /// deposed replica's stale write; the in-memory state that produced
    /// the write is garbage the queued LeaderLost wipe discards.
    pub(super) fn note_fenced_evidence_write(&self, write: &'static str) {
        warn!(
            serving_generation = self.serving_generation.as_i64(),
            write,
            "evidence write fenced: serving generation below the durable claims floor \
             (deposed replica; a newer tenure owns the evidence now)"
        );
        metrics::counter!("rio_scheduler_evidence_write_fenced_total").increment(1);
        #[cfg(test)]
        self.test_counters
            .evidence_writes_fenced
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Reset DAG + per-build maps to empty. Called on leader-acquire,
    /// leader-lost, and recovery-failure — every path that discards
    /// in-memory persisted state. Re-applies `soft_features` to the
    /// fresh DAG so the I-204 strip survives leader transitions
    /// (regression: the original `self.dag = DerivationDag::new()` at
    /// each site dropped soft_features → first prod deploy of I-204
    /// was a no-op after the lease acquired).
    pub(super) fn clear_persisted_state(&mut self) {
        // Exhaustive destructure so adding a DagActor field is a
        // compile error here until it's classified as cleared (bind +
        // mutate below) or retained (`_`-bind). The §Lease-transition-
        // edges shape — "field not in clear_persisted_state's
        // enumeration" — recurred (mb_001b: hung_nodes; the same r25
        // batch then added cost_was_leader/cost_reload_notify without
        // listing them); a `_`-bind makes the retained-decision
        // explicit and grep-able, not implicit-by-absence.
        let Self {
            dag,
            builds,
            events,
            attempt_record_retries,
            // Retained: witnessed-terminal marks are monotone FACTS
            // (a pod observed terminal stays terminal), keyed by
            // exec_id — immutable attempt identity, never per-tenure
            // evidence (the last_store_rpc_failure shape: a leader
            // transition does not change that the pod died). A
            // deposed replica's sweep is leader-gated and generation-
            // fenced, so a stale map acts on nothing; on
            // re-acquisition the open-view prune drops any mark whose
            // attempt another leader resolved meanwhile, and a
            // still-open marked attempt establishes on its original
            // witnessed clock — strictly better than re-waiting for
            // the controller's next level-triggered re-report.
            witnessed_terminal: _,
            // Cleared: term-scoped corroboration evidence — a new
            // tenure re-corroborates within one window (documented
            // self-healing; merged_bug_032).
            store_degraded_sightings,
            // Retained: the scheduler's own observation of store
            // health, not per-term evidence; a transition does not
            // change whether the store was failing 30s ago.
            last_store_rpc_failure: _,
            dispatched_cells,
            authoritative_binding,
            acked_spawned,
            dag_authoritative,
            materialization_jobs,
            pending_carriers,
            // Retained: `handle_leader_lost` drains-with-NotLeader
            // BEFORE this wipe (Hazard L — never `clear()`); the
            // other callers (recovery start, TOCTOU discard, failed-
            // recovery Err) only run while standby or mid-acquire,
            // where `handle_report_outcome`'s inline `is_leader()`
            // gate has been replying NotLeader and never pushed.
            pending_pull_outcomes: _,
            // Retained: an Interval is stateless across leadership;
            // `handle_report_outcome` resets it on the next first
            // push regardless.
            pull_flush_deadline: _,
            // Retained: flush-scoped (always empty between flushes;
            // `flush_pending_pull_outcomes` and `handle_leader_lost`
            // both `debug_assert!` it).
            pending_walk_completed: _,
            status_outbox,
            // Retained: rationale below.
            retry_policy: _,
            establishment_report_slack: _,
            exec_retention_days: _,
            poison_config: _,
            // Retained: operator deploy config, not per-term state — a
            // leader transition doesn't change the materialization flag.
            materialization_cfg: _,
            db: _,
            store_client: _,
            grpc_timeout: _,
            cache_breaker: _,
            sla_estimator: _,
            sla_tiers: _,
            sla_ceilings: _,
            sla_config: _,
            cost_table: _,
            cost_was_leader: _,
            cost_reload_notify: _,
            ice: _,
            solve_cache: _,
            tick_count: _,
            backpressure_active: _,
            leader: _,
            // Retained: the tenure stamp of the state being wiped. A
            // wipe does not change which tenure last claimed; LeaderLost
            // deliberately keeps the deposed tenure's value (below any
            // successor's floor — that IS the fence working), and the
            // next LeaderAcquired re-stamps it at its own claim before
            // any of its evidence writes run.
            serving_generation: _,
            // Retained: same fence-working rationale as the tenure
            // stamp it shadows — a deposed replica's stale cached
            // floor sits at or below the deposed serving generation,
            // so the soft self-reject stays inert until LeaderLost
            // lands; the next LeaderAcquired re-seeds it.
            generation_floor_cached: _,
            recovery_claim_stamped: _,
            // Retained: static replica identity (the generation-claim
            // ledger's holder column), not per-term state.
            holder_id: _,
            self_tx: _,
            soft_features,
            hmac_signer: _,
            service_signer: _,
            shutdown: _,
            // Retained: a leader transition doesn't change the SLA or
            // override config; the operator already saw the WARN. The
            // bound is the LRU cap (mb_001), not a `.retain()` —
            // these key on tenant-controlled fields with no
            // operator-config-bounded universe to retain against.
            unroutable_features_warned: _,
            cap_mismatch_warned: _,
            // Retained: same rationale as the two siblings above. The
            // key universe (`drv_hash` × `reason`) is the live DAG —
            // it doesn't change on leader transition. The bound is the
            // LRU cap (r34 bug_018), not a `.retain()`.
            forecast_dropped_warned: _,
            // Cleared: verdict budgets are tenure-scoped (IceBackoff
            // precedent — the controller re-mints fresh verdicts every
            // tick, so a successor re-burns its own budget). The
            // disclosure LRU inside resets with it: re-arming warns on
            // transition is the siblings' documented fail-safe
            // over-emit.
            supply_reval,
            probe_generation: _,
            // Retained: the probe-admission ledger is generation-KEYED
            // (structural expiry) — a stale generation resets itself on
            // the next sweep, so a leader transition needs no wipe and
            // gets none (no curated reset site to miss).
            probe_quota: _,
            // Retained: channel plumbing, not per-term state — every
            // queued fast query is request/reply and its handler is
            // leader-agnostic exactly as the mailbox Admin path is.
            admin_fast_rx: _,
            admin_fast_tx: _,
            // Retained: the cost estimate describes THIS process's
            // turn costs, which a leader transition does not change;
            // it decays in ~10 turns either way.
            turn_cost_ewma_secs: _,
            snapshot_tx: _,
            #[cfg(test)]
                recovery_toctou_gate: _,
            #[cfg(test)]
                fail_next_recovery_load: _,
            #[cfg(test)]
                fail_next_floor_read: _,
            #[cfg(test)]
                bump_claims_floor_before_fence_write: _,
            #[cfg(test)]
                fail_next_attempt_append: _,
            #[cfg(test)]
                fail_next_job_view_load: _,
            #[cfg(test)]
                test_counters: _,
            #[cfg(test)]
                tick_phase_stall: _,
        } = self;
        *dag = DerivationDag::new();
        dag.set_soft_features(soft_features.clone());
        builds.clear();
        events.clear();
        // `attempt_record_retries` tracks re-deliveries for the previous
        // generation's pre-report derivations; after the wipe those are
        // recovered from PG and re-driven by the backstop, so the
        // counters are meaningless (and would slowly leak) here.
        attempt_record_retries.clear();
        store_degraded_sightings.clear();
        // `dispatched_cells` is keyed on the previous generation's drv
        // hashes; a stale entry would let a re-spawned pod's first
        // pull clear the wrong cell.
        dispatched_cells.clear();
        // Snapshot of THIS generation's pod bindings (controller-
        // reported). Stale entries would let the pull mint or the
        // establishment sweep attribute a re-dispatched drv to a
        // previous-generation node.
        authoritative_binding.clear();
        // Spawn-ack witnesses are tenure-scoped: a previous
        // generation's ack must not defer the successor's
        // fleet-exhaust verdicts.
        acked_spawned.clear();
        // Tenure-scoped verdict budgets + disclosure debounce — see
        // the destructure note above.
        *supply_reval = Default::default();
        // The materialization-job view is a droppable per-tenure cache
        // of PG rows; a new tenure rebuilds it from PG (Phase B). The
        // wipe lands on UNAVAILABLE, not Hydrated(empty): stale entries
        // would project job state for a DAG this wipe just discarded,
        // and an empty-but-trusted view over a repopulated DAG is the
        // merged_bug_246 hole (fabricated absence).
        materialization_jobs.wipe();
        // Leader-scoped carrier stash: a deposed leader's pending
        // creates are fenced anyway; the dropped-carrier accounting is
        // the PG-authority class documented on the field.
        pending_carriers.clear();
        // r[impl sched.attempt.cancel-close-driven+3]
        // The outbox is leader-scoped: a deposed leader must not
        // re-drive status writes (they would be fenced anyway); the
        // rows now belong to the successor's recovery + the
        // charge-free establishment arm.
        status_outbox.clear();
        metrics::gauge!("rio_scheduler_status_outbox_depth").set(0.0);
        // The DAG this fn just emptied no longer reflects PG; only the
        // next successful recovery (handle_leader_acquired's Ok arm)
        // re-asserts authoritativeness. Clearing HERE covers all four
        // callers — LeaderLost, recovery start, the TOCTOU flap
        // discard, and the failed-recovery Err arm — so destructive
        // staleness inferences fail closed in every empty-DAG window.
        *dag_authoritative = false;
        // Deliberately retained across generations:
        // - `ice` ladder (`cells`): cluster-level cell-backoff signal,
        //   TTL'd — 60s self-heals (true for the ladder ONLY).
        // - `ice` watermark (`last_applied`): NOT self-healing (a
        //   TTL-less ratchet) — reset via the LEADER_EDGES row
        //   `ice-epoch-watermark` (the paired-hook law; bug_067), not
        //   by this wipe.
        // - `cache_breaker`: store availability is generation-independent.
        // - `sla_estimator`: cluster-wide fitted curves.
        // - `solve_cache`: bounded by `sla_estimator`'s live set via the
        //   `on_evict` hook; per-key Schmitt `prev_a` is generation-
        //   independent.
        // - `cost_table`/`cost_was_leader`/`cost_reload_notify`:
        //   shared with `interrupt_housekeeping` (the edge-reload
        //   owner). Correctly NOT in this wipe: the latch's lifecycle
        //   is edge-owned — `handle_leader_lost` writes the false
        //   store and `handle_leader_acquired` the reload nudge, both
        //   through `observability::LEADER_EDGES` (the paired-hook
        //   table; bug_310). This fn also serves recovery-start and
        //   flap-discard, where a latch write would be wrong.
        // - `tick_count`: harmless counter.
    }

    /// Phase 1b bounded re-enqueue: a collapsed failure handler's
    /// appending transaction (attempt row + `decide()` + status persist)
    /// failed, so nothing was recorded and the derivation is still in
    /// its pre-report state. Re-deliver the completion event to the
    /// actor's own mailbox after [`ATTEMPT_RECORD_REDELIVERY_DELAY`],
    /// at most [`MAX_ATTEMPT_RECORD_REDELIVERIES`] times per derivation
    /// (the worker never re-sends a `CompletionReport`, so this re-push
    /// is the retry mechanism; past the cap the backstop sweep is the
    /// fallback).
    ///
    /// The re-delivered command is the ORIGINAL `ProcessCompletion`
    /// payload, moved verbatim out of the [`CompletionEcho`] — there is
    /// NO site on this path constructing a `BuildResult`, so no field
    /// can be dropped by construction (merged_bug_003: the old
    /// reconstruction zeroed `store_degraded`, node identity, resource
    /// telemetry, and outputs, silently re-classifying flagged reports
    /// on re-delivery). The full `handle_completion` guard chain
    /// re-runs on delivery, so a derivation that completed or got
    /// cancelled in the meantime drops the stale re-delivery exactly
    /// like any other stale report.
    pub(super) fn requeue_failure_completion(
        &mut self,
        executor_id: &ExecutorId,
        drv_hash: &DrvHash,
        echo: CompletionEcho,
    ) {
        let attempt = self
            .attempt_record_retries
            .entry(drv_hash.clone())
            .or_insert(0);
        *attempt += 1;
        let attempt = *attempt;
        if attempt > MAX_ATTEMPT_RECORD_REDELIVERIES {
            error!(
                drv_hash = %drv_hash, executor_id = %executor_id,
                status = echo.result.status, attempt,
                "appending transaction kept failing; giving up on re-delivery — the derivation \
                 stays in its pre-report state until the backstop sweep re-drives it"
            );
            self.attempt_record_retries.remove(drv_hash);
            return;
        }
        let Some(weak_tx) = self.self_tx.clone() else {
            // Bare `run()` (tests without a self sender): nothing to
            // re-push onto. The derivation stays pre-report; the test
            // either re-sends the completion itself or asserts exactly
            // this state.
            warn!(
                drv_hash = %drv_hash, executor_id = %executor_id,
                "appending transaction failed and no self sender is wired; \
                 completion event not re-delivered"
            );
            return;
        };
        warn!(
            drv_hash = %drv_hash, executor_id = %executor_id,
            status = echo.result.status, attempt,
            max = MAX_ATTEMPT_RECORD_REDELIVERIES,
            "appending transaction failed; re-delivering the completion event"
        );
        metrics::counter!("rio_scheduler_attempt_record_retries_total").increment(1);
        let cmd = ActorCommand::ProcessCompletion {
            executor_id: executor_id.clone(),
            drv_key: drv_hash.as_str().to_string(),
            result: echo.result,
            peak_memory_bytes: echo.peak_memory_bytes,
            peak_cpu_cores: echo.peak_cpu_cores,
            node_name: echo.node_name,
            hw_class: echo.hw_class,
            final_resources: echo.final_resources,
            final_line_count: echo.final_line_count,
        };
        let drv = drv_hash.clone();
        rio_common::task::spawn_monitored("attempt-record-redeliver", async move {
            tokio::time::sleep(ATTEMPT_RECORD_REDELIVERY_DELAY).await;
            if let Some(tx) = weak_tx.upgrade()
                && tx.try_send(cmd).is_err()
            {
                tracing::warn!(
                    drv_hash = %drv,
                    "re-delivered completion dropped (channel full/closed); the backstop sweep \
                     will re-drive the derivation"
                );
            }
        });
    }

    /// Run the actor with a weak clone of its own sender for scheduling
    /// delayed internal commands (terminal cleanup, etc.). The weak sender
    /// ensures the actor doesn't keep itself alive after all handles drop.
    pub async fn run_with_self_tx(
        mut self,
        mut rx: mpsc::Receiver<ActorCommand>,
        self_tx: mpsc::WeakSender<ActorCommand>,
    ) {
        self.self_tx = Some(self_tx);
        self.run_inner(&mut rx).await;
    }

    async fn run_inner(&mut self, rx: &mut mpsc::Receiver<ActorCommand>) {
        info!("DAG actor started");

        // B8 fairness: consecutive fast-lane serves between main
        // commands. The biased fast arm is disabled past the quota so
        // a fast flood cannot starve the mailbox; the counter resets
        // when a main command is served, or vacuously when the
        // mailbox is empty (no main work to be fair to).
        let mut consecutive_fast: usize = 0;
        loop {
            if consecutive_fast >= ADMIN_FAST_LANE_DRAIN_QUOTA && rx.is_empty() {
                consecutive_fast = 0;
            }
            let cmd = tokio::select! {
                // biased: shutdown first (a cancelled token wins even
                // with commands pending — on SIGTERM we want fast
                // drain, not queue-process-then-exit), then the admin
                // fast lane (B8: O(1)-class admin must not starve
                // behind queued bulk work), then the mailbox.
                biased;
                _ = self.shutdown.cancelled() => {
                    info!("actor shutting down");
                    break;
                }
                fast = self.admin_fast_rx.recv(),
                    if consecutive_fast < ADMIN_FAST_LANE_DRAIN_QUOTA =>
                {
                    if let Some(fa) = fast {
                        self.serve_fast_admin(fa);
                        consecutive_fast += 1;
                    }
                    // Not a mailbox command: no depth/backpressure/
                    // latency bookkeeping (the fast lane has its own
                    // delivery histogram inside serve_fast_admin).
                    continue;
                }
                cmd = rx.recv() => match cmd {
                    Some(c) => c,
                    None => break,
                },
                // sh-027 §3, flush trigger (iv): the
                // REPORT_OUTCOME_FLUSH_DEADLINE backstop. Placed
                // AFTER `rx.recv()` so the `biased;` order drains
                // every queued mailbox command (and so every
                // ReportPullOutcome in a burst) BEFORE this arm is
                // considered — placement BEFORE rx would flush at
                // N=1 after any idle (the burst is still queued when
                // the stale tick fires). The guard means an idle
                // actor never wakes on this arm (unpolled → no
                // waker). `handle_report_outcome` resets the
                // Interval on the empty→nonempty transition, so the
                // first poll after idle is always 250ms out.
                _ = self.pull_flush_deadline.tick(),
                    if !self.pending_pull_outcomes.is_empty() =>
                {
                    self.flush_pending_pull_outcomes().await;
                    continue;
                }
            };
            consecutive_fast = 0;

            // Check backpressure state
            let queue_len = rx.len();
            let capacity = rx.max_capacity();
            // Mailbox-depth gauge: emitted once per dequeued command. The
            // actor is single-threaded — depth growth = commands arriving
            // faster than the loop body retires them. Pairs with
            // `actor_cmd_seconds` (per-command latency) to localize a
            // wedge: high depth + one slow `cmd` label = head-of-line
            // block; high depth + uniformly fast cmds = sustained burst.
            metrics::gauge!("rio_scheduler_actor_mailbox_depth").set(queue_len as f64);
            self.update_backpressure(queue_len, capacity);

            // I-140: per-command latency. The actor is single-threaded
            // — one slow handler head-of-line blocks every queued
            // command (admin RPCs time out, pulls and reports queue
            // up, dispatch stalls). Export as a histogram + WARN over 1s so the next
            // "actor wedged" report self-localizes from `kubectl logs`
            // instead of needing a debugger attach.
            let cmd_name = cmd.name();
            let t_cmd = Instant::now();

            match cmd {
                ActorCommand::MergeDag { req, reply } => {
                    let build_id = req.build_id;
                    let result = self.handle_merge_dag(req).await;
                    // If the reply channel was dropped (client disconnected during
                    // merge), the build is orphaned. Cancel it immediately.
                    if reply.send(result).is_err() {
                        warn!(
                            build_id = %build_id,
                            "MergeDag reply receiver dropped, cancelling orphaned build"
                        );
                        if let Err(e) = self
                            .handle_cancel_build(build_id, None, "client_disconnect_during_merge")
                            .await
                        {
                            error!(build_id = %build_id, error = %e, "failed to cancel orphaned build");
                        }
                    }
                }
                ActorCommand::ProcessCompletion {
                    executor_id,
                    drv_key,
                    result,
                    peak_memory_bytes,
                    peak_cpu_cores,
                    node_name,
                    hw_class,
                    final_resources,
                    final_line_count,
                } => {
                    // r[impl sched.lease.standby-drops-writes+4]
                    // Defense-in-depth under the stream-reader's
                    // generation fence (executor_service.rs): an
                    // ex-leader MUST NOT write terminal PG state
                    // (`persist_status(Completed)` + realisations +
                    // SLA samples) — races the new leader's recovery.
                    if !self.leader.is_leader() {
                        warn!(
                            %executor_id, drv = %drv_key,
                            "dropping ProcessCompletion: not leader"
                        );
                    } else {
                        self.handle_completion(
                            &executor_id,
                            &drv_key,
                            result,
                            (peak_memory_bytes, peak_cpu_cores),
                            (node_name, hw_class),
                            (final_resources, final_line_count),
                        )
                        .await;
                    }
                }
                ActorCommand::CancelBuild {
                    build_id,
                    caller_tenant,
                    reason,
                    reply,
                } => {
                    // r[impl sched.lease.standby-drops-writes+4]
                    // Defense-in-depth like ProcessCompletion: an
                    // ex-leader's cancel writes terminal PG state from
                    // a stale DAG, and its terminal_log_epilogue pins
                    // the write-once build_derivations.exec_id (AND
                    // exec_id IS NULL) — permanently blocking the new
                    // leader's correct correlation. An unretried
                    // gateway disconnect-cancel is backstopped by the
                    // new leader's orphan-watcher sweep.
                    if !self.leader.is_leader() {
                        warn!(%build_id, "dropping CancelBuild: not leader");
                        let _ = reply.send(Err(ActorError::NotLeader));
                    } else {
                        let result = self
                            .handle_cancel_build(build_id, caller_tenant, &reason)
                            .await;
                        let _ = reply.send(result);
                    }
                }
                ActorCommand::PullAssignment {
                    intent_id,
                    auth_intent,
                    kind,
                    executor_instance,
                    resume_exec_id,
                    claim_nonce,
                    confirm_only,
                    executor_token_sha256,
                    reply,
                } => {
                    // r[sched.lease.standby-drops-writes+4]: the handler
                    // self-gates on is_leader() and the mint transaction
                    // carries the durable generation fence.
                    self.handle_pull_assignment(
                        intent_id,
                        auth_intent,
                        kind,
                        executor_instance,
                        resume_exec_id,
                        claim_nonce,
                        confirm_only,
                        executor_token_sha256,
                        reply,
                    )
                    .await;
                }
                ActorCommand::ListMaterializationJobs {
                    limit,
                    instance,
                    reply,
                } => {
                    // r[sched.lease.standby-drops-writes+4]: the handler
                    // self-gates on is_leader() (standby answers empty)
                    // and is read-only either way.
                    self.handle_list_materialization_jobs(limit, instance, reply)
                        .await;
                }
                ActorCommand::ReportPullOutcome {
                    exec_id,
                    auth_intent,
                    payload,
                    reply,
                } => {
                    // r[sched.lease.standby-drops-writes+4]: the handler
                    // self-gates on is_leader(); the classification
                    // path it funnels into carries the same appending
                    // discipline as the stream Completion arm.
                    // sh-027 §3: the retired mailbox-empty signal
                    // (sh-002 trigger iv) sampled `rx.is_empty()`
                    // here; it interleaved with ListMaterializationJobs
                    // / SubstituteProgress and degraded N̄ to ~5.5.
                    // The REPORT_OUTCOME_FLUSH_DEADLINE select! arm
                    // (above) is the replacement.
                    self.handle_report_outcome(exec_id, auth_intent, payload, reply)
                        .await;
                }
                ActorCommand::ReportAttemptOutcome {
                    identity,
                    reason,
                    node_name,
                    resubmit_cycle,
                    reply,
                } => {
                    // r[sched.lease.standby-drops-writes+4]: the handler
                    // self-gates on is_leader(); its only write is the
                    // first-writer-wins reason fill.
                    self.handle_report_attempt_outcome(
                        identity,
                        reason,
                        node_name,
                        resubmit_cycle,
                        reply,
                    )
                    .await;
                }
                ActorCommand::AckSpawnedIntents {
                    spawned,
                    unfulfillable_cells,
                    registered_cells,
                    observed_instance_types,
                    bound_intents,
                    binding_snapshot,
                    rejected,
                    reply,
                } => {
                    // r[impl sched.lease.standby-drops-writes+4] —
                    // ICE state is lease-holder only. merged_bug_005:
                    // the reply makes the defense-in-depth drop
                    // VISIBLE — a deposed drain answers `NotLeader`
                    // instead of letting the gRPC layer ack an
                    // unapplied payload, so the controller's
                    // commit-on-Ack buffer survives to redeliver at
                    // the next leader.
                    let (poisons, verdict) = if self.leader.is_leader() {
                        self.handle_ack_spawned_intents(
                            &spawned,
                            &unfulfillable_cells,
                            &registered_cells,
                            &observed_instance_types,
                            &bound_intents,
                            binding_snapshot.as_deref(),
                            &rejected,
                        )
                    } else {
                        (Vec::new(), Err(command::AckApplyError::NotLeader))
                    };
                    // live_051(c): budget crossings poison AFTER the
                    // atomic apply (commit applies exactly the decoded
                    // planes — bug_142: poisons from an APPLIED
                    // verdict plane fire even when a sibling plane
                    // refused; the verdict below discloses which
                    // planes did not land) and BEFORE the reply, in
                    // this arm's async context.
                    self.apply_no_host_poisons(poisons).await;
                    // Receiver gone = RPC already failed client-side;
                    // nothing to report (the controller retains its
                    // buffer either way).
                    let _ = reply.send(verdict);
                }
                ActorCommand::Tick => {
                    self.handle_tick().await;
                }
                ActorCommand::QueryBuildStatus {
                    build_id,
                    caller_tenant,
                    reply,
                } => {
                    let result = self.handle_query_build_status(build_id, caller_tenant);
                    let _ = reply.send(result);
                }
                ActorCommand::WatchBuild {
                    build_id,
                    caller_tenant,
                    reply,
                } => {
                    let result = self.handle_watch_build(build_id, caller_tenant);
                    let _ = reply.send(result);
                }
                ActorCommand::CleanupTerminalBuild { build_id } => {
                    self.handle_cleanup_terminal_build(build_id).await;
                }
                ActorCommand::Admin(q) => {
                    self.handle_admin(q);
                }
                ActorCommand::ClearPoison { drv_hash, reply } => {
                    let cleared = self.handle_clear_poison(&drv_hash).await;
                    let _ = reply.send(cleared);
                }
                ActorCommand::LeaderLost => {
                    self.handle_leader_lost();
                }
                ActorCommand::LeaderAcquired => {
                    self.handle_leader_acquired().await;
                    // Immediate ready-set store sweep after recovery so
                    // recovered Ready derivations whose outputs already
                    // exist complete/substitute instead of waiting for
                    // the next merge or completion to trigger it.
                    self.sweep_ready_cached().await;
                }
                ActorCommand::LeaderRebound => {
                    self.handle_leader_rebound().await;
                    // Same post-recovery sweep as the acquire arm: the
                    // rebound's re-recovery may have loaded Ready
                    // derivations whose outputs already exist.
                    self.sweep_ready_cached().await;
                }
                ActorCommand::SubstituteProgress {
                    drv_hash,
                    bytes_done,
                    bytes_expected,
                    upstream_uri,
                } => {
                    self.handle_substitute_progress(
                        &drv_hash,
                        bytes_done,
                        bytes_expected,
                        upstream_uri,
                    );
                }
                #[cfg(test)]
                ActorCommand::Debug(d) => {
                    self.handle_debug(d).await;
                }
            }

            let cmd_elapsed = t_cmd.elapsed();
            // B6: feed the per-turn cost EWMA — the NEXT iteration's
            // update_backpressure prices the queue with it (the
            // engagement window is the first dequeue after a stall,
            // which is exactly when the built-up queue needs shedding).
            self.note_turn_cost(cmd_elapsed);
            metrics::histogram!("rio_scheduler_actor_cmd_seconds", "cmd" => cmd_name)
                .record(cmd_elapsed.as_secs_f64());
            if cmd_elapsed >= std::time::Duration::from_secs(1) {
                warn!(
                    cmd = cmd_name,
                    elapsed = ?cmd_elapsed,
                    mailbox_depth = queue_len,
                    "actor command exceeded 1s; head-of-line blocking the mailbox"
                );
            }
        }

        info!("DAG actor shutting down");
    }

    // -----------------------------------------------------------------------
    // Backpressure
    // -----------------------------------------------------------------------

    /// Record one actor work unit's cost into the per-turn EWMA
    /// (round-9 B6) — the producer side of the cost-axis backpressure
    /// law. Called by `run_inner` after every mailbox command and by
    /// `serve_fast_admin` after every fast-lane handler: any work that
    /// occupies the single-threaded actor inflates the drain time of
    /// everything queued behind it, whichever lane it arrived on.
    pub(crate) fn note_turn_cost(&mut self, elapsed: std::time::Duration) {
        let s = elapsed.as_secs_f64();
        self.turn_cost_ewma_secs = BACKPRESSURE_COST_EWMA_ALPHA * s
            + (1.0 - BACKPRESSURE_COST_EWMA_ALPHA) * self.turn_cost_ewma_secs;
    }

    // pub(crate) for the hysteresis + cost-axis unit tests
    // (tests/misc.rs). Called once per command iteration at the top of
    // run_inner; tests exercise the watermark transitions directly on
    // a bare actor.
    //
    // TWO axes (round-9 B6 added the second):
    // - depth: queue fraction vs the 0.80/0.60 watermarks (unchanged).
    // - cost: projected drain time = queue_len × per-turn cost EWMA
    //   vs the 30s/10s drain bounds. The live_053 inversion this
    //   closes: one 140s command at 1–12.8% depth = total
    //   time-starvation with silent watermarks — depth is a proxy for
    //   drain time only when turns are uniform, and the incident
    //   turns were 5 orders of magnitude apart.
    // Engage on EITHER axis high; release only when BOTH are low
    // (joint hysteresis — a one-axis release would re-admit work the
    // other axis is still drowning under).
    // r[impl sched.admission.work-per-turn]
    pub(crate) fn update_backpressure(&mut self, queue_len: usize, capacity: usize) {
        let fraction = queue_len as f64 / capacity as f64;
        let projected_drain_secs = queue_len as f64 * self.turn_cost_ewma_secs;
        // The cost signal's falsifiable production surface (the
        // W9-AH plane): visible BEFORE engagement, so dashboards can
        // watch the drain projection approach the budget.
        metrics::gauge!("rio_scheduler_backpressure_projected_drain_seconds")
            .set(projected_drain_secs);
        let was_active = self.backpressure_active.load(Ordering::Relaxed);

        if !was_active
            && (fraction >= BACKPRESSURE_HIGH_WATERMARK
                || projected_drain_secs >= BACKPRESSURE_DRAIN_HIGH_SECS)
        {
            self.backpressure_active.store(true, Ordering::Relaxed);
            warn!(
                queue_len,
                capacity,
                projected_drain_secs,
                turn_cost_ewma_secs = self.turn_cost_ewma_secs,
                "backpressure activated ({}): {:.0}% capacity, projected drain {:.1}s",
                if fraction >= BACKPRESSURE_HIGH_WATERMARK {
                    "depth"
                } else {
                    "work-cost"
                },
                fraction * 100.0,
                projected_drain_secs,
            );
            metrics::counter!("rio_scheduler_queue_backpressure").increment(1);
        } else if was_active
            && fraction <= BACKPRESSURE_LOW_WATERMARK
            && projected_drain_secs <= BACKPRESSURE_DRAIN_LOW_SECS
        {
            self.backpressure_active.store(false, Ordering::Relaxed);
            info!(
                queue_len,
                capacity,
                projected_drain_secs,
                "backpressure deactivated, resuming normal operation"
            );
        }
    }

    /// Clone the shared backpressure flag as a read-only reader for wiring
    /// into ActorHandle. The actor keeps the writable `Arc<AtomicBool>`.
    pub(crate) fn backpressure_flag(&self) -> BackpressureReader {
        BackpressureReader::new(Arc::clone(&self.backpressure_active))
    }

    /// Clone the admin fast-lane sender for wiring into ActorHandle
    /// (B8). The actor keeps the receiver as a field.
    pub(crate) fn admin_fast_sender(&self) -> mpsc::Sender<FastAdmin> {
        self.admin_fast_tx.clone()
    }

    /// Serve one fast-lane admin query: record true DELIVERY latency
    /// (enqueue → here — the axis the mailbox FIFO starved and the
    /// falsifiable surface of the B8 SLO), then dispatch through the
    /// same `handle_admin` the mailbox path uses (identical per-query
    /// semantics; only the queueing differs). Handler time feeds the
    /// existing per-command histogram under the same `Admin` label the
    /// mailbox path records, so total-work dashboards see one series.
    fn serve_fast_admin(&mut self, fa: FastAdmin) {
        let waited = fa.enqueued_at.elapsed();
        let name = fa.query.name();
        metrics::histogram!(
            "rio_scheduler_actor_admin_fast_delivery_seconds",
            "cmd" => name
        )
        .record(waited.as_secs_f64());
        if waited >= ADMIN_FAST_DELIVERY_SLO {
            warn!(
                cmd = name,
                waited = ?waited,
                "fast-lane admin delivery exceeded the SLO; the largest \
                 indivisible actor work slice is over budget"
            );
        }
        let t = Instant::now();
        self.handle_admin(fa.query);
        let elapsed = t.elapsed();
        // B6: fast-lane serves occupy the actor too — they price into
        // the same per-turn cost EWMA the mailbox path feeds.
        self.note_turn_cost(elapsed);
        metrics::histogram!("rio_scheduler_actor_cmd_seconds", "cmd" => "Admin")
            .record(elapsed.as_secs_f64());
        if elapsed >= std::time::Duration::from_secs(1) {
            warn!(
                cmd = name,
                elapsed = ?elapsed,
                "fast-lane admin handler exceeded 1s; it is inflating the \
                 work slice that bounds every other fast delivery"
            );
        }
    }

    /// Drain up to [`ADMIN_FAST_LANE_DRAIN_QUOTA`] queued fast-lane
    /// queries (work-per-turn — the A-1 discipline applied to the lane
    /// itself). Called at every Tick phase boundary; the run loop's
    /// biased select covers between-command service.
    // r[impl sched.admission.work-per-turn]
    pub(super) fn drain_admin_fast_lane(&mut self) {
        for _ in 0..ADMIN_FAST_LANE_DRAIN_QUOTA {
            match self.admin_fast_rx.try_recv() {
                Ok(fa) => self.serve_fast_admin(fa),
                Err(_) => break,
            }
        }
    }

    /// Clone the generation counter (and the recovery-complete flag its
    /// advertised view is gated on) as a read-only reader for
    /// `ActorHandle::leader_generation()` /
    /// `ActorHandle::advertised_generation()`. The lease task holds a
    /// direct `Arc<AtomicU64>` clone for writing — not through this
    /// reader. The reader type has no store/fetch_add methods, so
    /// handle consumers can't accidentally increment.
    pub(crate) fn generation_reader(&self) -> GenerationReader {
        GenerationReader::new(self.leader.generation_arc(), self.leader.clone())
    }
}
