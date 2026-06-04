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

// .sqlx/ surfaced as a tracked env-dep (see build.rs): the env! read
// records a dep-info `# env-dep:` line, so cargo and content-keyed
// rustc-wrapper caches (kache) re-key this crate when query metadata
// changes without .rs edits.
const _: &str = env!("RIO_SQLX_HASH");

pub mod config;
pub(crate) mod error;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod reconcilers;

/// Re-export of the shared embedded migrator from `rio-migrations` for
/// `nodeclaim_pool::sketch` PG tests. Same migration set as
/// rio-store/rio-scheduler — controller doesn't run this in `main()`
/// (store/scheduler own startup migration), only the
/// `TestDb::new(&MIGRATOR)` fixtures do.
#[cfg(test)]
pub use rio_migrations::MIGRATOR;

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

/// Registers prometheus metric descriptions. The help strings here are
/// the source for `docs/ref/metrics.typ` — see
/// `xtask/src/regen/docs_data.rs::metrics()` for the data-flow.
//
// Hoisted from main.rs so the `tests/metrics_registered.rs` integration
// test can call it — consistency with the other four components.
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
        "nodeclaim_pool intents dropped by `reason`. \
         reason=no_pool_covers: no configured Builder or Fetcher Pool \
         covers the intent's (kind, system, effective_features) — the \
         placer would never spawn a Job for it, so provisioning would \
         mint a permanently-idle NodeClaim; add a Pool or remove the \
         hwClass advertising the feature. \
         reason=no_hosting_class: no configured hw-class can host the \
         intent EVEN WITH no ICE-masking — wrong arch, footprint exceeds \
         every arch-matching class's max_cores/max_mem, or \
         required_features unmatched (the hwClasses key-set lacks a \
         `provides_features` entry for it). Persistent until \
         `[sla.hw_classes]` changes. \
         reason=all_cells_ice_masked: a class CAN host the intent but \
         every hosting cell is ICE-masked — NodeClaim launches are \
         failing in the cloud (capacity, quota, IAM). Self-heals once \
         the ICE backoff expires *if* the cloud recovers; persistent if \
         structural (e.g. missing AWSServiceRoleForEC2Spot). Check \
         `nodeclaim_reaped_total{reason=~\"ice|vanished\"}` and Karpenter. \
         reason=exceeds_cell_cap: intent's pod footprint exceeds the assigned \
         cell's per-class catalog ceiling (or max_node_disk) — \
         the scheduler's ClassCeiling gate didn't reject it (override-bypass \
         producer hole). The intent has no valid claim of any n; sizing drops \
         it instead of looping mint→Pending. \
         reason=unknown_hw_class: scheduler stamped a hwClass not yet in \
         the controller's GetHwClassConfig — config skew; self-heals within \
         ≤300s, persistent rate = controller's hw_refresh RPC failing."
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
         state=registered: Registered=True AND not terminating (FFD-placeable); \
         state=inflight: created but not yet Registered; state=terminating: \
         metadata.deletionTimestamp set (Karpenter finalizer draining, ~60-90s; \
         excluded from FFD placement, still in max_fleet_cores budget). \
         Σ(registered) ≈ warm capacity; inflight stuck high = check \
         reaped_total{reason=ice|boot-timeout}; terminating>0 with \
         registered=0 + ffd_unplaced_cores>0 = node draining out from under \
         a queue (cover_deficit mints the replacement next tick)."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_inflight_age_max_seconds",
        "Oldest in-flight NodeClaim per `cell` (now − metadata.creationTimestamp; \
         0 when none in-flight). The per-claim age the StuckPending alert keys on \
         — the inflight count never touches 0 under sustained scale-up, so \
         count-based `for: 90s` fires on healthy bursts."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_terminating_age_max_seconds",
        "Oldest terminating NodeClaim per `cell` (now − metadata.deletionTimestamp; \
         0 when none terminating). The per-claim age the StuckTerminating alert keys \
         on — the terminating count never touches 0 under sustained scale-down churn, \
         so a count-based `for:` fires on every healthy reap burst (r38 merged_001 — \
         lifecycle inverse of the inflight scale-up case)."
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
         lead-time sketch. What cover_deficit provisions ahead by. Stuck at the seed value = \
         no Registered=True transitions recorded yet (check seed_fallback_total)."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_forecast_hit_ewma",
        "Per-`cell` closed-loop SLI: EWMA(α=0.2) of the fraction of intents satisfiable \
         from already-Registered nodeclaims at dispatch time. Drives the Schmitt deadband \
         on lead_time_q (widen below 0.85·target, narrow above 1.05·target). \
         Restart-resets to 0.9 (target mid-zone)."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_lead_time_q_at_cap",
        "Per-`cell` boolean (0/1): the Schmitt loop can no longer widen lead_time_q — \
         either q≥0.99 (clamp) or lead_time≥sla.maxLeadTime (widen-gate closed). \
         Sustained 1 with low forecast_hit_ewma = structurally cannot cover \
         (fat-tailed eta_error). Derivable from lead_time_seconds vs config; exported \
         so the alert anchors on the actual gate predicate."
    );
    describe_gauge!(
        "rio_controller_nodeclaim_ice_timeout_seconds",
        "Per-`cell` boot/ICE reap threshold the reaper acts at: \
         max(2×lead_time_seed, q_0.99(boot)), floored at 2×seed until 100 \
         boot samples. The StuckPending alert anchors on this (×2) so the \
         alert always sits above the reaper — a firing alert means the \
         reaper failed, not just a slow boot. Distinct from \
         `lead_time_seconds` (q_0.9(boot−eta), what cover_deficit \
         provisions ahead by — learns DOWN; not a reap floor)."
    );
    // r[impl obs.metric.consolidate-threshold]
    describe_gauge!(
        "rio_controller_nodeclaim_consolidate_threshold_seconds",
        "Per-`cell` idle-NodeClaim reap threshold from the NA consolidation model \
         (last node evaluated this tick; 0 when no idle nodes in the cell). \
         max(boot_median/2, min_consolidation_time[cell]) floored. NA-extends \
         past the floor ONLY for cells packing ~1 intent/node \
         (E[c_fit] > cores/2); for bin-packed cells (the §13b MostAllocated \
         builder default) the floor is a hard bound the model cannot exceed. \
         Watch fetcher-* >= 600s and builder cells >= the 300s `*` floor to \
         confirm the per-cell e_fitting_cores partition and the policy floor \
         are both routing (r35 bug_023/bug_050; r38 bug_022)."
    );
    describe_counter!(
        "rio_controller_ddsketch_seed_fallback_total",
        "Per-`cell` seed injections at CellSketches::seed(). Incremented once per \
         cold-start cell whose z_active AND z_shadow sketches were both empty \
         after PG load. >1 over controller lifetime = sketch persist failing \
         (check tick errors)."
    );
    describe_gauge!(
        "rio_controller_sketches_reload_pending",
        "1 while the lease-acquire CellSketches reload from PG is pending (load() not \
         yet succeeded since on_acquire); 0 once latched. While 1, persist() is gated \
         off so a stale standby-startup snapshot doesn't overwrite the previous leader's \
         PG rows. Stuck at 1 = PG unreachable from controller; reconcile runs degraded."
    );
}
