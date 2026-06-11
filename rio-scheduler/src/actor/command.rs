//! Command/message types for the DAG actor.
//!
//! All gRPC handlers communicate with the actor via an mpsc channel carrying
//! [`ActorCommand`] variants. Reply channels (oneshot) carry results back.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::oneshot;
use uuid::Uuid;

use crate::state::{BuildOptions, DrvHash, ExecutorId, PriorityClass};

#[cfg(test)]
use super::handle::DebugDerivationInfo;

// r[impl sched.sla.ack-validate-then-commit+1]
/// Why an `AckSpawnedIntents` payload was NOT applied
/// (merged_bug_005 — ack means applied under leadership; bug_094 —
/// validate-then-commit: every refusal is computed by
/// `AckApplyPlan::validate` BEFORE the first state mutation, so an
/// erring Ack means NO plane landed). Mapped to a gRPC error by the
/// admin layer; the controller's commit-on-Ack buffer survives an
/// erring Ack and redelivers the whole buffer — safe, because an
/// erring Ack applied nothing, and redelivery after an Ok the
/// controller never observed is a no-op by construction
/// (merged_bug_008: cell events carry producer evidence epochs and
/// the ladder no-ops `epoch <= last_applied[cell]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckApplyError {
    /// Deposed between the gRPC-layer leader check and the actor
    /// drain (`r[sched.lease.standby-drops-writes+3]`
    /// defense-in-depth): the payload was dropped whole. Pre-fix the
    /// gRPC layer had already answered OK on enqueue, so the
    /// controller wiped consume-once evidence the standby never
    /// applied.
    NotLeader,
    /// A plane entry failed the strict shared-grammar decode
    /// (`rio_common::cell_wire`). The WHOLE request is refused before
    /// any mutation — pre-fix the entry was silently dropped while
    /// the Ack answered Ok, and the controller then destroyed its
    /// consume-once buffer on Ack-Ok ("the ONLY clear"). The refusal
    /// is a loud producer-skew signal: the controller warns and
    /// redelivers its retained buffer.
    PlaneEntryUndecodable {
        plane: AckPlane,
        /// The offending wire entry (for `unfulfillable_cells` /
        /// `registered_cells` / `observed_instance_types[].cell`, the
        /// whole string; for the arming plane, the capacity value).
        entry: String,
    },
    /// merged_bug_134: a spawned intent's echoed parallel arrays
    /// (`hw_class_names`, `node_affinity`) are structurally skewed —
    /// both non-empty with unequal lengths, or an aligned term
    /// missing its `karpenter.sh/capacity-type` requirement. Pre-fix
    /// `Iterator::zip` silently truncated to the shorter array: a
    /// 2-cell arm truncated to 1 forges the exactly-one-cell proof
    /// the §13a first-pull ICE clear gates on (`let [cell] =
    /// cells.as_slice()`), clearing the ladder for a cell the pod may
    /// never have scheduled on. Only skew shapes that could forge a
    /// DIFFERENT cell set refuse; the legacy one-side-empty shapes
    /// cannot arm anything and stay typed no-arm lanes
    /// (`ArmDecode::{Empty, LegacyUnarmed}` in the apply plan).
    ArmEchoSkewed {
        intent_id: String,
        /// `hw_class_names.len()` of the skewed echo.
        names: usize,
        /// `node_affinity.len()` of the skewed echo.
        terms: usize,
    },
}

/// Closed alphabet of `AckSpawnedIntentsRequest` evidence planes —
/// the `plane` axis of [`AckApplyError::PlaneEntryUndecodable`]. A
/// new wire plane extends this enum, and the exhaustive matches at
/// the Status mapping (admin) and [`Self::wire_field`] stop
/// compiling until it is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckPlane {
    /// `registered_cells` (wire field 3) — ICE-clear cell events.
    RegisteredCells,
    /// `unfulfillable_cells` (wire field 2) — ICE-mark cell events.
    UnfulfillableCells,
    /// `observed_instance_types[].cell` (wire field 4) — cost-table
    /// observations.
    ObservedTypes,
    /// `spawned[].node_affinity` capacity requirements (wire field 1)
    /// — the arm-on-ack echo.
    SpawnedArming,
    /// `rejected` (wire field 7) — live_051(c) per-intent
    /// no-hosting-class verdicts. An entry whose reason falls outside
    /// the closed `IntentVerdictReason` alphabet refuses the request.
    Rejected,
}

