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

// `oneshot` used by submodules via `use super::*;` — not here.
// Same for BuildOptions/PriorityClass in the state import.
// mod.rs is the import hub for the actor/* submodule tree.
#[allow(unused_imports)]
use tokio::sync::{broadcast, mpsc, oneshot};
use tonic::transport::Channel;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use rio_proto::StoreServiceClient;
use rio_proto::types::FindMissingPathsRequest;

use crate::dag::DerivationDag;
use crate::db::SchedulerDb;
use crate::estimator::{BucketedEstimate, Estimator};
use crate::queue::ReadyQueue;
#[allow(unused_imports)]
use crate::state::{
    BuildInfo, BuildOptions, BuildState, DerivationStatus, DrvHash, ExecutorId, ExecutorState,
    HEARTBEAT_TIMEOUT_SECS, MAX_MISSED_HEARTBEATS, POISON_TTL, PoisonConfig, PriorityClass,
    RetryPolicy,
};

mod command;
pub use command::*;

mod recovery;

/// Channel capacity for the actor command channel.
pub const ACTOR_CHANNEL_CAPACITY: usize = 10_000;

/// Max store paths per `PrefetchHint`. Shared between the initial-warm
/// hint in `worker.rs` (`on_worker_registered`) and the per-dispatch
/// hint in `dispatch.rs` — bump BOTH semantics by changing this once.
pub(crate) const MAX_PREFETCH_PATHS: usize = 100;

/// Backpressure: reject new work above this fraction of channel capacity.
const BACKPRESSURE_HIGH_WATERMARK: f64 = 0.80;

/// Backpressure: resume accepting work below this fraction.
const BACKPRESSURE_LOW_WATERMARK: f64 = 0.60;

/// Number of events to retain in each build's event buffer for late subscribers.
const BUILD_EVENT_BUFFER_SIZE: usize = 1024;

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

/// Delay before post-recovery worker reconciliation. Workers have
/// this long to reconnect after scheduler restart; after that, any
/// Assigned/Running derivation with an unknown worker is reconciled
/// (Completed if outputs in store, else reset to Ready).
///
/// 45s = 3× HEARTBEAT_INTERVAL (10s) + 15s slack. A worker that's
/// alive should reconnect within one heartbeat; 3× covers network
/// blips. Same cfg(test) shadow pattern as POISON_TTL.
#[cfg(not(test))]
const RECONCILE_DELAY: std::time::Duration = std::time::Duration::from_secs(45);
#[cfg(test)]
const RECONCILE_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Backdate an Instant by `secs_ago` seconds. `checked_sub` is used
/// defensively: if `secs_ago` is absurd (e.g. `u64::MAX`) and
/// `Instant::now()` can't represent that far back, clamp to "now"
/// (effectively 0 elapsed). Tokio paused time can't mock `Instant`
/// — this is why the DebugBackdate* handlers exist at all.
#[cfg(test)]
fn backdate(secs_ago: u64) -> Instant {
    Instant::now()
        .checked_sub(std::time::Duration::from_secs(secs_ago))
        .unwrap_or_else(Instant::now)
}

/// The DAG actor state.
pub struct DagActor {
    /// The global derivation DAG.
    dag: DerivationDag,
    /// FIFO queue of ready derivation hashes.
    ready_queue: ReadyQueue,
    /// Active builds indexed by build_id.
    builds: HashMap<Uuid, BuildInfo>,
    /// Build event broadcast channels.
    build_events: HashMap<Uuid, broadcast::Sender<rio_proto::types::BuildEvent>>,
    /// Per-build sequence counters.
    build_sequences: HashMap<Uuid, u64>,
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
    /// ContentLookup). Defaults to [`rio_common::grpc::DEFAULT_GRPC_TIMEOUT`]
    /// (30s). Tests that arm `MockStore.content_lookup_hang` to prove the
    /// timeout wrapper exists (e.g.
    /// `ca_cutoff_compare_slow_store_doesnt_block_completion`) override to
    /// 3s via [`with_grpc_timeout`](Self::with_grpc_timeout) — same
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
    /// Channel to the LogFlusher task. Completion handlers `try_send` a
    /// FlushRequest here so the S3 upload is ordered AFTER the state
    /// transition (hybrid model: buffer outside actor, flush triggered by
    /// actor). `None` in tests/environments without S3.
    ///
    /// `try_send` (not `send`): if the flusher is backed up, drop the
    /// request. The 30s periodic tick will still catch the buffer (it
    /// snapshots, doesn't drain) until CleanupTerminalBuild removes it.
    /// A dropped final-flush is a downgrade to "periodic snapshot only"
    /// for that one derivation, not a hang.
    log_flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
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
    /// Channel to the event-log persister task. emit_build_event
    /// try_sends (build_id, seq, prost-encoded BuildEvent) here
    /// AFTER the broadcast. Event::Log is filtered out — those
    /// flood PG (~20/sec chatty rustc) and S3 already durables
    /// them via log_flush_tx. None in tests without PG.
    event_persist_tx: Option<mpsc::Sender<crate::event_log::EventLogEntry>>,
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
    /// Create a new actor with the given database handle and optional store client.
    ///
    /// `store_client` is used for the scheduler-side cache check (closes the
    /// TOCTOU window between the gateway's FindMissingPaths and DAG merge).
    /// Pass `None` to skip this check (tests, or if the store is unavailable).
    pub fn new(db: SchedulerDb, store_client: Option<StoreServiceClient<Channel>>) -> Self {
        Self {
            dag: DerivationDag::new(),
            ready_queue: ReadyQueue::new(),
            builds: HashMap::new(),
            build_events: HashMap::new(),
            build_sequences: HashMap::new(),
            executors: HashMap::new(),
            retry_policy: RetryPolicy::default(),
            poison_config: PoisonConfig::default(),
            db,
            store_client,
            grpc_timeout: rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            substitute_max_concurrent: DEFAULT_SUBSTITUTE_CONCURRENCY,
            cache_breaker: CacheCheckBreaker::default(),
            estimator: Estimator::default(),
            tick_count: 0,
            backpressure_active: Arc::new(AtomicBool::new(false)),
            // 1 not 0: proto-default is 0. gen=0 tells workers "field
            // unset" (old scheduler); gen=1 is the real first generation.
            generation: Arc::new(AtomicU64::new(1)),
            self_tx: None,
            size_classes: Arc::new(parking_lot::RwLock::new(Vec::new())),
            headroom_mult: crate::estimator::DEFAULT_HEADROOM_MULTIPLIER,
            configured_cutoffs: Vec::new(),
            log_flush_tx: None,
            // Default true: non-K8s mode, always leader.
            // with_leader_flag() overrides for K8s deployments.
            is_leader: Arc::new(AtomicBool::new(true)),
            // Default true: non-K8s mode has no lease acquire →
            // no recovery trigger. DAG starts empty (as before).
            // with_leader_flag() sets this to the shared Arc from
            // LeaderState (initialized false there) so K8s
            // deployments gate on recovery.
            recovery_complete: Arc::new(AtomicBool::new(true)),
            event_persist_tx: None,
            hmac_signer: None,
            shutdown: rio_common::signal::Token::new(),
            fod_freeze_since: None,
            builder_freeze_since: None,
            #[cfg(test)]
            recovery_toctou_gate: None,
        }
    }

