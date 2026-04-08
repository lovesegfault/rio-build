//! Shared plumbing for the two Job-mode reconcilers (`manifest.rs`,
//! `ephemeral.rs`). Both follow the same skeleton: list Jobs by label,
//! filter active, diff against demand, spawn deficit, patch status.
//! The diff is different (per-bucket vs flat-ceiling); the plumbing
//! around it is the same.
//!
//! Extracted (P0513) after the mc=60 consolidator pass found 4
//! byte-identical-or-near segments (~50L) plus two `super::ephemeral::`
//! cross-refs from `manifest.rs` reaching into a sibling for helpers
//! that belong in neither.

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, DeleteParams, PostParams};
use kube::{CustomResourceExt, Resource, ResourceExt};
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::reconcilers::Ctx;
use rio_crds::builderpool::BuilderPool;

use super::MANAGER;
use super::builders::{SchedulerAddrs, StoreAddrs};

/// `(est_memory_bytes, est_cpu_millicores)` — manifest-mode bucket key.
/// Shared between the manifest reconciler's internal maps and
/// `Ctx::manifest_idle`'s per-pool idle state. `pub(crate)` so the
/// parent `reconcilers::mod` sees the same type alias instead of
/// re-inlining the tuple literally (no compiler check links two `(u64,
/// u32)` literals; the alias makes drift a type error).
///
/// Scheduler-side bucketing (admin_types.proto:234) rounds memory to
/// nearest 4GiB, cpu to nearest 2000m, so the keyspace is coarse by
/// design. A BTreeMap over this is small (typical queue has <10
/// distinct buckets).
pub(crate) type Bucket = (u64, u32);

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
/// counting a Complete Job is moot — TTL reaps it in ephemeral mode,
/// scale-down deletes it in manifest mode).
pub(crate) fn is_active_job(j: &Job) -> bool {
    let s = j.status.as_ref();
    s.and_then(|st| st.succeeded).unwrap_or(0) == 0 && s.and_then(|st| st.failed).unwrap_or(0) == 0
}

