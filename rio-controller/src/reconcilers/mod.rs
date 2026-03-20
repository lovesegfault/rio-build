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

pub mod gc_schedule;
pub mod workerpool;
pub mod workerpoolset;

use kube::Client;

/// Shared context for all reconcilers. Cloned into each
/// `Controller::run()` via Arc.
///
/// `admin` is a live client, not an address. When
/// `scheduler_balance_host` is set (production), it uses a
/// health-aware balanced Channel that routes only to the leader ---
/// no ClusterIP round-robin lottery (which fails ~50% with standby's
/// `UNAVAILABLE: "not leader"`). The startup retry loop in main()
/// handles the "scheduler not up yet" case; reconcilers just clone.
/// `scheduler_addr` kept for worker pod env injection (workers need
/// the address string, not a client).
pub struct Ctx {
    /// K8s client. Shared (clone per `Api<T>` call --- cheap, it's
    /// an Arc internally).
    pub client: Client,
    /// Balanced AdminServiceClient (DrainWorker in workerpool
    /// finalizer).
    pub admin: rio_proto::AdminServiceClient<tonic::transport::Channel>,
    /// rio-scheduler ClusterIP address (e.g., "rio-scheduler:9001").
    /// For worker pod env injection ONLY --- reconcilers use
    /// `admin` above.
    pub scheduler_addr: String,
    /// Headless Service host. Injected as `RIO_SCHEDULER_BALANCE_HOST`
    /// into worker pods. `None` = env var NOT injected --> workers
    /// fall back to single-channel (VM tests, single-replica).
    pub scheduler_balance_host: Option<String>,
    pub scheduler_balance_port: u16,
    /// rio-store gRPC address (e.g., "rio-store:9002"). Injected
    /// as `RIO_STORE_ADDR` into worker pod containers (workers
    /// connect to the store directly for PutPath/GetPath).
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
