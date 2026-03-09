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

use std::sync::Arc;

use dashmap::DashMap;
use kube::Client;

/// Shared context for all reconcilers. Cloned into each
/// `Controller::run()` via Arc.
///
/// `scheduler_addr` not a live client: reconcilers connect lazily
/// (per-reconcile) so a transient scheduler outage doesn't kill
/// the controller at startup. The autoscaler holds a live client
/// — it polls every 30s, the connection cost amortizes.
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
    /// Recorder for K8s Events. Reconcilers call `ctx.publish_
    /// event(obj, ev)` to emit; events show in `kubectl describe`
    /// and `kubectl get events`. Operator visibility for "what
    /// did the controller just do" without scraping logs.
    ///
    /// kube 3.0 API: Recorder is constructed ONCE with (client,
    /// reporter); `publish` takes the object_ref per-call. So we
    /// hold one Recorder and pass the ref at publish time.
    pub recorder: kube::runtime::events::Recorder,
    // r[impl ctrl.build.watch-by-uid]
    /// Tracks in-flight Build watch tasks, keyed by Build **uid**
    /// (NOT {ns}/{name}). uid is unique per K8s object lifetime:
    /// delete+recreate with the same name gets a FRESH uid. Keying
    /// by name would let an old drain_stream's scopeguard remove
    /// the NEW Build's entry after a delete+recreate → next
    /// reconcile spawns a duplicate watch (regressing the C1 fix
    /// we built this map to prevent).
    ///
    /// Dedupes spawns across reconciles: drain_stream patches
    /// Build.status on each BuildEvent → K8s API server emits a
    /// watch event → controller re-enqueues → apply() runs again.
    /// Without this gate, each reconcile spawns ANOTHER
    /// drain_stream — linear growth (N transitions = N tasks).
    ///
    /// drain_stream holds a scopeguard that removes the entry on
    /// exit (success, error, panic, cancel — any path). apply()
    /// checks contains_key() before spawning; if the watch is
    /// already running, returns await_change() without spawning.
    pub watching: Arc<DashMap<String, ()>>,
}

impl Ctx {
    /// Publish an event scoped to a K8s object. Best-effort —
    /// event-publish failure is logged, not propagated (events
    /// are observability, not correctness; a reconcile that
    /// succeeds but couldn't emit an event still succeeded).
    pub async fn publish_event<K>(&self, obj: &K, ev: &kube::runtime::events::Event)
    where
        K: kube::Resource<DynamicType = ()>,
    {
        if let Err(e) = self.recorder.publish(ev, &obj.object_ref(&())).await {
            tracing::warn!(error = %e, "failed to publish K8s event");
        }
    }
}
