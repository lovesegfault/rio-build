//! Kubernetes operator for rio-build.
//!
//! Watches `Pool` CRDs and spawns one-shot worker Jobs sized to
//! the scheduler's per-derivation `SpawnIntent`s.
//!
//! # Architecture
//!
//! ```text
//!   kube-apiserver
//!        │
//!        │ watch: Pool, Job
//!        ▼
//! ┌──────────────────────────────────────┐
//! │ rio-controller                        │
//! │                                       │
//! │  ┌─────────────────────────────────┐  │
//! │  │ Pool reconciler                  │  │
//! │  │  - poll GetSpawnIntents          │  │
//! │  │  - spawn Jobs to match queue     │  │
//! │  │  - reap completed/orphan Jobs    │  │
//! │  │  - patch status.readyReplicas    │  │
//! │  └─────────────────────────────────┘  │
//! │                                       │
//! │  ┌─────────────────────────────────┐  │
//! │  │ ComponentScaler (30s)            │  │
//! │  │  - gateway/scheduler Deployment  │  │
//! │  │    replica scaling on load       │  │
//! │  └─────────────────────────────────┘  │
//! └──────────────────────────────────────┘
//!        │
//!        │ gRPC: AdminService (ClusterStatus, GetSpawnIntents)
//!        ▼
//!   rio-scheduler
//! ```
//!
//! # What the controller does NOT manage
//!
//! Scheduler/store/gateway Deployments are NOT managed by CRD —
//! they're deployed via helm as standard Deployments (the
//! ComponentScaler only patches their replica counts). Build
//! submission is via SSH (`nix build --store ssh-ng://`) — no
//! K8s-native submission CRD.

pub(crate) mod error;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod reconcilers;

/// Embedded migrator for `nodeclaim_pool::sketch` PG tests. Same
/// `migrations/` dir as rio-store/rio-scheduler — controller doesn't run
/// this in `main()` (store/scheduler own startup migration), only the
/// `TestDb::new(&MIGRATOR)` fixtures do.
#[cfg(test)]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Histogram bucket boundaries for controller reconcile latency (seconds).
///
/// Reconciles are mostly K8s API round-trips — expect 10–500ms normally,
/// seconds only under API-server stress. Default Prometheus buckets
/// actually work here but the low end (5ms) is wasted; this set trades
/// that for a 10s top bucket.
const RECONCILE_DURATION_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// Histogram bucket boundaries for nodeclaim_pool tick duration (seconds).
///
/// One tick = list NodeClaims + GetSpawnIntents RPC + FFD-sim + create/
/// delete + PG persist. Dominated by apiserver round-trips (×2-10) and
/// the admin-RPC bound; FFD/anchor-bulk are µs. Low end at 50ms (one
/// list + one persist), top at 30s (well past the 5s `ADMIN_RPC_TIMEOUT`
/// + apiserver tail under load) so the ⊥-tick latency floor is visible.
const NODECLAIM_TICK_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

/// Per-crate histogram bucket overrides, passed to
/// `rio_common::server::bootstrap` → `init_metrics`. Every
/// `describe_histogram!` in this crate must have an entry here OR be in
/// the `DEFAULT_BUCKETS_OK` exemption list (`tests/metrics_registered.rs`);
/// histograms not listed fall through to the global `[0.005..10.0]` default.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    (
        "rio_controller_reconcile_duration_seconds",
        RECONCILE_DURATION_BUCKETS,
    ),
    (
        "rio_controller_nodeclaim_tick_duration_seconds",
        NODECLAIM_TICK_BUCKETS,
    ),
];