/// `status.failed > 0` — terminal under `backoff_limit=0` (one pod
/// crash → Job Failed permanently, no retry).
///
/// NOT the exact inverse of [`is_active_job`]: a Succeeded Job is
/// neither active nor failed. The manifest sweep cares only about
/// the Failed dimension — Succeeded Jobs are reaped by
/// `ttlSecondsAfterFinished`.
pub(super) fn is_failed_job(j: &Job) -> bool {
    j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0
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
/// boundary for `r[ctrl.ephemeral.reap-excess-pending]`.
///
/// `None` status (Job controller hasn't reconciled yet → pod not
/// created) is treated as Pending. That's the safe direction: a Job
/// with no pod is trivially reapable.
///
/// A Job with `deletionTimestamp` set is NOT pending — it's already
/// terminating (foreground-delete in flight). Re-selecting it would
/// be a no-op apiserver round-trip + log spam every tick until the
/// pod's `job-tracking` finalizer clears.
pub(crate) fn is_pending_job(j: &Job) -> bool {
    j.metadata.deletion_timestamp.is_none()
        && is_active_job(j)
        && j.status.as_ref().and_then(|s| s.ready).unwrap_or(0) == 0
}

/// Active AND `status.ready > 0` — the Job's pod container has
/// started. Complement of [`is_pending_job`] within the active set.
/// The orphan-reap boundary for `r[ctrl.ephemeral.reap-orphan-
/// running]`: only Running Jobs are candidates (Pending is handled by
/// `reap_excess_pending`; Complete/Failed by TTL).
pub(crate) fn is_running_job(j: &Job) -> bool {
    is_active_job(j) && j.status.as_ref().and_then(|s| s.ready).unwrap_or(0) > 0
}

/// Minimum age before a Running Job is orphan-reapable. 5min: MUST
/// exceed the builder's `RIO_IDLE_SECS` (default 120s) so
/// the process-level idle-exit gets first chance — a healthy idle
/// pod self-terminates at 120s and the Job goes Complete
/// well before this fires. The reap targets pods that CANNOT
/// self-exit (I-165: D-state FUSE wait, OOM-loop) and would otherwise
/// burn `activeDeadlineSeconds` (default 1h) holding a node.
pub(crate) const ORPHAN_REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(300);

/// Minimum age before a Pending Job is reapable. `JobStatus.ready` is
/// set by the K8s Job controller AFTER it observes pod readiness — a
/// container that just started may have already heartbeated and
/// received an assignment while `ready` is still 0 (Job-controller
/// sync lag, typically <1s but unbounded under apiserver load). One
/// requeue tick of grace makes the false-positive window negligible
/// without materially delaying the I-183 reap (the bug is Jobs sitting
/// for an HOUR; 10s grace is noise).
pub(crate) const REAP_PENDING_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Pending Jobs in excess of `queued`, oldest-first — the reap set
/// for `r[ctrl.ephemeral.reap-excess-pending]`.
///
/// `pending.len() <= queued` → empty (every Pending Job is plausibly
/// claimed by a queued derivation). `pending.len() > queued` → the
/// `pending - queued` oldest are surplus: even if every queued
/// derivation lands on a Pending pod, these will never get one. The
/// spawn formula's active-subtraction means we never spawn more than
/// `queued`, so this only fires when `queued` DROPS after spawn (user
/// cancel, build completes elsewhere, gateway disconnect).
///
/// `min_age`: Jobs younger than this are excluded — see
/// [`REAP_PENDING_GRACE`]. Passing `Duration::ZERO` disables the
/// grace (tests).
///
/// Oldest-first: same ordering as [`select_failed_jobs`]. The oldest
/// Pending Job has waited longest for a node; if Karpenter hasn't
/// provisioned one by now it's likely the most stuck. Newest-first
/// would reap the Job that's closest to scheduling.
///
/// Running Jobs are NOT in the result — [`is_pending_job`] excludes
/// them. A Running pod may already hold an assignment; the scheduler's
/// cancel-on-disconnect handles those when the gateway session that
/// queued the work closes.
pub(crate) fn select_excess_pending(
    jobs: &[Job],
    queued: u32,
    min_age: std::time::Duration,
) -> Vec<&Job> {
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

// r[impl ctrl.ephemeral.reap-excess-pending]
/// Delete Pending Jobs in excess of `queued`. Shared by the
/// builderpool and fetcherpool ephemeral reconcilers (both had the
/// spawn-only pattern before I-183; both now reap).
///
/// `pool`/`class` feed the metric labels and log fields. For
/// BuilderPool, `class` is `spec.size_class` (may be empty for
/// standalone pools). For FetcherPool, `class` is the per-class
/// `pool_name` from the class iteration.
///
/// warn+continue on delete failure — same posture as the spawn loop
/// (P0516) and `manifest.rs`'s sweep: one apiserver blip shouldn't
/// skip the status patch. Next tick re-lists and retries (the Job is
/// still Pending, still excess).
///
/// `queued = None` → scheduler unreachable; caller treated the poll
/// error as `queued=0` for spawn (fail-open: don't spawn). Reap MUST
/// NOT treat that as 0 (fail-closed: don't delete) — a scheduler
/// restart would otherwise nuke every Pending Job. Returns 0
/// immediately.
///
/// Returns the count actually deleted (for the reconcile summary log).
pub(crate) async fn reap_excess_pending(
    jobs_api: &Api<Job>,
    jobs: &[Job],
    queued: Option<u32>,
    pool: &str,
    class: &str,
) -> u32 {
    let Some(queued) = queued else {
        debug!(
            pool,
            class, "skipping Pending-reap: queued unknown (scheduler unreachable)"
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
                    pool, class, job = %job_name, queued,
                    "reaped excess Pending ephemeral Job (queued dropped below pending)"
                );
                reaped += 1;
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!(pool, class, job = %job_name, "Pending Job already gone");
            }
            Err(e) => {
                warn!(
                    pool, class, job = %job_name, error = %e,
                    "failed to reap excess Pending Job; will retry next tick"
                );
            }
        }
    }
    if reaped > 0 {
        metrics::counter!(
            "rio_controller_ephemeral_jobs_reaped_total",
            "pool" => pool.to_owned(),
            "class" => class.to_owned(),
        )
        .increment(reaped.into());
    }
    reaped
}

