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
//! WPS status) joins the `GetSizeClassStatus` admin RPC (scheduler-
//! side EMA-smoothed cutoffs + live queue depths) with the observed
//! child WorkerPool status (replicas / readyReplicas). The unit
//! tests here cover the SSA patch-body shape; P0239's VM lifecycle
//! test is the end-to-end `r[verify]` site.
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
//! # Prune-stale (`r[ctrl.wps.prune-stale]`)
//!
//! `apply()` lists WPS-owned children (by ownerRef UID) and
//! deletes any whose `size_class` isn't in the current
//! `spec.classes`. Without prune, a removed-class child is
//! orphaned: the standalone autoscaler skips it (has ownerRef),
//! the per-class autoscaler skips it (not in `spec.classes`
//! iteration) — neither scales it. See `prune_stale_children`.
//!
//! # What this does NOT do
//!
//! - Per-class autoscaling: lives in scaling.rs (`scale_wps_class`
//!   — separate task, poll-driven at autoscaler cadence).

use std::sync::Arc;
use std::time::Duration;

use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use kube::{CustomResourceExt, ResourceExt};
use tracing::{debug, info, warn};

use rio_proto::types::{GetSizeClassStatusRequest, GetSizeClassStatusResponse};

use crate::crds::workerpool::WorkerPool;
use crate::crds::workerpoolset::{ClassStatus, WorkerPoolSet};
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::Ctx;
use crate::scaling::is_wps_owned_by;

pub(crate) mod builders;
use builders::{build_child_workerpool, child_name};

/// Kubebuilder-convention finalizer name: `{kind}.{group}/{suffix}`.
/// `cleanup` describes what the finalizer gates (explicit child
/// deletion for deterministic timing). Matches the retrofit naming
/// applied to WorkerPool in [`super::workerpool`].
pub(crate) const FINALIZER: &str = "workerpoolset.rio.build/cleanup";

/// SSA field manager for child WorkerPool patches. Distinct from
/// the WorkerPool reconciler's `"rio-controller"` so `kubectl get
/// wp -o yaml | grep managedFields` shows which controller owns
/// which fields. The per-class autoscaler (scaling.rs) uses a
/// THIRD field manager (`rio-controller-wps-autoscaler`) so its
/// `spec.replicas` patches don't conflict with this reconciler's
/// template sync.
pub(crate) const MANAGER: &str = "rio-controller-wps";

/// SSA field manager for the WPS STATUS patch (per-class
/// `effective_cutoff_secs` + `queued`). Distinct from `MANAGER`
/// (which owns child WorkerPool spec fields) so a future
/// operator-owned status field wouldn't be clobbered.
pub(crate) const STATUS_MANAGER: &str = "rio-controller-wps-status";

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
    // reconcile_inner() already checked namespace is Some, but
    // re-derive via ok_or rather than .expect() — cross-function
    // invariants are refactor-fragile, and a panic here is a pod
    // crash-loop. InvalidSpec surfaces in error_policy instead.
    let ns = wps
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("WorkerPoolSet has no namespace".into()))?;
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

    // r[impl ctrl.wps.prune-stale]
    // ---- Prune stale children ----
    // WPS-owned children whose class was removed from spec.classes.
    // The orphan scenario: operator deletes a class → the
    // standalone-scaler skips the child (has ownerRef), the
    // per-class loop skips it (not in spec.classes iteration) →
    // NEITHER scales it. Delete it instead.
    //
    // List by ownerRef UID (not name-prefix — an operator could
    // rename the WPS; child names wouldn't follow, but ownerRef
    // UID does). Best-effort: delete failures warn! + continue
    // (non-fatal so a stuck child doesn't wedge the whole
    // reconcile); 404 is silently fine (already gone).
    prune_stale_children(&wps, &wp_api).await;

    // r[impl ctrl.wps.cutoff-status]
    // ---- Status refresh ----
    // Call GetSizeClassStatus, write per-class `effective_cutoff_secs`
    // + `queued` to WPS status via SSA patch. The SSA body MUST
    // include `apiVersion` + `kind` — without them the apiserver
    // returns 400 "apiVersion must be set" (3a bug, see lang-gotchas).
    //
    // Best-effort: if the scheduler is unavailable, log + skip
    // status (the child-apply loop above already succeeded; status
    // is observability, not correctness). Next reconcile retries.
    let wps_api: Api<WorkerPoolSet> = Api::namespaced(ctx.client.clone(), &ns);
    match ctx
        .admin
        .clone()
        .get_size_class_status(GetSizeClassStatusRequest {})
        .await
    {
        Ok(resp) => {
            let resp = resp.into_inner();
            let class_statuses = build_class_statuses(&wps, &resp, &wp_api).await;
            let patch = wps_status_patch(&class_statuses);
            wps_api
                .patch_status(
                    &wps.name_any(),
                    &PatchParams::apply(STATUS_MANAGER).force(),
                    &Patch::Apply(&patch),
                )
                .await?;
        }
        Err(e) => {
            warn!(
                wps = %wps.name_any(),
                error = %e,
                "GetSizeClassStatus unavailable; skipping status refresh (will retry next reconcile)"
            );
        }
    }

    info!(
        wps = %wps.name_any(),
        classes = wps.spec.classes.len(),
        "reconciled"
    );

    Ok(Action::requeue(REQUEUE_INTERVAL))
}

