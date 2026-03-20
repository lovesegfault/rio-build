//! WorkerPool reconciler: one StatefulSet of rio-worker pods.
//!
//! Reconcile flow:
//! 1. Ensure headless Service exists (StatefulSet needs one for
//!    stable pod DNS; workers don't actually serve anything, but
// r[impl ctrl.crd.workerpool]
// r[impl ctrl.reconcile.owner-refs]
// r[impl ctrl.drain.all-then-scale]
// r[impl ctrl.drain.sigterm]
//!    the StatefulSet controller requires `serviceName` to point
//!    to a real Service).
//! 2. Ensure StatefulSet exists with spec derived from the CRD.
//! 3. Read StatefulSet.status → patch WorkerPool.status.
//!
//! Server-side apply throughout: we PATCH with `fieldManager:
//! rio-controller`, K8s merges. Idempotent — same patch twice is
//! a no-op. No GET-modify-PUT race.
//!
//! Finalizer wraps everything: delete → cleanup (DrainWorker +
//! scale STS to 0 + wait for pods gone) → finalizer removed →
//! K8s GC's the children via ownerReference.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use kube::{CustomResourceExt, Resource, ResourceExt};
use tracing::{debug, info, warn};

use crate::crds::workerpool::WorkerPool;
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::Ctx;

mod builders;
pub mod disruption;
mod ephemeral;
use builders::*;

#[cfg(test)]
mod tests;

/// Finalizer name. Must be unique across the cluster — prefixed
/// with our group to avoid collisions. K8s stores this in
/// `metadata.finalizers`; delete blocks until we remove it.
const FINALIZER: &str = "rio.build/workerpool-drain";

/// Field manager for server-side apply. K8s tracks which fields
/// each manager owns; conflicting managers get a 409 unless
/// `force`. We use `force: true` — this controller is
/// authoritative for what it manages.
const MANAGER: &str = "rio-controller";

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
    fields(reconciler = "workerpool", pool = %wp.name_any(), ns = wp.namespace().as_deref().unwrap_or(""))
)]
pub async fn reconcile(wp: Arc<WorkerPool>, ctx: Arc<Ctx>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(wp, ctx).await;
    // Record duration regardless of success/error — error-path
    // duration is a useful signal (slow apiserver timeouts show
    // as long durations + error).
    metrics::histogram!("rio_controller_reconcile_duration_seconds",
        "reconciler" => "workerpool")
    .record(start.elapsed().as_secs_f64());
    result
}

/// Actual reconcile body. Separate from the metric-wrapped
/// `reconcile()` so `?` exits at the right scope (after the
/// histogram record, not short-circuiting it).
async fn reconcile_inner(wp: Arc<WorkerPool>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = wp.namespace().ok_or_else(|| {
        // WorkerPool is #[kube(namespaced)] so this can't happen
        // via normal apiserver paths (it'd reject a cluster-
        // scoped WorkerPool). But the type is Option<String>
        // (k8s-openapi models it that way). Belt-and-suspenders.
        Error::InvalidSpec("WorkerPool has no namespace (should be impossible)".into())
    })?;
    let api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);

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

