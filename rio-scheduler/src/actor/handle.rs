//! External-facing actor handle.
// r[impl sched.backpressure.hysteresis]

use super::*;

/// Actor in-memory snapshot of one executor's connection state.
///
/// Serves both unit-test assertions and the `DebugListExecutors` gRPC
/// RPC. The fields beyond the original four (`has_stream`, `warm`,
/// `kind`, `systems`, `last_heartbeat_ago_secs`) were added for the
/// I-048b/c diagnostic: PG showed fetchers `[alive]` (heartbeat unary
/// RPC succeeded against the new leader), but the actor map had no
/// entry (BuildExecution stream still stuck on TCP keepalive to the
/// old leader). `rio-cli workers` lied; only `has_stream` here knows.
#[derive(Debug, Clone)]
pub struct DebugExecutorInfo {
    pub executor_id: String,
    /// `stream_tx.is_some()` — BuildExecution bidi stream connected to
    /// THIS actor instance. The I-048b zombie signature is `false` here
    /// while PG `last_seen` is recent.
    pub has_stream: bool,
    /// `has_stream && !systems.is_empty()` — dispatch's hard filter.
    pub is_registered: bool,
    /// Warm-gate. `false` until `PrefetchComplete` ACK. A registered
    /// cold worker is filtered out of dispatch unless no warm worker
    /// passes the hard filter.
    pub warm: bool,
    /// Builder vs fetcher. FOD routing partitions on this.
    pub kind: rio_proto::types::ExecutorKind,
    /// Populated by first heartbeat; empty until then. The
    /// `is_registered` second leg.
    pub systems: Vec<String>,
    /// `last_heartbeat.elapsed().as_secs()`. Staleness of the actor's
    /// view — PG `last_seen` may differ (heartbeat reaches PG and actor
    /// independently; post-failover PG can be fresher).
    pub last_heartbeat_ago_secs: u64,
    /// P0537: at most one. Populated from `ExecutorState.running_build`.
    pub running_build: Option<String>,
    /// I-056b: `has_capacity()` checks both. Either true → invisible
    /// to dispatch regardless of `is_registered`/`warm`. Added after
    /// 45min chasing PG/recovery red herrings when a stale drain flag
    /// surviving reconnect was the actual FOD-stuck-22min root.
    pub draining: bool,
    pub store_degraded: bool,
    /// `r[sec.executor.identity-token]`: HMAC-attested SpawnIntent id
    /// the executor is bound to. `None` for hw-agnostic pods or after
    /// the not-Ready→None downgrade. Exposed for the heartbeat-spoof
    /// guard's regression test.
    pub intent_id: Option<String>,
}

/// Test-only: snapshot of derivation state for assertions. Mirrors the
/// nested sub-struct shape of [`crate::state::DerivationState`] so test
/// accesses (`info.retry.count`, `info.ca.output_unchanged`) read the
/// same as production code.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct DebugDerivationInfo {
    pub status: DerivationStatus,
    pub assigned_executor: Option<String>,
    pub output_paths: Vec<String>,
    pub retry: crate::state::RetryState,
    pub ca: crate::state::CaState,
    pub sched: crate::state::SchedHint,
    pub substitute_tried: bool,
    pub topdown_pruned: bool,
}

/// Handle for sending commands to the actor.
#[derive(Clone)]
pub struct ActorHandle {
    pub(super) tx: mpsc::Sender<ActorCommand>,
    /// Shared read-only backpressure flag with the actor. The actor computes
    /// hysteresis (activate at 80%, deactivate at 60%) and writes to its
    /// `Arc<AtomicBool>`; the handle reads it via this read-only view for
    /// send() and is_backpressured(). Without hysteresis, the handle used a
    /// simple threshold -> flapping under load near 80%.
    pub(super) backpressure: BackpressureReader,
    /// Leader generation for `HeartbeatResponse`. Lease task writes,
    /// gRPC layer reads via `leader_generation()`. See
    /// [`GenerationReader`] for ordering semantics.
    pub(super) generation: GenerationReader,
    /// Cached [`ClusterSnapshot`], refreshed each `Tick`. See
    /// [`ActorHandle::cluster_snapshot_cached`].
    pub(super) snapshot_rx: watch::Receiver<Arc<ClusterSnapshot>>,
}