/// Prune WPS-owned child WorkerPools whose `size_class` no longer
/// appears in `wps.spec.classes`. Identifies children by ownerRef
/// UID (via `is_wps_owned_by`) — NOT by name-prefix: a rename or
/// a second WPS in the same namespace would make name-based
/// matching prune the wrong thing.
///
/// Best-effort by design (spec `r[ctrl.wps.prune-stale]`):
///   - List failure → warn! + skip prune this reconcile (retried
///     next reconcile). Don't propagate: the SSA-apply loop above
///     already succeeded, so failing the WHOLE reconcile on a
///     stale-child list error loses more than it gains.
///   - Per-child delete failure → warn! + continue to next child.
///     A stuck child shouldn't wedge the entire reconcile.
///   - 404 on delete → already gone (GC won the race, or operator
///     manually deleted it, or a previous reconcile's prune
///     succeeded before this one was triggered). Silently fine.
///
/// Skips children with `deletionTimestamp` set — they're already
/// being deleted (finalizer in flight). Issuing a second delete
/// is harmless but noisy in `kubectl get events`.
async fn prune_stale_children(wps: &WorkerPoolSet, wp_api: &Api<WorkerPool>) {
    let active_classes: std::collections::BTreeSet<&str> =
        wps.spec.classes.iter().map(|c| c.name.as_str()).collect();

    let children = match wp_api.list(&ListParams::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            warn!(
                wps = %wps.name_any(),
                error = %e,
                "list WorkerPools failed; skipping stale-child prune (will retry next reconcile)"
            );
            return;
        }
    };

    for child in children {
        // Only THIS WPS's children (UID match). A second WPS in
        // the namespace owns its own children; name-prefix would
        // collide, UID doesn't.
        if !is_wps_owned_by(&child, wps) {
            continue;
        }
        // Already being deleted — don't double-issue.
        if child.metadata.deletion_timestamp.is_some() {
            continue;
        }
        // size_class is set to class.name by build_child_workerpool
        // (see builders.rs). A child whose size_class is still in
        // the active set is current; one whose size_class was
        // removed is stale.
        if active_classes.contains(child.spec.size_class.as_str()) {
            continue;
        }

        let name = child.name_any();
        info!(
            child = %name,
            class = %child.spec.size_class,
            "pruning stale WPS child (class removed from spec.classes)"
        );
        match wp_api.delete(&name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                // Already gone. Race with ownerRef GC or manual
                // delete — same tolerance as cleanup().
            }
            Err(e) => {
                warn!(
                    child = %name,
                    error = %e,
                    "prune delete failed (non-fatal; will retry next reconcile)"
                );
            }
        }
    }
}

