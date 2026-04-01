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
    pub running_count: usize,
    pub running_builds: Vec<String>,
    /// I-056b: `has_capacity()` checks both. Either true → invisible
    /// to dispatch regardless of `is_registered`/`warm`. Added after
    /// 45min chasing PG/recovery red herrings when a stale drain flag
    /// surviving reconnect was the actual FOD-stuck-22min root.
    pub draining: bool,
    pub store_degraded: bool,
}

/// Test-only: snapshot of derivation state for assertions.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct DebugDerivationInfo {
    pub status: DerivationStatus,
    pub retry_count: u32,
    pub assigned_executor: Option<String>,
    pub assigned_size_class: Option<String>,
    pub output_paths: Vec<String>,
    /// Distinct worker IDs that have failed this derivation. For
    /// asserting InfrastructureFailure does NOT populate this.
    pub failed_builders: Vec<String>,
    /// Flat failure counter (non-distinct mode). For asserting
    /// same-worker failures count under `require_distinct_workers=false`.
    pub failure_count: u32,
    /// Whether this derivation is content-addressed. For precondition
    /// asserts in CA cutoff-compare tests.
    pub is_ca: bool,
    /// CA cutoff-compare result. True iff every output's nar_hash
    /// matched the content index on completion. For asserting
    /// `r[sched.ca.cutoff-compare]` sets the flag correctly.
    pub ca_output_unchanged: bool,
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
}

impl ActorHandle {
    /// Create a new actor handle and spawn the actor task.
    ///
    /// Returns the handle for sending commands.
    pub fn spawn(
        db: SchedulerDb,
        store_client: Option<StoreServiceClient<Channel>>,
        log_flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
        size_classes: Vec<crate::assignment::SizeClassConfig>,
    ) -> Self {
        // Non-K8s default: always leader, generation stays at 1.
        // For K8s deployments main.rs calls spawn_with_leader().
        Self::spawn_with_leader(
            db,
            store_client,
            log_flush_tx,
            size_classes,
            // Test/bench default: poison+retry match `DagActor::new`'s
            // hardcoded defaults. Only production main.rs loads these
            // from scheduler.toml.
            PoisonConfig::default(),
            RetryPolicy::default(),
            super::DEFAULT_SUBSTITUTE_CONCURRENCY,
            crate::estimator::DEFAULT_HEADROOM_MULTIPLIER,
            None,
            None,
            None,
            rio_common::signal::Token::new(),
        )
    }

