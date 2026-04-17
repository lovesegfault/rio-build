//! Pool reconciler: spawn/reap one-shot rio-builder Jobs.
//!
//! Reconcile flow:
//! 1. Poll `GetSpawnIntents` for per-drv intents.
//! 2. Spawn Jobs up to `spec.maxConcurrent` (see `jobs.rs`).
//! 3. Reap excess Pending and orphan Running Jobs.
//! 4. Patch Pool.status from active Job count.
// r[impl ctrl.pool.reconcile]
// r[impl ctrl.crd.pool]
// r[impl ctrl.reconcile.owner-refs]
// r[impl ctrl.drain.sigterm]
//!
//! Server-side apply throughout: we PATCH with `fieldManager:
//! rio-controller`, K8s merges. Idempotent — same patch twice is
//! a no-op. No GET-modify-PUT race.
//!
//! Finalizer wraps everything: delete → cleanup() is a no-op (Jobs
//! carry an ownerReference to the Pool) → finalizer removed →
//! K8s ownerRef-GC cascades to the Jobs → their pods get SIGTERM and
//! drain at the executor level.

use std::sync::Arc;
use std::time::Duration;

use kube::ResourceExt;
use kube::runtime::controller::Action;
use tracing::info;

use crate::error::{Error, Result};
use crate::reconcilers::{Ctx, finalized, standard_error_policy, timed};
use rio_crds::pool::{ExecutorKind, Pool};

pub mod disruption;
pub(super) mod job;
pub(super) mod jobs;
pub mod pod;

#[cfg(test)]
pub(super) mod tests;

/// Finalizer name. Kubebuilder convention: `{kind}.{group}/{suffix}`
/// — the kind is the authoritative part (which controller owns this),
/// suffix describes WHAT the finalizer gates. K8s stores this in
/// `metadata.finalizers`; delete blocks until we remove it.
const FINALIZER: &str = "pool.rio.build/drain";

/// Label every Pool-owned pod carries. `executor_labels()` sets it;
/// `disruption::run` filters on it; `jobs` + cleanup list-selectors
/// match on it.
pub(crate) use pod::POOL_LABEL;

/// Canonical CRD-enum → proto-enum bridge.
///
/// `rio_crds::ExecutorKind` and `rio_proto::types::ExecutorKind` are
/// value-identical 2-variant enums kept separate so `rio-crds` stays
/// kube-only (no prost/tonic in the CRD codegen path) and so the
/// proto enum doesn't need `JsonSchema`/`KubeSchema` derives. A
/// `From` impl would violate the orphan rule (both types foreign), so
/// this free fn — defined in rio-controller, the only crate that
/// depends on both — is the single conversion point.
pub(super) fn executor_kind_to_proto(k: ExecutorKind) -> rio_proto::types::ExecutorKind {
    match k {
        ExecutorKind::Builder => rio_proto::types::ExecutorKind::Builder,
        ExecutorKind::Fetcher => rio_proto::types::ExecutorKind::Fetcher,
    }
}

/// Top-level reconcile. [`timed`] opens the `reconcile{reconciler,
/// name, ns}` span and records the duration histogram; `finalized`
/// handles the `metadata.finalizers` dance (Apply on normal
/// reconcile, Cleanup when deletionTimestamp is set).
pub async fn reconcile(pool: Arc<Pool>, ctx: Arc<Ctx>) -> Result<Action> {
    timed("pool", pool, ctx, |pool, ctx| {
        finalized(pool, ctx, FINALIZER, apply, cleanup)
    })
    .await
}

/// K8s Event reason for hostNetwork + !privileged spec-degrade.
/// Referenced by disruption_tests.rs event-reason reachability tests.
pub(crate) const REASON_HOST_USERS_SUPPRESSED: &str = "HostUsersSuppressedForHostNetwork";

/// Emit Warning events for every spec field the builder will silently
/// degrade. Each check mirrors a CEL rule at apply-time; the builder's
/// defensive override handles pre-CEL specs that the apiserver already
/// accepted, but the OPERATOR doesn't know their spec is stale unless
/// we surface it. Best-effort: event-publish failures are logged in
/// [`Ctx::publish_event`], never block reconcile.
// r[impl ctrl.event.spec-degrade]
async fn warn_on_spec_degrades(pool: &Pool, ctx: &Ctx) {
    use kube::runtime::events::{Event as KubeEvent, EventType};

    // r[impl ctrl.crd.host-users-network-exclusive]
    if pool.spec.host_network == Some(true) && pool.spec.privileged != Some(true) {
        ctx.publish_event(
            pool,
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
}

/// Normal reconcile: make the world match spec.
async fn apply(pool: Arc<Pool>, ctx: Arc<Ctx>) -> Result<Action> {
    warn_on_spec_degrades(&pool, &ctx).await;
    jobs::reconcile(&pool, &ctx).await
}

/// Cleanup on delete. Jobs are one-shot and complete on their own;
/// in-flight builds finish naturally. Returning here removes the
/// finalizer and ownerReference GC deletes the Jobs.
async fn cleanup(pool: Arc<Pool>, _ctx: Arc<Ctx>) -> Result<Action> {
    info!(pool = %pool.name_any(), "cleanup: ownerRef GC handles Jobs");
    Ok(Action::await_change())
}

/// Requeue policy on error. Transient (Kube, Scheduler) →
/// exponential backoff (5s → 300s). InvalidSpec → fixed 5min.
pub fn error_policy(pool: Arc<Pool>, err: &Error, ctx: Arc<Ctx>) -> Action {
    standard_error_policy("pool", pool, err, ctx, Duration::from_secs(300))
}
