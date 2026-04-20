//! Job-mode plumbing for the Pool reconciler: list Jobs by label,
//! filter active, diff against demand, spawn deficit, reap excess,
//! patch status. Consumed only by `jobs.rs`; kept as a separate file
//! because the merged module would top 2000 LoC.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{Pod, PodSpec, PodTemplateSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::CustomResourceExt;
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams};
use tracing::{debug, info, warn};

use rio_proto::types::{ReportExecutorTerminationRequest, SpawnIntent, TerminationReason};

use crate::error::Result;
use crate::reconcilers::{Ctx, KubeErrorExt, admin_call};
use rio_crds::pool::{Pool, PoolStatus};

use super::pod::POOL_LABEL;

/// Field manager for server-side apply on Job-mode pool resources
/// (Pool status, owned Jobs). K8s tracks which
/// fields each manager owns; conflicting managers get a 409 unless
/// `force`. We use `force: true` — this controller is authoritative
/// for what it manages. Shared by both pool reconcilers so a Job's
/// SSA history shows one consistent manager regardless of which
/// reconciler touched it.
pub(super) const MANAGER: &str = "rio-controller";

/// Requeue interval for Job-mode reconcilers. Job spawning is reactive
/// to queue depth, not just spec drift. 10s: one queue-depth poll per
/// tick. Shorter would mean more `ClusterStatus` RPCs to the scheduler
/// (cheap, but noise) and more `kubectl get jobs` calls (apiserver
/// load). Longer lengthens dispatch latency: a worker needs one
/// requeue interval + pod scheduling + container pull + FUSE mount +
/// heartbeat (~10s + 10-30s) before the scheduler sees it.
pub(super) const JOB_REQUEUE: Duration = Duration::from_secs(10);

/// `ttlSecondsAfterFinished` on spawned Jobs. K8s TTL controller
/// deletes the Job (and its pod, via ownerRef) this many seconds
/// after it reaches Complete or Failed. 600s (10min): long enough
/// that an operator debugging a failed build can `kubectl logs` the
/// pod; short enough that Job churn doesn't accumulate. The SCHEDULER
/// has already observed the completion (worker sent CompletionReport
/// before exiting) so there's no rio-side dependency on the Job
/// sticking around.
pub(super) const JOB_TTL_SECS: i32 = 600;

/// Pod-template annotation that opts a pod out of karpenter
/// consolidation/drift eviction. I-126: I-090's bin-packing +
/// `consolidateAfter:30s` on the NodePool means karpenter evicts
/// mid-build to consolidate (observed: 3 builders evicted in ~2min
/// warming inputs for the same drv → cascading reassigns). Set on
/// EVERY ephemeral Job pod via [`ephemeral_job`] — the node
/// consolidates AFTER Job completion. Goes on POD TEMPLATE metadata,
/// not the Job's: karpenter reads pod annotations.
pub(super) const KARPENTER_DO_NOT_DISRUPT: &str = "karpenter.sh/do-not-disrupt";

