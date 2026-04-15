//! BuilderPool Job-per-build reconciler.
//!
//!   1. Each `apply()` tick polls `ClusterStatus.queued_derivations`
//!      via the same `ctx.admin` client the finalizer uses for
//!      DrainExecutor.
//!   2. If `queued > 0` and active Jobs for this pool <
//!      `spec.maxConcurrent`, spawn Jobs (one per outstanding
//!      derivation, up to the ceiling).
//!   3. Each Job runs one rio-builder pod → worker exits after one
//!      build → pod terminates → `ttlSecondsAfterFinished`
//!      ([`JOB_TTL_SECS`]) reaps the Job.
//!
//! From the scheduler's perspective a Job pod is just an executor:
//! it heartbeats in, gets a dispatch, sends CompletionReport,
//! disconnects. The "ephemeral" property is purely worker-side
//! (exit after one build) + controller-side (Job lifecycle).
//!
//! # Why not a Scheduler→Controller RPC
//!
//! A push-mode RPC (scheduler calls controller at dispatch time)
//! was considered and rejected. It would require: controller gains
//! a gRPC server (it has none today), scheduler gains a "pool"
//! concept (it has none — only workers with size_class), and new
//! connection management / RBAC / NetworkPolicy.
//!
//! Polling ClusterStatus achieves the same outcome (Job spawned when
//! work exists) with existing infrastructure. Latency is one
//! reconciler requeue interval (~10s). For the "untrusted multi-
//! tenant" use case where isolation > throughput, 10s added latency
//! is acceptable. If
//! sub-second dispatch later becomes a hard requirement, an RPC
//! path can be reintroduced WITH an implementer — don't land
//! declaration-only proto (the previous speculative
//! `ControllerService.CreateEphemeralWorker` was removed for
//! exactly that reason).
//!
//! # Job naming
//!
//! `rio-builder-{pool}-{random-suffix}` — random because we don't have an
//! assignment_id at spawn time (the scheduler picks the derivation
//! AFTER the worker heartbeats). 6 lowercase-alnum chars: 36^6 ≈
//! 2 billion combinations; with [`JOB_TTL_SECS`] reaping and
//! realistic build rates, collision is effectively impossible. K8s
//! would reject on collision anyway (409 AlreadyExists) and next
//! tick retries.
//!
//! # Zero cross-build state
//!
//! Fresh pod = fresh emptyDir for FUSE cache + overlays. An
//! untrusted tenant CANNOT leave poisoned cache entries for the
//! next build — there is no "next build" on that pod.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;
use kube::api::ListParams;
use kube::runtime::controller::Action;
use tracing::{debug, warn};

use crate::error::Result;
use crate::reconcilers::Ctx;
#[cfg(test)]
use crate::reconcilers::common::job::JOB_TTL_SECS;
use crate::reconcilers::common::job::{
    DEADLINE_SLACK_SECS, EPHEMERAL_TGPS, JOB_REQUEUE, JobReconcilePrologue, ephemeral_job,
    is_active_job, job_reconcile_prologue, patch_job_pool_status, random_suffix,
    reap_excess_pending, reap_orphan_running, report_deadline_exceeded_jobs,
    report_terminated_pods, spawn_count, spawn_for_each, spawn_n,
};
use crate::reconcilers::common::pod::{self, ExecutorKind};
use rio_crds::builderpool::BuilderPool;
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

use super::builders::{self, UpstreamAddrs};

/// Fallback `activeDeadlineSeconds` when neither `ephemeral_deadline_
/// seconds` nor `size_class_cutoff_secs` is set (standalone unclassed
/// BuilderPool). 3600 (1h): long enough that a matched dispatch +
/// build completes in the common case; short enough that a wrong-pool
/// spawn (worker heartbeats but never matches dispatch — queue depth
/// was for a different pool's system/size_class) doesn't leak for the
/// life of the cluster. BuilderPoolSet children always carry
/// `size_class_cutoff_secs`, so this fires only for hand-authored
/// unclassed pools.
const DEFAULT_EPHEMERAL_DEADLINE_SECS: i64 = 3600;

