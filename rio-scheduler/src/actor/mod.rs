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
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use rio_proto::StoreServiceClient;
use rio_proto::types::FindMissingPathsRequest;

use crate::dag::DerivationDag;
use crate::db::SchedulerDb;
use crate::lease::LeaderState;
use crate::queue::ReadyQueue;
#[allow(unused_imports)]
use crate::state::{
    BuildInfo, BuildState, BuildStateExt, DerivationStatus, DrvHash, ExecutorId, ExecutorState,
    HEARTBEAT_TIMEOUT_SECS, POISON_TTL, PoisonConfig, RetryPolicy,
};

// `impl DagActor` is sharded across these submodules by concern.
// Cohesive field clusters live in sub-structs (`events: BuildEventBus`,
// `leader: LeaderState`); the genuinely
// cross-cutting fields (`dag`, `executors`, `builds`, `db`,
// `ready_queue`) remain flat — every handler reads/writes them. Keep
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
mod merge;
mod recovery;
mod snapshot;

pub(super) use breaker::CacheCheckBreaker;
pub use command::*;
pub use config::{DagActorConfig, DagActorPlumbing};
use event::BuildEventBus;
#[cfg(test)]
pub(crate) use executor::compute_initial_prefetch_paths;
pub use handle::ActorHandle;
#[cfg(test)]
pub(crate) use handle::DebugDerivationInfo;
pub(crate) use handle::DebugExecutorInfo;

#[cfg(test)]
mod debug;
#[cfg(test)]
use debug::backdate;
#[cfg(test)]
pub(crate) mod tests;

/// Channel capacity for the actor command channel.
pub(crate) const ACTOR_CHANNEL_CAPACITY: usize = 10_000;

/// Max store paths per `PrefetchHint`. Shared between the initial-warm
/// hint in `on_executor_registered` and the per-dispatch
/// hint in `dispatch.rs` — bump BOTH semantics by changing this once.
pub(crate) const MAX_PREFETCH_PATHS: usize = 100;

/// Backpressure: reject new work above this fraction of channel capacity.
const BACKPRESSURE_HIGH_WATERMARK: f64 = 0.80;

/// Backpressure: resume accepting work below this fraction.
const BACKPRESSURE_LOW_WATERMARK: f64 = 0.60;

/// Number of events to retain in each build's event buffer for late subscribers.
///
/// 4096 (was 1024 — I-144): `handle_merge_dag` calls `dispatch_ready()`
/// BEFORE returning `event_rx`, so the initial dispatch burst (one
/// Derivation::Started per ready node) lands in the ring before the
/// SubmitBuild bridge starts draining. A 153k-node submission with ~500
/// ready nodes plus Progress/Log emitted ~1.3k events synchronously →
/// the bridge's first `recv()` was `Lagged`. 4096 gives headroom for the
/// initial burst; the bridge now also continues across `Lagged` instead
/// of dropping the receiver (see `bridge_build_events`).
pub(super) const BUILD_EVENT_BUFFER_SIZE: usize = 4096;

/// Default cap on concurrent detached substitute-fetch tasks: an
/// in-flight detached-task MEMORY bound, NOT a throughput throttle.
/// Per-replica admission is `r[store.substitute.admission]` (the store
/// owns the gate); saturation surfaces here as `ResourceExhausted` and
/// is handled by [`SUBSTITUTE_FETCH_BACKOFF`]. Each task acquires a
/// `DagActor.substitute_sem` permit before its `QueryPathInfo`.
/// Overridable via `RIO_SUBSTITUTE_MAX_CONCURRENT` (operator escape
/// hatch — not chart-set).
pub const DEFAULT_SUBSTITUTE_CONCURRENCY: usize = 256;

/// Retry policy for the detached substitute fetch's `QueryPathInfo`.
/// Transient store errors (`Unavailable`/`Aborted`/`ResourceExhausted`
/// per [`rio_common::grpc::is_transient`]) retry up to
/// [`SUBSTITUTE_FETCH_MAX_ATTEMPTS`] with this curve.
pub const SUBSTITUTE_FETCH_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_millis(250),
    mult: 2.0,
    cap: std::time::Duration::from_secs(30),
    jitter: rio_common::backoff::Jitter::Proportional(0.2),
};

