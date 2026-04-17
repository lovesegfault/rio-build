//! Pool Job-per-build reconciler.
//!
//!   1. Each `apply()` tick polls `GetSpawnIntents` (filtered by
//!      `{kind, systems, features}`) via the same `ctx.admin` client
//!      the finalizer uses for DrainExecutor.
//!   2. If the scheduler returned intents and active Jobs for this
//!      pool < `spec.maxConcurrent`, spawn one Job per intent (up to
//!      the ceiling).
//!   3. Each Job runs one rio-builder pod → worker exits after one
//!      build → pod terminates → `ttlSecondsAfterFinished`
//!      ([`JOB_TTL_SECS`]) reaps the Job.
//!
//! From the scheduler's perspective a Job pod is just an executor:
//! it heartbeats in, gets a dispatch, sends CompletionReport,
//! disconnects. The "ephemeral" property is purely worker-side
//! (exit after one build) + controller-side (Job lifecycle).
//!
//! # Job naming
//!
//! `rio-{kind}-{pool}-{intent-suffix}` — suffix derives from
//! `intent_id` (= drv_hash, nixbase32) so a re-polled still-Ready
//! intent re-creates the SAME Job name and the apiserver's
//! NameCollision dedupes (cold-start re-spawn would otherwise fire
//! one pod per reconcile tick).
//!
//! # Zero cross-build state
//!
//! Fresh pod = fresh emptyDir for FUSE cache + overlays. An
//! untrusted tenant CANNOT leave poisoned cache entries for the
//! next build — there is no "next build" on that pod.

use std::collections::{BTreeMap, HashSet};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;
use kube::api::{Api, DeleteParams, ListParams};
use kube::runtime::controller::Action;
use tracing::{debug, info, warn};

#[cfg(test)]
use super::job::JOB_TTL_SECS;
use super::job::{
    JOB_REQUEUE, JobReconcilePrologue, ephemeral_job, is_active_job, job_reconcile_prologue,
    patch_job_pool_status, reap_excess_pending, reap_orphan_running, report_deadline_exceeded_jobs,
    report_terminated_pods, spawn_count, spawn_for_each,
};
use super::pod::{self, UpstreamAddrs, executor_kind_to_proto};
use crate::error::Result;
use crate::reconcilers::{Ctx, KubeErrorExt};
use rio_crds::pool::{ExecutorKind, Pool};
use rio_proto::types::SpawnIntent;

/// Pod-template annotation carrying `SpawnIntent.intent_id`. Read by
/// the builder via downward-API → `RIO_INTENT_ID` → heartbeat.
pub(crate) const INTENT_ID_ANNOTATION: &str = "rio.build/intent-id";

/// ADR-023 ephemeral-storage budget for the FUSE cache emptyDir. Added
/// to `SpawnIntent.disk_bytes` so the kubelet's disk-pressure eviction
/// accounts for both the build's overlay writes AND the input-closure
/// cache. Same 8 GiB as the per-pod cache cap in the SLA model.
const FUSE_CACHE_BUDGET_BYTES: u64 = 8 * (1 << 30);
/// Log + scratch budget. nix `build-dir` lands in the overlay emptyDir
/// (nix ≥2.30 default = stateDir/builds), but stdout/stderr capture and
/// the daemon's own state live outside. 1 GiB headroom.
const LOG_BUDGET_BYTES: u64 = 1 << 30;
/// Overlay emptyDir sizeLimit headroom multiplier on `disk_bytes`.
/// TODO(ADR-023 phase-2): replace with `headroom(n_eff)` from the SLA
/// estimator (variance-aware). 1.5× is the phase-1 flat fallback.
const OVERLAY_HEADROOM: f64 = 1.5;

/// Fallback `activeDeadlineSeconds` when neither the SpawnIntent nor
/// `spec.deadline_seconds` carries a deadline. Builder: 3600 (1h) —
/// long enough that a matched dispatch + build completes; short
/// enough that a wrong-pool spawn doesn't leak. Fetcher: 300 (5min)
/// — fetches are network-bound and short; a stuck download is the
/// failure mode, not a 90min compile.
const DEFAULT_BUILDER_DEADLINE_SECS: i64 = 3600;
const DEFAULT_FETCHER_DEADLINE_SECS: i64 = 300;