impl ActorHandle {
    /// Create a new actor handle and spawn the actor task.
    ///
    /// Tests / benches pass `DagActorConfig::default()` and
    /// `DagActorPlumbing::default()` (always-leader, no store/flusher).
    /// main.rs populates both from scheduler.toml and the lease task's
    /// shared `LeaderState`.
    // r[impl sched.retry.per-executor-budget]
    pub fn spawn(db: SchedulerDb, cfg: DagActorConfig, plumbing: DagActorPlumbing) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let actor = DagActor::new(db, cfg, plumbing);
        let backpressure = actor.backpressure_flag();
        let generation = actor.generation_reader();
        let snapshot_rx = actor.snapshot_receiver();
        let self_tx = tx.downgrade();
        rio_common::task::spawn_monitored("dag-actor", actor.run_with_self_tx(rx, self_tx));
        Self {
            tx,
            backpressure,
            generation,
            snapshot_rx,
        }
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

    /// Latest [`ClusterSnapshot`] published by the actor's `Tick`,
    /// without an actor round-trip. Up to one Tick (~1s) stale.
    ///
    /// I-163: `query_unchecked(ClusterSnapshot)` queues behind whatever
    /// is in the mailbox — under medium-mixed-32x load that was 9.5k
    /// commands × ~5ms avg ≈ 47s for a 37µs handler. The autoscaler
    /// and `xtask status` need a reading PRECISELY when the actor is
    /// saturated (I-056 diagnostic-blind-spot lesson). This path is
    /// O(1) Arc clone regardless of mailbox depth.
    ///
    /// Returns the `Default` snapshot (all zeros) until the first Tick
    /// fires — same observable behavior as a fresh actor with an empty
    /// DAG.
    // r[impl sched.admin.snapshot-cached]
    pub fn cluster_snapshot_cached(&self) -> Arc<ClusterSnapshot> {
        self.snapshot_rx.borrow().clone()
    }

    /// Current leader generation for `HeartbeatResponse.generation`.
    ///
    /// Workers compare this against `WorkAssignment.generation` to
    /// detect stale assignments after leader failover. Both reads come
    /// from the same `Arc<AtomicU64>` (actor for WorkAssignment, handle
    /// for heartbeat) so they agree modulo the atomic-load instant.
    pub fn leader_generation(&self) -> u64 {
        self.generation.get()
    }

    /// Send a command without backpressure check (for worker lifecycle events).
    pub async fn send_unchecked(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        self.tx.send(cmd).await.map_err(|_| ActorError::ChannelSend)
    }

    /// Send a command carrying a oneshot reply, await the reply. For
    /// admin-RPC patterns where the caller uses `send_unchecked` (bypass
    /// backpressure). Callers in the gRPC layer convert via
    /// `actor_error_to_status`.
    pub async fn query_unchecked<R>(
        &self,
        mk_cmd: impl FnOnce(oneshot::Sender<R>) -> ActorCommand,
    ) -> Result<R, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(mk_cmd(tx)).await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }

    /// Query the actor's in-memory executor map. Used by both unit
    /// tests and the `DebugListExecutors` gRPC handler. Bypasses
    /// backpressure (`send_unchecked`) — diagnostic queries must
    /// succeed under saturation, that's exactly when you need them.
    pub async fn debug_query_workers(&self) -> Result<Vec<DebugExecutorInfo>, ActorError> {
        self.query_unchecked(|reply| ActorCommand::Admin(AdminQuery::DebugQueryWorkers { reply }))
            .await
    }
}

/// Test-only `debug_*` actor queries. Thin wrappers over
/// [`ActorHandle::query_unchecked`] that wrap a [`DebugCmd`] variant
/// — kept as named methods so test call sites read as intent
/// (`handle.debug_force_assign(...)`) rather than open-coding the
/// command enum.
#[cfg(test)]
impl ActorHandle {
    async fn debug<R>(
        &self,
        mk: impl FnOnce(oneshot::Sender<R>) -> DebugCmd,
    ) -> Result<R, ActorError> {
        self.query_unchecked(|reply| ActorCommand::Debug(mk(reply)))
            .await
    }