/// `creation_timestamp` strictly before `now - min_age`. `None` →
/// not-old-enough (conservative; same posture as
/// [`select_excess_pending`]).
fn job_older_than(j: &Job, min_age: std::time::Duration) -> bool {
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
/// `r[ctrl.ephemeral.reap-orphan-running]`.
///
/// "Not busy" = no `ExecutorInfo` whose `executor_id` starts with
/// `{job_name}-` (pod never registered / already disconnected), OR
/// such an executor exists with `running_builds == 0` (registered but
/// idle past the builder's own 120s idle-exit — process can't act,
/// I-165 D-state). The `{job_name}-` prefix match is the same
/// pod-name convention as `manifest.rs::select_deletable_jobs`
/// (`RIO_WORKER_ID=$(POD_NAME)` via downward API; Job pod is
/// `{job_name}-{5char}`).
///
/// A Job whose executor reports `running_builds > 0` is excluded —
/// the scheduler believes a build is in progress; deleting it would
/// orphan the build mid-flight. `activeDeadlineSeconds` is the
/// backstop for stuck-mid-build.
///
/// Pending Jobs are excluded ([`is_running_job`] requires `ready >
/// 0`) — those are [`reap_excess_pending`]'s territory.
pub(crate) fn select_orphan_running<'a>(
    jobs: &'a [Job],
    executors: &[rio_proto::types::ExecutorInfo],
    min_age: std::time::Duration,
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
                Some(e) if e.running_builds > 0 => false,
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

// r[impl ctrl.ephemeral.reap-orphan-running]
/// Delete Running ephemeral Jobs the scheduler doesn't consider busy
/// after [`ORPHAN_REAP_GRACE`]. Shared by the builderpool and
/// fetcherpool ephemeral reconcilers — same I-165 stuck-process
/// failure mode applies to both.
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
pub(crate) async fn reap_orphan_running(
    jobs_api: &Api<Job>,
    jobs: &[Job],
    ctx: &Ctx,
    pool: &str,
    class: &str,
) -> u32 {
    // Cheap pre-filter: any candidates at all? Avoids the RPC on the
    // hot path (every 10s tick × every pool).
    if !jobs
        .iter()
        .any(|j| is_running_job(j) && job_older_than(j, ORPHAN_REAP_GRACE))
    {
        return 0;
    }
    let executors = match ctx
        .admin
        .clone()
        .list_executors(rio_proto::types::ListExecutorsRequest {
            status_filter: String::new(),
        })
        .await
    {
        Ok(resp) => resp.into_inner().executors,
        Err(e) => {
            warn!(
                pool, class, error = %e,
                "ListExecutors failed; skipping orphan-reap this tick (fail-closed)"
            );
            return 0;
        }
    };
    let orphans = select_orphan_running(jobs, &executors, ORPHAN_REAP_GRACE);
    if orphans.is_empty() {
        return 0;
    }
    let mut reaped = 0u32;
    for job in orphans {
        let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
        match jobs_api.delete(job_name, &DeleteParams::background()).await {
            Ok(_) => {
                info!(
                    pool, class, job = %job_name,
                    grace_secs = ORPHAN_REAP_GRACE.as_secs(),
                    "reaped orphan Running ephemeral Job (no scheduler assignment past grace)"
                );
                reaped += 1;
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!(pool, class, job = %job_name, "orphan Job already gone");
            }
            Err(e) => {
                warn!(
                    pool, class, job = %job_name, error = %e,
                    "failed to reap orphan Running Job; will retry next tick"
                );
            }
        }
    }
    if reaped > 0 {
        metrics::counter!(
            "rio_controller_orphan_jobs_reaped_total",
            "pool" => pool.to_owned(),
            "class" => class.to_owned(),
        )
        .increment(reaped.into());
    }
    reaped
}

/// ns/name/jobs_api prelude. Both Job-mode reconcilers start here:
/// extract the namespaced identity and build the Job API handle. The
/// namespace-missing case is `InvalidSpec` not `NotFound` — a
/// BuilderPool without `.metadata.namespace` is a cluster-scoped
/// apply error (the CRD is `Namespaced`), not a transient condition.
pub(super) fn job_reconcile_prologue(
    wp: &BuilderPool,
    ctx: &Ctx,
) -> Result<(String, String, Api<Job>)> {
    let ns = wp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("BuilderPool has no namespace".into()))?;
    let name = wp.name_any();
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
    Ok((ns, name, jobs_api))
}

/// OwnerReference + SchedulerAddrs — both spawn paths need both
/// before entering their per-Job create loop. The `controller_
/// owner_ref` error (no `.metadata.uid`) only happens on a
/// BuilderPool not read from the apiserver — tests that construct
/// one in memory forget this; production reconcile always has it.
pub(super) fn spawn_prerequisites(
    wp: &BuilderPool,
    ctx: &Ctx,
) -> Result<(OwnerReference, SchedulerAddrs, StoreAddrs)> {
    let oref = wp.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("BuilderPool has no metadata.uid (not from apiserver?)".into())
    })?;
    Ok((oref, ctx.scheduler_addrs(), ctx.store_addrs()))
}

