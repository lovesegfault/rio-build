//! ComponentScaler reconciler: predictive store autoscaling with a
//! learned `builders_per_replica` ratio.
//!
//! Reconcile (10s tick): poll scheduler `ClusterStatus` →
//! `Σ(queued+running)` builders; poll each `loadEndpoint` pod's
//! `StoreAdminService.GetLoad` → max `pg_pool_utilization`; feed
//! both into `decide::decide`; patch the target
//! Deployment's `/scale` subresource; write `.status` (which
//! persists `learnedRatio` across controller restarts).
//!
//! Why a `kube::runtime::Controller` (not a freestanding
//! `spawn_monitored` loop like `gc_schedule`): the CR's `.status` IS
//! the durable state (learnedRatio, lowLoadTicks). Watching the CR
//! means a `kubectl edit` of `spec.replicas.max` re-reconciles
//! immediately; a freestanding loop would only see it on the next
//! poll. The 10s tick is `Action::requeue(10s)`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{CustomResourceExt, ResourceExt};
use tracing::{debug, warn};

use crate::error::{Error, Result};
use crate::reconcilers::{Ctx, error_key, standard_error_policy, timed};
use rio_crds::componentscaler::{
    ComponentScaler, ComponentScalerSpec, ComponentScalerStatus, Signal,
};

mod decide;
use decide::Decision;

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
/// writes. One manager (not split like the Pool reconciler/
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
pub async fn reconcile(cs: Arc<ComponentScaler>, ctx: Arc<Ctx>) -> Result<Action> {
    timed("componentscaler", cs, ctx, reconcile_inner).await
}

async fn reconcile_inner(cs: Arc<ComponentScaler>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = crate::reconcilers::require_namespace(&*cs)?;
    let spec = &cs.spec;
    let status = cs.status.clone().unwrap_or_default();

    // ── Predictive: total builders (queued + running) ────────────
    let builders = match spec.signal {
        Signal::SchedulerBuilders => {
            // ClusterStatus, NOT GetSpawnIntents: Σqueued_by_system ==
            // queued_derivations (snapshot.rs counts both from the same
            // Ready set). GetSpawnIntents would make the scheduler run
            // solve_intent_for per Ready drv and serialize the full
            // intent vec — wasted work for a u32 ClusterStatus already
            // carries.
            // Timeout: connect_timeout+h2 keepalive on `ctx.admin`
            // detect dead TCP, NOT a stalled handler. kube-rs
            // serializes per-object reconciles with no deadline, so
            // an unbounded await here would silently stop scaling
            // until controller restart (bug_464). Elapsed →
            // InvalidSpec → error_policy 30s requeue keeps the loop
            // alive.
            let cs = tokio::time::timeout(LOAD_RPC_TIMEOUT, ctx.admin.clone().cluster_status(()))
                .await
                .map_err(|_| {
                    Error::InvalidSpec("ClusterStatus timed out; check scheduler readiness".into())
                })?
                .map_err(|e| {
                    Error::InvalidSpec(format!(
                        "ClusterStatus failed: {e}; check schedulerAddr / scheduler readiness"
                    ))
                })?;
            decide::total_builders(&cs.into_inner())
        }
    };

    // ── Observed: max(GetLoad) across loadEndpoint pods ──────────
    let max_load = poll_max_load(&spec.load_endpoint, ctx.service_interceptor.clone()).await;

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
    let (low_ticks_in, last_status_write) = {
        let low = ctx.scaler.low_ticks.lock().get(&key).copied().unwrap_or(0);
        let last = ctx.scaler.last_status_write.lock().get(&key).copied();
        (low, last)
    };
    let since_up = status.last_scale_up_time.as_ref().and_then(since);
    let mut status_in = status.clone();
    status_in.low_load_ticks = low_ticks_in;
    let decision = decide::decide(spec, &status_in, current, builders, max_load, since_up);
    ctx.scaler
        .low_ticks
        .lock()
        .insert(key.clone(), decision.low_load_ticks);

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

    // ── Status (rate-limited + only on material change) ──────────
    // `status_changed` alone is insufficient: `decide()` mutates
    // `learnedRatio` on every high-load tick, so under sustained
    // load every reconcile would write → watch fires → re-reconcile
    // at loop-rate (bug_213). `status_write_due` caps to once per
    // REQUEUE window via in-process timestamp (same pattern as
    // `low_ticks`). Worst case: 2 reconciles/10s (the requeue + one
    // watch-echo whose status-write is suppressed). Scale-ups bypass
    // the rate-limit: `lastScaleUpTime` is correctness-load-bearing
    // (gates SCALE_DOWN_STABILIZATION), not observability, and
    // scale-ups are rare + self-limiting so the watch-loop concern
    // doesn't apply (bug_060).
    if status_write_gate(
        decision.scaled_up,
        last_status_write,
        &status,
        &decision,
        max_load,
    ) {
        let cs_api: Api<ComponentScaler> = Api::namespaced(ctx.client.clone(), &ns);
        patch_status(&cs_api, &cs, spec, &status, &decision, max_load).await?;
        ctx.scaler
            .last_status_write
            .lock()
            .insert(key, Instant::now());
    }

    Ok(Action::requeue(REQUEUE))
}

