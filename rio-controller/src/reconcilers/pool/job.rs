//! Job-mode plumbing for the Pool reconciler: list Jobs by label,
//! filter active, diff against demand, spawn deficit, reap excess,
//! patch status. Consumed only by `jobs.rs`; kept as a separate file
//! because the merged module would top 2000 LoC.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{Pod, PodSpec, PodTemplateSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
use kube::CustomResourceExt;
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams};
use tracing::{debug, info, warn};

use rio_proto::types::{SpawnIntent, TerminationReason};

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
///     mean N pods sharing one Job → N executors fighting over the
///     single intent the Job was spawned for (only one pull wins).
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
/// [`super::pod::build_executor_pod_spec`] (`r[ctrl.pod.tgps-default+4]`:
/// the 45s AD5 abort grace, unconditional — pull-mode pods abort and
/// report instead of draining).
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
/// boundary for `r[ctrl.ephemeral.reap-excess-pending+3]`.
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
/// boundary for `r[ctrl.ephemeral.reap-orphan-running+5]`: only Running
/// Jobs are candidates (Pending is handled by `reap_excess_pending`;
/// Complete/Failed by TTL).
///
/// A Job with `deletionTimestamp` set is NOT running — it's already
/// terminating (foreground-delete in flight). Re-selecting it would
/// re-delete + re-fire `ListOpenAttempts` (defeating the lazy-RPC
/// pre-filter) + double-count `rio_controller_orphan_jobs_reaped_total`
/// every tick until the pod's `job-tracking` finalizer clears. A
/// D-state pod (the I-165 case this reaper exists for) ignores SIGTERM
/// and stays listed for up to `terminationGracePeriodSeconds`.
pub(super) fn is_running_job(j: &Job) -> bool {
    j.metadata.deletion_timestamp.is_none()
        && is_active_job(j)
        && j.status.as_ref().and_then(|s| s.ready).unwrap_or(0) > 0
}

/// Minimum age before a Running Job is orphan-reapable. 5min: must
/// exceed the builder's idle-exit bound so the process-level exit
/// gets first chance — a healthy idle pod self-terminates at
/// [`super::pod::POOL_IDLE_EXIT_SECS`] (the controller-rendered
/// `RIO_IDLE_SECS`) and the Job goes Complete well before this fires;
/// the headroom is a `const_assert` beside that const, not prose. The
/// reap targets pods that CANNOT self-exit (I-165: D-state FUSE wait,
/// OOM-loop) and would otherwise burn `activeDeadlineSeconds`
/// (default 1h) holding a node.
// r[impl ctrl.ephemeral.reap-orphan-running+5]
pub(super) const ORPHAN_REAP_GRACE: Duration = Duration::from_secs(300);

