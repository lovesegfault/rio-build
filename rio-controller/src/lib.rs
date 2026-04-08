//! Kubernetes operator for rio-build.
//!
//! Watches `BuilderPool`/`FetcherPool` CRDs and spawns one-shot
//! worker Jobs to match the scheduler's queue depth.
//!
//! # Architecture
//!
//! ```text
//!   kube-apiserver
//!        │
//!        │ watch: BuilderPool, FetcherPool, Job
//!        ▼
//! ┌──────────────────────────────────────┐
//! │ rio-controller                        │
//! │                                       │
//! │  ┌─────────────────────────────────┐  │
//! │  │ BuilderPool / FetcherPool        │  │
//! │  │  - poll ClusterStatus / manifest │  │
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
//!        │ gRPC: AdminService (ClusterStatus, GetCapacityManifest)
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

// CRD types live in rio-crds (extracted so rio-cli can use them
// without pulling the full reconciler stack). Re-exported as `crds`
// so existing `crate::crds::builderpool::...` paths still work.
pub use rio_crds as crds;
pub mod error;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod reconcilers;
pub mod scaling;

pub use crds::builderpool::{BuilderPool, BuilderPoolSpec, BuilderPoolStatus};

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
        "Reconcile loop latency. reconciler=builderpool|builderpoolset|fetcherpool|componentscaler. \
         Recorded on both success and error paths — long durations + errors \
         = slow/timing-out apiserver."
    );
    describe_counter!(
        "rio_controller_reconcile_errors_total",
        "Reconcile errors. reconciler=builderpool|builderpoolset, error_kind=kube|finalizer|invalid_spec|conflict. \
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
    describe_counter!(
        "rio_controller_manifest_spawn_failures_total",
        "Manifest Job spawn failures (labeled by pool). Non-zero rate with zero \
         reconcile_errors_total = warn+continue absorbing errors below threshold; \
         sustained high rate = threshold bailing every tick (check admission webhooks/RBAC)."
    );
}
