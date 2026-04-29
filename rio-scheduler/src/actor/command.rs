//! Command/message types for the DAG actor.
//!
//! All gRPC handlers communicate with the actor via an mpsc channel carrying
//! [`ActorCommand`] variants. Reply channels (oneshot) carry results back.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::state::{BuildOptions, DrvHash, ExecutorId, PriorityClass};

#[cfg(test)]
use super::handle::DebugDerivationInfo;
use super::handle::DebugExecutorInfo;

/// Payload for [`ActorCommand::Heartbeat`]. Passed by value into
/// `handle_heartbeat` instead of unpacking 9 positionals at the
/// run_inner arm.
pub struct HeartbeatPayload {
    pub executor_id: ExecutorId,
    /// Systems this worker can build for. Usually single-element
    /// but multi-arch workers (e.g., qemu-user-static) declare
    /// multiple. `rejection_reason` any-matches against the
    /// derivation's target. Empty vec is rejected at the gRPC layer
    /// (handle_heartbeat treats empty systems as "not registered").
    pub systems: Vec<String>,
    pub supported_features: Vec<String>,
    /// drv_path the worker reports as in-flight (not a hash). P0537:
    /// at most one build per pod — the wire field is `optional string
    /// running_build`, passed through as-is.
    pub running_build: Option<String>,
    /// ResourceUsage from the heartbeat. Prost generates Option for
    /// message fields; worker always populates, so None is defensive
    /// (shouldn't happen). Stored on ExecutorState as `last_resources`
    /// for `ListExecutors`.
    pub resources: Option<rio_proto::types::ResourceUsage>,
    /// FUSE circuit breaker open — worker can't fetch from store.
    /// Proto bool, field 9: wire-default false (old workers don't
    /// send it). Stored on ExecutorState; `has_capacity()` gates on
    /// it the same as `draining`.
    pub store_degraded: bool,
    /// SIGTERM received — finishing in-flight, not accepting new
    /// work. Proto bool, field 11. The worker is the authority
    /// (it knows whether it got SIGTERM); `handle_heartbeat`
    /// overwrites `worker.draining` from this every tick. I-063:
    /// supersedes I-056a's reconnect-clears-draining, which was
    /// wrong when the SAME draining process reconnects after a
    /// scheduler restart.
    pub draining: bool,
    /// Builder or Fetcher (from `HeartbeatRequest.kind`, proto
    /// field 10). Stored on ExecutorState; `hard_filter()` routes
    /// FODs to fetchers and non-FODs to builders (ADR-019).
    /// Wire-default 0 = Builder (pre-ADR-019 executors don't send
    /// it; treated as builders, the safe default).
    pub kind: rio_proto::types::ExecutorKind,
    /// ADR-023 SpawnIntent match key. `None` = Static-sized pod
    /// (proto empty-string). Stored on ExecutorState; dispatch
    /// (Phase-2) prefers the pre-computed assignment for this
    /// intent_id, falling through to pick-from-queue if no match
    /// (e.g., scheduler restarted between spawn and heartbeat).
    pub intent_id: Option<String>,
    /// k8s `spec.nodeName` (proto field 14, downward-API). `None` =
    /// proto empty-string (non-k8s builder). Stored on ExecutorState
    /// for `detect_hung_nodes`.
    pub node_name: Option<String>,
}