/// `activeDeadlineSeconds` for an ephemeral Job. Precedence:
///   1. `intent.deadline_secs` — scheduler-computed per-derivation
///      bound (D7: `wall_p99 × 5` for fitted, `[sla].probe.
///      deadline_secs` for unfitted, clamped `[floor, 86400]`). The
///      scheduler owns the 5× headroom; no controller-side multiplier.
///   2. `spec.deadline_seconds` — explicit operator override, verbatim.
///   3. Per-kind default.
///
/// `intent.deadline_secs == 0` (proto default) is treated as unset
/// and falls through.
// r[impl ctrl.ephemeral.intent-deadline]
pub(super) fn ephemeral_deadline(spec: &rio_crds::pool::PoolSpec, intent: &SpawnIntent) -> i64 {
    if intent.deadline_secs > 0 {
        return i64::from(intent.deadline_secs);
    }
    if let Some(explicit) = spec.deadline_seconds {
        return i64::from(explicit);
    }
    match spec.kind {
        ExecutorKind::Builder => DEFAULT_BUILDER_DEADLINE_SECS,
        ExecutorKind::Fetcher => DEFAULT_FETCHER_DEADLINE_SECS,
    }
}

// r[impl ctrl.pool.ephemeral]
/// Reconcile a Pool: count active Jobs, poll spawn intents, spawn
/// Jobs if work is waiting.
///
/// Status: `replicas` / `readyReplicas` / `desiredReplicas` mean
/// "active Jobs." `desiredReplicas` is the concurrent-Job ceiling
/// (`spec.maxConcurrent`).
pub(super) async fn reconcile(pool: &Pool, ctx: &Ctx) -> Result<Action> {
    let JobReconcilePrologue {
        ns,
        name,
        jobs_api,
        oref,
        scheduler,
        store,
    } = job_reconcile_prologue(pool, ctx)?;
    let ceiling = pool.spec.max_concurrent.map(|c| c as i32);

    // ---- Poll spawn intents ----
    // One GetSpawnIntents RPC per reconcile per pool. If the scheduler
    // is unreachable: log + treat as queued=0 + requeue. Next tick
    // retries. We ALSO set a SchedulerUnreachable condition on the
    // Pool so operators can see WHY nothing is spawning.
    let (intents, scheduler_err): (Vec<SpawnIntent>, Option<String>) =
        match queued_for_pool(ctx, pool).await {
            Ok(intents) => (intents, None),
            Err(e) => {
                warn!(
                    pool = %name, error = %e,
                    "spawn-intents poll failed; treating as queued=0, will retry"
                );
                (Vec::new(), Some(e.to_string()))
            }
        };
    let queued = intents.len().min(u32::MAX as usize) as u32;

    // ---- Count active Jobs for this pool ----
    // ORDERING (I-183): list AFTER the queued poll. The reap step
    // compares `pending` against `queued`. Polling first keeps the
    // comparison coherent.
    let jobs = jobs_api
        .list(&ListParams::default().labels(&format!("{}={name}", super::POOL_LABEL)))
        .await?;
    let active: i32 = jobs
        .items
        .iter()
        .filter(|j| is_active_job(j))
        .count()
        .try_into()
        .unwrap_or(i32::MAX);

    // ---- Spawn decision ----
    // `ceiling = None` → uncapped: headroom = MAX, so spawn_count
    // reduces to `queued - active`.
    let headroom = ceiling.map_or(u32::MAX, |c| c.saturating_sub(active).max(0) as u32);
    let to_spawn = spawn_count(queued, active as u32, headroom);

    // ---- Reap terminal Jobs blocking respawn ----
    // intent_suffix(drv_hash) is deterministic so a re-polled intent
    // for a STILL-ACTIVE Job NameCollisions (the intended dedupe).
    // But a drv that re-enters Ready after its prior Job went
    // Complete/Failed (resubmit-after-TimedOut, transient retry,
    // Cancel-then-resubmit) would NameCollision against the stale
    // terminal Job for JOB_TTL_SECS (600s) — the re-queued drv
    // never gets a worker. Delete those before the spawn pass.
    reap_terminal_for_intents(
        &jobs_api,
        &jobs.items,
        intents.iter().take(to_spawn as usize),
        &name,
        pool.spec.kind,
    )
    .await;

    // One pod per intent with that intent's resources + annotation.
    // `take(to_spawn)` truncates; the remainder is picked up next
    // tick after `active` decreases. Under mandatory `[sla]` (Phase
    // 5) the scheduler ALWAYS populates intents — empty list means
    // empty queue → `to_spawn=0` → `take(0)` spawns nothing.
    spawn_for_each(
        &jobs_api,
        intents.iter().take(to_spawn as usize),
        &name,
        |intent| build_job(pool, oref.clone(), &scheduler, &store, intent),
    )
    .await;
    if to_spawn == 0 {
        debug!(pool = %name, queued, active, ?ceiling, "no ephemeral Jobs to spawn");
    }

    // ---- Reap excess Pending ----
    // I-183: spawn-only is half a control loop. `None` when scheduler
    // unreachable: reap is fail-CLOSED (spawn is fail-open).
    let queued_known = scheduler_err.is_none().then_some(queued);
    reap_excess_pending(&jobs_api, &jobs.items, queued_known, &name).await;

    // ---- Reap orphan Running ----
    // I-165: a builder stuck in D-state (FUSE wait, OOM-loop) can't
    // self-exit and never disconnects, so the scheduler never
    // reassigns. After ORPHAN_REAP_GRACE (5min), any Running Job the
    // scheduler doesn't consider busy is deleted.
    reap_orphan_running(&jobs_api, &jobs.items, ctx, &name).await;

    // ---- Report terminations ----
    report_terminated_pods(ctx, &ns, &name).await;
    report_deadline_exceeded_jobs(ctx, &jobs.items).await;

    // ---- Status patch ----
    patch_job_pool_status(
        ctx,
        pool.status.as_ref(),
        &ns,
        &name,
        active,
        active,
        ceiling.unwrap_or(queued as i32),
        scheduler_err.as_deref(),
    )
    .await?;

    Ok(Action::requeue(JOB_REQUEUE))
}