impl AckPlane {
    /// Wire-field name for refusal messages (gRPC Status detail).
    pub fn wire_field(self) -> &'static str {
        match self {
            Self::RegisteredCells => "registered_cells",
            Self::UnfulfillableCells => "unfulfillable_cells",
            Self::ObservedTypes => "observed_instance_types",
            Self::SpawnedArming => "spawned.node_affinity",
            Self::Rejected => "rejected",
        }
    }
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
        /// `CompletionReport.final_line_count`: total log lines the
        /// execution emitted (header + body + footer — the worker
        /// line-number high-water mark after the footer). 0 = not
        /// reported (old executor / the count died with the build
        /// task). Stamped onto `drv_executions.final_line_count`
        /// (0 → SQL NULL) at terminal; rio-store's log completeness
        /// predicate is "the chunk manifest covers a contiguous
        /// `[0, final_line_count)`".
        final_line_count: u64,
    },

    /// Byte-level progress from a store replica's materialization
    /// execution (BC-4: posted by the `ReportMaterializationProgress`
    /// RPC handler). Handler emits `Event::SubstituteProgress` to the
    /// drv's interested builds via the log broadcast ring (display-only;
    /// not persisted). r[gw.activity.subst-progress+4]
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

    /// Pull-mode dispatch: a pod asks for the work it was spawned for
    /// (`ExecutorService.PullAssignment`). Leader-served; the actor
    /// answers Deliver/Gone/NotYetReady or rejects (stale generation,
    /// token mismatch). `send_unchecked` from the gRPC layer: the pod
    /// retries with backoff, so backpressure shows up as retried pulls
    /// rather than dropped work.
    PullAssignment {
        /// SpawnIntent.intent_id == drv hash (the DAG key).
        intent_id: String,
        /// HMAC-attested intent binding (None = dev mode, no key).
        auth_intent: Option<String>,
        /// The attempt class this pull claims (substitution-replacement
        /// Phase A). The gRPC layer maps proto UNSPECIFIED|BUILD →
        /// Build; materialization claims arrive only from store
        /// replicas, and only deliver flag-on.
        kind: rio_evidence_kernel::pull::PullKind,
        /// Per-replica executor identity for materialization pulls (the
        /// store replica's pod name; BC-1). `None` for build pulls —
        /// build-pull identity remains the attested intent (as-built).
        executor_instance: Option<String>,
        /// merged_bug_158: the materialization re-delivery resume token
        /// (`PullAssignmentRequest.resume_exec_id`), parsed at the gRPC
        /// boundary (unparseable ⇒ `None` = deny-by-default fresh
        /// claim). Always `None` for build pulls.
        resume_exec_id: Option<uuid::Uuid>,
        /// bug_251 (rule-4b): the client-chosen claim nonce
        /// (`PullAssignmentRequest.claim_nonce`), parsed at the gRPC
        /// boundary (unparseable ⇒ `None` = deny-by-default). The
        /// credential that survives response loss — persisted with
        /// the assignment at mint, compared by the kernel's
        /// re-delivery cell. Always `None` for build pulls.
        claim_nonce: Option<uuid::Uuid>,
        /// merged_bug_145: SHA-256 hex of the raw executor token —
        /// the confirm-exit fence key, computed at the gRPC boundary
        /// (the only layer that sees the raw credential; the actor
        /// stores/compares hashes only). `None` when the request
        /// carried no token: the IDENTITY-DISABLED deployment class
        /// (dev mode / keyless VM fixtures), routed to
        /// `FenceLane::Unfenced` — no key domain exists by
        /// construction, NOT a fail-open: production cannot reach it
        /// (the credential layer rejects token-less pulls
        /// Unauthenticated before the actor, with a second
        /// fail-closed arm at gRPC dispatch).
        executor_token_sha256: Option<String>,
        /// merged_bug_083: confirm-only pull
        /// (`PullAssignmentRequest.confirm_only`) — a READ of the
        /// puller's holdings. The DeliverNew admission is screened to
        /// NotYetReady at the decision-consumption site, so a confirm
        /// probe can never mint fresh work for a dying/idle-exiting
        /// pod.
        confirm_only: bool,
        reply: oneshot::Sender<Result<super::pull::PullOutcome, super::pull::PullRejection>>,
    },

    /// Store-replica poll for claimable materialization jobs
    /// (`ExecutorService.ListMaterializationJobs`, substitution-
    /// replacement Phase A). Leader-served and read-only; flag-off,
    /// standby, or no claimable jobs all answer an empty list (the
    /// AS-6 mixed-flag posture — never an error).
    /// `send_unchecked`: the store polls on an interval, so a dropped
    /// command is just a delayed listing.
    ListMaterializationJobs {
        /// Cap on returned descriptors (server clamps to 256).
        limit: u32,
        /// The VERIFIED per-worker member identity (live_041) — the
        /// `{pod}-w{n}` composite from the store-service token's
        /// signed instance claim (`ResolvedCredential::StoreService`;
        /// the gRPC chokepoint already rejected instance-unbound
        /// tokens for this surface), threaded so the listing arm can
        /// rendezvous-partition the claimable head per live worker.
        /// `None` = full dev mode (no key families configured): the
        /// caller contributes no member and is served the
        /// unpartitioned listing.
        instance: Option<String>,
        reply: oneshot::Sender<Vec<super::materialize::JobDescriptor>>,
    },

    /// Pull-mode dispatch: a pod reports the terminal outcome of its
    /// pulled attempt (`ExecutorService.ReportOutcome`), idempotent by
    /// exec_id. The reply is sent only after the classification's
    /// appending transaction has committed (the pod's exit-0 waits on
    /// it). `send_unchecked`: dropping a report would strand the
    /// attempt until the establishment sweep.
    ReportPullOutcome {
        exec_id: uuid::Uuid,
        /// HMAC-attested intent binding (None = dev mode, no key).
        auth_intent: Option<String>,
        payload: super::pull::PullReportPayload,
        reply: oneshot::Sender<Result<(), super::pull::PullRejection>>,
    },

    /// The controller's unified pod-terminal classification for one
    /// pull-mode attempt (`AdminService.ReportAttemptOutcome`, the
    /// C4/C5 unification). Idempotent by attempt identity; a report
    /// for an identity with no attempt charges nothing.
    ReportAttemptOutcome {
        identity: super::pull::AttemptIdentity,
        reason: rio_proto::types::AttemptTerminalReason,
        node_name: Option<String>,
        /// 124(b): the `resubmit_cycles` value the controller's verdict
        /// was computed against (`SpawnIntent.resubmit_cycle` echoed
        /// back). Consumed by the `NoEligibleSource` arm only — a
        /// stale echo means the derivation re-entered Ready since the
        /// verdict and the report is acknowledged without poisoning.
        resubmit_cycle: u32,
        /// The typed resolution witness (merged_bug_080 C-2): the
        /// admin layer consumes it into the wire `attempt_resolved`
        /// bit — adding the proto field alone would compile-break only
        /// the response constructor, so THIS typed reply is what makes
        /// the per-arm census compiler-forced at the fold's return
        /// sites.
        reply: oneshot::Sender<Result<super::pull::AttemptResolution, super::pull::PullRejection>>,
    },

    /// Controller acked it created Jobs for these intents → arm the
    /// Pending-watch (ICE-backoff) timer for each band-targeted one.
    /// Separated from `GetSpawnIntents` so that path stays read-only:
    /// dashboard/CLI polls and headroom-gated intents the controller
    /// truncated don't false-mark `(band, cap)` ICE-infeasible.
    ///
    /// `send_unchecked` (backpressure bypass) + `reply`
    /// (merged_bug_005): the gRPC layer answers the controller only
    /// AFTER the leader-gated apply — ack means APPLIED UNDER
    /// LEADERSHIP, never enqueued. A dropped command or a deposed
    /// drain errs the RPC, so the controller's commit-on-Ack buffer
    /// is retained and redelivered (idempotent on every plane:
    /// clears are removes, observed types upsert, marks
    /// refresh-not-step).
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
        /// clear path is the first successful pull instead (see the
        /// mint's ICE-clear in `actor/pull.rs`).
        registered_cells: Vec<String>,
        /// `r[sched.sla.cost-instance-type-feedback]`: per-cell
        /// instance types Karpenter resolved this tick. Folded into
        /// `CostTable.cells` so `spot_price_poller` knows what to
        /// price. Edge-detected per NodeClaim (controller-side).
        observed_instance_types: Vec<rio_proto::types::ObservedInstanceType>,
        /// `r[sched.admin.hung-node-detector+3]`: kube-authoritative
        /// `intent_id → spec.nodeName` from the controller's pod
        /// informer. Replaces worker-supplied node_name (untrusted).
        /// LEGACY field-5 read arm (rolling skew, R9): consulted only
        /// when `binding_snapshot` is None.
        bound_intents: Vec<rio_proto::types::BoundIntent>,
        /// live_051(c): per-intent `NO_HOSTING_CLASS` rejection
        /// verdicts (wire field 7) — the controller's typed answer to
        /// demand it structurally cannot host. Folded into the
        /// consecutive-verdict counters at the ack-apply plane; at
        /// the typed budget the drv poisons with the verdict's
        /// `detail` as the operator-actionable message.
        rejected: Vec<rio_proto::types::IntentVerdict>,
        /// C2/285 (`r[sched.snapshot.binding-presence]`): the explicit
        /// per-tick binding snapshot. `Some(set)` — even empty —
        /// wholesale-rebuilds `authoritative_binding` (present-and-
        /// empty CLEARS: scale-to-zero); `None` = "this Ack carries no
        /// snapshot" (per-pool reconcilers, pre-upgrade controllers).
        binding_snapshot: Option<Vec<rio_proto::types::BoundIntent>>,
        /// merged_bug_005: answered AFTER the leader-gated apply with
        /// the apply outcome. `Err` leaves the controller's
        /// commit-on-Ack buffer intact for redelivery.
        reply: oneshot::Sender<Result<(), AckApplyError>>,
    },

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
    /// `(receivers, snapshot)` — the snapshot is a fully-populated
    /// `BuildEvent::Snapshot` describing the build's state at the
    /// moment of subscribe (`r[sched.watch.snapshot-first]`). The gRPC
    /// layer sends it as the stream's first message, then bridges the
    /// live broadcast. Because the actor is single-threaded and
    /// `handle_watch_build` has no await between subscribe and
    /// snapshot, the broadcast carries exactly the events emitted
    /// after the snapshot — no replay, no dedup.
    WatchBuild {
        build_id: Uuid,
        /// See [`ActorCommand::CancelBuild::caller_tenant`].
        caller_tenant: Option<Uuid>,
        reply: oneshot::Sender<
            Result<
                (
                    super::BuildEventReceivers,
                    Box<rio_proto::types::BuildEvent>,
                ),
                ActorError,
            >,
        >,
    },

    /// Internal: clean up terminal build state (maps + DAG interest) after
    /// a delay. Scheduled by complete_build/transition_build_to_failed/cancel.
    CleanupTerminalBuild { build_id: Uuid },

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
    /// degrade to empty DAG, don't block; only the success arm also
    /// marks the DAG authoritative for destructive consumers); if
    /// the TOCTOU/bump-confirmation gate discards the recovery
    /// instead, neither flag is set and the next LeaderAcquired
    /// retries.
    ///
    /// In non-K8s mode (always_leader): never sent in production —
    /// the only production sender is the lease hooks' `on_acquire`
    /// (`SchedulerLeaseHooks`, rio-scheduler/src/lease_hooks.rs), and
    /// without a configured `lease_name` (env `RIO_LEASE_NAME`) no
    /// lease loop runs. So PG recovery
    /// and the write-ahead generation claim never run there;
    /// `recovery_complete` (and the actor's `dag_authoritative`)
    /// start true, dispatch is never gated, and the DAG starts
    /// empty and is populated by live MergeDag traffic only.
    /// Adding a spawn-time send for single-scheduler startup
    /// recovery requires first teaching the bump-confirmation gate
    /// in `handle_leader_acquired` (`sched.recovery.bump-confirm`)
    /// to confirm without a live lease loop: with `always_leader`'s
    /// frozen renew rounds, a bump-demanding PG floor waits out
    /// `BUMP_CONFIRMATION_CAP` and the recovery is discarded, with
    /// no later acquire edge to retry it.
    LeaderAcquired,

    /// Lease lost (or self-fenced): invalidate any recorded recovery
    /// completion (a kept same-epoch completion must not outlive the
    /// wiped DAG), clear in-memory builds/dag/events, and zero the
    /// leader-only state gauges. Symmetric with `LeaderAcquired`.
    /// Fire-and-forget — on a real loss the lease loop has already
    /// flipped `is_leader=false` via `on_lose()`; this command brings
    /// the actor's persisted state in line so a long-lived standby
    /// doesn't (a) hold a stale DAG indefinitely, (b) export frozen
    /// gauge values.
    LeaderLost,

    /// Rebound: a holder change observed late on a still-leading round
    /// (`sched.lease.rebound`) — a compressed lose→acquire pair whose
    /// standby interval was never locally observed. The actor runs the
    /// `LeaderEdge` table's Compound lose cells (cost latch, gauge
    /// family) and then the full acquire path; deliberately NOT the
    /// lost handler's DAG wipe (the immediately-following recovery
    /// rebuilds everything the wipe would clear — see
    /// `handle_leader_rebound`). Sent by the lease hooks' `on_rebound`
    /// only; fire-and-forget like its siblings.
    LeaderRebound,

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
    /// Actor in-memory snapshot of a build's derivations + live
    /// executor stream IDs. I-025 diagnostic: surfaces the PG-vs-
    /// stream-pool mismatch that silently freezes dispatch. Unlike
    /// GetBuildGraph (PG-backed, works for completed builds), this
    /// is the exact view dispatch_ready() sees.
    InspectBuildDag {
        build_id: Uuid,
        reply: oneshot::Sender<(Vec<rio_proto::types::DerivationDiagnostic>, Vec<String>)>,
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
    /// `(intent_id → token, keyless)`; intent_ids not in the current
    /// `compute_spawn_intents` snapshot are omitted (the controller
    /// skips them this tick — bug_121) and `keyless` is the
    /// dev-mode discriminator (`hmac_signer` is None: no tokens
    /// exist anywhere; spawn token-less).
    MintExecutorTokens {
        intent_ids: Vec<String>,
        reply: oneshot::Sender<(std::collections::HashMap<String, String>, bool)>,
    },
}