/// Normal reconcile: make the world match spec.
async fn apply(wp: Arc<WorkerPool>, ctx: &Ctx) -> Result<Action> {
    let ns = wp.namespace().expect("checked in reconcile()");
    let name = wp.name_any();

    // r[impl ctrl.pool.ephemeral]
    // Ephemeral mode: NO StatefulSet / headless Service / PDB. Jobs
    // are spawned on demand (reconcile_ephemeral polls ClusterStatus
    // for queued derivations). Each Job pod runs one build then
    // exits. See ephemeral.rs module doc for the full architecture.
    //
    // The branch is HERE (not deeper) because everything below is
    // STS-mode: Service for STS identity, PDB for STS eviction, the
    // STS itself. None of it applies to per-assignment Jobs.
    //
    // cleanup() also branches on ephemeral — no STS to scale to 0,
    // just wait for in-flight Jobs to finish (or let the finalizer
    // timeout → ownerRef GC deletes them).
    if wp.spec.ephemeral {
        return ephemeral::reconcile_ephemeral(&wp, ctx).await;
    }

    // r[impl ctrl.crd.host-users-network-exclusive]
    // Surface the silent degrade when a pre-CEL-rule spec has
    // hostNetwork:true + privileged:false (or unset). build_pod_spec
    // suppresses hostUsers for this combo to avoid a stuck-Pending
    // StatefulSet — that's the correctness half; THIS is the
    // visibility half. The CEL rule at apply time stops NEW specs
    // from landing this combo, but CRD upgrades don't re-validate
    // existing WorkerPools (see kube-rs #1456 — structural schema
    // is install-time). Warning (not Normal): the operator should
    // edit their spec, not ignore this.
    //
    // Before build_pod_spec (and therefore before the STS patch):
    // the event fires even if the reconcile later fails on a
    // different error, and build_pod_spec is pure (no Recorder
    // access). Best-effort publish — event failure is logged in
    // ctx.publish_event, doesn't block reconcile.
    if wp.spec.host_network == Some(true) && wp.spec.privileged != Some(true) {
        use kube::runtime::events::{Event as KubeEvent, EventType};
        ctx.publish_event(
            wp.as_ref(),
            &KubeEvent {
                type_: EventType::Warning,
                reason: "HostUsersSuppressedForHostNetwork".into(),
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

    // ownerReference: ties children to this CRD. Delete the
    // WorkerPool → K8s GC deletes the StatefulSet + Service.
    // `controller_owner_ref` sets controller=true and
    // blockOwnerDeletion=true — the GC waits for children
    // before removing the parent from etcd.
    //
    // `&()` because our DynamicType is () (statically typed CRD).
    // `.expect`: returns None only if metadata.uid or name is
    // missing — impossible for an apiserver-sourced object
    // (those fields are set on every read).
    let oref = wp
        .controller_owner_ref(&())
        .expect("apiserver-sourced object has uid");

    // ---- Headless Service ----
    // StatefulSet's serviceName MUST point to a real Service.
    // Headless (clusterIP: None) gives stable pod DNS
    // (`<pod>.<service>.<ns>.svc.cluster.local`) without load
    // balancing. Workers don't serve anything inbound (they
    // connect OUT to scheduler/store), but StatefulSet needs
    // this for pod identity.
    let svc = build_headless_service(&wp, oref.clone());
    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), &ns);
    svc_api
        .patch(
            svc.metadata.name.as_deref().expect("we set it"),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&svc),
        )
        .await?;

    // ---- PodDisruptionBudget ----
    // maxUnavailable=1: at most one worker evicted at a time
    // during node drain. The evicting pod's builds get reassigned
    // (via DrainWorker force → preemption); the rest of the pool
    // keeps working. ownerRef → GC on WorkerPool delete.
    let pdb = build_pdb(&wp, oref.clone());
    let pdb_api: Api<PodDisruptionBudget> = Api::namespaced(ctx.client.clone(), &ns);
    pdb_api
        .patch(
            pdb.metadata.name.as_deref().expect("we set it"),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&pdb),
        )
        .await?;

    // ---- StatefulSet ----
    let sts_name = format!("{name}-workers");
    let sts_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ns);

    // Check if STS already exists to decide whether to set
    // spec.replicas. SSA semantics: sending a field claims
    // ownership; omitting it releases ownership. The autoscaler
    // (scaling.rs) owns replicas via fieldManager
    // "rio-controller-autoscaler". If WE keep sending
    // replicas=min with .force(), every reconcile reverts the
    // autoscaler's patch. Instead: set it ONLY on first create
    // (STS doesn't exist), then omit it — SSA releases our
    // claim, autoscaler's value sticks.
    //
    // The extra GET is one round-trip per reconcile. Acceptable —
    // reconciles are driven by CR/STS changes, not a hot loop.
    let existing = sts_api.get_opt(&sts_name).await?;
    let initial_replicas = existing.is_none().then_some(wp.spec.replicas.min);
    // For status.desired_replicas: read what's ACTUALLY on the STS
    // (autoscaler's last decision). Falls back to min on first
    // create. Prevents kubectl's "Desired" column from showing min
    // regardless of autoscaler activity.
    let current_replicas = existing
        .as_ref()
        .and_then(|s| s.spec.as_ref())
        .and_then(|s| s.replicas)
        .unwrap_or(wp.spec.replicas.min);

    let sts = build_statefulset(
        &wp,
        oref,
        &builders::SchedulerAddrs {
            addr: ctx.scheduler_addr.clone(),
            balance_host: ctx.scheduler_balance_host.clone(),
            balance_port: ctx.scheduler_balance_port,
        },
        &ctx.store_addr,
        initial_replicas,
    )?;
    let applied = sts_api
        .patch(
            &sts_name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&sts),
        )
        .await?;

    // ---- Status ----
    // Read back what the StatefulSet controller observed. May lag
    // (StatefulSet controller hasn't reconciled our patch yet) —
    // that's fine, next reconcile catches up. `.owns()` in the
    // Controller setup watches StatefulSets; changes there
    // enqueue this reconcile.
    let sts_status = applied.status.unwrap_or_default();

    // Patch status subresource. Separate from spec — status is
    // write-only from the controller's perspective (operators
    // can't `kubectl edit` it into existence; only
    // PATCH /status works).
    //
    // apiVersion + kind are REQUIRED by server-side apply even
    // for status patches — apiserver uses them to resolve the
    // schema. Without them: 400 "apiVersion must be set in
    // apply patch".
    //
    // Partial status: we patch ONLY replicas/ready/desired. The
    // autoscaler owns lastScaleTime + conditions via a separate
    // SSA field-manager ("rio-controller-autoscaler-status").
    // SSA merges field ownership — our patch here doesn't touch
    // lastScaleTime, so the autoscaler's value persists across
    // our reconciles. Setting them here would clobber the
    // autoscaler's writes on every reconcile.
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);
    let ar = WorkerPool::api_resource();
    let status_patch = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "replicas": sts_status.replicas,
            "readyReplicas": sts_status.ready_replicas.unwrap_or(0),
            // What the autoscaler set on STS.spec.replicas (or min
            // on first create).
            "desiredReplicas": current_replicas,
            // lastScaleTime + conditions: NOT here. Autoscaler owns.
        },
    });
    wp_api
        .patch_status(
            &name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&status_patch),
        )
        .await?;

    info!(
        workerpool = %name,
        namespace = %ns,
        replicas = sts_status.replicas,
        ready = sts_status.ready_replicas.unwrap_or(0),
        "reconciled"
    );

    // Requeue in 5 minutes as a fallback. `.owns()` on StatefulSet
    // means changes there trigger us anyway; this catches the
    // edge case where something external deletes the StatefulSet
    // and the watch drops the event.
    Ok(Action::requeue(Duration::from_secs(300)))
}