/// I-200: worker-side `daemon_timeout_secs = cutoffSecs × DEADLINE_
/// MULTIPLIER`. 5×: a build still going at 5× its class's upper-bound
/// prediction is either hung or grossly misclassified — the worker's
/// own `tokio::time::timeout` (stderr_loop.rs) reports `BuildStatus::
/// TimedOut` → `r[sched.timeout.promote-on-exceed]` promotes
/// `size_class_floor` and the next dispatch lands on the next-larger
/// class (with a 5× longer timer). tiny (cutoff 30s) → 150s, small
/// (120s) → 600s, medium (600s) → 3000s, large (1800s) → 9000s,
/// xlarge (7200s) → 36000s. The k8s `activeDeadlineSeconds` BACKSTOP
/// is this + [`EPHEMERAL_TGPS`] + [`DEADLINE_SLACK_SECS`] (=+90s) so
/// the worker timer fires first; the k8s deadline only kills a worker
/// too wedged to fire its own timer (`r[ctrl.terminated.deadline-
/// exceeded]` then climbs the ladder anyway). NOT 2× (the
/// misclassification-detector threshold): that would race a
/// borderline-slow-but-legit build.
pub(crate) const DEADLINE_MULTIPLIER: i64 = 5;

/// `activeDeadlineSeconds` for an ephemeral Job. Precedence:
///   1. `ephemeral_deadline_seconds` — explicit override, verbatim.
///   2. `size_class_cutoff_secs × DEADLINE_MULTIPLIER + TGPS + SLACK`
///      (I-200, `r[ctrl.ephemeral.per-class-deadline]`). The worker's
///      own `daemon_timeout = cutoff × DEADLINE_MULTIPLIER` fires
///      first; this adds [`EPHEMERAL_TGPS`] + [`DEADLINE_SLACK_SECS`]
///      so k8s only kills a worker too wedged to time itself out.
///   3. `DEFAULT_EPHEMERAL_DEADLINE_SECS` — flat 1h fallback.
///
/// `ceil` so a fractional cutoff (EMA-derived) rounds UP — never
/// shorten below the integer-second boundary. `max(1)`: a zero/
/// negative cutoff (misconfigured spec) would set deadline=0 and K8s
/// kills the pod immediately; clamp to 1s so the misconfiguration is
/// at least observable (pod starts, then dies) rather than a silent
/// no-op spawn loop.
// r[impl ctrl.ephemeral.per-class-deadline+2]
pub(super) fn ephemeral_deadline(spec: &rio_crds::builderpool::BuilderPoolSpec) -> i64 {
    if let Some(explicit) = spec.deadline_seconds {
        return i64::from(explicit);
    }
    if let Some(cutoff) = spec.size_class_cutoff_secs {
        return ((cutoff * DEADLINE_MULTIPLIER as f64).ceil() as i64).max(1)
            + EPHEMERAL_TGPS
            + DEADLINE_SLACK_SECS;
    }
    DEFAULT_EPHEMERAL_DEADLINE_SECS
}

