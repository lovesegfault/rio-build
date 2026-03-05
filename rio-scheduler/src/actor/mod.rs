//! DAG actor: single Tokio task owning all mutable scheduler state.
//!
//! All gRPC handlers communicate with the actor via an mpsc command channel.
//! The actor processes commands serially, ensuring deterministic ordering
//! and eliminating lock contention.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, oneshot};
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
    BuildInfo, BuildOptions, BuildState, DerivationStatus, DrvHash, HEARTBEAT_TIMEOUT_SECS,
    MAX_MISSED_HEARTBEATS, POISON_THRESHOLD, POISON_TTL, PriorityClass, RetryPolicy, WorkerId,
    WorkerState,
};

/// Channel capacity for the actor command channel.
pub const ACTOR_CHANNEL_CAPACITY: usize = 10_000;

/// Backpressure: reject new work above this fraction of channel capacity.
const BACKPRESSURE_HIGH_WATERMARK: f64 = 0.80;

/// Backpressure: resume accepting work below this fraction.
const BACKPRESSURE_LOW_WATERMARK: f64 = 0.60;

/// Number of events to retain in each build's event buffer for late subscribers.
const BUILD_EVENT_BUFFER_SIZE: usize = 1024;

/// Delay before cleaning up terminal build state. Allows late WatchBuild
/// subscribers to receive the terminal event before the broadcast sender
/// is dropped.
const TERMINAL_CLEANUP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Request payload for [`ActorCommand::MergeDag`].
#[derive(Debug)]
pub struct MergeDagRequest {
    pub build_id: Uuid,
    pub tenant_id: Option<String>,
    pub priority_class: PriorityClass,
    pub nodes: Vec<rio_proto::types::DerivationNode>,
    pub edges: Vec<rio_proto::types::DerivationEdge>,
    pub options: BuildOptions,
    pub keep_going: bool,
}

/// Commands sent to the DAG actor.
pub enum ActorCommand {
    /// Merge a new build's derivation DAG into the global graph.
    MergeDag {
        req: MergeDagRequest,
        reply:
            oneshot::Sender<Result<broadcast::Receiver<rio_proto::types::BuildEvent>, ActorError>>,
    },

    /// Process a completion report from a worker.
    ProcessCompletion {
        worker_id: WorkerId,
        /// Either a drv_hash OR a full drv_path — handle_completion resolves both.
        /// Workers send drv_path; tests sometimes send drv_hash directly.
        drv_key: String,
        result: rio_proto::types::BuildResult,
        /// Peak memory (VmHWM) in bytes from the worker. 0 = no signal
        /// (proc gone, build failed early). Feeds build_history EMA.
        peak_memory_bytes: u64,
        /// Sum of uploaded NAR sizes. 0 = no signal (build failed, no
        /// outputs). Dashboards-only today; column exists so EMA it now.
        output_size_bytes: u64,
    },

    /// Cancel a build.
    CancelBuild {
        build_id: Uuid,
        reason: String,
        reply: oneshot::Sender<Result<bool, ActorError>>,
    },

    /// A worker opened a BuildExecution stream.
    WorkerConnected {
        worker_id: WorkerId,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
    },

    /// A worker's BuildExecution stream closed.
    WorkerDisconnected { worker_id: WorkerId },

    /// Periodic heartbeat from a worker.
    Heartbeat {
        worker_id: WorkerId,
        system: String,
        supported_features: Vec<String>,
        max_builds: u32,
        /// drv_paths from worker proto (not hashes).
        running_builds: Vec<String>,
        /// Parsed bloom filter from local_paths. `None` = worker didn't
        /// send one (old worker, or FUSE not mounted). Stored on
        /// WorkerState for assignment scoring.
        bloom: Option<rio_common::bloom::BloomFilter>,
        /// Size-class from worker config (e.g. "small", "large"). gRPC
        /// maps empty-string → None. Stored on WorkerState for D7's
        /// classify() → best_worker() filter.
        size_class: Option<String>,
    },

    /// Periodic tick for housekeeping (timeouts, poison TTL expiry).
    Tick,

    /// Query build status.
    QueryBuildStatus {
        build_id: Uuid,
        reply: oneshot::Sender<Result<rio_proto::types::BuildStatus, ActorError>>,
    },

