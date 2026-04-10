//! BuilderPool reconciler: spawn/reap one-shot rio-builder Jobs.
//!
//! Reconcile flow:
//! 1. Poll `ClusterStatus`/`GetSizeClassStatus` for queued depth.
//! 2. Spawn Jobs up to `spec.maxConcurrent` (see `jobs.rs`).
//! 3. Reap excess Pending and orphan Running Jobs.
//! 4. Patch BuilderPool.status from active Job count.
// r[impl ctrl.builderpool.reconcile]
// r[impl ctrl.crd.builderpool]
// r[impl ctrl.reconcile.owner-refs]
// r[impl ctrl.drain.sigterm]
//!
//! Server-side apply throughout: we PATCH with `fieldManager:
//! rio-controller`, K8s merges. Idempotent — same patch twice is
//! a no-op. No GET-modify-PUT race.
//!
//! Finalizer wraps everything: delete → cleanup() is a no-op (Jobs
//! carry an ownerReference to the BuilderPool) → finalizer removed →
//! K8s ownerRef-GC cascades to the Jobs → their pods get SIGTERM and
//! drain at the executor level.

use std::sync::Arc;
use std::time::Duration;

use kube::ResourceExt;
use kube::runtime::controller::Action;
use tracing::info;

use crate::error::{Error, Result};
use crate::reconcilers::{Ctx, finalized, standard_error_policy, timed};
use rio_crds::builderpool::BuilderPool;

mod builders;
pub mod disruption;
pub(super) mod jobs;

#[cfg(test)]
pub(super) mod tests;

/// Finalizer name. Kubebuilder convention: `{kind}.{group}/{suffix}`
/// — the kind is the authoritative part (which controller owns this),
/// suffix describes WHAT the finalizer gates. K8s stores this in
/// `metadata.finalizers`; delete blocks until we remove it.
const FINALIZER: &str = "builderpool.rio.build/drain";

/// Label every BuilderPool-owned pod carries. `builders::labels()`
/// sets it; `disruption::run` filters on it; `jobs` + cleanup
/// list-selectors match on it. Re-exported from common — shared
/// with the fetcherpool reconciler.
pub(crate) use crate::reconcilers::common::pod::POOL_LABEL;

/// Top-level reconcile. [`timed`] opens the `reconcile{reconciler,
/// name, ns}` span and records the duration histogram; `finalized`
/// handles the `metadata.finalizers` dance (Apply on normal
/// reconcile, Cleanup when deletionTimestamp is set).
pub async fn reconcile(wp: Arc<BuilderPool>, ctx: Arc<Ctx>) -> Result<Action> {
    timed("builderpool", wp, ctx, |wp, ctx| {
        finalized(wp, ctx, FINALIZER, apply, cleanup)
    })
    .await
}

/// Emit Warning events for every spec field the builder will silently
/// degrade. Each check mirrors a CEL rule at apply-time (builderpool.rs
/// `#[x_kube(validation)]` attrs); the builder's defensive override
/// handles pre-CEL specs that the apiserver already accepted, but the
/// OPERATOR doesn't know their spec is stale unless we surface it.
///
/// K8s Event reason for hostNetwork + !privileged spec-degrade.
/// Referenced by disruption_tests.rs event-reason reachability tests.
pub(crate) const REASON_HOST_USERS_SUPPRESSED: &str = "HostUsersSuppressedForHostNetwork";

/// Best-effort: event-publish failures are logged in
/// [`Ctx::publish_event`], never block reconcile.
// r[impl ctrl.event.spec-degrade]
async fn warn_on_spec_degrades(wp: &BuilderPool, ctx: &Ctx) {
    use kube::runtime::events::{Event as KubeEvent, EventType};

    // r[impl ctrl.crd.host-users-network-exclusive]
    // hostUsers suppressed when hostNetwork:true + !privileged.
    // CEL rejects NEW; build_pod_spec suppresses for OLD (see
    // builders.rs host_users gate). Warning (not Normal): the
    // operator should edit their spec, not ignore this.
    if wp.spec.host_network == Some(true) && wp.spec.privileged != Some(true) {
        ctx.publish_event(
            wp,
            &KubeEvent {
                type_: EventType::Warning,
                reason: REASON_HOST_USERS_SUPPRESSED.into(),
                note: Some(
                    "hostNetwork:true forces hostUsers omitted \
                     (K8s admission rejects the combo). Set \
                     privileged:true explicitly, or drop hostNetwork."
                        .into(),
                ),
                action: "Reconcile".into(),
                secondary: None,
            },
        )
        .await;
    }

    // NEXT constraint lands HERE as another `if` block — that's the
    // point: one helper, N checks, consistent visibility.
}

/// Normal reconcile: make the world match spec.
async fn apply(wp: Arc<BuilderPool>, ctx: Arc<Ctx>) -> Result<Action> {
    // Surface silent degrades before spawn — uses build_pod_spec.
    warn_on_spec_degrades(&wp, &ctx).await;
    jobs::reconcile(&wp, &ctx).await
}

/// Cleanup on delete. Jobs are one-shot and complete on their own;
/// in-flight builds finish naturally (the worker doesn't know the CR
/// is being deleted). Returning here removes the finalizer and
/// ownerReference GC deletes the Jobs. To interrupt in-flight builds,
/// `kubectl delete jobs -l rio.build/pool=X`.
async fn cleanup(wp: Arc<BuilderPool>, _ctx: Arc<Ctx>) -> Result<Action> {
    info!(builderpool = %wp.name_any(), "cleanup: ownerRef GC handles Jobs");
    Ok(Action::await_change())
}
/// Requeue policy on error. Transient (Kube, Scheduler) →
/// exponential backoff (5s → 300s). InvalidSpec → fixed 5min
/// (operator needs to fix it; retrying fast is noise).
pub fn error_policy(wp: Arc<BuilderPool>, err: &Error, ctx: Arc<Ctx>) -> Action {
    standard_error_policy("builderpool", wp, err, ctx, Duration::from_secs(300))
}