/// Request payload for [`ActorCommand::MergeDag`].
#[derive(Debug)]
pub struct MergeDagRequest {
    pub build_id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub priority_class: PriorityClass,
    pub nodes: Vec<rio_proto::types::DerivationNode>,
    pub edges: Vec<rio_proto::types::DerivationEdge>,
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
        reply: oneshot::Sender<Result<super::BuildEventReceivers, ActorError>>,
    },

    /// Process a completion report from a worker.
    ProcessCompletion {
        executor_id: ExecutorId,
        /// Either a drv_hash OR a full drv_path — handle_completion resolves both.
        /// Workers send drv_path; tests sometimes send drv_hash directly.
        drv_key: String,
        result: rio_proto::types::BuildResult,
        /// Peak memory from cgroup `memory.peak`, bytes. 0 = no signal
        /// (build failed before cgroup populated). Feeds the
        /// `build_samples` insert for the SLA mem fit.
        peak_memory_bytes: u64,
        /// Peak CPU cores-equivalent, polled 1Hz from cgroup
        /// `cpu.stat`. 0.0 = no signal (exited before first sample).
        /// Feeds `build_samples.peak_cpu_cores` for the SLA saturation
        /// detector.
        peak_cpu_cores: f64,
        /// k8s `spec.nodeName` the executor pod ran on (downward API
        /// → `RIO_NODE_NAME` → `CompletionReport.node_name`). For
        /// ADR-023's hw_class join. `None` = old executor / non-k8s.
        node_name: Option<String>,
        /// `RIO_HW_CLASS` (controller-stamped pod annotation →
        /// `CompletionReport.hw_class`). Written through to
        /// `build_samples.hw_class`; the scheduler has no Node informer
        /// so this is the only path. `None` = old executor / non-k8s /
        /// annotator hadn't stamped yet.
        hw_class: Option<String>,
        /// Builder's final cgroup-poll snapshot. Carries the ADR-023
        /// telemetry (`cpu_limit_cores`, `cpu_seconds_total`,
        /// `peak_io_pressure_pct`, `peak_disk_bytes`) for the
        /// `build_samples` insert. `None` = old executor.
        final_resources: Option<rio_proto::types::ResourceUsage>,
    },

    /// A detached upstream-substitute fetch (spawned by
    /// `spawn_substitute_fetches`) has finished. `ok=true` → all
    /// output paths now present in rio-store; handler completes the
    /// derivation. `ok=false` → fetch failed; handler reverts to
    /// Ready/Queued for normal scheduling. r[sched.substitute.detached+2]
    SubstituteComplete { drv_hash: DrvHash, ok: bool },

    /// Byte-level progress from a detached substitute fetch's closure
    /// walk. `bytes_done`/`bytes_expected` are AGGREGATE across all
    /// paths in the closure walked so far (`walk_substitute_closure`
    /// accumulates). Handler emits `Event::SubstituteProgress` to the
    /// drv's interested builds via the log broadcast ring (display-only;
    /// not persisted). r[gw.activity.subst-progress]
    SubstituteProgress {
        drv_hash: DrvHash,
        bytes_done: u64,
        bytes_expected: u64,
        upstream_uri: String,
    },

    /// Cancel a build.
    CancelBuild {
        build_id: Uuid,
        /// `r[sched.tenant.authz]`: attested `claims.sub` from the
        /// gRPC layer's `require_tenant`. `Some` → handler verifies
        /// `build.tenant_id == caller_tenant` and rejects with
        /// `PermissionDenied` on mismatch. `None` (dev mode / admin
        /// path) → unchecked.
        caller_tenant: Option<Uuid>,
        reason: String,
        reply: oneshot::Sender<Result<bool, ActorError>>,
    },

    /// A worker opened a BuildExecution stream.
    ExecutorConnected {
        executor_id: ExecutorId,
        stream_tx: mpsc::Sender<rio_proto::types::SchedulerMessage>,
        /// Per-stream epoch (monotonic across the process). The reader
        /// task echoes this on `ExecutorDisconnected`; a stale
        /// disconnect from a prior stream (I-056a connect-before-
        /// disconnect ordering) is ignored when the epoch doesn't
        /// match the entry's current `stream_epoch`.
        stream_epoch: u64,
        /// `r[sec.executor.identity-token]`: HMAC-attested
        /// `ExecutorClaims.intent_id` from `x-rio-executor-token`.
        /// `handle_worker_connected` rejects reconnect when this
        /// doesn't match the stored `intent_id` (stream-hijack
        /// guard). `None` in dev mode (no HMAC key configured).
        auth_intent: Option<String>,
        /// `r[sec.executor.identity-token]`: accept-gate. The gRPC
        /// handler awaits this BEFORE spawning the
        /// `worker-stream-reader` task; on `Err`, it returns
        /// `PERMISSION_DENIED` and the reader is never spawned.
        /// Without it, a spoofed `Register{executor_id=E_victim}` is
        /// rejected by the actor (live-stream / intent-mismatch) but
        /// the reader keeps forwarding `ProcessCompletion{E_victim}`
        /// — forging terminal results for another tenant's build.
        reply: oneshot::Sender<Result<(), &'static str>>,
    },

    /// A worker's BuildExecution stream closed.
    ExecutorDisconnected {
        executor_id: ExecutorId,
        /// Epoch of the stream that closed. Compared against
        /// `ExecutorState::stream_epoch`; mismatch → stale disconnect
        /// from a prior stream → no-op.
        stream_epoch: u64,
        /// `derivation_path` keys this stream pushed into
        /// `LogBuffers`. The actor — AFTER the epoch check — discards
        /// only those the DAG has never heard of (fabricated by an
        /// untrusted worker, or post-cleanup). Real drvs are reaped by
        /// the existing machinery: `seal()` on completion, `discard()`
        /// on next `assign_to_worker`, `discard()` in
        /// `handle_cleanup_terminal_build`. Moved out of the reader
        /// task: branching on `is_sealed` there raced the actor's
        /// `seal()` (TOCTOU under load), wasn't epoch-gated (stale
        /// reader wiped the reconnected stream's fresh buffer), and
        /// had no ownership check (compromised worker → discard a
        /// victim's buffer).
        seen_drvs: Vec<String>,
    },

    /// Controller observed a builder/fetcher Pod's container terminate
    /// and reports the k8s reason (OOMKilled / Evicted-DiskPressure /
    /// etc.). `OomKilled`/`EvictedDiskPressure` → promote
    /// `resource_floor` for whatever drv was running at disconnect
    /// (resolved via `recently_disconnected`). Other reasons → no-op.
    ///
    /// `send_unchecked`: a dropped report means a real OOM doesn't
    /// promote → retry-storm on the same undersized class. Same
    /// "must land under backpressure" reasoning as DrainExecutor.
    ReportExecutorTermination {
        executor_id: ExecutorId,
        reason: rio_proto::types::TerminationReason,
        reply: oneshot::Sender<bool>,
    },

    /// Controller acked it created Jobs for these intents → arm the
    /// Pending-watch (ICE-backoff) timer for each band-targeted one.
    /// Separated from `GetSpawnIntents` so that path stays read-only:
    /// dashboard/CLI polls and headroom-gated intents the controller
    /// truncated don't false-mark `(band, cap)` ICE-infeasible.
    ///
    /// `send_unchecked`: a dropped ack means the timer doesn't arm →
    /// no false-ICE, just delayed detection until the next spawn —
    /// acceptable under backpressure.
    AckSpawnedIntents {
        spawned: Vec<rio_proto::types::SpawnIntent>,
        /// §13b: cells the controller saw NodeClaim Launched=False /
        /// Registered timeout for this tick. Scheduler marks each
        /// ICE-infeasible on a backoff ladder. Consumed by B11; until
        /// then the actor handler accepts and ignores it.
        unfulfillable_cells: Vec<String>,
        /// §13b: cells for which a NodeClaim reached
        /// `Registered=True` this tick — the success signal that
        /// resets ICE backoff. `vec![]` until the controller's
        /// NodeClaim watcher (A18) populates it; the §13a interim
        /// clear path is first-heartbeat instead.
        registered_cells: Vec<String>,
        /// `r[sched.sla.cost-instance-type-feedback]`: per-cell
        /// instance types Karpenter resolved this tick. Folded into
        /// `CostTable.cells` so `spot_price_poller` knows what to
        /// price. Edge-detected per NodeClaim (controller-side).
        observed_instance_types: Vec<rio_proto::types::ObservedInstanceType>,
    },

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
    Heartbeat(HeartbeatPayload),

    /// Periodic tick for housekeeping (timeouts, poison TTL expiry).
    Tick,

    /// Query build status.
    QueryBuildStatus {
        build_id: Uuid,
        /// See [`ActorCommand::CancelBuild::caller_tenant`].
        caller_tenant: Option<Uuid>,
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
        /// See [`ActorCommand::CancelBuild::caller_tenant`].
        caller_tenant: Option<Uuid>,
        since_sequence: u64,
        reply: oneshot::Sender<Result<(super::BuildEventReceivers, u64), ActorError>>,
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
    /// The actor resolves it via `DerivationDag::hash_for_path` (DAG reverse index).
    ForwardLogBatch {
        drv_path: String,
        batch: rio_proto::types::BuildLogBatch,
    },

    /// Forward a build-phase change to interested gateways via
    /// `emit_build_event`. Same `try_send`-under-backpressure semantics
    /// as [`ForwardLogBatch`](Self::ForwardLogBatch) — a dropped phase
    /// is a cosmetic nom regression, not a hang. Fire-and-forget.
    ForwardPhase { phase: rio_proto::types::BuildPhase },

    /// Mark a worker draining: stop sending new assignments.
    ///
    /// Called by the worker itself (step 1 of SIGTERM drain) or by
    /// the controller (Pool finalizer cleanup). Idempotent: an
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

    /// Read-only admin/snapshot query. See [`AdminQuery`].
    Admin(AdminQuery),

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

    /// Lease lost (or self-fenced): clear in-memory builds/dag/events
    /// and zero the leader-only state gauges. Symmetric with
    /// `LeaderAcquired`. Fire-and-forget — the lease loop has already
    /// flipped `is_leader=false` via `on_lose()`; this command brings
    /// the actor's persisted state in line so a long-lived standby
    /// doesn't (a) hold a stale DAG indefinitely, (b) export frozen
    /// gauge values.
    LeaderLost,

    /// Post-recovery worker reconciliation (spec step 6). Scheduled
    /// ~45s after recovery via WeakSender. For each Assigned/Running
    /// derivation: if assigned_executor NOT in self.executors →
    /// query store → Completed (orphan) or reset to Ready.
    ReconcileAssignments,

    /// `cfg(test)` debug command. See [`DebugCmd`].
    #[cfg(test)]
    Debug(DebugCmd),
}

