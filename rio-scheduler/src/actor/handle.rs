//! External-facing actor handle.
// r[impl sched.backpressure.hysteresis+2]

use super::*;

/// Test-only: snapshot of derivation state for assertions. Mirrors the
/// nested sub-struct shape of [`crate::state::DerivationState`] so test
/// accesses (`info.retry.count`, `info.ca.output_unchanged`) read the
/// same as production code.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct DebugDerivationInfo {
    pub status: DerivationStatus,
    pub assigned_executor: Option<String>,
    /// Per-execution carrier — `None` unless an assignment is live or a
    /// terminal epilogue is about to read it. See `DerivationState::exec_id`.
    pub exec_id: Option<uuid::Uuid>,
    pub output_paths: Vec<String>,
    pub retry: crate::state::RetryState,
    pub ca: crate::state::CaState,
    pub sched: crate::state::SchedHint,
    pub substitute_tried: bool,
    pub topdown_pruned: bool,
    pub closure_hole: bool,
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
    /// Leader generation reader. The lease task and recovery's
    /// PG-floor seed write the underlying Arc; the gRPC layer reads
    /// the recovery-gated view via `advertised_generation()` for
    /// `HeartbeatResponse`, and `leader_generation()` exposes the raw
    /// (not recovery-gated) value. See [`GenerationReader`] for
    /// ordering semantics.
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
    // r[impl sched.retry.per-executor-budget+4]
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

    /// Current leader generation — the raw (not recovery-gated) value.
    /// NOT what the heartbeat reply carries (that is
    /// [`advertised_generation`](Self::advertised_generation)); this
    /// serves tests and any future debug surface that wants the value
    /// without the recovery gate.
    pub fn leader_generation(&self) -> u64 {
        self.generation.get()
    }

    /// The worker-visible generation for `HeartbeatResponse.generation`:
    /// carries 0 (the proto-unset sentinel) until the leader's recovery
    /// completes, then the post-recovery generation. Workers compare it
    /// against `WorkAssignment.generation` to detect stale assignments
    /// after leader failover; both ultimately read the same
    /// `Arc<AtomicU64>` (actor for WorkAssignment, handle for
    /// heartbeat), and both are gated on the same recovery condition
    /// (`dispatch_ready` for assignments, this accessor for the
    /// heartbeat payload).
    pub fn advertised_generation(&self) -> u64 {
        self.generation.advertised()
    }

    /// Send a command without backpressure check (for worker lifecycle events).
    pub async fn send_unchecked(&self, cmd: ActorCommand) -> Result<(), ActorError> {
        self.tx.send(cmd).await.map_err(|_| ActorError::ChannelSend)
    }

    /// Raw clone of the actor's command sender — the channel half of
    /// [`send_unchecked`](Self::send_unchecked) (hysteresis bypass:
    /// control messages, not work submission). Used by the lease-hook
    /// forwarder ([`crate::lease_hooks`]) so lease transitions reach the
    /// actor in invocation order through a single sender it owns.
    pub(crate) fn command_sender(&self) -> mpsc::Sender<ActorCommand> {
        self.tx.clone()
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
    ///
    /// CAVEAT: this is a reset+reassign shortcut, not a real dispatch —
    /// it clears `state.exec_id` (via `reset_to_ready`) without minting
    /// a new one. A test that force-assigns and then asserts on
    /// exec-keyed terminal writes (`drv_executions`, the
    /// `build_derivations.exec_id` correlation, log finalization)
    /// silently exercises the no-carrier early-return instead of the
    /// path it means to test. For those, deliver through a real pull
    /// (`pull_attempt`) so the mint stamps an exec_id.
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

    /// Read a derivation's in-memory attempt history (the committed
    /// ledger-suffix mirror). For the 1a acceptance battery.
    pub async fn debug_query_attempt_history(
        &self,
        drv_hash: &str,
    ) -> Result<Option<Vec<crate::state::AttemptRecord>>, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::QueryAttemptHistory { drv_hash, reply })
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

    /// Set a derivation's `substitute_tried` one-shot. Returns `false`
    /// if not found.
    pub async fn debug_set_substitute_tried(
        &self,
        drv_hash: &str,
        value: bool,
    ) -> Result<bool, ActorError> {
        let drv_hash = drv_hash.to_string();
        self.debug(|reply| DebugCmd::SetSubstituteTried {
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