/// Service lane for an [`AdminQuery`] (round-9 B8). `Fast` rides the
/// dedicated admin fast lane: served between mailbox commands with
/// priority AND at every Tick phase boundary, so delivery is bounded
/// by the largest indivisible actor work slice instead of the whole
/// mailbox FIFO (the live_053 shape: an 18s Tick starved
/// `MintExecutorTokens`, the controller's 5s admin deadline lapsed,
/// and spawn ticks were skipped while the fleet sat idle). `Bulk`
/// stays mailbox-FIFO — `Bulk` is therefore also the lane whose
/// request/reply round-trip flushes everything queued ahead of it
/// (the test-harness `barrier()` contract on `GcRoots`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminLane {
    /// Read-only AND cheap enough to serve mid-Tick: graph-independent
    /// state reads, or (for the spawn-path mint) one warm-memo
    /// forecast pass. Reordering ahead of queued commands is safe —
    /// every gRPC caller holds an independent request/reply pair and
    /// awaits its own reply before issuing a dependent call.
    Fast,
    /// Mailbox-FIFO: graph-bounded reads whose payload is bulk data,
    /// mutating admin operations, and the barrier query. Serving these
    /// mid-Tick would trade housekeeping latency for no SLO (their
    /// callers poll or are operator-driven).
    Bulk,
}