/// Per-drv spawn intents relevant to THIS pool.
///
/// D5: queries `GetSpawnIntents` filtered server-side by
/// `{kind=spec.kind, systems=spec.systems, features=spec.features}`.
/// The scheduler applies the same {system, feature} subset checks
/// `hard_filter` would (I-107/I-143/I-176/I-181), so every returned
/// intent is one this pool's workers could accept.
// r[impl ctrl.pool.fetcher-spawn-builtin]
async fn queued_for_pool(
    ctx: &Ctx,
    pool: &Pool,
) -> std::result::Result<Vec<SpawnIntent>, tonic::Status> {
    // I-176: `filter_features=true` even when `features` is empty: a
    // featureless pool then sees only featureless work. Fetcher pools
    // have `features=[]` always (FODs route by is_fixed_output alone).
    let resp = ctx
        .admin
        .clone()
        .get_spawn_intents(rio_proto::types::GetSpawnIntentsRequest {
            kind: Some(executor_kind_to_proto(pool.spec.kind).into()),
            systems: pool.spec.systems.clone(),
            features: pool.spec.features.clone(),
            filter_features: true,
        })
        .await?
        .into_inner();
    Ok(resp.intents)
}

/// DNS-1123-safe deterministic suffix from `intent_id`. In production
/// `intent_id == drv_hash` (nixbase32, already lowercase-alnum); the
/// filter is belt-and-suspenders for the proto's "opaque" contract.
fn intent_suffix(intent_id: &str) -> String {
    let s: String = intent_id
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(12)
        .collect();
    // Degenerate (all-filtered) — pad so the resulting Job name is
    // still valid DNS-1123 (no trailing hyphen).
    if s.is_empty() { "0".into() } else { s }
}

/// Delete NOT-active Jobs whose name collides with an intent we are
/// about to spawn. The deterministic `intent_suffix` makes a
/// Completed/Failed Job for drv X block respawn of X for
/// [`JOB_TTL_SECS`]; this clears that window so the immediate
/// `spawn_for_each` (or worst-case the next tick) succeeds.
///
/// Background-propagation delete: terminal ⇒ `status.{succeeded,
/// failed} > 0` ⇒ the Job controller has already cleared the pod's
/// `batch.kubernetes.io/job-tracking` finalizer, so the orphan-
/// finalizer race that forces foreground on the Pending/Running reaps
/// does not apply. Background lets the Job object vanish before the
/// pod (TTL would have done the same), so the create that follows
/// has the best chance of landing in the SAME tick.
async fn reap_terminal_for_intents<'a, I>(
    jobs_api: &Api<Job>,
    existing: &[Job],
    intents: I,
    pool: &str,
    kind: ExecutorKind,
) where
    I: Iterator<Item = &'a SpawnIntent>,
{
    let want: HashSet<String> = intents
        .map(|i| pod::job_name(pool, kind, &intent_suffix(&i.intent_id)))
        .collect();
    if want.is_empty() {
        return;
    }
    for j in existing.iter().filter(|j| !is_active_job(j)) {
        let Some(jn) = j.metadata.name.as_deref() else {
            continue;
        };
        if !want.contains(jn) {
            continue;
        }
        match jobs_api.delete(jn, &DeleteParams::background()).await {
            Ok(_) => {
                info!(
                    pool, job = %jn,
                    "reaped terminal Job blocking re-queued intent respawn"
                );
            }
            Err(e) if e.is_not_found() => {}
            Err(e) => {
                warn!(
                    pool, job = %jn, error = %e,
                    "failed to reap terminal Job; spawn will NameCollision \
                     this tick, retried next"
                );
            }
        }
    }
}