// r[impl ctrl.pool.ephemeral]
/// Reconcile an ephemeral BuilderPool: count active Jobs, poll queue
/// depth, spawn Jobs if work is waiting.
///
/// Status: `replicas` / `readyReplicas` / `desiredReplicas` mean
/// "active Jobs." `desiredReplicas` is the concurrent-Job ceiling
/// (`spec.maxConcurrent`).
pub(super) async fn reconcile(wp: &BuilderPool, ctx: &Ctx) -> Result<Action> {
    let JobReconcilePrologue {
        ns,
        name,
        jobs_api,
        oref,
        scheduler,
        store,
    } = job_reconcile_prologue(wp, ctx)?;
    let ceiling = wp.spec.max_concurrent.map(|c| c as i32);

    // ---- Poll queue depth ----
    // One ClusterStatus RPC per reconcile
    // per ephemeral pool. If the scheduler is unreachable (UNAVAILABLE
    // from standby, or genuinely down): log + treat as queued=0 +
    // requeue. Next tick retries. Spawning Jobs when we can't reach
    // the scheduler would waste pod starts (worker heartbeat would
    // fail → never Ready → Job eventually times out).
    //
    // We ALSO set a SchedulerUnreachable condition on the BuilderPool
    // so operators can see WHY nothing is spawning. Without this,
    // `kubectl get wp` shows queued=0 → "no demand" — indistinguishable
    // from the scheduler being healthy but idle. The condition
    // disambiguates. Still fail-open (queued=0, no spawn).
    //
    // Cloning the admin client: tonic clients are cheap to clone
    // (Arc-internal). The finalizer's cleanup() does the same.
    let (queued, intents, scheduler_err): (u32, Vec<SpawnIntent>, Option<String>) =
        match queued_for_pool(ctx, wp).await {
            Ok((q, intents)) => (q, intents, None),
            Err(e) => {
                warn!(
                    pool = %name, error = %e,
                    "queue-depth poll failed; treating as queued=0, will retry"
                );
                // Still patch status (so `kubectl get wp` shows current
                // active count even when scheduler is down) before
                // requeueing. Fall through with queued=0 → no spawn.
                (0, Vec::new(), Some(e.to_string()))
            }
        };

    // ---- Count active Jobs for this pool ----
    // "Active" = not yet reached Complete/Failed. K8s Job status has
    // `active` (running pods), `succeeded`, `failed`. We count Jobs
    // where succeeded==0 AND failed==0 — still in flight OR pending.
    // A Job whose pod is ContainerCreating is active for our purposes
    // (it'll heartbeat soon, don't double-spawn).
    //
    // ORDERING (I-183): list AFTER the queued poll. The reap step
    // compares `pending` (from this list) against `queued` (from the
    // poll above). If we listed first, a Job could transition Pending→
    // Running→dispatched between list and poll: stale `ready=0`
    // snapshot + fresh `queued=0` → false reap of a Job that just
    // took an assignment. With queued polled FIRST, any Job still
    // `ready=0` at list time had not started its container at poll
    // time → never heartbeated → never decremented queued → the
    // comparison is coherent.
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
    // The cast dance: queued is u32 (proto field), active/ceiling
    // are i32 (K8s replicas convention). saturating_sub handles
    // active >= ceiling can happen if max_concurrent was edited down
    // while Jobs were in flight — don't spawn more, but don't try to
    // cancel either. `ceiling = None` → uncapped: headroom = MAX, so
    // spawn_count reduces to `queued - active`.
    let headroom = ceiling.map_or(u32::MAX, |c| c.saturating_sub(active).max(0) as u32);
    let to_spawn = spawn_count(queued, active as u32, headroom);

    // ADR-023: when the scheduler returned per-drv SpawnIntents (Sla
    // mode), spawn one pod per intent with that intent's resources +
    // annotation. Else (Static mode / unclassed pool / scheduler
    // doesn't populate intents yet) → existing scalar-count path.
    if intents.is_empty() {
        spawn_n(&jobs_api, to_spawn, &name, &wp.spec.size_class, || {
            build_job(wp, oref.clone(), &scheduler, &store, None)
        })
        .await;
    } else {
        // Same `spawn_count` net (queued - active, capped by headroom)
        // as the scalar path. The scheduler already reflects in-flight
        // assignments in the intent list; `to_spawn` is the conservative
        // bound so a slow tick doesn't double-spawn for the same intent.
        // `take(to_spawn)` truncates; the remainder is picked up next
        // tick after `active` decreases.
        spawn_for_each(
            &jobs_api,
            intents.iter().take(to_spawn as usize),
            &name,
            &wp.spec.size_class,
            |intent| build_job(wp, oref.clone(), &scheduler, &store, Some(intent)),
        )
        .await;
    }
    if to_spawn == 0 {
        debug!(pool = %name, queued, active, ?ceiling, "no ephemeral Jobs to spawn");
    }

    // ---- Reap excess Pending ----
    // I-183: spawn-only is half a control loop. When `queued` drops
    // (user cancel, gateway disconnect) the Jobs already spawned but
    // still Pending sit until activeDeadlineSeconds (default 1h) and
    // Karpenter keeps provisioning nodes for them. `to_spawn > 0`
    // implies `queued > active >= pending`, so this only deletes when
    // we're not spawning — but the helper checks `pending > queued`
    // itself so the call is unconditional. `None` when scheduler
    // unreachable: reap is fail-CLOSED (spawn is fail-open).
    let queued_known = scheduler_err.is_none().then_some(queued);
    reap_excess_pending(
        &jobs_api,
        &jobs.items,
        queued_known,
        &name,
        &wp.spec.size_class,
    )
    .await;

    // ---- Reap orphan Running ----
    // I-165: a builder stuck in D-state (FUSE wait, OOM-loop) can't
    // self-exit via the 120s idle-timeout and never disconnects, so
    // the scheduler never reassigns. After ORPHAN_REAP_GRACE (5min),
    // any Running Job the scheduler doesn't consider busy is deleted.
    // Lazy ListExecutors (only fires if there ARE old Running Jobs);
    // fail-closed on RPC error.
    reap_orphan_running(&jobs_api, &jobs.items, ctx, &name, &wp.spec.size_class).await;

    // ---- Report terminations ----
    // Gate scheduler-side `size_class_floor` promotion on actual k8s
    // OOMKilled/DiskPressure (not bare disconnect). Best-effort;
    // scheduler-side dedup makes re-reporting every tick a no-op.
    report_terminated_pods(ctx, &ns, &name, &wp.spec.size_class).await;
    // `activeDeadlineSeconds` backstop fired (worker wedged past its
    // own daemon_timeout). Job controller deletes the Pod, so observe
    // the Job condition instead. Iterates `jobs.items` already listed
    // above — no extra apiserver call.
    report_deadline_exceeded_jobs(ctx, &jobs.items, &wp.spec.size_class).await;

    // ---- Status patch ----
    // `replicas` = active Jobs; `readyReplicas` = same (a Job pod is
    // "ready" when it's running; we don't probe individual Job pods
    // from here). `desiredReplicas` = ceiling, or current demand
    // (`queued`) when uncapped — keeps `kubectl get bp` DESIRED column
    // meaningful instead of showing i32::MAX. SchedulerUnreachable
    // condition reflects the poll above.
    patch_job_pool_status::<BuilderPool, _>(
        ctx,
        wp.status.as_ref(),
        &ns,
        &name,
        Some(active),
        active,
        ceiling.unwrap_or(queued as i32),
        scheduler_err.as_deref(),
    )
    .await?;

    Ok(Action::requeue(JOB_REQUEUE))
}