impl AdminQuery {
    /// Lane classification — exhaustive by variant (R14: a new query
    /// must pick its lane here or this stops compiling; no wildcard).
    pub(crate) fn lane(&self) -> AdminLane {
        match self {
            // The spawn-path mint: the controller calls it under
            // ADMIN_RPC_TIMEOUT (5s) right after GetSpawnIntents; a
            // mint that misses the deadline is a SKIPPED SPAWN TICK
            // (the round-8 erring-mint rider made that the failure
            // mode). Handler cost is one warm-memo forecast pass +
            // O(ids) HMAC signs — bounded by the same walk a
            // GetSpawnIntents poll already runs, priced acceptable
            // mid-Tick BY DESIGN (housekeeping yields to spawn-path
            // admin).
            Self::MintExecutorTokens { .. } => AdminLane::Fast,
            // Graph-independent estimator/ring reads (dashboard + CLI
            // surfaces): O(overrides) resolve, O(tiers) dry-run walk,
            // O(top_n) ring scan, O(classes) table read.
            Self::SlaStatus { .. }
            | Self::SlaExplain { .. }
            | Self::SlaMispredictors { .. }
            | Self::SlaHwSampled { .. } => AdminLane::Fast,
            // Graph-bounded bulk reads: full-population intent
            // compute, O(dag) root scan (ALSO the harness barrier —
            // must keep mailbox-FIFO semantics), per-build DAG dump.
            Self::GetSpawnIntents { .. } | Self::GcRoots { .. } | Self::InspectBuildDag { .. } => {
                AdminLane::Bulk
            }
            // Mutating or bulk-export admin ops: rare, operator-driven,
            // no latency SLO; mid-Tick mutation buys risk for nothing.
            Self::SlaEvict { .. } | Self::SlaImportCorpus { .. } | Self::SlaExportCorpus { .. } => {
                AdminLane::Bulk
            }
        }
    }
}

