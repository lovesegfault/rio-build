//! WorkerPool reconciler: one StatefulSet of rio-worker pods.
//!
//! Reconcile flow:
//! 1. Ensure headless Service exists (StatefulSet needs one for
//!    stable pod DNS; workers don't actually serve anything, but
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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EmptyDirVolumeSource, EnvVar, EnvVarSource,
    HTTPGetAction, HostPathVolumeSource, ObjectFieldSelector, Pod, PodSpec, PodTemplateSpec, Probe,
    SecurityContext, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use kube::{CustomResourceExt, Resource, ResourceExt};
use tracing::{debug, info, warn};

use crate::crds::workerpool::WorkerPool;
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::Ctx;

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
            &svc.metadata.name.clone().expect("we set it"),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&svc),
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
    let initial_replicas = if existing.is_none() {
        Some(wp.spec.replicas.min)
    } else {
        None
    };
    // For status.desired_replicas: read what's ACTUALLY on the STS
    // (autoscaler's last decision). Falls back to min on first
    // create. Without this, kubectl's "Desired" column showed min
    // forever regardless of autoscaler activity.
    let current_replicas = existing
        .as_ref()
        .and_then(|s| s.spec.as_ref())
        .and_then(|s| s.replicas)
        .unwrap_or(wp.spec.replicas.min);

    let sts = build_statefulset(
        &wp,
        oref,
        &ctx.scheduler_addr,
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
    // apply patch". (Same pattern as build.rs patch_status —
    // that one learned this the hard way; this one forgot.)
    //
    // Partial status: we patch ONLY replicas/ready/desired. The
    // autoscaler owns lastScaleTime + conditions via a separate
    // SSA field-manager ("rio-controller-autoscaler-status").
    // SSA merges field ownership — our patch here doesn't touch
    // lastScaleTime, so the autoscaler's value persists across
    // our reconciles. Previously we set both to empty on EVERY
    // reconcile, clobbering anything the autoscaler wrote.
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);
    let ar = WorkerPool::api_resource();
    let status_patch = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "replicas": sts_status.replicas,
            "readyReplicas": sts_status.ready_replicas.unwrap_or(0),
            // What the autoscaler set on STS.spec.replicas (or min
            // on first create). Previously hardcoded to min —
            // kubectl's "Desired" column was always wrong.
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

/// Max wait for replicas=0 during cleanup. terminationGrace is
/// 7200s (2h — long builds); +60s slop for the kubelet/STS
/// controller to observe the pod termination and update status.
/// After this, we give up and let ownerReference GC SIGKILL.
const DRAIN_MAX_WAIT: Duration = Duration::from_secs(7200 + 60);

/// Cleanup on delete. Three phases:
///
///   1. DrainWorker each pod → scheduler marks them draining,
///      stops dispatching new work. In-flight builds continue.
///   2. Scale STS to 0 → K8s sends SIGTERM to each pod. The
///      worker's D3 SIGTERM handler does `acquire_many` on its
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
    let sts_name = format!("{name}-workers");
    info!(workerpool = %name, "cleanup: starting drain");

    // ---- Phase 1: DrainWorker each pod ----
    // List pods by label. The pod's NAME is its worker_id (set
    // via RIO_WORKER_ID=$(POD_NAME) downward API in build_pod_spec).
    let pods_api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let pods = pods_api
        .list(&kube::api::ListParams::default().labels(&format!("rio.build/pool={name}")))
        .await?;

    // Best-effort scheduler connect. Failure → skip all drains,
    // proceed to scale-0. One connect for the batch (not per
    // pod — if pod 0's drain fails on connect, pod 1's would too).
    match rio_proto::client::connect_admin(&ctx.scheduler_addr).await {
        Ok(mut admin) => {
            for pod in &pods.items {
                let Some(worker_id) = &pod.metadata.name else {
                    continue;
                };
                // force=false: in-flight builds complete. The
                // scheduler just stops dispatching NEW work.
                // SIGTERM (phase 2) is what triggers the worker
                // to actually exit once drained.
                match admin
                    .drain_worker(rio_proto::types::DrainWorkerRequest {
                        worker_id: worker_id.clone(),
                        force: false,
                    })
                    .await
                {
                    Ok(resp) => {
                        let r = resp.into_inner();
                        debug!(
                            worker = %worker_id,
                            running = r.running_builds,
                            "DrainWorker OK"
                        );
                    }
                    Err(e) => {
                        // One pod's drain failed — log and
                        // continue with the rest. SIGTERM still
                        // drains it (just doesn't prevent the
                        // scheduler from sending it one more
                        // assignment in the gap).
                        warn!(worker = %worker_id, error = %e, "DrainWorker failed (continuing)");
                    }
                }
            }
        }
        Err(e) => {
            warn!(
                workerpool = %name,
                error = %e,
                "scheduler unreachable; skipping DrainWorker (SIGTERM still drains in-flight)"
            );
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
    let deadline = tokio::time::Instant::now() + DRAIN_MAX_WAIT;
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
                        timeout = ?DRAIN_MAX_WAIT,
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

// =============================================================================
// Object builders (pure: WorkerPool → K8s objects)
// =============================================================================

/// Labels applied to the StatefulSet, Service, and pods.
///
/// `rio.build/pool`: the WorkerPool name. The finalizer's
/// cleanup() lists pods by this label to DrainWorker each one.
/// The autoscaler could use it too but patches the StatefulSet
/// directly instead.
///
/// `app.kubernetes.io/*`: standard K8s recommended labels.
/// Dashboards and `kubectl get pods -l` queries expect these.
fn labels(wp: &WorkerPool) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("rio.build/pool".into(), wp.name_any()),
        ("app.kubernetes.io/name".into(), "rio-worker".into()),
        ("app.kubernetes.io/component".into(), "worker".into()),
        ("app.kubernetes.io/part-of".into(), "rio-build".into()),
    ])
}

