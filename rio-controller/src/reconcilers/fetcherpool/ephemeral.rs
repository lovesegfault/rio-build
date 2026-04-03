//! Ephemeral FetcherPool: Job-per-FOD instead of StatefulSet.
//!
//! Same Job lifecycle as `builderpool/ephemeral.rs` (see that
//! module's header for the full design rationale — push-vs-poll,
//! Job naming, zero cross-build state). Differences here:
//!
//!   - Queue signal is `queued_fod_derivations` (in-flight FOD
//!     demand), not `queued_derivations`.
//!   - Pod spec comes from `executor_params(fp)` →
//!     `sts::build_executor_pod_spec` (the fetcher-hardened params:
//!     `readOnlyRootFilesystem`, stricter seccomp, fetcher node
//!     affinity).
//!   - Default `activeDeadlineSeconds` is 300 not 3600 — fetches
//!     are network-bound and short; the failure mode is a stuck
//!     download, not a 90min compile.
//!
//! The pure helpers (`is_active_job`, `try_spawn_job`,
//! `random_suffix`, `scheduler_unreachable_condition`,
//! `spawn_count`, `EPHEMERAL_REQUEUE`, `JOB_TTL_SECS`) are reused
//! from the builderpool path. The CR-typed glue (status patch, Job
//! build) is reproduced — the two `*PoolStatus` shapes and
//! `executor_params` signatures differ nominally but a trait would
//! be more code than the ~40 lines duplicated.

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use kube::api::{Api, ListParams, ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{CustomResourceExt, Resource, ResourceExt};
use tracing::{debug, info, warn};

use crate::crds::fetcherpool::FetcherPool;
use crate::error::{Error, Result};
use crate::reconcilers::Ctx;
use crate::reconcilers::builderpool::ephemeral::{EPHEMERAL_REQUEUE, JOB_TTL_SECS, spawn_count};
use crate::reconcilers::builderpool::job_common::{
    SpawnOutcome, is_active_job, random_suffix, scheduler_unreachable_condition, try_spawn_job,
};
use crate::reconcilers::common::sts::{self, ExecutorRole, POOL_LABEL, SchedulerAddrs, env};

use super::{MANAGER, executor_params};

/// Default `activeDeadlineSeconds` when `FetcherPoolSpec.ephemeral_
/// deadline_seconds` is unset. 300 (5min): a fetch that hasn't
/// completed in 5 minutes is a stuck download (TCP black-hole,
/// upstream rate-limit) — kill and let the scheduler retry on a
/// fresh pod. The wrong-pool-spawn case (BuilderPool's 1h backstop
/// rationale) is less of a concern here: `queued_fod_derivations`
/// is FOD-specific, so a queue full of x86 FODs DOES match an x86
/// fetcher pool. The remaining mismatch is system-only (arm64 pool
/// spawning for x86 FODs), bounded by the same deadline.
pub(super) const FOD_EPHEMERAL_DEADLINE_SECS: i64 = 300;

/// Reconcile an ephemeral FetcherPool: count active Jobs, poll
/// `queued_fod_derivations`, spawn Jobs if FOD work is waiting.
pub(super) async fn reconcile_ephemeral(fp: &FetcherPool, ctx: &Ctx) -> Result<Action> {
    let ns = fp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("FetcherPool has no namespace".into()))?;
    let name = fp.name_any();
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);

    let jobs = jobs_api
        .list(&ListParams::default().labels(&format!("{POOL_LABEL}={name}")))
        .await?;
    let active: i32 = jobs
        .items
        .iter()
        .filter(|j| is_active_job(j))
        .count()
        .try_into()
        .unwrap_or(i32::MAX);
    let ceiling = fp.spec.replicas.max;

    let (queued, scheduler_err) = match ctx.admin.clone().cluster_status(()).await {
        Ok(resp) => (resp.into_inner().queued_fod_derivations, None),
        Err(e) => {
            warn!(
                pool = %name, error = %e,
                "ClusterStatus poll failed; treating as queued_fod=0, will retry"
            );
            (0, Some(e.to_string()))
        }
    };

    let headroom = ceiling.saturating_sub(active).max(0) as u32;
    let to_spawn = spawn_count(queued, active as u32, headroom);

    if to_spawn > 0 {
        let oref = fp.controller_owner_ref(&()).ok_or_else(|| {
            Error::InvalidSpec("FetcherPool has no metadata.uid (not from apiserver?)".into())
        })?;
        let scheduler = SchedulerAddrs {
            addr: ctx.scheduler_addr.clone(),
            balance_host: ctx.scheduler_balance_host.clone(),
            balance_port: ctx.scheduler_balance_port,
        };

        let store = ctx.store_addrs();
        for _ in 0..to_spawn {
            let job = build_job(fp, oref.clone(), &scheduler, &store)?;
            let job_name = job
                .metadata
                .name
                .clone()
                .ok_or_else(|| Error::InvalidSpec("job name missing".into()))?;
            match try_spawn_job(&jobs_api, &job).await {
                SpawnOutcome::Spawned => {
                    info!(
                        pool = %name, job = %job_name, queued, active, ceiling,
                        "spawned ephemeral fetcher Job"
                    );
                }
                SpawnOutcome::NameCollision => {
                    debug!(pool = %name, job = %job_name, "Job name collision; will retry");
                }
                SpawnOutcome::Failed(e) => {
                    warn!(
                        pool = %name, job = %job_name, queued, active, ceiling, error = %e,
                        "ephemeral fetcher Job spawn failed; continuing tick"
                    );
                }
            }
        }
    }

    patch_status(
        ctx,
        fp,
        &ns,
        &name,
        active,
        ceiling,
        scheduler_err.as_deref(),
    )
    .await?;

    // I-086: parity with STS-mode (mod.rs `reconciled FetcherPool`) and
    // builderpool/mod.rs:481. Without this, an idle ephemeral pool is
    // invisible at INFO and indistinguishable from a stuck reconciler.
    info!(
        pool = %name, queued, active, ceiling, spawned = to_spawn,
        "reconciled FetcherPool (ephemeral)"
    );

    Ok(Action::requeue(EPHEMERAL_REQUEUE))
}