/// A [`AdminQuery::lane`]`() == Fast` query in flight on the admin
/// fast lane, stamped at enqueue so the drain can record true
/// DELIVERY latency (enqueue → handler start) — the axis the mailbox
/// FIFO starved, and the falsifiable surface for the B8 latency SLO
/// (`rio_scheduler_actor_admin_fast_delivery_seconds`).
pub(crate) struct FastAdmin {
    pub(crate) query: AdminQuery,
    pub(crate) enqueued_at: std::time::Instant,
}

impl FastAdmin {
    pub(crate) fn new(query: AdminQuery) -> Self {
        Self {
            query,
            enqueued_at: std::time::Instant::now(),
        }
    }
}

/// `cfg(test)` debug commands that bypass the state machine / dispatch
/// path so tests can set up preconditions directly.
#[cfg(test)]
pub enum DebugCmd {
    /// merged_bug_032: stamp `last_store_rpc_failure = now` — the
    /// store-health corroboration leg, exactly what a failed
    /// dispatch-time FindMissingPaths probe records in production.
    MarkStoreRpcFailure { reply: oneshot::Sender<()> },
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
    /// Backdate an in-memory witnessed-terminal mark (live_058-c).
    /// For witnessed-clock establishment tests: the mark's clock is
    /// `epoch_now()` at intake and the config slack floor is 60s —
    /// real-time waiting is not a test strategy. The production mint
    /// stays the `ReportAttemptOutcome` intake; this only AGES an
    /// existing mark (the in-memory twin of the tests'
    /// `backdate_assignment` SQL helper). Replies `false` when no
    /// mark exists.
    BackdateWitnessedMark {
        exec_id: Uuid,
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
    /// Read a derivation's in-memory attempt history (the committed
    /// ledger-suffix mirror). `None` when the node is not in the DAG.
    /// For the 1a acceptance battery (failover reload comparison).
    QueryAttemptHistory {
        drv_hash: String,
        reply: oneshot::Sender<Option<Vec<crate::state::AttemptRecord>>>,
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
    /// Arm a synthetic long Tick (round-9 B8 / W9-AG): the next
    /// `handle_tick` sleeps `each` (REAL time — the latency SLO is a
    /// wall-clock law, paused-clock-free by design) inside up to
    /// `phases` consecutive phase bodies. Models a tick whose total
    /// cost far exceeds the admin deadline while every individual
    /// phase stays decomposed — the live_053 starvation shape.
    StallTickPhases {
        phases: u32,
        each: std::time::Duration,
        reply: oneshot::Sender<()>,
    },
}

impl AdminQuery {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::GetSpawnIntents { .. } => "GetSpawnIntents",
            Self::GcRoots { .. } => "GcRoots",
            Self::InspectBuildDag { .. } => "InspectBuildDag",
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
            Self::SubstituteProgress { .. } => "SubstituteProgress",
            Self::CancelBuild { .. } => "CancelBuild",
            Self::PullAssignment { .. } => "PullAssignment",
            Self::ListMaterializationJobs { .. } => "ListMaterializationJobs",
            Self::ReportPullOutcome { .. } => "ReportPullOutcome",
            Self::ReportAttemptOutcome { .. } => "ReportAttemptOutcome",
            Self::AckSpawnedIntents { .. } => "AckSpawnedIntents",
            Self::Tick => "Tick",
            Self::QueryBuildStatus { .. } => "QueryBuildStatus",
            Self::WatchBuild { .. } => "WatchBuild",
            Self::CleanupTerminalBuild { .. } => "CleanupTerminalBuild",
            Self::Admin(q) => q.name(),
            Self::ClearPoison { .. } => "ClearPoison",
            Self::LeaderAcquired => "LeaderAcquired",
            Self::LeaderLost => "LeaderLost",
            Self::LeaderRebound => "LeaderRebound",
            #[cfg(test)]
            Self::Debug(_) => "Debug",
        }
    }
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
    /// The ComponentScaler reads this for its predictive signal
    /// (whatever Deployment a CR targets).
    pub queued_by_system: std::collections::HashMap<String, u64>,
    /// `IceBackoff::masked_cells()` snapshot, formatted via
    /// [`crate::sla::config::cell_label`]. The controller's
    /// `cover_deficit` mask merges this with its own
    /// `detect_vanished` set so a controller restart inherits the
    /// scheduler's accumulated ladder instead of rediscovering ICE
    /// per cell.
    pub ice_masked_cells: Vec<String>,
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
    /// Executors with work in flight, counted from DAG status
    /// (`Assigned|Running` — one open pull-mode attempt per such
    /// derivation, one attempt per pod). The scheduler holds no
    /// registration state for pull-mode pods, so spawned-but-not-yet-
    /// pulled pods are not visible here — the controller's Job census
    /// is that view. Equals `active_executors`.
    pub total_executors: u32,
    /// Same count as `total_executors` (no registered-vs-draining
    /// distinction exists without the stream session).
    pub active_executors: u32,
    /// Always 0: per-executor drain retired with the stream session
    /// (Job/pool-level draining is the successor). Kept until the 1d
    /// proto sweep so the response shape is unchanged.
    pub draining_executors: u32,
    /// `BuildState::Pending` — merged but not yet active.
    pub pending_builds: u32,
    /// `BuildState::Active` — at least one derivation dispatched.
    pub active_builds: u32,
    /// `DerivationStatus::Ready` across the DAG: derivations waiting
    /// for a worker. This is the autoscaling input signal, and it is
    /// computed from DAG status (NOT the legacy ready-queue membership,
    /// which a pull mint never dequeued — the recorded over-count).
    /// Always equals the sum of `queued_by_system`.
    pub queued_derivations: u32,
    /// `DerivationStatus::{Assigned|Running}` across the DAG. Workers
    /// currently occupied.
    pub running_derivations: u32,
    /// Derivations carrying a CLAIMABLE materialization job —
    /// unclaimed, not parked, not deferred: `claimability()`'s three
    /// axes (wire-stable bucket name). Claimed (in-flight) fetches are
    /// EXCLUDED — they left the bucket at claim time; parked jobs stay
    /// visible via `rio_scheduler_materialization_stalled`, deferred
    /// jobs sit in neither gauge for their bounded <=300s window.
    /// Store-side claimable load: published as the
    /// `rio_scheduler_substituting_derivations` gauge each tick
    /// (`obs.metric.scheduler-substituting` — the store ScaledObject's
    /// leading trigger) and counted in the ComponentScaler predictive
    /// signal (`ctrl.scaler.signal-substituting`) for any CR target.
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

