//! DAG actor: single Tokio task owning all mutable scheduler state.
//!
//! All gRPC handlers communicate with the actor via an mpsc command channel.
//! The actor processes commands serially, ensuring deterministic ordering
//! and eliminating lock contention.
// r[impl sched.actor.single-owner]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tonic::transport::Channel;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use rio_proto::StoreServiceClient;
use rio_proto::types::FindMissingPathsRequest;

use crate::dag::DerivationDag;
use crate::db::SchedulerDb;
use crate::estimator::Estimator;
use crate::queue::ReadyQueue;
use crate::state::{
    BuildInfo, BuildState, BuildStateExt, DerivationStatus, DrvHash, ExecutorId, ExecutorState,
    HEARTBEAT_TIMEOUT_SECS, MAX_MISSED_HEARTBEATS, POISON_TTL, PoisonConfig, RetryPolicy,
};

// `impl DagActor` is sharded across these submodules by concern; each
// file's impl block reaches into the same struct fields (single-owner
// actor — no encapsulation boundary). Keep ALL `mod` decls here so the
// submodule list is discoverable in one place.
mod breaker;
mod build;
mod command;
mod completion;
mod config;
mod dispatch;
mod event;
mod executor;
mod handle;
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
/// hint in `worker.rs` (`on_worker_registered`) and the per-dispatch
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

/// Default cap on concurrent `QueryPathInfo` calls during merge-time
/// eager substitute fetch. 16 balances throughput against the store's
/// S3 connection-pool ceiling (~10-20 aws-sdk default). Unbounded
/// fan-out at ~1k paths causes "dispatch failure" → ~20% false demotes.
/// Overridable via `RIO_SUBSTITUTE_MAX_CONCURRENT`.
pub const DEFAULT_SUBSTITUTE_CONCURRENCY: usize = 16;