/// Register `# HELP` descriptions for all controller metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Descriptions
/// sourced from docs/src/observability.md (the Controller Metrics table).
/// See rio_gateway::describe_metrics for rationale.
///
/// Hoisted from main.rs so the `tests/metrics_registered.rs` integration
/// test can call it — consistency with the other four components.
// r[impl obs.metric.controller]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_histogram!(
        "rio_controller_reconcile_duration_seconds",
        "Reconcile loop latency. reconciler=pool|componentscaler. \
         Recorded on both success and error paths — long durations + errors \
         = slow/timing-out apiserver."
    );
    describe_counter!(
        "rio_controller_reconcile_errors_total",
        "Reconcile errors. reconciler=pool|componentscaler, \
         error_kind=kube|finalizer|invalid_spec. \
         error_kind is the variant discriminator (stable, low cardinality). \
         Sustained rate > 0 = check controller logs."
    );
    describe_counter!(
        "rio_controller_scaling_decisions_total",
        "Autoscale patches executed. direction=up|down. \
         High rate = queue depth oscillating (check stabilization windows)."
    );
    describe_counter!(
        "rio_controller_gc_runs_total",
        "GC cron runs. result=success|connect_failure|rpc_failure. \
         connect_failure=store unreachable; rpc_failure=TriggerGC returned error or stream aborted."
    );
    describe_counter!(
        "rio_controller_disruption_drains_total",
        "DisruptionTarget watcher DrainExecutor calls. result=sent|timeout|rpc_error. \
         Zero rate with evictions happening = watcher dead, falling back to 2h SIGTERM self-drain."
    );
    describe_gauge!(
        "rio_controller_component_scaler_learned_ratio",
        "ComponentScaler learned builders-per-replica ratio (labelled by cs=ns/name). \
         EMA-adjusted against observed PG-pool load; persisted in .status.learnedRatio."
    );
    describe_gauge!(
        "rio_controller_component_scaler_desired_replicas",
        "ComponentScaler desired replica count (labelled by cs=ns/name). \
         What was last patched onto deployments/scale."
    );
    describe_gauge!(
        "rio_controller_component_scaler_observed_load",
        "ComponentScaler observed load: max of pg-pool utilization and substitute-admission utilization \
         across loadEndpoint pods at the last tick (labelled by cs=ns/name)."
    );
    describe_counter!(
        "rio_controller_ephemeral_jobs_reaped_total",
        "Excess Pending ephemeral Jobs deleted (labeled by pool). \
         Non-zero rate = queued dropped after spawn (user cancel, gateway disconnect); \
         zero rate with stuck Pending pods = reap not firing (check RBAC delete on batch/jobs)."
    );
    describe_counter!(
        "rio_controller_orphan_jobs_reaped_total",
        "Running ephemeral Jobs deleted after orphan grace with no scheduler assignment \
         (labeled by pool). Non-zero rate = builders stuck unable to self-exit \
         (D-state FUSE wait, OOM-loop); investigate node/kernel health."
    );
    describe_counter!(
        "rio_controller_lease_acquired_total",
        "nodeclaim_pool lease acquire transitions. >1 over a short window = leadership churn \
         (check apiserver health / pod restarts)."
    );
    describe_counter!(
        "rio_controller_lease_lost_total",
        "nodeclaim_pool lease lose transitions (explicit lose or local self-fence)."
    );
    describe_counter!(
        "rio_controller_nodeclaim_reaped_total",
        "nodeclaim_pool NodeClaim deletions by `reason` × `cell`. \
         reason=idle: NA-consolidate break-even; reason=ice: \
         Launched=False (timeout or terminal LaunchFailed reason); \
         reason=boot-timeout: Launched=True ∧ Registered=False past \
         timeout; reason=dead: scheduler-reported hung node; \
         reason=vanished: in-flight claim Karpenter-GC'd between ticks."
    );
    describe_counter!(
        "rio_controller_nodeclaim_intent_dropped_total",
        "nodeclaim_pool cover_deficit intents dropped by `reason`. \
         reason=no_menu_for_arch: hw-agnostic intent.system maps to an arch \
         with no configured hw-class (and referenceHwClass mismatches) — the \
         cold-start fallback could not target ANY cell. Non-zero on a \
         dual-arch cluster = a hwClasses key-set is missing one arch. \
         reason=exceeds_cell_cap: intent's pod footprint exceeds the assigned \
         cell's per-class HwClassDef.max_cores/max_mem (or max_node_disk) — \
         the scheduler's ClassCeiling gate didn't reject it (override-bypass \
         producer hole). The intent has no valid claim of any n; sizing drops \
         it instead of looping mint→Pending."
    );
    describe_counter!(
        "rio_controller_nodeclaim_created_total",
        "nodeclaim_pool NodeClaim Api::create successes by `cell`. \
         Σrate(created) − Σrate(reaped) over a window ≈ fleet growth; \
         sustained created with zero placeable_intents = \
         FFD/kube-scheduler-packed mismatch."
    );
    describe_histogram!(
        "rio_controller_nodeclaim_tick_duration_seconds",
        "nodeclaim_pool reconcile_once latency. Recorded on success AND error \
         (⊥-tick, apiserver 5xx). p99 approaching ADMIN_RPC_TIMEOUT (5s) = scheduler \
         stalled; approaching tick interval = reconciler can't keep up."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_live",
        "Owned NodeClaims at the last tick by `cell` × `state`. \
         state=registered: Registered=True (FFD-placeable); state=inflight: \
         created but not yet Registered. Σ(registered) ≈ warm capacity; \
         inflight stuck high = check reaped_total{reason=ice|boot-timeout}."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_inflight_age_max_seconds",
        "Oldest in-flight NodeClaim per `cell` (now − metadata.creationTimestamp; \
         0 when none in-flight). The per-claim age the StuckPending alert keys on \
         — the inflight count never touches 0 under sustained scale-up, so \
         count-based `for: 90s` fires on healthy bursts."
    );
    describe_gauge!(
        "rio_controller_ffd_unplaced_cores",
        "Σ SpawnIntent.cores per `cell` left unplaced by the FFD simulation \
         at the last tick. cover_deficit's per-cell input. Non-zero with \
         created_total flat = max_fleet_cores or per-tick cap throttling."
    );
    describe_gauge!(
        "rio_controller_ffd_placeable_intents",
        "SpawnIntents FFD-placed at the last tick by `state`. state=registered: \
         on a Registered=True NodeClaim (Jobs created this tick); state=inflight: \
         on a not-yet-Registered claim (held by placeable-gate). Ratio \
         registered/(registered+inflight) is the forecast warm-hit proxy."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_lead_time_seconds",
        "Per-`cell` provisioning lead-time: lead_time_q-quantile of the z=boot−eta_error \
         DDSketch. What cover_deficit provisions ahead by. Stuck at the seed value = \
         no Registered=True transitions recorded yet (check seed_fallback_total)."
    );
    describe_counter!(
        "rio_controller_ddsketch_seed_fallback_total",
        "Per-`cell` seed injections at CellSketches::seed(). Incremented once per \
         cold-start cell whose z_active sketch was empty after PG load. >1 over \
         controller lifetime = sketch persist failing (check tick errors)."
    );
    describe_gauge!(
        "rio_controller_sketches_reload_pending",
        "1 while the lease-acquire CellSketches reload from PG is pending (load() not \
         yet succeeded since on_acquire); 0 once latched. While 1, persist() is gated \
         off so a stale standby-startup snapshot doesn't overwrite the previous leader's \
         PG rows. Stuck at 1 = PG unreachable from controller; reconcile runs degraded."
    );
}