    /// Enable HMAC signing for assignment tokens. Builder-style.
    /// Key loaded by main.rs from `hmac_key_path` config.
    pub fn with_hmac_signer(mut self, signer: rio_common::hmac::HmacSigner) -> Self {
        self.hmac_signer = Some(Arc::new(signer));
        self
    }

    /// Inject the event-log persister channel. Call before
    /// `run_with_self_tx`. Separate from `new()` (same rationale as
    /// `with_log_flusher`): tests without PG leave it None →
    /// emit_build_event skips the try_send, broadcast still works.
    pub fn with_event_persister(
        mut self,
        tx: mpsc::Sender<crate::event_log::EventLogEntry>,
    ) -> Self {
        self.event_persist_tx = Some(tx);
        self
    }

    /// Inject the log flusher channel. Call before `run_with_self_tx`.
    /// Separate from `new()` because tests don't have S3 and the None default
    /// there keeps `new()`'s signature stable.
    pub fn with_log_flusher(mut self, tx: mpsc::Sender<crate::logs::FlushRequest>) -> Self {
        self.log_flush_tx = Some(tx);
        self
    }

    /// Inject size-class config. Empty vec (the default) = no
    /// classification → all workers are candidates for all builds.
    /// Separate from `new()` for the same reason as `with_log_flusher`:
    /// tests don't need it, and deployments without size-class routing
    /// (VM tests phase1a/1b/2a/2b) leave size_classes unconfigured.
    pub fn with_size_classes(mut self, classes: Vec<crate::assignment::SizeClassConfig>) -> Self {
        // Snapshot the as-loaded cutoffs BEFORE the rebalancer sees
        // them. GetSizeClassSnapshot reports both: effective (mutated
        // hourly) vs configured (this snapshot) for drift visibility.
        self.configured_cutoffs = classes
            .iter()
            .map(|c| (c.name.clone(), c.cutoff_secs))
            .collect();
        self.size_classes = Arc::new(parking_lot::RwLock::new(classes));
        self
    }

    /// Inject poison-detection config. Default (3 distinct workers)
    /// matches prior `POISON_THRESHOLD` const behavior. Overriding
    /// `require_distinct_workers=false` lets single-worker dev
    /// deployments poison after N failures on the same worker.
    pub fn with_poison_config(mut self, config: PoisonConfig) -> Self {
        self.poison_config = config;
        self
    }

