//! ComponentScaler reconciler: predictive store autoscaling with a
//! learned `builders_per_replica` ratio.
//!
//! Reconcile (10s tick): poll scheduler `GetSizeClassStatus` →
//! `Σ(queued+running)` builders; poll each `loadEndpoint` pod's
//! `StoreAdminService.GetLoad` → max `pg_pool_utilization`; feed
//! both into [`crate::scaling::component::decide`]; patch the target
//! Deployment's `/scale` subresource; write `.status` (which
//! persists `learnedRatio` across controller restarts).
//!
//! Why a `kube::runtime::Controller` (not a freestanding
//! `spawn_monitored` loop like `gc_schedule`): the CR's `.status` IS
//! the durable state (learnedRatio, lowLoadTicks). Watching the CR
//! means a `kubectl edit` of `spec.replicas.max` re-reconciles
//! immediately; a freestanding loop would only see it on the next
//! poll. The 10s tick is `Action::requeue(10s)`.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{CustomResourceExt, ResourceExt};
use tracing::{debug, instrument, warn};

use crate::error::{Error, Result, error_kind};
use crate::reconcilers::{Ctx, error_key};
use crate::scaling::component::{self, Decision};
use rio_crds::componentscaler::{
    ComponentScaler, ComponentScalerSpec, ComponentScalerStatus, Signal,
};

/// Reconcile interval. 10s: fast enough that scale-up beats the
/// I-105 cliff (a 200-builder burst takes ~30s from queued to all-
/// FUSE-warm), slow enough that the per-pod GetLoad fan-out (≤14
/// RPCs) doesn't matter.
const REQUEUE: Duration = Duration::from_secs(10);

/// Per-pod GetLoad timeout. Generous — GetLoad is a single PgPool
/// stat read, sub-ms when healthy. A pod that takes >2s is
/// unhealthy enough that we don't want its load reading anyway
/// (treated as `None` for that pod, the max() over the others
/// covers it).
const LOAD_RPC_TIMEOUT: Duration = Duration::from_secs(2);

/// SSA field-manager for `deployments/scale` patches AND status
/// writes. One manager (not split like the BuilderPool reconciler/
/// autoscaler) because there's no second writer here — this
/// reconciler owns both. Distinct from `rio-controller` (helm's
/// Deployment apply) so a `helm upgrade` doesn't fight the replica
/// count.
const MANAGER: &str = "rio-controller-componentscaler";

/// Top-level reconcile. No finalizer wrap — ComponentScaler owns no
/// children (the Deployment is helm's). Delete = the apiserver GCs
/// the CR; the Deployment keeps its last-patched replica count
/// until the next `helm upgrade` (or another scaler) sets it.
// r[impl ctrl.scaler.component]
#[instrument(
    skip(cs, ctx),
    fields(reconciler = "componentscaler", cs = %cs.name_any(), ns = cs.namespace().as_deref().unwrap_or(""))
)]
pub async fn reconcile(cs: Arc<ComponentScaler>, ctx: Arc<Ctx>) -> Result<Action> {
    let start = std::time::Instant::now();
    let key = error_key(cs.as_ref());
    let result = reconcile_inner(cs, ctx.clone()).await;
    if result.is_ok() {
        ctx.reset_error_count(&key);
    }
    metrics::histogram!("rio_controller_reconcile_duration_seconds",
        "reconciler" => "componentscaler")
    .record(start.elapsed().as_secs_f64());
    result
}