    /// Subscribe to an existing build's events.
    WatchBuild {
        build_id: Uuid,
        since_sequence: u64,
        reply:
            oneshot::Sender<Result<broadcast::Receiver<rio_proto::types::BuildEvent>, ActorError>>,
    },

    /// Internal: clean up terminal build state (maps + DAG interest) after
    /// a delay. Scheduled by complete_build/transition_build_to_failed/cancel.
    CleanupTerminalBuild { build_id: Uuid },

    /// Forward a log batch to interested gateways via `emit_build_event`.
    ///
    /// Sent by the BuildExecution recv task via `try_send` (NOT
    /// `send_unchecked`) — under backpressure this drops, which is
    /// intentional: the ring buffer (written directly by the recv task,
    /// not through here) still has the lines, so AdminService.GetBuildLogs
    /// can serve them even if the live gateway feed missed some.
    /// Fire-and-forget; no reply.
    ///
    /// `drv_path` not `drv_hash` because that's what BuildLogBatch carries.
    /// The actor resolves it via `drv_path_to_hash` (DAG reverse index).
    ForwardLogBatch {
        drv_path: String,
        batch: rio_proto::types::BuildLogBatch,
    },

    /// Mark a worker draining: stop sending new assignments.
    ///
    /// Called by the worker itself (step 1 of SIGTERM preStop — D3) or
    /// by the controller (WorkerPool finalizer — F6). Idempotent: an
    /// already-draining worker replies `accepted=true` with the same
    /// running count. Unknown worker → `accepted=false, running=0`
    /// (not an error; the worker may have already disconnected).
    ///
    /// `force=true` additionally reassigns in-flight builds (same path
    /// as `WorkerDisconnected`). Use case: operator-initiated forced
    /// drain when the worker is unhealthy but still heartbeating —
    /// don't wait 2h for builds to complete, reassign now. The
    /// worker's builds will still run to completion on the worker
    /// (nix-daemon is already spawned) but the scheduler stops caring
    /// about the result and re-dispatches elsewhere. Wasteful but
    /// correct: deterministic builds, same output either way.
    ///
    /// `send_unchecked`: same reasoning as Heartbeat. A drain request
    /// MUST land — dropping it under backpressure would leave a
    /// shutting-down worker accepting new assignments right when
    /// capacity is shrinking. That's a feedback loop into MORE load.
    DrainWorker {
        worker_id: WorkerId,
        force: bool,
        reply: oneshot::Sender<DrainResult>,
    },

    /// Snapshot cluster state for `AdminService.ClusterStatus`.
    ///
    /// Counts are computed by iterating actor-owned collections
    /// (`workers`, `builds`, `ready_queue`, `dag`). That's O(n) in
    /// workers+derivations per call — fine for the controller's 30s
    /// autoscaling poll. If this ever gets hot (dashboard refresh at
    /// 1Hz × many clients), the actor could maintain running counters
    /// incremented on state transitions instead. Not a concern today.
    ///
    /// `send_unchecked` — bypasses backpressure like Heartbeat. The
    /// controller's autoscaling loop needs a reading even (especially!)
    /// when the scheduler is saturated; dropping the snapshot under
    /// backpressure would blind the autoscaler exactly when it needs
    /// to scale up.
    ClusterSnapshot {
        reply: oneshot::Sender<ClusterSnapshot>,
    },

    /// Test-only: query worker states.
    #[cfg(test)]
    DebugQueryWorkers {
        reply: oneshot::Sender<Vec<DebugWorkerInfo>>,
    },

    /// Test-only: query a derivation's state.
    #[cfg(test)]
    DebugQueryDerivation {
        drv_hash: String,
        reply: oneshot::Sender<Option<DebugDerivationInfo>>,
    },
}

/// Reply for `ActorCommand::DrainWorker`.
///
/// `accepted=false` only for unknown worker_id — NOT an error, the
/// worker may have disconnected (preStop races with stream close on
/// SIGTERM). Caller treats `accepted=false, running=0` as "nothing to
/// wait for, proceed."
#[derive(Debug, Clone, Copy)]
pub struct DrainResult {
    pub accepted: bool,
    /// Builds still in-flight on the worker after drain. For
    /// `force=false`, these will complete normally. For `force=true`,
    /// this is 0 (reassigned). The worker's preStop hook uses this to
    /// decide whether to wait.
    pub running_builds: u32,
}

