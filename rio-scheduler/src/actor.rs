//! DAG actor: single Tokio task owning all mutable scheduler state.
//!
//! All gRPC handlers communicate with the actor via an mpsc command channel.
//! The actor processes commands serially, ensuring deterministic ordering
//! and eliminating lock contention.

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

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
    /// Whether backpressure is currently active.
    backpressure_active: bool,
    /// Leader generation counter (for assignment tokens).
    generation: i64,
}

impl DagActor {
    /// Create a new actor with the given database handle.
    pub fn new(db: SchedulerDb) -> Self {
        Self {
            dag: DerivationDag::new(),
            ready_queue: ReadyQueue::new(),
            builds: HashMap::new(),
            build_events: HashMap::new(),
            build_sequences: HashMap::new(),
            workers: HashMap::new(),
            retry_policy: RetryPolicy::default(),
            db,
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
                    let _ = reply.send(result);
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
                    since_sequence: _,
                    reply,
                } => {
                    let result = self.handle_watch_build(build_id);
                    let _ = reply.send(result);
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
        let newly_inserted = self.dag.merge(build_id, &nodes, &edges);

        // Persist newly inserted derivations and edges to DB
        for node in &nodes {
            let drv_hash = &node.drv_hash;
            let status = if newly_inserted.contains(drv_hash) {
                DerivationStatus::Created
            } else {
                // Existing node, just link the build
                if let Some(state) = self.dag.nodes.get(drv_hash) {
                    state.status
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
            if let Some(state) = self.dag.nodes.get_mut(drv_hash) {
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

        // Compute initial states for newly inserted derivations
        // (TOCTOU: cache checks would happen here, inside the actor)
        let initial_states = self.dag.compute_initial_states(&newly_inserted);

        let total_derivations = nodes.len() as u32;
        let mut cached_count = 0u32;

        for (drv_hash, target_status) in &initial_states {
            if let Some(state) = self.dag.nodes.get_mut(drv_hash) {
                match target_status {
                    DerivationStatus::Ready => {
                        // Transition created -> queued -> ready
                        let _ = state.transition(DerivationStatus::Queued);
                        let _ = state.transition(DerivationStatus::Ready);
                        self.db
                            .update_derivation_status(drv_hash, DerivationStatus::Ready, None)
                            .await?;
                        self.ready_queue.push_back(drv_hash.clone());
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
                && let Some(state) = self.dag.nodes.get(&node.drv_hash)
                && state.status == DerivationStatus::Completed
            {
                cached_count += 1;
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

        // Find the derivation in the DAG
        let current_status = match self.dag.nodes.get(drv_hash) {
            Some(state) => state.status,
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
        if let Some(state) = self.dag.nodes.get_mut(drv_hash) {
            // Ensure we're in running state first
            if state.status == DerivationStatus::Assigned {
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

        // Update assignment
        if let Some(state) = self.dag.nodes.get(drv_hash)
            && let Some(db_id) = state.db_id
        {
            let _ = self.db.update_assignment_status(db_id, "completed").await;
        }

        // Async: update build history EMA
        if let Some(state) = self.dag.nodes.get(drv_hash)
            && let Some(pname) = &state.pname
            && let (Some(start), Some(stop)) = (&result.start_time, &result.stop_time)
        {
            let duration_secs = (stop.seconds - start.seconds) as f64
                + (stop.nanos - start.nanos) as f64 / 1_000_000_000.0;
            if duration_secs > 0.0 {
                let _ = self
                    .db
                    .update_build_history(pname, &state.system, duration_secs)
                    .await;
            }
        }

        // Emit derivation completed event
        let output_paths: Vec<String> = self
            .dag
            .nodes
            .get(drv_hash)
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

        // Release downstream: find newly ready derivations
        let newly_ready = self.dag.find_newly_ready(drv_hash);
        for ready_hash in &newly_ready {
            if let Some(state) = self.dag.nodes.get_mut(ready_hash)
                && state.transition(DerivationStatus::Ready).is_ok()
            {
                if let Err(e) = self
                    .db
                    .update_derivation_status(ready_hash, DerivationStatus::Ready, None)
                    .await
                {
                    error!(drv_hash = ready_hash, error = %e, "failed to update status");
                }
                self.ready_queue.push_back(ready_hash.clone());
            }
        }

        // Update build completion status
        for build_id in &interested_builds {
            self.update_build_counts(*build_id);
            self.check_build_completion(*build_id, worker_id).await;
        }
    }

    async fn handle_transient_failure(&mut self, drv_hash: &str, worker_id: &str) {
        let should_retry = if let Some(state) = self.dag.nodes.get_mut(drv_hash) {
            state.failed_workers.insert(worker_id.to_string());

            // Check poison threshold
            if state.failed_workers.len() >= POISON_THRESHOLD {
                // Transition to poisoned
                if state.status == DerivationStatus::Assigned {
                    let _ = state.transition(DerivationStatus::Running);
                }
                let _ = state.transition(DerivationStatus::Poisoned);
                state.poisoned_at = Some(Instant::now());
                false
            } else if state.retry_count < self.retry_policy.max_retries {
                // Transition running -> failed
                if state.status == DerivationStatus::Assigned {
                    let _ = state.transition(DerivationStatus::Running);
                }
                let _ = state.transition(DerivationStatus::Failed);
                true
            } else {
                // Max retries exceeded: treat as permanent
                if state.status == DerivationStatus::Assigned {
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
            let retry_count = self
                .dag
                .nodes
                .get(drv_hash)
                .map(|s| s.retry_count)
                .unwrap_or(0);

            // Schedule retry with backoff
            let backoff = self.retry_policy.backoff_duration(retry_count);
            let drv_hash_owned = drv_hash.to_string();

            if let Some(state) = self.dag.nodes.get_mut(&drv_hash_owned) {
                state.retry_count += 1;
                state.assigned_worker = None;
            }

            let _ = self
                .db
                .update_derivation_status(drv_hash, DerivationStatus::Failed, None)
                .await;
            let _ = self.db.increment_retry_count(drv_hash).await;

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
            if let Some(state) = self.dag.nodes.get_mut(&drv_hash_owned) {
                let _ = state.transition(DerivationStatus::Ready);
                self.ready_queue.push_back(drv_hash_owned);
            }
        } else {
            let _ = self
                .db
                .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
                .await;

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
        if let Some(state) = self.dag.nodes.get_mut(drv_hash) {
            if state.status == DerivationStatus::Assigned {
                let _ = state.transition(DerivationStatus::Running);
            }
            // Permanent failure -> poisoned (no retry)
            let _ = state.transition(DerivationStatus::Poisoned);
            state.poisoned_at = Some(Instant::now());
        }

        let _ = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Poisoned, None)
            .await;

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
                is_registered: false,
                last_heartbeat: Instant::now(),
                missed_heartbeats: 0,
            });

        worker.stream_tx = Some(stream_tx);

        // Check if we now have both stream + heartbeat
        if worker.system.is_some() {
            worker.is_registered = true;
            info!(worker_id, "worker fully registered (stream + heartbeat)");
        }
    }

    async fn handle_worker_disconnected(&mut self, worker_id: &str) {
        info!(worker_id, "worker disconnected");

        if let Some(worker) = self.workers.remove(worker_id) {
            // Reassign all derivations that were assigned to this worker
            for drv_hash in &worker.running_builds {
                if let Some(state) = self.dag.nodes.get_mut(drv_hash.as_str())
                    && matches!(
                        state.status,
                        DerivationStatus::Assigned | DerivationStatus::Running
                    )
                {
                    // Transition back to ready for reassignment
                    state.status = DerivationStatus::Ready;
                    state.assigned_worker = None;
                    let _ = self
                        .db
                        .update_derivation_status(drv_hash, DerivationStatus::Ready, None)
                        .await;
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
                is_registered: false,
                last_heartbeat: Instant::now(),
                missed_heartbeats: 0,
            });

        worker.system = Some(system);
        worker.supported_features = supported_features;
        worker.max_builds = max_builds;
        worker.last_heartbeat = Instant::now();
        worker.missed_heartbeats = 0;

        // Update running builds from heartbeat
        worker.running_builds = running_builds.into_iter().collect();

        // Check if we now have both stream + heartbeat
        if worker.stream_tx.is_some() && !worker.is_registered {
            worker.is_registered = true;
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
        for (drv_hash, state) in &self.dag.nodes {
            if state.status == DerivationStatus::Poisoned
                && let Some(poisoned_at) = state.poisoned_at
                && now.duration_since(poisoned_at) > POISON_TTL
            {
                expired_poisons.push(drv_hash.clone());
            }
        }

        for drv_hash in expired_poisons {
            info!(drv_hash, "poison TTL expired, resetting to created");
            if let Some(state) = self.dag.nodes.get_mut(&drv_hash) {
                state.status = DerivationStatus::Created;
                state.poisoned_at = None;
                state.retry_count = 0;
                state.failed_workers.clear();
                let _ = self
                    .db
                    .update_derivation_status(&drv_hash, DerivationStatus::Created, None)
                    .await;
            }
        }

        // Update metrics
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
            let (system, required_features) = match self.dag.nodes.get(&drv_hash) {
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
                .nodes
                .get(&drv_hash)
                .is_none_or(|s| s.status != DerivationStatus::Ready)
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
        if let Some(state) = self.dag.nodes.get_mut(drv_hash) {
            if state.transition(DerivationStatus::Assigned).is_err() {
                return;
            }
            state.assigned_worker = Some(worker_id.to_string());
        }

        // Update DB
        let _ = self
            .db
            .update_derivation_status(drv_hash, DerivationStatus::Assigned, Some(worker_id))
            .await;

        // Create assignment in DB
        if let Some(state) = self.dag.nodes.get(drv_hash)
            && let Some(db_id) = state.db_id
        {
            let _ = self
                .db
                .insert_assignment(db_id, worker_id, self.generation)
                .await;
        }

        // Track on worker
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.running_builds.insert(drv_hash.to_string());
        }

        // Send WorkAssignment to worker via stream
        if let Some(state) = self.dag.nodes.get(drv_hash) {
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
                // Reassign: put back in queue
                if let Some(state) = self.dag.nodes.get_mut(drv_hash) {
                    state.status = DerivationStatus::Ready;
                    state.assigned_worker = None;
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
            .nodes
            .get(drv_hash)
            .map(|s| s.interested_builds.iter().copied().collect())
            .unwrap_or_default()
    }

    fn drv_hash_to_path(&self, drv_hash: &str) -> Option<String> {
        self.dag.nodes.get(drv_hash).map(|s| s.drv_path.clone())
    }

    fn find_db_id_by_path(&self, drv_path: &str) -> Option<Uuid> {
        self.dag
            .nodes
            .values()
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
                    .nodes
                    .get(h)
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
    pub fn spawn(db: SchedulerDb) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let actor = DagActor::new(db);
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
}
