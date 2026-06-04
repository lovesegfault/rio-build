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
pub(crate) mod materialize;
mod merge;
pub(crate) mod pull;
mod recovery;
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
/// Key cardinality is `|Queued-with-incomplete-deps drvs| × |reason|`
/// (reason ∈ {lead_horizon, tenant_budget}, |reason| = 2). Bounded by
/// the DAG width; 4096 covers the largest observed Queued frontier
/// (e.g. nixpkgs-cross release closures). Eviction re-arms the warn —
/// fail-safe over-emit, bounded by cap × eviction churn.
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
/// with the store-side 4096-path truncation removed
/// (`r[store.substitute.probe-bounded+4]`), `check_available` runs the
/// FULL uncached set at 128-wide. Envelope: `⌈N_uncached/128⌉ × RTT` —
/// 153k paths at 30ms ≈ 36s, which the default 30s would clip. 90s
/// covers that with headroom for one 429-retry sleep; the merge phase
/// already sits inside the actor for a 153k-node submission, so the
/// extra 60s is acceptable for that (rare, inherently slow) shape.
/// Dispatch-time FMP stays on `grpc_timeout` (its batch is bounded by
/// `DISPATCH_PROBE_BATCH_CAP`).
pub const MERGE_FMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Delay before cleaning up terminal build state. Keeps the build
/// resident (and its broadcast channels alive) so late WatchBuild
/// subscribers can still attach and learn the outcome from their
/// snapshot (`r[sched.watch.snapshot-first]`).
const TERMINAL_CLEANUP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Max Ready candidates per `FindMissingPaths` batch in the ready-set
/// store short-circuit (`sweep_ready_cached`). Keeps the FMP RPC in
/// the actor's ~100ms budget for very wide DAG layers — the sweep runs
/// under `grpc_timeout` (30s), not [`MERGE_FMP_TIMEOUT`]. The
/// truncated tail is picked up on the next sweep (same
/// `probe_generation`, so the window advances rather than re-probing
/// the head).
pub(crate) const DISPATCH_PROBE_BATCH_CAP: usize = 2048;

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

/// The DAG actor state.
/// One failed terminal-status persist, owned for the outbox.
/// See `DagActor::status_outbox`.
#[derive(Debug)]
pub(super) struct StatusBatch {
    pub(super) drv_hashes: Vec<String>,
    pub(super) status: crate::state::DerivationStatus,
    pub(super) enqueued_at: std::time::Instant,
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
    // r[impl sched.attempt.cancel-close-driven]
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
    /// Internally `Arc<RwLock<…>>`; reads on the snapshot/dispatch path
    /// are a single `.cached()` clone.
    pub(crate) sla_estimator: crate::sla::SlaEstimator,
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
    /// `handle_leader_acquired`'s Ok arm (this tenure's
    /// `recover_from_pg` succeeded), cleared by every
    /// [`clear_persisted_state`](Self::clear_persisted_state) caller
    /// (LeaderLost, recovery start, the TOCTOU flap discard, and the
    /// failed-recovery Err arm).
    ///
    /// NOT the same thing as [`LeaderState::recovery_complete`]: that
    /// flag is deliberately set true even when recovery FAILS (empty
    /// DAG — "degrade, don't block", which the pull/admission paths
    /// want).
    /// Destructive consumers that infer "stale" from "not in the DAG"
    /// must check THIS bit instead.
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
    /// Advances once per `handle_tick`. The ready-set store
    /// short-circuit (`sweep_ready_cached`) stamps each checked node's
    /// `probed_generation` with this value and skips already-stamped
    /// nodes within the same generation, so the
    /// [`DISPATCH_PROBE_BATCH_CAP`] truncate window advances across
    /// inline sweep calls instead of re-FMP'ing the same head. Starts
    /// at 1 so freshly-inserted nodes (`probed_generation: 0`) are
    /// immediately eligible.
    probe_generation: u64,
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
}

