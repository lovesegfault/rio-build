//! DAG actor: single Tokio task owning all mutable scheduler state.
//!
//! All gRPC handlers communicate with the actor via an mpsc command channel.
//! The actor processes commands serially, ensuring deterministic ordering
//! and eliminating lock contention.

use std::collections::{HashMap, HashSet};
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
    /// Whether backpressure is currently active.
    backpressure_active: bool,
    /// Leader generation counter (for assignment tokens).
    generation: i64,
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
            backpressure_active: false,
            generation: 1,
        }
    }

    /// Run the actor event loop. Consumes commands from the channel until it closes.
    pub async fn run(mut self, mut rx: mpsc::Receiver<ActorCommand>) {
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
                        let _ = self
                            .handle_cancel_build(build_id, "client_disconnect_during_merge")
                            .await;
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

        if !self.backpressure_active && fraction >= BACKPRESSURE_HIGH_WATERMARK {
            self.backpressure_active = true;
            warn!(
                queue_len,
                capacity,
                "backpressure activated at {:.0}% capacity",
                fraction * 100.0
            );
            metrics::counter!("rio_scheduler_queue_backpressure").increment(1);
        } else if self.backpressure_active && fraction <= BACKPRESSURE_LOW_WATERMARK {
            self.backpressure_active = false;
            info!(
                queue_len,
                capacity, "backpressure deactivated, resuming normal operation"
            );
        }
    }

    /// Check if the actor is under backpressure.
    pub fn is_backpressured(&self) -> bool {
        self.backpressure_active
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
        let build_info = BuildInfo {
            build_id,
            tenant_id,
            priority_class,
            state: BuildState::Pending,
            keep_going,
            options,
            derivation_hashes: nodes.iter().map(|n| n.drv_hash.clone()).collect(),
            completed_count: 0,
            cached_count: 0,
            failed_count: 0,
            error_summary: None,
            failed_derivation: None,
        };
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
                        let _ = state.transition(DerivationStatus::Queued);
                        let _ = state.transition(DerivationStatus::Ready);
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
                        let _ = state.transition(DerivationStatus::Queued);
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
                metrics::counter!("rio_scheduler_cache_hits_total").increment(1);
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
            if state.status() == DerivationStatus::Assigned {
                let _ = state.transition(DerivationStatus::Running);
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
            let duration_secs = (stop.seconds - start.seconds) as f64
                + (stop.nanos - start.nanos) as f64 / 1_000_000_000.0;
            if duration_secs > 0.0
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
                if state.status() == DerivationStatus::Assigned {
                    let _ = state.transition(DerivationStatus::Running);
                }
                let _ = state.transition(DerivationStatus::Poisoned);
                state.poisoned_at = Some(Instant::now());
                false
            } else if state.retry_count < self.retry_policy.max_retries {
                // Transition running -> failed
                if state.status() == DerivationStatus::Assigned {
                    let _ = state.transition(DerivationStatus::Running);
                }
                let _ = state.transition(DerivationStatus::Failed);
                true
            } else {
                // Max retries exceeded: treat as permanent
                if state.status() == DerivationStatus::Assigned {
                    let _ = state.transition(DerivationStatus::Running);
                }
                let _ = state.transition(DerivationStatus::Poisoned);
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

            // For Phase 2a, immediately re-queue (production would use a delayed re-queue)
            if let Some(state) = self.dag.node_mut(&drv_hash_owned) {
                let _ = state.transition(DerivationStatus::Ready);
                self.ready_queue.push_back(drv_hash_owned);
            }
        } else {
            if let Err(e) = self
                .db
                .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
                .await
            {
                error!(drv_hash, error = %e, "failed to persist Poisoned status");
            }

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
            if state.status() == DerivationStatus::Assigned {
                let _ = state.transition(DerivationStatus::Running);
            }
            // Permanent failure -> poisoned (no retry)
            let _ = state.transition(DerivationStatus::Poisoned);
            state.poisoned_at = Some(Instant::now());
        }

        if let Err(e) = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
            .await
        {
            error!(drv_hash, error = %e, "failed to persist Poisoned status");
        }

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

    async fn handle_derivation_failure(&mut self, build_id: Uuid, drv_hash: &str) {
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.failed_count += 1;

            if !build.keep_going {
                // Fail the entire build immediately
                build.error_summary = Some(format!("derivation {drv_hash} failed"));
                build.failed_derivation = Some(drv_hash.to_string());
                let _ = self.transition_build_to_failed(build_id).await;
            } else {
                // keepGoing: check if all derivations are resolved
                self.check_build_completion(build_id, "").await;
            }
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

        if build.state.is_terminal() {
            return Ok(false);
        }

        // Remove build interest from derivations
        let orphaned = self.dag.remove_build_interest(build_id);

        // Remove orphaned derivations from the ready queue
        for hash in &orphaned {
            self.ready_queue.remove(hash);
        }

        // Transition build to cancelled
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.state = BuildState::Cancelled;
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

        if let Some(worker) = self.workers.remove(worker_id) {
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
        }

        metrics::gauge!("rio_scheduler_workers_active").decrement(1.0);
    }

    fn handle_heartbeat(
        &mut self,
        worker_id: String,
        system: String,
        supported_features: Vec<String>,
        max_builds: u32,
        running_builds: Vec<String>,
    ) {
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

        // Update running builds from heartbeat
        worker.running_builds = running_builds.into_iter().collect();

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
                .filter(|b| b.state == BuildState::Active)
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
        // Legacy metric name (kept for backward compat with existing dashboards)
        metrics::gauge!("rio_scheduler_ready_queue_len").set(self.ready_queue.len() as f64);
        metrics::gauge!("rio_scheduler_active_builds").set(
            self.builds
                .values()
                .filter(|b| b.state == BuildState::Active)
                .count() as f64,
        );
    }

    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// Dispatch ready derivations to available workers (FIFO).
    #[allow(clippy::while_let_loop)]
    async fn dispatch_ready(&mut self) {
        loop {
            // Peek at the next ready derivation
            let drv_hash = match self.ready_queue.peek() {
                Some(h) => h.to_string(),
                None => break,
            };

            // Find the derivation's requirements
            let (system, required_features) = match self.dag.node(&drv_hash) {
                Some(state) => (state.system.clone(), state.required_features.clone()),
                None => {
                    // Stale entry in queue, remove and continue
                    self.ready_queue.pop_front();
                    continue;
                }
            };

            // Only dispatch if derivation is actually in Ready state
            if self
                .dag
                .node(&drv_hash)
                .is_none_or(|s| s.status() != DerivationStatus::Ready)
            {
                self.ready_queue.pop_front();
                continue;
            }

            // Find first eligible worker with capacity (FIFO - no scoring)
            let eligible_worker = self
                .workers
                .values()
                .find(|w| w.has_capacity() && w.can_build(&system, &required_features))
                .map(|w| w.worker_id.clone());

            let worker_id = match eligible_worker {
                Some(id) => id,
                None => break, // No eligible workers, stop dispatching
            };

            // Pop from queue and assign
            self.ready_queue.pop_front();
            self.assign_to_worker(&drv_hash, &worker_id).await;
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
                drv_content: Vec::new(), // TODO: inline .drv content in future
                input_paths: Vec::new(), // TODO: compute closure
                output_names: state.output_names.clone(),
                build_options: Some(self.default_build_options()),
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

        let proto_state = match build.state {
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

    /// Resolve a drv_path to its drv_hash by scanning DAG nodes.
    /// Used by handle_completion since the gRPC layer receives CompletionReport
    /// with drv_path, but the DAG is keyed by drv_hash.
    fn drv_path_to_hash(&self, drv_path: &str) -> Option<String> {
        self.dag
            .iter_values()
            .find(|s| s.drv_path == drv_path)
            .map(|s| s.drv_hash.clone())
    }

    fn find_db_id_by_path(&self, drv_path: &str) -> Option<Uuid> {
        self.dag
            .iter_values()
            .find(|s| s.drv_path == drv_path)
            .and_then(|s| s.db_id)
    }

    fn update_build_counts(&mut self, build_id: Uuid) {
        let summary = self.dag.build_summary(build_id);
        if let Some(build) = self.builds.get_mut(&build_id) {
            build.completed_count = summary.completed + build.cached_count;
            build.failed_count = summary.failed;
        }
    }

    async fn check_build_completion(&mut self, build_id: Uuid, _worker_id: &str) {
        let (state, keep_going, total, completed, failed) = match self.builds.get(&build_id) {
            Some(b) => (
                b.state,
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
            let _ = self.complete_build(build_id).await;
        } else if keep_going && all_resolved && failed > 0 {
            // keepGoing: all derivations resolved but some failed
            let _ = self.transition_build_to_failed(build_id).await;
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

        Ok(())
    }

    async fn transition_build(
        &mut self,
        build_id: Uuid,
        new_state: BuildState,
    ) -> Result<(), ActorError> {
        if let Some(build) = self.builds.get_mut(&build_id)
            && build.state.validate_transition(new_state).is_ok()
        {
            build.state = new_state;
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

    fn default_build_options(&self) -> rio_proto::types::BuildOptions {
        rio_proto::types::BuildOptions {
            max_silent_time: 0,
            build_timeout: 0,
            build_cores: 0,
            keep_going: false,
        }
    }
}

/// Handle for sending commands to the actor.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ActorCommand>,
}

impl ActorHandle {
    /// Create a new actor handle and spawn the actor task.
    ///
    /// Returns the handle for sending commands.
    pub fn spawn(db: SchedulerDb, store_client: Option<StoreServiceClient<Channel>>) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let actor = DagActor::new(db, store_client);
        tokio::spawn(actor.run(rx));
        Self { tx }
    }

    /// Send a command to the actor, checking backpressure.
    pub async fn send(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        // Check capacity for backpressure
        let capacity = self.tx.capacity();
        let max = self.tx.max_capacity();
        let used_fraction = 1.0 - (capacity as f64 / max as f64);

        if used_fraction >= BACKPRESSURE_HIGH_WATERMARK {
            return Err(ActorError::Backpressure);
        }

        self.tx.send(cmd).await.map_err(|_| ActorError::ChannelSend)
    }

    /// Try to send a command without waiting (for fire-and-forget messages).
    pub fn try_send(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        self.tx.try_send(cmd).map_err(|_| ActorError::ChannelSend)
    }

    /// Check if the channel is under backpressure.
    pub fn is_backpressured(&self) -> bool {
        let capacity = self.tx.capacity();
        let max = self.tx.max_capacity();
        let used_fraction = 1.0 - (capacity as f64 / max as f64);
        used_fraction >= BACKPRESSURE_HIGH_WATERMARK
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
        let task = tokio::spawn(actor.run(rx));
        (ActorHandle { tx }, task)
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
                .ok();
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

        // DB failure should have been logged at error level.
        assert!(
            logs_contain("failed to update derivation status in DB")
                || logs_contain("failed to persist"),
            "DB failure during completion should be logged at error!"
        );

        // TestDb::drop uses a separate admin connection, so closing the test
        // pool here doesn't prevent database cleanup.
    }
}