/// Read-only admin/snapshot queries on actor in-memory state. All
/// `&self`; `send_unchecked` — the controller's autoscaling loop and
/// dashboards need a reading even (especially!) when the scheduler is
/// saturated; dropping the snapshot under backpressure blinds the
/// autoscaler exactly when it needs to scale up.
pub enum AdminQuery {
    /// Snapshot per-derivation spawn intents for
    /// `AdminService.GetSpawnIntents`. O(dag_nodes) — single pass over
    /// Ready derivations, one `solve_intent_for` each.
    GetSpawnIntents {
        req: SpawnIntentsRequest,
        reply: oneshot::Sender<SpawnIntentsSnapshot>,
    },
    /// Return expected output paths for all non-terminal
    /// derivations. Used by TriggerGC to pass as extra_roots to
    /// the store's mark phase — protects in-flight build outputs
    /// that may not be in narinfo yet (worker hasn't uploaded).
    GcRoots { reply: oneshot::Sender<Vec<String>> },
    /// Snapshot all workers for `AdminService.ListExecutors`.
    /// O(workers) scan; acceptable for dashboard polling.
    ListExecutors {
        reply: oneshot::Sender<Vec<ExecutorSnapshot>>,
    },
    /// Actor in-memory snapshot of a build's derivations + live
    /// executor stream IDs. I-025 diagnostic: surfaces the PG-vs-
    /// stream-pool mismatch that silently freezes dispatch. Unlike
    /// GetBuildGraph (PG-backed, works for completed builds), this
    /// is the exact view dispatch_ready() sees.
    InspectBuildDag {
        build_id: Uuid,
        reply: oneshot::Sender<(Vec<rio_proto::types::DerivationDiagnostic>, Vec<String>)>,
    },
    /// Actor in-memory executor map snapshot. I-048b/c diagnostic:
    /// surfaces `has_stream`/`warm`/`kind` per entry — what
    /// `dispatch_ready()` filters on, not what PG `last_seen` claims.
    /// Promoted from cfg(test) for the `DebugListExecutors` RPC; tests
    /// keep using it for assertions.
    DebugQueryWorkers {
        reply: oneshot::Sender<Vec<DebugExecutorInfo>>,
    },
    /// `AdminService.SlaStatus`: snapshot one cached `FittedParams` +
    /// the override that would apply to this key. Reflects the last
    /// tick refresh (~60s stale at worst). Reply tuple is `(fit,
    /// active_override_row)` — the RPC handler projects to proto.
    SlaStatus {
        key: crate::sla::types::ModelKey,
        reply: oneshot::Sender<(
            Option<crate::sla::types::FittedParams>,
            Option<crate::db::SlaOverrideRow>,
        )>,
    },
    /// `AdminService.ResetSlaModel` (cache half): drop one cached fit
    /// so the next dispatch falls back to the cold-start probe path.
    /// The PG `DELETE FROM build_samples` happens in the RPC handler
    /// BEFORE this fires; reply is whether an entry was present.
    /// Mutates via `RwLock` so still `&self`-dispatchable from
    /// `handle_admin`.
    SlaEvict {
        key: crate::sla::types::ModelKey,
        reply: oneshot::Sender<bool>,
    },
    /// `AdminService.SlaExplain`: re-run the tier walk for one key in
    /// dry-run mode. Reply is the full [`ExplainResult`] — the RPC
    /// handler projects to proto. Same ~60s tick staleness as
    /// `SlaStatus`.
    ///
    /// [`ExplainResult`]: crate::sla::explain::ExplainResult
    SlaExplain {
        key: crate::sla::types::ModelKey,
        reply: oneshot::Sender<crate::sla::explain::ExplainResult>,
    },
    /// `AdminService.GetSlaMispredictors`: top-`n` recent `|1 − ratio|`
    /// observations from the in-memory ring. Reply is the deduped +
    /// sorted entry list; the RPC handler projects to proto.
    SlaMispredictors {
        top_n: u32,
        reply: oneshot::Sender<Vec<crate::sla::metrics::MispredictorEntry>>,
    },
    /// `AdminService.ExportSlaCorpus`: dump every cached fit with
    /// `n_eff ≥ min_n` (optionally tenant-filtered) as a portable seed
    /// corpus. Reply is the corpus struct; the RPC handler serializes.
    SlaExportCorpus {
        tenant: Option<String>,
        min_n: u32,
        reply: oneshot::Sender<crate::sla::prior::SeedCorpus>,
    },
    /// `AdminService.ImportSlaCorpus`: merge a validated corpus into
    /// the seed-prior table. Reply is `(entries, rescale_factor)`.
    SlaImportCorpus {
        corpus: crate::sla::prior::ValidatedSeedCorpus,
        reply: oneshot::Sender<(usize, f64)>,
    },
    /// `AdminService.HwClassSampled`: per-hw_class **per-dimension
    /// distinct-tenant** count from the estimator's last
    /// `HwTable::load` (bug_013: same unit + granularity as
    /// `cross_tenant_median`'s `min_tenants` gate). Reply is
    /// `h → [u32; K]`; absent classes map to `[0; K]`.
    SlaHwSampled {
        hw_classes: Vec<String>,
        reply: oneshot::Sender<std::collections::HashMap<String, [u32; crate::sla::hw::K]>>,
    },
    /// `AdminService.MintExecutorTokens`: HMAC-sign `ExecutorClaims` for
    /// each requested `intent_id`. Controller-only — the credential
    /// lives on a controller-only surface so dashboard/CLI never hold
    /// it (`r[sec.executor.identity-token]`). Reply is
    /// `intent_id → token`; intent_ids not in the current
    /// `compute_spawn_intents` snapshot are omitted.
    MintExecutorTokens {
        intent_ids: Vec<String>,
        reply: oneshot::Sender<std::collections::HashMap<String, String>>,
    },
}