/// Build per-class `ClassStatus` entries: join the WPS spec
/// classes with the `GetSizeClassStatus` RPC response and the
/// observed child WorkerPool status (replicas / readyReplicas).
///
/// Missing RPC class (scheduler doesn't know this class) →
/// fall through to the spec's `cutoff_secs` for
/// `effective_cutoff_secs` and 0 for `queued`. The operator
/// sees "configured but not yet observed" — usually transient
/// (scheduler restarting, or the WPS class name doesn't match
/// the scheduler's configured size_classes).
///
/// Missing child WorkerPool (create failed, or not yet applied)
/// → 0 replicas. The `kubectl get wps` Ready column reads 0/0
/// which correctly surfaces "child doesn't exist yet."
async fn build_class_statuses(
    wps: &WorkerPoolSet,
    resp: &GetSizeClassStatusResponse,
    wp_api: &Api<WorkerPool>,
) -> Vec<ClassStatus> {
    let mut out = Vec::with_capacity(wps.spec.classes.len());
    for class in &wps.spec.classes {
        let child = child_name(wps, class);
        let rpc_class = resp.classes.iter().find(|c| c.name == class.name);

        // Child WorkerPool status lookup. get_opt: 404 → None
        // (child not yet created, or was just deleted). Treat
        // as 0/0 replicas — next reconcile creates it and the
        // status updates.
        let (replicas, ready_replicas) = match wp_api.get_opt(&child).await {
            Ok(Some(wp)) => wp
                .status
                .map(|s| (s.replicas, s.ready_replicas))
                .unwrap_or((0, 0)),
            Ok(None) => (0, 0),
            Err(e) => {
                // Transient K8s error. Log + zero — status is
                // best-effort. Next reconcile retries.
                warn!(child = %child, error = %e, "child WorkerPool GET failed");
                (0, 0)
            }
        };

        out.push(ClassStatus {
            name: class.name.clone(),
            effective_cutoff_secs: rpc_class
                .map(|c| c.effective_cutoff_secs)
                .unwrap_or(class.cutoff_secs),
            queued: rpc_class.map(|c| c.queued).unwrap_or(0),
            child_pool: child,
            replicas,
            ready_replicas,
        });
    }
    out
}

/// Build the SSA status-patch body for `WorkerPoolSet.status.classes`.
///
/// `apiVersion` + `kind` are MANDATORY — apiserver rejects SSA
/// patches without them (400 "apiVersion must be set"). Same
/// pattern as workerpool/mod.rs's status patch and
/// scaling.rs's `wp_status_patch`.
///
/// Extracted as a pure fn so a unit test can assert the body
/// shape without a mock-apiserver round-trip.
pub(crate) fn wps_status_patch(classes: &[ClassStatus]) -> serde_json::Value {
    let ar = WorkerPoolSet::api_resource();
    serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "classes": classes,
        },
    })
}

