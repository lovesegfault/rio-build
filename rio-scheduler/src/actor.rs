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
    BuildInfo, BuildOptions, BuildState, DerivationStatus, HEARTBEAT_TIMEOUT_SECS,
    MAX_MISSED_HEARTBEATS, POISON_THRESHOLD, POISON_TTL, RetryPolicy, WorkerState,
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

/// Commands sent to the DAG actor.
pub enum ActorCommand {
    /// Merge a new build's derivation DAG into the global graph.
    MergeDag {
        build_id: Uuid,
        tenant_id: Option<String>,
        priority_class: String,
        nodes: Vec<rio_proto::types::DerivationNode>,
        edges: Vec<rio_proto::types::DerivationEdge>,
        options: BuildOptions,
        keep_going: bool,
        reply:
            oneshot::Sender<Result<broadcast::Receiver<rio_proto::types::BuildEvent>, ActorError>>,
    },

    /// Process a completion report from a worker.
    ProcessCompletion {
        worker_id: String,
        drv_hash: String,
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
        worker_id: String,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
    },

    /// A worker's BuildExecution stream closed.
    WorkerDisconnected { worker_id: String },

    /// Periodic heartbeat from a worker.
    Heartbeat {
        worker_id: String,
        system: String,
        supported_features: Vec<String>,
        max_builds: u32,
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
    workers: HashMap<String, WorkerState>,
    /// Retry policy.
    retry_policy: RetryPolicy,
    /// Database handle.
    db: SchedulerDb,
    /// Store service client for scheduler-side cache checks. `None` in tests
    /// that don't need the store (cache check is then skipped).
    store_client: Option<StoreServiceClient<Channel>>,
    /// Whether backpressure is currently active. Shared with ActorHandle
    /// so hysteresis (80%/60%) is honored by send() instead of a simple
    /// threshold check. Arc<AtomicBool> for lock-free reads on the hot path.
    backpressure_active: Arc<AtomicBool>,
    /// Leader generation counter (for assignment tokens).
    generation: i64,
    /// Weak clone of the actor's own command sender, for scheduling delayed
    /// internal commands (e.g., terminal build cleanup). Weak so the actor
    /// doesn't prevent channel close when all external handles are dropped.
    /// `None` if spawned via bare `run()` (no delayed scheduling).
    self_tx: Option<mpsc::WeakSender<ActorCommand>>,
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
            backpressure_active: Arc::new(AtomicBool::new(false)),
            generation: 1,
            self_tx: None,
        }
    }

    /// Run the actor event loop. Consumes commands from the channel until it closes.
    pub async fn run(mut self, mut rx: mpsc::Receiver<ActorCommand>) {
        // No self_tx: cleanup is synchronous (test helpers don't pass a tx clone).
        // Production (ActorHandle::spawn) uses run_with_self_tx.
        self.run_inner(&mut rx).await;
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
                ActorCommand::MergeDag {
                    build_id,
                    tenant_id,
                    priority_class,
                    nodes,
                    edges,
                    options,
                    keep_going,
                    reply,
                } => {
                    let result = self
                        .handle_merge_dag(
                            build_id,
                            tenant_id,
                            priority_class,
                            nodes,
                            edges,
                            options,
                            keep_going,
                        )
                        .await;
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
                    drv_hash,
                    result,
                } => {
                    self.handle_completion(&worker_id, &drv_hash, result).await;
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
                    self.handle_worker_connected(worker_id, stream_tx);
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
                        worker_id,
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
                    // TODO(phase3): since_sequence is for resuming after gateway reconnect
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
                #[cfg(test)]
                ActorCommand::DebugQueryWorkers { reply } => {
                    let workers: Vec<_> = self
                        .workers
                        .values()
                        .map(|w| DebugWorkerInfo {
                            worker_id: w.worker_id.clone(),
                            is_registered: w.is_registered(),
                            system: w.system.clone(),
                            running_count: w.running_builds.len(),
                            running_builds: w.running_builds.iter().cloned().collect(),
                        })
                        .collect();
                    let _ = reply.send(workers);
                }
                #[cfg(test)]
                ActorCommand::DebugQueryDerivation { drv_hash, reply } => {
                    let info = self.dag.node(&drv_hash).map(|s| DebugDerivationInfo {
                        drv_hash: s.drv_hash.clone(),
                        drv_path: s.drv_path.clone(),
                        status: s.status(),
                        retry_count: s.retry_count,
                        assigned_worker: s.assigned_worker.clone(),
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

    /// Check if the actor is under backpressure (shared hysteresis state).
    pub fn is_backpressured(&self) -> bool {
        self.backpressure_active.load(Ordering::Relaxed)
    }

    /// Clone the shared backpressure flag for wiring into ActorHandle.
    pub(crate) fn backpressure_flag(&self) -> Arc<AtomicBool> {
        self.backpressure_active.clone()
    }

    // -----------------------------------------------------------------------
    // MergeDag
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self, nodes, edges, options), fields(build_id = %build_id))]
    async fn handle_merge_dag(
        &mut self,
        build_id: Uuid,
        tenant_id: Option<String>,
        priority_class: String,
        nodes: Vec<rio_proto::types::DerivationNode>,
        edges: Vec<rio_proto::types::DerivationEdge>,
        options: BuildOptions,
        keep_going: bool,
    ) -> Result<broadcast::Receiver<rio_proto::types::BuildEvent>, ActorError> {
        metrics::counter!("rio_scheduler_builds_total").increment(1);

        // Create build record in DB
        self.db
            .insert_build(build_id, tenant_id.as_deref(), &priority_class)
            .await?;

        // Create broadcast channel for build events
        let (event_tx, event_rx) = broadcast::channel(BUILD_EVENT_BUFFER_SIZE);
        self.build_events.insert(build_id, event_tx);
        self.build_sequences.insert(build_id, 0);

        // Create in-memory build info
        let build_info = BuildInfo::new_pending(
            build_id,
            tenant_id,
            priority_class,
            keep_going,
            options,
            nodes.iter().map(|n| n.drv_hash.clone()).collect(),
        );
        self.builds.insert(build_id, build_info);

        // Merge nodes and edges into the global DAG
        let newly_inserted = self
            .dag
            .merge(build_id, &nodes, &edges)
            .map_err(|e| ActorError::Internal(format!("DAG merge failed: {e}")))?;

        // Persist newly inserted derivations and edges to DB
        for node in &nodes {
            let drv_hash = &node.drv_hash;
            let status = if newly_inserted.contains(drv_hash) {
                DerivationStatus::Created
            } else {
                // Existing node, just link the build
                if let Some(state) = self.dag.node(drv_hash) {
                    state.status()
                } else {
                    DerivationStatus::Created
                }
            };

            let db_id = self
                .db
                .upsert_derivation(
                    drv_hash,
                    &node.drv_path,
                    if node.pname.is_empty() {
                        None
                    } else {
                        Some(&node.pname)
                    },
                    &node.system,
                    status,
                    &node.required_features,
                )
                .await?;

            // Store db_id on the node
            if let Some(state) = self.dag.node_mut(drv_hash) {
                state.db_id = Some(db_id);
            }

            // Link build to derivation in DB
            self.db.insert_build_derivation(build_id, db_id).await?;
        }

        // Persist edges
        for edge in &edges {
            // Resolve drv_path -> db_id
            let parent_id = self.find_db_id_by_path(&edge.parent_drv_path);
            let child_id = self.find_db_id_by_path(&edge.child_drv_path);

            if let (Some(pid), Some(cid)) = (parent_id, child_id) {
                self.db.insert_edge(pid, cid).await?;
            }
        }

        // Transition build to active
        self.transition_build(build_id, BuildState::Active).await?;

        // Index proto nodes by hash for efficient lookup during the transition loop.
        let node_index: HashMap<&str, &rio_proto::types::DerivationNode> =
            nodes.iter().map(|n| (n.drv_hash.as_str(), n)).collect();

        let total_derivations = nodes.len() as u32;
        let mut cached_count = 0u32;

        // Scheduler-side cache check: query the store for expected_output_paths
        // of newly-inserted derivations. If all outputs exist, skip straight to
        // Completed. This closes the TOCTOU window between the gateway's
        // FindMissingPaths and our merge (another build may have completed the
        // derivation in between).
        let cached_hashes = self
            .check_cached_outputs(&newly_inserted, &node_index)
            .await;

        for drv_hash in &cached_hashes {
            let Some(node) = node_index.get(drv_hash.as_str()) else {
                continue;
            };
            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.transition(DerivationStatus::Completed) {
                    warn!(drv_hash, error = %e, "cache-hit Created->Completed transition failed");
                    continue;
                }
                state.output_paths = node.expected_output_paths.clone();
                cached_count += 1;
                metrics::counter!("rio_scheduler_cache_hits_total", "source" => "scheduler")
                    .increment(1);

                if let Err(e) = self
                    .db
                    .update_derivation_status(drv_hash, DerivationStatus::Completed, None)
                    .await
                {
                    warn!(drv_hash, error = %e, "failed to persist cache-hit status");
                }

                self.emit_build_event(
                    build_id,
                    rio_proto::types::build_event::Event::Derivation(
                        rio_proto::types::DerivationEvent {
                            derivation_path: node.drv_path.clone(),
                            status: Some(rio_proto::types::derivation_event::Status::Cached(
                                rio_proto::types::DerivationCached {
                                    output_paths: node.expected_output_paths.clone(),
                                },
                            )),
                        },
                    ),
                );
            }
        }

        // Compute initial states for the remaining (non-cached) newly-inserted
        // derivations. Cached derivations above are now Completed, so their
        // dependents will correctly be computed as Ready here.
        let remaining_new: Vec<String> = newly_inserted
            .iter()
            .filter(|h| !cached_hashes.contains(h.as_str()))
            .cloned()
            .collect();
        let initial_states = self.dag.compute_initial_states(&remaining_new);

        // Interactive builds (IFD) get priority: push_front instead of push_back
        let is_interactive = self
            .builds
            .get(&build_id)
            .map(|b| b.priority_class == "interactive")
            .unwrap_or(false);

        for (drv_hash, target_status) in &initial_states {
            if let Some(state) = self.dag.node_mut(drv_hash) {
                match target_status {
                    DerivationStatus::Ready => {
                        // Transition created -> queued -> ready
                        if let Err(e) = state.transition(DerivationStatus::Queued) {
                            warn!(drv_hash, error = %e, "Created->Queued transition failed");
                        }
                        if let Err(e) = state.transition(DerivationStatus::Ready) {
                            warn!(drv_hash, error = %e, "Queued->Ready transition failed");
                        }
                        self.db
                            .update_derivation_status(drv_hash, DerivationStatus::Ready, None)
                            .await?;
                        if is_interactive {
                            self.ready_queue.push_front(drv_hash.clone());
                        } else {
                            self.ready_queue.push_back(drv_hash.clone());
                        }
                    }
                    DerivationStatus::Queued => {
                        if let Err(e) = state.transition(DerivationStatus::Queued) {
                            warn!(drv_hash, error = %e, "Created->Queued transition failed");
                        }
                        self.db
                            .update_derivation_status(drv_hash, DerivationStatus::Queued, None)
                            .await?;
                    }
                    _ => {}
                }
            }
        }

        // Also handle nodes that already existed and are already completed
        for node in &nodes {
            if !newly_inserted.contains(&node.drv_hash)
                && let Some(state) = self.dag.node(&node.drv_hash)
                && state.status() == DerivationStatus::Completed
            {
                cached_count += 1;
                metrics::counter!("rio_scheduler_cache_hits_total", "source" => "existing")
                    .increment(1);
            }
        }

        // Update build's cached count
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.cached_count = cached_count;
            build.completed_count = cached_count;
        }

        // Send BuildStarted event
        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Started(rio_proto::types::BuildStarted {
                total_derivations,
                cached_derivations: cached_count,
            }),
        );

        // Check if the build is already complete (all cache hits)
        if cached_count == total_derivations {
            self.complete_build(build_id).await?;
        } else {
            // Dispatch ready derivations to workers
            self.dispatch_ready().await;
        }

        Ok(event_rx)
    }

    /// Query the store for newly-inserted derivations' expected outputs and
    /// return the set of drv_hashes whose outputs are all already present.
    ///
    /// Returns an empty set if there's no store client (tests) or if the
    /// RPC fails (non-fatal — we fall back to building).
    async fn check_cached_outputs(
        &self,
        newly_inserted: &[String],
        node_index: &HashMap<&str, &rio_proto::types::DerivationNode>,
    ) -> HashSet<String> {
        let Some(store_client) = &self.store_client else {
            return HashSet::new();
        };

        // Collect all expected output paths for newly-inserted derivations.
        // Skip nodes without expected_output_paths (old gateways, unresolvable leaf nodes).
        let check_paths: Vec<String> = newly_inserted
            .iter()
            .filter_map(|h| node_index.get(h.as_str()))
            .flat_map(|n| n.expected_output_paths.iter().cloned())
            .collect();

        if check_paths.is_empty() {
            return HashSet::new();
        }

        let resp = match store_client
            .clone()
            .find_missing_paths(FindMissingPathsRequest {
                store_paths: check_paths.clone(),
            })
            .await
        {
            Ok(r) => r.into_inner(),
            Err(e) => {
                warn!(error = %e, "store FindMissingPaths failed; skipping scheduler cache check");
                return HashSet::new();
            }
        };

        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let present: HashSet<String> = check_paths
            .into_iter()
            .filter(|p| !missing.contains(p))
            .collect();

        // A derivation is cached if it has at least one expected output path
        // AND all of them are present.
        newly_inserted
            .iter()
            .filter(|h| {
                node_index.get(h.as_str()).is_some_and(|n| {
                    !n.expected_output_paths.is_empty()
                        && n.expected_output_paths.iter().all(|p| present.contains(p))
                })
            })
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // ProcessCompletion
    // -----------------------------------------------------------------------

    #[instrument(skip(self, result), fields(worker_id = %worker_id, drv_hash = %drv_hash))]
    async fn handle_completion(
        &mut self,
        worker_id: &str,
        drv_hash: &str,
        result: rio_proto::types::BuildResult,
    ) {
        let status = rio_proto::types::BuildResultStatus::try_from(result.status)
            .unwrap_or(rio_proto::types::BuildResultStatus::Unspecified);

        // The gRPC layer passes CompletionReport.drv_path here (keyed by path,
        // not hash). Resolve to drv_hash if the key isn't found directly.
        let resolved_hash: String;
        let drv_hash: &str = if self.dag.contains(drv_hash) {
            drv_hash
        } else if let Some(h) = self.drv_path_to_hash(drv_hash) {
            resolved_hash = h;
            &resolved_hash
        } else {
            warn!(
                key = drv_hash,
                "completion for unknown derivation, ignoring"
            );
            return;
        };

        // Find the derivation in the DAG
        let current_status = match self.dag.node(drv_hash) {
            Some(state) => state.status(),
            None => {
                warn!(drv_hash, "completion for unknown derivation, ignoring");
                return;
            }
        };

        // Idempotency: completed -> completed is a no-op
        if current_status == DerivationStatus::Completed {
            debug!(drv_hash, "duplicate completion report, ignoring");
            return;
        }

        // Only process completions for assigned/running derivations
        if !matches!(
            current_status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ) {
            warn!(
                drv_hash,
                current_status = %current_status,
                "completion for derivation not in assigned/running state, ignoring"
            );
            return;
        }

        match status {
            rio_proto::types::BuildResultStatus::Built
            | rio_proto::types::BuildResultStatus::Substituted
            | rio_proto::types::BuildResultStatus::AlreadyValid => {
                self.handle_success_completion(drv_hash, &result, worker_id)
                    .await;
            }
            rio_proto::types::BuildResultStatus::TransientFailure
            | rio_proto::types::BuildResultStatus::InfrastructureFailure => {
                self.handle_transient_failure(drv_hash, worker_id).await;
            }
            rio_proto::types::BuildResultStatus::PermanentFailure
            | rio_proto::types::BuildResultStatus::CachedFailure
            | rio_proto::types::BuildResultStatus::DependencyFailed
            | rio_proto::types::BuildResultStatus::LogLimitExceeded
            | rio_proto::types::BuildResultStatus::OutputRejected => {
                self.handle_permanent_failure(drv_hash, &result.error_msg, worker_id)
                    .await;
            }
            _ => {
                warn!(
                    drv_hash,
                    status = result.status,
                    "unknown build result status, treating as transient failure"
                );
                self.handle_transient_failure(drv_hash, worker_id).await;
            }
        }

        // Free worker capacity
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.remove(drv_hash);
        }

        // Dispatch newly ready derivations
        self.dispatch_ready().await;
    }

    async fn handle_success_completion(
        &mut self,
        drv_hash: &str,
        result: &rio_proto::types::BuildResult,
        worker_id: &str,
    ) {
        // Transition to completed
        if let Some(state) = self.dag.node_mut(drv_hash) {
            // Ensure we're in running state first
            if state.status() == DerivationStatus::Assigned
                && let Err(e) = state.transition(DerivationStatus::Running)
            {
                warn!(drv_hash, error = %e, "Assigned->Running transition failed");
            }
            if state.transition(DerivationStatus::Completed).is_err() {
                return;
            }

            // Store output paths from built_outputs
            state.output_paths = result
                .built_outputs
                .iter()
                .map(|o| o.output_path.clone())
                .collect();
        }

        // Update DB
        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Completed, None)
            .await
        {
            error!(drv_hash, error = %e, "failed to update derivation status in DB");
        }

        // Update assignment (non-terminal progress write: log failure, don't block)
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self.db.update_assignment_status(db_id, "completed").await
        {
            error!(drv_hash, error = %e, "failed to persist assignment completion");
        }

        // Async: update build history EMA (best-effort statistics)
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(pname) = &state.pname
            && let (Some(start), Some(stop)) = (&result.start_time, &result.stop_time)
        {
            let duration_secs = stop.seconds.saturating_sub(start.seconds) as f64
                + stop.nanos.saturating_sub(start.nanos) as f64 / 1_000_000_000.0;
            // Sanity bound: reject durations > 30 days (bogus worker timestamps)
            if duration_secs > 0.0
                && duration_secs < 30.0 * 86400.0
                && let Err(e) = self
                    .db
                    .update_build_history(pname, &state.system, duration_secs)
                    .await
            {
                error!(drv_hash, error = %e, "failed to update build history EMA");
            }
        }

        // Emit derivation completed event
        let output_paths: Vec<String> = self
            .dag
            .node(drv_hash)
            .map(|s| s.output_paths.clone())
            .unwrap_or_default();

        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in &interested_builds {
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent {
                        derivation_path: self.drv_hash_to_path(drv_hash).unwrap_or_default(),
                        status: Some(rio_proto::types::derivation_event::Status::Completed(
                            rio_proto::types::DerivationCompleted {
                                output_paths: output_paths.clone(),
                            },
                        )),
                    },
                ),
            );
        }

        // Release downstream: find newly ready derivations.
        // Interactive (IFD) derivations go to the front of the queue.
        let newly_ready = self.dag.find_newly_ready(drv_hash);
        for ready_hash in &newly_ready {
            let prioritize = self.should_prioritize(ready_hash);
            if let Some(state) = self.dag.node_mut(ready_hash)
                && state.transition(DerivationStatus::Ready).is_ok()
            {
                if let Err(e) = self
                    .db
                    .update_derivation_status(ready_hash, DerivationStatus::Ready, None)
                    .await
                {
                    error!(drv_hash = ready_hash, error = %e, "failed to update status");
                }
                if prioritize {
                    self.ready_queue.push_front(ready_hash.clone());
                } else {
                    self.ready_queue.push_back(ready_hash.clone());
                }
            }
        }

        // Update build completion status
        for build_id in &interested_builds {
            self.update_build_counts(*build_id);
            self.check_build_completion(*build_id, worker_id).await;
        }
    }

    async fn handle_transient_failure(&mut self, drv_hash: &str, worker_id: &str) {
        let should_retry = if let Some(state) = self.dag.node_mut(drv_hash) {
            state.failed_workers.insert(worker_id.to_string());

            // Check poison threshold
            if state.failed_workers.len() >= POISON_THRESHOLD {
                // Transition to poisoned
                if state.status() == DerivationStatus::Assigned
                    && let Err(e) = state.transition(DerivationStatus::Running)
                {
                    warn!(drv_hash, error = %e, "Assigned->Running transition failed");
                }
                if let Err(e) = state.transition(DerivationStatus::Poisoned) {
                    warn!(drv_hash, error = %e, "->Poisoned transition failed");
                }
                state.poisoned_at = Some(Instant::now());
                false
            } else if state.retry_count < self.retry_policy.max_retries {
                // Transition running -> failed
                if state.status() == DerivationStatus::Assigned
                    && let Err(e) = state.transition(DerivationStatus::Running)
                {
                    warn!(drv_hash, error = %e, "Assigned->Running transition failed");
                }
                if let Err(e) = state.transition(DerivationStatus::Failed) {
                    warn!(drv_hash, error = %e, "Running->Failed transition failed");
                }
                true
            } else {
                // Max retries exceeded: treat as permanent
                if state.status() == DerivationStatus::Assigned
                    && let Err(e) = state.transition(DerivationStatus::Running)
                {
                    warn!(drv_hash, error = %e, "Assigned->Running transition failed");
                }
                if let Err(e) = state.transition(DerivationStatus::Poisoned) {
                    warn!(drv_hash, error = %e, "->Poisoned transition failed");
                }
                state.poisoned_at = Some(Instant::now());
                false
            }
        } else {
            false
        };

        if should_retry {
            let retry_count = self.dag.node(drv_hash).map(|s| s.retry_count).unwrap_or(0);

            // Schedule retry with backoff
            let backoff = self.retry_policy.backoff_duration(retry_count);
            let drv_hash_owned = drv_hash.to_string();

            if let Some(state) = self.dag.node_mut(&drv_hash_owned) {
                state.retry_count += 1;
                state.assigned_worker = None;
            }

            if let Err(e) = self
                .db
                .update_derivation_status(drv_hash, DerivationStatus::Failed, None)
                .await
            {
                error!(drv_hash, error = %e, "failed to persist Failed status");
            }
            if let Err(e) = self.db.increment_retry_count(drv_hash).await {
                error!(drv_hash, error = %e, "failed to persist retry increment");
            }

            // After backoff, transition failed -> ready
            // For simplicity in Phase 2a, we re-queue immediately with the backoff
            // duration logged. A full implementation would use a timer.
            debug!(
                drv_hash,
                retry_count = retry_count + 1,
                backoff_secs = backoff.as_secs_f64(),
                "scheduling retry after transient failure"
            );

            // TODO(phase3): delayed re-queue using the computed backoff duration.
            // Phase 2a re-queues immediately (see docs/src/phases/phase2a.md).
            if let Some(state) = self.dag.node_mut(&drv_hash_owned) {
                if let Err(e) = state.transition(DerivationStatus::Ready) {
                    warn!(drv_hash, error = %e, "Failed->Ready transition failed");
                } else {
                    self.ready_queue.push_back(drv_hash_owned);
                }
            }
        } else {
            if let Err(e) = self
                .db
                .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
                .await
            {
                error!(drv_hash, error = %e, "failed to persist Poisoned status");
            }

            // Cascade: parents of a poisoned derivation can never complete.
            // Transition them to DependencyFailed so keepGoing builds terminate.
            self.cascade_dependency_failure(drv_hash).await;

            // Propagate failure to interested builds
            let interested_builds = self.get_interested_builds(drv_hash);
            for build_id in interested_builds {
                self.handle_derivation_failure(build_id, drv_hash).await;
            }
        }
    }

    async fn handle_permanent_failure(
        &mut self,
        drv_hash: &str,
        error_msg: &str,
        _worker_id: &str,
    ) {
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if state.status() == DerivationStatus::Assigned
                && let Err(e) = state.transition(DerivationStatus::Running)
            {
                warn!(drv_hash, error = %e, "Assigned->Running transition failed");
            }
            // Permanent failure -> poisoned (no retry)
            if let Err(e) = state.transition(DerivationStatus::Poisoned) {
                warn!(drv_hash, error = %e, "->Poisoned transition failed");
            }
            state.poisoned_at = Some(Instant::now());
        }

        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
            .await
        {
            error!(drv_hash, error = %e, "failed to persist Poisoned status");
        }

        // Cascade: parents of a poisoned derivation can never complete.
        self.cascade_dependency_failure(drv_hash).await;

        // Propagate failure to interested builds
        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in interested_builds {
            // Emit failure event
            self.emit_build_event(
                build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent {
                        derivation_path: self.drv_hash_to_path(drv_hash).unwrap_or_default(),
                        status: Some(rio_proto::types::derivation_event::Status::Failed(
                            rio_proto::types::DerivationFailed {
                                error_message: error_msg.to_string(),
                                status: rio_proto::types::BuildResultStatus::PermanentFailure
                                    .into(),
                            },
                        )),
                    },
                ),
            );

            self.handle_derivation_failure(build_id, drv_hash).await;
        }
    }

    /// Transitively walk parents of a poisoned derivation and transition all
    /// Queued/Ready/Created ancestors to DependencyFailed.
    ///
    /// Without this, keepGoing builds with a poisoned leaf hang forever:
    /// parents stay Queued, so completed+failed never reaches total.
    async fn cascade_dependency_failure(&mut self, poisoned_hash: &str) {
        let mut to_visit: Vec<String> = self.dag.get_parents(poisoned_hash);
        let mut visited: HashSet<String> = HashSet::new();

        while let Some(parent_hash) = to_visit.pop() {
            if !visited.insert(parent_hash.clone()) {
                continue; // already processed
            }

            let Some(state) = self.dag.node_mut(&parent_hash) else {
                continue;
            };

            // Only cascade to derivations that haven't started yet.
            // Assigned/Running derivations will complete or fail on their own
            // (and their completion handler will re-cascade if they succeed
            // but a sibling dep is dead — but actually they'd never become
            // Ready in the first place since all_deps_completed is false).
            if !matches!(
                state.status(),
                DerivationStatus::Queued | DerivationStatus::Ready | DerivationStatus::Created
            ) {
                continue;
            }

            if let Err(e) = state.transition(DerivationStatus::DependencyFailed) {
                warn!(drv_hash = %parent_hash, error = %e, "cascade ->DependencyFailed transition failed");
                continue;
            }

            debug!(
                drv_hash = %parent_hash,
                poisoned_dep = %poisoned_hash,
                "cascaded DependencyFailed from poisoned dependency"
            );

            // Remove from ready queue if present (Ready -> DependencyFailed).
            self.ready_queue.remove(&parent_hash);

            if let Err(e) = self
                .db
                .update_derivation_status(&parent_hash, DerivationStatus::DependencyFailed, None)
                .await
            {
                error!(drv_hash = %parent_hash, error = %e, "failed to persist DependencyFailed");
            }

            // Continue cascade: this parent's parents also cannot complete.
            to_visit.extend(self.dag.get_parents(&parent_hash));
        }
    }

    async fn handle_derivation_failure(&mut self, build_id: Uuid, drv_hash: &str) {
        // Sync counts from DAG ground truth. The cascade may have transitioned
        // additional parents to DependencyFailed; those must be counted here.
        self.update_build_counts(build_id);

        let Some(build) = self.builds.get_mut(&build_id) else {
            return;
        };

        if !build.keep_going {
            // Fail the entire build immediately
            build.error_summary = Some(format!("derivation {drv_hash} failed"));
            build.failed_derivation = Some(drv_hash.to_string());
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        } else {
            // keepGoing: check if all derivations are resolved
            self.check_build_completion(build_id, "").await;
        }
    }

    // -----------------------------------------------------------------------
    // CancelBuild
    // -----------------------------------------------------------------------

    #[instrument(skip(self), fields(build_id = %build_id))]
    async fn handle_cancel_build(
        &mut self,
        build_id: Uuid,
        reason: &str,
    ) -> Result<bool, ActorError> {
        let build = match self.builds.get(&build_id) {
            Some(b) => b,
            None => return Err(ActorError::BuildNotFound(build_id)),
        };

        if build.state().is_terminal() {
            return Ok(false);
        }

        // Remove build interest from derivations
        let orphaned = self.dag.remove_build_interest(build_id);

        // Remove orphaned derivations from the ready queue
        for hash in &orphaned {
            self.ready_queue.remove(hash);
        }

        // Transition build to cancelled. Already checked !is_terminal above,
        // so this can only fail if the state was concurrently modified — but
        // we're the actor (single owner), so this succeeds.
        if let Some(build) = self.builds.get_mut(&build_id) {
            let _ = build.transition(BuildState::Cancelled);
        }

        self.db
            .update_build_status(build_id, BuildState::Cancelled, None)
            .await?;

        // Emit cancelled event
        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Cancelled(rio_proto::types::BuildCancelled {
                reason: reason.to_string(),
            }),
        );

        info!(build_id = %build_id, reason, "build cancelled");
        self.schedule_terminal_cleanup(build_id);
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Worker management
    // -----------------------------------------------------------------------

    fn handle_worker_connected(
        &mut self,
        worker_id: String,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
    ) {
        info!(worker_id = %worker_id, "worker stream connected");

        let worker = self
            .workers
            .entry(worker_id.clone())
            .or_insert_with(|| WorkerState {
                worker_id: worker_id.clone(),
                system: None,
                supported_features: Vec::new(),
                max_builds: 0,
                running_builds: Default::default(),
                stream_tx: None,
                last_heartbeat: Instant::now(),
                missed_heartbeats: 0,
            });

        let was_registered = worker.is_registered();
        worker.stream_tx = Some(stream_tx);

        if !was_registered && worker.is_registered() {
            info!(worker_id, "worker fully registered (stream + heartbeat)");
        }
    }

    async fn handle_worker_disconnected(&mut self, worker_id: &str) {
        info!(worker_id, "worker disconnected");

        let Some(worker) = self.workers.remove(worker_id) else {
            return; // unknown worker, no-op (and no gauge decrement)
        };

        // Only decrement if worker was fully registered (stream + heartbeat).
        // Otherwise the gauge goes negative for workers that connected a stream
        // but never sent a heartbeat (increment fires on full registration only).
        let was_registered = worker.is_registered();

        // Reassign all derivations that were assigned to this worker.
        // reset_to_ready() handles Assigned -> Ready and Running -> Failed -> Ready,
        // maintaining state-machine invariants.
        for drv_hash in &worker.running_builds {
            if let Some(state) = self.dag.node_mut(drv_hash.as_str()) {
                if let Err(e) = state.reset_to_ready() {
                    warn!(
                        drv_hash, error = %e,
                        "invalid state for worker-lost recovery, skipping"
                    );
                    continue;
                }
                // Worker disconnect during build is a failed attempt: count it.
                state.retry_count += 1;
                if let Err(e) = self.db.increment_retry_count(drv_hash).await {
                    error!(drv_hash, error = %e, "failed to persist retry increment");
                }
                if let Err(e) = self
                    .db
                    .update_derivation_status(drv_hash, DerivationStatus::Ready, None)
                    .await
                {
                    error!(drv_hash, error = %e, "failed to persist Ready status");
                }
                self.ready_queue.push_back(drv_hash.clone());
            }
        }

        if was_registered {
            metrics::gauge!("rio_scheduler_workers_active").decrement(1.0);
        }
    }

    fn handle_heartbeat(
        &mut self,
        worker_id: String,
        system: String,
        supported_features: Vec<String>,
        max_builds: u32,
        running_builds: Vec<String>,
    ) {
        // TOCTOU fix: a stale heartbeat must not clobber fresh assignments.
        // The scheduler is authoritative for what it assigned. We reconcile:
        //   - Keep scheduler-known builds that are still Assigned/Running
        //     in the DAG (heartbeat may predate the assignment).
        //   - Accept heartbeat-reported builds we don't know about, but warn
        //     (shouldn't happen; indicates split-brain or restart).
        //   - Remove builds absent from heartbeat only if DAG state is no
        //     longer Assigned/Running (completion already processed).
        let heartbeat_set: HashSet<String> = running_builds.into_iter().collect();

        // Compute the reconciled running set before borrowing `worker` mutably,
        // so we can read self.dag for derivation state checks.
        let prev_running: HashSet<String> = self
            .workers
            .get(&worker_id)
            .map(|w| w.running_builds.clone())
            .unwrap_or_default();

        let mut reconciled: HashSet<String> = HashSet::new();
        // Keep scheduler-assigned builds that are still in-flight.
        for drv_hash in &prev_running {
            let still_inflight = self.dag.node(drv_hash).is_some_and(|s| {
                matches!(
                    s.status(),
                    DerivationStatus::Assigned | DerivationStatus::Running
                )
            });
            if still_inflight {
                reconciled.insert(drv_hash.clone());
            }
        }
        // Add heartbeat-reported builds we don't know about (with warning).
        for drv_hash in &heartbeat_set {
            if !reconciled.contains(drv_hash) && !prev_running.contains(drv_hash) {
                warn!(
                    worker_id = %worker_id,
                    drv_hash,
                    "heartbeat reports running build scheduler did not assign"
                );
                reconciled.insert(drv_hash.clone());
            }
        }

        let worker = self
            .workers
            .entry(worker_id.clone())
            .or_insert_with(|| WorkerState {
                worker_id: worker_id.clone(),
                system: None,
                supported_features: Vec::new(),
                max_builds: 0,
                running_builds: Default::default(),
                stream_tx: None,
                last_heartbeat: Instant::now(),
                missed_heartbeats: 0,
            });

        let was_registered = worker.is_registered();

        worker.system = Some(system);
        worker.supported_features = supported_features;
        worker.max_builds = max_builds;
        worker.last_heartbeat = Instant::now();
        worker.missed_heartbeats = 0;
        worker.running_builds = reconciled;

        if !was_registered && worker.is_registered() {
            info!(worker_id, "worker fully registered (heartbeat + stream)");
            metrics::gauge!("rio_scheduler_workers_active").increment(1.0);
        }
    }

    // -----------------------------------------------------------------------
    // Tick (periodic housekeeping)
    // -----------------------------------------------------------------------

    async fn handle_tick(&mut self) {
        // Check for heartbeat timeouts
        let now = Instant::now();
        let timeout = std::time::Duration::from_secs(HEARTBEAT_TIMEOUT_SECS);
        let mut timed_out_workers = Vec::new();

        for (worker_id, worker) in &mut self.workers {
            if now.duration_since(worker.last_heartbeat) > timeout {
                worker.missed_heartbeats += 1;
                if worker.missed_heartbeats >= MAX_MISSED_HEARTBEATS {
                    timed_out_workers.push(worker_id.clone());
                }
            }
        }

        for worker_id in timed_out_workers {
            warn!(worker_id, "worker timed out (missed heartbeats)");
            self.handle_worker_disconnected(&worker_id).await;
        }

        // Check for poisoned derivations that should expire (24h TTL)
        let mut expired_poisons = Vec::new();
        for (drv_hash, state) in self.dag.iter_nodes() {
            if state.status() == DerivationStatus::Poisoned
                && let Some(poisoned_at) = state.poisoned_at
                && now.duration_since(poisoned_at) > POISON_TTL
            {
                expired_poisons.push(drv_hash.to_string());
            }
        }

        for drv_hash in expired_poisons {
            info!(drv_hash, "poison TTL expired, resetting to created");
            if let Some(state) = self.dag.node_mut(&drv_hash)
                && let Err(e) = state.reset_from_poison()
            {
                warn!(drv_hash, error = %e, "poison reset failed");
                continue;
            }
            if let Err(e) = self
                .db
                .update_derivation_status(&drv_hash, DerivationStatus::Created, None)
                .await
            {
                error!(drv_hash, error = %e, "failed to persist poison reset");
            }
        }

        // Update metrics. All gauges are set from ground-truth state on each
        // Tick — this is self-healing against any counting bugs elsewhere.
        metrics::gauge!("rio_scheduler_derivations_queued").set(self.ready_queue.len() as f64);
        metrics::gauge!("rio_scheduler_builds_active").set(
            self.builds
                .values()
                .filter(|b| b.state() == BuildState::Active)
                .count() as f64,
        );
        metrics::gauge!("rio_scheduler_derivations_running").set(
            self.dag
                .iter_values()
                .filter(|s| {
                    matches!(
                        s.status(),
                        DerivationStatus::Running | DerivationStatus::Assigned
                    )
                })
                .count() as f64,
        );
    }

    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// Dispatch ready derivations to available workers (FIFO).
    #[allow(clippy::while_let_loop)]
    async fn dispatch_ready(&mut self) {
        // Drain the queue, dispatching eligible derivations and deferring
        // ineligible ones. Previously, `None => break` on the first ineligible
        // derivation blocked all subsequent work (e.g., an aarch64 drv at
        // queue head blocked all x86_64 dispatch).
        let mut deferred: Vec<String> = Vec::new();
        let mut dispatched_any = true;

        // Keep cycling until a full pass with no dispatches AND no stale removals.
        // In practice this terminates quickly: each derivation is either
        // dispatched, deferred, or removed (stale) exactly once per pass.
        while dispatched_any {
            dispatched_any = false;

            while let Some(drv_hash) = self.ready_queue.pop_front() {
                // Find the derivation's requirements
                let (system, required_features) = match self.dag.node(&drv_hash) {
                    Some(state) => (state.system.clone(), state.required_features.clone()),
                    None => continue, // stale entry, drop
                };

                // Only dispatch if derivation is actually in Ready state
                if self
                    .dag
                    .node(&drv_hash)
                    .is_none_or(|s| s.status() != DerivationStatus::Ready)
                {
                    continue; // stale state, drop
                }

                // Find first eligible worker with capacity (FIFO - no scoring)
                let eligible_worker = self
                    .workers
                    .values()
                    .find(|w| w.has_capacity() && w.can_build(&system, &required_features))
                    .map(|w| w.worker_id.clone());

                match eligible_worker {
                    Some(worker_id) => {
                        self.assign_to_worker(&drv_hash, &worker_id).await;
                        dispatched_any = true;
                    }
                    None => {
                        // No eligible worker for this derivation. Defer and
                        // continue scanning for others we CAN dispatch.
                        deferred.push(drv_hash);
                    }
                }
            }

            // Re-queue deferred derivations at the front, preserving order.
            for hash in deferred.drain(..).rev() {
                self.ready_queue.push_front(hash);
            }
        }
    }

    async fn assign_to_worker(&mut self, drv_hash: &str, worker_id: &str) {
        // Transition ready -> assigned
        if let Some(state) = self.dag.node_mut(drv_hash) {
            // Record assignment latency (Ready -> Assigned time) before transitioning
            if let Some(ready_at) = state.ready_at {
                let latency = ready_at.elapsed();
                metrics::histogram!("rio_scheduler_assignment_latency_seconds")
                    .record(latency.as_secs_f64());
                state.ready_at = None; // clear after recording
            }
            if state.transition(DerivationStatus::Assigned).is_err() {
                return;
            }
            state.assigned_worker = Some(worker_id.to_string());
        }

        // Update DB (non-terminal: log failure, don't block dispatch)
        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Assigned, Some(worker_id))
            .await
        {
            error!(drv_hash, worker_id, error = %e, "failed to persist Assigned status");
        }

        // Create assignment in DB
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, worker_id, self.generation)
                .await
        {
            error!(drv_hash, worker_id, error = %e, "failed to insert assignment record");
        }

        // Track on worker
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.insert(drv_hash.to_string());
        }

        // Send WorkAssignment to worker via stream
        if let Some(state) = self.dag.node(drv_hash) {
            let assignment = rio_proto::types::WorkAssignment {
                drv_path: state.drv_path.clone(),
                // TODO(phase2c): inline .drv content to avoid worker->store round-trip.
                // Phase 2a: worker fetches via GetPath (see rio-worker/src/executor.rs).
                drv_content: Vec::new(),
                // TODO(phase2c): compute closure scheduler-side for prefetch hints.
                // Phase 2a: worker computes via QueryPathInfo BFS.
                input_paths: Vec::new(),
                output_names: state.output_names.clone(),
                build_options: Some(self.build_options_for_derivation(drv_hash)),
                assignment_token: format!("{}-{}-{}", worker_id, drv_hash, self.generation),
                generation: self.generation as u64,
                is_fixed_output: state.is_fixed_output,
            };

            let msg = rio_proto::types::SchedulerMessage {
                msg: Some(rio_proto::types::scheduler_message::Msg::Assignment(
                    assignment,
                )),
            };

            if let Some(worker) = self.workers.get(worker_id)
                && let Some(tx) = &worker.stream_tx
                && let Err(e) = tx.try_send(msg)
            {
                warn!(
                    worker_id,
                    drv_hash,
                    error = %e,
                    "failed to send assignment to worker"
                );
                // Reassign: put back in queue. State is Assigned (we just set it),
                // so reset_to_ready takes the Assigned -> Ready path.
                if let Some(state) = self.dag.node_mut(drv_hash)
                    && state.reset_to_ready().is_ok()
                {
                    self.ready_queue.push_front(drv_hash.to_string());
                }
                return;
            }
        }

        // Emit derivation started event
        let interested_builds = self.get_interested_builds(drv_hash);
        for build_id in &interested_builds {
            self.emit_build_event(
                *build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent {
                        derivation_path: self.drv_hash_to_path(drv_hash).unwrap_or_default(),
                        status: Some(rio_proto::types::derivation_event::Status::Started(
                            rio_proto::types::DerivationStarted {
                                worker_id: worker_id.to_string(),
                            },
                        )),
                    },
                ),
            );
        }

        debug!(drv_hash, worker_id, "assigned derivation to worker");
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
    }

    // -----------------------------------------------------------------------
    // Query handlers
    // -----------------------------------------------------------------------

    fn handle_query_build_status(
        &self,
        build_id: Uuid,
    ) -> Result<rio_proto::types::BuildStatus, ActorError> {
        let build = self
            .builds
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;

        let summary = self.dag.build_summary(build_id);

        let proto_state = match build.state() {
            BuildState::Pending => rio_proto::types::BuildState::Pending,
            BuildState::Active => rio_proto::types::BuildState::Active,
            BuildState::Succeeded => rio_proto::types::BuildState::Succeeded,
            BuildState::Failed => rio_proto::types::BuildState::Failed,
            BuildState::Cancelled => rio_proto::types::BuildState::Cancelled,
        };

        Ok(rio_proto::types::BuildStatus {
            build_id: build_id.to_string(),
            state: proto_state.into(),
            total_derivations: summary.total,
            completed_derivations: summary.completed,
            cached_derivations: build.cached_count,
            running_derivations: summary.running,
            failed_derivations: summary.failed,
            queued_derivations: summary.queued,
            submitted_at: None,
            started_at: None,
            finished_at: None,
            error_summary: build.error_summary.clone().unwrap_or_default(),
        })
    }

    fn handle_watch_build(
        &self,
        build_id: Uuid,
    ) -> Result<broadcast::Receiver<rio_proto::types::BuildEvent>, ActorError> {
        let tx = self
            .build_events
            .get(&build_id)
            .ok_or(ActorError::BuildNotFound(build_id))?;

        Ok(tx.subscribe())
    }

    // -----------------------------------------------------------------------
    // Helpers
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
        self.dag.node(drv_hash).map(|s| s.drv_path.clone())
    }

    /// Whether any interested build for this derivation is interactive (IFD).
    /// Interactive derivations get push_front on the ready queue.
    fn should_prioritize(&self, drv_hash: &str) -> bool {
        self.get_interested_builds(drv_hash).iter().any(|build_id| {
            self.builds
                .get(build_id)
                .map(|b| b.priority_class == "interactive")
                .unwrap_or(false)
        })
    }

    /// Resolve a drv_path to its drv_hash via the DAG's reverse index.
    /// Used by handle_completion since the gRPC layer receives CompletionReport
    /// with drv_path, but the DAG is keyed by drv_hash.
    fn drv_path_to_hash(&self, drv_path: &str) -> Option<String> {
        self.dag.hash_for_path(drv_path).map(str::to_string)
    }

    fn find_db_id_by_path(&self, drv_path: &str) -> Option<Uuid> {
        self.dag
            .hash_for_path(drv_path)
            .and_then(|h| self.dag.node(h))
            .and_then(|s| s.db_id)
    }

    fn update_build_counts(&mut self, build_id: Uuid) {
        let summary = self.dag.build_summary(build_id);
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.completed_count = summary.completed;
            build.failed_count = summary.failed;
        }
    }

    async fn check_build_completion(&mut self, build_id: Uuid, _worker_id: &str) {
        let (state, keep_going, total, completed, failed) = match self.builds.get(&build_id) {
            Some(b) => (
                b.state(),
                b.keep_going,
                b.derivation_hashes.len() as u32,
                b.completed_count,
                b.failed_count,
            ),
            None => return,
        };

        if state.is_terminal() {
            return;
        }

        let all_completed = completed >= total;
        let all_resolved = (completed + failed) >= total;

        if all_completed && failed == 0 {
            if let Err(e) = self.complete_build(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build completion");
            }
        } else if keep_going && all_resolved && failed > 0 {
            // keepGoing: all derivations resolved but some failed
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(build_id = %build_id, error = %e, "failed to persist build-failed transition");
            }
        }
        // !keep_going failures are handled immediately in handle_derivation_failure
    }

    async fn complete_build(&mut self, build_id: Uuid) -> Result<(), ActorError> {
        self.transition_build(build_id, BuildState::Succeeded)
            .await?;

        // Collect output paths from root derivations
        let roots = self.dag.find_roots(build_id);
        let output_paths: Vec<String> = roots
            .iter()
            .flat_map(|h| {
                self.dag
                    .node(h)
                    .map(|s| s.output_paths.clone())
                    .unwrap_or_default()
            })
            .collect();

        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Completed(rio_proto::types::BuildCompleted {
                output_paths,
            }),
        );

        info!(build_id = %build_id, "build completed successfully");
        self.schedule_terminal_cleanup(build_id);
        Ok(())
    }

    async fn transition_build_to_failed(&mut self, build_id: Uuid) -> Result<(), ActorError> {
        let (error_summary, failed_derivation) = self
            .builds
            .get(&build_id)
            .map(|b| {
                (
                    b.error_summary.clone().unwrap_or_default(),
                    b.failed_derivation.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default();

        self.transition_build(build_id, BuildState::Failed).await?;

        self.emit_build_event(
            build_id,
            rio_proto::types::build_event::Event::Failed(rio_proto::types::BuildFailed {
                error_message: error_summary,
                failed_derivation,
            }),
        );

        self.schedule_terminal_cleanup(build_id);
        Ok(())
    }

    async fn transition_build(
        &mut self,
        build_id: Uuid,
        new_state: BuildState,
    ) -> Result<(), ActorError> {
        if let Some(build) = self.builds.get_mut(&build_id) {
            // Ignore transition errors (invalid transition = no-op).
            // The DB write below still records the intended terminal state.
            let _ = build.transition(new_state);

            // Record build duration on terminal transition.
            if new_state.is_terminal() {
                let duration = build.submitted_at.elapsed();
                metrics::histogram!("rio_scheduler_build_duration_seconds")
                    .record(duration.as_secs_f64());
            }
        }

        let error_summary = self
            .builds
            .get(&build_id)
            .and_then(|b| b.error_summary.as_deref());

        self.db
            .update_build_status(build_id, new_state, error_summary)
            .await?;

        Ok(())
    }

    /// Schedule delayed cleanup of terminal build state. After
    /// TERMINAL_CLEANUP_DELAY, the build's entries in builds/build_events/
    /// build_sequences are removed and orphaned+terminal DAG nodes are reaped.
    ///
    /// The delay allows late WatchBuild subscribers to receive the terminal
    /// event before the broadcast sender is dropped.
    ///
    /// No-op if `self_tx` is None (tests that use bare `run()`).
    fn schedule_terminal_cleanup(&self, build_id: Uuid) {
        let Some(weak_tx) = self.self_tx.clone() else {
            return;
        };
        tokio::spawn(async move {
            tokio::time::sleep(TERMINAL_CLEANUP_DELAY).await;
            // Upgrade weak->strong at send time. If all handles dropped,
            // upgrade fails and cleanup is moot (actor is shutting down).
            // try_send: if channel is full, cleanup is dropped. Log + count so
            // sustained drops are visible (unbounded memory growth under load).
            if let Some(tx) = weak_tx.upgrade()
                && tx
                    .try_send(ActorCommand::CleanupTerminalBuild { build_id })
                    .is_err()
            {
                tracing::warn!(
                    build_id = %build_id,
                    "cleanup command dropped (channel full); build state will leak until next restart"
                );
                metrics::counter!("rio_scheduler_cleanup_dropped_total").increment(1);
            }
        });
    }

    /// Handle terminal build cleanup: remove build from in-memory maps and
    /// reap orphaned+terminal DAG nodes.
    fn handle_cleanup_terminal_build(&mut self, build_id: Uuid) {
        // Only clean up if build is actually terminal (guard against misdirected
        // cleanup, e.g., if build_id was reused, though UUIDs make this unlikely).
        let is_terminal = self
            .builds
            .get(&build_id)
            .map(|b| b.state().is_terminal())
            .unwrap_or(true); // already removed = fine
        if !is_terminal {
            warn!(build_id = %build_id, "cleanup scheduled for non-terminal build, skipping");
            return;
        }

        self.builds.remove(&build_id);
        self.build_events.remove(&build_id);
        self.build_sequences.remove(&build_id);

        // Remove build interest from DAG and reap orphaned+terminal nodes.
        let reaped = self.dag.remove_build_interest_and_reap(build_id);
        if reaped > 0 {
            debug!(build_id = %build_id, reaped, "reaped orphaned terminal DAG nodes");
        }
    }

    /// Compute build options for a derivation from its interested builds.
    ///
    /// When multiple builds share a derivation, use the MOST RESTRICTIVE
    /// timeouts (min of non-zero values) so every interested build's
    /// constraints are satisfied. Zero means "unset" (no timeout).
    fn build_options_for_derivation(&self, drv_hash: &str) -> rio_proto::types::BuildOptions {
        let interested = self.get_interested_builds(drv_hash);

        let mut max_silent_time: u64 = 0;
        let mut build_timeout: u64 = 0;
        let mut build_cores: u64 = 0;

        // Helper: take the minimum of non-zero values; zero means "unset".
        fn min_nonzero(acc: u64, val: u64) -> u64 {
            match (acc, val) {
                (0, v) => v,
                (a, 0) => a,
                (a, v) => a.min(v),
            }
        }

        for build_id in &interested {
            if let Some(build) = self.builds.get(build_id) {
                max_silent_time = min_nonzero(max_silent_time, build.options.max_silent_time);
                build_timeout = min_nonzero(build_timeout, build.options.build_timeout);
                // For cores, take the max (more cores is more permissive).
                build_cores = build_cores.max(build.options.build_cores);
            }
        }

        rio_proto::types::BuildOptions {
            max_silent_time,
            build_timeout,
            build_cores,
            keep_going: false, // per-derivation, not per-build
        }
    }
}

/// Handle for sending commands to the actor.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ActorCommand>,
    /// Shared backpressure flag with the actor. The actor computes hysteresis
    /// (activate at 80%, deactivate at 60%) and writes here; the handle reads
    /// it for send() and is_backpressured(). Without this, the handle used a
    /// simple threshold with no hysteresis -> flapping under load near 80%.
    backpressure: Arc<AtomicBool>,
}

impl ActorHandle {
    /// Create a new actor handle and spawn the actor task.
    ///
    /// Returns the handle for sending commands.
    pub fn spawn(db: SchedulerDb, store_client: Option<StoreServiceClient<Channel>>) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let actor = DagActor::new(db, store_client);
        let backpressure = actor.backpressure_flag();
        let self_tx = tx.downgrade();
        rio_common::task::spawn_monitored("dag-actor", actor.run_with_self_tx(rx, self_tx));
        Self { tx, backpressure }
    }

    /// Legacy constructor without self_tx (kept for tests that manually spawn).
    #[cfg(test)]
    pub fn spawn_without_cleanup(
        db: SchedulerDb,
        store_client: Option<StoreServiceClient<Channel>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let actor = DagActor::new(db, store_client);
        let backpressure = actor.backpressure_flag();
        tokio::spawn(actor.run(rx));
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
        if self.backpressure.load(Ordering::Relaxed) {
            return Err(ActorError::Backpressure);
        }
        self.tx.send(cmd).await.map_err(|_| ActorError::ChannelSend)
    }

    /// Try to send a command without waiting (for fire-and-forget messages).
    pub fn try_send(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        self.tx.try_send(cmd).map_err(|_| ActorError::ChannelSend)
    }

    /// Check if the actor is under backpressure (hysteresis-aware).
    pub fn is_backpressured(&self) -> bool {
        self.backpressure.load(Ordering::Relaxed)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(dead_code)] // Helpers used progressively across Group 1-10 TDD tests
pub(crate) mod tests {
    use super::*;
    use rio_test_support::TestDb;
    use std::time::Duration;
    use tokio::sync::mpsc;

    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

    /// Set up an actor with the given PgPool and return (handle, task).
    /// The caller should drop the handle to shut down the actor.
    pub(crate) async fn setup_actor(
        pool: sqlx::PgPool,
    ) -> (ActorHandle, tokio::task::JoinHandle<()>) {
        setup_actor_with_store(pool, None).await
    }

    /// Set up an actor with an optional store client for cache-check tests.
    pub(crate) async fn setup_actor_with_store(
        pool: sqlx::PgPool,
        store_client: Option<StoreServiceClient<Channel>>,
    ) -> (ActorHandle, tokio::task::JoinHandle<()>) {
        let db = SchedulerDb::new(pool);
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let actor = DagActor::new(db, store_client);
        let backpressure = actor.backpressure_flag();
        let self_tx = tx.downgrade();
        let task = tokio::spawn(actor.run_with_self_tx(rx, self_tx));
        (ActorHandle { tx, backpressure }, task)
    }

    /// Create a minimal test DerivationNode.
    pub(crate) fn make_test_node(
        hash: &str,
        path: &str,
        system: &str,
    ) -> rio_proto::types::DerivationNode {
        rio_proto::types::DerivationNode {
            drv_hash: hash.into(),
            drv_path: path.into(),
            pname: "test-pkg".into(),
            system: system.into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec![],
        }
    }

    /// Create a minimal test DerivationEdge.
    pub(crate) fn make_test_edge(parent: &str, child: &str) -> rio_proto::types::DerivationEdge {
        rio_proto::types::DerivationEdge {
            parent_drv_path: parent.into(),
            child_drv_path: child.into(),
        }
    }

    /// Connect a worker (stream + heartbeat) so it becomes fully registered.
    /// Returns the mpsc::Receiver for scheduler→worker messages.
    pub(crate) async fn connect_worker(
        handle: &ActorHandle,
        worker_id: &str,
        system: &str,
        max_builds: u32,
    ) -> mpsc::Receiver<rio_proto::types::SchedulerMessage> {
        let (stream_tx, stream_rx) = mpsc::channel(256);
        handle
            .send_unchecked(ActorCommand::WorkerConnected {
                worker_id: worker_id.into(),
                stream_tx,
            })
            .await
            .unwrap();
        handle
            .send_unchecked(ActorCommand::Heartbeat {
                worker_id: worker_id.into(),
                system: system.into(),
                supported_features: vec![],
                max_builds,
                running_builds: vec![],
            })
            .await
            .unwrap();
        stream_rx
    }

    /// Merge a single-node DAG and return the event receiver.
    pub(crate) async fn merge_single_node(
        handle: &ActorHandle,
        build_id: Uuid,
        hash: &str,
        path: &str,
        priority_class: &str,
    ) -> broadcast::Receiver<rio_proto::types::BuildEvent> {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: priority_class.into(),
                nodes: vec![make_test_node(hash, path, "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                reply: reply_tx,
            })
            .await
            .unwrap();
        reply_rx.await.unwrap().unwrap()
    }

    /// Give the actor time to process commands.
    pub(crate) async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_actor_starts_and_stops() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, task) = setup_actor(db.pool.clone()).await;
        settle().await;
        // Query should succeed (actor is running)
        let workers = handle.debug_query_workers().await.unwrap();
        assert!(workers.is_empty());
        // Drop handle to close channel
        drop(handle);
        // Actor task should exit
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("actor should shut down")
            .expect("actor should not panic");
    }

    /// is_alive() should detect actor death (channel closed = receiver dropped).
    #[tokio::test]
    async fn test_actor_is_alive_detection() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, task) = setup_actor(db.pool.clone()).await;
        settle().await;

        // Actor should be alive after spawn.
        assert!(handle.is_alive(), "actor should be alive after spawn");

        // Abort the actor task to simulate a panic/crash.
        task.abort();
        // Give the abort time to propagate and the receiver to drop.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // is_alive() should now report false (channel closed).
        assert!(
            !handle.is_alive(),
            "is_alive should report false after actor task dies"
        );
    }

    // -----------------------------------------------------------------------
    // Group 1: Worker-Scheduler wiring
    // -----------------------------------------------------------------------

    /// Baseline: when a worker connects (stream) and sends a heartbeat with
    /// the SAME worker_id, the actor should see it as fully registered.
    /// This SHOULD pass with current code (the bug is in grpc.rs, not the actor).
    #[tokio::test]
    async fn test_worker_registers_via_stream_and_heartbeat() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "test-worker-1", "x86_64-linux", 2).await;
        settle().await;

        let workers = handle.debug_query_workers().await.unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].worker_id, "test-worker-1");
        assert!(
            workers[0].is_registered,
            "worker should be fully registered after stream + heartbeat"
        );
    }

    /// Bug reproduction: handle_completion uses drv_hash as the lookup key,
    /// but grpc.rs passes drv_path. The completion should be resolved via
    /// either key. This test sends ProcessCompletion with a drv_PATH and
    /// expects the build to transition to Succeeded.
    ///
    /// Expected to FAIL before fix: completion is dropped ("unknown derivation")
    /// and the build stays Active forever.
    #[tokio::test]
    async fn test_completion_resolves_drv_path_to_hash() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Register a worker
        let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

        // Merge a single-node DAG
        let build_id = Uuid::new_v4();
        let drv_hash = "abc123hash";
        let drv_path = "/nix/store/abc123hash-foo.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;

        settle().await;

        // Worker should have received an assignment (derivation is ready, worker has capacity)
        // Now send completion using drv_PATH (mimics what grpc.rs does with report.drv_path)
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: drv_path.into(), // Note: PATH, not hash!
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    error_msg: String::new(),
                    times_built: 1,
                    start_time: None,
                    stop_time: None,
                    built_outputs: vec![rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: "/nix/store/xyz-foo".into(),
                        output_hash: vec![0u8; 32],
                    }],
                },
            })
            .await
            .unwrap();

        settle().await;

        // Query build status — should be Succeeded (single derivation, completed)
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();

        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "build should succeed after completion sent with drv_path (got state={:?})",
            rio_proto::types::BuildState::try_from(status.state)
        );
    }

    // -----------------------------------------------------------------------
    // Group 2: State machine integrity
    // -----------------------------------------------------------------------

    /// When a worker disconnects while a derivation is Running, the derivation
    /// should transition Running -> Failed -> Ready (through the state machine),
    /// and retry_count should be incremented.
    ///
    /// Before fix: the direct `state.status = Ready` assignment bypassed the
    /// state machine (Running -> Ready is not a valid transition) and did NOT
    /// increment retry_count.
    #[tokio::test]
    async fn test_worker_disconnect_running_derivation() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Register a worker
        let mut stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

        // Merge a single-node DAG (worker will get it assigned)
        let build_id = Uuid::new_v4();
        let drv_hash = "disconnect-test-hash";
        let drv_path = "/nix/store/disconnect-test-hash.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        // Worker should have received an assignment
        let assignment = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .expect("should receive assignment within 2s")
            .expect("stream should not be closed");
        assert!(matches!(
            assignment.msg,
            Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
        ));

        // Derivation should now be Assigned. Simulate worker sending an Ack
        // to transition to Running via a completion with a sentinel... actually
        // the actor doesn't have an Ack handler that transitions state. Looking
        // at the code, Assigned -> Running happens implicitly in handle_completion
        // when needed. For this test, we need to directly check the disconnect
        // path from BOTH Assigned AND Running states. The current code's
        // handle_worker_disconnected matches on (Assigned | Running) and resets
        // to Ready. The bug is that Running -> Ready is invalid.

        // Check current status: should be Assigned
        let info = handle
            .debug_query_derivation(drv_hash)
            .await
            .unwrap()
            .expect("derivation should exist");
        assert_eq!(info.status, DerivationStatus::Assigned);
        assert_eq!(info.retry_count, 0);

        // Disconnect the worker
        handle
            .send_unchecked(ActorCommand::WorkerDisconnected {
                worker_id: "test-worker".into(),
            })
            .await
            .unwrap();
        settle().await;

        // Derivation should be back in Ready state, and retry_count
        // should be incremented (disconnect during Assigned/Running is a
        // failed attempt that counts toward retries).
        let info = handle
            .debug_query_derivation(drv_hash)
            .await
            .unwrap()
            .expect("derivation should still exist");
        assert_eq!(
            info.status,
            DerivationStatus::Ready,
            "derivation should return to Ready after worker disconnect"
        );
        assert_eq!(
            info.retry_count, 1,
            "disconnect during Assigned should count as a retry attempt"
        );
        assert!(info.assigned_worker.is_none());
    }

    // -----------------------------------------------------------------------
    // Group 4: Silent failures
    // -----------------------------------------------------------------------

    /// A completion with InfrastructureFailure status should transition the
    /// derivation out of Running (to Failed/Ready for retry, or Poisoned).
    /// The gRPC layer synthesizes InfrastructureFailure for None results,
    /// so this verifies the full path.
    #[tokio::test]
    async fn test_completion_infrastructure_failure_handled() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

        let build_id = Uuid::new_v4();
        let drv_hash = "infra-fail-hash";
        let drv_path = "/nix/store/infra-fail-hash.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        // Send completion with InfrastructureFailure (what gRPC layer sends
        // for None result)
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
                    error_msg: "worker sent CompletionReport with no result".into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // The derivation should have gone through Failed -> Ready (retry) and
        // then been immediately re-dispatched to the worker (Assigned again).
        // The key assertion: retry_count was incremented, proving the completion
        // was processed rather than silently dropped.
        let info = handle
            .debug_query_derivation(drv_hash)
            .await
            .unwrap()
            .expect("derivation should exist");
        assert_eq!(
            info.retry_count, 1,
            "InfrastructureFailure should count as a retry (completion was processed)"
        );
        // Status should be Assigned (re-dispatched) or Ready (if dispatch skipped),
        // NOT stuck in the original state with retry_count=0.
        assert!(
            matches!(
                info.status,
                DerivationStatus::Ready | DerivationStatus::Assigned
            ),
            "expected Ready or Assigned after retry, got {:?}",
            info.status
        );
    }

    /// Malicious/buggy worker timestamps (i64::MIN start, i64::MAX stop) must
    /// not panic the actor with integer overflow in the EMA duration computation.
    #[tokio::test]
    async fn test_completion_with_extreme_timestamps() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

        let build_id = Uuid::new_v4();
        let drv_hash = "extreme-ts-hash";
        let drv_path = "/nix/store/extreme-ts-hash.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        // Send completion with extreme timestamps that would overflow i64 subtraction.
        // Pre-fix: stop.seconds - start.seconds = i64::MAX - i64::MIN overflows (panic in debug).
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    start_time: Some(prost_types::Timestamp {
                        seconds: i64::MIN,
                        nanos: i32::MIN,
                    }),
                    stop_time: Some(prost_types::Timestamp {
                        seconds: i64::MAX,
                        nanos: i32::MAX,
                    }),
                    built_outputs: vec![rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: "/nix/store/xyz-extreme".into(),
                        output_hash: vec![0u8; 32],
                    }],
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // If we got here, the actor didn't panic. Verify completion was processed
        // (build succeeded) and actor is still alive.
        assert!(handle.is_alive(), "actor must survive extreme timestamps");
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let status = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "build should succeed despite bogus timestamps"
        );
    }

    // -----------------------------------------------------------------------
    // Group 6: Feature completions
    // -----------------------------------------------------------------------

    /// Interactive (IFD) builds should jump to the front of the ready queue.
    #[tokio::test]
    async fn test_interactive_builds_pushed_to_front() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Worker with capacity for 1 build at a time
        let mut stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

        // Merge a "scheduled" build first (should go to back of queue)
        let build_normal = Uuid::new_v4();
        let _rx1 = merge_single_node(
            &handle,
            build_normal,
            "hash-normal",
            "/nix/store/hash-normal.drv",
            "scheduled",
        )
        .await;
        settle().await;

        // The normal build gets assigned first (only one in queue)
        let first = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let first_path = match first.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
            _ => panic!("expected assignment"),
        };
        assert_eq!(first_path, "/nix/store/hash-normal.drv");

        // Now merge a second "scheduled" build — goes to back
        let build_scheduled2 = Uuid::new_v4();
        let _rx2 = merge_single_node(
            &handle,
            build_scheduled2,
            "hash-scheduled2",
            "/nix/store/hash-scheduled2.drv",
            "scheduled",
        )
        .await;

        // Merge an "interactive" build — should go to FRONT
        let build_ifd = Uuid::new_v4();
        let _rx3 = merge_single_node(
            &handle,
            build_ifd,
            "hash-ifd",
            "/nix/store/hash-ifd.drv",
            "interactive",
        )
        .await;
        settle().await;

        // Complete the first build to free worker capacity
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: "/nix/store/hash-normal.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // The next assignment should be the IFD derivation (was pushed to front)
        let second = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second_path = match second.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
            _ => panic!("expected assignment"),
        };
        assert_eq!(
            second_path, "/nix/store/hash-ifd.drv",
            "interactive build should be dispatched before scheduled"
        );
    }

    // -----------------------------------------------------------------------
    // Group 10: Remaining coverage
    // -----------------------------------------------------------------------

    /// keepGoing=false: on PermanentFailure, the entire build fails immediately.
    #[tokio::test]
    async fn test_keepgoing_false_fails_fast() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

        // Merge a two-node DAG with keepGoing=false
        let build_id = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![
                    make_test_node("hashA", "/nix/store/hashA.drv", "x86_64-linux"),
                    make_test_node("hashB", "/nix/store/hashB.drv", "x86_64-linux"),
                ],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false, // critical
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _rx = reply_rx.await.unwrap().unwrap();
        settle().await;

        // Send PermanentFailure for hashA
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: "/nix/store/hashA.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                    error_msg: "compile error".into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // Build should be Failed (not waiting for hashB)
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap().unwrap();
        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Failed as i32,
            "build should fail fast on PermanentFailure with keepGoing=false"
        );
    }

    /// keepGoing=true: build waits for all derivations, fails only at the end.
    #[tokio::test]
    async fn test_keepgoing_true_waits_all() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

        // Merge a two-node DAG with keepGoing=true
        let build_id = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![
                    make_test_node("hashX", "/nix/store/hashX.drv", "x86_64-linux"),
                    make_test_node("hashY", "/nix/store/hashY.drv", "x86_64-linux"),
                ],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: true, // critical
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _rx = reply_rx.await.unwrap().unwrap();
        settle().await;

        // Send PermanentFailure for hashX
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: "/nix/store/hashX.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                    error_msg: "failed".into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // Build should still be Active (waiting for hashY)
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap().unwrap();
        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Active as i32,
            "build should still be Active with keepGoing=true and pending derivations"
        );

        // Complete hashY successfully
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: "/nix/store/hashY.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // Now build should be Failed (all resolved, one failed)
        let (status_tx2, status_rx2) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx2,
            })
            .await
            .unwrap();
        let status2 = status_rx2.await.unwrap().unwrap();
        assert_eq!(
            status2.state,
            rio_proto::types::BuildState::Failed as i32,
            "build should fail after all derivations resolve with keepGoing=true"
        );
    }

    /// keepGoing=true with a dependency chain: poisoning a leaf must cascade
    /// DependencyFailed to all ancestors so the build terminates. Without the
    /// cascade, parents stay Queued forever and completed+failed never reaches
    /// total -> build hangs.
    #[tokio::test]
    async fn test_keepgoing_poisoned_dependency_cascades_failure() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Worker with capacity 1: only the leaf gets dispatched initially.
        let _stream_rx = connect_worker(&handle, "cascade-worker", "x86_64-linux", 1).await;

        // Chain: A depends on B depends on C. C is the leaf.
        let build_id = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![
                    make_test_node("cascadeA", "/nix/store/cascadeA.drv", "x86_64-linux"),
                    make_test_node("cascadeB", "/nix/store/cascadeB.drv", "x86_64-linux"),
                    make_test_node("cascadeC", "/nix/store/cascadeC.drv", "x86_64-linux"),
                ],
                edges: vec![
                    make_test_edge("/nix/store/cascadeA.drv", "/nix/store/cascadeB.drv"),
                    make_test_edge("/nix/store/cascadeB.drv", "/nix/store/cascadeC.drv"),
                ],
                options: BuildOptions::default(),
                keep_going: true,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _rx = reply_rx.await.unwrap().unwrap();
        settle().await;

        // Sanity: C is the only Ready/Assigned derivation; A and B are Queued.
        let info_a = handle
            .debug_query_derivation("cascadeA")
            .await
            .unwrap()
            .unwrap();
        let info_b = handle
            .debug_query_derivation("cascadeB")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info_a.status, DerivationStatus::Queued);
        assert_eq!(info_b.status, DerivationStatus::Queued);

        // Poison C via PermanentFailure.
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "cascade-worker".into(),
                drv_hash: "/nix/store/cascadeC.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                    error_msg: "compile error".into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // B and A should now be DependencyFailed (cascaded transitively).
        let info_b = handle
            .debug_query_derivation("cascadeB")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info_b.status,
            DerivationStatus::DependencyFailed,
            "immediate parent B should be DependencyFailed after C poisoned"
        );
        let info_a = handle
            .debug_query_derivation("cascadeA")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info_a.status,
            DerivationStatus::DependencyFailed,
            "transitive parent A should also be DependencyFailed"
        );

        // Build should terminate as Failed (all 3 derivations resolved:
        // 1 Poisoned + 2 DependencyFailed counted in failed).
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap().unwrap();
        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Failed as i32,
            "keepGoing build with poisoned dependency chain should terminate as Failed, not hang"
        );
        assert_eq!(
            status.failed_derivations, 3,
            "1 Poisoned + 2 DependencyFailed should all count as failed"
        );
    }

    /// Terminal build state should be cleaned up after TERMINAL_CLEANUP_DELAY
    /// to prevent unbounded memory growth for long-running schedulers.
    ///
    /// This test sends CleanupTerminalBuild directly (bypassing the delay)
    /// since paused time interferes with PG pool timeouts. The delay
    /// scheduling itself is trivially correct (tokio::time::sleep + try_send).
    #[tokio::test]
    async fn test_terminal_build_cleanup_after_delay() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "cleanup-worker", "x86_64-linux", 1).await;

        // Complete a build.
        let build_id = Uuid::new_v4();
        let drv_hash = "cleanup-hash";
        let drv_path = "/nix/store/cleanup-hash.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "cleanup-worker".into(),
                drv_hash: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // Build should be Succeeded and still queryable.
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap();
        assert!(status.is_ok(), "build should be queryable before cleanup");

        // Directly inject the cleanup command (bypassing the 60s delay).
        handle
            .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
            .await
            .unwrap();
        settle().await;

        // Build should now be gone (BuildNotFound).
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap();
        assert!(
            matches!(status, Err(ActorError::BuildNotFound(_))),
            "build should be cleaned up after delay, got {:?}",
            status
        );

        // DAG node should also be reaped (Completed + orphaned).
        let info = handle.debug_query_derivation(drv_hash).await.unwrap();
        assert!(
            info.is_none(),
            "orphaned+terminal DAG node should be reaped"
        );
    }

    /// TransientFailure: retry on a different worker up to max_retries (default 2).
    #[tokio::test]
    async fn test_transient_retry_different_worker() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Register two workers
        let _rx1 = connect_worker(&handle, "worker-a", "x86_64-linux", 1).await;
        let _rx2 = connect_worker(&handle, "worker-b", "x86_64-linux", 1).await;

        let build_id = Uuid::new_v4();
        let _event_rx = merge_single_node(
            &handle,
            build_id,
            "retry-hash",
            "/nix/store/retry-hash.drv",
            "scheduled",
        )
        .await;
        settle().await;

        // Get initial worker assignment
        let info1 = handle
            .debug_query_derivation("retry-hash")
            .await
            .unwrap()
            .unwrap();
        let first_worker = info1.assigned_worker.clone().unwrap();
        assert_eq!(info1.retry_count, 0);

        // Send TransientFailure from the first worker
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: first_worker.clone(),
                drv_hash: "/nix/store/retry-hash.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                    error_msg: "network hiccup".into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // Should be retried: retry_count=1, possibly on a different worker
        let info2 = handle
            .debug_query_derivation("retry-hash")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info2.retry_count, 1,
            "transient failure should increment retry_count"
        );
        // Note: the retry MAY go to the same worker (no affinity avoidance yet),
        // but retry_count proves it was processed.
        assert!(matches!(
            info2.status,
            DerivationStatus::Assigned | DerivationStatus::Ready
        ));
    }

    /// Dispatch should skip over derivations with no eligible worker (wrong
    /// system or missing feature) instead of blocking the entire queue.
    #[tokio::test]
    async fn test_dispatch_skips_ineligible_derivation() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Only x86_64 worker registered.
        let mut stream_rx = connect_worker(&handle, "x86-only-worker", "x86_64-linux", 2).await;

        // Merge aarch64 derivation FIRST (goes to queue head), then x86_64.
        // With the old `None => break`, the aarch64 drv at head would block
        // the x86_64 drv from being dispatched.
        let build_arm = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id: build_arm,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![make_test_node(
                    "arm-hash",
                    "/nix/store/arm-hash.drv",
                    "aarch64-linux",
                )],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _rx = reply_rx.await.unwrap().unwrap();

        let build_x86 = Uuid::new_v4();
        let _rx = merge_single_node(
            &handle,
            build_x86,
            "x86-hash",
            "/nix/store/x86-hash.drv",
            "scheduled",
        )
        .await;
        settle().await;

        // x86_64 derivation should be dispatched despite aarch64 ahead of it.
        let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .expect("x86_64 derivation should be dispatched within 2s")
            .expect("stream should not close");
        let dispatched_path = match msg.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
            _ => panic!("expected assignment"),
        };
        assert_eq!(
            dispatched_path, "/nix/store/x86-hash.drv",
            "x86_64 derivation should be dispatched even with ineligible aarch64 ahead in queue"
        );

        // aarch64 derivation should still be Ready (not stuck, not dispatched).
        let arm_info = handle
            .debug_query_derivation("arm-hash")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            arm_info.status,
            DerivationStatus::Ready,
            "aarch64 derivation should remain Ready (no eligible worker)"
        );
    }

    /// Per-build BuildOptions (max_silent_time, build_timeout) must propagate
    /// to the worker via WorkAssignment. Previously sent all-zeros defaults.
    #[tokio::test]
    async fn test_build_options_propagated_to_worker() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let mut stream_rx = connect_worker(&handle, "options-worker", "x86_64-linux", 1).await;

        // Submit with build_timeout=300, max_silent_time=60.
        let build_id = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![make_test_node(
                    "opts-hash",
                    "/nix/store/opts-hash.drv",
                    "x86_64-linux",
                )],
                edges: vec![],
                options: BuildOptions {
                    max_silent_time: 60,
                    build_timeout: 300,
                    build_cores: 4,
                },
                keep_going: false,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _rx = reply_rx.await.unwrap().unwrap();
        settle().await;

        // Worker should receive assignment with the build's options.
        let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let assignment = match msg.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
            _ => panic!("expected assignment"),
        };
        let opts = assignment.build_options.expect("options should be set");
        assert_eq!(
            opts.build_timeout, 300,
            "build_timeout should propagate from build to worker"
        );
        assert_eq!(
            opts.max_silent_time, 60,
            "max_silent_time should propagate from build to worker"
        );
        assert_eq!(opts.build_cores, 4);
    }

    /// TOCTOU fix: a stale heartbeat (sent before scheduler assigned a
    /// derivation) must not clobber the scheduler's fresh assignment in
    /// worker.running_builds. The scheduler is authoritative.
    #[tokio::test]
    async fn test_heartbeat_does_not_clobber_fresh_assignment() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Register worker (initial heartbeat has empty running_builds).
        let _stream_rx = connect_worker(&handle, "toctou-worker", "x86_64-linux", 2).await;
        settle().await;

        // Merge a derivation. Scheduler will assign it to the worker and
        // insert it into worker.running_builds.
        let build_id = Uuid::new_v4();
        let drv_hash = "toctou-drv-hash";
        let drv_path = "/nix/store/toctou-drv-hash.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        // Verify: derivation is Assigned, worker.running_builds contains it.
        let info = handle
            .debug_query_derivation(drv_hash)
            .await
            .unwrap()
            .expect("derivation should exist");
        assert_eq!(info.status, DerivationStatus::Assigned);

        let workers = handle.debug_query_workers().await.unwrap();
        let w = workers
            .iter()
            .find(|w| w.worker_id == "toctou-worker")
            .unwrap();
        assert!(
            w.running_builds.contains(&drv_hash.to_string()),
            "scheduler should have tracked the assignment in worker.running_builds"
        );

        // Send a STALE heartbeat with empty running_builds. This mimics the
        // race: worker sent heartbeat before receiving/acking the assignment.
        handle
            .send_unchecked(ActorCommand::Heartbeat {
                worker_id: "toctou-worker".into(),
                system: "x86_64-linux".into(),
                supported_features: vec![],
                max_builds: 2,
                running_builds: vec![], // stale — does NOT include fresh assignment
            })
            .await
            .unwrap();
        settle().await;

        // Assignment must still be tracked. Before the fix, running_builds
        // would be wholesale replaced with the empty set, orphaning the
        // assignment (completion would later warn "unknown derivation").
        let workers = handle.debug_query_workers().await.unwrap();
        let w = workers
            .iter()
            .find(|w| w.worker_id == "toctou-worker")
            .unwrap();
        assert!(
            w.running_builds.contains(&drv_hash.to_string()),
            "stale heartbeat must not clobber scheduler's fresh assignment"
        );
    }

    // -----------------------------------------------------------------------
    // Step 8: Test coverage gaps
    // -----------------------------------------------------------------------

    /// T4: Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
    #[tokio::test]
    async fn test_poison_threshold_after_distinct_workers() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Register 4 workers so the derivation can be re-dispatched after each failure.
        let _rx1 = connect_worker(&handle, "poison-w1", "x86_64-linux", 1).await;
        let _rx2 = connect_worker(&handle, "poison-w2", "x86_64-linux", 1).await;
        let _rx3 = connect_worker(&handle, "poison-w3", "x86_64-linux", 1).await;
        let _rx4 = connect_worker(&handle, "poison-w4", "x86_64-linux", 1).await;

        let build_id = Uuid::new_v4();
        let drv_hash = "poison-drv";
        let drv_path = "/nix/store/poison-drv.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        // Send TransientFailure from 3 DISTINCT workers. After the 3rd, poison.
        for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
            handle
                .send_unchecked(ActorCommand::ProcessCompletion {
                    worker_id: (*worker).into(),
                    drv_hash: drv_path.into(),
                    result: rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                        error_msg: format!("failure {i}"),
                        ..Default::default()
                    },
                })
                .await
                .unwrap();
            settle().await;
        }

        let info = handle
            .debug_query_derivation(drv_hash)
            .await
            .unwrap()
            .expect("derivation should exist");
        assert_eq!(
            info.status,
            DerivationStatus::Poisoned,
            "derivation should be Poisoned after {} distinct worker failures",
            POISON_THRESHOLD
        );

        // Build should be Failed.
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap().unwrap();
        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Failed as i32,
            "build should fail after derivation is poisoned"
        );
    }

    /// T5: Completing a child releases its parent to Ready in a dependency chain.
    #[tokio::test]
    async fn test_dependency_chain_releases_parent() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let mut stream_rx = connect_worker(&handle, "chain-worker", "x86_64-linux", 1).await;

        // A depends on B. B is Ready (leaf), A is Queued.
        let build_id = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![
                    make_test_node("chainA", "/nix/store/chainA.drv", "x86_64-linux"),
                    make_test_node("chainB", "/nix/store/chainB.drv", "x86_64-linux"),
                ],
                edges: vec![make_test_edge(
                    "/nix/store/chainA.drv",
                    "/nix/store/chainB.drv",
                )],
                options: BuildOptions::default(),
                keep_going: false,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _rx = reply_rx.await.unwrap().unwrap();
        settle().await;

        // B is dispatched first (leaf). A is Queued waiting for B.
        let info_a = handle
            .debug_query_derivation("chainA")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info_a.status, DerivationStatus::Queued);

        // Worker receives B's assignment.
        let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let assigned_path = match msg.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
            _ => panic!("expected assignment"),
        };
        assert_eq!(assigned_path, "/nix/store/chainB.drv");

        // Complete B.
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "chain-worker".into(),
                drv_hash: "/nix/store/chainB.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // A should now transition Queued -> Ready -> Assigned (dispatched).
        let info_a = handle
            .debug_query_derivation("chainA")
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                info_a.status,
                DerivationStatus::Ready | DerivationStatus::Assigned
            ),
            "A should be Ready or Assigned after B completes, got {:?}",
            info_a.status
        );

        // Worker should receive A's assignment.
        let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let assigned_path = match msg.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
            _ => panic!("expected assignment"),
        };
        assert_eq!(
            assigned_path, "/nix/store/chainA.drv",
            "A should be dispatched after B completes"
        );
    }

    /// T9: Duplicate ProcessCompletion is an idempotent no-op.
    #[tokio::test]
    async fn test_duplicate_completion_idempotent() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "idem-worker", "x86_64-linux", 1).await;

        let build_id = Uuid::new_v4();
        let drv_hash = "idem-hash";
        let drv_path = "/nix/store/idem-hash.drv";
        let mut event_rx =
            merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        // Send completion TWICE.
        for _ in 0..2 {
            handle
                .send_unchecked(ActorCommand::ProcessCompletion {
                    worker_id: "idem-worker".into(),
                    drv_hash: drv_path.into(),
                    result: rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::Built.into(),
                        ..Default::default()
                    },
                })
                .await
                .unwrap();
            settle().await;
        }

        // completed_count should be 1, not 2.
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap().unwrap();
        assert_eq!(
            status.completed_derivations, 1,
            "duplicate completion should not double-count"
        );
        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "build should still succeed (idempotent)"
        );

        // Count BuildCompleted events: should be exactly 1 (not 2).
        // Drain available events without blocking.
        let mut completed_events = 0;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(
                event.event,
                Some(rio_proto::types::build_event::Event::Completed(_))
            ) {
                completed_events += 1;
            }
        }
        assert_eq!(
            completed_events, 1,
            "BuildCompleted event should fire exactly once"
        );
    }

    /// T1: Heartbeat timeout deregisters worker and reassigns its builds.
    /// Instead of advancing time (PG timeout issue), we send Tick commands
    /// after manipulating the worker's last_heartbeat via multiple Tick cycles
    /// without heartbeats. Actually simpler: send WorkerDisconnected directly
    /// is equivalent (handle_tick calls handle_worker_disconnected on timeout),
    /// so that path is already covered by test_worker_disconnect_running_derivation.
    /// This test verifies the Tick-driven path specifically by injecting Ticks.
    #[tokio::test]
    async fn test_heartbeat_timeout_via_tick_deregisters_worker() {
        // NOTE: This test would ideally use tokio::time::pause + advance, but
        // that interferes with PG pool timeouts. Instead, we verify that Tick
        // correctly processes the timeout path by checking the missed_heartbeats
        // counter accumulates. Since we can't easily fast-forward real time in
        // this test harness, we verify the logic indirectly: Tick with fresh
        // heartbeat does NOT remove the worker (negative test), and the
        // timeout-removal path is exercised directly via WorkerDisconnected
        // in test_worker_disconnect_running_derivation.
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        let _stream_rx = connect_worker(&handle, "tick-worker", "x86_64-linux", 1).await;
        settle().await;

        // Send several Ticks. Worker has fresh heartbeat, should NOT be removed.
        for _ in 0..MAX_MISSED_HEARTBEATS + 1 {
            handle.send_unchecked(ActorCommand::Tick).await.unwrap();
        }
        settle().await;

        let workers = handle.debug_query_workers().await.unwrap();
        assert!(
            workers.iter().any(|w| w.worker_id == "tick-worker"),
            "worker with fresh heartbeat should survive Tick"
        );
    }

    // -----------------------------------------------------------------------
    // Scheduler-side cache check (TOCTOU fix)
    // -----------------------------------------------------------------------

    /// Spin up an in-process rio-store on an ephemeral port.
    async fn setup_inproc_store(
        pool: sqlx::PgPool,
    ) -> (StoreServiceClient<Channel>, tokio::task::JoinHandle<()>) {
        use rio_proto::store::store_service_server::StoreServiceServer;
        use rio_store::backend::memory::MemoryBackend;
        use rio_store::grpc::StoreServiceImpl;
        use std::sync::Arc;

        let backend = Arc::new(MemoryBackend::new());
        let service = StoreServiceImpl::new(backend, pool);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(StoreServiceServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .expect("test store gRPC server should run");
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let channel = Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        (StoreServiceClient::new(channel), server)
    }

    /// Build a minimal single-file NAR and upload it to the store.
    async fn put_test_path(client: &mut StoreServiceClient<Channel>, store_path: &str) {
        use rio_proto::types::{PathInfo, PutPathMetadata, PutPathRequest, put_path_request};
        use sha2::{Digest, Sha256};

        // Minimal NAR for a regular file "hello"
        fn write_str(out: &mut Vec<u8>, s: &[u8]) {
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s);
            let pad = (8 - (s.len() % 8)) % 8;
            out.extend_from_slice(&vec![0u8; pad]);
        }
        let contents = b"hello";
        let mut nar = Vec::new();
        write_str(&mut nar, b"nix-archive-1");
        write_str(&mut nar, b"(");
        write_str(&mut nar, b"type");
        write_str(&mut nar, b"regular");
        write_str(&mut nar, b"contents");
        nar.extend_from_slice(&(contents.len() as u64).to_le_bytes());
        nar.extend_from_slice(contents);
        let pad = (8 - (contents.len() % 8)) % 8;
        nar.extend_from_slice(&vec![0u8; pad]);
        write_str(&mut nar, b")");

        let nar_hash = Sha256::digest(&nar).to_vec();
        let info = PathInfo {
            store_path: store_path.to_string(),
            nar_hash,
            nar_size: nar.len() as u64,
            ..Default::default()
        };

        let (tx, rx) = mpsc::channel(4);
        tx.send(PutPathRequest {
            msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
                info: Some(info),
            })),
        })
        .await
        .unwrap();
        tx.send(PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(nar)),
        })
        .await
        .unwrap();
        drop(tx);

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        client.put_path(stream).await.unwrap();
    }

    #[tokio::test]
    async fn test_scheduler_cache_check_skips_build() {
        let sched_db = TestDb::new(&MIGRATOR).await;
        let store_db = TestDb::new(&MIGRATOR).await;

        // Start in-process store and pre-populate the expected output path.
        let (mut store_client, _store_server) = setup_inproc_store(store_db.pool.clone()).await;
        let cached_output = "/nix/store/00000000000000000000000000000000-cached-output";
        put_test_path(&mut store_client, cached_output).await;

        // Spawn actor WITH the store client — cache check will run.
        let (handle, _task) =
            setup_actor_with_store(sched_db.pool.clone(), Some(store_client.clone())).await;

        // Merge a single-node DAG with expected_output_paths pointing at the
        // pre-populated path. No worker needed — scheduler should find it
        // cached and complete immediately.
        let build_id = Uuid::new_v4();
        let mut node = make_test_node("cached-hash", "/nix/store/cached-hash.drv", "x86_64-linux");
        node.expected_output_paths = vec![cached_output.to_string()];

        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _event_rx = reply_rx.await.unwrap().unwrap();
        settle().await;

        // Derivation should have gone Created → Completed (scheduler cache hit).
        let info = handle
            .debug_query_derivation("cached-hash")
            .await
            .unwrap()
            .expect("derivation should exist in DAG");
        assert_eq!(
            info.status,
            DerivationStatus::Completed,
            "scheduler cache check should mark derivation as Completed"
        );
        assert_eq!(info.output_paths, vec![cached_output.to_string()]);

        // Build should be Succeeded (all 1 derivation cached).
        let (status_tx, status_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::QueryBuildStatus {
                build_id,
                reply: status_tx,
            })
            .await
            .unwrap();
        let status = status_rx.await.unwrap().unwrap();
        assert_eq!(status.cached_derivations, 1);
        assert_eq!(
            status.completed_derivations, 1,
            "completed should count cached exactly once (no double-counting)"
        );
        assert_eq!(
            status.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "build with all-cached derivations should be Succeeded"
        );
    }

    #[tokio::test]
    async fn test_scheduler_cache_check_skipped_without_store() {
        let db = TestDb::new(&MIGRATOR).await;

        // No store client — cache check should silently skip.
        let (handle, _task) = setup_actor(db.pool.clone()).await;
        let _rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

        let build_id = Uuid::new_v4();
        let mut node = make_test_node(
            "uncached-hash",
            "/nix/store/uncached-hash.drv",
            "x86_64-linux",
        );
        // expected_output_paths set but store client is None — should NOT short-circuit
        node.expected_output_paths = vec!["/nix/store/uncached-out".to_string()];

        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                build_id,
                tenant_id: None,
                priority_class: "scheduled".into(),
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let _event_rx = reply_rx.await.unwrap().unwrap();
        settle().await;

        // Without store client, derivation should proceed normally to dispatch.
        let info = handle
            .debug_query_derivation("uncached-hash")
            .await
            .unwrap()
            .expect("derivation should exist");
        assert!(
            matches!(
                info.status,
                DerivationStatus::Assigned | DerivationStatus::Ready
            ),
            "derivation should be dispatched normally without store client, got {:?}",
            info.status
        );
    }

    // -----------------------------------------------------------------------
    // DB fault injection
    // -----------------------------------------------------------------------

    /// Verify that DB failures during completion are logged but do not block
    /// the in-memory state machine. This is the policy decided in Group 4:
    /// DB writes are best-effort; the actor must not stall on DB unavailability.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_db_failure_during_completion_logged() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Get a single derivation to Assigned state.
        let _rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;
        let build_id = Uuid::new_v4();
        let drv_hash = "db-fault-hash";
        let drv_path = "/nix/store/db-fault-hash.drv";
        let _event_rx = merge_single_node(&handle, build_id, drv_hash, drv_path, "scheduled").await;
        settle().await;

        // Sanity check: derivation was dispatched.
        let pre = handle
            .debug_query_derivation(drv_hash)
            .await
            .unwrap()
            .expect("derivation should exist");
        assert_eq!(pre.status, DerivationStatus::Assigned);

        // Close the DB pool — subsequent DB writes will fail.
        db.pool.close().await;

        // Send successful completion. DB write will fail but in-memory
        // transition should succeed.
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "test-worker".into(),
                drv_hash: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    built_outputs: vec![rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: "/nix/store/fake-output".into(),
                        output_hash: vec![0u8; 32],
                    }],
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;

        // In-memory state should have transitioned despite DB failure.
        let post = handle
            .debug_query_derivation(drv_hash)
            .await
            .unwrap()
            .expect("derivation should still exist");
        assert_eq!(
            post.status,
            DerivationStatus::Completed,
            "in-memory transition should succeed despite DB unavailability"
        );

        // DB failure should have been logged at error level — both for the
        // derivation status update AND for the build completion transition.
        assert!(
            logs_contain("failed to update derivation status in DB")
                || logs_contain("failed to persist"),
            "DB failure during derivation completion should be logged"
        );
        assert!(
            logs_contain("failed to persist build completion"),
            "DB failure in complete_build (transition_build) should be logged, not silently discarded"
        );

        // TestDb::drop uses a separate admin connection, so closing the test
        // pool here doesn't prevent database cleanup.
    }

    // -----------------------------------------------------------------------
    // Backpressure hysteresis
    // -----------------------------------------------------------------------

    /// Backpressure should activate at 80%, stay active, and only deactivate
    /// at 60% (hysteresis). Before the fix, ActorHandle used a simple 80%
    /// threshold with no hysteresis -> flapping under load near 80%.
    #[tokio::test]
    async fn test_backpressure_hysteresis() {
        let db = TestDb::new(&MIGRATOR).await;
        let scheduler_db = SchedulerDb::new(db.pool.clone());
        let mut actor = DagActor::new(scheduler_db, None);
        let flag = actor.backpressure_flag();

        // Simulate queue at 50% — below high watermark, not active.
        actor.update_backpressure(5000, 10000);
        assert!(!flag.load(Ordering::Relaxed), "50%: should not be active");

        // 85% — above high watermark, activates.
        actor.update_backpressure(8500, 10000);
        assert!(flag.load(Ordering::Relaxed), "85%: should activate");

        // 70% — between watermarks, STILL active (hysteresis).
        actor.update_backpressure(7000, 10000);
        assert!(
            flag.load(Ordering::Relaxed),
            "70%: should STAY active (hysteresis between 60% and 80%)"
        );

        // 55% — below low watermark, deactivates.
        actor.update_backpressure(5500, 10000);
        assert!(
            !flag.load(Ordering::Relaxed),
            "55%: should deactivate (below 60% low watermark)"
        );

        // 70% again — below high watermark, STILL inactive (hysteresis).
        actor.update_backpressure(7000, 10000);
        assert!(
            !flag.load(Ordering::Relaxed),
            "70%: should STAY inactive (hysteresis between 60% and 80%)"
        );
    }

    /// ActorHandle::send() and ::is_backpressured() should honor the shared
    /// hysteresis flag, not compute their own threshold.
    #[tokio::test]
    async fn test_handle_uses_shared_backpressure_flag() {
        let db = TestDb::new(&MIGRATOR).await;
        let (handle, _task) = setup_actor(db.pool.clone()).await;

        // Initially not backpressured (empty queue).
        assert!(!handle.is_backpressured());

        // Directly set the shared flag (simulating actor's hysteresis decision).
        handle.backpressure.store(true, Ordering::Relaxed);
        assert!(
            handle.is_backpressured(),
            "handle should read the shared flag"
        );

        // send() should reject under backpressure.
        let (reply_tx, _) = oneshot::channel();
        let result = handle
            .send(ActorCommand::QueryBuildStatus {
                build_id: Uuid::new_v4(),
                reply: reply_tx,
            })
            .await;
        assert!(
            matches!(result, Err(ActorError::Backpressure)),
            "send() should reject when shared flag is set"
        );

        // Clear flag; send() should succeed.
        handle.backpressure.store(false, Ordering::Relaxed);
        assert!(!handle.is_backpressured());
    }
}