/// `cfg(test)` debug commands that bypass the state machine / dispatch
/// path so tests can set up preconditions directly.
#[cfg(test)]
pub enum DebugCmd {
    /// Query a derivation's state.
    QueryDerivation {
        drv_hash: String,
        reply: oneshot::Sender<Option<DebugDerivationInfo>>,
    },
    /// Force a derivation to Assigned with the given worker, bypassing
    /// dispatch + backoff. For retry/poison tests that need to drive
    /// multiple completion cycles without waiting for real backoff.
    ForceAssign {
        drv_hash: String,
        executor_id: ExecutorId,
        reply: oneshot::Sender<bool>,
    },
    /// Set `worker.running_build = Some(drv_hash)` directly, bypassing
    /// the DAG-status guard. For heartbeat-reconcile safety-net tests
    /// that need a `running_build` entry pointing at a terminal-status
    /// drv (where `ForceAssign` would refuse the Poisoned→Assigned
    /// transition).
    SetRunningBuild {
        executor_id: ExecutorId,
        drv_hash: String,
        reply: oneshot::Sender<bool>,
    },
    /// Backdate a derivation's `running_since` and force it into
    /// Running status. For backstop-timeout tests: with the cfg(test)
    /// floor of 0s, any positive elapsed triggers the backstop on the
    /// next Tick.
    BackdateRunning {
        drv_hash: String,
        secs_ago: u64,
        reply: oneshot::Sender<bool>,
    },
    /// Backdate a build's `submitted_at` timestamp. For per-build-
    /// timeout tests (`sched.timeout.per-build` spec): `submitted_at`
    /// is `std::time::Instant` — tokio paused time cannot mock it.
    BackdateSubmitted {
        build_id: Uuid,
        secs_ago: u64,
        reply: oneshot::Sender<bool>,
    },
    /// Force a derivation into `Poisoned` with the given
    /// `resubmit_cycles`. For the I-169 resubmit-bound tests
    /// (`sched.merge.poisoned-resubmit-bounded`).
    ForcePoisoned {
        drv_hash: String,
        resubmit_cycles: u32,
        reply: oneshot::Sender<bool>,
    },
    /// Force a derivation into an arbitrary status, bypassing the
    /// transition table. For tests that need a precondition the state
    /// machine has no path to (e.g. a `Skipped` node with output_paths
    /// already set, for the I-047 stale-reset Skipped lane).
    ForceStatus {
        drv_hash: String,
        status: crate::state::DerivationStatus,
        reply: oneshot::Sender<bool>,
    },
    /// Overwrite a derivation's `output_paths`. For tests staging a
    /// pre-existing Completed/Skipped node without driving the full
    /// worker→completion path (which is one-shot per worker).
    SetOutputPaths {
        drv_hash: String,
        paths: Vec<String>,
        reply: oneshot::Sender<bool>,
    },
    /// Set a derivation's `topdown_pruned`. For staging the
    /// B1-topdown / B2-full-merge race deterministically.
    SetTopdownPruned {
        drv_hash: String,
        value: bool,
        reply: oneshot::Sender<bool>,
    },
    /// Clear a derivation's `drv_content`. Simulates the post-recovery
    /// state for the `sched.ca.resolve` recovery-fetch test.
    ClearDrvContent {
        drv_hash: String,
        reply: oneshot::Sender<bool>,
    },
    /// Call `cache_breaker.record_failure()` `n` times. For CA
    /// cutoff-compare breaker-integration tests. `OPEN_THRESHOLD` is 5;
    /// callers pass `n=5` to trip immediately.
    TripBreaker {
        n: u32,
        reply: oneshot::Sender<bool>,
    },
    /// Backdate an executor's `last_heartbeat`. For heartbeat-timeout
    /// tests: `tokio::time::pause` interferes with PG pool timeouts so
    /// real-time can't be advanced. With this, one Tick at
    /// `secs_ago > HEARTBEAT_TIMEOUT_SECS` proves the reap fires
    /// without the pre-fix double-multiply.
    BackdateHeartbeat {
        executor_id: ExecutorId,
        secs_ago: u64,
        reply: oneshot::Sender<bool>,
    },
    /// Seed the SLA estimator's hw_table. For ref-seconds → wall-seconds
    /// denormalization tests (`min_factor()` needs a non-default table).
    SeedHwTable {
        factors: std::collections::HashMap<String, f64>,
        reply: oneshot::Sender<()>,
    },
    /// Swap the actor's `SchedulerDb` for a fresh pool. For
    /// transient-DB-fault recovery tests: `db.pool.close()` closes
    /// all clones (incl. the actor's), so [`TestDb::reopen`] mints a
    /// fresh pool to the same database and this installs it.
    ///
    /// [`TestDb::reopen`]: rio_test_support::TestDb::reopen
    SwapDb {
        pool: sqlx::PgPool,
        reply: oneshot::Sender<()>,
    },
    /// Snapshot the actor's [`TestCounters`](super::TestCounters).
    /// For structural assertions on call-count (vs. wall-clock or
    /// absence-of-side-effect) — see I-163 / I-139 regression tests.
    Counters {
        reply: oneshot::Sender<super::TestCountersSnapshot>,
    },
    /// Seed `state.sched.last_intent` and/or `resource_floor` for D4
    /// floor tests. Per-field `Option` (builder-style); any `Some`
    /// field materializes a `last_intent`.
    SeedSchedHint {
        drv_hash: String,
        est_memory_bytes: Option<u64>,
        est_disk_bytes: Option<u64>,
        est_deadline_secs: Option<u32>,
        floor: Option<crate::state::ResourceFloor>,
        reply: oneshot::Sender<bool>,
    },
}

