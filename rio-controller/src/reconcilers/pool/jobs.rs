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
use kube::api::{Api, DeleteParams, ListParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use tracing::{debug, info, warn};

#[cfg(test)]
use super::job::JOB_TTL_SECS;
use super::job::{
    JOB_REQUEUE, ephemeral_job, is_active_job, is_pending_job, patch_job_pool_status,
    reap_excess_pending, reap_orphan_running, report_deadline_exceeded_jobs,
    report_terminated_pods, spawn_for_each,
};
use super::pod::{self, UpstreamAddrs};
use crate::error::{Error, Result};
use crate::reconcilers::{Ctx, KubeErrorExt, require_namespace};
use rio_crds::pool::{ExecutorKind, Pool};
use rio_proto::types::SpawnIntent;

/// Pod-template annotation carrying `SpawnIntent.intent_id`. Read by
/// the builder via downward-API → `RIO_INTENT_ID` → heartbeat.
pub(crate) const INTENT_ID_ANNOTATION: &str = "rio.build/intent-id";

/// Job-metadata annotation carrying a fingerprint of
/// `SpawnIntent.node_selector`. Compared on each tick so a Pending Job
/// whose selector no longer matches the scheduler's current solve
/// (ICE-backoff spot→on-demand fallback) is reaped instead of
/// NameCollision-blocking the re-solved intent forever.
pub(crate) const INTENT_SELECTOR_ANNOTATION: &str = "rio.build/intent-selector";

/// Log + scratch budget. nix `build-dir` lands in the overlay emptyDir
/// (nix ≥2.30 default = stateDir/builds), but stdout/stderr capture and
/// the daemon's own state live outside. 1 GiB headroom.
const LOG_BUDGET_BYTES: u64 = 1 << 30;
/// Overlay emptyDir sizeLimit headroom multiplier on `disk_bytes`.
/// TODO(ADR-023 phase-2): replace with `headroom(n_eff)` from the SLA
/// estimator (variance-aware). 1.5× is the phase-1 flat fallback.
const OVERLAY_HEADROOM: f64 = 1.5;
/// Margin between the worker's `daemon_timeout` and K8s
/// `activeDeadlineSeconds`, so the worker's `tokio::time::timeout`
/// fires first and emits `CompletionReport{TimedOut}` (telemetry +
/// `handle_timeout_failure` cap-check) before K8s SIGKILLs.
const WORKER_DEADLINE_SLACK_SECS: i64 = 90;

/// `activeDeadlineSeconds` for an ephemeral Job: `intent.
/// deadline_secs` verbatim. The scheduler computes it per-derivation
/// (D7: `wall_p99 × 5` for fitted, `[sla].probe.deadline_secs` for
/// unfitted, clamped `[floor, 86400]`) and `SlaConfig::validate`
/// guarantees `probe.deadline_secs >= 180`, so the intent value is
/// always `>= 180`. No controller-side multiplier or per-kind
/// fallback. `.max(180)` is defensive only — proto default is 0; a
/// 0s deadline would fail the Job at creation, and `< 180` would tie
/// the worker's `daemon_timeout = deadline − 90` against this timer.
// r[impl ctrl.ephemeral.intent-deadline]
pub(super) fn ephemeral_deadline(intent: &SpawnIntent) -> i64 {
    i64::from(intent.deadline_secs).max(180)
}

// r[impl ctrl.pool.ephemeral]
/// Reconcile a Pool: count active Jobs, poll spawn intents, spawn
/// Jobs if work is waiting.
///
/// Status: `replicas` / `readyReplicas` / `desiredReplicas` mean
/// "active Jobs." `desiredReplicas` is the concurrent-Job ceiling
/// (`spec.maxConcurrent`).
pub(super) async fn reconcile(pool: &Pool, ctx: &Ctx) -> Result<Action> {
    // Namespace-missing is `InvalidSpec` not `NotFound` — a Pool CR
    // without `.metadata.namespace` is a cluster-scoped apply error
    // (the CRD is `Namespaced`), not a transient condition.
    let ns = require_namespace(pool)?;
    let name = pool.name_any();
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
    // The no-`.metadata.uid` error only happens on a CR not read from
    // the apiserver — tests that construct one in memory forget this;
    // production reconcile always has it.
    let oref = pool.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("Pool has no metadata.uid (not from apiserver?)".into())
    })?;
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
    // We do NOT subtract `active`: `queued` counts only Ready intents
    // but `active` counts ALL non-terminal Jobs (incl. Running, whose
    // drvs have left the Ready set). Under per-size-class pools that
    // mismatch was bounded by class cutoff (~30s); under one Pool it's
    // bounded by the slowest build, so `queued.sub(active)` starved
    // new Ready drvs for hours (bug_045). `ceiling = None` → uncapped.
    let headroom = ceiling.map_or(usize::MAX, |c| c.saturating_sub(active).max(0) as usize);

    // ---- Reap stale Jobs blocking respawn ----
    // (a) Terminal: a drv that re-enters Ready after its prior Job
    //     went Complete/Failed would NameCollision against the stale
    //     terminal Job for JOB_TTL_SECS (600s). (b) Selector-drift: a
    //     Pending Job whose selector no longer matches the scheduler's
    //     re-solve (ICE-backoff) NameCollision-blocks the new intent
    //     forever. Delete both before the spawn pass.
    //
    // Reap sees the FULL intent set, NOT the headroom-truncated
    // slice: when `ceiling` is set and every active slot is a
    // selector-drifted Pending, headroom=0 → truncated slice is empty
    // → reap's `want.is_empty()` early-return fires → nothing freed →
    // headroom stays 0 forever. Reaping frees slots; it doesn't
    // consume headroom, so the cap doesn't apply.
    let reaped =
        reap_stale_for_intents(&jobs_api, &jobs.items, &intents, &name, pool.spec.kind).await;

    // Names already present (minus what we just reaped) are skipped
    // in the spawn pass to avoid a create()→409 per still-Ready
    // intent every tick.
    let existing_names: HashSet<String> = jobs
        .items
        .iter()
        .filter_map(|j| j.metadata.name.clone())
        .filter(|n| !reaped.contains(n))
        .collect();

    // Filter-existing BEFORE truncate: `headroom = ceiling - active`
    // already accounts for still-Pending Jobs, but those Jobs' drvs
    // (still Ready, not yet heartbeated) ALSO appear in `intents`. A
    // positional `intents[..headroom]` slice spent slots on them then
    // skipped them in `spawn_for_each` — new intents past index
    // `headroom` were never considered even with free slots. Intents
    // are scheduler-side priority-sorted, so `take(headroom)` over
    // genuinely-new work drops lowest-priority, not HashMap-order.
    let to_spawn_intents: Vec<SpawnIntent> = intents
        .iter()
        .filter(|i| {
            !existing_names.contains(&pod::job_name(
                &name,
                pool.spec.kind,
                &intent_suffix(&i.intent_id),
            ))
        })
        .take(headroom)
        .cloned()
        .collect();

    // One pod per intent with that intent's resources + annotation.
    // Headroom truncates; the remainder is picked up next tick after
    // `active` decreases. Under mandatory `[sla]` (Phase 5) the
    // scheduler ALWAYS populates intents — empty list means empty
    // queue → spawns nothing.
    spawn_for_each(
        &jobs_api,
        &to_spawn_intents,
        &existing_names,
        &name,
        |intent| build_job(pool, oref.clone(), &ctx.scheduler, &ctx.store, intent),
    )
    .await;
    // Ack to the scheduler so it arms the Pending-watch (ICE-backoff)
    // timer for intents that have a Pending Job — both newly spawned
    // AND already-Pending-before-this-tick. The latter covers scheduler
    // restart: `pending_intents` is in-memory, so without re-ack a
    // pre-restart Pending Job under deterministic softmax (same
    // selector → no reap → no respawn → no fresh ack) never re-arms.
    // Scheduler-side `or_insert` makes re-ack of a live timer a no-op.
    let pending_job_names: HashSet<String> = jobs
        .items
        .iter()
        .filter(|j| is_pending_job(j))
        .filter_map(|j| j.metadata.name.clone())
        .filter(|n| !reaped.contains(n))
        .collect();
    let to_ack: Vec<SpawnIntent> = intents
        .iter()
        .filter(|i| {
            pending_job_names.contains(&pod::job_name(
                &name,
                pool.spec.kind,
                &intent_suffix(&i.intent_id),
            ))
        })
        .cloned()
        .chain(to_spawn_intents)
        .collect();
    if to_ack.is_empty() {
        debug!(pool = %name, queued, active, ?ceiling, "no Pending intents to ack");
    } else {
        if let Err(e) = ctx
            .admin
            .clone()
            .ack_spawned_intents(rio_proto::types::AckSpawnedIntentsRequest { spawned: to_ack })
            .await
        {
            warn!(pool = %name, error = %e, "ack_spawned_intents failed; ICE-timer not armed this tick");
        }
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
            kind: Some(super::executor_kind_to_proto(pool.spec.kind).into()),
            systems: pool.spec.systems.clone(),
            features: pool.spec.features.clone(),
            filter_features: true,
        })
        .await?
        .into_inner();
    Ok(resp.intents)
}

