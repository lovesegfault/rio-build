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
use kube::api::{Api, PostParams};
use kube::{CustomResourceExt, Resource, ResourceExt};

use crate::crds::builderpool::BuilderPool;
use crate::error::{Error, Result};
use crate::reconcilers::Ctx;

use super::MANAGER;
use super::builders::SchedulerAddrs;

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
/// the Failed dimension — Succeeded only happens on deliberate
/// scale-down (manifest pods loop, no `RIO_EPHEMERAL=1`) and the
/// reapable pass already handles those. Deduplicated from two
/// open-coded sites (inventory count + `select_failed_jobs`) after
/// P0511 landed both.
pub(super) fn is_failed_job(j: &Job) -> bool {
    j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0
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
) -> Result<(OwnerReference, SchedulerAddrs)> {
    let oref = wp.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("BuilderPool has no metadata.uid (not from apiserver?)".into())
    })?;
    let scheduler = SchedulerAddrs {
        addr: ctx.scheduler_addr.clone(),
        balance_host: ctx.scheduler_balance_host.clone(),
        balance_port: ctx.scheduler_balance_port,
    };
    Ok((oref, scheduler))
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
/// "Replicas" means "active Jobs" in Job-mode — `kubectl get wp`
/// shows the same columns either way (STS mode fills them from
/// `StatefulSet.status`; Job mode fills them here). The autoscaler
/// skips Job-mode pools (`scaling.rs` checks `spec.ephemeral` and
/// `spec.sizing`) so it won't overwrite this.
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
    use rand::Rng;
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
}
