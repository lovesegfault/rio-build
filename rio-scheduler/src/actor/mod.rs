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
use crate::estimator::Estimator;
use crate::queue::ReadyQueue;
#[allow(unused_imports)]
use crate::state::{
    BuildInfo, BuildOptions, BuildState, DerivationStatus, DrvHash, HEARTBEAT_TIMEOUT_SECS,
    MAX_MISSED_HEARTBEATS, POISON_THRESHOLD, POISON_TTL, PriorityClass, RetryPolicy, WorkerId,
    WorkerState,
};

mod command;
pub use command::*;

mod recovery;

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
    /// signs a Claims { worker_id, drv_hash, expected_output_paths,
    /// expiry } into WorkAssignment.assignment_token. The store
    /// verifies on PutPath — a worker can only upload outputs
    /// matching a valid assignment.
    ///
    /// None = tokens are the legacy format-string (unsigned).
    /// Store with hmac_verifier=None accepts both (dev mode).
    /// Arc because assign_to_worker is hot path and cloning the
    /// underlying key Vec on every dispatch would allocate.
    hmac_signer: Option<Arc<rio_common::hmac::HmacSigner>>,
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
            // 1 not 0: proto-default is 0. gen=0 tells workers "field
            // unset" (old scheduler); gen=1 is the real first generation.
            generation: Arc::new(AtomicU64::new(1)),
            self_tx: None,
            size_classes: Vec::new(),
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
                    peak_cpu_cores,
                } => {
                    self.handle_completion(
                        &worker_id,
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
                    systems,
                    supported_features,
                    max_builds,
                    running_builds,
                    bloom,
                    size_class,
                } => {
                    self.handle_heartbeat(
                        &worker_id,
                        systems,
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
                ActorCommand::GcRoots { reply } => {
                    // Collect expected_output_paths ∪ output_paths
                    // from all non-terminal derivations. These are
                    // the live-build roots that GC must NOT delete —
                    // either the worker is about to upload them
                    // (expected) or just did (output). Both cases:
                    // don't race the upload.
                    //
                    // Dedup via HashSet: the same drv can appear in
                    // multiple builds (shared dependency) → same
                    // expected_output_paths would be duplicated
                    // N× in the roots list. The store's mark CTE
                    // handles dups correctly, but it's wasted
                    // network + CTE work.
                    let roots: Vec<String> = self
                        .dag
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
                        .collect();
                    let _ = reply.send(roots);
                }
                ActorCommand::LeaderAcquired => {
                    self.handle_leader_acquired().await;
                    // Schedule reconciliation ~45s out via WeakSender.
                    // Same pattern as schedule_terminal_cleanup.
                    // Workers have ~45s (3× heartbeat + slack) to
                    // reconnect after scheduler restart. Any
                    // Assigned/Running derivation whose worker
                    // DIDN'T reconnect by then gets reconciled
                    // (Completed if outputs in store, else reset).
                    if let Some(weak_tx) = self.self_tx.clone() {
                        rio_common::task::spawn_monitored("reconcile-timer", async move {
                            tokio::time::sleep(RECONCILE_DELAY).await;
                            if let Some(tx) = weak_tx.upgrade()
                                && tx.try_send(ActorCommand::ReconcileAssignments).is_err()
                            {
                                tracing::warn!("reconcile command dropped (channel full)");
                            }
                        });
                    }
                }
                ActorCommand::ReconcileAssignments => {
                    self.handle_reconcile_assignments().await;
                }
                #[cfg(test)]
                ActorCommand::DebugQueryWorkers { reply } => {
                    let workers: Vec<_> = self
                        .workers
                        .values()
                        .map(|w| DebugWorkerInfo {
                            worker_id: w.worker_id.to_string(),
                            is_registered: w.is_registered(),
                            systems: w.systems.clone(),
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
                #[cfg(test)]
                ActorCommand::DebugForceAssign {
                    drv_hash,
                    worker_id,
                    reply,
                } => {
                    // Force Ready→Assigned (or Failed→Ready→Assigned)
                    // bypassing backoff + failed_workers exclusion.
                    // For retry/poison tests that need to drive
                    // multiple completion cycles without waiting
                    // for real backoff. Clears backoff_until.
                    let ok = if let Some(state) = self.dag.node_mut(&drv_hash) {
                        // If not already Ready, try to get there.
                        // Assigned/Running → reset_to_ready, Failed →
                        // transition Ready, Ready → no-op.
                        let prepped = match state.status() {
                            DerivationStatus::Ready => true,
                            DerivationStatus::Assigned | DerivationStatus::Running => {
                                state.reset_to_ready().is_ok()
                            }
                            DerivationStatus::Failed => {
                                state.transition(DerivationStatus::Ready).is_ok()
                            }
                            _ => false, // terminal or pre-Ready: can't force
                        };
                        if prepped {
                            state.backoff_until = None;
                            state.assigned_worker = Some(worker_id.clone());
                            // Add to worker's running set so subsequent
                            // complete_failure finds a consistent state.
                            if let Some(w) = self.workers.get_mut(&worker_id) {
                                w.running_builds.insert((&*drv_hash).into());
                            }
                            state.transition(DerivationStatus::Assigned).is_ok()
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    let _ = reply.send(ok);
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

    /// Clone the generation counter as a read-only reader for
    /// `ActorHandle::leader_generation()`. The lease task (C2) holds a
    /// direct `Arc<AtomicU64>` clone for writing — not through this
    /// reader. The reader type has no store/fetch_add methods, so
    /// handle consumers can't accidentally increment.
    pub(crate) fn generation_reader(&self) -> GenerationReader {
        GenerationReader::new(self.generation.clone())
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