    /// `r[sched.lease.standby-drops-writes]`: the actor dequeued a
    /// state-writing command after losing the lease. Maps to gRPC
    /// UNAVAILABLE (same string as the gRPC layer's `ensure_leader`)
    /// so BalancedChannel clients retry against the new leader.
    #[error("not leader (standby replica)")]
    NotLeader,

    // r[impl sched.evidence.durability+4]
    // r[impl sched.grpc.fence-retryable]
    /// The merge transaction's claims-floor fence rejected the write:
    /// this replica's serving generation sits below the durable floor
    /// (a successor has claimed), so committing the merge would let a
    /// deposed believer write evidence over the new tenure's. Maps to
    /// gRPC UNAVAILABLE (the `ensure_leader` "not leader" family): the
    /// fence trips exactly in the deposed-believer window — the lease
    /// loss precedes the successor's claim, so a health-aware balanced
    /// channel has usually ejected this replica already, and a client
    /// that still reaches it must RETRY (bounded), not surface a
    /// terminal error for a request that is perfectly valid on the
    /// live leader. No client retries FAILED_PRECONDITION; mapping the
    /// refusal there turned every fence trip into a user-visible
    /// failure (bug_393).
    #[error(
        "merge fenced: serving generation {serving} is below the durable claims floor {floor} \
         (a newer leader has claimed); retry against the current leader"
    )]
    StaleGeneration { serving: i64, floor: i64 },
}

