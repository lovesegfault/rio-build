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

use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types::FindMissingPathsRequest;

use crate::dag::DerivationDag;
use crate::db::SchedulerDb;
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

/// Test-only: snapshot of worker state for assertions.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct DebugWorkerInfo {
    pub worker_id: String,
    pub is_registered: bool,
    pub system: Option<String>,
    pub running_count: usize,
    pub running_builds: Vec<String>,
}

/// Test-only: snapshot of derivation state for assertions.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct DebugDerivationInfo {
    pub drv_hash: String,
    pub drv_path: String,
    pub status: DerivationStatus,
    pub retry_count: u32,
    pub assigned_worker: Option<String>,
    pub output_paths: Vec<String>,
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

    #[error("internal error: {0}")]
    Internal(String),

    /// Store service is unreachable (cache-check circuit breaker is open).
    /// Maps to gRPC UNAVAILABLE. Rejecting SubmitBuild here is the user
    /// decision from phase2c planning: if the store is down, builds can't
    /// dispatch anyway (workers PutPath/GetPath also fail), so fail fast
    /// with a clear error instead of queueing builds that will all stall.
    #[error("store service unavailable (cache-check circuit breaker open)")]
    StoreUnavailable,
}

/// Circuit breaker for the scheduler's cache-check FindMissingPaths call.
///
/// Without this, a sustained store outage means every SubmitBuild treats
/// every derivation as a cache miss — an avalanche of unnecessary rebuilds
/// once the store comes back (or workers thrash trying to fetch inputs).
///
/// # States
///
/// - **Closed** (normal): cache check runs, result used. Tracks consecutive
///   failures; trips open after [`OPEN_THRESHOLD`].
/// - **Open**: cache check STILL runs (half-open probe), but if it fails,
///   SubmitBuild is rejected with `StoreUnavailable` instead of queueing.
///   If the probe succeeds, the breaker closes and the result is used.
///   Auto-closes after [`OPEN_DURATION`] even without a successful probe
///   (so a transient store blip doesn't lock us out forever if no builds
///   arrive to probe with).
///
/// # Why half-open probe, not skip-the-call
///
/// If we skipped the cache check entirely while open, the breaker could
/// only close via timeout. A successful probe is a faster, more responsive
/// signal that the store is back. The probe costs one RPC per SubmitBuild
/// — same as the closed state — so there's no extra load.
#[derive(Debug, Default)]
pub(super) struct CacheCheckBreaker {
    /// Consecutive cache-check failures. Reset to 0 on any success.
    consecutive_failures: u32,
    /// If `Some`, the breaker is open until this instant. `None` = closed.
    open_until: Option<Instant>,
}

/// Trip open after this many consecutive failures.
const OPEN_THRESHOLD: u32 = 5;

/// Stay open for this long before auto-closing (even without a probe success).
const OPEN_DURATION: std::time::Duration = std::time::Duration::from_secs(30);

impl CacheCheckBreaker {
    /// Record a failure. Returns `true` if this failure trips the breaker open
    /// (or it was already open) — caller should reject SubmitBuild.
    ///
    /// The "already open" case matters: while open, we STILL attempt the cache
    /// check (half-open probe). A failed probe keeps us open but doesn't
    /// re-increment the open-transition metric (it's the same outage).
    pub(super) fn record_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        // Already open: stay open. No metric (same outage, not a new trip).
        if self.is_open() {
            return true;
        }

        // Trip open on threshold crossing.
        if self.consecutive_failures >= OPEN_THRESHOLD {
            self.open_until = Some(Instant::now() + OPEN_DURATION);
            warn!(
                consecutive_failures = self.consecutive_failures,
                open_for = ?OPEN_DURATION,
                "cache-check circuit breaker OPENING — rejecting SubmitBuild until store recovers"
            );
            metrics::counter!("rio_scheduler_cache_check_circuit_open_total").increment(1);
            return true;
        }

