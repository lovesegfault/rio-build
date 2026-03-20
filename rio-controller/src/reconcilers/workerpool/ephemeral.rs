//! Ephemeral WorkerPool: Job-per-assignment instead of StatefulSet.
//!
//! When `WorkerPoolSpec.ephemeral: true`, the reconciler does NOT
//! create a StatefulSet. Instead:
//!
//!   1. Each `apply()` tick polls `ClusterStatus.queued_derivations`
//!      via the same `ctx.admin` client the finalizer uses for
//!      DrainWorker.
//!   2. If `queued > 0` and active Jobs for this pool <
//!      `spec.replicas.max`, spawn Jobs (one per outstanding
//!      derivation, up to the ceiling).
//!   3. Each Job runs one rio-worker pod with `RIO_EPHEMERAL=1` →
//!      worker's main loop exits after one build → pod terminates →
//!      `ttlSecondsAfterFinished: 60` reaps the Job.
//!
//! From the scheduler's perspective, an ephemeral Job pod is
//! indistinguishable from an STS pod: it heartbeats in, gets a
//! dispatch, sends CompletionReport, disconnects. No scheduler-side
//! changes needed. The "ephemeral" property is purely worker-side
//! (exit after one build) + controller-side (Job lifecycle, not STS).
//!
//! # Why not a Scheduler→Controller RPC
//!
//! The plan (P0296) envisions the scheduler calling
//! `CreateEphemeralWorker` on the controller at dispatch time. That
//! requires: controller gains a gRPC server (it has none today),
//! scheduler gains a "pool" concept (it has none — only workers with
//! size_class), and new connection management / RBAC / NetworkPolicy.
//!
//! Polling ClusterStatus achieves the same outcome (Job spawned when
//! work exists) with existing infrastructure. Latency is one
//! reconciler requeue interval (~10s for ephemeral pools vs 5min for
//! STS pools). For the "untrusted multi-tenant" use case where
//! isolation > throughput, 10s added latency is acceptable. If
//! sub-second dispatch becomes a requirement, the RPC path can be
//! added later — the proto stub exists in `admin.proto`
//! `ControllerService`.
//!
//! # Job naming
//!
//! `{pool}-eph-{random-suffix}` — random because we don't have an
//! assignment_id at spawn time (the scheduler picks the derivation
//! AFTER the worker heartbeats). 6 lowercase-alnum chars: 36^6 ≈
//! 2 billion combinations; with `ttlSecondsAfterFinished: 60` and
//! realistic build rates, collision is effectively impossible. K8s
//! would reject on collision anyway (409 AlreadyExists) and next
//! tick retries.
//!
//! # Zero cross-build state
//!
//! Fresh pod = fresh emptyDir for FUSE cache + overlays. No bloom
//! accumulation (every heartbeat sends an empty filter →
//! `count_missing` in assignment.rs returns full closure for every
//! candidate → `W_LOCALITY` contributes nothing). An untrusted
//! tenant CANNOT leave poisoned cache entries for the next build —
//! there is no "next build" on that pod.

use std::time::Duration;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ListParams, ObjectMeta, PostParams};
use kube::runtime::controller::Action;
use kube::{CustomResourceExt, Resource, ResourceExt};
use tracing::{debug, info, warn};

use crate::crds::workerpool::WorkerPool;
use crate::error::Result;
use crate::reconcilers::Ctx;

use super::MANAGER;
use super::builders::{self, SchedulerAddrs};

/// Requeue interval for ephemeral pools. Shorter than the STS path's
/// 5min because Job spawning is reactive to queue depth, not just
/// spec drift. 10s: one queue-depth poll per tick. Shorter would
/// mean more `ClusterStatus` RPCs to the scheduler (cheap, but noise)
/// and more `kubectl get jobs` calls (apiserver load). Longer
/// lengthens dispatch latency.
///
/// This is the PRIMARY latency cost of ephemeral vs STS: an STS
/// worker is already heartbeating when the derivation arrives
/// (dispatch latency ~ms); an ephemeral worker needs one requeue
/// interval + pod scheduling + container pull + FUSE mount +
/// heartbeat (~10s + 10-30s). For "isolation > throughput" this
/// is the tradeoff.
const EPHEMERAL_REQUEUE: Duration = Duration::from_secs(10);

