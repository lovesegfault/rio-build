//! WorkerPoolSet reconciler: one child WorkerPool per size class.
//!
// r[impl ctrl.wps.reconcile]
//!
//! Reconcile flow:
//!   1. For each `spec.classes[i]`, build a child WorkerPool named
//!      `{wps}-{class.name}` with `ownerReferences → WPS`.
//!   2. SSA-apply it (field manager `rio-controller-wps`, force).
//!   3. Finalizer-wrapped cleanup explicitly deletes children.
//!
//! Status refresh (per-class `effective_cutoff_secs` + `queued` →
//! WPS status) is OMITTED here — it needs `GetSizeClassStatus` which
//! lands in P0231/P0234. Between this plan's merge and P0234's,
//! `tracey query untested` shows `ctrl.wps.reconcile`. Expected,
//! transient — the VM lifecycle test (P0239) is the proper
//! `r[verify]` site.
//!
//! # Child lifecycle
//!
//! The child WorkerPool is SSA-applied every reconcile. Same
//! idempotency as the WorkerPool→StatefulSet path: same patch
//! twice is a no-op. The apiserver merges field ownership.
//!
//! Children carry `ownerReferences` with `controller=true` → K8s
//! GC deletes them when the WPS is deleted. The finalizer-wrapped
//! `cleanup()` ALSO explicitly deletes — belt-and-suspenders:
//! ownerRef GC is eventual (etcd compaction → GC queue → actual
//! delete can lag seconds to minutes under load). VM tests that
//! assert "WPS delete → children gone within 5s" need the explicit
//! delete's deterministic timing.
//!
//! # What this does NOT do
//!
//! - Prune stale children: if an operator edits `spec.classes` to
//!   remove a class, the now-orphaned child WorkerPool is NOT
//!   deleted. P0234's status refresh can detect the mismatch
//!   (ClassStatus entry with no matching spec.classes entry); a
//!   followup plan can add the prune. For now, operators `kubectl
//!   delete wp {wps}-{removed-class}` manually.
//! - Per-class autoscaling: `r[ctrl.wps.autoscale]` (P0234).
//! - Status aggregation: `r[ctrl.wps.cutoff-status]` (P0234).

use std::sync::Arc;
use std::time::Duration;

use kube::ResourceExt;
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use tracing::{debug, info, warn};

use crate::crds::workerpool::WorkerPool;
use crate::crds::workerpoolset::WorkerPoolSet;
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::Ctx;

mod builders;
use builders::{build_child_workerpool, child_name};

/// Kubebuilder-convention finalizer name: `{kind}.{group}/{suffix}`.
/// `cleanup` describes what the finalizer gates (explicit child
/// deletion for deterministic timing). Matches the retrofit naming
/// applied to WorkerPool in [`super::workerpool`].
pub const FINALIZER: &str = "workerpoolset.rio.build/cleanup";

/// SSA field manager for child WorkerPool patches. Distinct from
/// the WorkerPool reconciler's `"rio-controller"` so `kubectl get
/// wp -o yaml | grep managedFields` shows which controller owns
/// which fields. P0234's autoscaler uses a THIRD field manager
/// (`rio-controller-wps-autoscaler`) so its `spec.replicas`
/// patches don't conflict with this reconciler's template sync.
pub const MANAGER: &str = "rio-controller-wps";

/// Requeue interval on successful apply. 5min matches the
/// WorkerPool reconciler — event-driven reconciles (via `.owns()`
/// on WorkerPool in main.rs) handle most changes; the timer is a
/// backstop for dropped watch events.
const REQUEUE_INTERVAL: Duration = Duration::from_secs(300);