/// Resolve `loadEndpoint` (headless-svc DNS) → per-pod GetLoad →
/// max utilization. `None` on total failure (endpoint malformed,
/// DNS unresolved/timed-out, all RPCs failed) — the caller skips
/// ratio correction this tick.
///
/// Bounds total latency to ≤2×`LOAD_RPC_TIMEOUT` (DNS resolve +
/// concurrent fan-out). DNS is timeout-wrapped: `lookup_host` is
/// `spawn_blocking(getaddrinfo)` with no inherent deadline; under
/// k8s `ndots:5` + 3-4 search domains + degraded CoreDNS that's
/// 20-40s of `glibc timeout:5 × attempts:2` per expansion — the
/// entire I-105 cliff window with the per-object reconcile parked
/// (bug_062). Fan-out is concurrent (JoinSet): sequential would
/// cost N×2s under N stale headless-DNS endpoints (rolling restart,
/// drained node) — 5 stale = the entire `REQUEUE` budget gone before
/// `decide()` runs (bug_194).
async fn poll_max_load(
    endpoint: &str,
    service_interceptor: rio_auth::hmac::ServiceTokenInterceptor,
) -> Option<f64> {
    let Some((host, port)) = endpoint.rsplit_once(':') else {
        warn!(
            endpoint,
            "componentscaler: loadEndpoint missing ':port'; fix spec.loadEndpoint"
        );
        return None;
    };
    let Ok(port) = port.parse::<u16>() else {
        warn!(
            endpoint,
            "componentscaler: loadEndpoint port not a u16; fix spec.loadEndpoint"
        );
        return None;
    };

    let addrs: Vec<_> =
        match tokio::time::timeout(LOAD_RPC_TIMEOUT, tokio::net::lookup_host((host, port))).await {
            Ok(Ok(it)) => it.collect(),
            Ok(Err(e)) => {
                warn!(host, error = %e, "componentscaler: loadEndpoint DNS resolve failed");
                return None;
            }
            Err(_) => {
                warn!(host, "componentscaler: loadEndpoint DNS resolve timed out");
                return None;
            }
        };
    if addrs.is_empty() {
        debug!(host, "componentscaler: loadEndpoint resolved to 0 addrs");
        return None;
    }
    poll_max_load_addrs(addrs, service_interceptor).await
}

/// Per-pod load fold: `max(pg_pool_utilization,
/// substitute_admission_utilization)`. Substitution admission can
/// saturate independently of PG (upstream HTTP bottleneck — permits
/// held across the NAR fetch, PG connection released per-query), so a
/// replica is "loaded" if EITHER dimension is high. `decide()` then
/// sees one scalar per pod and its 0.8/0.3 thresholds need no change.
/// Extracted from the `poll_max_load_addrs` closure so the
/// 2-dimension fold is unit-testable without a mock gRPC server.
fn fold_load(r: &rio_proto::types::GetLoadResponse) -> f64 {
    (r.pg_pool_utilization as f64).max(r.substitute_admission_utilization as f64)
}