/// Poll interval while waiting for StatefulSet scale-down. Long
/// enough to not spam the apiserver, short enough that a 30s
/// build completion doesn't add much latency to delete.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default pod terminationGracePeriodSeconds. 2h — long builds
/// (LLVM from cold ccache, NixOS closures). Overridable via
/// `WorkerPoolSpec.termination_grace_period_seconds`.
const DEFAULT_TERMINATION_GRACE: i64 = 7200;

/// Slop for kubelet/STS controller to observe pod termination
/// and update status.replicas AFTER grace period SIGKILL.
const DRAIN_WAIT_SLOP: Duration = Duration::from_secs(60);

/// Cleanup on delete. Three phases:
///
///   1. DrainWorker each pod → scheduler marks them draining,
///      stops dispatching new work. In-flight builds continue.
///   2. Scale STS to 0 → K8s sends SIGTERM to each pod. The
///      worker's SIGTERM handler does `acquire_many` on its
///      build semaphore → blocks until in-flight builds finish
///      → exits 0. terminationGracePeriodSeconds=7200 gives it
///      time.
///   3. Wait for replicas=0. THEN return → finalizer removed →
///      ownerReference GC deletes the StatefulSet + Service.
///
/// Why DrainWorker FIRST (not scale-to-0 then drain): with 3
/// replicas, scaling to 0 terminates pods ONE AT A TIME (STS
/// podManagementPolicy default is OrderedReady). Pod-2 gets
/// SIGTERM, starts draining; pods 0,1 are STILL SERVING. If
/// the scheduler doesn't know they're draining, it dispatches
/// new work to them — exactly what we want to prevent. Mark
/// ALL as draining up front, THEN let K8s terminate.
///
/// All best-effort. Scheduler down → skip DrainWorker, proceed
/// to scale-0 → SIGTERM still drains in-flight (worker's own
/// logic, doesn't need the scheduler). We just lose the "stop
/// accepting NEW work early" optimization for pods 0,1.
async fn cleanup(wp: Arc<WorkerPool>, ctx: &Ctx) -> Result<Action> {
    let ns = wp.namespace().expect("checked in reconcile()");
    let name = wp.name_any();

    // Ephemeral mode: no STS to scale to 0, no long-lived workers
    // to DrainWorker. Jobs complete on their own (one build each);
    // in-flight Jobs finish naturally. ownerRef GC deletes them
    // once the finalizer is removed. We return immediately —
    // the finalizer is removed, K8s GC handles the rest.
    //
    // NOT waiting for in-flight Jobs: a running ephemeral build
    // will complete regardless (worker doesn't know the WorkerPool
    // is being deleted; it finishes its one build and exits).
    // Waiting would just delay the CR deletion. If an operator
    // wants to interrupt in-flight ephemeral builds, they can
    // `kubectl delete jobs -l rio.build/pool=X` separately.
    if wp.spec.ephemeral {
        info!(workerpool = %name, "cleanup: ephemeral pool; ownerRef GC handles Jobs");
        return Ok(Action::await_change());
    }

    let sts_name = format!("{name}-workers");
    info!(workerpool = %name, "cleanup: starting drain");

    // ---- Phase 1: DrainWorker each pod ----
    // List pods by label. The pod's NAME is its worker_id (set
    // via RIO_WORKER_ID=$(POD_NAME) downward API in build_pod_spec).
    let pods_api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let pods = pods_api
        .list(&kube::api::ListParams::default().labels(&format!("rio.build/pool={name}")))
        .await?;

    // Best-effort DrainWorker per pod. Balanced client routes to
    // the leader; if the scheduler is down, each RPC fails and we
    // log+continue (SIGTERM still drains in-flight). If the leader
    // comes back mid-loop, later pods succeed.
    let mut admin = ctx.admin.clone();
    for pod in &pods.items {
        let Some(worker_id) = &pod.metadata.name else {
            continue;
        };
        // force=false: in-flight builds complete. The scheduler
        // just stops dispatching NEW work. SIGTERM (phase 2) is
        // what triggers the worker to actually exit once drained.
        match admin
            .drain_worker(rio_proto::types::DrainWorkerRequest {
                worker_id: worker_id.clone(),
                force: false,
            })
            .await
        {
            Ok(resp) => {
                let r = resp.into_inner();
                debug!(worker = %worker_id, running = r.running_builds, "DrainWorker OK");
            }
            Err(e) => {
                // One pod's drain failed --- log and continue.
                // SIGTERM still drains it (just doesn't prevent
                // the scheduler from sending it one more assignment
                // in the gap).
                warn!(worker = %worker_id, error = %e, "DrainWorker failed (continuing)");
            }
        }
    }

    // ---- Phase 2: scale StatefulSet to 0 ----
    // JSON merge patch on spec.replicas. NOT server-side apply:
    // we used SSA with fieldManager=rio-controller to OWN the
    // whole spec at apply time, but here we're in cleanup — the
    // CRD is being deleted, no more reconcile conflicts possible.
    // A simple merge patch is less ceremony (no apiVersion/kind
    // envelope, no fieldManager).
    //
    // 404 tolerance: the STS might already be gone (operator
    // manually deleted, or ownerRef GC ran early somehow). That's
    // fine — skip to await_change, finalizer removed, done.
    let sts_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ns);
    let scale_patch = serde_json::json!({ "spec": { "replicas": 0 } });
    match sts_api
        .patch(
            &sts_name,
            &PatchParams::default(),
            &Patch::Merge(&scale_patch),
        )
        .await
    {
        Ok(_) => debug!(statefulset = %sts_name, "scaled to 0"),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            info!(statefulset = %sts_name, "already gone; cleanup done");
            return Ok(Action::await_change());
        }
        Err(e) => return Err(e.into()),
    }

    // ---- Phase 3: wait for replicas=0 ----
    // Bounded poll. NOT a watch: a watch stream on StatefulSet
    // here would fight with the Controller's `.owns()` watch
    // (kube-runtime dedupes watches by Api<T>, two watches on the
    // same type from the same client can interfere). Polling is
    // simpler and the event rate is low (one transition per pod
    // termination, 5s poll is fine).
    //
    // `replicas` field, not `ready_replicas`: replicas counts
    // pods that exist (any state); ready_replicas counts pods
    // passing readiness. A terminating pod is NOT ready (it
    // fails readiness immediately) but still exists until
    // exit+grace. We want "all pods GONE" not "all pods not-
    // ready" — otherwise we'd remove the finalizer while pods
    // are still running builds.
    //
    // Derived from the spec's grace period (+ 60s slop) instead of
    // a hardcoded 2h: a cluster with 90s builds and
    // terminationGracePeriodSeconds=180 shouldn't block WorkerPool
    // delete for 2h on a stuck/never-Ready pod (vm-lifecycle-autoscale-k3s
    // v24/v25: autoscaler-spawned pod-1 never went Ready; STS
    // sequential termination stalls on it).
    let grace = wp
        .spec
        .termination_grace_period_seconds
        .unwrap_or(DEFAULT_TERMINATION_GRACE);
    let drain_max_wait = Duration::from_secs(grace.max(0) as u64) + DRAIN_WAIT_SLOP;
    let deadline = tokio::time::Instant::now() + drain_max_wait;
    loop {
        match sts_api.get_opt(&sts_name).await? {
            Some(sts) => {
                let replicas = sts.status.map(|s| s.replicas).unwrap_or(0);
                if replicas == 0 {
                    info!(workerpool = %name, "drain complete (replicas=0)");
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    // Grace expired. Pods are STILL running —
                    // either a build is stuck or the kubelet is
                    // dead. We can't do anything more from here.
                    // Remove the finalizer; ownerRef GC will
                    // SIGKILL (kubelet's `deletion_grace_period`
                    // handling) eventually. Operator sees the
                    // stuck pods in `kubectl get pods`.
                    warn!(
                        workerpool = %name,
                        remaining = replicas,
                        timeout = ?drain_max_wait,
                        "drain timeout; proceeding (ownerRef GC will force-delete)"
                    );
                    break;
                }
                debug!(workerpool = %name, remaining = replicas, "waiting for drain");
                tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
            }
            None => {
                // STS deleted out from under us. Unusual (we
                // own it via ownerRef; GC shouldn't run until
                // the finalizer is removed) but handle it.
                info!(workerpool = %name, "STS disappeared mid-wait; done");
                break;
            }
        }
    }

    // ownerReference GC deletes the STS + Service once the
    // finalizer is removed (which happens when we return Ok).
    // No explicit delete needed — it'd race with GC anyway.
    Ok(Action::await_change())
}

/// Requeue policy on error. Transient (Kube, Scheduler) → short
/// backoff. InvalidSpec → longer (operator needs to fix it;
/// retrying fast is noise).
pub fn error_policy(_wp: Arc<WorkerPool>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    metrics::counter!("rio_controller_reconcile_errors_total",
        "reconciler" => "workerpool", "error_kind" => error_kind(err))
    .increment(1);

    match err {
        Error::InvalidSpec(msg) => {
            // Operator error. Requeue slow — they need to edit
            // the CRD. The log is their signal.
            warn!(error = %msg, "invalid WorkerPool spec; fix the CRD");
            Action::requeue(Duration::from_secs(300))
        }
        _ => {
            // Transient (apiserver hiccup, scheduler restarting).
            // Short backoff, retry. warn! not debug! — a 30s
            // silent retry loop is invisible at INFO and cost
            // us ~10min of VM debugging once.
            warn!(error = %err, "reconcile failed; retrying");
            Action::requeue(Duration::from_secs(30))
        }
    }
}