/// Delay before cleaning up terminal build state. Allows late WatchBuild
/// subscribers to receive the terminal event before the broadcast sender
/// is dropped.
const TERMINAL_CLEANUP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// Connected workers.
    executors: HashMap<ExecutorId, ExecutorState>,
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
    /// Max in-flight `QueryPathInfo` calls during merge-time eager
    /// substitute fetch (r[sched.merge.substitute-fetch]). Bounds
    /// `buffer_unordered(N)` in `check_cached_outputs`. Unbounded
    /// fan-out of ~1k concurrent QPI calls saturates the store's S3
    /// connection pool → "dispatch failure" → false demotes. Default
    /// [`DEFAULT_SUBSTITUTE_CONCURRENCY`] (16). Overridable via
    /// `RIO_SUBSTITUTE_MAX_CONCURRENT` env or scheduler.toml.
    substitute_max_concurrent: usize,
    /// Circuit breaker for the cache-check FindMissingPaths call. Owned by
    /// the actor (single-threaded, no lock needed). Checked/updated in
    /// `merge.rs::check_cached_outputs`.
    cache_breaker: CacheCheckBreaker,
    /// Build duration estimator. Snapshot of `build_history`, refreshed
    /// periodically on Tick. Critical-path and size-class routing read
    /// from this. Single-threaded actor owns it — no Arc/lock.
    estimator: Estimator,
    /// Tick counter for periodic tasks that run less often than every
    /// Tick (e.g., estimator refresh every ~60s with a 10s tick interval).
    /// Wraps at u64::MAX — harmless, just means the 60s cadence drifts
    /// by one tick after ~5.8 billion years.
    tick_count: u64,
    /// Whether backpressure is currently active. Shared with ActorHandle
    /// so hysteresis (80%/60%) is honored by send() instead of a simple
    /// threshold check. `Arc<AtomicBool>` for lock-free reads on the hot path.
    backpressure_active: Arc<AtomicBool>,
    /// Leader generation counter (for assignment tokens + stale-work
    /// detection at workers).
    ///
    /// `Arc<AtomicU64>` not `i64`: the lease task (C2, spawned in
    /// main.rs) is the sole WRITER — it `fetch_add(1, Release)` on
    /// each leadership acquisition. The actor reads via the same Arc
    /// for dispatch; `ActorHandle` clones a `GenerationReader` for
    /// gRPC's `HeartbeatResponse.generation`. Same cross-task sharing
    /// pattern as `backpressure_active`.
    ///
    /// u64 not i64: the proto is `uint64` (WorkAssignment, Heartbeat).
    /// The prior `i64 as u64` cast at dispatch.rs was a silent sign-
    /// reinterpret — harmless in practice (Lease transitions can't go
    /// negative) but a latent footgun. PG's `assignments.generation`
    /// is BIGINT (signed); cast `u64 as i64` at THAT single boundary
    /// instead of at every proto-encode site. One cast, edge not hot
    /// path.
    generation: Arc<AtomicU64>,
    /// Weak clone of the actor's own command sender, for scheduling delayed
    /// internal commands (e.g., terminal build cleanup). Weak so the actor
    /// doesn't prevent channel close when all external handles are dropped.
    /// `None` if spawned via bare `run()` (no delayed scheduling).
    self_tx: Option<mpsc::WeakSender<ActorCommand>>,
    /// Size-class cutoff config. Empty = feature off (no classification).
    /// dispatch.rs calls classify() with a read guard; completion.rs
    /// reads cutoff_for() for misclassification detection.
    ///
    /// `Arc<parking_lot::RwLock<...>>` — shared with the rebalancer
    /// task (spawned in `run_inner`) which writes new cutoffs hourly.
    /// parking_lot not tokio::sync: writes are rare (1/hour) so
    /// contention is near-zero, and a sync lock keeps `classify()`
    /// sync — no `.await` inside dispatch's hot read path.
    ///
    /// R10 CHECK: callers MUST NOT hold a read/write guard across
    /// `.await`. parking_lot guards are not `Send` so the borrow
    /// checker catches some misuse, but a `.read()` followed by
    /// `.await` on the same task blocks the executor thread. See
    /// dispatch.rs: guards are dropped before any await boundary.
    size_classes: Arc<parking_lot::RwLock<Vec<crate::assignment::SizeClassConfig>>>,
    /// Fetcher size-class config (I-170). Empty = feature off (single
    /// fetcher pool, no class filter — original behavior). Ordered
    /// smallest→largest; `find_executor_with_overflow`'s FOD branch
    /// walks from `DerivationState.sched.size_class_floor` upward. Plain
    /// `Vec` (not `Arc<RwLock>`): no rebalancer mutates this — it's
    /// just an ordered name list, config-static after construction.
    fetcher_size_classes: Vec<String>,
    /// I-204: capability-hint features stripped at DAG insertion.
    /// Mirrored onto `self.dag` by `with_soft_features` and re-applied
    /// in `clear_persisted_state` (recovery replaces the DAG).
    soft_features: Vec<crate::assignment::SoftFeature>,
    /// ADR-020 capacity manifest headroom. Applied by both
    /// `compute_capacity_manifest` (manifest RPC) and the dispatch-time
    /// resource-fit filter. Config-global; per-pool later if needed.
    /// Validated finite + positive at startup (main.rs).
    ///
    /// f64 not Arc: config-static, never mutated after
    /// `with_headroom_mult()`. No runtime override.
    headroom_mult: f64,
    /// Static TOML cutoffs, captured once in `with_size_classes()`
    /// BEFORE the rebalancer's first write. The rebalancer mutates
    /// `size_classes[i].cutoff_secs` in-place hourly; without this
    /// snapshot, there's no way to report drift to operators.
    /// `(name, cutoff_secs)` pairs — HashMap would be idiomatic but
    /// Vec preserves config order (matters for the RPC response which
    /// sorts by effective cutoff, but configured order is useful for
    /// logging).
    configured_cutoffs: Vec<(String, f64)>,
    /// Leader flag from the lease task. `dispatch_ready` early-
    /// returns if false → standby schedulers merge DAGs (state
    /// stays warm) but don't send assignments. Default `true` for
    /// non-K8s mode (no lease task = always leader).
    ///
    /// Relaxed load: it's a standalone flag with no other state to
    /// synchronize. A one-pass lag on false→true is harmless (next
    /// dispatch pass works); true→false means one lame-duck
    /// dispatch (idempotent — workers reject stale-gen assignments
    /// after the new leader increments).
    is_leader: Arc<AtomicBool>,
    /// Set by handle_leader_acquired AFTER recover_from_pg
    /// completes (success or failure — see recovery.rs module
    /// doc). dispatch_ready gates on BOTH is_leader AND this.
    ///
    /// Why two flags: the lease loop sets is_leader=true
    /// IMMEDIATELY on acquire (non-blocking), then fire-and-
    /// forgets LeaderAcquired. Recovery may take seconds for a
    /// large DAG. If dispatch gated ONLY on is_leader, it would
    /// try to dispatch from an incomplete DAG mid-recovery.
    ///
    /// Non-K8s mode (always_leader): initialized `true` since
    /// there's no lease acquisition to trigger recovery. The DAG
    /// starts empty (as before); no recovery from PG because
    /// there's no failover.
    ///
    /// Relaxed/Release/Acquire: Release on store (handle_leader_
    /// acquired), Acquire on load (dispatch_ready) so dispatch
    /// sees all writes from recovery before proceeding. Not
    /// STRICTLY needed (actor is single-threaded → single-thread
    /// sequential consistency) but documents the pairing and
    /// doesn't cost anything.
    recovery_complete: Arc<AtomicBool>,
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
    hmac_signer: Option<Arc<rio_common::hmac::HmacSigner>>,
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
    /// `rio_scheduler_fod_queue_depth` + `rio_scheduler_fetcher_utilization`
    /// gauges — but those require a port-forward to observe. A WARN lands
    /// in `kubectl logs`. QA I-025: all 4 builds froze at 29/219 for 20min
    /// with zero ERROR/WARN while fod_queue_depth=41 and fetcher streams=0.
    fod_freeze_since: Option<Instant>,
    /// Same pattern for non-FOD derivations stuck with zero builder streams.
    /// Tracks `class_deferred.values().sum() > 0 && builder_streams == 0`.
    builder_freeze_since: Option<Instant>,
    /// Set by events that change dispatch eligibility (Heartbeat, drain).
    /// `handle_tick` consumes it: `if dirty { dispatch_ready(); dirty=false; }`.
    /// I-163: Heartbeat used to call `dispatch_ready` inline — at 290
    /// workers / 10s × 169ms each that's ~5× actor capacity. Coalescing
    /// to once-per-Tick drops it to ≤1/s; state-change events
    /// (PrefetchComplete, ProcessCompletion, MergeDag) still dispatch
    /// inline because those genuinely unlock new work.
    // r[impl sched.actor.dispatch-decoupled]
    dispatch_dirty: bool,
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
}

