//! BuilderPool reconciler: spawn/reap one-shot rio-builder Jobs.
//!
//! Reconcile flow:
//! 1. Poll `ClusterStatus`/`GetSizeClassStatus` for queued depth.
//! 2. Spawn Jobs up to `spec.maxConcurrent` (see `ephemeral.rs`).
//! 3. Reap excess Pending and orphan Running Jobs.
//! 4. Patch BuilderPool.status from active Job count.
// r[impl ctrl.crd.builderpool]
// r[impl ctrl.reconcile.owner-refs]
// r[impl ctrl.drain.sigterm]
//!
//! Server-side apply throughout: we PATCH with `fieldManager:
//! rio-controller`, K8s merges. Idempotent — same patch twice is
//! a no-op. No GET-modify-PUT race.
//!
//! Finalizer wraps everything: delete → cleanup (DrainExecutor +
//! scale STS to 0 + wait for pods gone) → finalizer removed →
//! K8s GC's the children via ownerReference.

use std::sync::Arc;
use std::time::Duration;

// Pod + Resource are used by the tests/ submodule via `use super::*`;
// lib code only needs ResourceExt.
#[allow(unused_imports)]
use k8s_openapi::api::core::v1::Pod;
#[allow(unused_imports)]
use kube::Resource;
use kube::ResourceExt;
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use tracing::{info, warn};

use crate::crds::builderpool::{BuilderPool, Sizing};
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::{Ctx, error_key};

mod builders;
pub mod disruption;
pub(super) mod ephemeral;
pub(super) mod job_common;
mod manifest;
// pub(crate) so fixtures.rs (at crate root) can see it. Gated
// on test: production code in this module pulls it via the glob
// below; only the cfg(test) fixtures module needs the wider
// visibility.
#[allow(unused_imports)]
use builders::*;

#[cfg(test)]
pub(super) mod tests;

/// Finalizer name. Kubebuilder convention: `{kind}.{group}/{suffix}`
/// — the kind is the authoritative part (which controller owns this),
/// suffix describes WHAT the finalizer gates. K8s stores this in
/// `metadata.finalizers`; delete blocks until we remove it.
const FINALIZER: &str = "builderpool.rio.build/drain";

/// Field manager for server-side apply. K8s tracks which fields
/// each manager owns; conflicting managers get a 409 unless
/// `force`. We use `force: true` — this controller is
/// authoritative for what it manages.
const MANAGER: &str = "rio-controller";

/// Label every BuilderPool-owned pod carries. `builders::labels()`
/// sets it; `disruption::run` filters on it; `ephemeral` + cleanup
/// list-selectors match on it. Re-exported from common — shared
/// with the fetcherpool reconciler.
pub(crate) use crate::reconcilers::common::pod::POOL_LABEL;

/// Top-level reconcile. Wrapped in `finalizer()` which handles
/// the metadata.finalizers dance: Apply on normal reconcile,
/// Cleanup when deletionTimestamp is set.
///
/// `#[instrument]` creates a span carrying pool/ns for every
/// log line inside. Histogram records duration — the
/// observability spec (observability.md:132) calls for
/// `rio_controller_reconcile_duration_seconds` labeled by
/// reconciler; this provides it.
#[tracing::instrument(
    skip(wp, ctx),
    fields(reconciler = "builderpool", pool = %wp.name_any(), ns = wp.namespace().as_deref().unwrap_or(""))
)]
pub async fn reconcile(wp: Arc<BuilderPool>, ctx: Arc<Ctx>) -> Result<Action> {
    let start = std::time::Instant::now();
    let key = error_key(wp.as_ref());
    let result = reconcile_inner(wp, ctx.clone()).await;
    // Reset the error-backoff counter on success so the NEXT
    // failure starts the curve from 5s, not from wherever the
    // last streak left off.
    if result.is_ok() {
        ctx.reset_error_count(&key);
    }
    // Record duration regardless of success/error — error-path
    // duration is a useful signal (slow apiserver timeouts show
    // as long durations + error).
    metrics::histogram!("rio_controller_reconcile_duration_seconds",
        "reconciler" => "builderpool")
    .record(start.elapsed().as_secs_f64());
    result
}