    /// Query a derivation's state.
    pub async fn debug_query_derivation(
        &self,
        drv_hash: &str,
    ) -> Result<Option<DebugDerivationInfo>, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::QueryDerivation { drv_hash, reply })
            .await
    }

    /// Force a derivation to Assigned for a given worker, bypassing
    /// dispatch's backoff + failed_builders exclusion. For retry/poison
    /// tests that drive multiple completion cycles. Returns `false` if
    /// the derivation couldn't be forced (terminal state, not found).
    pub async fn debug_force_assign(
        &self,
        drv_hash: &str,
        executor_id: &str,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        let executor_id = executor_id.into();
        self.debug(|reply| DebugCmd::ForceAssign {
            drv_hash,
            executor_id,
            reply,
        })
        .await
    }

    /// Set `worker.running_build` directly, bypassing DAG-status guards.
    /// For the heartbeat-reconcile safety-net test where the drv is
    /// terminal (Poisoned). Returns `false` if the executor isn't
    /// registered.
    pub async fn debug_set_running_build(
        &self,
        executor_id: &str,
        drv_hash: &str,
    ) -> Result<bool, ActorError> {
        let executor_id = executor_id.into();
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::SetRunningBuild {
            executor_id,
            drv_hash,
            reply,
        })
        .await
    }

    /// Backdate `running_since` and force Running status. For
    /// backstop-timeout tests. Returns `false` if not found or not in
    /// Assigned/Running.
    pub async fn debug_backdate_running(
        &self,
        drv_hash: &str,
        secs_ago: u64,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::BackdateRunning {
            drv_hash,
            secs_ago,
            reply,
        })
        .await
    }

    /// Backdate an executor's `last_heartbeat`. For heartbeat-timeout
    /// tests. Returns `false` if executor not found.
    pub async fn debug_backdate_heartbeat(
        &self,
        executor_id: &str,
        secs_ago: u64,
    ) -> Result<bool, ActorError> {
        let executor_id = executor_id.into();
        self.debug(|reply| DebugCmd::BackdateHeartbeat {
            executor_id,
            secs_ago,
            reply,
        })
        .await
    }

    /// Seed the SLA estimator's hw_table for ref→wall tests.
    pub async fn debug_seed_hw_table(
        &self,
        factors: std::collections::HashMap<String, f64>,
    ) -> Result<(), ActorError> {
        self.debug(|reply| DebugCmd::SeedHwTable { factors, reply })
            .await
    }

    /// Backdate a build's `submitted_at`. For per-build-timeout tests.
    /// Returns `false` if build not found.
    pub async fn debug_backdate_submitted(
        &self,
        build_id: Uuid,
        secs_ago: u64,
    ) -> Result<bool, ActorError> {
        self.debug(|reply| DebugCmd::BackdateSubmitted {
            build_id,
            secs_ago,
            reply,
        })
        .await
    }

    /// Force a derivation into `Poisoned` with the given
    /// `resubmit_cycles`. Returns `false` if not found.
    pub async fn debug_force_poisoned(
        &self,
        drv_hash: &str,
        resubmit_cycles: u32,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::ForcePoisoned {
            drv_hash,
            resubmit_cycles,
            reply,
        })
        .await
    }

    /// Overwrite a derivation's `output_paths`. Returns `false` if not
    /// found.
    pub async fn debug_set_output_paths(
        &self,
        drv_hash: &str,
        paths: Vec<String>,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::SetOutputPaths {
            drv_hash,
            paths,
            reply,
        })
        .await
    }

    /// Set a derivation's `topdown_pruned`. Returns `false` if not
    /// found.
    pub async fn debug_set_topdown_pruned(
        &self,
        drv_hash: &str,
        value: bool,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::SetTopdownPruned {
            drv_hash,
            value,
            reply,
        })
        .await
    }

    /// Force a derivation into `status`, bypassing the transition
    /// table. Returns `false` if not found.
    pub async fn debug_force_status(
        &self,
        drv_hash: &str,
        status: crate::state::DerivationStatus,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::ForceStatus {
            drv_hash,
            status,
            reply,
        })
        .await
    }

    /// Clear a derivation's `drv_content` to simulate post-recovery
    /// state. Returns `false` if not found.
    pub async fn debug_clear_drv_content(&self, drv_hash: &str) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::ClearDrvContent { drv_hash, reply })
            .await
    }

    /// Call `cache_breaker.record_failure()` `n` times. Returns
    /// `is_open()` after. For breaker-gate tests that need the breaker
    /// open WITHOUT driving N failing RPCs through the full
    /// merge/completion path.
    pub async fn debug_counters(&self) -> Result<super::TestCountersSnapshot, ActorError> {
        self.debug(|reply| DebugCmd::Counters { reply }).await
    }

    pub async fn debug_trip_breaker(&self, n: u32) -> Result<bool, ActorError> {
        self.debug(|reply| DebugCmd::TripBreaker { n, reply }).await
    }

    /// Seed `state.sched.last_intent` for D4 floor tests. `None`
    /// fields are left unchanged; any `Some` materializes a
    /// `last_intent`.
    pub async fn debug_seed_sched_hint(
        &self,
        drv_hash: &str,
        est_memory_bytes: Option<u64>,
        est_disk_bytes: Option<u64>,
        est_deadline_secs: Option<u32>,
        floor: Option<crate::state::ResourceFloor>,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::SeedSchedHint {
            drv_hash,
            est_memory_bytes,
            est_disk_bytes,
            est_deadline_secs,
            floor,
            reply,
        })
        .await
    }
}