/// Max attempts per path for the detached substitute fetch. With
/// [`SUBSTITUTE_FETCH_BACKOFF`]: 250ms→500ms→1s→2s→4s→8s→16s ≈ 31.75 s
/// total retry budget per path (7 backoffs between 8 attempts; the loop
/// breaks before the final sleep — the 30 s `cap` limits the 7th
/// backoff to 16 s, not 32 s) before demoting to cache-miss. Raised
/// 5→8 alongside `r[store.substitute.admission]`: the store now queues
/// up to `SUBSTITUTE_ADMISSION_WAIT` (30 s) before returning
/// `RESOURCE_EXHAUSTED`, so each attempt is itself a 30 s server-side
/// wait under saturation; 8 attempts give a ~90 s window
/// (≥1 attempt's bounded-wait + backoffs) for the burst to clear
/// before demoting. Belt-and-suspenders — under normal load the
/// store's bounded-wait absorbs the burst on attempt 1.
pub const SUBSTITUTE_FETCH_MAX_ATTEMPTS: u32 = 8;

/// Per-path timeout for the detached substitute fetch's
/// `QueryPathInfo`. Separate from `grpc_timeout` (30s) because the
/// store-side `try_substitute` recursively walks the runtime closure
/// — a single ghc-9.8.4 (1.9 GB) fetch legitimately takes minutes.
/// The fetch runs OUTSIDE the actor loop, so a long timeout here
/// doesn't head-of-line block. r[sched.substitute.detached]
pub const SUBSTITUTE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Delay before cleaning up terminal build state. Allows late WatchBuild
/// subscribers to receive the terminal event before the broadcast sender
/// is dropped.
const TERMINAL_CLEANUP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Max inline `dispatch_ready` calls from the "worker newly available"
/// carve-out per Tick — Heartbeat `became_idle` (capacity 0→1) and
/// `PrefetchComplete` (cold→warm) share this budget. Past it, both
/// only set `dispatch_dirty` (deferred ≤1 Tick). Prevents
/// leader-failover from reintroducing the I-163 storm: with 290
/// executors all reconnecting, 290 first-heartbeats are 0→1
/// transitions and 290 PrefetchComplete ACKs follow → 580 sequential
/// `dispatch_ready` passes uncapped (each does the ~150ms batch-FOD
/// precheck). 4 inline + remainder coalesced bounds the burst at
/// ~600ms regardless of fleet size while keeping the steady-state
/// "fresh ephemeral dispatches immediately" win.
pub(crate) const BECAME_IDLE_INLINE_CAP: u32 = 4;

/// Max Ready candidates per dispatch-time `FindMissingPaths` batch.
/// Belt-and-suspenders under the store-side `SUBSTITUTE_PROBE_MAX_PATHS`
/// (4096): keeps the FMP RPC in the actor's ~100ms budget for very wide
/// DAG layers. The truncated tail is picked up on the next inline
/// `dispatch_ready` (same `probe_generation`, so the window advances
/// rather than re-probing the head).
pub(crate) const DISPATCH_PROBE_BATCH_CAP: usize = 2048;