/// Stable fingerprint of `SpawnIntent.node_selector`: `k=v,k=v` over
/// sorted keys. Empty map → "". Only the intent-supplied selector is
/// fingerprinted (NOT the pool's base selector that
/// `build_executor_pod_spec` merges in) so drift detection compares
/// scheduler decisions, not pool config.
fn selector_fingerprint(sel: &std::collections::HashMap<String, String>) -> String {
    let mut kv: Vec<_> = sel.iter().map(|(k, v)| format!("{k}={v}")).collect();
    kv.sort_unstable();
    kv.join(",")
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

/// Delete Jobs whose name collides with an intent we are about to
/// spawn AND would block that spawn:
///
///   - **Terminal** (Complete/Failed): the deterministic `intent_
///     suffix` makes a finished Job for drv X block respawn of X for
///     [`JOB_TTL_SECS`]; clear that window so the immediate
///     `spawn_for_each` (or worst-case next tick) succeeds.
///     Background-propagation delete — terminal ⇒ `status.{succeeded,
///     failed} > 0` ⇒ the Job controller has already cleared the
///     pod's `batch.kubernetes.io/job-tracking` finalizer, so the
///     orphan-finalizer race does not apply and the Job object can
///     vanish before the pod.
///   - **Pending with stale selector**: the scheduler re-solved
///     (ICE-backoff spot→on-demand) but the prior Pending Job sits
///     unschedulable on the OLD selector and NameCollision-blocks the
///     new intent forever — `reap_excess_pending` won't catch it
///     because `pending == queued`. Foreground-propagation delete —
///     the pod's `job-tracking` finalizer is still live (same
///     reasoning as `reap_excess_pending`).
///
/// A Pending Job whose selector MATCHES the current intent is NOT
/// reaped — that's the intended NameCollision dedupe.
pub(super) async fn reap_stale_for_intents(
    jobs_api: &Api<Job>,
    existing: &[Job],
    intents: &[SpawnIntent],
    pool: &str,
    kind: ExecutorKind,
) -> HashSet<String> {
    use std::collections::HashMap;
    let mut reaped = HashSet::new();
    let want: HashMap<String, String> = intents
        .iter()
        .map(|i| {
            (
                pod::job_name(pool, kind, &intent_suffix(&i.intent_id)),
                selector_fingerprint(&i.node_selector),
            )
        })
        .collect();
    if want.is_empty() {
        return reaped;
    }
    for j in existing {
        let Some(jn) = j.metadata.name.as_deref() else {
            continue;
        };
        let Some(want_sel) = want.get(jn) else {
            continue;
        };
        let (params, why) = if !is_active_job(j) {
            (DeleteParams::background(), "terminal")
        } else if is_pending_job(j)
            && j.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(INTENT_SELECTOR_ANNOTATION))
                .map(String::as_str)
                != Some(want_sel.as_str())
        {
            (DeleteParams::foreground(), "selector-drift")
        } else {
            continue;
        };
        match jobs_api.delete(jn, &params).await {
            Ok(_) => {
                info!(
                    pool, job = %jn, why,
                    "reaped stale Job blocking re-queued intent respawn"
                );
                reaped.insert(jn.to_owned());
            }
            Err(e) if e.is_not_found() => {
                reaped.insert(jn.to_owned());
            }
            Err(e) => {
                warn!(
                    pool, job = %jn, why, error = %e,
                    "failed to reap stale Job; spawn will NameCollision \
                     this tick, retried next"
                );
            }
        }
    }
    reaped
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
    let mut pod_spec = pod::build_executor_pod_spec(pool, scheduler, store);
    apply_intent_resources(&mut pod_spec, pool, intent);
    let mut job = ephemeral_job(
        job_name,
        pool.namespace(),
        oref,
        pod::executor_labels(pool),
        ephemeral_deadline(intent),
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
    // Stamp the selector fingerprint on the JOB metadata (not pod
    // template) so `reap_stale_for_intents` can compare without
    // dereferencing `spec.template`.
    job.metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(
            INTENT_SELECTOR_ANNOTATION.into(),
            selector_fingerprint(&intent.node_selector),
        );
    Ok(job)
}

/// Stamp scheduler-computed `(cores, mem, disk)` onto the executor
/// container's `resources` and the overlay emptyDir's `sizeLimit`.
///
/// `requests == limits` (hard caps, no burst) — ADR-023 §sizing-model.
/// Quantities rendered as raw byte counts (no SI suffix): k8s parses
/// bare integers as base-unit and they roundtrip exactly.
///
/// `ephemeral-storage` = `disk_bytes` (overlay writes, from the SLA
/// model's prjquota fit) + the per-pool FUSE cache budget (input
/// closure, NOT captured by `disk_p90`) + log/scratch headroom. The
/// FUSE budget is [`pod::fuse_cache_bytes`] — the SAME value that
/// sets the `fuse-cache` emptyDir sizeLimit, so kubelet's pod-level
/// sum (writable-layer + logs + disk-backed emptyDirs) cannot exceed
/// the limit before the volume-level limit fires.
// r[impl sched.sla.disk-reaches-ephemeral-storage]
fn apply_intent_resources(pod_spec: &mut PodSpec, pool: &Pool, i: &SpawnIntent) {
    let ephemeral = i
        .disk_bytes
        .saturating_add(pod::fuse_cache_bytes(pool))
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

    // Couple the worker's `daemon_timeout` to the per-intent K8s
    // `activeDeadlineSeconds`: worker fires `WORKER_DEADLINE_SLACK_SECS`
    // BEFORE K8s SIGKILLs, so `CompletionReport{TimedOut}` (primary
    // path) carries telemetry and reaches `handle_timeout_failure`'s
    // cap-check; `DeadlineExceeded` stays the wedged-worker backstop
    // per `r[sched.termination.deadline-exceeded+2]`.
    // `ephemeral_deadline` floors at 180 so `− 90` never underflows
    // the `.max(60)` clamp into a tie.
    let worker_timeout = (ephemeral_deadline(i) - WORKER_DEADLINE_SLACK_SECS).max(60);
    let env = container.env.get_or_insert_with(Vec::new);
    env.push(pod::env(
        "RIO_DAEMON_TIMEOUT_SECS",
        &worker_timeout.to_string(),
    ));

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
        // r[verify ctrl.ephemeral.intent-deadline]
        assert_eq!(
            spec.active_deadline_seconds,
            Some(180),
            "activeDeadlineSeconds backstop (proto-default 0 → 180s floor)"
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
            "build_executor_pod_spec env should be preserved"
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

    // r[verify ctrl.ephemeral.intent-deadline]
    /// D7: `SpawnIntent.deadline_secs` propagates verbatim;
    /// proto-default 0 floors to 180.
    #[test]
    fn intent_deadline_propagates_to_job_spec() {
        let i = SpawnIntent {
            intent_id: "abc123def456".into(),
            deadline_secs: 240,
            ..Default::default()
        };
        assert_eq!(ephemeral_deadline(&i), 240);
        assert_eq!(ephemeral_deadline(&intent("abc")), 180, "0 → 180s floor");
    }

    /// `apply_intent_resources` injects `RIO_DAEMON_TIMEOUT_SECS =
    /// activeDeadlineSeconds − 90` so the worker times out before K8s
    /// SIGKILLs. Regression: a fitted `deadline_secs=15000` build with
    /// the old decoupled 7200s static default looped `TimedOut` at
    /// 7200s while only the K8s side doubled.
    #[test]
    fn build_job_daemon_timeout_couples_to_intent_deadline() {
        let pool = test_pool("p", ExecutorKind::Builder);
        for (deadline, want) in [(15000, "14910"), (240, "150"), (0, "90"), (120, "90")] {
            let i = SpawnIntent {
                intent_id: "abc".into(),
                deadline_secs: deadline,
                ..Default::default()
            };
            let job = build_job(
                &pool,
                crate::fixtures::oref(&pool),
                &test_sched_addrs(),
                &test_store_addrs(),
                &i,
            )
            .unwrap();
            let env = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .map(|p| &p.containers[0])
                .and_then(|c| c.env.as_deref())
                .unwrap();
            let envs = crate::fixtures::env_map(env);
            assert_eq!(
                envs.get("RIO_DAEMON_TIMEOUT_SECS"),
                Some(&want),
                "deadline_secs={deadline} → daemon_timeout={want} \
                 (activeDeadlineSeconds − {WORKER_DEADLINE_SLACK_SECS}; \
                 ephemeral_deadline floored at 180)"
            );
            assert_eq!(
                env.iter()
                    .filter(|e| e.name == "RIO_DAEMON_TIMEOUT_SECS")
                    .count(),
                1,
                "exactly one entry"
            );
        }
    }

    /// `selector_fingerprint` is deterministic over key order. Empty
    /// → "". `build_job` stamps it on Job metadata.annotations so
    /// `reap_stale_for_intents` can compare without dereferencing
    /// `spec.template`.
    #[test]
    fn build_job_stamps_selector_fingerprint() {
        use std::collections::HashMap;
        let a: HashMap<_, _> = [
            ("karpenter.sh/capacity-type".into(), "spot".into()),
            ("rio.build/hw-band".into(), "mid".into()),
        ]
        .into();
        let b: HashMap<_, _> = [
            ("rio.build/hw-band".into(), "mid".into()),
            ("karpenter.sh/capacity-type".into(), "spot".into()),
        ]
        .into();
        assert_eq!(selector_fingerprint(&a), selector_fingerprint(&b));
        assert_eq!(
            selector_fingerprint(&a),
            "karpenter.sh/capacity-type=spot,rio.build/hw-band=mid"
        );
        assert_eq!(selector_fingerprint(&HashMap::new()), "");

        let pool = test_pool("p", ExecutorKind::Builder);
        let i = SpawnIntent {
            intent_id: "abc".into(),
            node_selector: a,
            ..Default::default()
        };
        let job = build_job(
            &pool,
            crate::fixtures::oref(&pool),
            &test_sched_addrs(),
            &test_store_addrs(),
            &i,
        )
        .unwrap();
        assert_eq!(
            job.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(INTENT_SELECTOR_ANNOTATION))
                .map(String::as_str),
            Some("karpenter.sh/capacity-type=spot,rio.build/hw-band=mid"),
        );
    }

    /// bug_074: fetcher overlay emptyDir is disk-backed (NOT
    /// `medium: Memory`). Under ADR-023 `limits.memory` is RSS-only
    /// and `disk_bytes` budgets `ephemeral-storage`; a tmpfs overlay
    /// charged the unpack against memory → OOM while the disk
    /// reservation sat unused.
    #[test]
    fn fetcher_overlay_is_disk_backed() {
        let pool = test_pool("f", ExecutorKind::Fetcher);
        let job = build_job(
            &pool,
            crate::fixtures::oref(&pool),
            &test_sched_addrs(),
            &test_store_addrs(),
            &SpawnIntent {
                intent_id: "abc".into(),
                disk_bytes: 8 << 30,
                ..Default::default()
            },
        )
        .unwrap();
        let overlay = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|p| p.volumes.as_ref())
            .and_then(|v| v.iter().find(|v| v.name == "overlays"))
            .and_then(|v| v.empty_dir.as_ref())
            .expect("fetcher pod has overlays emptyDir");
        assert_eq!(
            overlay.medium, None,
            "fetcher overlay must be disk-backed so disk_bytes budgets \
             ephemeral-storage and quota::peak_bytes() sees prjquota"
        );
        assert!(overlay.size_limit.is_some(), "sizeLimit still applied");
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
            Quantity(((40 + 8 + 1) * GI).to_string()),
            "disk_bytes + BUILDER_FUSE_CACHE_BYTES + LOG_BUDGET_BYTES"
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

    /// The `fuse-cache` emptyDir sizeLimit and the FUSE-cache addend in
    /// the container's `ephemeral-storage` limit MUST come from the
    /// same per-pool value. Kubelet sums disk-backed emptyDirs against
    /// the container limit, so a sizeLimit larger than the budget
    /// evicts on the pod-level limit before the volume cap — and
    /// `disk_p90` (overlay prjquota only) never learns the input-
    /// closure size, so every fresh drv_hash re-climbs the floor.
    ///
    /// Third arm: `PoolSpec.fuse_cache_bytes` override is honoured for
    /// BOTH sites — helm-rendered prod Pools set 50Gi, which would make
    /// every pod request ≥51Gi ephemeral-storage on small-disk nodes
    /// (k3s VM tests) without the override.
    #[test]
    fn fuse_cache_budget_matches_sizelimit() {
        const GI: u64 = 1 << 30;
        for (kind, override_, expect) in [
            (ExecutorKind::Builder, None, pod::BUILDER_FUSE_CACHE_BYTES),
            (ExecutorKind::Fetcher, None, pod::FETCHER_FUSE_CACHE_BYTES),
            (ExecutorKind::Builder, Some(4 * GI), 4 * GI),
        ] {
            let mut pool = test_pool("p", kind);
            pool.spec.fuse_cache_bytes = override_;
            let job = build_job(
                &pool,
                crate::fixtures::oref(&pool),
                &test_sched_addrs(),
                &test_store_addrs(),
                &SpawnIntent {
                    intent_id: "abc".into(),
                    disk_bytes: 5 * GI,
                    ..Default::default()
                },
            )
            .unwrap();
            let pod_spec = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .unwrap();
            let fuse = pod_spec
                .volumes
                .as_ref()
                .and_then(|v| v.iter().find(|v| v.name == "fuse-cache"))
                .and_then(|v| v.empty_dir.as_ref())
                .and_then(|e| e.size_limit.as_ref())
                .expect("fuse-cache emptyDir has sizeLimit");
            assert_eq!(fuse, &Quantity(expect.to_string()), "{kind:?} sizeLimit");
            let eph = pod_spec.containers[0]
                .resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .map(|l| l["ephemeral-storage"].clone())
                .unwrap();
            assert_eq!(
                eph,
                Quantity((5 * GI + expect + LOG_BUDGET_BYTES).to_string()),
                "{kind:?} ephemeral-storage budget must include the SAME \
                 fuse-cache bytes as the emptyDir sizeLimit"
            );
        }
    }
}
