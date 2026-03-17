//! Kubernetes operator for rio-build.
//!
//! Watches `WorkerPool` and `Build` CRDs, reconciles worker
//! StatefulSets, autoscales based on `AdminService.ClusterStatus`
//! queue depth.
//!
//! # Architecture
//!
//! ```text
//!   kube-apiserver
//!        │
//!        │ watch: WorkerPool, Build, StatefulSet
//!        ▼
//! ┌──────────────────────────────────────┐
//! │ rio-controller                        │
//! │                                       │
//! │  ┌─────────────────────────────────┐  │
//! │  │ WorkerPool reconciler           │  │
//! │  │  - ensure StatefulSet exists    │  │
//! │  │  - sync spec (resources, caps)  │  │
//! │  │  - patch status.replicas        │  │
//! │  │  - finalizer: drain on delete   │  │
//! │  └─────────────────────────────────┘  │
//! │                                       │
//! │  ┌─────────────────────────────────┐  │
//! │  │ Build reconciler                │  │
//! │  │  - SubmitBuild to scheduler     │  │
//! │  │  - watch stream → patch status  │  │
//! │  │  - finalizer: CancelBuild       │  │
//! │  └─────────────────────────────────┘  │
//! │                                       │
//! │  ┌─────────────────────────────────┐  │
//! │  │ Autoscaler loop (30s)           │  │
//! │  │  - ClusterStatus.queued_drvs    │  │
//! │  │  - patch StatefulSet.replicas   │  │
//! │  │  - 30s up / 10m down windows    │  │
//! │  └─────────────────────────────────┘  │
//! └──────────────────────────────────────┘
//!        │
//!        │ gRPC: AdminService (ClusterStatus, DrainWorker)
//!        │       SchedulerService (SubmitBuild, CancelBuild)
//!        ▼
//!   rio-scheduler
//! ```
//!
//! # What the controller does NOT manage
//!
//! Scheduler/store/gateway Deployments are NOT managed by CRD —
//! they're deployed via kustomize as standard Deployments. The
//! controller only manages worker StatefulSets (complex lifecycle:
//! drain before scale-down, terminationGracePeriodSeconds=7200) and
//! Build CRDs (K8s-native build submission alternative to SSH).

pub mod crds;
pub mod error;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod reconcilers;
pub mod scaling;

pub use crds::build::{Build, BuildSpec, BuildStatus};
pub use crds::workerpool::{WorkerPool, WorkerPoolSpec, WorkerPoolStatus};

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
        "Reconcile loop latency. reconciler=build|workerpool. \
         Recorded on both success and error paths — long durations + errors \
         = slow/timing-out apiserver."
    );
    describe_counter!(
        "rio_controller_reconcile_errors_total",
        "Reconcile errors. reconciler=build|workerpool, error_kind=kube|finalizer|invalid_spec|scheduler_unavailable. \
         error_kind is the variant discriminator (stable, low cardinality). \
         Sustained rate > 0 = check controller logs."
    );
    describe_counter!(
        "rio_controller_scaling_decisions_total",
        "Autoscale patches executed. direction=up|down. \
         High rate = queue depth oscillating (check stabilization windows)."
    );
    describe_gauge!(
        "rio_controller_workerpool_replicas",
        "WorkerPool replica counts. kind=actual|desired, pool=namespace/name. \
         Gap between actual and desired = StatefulSet rollout lag or stabilization window."
    );
    describe_counter!(
        "rio_controller_build_watch_spawns_total",
        "drain_stream tasks spawned (initial + reconnect). \
         Should be ~1 per Build lifetime; high rate = reconnect churn (scheduler instability)."
    );
    describe_counter!(
        "rio_controller_build_watch_reconnects_total",
        "BuildEvent stream reconnect attempts after stream drop. Distinct from \
         spawns_total: spawns counts initial watch + every reconnect-that-reached-\
         WatchBuild; reconnects_total counts every attempt including those that \
         fail before reaching the RPC. High rate = scheduler instability or \
         network partition; alert on rate > 0 sustained over 5m."
    );
}
