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

use std::time::Duration;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;
use kube::api::{ListParams, ObjectMeta};
use kube::runtime::controller::Action;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::reconcilers::Ctx;
use crate::reconcilers::common::pod::{self, ExecutorRole};
use rio_crds::builderpool::BuilderPool;

use super::builders::{self, SchedulerAddrs, StoreAddrs};
use super::job_common::{
    SpawnOutcome, is_active_job, job_reconcile_prologue, patch_job_pool_status, random_suffix,
    reap_excess_pending, reap_orphan_running, spawn_prerequisites, try_spawn_job,
};

/// Requeue interval. Job spawning is reactive to queue depth, not
/// just spec drift. 10s: one queue-depth poll per tick. Shorter would
/// mean more `ClusterStatus` RPCs to the scheduler (cheap, but noise)
/// and more `kubectl get jobs` calls (apiserver load). Longer
/// lengthens dispatch latency: a worker needs one requeue interval +
/// pod scheduling + container pull + FUSE mount + heartbeat
/// (~10s + 10-30s) before the scheduler sees it.
pub(crate) const EPHEMERAL_REQUEUE: Duration = Duration::from_secs(10);

/// `ttlSecondsAfterFinished` on spawned Jobs. K8s TTL controller
/// deletes the Job (and its pod, via ownerRef) this many seconds
/// after it reaches Complete or Failed. 600s (10min): long enough
/// that an operator debugging a failed build can `kubectl logs` the
/// pod; short enough that Job churn doesn't accumulate. The SCHEDULER
/// has already observed the completion (worker sent CompletionReport
/// before exiting) so there's no rio-side dependency on the Job
/// sticking around.
pub(crate) const JOB_TTL_SECS: i32 = 600;

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

/// I-200: `activeDeadlineSeconds = class.cutoffSecs × DEADLINE_
/// MULTIPLIER`. 5×: a build that's still going at 5× its class's
/// upper-bound prediction is either hung or grossly misclassified —
/// either way, K8s kills the pod → ExecutorDisconnected →
/// `r[sched.reassign.no-promote-on-ephemeral-disconnect+2]` promotes
/// `size_class_floor` and the next dispatch lands on the next-larger
/// class (with a 5× longer deadline). tiny (cutoff 30s) → 150s,
/// small (120s) → 600s, medium (600s) → 3000s, large (1800s) →
/// 9000s, xlarge (7200s) → 36000s. NOT 2× (the misclassification-
/// detector threshold): that would race the worker-side completion
/// for a borderline-slow-but-legit build; 5× leaves room for cold-
/// FUSE warmup + pod-scheduling overhead on top of the build itself.
pub(crate) const DEADLINE_MULTIPLIER: i64 = 5;