/// Queue depth relevant to THIS ephemeral pool.
///
/// Two cases:
///   - `size_class` empty (standalone pool) → cluster-wide
///     `ClusterStatus` filtered by `spec.systems` (I-107 per-arch
///     filter). Preserves pre-I-117 behavior.
///   - `size_class` set (typically a WPS child) → per-class
///     `queued` from `GetSizeClassStatus`. Without this, N
///     ephemeral child pools each spawn for the FULL backlog →
///     N× over-provisioning. With it, each pool spawns only for
///     work that `classify()` would route to its class.
///
/// Missing class in the RPC response (scheduler doesn't have
/// `size_classes` configured for this name, or feature off) →
/// fall back to the systems-filtered cluster count. Better to
/// over-spawn than to never spawn — the `activeDeadlineSeconds`
/// backstop reaps wrong-class Jobs after 1h.
///
/// Returns `(queued, spawn_intents)`. ADR-023: `spawn_intents` is
/// the matching class's per-drv intent list (empty under Static
/// sizing or unclassed pools); `queued` is the legacy scalar for
/// `spawn_count` and the reap comparison. Both are read from the
/// same RPC response, so they're a coherent snapshot.
async fn queued_for_pool(
    ctx: &Ctx,
    wp: &BuilderPool,
) -> std::result::Result<(u32, Vec<SpawnIntent>), tonic::Status> {
    if !wp.spec.size_class.is_empty() {
        // I-176: pass `spec.features` so the scheduler excludes
        // derivations whose `required_features` this pool's workers
        // can't satisfy (mirrors hard_filter's `feature-missing`).
        // `filter_features=true` even when `features` is empty: a
        // featureless pool then sees only featureless work — it stops
        // spawning builders that hard_filter rejects on dispatch.
        let resp = ctx
            .admin
            .clone()
            .get_size_class_status(rio_proto::types::GetSizeClassStatusRequest {
                pool_features: wp.spec.features.clone(),
                filter_features: true,
            })
            .await?
            .into_inner();
        // I-143 (per-system) + I-176 (per-feature, cross-class for
        // feature-gated pools). See class_queued_for_pool() doc.
        if let Some(queued) = crate::scaling::class_queued_for_pool(
            &resp,
            &wp.spec.size_class,
            &wp.spec.systems,
            &wp.spec.features,
        ) {
            // ADR-023: extract this class's SpawnIntents. The scheduler
            // already buckets by class; the controller takes them
            // verbatim. TODO(ADR-023 phase-2): per-system filter once
            // intents carry a `system` field — for MVP the scheduler
            // populates per-pool-features-filtered intents only.
            let intents = resp
                .classes
                .into_iter()
                .find(|c| c.name == wp.spec.size_class)
                .map(|c| c.spawn_intents)
                .unwrap_or_default();
            // proto field is u64; spawn_count takes u32. Saturate —
            // a queue > 4 billion derivations is pathological but
            // shouldn't wrap to 0 (would scale DOWN under extreme load).
            return Ok((queued.min(u32::MAX as u64) as u32, intents));
        }
        // Class not in response → fall through to systems filter.
    }
    let resp = ctx.admin.clone().cluster_status(()).await?.into_inner();
    Ok((
        crate::scaling::queued_for_systems(&resp, &wp.spec.systems),
        Vec::new(),
    ))
}