        // Still under threshold: proceed as if the cache check just missed.
        false
    }

    /// Record a success. Closes the breaker and resets the failure counter.
    pub(super) fn record_success(&mut self) {
        if self.is_open() {
            info!(
                after_failures = self.consecutive_failures,
                "cache-check circuit breaker CLOSING — store recovered"
            );
        }
        self.consecutive_failures = 0;
        self.open_until = None;
    }

    /// Whether the breaker is open RIGHT NOW. Handles timeout-based
    /// auto-close: if `open_until` is in the past, we're closed.
    ///
    /// Doesn't mutate state — the stale `open_until` is cleaned up lazily
    /// on the next `record_success()`. This keeps the check cheap (one
    /// `Instant::now()` compare) and avoids needing `&mut self` here.
    fn is_open(&self) -> bool {
        self.open_until.is_some_and(|until| Instant::now() < until)
    }
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
            backpressure_active: Arc::new(AtomicBool::new(false)),
            generation: 1,
            self_tx: None,
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
                } => {
                    self.handle_completion(&worker_id, &drv_key, result).await;
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
                } => {
                    self.handle_heartbeat(
                        &worker_id,
                        system,
                        supported_features,
                        max_builds,
                        running_builds,
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

    fn get_interested_builds(&self, drv_hash: &str) -> Vec<Uuid> {
        self.dag
            .node(drv_hash)
            .map(|s| s.interested_builds.iter().copied().collect())
            .unwrap_or_default()
    }

    fn drv_hash_to_path(&self, drv_hash: &str) -> Option<String> {
        self.dag.node(drv_hash).map(|s| s.drv_path().to_string())
    }

    /// Whether any interested build for this derivation is interactive (IFD).
    /// Interactive derivations get push_front on the ready queue.
    fn should_prioritize(&self, drv_hash: &str) -> bool {
        self.get_interested_builds(drv_hash).iter().any(|build_id| {
            self.builds
                .get(build_id)
                .is_some_and(|b| b.priority_class.is_interactive())
        })
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
    fn trigger_log_flush(&self, drv_hash: &str, interested_builds: Vec<Uuid>) {
        let Some(tx) = &self.log_flush_tx else {
            return;
        };
        let Some(drv_path) = self.drv_hash_to_path(drv_hash) else {
            // Should be impossible at this call site (completion handlers
            // already validated the hash exists in the DAG), but defensive.
            warn!(drv_hash, "trigger_log_flush: hash not in DAG, skipping");
            return;
        };
        let req = crate::logs::FlushRequest {
            drv_path,
            drv_hash: drv_hash.to_string(),
            interested_builds,
        };
        if tx.try_send(req).is_err() {
            warn!(
                drv_hash,
                "log flush channel full, dropped; periodic tick will snapshot"
            );
            metrics::counter!("rio_scheduler_log_flush_dropped_total").increment(1);
        }
    }
}

mod build;
mod completion;
mod dispatch;
mod merge;
mod worker;

#[cfg(test)]
pub(crate) mod tests;

/// Handle for sending commands to the actor.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ActorCommand>,
    /// Shared read-only backpressure flag with the actor. The actor computes
    /// hysteresis (activate at 80%, deactivate at 60%) and writes to its
    /// `Arc<AtomicBool>`; the handle reads it via this read-only view for
    /// send() and is_backpressured(). Without hysteresis, the handle used a
    /// simple threshold -> flapping under load near 80%.
    backpressure: BackpressureReader,
}

impl ActorHandle {
    /// Create a new actor handle and spawn the actor task.
    ///
    /// Returns the handle for sending commands.
    pub fn spawn(
        db: SchedulerDb,
        store_client: Option<StoreServiceClient<Channel>>,
        log_flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let mut actor = DagActor::new(db, store_client);
        if let Some(flush_tx) = log_flush_tx {
            actor = actor.with_log_flusher(flush_tx);
        }
        let backpressure = actor.backpressure_flag();
        let self_tx = tx.downgrade();
        rio_common::task::spawn_monitored("dag-actor", actor.run_with_self_tx(rx, self_tx));
        Self { tx, backpressure }
    }
    /// Whether the actor task is still alive. Returns false if the actor
    /// panicked or exited (its receiver dropped, closing the channel).
    ///
    /// gRPC handlers should check this and return UNAVAILABLE if false.
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Send a command to the actor, checking backpressure (with hysteresis).
    pub async fn send(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        // Read the actor's hysteresis-aware backpressure flag, not a simple
        // threshold. Activated at 80%, stays active until drained to 60%.
        if self.backpressure.is_active() {
            return Err(ActorError::Backpressure);
        }
        self.tx.send(cmd).await.map_err(|_| ActorError::ChannelSend)
    }

    /// Try to send a command without waiting (for fire-and-forget messages).
    /// Distinguishes `Full` (transient, retry helps) from `Closed` (actor
    /// panicked, permanent) so callers can choose retry vs fail-fast.
    pub fn try_send(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        use tokio::sync::mpsc::error::TrySendError;
        self.tx.try_send(cmd).map_err(|e| match e {
            TrySendError::Full(_) => ActorError::Backpressure,
            TrySendError::Closed(_) => ActorError::ChannelSend,
        })
    }

    /// Check if the actor is under backpressure (hysteresis-aware).
    pub fn is_backpressured(&self) -> bool {
        self.backpressure.is_active()
    }

    /// Send a command without backpressure check (for worker lifecycle events).
    pub async fn send_unchecked(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        self.tx.send(cmd).await.map_err(|_| ActorError::ChannelSend)
    }

    /// Test-only: query all worker states.
    #[cfg(test)]
    pub async fn debug_query_workers(&self) -> Result<Vec<DebugWorkerInfo>, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(ActorCommand::DebugQueryWorkers { reply: tx })
            .await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }

    /// Test-only: query a derivation's state.
    #[cfg(test)]
    pub async fn debug_query_derivation(
        &self,
        drv_hash: &str,
    ) -> Result<Option<DebugDerivationInfo>, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(ActorCommand::DebugQueryDerivation {
            drv_hash: drv_hash.to_string(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }
}