/// `activeDeadlineSeconds` for an ephemeral Job. Precedence:
///   1. `ephemeral_deadline_seconds` — explicit override, verbatim.
///   2. `size_class_cutoff_secs × DEADLINE_MULTIPLIER` — per-class
///      (I-200, `r[ctrl.ephemeral.per-class-deadline]`).
///   3. `DEFAULT_EPHEMERAL_DEADLINE_SECS` — flat 1h fallback.
///
/// `ceil` so a fractional cutoff (EMA-derived) rounds UP — never
/// shorten below the integer-second boundary. `max(1)`: a zero/
/// negative cutoff (misconfigured spec) would set deadline=0 and K8s
/// kills the pod immediately; clamp to 1s so the misconfiguration is
/// at least observable (pod starts, then dies) rather than a silent
/// no-op spawn loop.
fn ephemeral_deadline(spec: &rio_crds::builderpool::BuilderPoolSpec) -> i64 {
    if let Some(explicit) = spec.deadline_seconds {
        return i64::from(explicit);
    }
    if let Some(cutoff) = spec.size_class_cutoff_secs {
        return ((cutoff * DEADLINE_MULTIPLIER as f64).ceil() as i64).max(1);
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
pub(super) async fn reconcile_ephemeral(wp: &BuilderPool, ctx: &Ctx) -> Result<Action> {
    let (ns, name, jobs_api) = job_reconcile_prologue(wp, ctx)?;
    let ceiling = wp.spec.max_concurrent as i32;

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
    let (queued, scheduler_err): (u32, Option<String>) = match queued_for_pool(ctx, wp).await {
        Ok(q) => (q, None),
        Err(e) => {
            warn!(
                pool = %name, error = %e,
                "queue-depth poll failed; treating as queued=0, will retry"
            );
            // Still patch status (so `kubectl get wp` shows current
            // active count even when scheduler is down) before
            // requeueing. Fall through with queued=0 → no spawn.
            (0, Some(e.to_string()))
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
    // cancel either.
    let headroom = ceiling.saturating_sub(active).max(0) as u32;
    let to_spawn = spawn_count(queued, active as u32, headroom);

    if to_spawn > 0 {
        let (oref, scheduler, store) = spawn_prerequisites(wp, ctx)?;

        for _ in 0..to_spawn {
            let job = build_job(wp, oref.clone(), &scheduler, &store)?;
            // build_job() always sets metadata.name, so this can't
            // fail today — but .expect() here violates the crate's
            // "reconciler panic = pod crash-loop" convention
            // (cf. builderpool/mod.rs:317-319). Error path instead.
            let job_name = job
                .metadata
                .name
                .clone()
                .ok_or_else(|| Error::InvalidSpec("job name missing".into()))?;
            match try_spawn_job(&jobs_api, &job).await {
                SpawnOutcome::Spawned => {
                    info!(
                        pool = %name, job = %job_name,
                        queued, active, ceiling,
                        "spawned ephemeral Job"
                    );
                }
                SpawnOutcome::NameCollision => {
                    debug!(pool = %name, job = %job_name, "Job name collision; will retry");
                }
                SpawnOutcome::Failed(e) => {
                    // Was `return Err(e.into())` — THE bug. Matches
                    // pre-P0516 manifest.rs. Now warn+continue:
                    // status patch at :242 runs regardless; next
                    // tick retries.
                    warn!(
                        pool = %name, job = %job_name,
                        queued, active, ceiling, error = %e,
                        "ephemeral Job spawn failed; continuing tick"
                    );
                }
            }
        }
    } else {
        debug!(
            pool = %name, queued, active, ceiling,
            "no ephemeral Jobs to spawn"
        );
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

    // ---- Status patch ----
    // `replicas` = active Jobs; `readyReplicas` = same (a Job pod is
    // "ready" when it's running; we don't probe individual Job pods
    // from here). `desiredReplicas` = ceiling. SchedulerUnreachable
    // condition reflects the poll above.
    patch_job_pool_status(
        ctx,
        wp,
        &ns,
        &name,
        active,
        active,
        ceiling,
        scheduler_err.as_deref(),
    )
    .await?;

    Ok(Action::requeue(EPHEMERAL_REQUEUE))
}

/// Compute how many Jobs to spawn this tick.
///
/// `(queued - active).min(headroom)` — each active Job is treated as
/// already claiming one queued derivation. This prevents the runaway
/// where a single queued derivation triggers a fresh Job every 10s
/// tick until the ceiling is hit:
///
///   - t=0:  queued=1, active=0 → spawn Job-A
///   - t=10: queued=1, active=1 → old formula spawned Job-B (wrong);
///     new formula: 1-1=0 → no spawn
///   - t=20: Job-A's pod heartbeats, dispatch fires, queued→0
///
/// `queued_derivations` is `ready_queue.len()` on the scheduler side
/// (actor/mod.rs compute_cluster_snapshot). A derivation stays in the
/// ready_queue until a worker heartbeats and `dispatch_ready` pops it
/// — NOT when we spawn the Job. Pod startup (schedule + pull + FUSE
/// mount + first heartbeat) is ~10-30s; with a 10s requeue interval
/// the old `queued.min(headroom)` formula fired 2-4 extra Jobs per
/// build before the first pod came online.
///
/// Conservative bias: if some active Jobs are already busy (dispatched
/// work, not starting), subtracting them under-spawns by that count.
/// Next tick after those Jobs succeed corrects it. Under-spawn = one
/// requeue interval of latency; over-spawn = wasted pod starts +
/// idle workers heartbeating for work that doesn't exist. For the
/// "isolation > throughput" ephemeral use case, the latency cost is
/// acceptable; the resource waste is not.
///
/// Global-Q caveat: for unclassified pools, `queued` is cluster-
/// wide (per-system filtered). With multiple unclassified pools,
/// each over-counts need by what the others will claim — but
/// headroom caps it, and the others draining Q on the next tick
/// self-corrects. Pools with `size_class` set use per-class depth
/// (see [`queued_for_pool`]) so this caveat doesn't apply to them.
pub(crate) fn spawn_count(queued: u32, active: u32, headroom: u32) -> u32 {
    queued.saturating_sub(active).min(headroom)
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
async fn queued_for_pool(ctx: &Ctx, wp: &BuilderPool) -> std::result::Result<u32, tonic::Status> {
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
            // proto field is u64; spawn_count takes u32. Saturate —
            // a queue > 4 billion derivations is pathological but
            // shouldn't wrap to 0 (would scale DOWN under extreme load).
            return Ok(queued.min(u32::MAX as u64) as u32);
        }
        // Class not in response → fall through to systems filter.
    }
    let resp = ctx.admin.clone().cluster_status(()).await?.into_inner();
    Ok(crate::scaling::queued_for_systems(&resp, &wp.spec.systems))
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
///   - `ttlSecondsAfterFinished: 60` — K8s TTL controller reaps.
///   - `activeDeadlineSeconds` — backstop for wrong-pool spawns.
///     `reconcile_ephemeral` spawns from the CLUSTER-WIDE
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
pub(super) fn build_job(
    wp: &BuilderPool,
    oref: OwnerReference,
    scheduler: &SchedulerAddrs,
    store: &StoreAddrs,
) -> Result<Job> {
    let pool = wp.name_any();
    let labels = builders::labels(wp);

    let mut pod_spec = builders::build_pod_spec(wp, scheduler, store, None);

    // restartPolicy: Never is REQUIRED by K8s for Jobs with
    // backoffLimit=0. "Always" (the PodSpec default) is rejected.
    pod_spec.restart_policy = Some("Never".into());
    // I-120: one build per pod; on SIGTERM exit fast. 30s covers
    // FUSE unmount + completion report.
    pod_spec.termination_grace_period_seconds = Some(30);

    // Random suffix: 6 lowercase alphanumeric. Not crypto; just
    // avoiding collisions. The executor_id downward-API pattern
    // from common/pod.rs means each pod's RIO_EXECUTOR_ID is the
    // Job's pod name (also random-suffixed by K8s on top of our
    // suffix) — unique per pod, which is what the scheduler needs
    // for its executors map.
    // K8s name limit: 63 chars. `rio-builder-{pool}-{6}` = pool+19.
    // Pool names are short (<20 chars); a 49+ char pool gets a
    // clear K8s rejection — no silent truncation.
    let job_name = pod::job_name(&pool, ExecutorRole::Builder, &random_suffix());

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            // One pod. parallelism/completions default to 1 if
            // unset, but explicit for clarity (a Job with
            // parallelism>1 would mean N pods sharing one Job →
            // N workers heartbeat with the SAME pod-name prefix →
            // scheduler's workers map merges on heartbeat — chaos).
            parallelism: Some(1),
            completions: Some(1),
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(JOB_TTL_SECS),
            // r[impl ctrl.pool.ephemeral-deadline]
            // r[impl ctrl.ephemeral.per-class-deadline]
            // Wrong-pool-spawn backstop + per-class hung-build
            // detector (I-200): K8s kills the pod at deadline →
            // Job Failed → TTL reaps; scheduler-side the disconnect
            // promotes size_class_floor. Precedence: explicit
            // override > cutoff×5 > flat 3600.
            active_deadline_seconds: Some(ephemeral_deadline(&wp.spec)),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    // I-126: I-090's bin-packing + consolidateAfter:30s on
                    // the NodePool means karpenter evicts mid-build to
                    // consolidate (observed: 3 builders evicted in ~2min
                    // warming inputs for the same drv → cascading
                    // reassigns). do-not-disrupt pins the pod for its
                    // lifetime; the node consolidates AFTER Job completion.
                    // Annotation goes on the POD TEMPLATE metadata, not
                    // the Job's — karpenter reads pod annotations.
                    annotations: Some(std::collections::BTreeMap::from([(
                        "karpenter.sh/do-not-disrupt".into(),
                        "true".into(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{test_sched_addrs, test_store_addrs};
    // `controller_owner_ref` comes from `kube::Resource`. Module-
    // level import moved to job_common with spawn_prerequisites;
    // tests still build Jobs directly, so import here.
    use kube::Resource;

    fn test_wp() -> BuilderPool {
        // Start from the shared fixture, then override the fields
        // that differ for ephemeral-mode tests. Keeps this site
        // out of the E0063 blast radius when BuilderPoolSpec gains
        // a field — the fixture is the single touch point.
        let mut spec = crate::fixtures::test_builderpool_spec();
        spec.max_concurrent = 4;
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
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();

        // ownerReference → GC on BuilderPool delete.
        let orefs = job.metadata.owner_references.as_ref().unwrap();
        assert_eq!(orefs[0].kind, "BuilderPool");
        assert_eq!(orefs[0].controller, Some(true));

        // rio.build/pool label → reconcile_ephemeral's active-count
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
            env.iter().any(|e| e.name == "RIO_SCHEDULER_ADDR"),
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
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();
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

    /// Spawn-count formula: active Jobs claim queued derivations.
    ///
    /// Regression for the KVM-speed runaway: under the old formula
    /// `queued.min(headroom)`, a single queued derivation spawned a
    /// fresh Job every 10s tick until the ceiling. With pod startup
    /// ~10-30s and ceiling=4, one build produced 3-4 Jobs; two
    /// sequential builds produced 9+ (lifecycle.nix ephemeral-pool
    /// subtest observed this under KVM).
    ///
    /// Mutation check: revert to `queued.min(headroom)` → the
    /// `q1_a1_no_spawn` case fails (expects 0, gets 1).
    #[test]
    fn spawn_count_subtracts_active() {
        // The bug case: 1 queued, 1 Job already in flight (pod
        // starting, hasn't heartbeated). Old formula: min(1,3)=1
        // → runaway. New: 1-1=0 → wait for the in-flight Job.
        assert_eq!(spawn_count(1, 1, 3), 0, "q1_a1_no_spawn");

        // Cold start: nothing active, spawn up to queued.
        assert_eq!(spawn_count(1, 0, 4), 1, "cold start single");
        assert_eq!(spawn_count(3, 0, 4), 3, "cold start multi");

        // Ceiling clamp: 10 queued, 0 active, ceiling 4 → spawn 4.
        assert_eq!(spawn_count(10, 0, 4), 4, "headroom caps");

        // Steady state at ceiling: headroom=0 → no spawn regardless.
        assert_eq!(spawn_count(10, 4, 0), 0, "ceiling reached");

        // Recovery after a Job completes: 5 queued, 3 active (one
        // succeeded and dropped out of the filter), headroom=1.
        // Need = 5-3=2, but headroom caps at 1.
        assert_eq!(spawn_count(5, 3, 1), 1, "post-complete refill");

        // Saturating: active > queued (some Jobs are running
        // dispatched work, Q already drained). Don't underflow.
        assert_eq!(spawn_count(0, 3, 1), 0, "drained queue");
        assert_eq!(spawn_count(1, 3, 1), 0, "more active than queued");

        // Empty everything.
        assert_eq!(spawn_count(0, 0, 4), 0, "idle");
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
        // Cutoff also set → would compute 30×5=150 if precedence
        // were wrong. 7200 must win.
        wp.spec.size_class_cutoff_secs = Some(30.0);
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();

        let spec = job.spec.as_ref().unwrap();
        assert_eq!(
            spec.active_deadline_seconds,
            Some(7200),
            "explicit ephemeral_deadline_seconds=7200 must override \
             cutoff×5 (=150) AND default (=3600)"
        );
    }

    // r[verify ctrl.ephemeral.per-class-deadline]
    /// I-200: with `size_class_cutoff_secs` set and no explicit
    /// override, `activeDeadlineSeconds = cutoff × DEADLINE_MULTIPLIER`.
    /// tiny (cutoff=30s) → 150s, NOT the flat 3600. The deadline
    /// becomes a per-class hung-build detector: K8s kills the pod →
    /// disconnect → I-197 promotes `size_class_floor`.
    ///
    /// Mutation check: revert `ephemeral_deadline()` to ignore cutoff
    /// → this fails (expects 150, gets 3600). Drop the `.ceil()` →
    /// the fractional case fails (expects 601, gets 600).
    #[test]
    fn per_class_deadline_from_cutoff_secs() {
        let mut wp = test_wp();
        wp.spec.size_class_cutoff_secs = Some(30.0);
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(30 * DEADLINE_MULTIPLIER),
            "cutoff=30 × DEADLINE_MULTIPLIER({DEADLINE_MULTIPLIER}) → 150, \
             not flat DEFAULT_EPHEMERAL_DEADLINE_SECS"
        );

        // xlarge: cutoff=7200 → 36000. Proves the multiplier scales
        // (not a clamped-to-default refactor).
        wp.spec.size_class_cutoff_secs = Some(7200.0);
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(36000)
        );

        // Fractional cutoff (EMA-derived) rounds UP. 120.1 × 5 =
        // 600.5 → 601. Floor would give 600 and shave a second off
        // the bound — wrong direction.
        wp.spec.size_class_cutoff_secs = Some(120.1);
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();
        assert_eq!(
            job.spec.as_ref().unwrap().active_deadline_seconds,
            Some(601)
        );

        // Degenerate cutoff=0 clamps to 1s (not 0, which K8s treats
        // as "kill immediately" → silent spawn loop).
        wp.spec.size_class_cutoff_secs = Some(0.0);
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();
        assert_eq!(job.spec.as_ref().unwrap().active_deadline_seconds, Some(1));
    }

    /// I-045: ephemeral Job pods stuck `READY 0/1`, "no SERVING
    /// endpoints for rio-scheduler-headless" — the gRPC balance
    /// client uses mTLS and the Job's pod spec was missing the `tls`
    /// volume. The live BuilderPool CR had `ephemeral: true` but no
    /// `tlsSecretName` (helm template doesn't render `ephemeral`, so
    /// the test CR was hand-applied and skipped the field).
    ///
    /// This isn't a controller bug — `build_job → build_pod_spec →
    /// executor_params` reads `wp.spec.tls_secret_name` correctly.
    /// The `None` at the build_pod_spec call site is `resources_
    /// override`, not TLS. When the live pod was missing TLS, "is
    /// build_job dropping it?" was an open question. This pins it
    /// shut: same volume/mount/env trio, sourced from
    /// `common/pod.rs::build_executor_pod_spec`.
    #[test]
    fn job_tls_secret_mounted_when_set() {
        let mut wp = test_wp();
        wp.spec.tls_secret_name = Some("rio-builder-tls".into());
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();
        let pod = job.spec.unwrap().template.spec.unwrap();

        let tls_vol = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "tls")
            .expect(
                "tls volume missing from ephemeral Job pod — without it the \
                 mTLS balance client has no client cert and every health \
                 probe fails the TLS handshake → 'no SERVING endpoints' → \
                 pod never goes Ready",
            );
        assert_eq!(
            tls_vol.secret.as_ref().unwrap().secret_name,
            Some("rio-builder-tls".into())
        );

        let container = &pod.containers[0];
        let mount = container
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "tls")
            .expect("tls mount");
        assert_eq!(mount.mount_path, "/etc/rio/tls");
        assert_eq!(mount.read_only, Some(true));

        let envs: std::collections::HashMap<_, _> = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|e| e.value.as_ref().map(|v| (e.name.as_str(), v.as_str())))
            .collect();
        assert_eq!(
            envs.get("RIO_TLS__CERT_PATH"),
            Some(&"/etc/rio/tls/tls.crt")
        );
        assert_eq!(envs.get("RIO_TLS__KEY_PATH"), Some(&"/etc/rio/tls/tls.key"));
        assert_eq!(envs.get("RIO_TLS__CA_PATH"), Some(&"/etc/rio/tls/ca.crt"));
    }

    // scheduler_unreachable_condition_shape + random_suffix_valid_
    // dns1123 moved to job_common::tests alongside their functions.
}