/// Top-level reconcile. Wrapped in `finalizer()` — same pattern
/// as the WorkerPool reconciler (see workerpool/mod.rs for the
/// full metadata.finalizers dance explanation).
#[tracing::instrument(
    skip(wps, ctx),
    fields(reconciler = "workerpoolset", wps = %wps.name_any(), ns = wps.namespace().as_deref().unwrap_or(""))
)]
pub async fn reconcile(wps: Arc<WorkerPoolSet>, ctx: Arc<Ctx>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(wps, ctx).await;
    metrics::histogram!("rio_controller_reconcile_duration_seconds",
        "reconciler" => "workerpoolset")
    .record(start.elapsed().as_secs_f64());
    result
}

async fn reconcile_inner(wps: Arc<WorkerPoolSet>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = wps
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("WorkerPoolSet has no namespace".into()))?;
    let api: Api<WorkerPoolSet> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&api, FINALIZER, wps, |event| async {
        match event {
            Event::Apply(wps) => apply(wps, &ctx).await,
            Event::Cleanup(wps) => cleanup(wps, &ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

/// Normal reconcile: for each size class, SSA-apply its child
/// WorkerPool. Idempotent — SSA with the same body is a no-op.
async fn apply(wps: Arc<WorkerPoolSet>, ctx: &Ctx) -> Result<Action> {
    let ns = wps.namespace().expect("checked in reconcile_inner()");
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);

    for class in &wps.spec.classes {
        let child = build_child_workerpool(&wps, class)?;
        let name = child.name_any();
        wp_api
            .patch(
                &name,
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(&child),
            )
            .await?;
        debug!(child = %name, class = %class.name, "child WorkerPool applied");
    }

    info!(
        wps = %wps.name_any(),
        classes = wps.spec.classes.len(),
        "reconciled"
    );

    // Status refresh: OMITTED. P0234 adds the GetSizeClassStatus
    // call + status SSA patch block here. Until then, reconcile
    // succeeds without touching status (and `tracey query untested`
    // correctly shows `ctrl.wps.reconcile` as missing a verify site
    // — P0239's VM test is that site).

    Ok(Action::requeue(REQUEUE_INTERVAL))
}

/// Cleanup on delete. Explicitly delete each child WorkerPool.
///
/// `ownerRef` GC would eventually do this, but "eventually" is
/// racy for tests and operationally opaque (operators don't see
/// progress). Explicit delete is deterministic and produces clear
/// `kubectl get events` output.
///
/// 404 tolerance: the child might already be gone (GC ran first,
/// or the operator manually deleted it, or a previous cleanup
/// succeeded on this child before crashing on the next).
/// Fine — skip and continue to the next child.
async fn cleanup(wps: Arc<WorkerPoolSet>, ctx: &Ctx) -> Result<Action> {
    let ns = wps.namespace().expect("checked in reconcile_inner()");
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);

    for class in &wps.spec.classes {
        let name = child_name(&wps, class);
        match wp_api.delete(&name, &DeleteParams::default()).await {
            Ok(_) => debug!(child = %name, "child WorkerPool deleted"),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!(child = %name, "child already gone (404)");
            }
            Err(e) => {
                // One child's delete failed — propagate so the
                // finalizer stays and cleanup retries. Better than
                // leaking a child that survives WPS delete.
                warn!(child = %name, error = %e, "child delete failed");
                return Err(e.into());
            }
        }
    }

    info!(wps = %wps.name_any(), "cleanup complete");
    Ok(Action::await_change())
}

/// Requeue policy on error. Same shape as WorkerPool's
/// `error_policy` — InvalidSpec (operator needs to fix the CRD)
/// gets a long backoff; transient Kube/Finalizer errors retry
/// sooner.
pub fn error_policy(_wps: Arc<WorkerPoolSet>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    metrics::counter!("rio_controller_reconcile_errors_total",
        "reconciler" => "workerpoolset", "error_kind" => error_kind(err))
    .increment(1);

    match err {
        Error::InvalidSpec(msg) => {
            warn!(error = %msg, "invalid WorkerPoolSet spec; fix the CRD");
            Action::requeue(Duration::from_secs(300))
        }
        _ => {
            warn!(error = %err, "reconcile failed; retrying");
            Action::requeue(Duration::from_secs(30))
        }
    }
}
