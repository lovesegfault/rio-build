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
/// stat read, sub-ms when healthy. A pod that takes >2s is unhealthy
/// enough that we don't trust its reading this tick — but a skipped
/// reading is NOT covered by the survivors' max(): per-replica gauges
/// drop the skipped replica exactly when its reading may BE the max
/// (saturation is what makes a pod slow to answer), so the fold
/// degrades the coverage letter (`answered < resolved`) and
/// `decide()` consumes partial evidence asymmetrically — survivor-
/// high still scales up, survivor-low never funds ratio growth
/// ([`decide::LoadAggregate`]).
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
// r[impl ctrl.scaler.component+2]
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
            // timeout-census: delay — InvalidSpec → error_policy 30s
            // requeue; scaling resumes next tick.
            // census[gen: rio-controller/tests/timeout_census.txt]
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
    let load = poll_max_load(&spec.load_endpoint, ctx.service_interceptor.clone()).await;
    let max_load = load.map(|l| l.max);

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
    let (low_ticks_in, last_status_write, last_scale_up) = {
        let low = ctx.scaler.low_ticks.lock().get(&key).copied().unwrap_or(0);
        let last = ctx.scaler.last_status_write.lock().get(&key).copied();
        let up = ctx.scaler.last_scale_up.lock().get(&key).copied();
        (low, last, up)
    };
    // r[impl ctrl.scaler.fence-arming]
    // The fence reads the freshest of (in-process record, status
    // stamp): a transient patch_status failure after a successful
    // scale patch leaves status stale/None but the in-process record
    // fresh (bug_021 — the second-write failure edge of the bug_060
    // window).
    let since_up = freshest_since_up(
        status.last_scale_up_time.as_ref().and_then(since),
        last_scale_up,
    );
    let mut status_in = status.clone();
    status_in.low_load_ticks = low_ticks_in;
    let decision = decide::decide(spec, &status_in, current, builders, load, since_up);
    ctx.scaler
        .low_ticks
        .lock()
        .insert(key.clone(), decision.low_load_ticks);

    publish_metrics(&cs, &decision, load);
    debug!(
        builders,
        ?load,
        current,
        desired = decision.desired,
        learned_ratio = decision.learned_ratio,
        low_load_ticks = decision.low_load_ticks,
        "componentscaler decision"
    );

    // ── Patch /scale (only if changed) ───────────────────────────
    if decision.desired != current {
        patch_scale(&dep_api, &spec.target_ref.name, decision.desired).await?;
        // r[impl ctrl.scaler.fence-arming]
        // The fence-arming record at the mutating write's SUCCESS
        // site (R34-w(v)): stamped in-process the moment patch_scale
        // returns Ok, before and independent of the second
        // (patch_status) write. The status stamp below is the
        // durable backfill; this is the correctness copy (bug_021).
        if decision.scaled_up {
            ctx.scaler
                .last_scale_up
                .lock()
                .insert(key.clone(), Instant::now());
        }
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
    // the rate-limit: `lastScaleUpTime` is the durable backfill of
    // the in-process fence record (the correctness copy is stamped
    // at patch_scale's success site above — bug_021), and scale-ups
    // are rare + self-limiting so the watch-loop concern doesn't
    // apply (bug_060).
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
/// the denominated max-utilization letter
/// ([`decide::LoadAggregate`]: max over answering pods + answered/
/// resolved coverage). `None` on total failure (endpoint malformed,
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
) -> Option<decide::LoadAggregate> {
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

    // timeout-census: delay — no load reading this tick; the scaler
    // re-polls next pass. census[gen: rio-controller/tests/timeout_census.txt]
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

/// Concurrent per-pod `GetLoad` fan-out → the denominated letter.
/// Split from [`poll_max_load`] so the timeout-aggregate behavior is
/// unit-testable without DNS. `resolved` is the address count handed
/// in; every task that errors or times out leaves its reading `None`,
/// so the fold's denominator counts exactly the answers
/// ([`decide::LoadAggregate::fold`]).
// r[impl ctrl.scaler.load-coverage]
async fn poll_max_load_addrs(
    addrs: Vec<SocketAddr>,
    service_interceptor: rio_auth::hmac::ServiceTokenInterceptor,
) -> Option<decide::LoadAggregate> {
    // `connect_store_admin_at` (not the balanced channel): we need
    // each pod's individual GetLoad reading for the max(), so dial
    // every resolved IP directly — p2c would route all calls to one
    // or two pods. Wrapped with the service-token interceptor —
    // `r[store.admin.service-gate]` requires it on `GetLoad`.
    let resolved = addrs.len();
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
            // A load-correlated timeout recurs every pass (the slow
            // pod is the saturated one), so the consequence is priced
            // by the LETTER, not by the re-poll: the fold's coverage
            // degrades (answered < resolved), survivor-high still
            // scales up, survivor-low funds nothing, and load_poll_
            // partial_total is the recurrence's operator trail.
            // timeout-census: delay — reading skipped; letter degraded.
            // census[gen: rio-controller/tests/timeout_census.txt]
            (addr, tokio::time::timeout(LOAD_RPC_TIMEOUT, load).await)
        });
    }
    let mut readings: Vec<Option<f64>> = Vec::with_capacity(resolved);
    while let Some(joined) = set.join_next().await {
        let Ok((addr, res)) = joined else { continue };
        match res {
            Ok(Ok(l)) => readings.push(Some(l)),
            Ok(Err(e)) => {
                debug!(%addr, error = %e, "componentscaler: GetLoad failed");
                readings.push(None);
            }
            Err(_) => {
                debug!(%addr, "componentscaler: GetLoad timed out");
                readings.push(None);
            }
        }
    }
    // A panicked/cancelled task never pushed its reading — pad the
    // denominator so a lost task still degrades coverage instead of
    // silently shrinking `resolved`.
    readings.resize(resolved, None);
    decide::LoadAggregate::fold(&readings)
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
/// passes the rate-limit. `lastScaleUpTime` is the durable backfill
/// of `decide()`'s `SCALE_DOWN_STABILIZATION` fence (the
/// correctness copy is the in-process `ScalerState::last_scale_up`
/// record); a scale-up landing in the suppression window
/// would patch `/scale` but skip the durable stamp until the next
/// non-suppressed tick (bug_060). Scale-ups are rare and
/// self-limiting (next tick sees `desired==current`), so bypassing
/// the rate-limit for them does not reintroduce the bug_213
/// watch-loop.
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
/// The partial-coverage counter is the operator trail for the
/// load-correlated timeout regime — a chronically slow/saturated
/// replica degrades every tick's letter, and a rate here (rather
/// than a once-off) is exactly that recurrence.
fn publish_metrics(cs: &ComponentScaler, decision: &Decision, load: Option<decide::LoadAggregate>) {
    let label = format!("{}/{}", cs.namespace().unwrap_or_default(), cs.name_any());
    metrics::gauge!("rio_controller_component_scaler_learned_ratio",
        "cs" => label.clone())
    .set(decision.learned_ratio);
    metrics::gauge!("rio_controller_component_scaler_desired_replicas",
        "cs" => label.clone())
    .set(decision.desired as f64);
    if let Some(l) = load {
        if !l.total_coverage() {
            metrics::counter!("rio_controller_component_scaler_load_poll_partial_total",
                "cs" => label.clone())
            .increment(1);
        }
        metrics::gauge!("rio_controller_component_scaler_observed_load",
            "cs" => label)
        .set(l.max);
    }
}