#[cfg(test)]
impl TestCounters {
    pub(crate) fn snapshot(&self) -> TestCountersSnapshot {
        use std::sync::atomic::Ordering::SeqCst;
        TestCountersSnapshot {
            persist_status_calls: self.persist_status_calls.load(SeqCst),
            solve_inputs_calls: self.solve_inputs_calls.load(SeqCst),
            evidence_writes_fenced: self.evidence_writes_fenced.load(SeqCst),
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
        }
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
            status_outbox: std::collections::VecDeque::new(),
            db,
            store_client: plumbing.store_client,
            grpc_timeout: cfg.grpc_timeout,
            cache_breaker: CacheCheckBreaker::default(),
            sla_estimator: crate::sla::SlaEstimator::new(&cfg.sla),
            sla_tiers: cfg.sla.solve_tiers(),
            sla_ceilings,
            sla_config: cfg.sla,
            cost_table: plumbing.cost_table,
            cost_was_leader: plumbing.cost_was_leader,
            cost_reload_notify: plumbing.cost_reload_notify,
            ice: Arc::new(crate::sla::cost::IceBackoff::new(max_lead_time)),
            dispatched_cells: dashmap::DashMap::new(),
            solve_cache: Arc::default(),
            tick_count: 0,
            backpressure_active: Arc::new(AtomicBool::new(false)),
            // The initial tenure stamp (see the field doc): the lease
            // read at construction time. K8s-mode actors re-stamp it at
            // every LeaderAcquired's generation claim; always-leader
            // actors keep this value for the process lifetime.
            serving_generation: crate::db::ServingGeneration::stamp_from_claim(
                plumbing.leader.generation(),
            ),
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
            probe_generation: 1,
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
            test_counters: TestCounters::default(),
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
            dispatched_cells,
            authoritative_binding,
            acked_spawned,
            dag_authoritative,
            materialization_jobs,
            pending_carriers,
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
            probe_generation: _,
            snapshot_tx: _,
            #[cfg(test)]
                recovery_toctou_gate: _,
            #[cfg(test)]
                fail_next_recovery_load: _,
            #[cfg(test)]
                fail_next_floor_read: _,
            #[cfg(test)]
                fail_next_job_view_load: _,
            #[cfg(test)]
                test_counters: _,
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
        // r[impl sched.attempt.cancel-close-driven]
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
        // - `ice`: cluster-level cell-backoff signal, 60s TTL self-heals.
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
    /// The re-delivered command carries only the fields the failure
    /// paths consume (status, error message, line count) — the
    /// telemetry fields are success-path-only and are zeroed. The full
    /// `handle_completion` guard chain re-runs on delivery, so a
    /// derivation that completed or got cancelled in the meantime drops
    /// the stale re-delivery exactly like any other stale report.
    pub(super) fn requeue_failure_completion(
        &mut self,
        executor_id: &ExecutorId,
        drv_hash: &DrvHash,
        status: rio_proto::types::BuildResultStatus,
        error_msg: &str,
        final_line_count: u64,
    ) {
        let attempt = self
            .attempt_record_retries
            .entry(drv_hash.clone())
            .or_insert(0);
        *attempt += 1;
        let attempt = *attempt;
        if attempt > MAX_ATTEMPT_RECORD_REDELIVERIES {
            error!(
                drv_hash = %drv_hash, executor_id = %executor_id, ?status, attempt,
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
            drv_hash = %drv_hash, executor_id = %executor_id, ?status, attempt,
            max = MAX_ATTEMPT_RECORD_REDELIVERIES,
            "appending transaction failed; re-delivering the completion event"
        );
        metrics::counter!("rio_scheduler_attempt_record_retries_total").increment(1);
        let cmd = ActorCommand::ProcessCompletion {
            executor_id: executor_id.clone(),
            drv_key: drv_hash.as_str().to_string(),
            result: rio_proto::types::BuildResult {
                status: status.into(),
                error_msg: error_msg.to_string(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_resources: None,
            final_line_count,
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

        loop {
            let cmd = tokio::select! {
                // biased: check the shutdown arm first so a cancelled
                // token wins even if commands are pending. On SIGTERM
                // we want fast drain, not a queue-process-then-exit.
                biased;
                _ = self.shutdown.cancelled() => {
                    info!("actor shutting down");
                    break;
                }
                cmd = rx.recv() => match cmd {
                    Some(c) => c,
                    None => break,
                },
            };

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
                    // r[impl sched.lease.standby-drops-writes+3]
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
                    // r[impl sched.lease.standby-drops-writes+3]
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
                    reply,
                } => {
                    // r[sched.lease.standby-drops-writes+3]: the handler
                    // self-gates on is_leader() and the mint transaction
                    // carries the durable generation fence.
                    self.handle_pull_assignment(
                        intent_id,
                        auth_intent,
                        kind,
                        executor_instance,
                        resume_exec_id,
                        reply,
                    )
                    .await;
                }
                ActorCommand::ListMaterializationJobs { limit, reply } => {
                    // r[sched.lease.standby-drops-writes+3]: the handler
                    // self-gates on is_leader() (standby answers empty)
                    // and is read-only either way.
                    self.handle_list_materialization_jobs(limit, reply).await;
                }
                ActorCommand::ReportPullOutcome {
                    exec_id,
                    auth_intent,
                    payload,
                    reply,
                } => {
                    // r[sched.lease.standby-drops-writes+3]: the handler
                    // self-gates on is_leader(); the classification
                    // path it funnels into carries the same appending
                    // discipline as the stream Completion arm.
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
                    // r[sched.lease.standby-drops-writes+3]: the handler
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
                } => {
                    // r[impl sched.lease.standby-drops-writes+3] —
                    // ICE state is lease-holder only.
                    if self.leader.is_leader() {
                        self.handle_ack_spawned_intents(
                            &spawned,
                            &unfulfillable_cells,
                            &registered_cells,
                            &observed_instance_types,
                            &bound_intents,
                            binding_snapshot.as_deref(),
                        );
                    }
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

    // pub(crate) for hysteresis unit test (tests/misc.rs). Called once
    // per command iteration at the top of run_inner (line ~295); tests
    // exercise the watermark transitions directly on a bare actor.
    pub(crate) fn update_backpressure(&mut self, queue_len: usize, capacity: usize) {
        let fraction = queue_len as f64 / capacity as f64;
        let was_active = self.backpressure_active.load(Ordering::Relaxed);

        if !was_active && fraction >= BACKPRESSURE_HIGH_WATERMARK {
            self.backpressure_active.store(true, Ordering::Relaxed);
            warn!(
                queue_len,
                capacity,
                "backpressure activated at {:.0}% capacity",
                fraction * 100.0
            );
            metrics::counter!("rio_scheduler_queue_backpressure").increment(1);
        } else if was_active && fraction <= BACKPRESSURE_LOW_WATERMARK {
            self.backpressure_active.store(false, Ordering::Relaxed);
            info!(
                queue_len,
                capacity, "backpressure deactivated, resuming normal operation"
            );
        }
    }

    /// Clone the shared backpressure flag as a read-only reader for wiring
    /// into ActorHandle. The actor keeps the writable `Arc<AtomicBool>`.
    pub(crate) fn backpressure_flag(&self) -> BackpressureReader {
        BackpressureReader::new(Arc::clone(&self.backpressure_active))
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