// r[impl sched.grpc.fence-retryable]
/// Whether a refusal is RETRYABLE (the caller should try again — on
/// this replica after backoff or on another replica) or TERMINAL (the
/// request itself can never succeed as posed). The gRPC code derives
/// from this class: `Retryable ⟺ code ∈ {UNAVAILABLE,
/// RESOURCE_EXHAUSTED}` (pinned by `retry_class_code_consistency`), so
/// a future refusal surface cannot silently map a retryable condition
/// to a code no client retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryClass {
    /// Transient: another replica (or this one, later) will accept it.
    Retryable,
    /// The request can never succeed as posed (bad input, missing
    /// entity, authz, internal invariant failure).
    Terminal,
}

impl ActorError {
    /// The refusal class — exhaustive (a new variant must choose).
    pub(crate) fn retry_class(&self) -> RetryClass {
        match self {
            // Leadership / load / fence / dependency-outage refusals:
            // valid requests the cluster can serve elsewhere or later.
            Self::NotLeader
            | Self::ChannelSend
            | Self::Backpressure
            | Self::StoreUnavailable
            | Self::StaleGeneration { .. } => RetryClass::Retryable,
            // The request itself is unservable as posed.
            Self::BuildNotFound(_)
            | Self::Database(_)
            | Self::Dag(_)
            | Self::MissingDbId { .. }
            | Self::PermissionDenied { .. } => RetryClass::Terminal,
        }
    }
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
/// Same pattern as [`BackpressureReader`]: the writers (the lease
/// task's `on_acquire`/`on_rebound` deriving from the Lease's
/// transition count, and recovery's PG-floor seed — all `fetch_max` on
/// the inner Arc) only ever raise it; everyone else observes. The
/// worker-visible consumer is gated on the recovery condition:
/// `WorkAssignment.generation` carries [`advertised`](Self::advertised)
/// as observability on the pull payload; the durable claims-floor
/// fence inside the pull/establishment transactions is what actually
/// rejects stale authority.
///
/// `Acquire` not `Relaxed`: the generation is a fence. When the lease
/// task acquires leadership and writes the new generation, it also
/// sets `is_leader=true`. A reader seeing the new generation should
/// also see the new leader state. Relaxed would be fine in practice
/// (the atomic itself has no reordering peers here) but Acquire makes
/// the pairing with the lease task's RMW explicit.
///
/// Starts at 1 (not 0): generation=0 is the proto-default, so a worker
/// receiving `generation=0` knows the field was unset (or the leader is
/// still recovering) rather than "first generation." Non-K8s mode (no
/// lease) stays at 1 forever — correct for a single scheduler.
#[derive(Clone)]
pub struct GenerationReader {
    generation: Arc<AtomicU64>,
    leader: crate::lease::LeaderState,
}

impl GenerationReader {
    pub(super) fn new(generation: Arc<AtomicU64>, leader: crate::lease::LeaderState) -> Self {
        Self { generation, leader }
    }