/// `ttlSecondsAfterFinished` on spawned Jobs. K8s TTL controller
/// deletes the Job (and its pod, via ownerRef) this many seconds
/// after it reaches Complete or Failed. 60s: long enough that an
/// operator debugging a failed build can `kubectl logs` the pod;
/// short enough that Job churn doesn't accumulate. The SCHEDULER
/// has already observed the completion (worker sent CompletionReport
/// before exiting) so there's no rio-side dependency on the Job
/// sticking around.
const JOB_TTL_SECS: i32 = 60;

// r[impl ctrl.pool.ephemeral]
/// Reconcile an ephemeral WorkerPool: count active Jobs, poll queue
/// depth, spawn Jobs if work is waiting.
///
/// NO StatefulSet / Service / PDB — those are STS-mode artifacts.
/// A headless Service for pod DNS is pointless (Job pods have random
/// names, no stable identity). A PDB is meaningless (Jobs aren't
/// evicted mid-drain; they run to completion or fail).
///
/// Status: `replicas` / `readyReplicas` / `desiredReplicas` are
/// repurposed to mean "active Jobs." `kubectl get wp` shows the same
/// columns either way. `desiredReplicas` is the concurrent-Job
/// ceiling (`spec.replicas.max`).
pub(super) async fn reconcile_ephemeral(wp: &WorkerPool, ctx: &Ctx) -> Result<Action> {
    let ns = wp.namespace().expect("checked in reconcile_inner");
    let name = wp.name_any();
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);

    // ---- Count active Jobs for this pool ----
    // "Active" = not yet reached Complete/Failed. K8s Job status has
    // `active` (running pods), `succeeded`, `failed`. We count Jobs
    // where succeeded==0 AND failed==0 — still in flight OR pending.
    // A Job whose pod is ContainerCreating is active for our purposes
    // (it'll heartbeat soon, don't double-spawn).
    let jobs = jobs_api
        .list(&ListParams::default().labels(&format!("rio.build/pool={name}")))
        .await?;
    let active: i32 = jobs
        .items
        .iter()
        .filter(|j| {
            // Neither succeeded nor failed → still active (running or
            // pending). unwrap_or(0): status may be None on a fresh
            // Job before the Job controller populates it. Treat that
            // as active (don't spawn another until we know).
            let s = j.status.as_ref();
            s.and_then(|st| st.succeeded).unwrap_or(0) == 0
                && s.and_then(|st| st.failed).unwrap_or(0) == 0
        })
        .count()
        .try_into()
        .unwrap_or(i32::MAX);
    let ceiling = wp.spec.replicas.max;

    // ---- Poll queue depth ----
    // Same ClusterStatus the autoscaler polls. One RPC per reconcile
    // per ephemeral pool. If the scheduler is unreachable (UNAVAILABLE
    // from standby, or genuinely down): log + treat as queued=0 +
    // requeue. Next tick retries. Spawning Jobs when we can't reach
    // the scheduler would waste pod starts (worker heartbeat would
    // fail → never Ready → Job eventually times out).
    //
    // Cloning the admin client: tonic clients are cheap to clone
    // (Arc-internal). The finalizer's cleanup() does the same.
    let queued: u32 = match ctx.admin.clone().cluster_status(()).await {
        Ok(resp) => resp.into_inner().queued_derivations,
        Err(e) => {
            warn!(
                pool = %name, error = %e,
                "ClusterStatus poll failed; treating as queued=0, will retry"
            );
            // Still patch status (so `kubectl get wp` shows current
            // active count even when scheduler is down) before
            // requeueing. Fall through with queued=0 → no spawn.
            0
        }
    };

    // ---- Spawn decision ----
    // Spawn min(queued, ceiling - active) Jobs. The cast dance:
    // queued is u32 (proto field), active/ceiling are i32 (K8s
    // replicas convention). saturating_sub handles active >= ceiling
    // (autoscaler isn't running for ephemeral pools, so this can
    // only happen if replicas.max was edited down while Jobs were
    // in flight — don't spawn more, but don't try to cancel either).
    let headroom = ceiling.saturating_sub(active).max(0) as u32;
    let to_spawn = queued.min(headroom);

    if to_spawn > 0 {
        let oref = wp
            .controller_owner_ref(&())
            .expect("apiserver-sourced object has uid");
        let scheduler = SchedulerAddrs {
            addr: ctx.scheduler_addr.clone(),
            balance_host: ctx.scheduler_balance_host.clone(),
            balance_port: ctx.scheduler_balance_port,
        };

        for _ in 0..to_spawn {
            let job = build_job(wp, oref.clone(), &scheduler, &ctx.store_addr)?;
            let job_name = job.metadata.name.clone().expect("we set it");
            // PostParams::default (not SSA): Jobs are create-once.
            // SSA's patch-merge semantics don't fit — there's no
            // "update existing Job to match spec," the Job is
            // immutable after create (K8s rejects most spec edits).
            // A 409 AlreadyExists (name collision from a concurrent
            // reconcile? random suffix collision?) is retried next
            // tick with a fresh random name.
            match jobs_api.create(&PostParams::default(), &job).await {
                Ok(_) => {
                    info!(
                        pool = %name, job = %job_name,
                        queued, active, ceiling,
                        "spawned ephemeral Job"
                    );
                }
                Err(kube::Error::Api(ae)) if ae.code == 409 => {
                    // AlreadyExists — random-suffix collision or
                    // concurrent reconcile. Next tick picks a fresh
                    // name. Not an error worth propagating (would
                    // trigger error_policy's backoff).
                    debug!(pool = %name, job = %job_name, "Job name collision; will retry");
                }
                Err(e) => return Err(e.into()),
            }
        }
    } else {
        debug!(
            pool = %name, queued, active, ceiling,
            "no ephemeral Jobs to spawn"
        );
    }

    // ---- Status patch ----
    // Repurpose the STS-oriented fields. `replicas` = active Jobs;
    // `readyReplicas` = same (a Job pod is "ready" when it's
    // running; we don't probe individual Job pods from here).
    // `desiredReplicas` = ceiling. The autoscaler skips ephemeral
    // pools (scaling.rs checks spec.ephemeral) so it won't
    // overwrite this.
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);
    let ar = WorkerPool::api_resource();
    let status_patch = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "replicas": active,
            "readyReplicas": active,
            "desiredReplicas": ceiling,
        },
    });
    wp_api
        .patch_status(
            &name,
            &kube::api::PatchParams::apply(MANAGER).force(),
            &kube::api::Patch::Apply(&status_patch),
        )
        .await?;

    Ok(Action::requeue(EPHEMERAL_REQUEUE))
}