/// Build a K8s Job for one ephemeral worker pod.
///
/// The pod spec is REUSED from `build_pod_spec` — same volumes,
/// security context, env.
///
/// Job-specific settings:
///   - `restartPolicy: Never` — if the worker crashes (OOM,
///     panic), the Job goes Failed. The SCHEDULER owns retry
///     (reassign to a different worker or a different pool). K8s
///     retrying the same pod on the same node risks retry-in-a-
///     -tight-loop on a node-local problem.
///   - `backoffLimit: 0` — same reasoning. One attempt.
///   - `ttlSecondsAfterFinished: 600` — K8s TTL controller reaps.
///   - `activeDeadlineSeconds` — backstop for wrong-pool spawns.
///     `reconcile` spawns from the CLUSTER-WIDE
///     `queued_derivations` count, not pool-matching depth. A
///     queue full of x86 work on an arm64 ephemeral pool
///     triggers a spawn; the worker heartbeats, never matches
///     dispatch, and would hang indefinitely. K8s kills the pod
///     at deadline → Job Failed → TTL reaps. Default 1h; raise
///     via `spec.deadlineSeconds` for known-long-build
///     pools. This DOES bound build time too (K8s doesn't
///     distinguish "worker idle" from "worker busy on 90min
///     build"), so the default is a compromise — per-pool queue
///     depth (phase5) is the proper fix. The state-inconsistency
///     concern (scheduler thinks running, pod gone) is handled
///     the same way any other pod-death is: heartbeat timeout →
///     reassign.
// r[impl ctrl.pool.ephemeral]
// r[impl ctrl.pool.ephemeral-deadline]
// r[impl ctrl.ephemeral.per-class-deadline+2]
pub(super) fn build_job(
    wp: &BuilderPool,
    oref: OwnerReference,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
    intent: Option<&SpawnIntent>,
) -> Result<Job> {
    let pool = wp.name_any();
    // K8s name limit: 63 chars. `rio-builder-{pool}-{6}` = pool+19.
    // Pool names are short (<20 chars); a 49+ char pool gets a
    // clear K8s rejection — no silent truncation.
    let job_name = pod::job_name(&pool, ExecutorKind::Builder, &random_suffix());
    let mut pod_spec = builders::build_pod_spec(wp, scheduler, store);
    // ADR-023 Sla mode: override the per-class `spec.resources` with
    // the scheduler-computed per-drv values. Static mode (`intent:
    // None`) leaves the class's `wp.spec.resources` in place
    // (set by `executor_params` → `build_executor_pod_spec`).
    if let Some(i) = intent {
        apply_intent_resources(&mut pod_spec, i);
    }
    let mut job = ephemeral_job(
        job_name,
        wp.namespace(),
        oref,
        builders::labels(wp),
        // Wrong-pool-spawn backstop + per-class hung-build detector
        // (I-200). Precedence: explicit override > cutoff×5 > flat 3600.
        ephemeral_deadline(&wp.spec),
        pod_spec,
    );
    // ADR-023 Sla mode: stamp `rio.build/intent-id` on the pod
    // template so the builder reads it via downward-API →
    // `RIO_INTENT_ID` → heartbeat → scheduler matches the pod to its
    // pre-computed assignment. `ephemeral_job` always sets the
    // template's annotations map (KARPENTER_DO_NOT_DISRUPT), so the
    // chain is non-None; `expect`s document the structural invariant.
    if let Some(i) = intent {
        job.spec
            .as_mut()
            .and_then(|s| s.template.metadata.as_mut())
            .and_then(|m| m.annotations.as_mut())
            .expect("ephemeral_job sets template.metadata.annotations")
            .insert(INTENT_ID_ANNOTATION.into(), i.intent_id.clone());
    }
    Ok(job)
}