/// Point-in-time cluster state counts for `AdminService.ClusterStatus`.
///
/// Internal (not proto) so the actor doesn't depend on proto-type
/// construction details. `admin.rs` translates. All `u32` — a cluster
/// with >4B workers would have other problems first.
///
/// NOT `Copy`: `u32 × 6` is 24 bytes, comfortably `Copy`-sized, but
/// the reply oneshot MOVES it anyway so Copy gains nothing. Derive
/// conservatively; adding a field later that isn't Copy (e.g.,
/// per-class queue depth Vec) would be a silent semantic break if
/// callers had started relying on implicit copies.
#[derive(Debug, Clone, Default)]
pub struct ClusterSnapshot {
    /// `workers.len()`. Includes unregistered (stream-only or
    /// heartbeat-only) and draining.
    pub total_workers: u32,
    /// `is_registered() && !draining`. The dispatchable population.
    /// E2 adds the draining flag; until then this equals registered.
    pub active_workers: u32,
    /// `draining` flag set. 0 until E2.
    pub draining_workers: u32,
    /// `BuildState::Pending` — merged but not yet active.
    pub pending_builds: u32,
    /// `BuildState::Active` — at least one derivation dispatched.
    pub active_builds: u32,
    /// `ready_queue.len()`. Ready-to-dispatch derivations waiting for
    /// worker capacity. This is the autoscaling input signal.
    pub queued_derivations: u32,
    /// `DerivationStatus::{Assigned|Running}` across the DAG. Workers
    /// currently occupied.
    pub running_derivations: u32,
}

/// Errors from the actor.
#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("build not found: {0}")]
    BuildNotFound(Uuid),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("channel send error")]
    ChannelSend,

    #[error("backpressure: actor queue is overloaded")]
    Backpressure,

    #[error("DAG merge failed: {0}")]
    Dag(#[from] crate::dag::DagError),

    /// Invariant violation: an edge references a derivation that was never
    /// persisted to PG. Merge assigns db_ids to every node before processing
    /// edges; if `find_db_id_by_path` returns None for an edge endpoint, the
    /// node was never in the submission (malformed request) or the id_map
    /// build loop has a bug.
    #[error("edge references unpersisted derivation (db_id missing): {drv_path}")]
    MissingDbId { drv_path: String },

    /// Store service is unreachable (cache-check circuit breaker is open).
    /// Maps to gRPC UNAVAILABLE. Rejecting SubmitBuild here is the user
    /// decision from phase2c planning: if the store is down, builds can't
    /// dispatch anyway (workers PutPath/GetPath also fail), so fail fast
    /// with a clear error instead of queueing builds that will all stall.
    #[error("store service unavailable (cache-check circuit breaker open)")]
    StoreUnavailable,
}

/// Read-only view of the actor's backpressure state.
///
/// Only the actor can toggle backpressure (it computes the 80%/60% hysteresis);
/// handles can only observe. Wrapping `Arc<AtomicBool>` makes the read-only
/// invariant compile-time: there's no `store()` method on this type.
#[derive(Clone)]
pub struct BackpressureReader(Arc<AtomicBool>);