/// The DAG actor state.
pub struct DagActor {
    /// The global derivation DAG.
    dag: DerivationDag,
    /// FIFO queue of ready derivation hashes.
    ready_queue: ReadyQueue,
    /// Active builds indexed by build_id.
    builds: HashMap<Uuid, BuildInfo>,
    /// Per-build event broadcast channels + sequence/debounce state +
    /// persister/flusher wires. See [`BuildEventBus`].
    events: BuildEventBus,
    /// Shared log ring buffers. The actor only seals (via
    /// [`Self::seal_log_buffer`]) on terminal completion so a late
    /// `LogBatch` can't recreate an entry the flusher already drained.
    /// `None` in tests that don't exercise the log pipeline.
    log_buffers: Option<Arc<crate::logs::LogBuffers>>,
    /// Connected workers.
    executors: HashMap<ExecutorId, ExecutorState>,
    /// Executors that disconnected mid-build, awaiting the controller's
    /// `ReportExecutorTermination` (k8s OOMKilled/Evicted reason).
    /// `(drv_hash, inserted_at)` — captured before
    /// `self.executors.remove()`. The controller's report arrives ~1-3s
    /// after disconnect; entries are swept on Tick after
    /// [`executor::TERMINATION_REPORT_TTL`]. In-memory only: a lost
    /// entry (scheduler restart) degrades to "one OOM doesn't promote"
    /// — same as pre-I-197 behavior for one cycle.
    pub(crate) recently_disconnected: HashMap<ExecutorId, (DrvHash, Instant)>,
    /// Retry policy.
    retry_policy: RetryPolicy,
    /// Poison threshold + distinct-workers config. Replaces the
    /// former `POISON_THRESHOLD` const (3). Default matches prior
    /// behavior: 3 distinct workers.
    poison_config: PoisonConfig,
    /// Database handle.
    db: SchedulerDb,
    /// Store service client for scheduler-side cache checks. `None` in tests
    /// that don't need the store (cache check is then skipped).
    store_client: Option<StoreServiceClient<Channel>>,
    /// Timeout for metadata gRPC calls to the store (FindMissingPaths,
    /// QueryPathInfo). Defaults to [`rio_common::grpc::DEFAULT_GRPC_TIMEOUT`]
    /// (30s). Tests that arm a hung MockStore to prove the timeout wrapper
    /// exists override to 3s via
    /// [`with_grpc_timeout`](Self::with_grpc_timeout) — same
    /// wrapper-exists proof at 10× less wall-clock. Plumbed as a field
    /// (not `cfg(test)` on the const) because `cfg(test)` is per-crate:
    /// rio-scheduler's test build links against rio-common built WITHOUT
    /// `cfg(test)`, so a test-gated constant there is invisible here.
    grpc_timeout: std::time::Duration,
    /// Bounds in-flight detached substitute-fetch tasks. The
    /// pre-59a6803a synchronous path used `buffer_unordered(max)`; the
    /// detached spawn loop dropped that, so a 17k-path merge spawned
    /// 23k unbounded QueryPathInfo's → store PG-pool/S3 saturated →
    /// 90% failed → demoted to Ready → built from source.
    substitute_sem: Arc<tokio::sync::Semaphore>,
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
    /// Hard ceilings from `cfg.sla.ceilings()`.
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
    /// In-process insufficient-capacity backoff. `solve_full` skips
    /// marked cells; the Pending-watch marks them. Arc so the snapshot
    /// path (`&self`) and housekeeping (`&mut self`) share one map.
    pub(crate) ice: Arc<crate::sla::cost::IceBackoff>,
    /// Pending-watch ledger: `drv_hash → (band, cap, armed_at)`.
    /// Inserted by `handle_ack_spawned_intents` when the CONTROLLER
    /// confirms it created a Job for a band-targeted intent (NOT at
    /// emit time — `compute_spawn_intents` is read-only so dashboard
    /// polls and headroom-gated intents don't false-arm).
    /// `compute_spawn_intents` LOOKS UP entries to pin the returned
    /// selector across re-emits (softmax re-roll would otherwise drift
    /// it and trip the controller's selector-drift reaper).
    /// `handle_heartbeat` removes when the pod checks in. Housekeeping
    /// sweeps entries past `hw_fallback_after_secs` →
    /// [`ice`](Self::ice) `.mark(band, cap)` and removes (next snapshot
    /// re-solves excluding the cell). DashMap because the snapshot
    /// lookup is `&self`.
    pub(crate) pending_intents:
        dashmap::DashMap<DrvHash, (crate::sla::cost::Band, crate::sla::cost::Cap, Instant)>,
    /// Per-derivation ICE-ladder attempt log: `(band, cap)` cells this
    /// drv has timed-out on so far. Separate from `pending_intents`
    /// because the sweep DROPS that entry on timeout (so the selector-
    /// pin doesn't re-emit the dead cell) and the next ack `or_insert`s
    /// fresh — `attempted` must survive that cycle. `solve_intent_for`
    /// checks `len() >= IceBackoff::ladder_cap()` to force band-
    /// agnostic dispatch (`r[sched.sla.ice-ladder-cap]`).
    /// `handle_heartbeat` clears on first heartbeat (pod scheduled →
    /// ladder reset); `clear_persisted_state` clears on leader edge.
    pub(crate) ice_attempts:
        dashmap::DashMap<DrvHash, Vec<(crate::sla::cost::Band, crate::sla::cost::Cap)>>,
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
    /// the lease task writes `is_leader`/`generation` via
    /// [`LeaderState::on_acquire`]/[`LeaderState::on_lose`]; the actor
    /// writes `recovery_complete` via
    /// [`LeaderState::set_recovery_complete`]; everything else is
    /// `SeqCst`/`Acquire` reads. See [`LeaderState`] for the
    /// multi-field ordering rationale.
    ///
    /// u64 generation, not i64: the proto is `uint64` (WorkAssignment,
    /// Heartbeat). PG's `assignments.generation` is BIGINT (signed);
    /// cast `u64 as i64` at THAT single boundary instead of at every
    /// proto-encode site.
    leader: LeaderState,
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
    /// ([`dispatch::DagActor::batch_probe_cached_ready`]) sets
    /// `x-rio-service-token` + `x-rio-probe-tenant-id` so the store's
    /// upstream-substitution probe fires —
    /// `r[sched.dispatch.fod-substitute]`. None = local-presence-only
    /// (the pre-fix behaviour).
    service_signer: Option<Arc<rio_auth::hmac::HmacSigner>>,
    /// Shutdown token. When cancelled (SIGTERM via `shutdown_signal`),
    /// the run loop drains `self.executors` and breaks. Dropping the
    /// worker `stream_tx` senders cascades: `build-exec-bridge` tasks
    /// exit → `ReceiverStream` closes → tonic's `serve_with_shutdown`
    /// sees all response streams closed → server returns. Without
    /// this, `serve_with_shutdown` deadlocks on open bidi streams
    /// because the `SchedulerGrpc` that holds an `ActorHandle`
    /// (sender) is itself held by the server's handler registry —
    /// circular wait.
    ///
    /// Default (from `new()`) is a fresh never-cancelled token →
    /// tests and non-production constructors are unchanged.
    shutdown: rio_common::signal::Token,
    /// I-025 freeze detector: when `fod_deferred > 0 && fetcher_streams == 0`
    /// first became true. `dispatch_ready` WARNs after 60s elapsed, then
    /// resets this so the WARN re-fires once/minute (not once/dispatch-pass).
    /// Reset to None when either side of the AND clears.
    ///
    /// The scheduler already surfaces the freeze via the
    /// `rio_scheduler_queue_depth{kind}` + `rio_scheduler_utilization{kind}`
    /// gauges — but those require a port-forward to observe. A WARN lands
    /// in `kubectl logs`. QA I-025: all 4 builds froze at 29/219 for 20min
    /// with zero ERROR/WARN while queue_depth{fetcher}=41 and fetcher
    /// streams=0. Per-kind so builder/fetcher freeze independently.
    freeze_builders_since: Option<Instant>,
    freeze_fetchers_since: Option<Instant>,
    /// Systems already WARNed as unroutable. Edge-triggers the
    /// `r[sched.dispatch.unroutable-system]` log: WARN once when a
    /// system first has Ready drvs but zero advertising executors;
    /// re-armed when the system becomes routable again. Also the set
    /// the gauge zeroing loop iterates so stale labels don't persist.
    unroutable_warned: HashSet<String>,
    /// Set by events that change dispatch eligibility (Heartbeat, drain).
    /// `handle_tick` consumes it: `if dirty { dispatch_ready(); dirty=false; }`.
    /// I-163: Heartbeat used to call `dispatch_ready` inline — at 290
    /// workers / 10s × 169ms each that's ~5× actor capacity. Coalescing
    /// to once-per-Tick drops it to ≤1/s; ProcessCompletion / MergeDag
    /// still dispatch inline (those genuinely unlock new derivations);
    /// Heartbeat became_idle and PrefetchComplete share the
    /// [`BECAME_IDLE_INLINE_CAP`] budget (those only change placement
    /// candidacy).
    // r[impl sched.actor.dispatch-decoupled]
    dispatch_dirty: bool,
    /// Advances once per `handle_tick`. The dispatch-time substitute
    /// probe stamps each checked node's `probed_generation` with this
    /// value and skips already-stamped nodes within the same
    /// generation, so the [`DISPATCH_PROBE_BATCH_CAP`] truncate window
    /// advances across inline `dispatch_ready` calls instead of
    /// re-FMP'ing the same head. Starts at 1 so freshly-inserted nodes
    /// (`probed_generation: 0`) are immediately eligible.
    probe_generation: u64,
    /// Inline `dispatch_ready` calls fired from the "worker newly
    /// available" carve-out (Heartbeat `became_idle` + `PrefetchComplete`
    /// cold→warm) since the last Tick. Capped at
    /// [`BECAME_IDLE_INLINE_CAP`]; once hit, further such edges only
    /// set `dispatch_dirty`. Reset to 0 in `handle_tick`. Guards the
    /// failover/mass-reconnect case where every executor's first
    /// heartbeat is a 0→1 transition AND its PrefetchComplete is a
    /// cold→warm edge — the `r[sched.dispatch.became-idle-immediate]`
    /// carve-out assumed "≤1 per executor per spawn cycle", which is
    /// true steady-state but becomes 2N-at-once after leader failover
    /// (the I-163 storm via the back door).
    became_idle_inline_this_tick: u32,
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
    /// Incremented on every `dispatch_ready` entry (after the
    /// leader/recovery gate). Asserts on the I-163 dispatch-decoupled
    /// rule: a steady-state heartbeat must NOT bump this.
    pub dispatch_ready_calls: std::sync::atomic::AtomicU64,
    /// Incremented on every singular `persist_status` call (NOT the
    /// batch variant). Asserts on the I-139 rule: a batched completion
    /// path must NOT touch the per-row helper.
    pub persist_status_calls: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl TestCounters {
    pub(crate) fn snapshot(&self) -> TestCountersSnapshot {
        use std::sync::atomic::Ordering::SeqCst;
        TestCountersSnapshot {
            dispatch_ready_calls: self.dispatch_ready_calls.load(SeqCst),
            persist_status_calls: self.persist_status_calls.load(SeqCst),
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
    pub dispatch_ready_calls: u64,
    pub persist_status_calls: u64,
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

        Self {
            dag,
            ready_queue: ReadyQueue::new(),
            builds: HashMap::new(),
            events: BuildEventBus::new(plumbing.event_persist_tx, plumbing.log_flush_tx),
            log_buffers: plumbing.log_buffers,
            executors: HashMap::new(),
            recently_disconnected: HashMap::new(),
            retry_policy: cfg.retry_policy,
            poison_config: cfg.poison,
            db,
            store_client: plumbing.store_client,
            grpc_timeout: cfg.grpc_timeout,
            substitute_sem: Arc::new(tokio::sync::Semaphore::new(
                cfg.substitute_max_concurrent.max(1),
            )),
            cache_breaker: CacheCheckBreaker::default(),
            sla_estimator: crate::sla::SlaEstimator::new(&cfg.sla),
            sla_tiers: cfg.sla.solve_tiers(),
            sla_ceilings: cfg.sla.ceilings(),
            sla_config: cfg.sla,
            cost_table: plumbing.cost_table,
            ice: Arc::new(crate::sla::cost::IceBackoff::default()),
            pending_intents: dashmap::DashMap::new(),
            ice_attempts: dashmap::DashMap::new(),
            tick_count: 0,
            backpressure_active: Arc::new(AtomicBool::new(false)),
            leader: plumbing.leader,
            self_tx: None,
            soft_features: cfg.soft_features,
            hmac_signer: plumbing.hmac_signer,
            service_signer: plumbing.service_signer,
            shutdown: plumbing.shutdown,
            freeze_builders_since: None,
            freeze_fetchers_since: None,
            unroutable_warned: HashSet::new(),
            dispatch_dirty: false,
            probe_generation: 1,
            became_idle_inline_this_tick: 0,
            snapshot_tx: watch::channel(Arc::new(ClusterSnapshot::default())).0,
            #[cfg(test)]
            recovery_toctou_gate: plumbing.recovery_toctou_gate,
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

    /// Reset DAG + per-build maps to empty. Called on leader-acquire,
    /// leader-lost, and recovery-failure — every path that discards
    /// in-memory persisted state. Re-applies `soft_features` to the
    /// fresh DAG so the I-204 strip survives leader transitions
    /// (regression: the original `self.dag = DerivationDag::new()` at
    /// each site dropped soft_features → first prod deploy of I-204
    /// was a no-op after the lease acquired). Does NOT touch
    /// `self.executors` — those are live connections, not persisted.
    pub(super) fn clear_persisted_state(&mut self) {
        self.dag = DerivationDag::new();
        self.dag.set_soft_features(self.soft_features.clone());
        self.ready_queue.clear();
        self.builds.clear();
        self.events.clear();
        // Per-generation maps: a same-process lose→reacquire (apiserver
        // blip) would otherwise carry stale `(band, cap, since)` entries
        // into the new generation → `compute_spawn_intents` pins
        // selectors to the previous leader's cell + false `ice.mark()`
        // from stale `since`. `recently_disconnected` is keyed by
        // executor IDs from the previous generation — a stale entry
        // would let a `ReportExecutorTermination` from the previous gen
        // spuriously bump `resource_floor` on a drv this generation
        // never assigned.
        self.pending_intents.clear();
        self.ice_attempts.clear();
        self.recently_disconnected.clear();
        // Deliberately retained across generations:
        // - `executors`: live connections, not persisted (doc above).
        // - `ice`: cluster-level cell-backoff signal, 60s TTL self-heals.
        // - `cache_breaker`: store availability is generation-independent.
        // - `sla_estimator`: cluster-wide fitted curves.
        // - `tick_count`: harmless counter.
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
                    info!(
                        workers = self.executors.len(),
                        "actor shutting down, dropping worker streams"
                    );
                    // Drop all stream_tx → build-exec-bridge tasks
                    // see actor_rx close → drop output_tx →
                    // ReceiverStream closes → serve_with_shutdown
                    // unblocks.
                    self.executors.clear();
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
            // command (admin RPCs timeout, heartbeats pile up, dispatch
            // stalls). Export as a histogram + WARN over 1s so the next
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
                } => {
                    // r[impl sched.lease.standby-drops-writes]
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
                            final_resources,
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
                    let result = self
                        .handle_cancel_build(build_id, caller_tenant, &reason)
                        .await;
                    let _ = reply.send(result);
                }
                ActorCommand::ExecutorConnected {
                    executor_id,
                    stream_tx,
                    stream_epoch,
                    auth_intent,
                    reply,
                } => {
                    let result = self.handle_worker_connected(
                        &executor_id,
                        stream_tx,
                        stream_epoch,
                        auth_intent,
                    );
                    let _ = reply.send(result);
                }
                ActorCommand::ExecutorDisconnected {
                    executor_id,
                    stream_epoch,
                    seen_drvs,
                } => {
                    self.handle_executor_disconnected(&executor_id, stream_epoch, seen_drvs)
                        .await;
                }
                ActorCommand::ReportExecutorTermination {
                    executor_id,
                    reason,
                    reply,
                } => {
                    // r[impl sched.lease.standby-drops-writes] —
                    // would bump `resource_floor` from a previous
                    // generation's assignment.
                    let promoted = if self.leader.is_leader() {
                        self.handle_executor_termination(&executor_id, reason).await
                    } else {
                        false
                    };
                    let _ = reply.send(promoted);
                }
                ActorCommand::AckSpawnedIntents { spawned } => {
                    // r[impl sched.lease.standby-drops-writes] —
                    // would arm `pending_intents` for a previous
                    // generation's spawn.
                    if self.leader.is_leader() {
                        self.handle_ack_spawned_intents(&spawned);
                    }
                }
                ActorCommand::PrefetchComplete {
                    executor_id,
                    paths_fetched,
                } => {
                    // r[impl sched.dispatch.became-idle-immediate]
                    // Cold→warm is the same "worker newly available"
                    // edge as Heartbeat became_idle — it changes
                    // placement candidacy, not derivation readiness.
                    // Share the inline budget so leader-failover (N
                    // reconnects → N near-simultaneous PrefetchComplete
                    // ACKs) coalesces to dispatch_dirty instead of N
                    // sequential dispatch_ready passes. Already-warm
                    // re-ACKs (per-assignment hints) change no
                    // eligibility → skip dispatch entirely.
                    //
                    // r[sched.lease.standby-drops-writes]: arm stays
                    // ungated — `handle_prefetch_complete` is in-memory
                    // only; `dispatch_ready` self-gates on `is_leader()`
                    // (dispatch.rs).
                    if self.handle_prefetch_complete(&executor_id, paths_fetched) {
                        if self.became_idle_inline_this_tick < BECAME_IDLE_INLINE_CAP {
                            self.became_idle_inline_this_tick += 1;
                            self.dispatch_ready().await;
                        } else {
                            self.dispatch_dirty = true;
                        }
                    }
                }
                ActorCommand::Heartbeat(hb) => {
                    let executor_id = hb.executor_id.clone();
                    let (phantoms, became_idle) = self.handle_heartbeat(hb);
                    // I-035: drain phantom assignments BEFORE the next
                    // dispatch so the freed slot + re-queued derivation
                    // are both visible to it.
                    // r[impl sched.lease.standby-drops-writes] —
                    // `drain_phantoms` persists Ready to PG; the arm
                    // itself stays ungated (`handle_heartbeat` keeps
                    // `self.executors` accurate for reconnect-after-
                    // reacquire and doesn't write PG). `dispatch_ready`
                    // below self-gates (dispatch.rs).
                    if !phantoms.is_empty() && self.leader.is_leader() {
                        self.drain_phantoms(&executor_id, phantoms).await;
                    }
                    // I-163: mark dirty instead of dispatching inline.
                    // 290 workers × 10s heartbeat × 169ms dispatch_ready
                    // = ~5× actor capacity → mailbox_depth=9.5k → admin
                    // RPC timeouts. handle_tick drains the flag at ≤1/s;
                    // ProcessCompletion / MergeDag still dispatch inline
                    // (those unlock new derivations).
                    // r[impl sched.actor.dispatch-decoupled]
                    //
                    // r[impl sched.dispatch.became-idle-immediate]
                    // Carve-out: capacity 0→1 (fresh ephemeral, degrade
                    // clear, drain clear) dispatches inline. ≤1 per
                    // executor per spawn cycle steady-state — but
                    // leader-failover makes EVERY executor's first
                    // heartbeat a 0→1 edge. Cap inline dispatches per
                    // Tick so mass-reconnect coalesces to dirty
                    // instead of N sequential dispatch_ready passes.
                    if became_idle && self.became_idle_inline_this_tick < BECAME_IDLE_INLINE_CAP {
                        self.became_idle_inline_this_tick += 1;
                        self.dispatch_ready().await;
                    } else {
                        self.dispatch_dirty = true;
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
                    // Actor doesn't use this — it's the gRPC layer's
                    // lower bound for PG replay. We only supply the
                    // upper bound (last_seq, inside handle_watch_build).
                    since_sequence: _,
                    reply,
                } => {
                    let result = self.handle_watch_build(build_id, caller_tenant);
                    let _ = reply.send(result);
                }
                ActorCommand::CleanupTerminalBuild { build_id } => {
                    self.handle_cleanup_terminal_build(build_id);
                }
                ActorCommand::Admin(q) => {
                    self.handle_admin(q);
                }
                ActorCommand::ClearPoison { drv_hash, reply } => {
                    let cleared = self.handle_clear_poison(&drv_hash).await;
                    let _ = reply.send(cleared);
                }
                ActorCommand::DrainExecutor {
                    executor_id,
                    force,
                    reply,
                } => {
                    let result = self.handle_drain_executor(&executor_id, force).await;
                    let _ = reply.send(result);
                }
                ActorCommand::ForwardLogBatch { drv_path, batch } => {
                    self.handle_forward_log_batch(&drv_path, batch);
                }
                ActorCommand::ForwardPhase { phase } => {
                    self.handle_forward_phase(phase);
                }
                ActorCommand::LeaderLost => {
                    self.handle_leader_lost();
                }
                ActorCommand::LeaderAcquired => {
                    self.handle_leader_acquired().await;
                    self.schedule_reconcile_timer();
                    // Immediate dispatch attempt after recovery. If
                    // workers haven't reconnected yet, dispatch finds
                    // no candidates → no-op. If they HAVE (workers
                    // reconnect on scheduler restart faster than this
                    // actor command is processed), dispatch fires
                    // immediately instead of waiting ~10s for the
                    // first heartbeat to trigger it.
                    self.dispatch_ready().await;
                }
                ActorCommand::ReconcileAssignments => {
                    // r[impl sched.lease.standby-drops-writes]
                    if self.leader.is_leader() {
                        self.handle_reconcile_assignments().await;
                    }
                }
                ActorCommand::SubstituteComplete { drv_hash, ok } => {
                    // r[impl sched.lease.standby-drops-writes]
                    if self.leader.is_leader() {
                        self.handle_substitute_complete(&drv_hash, ok).await;
                    }
                }
                #[cfg(test)]
                ActorCommand::Debug(d) => {
                    self.handle_debug(d);
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

    /// Clone the generation counter as a read-only reader for
    /// `ActorHandle::leader_generation()`. The lease task holds a
    /// direct `Arc<AtomicU64>` clone for writing — not through this
    /// reader. The reader type has no store/fetch_add methods, so
    /// handle consumers can't accidentally increment.
    pub(crate) fn generation_reader(&self) -> GenerationReader {
        GenerationReader::new(self.leader.generation_arc())
    }
}