/// Outcome of a single `jobs_api.create` attempt. Caller decides
/// what to do on `Failed` — manifest counts toward a consecutive-fail
/// threshold (P0522); ephemeral just loops.
///
/// NOT a `Result`: `Failed` is not an error the caller propagates —
/// it's a classified non-success the caller handles inline. A
/// `Result<_, kube::Error>` with `?` at the call site would
/// re-introduce the bail this was extracted to eliminate. The enum
/// forces exhaustive handling.
pub(crate) enum SpawnOutcome {
    Spawned,
    /// 409 AlreadyExists — name collision. Next tick picks a fresh
    /// name. Not worth propagating — would trigger error_policy
    /// backoff for what is expected-noise.
    ///
    /// Two sources: random-suffix collision (36^6 ≈ 2 billion, TTL
    /// 60s, birthday-negligible but K8s handles it) or a concurrent
    /// reconcile racing the same pool.
    NameCollision,
    /// Spawn failed (quota blip, admission webhook, apiserver flap).
    /// NOT a bail — P0516 (`33424b8a`): one spawn error shouldn't
    /// skip the rest of the tick. Subsequent spawns may succeed;
    /// the status patch below is independent. Caller logs `warn!`
    /// with its own context (bucket, queued, ceiling, etc).
    Failed(kube::Error),
}

/// Create a Job, classifying the outcome. Shared by manifest +
/// ephemeral spawn loops — both had the same match arm at `ea64f7f2`;
/// manifest got warn+continue at P0516 (`33424b8a`), ephemeral
/// didn't. Extracting here means both get it, and P0522's threshold
/// lives in one place.
///
/// `PostParams::default` (not SSA): Jobs are create-once. SSA's
/// patch-merge semantics don't fit — there's no "update existing Job
/// to match spec," the Job is immutable after create (K8s rejects
/// most spec edits). A 409 is retried next tick with a fresh random
/// name.
pub(crate) async fn try_spawn_job(jobs_api: &Api<Job>, job: &Job) -> SpawnOutcome {
    match jobs_api.create(&PostParams::default(), job).await {
        Ok(_) => SpawnOutcome::Spawned,
        Err(kube::Error::Api(ae)) if ae.code == 409 => SpawnOutcome::NameCollision,
        Err(e) => SpawnOutcome::Failed(e),
    }
}