// r[impl ctrl.scaler.fence-arming]
/// `since_last_scale_up` for [`decide::decide`]: the freshest of the
/// in-process record (stamped at `patch_scale`'s success site) and
/// the durable status stamp. The smaller Duration is the more-recent
/// record; either alone suffices. A scale-up is two non-atomic
/// apiserver writes — `patch_scale` then `patch_status` — and only
/// the second carried `lastScaleUpTime`, so a transient status-write
/// failure after a successful scale patch left the next reconcile
/// reading `current==desired`, `scaled_up=false`, and the stale/
/// `None` stamp preserved permanently → the 5-minute anti-flap fence
/// silently disarmed for that burst (bug_021; the bug_060 close
/// fixed the rate-limit edge of the same window, not the
/// write-failure edge). The in-process record is the R34-w(v)
/// fence-arming fact at the mutating write's success site; the
/// status stamp is the restart-surviving backfill.
///
/// Alternates priced: (a) stamp status BEFORE the scale patch — an
/// earlier write can still fail separately and the invariant is
/// "every successful scale-up leaves a fresh fence", so reordering
/// alone does not close the window; (b) infer scale-up from
/// observed-replicas vs `status.desiredReplicas` delta — ties the
/// fence to a third apiserver-dependent observation. The in-process
/// record is the cheapest sound close (restart loss ≤ one 10s poll
/// of the 5-minute window).
pub(super) fn freshest_since_up(
    status: Option<Duration>,
    in_process: Option<Instant>,
) -> Option<Duration> {
    let in_process = in_process.map(|t| t.elapsed());
    match (status, in_process) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `now() - t` as a non-negative Duration. `None` on parse failure
/// or future timestamp (clock skew between controller restarts) —
/// the [`freshest_since_up`] merge then falls through to the
/// in-process record; if that too is absent the caller treats
/// `None` as "infinitely long ago" (allow scale-down).
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
    // r[verify store.admin.get-load+3]
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
        // Across-pod aggregate: the same fold `poll_max_load_addrs`
        // applies (`LoadAggregate::fold` over the readings vec).
        let readings: Vec<Option<f64>> = [&pod_a, &pod_b]
            .into_iter()
            .map(|r| Some(fold_load(r)))
            .collect();
        let agg = decide::LoadAggregate::fold(&readings).expect("two answers");
        assert_eq!(agg.max, 0.9_f32 as f64);
        assert!(agg.total_coverage(), "both pods answered");
        // Old behavior (pg-only) would have returned 0.7 — proving
        // the new dimension is load-bearing.
        let pg_only = [&pod_a, &pod_b]
            .into_iter()
            .map(|r| r.pg_pool_utilization as f64)
            .fold(f64::MIN, f64::max);
        assert!(agg.max > pg_only);
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

    fn cs_spec(min: i32, max: i32) -> ComponentScalerSpec {
        ComponentScalerSpec {
            target_ref: rio_crds::componentscaler::TargetRef {
                kind: "Deployment".into(),
                name: "rio-store".into(),
            },
            signal: Signal::SchedulerBuilders,
            replicas: rio_crds::componentscaler::Replicas { min, max },
            seed_ratio: 50.0,
            load_endpoint: "rio-store-headless:9002".into(),
            load_thresholds: rio_crds::componentscaler::LoadThresholds::default(),
        }
    }

    fn cs_status(ratio: f64) -> ComponentScalerStatus {
        ComponentScalerStatus {
            learned_ratio: Some(ratio),
            ..Default::default()
        }
    }

    fn cs_total(l: f64) -> Option<decide::LoadAggregate> {
        Some(decide::LoadAggregate {
            max: l,
            answered: 3,
            resolved: 3,
        })
    }

    // r[verify ctrl.scaler.fence-arming]
    /// W14-B1 (bug_021) — proposition: every successful scale-up
    /// leaves a fresh fence regardless of whether the second
    /// (status) write succeeds; population: the transient
    /// apiserver-failure regime (patch_scale lands, patch_status
    /// 5xx — the write-failure edge of the bug_060 window). Pre-fix
    /// RED (verbatim in the commit body): since_up sourced solely
    /// from status → None → `unwrap_or(true)` stabilized → the
    /// EMITTED scale-down (left: 5, right: 6) at the next lull.
    #[test]
    fn w14_b1_status_write_failure_keeps_fence_armed() {
        // r13-allow(decide-seam): the patch-failure scenario is
        // expressed as the state the retried reconcile observes —
        // the in-process record + status stamp the production fold
        // (`freshest_since_up`) consumes.
        let state = crate::reconcilers::ScalerState::default();
        let key = "ns/cs".to_string();
        // Tick N: scale-up. patch_scale returned Ok → the in-process
        // record is stamped at the mutating write's success site.
        state
            .last_scale_up
            .lock()
            .insert(key.clone(), Instant::now());
        // patch_status FAILED (transient 5xx) → status.lastScaleUpTime
        // stays None. The retried reconcile sees current=6 (the
        // /scale patch landed), low builders (the lull after the
        // burst), in-band load.
        let status_since: Option<Duration> = None;
        let since = freshest_since_up(status_since, state.last_scale_up.lock().get(&key).copied());
        let d = decide::decide(
            &cs_spec(2, 14),
            &cs_status(50.0),
            6,
            100,
            cs_total(0.5),
            since,
        );
        // Load-bearing: the EMITTED decision (the walk-down-at-next-
        // lull blast radius — `d.desired`, not gate state).
        assert_eq!(
            d.desired, 6,
            "a transient patch_status failure after a successful \
             scale-up must NOT disarm the 5-min stabilization fence \
             (pre-fix: since_up=None from status only → d.desired=5)"
        );
        // Secondary mechanism check: the merge supplied the fresh
        // in-process record where status was absent.
        assert!(
            since.is_some_and(|d| d < decide::SCALE_DOWN_STABILIZATION),
            "the in-process record arms the fence"
        );
        // The status backfill: the next successful patch_status
        // writes lastScaleUpTime (the durable copy); both records
        // present → the merge picks the freshest (smaller Duration).
        let backfilled = freshest_since_up(Some(Duration::from_secs(10)), Some(Instant::now()));
        assert!(backfilled.is_some_and(|d| d < Duration::from_secs(10)));
    }

    // r[verify ctrl.scaler.fence-arming]
    /// W14-B1b — the R34-w(v) success-site negative: patch_scale
    /// itself failing MUST NOT stamp the in-process record (no
    /// phantom fence). The next reconcile's scale-up is not
    /// suppressed by a stamp from a write that never landed.
    #[test]
    fn w14_b1b_scale_patch_failure_stamps_nothing() {
        let state = crate::reconcilers::ScalerState::default();
        let key = "ns/cs".to_string();
        // Tick N: patch_scale FAILED → reconcile_inner returns Err
        // BEFORE the in-process stamp (`?` at the patch_scale call
        // site precedes the insert). The record stays absent.
        assert!(
            state.last_scale_up.lock().get(&key).is_none(),
            "no successful scale → no in-process record"
        );
        // Tick N+1: high load, current still 5 (the patch never
        // landed). With NO record either side, since_up=None and the
        // scale-UP path is unaffected (the fence gates scale-DOWN
        // only).
        let since = freshest_since_up(None, state.last_scale_up.lock().get(&key).copied());
        assert_eq!(since, None);
        let d = decide::decide(
            &cs_spec(2, 14),
            &cs_status(50.0),
            5,
            200,
            cs_total(0.9),
            since,
        );
        assert_eq!(
            d.desired, 6,
            "a failed scale patch leaves no phantom fence — the \
             next reconcile's scale-up is not suppressed"
        );
        assert!(d.scaled_up);
    }

    // r[verify ctrl.scaler.fence-arming]
    /// W14-B2 — the None/restart polarity, both directions priced.
    /// With BOTH records absent (fresh CR, or controller restart with
    /// status wiped), `decide()` treats since_up=None as "infinitely
    /// long ago" → allow scale-down. R26 examined: absence of both
    /// is genuine "never scaled up" — the bug_021 window (status
    /// absent but a scale-up DID happen) is closed by the in-process
    /// record, so the only None-BOTH case left is the deliberate
    /// one. Conservative-hold (treat None as "just now") would
    /// freeze every fresh CR for 5 minutes from an over-provisioned
    /// chart `replicas` and freeze every restart — rejected; the
    /// `unwrap_or(true)` polarity is KEPT.
    #[test]
    fn w14_b2_restart_none_both_allows_scale_down() {
        // Fresh restart: neither record.
        let since = freshest_since_up(None, None);
        assert_eq!(since, None);
        let d = decide::decide(
            &cs_spec(2, 14),
            &cs_status(50.0),
            8,
            100,
            cs_total(0.5),
            since,
        );
        assert_eq!(
            d.desired, 7,
            "None-both (restart/fresh CR) → allow scale-down \
             (the deliberate polarity; conservative-hold rejected)"
        );
        // Restart with status PRESERVED: the durable stamp suffices
        // alone — the in-process record is the supplement for the
        // status-write-failure edge, never a replacement.
        let since = freshest_since_up(Some(Duration::from_secs(30)), None);
        let d = decide::decide(
            &cs_spec(2, 14),
            &cs_status(50.0),
            8,
            100,
            cs_total(0.5),
            since,
        );
        assert_eq!(
            d.desired, 8,
            "restart with status preserved → the durable stamp \
             alone arms the fence"
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