/// Build a K8s Job for one ephemeral worker pod.
///
/// Job-specific settings:
///   - `restartPolicy: Never` — if the worker crashes, the Job goes
///     Failed. The SCHEDULER owns retry.
///   - `backoffLimit: 0` — same reasoning. One attempt.
///   - `ttlSecondsAfterFinished: 600` — K8s TTL controller reaps.
///   - `activeDeadlineSeconds` — backstop for hung builds + wrong-
///     pool spawns.
// r[impl ctrl.pool.ephemeral]
// r[impl ctrl.pool.ephemeral-deadline]
// r[impl ctrl.ephemeral.intent-deadline]
pub(super) fn build_job(
    pool: &Pool,
    oref: OwnerReference,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
    intent: &SpawnIntent,
) -> Result<Job> {
    let pool_name = pool.name_any();
    // Suffix derives from `intent_id` so a re-polled still-Ready
    // intent re-creates the SAME Job name and the apiserver's
    // NameCollision dedupes.
    let suffix = intent_suffix(&intent.intent_id);
    let job_name = pod::job_name(&pool_name, pool.spec.kind, &suffix);
    let mut pod_spec = super::build_pod_spec(pool, scheduler, store);
    apply_intent_resources(&mut pod_spec, intent);
    let mut job = ephemeral_job(
        job_name,
        pool.namespace(),
        oref,
        super::labels(pool),
        ephemeral_deadline(&pool.spec, intent),
        pod_spec,
    );
    // Stamp `rio.build/intent-id` on the pod template so the builder
    // reads it via downward-API → `RIO_INTENT_ID` → heartbeat →
    // scheduler matches the pod to its pre-computed assignment.
    job.spec
        .as_mut()
        .and_then(|s| s.template.metadata.as_mut())
        .and_then(|m| m.annotations.as_mut())
        .expect("ephemeral_job sets template.metadata.annotations")
        .insert(INTENT_ID_ANNOTATION.into(), intent.intent_id.clone());
    Ok(job)
}

