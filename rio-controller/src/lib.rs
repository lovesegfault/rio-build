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
pub(crate) mod scaling;

/// Histogram bucket boundaries for controller reconcile latency (seconds).
///
/// Reconciles are mostly K8s API round-trips — expect 10–500ms normally,
/// seconds only under API-server stress. Default Prometheus buckets
/// actually work here but the low end (5ms) is wasted; this set trades
/// that for a 10s top bucket.
const RECONCILE_DURATION_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// Per-crate histogram bucket overrides, passed to
/// `rio_common::server::bootstrap` → `init_metrics`. Every
/// `describe_histogram!` in this crate must have an entry here OR be in
/// the `DEFAULT_BUCKETS_OK` exemption list (`tests/metrics_registered.rs`);
/// histograms not listed fall through to the global `[0.005..10.0]` default.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[(
    "rio_controller_reconcile_duration_seconds",
    RECONCILE_DURATION_BUCKETS,
)];

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
        "DisruptionTarget watcher DrainExecutor calls. result=sent|rpc_error. \
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
        "ComponentScaler observed load: max(GetLoad.pg_pool_utilization) across \
         loadEndpoint pods at the last tick (labelled by cs=ns/name)."
    );
    describe_counter!(
        "rio_controller_ephemeral_jobs_reaped_total",
        "Excess Pending ephemeral Jobs deleted (labeled by pool, class). \
         Non-zero rate = queued dropped after spawn (user cancel, gateway disconnect); \
         zero rate with stuck Pending pods = reap not firing (check RBAC delete on batch/jobs)."
    );
    describe_counter!(
        "rio_controller_orphan_jobs_reaped_total",
        "Running ephemeral Jobs deleted after orphan grace with no scheduler assignment \
         (labeled by pool, class). Non-zero rate = builders stuck unable to self-exit \
         (D-state FUSE wait, OOM-loop); investigate node/kernel health."
    );
    describe_gauge!(
        "rio_controller_nodepool_budget_used_millicores",
        "Σ NodePool.status.resources.cpu across budget-governed pools. \
         Approaching budget = expect freeze; well over = check why a pool overshot."
    );
    describe_gauge!(
        "rio_controller_nodepool_budget_headroom_millicores",
        "Shared headroom (budget − used). 0 = frozen (no new node provisioning on governed pools)."
    );
}
