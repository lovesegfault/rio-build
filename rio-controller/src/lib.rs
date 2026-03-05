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
pub mod reconcilers;

pub use crds::build::{Build, BuildSpec, BuildStatus};
pub use crds::workerpool::{WorkerPool, WorkerPoolSpec, WorkerPoolStatus};