/// Stamp scheduler-computed `(cores, mem, disk)` onto the executor
/// container's `resources` and the overlay emptyDir's `sizeLimit`.
///
/// `requests == limits` (hard caps, no burst) — ADR-023 §sizing-model.
/// Quantities rendered as raw byte counts (no SI suffix): k8s parses
/// bare integers as base-unit and they roundtrip exactly.
// r[impl sched.sla.disk-reaches-ephemeral-storage]
fn apply_intent_resources(pod_spec: &mut PodSpec, i: &SpawnIntent) {
    let ephemeral = i
        .disk_bytes
        .saturating_add(FUSE_CACHE_BUDGET_BYTES)
        .saturating_add(LOG_BUDGET_BYTES);
    let map: BTreeMap<String, Quantity> = BTreeMap::from([
        ("cpu".into(), Quantity(i.cores.to_string())),
        ("memory".into(), Quantity(i.mem_bytes.to_string())),
        ("ephemeral-storage".into(), Quantity(ephemeral.to_string())),
    ]);
    let container = pod_spec
        .containers
        .first_mut()
        .expect("build_executor_pod_spec emits exactly one container");
    container.resources = Some(ResourceRequirements {
        requests: Some(map.clone()),
        limits: Some(map),
        ..Default::default()
    });

    // ADR-023 phase-13: per-(band, cap) targeting. Merge into the
    // existing nodeSelector; intent keys win on collision.
    if !i.node_selector.is_empty() {
        let ns = pod_spec.node_selector.get_or_insert_with(BTreeMap::new);
        for (k, v) in &i.node_selector {
            ns.insert(k.clone(), v.clone());
        }
    }

    // Overlay emptyDir sizeLimit = disk_bytes × headroom.
    let overlay_limit = (i.disk_bytes as f64 * OVERLAY_HEADROOM) as u64;
    if let Some(volumes) = pod_spec.volumes.as_mut() {
        for v in volumes.iter_mut() {
            if v.name == "overlays"
                && let Some(ed) = v.empty_dir.as_mut()
            {
                ed.size_limit = Some(Quantity(overlay_limit.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{test_pool, test_sched_addrs, test_store_addrs};

    fn intent(id: &str) -> SpawnIntent {
        SpawnIntent {
            intent_id: id.into(),
            ..Default::default()
        }
    }

    /// Built Job has all the load-bearing settings. If any of these
    /// drift, the Job reconciler breaks silently:
    ///   - restartPolicy != Never → K8s rejects the Job on create
    ///   - backoffLimit > 0 → K8s retries on crash, scheduler ALSO
    ///     retries → duplicate build
    ///   - ttlSecondsAfterFinished missing → completed Jobs
    ///     accumulate forever
    ///   - ownerReference missing → Pool delete leaves orphan Jobs
    // r[verify ctrl.pool.ephemeral]
    #[test]
    fn job_spec_load_bearing_fields() {
        let pool = test_pool("eph-pool", ExecutorKind::Builder);
        let oref = crate::fixtures::oref(&pool);
        let job = build_job(
            &pool,
            oref,
            &test_sched_addrs(),
            &test_store_addrs(),
            &intent("abc123"),
        )
        .unwrap();

        let orefs = job.metadata.owner_references.as_ref().unwrap();
        assert_eq!(orefs[0].kind, "Pool");
        assert_eq!(orefs[0].controller, Some(true));

        let labels = job.metadata.labels.as_ref().unwrap();
        assert_eq!(labels.get("rio.build/pool"), Some(&"eph-pool".to_string()));

        let spec = job.spec.as_ref().unwrap();
        assert_eq!(spec.backoff_limit, Some(0), "K8s must not retry");
        assert_eq!(spec.parallelism, Some(1), "one pod per Job");
        assert_eq!(
            spec.ttl_seconds_after_finished,
            Some(JOB_TTL_SECS),
            "completed Jobs must auto-reap"
        );
        // r[verify ctrl.pool.ephemeral-deadline]
        assert_eq!(
            spec.active_deadline_seconds,
            Some(DEFAULT_BUILDER_DEADLINE_SECS),
            "activeDeadlineSeconds backstop missing"
        );

        let pod_anns = spec
            .template
            .metadata
            .as_ref()
            .and_then(|m| m.annotations.as_ref())
            .expect("pod template must have annotations");
        assert_eq!(
            pod_anns
                .get("karpenter.sh/do-not-disrupt")
                .map(String::as_str),
            Some("true"),
            "I-126: ephemeral pods must opt out of karpenter disruption"
        );
        assert_eq!(
            pod_anns.get(INTENT_ID_ANNOTATION),
            Some(&"abc123".to_string()),
            "intent_id annotation feeds RIO_INTENT_ID downward-API"
        );

        let pod_spec = spec.template.spec.as_ref().unwrap();
        assert_eq!(
            pod_spec.restart_policy.as_deref(),
            Some("Never"),
            "K8s rejects Jobs with restartPolicy=Always"
        );
        assert!(
            pod_spec.affinity.is_none() && pod_spec.topology_spread_constraints.is_none(),
            "I-090: ephemeral Jobs bin-pack (no anti-affinity/spread)"
        );

        let env = pod_spec.containers[0].env.as_ref().unwrap();
        assert!(
            env.iter().any(|e| e.name == "RIO_SCHEDULER__ADDR"),
            "build_pod_spec env should be preserved"
        );
    }

    /// Job name derives from intent_id so re-poll dedupes.
    #[test]
    fn job_name_format() {
        let pool = test_pool("eph-pool", ExecutorKind::Builder);
        let oref = crate::fixtures::oref(&pool);
        let job = build_job(
            &pool,
            oref,
            &test_sched_addrs(),
            &test_store_addrs(),
            &intent("0a1b2c3d4f5g6h7i"),
        )
        .unwrap();
        assert_eq!(
            job.metadata.name.as_deref(),
            Some("rio-builder-eph-pool-0a1b2c3d4f5g")
        );
    }

    // r[verify ctrl.pool.ephemeral-deadline]
    /// `spec.deadline_seconds` propagates verbatim when no intent
    /// carries a deadline.
    #[test]
    fn ephemeral_deadline_some_propagates_to_job_spec() {
        let mut pool = test_pool("eph-pool", ExecutorKind::Builder);
        pool.spec.deadline_seconds = Some(7200);
        let oref = crate::fixtures::oref(&pool);
        let job = build_job(
            &pool,
            oref,
            &test_sched_addrs(),
            &test_store_addrs(),
            &intent("abc"),
        )
        .unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(7200)
        );
    }

    // r[verify ctrl.ephemeral.intent-deadline]
    /// D7: `SpawnIntent.deadline_secs` wins over BOTH the spec
    /// override and the default. `deadline_secs == 0` is unset.
    #[test]
    fn intent_deadline_propagates_to_job_spec() {
        let mut pool = test_pool("eph-pool", ExecutorKind::Builder);
        pool.spec.deadline_seconds = Some(7200);
        let i = SpawnIntent {
            intent_id: "abc123def456".into(),
            deadline_secs: 240,
            ..Default::default()
        };
        assert_eq!(ephemeral_deadline(&pool.spec, &i), 240);

        let zero = intent("abc");
        assert_eq!(ephemeral_deadline(&pool.spec, &zero), 7200);

        pool.spec.deadline_seconds = None;
        assert_eq!(
            ephemeral_deadline(&pool.spec, &zero),
            DEFAULT_BUILDER_DEADLINE_SECS
        );

        // Per-kind default.
        let fetcher = test_pool("f", ExecutorKind::Fetcher);
        assert_eq!(
            ephemeral_deadline(&fetcher.spec, &zero),
            DEFAULT_FETCHER_DEADLINE_SECS,
            "default 5min, not Builder's 1h — stuck download is the failure mode"
        );
    }

    /// `intent_suffix` is deterministic and DNS-1123-safe.
    #[test]
    fn intent_suffix_deterministic_and_dns_safe() {
        let h = "0a1b2c3d4f5g6h7i8j9k0l1m2n3p4q5r";
        assert_eq!(intent_suffix(h), "0a1b2c3d4f5g");
        assert_eq!(intent_suffix(h), intent_suffix(h), "deterministic");
        assert_eq!(intent_suffix("FOO-bar.baz/9"), "barbaz9");
        assert_eq!(intent_suffix("---"), "0");
    }

    /// ADR-023: `build_job` stamps the scheduler-computed resources
    /// onto the executor container and the overlay emptyDir.
    // r[verify sched.sla.disk-reaches-ephemeral-storage]
    #[test]
    fn build_job_with_intent_computed_resources() {
        const GI: u64 = 1 << 30;
        let pool = test_pool("eph-pool", ExecutorKind::Builder);
        let oref = crate::fixtures::oref(&pool);
        let i = SpawnIntent {
            intent_id: "i-abc".into(),
            cores: 8,
            mem_bytes: 16 * GI,
            disk_bytes: 40 * GI,
            node_selector: [
                ("rio.build/hw-band".into(), "mid".into()),
                ("karpenter.sh/capacity-type".into(), "spot".into()),
            ]
            .into(),
            ..Default::default()
        };
        let job = build_job(&pool, oref, &test_sched_addrs(), &test_store_addrs(), &i).unwrap();

        let tmpl = &job.spec.as_ref().unwrap().template;
        assert_eq!(
            job.metadata.name.as_deref(),
            Some("rio-builder-eph-pool-iabc")
        );

        let pod_spec = tmpl.spec.as_ref().unwrap();
        let res = pod_spec.containers[0].resources.as_ref().unwrap();
        let req = res.requests.as_ref().unwrap();
        assert_eq!(req["cpu"], Quantity("8".into()));
        assert_eq!(req["memory"], Quantity((16 * GI).to_string()));
        assert_eq!(
            req["ephemeral-storage"],
            Quantity(((40 + 8 + 1) * GI).to_string())
        );
        assert_eq!(
            res.limits.as_ref(),
            Some(req),
            "limits == requests (hard caps, no burst)"
        );

        let overlay = pod_spec
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "overlays")
            .unwrap();
        assert_eq!(
            overlay.empty_dir.as_ref().unwrap().size_limit,
            Some(Quantity((60 * GI).to_string()))
        );
    }
}
