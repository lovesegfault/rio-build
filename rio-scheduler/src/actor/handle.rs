//! External-facing actor handle.

use super::*;

/// Test-only: snapshot of worker state for assertions.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct DebugWorkerInfo {
    pub worker_id: String,
    pub is_registered: bool,
    pub systems: Vec<String>,
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
    pub assigned_size_class: Option<String>,
    pub output_paths: Vec<String>,
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
        // For K8s deployments main.rs calls spawn_with_lease().
        Self::spawn_with_leader(db, store_client, log_flush_tx, size_classes, None, None)
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
    pub fn spawn_with_leader(
        db: SchedulerDb,
        store_client: Option<StoreServiceClient<Channel>>,
        log_flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
        size_classes: Vec<crate::assignment::SizeClassConfig>,
        leader: Option<crate::lease::LeaderState>,
        event_persist_tx: Option<mpsc::Sender<crate::event_log::EventLogEntry>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
        let mut actor = DagActor::new(db, store_client).with_size_classes(size_classes);
        if let Some(flush_tx) = log_flush_tx {
            actor = actor.with_log_flusher(flush_tx);
        }
        if let Some(persist_tx) = event_persist_tx {
            actor = actor.with_event_persister(persist_tx);
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
            // This replaces the actor's default Arc(AtomicU64(1))
            // with the caller's. Caller initializes it to 1 too
            // (LeaderState constructors do). Same init, shared ref.
            actor = actor
                .with_leader_flag(leader.is_leader)
                .with_generation(leader.generation);
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