/// Build a K8s Job for one ephemeral worker pod.
///
/// The pod spec is REUSED from `build_pod_spec` — same volumes,
/// security context, env. One addition: `RIO_EPHEMERAL=1` so the
/// worker's main loop exits after one build.
///
/// Job-specific settings:
///   - `restartPolicy: Never` — if the worker crashes (OOM,
///     panic), the Job goes Failed. The SCHEDULER owns retry
///     (reassign to a different worker or a different pool). K8s
///     retrying the same pod on the same node risks retry-in-a-
///     -tight-loop on a node-local problem.
///   - `backoffLimit: 0` — same reasoning. One attempt.
///   - `ttlSecondsAfterFinished: 60` — K8s TTL controller reaps.
///
/// NOT set: `activeDeadlineSeconds`. The worker's own daemon
/// timeout (from `spec.daemonTimeoutSecs`) + the per-build
/// timeout from BuildOptions are the real limits. Setting a Job
/// deadline would mean a legitimately slow build gets killed at
/// a DIFFERENT point than the scheduler expects → inconsistent
/// state (scheduler thinks it's running, pod is gone).
// r[impl ctrl.pool.ephemeral]
pub(super) fn build_job(
    wp: &WorkerPool,
    oref: OwnerReference,
    scheduler: &SchedulerAddrs,
    store_addr: &str,
) -> Result<Job> {
    let pool = wp.name_any();
    let labels = builders::labels(wp);

    // Same cache-size parse as build_statefulset. Ephemeral workers
    // still get a FUSE cache (emptyDir) — it's just wiped when the
    // pod terminates. The RIO_FUSE_CACHE_SIZE_GB env is still needed
    // for the worker's internal LRU bookkeeping.
    let cache_quantity = Quantity(wp.spec.fuse_cache_size.clone());
    let cache_gb = builders::parse_quantity_to_gb(&wp.spec.fuse_cache_size)?;

    let mut pod_spec =
        builders::build_pod_spec(wp, scheduler, store_addr, cache_gb, cache_quantity);

    // Append RIO_EPHEMERAL=1 to the worker container's env. The
    // container is always index 0 (build_pod_spec constructs exactly
    // one). unwrap on env: build_container ALWAYS sets Some(vec![..]).
    // If that invariant breaks (future refactor), panic here is
    // correct — an ephemeral pod without RIO_EPHEMERAL would loop
    // forever waiting for a second assignment that never comes (Job
    // wouldn't complete → ttlSecondsAfterFinished never fires →
    // leaked pod).
    pod_spec.containers[0]
        .env
        .as_mut()
        .expect("build_container always sets env")
        .push(builders::env("RIO_EPHEMERAL", "1"));

    // r[impl ctrl.pool.ephemeral-single-build]
    // Force RIO_MAX_BUILDS=1 regardless of spec.max_concurrent_builds.
    // build_pod_spec set it from spec (builders.rs build_container env
    // vec); ephemeral mode needs exactly one build per pod — a pod
    // running N builds shares FUSE cache + overlayfs upper across
    // those N, breaking the one-pod-per-build isolation guarantee.
    //
    // The CEL at workerpool.rs rejects ephemeral+maxConcurrentBuilds>1
    // at apply time; this override is DEFENSIVE for existing CRs
    // applied before the CEL landed, or future CEL drift.
    //
    // Find-and-replace, not push: build_pod_spec already set it. A
    // second env var with the same name is last-wins in K8s but
    // depending on that is fragile — explicit replace.
    for e in pod_spec.containers[0]
        .env
        .as_mut()
        .expect("build_container always sets env")
        .iter_mut()
    {
        if e.name == "RIO_MAX_BUILDS" {
            e.value = Some("1".into());
        }
    }

    // restartPolicy: Never is REQUIRED by K8s for Jobs with
    // backoffLimit=0. "Always" (the PodSpec default) is rejected.
    // build_pod_spec doesn't set it (STS pods default to Always
    // which is correct there). Set it here.
    pod_spec.restart_policy = Some("Never".into());

    // Random suffix: 6 lowercase alphanumeric. Not crypto; just
    // avoiding collisions. Using the worker_id downward-API pattern
    // from build_container means each pod's RIO_WORKER_ID is the
    // Job's pod name (also random-suffixed by K8s on top of our
    // suffix) — unique per ephemeral pod, which is what the
    // scheduler needs for its workers map.
    let suffix = random_suffix();
    // K8s name limit: 63 chars. `{pool}-eph-{6}` = pool+11. Most
    // pool names are short (<20 chars); if someone names a pool
    // 53+ chars, K8s rejects the Job with a clear error — no
    // silent truncation.
    let job_name = format!("{pool}-eph-{suffix}");

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
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// 6-char lowercase-alphanumeric random suffix for Job names.
///
/// Not `generate_name`: K8s's generateName appends 5 chars AFTER
/// a create, meaning we don't know the name until the apiserver
/// returns. We want to log the name in the create-error path
/// (409, other API errors). Generating our own suffix is the
/// same collision math with better observability.
fn random_suffix() -> String {
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
    use crate::crds::workerpool::{Autoscaling, Replicas, WorkerPoolSpec};

    fn test_wp() -> WorkerPool {
        let spec = WorkerPoolSpec {
            replicas: Replicas { min: 0, max: 4 },
            ephemeral: true,
            autoscaling: Autoscaling {
                metric: "queueDepth".into(),
                target_value: 5,
            },
            resources: None,
            max_concurrent_builds: 1,
            fuse_cache_size: "10Gi".into(),
            fuse_threads: None,
            fuse_passthrough: None,
            daemon_timeout_secs: None,
            features: vec![],
            systems: vec!["x86_64-linux".into()],
            size_class: String::new(),
            image: "rio-worker:test".into(),
            image_pull_policy: None,
            node_selector: None,
            tolerations: None,
            termination_grace_period_seconds: None,
            privileged: None,
            seccomp_profile: None,
            host_network: None,
            tls_secret_name: None,
            topology_spread: None,
            fod_proxy_url: None,
        };
        let mut wp = WorkerPool::new("eph-pool", spec);
        wp.metadata.uid = Some("uid-eph".into());
        wp.metadata.namespace = Some("rio".into());
        wp
    }

    fn test_sched() -> SchedulerAddrs {
        SchedulerAddrs {
            addr: "sched:9001".into(),
            balance_host: None,
            balance_port: 9001,
        }
    }

    /// Built Job has all the load-bearing settings. If any of these
    /// drift, ephemeral mode breaks silently:
    ///   - RIO_EPHEMERAL missing → worker loops forever, Job never
    ///     completes, pod leaked until manual intervention
    ///   - restartPolicy != Never → K8s rejects the Job on create
    ///     (hard error, at least visible)
    ///   - backoffLimit > 0 → K8s retries on crash, scheduler ALSO
    ///     retries → duplicate build
    ///   - ttlSecondsAfterFinished missing → completed Jobs
    ///     accumulate forever
    ///   - ownerReference missing → WorkerPool delete leaves orphan
    ///     Jobs (no GC)
    // r[verify ctrl.pool.ephemeral]
    #[test]
    fn job_spec_load_bearing_fields() {
        let wp = test_wp();
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched(), "store:9002").unwrap();

        // ownerReference → GC on WorkerPool delete.
        let orefs = job.metadata.owner_references.as_ref().unwrap();
        assert_eq!(orefs[0].kind, "WorkerPool");
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

        let pod_spec = spec.template.spec.as_ref().unwrap();
        assert_eq!(
            pod_spec.restart_policy.as_deref(),
            Some("Never"),
            "K8s rejects Jobs with restartPolicy=Always"
        );

        // RIO_EPHEMERAL=1 present. The MOST load-bearing assertion —
        // without it the worker doesn't know to exit after one build.
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        let eph = env
            .iter()
            .find(|e| e.name == "RIO_EPHEMERAL")
            .expect("RIO_EPHEMERAL env must be set");
        assert_eq!(eph.value.as_deref(), Some("1"));

        // Sanity: the REST of the env is still there (build_pod_spec
        // reuse, not a from-scratch pod). Check one representative.
        assert!(
            env.iter().any(|e| e.name == "RIO_SCHEDULER_ADDR"),
            "build_pod_spec env should be preserved"
        );
    }

    /// Job name format: {pool}-eph-{6-char-alnum}. The test can't
    /// pin the random suffix but CAN pin the structure — a future
    /// refactor that changes the format (say, to generateName)
    /// would break the "log name before create" observability.
    #[test]
    fn job_name_format() {
        let wp = test_wp();
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched(), "store:9002").unwrap();
        let name = job.metadata.name.unwrap();

        // Precondition self-assert: test_wp names the pool "eph-pool".
        // If someone renames it, the prefix check below would pass
        // for the wrong reason (or fail misleadingly).
        assert_eq!(wp.name_any(), "eph-pool");

        assert!(
            name.starts_with("eph-pool-eph-"),
            "expected {{pool}}-eph-{{suffix}}, got {name}"
        );
        let suffix = name.strip_prefix("eph-pool-eph-").unwrap();
        assert_eq!(suffix.len(), 6, "6-char random suffix");
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "suffix must be lowercase alnum (K8s DNS-1123): {suffix}"
        );
    }

    /// Regression: ephemeral with maxConcurrentBuilds=4 still gets
    /// RIO_MAX_BUILDS=1 in the Job pod. The CEL at workerpool.rs
    /// rejects this combo at apply time; this test proves the
    /// DEFENSIVE env-override fires regardless (existing bad CRs
    /// applied before the CEL landed, future CEL drift).
    ///
    /// Mutation check: comment out the find-and-replace loop in
    /// build_job → this fails with "spec had 4" (RIO_MAX_BUILDS
    /// stays at spec.max_concurrent_builds, build_pod_spec's value).
    ///
    /// Also asserts there's exactly ONE RIO_MAX_BUILDS env entry —
    /// the override is find-and-replace, not push-duplicate. K8s
    /// env is last-wins on duplicate names, but depending on that
    /// is fragile and surprising in `kubectl describe pod` output.
    // r[verify ctrl.pool.ephemeral-single-build]
    #[test]
    fn build_job_forces_max_builds_1_ignoring_spec() {
        let mut wp = test_wp();
        // CEL rejects this combo at apply time; the fixture sidesteps
        // the apiserver so we can prove the defensive override
        // independently. A real cluster would never let this spec
        // through — the point is belt-and-suspenders.
        wp.spec.max_concurrent_builds = 4;
        let oref = wp.controller_owner_ref(&()).unwrap();
        let job = build_job(&wp, oref, &test_sched(), "store:9002").unwrap();

        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let env = pod_spec.containers[0].env.as_ref().unwrap();

        let max_builds: Vec<_> = env.iter().filter(|e| e.name == "RIO_MAX_BUILDS").collect();
        assert_eq!(
            max_builds.len(),
            1,
            "exactly one RIO_MAX_BUILDS entry — override is \
             find-and-replace, not push-duplicate"
        );
        assert_eq!(
            max_builds[0].value.as_deref(),
            Some("1"),
            "ephemeral Job must force RIO_MAX_BUILDS=1 — spec had {}",
            wp.spec.max_concurrent_builds
        );
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
}
