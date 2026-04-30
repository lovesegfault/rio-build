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
pub(crate) const REASON_FETCHER_PRIVILEGED_SUPPRESSED: &str = "FetcherPrivilegedSuppressed";
pub(crate) const REASON_FETCHER_HOST_NETWORK_SUPPRESSED: &str = "FetcherHostNetworkSuppressed";
pub(crate) const REASON_FETCHER_SECCOMP_OVERRIDDEN: &str = "FetcherSeccompOverridden";
pub(crate) const REASON_FETCHER_FUSE_TUNING_IGNORED: &str = "FetcherFuseTuningIgnored";
pub(crate) const REASON_FETCHER_FEATURES_IGNORED: &str = "FetcherFeaturesIgnored";
pub(crate) const REASON_BUILDER_FUSE_CACHE_IGNORED: &str = "BuilderFuseCacheBytesIgnored";

/// One spec-degrade check. `applies` is a pure predicate over the
/// spec; if true, a `Warning` event with `reason`/`note` is emitted.
/// Each entry mirrors a CEL admission rule (or a non-CEL silent
/// override in `pod.rs`). Table-driven so adding a CEL rule without a
/// corresponding entry here is the only way to forget — the spec
/// `r[ctrl.event.spec-degrade]` says "every", so the test asserts
/// `DEGRADE_CHECKS.len() >= <count of CEL rules + 1>`.
pub(crate) struct DegradeCheck {
    pub applies: fn(&rio_crds::pool::PoolSpec) -> bool,
    pub reason: &'static str,
    pub note: &'static str,
}

#[inline]
fn is_fetcher_spec(s: &rio_crds::pool::PoolSpec) -> bool {
    s.kind == ExecutorKind::Fetcher
}

/// One entry per silent override in `pod.rs::effective_*` /
/// `wants_kvm`. Mirrors the CEL rules at `rio-crds/src/pool.rs:
/// 121-155` plus the non-CEL `wants_kvm` drop.
pub(crate) const DEGRADE_CHECKS: &[DegradeCheck] = &[
    // r[impl ctrl.crd.host-users-network-exclusive]
    // Builder-only: the pod.rs:327 suppression this mirrors only fires
    // on the Builder path. Fetchers always get `Some(false)` from
    // `effective_host_users` (never omitted) and `host_network=None`
    // forced — entry [2] (`FetcherHostNetworkSuppressed`) is the
    // correct event for a Fetcher with `hostNetwork:true`. Without
    // this gate, a pre-CEL `Fetcher{hostNetwork:true}` emitted a
    // factually-wrong "hostUsers omitted" + unactionable "Set
    // privileged:true" (forced false for Fetchers).
    DegradeCheck {
        applies: |s| {
            !is_fetcher_spec(s) && s.host_network == Some(true) && s.privileged != Some(true)
        },
        reason: REASON_HOST_USERS_SUPPRESSED,
        note: "hostNetwork:true forces hostUsers omitted \
               (K8s admission rejects the combo). Set \
               privileged:true explicitly, or drop hostNetwork.",
    },
    DegradeCheck {
        applies: |s| is_fetcher_spec(s) && s.privileged == Some(true),
        reason: REASON_FETCHER_PRIVILEGED_SUPPRESSED,
        note: "kind=Fetcher forces privileged:false — fetchers face the \
               open internet; the escape hatch stays closed (ADR-019). \
               Drop privileged:true.",
    },
    DegradeCheck {
        applies: |s| is_fetcher_spec(s) && s.host_network == Some(true),
        reason: REASON_FETCHER_HOST_NETWORK_SUPPRESSED,
        note: "kind=Fetcher forces hostNetwork:false — fetchers run on \
               dedicated nodes with pod networking (ADR-019). Drop \
               hostNetwork:true.",
    },
    DegradeCheck {
        applies: |s| is_fetcher_spec(s) && s.seccomp_profile.is_some(),
        reason: REASON_FETCHER_SECCOMP_OVERRIDDEN,
        note: "kind=Fetcher forces seccompProfile=Localhost \
               operator/rio-fetcher.json (ADR-019). Drop seccompProfile.",
    },
    DegradeCheck {
        applies: |s| {
            is_fetcher_spec(s) && (s.fuse_threads.is_some() || s.fuse_passthrough.is_some())
        },
        reason: REASON_FETCHER_FUSE_TUNING_IGNORED,
        note: "kind=Fetcher ignores fuseThreads/fusePassthrough — fetches \
               are network-bound, not FUSE-bound. Drop the FUSE tuning knobs.",
    },
    // r[impl ctrl.crd.fetcher-no-features]
    // ANY non-empty features (not just "kvm"): the I-181 ∅-guard at
    // scheduler `snapshot.rs:221` filters featureless FODs whenever
    // the pool's features list is non-empty, regardless of value.
    DegradeCheck {
        applies: |s| is_fetcher_spec(s) && !s.features.is_empty(),
        reason: REASON_FETCHER_FEATURES_IGNORED,
        note: "kind=Fetcher ignores features — FODs route by \
               is_fixed_output alone. Drop features.",
    },
    DegradeCheck {
        applies: |s| s.kind == ExecutorKind::Builder && s.fuse_cache_bytes.is_some(),
        reason: REASON_BUILDER_FUSE_CACHE_IGNORED,
        note: "kind=Builder ignores fuseCacheBytes — Builder pools \
               single-source from controller [nodeclaim_pool].fuse_cache_bytes \
               so FFD/cover/stamp agree (mb_035). Drop fuseCacheBytes.",
    },
];

/// Emit Warning events for every spec field the builder will silently
/// degrade. Each check mirrors a CEL rule at apply-time; the builder's
/// defensive override handles pre-CEL specs that the apiserver already
/// accepted, but the OPERATOR doesn't know their spec is stale unless
/// we surface it. Best-effort: event-publish failures are logged in
/// [`Ctx::publish_event`], never block reconcile.
// r[impl ctrl.event.spec-degrade]
async fn warn_on_spec_degrades(pool: &Pool, ctx: &Ctx) {
    use kube::runtime::events::{Event as KubeEvent, EventType};
    for check in DEGRADE_CHECKS {
        if (check.applies)(&pool.spec) {
            ctx.publish_event(
                pool,
                &KubeEvent {
                    type_: EventType::Warning,
                    reason: check.reason.into(),
                    note: Some(check.note.into()),
                    action: "Reconcile".into(),
                    secondary: None,
                },
            )
            .await;
        }
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