async fn reconcile_inner(cs: Arc<ComponentScaler>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = cs
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("ComponentScaler has no namespace".into()))?;
    let spec = &cs.spec;
    let status = cs.status.clone().unwrap_or_default();

    // ── Predictive: total builders (queued + running) ────────────
    // `self-reported` is reserved (gateway scaling); reject it now
    // so a misconfigured CR surfaces in `kubectl describe` instead
    // of silently doing nothing.
    let builders = match spec.signal {
        Signal::SchedulerBuilders => {
            let resp = ctx.size_class_status().await.map_err(|e| {
                Error::InvalidSpec(format!(
                    "GetSizeClassStatus failed: {e}; check schedulerAddr / scheduler readiness"
                ))
            })?;
            // Empty classes = scheduler.sizeClasses not configured.
            // Fall back to ClusterStatus.queued+running so a
            // ComponentScaler on a single-class cluster still
            // scales (rather than silently pinning at min). EKS
            // always has sizeClasses (xtask deploy sets it); the
            // fallback covers VM tests + bare deployments.
            if resp.classes.is_empty() {
                let cs = ctx.admin.clone().cluster_status(()).await.map_err(|e| {
                    Error::InvalidSpec(format!(
                        "ClusterStatus failed: {e}; check schedulerAddr / scheduler readiness"
                    ))
                })?;
                let cs = cs.into_inner();
                u64::from(cs.queued_derivations) + u64::from(cs.running_derivations)
            } else {
                component::total_builders(&resp)
            }
        }
        Signal::SelfReported => {
            return Err(Error::InvalidSpec(
                "spec.signal=self-reported is reserved (gateway scaling); \
                 use scheduler-builders for store"
                    .into(),
            ));
        }
    };

    // ── Observed: max(GetLoad) across loadEndpoint pods ──────────
    let max_load = poll_max_load(&spec.load_endpoint).await;

    // ── Current replica count from the Deployment ────────────────
    let dep_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    let dep = dep_api.get(&spec.target_ref.name).await.map_err(|e| {
        Error::InvalidSpec(format!(
            "targetRef Deployment {}/{} not found: {e}; check spec.targetRef.name",
            ns, spec.target_ref.name
        ))
    })?;
    let current = dep
        .spec
        .as_ref()
        .and_then(|s| s.replicas)
        .unwrap_or(spec.replicas.min);

    // ── Decide ───────────────────────────────────────────────────
    // low_load_ticks comes from Ctx (in-process), NOT status: writing
    // it to status on every tick would change the CR → watch fires →
    // tight reconcile loop instead of the 10s requeue. The status
    // field still exists for `kubectl get` observability — populated
    // from the in-process counter, but the reconciler reads from Ctx.
    let key = error_key(cs.as_ref());
    let low_ticks_in = ctx
        .component_low_ticks
        .lock()
        .get(&key)
        .copied()
        .unwrap_or(0);
    let since_up = status.last_scale_up_time.as_ref().and_then(since);
    let mut status_in = status.clone();
    status_in.low_load_ticks = low_ticks_in;
    let decision = component::decide(spec, &status_in, current, builders, max_load, since_up);
    ctx.component_low_ticks
        .lock()
        .insert(key, decision.low_load_ticks);

    publish_metrics(&cs, &decision, max_load);
    debug!(
        builders,
        ?max_load,
        current,
        desired = decision.desired,
        learned_ratio = decision.learned_ratio,
        low_load_ticks = decision.low_load_ticks,
        "componentscaler decision"
    );

    // ── Patch /scale (only if changed) ───────────────────────────
    if decision.desired != current {
        patch_scale(&dep_api, &spec.target_ref.name, decision.desired).await?;
        metrics::counter!("rio_controller_scaling_decisions_total",
            "direction" => if decision.scaled_up { "up" } else { "down" })
        .increment(1);
    }

    // ── Status (only on material change) ─────────────────────────
    // SSA-apply with identical content is a no-op apiserver-side
    // (no resourceVersion bump → no watch event), but float-
    // formatting jitter on observedLoadFactor can defeat that.
    // Explicitly skip when nothing the operator cares about
    // changed; the 10s requeue is the next tick.
    if status_changed(&status, &decision, max_load) {
        let cs_api: Api<ComponentScaler> = Api::namespaced(ctx.client.clone(), &ns);
        patch_status(&cs_api, &cs, spec, &status, &decision, max_load).await?;
    }

    Ok(Action::requeue(REQUEUE))
}

/// Resolve `loadEndpoint` (headless-svc DNS) → per-pod GetLoad →
/// max utilization. `None` on total failure (DNS unresolved, all
/// RPCs failed) — the caller skips ratio correction this tick.
///
/// Fan-out is sequential (not concurrent): ≤14 pods × sub-ms RPC =
/// negligible. Concurrent would need a JoinSet + per-pod channel;
/// not worth it for the latency budget.
async fn poll_max_load(endpoint: &str) -> Option<f64> {
    let (host, port) = endpoint.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;

    let addrs: Vec<_> = match tokio::net::lookup_host((host, port)).await {
        Ok(it) => it.collect(),
        Err(e) => {
            warn!(host, error = %e, "componentscaler: loadEndpoint DNS resolve failed");
            return None;
        }
    };
    if addrs.is_empty() {
        debug!(host, "componentscaler: loadEndpoint resolved to 0 addrs");
        return None;
    }

    let mut max: Option<f64> = None;
    for addr in addrs {
        // `connect_store_admin_at` (not `connect_store_admin`): the
        // per-pod connect must override the TLS-verify domain to
        // `rio-store` (the cert's SAN), since we dial a pod IP. The
        // plain helper would SAN-check against the IP and fail under
        // mTLS.
        let load = async {
            let mut c = rio_proto::client::balance::connect_store_admin_at(addr).await?;
            let r = c
                .get_load(rio_proto::types::GetLoadRequest {})
                .await?
                .into_inner();
            anyhow::Ok(r.pg_pool_utilization as f64)
        };
        match tokio::time::timeout(LOAD_RPC_TIMEOUT, load).await {
            Ok(Ok(l)) => max = Some(max.map_or(l, |m: f64| m.max(l))),
            Ok(Err(e)) => debug!(%addr, error = %e, "componentscaler: GetLoad failed"),
            Err(_) => debug!(%addr, "componentscaler: GetLoad timed out"),
        }
    }
    max
}