/// Concurrent per-pod `GetLoad` fan-out → max. Split from
/// [`poll_max_load`] so the timeout-aggregate behavior is
/// unit-testable without DNS.
async fn poll_max_load_addrs(
    addrs: Vec<SocketAddr>,
    service_interceptor: rio_auth::hmac::ServiceTokenInterceptor,
) -> Option<f64> {
    // `connect_store_admin_at` (not the balanced channel): we need
    // each pod's individual GetLoad reading for the max(), so dial
    // every resolved IP directly — p2c would route all calls to one
    // or two pods. Wrapped with the service-token interceptor —
    // `r[store.admin.service-gate]` requires it on `GetLoad`.
    let mut set = tokio::task::JoinSet::new();
    for addr in addrs {
        let int = service_interceptor.clone();
        set.spawn(async move {
            let load = async {
                let mut c = rio_proto::client::balance::connect_store_admin_at(addr, int).await?;
                let r = c
                    .get_load(rio_proto::types::GetLoadRequest {})
                    .await?
                    .into_inner();
                anyhow::Ok(fold_load(&r))
            };
            (addr, tokio::time::timeout(LOAD_RPC_TIMEOUT, load).await)
        });
    }
    let mut max: Option<f64> = None;
    while let Some(joined) = set.join_next().await {
        let Ok((addr, res)) = joined else { continue };
        match res {
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

/// True if at least `REQUEUE` has elapsed since the last successful
/// `patch_status` for this CR. The watch-loop guard: status writes
/// bump resourceVersion → watch fires → re-reconcile, so without
/// this a `learnedRatio` change every tick collapses the 10s cadence
/// to loop-body-latency. The caller stamps `last` on a successful
/// patch.
pub(super) fn status_write_due(last: Option<Instant>) -> bool {
    last.is_none_or(|t| t.elapsed() >= REQUEUE)
}

/// Combined gate for `patch_status`. Rate-limited to once per
/// `REQUEUE` AND only on material change — except `scaled_up` always
/// passes the rate-limit. `lastScaleUpTime` is the sole input to
/// `decide()`'s `SCALE_DOWN_STABILIZATION` (correctness, not
/// observability); a scale-up landing in the suppression window
/// would patch `/scale` but skip the stamp, and by the next
/// non-suppressed tick `current` has caught up so `scaled_up=false`
/// preserves the stale timestamp → stabilization bypassed → flap
/// (bug_060). Scale-ups are rare and self-limiting (next tick sees
/// `desired==current`), so bypassing the rate-limit for them does
/// not reintroduce the bug_213 watch-loop.
pub(super) fn status_write_gate(
    scaled_up: bool,
    last: Option<Instant>,
    prev: &ComponentScalerStatus,
    d: &Decision,
    max_load: Option<f64>,
) -> bool {
    (scaled_up || status_write_due(last)) && status_changed(prev, d, max_load)
}

/// True if the new status would differ from `prev` in a way that
/// matters to operators (or to the next reconcile). `low_load_ticks`
/// is held in-process and excluded; `observed_load_factor` is
/// compared to one decimal place — sub-0.1 jitter isn't worth a CR
/// write. The watch-loop guard is at the caller via
/// [`status_write_due`] — this predicate alone is insufficient
/// (`learned_ratio` changes every high-load tick).
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

/// Requeue policy. 30s on `InvalidSpec` (not 300s like the other
/// reconcilers): transient scheduler/store unreachability funnels
/// through `InvalidSpec` here, and 5min of no scaling under a builder
/// burst is the I-105 cliff. The error message names the fix for
/// genuine spec errors.
pub fn error_policy(cs: Arc<ComponentScaler>, err: &Error, ctx: Arc<Ctx>) -> Action {
    standard_error_policy("componentscaler", cs, err, ctx, Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::GetLoadResponse;

    /// `fold_load` per-pod = `max(pg, substitute_admission)`; the
    /// across-pod aggregate (the loop in `poll_max_load_addrs`) is
    /// `max` over those. Two pods with the saturated dimension on
    /// opposite axes: the result is the global max regardless of
    /// which axis carries it.
    // r[verify store.admin.get-load+2]
    #[test]
    fn fold_load_max_of_dimensions_then_pods() {
        let pod_a = GetLoadResponse {
            pg_pool_utilization: 0.2,
            substitute_admission_utilization: 0.9,
        };
        let pod_b = GetLoadResponse {
            pg_pool_utilization: 0.7,
            substitute_admission_utilization: 0.1,
        };
        // Per-pod fold picks the saturated dimension.
        assert!((fold_load(&pod_a) - 0.9).abs() < 1e-6);
        assert!((fold_load(&pod_b) - 0.7).abs() < 1e-6);
        // Across-pod aggregate: same `f64::max` reduction the
        // `poll_max_load_addrs` JoinSet loop applies.
        let agg = [&pod_a, &pod_b]
            .into_iter()
            .map(fold_load)
            .fold(None::<f64>, |m, l| Some(m.map_or(l, |m| m.max(l))));
        assert_eq!(agg, Some(0.9_f32 as f64));
        // Old behavior (pg-only) would have returned 0.7 — proving
        // the new dimension is load-bearing.
        let pg_only = [&pod_a, &pod_b]
            .into_iter()
            .map(|r| r.pg_pool_utilization as f64)
            .fold(f64::MIN, f64::max);
        assert!(agg.unwrap() > pg_only);
    }

    /// bug_213 regression: status writes rate-limited to once per
    /// REQUEUE window. First reconcile (no last) → due; immediately
    /// after a write → suppressed; after REQUEUE elapsed → due again.
    #[test]
    fn status_write_rate_limited() {
        assert!(status_write_due(None), "first reconcile → due");
        assert!(
            !status_write_due(Some(Instant::now())),
            "just wrote → suppressed (watch-echo doesn't re-write)"
        );
        let past = Instant::now()
            .checked_sub(REQUEUE + Duration::from_secs(1))
            .expect("monotonic clock far enough past boot");
        assert!(status_write_due(Some(past)), "REQUEUE elapsed → due");
    }

    /// bug_194 regression: per-pod fan-out is concurrent, so total
    /// latency is bounded by ONE `LOAD_RPC_TIMEOUT` regardless of how
    /// many endpoints hang. Three never-accepting listeners would
    /// cost 3×2s sequential; the outer 2× timeout asserts ≤4s.
    /// Wall-clock with 100% slack budget — sequential at 6s cleanly
    /// fails the 4s outer timeout, concurrent at ~2s cleanly passes.
    #[tokio::test]
    async fn poll_max_load_bounded_under_hang() {
        // Listeners that never accept(): TCP connect lands in the
        // backlog, then the gRPC h2 handshake hangs → per-task
        // LOAD_RPC_TIMEOUT fires.
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..3 {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(l.local_addr().unwrap());
            listeners.push(l);
        }
        let int = rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller");
        let res = tokio::time::timeout(LOAD_RPC_TIMEOUT * 2, poll_max_load_addrs(addrs, int)).await;
        assert!(
            res.is_ok(),
            "concurrent fan-out must complete within 2×LOAD_RPC_TIMEOUT \
             (sequential would take 3× = 6s and fail this)"
        );
        assert_eq!(res.unwrap(), None, "all hung → no reading");
    }

    /// bug_060 regression: a scale-up that lands inside the
    /// `status_write_due` suppression window MUST still write status
    /// (so `lastScaleUpTime` is stamped). Without the bypass the
    /// stabilization gate is defeated and the Deployment flaps
    /// up→down within ~20s.
    #[test]
    fn scale_up_bypasses_status_rate_limit() {
        let prev = ComponentScalerStatus::default();
        let d = Decision {
            desired: 5,
            learned_ratio: 50.0,
            low_load_ticks: 0,
            scaled_up: true,
        };
        // Just wrote (suppression active): scaled_up=true → gate open.
        assert!(
            status_write_gate(true, Some(Instant::now()), &prev, &d, None),
            "scale-up must write status even inside suppression window"
        );
        // Same suppression, scaled_up=false → gate closed (the
        // bug_213 rate-limit still applies to non-scale-up writes).
        let d_no_up = Decision {
            scaled_up: false,
            ..d
        };
        assert!(
            !status_write_gate(false, Some(Instant::now()), &prev, &d_no_up, None),
            "non-scale-up inside suppression → suppressed"
        );
    }

    /// bug_062 regression: end-to-end `poll_max_load` is bounded.
    /// Literal IP → DNS resolves instantly, then the never-accepting
    /// listener exercises the timeout-wrapped fan-out. The DNS-hang
    /// arm itself can't be portably injected (`getaddrinfo` is
    /// `spawn_blocking`, ignores `tokio::time::pause()`), but this
    /// asserts the public entrypoint completes within
    /// 3×`LOAD_RPC_TIMEOUT` and the DNS step is structurally inside
    /// the timeout-wrapped region.
    #[tokio::test]
    async fn poll_max_load_end_to_end_bounded() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("127.0.0.1:{}", l.local_addr().unwrap().port());
        let int = rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller");
        let res = tokio::time::timeout(LOAD_RPC_TIMEOUT * 3, poll_max_load(&endpoint, int)).await;
        assert!(
            res.is_ok(),
            "poll_max_load must complete within 3×LOAD_RPC_TIMEOUT end-to-end \
             (DNS resolve + concurrent fan-out both bounded)"
        );
        assert_eq!(res.unwrap(), None, "hung listener → no reading");
    }
}