/// SSA-patch `.status` for a Job-mode FetcherPool. Same shape as
/// `builderpool/job_common::patch_job_pool_status` but FetcherPool-
/// typed (no `replicas` field on FetcherPoolStatus — only
/// ready/desired).
async fn patch_status(
    ctx: &Ctx,
    fp: &FetcherPool,
    ns: &str,
    name: &str,
    active: i32,
    desired: i32,
    scheduler_err: Option<&str>,
) -> Result<()> {
    let api: Api<FetcherPool> = Api::namespaced(ctx.client.clone(), ns);
    let ar = FetcherPool::api_resource();
    let prev = crate::scaling::find_fp_condition(fp, "SchedulerUnreachable");
    let cond = scheduler_unreachable_condition(scheduler_err, prev.as_ref());
    api.patch_status(
        name,
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(serde_json::json!({
            "apiVersion": ar.api_version,
            "kind": ar.kind,
            "status": {
                "readyReplicas": active,
                "desiredReplicas": desired,
                "conditions": [cond],
            },
        })),
    )
    .await?;
    Ok(())
}

/// Build a K8s Job for one ephemeral fetcher pod. Same Job-level
/// settings as `builderpool/ephemeral::build_job` (`backoffLimit: 0`,
/// `restartPolicy: Never`, `ttlSecondsAfterFinished`); the pod spec
/// comes from the fetcher-hardened `executor_params`.
pub(super) fn build_job(
    fp: &FetcherPool,
    oref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    scheduler: &SchedulerAddrs,
    store: &sts::StoreAddrs,
) -> Result<Job> {
    let pool = fp.name_any();
    let params = executor_params(fp)?;
    let labels = sts::executor_labels(&params);

    let mut pod_spec = sts::build_executor_pod_spec(&params, scheduler, store);
    pod_spec.containers[0]
        .env
        .as_mut()
        .ok_or_else(|| {
            Error::InvalidSpec("build_container produced a container with no env".into())
        })?
        .push(env("RIO_EPHEMERAL", "1"));
    pod_spec.restart_policy = Some("Never".into());
    // I-090: ephemeral Jobs are short-lived single-FOD downloads — the
    // STS-mode anti-affinity/spread (HA for long-lived pods) makes
    // karpenter provision one node per Job. Bin-pack instead.
    pod_spec.affinity = None;
    pod_spec.topology_spread_constraints = None;
    // I-114: same as builderpool — drop liveness/readiness for one-shot Jobs.
    pod_spec.containers[0].liveness_probe = None;
    pod_spec.containers[0].readiness_probe = None;
    pod_spec.containers[0].startup_probe = None;
    pod_spec.termination_grace_period_seconds = Some(30);

    let job_name = sts::ephemeral_job_name(&pool, ExecutorRole::Fetcher, &random_suffix());

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: fp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            parallelism: Some(1),
            completions: Some(1),
            backoff_limit: Some(0),
            ttl_seconds_after_finished: Some(JOB_TTL_SECS),
            active_deadline_seconds: Some(
                fp.spec
                    .ephemeral_deadline_seconds
                    .map(i64::from)
                    .unwrap_or(FOD_EPHEMERAL_DEADLINE_SECS),
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::builderpool::{Autoscaling, Replicas};
    use crate::fixtures::test_sched_addrs;
    use kube::Resource;

    fn test_fp() -> FetcherPool {
        let spec: crate::crds::fetcherpool::FetcherPoolSpec = serde_json::from_value(
            serde_json::to_value(crate::crds::fetcherpool::FetcherPoolSpec {
                ephemeral: true,
                ephemeral_deadline_seconds: None,
                replicas: Replicas { min: 0, max: 8 },
                autoscaling: Autoscaling {
                    metric: "fodQueueDepth".into(),
                    target_value: 5,
                },
                image: "rio-fetcher:test".into(),
                systems: vec!["x86_64-linux".into()],
                node_selector: None,
                tolerations: None,
                resources: None,
                tls_secret_name: None,
                host_users: None,
            })
            .unwrap(),
        )
        .unwrap();
        let mut fp = FetcherPool::new("eph-fp", spec);
        fp.metadata.namespace = Some("rio-fetchers".into());
        fp.metadata.uid = Some("11111111-1111-1111-1111-111111111111".into());
        fp
    }

    /// Job carries the fetcher-hardened pod spec (not the builder
    /// one): `RIO_EXECUTOR_KIND=fetcher`, `readOnlyRootFilesystem:
    /// true`, fetcher seccomp, plus `RIO_EPHEMERAL=1`.
    #[test]
    fn build_job_uses_fetcher_params() {
        let fp = test_fp();
        let oref = fp.controller_owner_ref(&()).unwrap();
        let job = build_job(
            &fp,
            oref,
            &test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
        )
        .unwrap();
        let spec = job.spec.unwrap();
        let pod = spec.template.spec.unwrap();
        let c = &pod.containers[0];
        let envs: std::collections::BTreeMap<_, _> = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
            .collect();
        assert_eq!(envs.get("RIO_EXECUTOR_KIND"), Some(&"fetcher"));
        assert_eq!(envs.get("RIO_EPHEMERAL"), Some(&"1"));
        assert_eq!(
            c.security_context
                .as_ref()
                .and_then(|s| s.read_only_root_filesystem),
            Some(true),
            "fetcher hardening applied"
        );
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        assert!(
            pod.affinity.is_none() && pod.topology_spread_constraints.is_none(),
            "I-090: ephemeral Jobs bin-pack (no anti-affinity/spread)"
        );
        assert!(
            pod.node_selector
                .as_ref()
                .is_none_or(|ns| !ns.contains_key("kubernetes.io/arch")),
            "I-098: fetchers float across arches (builtin runs anywhere)"
        );
        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(spec.ttl_seconds_after_finished, Some(JOB_TTL_SECS));
        assert_eq!(
            spec.active_deadline_seconds,
            Some(FOD_EPHEMERAL_DEADLINE_SECS),
            "default 5min, not BuilderPool's 1h — stuck download is the failure mode"
        );
    }

    /// `ephemeralDeadlineSeconds` overrides the 300s default.
    #[test]
    fn build_job_honors_deadline_override() {
        let mut fp = test_fp();
        fp.spec.ephemeral_deadline_seconds = Some(900);
        let oref = fp.controller_owner_ref(&()).unwrap();
        let job = build_job(
            &fp,
            oref,
            &test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
        )
        .unwrap();
        assert_eq!(job.spec.unwrap().active_deadline_seconds, Some(900));
    }

    /// Labels include `rio.build/pool=<name>` so the active-Job list
    /// in `reconcile_ephemeral` finds them.
    #[test]
    fn build_job_labels_include_pool() {
        let fp = test_fp();
        let oref = fp.controller_owner_ref(&()).unwrap();
        let job = build_job(
            &fp,
            oref,
            &test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
        )
        .unwrap();
        let labels = job.metadata.labels.unwrap();
        assert_eq!(labels.get(POOL_LABEL).map(String::as_str), Some("eph-fp"));
        assert_eq!(
            labels.get("rio.build/role").map(String::as_str),
            Some("fetcher")
        );
    }
}