    /// Inject retry backoff policy. Default: 2 retries, 5s→300s
    /// exponential with 20% jitter. Same builder pattern as
    /// `with_poison_config` — main.rs loads both from scheduler.toml
    /// and threads them through `spawn_with_leader`.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Override the store-RPC timeout. Production leaves the default
    /// (30s); slow-store regression tests set 3s so the wrapper-exists
    /// proof costs ~3s wall-clock instead of ~30s. See the
    /// [`grpc_timeout`](Self#structfield.grpc_timeout) field doc for why
    /// this is plumbed rather than `cfg(test)`-gated.
    pub fn with_grpc_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.grpc_timeout = timeout;
        self
    }

    /// Inject the ADR-020 headroom multiplier. main.rs threads
    /// `cfg.headroom_multiplier` through so dispatch-time resource-fit
    /// and the manifest RPC both see the same operator-configured
    /// value. Default [`DEFAULT_HEADROOM_MULTIPLIER`] (1.25). Same
    /// builder-style optionality as `with_size_classes`.
    ///
    /// [`DEFAULT_HEADROOM_MULTIPLIER`]: crate::estimator::DEFAULT_HEADROOM_MULTIPLIER
    pub fn with_headroom_mult(mut self, mult: f64) -> Self {
        self.headroom_mult = mult;
        self
    }

    /// Override the eager-substitute-fetch concurrency cap. Production
    /// sets via `RIO_SUBSTITUTE_MAX_CONCURRENT`; tests leave the
    /// default. See the
    /// [`substitute_max_concurrent`](Self#structfield.substitute_max_concurrent)
    /// field doc.
    pub fn with_substitute_concurrency(mut self, n: usize) -> Self {
        self.substitute_max_concurrent = n;
        self
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
            self.update_backpressure(queue_len, capacity);

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
                    output_size_bytes,
                    peak_cpu_cores,
                } => {
                    self.handle_completion(
                        &executor_id,
                        &drv_key,
                        result,
                        (peak_memory_bytes, output_size_bytes, peak_cpu_cores),
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
                ActorCommand::Heartbeat {
                    executor_id,
                    systems,
                    supported_features,
                    max_builds,
                    running_builds,
                    bloom,
                    size_class,
                    resources,
                    store_degraded,
                    kind,
                } => {
                    let phantoms = self.handle_heartbeat(
                        &executor_id,
                        systems,
                        supported_features,
                        max_builds,
                        running_builds,
                        bloom,
                        size_class,
                        resources,
                        store_degraded,
                        kind,
                    );
                    // I-035: drain phantom assignments BEFORE dispatch
                    // so the freed slot + re-queued derivation are both
                    // visible to the same dispatch pass.
                    if !phantoms.is_empty() {
                        self.drain_phantoms(phantoms).await;
                    }
                    // Dispatch on heartbeat: new capacity may be available
                    self.dispatch_ready().await;
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
                ActorCommand::ClusterSnapshot { reply } => {
                    let _ = reply.send(self.compute_cluster_snapshot());
                }
                ActorCommand::GetSizeClassSnapshot { reply } => {
                    let _ = reply.send(self.compute_size_class_snapshot());
                }
                ActorCommand::CapacityManifest { reply } => {
                    let _ = reply.send(self.compute_capacity_manifest());
                }
                ActorCommand::ClearPoison { drv_hash, reply } => {
                    let cleared = self.handle_clear_poison(&drv_hash).await;
                    let _ = reply.send(cleared);
                }
                ActorCommand::ListExecutors { reply } => {
                    let _ = reply.send(self.handle_list_executors());
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
                ActorCommand::GcRoots { reply } => {
                    let _ = reply.send(self.handle_gc_roots());
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
                ActorCommand::InspectBuildDag { build_id, reply } => {
                    let _ = reply.send(self.handle_inspect_build_dag(build_id));
                }
                #[cfg(test)]
                ActorCommand::DebugQueryWorkers { reply } => {
                    let _ = reply.send(self.handle_debug_query_workers());
                }
                #[cfg(test)]
                ActorCommand::DebugQueryDerivation { drv_hash, reply } => {
                    let _ = reply.send(self.handle_debug_query_derivation(&drv_hash));
                }
                #[cfg(test)]
                ActorCommand::DebugForceAssign {
                    drv_hash,
                    executor_id,
                    reply,
                } => {
                    let _ = reply.send(self.handle_debug_force_assign(&drv_hash, &executor_id));
                }
                #[cfg(test)]
                ActorCommand::DebugBackdateRunning {
                    drv_hash,
                    secs_ago,
                    reply,
                } => {
                    let _ = reply.send(self.handle_debug_backdate_running(&drv_hash, secs_ago));
                }
                #[cfg(test)]
                ActorCommand::DebugBackdateSubmitted {
                    build_id,
                    secs_ago,
                    reply,
                } => {
                    let _ = reply.send(self.handle_debug_backdate_submitted(build_id, secs_ago));
                }
                #[cfg(test)]
                ActorCommand::DebugClearDrvContent { drv_hash, reply } => {
                    let _ = reply.send(self.handle_debug_clear_drv_content(&drv_hash));
                }
                #[cfg(test)]
                ActorCommand::DebugTripBreaker { n, reply } => {
                    let _ = reply.send(self.handle_debug_trip_breaker(n));
                }
            }
        }

        info!("DAG actor shutting down");
    }

    // -----------------------------------------------------------------------
    // Dispatch-arm handlers extracted from run_inner
    // -----------------------------------------------------------------------

    fn handle_list_executors(&self) -> Vec<command::ExecutorSnapshot> {
        self.executors
            .values()
            .map(|w| command::ExecutorSnapshot {
                executor_id: w.executor_id.clone(),
                kind: w.kind,
                systems: w.systems.clone(),
                supported_features: w.supported_features.clone(),
                max_builds: w.max_builds,
                running_builds: w.running_builds.len() as u32,
                draining: w.draining,
                size_class: w.size_class.clone(),
                connected_since: w.connected_since,
                last_heartbeat: w.last_heartbeat,
                last_resources: w.last_resources,
            })
            .collect()
    }

    /// Resolve drv_path → drv_hash → interested_builds, then emit
    /// `BuildEvent::Log` on each build's broadcast channel. The gateway
    /// already handles Event::Log (handler/build.rs:27-32) — it
    /// translates to STDERR_NEXT for the Nix client.
    ///
    /// Unknown drv_path → drop silently. Two legitimate cases:
    /// (a) batch arrived after CleanupTerminalBuild removed the DAG
    ///     entry (race between worker stream and actor loop — the
    ///     build is done, gateway already saw Completed, late log
    ///     lines are irrelevant);
    /// (b) malformed batch from a buggy worker. Neither warrants a
    ///     `warn!()` — (a) is expected, (b) would spam.
    fn handle_forward_log_batch(&mut self, drv_path: &str, batch: rio_proto::types::BuildLogBatch) {
        let Some(hash) = self.drv_path_to_hash(drv_path) else {
            return;
        };
        let lines = batch.lines.len() as u64;
        for build_id in self.get_interested_builds(&hash) {
            // batch.clone(): BuildLogBatch has Vec<Vec<u8>> so this is
            // a deep copy. For 64 lines × 100 bytes that's ~6.5KB × N
            // interested builds. Typically N=1 (one gateway per build).
            // If profiling ever shows this hot, Arc<BuildLogBatch> in
            // BuildEvent.
            self.emit_build_event(
                build_id,
                rio_proto::types::build_event::Event::Log(batch.clone()),
            );
        }
        // Metric: proves worker → scheduler → actor pipeline works.
        // vm-phase2b asserts this > 0. The gateway → client leg
        // (STDERR_NEXT rendering) depends on the Nix client's
        // verbosity and activity-context handling — not something we
        // control, so not asserted on in the VM test. The ring buffer
        // + AdminService give the authoritative log-serving path;
        // STDERR_NEXT is a convenience tail that may or may not render.
        metrics::counter!("rio_scheduler_log_lines_forwarded_total").increment(lines);
    }

    /// Collect `expected_output_paths ∪ output_paths` from all
    /// non-terminal derivations. These are the live-build roots that
    /// GC must NOT delete — either the worker is about to upload them
    /// (expected) or just did (output). Both cases: don't race the
    /// upload.
    ///
    /// Dedup via HashSet: the same drv can appear in multiple builds
    /// (shared dependency) → same expected_output_paths would be
    /// duplicated N× in the roots list. The store's mark CTE handles
    /// dups correctly, but it's wasted network + CTE work.
    fn handle_gc_roots(&self) -> Vec<String> {
        self.dag
            .iter_nodes()
            .filter(|(_, s)| !s.status().is_terminal())
            .flat_map(|(_, s)| {
                s.expected_output_paths
                    .iter()
                    .chain(s.output_paths.iter())
                    .cloned()
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Schedule reconciliation ~45s out via WeakSender. Same pattern as
    /// `schedule_terminal_cleanup`. Workers have ~45s (3× heartbeat +
    /// slack) to reconnect after scheduler restart. Any
    /// Assigned/Running derivation whose worker DIDN'T reconnect by
    /// then gets reconciled (Completed if outputs in store, else reset).
    fn schedule_reconcile_timer(&self) {
        let Some(weak_tx) = self.self_tx.clone() else {
            return;
        };
        rio_common::task::spawn_monitored("reconcile-timer", async move {
            tokio::time::sleep(RECONCILE_DELAY).await;
            if let Some(tx) = weak_tx.upgrade()
                && tx.try_send(ActorCommand::ReconcileAssignments).is_err()
            {
                tracing::warn!("reconcile command dropped (channel full)");
                metrics::counter!("rio_scheduler_reconcile_dropped_total").increment(1);
            }
        });
    }

    /// Actor in-memory snapshot of a build's derivations cross-referenced
    /// with the live stream pool. I-025 diagnostic: `executor_has_stream`
    /// is false when a derivation is Assigned to an executor whose gRPC
    /// bidi stream is gone from `self.executors` — dispatch can never
    /// complete. PG (`rio-cli workers`) may still show the executor as
    /// alive; only the actor's HashMap knows the stream is dead.
    fn handle_inspect_build_dag(
        &self,
        build_id: Uuid,
    ) -> (Vec<rio_proto::types::DerivationDiagnostic>, Vec<String>) {
        let now = std::time::Instant::now();
        let derivations = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| s.interested_builds.contains(&build_id))
            .map(|(_, s)| {
                let assigned_executor = s
                    .assigned_executor
                    .as_ref()
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                // THE I-025 signal.
                let executor_has_stream = s
                    .assigned_executor
                    .as_ref()
                    .is_some_and(|e| self.executors.contains_key(e));
                let backoff_remaining_secs = s
                    .backoff_until
                    .and_then(|deadline| deadline.checked_duration_since(now))
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                rio_proto::types::DerivationDiagnostic {
                    drv_path: s.drv_path().to_string(),
                    drv_hash: s.drv_hash.to_string(),
                    status: format!("{:?}", s.status()),
                    is_fod: s.is_fixed_output,
                    assigned_executor,
                    executor_has_stream,
                    retry_count: s.retry_count,
                    infra_retry_count: s.infra_retry_count,
                    backoff_remaining_secs,
                    interested_build_count: s.interested_builds.len() as u32,
                }
            })
            .collect();
        let live_executor_ids = self.executors.keys().map(|e| e.to_string()).collect();
        (derivations, live_executor_ids)
    }

    // ----- cfg(test) debug handlers ----------------------------------

    #[cfg(test)]
    fn handle_debug_query_workers(&self) -> Vec<DebugExecutorInfo> {
        self.executors
            .values()
            .map(|w| DebugExecutorInfo {
                executor_id: w.executor_id.to_string(),
                is_registered: w.is_registered(),
                running_count: w.running_builds.len(),
                running_builds: w.running_builds.iter().map(|h| h.to_string()).collect(),
            })
            .collect()
    }

    #[cfg(test)]
    fn handle_debug_query_derivation(&self, drv_hash: &str) -> Option<DebugDerivationInfo> {
        self.dag.node(drv_hash).map(|s| DebugDerivationInfo {
            status: s.status(),
            retry_count: s.retry_count,
            assigned_executor: s.assigned_executor.as_ref().map(|w| w.to_string()),
            assigned_size_class: s.assigned_size_class.clone(),
            output_paths: s.output_paths.clone(),
            failed_builders: s.failed_builders.iter().map(|w| w.to_string()).collect(),
            failure_count: s.failure_count,
            is_ca: s.is_ca,
            ca_output_unchanged: s.ca_output_unchanged,
        })
    }

    /// Force Ready→Assigned (or Failed→Ready→Assigned) bypassing
    /// backoff + failed_builders exclusion. For retry/poison tests
    /// that need to drive multiple completion cycles without waiting
    /// for real backoff. Clears `backoff_until`.
    #[cfg(test)]
    fn handle_debug_force_assign(&mut self, drv_hash: &str, executor_id: &ExecutorId) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        // If not already Ready, try to get there. Assigned/Running →
        // reset_to_ready, Failed → transition Ready, Ready → no-op.
        let prepped = match state.status() {
            DerivationStatus::Ready => true,
            DerivationStatus::Assigned | DerivationStatus::Running => {
                state.reset_to_ready().is_ok()
            }
            DerivationStatus::Failed => state.transition(DerivationStatus::Ready).is_ok(),
            _ => false, // terminal or pre-Ready: can't force
        };
        if !prepped {
            return false;
        }
        state.backoff_until = None;
        state.assigned_executor = Some(executor_id.clone());
        let assigned = state.transition(DerivationStatus::Assigned).is_ok();
        // Add to worker's running set so subsequent complete_failure
        // finds a consistent state.
        if let Some(w) = self.executors.get_mut(executor_id) {
            w.running_builds.insert(drv_hash.into());
        }
        assigned
    }

    /// Force to Running with `running_since` backdated. Used by
    /// backstop-timeout tests (`handle_tick` checks Running +
    /// `running_since > threshold`). The cfg(test) backstop floor is
    /// 0s so any `secs_ago > 0` triggers the backstop on Tick.
    #[cfg(test)]
    fn handle_debug_backdate_running(&mut self, drv_hash: &str, secs_ago: u64) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        // Transition to Running if not already there. Assigned →
        // Running is a valid transition; Ready/Created would fail
        // (need Assigned first). DebugForceAssign → Assigned, then
        // this → Running is the typical test sequence.
        let running = match state.status() {
            DerivationStatus::Running => true,
            DerivationStatus::Assigned => state.transition(DerivationStatus::Running).is_ok(),
            _ => false,
        };
        if running {
            state.running_since = Some(backdate(secs_ago));
        }
        running
    }

    /// Backdate `submitted_at` for per-build-timeout tests.
    /// `handle_tick`'s `r[sched.timeout.per-build]` check uses
    /// `submitted_at.elapsed()` — a `std::time::Instant`, which tokio
    /// paused time cannot mock (and paused time breaks PG pool
    /// timeouts anyway).
    #[cfg(test)]
    fn handle_debug_backdate_submitted(&mut self, build_id: Uuid, secs_ago: u64) -> bool {
        let Some(build) = self.builds.get_mut(&build_id) else {
            return false;
        };
        build.submitted_at = backdate(secs_ago);
        true
    }

    /// Clear `drv_content` to simulate post-recovery state (DAG
    /// reloaded from PG, drv_content not persisted). For CA
    /// recovery-fetch tests.
    #[cfg(test)]
    fn handle_debug_clear_drv_content(&mut self, drv_hash: &str) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        state.drv_content.clear();
        true
    }

    /// Trip the cache-check circuit breaker directly. For CA
    /// cutoff-compare breaker-gate tests — bypasses the
    /// N-failing-SubmitBuild dance.
    #[cfg(test)]
    fn handle_debug_trip_breaker(&mut self, n: u32) -> bool {
        for _ in 0..n {
            let _ = self.cache_breaker.record_failure();
        }
        self.cache_breaker.is_open()
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

    /// Inject the shared `is_leader` flag. The lease task writes;
    /// `dispatch_ready` reads. Builder-style — call before
    /// `run_with_self_tx`. Default (no call) is `true` (non-K8s
    /// mode: always leader).
    pub fn with_leader_flag(mut self, is_leader: Arc<AtomicBool>) -> Self {
        self.is_leader = is_leader;
        self
    }

    /// Inject the shared generation Arc. The lease task writes
    /// via `fetch_add`; dispatch reads for WorkAssignment;
    /// ActorHandle reads for HeartbeatResponse. REPLACES the
    /// default `Arc::new(AtomicU64::new(1))` — caller initializes
    /// to 1 too so behavior is identical, but now shared.
    ///
    /// Paired with `with_leader_flag` — both come from the same
    /// `LeaderState`. spawn_with_leader calls both.
    pub fn with_generation(mut self, generation: Arc<AtomicU64>) -> Self {
        self.generation = generation;
        self
    }

    /// Inject the shared recovery_complete flag. The actor's
    /// `handle_leader_acquired` sets it; the lease loop clears it
    /// on lose. dispatch_ready gates on it. REPLACES the default
    /// `Arc::new(true)` (non-K8s mode: no recovery needed).
    ///
    /// Triad with `with_leader_flag` + `with_generation` — all
    /// three come from the same `LeaderState`.
    pub fn with_recovery_flag(mut self, recovery_complete: Arc<AtomicBool>) -> Self {
        self.recovery_complete = recovery_complete;
        self
    }

    /// Test-only: install a oneshot gate pair for deterministic
    /// interleaving in `handle_leader_acquired`. See the field doc.
    #[cfg(test)]
    pub fn with_recovery_toctou_gate(
        mut self,
        reached_tx: oneshot::Sender<()>,
        release_rx: oneshot::Receiver<()>,
    ) -> Self {
        self.recovery_toctou_gate = Some((reached_tx, release_rx));
        self
    }

    /// Inject the shutdown token from `shutdown_signal()`. The run
    /// loop `select!`s on `token.cancelled()` with `biased` ordering
    /// so SIGTERM drains workers immediately. REPLACES the default
    /// never-cancelled token from `new()`. Only `spawn_with_leader`
    /// (production path) calls this; test actors keep the default.
    pub fn with_shutdown_token(mut self, token: rio_common::signal::Token) -> Self {
        self.shutdown = token;
        self
    }

    /// Compute counts for `AdminService.ClusterStatus`.
    ///
    /// O(workers + builds + dag_nodes) per call. The autoscaler polls
    /// every 30s; even with 10k active derivations that's ~300μs/call —
    /// not worth maintaining incremental counters. Revisit if dashboards
    /// start polling at 1Hz.
    ///
    /// `as u32` casts: if any collection exceeds 4B entries, truncation
    /// is the LEAST of our problems. The `ready_queue.len()` is bounded
    /// by `ACTOR_CHANNEL_CAPACITY × derivations_per_submit` anyway (you
    /// can't enqueue what you can't merge).
    fn compute_cluster_snapshot(&self) -> ClusterSnapshot {
        let mut active_executors = 0u32;
        let mut draining_executors = 0u32;
        // Single pass: registered ∧ ¬draining → active. draining →
        // draining (regardless of registered — a draining worker that
        // lost its stream mid-drain is still "draining" for the
        // controller's "how many pods are shutting down" question).
        for w in self.executors.values() {
            if w.draining {
                draining_executors += 1;
            } else if w.is_registered() {
                active_executors += 1;
            }
        }

        let mut pending_builds = 0u32;
        let mut active_builds = 0u32;
        for b in self.builds.values() {
            match b.state() {
                BuildState::Pending => pending_builds += 1,
                BuildState::Active => active_builds += 1,
                // Terminal builds stay in the map until CleanupTerminalBuild
                // (delayed ~30s). Don't count them — they're not "active"
                // in any autoscaling sense.
                BuildState::Succeeded | BuildState::Failed | BuildState::Cancelled => {}
            }
        }

        // Running = Assigned | Running. Both mean "a worker slot is taken."
        // Assigned hasn't acked yet but the slot is reserved; for "how
        // busy are workers" they're equivalent.
        let running_derivations = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            })
            .count() as u32;

        ClusterSnapshot {
            total_executors: self.executors.len() as u32,
            active_executors,
            draining_executors,
            pending_builds,
            active_builds,
            queued_derivations: self.ready_queue.len() as u32,
            running_derivations,
        }
    }

    /// Compute per-size-class snapshot for `GetSizeClassStatus`.
    ///
    /// Three passes:
    /// 1. `size_classes.read()` → effective cutoffs (post-rebalancer)
    /// 2. `configured_cutoffs` lookup → static TOML cutoffs
    /// 3. Single `iter_nodes()` pass: for each derivation, increment
    ///    the appropriate class's `queued` or `running` counter.
    ///
    /// For **queued**: Ready-status derivations. They haven't been
    /// dispatched yet so `assigned_size_class` is None. We call
    /// `classify()` with the SAME inputs dispatch would use
    /// (est_duration + estimator peaks) — this is a forecast, not a
    /// fact. If the rebalancer shifts cutoffs between this call and
    /// actual dispatch, the class may differ. Acceptable for an
    /// operator view.
    ///
    /// For **running**: Assigned/Running derivations. Use
    /// `assigned_size_class` directly — that's the class we ACTUALLY
    /// routed to (may be larger than classify() would give if we
    /// overflowed due to no capacity in the target class).
    ///
    /// O(dag_nodes + n_classes) per call. The classify() inside the
    /// loop is O(n_classes) but n_classes is ~3-5. Total cost is
    /// dominated by the node iteration, same as `compute_cluster_snapshot`.
    // pub(crate) for the configured-vs-effective test (tests/misc.rs)
    // which exercises it on a bare (unspawned) actor so it can mutate
    // size_classes directly to simulate a rebalancer pass.
    pub(crate) fn compute_size_class_snapshot(&self) -> Vec<SizeClassSnapshot> {
        // Take a read lock for the whole computation. Rebalancer
        // writes hourly; contention is near-zero. Dropped at end of
        // scope (no await in this fn).
        let classes = self.size_classes.read();
        if classes.is_empty() {
            // Feature off — return empty. Handler maps to empty
            // response which the CLI can render as "size-class
            // routing disabled."
            return Vec::new();
        }

        // name → index into `snapshots`. classify() and
        // assigned_size_class both return names, not indices.
        let mut index: HashMap<String, usize> = HashMap::with_capacity(classes.len());
        let mut snapshots: Vec<SizeClassSnapshot> = classes
            .iter()
            .enumerate()
            .map(|(i, c)| {
                index.insert(c.name.clone(), i);
                // configured_cutoffs lookup: linear scan is fine for
                // ~3-5 classes. Falls back to effective if not found
                // (shouldn't happen — both populated from the same
                // config in with_size_classes, but defensive against
                // a future config-reload path that forgets one).
                let configured = self
                    .configured_cutoffs
                    .iter()
                    .find(|(n, _)| n == &c.name)
                    .map(|(_, cut)| *cut)
                    .unwrap_or(c.cutoff_secs);
                SizeClassSnapshot {
                    name: c.name.clone(),
                    effective_cutoff_secs: c.cutoff_secs,
                    configured_cutoff_secs: configured,
                    queued: 0,
                    running: 0,
                }
            })
            .collect();

        // Single pass: classify or look up per derivation.
        for (_, state) in self.dag.iter_nodes() {
            match state.status() {
                DerivationStatus::Ready => {
                    // Forecast: what class WOULD dispatch pick?
                    // Same inputs as dispatch.rs:82-92 — est_duration
                    // stored on the state at merge time; peak_memory
                    // / peak_cpu from the estimator.
                    if let Some(class) = crate::assignment::classify(
                        state.est_duration,
                        self.estimator
                            .peak_memory(state.pname.as_deref(), &state.system),
                        self.estimator
                            .peak_cpu(state.pname.as_deref(), &state.system),
                        &classes,
                    ) && let Some(&i) = index.get(&class)
                    {
                        snapshots[i].queued += 1;
                    }
                }
                DerivationStatus::Assigned | DerivationStatus::Running => {
                    // Fact: what class DID we dispatch to?
                    // assigned_size_class reflects overflow — if the
                    // target was "small" but only "large" had
                    // capacity, this says "large". That's the
                    // operator-relevant answer for "where are my
                    // workers busy?"
                    if let Some(class) = &state.assigned_size_class
                        && let Some(&i) = index.get(class)
                    {
                        snapshots[i].running += 1;
                    }
                }
                // Terminal + pre-Ready: neither queued nor running.
                _ => {}
            }
        }

        // Sort by effective cutoff ascending — smallest class first.
        // The proto doc says "sorted by effective_cutoff_secs";
        // consumers (P0236's CLI table, P0234's autoscaler) can rely
        // on this order. total_cmp for NaN-safety (same defense as
        // assignment.rs:106).
        snapshots.sort_by(|a, b| a.effective_cutoff_secs.total_cmp(&b.effective_cutoff_secs));
        snapshots
    }

    /// Bucketed resource estimates for `GetCapacityManifest` (ADR-020).
    ///
    /// Iterates DAG nodes filtered to `Ready` status — same set
    /// `ready_queue.len()` counts for `queued_derivations`. For each:
    /// look up `(pname, system)` in the estimator, apply headroom,
    /// bucket to 4GiB/2000mcore.
    ///
    /// Omissions (controller uses its operator-configured floor for
    /// each missing estimate):
    /// - `pname` is `None`: no key for `build_history` lookup
    /// - No history entry: cold start (never built before)
    /// - No memory sample: `bucketed_estimate` returns `None`
    pub(crate) fn compute_capacity_manifest(&self) -> Vec<BucketedEstimate> {
        let mut out = Vec::new();
        for (_, state) in self.dag.iter_nodes() {
            if state.status() != DerivationStatus::Ready {
                continue;
            }
            let Some(pname) = state.pname.as_deref() else {
                continue;
            };
            let Some(entry) = self.estimator.lookup_entry(pname, &state.system) else {
                continue;
            };
            if let Some(b) = Estimator::bucketed_estimate(&entry, self.headroom_mult) {
                out.push(b);
            }
        }
        out
    }

    /// Test-only: inject a derivation directly into the DAG at `Ready`
    /// status. Bypasses MergeDag + PG persist. For populating
    /// `compute_capacity_manifest` preconditions without the full
    /// merge-proto-node dance.
    #[cfg(test)]
    pub(crate) fn test_inject_ready(&mut self, hash: &str, pname: Option<&str>, system: &str) {
        let row = crate::db::RecoveryDerivationRow {
            derivation_id: uuid::Uuid::new_v4(),
            drv_hash: hash.to_string(),
            drv_path: rio_test_support::fixtures::test_drv_path(hash),
            pname: pname.map(String::from),
            system: system.to_string(),
            status: "ready".into(),
            required_features: vec![],
            assigned_builder_id: None,
            retry_count: 0,
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            is_ca: false,
            failed_builders: vec![],
        };
        let state = crate::state::DerivationState::from_recovery_row(row, DerivationStatus::Ready)
            .expect("test_drv_path generates valid StorePath");
        self.dag.insert_recovered_node(state);
    }

    /// Test-only: seed the Estimator directly, bypassing the Tick
    /// refresh path. Pairs with [`Self::test_inject_ready`].
    #[cfg(test)]
    pub(crate) fn test_refresh_estimator(&mut self, rows: Vec<crate::db::BuildHistoryRow>) {
        self.estimator.refresh(rows);
    }

    // -----------------------------------------------------------------------
    // Shared helpers (used across merge/completion/dispatch/build submodules)
    // -----------------------------------------------------------------------

    fn emit_build_event(&mut self, build_id: Uuid, event: rio_proto::types::build_event::Event) {
        use rio_proto::types::build_event::Event;

        let seq = self.build_sequences.entry(build_id).or_insert(0);
        *seq += 1;
        let seq = *seq;

        let build_event = rio_proto::types::BuildEvent {
            build_id: build_id.to_string(),
            sequence: seq,
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            event: Some(event),
        };

        // Persist to PG for since_sequence replay. BEFORE the
        // broadcast: the prost encode borrows build_event, and the
        // broadcast consumes it. Ordering doesn't matter for
        // correctness (the persister is a separate FIFO task; a
        // watcher that subscribes between try_send and tx.send below
        // still sees this event via broadcast).
        //
        // Event::Log filtered: ~20/sec under a chatty rustc would
        // flood PG. Log lines are already durable via S3 (the
        // LogFlusher, same pattern). Gateway reconnect cares about
        // state-machine events (Started/Completed/Derivation*), not
        // log lines — those it re-fetches from S3.
        if let Some(tx) = &self.event_persist_tx
            && !matches!(build_event.event, Some(Event::Log(_)))
        {
            use prost::Message;
            let bytes = build_event.encode_to_vec();
            if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send((build_id, seq, bytes)) {
                // Persister backed up (PG slow/down). The broadcast
                // below still carries the event to live watchers;
                // only a mid-backlog reconnect loses it. 1000 events
                // of backlog = ~200s at steady-state — if we're
                // here, PG is probably unreachable anyway.
                metrics::counter!("rio_scheduler_event_persist_dropped_total").increment(1);
            }
            // Closed variant: persister task died. Don't spam the
            // metric — spawn_monitored already logged the panic.
        }

        if let Some(tx) = self.build_events.get(&build_id) {
            // broadcast::send returns Err only if there are no receivers, which is fine
            let _ = tx.send(build_event);
        }
    }

    /// Emit a BuildProgress snapshot for a build.
    ///
    /// Computes fresh counts + critpath + workers via `build_summary()`
    /// (one O(nodes) pass). Call after state changes that affect the
    /// aggregate view — dispatch (running count + worker set changed)
    /// and completion (completed count changed + critpath dropped via
    /// `update_ancestors`). NOT called from recovery (recovery
    /// rebuilds state but watchers replay from PG event log; emitting
    /// here would inject a spurious event into the sequence).
    ///
    /// Why a separate event (not folding into DerivationEvent): the
    /// dashboard wants a single ETA number it can display without
    /// tracking state. Pushing the aggregate means the client stays
    /// dumb — no stateful reconstruction from the DerivationEvent
    /// stream.
    pub(super) fn emit_progress(&mut self, build_id: Uuid) {
        let summary = self.dag.build_summary(build_id);
        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Progress(rio_proto::types::BuildProgress {
                completed: summary.completed,
                running: summary.running,
                queued: summary.queued,
                total: summary.total,
                critical_path_remaining_secs: Some(summary.critpath_remaining.round() as u64),
                assigned_executors: summary.assigned_executors,
            }),
        );
    }

    fn get_interested_builds(&self, drv_hash: &DrvHash) -> Vec<Uuid> {
        self.dag
            .node(drv_hash)
            .map(|s| s.interested_builds.iter().copied().collect())
            .unwrap_or_default()
    }

    fn drv_hash_to_path(&self, drv_hash: &DrvHash) -> Option<String> {
        self.dag.node(drv_hash).map(|s| s.drv_path().to_string())
    }

    /// Resolve drv_hash → drv_path, falling back to the hash string
    /// if the node isn't in the DAG. Used for `derivation_path`
    /// fields in BuildEvents — better to emit SOMETHING (the hash
    /// is still a useful identifier) than empty. Extracted from
    /// three duplicate call sites (dispatch.rs + completion.rs ×2).
    pub(super) fn drv_path_or_hash_fallback(&self, drv_hash: &DrvHash) -> String {
        self.drv_hash_to_path(drv_hash).unwrap_or_else(|| {
            warn!(
                drv_hash = %drv_hash,
                "drv_hash_to_path returned None; using hash as fallback"
            );
            drv_hash.to_string()
        })
    }

    /// Whether any interested build for this derivation is interactive (IFD).
    /// Interactive derivations get a priority boost in the queue.
    fn should_prioritize(&self, drv_hash: &DrvHash) -> bool {
        self.get_interested_builds(drv_hash).iter().any(|build_id| {
            self.builds
                .get(build_id)
                .is_some_and(|b| b.priority_class.is_interactive())
        })
    }

    /// Compute the effective queue priority for a derivation: its
    /// critical-path priority + interactive boost if applicable.
    ///
    /// All queue pushes go through this. Replaces the old `push_front`/
    /// `push_back` split — interactive is now a number, not a position.
    ///
    /// Returns 0.0 if the node isn't in the DAG (stale hash). The
    /// caller probably shouldn't be pushing it, but 0.0 = lowest
    /// priority = harmless (stale entries get skipped on pop anyway
    /// if status != Ready).
    fn queue_priority(&self, drv_hash: &DrvHash) -> f64 {
        let base = self.dag.node(drv_hash).map(|n| n.priority).unwrap_or(0.0);
        if self.should_prioritize(drv_hash) {
            base + crate::queue::INTERACTIVE_BOOST
        } else {
            base
        }
    }

    /// Push a derivation onto the ready queue with its computed priority.
    /// Centralizes the priority lookup so call sites are simple.
    fn push_ready(&mut self, drv_hash: DrvHash) {
        let prio = self.queue_priority(&drv_hash);
        self.ready_queue.push(drv_hash, prio);
    }

    /// Resolve a drv_path to its drv_hash via the DAG's reverse index.
    /// Used by handle_completion since the gRPC layer receives CompletionReport
    /// with drv_path, but the DAG is keyed by drv_hash.
    fn drv_path_to_hash(&self, drv_path: &str) -> Option<DrvHash> {
        self.dag.hash_for_path(drv_path).cloned()
    }

    fn find_db_id_by_path(&self, drv_path: &str) -> Option<Uuid> {
        self.dag
            .hash_for_path(drv_path)
            .and_then(|h| self.dag.node(h))
            .and_then(|s| s.db_id)
    }

    /// Fire a log-flush request for the given derivation. No-op if the
    /// flusher isn't configured (tests, or `RIO_LOG_S3_BUCKET` unset).
    ///
    /// `try_send`: if the flusher channel is full (shouldn't happen — 1000
    /// cap and the flusher's S3 PUT latency is sub-second), drop silently.
    /// The 30s periodic tick will still snapshot until CleanupTerminalBuild.
    ///
    /// Called from `handle_completion_success` AND `handle_permanent_failure`
    /// — both paths flush because failed builds still have useful logs.
    /// NOT called from `handle_transient_failure`: the derivation gets
    /// re-queued, a new worker builds it from scratch, and that worker's
    /// logs replace the partial ones. The ring buffer gets `discard()`ed
    /// by the BuildExecution recv task on worker disconnect.
    fn trigger_log_flush(&self, drv_hash: &DrvHash, interested_builds: Vec<Uuid>) {
        let Some(tx) = &self.log_flush_tx else {
            return;
        };
        let Some(drv_path) = self.drv_hash_to_path(drv_hash) else {
            // Should be impossible at this call site (completion handlers
            // already validated the hash exists in the DAG), but defensive.
            warn!(drv_hash = %drv_hash, "trigger_log_flush: hash not in DAG, skipping");
            return;
        };
        let req = crate::logs::FlushRequest {
            drv_path,
            drv_hash: drv_hash.clone(),
            interested_builds,
        };
        if tx.try_send(req).is_err() {
            warn!(
                drv_hash = %drv_hash,
                "log flush channel full, dropped; periodic tick will snapshot"
            );
            metrics::counter!("rio_scheduler_log_flush_dropped_total").increment(1);
        }
    }
}

mod breaker;
mod build;
mod completion;
mod dispatch;
mod executor;
mod handle;
mod merge;

pub(super) use breaker::CacheCheckBreaker;
#[cfg(test)]
pub(crate) use executor::compute_initial_prefetch_paths;
pub use handle::ActorHandle;
#[cfg(test)]
pub(crate) use handle::{DebugDerivationInfo, DebugExecutorInfo};

#[cfg(test)]
pub(crate) mod tests;