impl BackpressureReader {
    fn new(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    /// Whether backpressure is currently active (hysteresis-aware).
    pub fn is_active(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Test-only: simulate the actor toggling backpressure.
    /// In production, only the actor's update_backpressure writes this.
    #[cfg(test)]
    pub(crate) fn set_for_test(&self, active: bool) {
        self.0.store(active, Ordering::Relaxed);
    }
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
    workers: HashMap<WorkerId, WorkerState>,
    /// Retry policy.
    retry_policy: RetryPolicy,
    /// Database handle.
    db: SchedulerDb,
    /// Store service client for scheduler-side cache checks. `None` in tests
    /// that don't need the store (cache check is then skipped).
    store_client: Option<StoreServiceClient<Channel>>,
    /// Circuit breaker for the cache-check FindMissingPaths call. Owned by
    /// the actor (single-threaded, no lock needed). Checked/updated in
    /// `merge.rs::check_cached_outputs`.
    cache_breaker: CacheCheckBreaker,
    /// Build duration estimator. Snapshot of `build_history`, refreshed
    /// periodically on Tick. D4 (critical-path) and D7 (size-class) read
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
    /// Leader generation counter (for assignment tokens).
    generation: i64,
    /// Weak clone of the actor's own command sender, for scheduling delayed
    /// internal commands (e.g., terminal build cleanup). Weak so the actor
    /// doesn't prevent channel close when all external handles are dropped.
    /// `None` if spawned via bare `run()` (no delayed scheduling).
    self_tx: Option<mpsc::WeakSender<ActorCommand>>,
    /// Size-class cutoff config. Empty = feature off (no classification).
    /// dispatch.rs calls classify() with this; completion.rs looks up
    /// cutoff_for() for misclassification detection.
    size_classes: Vec<crate::assignment::SizeClassConfig>,
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
            workers: HashMap::new(),
            retry_policy: RetryPolicy::default(),
            db,
            store_client,
            cache_breaker: CacheCheckBreaker::default(),
            estimator: Estimator::default(),
            tick_count: 0,
            backpressure_active: Arc::new(AtomicBool::new(false)),
            generation: 1,
            self_tx: None,
            size_classes: Vec::new(),
            log_flush_tx: None,
        }
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
        self.size_classes = classes;
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

        while let Some(cmd) = rx.recv().await {
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
                    worker_id,
                    drv_key,
                    result,
                    peak_memory_bytes,
                    output_size_bytes,
                } => {
                    self.handle_completion(
                        &worker_id,
                        &drv_key,
                        result,
                        peak_memory_bytes,
                        output_size_bytes,
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
                ActorCommand::WorkerConnected {
                    worker_id,
                    stream_tx,
                } => {
                    self.handle_worker_connected(&worker_id, stream_tx);
                }
                ActorCommand::WorkerDisconnected { worker_id } => {
                    self.handle_worker_disconnected(&worker_id).await;
                }
                ActorCommand::Heartbeat {
                    worker_id,
                    system,
                    supported_features,
                    max_builds,
                    running_builds,
                    bloom,
                    size_class,
                } => {
                    self.handle_heartbeat(
                        &worker_id,
                        system,
                        supported_features,
                        max_builds,
                        running_builds,
                        (bloom, size_class),
                    );
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
                    // TODO(phase3a): since_sequence is for resuming after gateway reconnect
                    // to a new leader. Phase 2a has no leader election; the broadcast
                    // channel's 1024-event buffer provides limited late-subscriber replay.
                    // Full resumption requires persistent event log + leader handoff.
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
                ActorCommand::DrainWorker {
                    worker_id,
                    force,
                    reply,
                } => {
                    let result = self.handle_drain_worker(&worker_id, force).await;
                    let _ = reply.send(result);
                }
                ActorCommand::ForwardLogBatch { drv_path, batch } => {
                    // Resolve drv_path → drv_hash → interested_builds, then
                    // emit BuildEvent::Log on each build's broadcast channel.
                    // The gateway already handles Event::Log (handler/build.rs
                    // :27-32) — it translates to STDERR_NEXT for the Nix client.
                    //
                    // Unknown drv_path → drop silently. Two legitimate cases:
                    // (a) batch arrived after CleanupTerminalBuild removed the
                    //     DAG entry (race between worker stream and actor loop
                    //     — the build is done, gateway already saw Completed,
                    //     late log lines are irrelevant);
                    // (b) malformed batch from a buggy worker. Neither warrants
                    //     a warn!() — (a) is expected, (b) would spam.
                    if let Some(hash) = self.drv_path_to_hash(&drv_path) {
                        let lines = batch.lines.len() as u64;
                        for build_id in self.get_interested_builds(&hash) {
                            // batch.clone(): BuildLogBatch has Vec<Vec<u8>> so
                            // this is a deep copy. For 64 lines × 100 bytes
                            // that's ~6.5KB × N interested builds. Typically
                            // N=1 (one gateway per build). If profiling ever
                            // shows this hot, Arc<BuildLogBatch> in BuildEvent.
                            self.emit_build_event(
                                build_id,
                                rio_proto::types::build_event::Event::Log(batch.clone()),
                            );
                        }
                        // Metric: proves worker → scheduler → actor pipeline
                        // works. vm-phase2b asserts this > 0. The gateway →
                        // client leg (STDERR_NEXT rendering) depends on the
                        // Nix client's verbosity and activity-context handling
                        // — not something we control, so not asserted on in
                        // the VM test. The ring buffer + AdminService give
                        // the authoritative log-serving path; STDERR_NEXT is
                        // a convenience tail that may or may not render.
                        metrics::counter!("rio_scheduler_log_lines_forwarded_total")
                            .increment(lines);
                    }
                }
                #[cfg(test)]
                ActorCommand::DebugQueryWorkers { reply } => {
                    let workers: Vec<_> = self
                        .workers
                        .values()
                        .map(|w| DebugWorkerInfo {
                            worker_id: w.worker_id.to_string(),
                            is_registered: w.is_registered(),
                            system: w.system.clone(),
                            running_count: w.running_builds.len(),
                            running_builds: w
                                .running_builds
                                .iter()
                                .map(|h| h.to_string())
                                .collect(),
                        })
                        .collect();
                    let _ = reply.send(workers);
                }
                #[cfg(test)]
                ActorCommand::DebugQueryDerivation { drv_hash, reply } => {
                    let info = self.dag.node(&drv_hash).map(|s| DebugDerivationInfo {
                        drv_hash: s.drv_hash.to_string(),
                        drv_path: s.drv_path().to_string(),
                        status: s.status(),
                        retry_count: s.retry_count,
                        assigned_worker: s.assigned_worker.as_ref().map(|w| w.to_string()),
                        assigned_size_class: s.assigned_size_class.clone(),
                        output_paths: s.output_paths.clone(),
                    });
                    let _ = reply.send(info);
                }
            }
        }

        info!("DAG actor shutting down");
    }

    // -----------------------------------------------------------------------
    // Backpressure
    // -----------------------------------------------------------------------

    fn update_backpressure(&mut self, queue_len: usize, capacity: usize) {
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
    /// into ActorHandle. The actor keeps the writable Arc<AtomicBool>.
    pub(crate) fn backpressure_flag(&self) -> BackpressureReader {
        BackpressureReader::new(self.backpressure_active.clone())
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
        let mut active_workers = 0u32;
        let mut draining_workers = 0u32;
        // Single pass: registered ∧ ¬draining → active. draining →
        // draining (regardless of registered — a draining worker that
        // lost its stream mid-drain is still "draining" for the
        // controller's "how many pods are shutting down" question).
        for w in self.workers.values() {
            if w.draining {
                draining_workers += 1;
            } else if w.is_registered() {
                active_workers += 1;
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
            total_workers: self.workers.len() as u32,
            active_workers,
            draining_workers,
            pending_builds,
            active_builds,
            queued_derivations: self.ready_queue.len() as u32,
            running_derivations,
        }
    }

    // -----------------------------------------------------------------------
    // Shared helpers (used across merge/completion/dispatch/build submodules)
    // -----------------------------------------------------------------------

    fn emit_build_event(&mut self, build_id: Uuid, event: rio_proto::types::build_event::Event) {
        let seq = self.build_sequences.entry(build_id).or_insert(0);
        *seq += 1;

        let build_event = rio_proto::types::BuildEvent {
            build_id: build_id.to_string(),
            sequence: *seq,
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            event: Some(event),
        };

        if let Some(tx) = self.build_events.get(&build_id) {
            // broadcast::send returns Err only if there are no receivers, which is fine
            let _ = tx.send(build_event);
        }
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

    /// Whether any interested build for this derivation is interactive (IFD).
    /// Interactive derivations get a priority boost (D5).
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
    /// by the BuildExecution recv task on worker disconnect (future: C10).
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
mod handle;
mod merge;
mod worker;

pub(super) use breaker::CacheCheckBreaker;
pub use handle::ActorHandle;
#[cfg(test)]
pub(crate) use handle::{DebugDerivationInfo, DebugWorkerInfo};

#[cfg(test)]
pub(crate) mod tests;