/// SSA-patch `.status.{replicas,readyReplicas,desiredReplicas,
/// conditions}` for a Job-mode BuilderPool.
///
/// "Replicas" means "active Jobs" — `kubectl get wp` columns are
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn patch_job_pool_status(
    ctx: &Ctx,
    wp: &BuilderPool,
    ns: &str,
    name: &str,
    active: i32,
    ready: i32,
    desired: i32,
    scheduler_err: Option<&str>,
) -> Result<()> {
    let wp_api: Api<BuilderPool> = Api::namespaced(ctx.client.clone(), ns);
    let ar = BuilderPool::api_resource();
    let prev = crate::scaling::find_condition(wp, "SchedulerUnreachable");
    let cond = scheduler_unreachable_condition(scheduler_err, prev.as_ref());
    let status_patch = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "replicas": active,
            "readyReplicas": ready,
            "desiredReplicas": desired,
            "conditions": [cond],
        },
    });
    wp_api
        .patch_status(
            name,
            &kube::api::PatchParams::apply(MANAGER).force(),
            &kube::api::Patch::Apply(&status_patch),
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
/// Same json!-not-struct pattern as `scaling::scaling_condition`:
/// k8s_openapi's Condition struct requires observedGeneration which
/// we don't track.
///
/// `prev`: existing SchedulerUnreachable condition (if any). Its
/// `lastTransitionTime` is preserved when `status` hasn't changed —
/// this reconciler writes every 10s tick; without preservation the
/// timestamp always reads "~10s ago" regardless of when the
/// scheduler actually went down/recovered.
// TODO(P0304): T505 adds an `rpc_name` param so the message names
// which RPC failed (ClusterStatus vs GetCapacityManifest vs
// ListExecutors). Apply here post-extraction.
pub(crate) fn scheduler_unreachable_condition(
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
    serde_json::json!({
        "type": "SchedulerUnreachable",
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": crate::scaling::transition_time(status, prev),
    })
}

/// 6-char lowercase-alphanumeric random suffix for Job names.
///
/// Not `generate_name`: K8s's generateName appends 5 chars AFTER
/// a create, meaning we don't know the name until the apiserver
/// returns. We want to log the name in the create-error path
/// (409, other API errors). Generating our own suffix is the
/// same collision math with better observability.
pub(crate) fn random_suffix() -> String {
    use rand::RngExt;
    // 36^6 ≈ 2.18 billion combinations. With ttl=60s and even
    // 1000 Jobs/sec, steady-state population is ~60k live names
    // → birthday collision ~0.08%. K8s 409 handles it.
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..6)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SchedulerUnreachable condition: status flips True/False based
    /// on whether the ClusterStatus RPC failed. Operators need this
    /// to distinguish "scheduler idle (queued=0)" from "scheduler
    /// down (queued unknown, treated as 0)."
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
    }

    /// random_suffix returns valid K8s name chars. DNS-1123 subdomain
    /// rules: lowercase alphanumeric, '-', max 253 chars. Our suffix
    /// is a tail fragment so '-' is fine contextually, but we use
    /// only alnum to be safe with any future prefix.
    #[test]
    fn random_suffix_valid_dns1123() {
        for _ in 0..100 {
            let s = random_suffix();
            assert_eq!(s.len(), 6);
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "invalid char in suffix: {s}"
            );
        }
    }

    /// `is_active_job` / `is_failed_job`: verify status→predicate
    /// mapping for all four Job-status quadrants. Not the exact
    /// inverse — Succeeded is neither active nor failed.
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

        // Fresh Job, no status → active, not failed.
        let fresh = Job::default();
        assert!(is_active_job(&fresh));
        assert!(!is_failed_job(&fresh));

        // Running: status populated, neither terminal → active.
        let running = job(Some(0), Some(0));
        assert!(is_active_job(&running));
        assert!(!is_failed_job(&running));

        // Succeeded: NOT active, NOT failed. Manifest pods loop so
        // this only happens on deliberate scale-down; reapable pass
        // handles it, sweep does not.
        let succeeded = job(Some(1), Some(0));
        assert!(!is_active_job(&succeeded));
        assert!(!is_failed_job(&succeeded));

        // Failed under backoff_limit=0: NOT active, IS failed. The
        // sweep target.
        let failed = job(Some(0), Some(1));
        assert!(!is_active_job(&failed));
        assert!(is_failed_job(&failed));
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

    const NO_GRACE: std::time::Duration = std::time::Duration::ZERO;

    // r[verify ctrl.ephemeral.reap-excess-pending]
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

    // r[verify ctrl.ephemeral.reap-excess-pending]
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

    // r[verify ctrl.ephemeral.reap-excess-pending]
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
    }

    /// Minimal `ExecutorInfo`. Only `executor_id` + `running_builds`
    /// are read by the orphan selector.
    fn executor(id: &str, running: u32) -> rio_proto::types::ExecutorInfo {
        rio_proto::types::ExecutorInfo {
            executor_id: id.into(),
            running_builds: running,
            ..Default::default()
        }
    }

    // r[verify ctrl.ephemeral.reap-orphan-running]
    /// I-165: Running Jobs older than grace, scheduler has no live
    /// assignment for them → reap. Jobs whose executor reports
    /// `running_builds > 0` are skipped (build in progress per
    /// scheduler; activeDeadlineSeconds is the backstop there).
    #[test]
    fn select_orphan_running_reaps_unassigned() {
        let jobs = vec![
            // Busy: executor registered, running_builds=1 → skip.
            job_with("rio-builder-x86-busy01", Some(1), Some(0), 600),
            // Idle-stuck: executor registered, running_builds=0, past
            // grace → process can't self-exit (D-state). Reap.
            job_with("rio-builder-x86-idle01", Some(1), Some(0), 600),
            // Ghost: no executor entry at all, past grace → never
            // registered or disconnected without Job→Failed. Reap.
            job_with("rio-builder-x86-ghost1", Some(1), Some(0), 600),
        ];
        let executors = vec![
            executor("rio-builder-x86-busy01-abcde", 1),
            executor("rio-builder-x86-idle01-fghij", 0),
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

    // r[verify ctrl.ephemeral.reap-orphan-running]
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

    // r[verify ctrl.ephemeral.reap-orphan-running]
    /// `{job_name}-` prefix match: trailing dash prevents Job
    /// "pool-abc" from matching executor of Job "pool-abcdef".
    #[test]
    fn select_orphan_running_prefix_is_dash_anchored() {
        let jobs = vec![job_with("rio-builder-x86-abc", Some(1), Some(0), 600)];
        // Executor for a DIFFERENT Job whose name shares the prefix.
        let executors = vec![executor("rio-builder-x86-abcdef-qwert", 1)];
        let orphans = select_orphan_running(&jobs, &executors, ORPHAN_REAP_GRACE);
        assert_eq!(
            orphans.len(),
            1,
            "executor 'abcdef-…' must NOT match Job 'abc' (would have \
             skipped as busy without the trailing-dash anchor)"
        );
    }
}