impl DagActor {
    /// Create a new actor.
    ///
    /// `cfg` holds operator deploy config (scheduler.toml / env);
    /// `plumbing` holds runtime channels and shared leader state. Both
    /// are `Default`-able — tests / non-K8s spawns can
    /// `..Default::default()` and override one or two fields.
    pub fn new(db: SchedulerDb, cfg: DagActorConfig, plumbing: DagActorPlumbing) -> Self {
        // Snapshot the as-loaded cutoffs BEFORE the rebalancer sees
        // them. GetSizeClassSnapshot reports both: effective (mutated
        // hourly) vs configured (this snapshot) for drift visibility.
        let configured_cutoffs = cfg
            .size_classes
            .iter()
            .map(|c| (c.name.clone(), c.cutoff_secs))
            .collect();
        let size_classes = Arc::new(parking_lot::RwLock::new(cfg.size_classes));
        // I-204: soft-feature stripping is configured on the DAG. Stored
        // on the actor (not just the DAG) because `clear_persisted_state`
        // replaces `self.dag` on every leader transition — the actor copy
        // is what survives. The class-order snapshot (for I-213 floor-hint
        // comparison) is derived from the SAME size_classes the rebalancer
        // shares, so soft_features always sees the right order.
        let mut dag = DerivationDag::new();
        let order = crate::assignment::builder_class_order(&size_classes.read());
        dag.set_soft_features(cfg.soft_features.clone(), order);

        Self {
            dag,
            ready_queue: ReadyQueue::new(),
            builds: HashMap::new(),
            events: BuildEventBus::new(plumbing.event_persist_tx, plumbing.log_flush_tx),
            executors: HashMap::new(),
            retry_policy: cfg.retry_policy,
            poison_config: cfg.poison,
            db,
            store_client: plumbing.store_client,
            grpc_timeout: cfg.grpc_timeout,
            substitute_max_concurrent: cfg.substitute_max_concurrent,
            cache_breaker: CacheCheckBreaker::default(),
            estimator: Estimator::default(),
            tick_count: 0,
            backpressure_active: Arc::new(AtomicBool::new(false)),
            generation: plumbing.leader.generation_arc(),
            self_tx: None,
            size_classes,
            fetcher_size_classes: cfg.fetcher_size_classes,
            soft_features: cfg.soft_features,
            headroom_mult: cfg.headroom_mult,
            configured_cutoffs,
            is_leader: plumbing.leader.is_leader_arc(),
            recovery_complete: plumbing.leader.recovery_complete_arc(),
            hmac_signer: plumbing.hmac_signer,
            shutdown: plumbing.shutdown,
            fod_freeze_since: None,
            builder_freeze_since: None,
            dispatch_dirty: false,
            snapshot_tx: watch::channel(Arc::new(ClusterSnapshot::default())).0,
            #[cfg(test)]
            recovery_toctou_gate: plumbing.recovery_toctou_gate,
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
        let order = crate::assignment::builder_class_order(&self.size_classes.read());
        self.dag
            .set_soft_features(self.soft_features.clone(), order);
        self.ready_queue.clear();
        self.builds.clear();
        self.events.clear();
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

        // Rebalancer: hourly recompute of size-class cutoffs.
        // Shares the `size_classes` Arc — writes new `cutoff_secs`
        // via write lock; dispatch/completion read via `.read()`.
        // No-op if size_classes empty (feature off).
        crate::rebalancer::spawn_task(
            self.db.clone(),
            Arc::clone(&self.size_classes),
            self.shutdown.clone(),
        );

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
                            .handle_cancel_build(build_id, "client_disconnect_during_merge")
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
                } => {
                    self.handle_completion(
                        &executor_id,
                        &drv_key,
                        result,
                        (peak_memory_bytes, peak_cpu_cores),
                    )
                    .await;
                }
                ActorCommand::CancelBuild {
                    build_id,
                    reason,
                    reply,
                } => {
                    let result = self.handle_cancel_build(build_id, &reason).await;
                    let _ = reply.send(result);
                }
                ActorCommand::ExecutorConnected {
                    executor_id,
                    stream_tx,
                } => {
                    self.handle_worker_connected(&executor_id, stream_tx);
                }
                ActorCommand::ExecutorDisconnected { executor_id } => {
                    self.handle_executor_disconnected(&executor_id).await;
                }
                ActorCommand::PrefetchComplete {
                    executor_id,
                    paths_fetched,
                } => {
                    self.handle_prefetch_complete(&executor_id, paths_fetched);
                    // Dispatch: a newly-warm worker may now be the
                    // best candidate for queued derivations that were
                    // previously deferred (no warm worker passed the
                    // hard filter).
                    self.dispatch_ready().await;
                }
                ActorCommand::Heartbeat(hb) => {
                    let (phantoms, became_idle) = self.handle_heartbeat(hb);
                    // I-035: drain phantom assignments BEFORE the next
                    // dispatch so the freed slot + re-queued derivation
                    // are both visible to it.
                    if !phantoms.is_empty() {
                        self.drain_phantoms(phantoms).await;
                    }
                    // I-163: mark dirty instead of dispatching inline.
                    // 290 workers × 10s heartbeat × 169ms dispatch_ready
                    // = ~5× actor capacity → mailbox_depth=9.5k → admin
                    // RPC timeouts. handle_tick drains the flag at ≤1/s;
                    // ProcessCompletion / PrefetchComplete / MergeDag
                    // still dispatch inline (those unlock new work).
                    // r[impl sched.actor.dispatch-decoupled]
                    //
                    // r[impl sched.dispatch.became-idle-immediate]
                    // Carve-out: capacity 0→1 (fresh ephemeral, degrade
                    // clear, drain clear) dispatches inline. ≤1 per
                    // executor per spawn cycle — not the 29/s storm.
                    // Steady-state (already-idle or already-busy) still
                    // only sets dirty.
                    if became_idle {
                        self.dispatch_ready().await;
                    } else {
                        self.dispatch_dirty = true;
                    }
                }
                ActorCommand::Tick => {
                    self.handle_tick().await;
                }
                ActorCommand::QueryBuildStatus { build_id, reply } => {
                    let result = self.handle_query_build_status(build_id);
                    let _ = reply.send(result);
                }
                ActorCommand::WatchBuild {
                    build_id,
                    // Actor doesn't use this — it's the gRPC layer's
                    // lower bound for PG replay. We only supply the
                    // upper bound (last_seq, inside handle_watch_build).
                    since_sequence: _,
                    reply,
                } => {
                    let result = self.handle_watch_build(build_id);
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
                    self.handle_reconcile_assignments().await;
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
        GenerationReader::new(Arc::clone(&self.generation))
    }
}