impl AdminQuery {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::GetSpawnIntents { .. } => "GetSpawnIntents",
            Self::GcRoots { .. } => "GcRoots",
            Self::ListExecutors { .. } => "ListExecutors",
            Self::InspectBuildDag { .. } => "InspectBuildDag",
            Self::DebugQueryWorkers { .. } => "DebugQueryWorkers",
            Self::SlaStatus { .. } => "SlaStatus",
            Self::SlaEvict { .. } => "SlaEvict",
            Self::SlaExplain { .. } => "SlaExplain",
            Self::SlaMispredictors { .. } => "SlaMispredictors",
            Self::SlaExportCorpus { .. } => "SlaExportCorpus",
            Self::SlaHwSampled { .. } => "SlaHwSampled",
            Self::SlaImportCorpus { .. } => "SlaImportCorpus",
            Self::MintExecutorTokens { .. } => "MintExecutorTokens",
        }
    }
}

impl ActorCommand {
    /// Static variant name for per-command latency instrumentation
    /// (I-140). Used as the `cmd` label on the actor-loop histogram +
    /// slow-WARN. `&'static str` so the metrics layer doesn't allocate
    /// a label per command.
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::MergeDag { .. } => "MergeDag",
            Self::ProcessCompletion { .. } => "ProcessCompletion",
            Self::SubstituteComplete { .. } => "SubstituteComplete",
            Self::SubstituteProgress { .. } => "SubstituteProgress",
            Self::CancelBuild { .. } => "CancelBuild",
            Self::ExecutorConnected { .. } => "ExecutorConnected",
            Self::ExecutorDisconnected { .. } => "ExecutorDisconnected",
            Self::ReportExecutorTermination { .. } => "ReportExecutorTermination",
            Self::AckSpawnedIntents { .. } => "AckSpawnedIntents",
            Self::PrefetchComplete { .. } => "PrefetchComplete",
            Self::Heartbeat(_) => "Heartbeat",
            Self::Tick => "Tick",
            Self::QueryBuildStatus { .. } => "QueryBuildStatus",
            Self::WatchBuild { .. } => "WatchBuild",
            Self::CleanupTerminalBuild { .. } => "CleanupTerminalBuild",
            Self::ForwardLogBatch { .. } => "ForwardLogBatch",
            Self::ForwardPhase { .. } => "ForwardPhase",
            Self::DrainExecutor { .. } => "DrainExecutor",
            Self::Admin(q) => q.name(),
            Self::ClearPoison { .. } => "ClearPoison",
            Self::LeaderAcquired => "LeaderAcquired",
            Self::LeaderLost => "LeaderLost",
            Self::ReconcileAssignments => "ReconcileAssignments",
            #[cfg(test)]
            Self::Debug(_) => "Debug",
        }
    }
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
    /// Whether a build is still in-flight on the worker after drain
    /// (P0537: at most one). For `force=false`, it will complete
    /// normally. For `force=true`, this is `false` (reassigned). The
    /// worker's preStop hook uses this to decide whether to wait.
    pub busy: bool,
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
    /// P0537: at most one build per executor.
    pub busy: bool,
    pub draining: bool,
    pub store_degraded: bool,
    pub connected_since: std::time::Instant,
    pub last_heartbeat: std::time::Instant,
    pub last_resources: Option<rio_proto::types::ResourceUsage>,
}