/// The shared one-shot Job literal for executor pods. Both pool
/// kinds (Builder, Fetcher) route through this so the load-bearing
/// invariants can't drift per call site:
///
///   - `restartPolicy: Never` + `backoffLimit: 0` — the SCHEDULER
///     owns retry (reassign to a different worker / pool / floor).
///     K8s retrying the same pod on the same node risks
///     tight-loop on a node-local problem.
///   - `parallelism/completions: 1` — one pod per Job. >1 would
///     mean N pods sharing one Job → N workers heartbeat with the
///     SAME pod-name prefix → scheduler's executors map collides.
///   - [`JOB_TTL_SECS`] — completed Jobs auto-reap.
///   - `activeDeadlineSeconds` — backstop for hung/wrong-pool pods.
///     ALWAYS set (no `None`): a missing deadline means a stuck pod
///     leaks for the life of the cluster. Callers compute the
///     per-role/per-class value (cutoff×5 for builders, 300s for
///     fetchers) and pass it in.
///   - [`KARPENTER_DO_NOT_DISRUPT`] on the pod template — I-126
///     mid-build eviction protection.
///
/// `termination_grace_period_seconds` is left to
/// [`super::pod::build_executor_pod_spec`] (`r[ctrl.pod.tgps-default]`:
/// 7200s builders, 600s fetchers, or `PoolSpec` override). The
/// builder's SIGTERM handler blocks on its single in-flight build, so
/// "ephemeral" ≠ "fast exit".
///
/// Consolidated here so the Job-lifecycle invariants (karpenter
/// annotation, deadline backstop) can't drift between callers —
/// pre-P0513 they had.
///
/// `pod_spec` arrives with role-specific content (volumes, env,
/// resources) already filled by `build_executor_pod_spec`; this
/// fn only stamps the Job-lifecycle fields on top.
pub(super) fn ephemeral_job(
    name: String,
    namespace: Option<String>,
    oref: OwnerReference,
    labels: BTreeMap<String, String>,
    deadline_seconds: i64,
    mut pod_spec: PodSpec,
) -> Job {
    // restartPolicy: Never is REQUIRED by K8s for Jobs with
    // backoffLimit=0 ("Always" — the PodSpec default — is rejected).
    pod_spec.restart_policy = Some("Never".into());

    Job {
        metadata: ObjectMeta {
            name: Some(name),
            namespace,
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            parallelism: Some(1),
            completions: Some(1),
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(JOB_TTL_SECS),
            active_deadline_seconds: Some(deadline_seconds),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: Some(BTreeMap::from([(
                        KARPENTER_DO_NOT_DISRUPT.into(),
                        "true".into(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Neither Succeeded nor Failed → still active (running or pending).
/// `None` status treated as active — fresh Job before the Job
/// controller populates; don't double-spawn on the next tick just
/// because status hasn't materialized yet.
///
/// Both Job-mode reconcilers use this exact predicate for their
/// inventory pass: a Job with `succeeded > 0` (Complete) or
/// `failed > 0` (Failed under `backoff_limit=0`) is NOT supply — it
/// won't heartbeat again. Counting only active Jobs prevents
/// over-spawn (counting a Failed Job as supply would under-spawn;
/// counting a Complete Job is moot — TTL reaps it).
pub(super) fn is_active_job(j: &Job) -> bool {
    let s = j.status.as_ref();
    s.and_then(|st| st.succeeded).unwrap_or(0) == 0 && s.and_then(|st| st.failed).unwrap_or(0) == 0
}

/// Per-tick Job inventory: `active` (consumes a `maxConcurrent` slot)
/// and `ready` (container started — heartbeating). Computed once at
/// the top of `jobs::reconcile` so the headroom math, status patch,
/// and reap passes see ONE consistent view.
///
/// `active` excludes `deletion_timestamp` (terminating Jobs do not
/// consume headroom — they were foreground-deleted on a prior tick and
/// their slot is free for spawn). `ready` is the [`is_running_job`]
/// subset, which already excludes terminating; this is what
/// `PoolStatus.ready_replicas` documents as "passed readinessProbe".
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct JobCensus {
    pub active: i32,
    pub ready: i32,
}

impl JobCensus {
    /// Single-subtraction headroom: `ceiling - effective_active`,
    /// floored at 0. `freed` (active Jobs reaped THIS tick) is
    /// subtracted from `active` BEFORE the clamp, so an
    /// over-committed pool (`active > ceiling`, e.g. operator
    /// lowered `maxConcurrent` while Jobs are live) computes
    /// `ceiling − (active − freed)` instead of `0 + freed`. The
    /// add-after-clamp form (`clamp(ceiling − active) + freed`)
    /// overshoots `ceiling` in that case. `ceiling = None` →
    /// uncapped.
    pub fn headroom(self, ceiling: Option<i32>, freed: i32) -> usize {
        ceiling.map_or(usize::MAX, |c| {
            c.saturating_sub(self.active.saturating_sub(freed)).max(0) as usize
        })
    }
}

/// Compute the per-tick [`JobCensus`] from a Job list.
pub(super) fn job_census(jobs: &[Job]) -> JobCensus {
    let active = jobs
        .iter()
        .filter(|j| is_active_job(j) && j.metadata.deletion_timestamp.is_none())
        .count()
        .try_into()
        .unwrap_or(i32::MAX);
    let ready = jobs
        .iter()
        .filter(|j| is_running_job(j))
        .count()
        .try_into()
        .unwrap_or(i32::MAX);
    JobCensus { active, ready }
}

/// Active AND `status.ready == 0` — the Job's pod is in `Pending`
/// phase (unscheduled or `ContainerCreating`).
///
/// With `parallelism: 1` and no readiness probe (I-114 dropped probes
/// for ephemeral Jobs), `JobStatus.ready` flips to 1 the moment the
/// container starts — at which point the worker is dialing the
/// scheduler and may receive an assignment any millisecond. `ready ==
/// 0` means the container has NOT started: never heartbeated, never
/// dispatched, deleting it loses nothing. This is the reap-safety
/// boundary for `r[ctrl.ephemeral.reap-excess-pending+2]`.
///
/// `None` status (Job controller hasn't reconciled yet → pod not
/// created) is treated as Pending. That's the safe direction: a Job
/// with no pod is trivially reapable.
///
/// A Job with `deletionTimestamp` set is NOT pending — it's already
/// terminating (foreground-delete in flight). Re-selecting it would
/// be a no-op apiserver round-trip + log spam every tick until the
/// pod's `job-tracking` finalizer clears.
pub(super) fn is_pending_job(j: &Job) -> bool {
    j.metadata.deletion_timestamp.is_none()
        && is_active_job(j)
        && j.status.as_ref().and_then(|s| s.ready).unwrap_or(0) == 0
}

/// Active AND `status.ready > 0` — the Job's pod container has
/// started. Complement of [`is_pending_job`] within the active set
/// (both predicates exclude terminating Jobs). The orphan-reap
/// boundary for `r[ctrl.ephemeral.reap-orphan-running+2]`: only Running
/// Jobs are candidates (Pending is handled by `reap_excess_pending`;
/// Complete/Failed by TTL).
///
/// A Job with `deletionTimestamp` set is NOT running — it's already
/// terminating (foreground-delete in flight). Re-selecting it would
/// re-delete + re-fire `ListExecutors` (defeating the lazy-RPC
/// pre-filter) + double-count `rio_controller_orphan_jobs_reaped_total`
/// every tick until the pod's `job-tracking` finalizer clears. A
/// D-state pod (the I-165 case this reaper exists for) ignores SIGTERM
/// and stays listed for up to `terminationGracePeriodSeconds`.
pub(super) fn is_running_job(j: &Job) -> bool {
    j.metadata.deletion_timestamp.is_none()
        && is_active_job(j)
        && j.status.as_ref().and_then(|s| s.ready).unwrap_or(0) > 0
}

/// Minimum age before a Running Job is orphan-reapable. 5min: MUST
/// exceed the builder's `RIO_IDLE_SECS` (default 120s) so
/// the process-level idle-exit gets first chance — a healthy idle
/// pod self-terminates at 120s and the Job goes Complete
/// well before this fires. The reap targets pods that CANNOT
/// self-exit (I-165: D-state FUSE wait, OOM-loop) and would otherwise
/// burn `activeDeadlineSeconds` (default 1h) holding a node.
pub(super) const ORPHAN_REAP_GRACE: Duration = Duration::from_secs(300);

/// Minimum age before a Pending Job is reapable. `JobStatus.ready` is
/// set by the K8s Job controller AFTER it observes pod readiness — a
/// container that just started may have already heartbeated and
/// received an assignment while `ready` is still 0 (Job-controller
/// sync lag, typically <1s but unbounded under apiserver load). One
/// requeue tick of grace makes the false-positive window negligible
/// without materially delaying the I-183 reap (the bug is Jobs sitting
/// for an HOUR; 10s grace is noise).
pub(super) const REAP_PENDING_GRACE: Duration = Duration::from_secs(10);

/// Pending Jobs in excess of `queued`, oldest-first — RESIDUAL
/// fallback for `r[ctrl.ephemeral.reap-excess-pending+2]` after
/// `reap_stale_for_intents`' orphan-pending arm has already reaped by
/// intent-membership.
///
/// `pending.len() <= queued` → empty. `pending.len() > queued` → the
/// `pending - queued` oldest are surplus. The spawn loop's
/// NameCollision dedupe means we never spawn more than one Job per
/// intent IN ONE POOL, so after the orphan-pending arm this only
/// fires for the overlapping-pool double-spawn case (two Pools with
/// intersecting `{systems, features}` both spawn for the same intent
/// under different names — neither is "orphan" by name-membership but
/// `pending > queued`).
///
/// `min_age`: Jobs younger than this are excluded — see
/// [`REAP_PENDING_GRACE`]. Passing `Duration::ZERO` disables the
/// grace (tests).
///
/// Oldest-first: the oldest
/// Pending Job has waited longest for a node; if Karpenter hasn't
/// provisioned one by now it's likely the most stuck. Newest-first
/// would reap the Job that's closest to scheduling.
///
/// Running Jobs are NOT in the result — [`is_pending_job`] excludes
/// them. A Running pod may already hold an assignment; the scheduler's
/// cancel-on-disconnect handles those when the gateway session that
/// queued the work closes.
pub(super) fn select_excess_pending(jobs: &[Job], queued: u32, min_age: Duration) -> Vec<&Job> {
    let mut pending: Vec<&Job> = jobs
        .iter()
        .filter(|j| is_pending_job(j) && job_older_than(j, min_age))
        .collect();
    let queued = queued as usize;
    if pending.len() <= queued {
        return Vec::new();
    }
    // Option<Time> sorts None-first (treated as oldest) — same as
    // select_failed_jobs.
    pending.sort_by_key(|j| j.metadata.creation_timestamp.clone());
    pending.truncate(pending.len() - queued);
    pending
}

// r[impl ctrl.ephemeral.reap-excess-pending+2]
/// Delete Pending Jobs in excess of `queued`. Shared by the
/// builder and fetcher pool reconcilers (both had the spawn-only
/// pattern before I-183; both now reap).
///
/// `pool` feeds the metric labels and log fields.
///
/// warn+continue on delete failure — same posture as the spawn loop
/// (P0516): one apiserver blip shouldn't skip the status patch. Next
/// tick re-lists and retries (the Job is still Pending, still excess).
///
/// `queued = None` → scheduler unreachable; caller treated the poll
/// error as `queued=0` for spawn (fail-open: don't spawn). Reap MUST
/// NOT treat that as 0 (fail-closed: don't delete) — a scheduler
/// restart would otherwise nuke every Pending Job. Returns 0
/// immediately.
///
/// Returns the count actually deleted (for the reconcile summary log).
pub(super) async fn reap_excess_pending(
    jobs_api: &Api<Job>,
    jobs: &[Job],
    queued: Option<u32>,
    pool: &str,
) -> u32 {
    let Some(queued) = queued else {
        debug!(
            pool,
            "skipping Pending-reap: queued unknown (scheduler unreachable)"
        );
        return 0;
    };
    let excess = select_excess_pending(jobs, queued, REAP_PENDING_GRACE);
    if excess.is_empty() {
        return 0;
    }
    let mut reaped = 0u32;
    for job in excess {
        let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
        // Foreground: the Job stays (with deletionTimestamp) until its
        // pod is gone, so the Job controller gets to remove the pod's
        // `batch.kubernetes.io/job-tracking` finalizer. Background
        // races Job-Complete: if the Job vanishes first the finalizer
        // is orphaned and the pod sits Terminating until GC catches up
        // — >180s under TCG, which times out the lifecycle VM-test
        // pod-phase wait. Reap targets are ready==0 (unscheduled /
        // ContainerCreating / just-completed) so foreground adds <1s.
        match jobs_api.delete(job_name, &DeleteParams::foreground()).await {
            Ok(_) => {
                info!(
                    pool, job = %job_name, queued,
                    "reaped excess Pending ephemeral Job (queued dropped below pending)"
                );
                reaped += 1;
            }
            Err(e) if e.is_not_found() => {
                debug!(pool, job = %job_name, "Pending Job already gone");
            }
            Err(e) => {
                warn!(
                    pool, job = %job_name, error = %e,
                    "failed to reap excess Pending Job; will retry next tick"
                );
            }
        }
    }
    if reaped > 0 {
        metrics::counter!(
            "rio_controller_ephemeral_jobs_reaped_total",
            "pool" => pool.to_owned(),
        )
        .increment(reaped.into());
    }
    reaped
}

/// `creation_timestamp` strictly before `now - min_age`. `None` →
/// not-old-enough (conservative; same posture as
/// [`select_excess_pending`]).
pub(super) fn job_older_than(j: &Job, min_age: Duration) -> bool {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    let cutoff = Time(
        k8s_openapi::jiff::Timestamp::now()
            - k8s_openapi::jiff::SignedDuration::try_from(min_age)
                .unwrap_or(k8s_openapi::jiff::SignedDuration::ZERO),
    );
    j.metadata
        .creation_timestamp
        .as_ref()
        .is_some_and(|t| t < &cutoff)
}

/// Running Jobs older than `min_age` whose executor is NOT busy in
/// the scheduler's view — the reap set for
/// `r[ctrl.ephemeral.reap-orphan-running+2]`.
///
/// "Not busy" = no `ExecutorInfo` whose `executor_id` starts with
/// `{job_name}-` (pod never registered / already disconnected), OR
/// such an executor exists with `busy == false` (registered but
/// idle past the builder's own 120s idle-exit — process can't act,
/// I-165 D-state). The `{job_name}-` prefix match relies on
/// `RIO_EXECUTOR_ID=$(POD_NAME)` via downward API; Job pod is
/// `{job_name}-{5char}`.
///
/// A Job whose executor reports `busy == true` is excluded —
/// the scheduler believes a build is in progress; deleting it would
/// orphan the build mid-flight. `activeDeadlineSeconds` is the
/// backstop for stuck-mid-build.
///
/// Pending Jobs are excluded ([`is_running_job`] requires `ready >
/// 0`) — those are [`reap_excess_pending`]'s territory.
pub(super) fn select_orphan_running<'a>(
    jobs: &'a [Job],
    executors: &[rio_proto::types::ExecutorInfo],
    min_age: Duration,
) -> Vec<&'a Job> {
    jobs.iter()
        .filter(|j| is_running_job(j) && job_older_than(j, min_age))
        .filter(|j| {
            let Some(job_name) = j.metadata.name.as_deref() else {
                // Can't delete by name anyway — and can't match
                // executor by prefix. Skip (conservative).
                return false;
            };
            let prefix = format!("{job_name}-");
            match executors
                .iter()
                .find(|e| e.executor_id.starts_with(&prefix))
            {
                // Registered + busy → scheduler owns it. Not orphan.
                Some(e) if e.busy => false,
                // Registered + idle past grace → process can't
                // self-exit (idle-timeout would have fired by 120s).
                Some(_) => true,
                // Not in executor list. Either never registered
                // (stuck before first heartbeat) or disconnected
                // without the Job going Failed. Past grace, both
                // are reapable — no assignment can land on it.
                None => true,
            }
        })
        .collect()
}

/// Fail-closed gate for [`reap_orphan_running`]: `None` (skip the
/// reap) for both `Err` AND `Ok(empty)`. On 2-replica scheduler
/// failover the new leader returns `Ok([])` — `self.executors` is
/// empty until workers reconnect, but `ListExecutors` passes
/// `ensure_leader()`. `select_orphan_running`'s `None => true` arm
/// would mark every Running Job past the 5-min grace as orphan and
/// foreground-delete it. In the genuine zero-executors steady state
/// there are no Running Jobs to reap anyway (workers register before
/// `ready>0`), so the empty-check costs nothing.
///
/// A server-side `recovery_complete` gate on `ListExecutors` is the
/// cleaner long-term fix but touches `rio-scheduler` + `rio-proto`.
pub(super) fn orphan_reap_gate(
    result: std::result::Result<Vec<rio_proto::types::ExecutorInfo>, tonic::Status>,
    pool: &str,
) -> Option<Vec<rio_proto::types::ExecutorInfo>> {
    match result {
        Ok(execs) if execs.is_empty() => {
            warn!(
                pool,
                "ListExecutors returned empty; skipping orphan-reap \
                 (fail-closed: scheduler may be mid-failover)"
            );
            None
        }
        Ok(execs) => Some(execs),
        Err(e) => {
            warn!(
                pool, error = %e,
                "ListExecutors failed; skipping orphan-reap this tick (fail-closed)"
            );
            None
        }
    }
}

// r[impl ctrl.ephemeral.reap-orphan-running+2]
/// Delete Running ephemeral Jobs the scheduler doesn't consider busy
/// after [`ORPHAN_REAP_GRACE`]. Same I-165 stuck-process failure
/// mode applies to both builder and fetcher pools.
///
/// Lazy RPC: `ListExecutors` is only called if there are Running Jobs
/// past the grace. The common case (all Jobs young or none Running)
/// costs zero scheduler round-trips.
///
/// Fail-closed: `ListExecutors` error → skip the reap entirely (can't
/// prove orphaned → don't delete). Same posture as
/// [`reap_excess_pending`]'s `queued = None` arm. A scheduler restart
/// must not nuke every Running ephemeral Job.
///
/// Returns the count actually deleted (for the reconcile summary log).
pub(super) async fn reap_orphan_running(
    jobs_api: &Api<Job>,
    jobs: &[Job],
    ctx: &Ctx,
    pool: &str,
) -> u32 {
    // Cheap pre-filter: any candidates at all? Avoids the RPC on the
    // hot path (every 10s tick × every pool).
    if !jobs
        .iter()
        .any(|j| is_running_job(j) && job_older_than(j, ORPHAN_REAP_GRACE))
    {
        return 0;
    }
    let result = admin_call(ctx.admin.clone().list_executors(
        rio_proto::types::ListExecutorsRequest {
            status_filter: String::new(),
        },
    ))
    .await
    .map(|r| r.into_inner().executors);
    let Some(executors) = orphan_reap_gate(result, pool) else {
        return 0;
    };
    let orphans = select_orphan_running(jobs, &executors, ORPHAN_REAP_GRACE);
    if orphans.is_empty() {
        return 0;
    }
    let mut reaped = 0u32;
    for job in orphans {
        let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
        // Foreground: same job-tracking-finalizer-orphan race as
        // reap_excess_pending (see its comment). Targets here are
        // ready>0 so foreground blocks until the pod actually
        // terminates, but orphans are past ORPHAN_REAP_GRACE with no
        // scheduler assignment — there's nothing to preempt. The wait
        // is per-reconcile-tick, not per-build.
        match jobs_api.delete(job_name, &DeleteParams::foreground()).await {
            Ok(_) => {
                info!(
                    pool, job = %job_name,
                    grace_secs = ORPHAN_REAP_GRACE.as_secs(),
                    "reaped orphan Running ephemeral Job (no scheduler assignment past grace)"
                );
                reaped += 1;
            }
            Err(e) if e.is_not_found() => {
                debug!(pool, job = %job_name, "orphan Job already gone");
            }
            Err(e) => {
                warn!(
                    pool, job = %job_name, error = %e,
                    "failed to reap orphan Running Job; will retry next tick"
                );
            }
        }
    }
    if reaped > 0 {
        metrics::counter!(
            "rio_controller_orphan_jobs_reaped_total",
            "pool" => pool.to_owned(),
        )
        .increment(reaped.into());
    }
    reaped
}

/// Outcome of a single `jobs_api.create` attempt. Caller decides
/// what to do on `Failed` — both ephemeral reconcilers warn+continue.
///
/// NOT a `Result`: `Failed` is not an error the caller propagates —
/// it's a classified non-success the caller handles inline. A
/// `Result<_, kube::Error>` with `?` at the call site would
/// re-introduce the bail this was extracted to eliminate. The enum
/// forces exhaustive handling.
pub(super) enum SpawnOutcome {
    Spawned,
    /// 409 AlreadyExists — Job for this `intent_id` already exists
    /// (deterministic name = intentional dedupe, 9ff95c7). The
    /// `skip_existing` pre-filter in `spawn_for_each` makes this the
    /// rare list-race fallback. Not worth propagating — would trigger
    /// error_policy backoff for what is expected-noise.
    NameCollision,
    /// Spawn failed (quota blip, admission webhook, apiserver flap).
    /// NOT a bail — P0516 (`33424b8a`): one spawn error shouldn't
    /// skip the rest of the tick. Subsequent spawns may succeed;
    /// the status patch below is independent. Caller logs `warn!`
    /// with its own context (bucket, queued, ceiling, etc).
    Failed(kube::Error),
}

/// Create a Job, classifying the outcome. Shared by both
/// ephemeral spawn loops — both had the same match arm at `ea64f7f2`;
/// warn+continue landed at P0516 (`33424b8a`); ephemeral
/// didn't. Extracting here means both get it, and P0522's threshold
/// lives in one place.
///
/// `PostParams::default` (not SSA): Jobs are create-once. SSA's
/// patch-merge semantics don't fit — there's no "update existing Job
/// to match spec," the Job is immutable after create (K8s rejects
/// most spec edits). A 409 means the intent's Job already exists
/// (deterministic naming) — `spawn_for_each` skips it next tick.
pub(super) async fn try_spawn_job(jobs_api: &Api<Job>, job: &Job) -> SpawnOutcome {
    match jobs_api.create(&PostParams::default(), job).await {
        Ok(_) => SpawnOutcome::Spawned,
        Err(e) if e.is_conflict() => SpawnOutcome::NameCollision,
        Err(e) => SpawnOutcome::Failed(e),
    }
}

/// Spawn one Job per item, logging each outcome. ADR-023: the
/// scheduler returns per-drv `SpawnIntent`s; the controller spawns
/// one pod per intent with that intent's resources stamped in — the
/// closure receives the item so `build_job(pool, intent)` can read it.
///
/// Returns the subset of `intents` whose Job now observably exists
/// (`Spawned` or `NameCollision` — a 409 means the Job is there).
/// `Failed` and `build_job`-Err entries are logged and OMITTED so the
/// caller does NOT ack them: acking a failed spawn arms the
/// scheduler's ICE-backoff timer for a Job that doesn't exist → no
/// heartbeat ever clears it → false ICE mark on the `(band, cap)`
/// cell. `skip_existing` hits are also omitted — the caller's
/// `pending_job_names` re-ack covers those independently.
///
/// The loop CONTINUES on every error — the P0516 invariant: a spawn
/// error never short-circuits the reconcile tick, so the caller's
/// status patch still runs. The structural guard at `pool/tests/
/// jobs_tests.rs::ephemeral_spawn_fail_still_patches_status` asserts
/// this body contains no `return Err`.
pub(super) async fn spawn_for_each(
    jobs_api: &Api<Job>,
    intents: &[SpawnIntent],
    skip_existing: &std::collections::HashSet<String>,
    pool: &str,
    mut build: impl FnMut(&SpawnIntent) -> Result<Job>,
) -> Vec<SpawnIntent> {
    let mut spawned = Vec::with_capacity(intents.len());
    for intent in intents {
        let job = match build(intent) {
            Ok(j) => j,
            Err(e) => {
                warn!(pool, error = %e, "build_job failed; continuing tick");
                continue;
            }
        };
        let job_name = job.metadata.name.clone().unwrap_or_default();
        // Pre-filter against the Job list already fetched this tick:
        // a still-Ready intent whose Job is already Running would
        // otherwise issue a create() that 409s every JOB_REQUEUE.
        // Names reaped this tick are excluded from `skip_existing`
        // by the caller so the post-reap respawn attempt still goes
        // out (worst-case 409 → next tick).
        if skip_existing.contains(&job_name) {
            debug!(pool, job = %job_name, "Job already exists; skipping create");
            continue;
        }
        match try_spawn_job(jobs_api, &job).await {
            SpawnOutcome::Spawned => {
                spawned.push(intent.clone());
                info!(pool, job = %job_name, "spawned ephemeral Job");
            }
            SpawnOutcome::NameCollision => {
                // 409 ⇒ a Job by that name exists; safe to ack.
                spawned.push(intent.clone());
                debug!(pool, job = %job_name, "Job name collision; will retry");
            }
            SpawnOutcome::Failed(e) => {
                warn!(
                    pool, job = %job_name, error = %e,
                    "ephemeral Job spawn failed; continuing tick"
                );
            }
        }
    }
    spawned
}

/// SSA-patch `.status.{replicas,readyReplicas,desiredReplicas,
/// conditions}` for a Pool CR.
///
/// "Replicas" means "active Jobs" — `kubectl get` columns are
/// filled here from the Job inventory.
///
/// `conditions`: `SchedulerUnreachable` reflects the reconciler's
/// poll-phase RPC result. `scheduler_err = Some` → status="True"
/// (operators see WHY nothing is spawning — otherwise `replicas=0`
/// looks like "no demand"). `None` → status="False" (clears stale
/// True after recovery). SSA with this field manager owns the
/// condition, so we write it every reconcile or a stale True would
/// persist. The autoscaler's `Scaling` condition lives under a
/// different field manager; SSA keeps them separate.
///
/// `prev_status`: the CR's `.status` (for `lastTransitionTime`
/// preservation).
#[allow(clippy::too_many_arguments)]
pub(super) async fn patch_job_pool_status(
    ctx: &Ctx,
    prev_status: Option<&PoolStatus>,
    ns: &str,
    name: &str,
    replicas: i32,
    ready: i32,
    desired: i32,
    scheduler_err: Option<&str>,
) -> Result<()> {
    let api: Api<Pool> = Api::namespaced(ctx.client.clone(), ns);
    let ar = Pool::api_resource();
    // Find the existing SchedulerUnreachable condition so its
    // `lastTransitionTime` can be preserved on non-transitions.
    let prev = prev_status
        .and_then(|s| {
            s.conditions
                .iter()
                .find(|c| c.type_ == "SchedulerUnreachable")
        })
        .and_then(|c| serde_json::to_value(c).ok());
    let cond = scheduler_unreachable_condition(scheduler_err, prev.as_ref());
    let status = serde_json::json!({
        "replicas": replicas,
        "readyReplicas": ready,
        "desiredReplicas": desired,
        "conditions": [cond],
    });
    api.patch_status(
        name,
        &kube::api::PatchParams::apply(MANAGER).force(),
        &kube::api::Patch::Apply(serde_json::json!({
            "apiVersion": ar.api_version,
            "kind": ar.kind,
            "status": status,
        })),
    )
    .await?;
    Ok(())
}

/// Build a `SchedulerUnreachable` K8s Condition for the Job-mode
/// reconciler's status patch.
///
/// `err = Some(msg)` → status="True", reason="ClusterStatusFailed",
/// message carries the gRPC error. Operators see `kubectl describe
/// wp` show why nothing is spawning (otherwise "queued=0" is
/// indistinguishable from "scheduler idle").
///
/// `err = None` → status="False", reason="ClusterStatusOK". We
/// write this every reconcile (not just on recovery) because SSA
/// with our field manager owns this condition — omitting it would
/// leave a stale True after the scheduler comes back.
///
/// json!-not-struct: k8s_openapi's Condition struct requires
/// observedGeneration which we don't track.
///
/// `prev`: existing SchedulerUnreachable condition (if any). Its
/// `lastTransitionTime` is preserved when `status` hasn't changed —
/// this reconciler writes every 10s tick; without preservation the
/// timestamp always reads "~10s ago" regardless of when the
/// scheduler actually went down/recovered.
// r[impl ctrl.condition.sched-unreachable]
// TODO(P0304): T505 adds an `rpc_name` param so the message names
// which RPC failed (ClusterStatus vs ListExecutors). Apply here
// post-extraction.
pub(super) fn scheduler_unreachable_condition(
    err: Option<&str>,
    prev: Option<&serde_json::Value>,
) -> serde_json::Value {
    let (status, reason, message) = match err {
        Some(e) => (
            "True",
            "ClusterStatusFailed",
            format!("ClusterStatus RPC failed: {e}; treating as queued=0"),
        ),
        None => (
            "False",
            "ClusterStatusOK",
            "scheduler reachable".to_string(),
        ),
    };
    // K8s convention: preserve `lastTransitionTime` if `status` is
    // unchanged, stamp now() on an actual transition (or first write).
    // Without this, writing the same condition every 10s tick makes
    // `lastTransitionTime` always read "~10s ago" — useless for "when
    // did the scheduler become unreachable."
    let transition_time = if let Some(p) = prev
        && p.get("status").and_then(|s| s.as_str()) == Some(status)
        && let Some(ts) = p.get("lastTransitionTime").and_then(|t| t.as_str())
    {
        ts.to_string()
    } else {
        k8s_openapi::jiff::Timestamp::now().to_string()
    };
    serde_json::json!({
        "type": "SchedulerUnreachable",
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": transition_time,
    })
}

/// Classify a Pod's termination reason from k8s PodStatus.
///
/// Precedence:
///   1. `status.reason == "Evicted"` + message → DiskPressure or Other.
///      Kubelet eviction sets `reason=Evicted` at the Pod level;
///      `message` carries the resource (`"The node was low on
///      resource: ephemeral-storage"`, `"…DiskPressure"`).
///   2. `containerStatuses[].state.terminated.reason` → OOMKilled /
///      Completed / Error. Only the rio-builder/rio-fetcher container
///      matters (single-container pod), so first terminated state wins.
///   3. Neither → Unknown (Pod still running, or status not yet
///      populated). Caller skips Unknown.
///
/// `ephemeral-storage` is matched alongside `DiskPressure` because
/// kubelet's eviction message for the per-pod ephemeral-storage limit
/// uses that phrase, not "DiskPressure" (DiskPressure is the NODE
/// condition; the per-pod limit eviction is the production firefox
/// I-213 case).
pub(super) fn pod_termination_reason(pod: &Pod) -> TerminationReason {
    let Some(status) = &pod.status else {
        return TerminationReason::Unknown;
    };
    if status.reason.as_deref() == Some("Evicted") {
        let msg = status.message.as_deref().unwrap_or("");
        // kubelet's per-pod limit message is "ephemeral local storage"
        // (spaces); the hyphenated "ephemeral-storage" is the resource
        // NAME and only appears in node-condition messages. The
        // emptyDir-sizeLimit eviction (`emptyDirLimit` in kubelet's
        // eviction manager) says `Usage of EmptyDir volume "<name>"
        // exceeds the limit "<N>".` — neither of the above substrings.
        // Match all three; all should bump the disk floor.
        if msg.contains("DiskPressure")
            || msg.contains("ephemeral-storage")
            || msg.contains("ephemeral local storage")
            || msg.contains("EmptyDir volume")
        {
            return TerminationReason::EvictedDiskPressure;
        }
        return TerminationReason::EvictedOther;
    }
    for cs in status.container_statuses.iter().flatten() {
        if let Some(term) = cs.state.as_ref().and_then(|st| st.terminated.as_ref()) {
            return match term.reason.as_deref() {
                Some("OOMKilled") => TerminationReason::OomKilled,
                Some("Completed") => TerminationReason::Completed,
                _ => TerminationReason::Error,
            };
        }
    }
    TerminationReason::Unknown
}

/// Report each terminated Pod's k8s reason to the scheduler so it can
/// gate `resource_floor` promotion on actual OOMKilled/DiskPressure
/// (not bare disconnect). Called from the Job-mode reconcilers' tick
/// after the spawn/reap steps.
///
/// Lists Pods (not Jobs — JobStatus doesn't carry per-container
/// termination reason) by `POOL_LABEL` selector. For each Pod with a
/// terminated container or Evicted status, calls `AdminService.
/// ReportExecutorTermination(executor_id = pod name, reason)`.
///
/// Idempotent: the scheduler's `recently_disconnected` map dedups
/// (first-report-wins, `remove()` on hit), so re-reporting the same
/// Pod every ~10s tick during `JOB_TTL_SECS=600` is a no-op past the
/// first. `Unknown` (Pod still running / status not populated) is
/// skipped — next tick will see the terminated state.
///
/// Best-effort: list error or RPC error is logged at debug/warn and
/// the reconcile continues. A missed report degrades to "one OOM
/// doesn't promote"; the next OOM on the same drv will (floor is
/// sticky). Never blocks the spawn/reap loop.
pub(super) async fn report_terminated_pods(ctx: &Ctx, ns: &str, pool: &str) {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let list = match pods
        .list(&ListParams::default().labels(&format!("{POOL_LABEL}={pool}")))
        .await
    {
        Ok(l) => l,
        Err(e) => {
            debug!(pool, error = %e, "report_terminated_pods: pod list failed; skipping");
            return;
        }
    };
    let mut admin = ctx.admin.clone();
    for pod in &list.items {
        let reason = pod_termination_reason(pod);
        // Only the promoting reasons go over the wire. `Completed`/
        // `Error`/`EvictedOther` are sent every tick for every TTL-
        // window Job today; the scheduler's `reason_label` gate
        // no-ops them anyway. `Error` IS observable for a deadline-
        // SIGKILL'd pod (restartPolicy:Never + backoffLimit:0 +
        // TGPS=7200 + job-tracking finalizer keep it listable for
        // the full grace window) — sending it would consume the
        // `recently_disconnected` entry the same-tick
        // `report_deadline_exceeded_jobs` needs. The scheduler-side
        // gate makes consume-then-noop unrepresentable; this filter
        // eliminates the wasted RPCs.
        if !matches!(
            reason,
            TerminationReason::OomKilled | TerminationReason::EvictedDiskPressure
        ) {
            continue;
        }
        let Some(name) = pod.metadata.name.as_deref() else {
            continue;
        };
        match admin_call(
            admin.report_executor_termination(ReportExecutorTerminationRequest {
                executor_id: name.to_owned(),
                reason: reason.into(),
            }),
        )
        .await
        {
            Ok(resp) => {
                if resp.into_inner().promoted {
                    info!(
                        pool, executor_id = %name, ?reason,
                        "reported pod termination → scheduler bumped resource_floor"
                    );
                }
            }
            Err(e) => {
                warn!(
                    pool, executor_id = %name, ?reason, error = %e,
                    "ReportExecutorTermination failed; skipping (best-effort)"
                );
                // Scheduler unreachable → no point retrying the rest
                // this tick. Next tick re-lists and retries.
                return;
            }
        }
    }
}

/// Job has a `Failed` condition with `reason=DeadlineExceeded` —
/// `activeDeadlineSeconds` fired. With `restartPolicy:Never` +
/// `backoffLimit:0` + TGPS=7200 + the `job-tracking` finalizer, the
/// SIGKILL'd pod IS listable (`deletionTimestamp` set,
/// `containerStatuses[].state.terminated.reason="Error"`) for the
/// full grace window — but `Error` is non-promoting, so
/// [`report_terminated_pods`] skips it; this reads the Job condition
/// instead.
pub(super) fn job_deadline_exceeded(job: &Job) -> bool {
    job.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "Failed" && c.reason.as_deref() == Some("DeadlineExceeded"))
}

/// Report each `DeadlineExceeded` Job to the scheduler so the
/// `activeDeadlineSeconds` backstop still climbs the resource_floor
/// ladder when the worker is too wedged to fire its own `daemon_timeout`
/// (`r[ctrl.terminated.deadline-exceeded+2]`). Defense-in-depth behind
/// the worker-side `BuildResultStatus::TimedOut` primary path.
///
/// Iterates the already-listed `jobs` (no extra apiserver call). For
/// each Job with a `Failed/DeadlineExceeded` condition, sends
/// `ReportExecutorTermination{executor_id = JOB name, reason =
/// DeadlineExceeded}`. The Job controller deletes the Pod when the
/// deadline fires, so the scheduler prefix-matches the Job name
/// against `recently_disconnected` keys (pod name = `{job}-{5char}`).
///
/// Idempotent per the same dedup as [`report_terminated_pods`]. Best-
/// effort: RPC error logged, reconcile continues. `JOB_TTL_SECS=600`
/// keeps the Job observable for ~60 reconcile ticks.
// r[impl ctrl.terminated.deadline-exceeded+2]
pub(super) async fn report_deadline_exceeded_jobs(ctx: &Ctx, jobs: &[Job]) {
    let mut admin = ctx.admin.clone();
    for job in jobs {
        if !job_deadline_exceeded(job) {
            continue;
        }
        let Some(name) = job.metadata.name.as_deref() else {
            continue;
        };
        match admin_call(
            admin.report_executor_termination(ReportExecutorTerminationRequest {
                executor_id: name.to_owned(),
                reason: TerminationReason::DeadlineExceeded.into(),
            }),
        )
        .await
        {
            Ok(resp) => {
                if resp.into_inner().promoted {
                    info!(
                        executor_id = %name,
                        "reported Job DeadlineExceeded → scheduler bumped resource_floor"
                    );
                }
            }
            Err(e) => {
                warn!(
                    executor_id = %name, error = %e,
                    "ReportExecutorTermination(DeadlineExceeded) failed; skipping (best-effort)"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::ResourceExt;

    /// SchedulerUnreachable condition: status flips True/False based
    /// on whether the ClusterStatus RPC failed. Operators need this
    /// to distinguish "scheduler idle (queued=0)" from "scheduler
    /// down (queued unknown, treated as 0)."
    // r[verify ctrl.condition.sched-unreachable]
    #[test]
    fn scheduler_unreachable_condition_shape() {
        // RPC failed → status=True, error in message.
        let c = scheduler_unreachable_condition(Some("connection refused"), None);
        assert_eq!(c["type"], "SchedulerUnreachable");
        assert_eq!(c["status"], "True");
        assert_eq!(c["reason"], "ClusterStatusFailed");
        assert!(
            c["message"]
                .as_str()
                .unwrap()
                .contains("connection refused")
        );
        // K8s requires lastTransitionTime (RFC3339).
        assert!(c["lastTransitionTime"].is_string());

        // RPC succeeded → status=False (clears stale True after
        // recovery).
        let c = scheduler_unreachable_condition(None, None);
        assert_eq!(c["type"], "SchedulerUnreachable");
        assert_eq!(c["status"], "False");
        assert_eq!(c["reason"], "ClusterStatusOK");

        // ── lastTransitionTime preservation (the let-chain at :706) ──
        // Same-status write (True→True): timestamp PRESERVED. Without
        // this the 10s tick stamps "now" every write and "when did the
        // scheduler become unreachable" reads ~10s ago regardless.
        let prev = serde_json::json!({
            "type": "SchedulerUnreachable",
            "status": "True",
            "lastTransitionTime": "2020-01-01T00:00:00Z",
        });
        let c = scheduler_unreachable_condition(Some("still refused"), Some(&prev));
        assert_eq!(
            c["lastTransitionTime"], "2020-01-01T00:00:00Z",
            "same-status write must preserve lastTransitionTime"
        );
        // Transition (True→False): fresh stamp.
        let c = scheduler_unreachable_condition(None, Some(&prev));
        assert_ne!(
            c["lastTransitionTime"], "2020-01-01T00:00:00Z",
            "True→False transition must stamp fresh lastTransitionTime"
        );
    }

    /// `pod_termination_reason` classification. Mirrors what k8s
    /// kubelet populates for each case.
    #[test]
    fn pod_termination_reason_classification() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };

        fn pod_with_term(reason: &str) -> Pod {
            Pod {
                status: Some(PodStatus {
                    container_statuses: Some(vec![ContainerStatus {
                        state: Some(ContainerState {
                            terminated: Some(ContainerStateTerminated {
                                reason: Some(reason.into()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
        fn pod_evicted(msg: &str) -> Pod {
            Pod {
                status: Some(PodStatus {
                    reason: Some("Evicted".into()),
                    message: Some(msg.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        assert_eq!(
            pod_termination_reason(&pod_with_term("OOMKilled")),
            TerminationReason::OomKilled
        );
        assert_eq!(
            pod_termination_reason(&pod_with_term("Completed")),
            TerminationReason::Completed
        );
        assert_eq!(
            pod_termination_reason(&pod_with_term("Error")),
            TerminationReason::Error
        );
        // Kubelet's per-pod ephemeral-storage limit eviction message
        // (the production firefox I-213 case). VERBATIM from live
        // cluster — ends with the limit value, NOT the resource name;
        // the original 2acd1b32 fixture ended in "ephemeral-storage"
        // and matched by accident.
        assert_eq!(
            pod_termination_reason(&pod_evicted(
                "Pod ephemeral local storage usage exceeds the total limit \
                 of containers 4Gi."
            )),
            TerminationReason::EvictedDiskPressure
        );
        // Node-condition form (resource name, hyphenated).
        assert_eq!(
            pod_termination_reason(&pod_evicted(
                "The node was low on resource: ephemeral-storage."
            )),
            TerminationReason::EvictedDiskPressure
        );
        // Node-condition DiskPressure eviction.
        assert_eq!(
            pod_termination_reason(&pod_evicted("The node was low on resource: DiskPressure.")),
            TerminationReason::EvictedDiskPressure
        );
        // emptyDir-sizeLimit eviction (kubelet `emptyDirLimit` check —
        // pkg/kubelet/eviction/helpers.go `emptyDirMessageFmt`). Fires
        // when the overlay sizeLimit (1.5×disk_bytes) is tighter than
        // the container's ephemeral-storage limit. Must bump the disk
        // floor, else the build loops at the same undersized overlay.
        assert_eq!(
            pod_termination_reason(&pod_evicted(
                "Usage of EmptyDir volume \"overlays\" exceeds the limit \
                 \"8053063680\". "
            )),
            TerminationReason::EvictedDiskPressure
        );
        // MemoryPressure eviction (node-level, NOT a per-drv signal).
        assert_eq!(
            pod_termination_reason(&pod_evicted("The node was low on resource: memory.")),
            TerminationReason::EvictedOther
        );
        // Still running → Unknown.
        assert_eq!(
            pod_termination_reason(&Pod::default()),
            TerminationReason::Unknown
        );
    }

    /// `job_deadline_exceeded` reads the `Failed/DeadlineExceeded` Job
    /// condition. Mirrors what the k8s Job controller sets when
    /// `activeDeadlineSeconds` fires (live: `kubectl get job -o
    /// jsonpath` showed `cond=FailureTarget Failed/DeadlineExceeded
    /// DeadlineExceeded`).
    // r[verify ctrl.terminated.deadline-exceeded+2]
    #[test]
    fn job_deadline_exceeded_condition() {
        use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

        fn job_with_cond(type_: &str, reason: Option<&str>) -> Job {
            Job {
                status: Some(JobStatus {
                    conditions: Some(vec![JobCondition {
                        type_: type_.into(),
                        reason: reason.map(String::from),
                        status: "True".into(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        assert!(job_deadline_exceeded(&job_with_cond(
            "Failed",
            Some("DeadlineExceeded")
        )));
        // Failed for another reason (BackoffLimitExceeded) → not a
        // deadline kill.
        assert!(!job_deadline_exceeded(&job_with_cond(
            "Failed",
            Some("BackoffLimitExceeded")
        )));
        // Complete → not deadline.
        assert!(!job_deadline_exceeded(&job_with_cond("Complete", None)));
        // No status → not deadline.
        assert!(!job_deadline_exceeded(&Job::default()));
    }

    /// `is_active_job`: verify status→predicate mapping for all four
    /// Job-status quadrants.
    #[test]
    fn job_status_predicates() {
        use k8s_openapi::api::batch::v1::JobStatus;

        fn job(succeeded: Option<i32>, failed: Option<i32>) -> Job {
            Job {
                status: Some(JobStatus {
                    succeeded,
                    failed,
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        // Fresh Job, no status → active.
        let fresh = Job::default();
        assert!(is_active_job(&fresh));

        // Running: status populated, neither terminal → active.
        let running = job(Some(0), Some(0));
        assert!(is_active_job(&running));

        // Succeeded: NOT active.
        let succeeded = job(Some(1), Some(0));
        assert!(!is_active_job(&succeeded));

        // Failed under backoff_limit=0: NOT active.
        let failed = job(Some(0), Some(1));
        assert!(!is_active_job(&failed));
    }

    fn job_with(name: &str, ready: Option<i32>, succeeded: Option<i32>, age_s: i64) -> Job {
        use k8s_openapi::api::batch::v1::JobStatus;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::{SignedDuration, Timestamp};
        use kube::api::ObjectMeta;
        Job {
            metadata: ObjectMeta {
                name: Some(name.into()),
                creation_timestamp: Some(Time(Timestamp::now() - SignedDuration::from_secs(age_s))),
                ..Default::default()
            },
            status: Some(JobStatus {
                ready,
                succeeded,
                failed: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// `is_pending_job`: active AND ready==0. With `parallelism=1` and
    /// no readiness probe, ready==1 ⇔ container started ⇔ may already
    /// hold an assignment. Only ready==0 (Pending phase / Container-
    /// Creating) is reap-safe.
    #[test]
    fn pending_job_predicate() {
        // Fresh Job, status=None entirely → pending (pod not created).
        assert!(is_pending_job(&Job::default()));
        // ready=0 → pending (ContainerCreating or unscheduled).
        assert!(is_pending_job(&job_with("a", Some(0), Some(0), 0)));
        // ready=None, active → pending (Job controller hasn't observed
        // pod readiness yet).
        assert!(is_pending_job(&job_with("a", None, Some(0), 0)));
        // ready=1 → Running. NOT pending — may hold assignment.
        assert!(!is_pending_job(&job_with("a", Some(1), Some(0), 0)));
        // succeeded=1 → Completed. NOT pending — TTL reaps.
        assert!(!is_pending_job(&job_with("a", Some(0), Some(1), 0)));
        // deletionTimestamp set → already terminating (foreground
        // delete in flight). NOT pending — re-selecting is a no-op
        // apiserver round-trip per tick.
        let mut terminating = job_with("a", Some(0), Some(0), 0);
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ));
        assert!(!is_pending_job(&terminating));
    }

    const NO_GRACE: Duration = Duration::ZERO;

    // r[verify ctrl.ephemeral.reap-excess-pending+2]
    /// I-183 scenario A: 3 Pending Jobs for class=medium, queued=1 →
    /// reap the 2 oldest, keep the 1 newest. The newest is closest to
    /// scheduling; the oldest has waited longest for a node Karpenter
    /// hasn't provisioned.
    #[test]
    fn select_excess_pending_reaps_oldest() {
        let jobs = vec![
            job_with("med-new", Some(0), Some(0), 30),
            job_with("med-mid", Some(0), Some(0), 60),
            job_with("med-old", Some(0), Some(0), 120),
        ];
        let excess = select_excess_pending(&jobs, 1, NO_GRACE);
        let names: Vec<_> = excess.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["med-old", "med-mid"],
            "deletes 2 oldest, keeps 1 newest"
        );
        // queued >= pending → nothing to reap.
        assert!(select_excess_pending(&jobs, 3, NO_GRACE).is_empty());
        assert!(select_excess_pending(&jobs, 5, NO_GRACE).is_empty());
        // queued=0 → reap all pending.
        assert_eq!(select_excess_pending(&jobs, 0, NO_GRACE).len(), 3);
    }

    // r[verify ctrl.ephemeral.reap-excess-pending+2]
    /// I-183 scenario B: 1 Pending + 2 Running, queued=0 → reap the
    /// 1 Pending only. Running Jobs are NOT touched — they may hold
    /// assignments; scheduler's cancel-on-disconnect handles those.
    #[test]
    fn select_excess_pending_ignores_running_and_completed() {
        let jobs = vec![
            job_with("pend", Some(0), Some(0), 30),
            job_with("run-a", Some(1), Some(0), 60),
            job_with("run-b", Some(1), Some(0), 90),
            job_with("done", Some(0), Some(1), 120),
        ];
        let excess = select_excess_pending(&jobs, 0, NO_GRACE);
        let names: Vec<_> = excess.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["pend"],
            "queued=0: only the Pending Job is reaped; Running/Completed untouched"
        );
    }

    // r[verify ctrl.ephemeral.reap-excess-pending+2]
    /// Grace window: a Job younger than `min_age` is excluded even if
    /// `ready=0`. `JobStatus.ready` is set asynchronously by the K8s
    /// Job controller; a freshly-started container may already hold an
    /// assignment while `ready` is still 0. The vm-lifecycle-autoscale
    /// ephemeral subtest tripped this on the first I-183 cut: Job
    /// reaped at age ~7s while its container was mid-build.
    #[test]
    fn select_excess_pending_respects_grace() {
        let jobs = vec![
            job_with("fresh", Some(0), Some(0), 3),
            job_with("aged", Some(0), Some(0), 60),
        ];
        let excess = select_excess_pending(&jobs, 0, REAP_PENDING_GRACE);
        let names: Vec<_> = excess.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["aged"],
            "fresh (3s < 10s grace) excluded; aged reapable"
        );
        // None timestamp → conservative (not-old-enough).
        let mut no_ts = job_with("no-ts", Some(0), Some(0), 60);
        no_ts.metadata.creation_timestamp = None;
        assert!(
            select_excess_pending(&[no_ts], 0, REAP_PENDING_GRACE).is_empty(),
            "no creation_timestamp → not reapable (conservative)"
        );
    }

    /// `is_running_job`: active AND ready>0. Exact complement of
    /// `is_pending_job` within the active set; Complete/Failed are
    /// neither.
    #[test]
    fn running_job_predicate() {
        assert!(!is_running_job(&Job::default()), "no status → not running");
        assert!(!is_running_job(&job_with("a", Some(0), Some(0), 0)));
        assert!(!is_running_job(&job_with("a", None, Some(0), 0)));
        assert!(is_running_job(&job_with("a", Some(1), Some(0), 0)));
        assert!(
            !is_running_job(&job_with("a", Some(0), Some(1), 0)),
            "Completed → not running"
        );
        // deletionTimestamp set → already terminating (foreground delete
        // in flight). NOT running — re-selecting it re-deletes + re-fires
        // ListExecutors + double-counts the orphan-reaped metric every tick.
        let mut terminating = job_with("a", Some(1), Some(0), 0);
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ));
        assert!(!is_running_job(&terminating));
    }

    /// Minimal `ExecutorInfo`. Only `executor_id` + `busy` are read
    /// by the orphan selector.
    fn executor(id: &str, busy: bool) -> rio_proto::types::ExecutorInfo {
        rio_proto::types::ExecutorInfo {
            executor_id: id.into(),
            busy,
            ..Default::default()
        }
    }

    // r[verify ctrl.ephemeral.reap-orphan-running+2]
    /// I-165: Running Jobs older than grace, scheduler has no live
    /// assignment for them → reap. Jobs whose executor reports
    /// `busy` are skipped (build in progress per scheduler;
    /// activeDeadlineSeconds is the backstop there).
    #[test]
    fn select_orphan_running_reaps_unassigned() {
        let jobs = vec![
            // Busy: executor registered, busy=true → skip.
            job_with("rio-builder-x86-busy01", Some(1), Some(0), 600),
            // Idle-stuck: executor registered, busy=false, past
            // grace → process can't self-exit (D-state). Reap.
            job_with("rio-builder-x86-idle01", Some(1), Some(0), 600),
            // Ghost: no executor entry at all, past grace → never
            // registered or disconnected without Job→Failed. Reap.
            job_with("rio-builder-x86-ghost1", Some(1), Some(0), 600),
        ];
        let executors = vec![
            executor("rio-builder-x86-busy01-abcde", true),
            executor("rio-builder-x86-idle01-fghij", false),
            // ghost1 has no entry
        ];
        let orphans = select_orphan_running(&jobs, &executors, ORPHAN_REAP_GRACE);
        let names: Vec<_> = orphans.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["rio-builder-x86-idle01", "rio-builder-x86-ghost1"],
            "busy skipped; idle-stuck + ghost reaped"
        );
    }

    // r[verify ctrl.ephemeral.reap-orphan-running+2]
    /// Grace + phase filtering: Jobs younger than grace are excluded
    /// (process-level idle-exit gets first chance); Pending and
    /// Completed Jobs are excluded (other reapers' territory).
    #[test]
    fn select_orphan_running_respects_grace_and_phase() {
        let jobs = vec![
            // Young Running, no executor → NOT reaped (under grace;
            // 120s idle-exit hasn't had its chance yet).
            job_with("rio-builder-x86-young1", Some(1), Some(0), 60),
            // Old Pending → NOT reaped (reap_excess_pending owns it).
            job_with("rio-builder-x86-pend01", Some(0), Some(0), 600),
            // Old Completed → NOT reaped (TTL owns it).
            job_with("rio-builder-x86-done01", Some(0), Some(1), 600),
            // Old Running, no executor → reaped.
            job_with("rio-builder-x86-stuck1", Some(1), Some(0), 600),
            // Old Running, deletionTimestamp set → NOT reaped (foreground
            // delete already in flight; re-selecting double-counts the
            // metric and re-fires ListExecutors every tick).
            {
                let mut j = job_with("rio-builder-x86-term01", Some(1), Some(0), 600);
                j.metadata.deletion_timestamp =
                    Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                        k8s_openapi::jiff::Timestamp::now(),
                    ));
                j
            },
        ];
        let orphans = select_orphan_running(&jobs, &[], ORPHAN_REAP_GRACE);
        let names: Vec<_> = orphans.iter().map(|j| j.name_any()).collect();
        assert_eq!(names, vec!["rio-builder-x86-stuck1"]);

        // None creation_timestamp → not-old-enough (conservative).
        let mut no_ts = job_with("rio-builder-x86-nots01", Some(1), Some(0), 600);
        no_ts.metadata.creation_timestamp = None;
        assert!(
            select_orphan_running(&[no_ts], &[], ORPHAN_REAP_GRACE).is_empty(),
            "no creation_timestamp → not orphan-reapable (conservative)"
        );
    }

    // r[verify ctrl.ephemeral.reap-orphan-running+2]
    /// `{job_name}-` prefix match: trailing dash prevents Job
    /// "pool-abc" from matching executor of Job "pool-abcdef".
    #[test]
    fn select_orphan_running_prefix_is_dash_anchored() {
        let jobs = vec![job_with("rio-builder-x86-abc", Some(1), Some(0), 600)];
        // Executor for a DIFFERENT Job whose name shares the prefix.
        let executors = vec![executor("rio-builder-x86-abcdef-qwert", true)];
        let orphans = select_orphan_running(&jobs, &executors, ORPHAN_REAP_GRACE);
        assert_eq!(
            orphans.len(),
            1,
            "executor 'abcdef-…' must NOT match Job 'abc' (would have \
             skipped as busy without the trailing-dash anchor)"
        );
    }
}