    /// Current leader generation — the raw (not recovery-gated) value.
    /// For callers that genuinely need the ungated value (currently
    /// tests only); the recovery-gated view is
    /// [`advertised`](Self::advertised) instead.
    pub fn get(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The worker-visible generation: the raw value once the leader's
    /// recovery has completed, `0` (the proto-unset sentinel — a no-op
    /// for the executor's `fetch_max` fence latch) before that.
    ///
    /// Ordering: the recovery seed's `fetch_max` is sequenced before
    /// the SeqCst `set_recovery_complete()` call on the actor task, so
    /// a reader that observes `recovery_complete()` true here also
    /// observes the seeded generation. The loads are NOT one atomic
    /// snapshot — a read composed exactly across a lose→re-acquire
    /// edge, or across a rebound (`LeaderState::on_rebound` clears the
    /// completion stamp first, then raises the generation), can pair
    /// them inconsistently for one read; that exposure is no worse
    /// than the pre-gating default for every read. A
    /// TOCTOU-discarded recovery never stamps a completion — and a
    /// completion racing a rebound or a re-acquire at a different count
    /// is stamped with an epoch the recorded count no longer matches —
    /// so those re-runs keep advertising 0 until they complete.
    /// A completion racing a bare lose is the one shape the stamp does
    /// not catch (the count never moved): the deposed replica keeps
    /// advertising that generation until the actor processes the
    /// `LeaderLost` queued by that lose (`handle_leader_lost`
    /// invalidates the orphaned completion — typically the next command
    /// drained) or, at the latest, until its own next acquire edge.
    /// Dispatch is still gated by `is_leader`, and the worker latch is
    /// a `fetch_max` floor either way; the ledger bound, though, only
    /// holds on the claimed path (a successor seeds above it via the PG
    /// floor, GREATEST(MAX(assignments), MAX(claims))). On a term that
    /// proceeded unclaimed — claim-INSERT failure or conflict
    /// exhaustion — that leg does not hold and the exposure is the
    /// pre-existing claim-failure residual priced in
    /// `await_post_claim_leadership_confirmation`'s doc
    /// (`sched.recovery.bump-confirm`) in recovery.rs. (A DAG-load
    /// failure no longer lands here: the floor is read independently
    /// of the load, so that term floors, claims, and confirms like any
    /// other; only the floor-unreadable fallback remains, which also
    /// completes unclaimed (no claim is possible without the floor
    /// read), so it carries the same pre-existing claim-failure
    /// residual and, additionally, under-floor advertisement in the
    /// saturated regime — the latched builders silently reject its
    /// work.) The exits are
    /// the queued-loss invalidation above, a re-acquire at a different
    /// count (the stamp mismatches — the different-count case above),
    /// or at the same count (the deliberate same-epoch keep); a rebound
    /// only becomes possible again after re-acquiring.
    pub fn advertised(&self) -> u64 {
        if self.leader.recovery_complete() {
            self.generation.load(Ordering::Acquire)
        } else {
            0
        }
    }
}