/// Headless Service. `clusterIP: None` + no ports (workers don't
/// serve). Exists purely to satisfy StatefulSet's `serviceName`.
fn build_headless_service(wp: &WorkerPool, oref: OwnerReference) -> Service {
    let name = format!("{}-workers", wp.name_any());
    let labels = labels(wp);
    Service {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            // clusterIP: None = headless. Pod DNS records are
            // created, no load balancer VIP.
            cluster_ip: Some("None".into()),
            selector: Some(labels),
            // StatefulSet wants a port even for headless. Metrics
            // port is as good as any — it's exposed on every pod.
            ports: Some(vec![ServicePort {
                name: Some("metrics".into()),
                port: 9093,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The StatefulSet. All the worker pod spec details live here —
/// security context, volumes, env, probes, termination grace.
///
/// `replicas`: `Some(n)` to claim SSA ownership (first create),
/// `None` to omit from the patch (subsequent reconciles — lets
/// the autoscaler's field manager own it).
///
/// Returns `Err(InvalidSpec)` if `fuse_cache_size` doesn't parse
/// as a K8s Quantity. That's the one operator-supplied string
/// we need to validate.
fn build_statefulset(
    wp: &WorkerPool,
    oref: OwnerReference,
    scheduler_addr: &str,
    store_addr: &str,
    replicas: Option<i32>,
) -> Result<StatefulSet> {
    let name = format!("{}-workers", wp.name_any());
    let labels = labels(wp);

    // Parse fuseCacheSize. K8s Quantity accepts "50Gi", "100G",
    // "107374182400" etc. We store the raw Quantity for the
    // emptyDir sizeLimit, and also extract GB for the worker's
    // RIO_FUSE_CACHE_SIZE_GB env var (it wants an integer).
    //
    // Quantity parsing is just "is it a valid string" — the
    // k8s-openapi type is a String wrapper. Real validation
    // happens at the apiserver. But we need the numeric value
    // for the env var, so parse it ourselves.
    let cache_quantity = Quantity(wp.spec.fuse_cache_size.clone());
    let cache_gb = parse_quantity_to_gb(&wp.spec.fuse_cache_size)?;

    Ok(StatefulSet {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            // serviceName MUST match the headless Service above.
            // StatefulSet controller uses it for pod DNS.
            service_name: Some(name),
            // None on subsequent reconciles → field omitted from
            // SSA patch → autoscaler's ownership preserved. See
            // apply() for the existence check.
            replicas,
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(build_pod_spec(
                    wp,
                    scheduler_addr,
                    store_addr,
                    cache_gb,
                    cache_quantity,
                )),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// The pod spec. Separate fn because it's the bulk of the
/// StatefulSet complexity and it's useful to test in isolation.
fn build_pod_spec(
    wp: &WorkerPool,
    scheduler_addr: &str,
    store_addr: &str,
    cache_gb: u64,
    cache_quantity: Quantity,
) -> PodSpec {
    // cgroup handling: we do NOT hostPath-mount /sys/fs/cgroup.
    // containerd cgroup-namespaces the container by default:
    // /proc/self/cgroup shows 0::/ and /sys/fs/cgroup is the
    // NAMESPACED view (container's cgroup appears as root).
    // With privileged, that namespaced mount is RW (runc default).
    //
    // If we hostPath-mounted the HOST's /sys/fs/cgroup over it:
    // /proc/self/cgroup STILL shows 0::/ (cgroupns is independent
    // of mount ns), but /sys/fs/cgroup now shows the HOST root.
    // The worker's ns-root-detection would mkdir+move into
    // /sys/fs/cgroup/leaf/ on the HOST — clobbering host systemd.
    //
    // Instead: worker's delegated_root() detects the ns-root case
    // and moves itself into a /leaf/ sub-cgroup WITHIN the
    // namespaced view. Safe (writes stay in the container's
    // cgroup subtree). privileged → rw → writes succeed.

    PodSpec {
        containers: vec![build_container(wp, scheduler_addr, store_addr, cache_gb)],
        // hostNetwork from spec. None → K8s default (false).
        // Some(false) → explicit false (same effect). Some(true)
        // → pod shares node netns. `filter`: PodSpec field is
        // None (not Some(false)) when unset — less diff noise
        // in `kubectl get -o yaml`.
        host_network: wp.spec.host_network.filter(|&h| h),

        volumes: Some(vec![
            // /dev/fuse character device. hostPath not because
            // we need a file from the host, but because it's a
            // DEVICE node that must be the same major/minor as
            // the host's. CharDevice type makes K8s verify it's
            // actually a char device (catches a node without
            // FUSE compiled in).
            Volume {
                name: "dev-fuse".into(),
                host_path: Some(HostPathVolumeSource {
                    path: "/dev/fuse".into(),
                    type_: Some("CharDevice".into()),
                }),
                ..Default::default()
            },
            // FUSE cache. emptyDir = local ephemeral storage,
            // wiped on pod restart. sizeLimit enforced by the
            // kubelet (evicts pod if exceeded). For persistent
            // cache across restarts, operators can override
            // with a PVC via kustomize — we don't model that
            // in the CRD (too many backend-specific knobs).
            Volume {
                name: "fuse-cache".into(),
                empty_dir: Some(EmptyDirVolumeSource {
                    size_limit: Some(cache_quantity),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // Overlay upperdir/workdir. MUST be a real filesystem
            // (ext4/xfs via emptyDir on the node's disk), NOT the
            // container's root — containerd's root is overlayfs,
            // and overlayfs-as-upperdir can't create trusted.*
            // xattrs → mount fails with EINVAL. emptyDir gives us
            // the kubelet's local disk (/var/lib/kubelet/pods/...),
            // which is ext4/xfs on every sane node. No sizeLimit:
            // overlays are per-build, cleaned on Drop; unbounded
            // growth = a leak bug that sizeLimit would mask with
            // a pod eviction (worse debugging).
            Volume {
                name: "overlays".into(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            },
        ]),

        // Security context. SYS_ADMIN for mount (FUSE, overlayfs,
        // CLONE_NEWNS in the executor's pre_exec). SYS_CHROOT
        // for nix-daemon's sandbox pivot_root. NOT privileged —
        // that disables seccomp entirely. Capabilities are
        // granular.
        //
        // This is the PRODUCTION default. The vm-phase3a test
        // (H2) overrides with privileged: true because
        // containerd's device cgroup doesn't include /dev/fuse
        // by default (needs smarter-device-manager or similar
        // in prod; privileged is the test shortcut).
        //
        // Container-level not pod-level: PodSecurityContext
        // doesn't have capabilities; Container's does.
        // (security_context on PodSpec is for UID/fsGroup etc.)

        // automountServiceAccountToken: workers talk to the
        // scheduler via gRPC, not the K8s API. No token needed.
        // Defense-in-depth: a compromised worker can't use the
        // pod's SA to read secrets.
        automount_service_account_token: Some(false),

        // 2 hours. nix builds can legitimately take that long
        // (LLVM from cold ccache, NixOS closures). SIGTERM →
        // D3's drain sequence (DrainWorker + wait for in-flight).
        // After grace period: SIGKILL, builds lost.
        termination_grace_period_seconds: Some(7200),

        node_selector: wp.spec.node_selector.clone(),
        tolerations: wp.spec.tolerations.clone(),

        ..Default::default()
    }
}

/// The worker container.
fn build_container(
    wp: &WorkerPool,
    scheduler_addr: &str,
    store_addr: &str,
    cache_gb: u64,
) -> Container {
    Container {
        name: "worker".into(),
        image: Some(wp.spec.image.clone()),
        // imagePullPolicy: K8s defaults to Always for :latest,
        // IfNotPresent otherwise. Airgap clusters need an explicit
        // IfNotPresent/Never — kustomize can't patch the generated
        // STS (only the CRD), so the spec has this knob.
        image_pull_policy: wp.spec.image_pull_policy.clone(),
        env: Some(vec![
            env("RIO_SCHEDULER_ADDR", scheduler_addr),
            // From ctx.store_addr — NOT derived from scheduler_addr.
            // The kustomize base uses different Service names
            // (rio-scheduler vs rio-store); a port-replace would
            // yield "rio-scheduler:9002" which doesn't exist.
            env("RIO_STORE_ADDR", store_addr),
            env("RIO_MAX_BUILDS", &wp.spec.max_concurrent_builds.to_string()),
            env("RIO_FUSE_CACHE_SIZE_GB", &cache_gb.to_string()),
            env("RIO_FUSE_MOUNT_POINT", "/var/rio/fuse-store"),
            env("RIO_FUSE_CACHE_DIR", "/var/rio/cache"),
            env("RIO_OVERLAY_BASE_DIR", "/var/rio/overlays"),
            env("RIO_LOG_FORMAT", "json"),
            // size_class: empty → no env var (figment: absent
            // field uses default). if_let_some keeps the Vec
            // building linear without extend() gymnastics.
            // Actually — just always set it. Empty string is
            // the Config default anyway.
            env("RIO_SIZE_CLASS", &wp.spec.size_class),
            // systems + features: comma-sep strings; worker's
            // comma_vec deserialize helper splits them. systems
            // is CEL-validated non-empty (crds/workerpool.rs:97),
            // so join() always produces at least one element.
            // features may be empty (empty string → comma_vec
            // filters to empty vec, same as unset).
            //
            // Previously MISSING — the CRD's features field was
            // silently dropped, and heartbeat hardcoded features
            // to Vec::new(). Any derivation with
            // requiredSystemFeatures never dispatched.
            env("RIO_SYSTEMS", &wp.spec.systems.join(",")),
            env("RIO_FEATURES", &wp.spec.features.join(",")),
            // RIO_WORKER_ID from pod name via downward API.
            // StatefulSet pods are `<sts-name>-<ordinal>`, e.g.,
            // `default-workers-0` — unique, stable. Two pools
            // can't collide (different sts names).
            env_from_field("RIO_WORKER_ID", "metadata.name"),
        ]),

        volume_mounts: Some(vec![
            VolumeMount {
                name: "dev-fuse".into(),
                mount_path: "/dev/fuse".into(),
                ..Default::default()
            },
            VolumeMount {
                name: "fuse-cache".into(),
                mount_path: "/var/rio/cache".into(),
                ..Default::default()
            },
            VolumeMount {
                name: "overlays".into(),
                mount_path: "/var/rio/overlays".into(),
                ..Default::default()
            },
        ]),

        security_context: Some(SecurityContext {
            // Granular caps are the default. privileged=true
            // overrides — it disables seccomp and grants ALL
            // caps (the capabilities list below becomes
            // irrelevant but harmless). Set BOTH: if an
            // operator flips privileged back to false later,
            // the caps are still there and the pod keeps
            // working.
            privileged: wp.spec.privileged.filter(|&p| p),
            capabilities: Some(Capabilities {
                add: Some(vec!["SYS_ADMIN".into(), "SYS_CHROOT".into()]),
                ..Default::default()
            }),
            ..Default::default()
        }),

        resources: wp.spec.resources.clone(),

        ports: Some(vec![
            ContainerPort {
                name: Some("metrics".into()),
                container_port: 9093,
                ..Default::default()
            },
            ContainerPort {
                name: Some("health".into()),
                container_port: 9193,
                ..Default::default()
            },
        ]),

        // Probes. HTTP /healthz + /readyz per D4.
        //
        // Liveness: /healthz, always 200. If it fails, the
        // process is dead (not responding) → restart.
        //
        // Readiness: /readyz, 200 after first heartbeat accepted.
        // NOT_READY → not counted in Service endpoints (though
        // workers aren't load-balanced anyway — mainly for
        // `kubectl get pods` visibility + rollout gating).
        //
        // Startup: /healthz with long failureThreshold. The
        // worker needs time to mount FUSE + do cgroup setup.
        // 120s total (30 × 4s) per controller.md. Until startup
        // passes, liveness/readiness are suppressed — prevents
        // restart loops during slow boots.
        liveness_probe: Some(http_probe("/healthz", 9193, 10, 3)),
        readiness_probe: Some(http_probe("/readyz", 9193, 5, 3)),
        startup_probe: Some(http_probe("/healthz", 9193, 4, 30)),

        ..Default::default()
    }
}

// ----- small builder helpers --------------------------------------------------

fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

/// Downward API: env var from pod metadata field.
fn env_from_field(name: &str, field_path: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: field_path.into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn http_probe(path: &str, port: i32, period: i32, failure_threshold: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::Int(port),
            ..Default::default()
        }),
        period_seconds: Some(period),
        failure_threshold: Some(failure_threshold),
        ..Default::default()
    }
}

/// Parse a K8s Quantity string to gigabytes (integer, rounded DOWN).
///
/// K8s Quantity is complex (binary vs decimal suffixes, fixed-
/// point). We handle the common cases operators actually write:
/// "100Gi" (binary), "100G" (decimal), "107374182400" (raw bytes).
///
/// Rounding DOWN: the worker's cache LRU uses this as a ceiling.
/// Better to under-report (cache a bit below emptyDir limit) than
/// over (kubelet evicts the pod when cache fills to what we
/// THOUGHT was the limit but is actually over).
///
/// Doesn't handle milli ("100m") or exponential ("1e9") —
/// nobody writes cache sizes that way. Returns InvalidSpec with
/// a helpful message.
fn parse_quantity_to_gb(q: &str) -> Result<u64> {
    let q = q.trim();
    // Suffixes in DECREASING length order — "Gi" before "G" so
    // strip_suffix doesn't match "G" on "100Gi" leaving "100i".
    // Each tuple: (suffix, bytes-per-unit).
    const SUFFIXES: &[(&str, u64)] = &[
        ("Gi", 1024 * 1024 * 1024),
        ("Mi", 1024 * 1024),
        ("Ki", 1024),
        ("Ti", 1024 * 1024 * 1024 * 1024),
        ("G", 1_000_000_000),
        ("M", 1_000_000),
        ("K", 1_000),
        ("T", 1_000_000_000_000),
    ];

    for (suffix, mult) in SUFFIXES {
        if let Some(num) = q.strip_suffix(suffix) {
            let n: u64 = num.trim().parse().map_err(|_| {
                Error::InvalidSpec(format!(
                    "fuseCacheSize {q:?}: {num:?} before suffix is not a number"
                ))
            })?;
            let bytes = n
                .checked_mul(*mult)
                .ok_or_else(|| Error::InvalidSpec(format!("fuseCacheSize {q:?}: overflows u64")))?;
            // Integer division rounds down.
            return Ok(bytes / (1024 * 1024 * 1024));
        }
    }

    // No suffix → raw bytes.
    let bytes: u64 = q.parse().map_err(|_| {
        Error::InvalidSpec(format!(
            "fuseCacheSize {q:?}: not a recognized quantity \
             (expected e.g. '100Gi', '50G', or raw bytes)"
        ))
    })?;
    Ok(bytes / (1024 * 1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::workerpool::{Autoscaling, Replicas, WorkerPoolSpec};

    /// Construct a minimal WorkerPool for builder tests. No K8s
    /// interaction — pure struct-to-struct.
    fn test_wp() -> WorkerPool {
        let spec = WorkerPoolSpec {
            replicas: Replicas { min: 2, max: 10 },
            autoscaling: Autoscaling {
                metric: "queueDepth".into(),
                target_value: 5,
            },
            resources: None,
            max_concurrent_builds: 4,
            fuse_cache_size: "50Gi".into(),
            features: vec!["kvm".into()],
            systems: vec!["x86_64-linux".into()],
            size_class: "small".into(),
            image: "rio-worker:test".into(),
            image_pull_policy: None,
            node_selector: None,
            tolerations: None,
            privileged: None,
            host_network: None,
        };
        let mut wp = WorkerPool::new("test-pool", spec);
        // UID + namespace: controller_owner_ref needs these. In
        // real usage the apiserver sets them; tests fake them.
        wp.metadata.uid = Some("test-uid-123".into());
        wp.metadata.namespace = Some("rio".into());
        wp
    }

    /// Shorthand for tests: builds with default scheduler/store
    /// addrs and replicas=Some(min). Use `build_statefulset`
    /// directly for tests that care about those params.
    fn test_sts(wp: &WorkerPool) -> StatefulSet {
        build_statefulset(
            wp,
            wp.controller_owner_ref(&()).unwrap(),
            "sched:9001",
            "store:9002",
            Some(wp.spec.replicas.min),
        )
        .unwrap()
    }

    #[test]
    fn statefulset_has_owner_reference() {
        let wp = test_wp();
        let sts = test_sts(&wp);

        let orefs = sts.metadata.owner_references.expect("ownerRef set");
        assert_eq!(orefs.len(), 1);
        assert_eq!(orefs[0].kind, "WorkerPool");
        assert_eq!(orefs[0].name, "test-pool");
        assert_eq!(orefs[0].controller, Some(true), "controller=true for GC");
    }

    #[test]
    fn statefulset_security_context() {
        let wp = test_wp();
        let sts = test_sts(&wp);

        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
        let caps = container
            .security_context
            .as_ref()
            .unwrap()
            .capabilities
            .as_ref()
            .unwrap();
        let add = caps.add.as_ref().unwrap();
        assert!(add.contains(&"SYS_ADMIN".to_string()));
        assert!(add.contains(&"SYS_CHROOT".to_string()));
        // NOT privileged — capabilities are granular, privileged
        // disables seccomp. If someone adds privileged here, this
        // test fails.
        assert_eq!(
            container.security_context.as_ref().unwrap().privileged,
            None
        );
    }

    #[test]
    fn statefulset_dev_fuse_volume() {
        let wp = test_wp();
        let sts = test_sts(&wp);

        let pod = sts.spec.unwrap().template.spec.unwrap();
        let fuse_vol = pod
            .volumes
            .unwrap()
            .into_iter()
            .find(|v| v.name == "dev-fuse")
            .expect("/dev/fuse volume");
        let hp = fuse_vol.host_path.expect("hostPath");
        assert_eq!(hp.path, "/dev/fuse");
        assert_eq!(
            hp.type_,
            Some("CharDevice".into()),
            "CharDevice type makes K8s verify it's a char device (catches no-FUSE nodes)"
        );
    }

    #[test]
    fn statefulset_overlays_volume_mounted() {
        // RIO_OVERLAY_BASE_DIR points to /var/rio/overlays. If
        // there's no volume mount for it, it lands on the
        // container's root filesystem — which is overlayfs.
        // Overlayfs-as-upperdir can't create trusted.* xattrs →
        // every overlay mount fails with EINVAL. Regression guard.
        let wp = test_wp();
        let sts = test_sts(&wp);
        let pod = sts.spec.unwrap().template.spec.unwrap();

        let vol = pod
            .volumes
            .unwrap()
            .into_iter()
            .find(|v| v.name == "overlays")
            .expect("overlays volume must exist");
        assert!(
            vol.empty_dir.is_some(),
            "emptyDir → node's real disk (ext4/xfs with trusted.* xattr support)"
        );

        let mount = pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "overlays")
            .expect("overlays volumeMount");
        assert_eq!(mount.mount_path, "/var/rio/overlays");
    }

    #[test]
    fn statefulset_termination_grace() {
        let wp = test_wp();
        let sts = test_sts(&wp);
        let pod = sts.spec.unwrap().template.spec.unwrap();
        assert_eq!(
            pod.termination_grace_period_seconds,
            Some(7200),
            "2h for long nix builds (D3 drain sequence runs within this)"
        );
        assert_eq!(
            pod.automount_service_account_token,
            Some(false),
            "workers use gRPC, not K8s API — no SA token needed"
        );
    }

    #[test]
    fn statefulset_env_vars() {
        let wp = test_wp();
        let sts = test_sts(&wp);

        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
        let envs: BTreeMap<String, String> = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|e| e.value.clone().map(|v| (e.name.clone(), v)))
            .collect();

        assert_eq!(envs.get("RIO_SCHEDULER_ADDR"), Some(&"sched:9001".into()));
        assert_eq!(
            envs.get("RIO_STORE_ADDR"),
            Some(&"store:9002".into()),
            "from ctx.store_addr (NOT derived from scheduler — \
             different hostnames in kustomize base)"
        );
        assert_eq!(envs.get("RIO_MAX_BUILDS"), Some(&"4".into()));
        assert_eq!(envs.get("RIO_SIZE_CLASS"), Some(&"small".into()));
        assert_eq!(
            envs.get("RIO_FUSE_CACHE_SIZE_GB"),
            Some(&"50".into()),
            "50Gi parsed to 50 GB (binary → binary, integer)"
        );
        // systems + features: comma-sep strings. fixture wp has
        // systems=["x86_64-linux"], features=["kvm"]. Previously
        // these were SILENTLY DROPPED — CRD defined them, reconciler
        // never passed them, worker hardcoded features=Vec::new().
        assert_eq!(
            envs.get("RIO_SYSTEMS"),
            Some(&"x86_64-linux".into()),
            "systems comma-joined → worker's comma_vec deserialize"
        );
        assert_eq!(
            envs.get("RIO_FEATURES"),
            Some(&"kvm".into()),
            "features comma-joined (was silently dropped before)"
        );

        // RIO_WORKER_ID uses fieldRef, not value — check separately.
        let worker_id = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "RIO_WORKER_ID")
            .unwrap();
        assert_eq!(worker_id.value, None, "not a literal value");
        assert_eq!(
            worker_id
                .value_from
                .as_ref()
                .unwrap()
                .field_ref
                .as_ref()
                .unwrap()
                .field_path,
            "metadata.name",
            "downward API: pod name (StatefulSet ordinal, unique)"
        );
    }

    #[test]
    fn statefulset_replicas_starts_at_min() {
        let wp = test_wp();
        let sts = test_sts(&wp);
        assert_eq!(
            sts.spec.unwrap().replicas,
            Some(2),
            "initial create: set to spec.replicas.min"
        );
    }

    /// replicas=None → field omitted from the SSA patch → the
    /// autoscaler's field-manager ownership is preserved.
    /// Without this, every reconcile would revert the autoscaler's
    /// scaling decision to min (SSA .force() takes ownership of
    /// every field in the patch).
    ///
    /// k8s-openapi's custom Serialize impl skips Option::None
    /// fields, so None → field absent → SSA leaves it alone.
    #[test]
    fn statefulset_replicas_omitted_when_none() {
        let wp = test_wp();
        let sts = build_statefulset(
            &wp,
            wp.controller_owner_ref(&()).unwrap(),
            "sched:9001",
            "store:9002",
            None,
        )
        .unwrap();

        assert_eq!(
            sts.spec.as_ref().unwrap().replicas,
            None,
            "subsequent reconciles: replicas=None → autoscaler owns it"
        );

        // And the crucial part: serialized JSON doesn't contain
        // the field. SSA semantics: absent field = "I don't
        // manage this." Verifies k8s-openapi's skip-None-on-
        // serialize behavior hasn't regressed.
        let json = serde_json::to_string(&sts).unwrap();
        assert!(
            !json.contains("\"replicas\""),
            "replicas must be absent from serialized JSON for SSA to \
             preserve autoscaler ownership. Found in: {json}"
        );
    }

    #[test]
    fn statefulset_image_pull_policy_passthrough() {
        // None stays None — K8s applies its tag-based default
        // (IfNotPresent for non-:latest, Always for :latest).
        let wp = test_wp();
        let sts = test_sts(&wp);
        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(container.image_pull_policy, None);

        // Explicit value passes through. Airgap k3s/kind need
        // IfNotPresent or Never to use ctr-imported images.
        let mut wp = test_wp();
        wp.spec.image_pull_policy = Some("IfNotPresent".into());
        let sts = test_sts(&wp);
        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(container.image_pull_policy.as_deref(), Some("IfNotPresent"));
    }

    // ----- quantity parsing -----

    #[test]
    fn quantity_binary_suffix() {
        assert_eq!(parse_quantity_to_gb("50Gi").unwrap(), 50);
        assert_eq!(parse_quantity_to_gb("100Gi").unwrap(), 100);
        assert_eq!(
            parse_quantity_to_gb("1Ti").unwrap(),
            1024,
            "1 TiB = 1024 GiB"
        );
        // Mi rounds DOWN to GB. 2048 MiB = 2 GiB exactly.
        assert_eq!(parse_quantity_to_gb("2048Mi").unwrap(), 2);
        // 1500 MiB = 1.46 GiB → rounds down to 1.
        assert_eq!(
            parse_quantity_to_gb("1500Mi").unwrap(),
            1,
            "rounds DOWN (cache limit: better under than over → kubelet eviction)"
        );
    }

    #[test]
    fn quantity_decimal_suffix() {
        // 100G = 100 * 10^9 bytes = 93.13 GiB → 93.
        assert_eq!(parse_quantity_to_gb("100G").unwrap(), 93);
        // 50G = 46.56 GiB → 46.
        assert_eq!(parse_quantity_to_gb("50G").unwrap(), 46);
    }

    #[test]
    fn quantity_raw_bytes() {
        // 107374182400 = 100 * 1024^3 = 100 GiB exactly.
        assert_eq!(parse_quantity_to_gb("107374182400").unwrap(), 100);
    }

    #[test]
    fn quantity_suffix_order_gi_before_g() {
        // If we matched "G" before "Gi", "100Gi" would strip "G"
        // leaving "100i" which doesn't parse. The SUFFIXES const
        // is ordered to prevent this. If someone reorders it,
        // this test catches it.
        assert_eq!(
            parse_quantity_to_gb("100Gi").unwrap(),
            100,
            "Gi matched, not G"
        );
    }

    #[test]
    fn quantity_invalid_rejected() {
        assert!(matches!(
            parse_quantity_to_gb("not-a-number"),
            Err(Error::InvalidSpec(_))
        ));
        assert!(matches!(
            parse_quantity_to_gb("100Pi"),
            Err(Error::InvalidSpec(_))
        ));
        // Empty
        assert!(matches!(
            parse_quantity_to_gb(""),
            Err(Error::InvalidSpec(_))
        ));
    }

    #[test]
    fn quantity_invalidspec_from_statefulset() {
        let mut wp = test_wp();
        wp.spec.fuse_cache_size = "garbage".into();
        let result = build_statefulset(
            &wp,
            wp.controller_owner_ref(&()).unwrap(),
            "sched:9001",
            "store:9002",
            Some(1),
        );
        match result {
            Err(Error::InvalidSpec(msg)) => {
                assert!(msg.contains("fuseCacheSize"));
                assert!(msg.contains("garbage"));
            }
            other => panic!("expected InvalidSpec with helpful message, got {other:?}"),
        }
    }

    // =========================================================
    // Mock-apiserver integration tests
    //
    // These test the WIRING: apply() calls Service PATCH then
    // StatefulSet PATCH then WorkerPool/status PATCH, in that
    // order, with server-side-apply params. The builder tests
    // above cover WHAT gets patched; these cover WHEN/HOW.
    // =========================================================

    use crate::fixtures::{ApiServerVerifier, Scenario, apply_ok_scenarios};

    fn test_ctx(client: kube::Client) -> Arc<Ctx> {
        Arc::new(Ctx {
            client,
            // Unreachable — apply() doesn't touch the scheduler,
            // and cleanup() treats connect failure as best-effort
            // skip. Using an address that fails fast (port 1 is
            // never listened on) vs one that times out.
            scheduler_addr: "http://127.0.0.1:1".into(),
            store_addr: "http://127.0.0.1:1".into(),
        })
    }

    /// apply() hits Service → StatefulSet → WorkerPool/status,
    /// server-side apply all three. Wrong order or missing call
    /// → verifier panics.
    #[tokio::test]
    async fn apply_patches_in_order() {
        let (client, verifier) = ApiServerVerifier::new();
        let ctx = test_ctx(client);
        let wp = Arc::new(test_wp());

        // sts_exists=false: first-create path (STS GET → 404 →
        // initial_replicas=Some(min)).
        let task = verifier.run(apply_ok_scenarios("test-pool", "rio", 2, false));

        // Call apply() directly (not reconcile() — finalizer()
        // would do its own GET + PATCH of metadata.finalizers
        // first, which adds scenarios we don't care about here).
        let action = apply(wp, &ctx).await.expect("apply succeeds");

        // Requeue in 5m — the fallback re-reconcile.
        assert_eq!(action, Action::requeue(Duration::from_secs(300)));

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("verifier consumed all scenarios (right number of calls)")
            .expect("verifier assertions passed");
    }

    /// Server-side apply params in the query string. SSA is
    /// what makes reconcile idempotent — if we accidentally
    /// switch to PUT or merge-patch, this fails.
    #[tokio::test]
    async fn apply_uses_server_side_apply() {
        let (client, verifier) = ApiServerVerifier::new();
        let ctx = test_ctx(client);
        let wp = Arc::new(test_wp());

        // Custom scenarios that assert fieldManager=rio-controller
        // in the query string (SSA puts it there; merge patch
        // doesn't). path_contains is a substring match so
        // embedding the query param works.
        let task = verifier.run(vec![
            Scenario::ok(
                http::Method::PATCH,
                "fieldManager=rio-controller",
                serde_json::json!({"metadata":{"name":"test-pool-workers"}}).to_string(),
            ),
            // GET before STS PATCH (replicas-ownership check).
            // 404 → first-create → replicas set to min.
            Scenario {
                method: http::Method::GET,
                path_contains: "/statefulsets/test-pool-workers",
                status: 404,
                body_json: serde_json::json!({
                    "kind":"Status","apiVersion":"v1",
                    "status":"Failure","reason":"NotFound","code":404,
                })
                .to_string(),
            },
            Scenario::ok(
                http::Method::PATCH,
                "fieldManager=rio-controller",
                serde_json::json!({
                    "metadata":{"name":"test-pool-workers"},
                    "status":{"replicas":0}
                })
                .to_string(),
            ),
            Scenario::ok(
                http::Method::PATCH,
                "workerpools/test-pool/status",
                serde_json::json!({
                    "apiVersion":"rio.build/v1alpha1","kind":"WorkerPool",
                    "metadata":{"name":"test-pool"},
                    "spec":{"replicas":{"min":1,"max":1},"autoscaling":{"metric":"x","targetValue":1},
                        "maxConcurrentBuilds":1,"fuseCacheSize":"1Gi","features":[],
                        "systems":["x"],"sizeClass":"x","image":"x"}
                })
                .to_string(),
            ),
        ]);

        apply(wp, &ctx).await.expect("apply succeeds");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    /// cleanup with STS already gone → 404 → short-circuit.
    /// Proves the "STS not found → done" branch doesn't hang
    /// on the poll loop.
    #[tokio::test]
    async fn cleanup_tolerates_missing_statefulset() {
        let (client, verifier) = ApiServerVerifier::new();
        let ctx = test_ctx(client);
        let wp = Arc::new(test_wp());

        // Phase 1: pod list (empty — no pods to drain). Phase
        // 2: STS scale PATCH → 404. cleanup() short-circuits.
        // No phase 3 GET.
        let task = verifier.run(vec![
            // Pod list. Empty — cleanup skips the DrainWorker
            // loop (scheduler unreachable anyway with port 1).
            Scenario::ok(
                http::Method::GET,
                "/pods?&labelSelector=rio.build",
                serde_json::json!({"apiVersion":"v1","kind":"PodList","items":[]}).to_string(),
            ),
            // STS PATCH → 404. K8s 404 body is a Status object.
            Scenario {
                method: http::Method::PATCH,
                path_contains: "/statefulsets/test-pool-workers",
                status: 404,
                body_json: serde_json::json!({
                    "apiVersion":"v1","kind":"Status","status":"Failure",
                    "reason":"NotFound","code":404,
                    "message":"statefulsets.apps \"test-pool-workers\" not found"
                })
                .to_string(),
            },
        ]);

        let action = cleanup(wp, &ctx).await.expect("cleanup tolerates 404");
        assert_eq!(action, Action::await_change());

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    /// cleanup poll loop: first GET shows replicas=1, second
    /// shows 0 → break. Proves we poll `status.replicas` not
    /// `readyReplicas` (the latter would see 0 immediately on
    /// a terminating pod — wrong).
    #[tokio::test(start_paused = true)]
    async fn cleanup_polls_until_replicas_zero() {
        let (client, verifier) = ApiServerVerifier::new();
        let ctx = test_ctx(client);
        let wp = Arc::new(test_wp());

        // start_paused + tokio::time means the 5s sleep between
        // polls auto-advances. Without it, this test would take
        // 5 real seconds.
        let sts_with = |replicas: i32| {
            serde_json::json!({
                "apiVersion":"apps/v1","kind":"StatefulSet",
                "metadata":{"name":"test-pool-workers"},
                "status":{"replicas": replicas, "readyReplicas": 0}
            })
            .to_string()
        };

        let task = verifier.run(vec![
            Scenario::ok(
                http::Method::GET,
                "/pods?&labelSelector=rio.build",
                serde_json::json!({"apiVersion":"v1","kind":"PodList","items":[]}).to_string(),
            ),
            Scenario::ok(
                http::Method::PATCH,
                "/statefulsets/test-pool-workers",
                sts_with(1),
            ),
            // First poll: replicas=1 (pod still terminating).
            // readyReplicas=0 but we DON'T break — proves we
            // read the right field.
            Scenario::ok(
                http::Method::GET,
                "/statefulsets/test-pool-workers",
                sts_with(1),
            ),
            // Second poll: replicas=0. Break.
            Scenario::ok(
                http::Method::GET,
                "/statefulsets/test-pool-workers",
                sts_with(0),
            ),
        ]);

        let action = cleanup(wp, &ctx).await.expect("cleanup completes");
        assert_eq!(action, Action::await_change());

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }
}