/// Effective orphan-reap grace: [`ORPHAN_REAP_GRACE`] unless the
/// VM-fixture-only env override `RIO_ORPHAN_REAP_GRACE_OVERRIDE_SECS`
/// is set. The pull-mode VM scenarios hold a build past the grace to
/// prove the open-attempt busy bridge without waiting 5 real minutes;
/// env-only by design — NOT a controller Config field, so production
/// semantics (300 s) and the config schema are untouched. Unparsable
/// values fall back to the default.
pub(super) fn orphan_reap_grace() -> Duration {
    std::env::var("RIO_ORPHAN_REAP_GRACE_OVERRIDE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(ORPHAN_REAP_GRACE, Duration::from_secs)
}

/// The intent id a Job was spawned for: the `rio.build/intent-id`
/// pod-template annotation `build_job` stamps (the same value the pod
/// reads via downward API as `RIO_INTENT_ID`). `None` for Jobs created
/// before the annotation existed or outside the spawn path.
fn job_intent_id(j: &Job) -> Option<&str> {
    j.spec
        .as_ref()?
        .template
        .metadata
        .as_ref()?
        .annotations
        .as_ref()?
        .get(super::jobs::INTENT_ID_ANNOTATION)
        .map(String::as_str)
}

/// Is this Job covered by an open pull-mode attempt from the
/// scheduler's ledger-backed view (`AdminService.ListOpenAttempts`,
/// pull-filtered server-side)?
///
/// Match key: the Job's `rio.build/intent-id` pod-template annotation
/// equals the attempt's `intent_id` — attempts are bound to the
/// HMAC-attested intent identity, which is exactly what the spawn path
/// stamps on the Job. (The stream-era `{job_name}-` executor-id prefix
/// fallback was dropped with the stream path: attempt identity is the
/// only correlation left, and it is authoritative.)
///
/// A covered Job is busy: its build is in flight.
fn covered_by_open_pull_attempt(
    job: &Job,
    open_attempts: &[rio_proto::types::OpenAttempt],
) -> bool {
    if open_attempts.is_empty() {
        return false;
    }
    let intent = job_intent_id(job);
    open_attempts
        .iter()
        .any(|a| intent.is_some_and(|i| !i.is_empty() && a.intent_id == i))
}

/// Minimum age before a Pending Job is reapable. `JobStatus.ready` is
/// set by the K8s Job controller AFTER it observes pod readiness — a
/// container that just started may have already heartbeated and
/// received an assignment while `ready` is still 0 (Job-controller
/// sync lag, typically <1s but unbounded under apiserver load). One
/// requeue tick of grace makes the false-positive window negligible
/// without materially delaying the I-183 reap (the bug is Jobs sitting
/// for an HOUR; 10s grace is noise).
///
/// NOTE: this is age-from-**creation**. A cold-start Job that takes
/// 50s for Karpenter to provision a node is past this grace the moment
/// its pod starts — that's the case [`any_live_running_pod`] covers.
// r[impl ctrl.ephemeral.reap-excess-pending+3]
pub(super) const REAP_PENDING_GRACE: Duration = Duration::from_secs(10);

/// Live (non-informer) check: any pod of `job_name` in `phase==Running`?
///
/// `JobStatus.ready` (the snapshot the Pending-reap selects on) lags
/// the actual pod phase by kubelet→apiserver→Job-controller→informer;
/// after a Karpenter cold start the pod can be Running and assigned
/// while the snapshot still says `ready==0`. A live `pods.list` here
/// closes the gap for the rare excess-reap candidate.
///
/// `Err` → fail-closed: caller skips the delete (next tick will see
/// `ready>0` and the Job leaves the Pending set anyway).
async fn any_live_running_pod(pods_api: &Api<Pod>, job_name: &str) -> kube::Result<bool> {
    let list = pods_api
        .list(&ListParams::default().labels(&format!("job-name={job_name}")))
        .await?;
    Ok(list
        .items
        .iter()
        .any(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running")))
}

/// Pending Jobs in excess of `queued`, oldest-first — RESIDUAL
/// fallback for `r[ctrl.ephemeral.reap-excess-pending+3]` after
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
///
/// `reaped`: names already foreground-deleted THIS tick by
/// `reap_stale_for_intents`. Those Jobs' snapshot `deletion_timestamp`
/// is still `None` (snapshot pre-dates the delete), so without this
/// filter a younger reaped orphan still counts toward `pending`, the
/// oldest-first sort then deletes a still-WANTED Job — exactly what
/// the orphan-first reap exists to prevent (see `jobs.rs` callsite).
pub(super) fn select_excess_pending<'a>(
    jobs: &'a [Job],
    reaped: &HashSet<String>,
    queued: u32,
    min_age: Duration,
) -> Vec<&'a Job> {
    let mut pending: Vec<&Job> = jobs
        .iter()
        .filter(|j| {
            is_pending_job(j)
                && job_older_than(j, min_age)
                && !j
                    .metadata
                    .name
                    .as_deref()
                    .is_some_and(|n| reaped.contains(n))
        })
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

// r[impl ctrl.ephemeral.reap-excess-pending+3]
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
/// Deletions route through [`delete_job_with_synthesized_report`]
/// (reason `Reaped`): a Pending Job's pod normally has never pulled,
/// so the synthesize arm degenerates to today's plain delete; the rare
/// pulled-then-crashed-before-ready case gets its open attempt closed
/// at deletion instead of waiting for the establishment sweep.
///
/// Returns the count actually deleted (for the reconcile summary log).
pub(super) async fn reap_excess_pending(
    jobs_api: &Api<Job>,
    pods_api: &Api<Pod>,
    jobs: &[Job],
    reaped: &HashSet<String>,
    queued: Option<u32>,
    ctx: &Ctx,
    pool: &str,
) -> u32 {
    let Some(queued) = queued else {
        debug!(
            pool,
            "skipping Pending-reap: queued unknown (scheduler unreachable)"
        );
        return 0;
    };
    let excess = select_excess_pending(jobs, reaped, queued, REAP_PENDING_GRACE);
    if excess.is_empty() {
        return 0;
    }
    // One best-effort view read per tick-with-deletions (never per Job)
    // for the synthesize-on-delete arm.
    let open_attempts = open_attempts_best_effort(ctx, pool).await;
    let mut reaped = 0u32;
    for job in excess {
        let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
        // Live phase re-check: `select_excess_pending` keys on the
        // informer-cached `JobStatus.ready==0`, which lags
        // `Pod.status.phase` by kubelet→Job-controller→informer. After
        // a Karpenter cold start the pod may already be Running and
        // hold an assignment (scheduler `queued` dropped on assign,
        // which is why we're here). Fail-closed on lookup error — next
        // tick's snapshot will see `ready>0`.
        match any_live_running_pod(pods_api, job_name).await {
            Ok(true) => {
                debug!(
                    pool, job = %job_name,
                    "skipping reap: pod is live Running (informer-cached Job.status.ready lags)"
                );
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(
                    pool, job = %job_name, error = %e,
                    "skipping reap: live pod-phase check failed (fail-closed)"
                );
                continue;
            }
        }
        // Foreground: the Job stays (with deletionTimestamp) until its
        // pod is gone, so the Job controller gets to remove the pod's
        // `batch.kubernetes.io/job-tracking` finalizer. Background
        // races Job-Complete: if the Job vanishes first the finalizer
        // is orphaned and the pod sits Terminating until GC catches up
        // — >180s under TCG, which times out the lifecycle VM-test
        // pod-phase wait. Reap targets are ready==0 (unscheduled /
        // ContainerCreating / just-completed) so foreground adds <1s.
        match delete_job_with_synthesized_report(
            jobs_api,
            ctx,
            job,
            job_name,
            &DeleteParams::foreground(),
            rio_proto::types::AttemptTerminalReason::Reaped,
            &open_attempts,
        )
        .await
        {
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

/// Prometheus `reason` label for the OA1 histogram on the
/// synthesize-on-delete path. The controller-synthesized verdicts only
/// (cancelled / preempted / reaped); the pod-terminal classifications
/// keep flowing through [`termination_reason_label`].
fn attempt_reason_label(reason: rio_proto::types::AttemptTerminalReason) -> &'static str {
    use rio_proto::types::AttemptTerminalReason as R;
    match reason {
        R::Cancelled => "cancelled",
        R::Preempted => "preempted",
        R::Reaped => "reaped",
        _ => "other",
    }
}

/// The synthesize-on-delete decision: `Some(request)` exactly when an
/// open pull-mode attempt covers the Job about to be deleted (the
/// pull-filtered `ListOpenAttempts` view is the input; the match key
/// is the Job's intent annotation, the same correlation the busy view
/// uses). Pure, so the no-RPC-when-no-attempt property is
/// unit-testable without a wire.
///
/// The request is keyed by the attempt's `exec_id` (the strongest
/// identity), carries the Job name and the attempt's intent id for the
/// scheduler's resolution fallbacks, and forwards the attempt's
/// `source_node` as the AD2c attribution when known.
// r[impl ctrl.job.synthesize-on-delete]
pub(super) fn synthesized_report_for_job(
    job: &Job,
    reason: rio_proto::types::AttemptTerminalReason,
    open_attempts: &[rio_proto::types::OpenAttempt],
) -> Option<rio_proto::types::ReportAttemptOutcomeRequest> {
    let job_name = job.metadata.name.clone().unwrap_or_default();
    synthesized_report_for_intent(
        job_intent_id(job),
        job_name.clone(),
        reason,
        open_attempts,
        AttemptOwner::Job(&job_name),
    )
}

/// merged_bug_298: the owner identity a synthesized verdict must bind.
/// The retired resolution matched by `intent_id` alone over the
/// cluster-wide `ListOpenAttempts` view — pool B's reap (or a
/// disruption event) could close pool A's healthy attempt with a
/// charge-free verdict against the wrong executor. There is no
/// constructor without an owner, so an unowned attempt is unmatchable
/// by construction.
pub(super) enum AttemptOwner<'a> {
    /// Job-delete arms: a pull-mode attempt's `executor_id` is the k8s
    /// pod name, and a Job's pods carry the Job name as a dash-joined
    /// prefix with a dashless random suffix.
    Job(&'a str),
    /// The disruption watcher targets one pod on one known node.
    Pod { pod: &'a str, node: &'a str },
}

impl AttemptOwner<'_> {
    fn owns(&self, a: &rio_proto::types::OpenAttempt) -> bool {
        match self {
            // `{job}-{suffix}` with a dashless suffix: "metal" owns
            // "metal-7k2fq" but not "metal-big-7k2fq" (a sibling Job's
            // pod), and never bare "metal".
            AttemptOwner::Job(job) => a.executor_id.strip_prefix(job).is_some_and(|rest| {
                rest.len() > 1 && rest.starts_with('-') && !rest[1..].contains('-')
            }),
            AttemptOwner::Pod { pod, node } => {
                a.executor_id == *pod
                    || (!node.is_empty() && !a.source_node.is_empty() && a.source_node == *node)
            }
        }
    }
}

// r[impl ctrl.drain.disruption-target+4]
/// The shared synthesized-verdict constructor (merged_bug_135,
/// owner-bound per merged_bug_298): a controller-synthesized terminal
/// report exists ONLY when an open attempt exists for the intent AND
/// the caller owns it, and is keyed by THAT attempt's `exec_id` — the
/// scheduler refuses exec_id-less synthesized verdicts, so a caller
/// with no owned open attempt sends nothing (the Job delete still
/// proceeds; the establishment sweep classifies any attempt that
/// appears later). One constructor means the disruption watcher and
/// every Job-delete arm speak the same identity rule.
pub(super) fn synthesized_report_for_intent(
    intent: Option<&str>,
    job_name: String,
    reason: rio_proto::types::AttemptTerminalReason,
    open_attempts: &[rio_proto::types::OpenAttempt],
    owner: AttemptOwner<'_>,
) -> Option<rio_proto::types::ReportAttemptOutcomeRequest> {
    let attempt = open_attempts
        .iter()
        .find(|a| intent.is_some_and(|i| !i.is_empty() && a.intent_id == i) && owner.owns(a))?;
    Some(rio_proto::types::ReportAttemptOutcomeRequest {
        resubmit_cycle: 0,
        intent_id: attempt.intent_id.clone(),
        job_name,
        exec_id: attempt.exec_id.clone(),
        reason: reason.into(),
        node_name: attempt.source_node.clone(),
    })
}

/// Delete one Job, synthesizing the terminal `ReportAttemptOutcome`
/// first when (and only when) the Job still has an open pull-mode
/// attempt. The deletion this performs destroys the only Job/pod
/// terminal status the unified report path could otherwise fold, so
/// the controller speaks for it (reason cancelled / preempted /
/// reaped) before the object goes away; for any Job without an open
/// pull-mode attempt — including stream Jobs mid-build, which the
/// pull-filtered view never lists — this is exactly today's deletion
/// and no `ReportAttemptOutcome` RPC is attempted.
///
/// The synthesis is best-effort: a failed report is logged and the
/// deletion proceeds — the establishment sweep remains the fallback
/// classifier (the cost of a missed synthesis is requeue latency,
/// never a lost or doubled charge, by the scheduler's idempotent-fill
/// rule). The delete error is returned unchanged so call sites keep
/// their existing Ok/NotFound/Err handling (the deleted-object body is
/// dropped — no current call site reads it).
// r[impl ctrl.job.synthesize-on-delete]
pub(super) async fn delete_job_with_synthesized_report(
    jobs_api: &Api<Job>,
    ctx: &Ctx,
    job: &Job,
    job_name: &str,
    params: &DeleteParams,
    reason: rio_proto::types::AttemptTerminalReason,
    open_attempts: &[rio_proto::types::OpenAttempt],
) -> kube::Result<()> {
    if let Some(request) = synthesized_report_for_job(job, reason, open_attempts) {
        let exec_id = request.exec_id.clone();
        match admin_call(ctx.admin.clone().report_attempt_outcome(request)).await {
            Ok(_) => {
                info!(
                    job = %job_name, exec_id = %exec_id, reason = ?reason,
                    "synthesized ReportAttemptOutcome for open pull-mode attempt before Job deletion"
                );
                // OA1: the synthesized path closes the terminal→report
                // interval at the moment of deletion (the controller is
                // the initiator, so the interval is ~0 by construction).
                // Sampled once per Job name so a delete that fails and
                // retries next tick doesn't re-record.
                if first_terminal_report_sample(
                    &mut ctx.terminal_report_sampled.lock(),
                    job_name,
                    epoch_now_secs(),
                    epoch_now_secs(),
                )
                .is_some()
                {
                    metrics::histogram!(
                        "rio_controller_job_terminal_report_seconds",
                        "reason" => attempt_reason_label(reason)
                    )
                    .record(0.0);
                }
            }
            Err(e) => {
                warn!(
                    job = %job_name, exec_id = %exec_id, reason = ?reason, error = %e,
                    "failed to synthesize ReportAttemptOutcome before Job deletion; \
                     proceeding with the delete (establishment sweep is the fallback)"
                );
            }
        }
    }
    jobs_api.delete(job_name, params).await.map(|_| ())
}

/// Best-effort read of the open pull-mode attempt view for the
/// synthesize-on-delete arm of the C1/C2 reap paths. `&[]` (empty) when
/// the view is unavailable this tick — callers proceed with the plain
/// delete; the establishment sweep is the fallback classifier. The
/// orphan-Running reap (C3) does NOT use this: its read is fail-closed
/// (`reap_orphan_running` skips the whole reap on error) because there
/// the view is a busy-signal input, not just attribution.
pub(super) async fn open_attempts_best_effort(
    ctx: &Ctx,
    pool: &str,
) -> Vec<rio_proto::types::OpenAttempt> {
    match admin_call(
        ctx.admin
            .clone()
            .list_open_attempts(rio_proto::types::ListOpenAttemptsRequest {}),
    )
    .await
    {
        Ok(resp) => resp.into_inner().attempts,
        Err(e) => {
            debug!(
                pool, error = %e,
                "ListOpenAttempts unavailable; deleting without synthesized reports this tick"
            );
            Vec::new()
        }
    }
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

/// Running Jobs older than `min_age` with no open pull-mode attempt
/// covering them — the reap set for
/// `r[ctrl.ephemeral.reap-orphan-running+5]`.
///
/// Busy has exactly one carrier: an open attempt from the scheduler's
/// durable open-attempt view ([`covered_by_open_pull_attempt`]). A
/// covered Job is never selected — the ledger says a build is in
/// flight; deleting it would orphan the build mid-flight
/// (`activeDeadlineSeconds` is the backstop for stuck-mid-build). An
/// uncovered Running Job past the grace is reapable: its pod either
/// never managed a successful pull (stuck before delivery — the I-165
/// D-state case this reaper exists for) or its attempt has already
/// been classified — either way no new work can land on it (a pull
/// only ever delivers the intent the pod was spawned for).
///
/// Pending Jobs are excluded ([`is_running_job`] requires `ready >
/// 0`) — those are [`reap_excess_pending`]'s territory.
///
/// `reaped`: names already foreground-deleted THIS tick by
/// `reap_stale_for_intents`. The 5-min grace makes the same-tick race
/// unlikely but the filter is structurally consistent with
/// [`select_excess_pending`] and free.
// r[impl ctrl.job.busy-from-open-attempts+2]
pub(super) fn select_orphan_running<'a>(
    jobs: &'a [Job],
    reaped: &HashSet<String>,
    open_attempts: &[rio_proto::types::OpenAttempt],
    min_age: Duration,
) -> Vec<&'a Job> {
    jobs.iter()
        .filter(|j| {
            is_running_job(j)
                && job_older_than(j, min_age)
                && !j
                    .metadata
                    .name
                    .as_deref()
                    .is_some_and(|n| reaped.contains(n))
        })
        .filter(|j| {
            if j.metadata.name.is_none() {
                // Can't delete by name anyway. Skip (conservative).
                return false;
            }
            // The single busy arm: an open attempt is the ledger's
            // word that a build is in flight on this Job's pod.
            !covered_by_open_pull_attempt(j, open_attempts)
        })
        .collect()
}

// r[impl ctrl.ephemeral.reap-orphan-running+5]
// r[impl ctrl.job.busy-from-open-attempts+2]
/// Delete Running ephemeral Jobs with no open attempt covering them
/// after [`orphan_reap_grace`]. Same I-165 stuck-process failure
/// mode applies to both builder and fetcher pools.
///
/// Busy is the ledger-backed open-attempt view (`ListOpenAttempts`,
/// pull-filtered server-side) — durable PG state that survives
/// scheduler failover, so a successful read is authoritative whatever
/// its size. The one freshness input is the leader's OWN AGE
/// (`leader_for_secs`): a never-pulled pod has no row BY CONSTRUCTION
/// (the builder retries pull transport errors forever and a row only
/// exists after a successful mint), so during a scheduler outage +
/// failover the view can be durably, truthfully EMPTY of rows for
/// pods that are about to pull. Reaping on that emptiness right after
/// failover would mass-delete the whole waiting cohort; gating on
/// `leader_for_secs >= grace` gives every such pod one full grace
/// against the NEW leader before absence becomes reapable.
///
/// Lazy RPC: `ListOpenAttempts` is only called if there are Running
/// Jobs past the grace. The common case (all Jobs young or none
/// Running) costs zero scheduler round-trips.
///
/// Fail-closed: an error on the view read → skip the reap entirely
/// (can't prove orphaned → don't delete). Same posture as
/// [`reap_excess_pending`]'s `queued = None` arm. A scheduler restart
/// must not nuke every Running ephemeral Job.
///
/// Returns the count actually deleted (for the reconcile summary log).
pub(super) async fn reap_orphan_running(
    jobs_api: &Api<Job>,
    jobs: &[Job],
    reaped: &HashSet<String>,
    ctx: &Ctx,
    pool: &str,
) -> u32 {
    let grace = orphan_reap_grace();
    // Cheap pre-filter: any candidates at all? Avoids the RPC on the
    // hot path (every 10s tick × every pool). Same `reaped` skip as
    // `select_orphan_running` so the lazy-RPC short-circuit is
    // consistent with the actual selection.
    if !jobs.iter().any(|j| {
        is_running_job(j)
            && job_older_than(j, grace)
            && !j
                .metadata
                .name
                .as_deref()
                .is_some_and(|n| reaped.contains(n))
    }) {
        return 0;
    }
    // The busy source: the open pull-mode attempt view. Fail-closed —
    // an error here means the ledger view is unavailable, so absence
    // cannot be proven and nothing is reaped this tick. Freshness is
    // the leader-age gate below (rationale single-homed in this fn's
    // doc).
    let resp = match admin_call(
        ctx.admin
            .clone()
            .list_open_attempts(rio_proto::types::ListOpenAttemptsRequest {}),
    )
    .await
    {
        Ok(resp) => resp.into_inner(),
        Err(e) => {
            warn!(
                pool, error = %e,
                "ListOpenAttempts failed; skipping orphan-reap this tick (fail-closed)"
            );
            return 0;
        }
    };
    // r[impl ctrl.job.orphan-leader-age]
    // Freshness gate (merged_bug_221): never-pulled pods have no row
    // by construction, so a freshly-failed-over leader's view cannot
    // distinguish "orphaned" from "about to pull". One full grace
    // against the NEW leader before absence is actionable.
    if resp.leader_for_secs < grace.as_secs() {
        debug!(
            pool,
            leader_for_secs = resp.leader_for_secs,
            grace_secs = grace.as_secs(),
            "leader younger than the orphan grace; skipping orphan-reap this tick (fail-closed)"
        );
        return 0;
    }
    let open_attempts = resp.attempts;
    let orphans = select_orphan_running(jobs, reaped, &open_attempts, grace);
    if orphans.is_empty() {
        return 0;
    }
    let mut reaped = 0u32;
    for job in orphans {
        let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
        // Foreground: same job-tracking-finalizer-orphan race as
        // reap_excess_pending (see its comment). Targets here are
        // ready>0 so foreground blocks until the pod actually
        // terminates, but orphans are past the grace with no
        // scheduler assignment — there's nothing to preempt. The wait
        // is per-reconcile-tick, not per-build.
        //
        // The synthesize arm is structurally inert here: a Job covered
        // by an open pull-mode attempt was never selected as orphan
        // (the busy bridge above), so the helper degenerates to the
        // plain delete. Routed through it anyway so every controller
        // Job-delete call site speaks the same way.
        match delete_job_with_synthesized_report(
            jobs_api,
            ctx,
            job,
            job_name,
            &DeleteParams::foreground(),
            rio_proto::types::AttemptTerminalReason::Reaped,
            &open_attempts,
        )
        .await
        {
            Ok(_) => {
                info!(
                    pool, job = %job_name,
                    grace_secs = grace.as_secs(),
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

/// §5-Q19 (signed): PG↔kube-apiserver clock-skew slack for the cancel
/// arm's generation conjunct. Both clocks are server-side/NTP-
/// disciplined; ≤ 10 s of skew stands as a deployment guarantee.
/// Documented const, deliberately not config (config would drag the
/// BLESS/docs-data schema obligations for a deployment invariant).
/// A genuine cancel whose close lands within the slack of Job
/// creation (insta-cancel inside the window) NEVER binds — both
/// `closed_age_secs` and the Job's age are live-recomputed, so the
/// generation inequality is time-invariant for that close, not a
/// delay: the missed CLASS is bounded by the slack window, and such
/// closes are handled entirely by the backstop pair (orphan-reap /
/// `activeDeadlineSeconds`, up to ~300s). The signed trade accepts
/// that bounded miss to keep respawned same-name Jobs structurally
/// unselectable.
pub(super) const CANCEL_CLOSE_SKEW_SLACK_SECS: u64 = 10;

/// A close bound to a Job it may tear down — the ONLY way the cancel
/// arm selects (bug_113). [`CancelTarget::bind`] owns ALL FOUR
/// conjuncts; call sites cannot recombine a subset.
pub(super) struct CancelTarget<'a> {
    pub(super) job: &'a Job,
}

impl<'a> CancelTarget<'a> {
    // r[impl ctrl.job.cancel-close-cause+2]
    /// Bind `close` to `job` iff every conjunct holds:
    ///
    /// 1. **cause**: the close is `CLOSE_CAUSE_CANCELLED` (a normal
    ///    completion in the Job-status propagation lag is untouchable
    ///    by type);
    /// 2. **intent**: the Job's intent annotation matches the close;
    /// 3. **generation**: the Job PREDATES the close —
    ///    `job_older_than(closed_age_secs + CANCEL_CLOSE_SKEW_SLACK_SECS)`.
    ///    A derivation cancelled and re-submitted respawns the SAME
    ///    deterministic Job name; the fresh Job is younger than the
    ///    close and therefore structurally unselectable, whether or
    ///    not its pod has pulled yet (`closed_age_secs` is already on
    ///    the wire — PG clock; the slack absorbs PG↔apiserver skew);
    /// 4. **liveness**: no open attempt covers the Job (a re-dispatch
    ///    that already pulled is busy, not cancellable evidence).
    pub(super) fn bind(
        close: &rio_proto::types::ClosedAttempt,
        job: &'a Job,
        open_attempts: &[rio_proto::types::OpenAttempt],
    ) -> Option<Self> {
        if close.cause != rio_proto::types::CloseCause::Cancelled as i32 {
            return None;
        }
        let intent = job_intent_id(job)?;
        if intent.is_empty() || intent != close.intent_id {
            return None;
        }
        let min_age = Duration::from_secs(
            close
                .closed_age_secs
                .saturating_add(CANCEL_CLOSE_SKEW_SLACK_SECS),
        );
        if !job_older_than(job, min_age) {
            return None;
        }
        if covered_by_open_pull_attempt(job, open_attempts) {
            return None;
        }
        Some(Self { job })
    }
}

/// The cancel arm's pure selection fold: a Job is selected iff some
/// `recently_closed` entry binds to it through [`CancelTarget::bind`]
/// (all four conjuncts — cause, intent, generation, liveness). The
/// cause travels WITH the close on the wire — closed-ness alone (the
/// retired closed→active edge inference over tick-local `seen_open`
/// evidence) stopped being a verdict, and bare absence (a pod that
/// never pulled) matches no entry at all. Re-dispatch is covered
/// structurally: the respawned Job postdates the close (conjunct 3)
/// and, once pulled, is covered by its fresh open attempt (conjunct
/// 4) — the guarantee no longer leans on pull timing.
pub(super) fn select_closed_attempt_jobs<'a>(
    active: &[&'a Job],
    open_attempts: &[rio_proto::types::OpenAttempt],
    recently_closed: &[rio_proto::types::ClosedAttempt],
) -> Vec<&'a Job> {
    active
        .iter()
        .filter_map(|j| {
            recently_closed
                .iter()
                .find_map(|c| CancelTarget::bind(c, j, open_attempts))
        })
        .map(|t| t.job)
        .collect()
}

// r[impl ctrl.drain.disruption-target+4]
/// AD5 cancel arm: each tick, foreground-delete an active Job whose
/// attempt has CLOSED while the Job is still active, so a
/// scheduler-side cancel verdict aborts the pod now (Job deletion →
/// SIGTERM → builder cgroup-kill) instead of at
/// `activeDeadlineSeconds`.
///
/// Evidence rule: a Job is cancelled only when the scheduler's
/// `recently_closed` window lists its intent with
/// `CLOSE_CAUSE_CANCELLED` and no open attempt covers it — the cause
/// travels with the close ([`select_closed_attempt_jobs`]). Bare
/// absence is NEVER cancellation evidence: a pull-mode Job whose pod
/// has not yet pulled (Pending, image pull, cold NodeClaim, pull-retry
/// backoff) or is receiving `NotYetReady` matches no closed entry and
/// is never touched by this arm — those Jobs remain covered only by
/// the grace-gated orphan reap and `activeDeadlineSeconds`. The view
/// read is fail-closed exactly like the orphan reap's: an error
/// produces no cancel decisions. A close that ages out of the window
/// during a controller outage falls back to the orphan-reap arm /
/// `activeDeadlineSeconds` (accepted; see the composite-budget note at
/// [`super::pod::PULL_MODE_TGPS_SECS`]). Latency is bounded by the
/// window (the close is visible for `RECENTLY_CLOSED_WINDOW_SECS` on
/// the scheduler side) plus one controller tick.
///
/// Deletions route through [`delete_job_with_synthesized_report`]
/// (reason `Cancelled`): the attempt is already closed, so the
/// synthesize arm is structurally inert (the scheduler already holds
/// the verdict — there is nothing left to report), but the shared
/// helper keeps every controller Job-delete call site speaking the
/// same way and covers the race where the view briefly still lists
/// the attempt at delete time.
///
/// Returns the number of Jobs deleted this tick.
pub(super) async fn cancel_closed_attempt_jobs(
    jobs_api: &Api<Job>,
    jobs: &[Job],
    ctx: &Ctx,
    pool: &str,
) -> u32 {
    let active: Vec<&Job> = jobs
        .iter()
        .filter(|j| is_active_job(j) && j.metadata.deletion_timestamp.is_none())
        .collect();
    if active.is_empty() {
        return 0;
    }
    // One view read per tick; fail-closed on error (no decisions).
    let resp = match admin_call(
        ctx.admin
            .clone()
            .list_open_attempts(rio_proto::types::ListOpenAttemptsRequest {}),
    )
    .await
    {
        Ok(resp) => resp.into_inner(),
        Err(e) => {
            warn!(
                pool, error = %e,
                "ListOpenAttempts failed; no cancel decisions this tick (fail-closed)"
            );
            return 0;
        }
    };
    let open_attempts = resp.attempts;
    let to_cancel: Vec<&Job> =
        select_closed_attempt_jobs(&active, &open_attempts, &resp.recently_closed);
    let mut cancelled = 0u32;
    for job in to_cancel {
        let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
        match delete_job_with_synthesized_report(
            jobs_api,
            ctx,
            job,
            job_name,
            &DeleteParams::foreground(),
            rio_proto::types::AttemptTerminalReason::Cancelled,
            &open_attempts,
        )
        .await
        {
            Ok(_) => {
                info!(
                    pool, job = %job_name,
                    "cancelled pull-mode Job whose attempt closed while the Job was still active"
                );
                cancelled += 1;
            }
            Err(e) if e.is_not_found() => {
                debug!(pool, job = %job_name, "cancel-arm Job already gone");
            }
            Err(e) => {
                warn!(
                    pool, job = %job_name, error = %e,
                    "failed to delete cancelled pull-mode Job; will retry next tick"
                );
            }
        }
    }
    cancelled
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
// r[impl ctrl.pool.spawn-once]
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
/// Returns the subset of `intents` whose Job was `Spawned`. `Failed`,
/// `NameCollision`, and `build_job`-Err entries are logged and OMITTED
/// so the caller does NOT ack them: acking a failed spawn arms the
/// scheduler's ICE-backoff timer for a Job that doesn't exist → no
/// heartbeat ever clears it → false ICE mark on the `(band, cap)`
/// cell. A 409 on the post-reap path is the terminating old-selector
/// Job — same shape as `Failed` (won't heartbeat for the new
/// selector). `skip_existing` hits are also omitted — the caller's
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
                // 409 ⇒ a Job by that name exists — but on the common
                // post-reap path it's the terminating old-selector
                // Job, which won't heartbeat. Don't ack; the rare
                // healthy-collision case is covered by next tick's
                // `pending_job_names` re-ack once the Job lists.
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
        // Match all three; all classify as EvictedDiskPressure for
        // the AD2c classification fill (the scheduler intake never
        // promotes a floor from controller reports — sla-sizing.typ
        // accepted residual; the floor's disk arm is parked).
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

/// `metav1.Time` → unix-epoch seconds (kube wraps `jiff::Timestamp`).
fn time_epoch_secs(t: &Time) -> f64 {
    t.0.as_second() as f64
}

/// Unix-epoch seconds `now()`, the ack-side endpoint of the OA1
/// interval-(i) sample arithmetic.
fn epoch_now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// OA1 interval-(i) endpoint A for the Pod path: the Pod's
/// terminal-condition timestamp. The first terminated containerStatus's
/// `finishedAt` (the OOMKilled shape); for evictions where kubelet sets
/// only pod-level status, the latest pod-condition `lastTransitionTime`.
/// `None` when neither is populated — the caller skips the sample
/// rather than guessing an endpoint.
pub(super) fn pod_terminal_epoch_secs(pod: &Pod) -> Option<f64> {
    let status = pod.status.as_ref()?;
    if let Some(t) = status
        .container_statuses
        .iter()
        .flatten()
        .find_map(|cs| cs.state.as_ref()?.terminated.as_ref()?.finished_at.as_ref())
    {
        return Some(time_epoch_secs(t));
    }
    status
        .conditions
        .iter()
        .flatten()
        .filter_map(|c| c.last_transition_time.as_ref())
        .map(time_epoch_secs)
        .reduce(f64::max)
}

/// OA1 interval-(i) endpoint A for the Job path: the
/// `Failed/DeadlineExceeded` condition's `lastTransitionTime`.
pub(super) fn job_deadline_exceeded_epoch_secs(job: &Job) -> Option<f64> {
    job.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == "Failed" && c.reason.as_deref() == Some("DeadlineExceeded"))?
        .last_transition_time
        .as_ref()
        .map(time_epoch_secs)
}

/// Prometheus `reason` label for the OA1 interval-(i) histogram. Same
/// strings the scheduler's floor path persists as `termination_reason`
/// — BY CONSTRUCTION since bug_255: the emitting arms route through
/// `rio_common::classify::attempt_terminal_reason_label` via the
/// unified wire vocabulary, so the planes cannot drift (the retired
/// hand-mirrored match emitted `disk_pressure` against the scheduler's
/// `evicted_disk_pressure`; equality joins returned nothing). Only the
/// reported classification-fill reasons (OomKilled /
/// EvictedDiskPressure) and DeadlineExceeded ever reach the metric
/// (the report path filters the rest before the RPC) — the filtered
/// arms keep the deliberate `other` collapse.
fn termination_reason_label(reason: TerminationReason) -> &'static str {
    match reason {
        TerminationReason::OomKilled
        | TerminationReason::EvictedDiskPressure
        | TerminationReason::DeadlineExceeded => {
            rio_common::classify::attempt_terminal_reason_label(
                unified_attempt_reason(reason).into(),
            )
        }
        TerminationReason::EvictedOther
        | TerminationReason::Completed
        | TerminationReason::Error
        | TerminationReason::Unknown => "other",
    }
}

/// Map the controller's k8s pod-terminal classification onto the
/// unified `AttemptTerminalReason` vocabulary (the C4/C5 unification:
/// one idempotent `ReportAttemptOutcome` carries what the retired
/// stream-era termination report used to). The mapping is 1:1.
fn unified_attempt_reason(reason: TerminationReason) -> rio_proto::types::AttemptTerminalReason {
    use rio_proto::types::AttemptTerminalReason as A;
    match reason {
        TerminationReason::Unknown => A::Unspecified,
        TerminationReason::OomKilled => A::OomKilled,
        TerminationReason::EvictedDiskPressure => A::EvictedDiskPressure,
        TerminationReason::EvictedOther => A::EvictedOther,
        TerminationReason::Completed => A::Completed,
        TerminationReason::Error => A::Error,
        TerminationReason::DeadlineExceeded => A::DeadlineExceeded,
    }
}

/// The intent id a pod-terminal `ReportAttemptOutcome` should carry:
/// the pod's `rio.build/intent-id` annotation (the attested intent
/// identity every attempt is keyed by). Empty when the annotation is
/// absent (recovery-path pods spawned outside the intent loop) — the
/// report then resolves by Job/pod name only.
pub(super) fn report_intent_id_for_pod(pod: &Pod) -> String {
    pod.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(super::jobs::INTENT_ID_ANNOTATION))
        .cloned()
        .unwrap_or_default()
}

/// The same population rule for the deadline-exceeded JOB report (the
/// intent annotation read from the Job's pod template).
pub(super) fn report_intent_id_for_job(job: &Job) -> String {
    job_intent_id(job).unwrap_or_default().to_owned()
}

/// How long a sampled Pod/Job name stays in `Ctx::terminal_report_sampled`
/// before pruning: 2× the Job TTL, by which point the object is no
/// longer listable so the entry can never suppress a legitimate new
/// sample.
const TERMINAL_REPORT_SAMPLED_TTL: Duration = Duration::from_secs(2 * JOB_TTL_SECS as u64);

/// OA1 interval-(i) sample gate: returns the latency sample (seconds,
/// clamped at zero against apiserver/controller clock skew) for `key`
/// exactly once per controller process; later calls for the same key
/// return `None`. See `Ctx::terminal_report_sampled` for why the
/// TTL-window re-reports must not be re-sampled.
pub(super) fn first_terminal_report_sample(
    seen: &mut HashMap<String, Instant>,
    key: &str,
    terminal_epoch_secs: f64,
    now_epoch_secs: f64,
) -> Option<f64> {
    if seen.contains_key(key) {
        return None;
    }
    seen.insert(key.to_owned(), Instant::now());
    Some((now_epoch_secs - terminal_epoch_secs).max(0.0))
}

/// Report each terminated Pod's k8s reason to the scheduler as the
/// kube-authoritative CLASSIFICATION FILL (AD2c): the intake's only
/// write is the reason-only second installment on an existing,
/// still-unfilled classification row — it never inserts a row, never
/// consumes budget, and never bumps a floor (floor promotion is
/// worker-reported-only; sla-sizing.typ accepted residual). Called
/// from the Job-mode reconcilers' tick after the spawn/reap steps.
///
/// Lists Pods (not Jobs — JobStatus doesn't carry per-container
/// termination reason) by `POOL_LABEL` selector. For each Pod with a
/// terminated container or Evicted status, calls the unified
/// `AdminService.ReportAttemptOutcome(job_name = pod name, reason)`
/// (the C4/C5 unification).
///
/// Idempotent: the fill targets an unfilled row exactly once, so
/// re-reporting the same Pod every ~10s tick during
/// `JOB_TTL_SECS=600` is a no-op past the first. `Unknown` (Pod still
/// running / status not populated) is skipped — next tick will see
/// the terminated state.
///
/// Best-effort: list error or RPC error is logged at debug/warn and
/// the reconcile continues. A missed report degrades to "this
/// attempt's classification stays establishment-sweep-classified" —
/// nothing depends on the fill for liveness. Never blocks the
/// spawn/reap loop.
pub(super) async fn report_terminated_pods(ctx: &Ctx, ns: &str, pool: &str) {
    // OA1 sample-gate hygiene: drop entries for objects that can no
    // longer be listed (past the TTL window) so the map stays bounded.
    ctx.terminal_report_sampled
        .lock()
        .retain(|_, inserted| inserted.elapsed() < TERMINAL_REPORT_SAMPLED_TTL);
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
    // merged_bug_135 sibling (the AD2c fill is exec-resolved-only on
    // the scheduler side): pin the pod-terminal classification to the
    // open attempt's exec_id when one exists, so the kube-authoritative
    // node attribution keeps flowing into the establishment charge.
    // One view read per tick, only when a reportable pod exists; on
    // error the reports go intent-keyed (the scheduler then skips the
    // node fill — conservative, never wrong-attempt).
    let open_attempts: Vec<rio_proto::types::OpenAttempt> = if list.items.iter().any(|pod| {
        matches!(
            pod_termination_reason(pod),
            TerminationReason::OomKilled | TerminationReason::EvictedDiskPressure
        )
    }) {
        match admin_call(
            admin
                .clone()
                .list_open_attempts(rio_proto::types::ListOpenAttemptsRequest {}),
        )
        .await
        {
            Ok(resp) => resp.into_inner().attempts,
            Err(e) => {
                debug!(pool, error = %e,
                       "report_terminated_pods: open-attempt view unavailable; \
                        reports go intent-keyed (node fill skipped scheduler-side)");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    for pod in &list.items {
        let reason = pod_termination_reason(pod);
        // Only the classification-fill reasons (OomKilled /
        // EvictedDiskPressure) go over the wire. `Completed`/
        // `Error`/`EvictedOther` would be sent every tick for every
        // TTL-window Job; the scheduler's fill-once intake no-ops
        // them anyway. `Error` IS observable for a deadline-
        // SIGKILL'd pod (restartPolicy:Never + backoffLimit:0 +
        // TGPS=7200 + job-tracking finalizer keep it listable for
        // the full grace window) — reporting it here would race the
        // same-tick `report_deadline_exceeded_jobs`, which owns the
        // DeadlineExceeded classification from the Job condition.
        // This filter eliminates the wasted RPCs and the race.
        if !matches!(
            reason,
            TerminationReason::OomKilled | TerminationReason::EvictedDiskPressure
        ) {
            continue;
        }
        let Some(name) = pod.metadata.name.as_deref() else {
            continue;
        };
        // C4/C5 unification: the classification rides the unified
        // idempotent ReportAttemptOutcome, keyed by the pod/Job name
        // plus the pod's intent annotation and kube-authoritative node
        // (the AD2c attribution).
        let intent_id = report_intent_id_for_pod(pod);
        let node_name = pod
            .spec
            .as_ref()
            .and_then(|s| s.node_name.clone())
            .unwrap_or_default();
        // Exec-pinned when the attempt is open (R4: only an
        // exec-resolved Build report may fill the node).
        let exec_id = open_attempts
            .iter()
            .find(|a| !intent_id.is_empty() && a.intent_id == intent_id)
            .map(|a| a.exec_id.clone())
            .unwrap_or_default();
        match admin_call(admin.report_attempt_outcome(
            rio_proto::types::ReportAttemptOutcomeRequest {
                resubmit_cycle: 0,
                intent_id,
                job_name: name.to_owned(),
                exec_id,
                reason: unified_attempt_reason(reason).into(),
                node_name,
            },
        ))
        .await
        {
            Ok(_) => {
                // OA1 interval (i): Pod terminal-condition timestamp →
                // report acked, sampled once per Pod (the same Pod is
                // re-reported every tick for the TTL window — see
                // `Ctx::terminal_report_sampled`).
                if let Some(terminal) = pod_terminal_epoch_secs(pod)
                    && let Some(latency) = first_terminal_report_sample(
                        &mut ctx.terminal_report_sampled.lock(),
                        name,
                        terminal,
                        epoch_now_secs(),
                    )
                {
                    metrics::histogram!(
                        "rio_controller_job_terminal_report_seconds",
                        "reason" => termination_reason_label(reason)
                    )
                    .record(latency);
                }
            }
            Err(e) => {
                warn!(
                    pool, executor_id = %name, ?reason, error = %e,
                    "ReportAttemptOutcome(pod-terminal) failed; skipping (best-effort)"
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
/// full grace window — but `Error` is not a classification-fill
/// reason, so [`report_terminated_pods`] skips it; this reads the Job
/// condition instead.
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
/// (`r[ctrl.terminated.deadline-exceeded+3]`). Defense-in-depth behind
/// the worker-side `BuildResultStatus::TimedOut` primary path.
///
/// Iterates the already-listed `jobs` (no extra apiserver call). For
/// each Job with a `Failed/DeadlineExceeded` condition, sends the
/// unified `ReportAttemptOutcome{job_name = JOB name, reason =
/// DeadlineExceeded}`. The Job controller deletes the Pod when the
/// deadline fires; the scheduler resolves the attempt by the Job-name
/// / intent identity carried on the request (the second-installment
/// fill).
///
/// Idempotent per the same dedup as [`report_terminated_pods`]. Best-
/// effort: RPC error logged, reconcile continues. `JOB_TTL_SECS=600`
/// keeps the Job observable for ~60 reconcile ticks.
// r[impl ctrl.terminated.deadline-exceeded+3]
pub(super) async fn report_deadline_exceeded_jobs(ctx: &Ctx, jobs: &[Job]) {
    let mut admin = ctx.admin.clone();
    for job in jobs {
        if !job_deadline_exceeded(job) {
            continue;
        }
        let Some(name) = job.metadata.name.as_deref() else {
            continue;
        };
        // C4/C5 unification: keyed by the JOB name (the pod is already
        // deleted by the Job controller when the deadline fires) plus
        // the intent annotation read from the Job's pod template
        // (`report_intent_id_for_job`).
        match admin_call(admin.report_attempt_outcome(
            rio_proto::types::ReportAttemptOutcomeRequest {
                resubmit_cycle: 0,
                intent_id: report_intent_id_for_job(job),
                job_name: name.to_owned(),
                exec_id: String::new(),
                reason: rio_proto::types::AttemptTerminalReason::DeadlineExceeded.into(),
                node_name: String::new(),
            },
        ))
        .await
        {
            Ok(_) => {
                // OA1 interval (i): the Job's Failed/DeadlineExceeded
                // condition transition → report acked, sampled once per
                // Job (see `Ctx::terminal_report_sampled`).
                if let Some(terminal) = job_deadline_exceeded_epoch_secs(job)
                    && let Some(latency) = first_terminal_report_sample(
                        &mut ctx.terminal_report_sampled.lock(),
                        name,
                        terminal,
                        epoch_now_secs(),
                    )
                {
                    metrics::histogram!(
                        "rio_controller_job_terminal_report_seconds",
                        "reason" => termination_reason_label(TerminationReason::DeadlineExceeded)
                    )
                    .record(latency);
                }
            }
            Err(e) => {
                warn!(
                    executor_id = %name, error = %e,
                    "ReportAttemptOutcome(DeadlineExceeded) failed; skipping (best-effort)"
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

    /// The C4/C5 wire-vocabulary bridge is total and 1:1 — every k8s
    /// pod-terminal classification has exactly one unified spelling, so
    /// nothing the legacy RPC could express is lost behind the
    /// re-point.
    // r[verify ctrl.terminated.deadline-exceeded+3]
    #[test]
    fn unified_attempt_reason_is_total() {
        use rio_proto::types::AttemptTerminalReason as A;
        for (legacy, unified) in [
            (TerminationReason::Unknown, A::Unspecified),
            (TerminationReason::OomKilled, A::OomKilled),
            (
                TerminationReason::EvictedDiskPressure,
                A::EvictedDiskPressure,
            ),
            (TerminationReason::EvictedOther, A::EvictedOther),
            (TerminationReason::Completed, A::Completed),
            (TerminationReason::Error, A::Error),
            (TerminationReason::DeadlineExceeded, A::DeadlineExceeded),
        ] {
            assert_eq!(unified_attempt_reason(legacy), unified);
        }
    }

    /// OA1 interval-(i) instrument helpers: endpoint-A extraction from
    /// the k8s objects and the once-per-object sample gate that keeps
    /// the TTL-window re-reports from skewing
    /// `rio_controller_job_terminal_report_seconds` (the histogram
    /// itself is described in lib.rs and covered by
    /// tests/metrics_registered.rs).
    #[test]
    fn terminal_report_sample_helpers() {
        use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodCondition, PodStatus,
        };
        use k8s_openapi::jiff::{SignedDuration, Timestamp};

        // OOMKilled shape: terminated containerStatus carries finishedAt.
        let finished = Timestamp::now() - SignedDuration::from_secs(42);
        let oom = Pod {
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            reason: Some("OOMKilled".into()),
                            finished_at: Some(Time(finished)),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = pod_terminal_epoch_secs(&oom).expect("finishedAt present");
        assert!((got - finished.as_second() as f64).abs() < 1.0);

        // Eviction shape: pod-level status only — fall back to the
        // latest condition lastTransitionTime.
        let cond_t = Timestamp::now() - SignedDuration::from_secs(10);
        let evicted = Pod {
            status: Some(PodStatus {
                reason: Some("Evicted".into()),
                conditions: Some(vec![PodCondition {
                    type_: "Ready".into(),
                    status: "False".into(),
                    last_transition_time: Some(Time(cond_t)),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(pod_terminal_epoch_secs(&evicted).is_some());

        // No status → no endpoint A → no sample (caller skips).
        assert!(pod_terminal_epoch_secs(&Pod::default()).is_none());

        // Job path: the Failed/DeadlineExceeded condition's
        // lastTransitionTime; any other condition → None.
        let deadline_t = Timestamp::now() - SignedDuration::from_secs(30);
        let deadline_job = Job {
            status: Some(JobStatus {
                conditions: Some(vec![JobCondition {
                    type_: "Failed".into(),
                    reason: Some("DeadlineExceeded".into()),
                    status: "True".into(),
                    last_transition_time: Some(Time(deadline_t)),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(job_deadline_exceeded_epoch_secs(&deadline_job).is_some());
        assert!(job_deadline_exceeded_epoch_secs(&Job::default()).is_none());

        // The sample gate: first ack for a key yields the latency,
        // re-reports of the same key do not re-sample, a different key
        // does, and clock skew clamps at zero (never a negative sample).
        let mut seen = HashMap::new();
        assert_eq!(
            first_terminal_report_sample(&mut seen, "pod-a", 100.0, 130.0),
            Some(30.0)
        );
        assert_eq!(
            first_terminal_report_sample(&mut seen, "pod-a", 100.0, 140.0),
            None,
            "TTL-window re-report must not re-sample"
        );
        assert_eq!(
            first_terminal_report_sample(&mut seen, "pod-b", 200.0, 190.0),
            Some(0.0),
            "clock skew clamps at zero"
        );

        // Label values match the scheduler-side termination_reason
        // strings for the reasons that reach the wire.
        assert_eq!(
            termination_reason_label(TerminationReason::OomKilled),
            "oom_killed"
        );
        assert_eq!(
            termination_reason_label(TerminationReason::EvictedDiskPressure),
            "evicted_disk_pressure"
        );
        assert_eq!(
            termination_reason_label(TerminationReason::DeadlineExceeded),
            "deadline_exceeded"
        );
    }

    /// bug_255 parity: the controller-side OA1 `reason` label and the
    /// scheduler's persisted `termination_reason` label must be EQUAL
    /// for every reason both planes emit (the documented "series line
    /// up" contract — equality joins between the planes depend on it).
    #[test]
    fn termination_reason_label_parity_with_scheduler() {
        use rio_common::classify::{AttemptTerminalKind, attempt_terminal_reason_label};
        for (reason, kind) in [
            (TerminationReason::OomKilled, AttemptTerminalKind::OomKilled),
            (
                TerminationReason::EvictedDiskPressure,
                AttemptTerminalKind::EvictedDiskPressure,
            ),
            (
                TerminationReason::DeadlineExceeded,
                AttemptTerminalKind::DeadlineExceeded,
            ),
        ] {
            assert_eq!(
                termination_reason_label(reason),
                attempt_terminal_reason_label(kind),
                "the planes' series must line up for {reason:?}"
            );
        }
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
    // r[verify ctrl.terminated.deadline-exceeded+3]
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

    // r[verify ctrl.ephemeral.reap-excess-pending+3]
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
        let none = HashSet::new();
        let excess = select_excess_pending(&jobs, &none, 1, NO_GRACE);
        let names: Vec<_> = excess.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["med-old", "med-mid"],
            "deletes 2 oldest, keeps 1 newest"
        );
        // queued >= pending → nothing to reap.
        assert!(select_excess_pending(&jobs, &none, 3, NO_GRACE).is_empty());
        assert!(select_excess_pending(&jobs, &none, 5, NO_GRACE).is_empty());
        // queued=0 → reap all pending.
        assert_eq!(select_excess_pending(&jobs, &none, 0, NO_GRACE).len(), 3);
    }

    // r[verify ctrl.ephemeral.reap-excess-pending+3]
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
        let excess = select_excess_pending(&jobs, &HashSet::new(), 0, NO_GRACE);
        let names: Vec<_> = excess.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["pend"],
            "queued=0: only the Pending Job is reaped; Running/Completed untouched"
        );
    }

    // r[verify ctrl.ephemeral.reap-excess-pending+3]
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
        let none = HashSet::new();
        let excess = select_excess_pending(&jobs, &none, 0, REAP_PENDING_GRACE);
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
            select_excess_pending(&[no_ts], &none, 0, REAP_PENDING_GRACE).is_empty(),
            "no creation_timestamp → not reapable (conservative)"
        );
    }

    // r[verify ctrl.ephemeral.reap-excess-pending+3]
    /// bug_015: `reap_stale_for_intents` foreground-deletes orphan D
    /// (younger), but the unfiltered snapshot still counts it as
    /// Pending → `pending=2 > queued=1` → oldest-first deletes WANTED
    /// Job A (mid-Karpenter-provisioning). With `reaped={D}`, D is
    /// filtered → `pending=1 ≤ 1` → empty result.
    #[test]
    fn select_excess_pending_skips_reaped() {
        let jobs = vec![
            job_with("rio-builder-p-a", Some(0), Some(0), 30),
            job_with("rio-builder-p-d", Some(0), Some(0), 20),
        ];
        let reaped: HashSet<String> = ["rio-builder-p-d".into()].into();
        assert!(
            select_excess_pending(&jobs, &reaped, 1, NO_GRACE).is_empty(),
            "reaped D filtered → pending=1 ≤ queued=1 → no excess"
        );
        // Without the filter (pre-fix behavior): D counts, A is oldest
        // → A would be returned. Prove the filter is load-bearing.
        let names: Vec<_> = select_excess_pending(&jobs, &HashSet::new(), 1, NO_GRACE)
            .iter()
            .map(|j| j.name_any())
            .collect();
        assert_eq!(names, vec!["rio-builder-p-a"]);
    }

    // r[verify ctrl.ephemeral.reap-orphan-running+5]
    /// Same-tick consistency for the orphan-running selector: a Job
    /// already foreground-deleted by `reap_stale_for_intents` is not
    /// re-selected (would re-delete + double-count the metric).
    #[test]
    fn select_orphan_running_skips_reaped() {
        let jobs = vec![job_with("rio-builder-p-stuck", Some(1), Some(0), 600)];
        let reaped: HashSet<String> = ["rio-builder-p-stuck".into()].into();
        assert!(
            select_orphan_running(&jobs, &reaped, &[], ORPHAN_REAP_GRACE).is_empty(),
            "reaped this tick → not re-selected"
        );
        assert_eq!(
            select_orphan_running(&jobs, &HashSet::new(), &[], ORPHAN_REAP_GRACE).len(),
            1,
            "control: not in `reaped` → selected"
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
        // ListOpenAttempts + double-counts the orphan-reaped metric every tick.
        let mut terminating = job_with("a", Some(1), Some(0), 0);
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ));
        assert!(!is_running_job(&terminating));
    }

    // r[verify ctrl.ephemeral.reap-orphan-running+5]
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
            // metric and re-fires ListOpenAttempts every tick).
            {
                let mut j = job_with("rio-builder-x86-term01", Some(1), Some(0), 600);
                j.metadata.deletion_timestamp =
                    Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                        k8s_openapi::jiff::Timestamp::now(),
                    ));
                j
            },
        ];
        let none = HashSet::new();
        let orphans = select_orphan_running(&jobs, &none, &[], ORPHAN_REAP_GRACE);
        let names: Vec<_> = orphans.iter().map(|j| j.name_any()).collect();
        assert_eq!(names, vec!["rio-builder-x86-stuck1"]);

        // None creation_timestamp → not-old-enough (conservative).
        let mut no_ts = job_with("rio-builder-x86-nots01", Some(1), Some(0), 600);
        no_ts.metadata.creation_timestamp = None;
        assert!(
            select_orphan_running(&[no_ts], &none, &[], ORPHAN_REAP_GRACE).is_empty(),
            "no creation_timestamp → not orphan-reapable (conservative)"
        );
    }

    /// `job_with` plus the `rio.build/intent-id` pod-template
    /// annotation `build_job` stamps — what a real spawned Job carries.
    fn job_with_intent(
        name: &str,
        intent_id: &str,
        ready: Option<i32>,
        succeeded: Option<i32>,
        age_s: i64,
    ) -> Job {
        let mut j = job_with(name, ready, succeeded, age_s);
        j.spec = Some(JobSpec {
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    annotations: Some(
                        [(
                            super::super::jobs::INTENT_ID_ANNOTATION.to_string(),
                            intent_id.to_string(),
                        )]
                        .into(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        });
        j
    }

    /// Minimal `OpenAttempt` as `ListOpenAttempts` returns it for a
    /// pull-mode attempt: `executor_id` is the HMAC-attested intent id
    /// (NOT a pod name) and `intent_id` carries the same value.
    fn open_attempt(intent_id: &str) -> rio_proto::types::OpenAttempt {
        rio_proto::types::OpenAttempt {
            intent_id: intent_id.into(),
            executor_id: intent_id.into(),
            exec_id: "0198c0de-0000-7000-8000-000000000001".into(),
            ..Default::default()
        }
    }

    // r[verify ctrl.job.busy-from-open-attempts+2]
    // r[verify ctrl.ephemeral.reap-orphan-running+5]
    /// The open-attempt busy view: a Running Job past the grace backed
    /// by an open pull-mode attempt is NOT selected; the same Job with
    /// no open attempt still IS selected (the I-165 stuck-pod reap
    /// preserved); a ghost Job (no attempt at all) is reaped.
    #[test]
    fn select_orphan_running_open_pull_attempt_is_busy() {
        let pull_intent = "drvhash-pull-0001";
        let jobs = vec![
            // Backed by an open pull-mode attempt → busy → NOT selected.
            job_with_intent("rio-builder-x86-pull01", pull_intent, Some(1), Some(0), 600),
            // Ghost: no open attempt → reaped.
            job_with("rio-builder-x86-ghost1", Some(1), Some(0), 600),
        ];
        let attempts = vec![open_attempt(pull_intent)];

        let orphans = select_orphan_running(&jobs, &HashSet::new(), &attempts, ORPHAN_REAP_GRACE);
        let names: Vec<_> = orphans.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["rio-builder-x86-ghost1"],
            "a Running Job past the grace backed by an open pull-mode \
             attempt must NOT be selected; the ghost still is"
        );

        // (b) The same Job with NO open attempt: the open-attempt arm
        // must not weaken the I-165 reap — it IS selected again.
        let orphans = select_orphan_running(&jobs, &HashSet::new(), &[], ORPHAN_REAP_GRACE);
        let names: Vec<_> = orphans.iter().map(|j| j.name_any()).collect();
        assert_eq!(
            names,
            vec!["rio-builder-x86-pull01", "rio-builder-x86-ghost1"],
            "with no open attempt the pull-mode Job is reapable again \
             (I-165 preserved)"
        );
    }

    // r[verify ctrl.job.busy-from-open-attempts+2]
    /// Match key for the open-attempt cover: the intent-id annotation
    /// only (attempt identity is the single correlation since the
    /// stream-era executor-id prefix fallback was dropped); a missing
    /// or non-matching annotation → not covered.
    #[test]
    fn covered_by_open_pull_attempt_match_keys() {
        // Annotation match (the executor_id is the intent, not a pod name).
        let j = job_with_intent("rio-builder-x86-pull01", "drv-abc", Some(1), Some(0), 600);
        assert!(covered_by_open_pull_attempt(&j, &[open_attempt("drv-abc")]));
        assert!(!covered_by_open_pull_attempt(
            &j,
            &[open_attempt("drv-zzz")]
        ));
        assert!(!covered_by_open_pull_attempt(&j, &[]));

        // A pod-named executor_id with no matching intent annotation
        // does NOT cover: the stream-era prefix fallback is gone, so an
        // un-annotated Job is never covered by name similarity alone.
        let unannotated = job_with("rio-builder-x86-abc", Some(1), Some(0), 600);
        let pod_named = rio_proto::types::OpenAttempt {
            executor_id: "rio-builder-x86-abc-qwert".into(),
            ..Default::default()
        };
        assert!(
            !covered_by_open_pull_attempt(&unannotated, &[pod_named]),
            "no intent annotation → not covered (prefix fallback removed)"
        );

        // A Job with no name and no annotation is never covered.
        let unnamed = Job::default();
        assert!(!covered_by_open_pull_attempt(
            &unnamed,
            &[open_attempt("x")]
        ));
    }

    /// `RIO_ORPHAN_REAP_GRACE_OVERRIDE_SECS` (VM-fixture-only): honored
    /// when set and parsable, [`ORPHAN_REAP_GRACE`] (300 s) otherwise.
    #[test]
    fn orphan_reap_grace_override_env() {
        rio_test_support::Jail::expect_with(|jail| {
            // Default: no env → the production constant.
            assert_eq!(orphan_reap_grace(), ORPHAN_REAP_GRACE);
            assert_eq!(ORPHAN_REAP_GRACE, Duration::from_secs(300));
            // Override honored.
            jail.set_env("RIO_ORPHAN_REAP_GRACE_OVERRIDE_SECS", "7");
            assert_eq!(orphan_reap_grace(), Duration::from_secs(7));
            // Unparsable → fall back to the constant.
            jail.set_env("RIO_ORPHAN_REAP_GRACE_OVERRIDE_SECS", "not-a-number");
            assert_eq!(orphan_reap_grace(), ORPHAN_REAP_GRACE);
            Ok(())
        });
    }

    /// bug_113 red: a Job created AFTER its intent's cancelled close
    /// (cancel + fast re-submit respawns the deterministic name) must
    /// be structurally unselectable — today the selection has no
    /// generation conjunct and tears down the fresh Job.
    #[test]
    fn cancel_never_selects_a_job_created_after_the_close() {
        // The close happened 60s ago; the respawned Job is 5s old.
        let fresh = job_with_intent("respawn", "drv-x", Some(1), None, 5);
        let close = rio_proto::types::ClosedAttempt {
            intent_id: "drv-x".into(),
            exec_id: "0198c0de-0000-7000-8000-00000000cafe".into(),
            cause: rio_proto::types::CloseCause::Cancelled as i32,
            closed_age_secs: 60,
        };
        let active = vec![&fresh];
        let selected = select_closed_attempt_jobs(&active, &[], &[close]);
        assert!(
            selected.is_empty(),
            "a Job younger than its close must never be cancel-selected: {:?}",
            selected.iter().map(|j| j.name_any()).collect::<Vec<_>>()
        );
    }

    // r[verify ctrl.job.cancel-close-cause+2]
    /// The four-conjunct bind battery: selected iff cause=Cancelled ∧
    /// intent match ∧ job-predates-close ∧ ¬covered.
    #[test]
    fn cancel_bind_requires_all_four_conjuncts() {
        let old_job = job_with_intent("old", "drv-x", Some(1), None, 600);
        let close = |cause: rio_proto::types::CloseCause| rio_proto::types::ClosedAttempt {
            intent_id: "drv-x".into(),
            exec_id: "0198c0de-0000-7000-8000-00000000beef".into(),
            cause: cause as i32,
            closed_age_secs: 60,
        };
        let cancelled = close(rio_proto::types::CloseCause::Cancelled);
        // All four hold → selected.
        let active = vec![&old_job];
        assert_eq!(
            select_closed_attempt_jobs(&active, &[], std::slice::from_ref(&cancelled)).len(),
            1
        );
        // Cause: a COMPLETED close is untouchable by type.
        let completed = close(rio_proto::types::CloseCause::Completed);
        assert!(select_closed_attempt_jobs(&active, &[], &[completed]).is_empty());
        // Intent: no match, no bind.
        let other = job_with_intent("other", "drv-y", Some(1), None, 600);
        let active_other = vec![&other];
        assert!(
            select_closed_attempt_jobs(&active_other, &[], std::slice::from_ref(&cancelled))
                .is_empty()
        );
        // Liveness: a covering open attempt blocks the bind.
        let covering = open_attempt("drv-x");
        assert!(
            select_closed_attempt_jobs(&active, &[covering], std::slice::from_ref(&cancelled))
                .is_empty()
        );
        // Generation borderline: a Job exactly inside the skew slack
        // (age between closed_age and closed_age+slack) is NOT selected
        // — misses fall to orphan-reap / activeDeadlineSeconds (Q19).
        let borderline = job_with_intent(
            "borderline",
            "drv-x",
            Some(1),
            None,
            60 + (CANCEL_CLOSE_SKEW_SLACK_SECS as i64) - 2,
        );
        let active_b = vec![&borderline];
        assert!(
            select_closed_attempt_jobs(&active_b, &[], std::slice::from_ref(&cancelled)).is_empty()
        );
    }
}