/// Patch `apps/v1 Deployment {name} /scale`. Uses the `/scale`
/// subresource (not `spec.replicas` SSA): /scale is the contract K8s
/// HPA uses, it's what the `deployments/scale` RBAC verb covers, and
/// it doesn't conflict with helm's field-ownership of the rest of
/// the Deployment spec.
async fn patch_scale(api: &Api<Deployment>, name: &str, replicas: i32) -> Result<()> {
    let patch = serde_json::json!({ "spec": { "replicas": replicas } });
    api.patch_scale(
        name,
        &PatchParams {
            field_manager: Some(MANAGER.into()),
            ..Default::default()
        },
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

/// True if the new status would differ from `prev` in a way that
/// matters to operators (or to the next reconcile). `low_load_ticks`
/// is held in-process and excluded; `observed_load_factor` is
/// compared to one decimal place — sub-0.1 jitter isn't worth a CR
/// write (and would risk watch-loop if SSA byte-compares the float).
fn status_changed(prev: &ComponentScalerStatus, d: &Decision, max_load: Option<f64>) -> bool {
    let load_bucket = |l: Option<f64>| l.map(|v| (v * 10.0).round() as i64);
    prev.learned_ratio.map(|r| r.to_bits()) != Some(d.learned_ratio.to_bits())
        || prev.desired_replicas != d.desired
        || load_bucket(prev.observed_load_factor) != load_bucket(max_load)
        || d.scaled_up
}

/// Patch `.status`. Preserves `lastScaleUpTime` unless `decision.
/// scaled_up` (then stamps now()). The whole status is rewritten
/// each tick — there's no second writer to coordinate with.
async fn patch_status(
    api: &Api<ComponentScaler>,
    cs: &ComponentScaler,
    _spec: &ComponentScalerSpec,
    prev: &ComponentScalerStatus,
    decision: &Decision,
    max_load: Option<f64>,
) -> Result<()> {
    let last_up = if decision.scaled_up {
        Some(Timestamp::now().to_string())
    } else {
        prev.last_scale_up_time.as_ref().map(|t| t.0.to_string())
    };
    let ar = ComponentScaler::api_resource();
    let body = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "learnedRatio": decision.learned_ratio,
            "observedLoadFactor": max_load,
            "desiredReplicas": decision.desired,
            "lastScaleUpTime": last_up,
            "lowLoadTicks": decision.low_load_ticks,
        },
    });
    api.patch_status(
        &cs.name_any(),
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(&body),
    )
    .await?;
    Ok(())
}

/// Publish per-CR gauges. Labelled by `cs={ns}/{name}` so multiple
/// ComponentScalers (store + future gateway) get separate series.
fn publish_metrics(cs: &ComponentScaler, decision: &Decision, max_load: Option<f64>) {
    let label = format!("{}/{}", cs.namespace().unwrap_or_default(), cs.name_any());
    metrics::gauge!("rio_controller_component_scaler_learned_ratio",
        "cs" => label.clone())
    .set(decision.learned_ratio);
    metrics::gauge!("rio_controller_component_scaler_desired_replicas",
        "cs" => label.clone())
    .set(decision.desired as f64);
    if let Some(l) = max_load {
        metrics::gauge!("rio_controller_component_scaler_observed_load",
            "cs" => label)
        .set(l);
    }
}

/// `now() - t` as a non-negative Duration. `None` on parse failure
/// or future timestamp (clock skew between controller restarts) —
/// caller treats `None` as "infinitely long ago" (allow
/// scale-down), which is the safe direction for a corrupt
/// lastScaleUpTime.
fn since(t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time) -> Option<Duration> {
    let then = &t.0;
    let now = Timestamp::now();
    let span = now.since(*then).ok()?;
    let secs = span.get_seconds();
    if secs < 0 {
        return None;
    }
    Some(Duration::from_secs(secs as u64))
}

/// Requeue policy. Same shape as the other reconcilers.
pub fn error_policy(cs: Arc<ComponentScaler>, err: &Error, ctx: Arc<Ctx>) -> Action {
    metrics::counter!("rio_controller_reconcile_errors_total",
        "reconciler" => "componentscaler", "error_kind" => error_kind(err))
    .increment(1);
    match err {
        Error::InvalidSpec(msg) => {
            warn!(error = %msg, "ComponentScaler invalid spec / upstream unreachable");
            // 30s (not 300s like the other InvalidSpec branches): a
            // transient scheduler/store unreachability funnels
            // through InvalidSpec here, and 5min of no scaling under
            // a builder burst is the I-105 cliff. The error message
            // names the fix for genuine spec errors.
            Action::requeue(Duration::from_secs(30))
        }
        _ => {
            let delay = ctx.error_backoff(&error_key(cs.as_ref()));
            warn!(error = %err, backoff = ?delay, "componentscaler reconcile failed; retrying");
            Action::requeue(delay)
        }
    }
}