/// Server-side filter for [`AdminQuery::GetSpawnIntents`]. Mirrors the
/// proto request; collapsed `(filter_features, features)` →
/// `Option<Vec>` so the actor sees the I-176 tristate directly.
#[derive(Debug, Clone, Default)]
pub struct SpawnIntentsRequest {
    /// `None` (proto3 `UNKNOWN`) = unfiltered.
    pub kind: Option<rio_proto::types::ExecutorKind>,
    /// Empty = unfiltered.
    pub systems: Vec<String>,
    /// `None` = unfiltered. `Some(vec![])` = featureless pool —
    /// only emits intents with empty `required_features`.
    pub features: Option<Vec<String>>,
}

/// Point-in-time spawn-intent snapshot for
/// `AdminService.GetSpawnIntents`. Internal (not proto) —
/// `admin/spawn_intents.rs` translates.
#[derive(Debug, Clone, Default)]
pub struct SpawnIntentsSnapshot {
    /// One intent per Ready derivation that passed the request
    /// filter. `intent_id == drv_hash` — dispatch matches
    /// `worker.intent_id == drv_hash` so the mapping is structural
    /// (no separate table to keep in sync).
    pub intents: Vec<rio_proto::types::SpawnIntent>,
    /// Per-system breakdown of Ready derivations (kind/feature filters
    /// NOT applied — same population as
    /// `ClusterSnapshot.queued_by_system`, but `u64` for proto-compat).
    /// The ComponentScaler reads this for the predictive store-replica
    /// signal.
    pub queued_by_system: std::collections::HashMap<String, u64>,
    /// `IceBackoff::masked_cells()` snapshot, formatted via
    /// [`crate::sla::config::cell_label`]. The controller's
    /// `cover_deficit` mask merges this with its own
    /// `detect_vanished` set so a controller restart inherits the
    /// scheduler's accumulated ladder instead of rediscovering ICE
    /// per cell.
    pub ice_masked_cells: Vec<String>,
    /// k8s `spec.nodeName`s the hung-node detector flagged
    /// (`r[sched.admin.hung-node-detector]`). The controller's
    /// `nodeclaim_pool::health` reaps the corresponding NodeClaim
    /// (`ReapReason::Dead`, capped per tick).
    pub dead_nodes: Vec<String>,
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
    /// `DerivationStatus::Substituting` across the DAG. Upstream fetch
    /// in flight (store-side `try_substitute` closure walk + NAR
    /// ingest). Counted as store load for the ComponentScaler
    /// predictive signal (`ctrl.scaler.signal-substituting`).
    pub substituting_derivations: u32,
    /// Per-system breakdown of `queued_derivations` (Ready-only). Sum
    /// across keys == `queued_derivations`. Populated from
    /// `DerivationState.system` during the same DAG iteration that
    /// computes `running_derivations`. Consumed by the controller's
    /// Pool autoscaler so per-arch pools scale on their own
    /// backlog (I-107).
    pub queued_by_system: std::collections::HashMap<String, u32>,
}

/// Errors from the actor.
#[derive(Debug, thiserror::Error, strum::EnumCount)]
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
    /// edges; if `DerivationDag::db_id_for_path` returns None for an endpoint, the
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

    /// `r[sched.tenant.authz]`: caller's attested `claims.sub` does
    /// not own the build. Maps to gRPC PERMISSION_DENIED.
    #[error("permission denied: build {build_id} belongs to a different tenant")]
    PermissionDenied { build_id: Uuid },
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