/// Actual reconcile body. Separate from the metric-wrapped
/// `reconcile()` so `?` exits at the right scope (after the
/// histogram record, not short-circuiting it).
async fn reconcile_inner(wp: Arc<BuilderPool>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = wp.namespace().ok_or_else(|| {
        // BuilderPool is #[kube(namespaced)] so this can't happen
        // via normal apiserver paths (it'd reject a cluster-
        // scoped BuilderPool). But the type is Option<String>
        // (k8s-openapi models it that way). Belt-and-suspenders.
        Error::InvalidSpec("BuilderPool has no namespace (should be impossible)".into())
    })?;
    let api: Api<BuilderPool> = Api::namespaced(ctx.client.clone(), &ns);

    // finalizer() manages the metadata.finalizers entry. It calls
    // our closure with Event::Apply or Event::Cleanup. After
    // Cleanup returns Ok, it removes the finalizer → K8s GC
    // proceeds. Cleanup Err → finalizer stays, reconcile retries.
    //
    // Box::new on the Err: finalizer::Error<Error> is recursive
    // (see error.rs). The `?` converts via our From<Box<...>>.
    finalizer(&api, FINALIZER, wp, |event| async {
        match event {
            Event::Apply(wp) => apply(wp, &ctx).await,
            Event::Cleanup(wp) => cleanup(wp, &ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

/// Emit Warning events for every spec field the builder will silently
/// degrade. Each check mirrors a CEL rule at apply-time (builderpool.rs
/// `#[x_kube(validation)]` attrs); the builder's defensive override
/// handles pre-CEL specs that the apiserver already accepted, but the
/// OPERATOR doesn't know their spec is stale unless we surface it.
///
/// Runs BEFORE the ephemeral branch in [`apply`] so both paths
/// (STS-mode + ephemeral) get visibility. `build_pod_spec` is shared
/// by both (ephemeral calls it via `build_job`), so any builder-side
/// degrade applies to both; the Warning should too.
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
    // point: one helper, N checks, consistent visibility across both
    // reconcile modes.
}

/// Normal reconcile: make the world match spec.
async fn apply(wp: Arc<BuilderPool>, ctx: &Ctx) -> Result<Action> {
    // reconcile_inner() already checked namespace is Some, but
    // re-derive via ok_or rather than .expect() — cross-function
    // invariants are refactor-fragile, and a panic here is a pod
    // crash-loop. InvalidSpec surfaces in error_policy instead.
    let _ns = wp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("BuilderPool has no namespace".into()))?;
    let _name = wp.name_any();

    // Surface silent degrades — shared by Static and Manifest
    // reconcile paths (both use build_pod_spec).
    warn_on_spec_degrades(&wp, ctx).await;

    // Both reconcile paths spawn one-shot Jobs. The only difference
    // is sizing: Manifest reads per-derivation resource estimates
    // from GetCapacityManifest (ADR-020); Static uses fixed pool
    // resources from spec.
    // r[impl ctrl.pool.manifest-reconcile]
    if wp.spec.sizing == Sizing::Manifest {
        return manifest::reconcile_manifest(&wp, ctx).await;
    }
    ephemeral::reconcile_ephemeral(&wp, ctx).await
}

/// Cleanup on delete. Jobs are one-shot and complete on their own;
/// in-flight builds finish naturally (the worker doesn't know the CR
/// is being deleted). Returning here removes the finalizer and
/// ownerReference GC deletes the Jobs. To interrupt in-flight builds,
/// `kubectl delete jobs -l rio.build/pool=X`.
async fn cleanup(wp: Arc<BuilderPool>, _ctx: &Ctx) -> Result<Action> {
    info!(builderpool = %wp.name_any(), sizing = ?wp.spec.sizing,
          "cleanup: ownerRef GC handles Jobs");
    Ok(Action::await_change())
}
/// Requeue policy on error. Transient (Kube, Scheduler) →
/// exponential backoff (5s → 300s). InvalidSpec → fixed 5min
/// (operator needs to fix it; retrying fast is noise).
pub fn error_policy(wp: Arc<BuilderPool>, err: &Error, ctx: Arc<Ctx>) -> Action {
    metrics::counter!("rio_controller_reconcile_errors_total",
        "reconciler" => "builderpool", "error_kind" => error_kind(err))
    .increment(1);

    match err {
        Error::InvalidSpec(msg) => {
            // Operator error. Requeue slow — they need to edit
            // the CRD. The log is their signal.
            warn!(error = %msg, "invalid BuilderPool spec; fix the CRD");
            Action::requeue(Duration::from_secs(300))
        }
        _ => {
            // Transient (apiserver hiccup, scheduler restarting).
            // Exponential backoff: 5s → 10s → … → 300s cap.
            // A persistent 5xx backs off to 5min after ~6
            // failures instead of retrying every 30s indefinitely.
            // Reset on the next successful reconcile.
            //
            // warn! not debug! — a silent retry loop is invisible
            // at INFO and cost us ~10min of VM debugging once.
            let delay = ctx.error_backoff(&error_key(wp.as_ref()));
            warn!(error = %err, backoff = ?delay, "reconcile failed; retrying");
            Action::requeue(delay)
        }
    }
}