/// Cleanup on delete. Explicitly delete each child WorkerPool.
///
/// `ownerRef` GC would eventually do this, but "eventually" is
/// racy for tests and operationally opaque (operators don't see
/// progress). Explicit delete is deterministic and produces clear
/// `kubectl get events` output.
///
/// Children are discovered by ownerRef UID (same as
/// `prune_stale_children`), NOT by iterating `spec.classes`. The
/// spec-iteration approach leaks orphans in this sequence:
///
///   1. Operator removes class "large" from `spec.classes`
///   2. Before the next reconcile runs `prune_stale_children`,
///      operator deletes the WPS
///   3. Cleanup iterates the (now-shortened) `spec.classes` →
///      never sees "large" → orphan survives WPS delete
///
/// Listing by ownerRef UID catches every child regardless of
/// whether it's still in spec.
///
/// 404 tolerance: the child might already be gone (GC ran first,
/// or the operator manually deleted it, or a previous cleanup
/// succeeded on this child before crashing on the next).
/// Fine — skip and continue to the next child.
async fn cleanup(wps: Arc<WorkerPoolSet>, ctx: &Ctx) -> Result<Action> {
    let ns = wps
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("WorkerPoolSet has no namespace".into()))?;
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);

    // List all WorkerPools in the namespace, filter by ownerRef
    // UID. Same pattern as prune_stale_children — a second WPS in
    // the same namespace owns its own children; UID-match ensures
    // we only delete ours. Unlike prune, a list failure HERE is
    // propagated (finalizer stays, cleanup retries) — leaking
    // children after WPS delete is worse than retrying.
    let children = wp_api.list(&ListParams::default()).await?;

    for child in &children.items {
        if !is_wps_owned_by(child, &wps) {
            continue;
        }
        let name = child.name_any();
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

// r[verify ctrl.wps.cutoff-status]
#[cfg(test)]
mod tests {
    use super::*;

    fn test_class_status(name: &str, cutoff: f64, queued: u64) -> ClassStatus {
        ClassStatus {
            name: name.into(),
            effective_cutoff_secs: cutoff,
            queued,
            child_pool: format!("test-wps-{name}"),
            replicas: 0,
            ready_replicas: 0,
        }
    }

    /// SSA status-patch body MUST carry `apiVersion` + `kind` or
    /// the apiserver returns 400 "apiVersion must be set". A body
    /// like `{"status":{"classes":[...]}}` silently fails every
    /// status refresh. This is the 3a bug (see lang-gotchas) —
    /// tripwire so nobody strips the GVK "for brevity."
    ///
    /// Also verify the body is STATUS-only (no spec fields). SSA
    /// with the `rio-controller-wps-status` field manager owns
    /// `.status.classes`; the reconciler's `rio-controller-wps`
    /// manager owns child WorkerPool spec. Including spec here
    /// would clobber the reconciler's apply.
    #[test]
    fn wps_status_patch_has_gvk_and_status_only() {
        let classes = vec![
            test_class_status("small", 60.0, 12),
            test_class_status("large", 600.0, 3),
        ];
        let patch = wps_status_patch(&classes);

        // --- GVK: MANDATORY for SSA ---
        // rio.build/v1alpha1 — mirrors the #[kube(group, version)]
        // attrs on WorkerPoolSetSpec.
        assert_eq!(
            patch.get("apiVersion").and_then(|v| v.as_str()),
            Some("rio.build/v1alpha1"),
            "SSA body without apiVersion → apiserver 400"
        );
        assert_eq!(
            patch.get("kind").and_then(|v| v.as_str()),
            Some("WorkerPoolSet"),
            "SSA body without kind → apiserver 400"
        );

        // --- status.classes present with camelCase keys ---
        let status_classes = patch
            .get("status")
            .and_then(|s| s.get("classes"))
            .and_then(|c| c.as_array())
            .expect("status.classes array");
        assert_eq!(status_classes.len(), 2);
        // ClassStatus has #[serde(rename_all = "camelCase")] —
        // effectiveCutoffSecs not effective_cutoff_secs. A
        // snake_case leak means kubectl reads null.
        assert_eq!(
            status_classes[0]
                .get("effectiveCutoffSecs")
                .and_then(|v| v.as_f64()),
            Some(60.0),
            "camelCase key required (kubectl reads camelCase)"
        );
        assert_eq!(
            status_classes[0].get("queued").and_then(|v| v.as_u64()),
            Some(12)
        );
        assert_eq!(
            status_classes[1].get("childPool").and_then(|v| v.as_str()),
            Some("test-wps-large")
        );

        // --- spec ABSENT (we own status, reconciler owns spec) ---
        assert!(
            patch.get("spec").is_none(),
            "status patch must not touch spec (field-manager ownership split)"
        );
    }
}
