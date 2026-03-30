//! Command/message types for the DAG actor.
//!
//! All gRPC handlers communicate with the actor via an mpsc channel carrying
//! [`ActorCommand`] variants. Reply channels (oneshot) carry results back.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::estimator::BucketedEstimate;
use crate::state::{BuildOptions, DrvHash, ExecutorId, PriorityClass};

#[cfg(test)]
use super::handle::{DebugDerivationInfo, DebugExecutorInfo};

/// Request payload for [`ActorCommand::MergeDag`].
#[derive(Debug)]
pub struct MergeDagRequest {
    pub build_id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub priority_class: PriorityClass,
    pub nodes: Vec<rio_proto::dag::DerivationNode>,
    pub edges: Vec<rio_proto::dag::DerivationEdge>,
    pub options: BuildOptions,
    pub keep_going: bool,
    /// W3C traceparent of the submitting gRPC handler's span. Span
    /// context does NOT cross the mpsc channel to the actor task, so
    /// we carry it as plain data. Stored on each newly-inserted
    /// `DerivationState` so dispatch can embed it in `WorkAssignment`
    /// regardless of which code path triggers dispatch.
    pub traceparent: String,
    /// JWT ID (`jti` claim) from the submitting request's Claims, if
    /// the gateway was in JWT mode. Written to `builds.jwt_jti` for
    /// audit-trail queries per r[gw.jwt.issue]. `None` in dev/test
    /// mode (no JWT interceptor) or dual-mode SSH-comment fallback.
    pub jti: Option<String>,
    /// Raw JWT token string (`x-rio-tenant-token` header value) from
    /// the submitting request. Threaded to the merge-time
    /// `FindMissingPaths` store call so the store's per-tenant
    /// upstream substitution probe fires — see
    /// r[sched.merge.substitute-probe]. `None` in the same cases as
    /// `jti`. Distinct from `jti`: `jti` is the DECODED claim (for
    /// revocation lookup); this is the OPAQUE token (for re-inject).
    pub jwt_token: Option<String>,
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
        executor_id: ExecutorId,
        /// Either a drv_hash OR a full drv_path — handle_completion resolves both.
        /// Workers send drv_path; tests sometimes send drv_hash directly.
        drv_key: String,
        result: rio_proto::build_types::BuildResult,
        /// Peak memory from cgroup `memory.peak`, bytes. 0 = no signal
        /// (build failed before cgroup populated). Feeds
        /// `build_history.ema_peak_memory_bytes` for size-class
        /// memory-bump.
        peak_memory_bytes: u64,
        /// Sum of uploaded NAR sizes. 0 = no signal (build failed, no
        /// outputs). Dashboards-only today; column exists so EMA it now.
        output_size_bytes: u64,
        /// Peak CPU cores-equivalent, polled 1Hz from cgroup
        /// `cpu.stat`. 0.0 = no signal (exited before first sample).
        /// Feeds `build_history.ema_peak_cpu_cores` — not used for
        /// routing yet (size-class bumps on memory only).
        peak_cpu_cores: f64,
    },

    /// Cancel a build.
    CancelBuild {
        build_id: Uuid,
        reason: String,
        reply: oneshot::Sender<Result<bool, ActorError>>,
    },

    /// A worker opened a BuildExecution stream.
    ExecutorConnected {
        executor_id: ExecutorId,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
    },

    /// A worker's BuildExecution stream closed.
    ExecutorDisconnected { executor_id: ExecutorId },

    /// A worker ACKed its initial `PrefetchHint` with `PrefetchComplete`.
    /// Flips `ExecutorState.warm = true` so `best_executor()` starts
    /// considering this worker on the warm-pass. Spec:
    /// `r[sched.assign.warm-gate]`.
    ///
    /// `send_unchecked`: same reasoning as ExecutorConnected/Heartbeat.
    /// Dropping this under backpressure would leave a warmed worker
    /// permanently cold in the scheduler's view — dispatchable capacity
    /// sitting idle right when the scheduler is busiest. Feedback loop.
    PrefetchComplete {
        executor_id: ExecutorId,
        /// Observability only — the warm flip gates on receipt, not count.
        paths_fetched: u32,
    },

    /// Periodic heartbeat from a worker.
    Heartbeat {
        executor_id: ExecutorId,
        /// Systems this worker can build for. Usually single-element
        /// but multi-arch workers (e.g., qemu-user-static) declare
        /// multiple. can_build() any-matches against the derivation's
        /// target. Empty vec is rejected at the gRPC layer
        /// (handle_heartbeat treats empty systems as "not registered").
        systems: Vec<String>,
        supported_features: Vec<String>,
        max_builds: u32,
        /// drv_paths from worker proto (not hashes).
        running_builds: Vec<String>,
        /// Parsed bloom filter from local_paths. `None` = worker didn't
        /// send one (old worker, or FUSE not mounted). Stored on
        /// ExecutorState for assignment scoring.
        bloom: Option<rio_common::bloom::BloomFilter>,
        /// Size-class from worker config (e.g. "small", "large"). gRPC
        /// maps empty-string → None. Stored on ExecutorState for the
        /// classify() → best_executor() filter.
        size_class: Option<String>,
        /// ResourceUsage from the heartbeat. Prost generates Option for
        /// message fields; worker always populates, so None is defensive
        /// (shouldn't happen). Stored on ExecutorState as `last_resources`
        /// for `ListExecutors`.
        resources: Option<rio_proto::types::ResourceUsage>,
        /// FUSE circuit breaker open — worker can't fetch from store.
        /// Proto bool, field 9: wire-default false (old workers don't
        /// send it). Stored on ExecutorState; `has_capacity()` gates on
        /// it the same as `draining`.
        store_degraded: bool,
        /// Builder or Fetcher (from `HeartbeatRequest.kind`, proto
        /// field 10). Stored on ExecutorState; `hard_filter()` routes
        /// FODs to fetchers and non-FODs to builders (ADR-019).
        /// Wire-default 0 = Builder (pre-ADR-019 executors don't send
        /// it; treated as builders, the safe default).
        kind: rio_proto::types::ExecutorKind,
    },

    /// Periodic tick for housekeeping (timeouts, poison TTL expiry).
    Tick,

    /// Query build status.
    QueryBuildStatus {
        build_id: Uuid,
        reply: oneshot::Sender<Result<rio_proto::types::BuildStatus, ActorError>>,
    },

    /// Subscribe to an existing build's events. Reply carries
    /// `(receiver, last_seq)` — `last_seq` is the sequence of the
    /// last-emitted event at the moment of subscribe. The gRPC
    /// layer uses it to (a) bound PG replay (`WHERE seq <= last_seq`)
    /// and (b) dedup the broadcast stream (skip `seq <= last_seq` —
    /// those may ALSO be in the broadcast ring's 1024 buffer, and
    /// PG already delivered them).
    ///
    /// `since_sequence` is NOT read by the actor — it's passed
    /// through to the gRPC layer for the PG replay range's lower
    /// bound. The actor only knows about the broadcast.
    WatchBuild {
        build_id: Uuid,
        since_sequence: u64,
        reply: oneshot::Sender<
            Result<(broadcast::Receiver<rio_proto::types::BuildEvent>, u64), ActorError>,
        >,
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
    /// Called by the worker itself (step 1 of SIGTERM drain) or by
    /// the controller (BuilderPool finalizer cleanup). Idempotent: an
    /// already-draining worker replies `accepted=true` with the same
    /// running count. Unknown worker → `accepted=false, running=0`
    /// (not an error; the worker may have already disconnected).
    ///
    /// `force=true` additionally reassigns in-flight builds (same path
    /// as `ExecutorDisconnected`). Use case: operator-initiated forced
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
    DrainExecutor {
        executor_id: ExecutorId,
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

    /// Snapshot SITA-E size-class state for
    /// `AdminService.GetSizeClassStatus`.
    ///
    /// Per-class: effective (post-rebalancer) + configured (TOML)
    /// cutoffs, queued count (ready derivations that classify() into
    /// this class), running count (Assigned/Running derivations whose
    /// `assigned_size_class` matches).
    ///
    /// O(dag_nodes) for the classify() pass over Ready derivations.
    /// Same reasoning as `ClusterSnapshot` — admin RPC, not hot path.
    ///
    /// `send_unchecked`: the WPS autoscaler (P0234) reads this to
    /// decide per-class replica targets. Blinding it under load is the
    /// same failure mode as blinding ClusterStatus.
    GetSizeClassSnapshot {
        reply: oneshot::Sender<Vec<SizeClassSnapshot>>,
    },

    /// Bucketed resource estimates for ready-queue derivations
    /// (ADR-020 capacity manifest). `headroom_mult` applied before
    /// bucketing. Cold-start derivations (no `build_history` sample)
    /// are omitted — controller uses its operator floor.
    ///
    /// `send_unchecked`: the controller polls this to size the
    /// builder fleet. Same blinding-under-load rationale as above.
    CapacityManifest {
        headroom_mult: f64,
        reply: oneshot::Sender<Vec<BucketedEstimate>>,
    },

    /// Return expected output paths for all non-terminal
    /// derivations. Used by TriggerGC to pass as extra_roots to
    /// the store's mark phase — protects in-flight build outputs
    /// that may not be in narinfo yet (worker hasn't uploaded).
    GcRoots { reply: oneshot::Sender<Vec<String>> },

    /// Snapshot all workers for `AdminService.ListExecutors`.
    /// O(workers) scan; acceptable for dashboard polling.
    /// `send_unchecked`: same rationale as `ClusterSnapshot` —
    /// dashboard needs a reading even (especially) under saturation.
    ListExecutors {
        reply: oneshot::Sender<Vec<ExecutorSnapshot>>,
    },

    /// Clear poison state for a derivation: in-mem reset + PG clear.
    /// Returns `true` if the derivation was poisoned and is now cleared.
    /// `false` if not found or not in Poisoned status.
    ///
    /// `send_unchecked`: ClearPoison is operator-initiated, rare,
    /// and should work even under saturation.
    ClearPoison {
        drv_hash: DrvHash,
        reply: oneshot::Sender<bool>,
    },

    /// Lease acquired: trigger state recovery from PG. Fire-and-
    /// forget (no reply) — the lease loop keeps renewing while
    /// recovery runs in the actor task. handle_leader_acquired
    /// sets recovery_complete=true when done (or on failure —
    /// degrade to empty DAG, don't block).
    ///
    /// In non-K8s mode (always_leader): sent once at spawn.
    /// recovery_complete is already true there (no recovery needed
    /// for single-instance) but the command is still processed
    /// (no-op: empty PG → empty DAG → recovery_complete already true).
    LeaderAcquired,

    /// Post-recovery worker reconciliation (spec step 6). Scheduled
    /// ~45s after recovery via WeakSender. For each Assigned/Running
    /// derivation: if assigned_executor NOT in self.executors →
    /// query store → Completed (orphan) or reset to Ready.
    ReconcileAssignments,

    /// Test-only: query worker states.
    #[cfg(test)]
    DebugQueryWorkers {
        reply: oneshot::Sender<Vec<DebugExecutorInfo>>,
    },

    /// Test-only: query a derivation's state.
    #[cfg(test)]
    DebugQueryDerivation {
        drv_hash: String,
        reply: oneshot::Sender<Option<DebugDerivationInfo>>,
    },

    /// Test-only: force a derivation to Assigned state with the
    /// given worker, bypassing dispatch + backoff. Used by retry/
    /// poison tests that need to drive multiple completion cycles
    /// without waiting for real backoff durations.
    ///
    /// With backoff + failed_builders exclusion, dispatch won't
    /// re-assign immediately after a failure. This helper lets tests
    /// control the state machine directly instead of waiting for
    /// real backoff durations.
    #[cfg(test)]
    DebugForceAssign {
        drv_hash: String,
        executor_id: ExecutorId,
        reply: oneshot::Sender<bool>,
    },

    /// Test-only: backdate a derivation's `running_since` and force it
    /// into Running status. For backstop-timeout tests: with the
    /// cfg(test) floor of 0s, any positive elapsed triggers the
    /// backstop on the next Tick. `secs_ago` controls how stale the
    /// timestamp looks. Returns `false` if the derivation isn't in
    /// the DAG or the transition to Running failed.
    #[cfg(test)]
    DebugBackdateRunning {
        drv_hash: String,
        secs_ago: u64,
        reply: oneshot::Sender<bool>,
    },

    /// Test-only: backdate a build's `submitted_at` timestamp. For
    /// per-build-timeout tests (`sched.timeout.per-build` spec): handle_tick
    /// checks `submitted_at.elapsed() > build_timeout`. `submitted_at`
    /// is `std::time::Instant` — tokio paused time cannot mock it, and
    /// paused time breaks PG pool timeouts anyway (see tests/worker.rs
    /// comment). Returns `false` if the build isn't in `self.builds`.
    #[cfg(test)]
    DebugBackdateSubmitted {
        build_id: Uuid,
        secs_ago: u64,
        reply: oneshot::Sender<bool>,
    },

    /// Test-only: clear a derivation's `drv_content`. Simulates the
    /// post-recovery state where the DAG was reloaded from PG but
    /// `drv_content` wasn't persisted (too large to store for every
    /// derivation). For the `sched.ca.resolve` recovery-fetch test:
    /// `maybe_resolve_ca` should fetch the ATerm from the store when
    /// `drv_content` is empty on a CA-on-CA dispatch. Returns `false`
    /// if the derivation isn't in the DAG.
    #[cfg(test)]
    DebugClearDrvContent {
        drv_hash: String,
        reply: oneshot::Sender<bool>,
    },

    /// Test-only: call `cache_breaker.record_failure()` `n` times.
    /// For CA cutoff-compare breaker-integration tests: trip the
    /// breaker open without driving N failing SubmitBuild merges
    /// (slow + lots of boilerplate) or N failing CA completions
    /// (also slow). `OPEN_THRESHOLD` is 5; callers pass `n=5` to
    /// trip immediately. Reply is `is_open()` after the calls.
    #[cfg(test)]
    DebugTripBreaker {
        n: u32,
        reply: oneshot::Sender<bool>,
    },
}

/// Reply for `ActorCommand::DrainExecutor`.
///
/// `accepted=false` only for unknown executor_id — NOT an error, the
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

/// Point-in-time executor snapshot for `AdminService.ListExecutors`.
/// Internal (not proto) — `admin.rs` translates to `ExecutorInfo`.
/// `Instant` fields are converted to wall-clock `SystemTime` there.
#[derive(Debug, Clone)]
pub struct ExecutorSnapshot {
    pub executor_id: ExecutorId,
    pub kind: rio_proto::types::ExecutorKind,
    pub systems: Vec<String>,
    pub supported_features: Vec<String>,
    pub max_builds: u32,
    pub running_builds: u32,
    pub draining: bool,
    pub size_class: Option<String>,
    pub connected_since: std::time::Instant,
    pub last_heartbeat: std::time::Instant,
    pub last_resources: Option<rio_proto::types::ResourceUsage>,
}

/// Point-in-time per-size-class snapshot for
/// `AdminService.GetSizeClassStatus`. Internal (not proto) —
/// `admin/sizeclass.rs` translates + joins DB sample counts.
///
/// `sample_count` is NOT filled here (it's DB state, not actor state).
/// The admin handler queries `build_samples` separately and partitions
/// by the effective cutoffs in THIS snapshot.
#[derive(Debug, Clone)]
pub struct SizeClassSnapshot {
    pub name: String,
    /// Current (post-rebalancer) cutoff, from `size_classes.read()`.
    pub effective_cutoff_secs: f64,
    /// Static TOML cutoff, from `configured_cutoffs` (captured in
    /// `with_size_classes()` before the rebalancer's first write).
    /// Drift visibility: if this diverges from effective, the
    /// rebalancer has moved — check if the workload shifted or the
    /// initial config was wrong.
    pub configured_cutoff_secs: f64,
    /// Ready-status derivations that `classify()` assigns here.
    pub queued: u64,
    /// Assigned/Running derivations with `assigned_size_class == name`.
    pub running: u64,
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
    pub total_executors: u32,
    /// `is_registered() && !draining`. The dispatchable population.
    pub active_executors: u32,
    /// `draining` flag set.
    pub draining_executors: u32,
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
    pub(super) fn new(flag: Arc<AtomicBool>) -> Self {
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

/// Read-only view of the leader generation counter.
///
/// Same pattern as [`BackpressureReader`]: the lease task is the
/// sole writer (via `fetch_add` on the inner Arc it holds directly);
/// everyone else observes. `HeartbeatResponse.generation` and
/// `WorkAssignment.generation` both read from here — workers compare
/// to detect stale assignments after leader failover.
///
/// `Acquire` not `Relaxed`: the generation is a fence. When the lease
/// task acquires leadership and increments, it also sets
/// `is_leader=true`. A reader seeing the new generation should
/// also see the new leader state. Relaxed would be fine in practice
/// (the atomic itself has no reordering peers here) but Acquire makes
/// the pairing with the lease task's Release store explicit.
///
/// Starts at 1 (not 0): generation=0 is the proto-default, so a worker
/// receiving `generation=0` knows the field was unset (old scheduler)
/// rather than "first generation." Non-K8s mode (no lease) stays at 1
/// forever — correct for a single scheduler.
#[derive(Clone)]
pub struct GenerationReader(Arc<AtomicU64>);

impl GenerationReader {
    pub(super) fn new(inner: Arc<AtomicU64>) -> Self {
        Self(inner)
    }

    /// Current leader generation.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