/// Stamp scheduler-computed `(cores, mem, disk)` onto the executor
/// container's `resources` and the overlay emptyDir's `sizeLimit`.
///
/// `requests == limits` (hard caps, no burst) — ADR-023 §sizing-model.
/// Quantities rendered as raw byte counts (no SI suffix): k8s parses
/// bare integers as base-unit (bytes for memory/ephemeral-storage,
/// cores for CPU) and they roundtrip exactly — no float-format
/// ambiguity from `"{n}Gi"` strings.
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
    // `build_executor_pod_spec` puts the executor container at [0]
    // (single-container pod). expect() documents the structural
    // invariant — if a sidecar lands first, this fails loudly in
    // tests rather than silently sizing the wrong container.
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
    // existing nodeSelector (which already carries kubernetes.io/arch
    // from `build_executor_pod_spec`); intent keys win on collision —
    // the scheduler's solve is authoritative for hw-band/capacity-type.
    if !i.node_selector.is_empty() {
        let ns = pod_spec.node_selector.get_or_insert_with(BTreeMap::new);
        for (k, v) in &i.node_selector {
            ns.insert(k.clone(), v.clone());
        }
    }

    // Overlay emptyDir sizeLimit = disk_bytes × headroom. The kubelet
    // evicts on overshoot; headroom keeps a slow-EMA estimate from
    // killing a build that's slightly over its predicted disk peak.
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
    use crate::fixtures::{test_sched_addrs, test_store_addrs};

    fn test_wp() -> BuilderPool {
        // Start from the shared fixture, then override the fields
        // that differ for ephemeral-mode tests. Keeps this site
        // out of the E0063 blast radius when BuilderPoolSpec gains
        // a field — the fixture is the single touch point.
        let mut spec = crate::fixtures::test_builderpool_spec();
        spec.max_concurrent = Some(4);
        spec.features = vec![];
        spec.size_class = String::new();
        let mut wp = BuilderPool::new("eph-pool", spec);
        wp.metadata.uid = Some("uid-eph".into());
        wp.metadata.namespace = Some("rio".into());
        wp
    }

    /// Built Job has all the load-bearing settings. If any of these
    /// drift, the Job reconciler breaks silently:
    ///   - restartPolicy != Never → K8s rejects the Job on create
    ///     (hard error, at least visible)
    ///   - backoffLimit > 0 → K8s retries on crash, scheduler ALSO
    ///     retries → duplicate build
    ///   - ttlSecondsAfterFinished missing → completed Jobs
    ///     accumulate forever
    ///   - ownerReference missing → BuilderPool delete leaves orphan
    ///     Jobs (no GC)
    // r[verify ctrl.pool.ephemeral]
    #[test]
    fn job_spec_load_bearing_fields() {
        let wp = test_wp();
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();

        // ownerReference → GC on BuilderPool delete.
        let orefs = job.metadata.owner_references.as_ref().unwrap();
        assert_eq!(orefs[0].kind, "BuilderPool");
        assert_eq!(orefs[0].controller, Some(true));

        // rio.build/pool label → reconcile's active-count
        // query finds this Job. Without it, every reconcile thinks
        // active=0 and spawns more Jobs → runaway.
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
        // Wrong-pool-spawn backstop present, defaults to 3600 when
        // BOTH ephemeral_deadline_seconds and size_class_cutoff_secs
        // are unset (test_wp sets neither). The per-class branch is
        // covered by `per_class_deadline_from_cutoff_secs`.
        assert_eq!(
            spec.active_deadline_seconds,
            Some(DEFAULT_EPHEMERAL_DEADLINE_SECS),
            "activeDeadlineSeconds backstop missing — wrong-pool \
             spawns (worker never matches dispatch) would leak \
             indefinitely"
        );

        // I-126: do-not-disrupt on the POD TEMPLATE metadata (not the
        // Job's). Without it, karpenter evicts mid-build to consolidate
        // (I-090 bin-packing makes the pod a consolidation candidate).
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
        assert!(
            job.metadata.annotations.is_none(),
            "annotation belongs on pod template, not Job metadata"
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

        // Sanity: build_pod_spec env is present (reuse, not a
        // from-scratch pod). Check one representative.
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        assert!(
            env.iter().any(|e| e.name == "RIO_SCHEDULER__ADDR"),
            "build_pod_spec env should be preserved"
        );
    }

    /// Job name format: {pool}-builder-{6-char-alnum}. The test can't
    /// pin the random suffix but CAN pin the structure — a future
    /// refactor that changes the format (say, to generateName)
    /// would break the "log name before create" observability.
    #[test]
    fn job_name_format() {
        let wp = test_wp();
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();
        let name = job.metadata.name.unwrap();

        // Precondition self-assert: test_wp names the pool "eph-pool".
        // If someone renames it, the prefix check below would pass
        // for the wrong reason (or fail misleadingly).
        assert_eq!(wp.name_any(), "eph-pool");

        assert!(
            name.starts_with("rio-builder-eph-pool-"),
            "expected rio-builder-{{pool}}-{{suffix}}, got {name}"
        );
        let suffix = name.strip_prefix("rio-builder-eph-pool-").unwrap();
        assert_eq!(suffix.len(), 6, "6-char random suffix");
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "suffix must be lowercase alnum (K8s DNS-1123): {suffix}"
        );
    }

    // r[verify ctrl.pool.ephemeral-deadline]
    /// Non-default `ephemeral_deadline_seconds` propagates to the Job
    /// spec verbatim AND wins over `size_class_cutoff_secs`.
    /// Complements `job_spec_load_bearing_fields` (which covers the
    /// `None → DEFAULT_EPHEMERAL_DEADLINE_SECS` branch) and
    /// `per_class_deadline_from_cutoff_secs` (cutoff×5 branch) with
    /// the explicit-override branch.
    ///
    /// Without this: a refactor that always used the default (or
    /// swapped the precedence, or dropped the `i64::from` and wired
    /// the wrong field) would pass the other two tests and silently
    /// ignore user-configured deadlines.
    #[test]
    fn ephemeral_deadline_some_propagates_to_job_spec() {
        let mut wp = test_wp();
        wp.spec.deadline_seconds = Some(7200);
        // Cutoff also set → would compute 30×5+90=240 if precedence
        // were wrong. 7200 must win.
        wp.spec.size_class_cutoff_secs = Some(30.0);
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();

        let spec = job.spec.as_ref().unwrap();
        assert_eq!(
            spec.active_deadline_seconds,
            Some(7200),
            "explicit ephemeral_deadline_seconds=7200 must override \
             cutoff×5 (=150) AND default (=3600)"
        );
    }

    // r[verify ctrl.ephemeral.per-class-deadline+2]
    /// I-200: with `size_class_cutoff_secs` set and no explicit
    /// override, `activeDeadlineSeconds = cutoff × DEADLINE_MULTIPLIER
    /// plus `EPHEMERAL_TGPS + DEADLINE_SLACK_SECS`. The k8s deadline is
    /// a BACKSTOP behind the worker's own `daemon_timeout = cutoff×5`:
    /// the 90s margin lets the worker report `TimedOut` cleanly before
    /// k8s kills it (medium-shallow-32x looped python3 at tiny for 17h
    /// when 2acd1b32 removed disconnect-promote with no margin).
    ///
    /// Mutation check: revert `ephemeral_deadline()` to drop the
    /// margin → first case fails (expects 240, gets 150). Drop
    /// `.ceil()` → fractional case fails (expects 691, gets 690).
    #[test]
    fn per_class_deadline_from_cutoff_secs() {
        const MARGIN: i64 = EPHEMERAL_TGPS + DEADLINE_SLACK_SECS;
        let mut wp = test_wp();
        wp.spec.size_class_cutoff_secs = Some(30.0);
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(30 * DEADLINE_MULTIPLIER + MARGIN),
            "cutoff=30 × {DEADLINE_MULTIPLIER} + {MARGIN} (TGPS+slack) → 240; \
             k8s deadline backstops the worker's daemon_timeout=150"
        );

        // xlarge: cutoff=7200 → 36000+90. Proves the multiplier
        // scales (not a clamped-to-default refactor).
        wp.spec.size_class_cutoff_secs = Some(7200.0);
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(36000 + MARGIN)
        );

        // Fractional cutoff (EMA-derived) rounds UP. 120.1 × 5 =
        // 600.5 → 601 → +90 = 691. Floor would give 690 and shave a
        // second off the bound — wrong direction.
        wp.spec.size_class_cutoff_secs = Some(120.1);
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(601 + MARGIN)
        );

        // Degenerate cutoff=0 clamps the cutoff×5 term to 1s (not 0)
        // before adding margin → 1 + 90 = 91.
        wp.spec.size_class_cutoff_secs = Some(0.0);
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(1 + MARGIN)
        );
    }

    // scheduler_unreachable_condition_shape, random_suffix_valid_
    // dns1123, spawn_count_subtracts_active moved to common::job::
    // tests alongside their functions.

    /// ADR-023 Sla mode: `build_job(.., Some(intent))` stamps the
    /// scheduler-computed resources onto the executor container and
    /// the overlay emptyDir, plus the `rio.build/intent-id` annotation.
    ///
    /// Pins the exact byte-count Quantity strings — k8s parses bare
    /// integers as base-unit, so a regression to `"{n}Gi"` formatting
    /// would silently misrequest by 1024^3.
    // r[verify sched.sla.disk-reaches-ephemeral-storage]
    #[test]
    fn build_job_with_intent_computed_resources() {
        const GI: u64 = 1 << 30;
        let wp = test_wp();
        let oref = crate::fixtures::oref(&wp);
        let intent = SpawnIntent {
            intent_id: "i-abc".into(),
            cores: 8,
            mem_bytes: 16 * GI,
            disk_bytes: 40 * GI,
            node_selector: [
                ("rio.build/hw-band".into(), "mid".into()),
                ("karpenter.sh/capacity-type".into(), "spot".into()),
            ]
            .into(),
        };
        let job = build_job(
            &wp,
            oref,
            &test_sched_addrs(),
            &test_store_addrs(),
            Some(&intent),
        )
        .unwrap();

        let tmpl = &job.spec.as_ref().unwrap().template;
        let pod_anns = tmpl
            .metadata
            .as_ref()
            .unwrap()
            .annotations
            .as_ref()
            .unwrap();
        assert_eq!(
            pod_anns.get(INTENT_ID_ANNOTATION),
            Some(&"i-abc".to_string()),
            "intent_id annotation feeds RIO_INTENT_ID downward-API"
        );

        let pod_spec = tmpl.spec.as_ref().unwrap();
        let res = pod_spec.containers[0].resources.as_ref().unwrap();
        let req = res.requests.as_ref().unwrap();
        assert_eq!(req["cpu"], Quantity("8".into()));
        assert_eq!(req["memory"], Quantity((16 * GI).to_string()));
        // ephemeral-storage = disk + FUSE_CACHE_BUDGET (8Gi) + LOG (1Gi)
        assert_eq!(
            req["ephemeral-storage"],
            Quantity(((40 + 8 + 1) * GI).to_string())
        );
        assert_eq!(
            res.limits.as_ref(),
            Some(req),
            "limits == requests (hard caps, no burst)"
        );

        // overlay emptyDir sizeLimit = disk × 1.5 = 60Gi.
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

    /// Static mode (`intent: None`) leaves the per-class
    /// `wp.spec.resources` untouched and sets no intent annotation.
    #[test]
    fn build_job_without_intent_static_resources() {
        let mut wp = test_wp();
        wp.spec.resources = Some(ResourceRequirements {
            requests: Some(BTreeMap::from([("cpu".into(), Quantity("4".into()))])),
            ..Default::default()
        });
        let oref = crate::fixtures::oref(&wp);
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs(), None).unwrap();

        let tmpl = &job.spec.as_ref().unwrap().template;
        let pod_anns = tmpl
            .metadata
            .as_ref()
            .unwrap()
            .annotations
            .as_ref()
            .unwrap();
        assert!(
            !pod_anns.contains_key(INTENT_ID_ANNOTATION),
            "Static-mode pods carry no intent annotation"
        );

        let res = tmpl.spec.as_ref().unwrap().containers[0]
            .resources
            .as_ref()
            .unwrap();
        assert_eq!(
            res.requests.as_ref().unwrap()["cpu"],
            Quantity("4".into()),
            "Static mode uses class spec.resources verbatim"
        );
    }
}
