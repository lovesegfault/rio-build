//! Reconciliation loops.
//!
//! Each reconciler is a `fn(Arc<CR>, Arc<Ctx>) -> Result<Action>`.
//! `Controller::new().owns().run()` calls it whenever the CR or an
//! owned child changes. The reconcile fn makes the world match the
//! spec: ensure StatefulSet exists with the right shape, patch
//! status to reflect observed state.
//!
//! Idempotent by construction: every reconcile starts from
//! scratch, reads current state, computes desired, applies the
//! diff via server-side apply. Reconciling twice is a no-op.

pub mod build;
pub mod workerpool;

use kube::Client;

/// Shared context for all reconcilers. Cloned into each
/// `Controller::run()` via Arc.
///
/// `scheduler_addr` not a live client: reconcilers connect lazily
/// (per-reconcile) so a transient scheduler outage doesn't kill
/// the controller at startup. The autoscaler (F4) holds a live
/// client — it polls every 30s, the connection cost amortizes.
pub struct Ctx {
    /// K8s client. Shared (clone per `Api<T>` call — cheap, it's
    /// an Arc internally).
    pub client: Client,
    /// rio-scheduler gRPC address (e.g., "rio-scheduler:9001").
    /// AdminService (ClusterStatus, DrainWorker) and
    /// SchedulerService (SubmitBuild, CancelBuild) are on the
    /// same port.
    pub scheduler_addr: String,
    /// rio-store gRPC address (e.g., "rio-store:9002"). The Build
    /// reconciler fetches .drv content from here to construct the
    /// DerivationNode. Same lazy-connect rationale as scheduler.
    pub store_addr: String,
}