    /// Spawn with an external leader state. main.rs uses this
    /// when `RIO_LEASE_NAME` is set — the lease task writes to
    /// `leader.generation` and `leader.is_leader`; the actor
    /// reads both.
    ///
    /// `leader = None` → construct a fresh `LeaderState::always_leader`
    /// internally (same as the default `spawn()`). `Some` → share
    /// the caller's state.
    ///
    /// Separate fn rather than adding a param to `spawn()`: the
    /// test callsites (setup_actor etc) all use `spawn()` with no
    /// lease; keeping that signature stable avoids updating ~8
    /// test helpers.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_leader(
        db: SchedulerDb,
        store_client: Option<StoreServiceClient<Channel>>,
        log_flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
        size_classes: Vec<crate::assignment::SizeClassConfig>,
        poison_config: PoisonConfig,
        retry_policy: RetryPolicy,
        substitute_max_concurrent: usize,
        headroom_mult: f64,
        leader: Option<crate::lease::LeaderState>,
        event_persist_tx: Option<mpsc::Sender<crate::event_log::EventLogEntry>>,
        hmac_signer: Option<rio_common::hmac::HmacSigner>,
        shutdown: rio_common::signal::Token,
    ) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let mut actor = DagActor::new(db, store_client)
            .with_size_classes(size_classes)
            .with_substitute_concurrency(substitute_max_concurrent)
            .with_headroom_mult(headroom_mult)
            // r[impl sched.retry.per-worker-budget]
            // scheduler.md:110 — "Both knobs are configurable via
            // scheduler.toml". P0219 shipped the structs + builders;
            // this threads the TOML-loaded values through. The
            // `DagActor::new` defaults (RetryPolicy::default(),
            // PoisonConfig::default()) are immediately replaced by
            // whatever main.rs loaded — which is ALSO Default::
            // default() in the no-config case via `#[serde(default)]`.
            // Net effect: same behavior unless the operator writes
            // a `[poison]` or `[retry]` table.
            .with_poison_config(poison_config)
            .with_retry_policy(retry_policy)
            .with_shutdown_token(shutdown);
        if let Some(flush_tx) = log_flush_tx {
            actor = actor.with_log_flusher(flush_tx);
        }
        if let Some(persist_tx) = event_persist_tx {
            actor = actor.with_event_persister(persist_tx);
        }
        if let Some(signer) = hmac_signer {
            actor = actor.with_hmac_signer(signer);
        }

        // Wire leader state. With None: the actor's default
        // is_leader=true is fine, and we just clone the actor's
        // own generation Arc for the reader (no external writer).
        // With Some: inject the SHARED is_leader and verify the
        // generation Arc is the one we expect (caller must have
        // built LeaderState from actor.generation_arc()... but
        // we haven't given them that yet. Chicken-and-egg.)
        //
        // RESOLVED: LeaderState carries its OWN generation Arc.
        // The caller builds it BEFORE spawn. We inject BOTH into
        // the actor, REPLACING the actor's default. The actor's
        // generation reader and dispatch.rs then see the shared
        // Arc. No chicken-and-egg.
        if let Some(leader) = leader {
            // This replaces the actor's default Arcs with the
            // caller's. Same init values, shared references. All
            // three flow from LeaderState: the lease task writes
            // is_leader + generation + clears recovery_complete;
            // the actor reads all three (dispatch) and writes
            // recovery_complete (handle_leader_acquired).
            actor = actor
                .with_leader_flag(leader.is_leader)
                .with_generation(leader.generation)
                .with_recovery_flag(leader.recovery_complete);
        }

        let backpressure = actor.backpressure_flag();
        let generation = actor.generation_reader();
        let self_tx = tx.downgrade();
        rio_common::task::spawn_monitored("dag-actor", actor.run_with_self_tx(rx, self_tx));
        Self {
            tx,
            backpressure,
            generation,
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

    /// Test-only: force a derivation to Assigned for a given
    /// worker, bypassing dispatch's backoff + failed_builders
    /// exclusion. For retry/poison tests that drive multiple
    /// completion cycles. Returns `false` if the derivation
    /// couldn't be forced (terminal state, not found).
    #[cfg(test)]
    pub async fn debug_force_assign(
        &self,
        drv_hash: &str,
        executor_id: &str,
    ) -> Result<bool, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(ActorCommand::DebugForceAssign {
            drv_hash: drv_hash.to_string(),
            executor_id: executor_id.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }

    /// Test-only: backdate `running_since` and force Running status.
    /// For backstop-timeout tests. Returns `false` if not found or
    /// not in Assigned/Running.
    #[cfg(test)]
    pub async fn debug_backdate_running(
        &self,
        drv_hash: &str,
        secs_ago: u64,
    ) -> Result<bool, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(ActorCommand::DebugBackdateRunning {
            drv_hash: drv_hash.to_string(),
            secs_ago,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }

    /// Test-only: backdate a build's `submitted_at`. For per-build-
    /// timeout tests. Returns `false` if build not found.
    #[cfg(test)]
    pub async fn debug_backdate_submitted(
        &self,
        build_id: Uuid,
        secs_ago: u64,
    ) -> Result<bool, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(ActorCommand::DebugBackdateSubmitted {
            build_id,
            secs_ago,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }

    /// Test-only: clear a derivation's `drv_content` to simulate
    /// post-recovery state. Returns `false` if not found.
    #[cfg(test)]
    pub async fn debug_clear_drv_content(&self, drv_hash: &str) -> Result<bool, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(ActorCommand::DebugClearDrvContent {
            drv_hash: drv_hash.to_string(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }

    /// Test-only: call `cache_breaker.record_failure()` `n` times.
    /// Returns `is_open()` after. For breaker-gate tests that need
    /// the breaker open WITHOUT driving N failing RPCs through the
    /// full merge/completion path.
    #[cfg(test)]
    pub async fn debug_trip_breaker(&self, n: u32) -> Result<bool, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.send_unchecked(ActorCommand::DebugTripBreaker { n, reply: tx })
            .await?;
        rx.await.map_err(|_| ActorError::ChannelSend)
    }
}
