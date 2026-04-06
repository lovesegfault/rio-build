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

use crate::crds::fetcherpool::{FetcherPool, FetcherSizeClass};
use crate::error::{Error, Result};
use crate::reconcilers::Ctx;
use crate::reconcilers::builderpool::ephemeral::{EPHEMERAL_REQUEUE, JOB_TTL_SECS, spawn_count};
use crate::reconcilers::builderpool::job_common::{
    SpawnOutcome, is_active_job, random_suffix, reap_excess_pending, reap_orphan_running,
    scheduler_unreachable_condition, try_spawn_job,
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

/// Reconcile an ephemeral FetcherPool: count active Jobs, poll FOD
/// queue depth, spawn Jobs if FOD work is waiting.
///
/// Two paths:
///   - `classes` empty (back-compat): one logical pool, queue signal
///     is the flat `queued_fod_derivations`.
///   - `classes` non-empty (P0556, `r[ctrl.fetcherpool.ephemeral-
///     per-class]`): per-class loop. Queue signal is `GetSizeClass
///     Status.fod_classes[class.name].queued` (in-flight FOD demand
///     bucketed by `size_class_floor`); active Jobs counted per class
///     via `rio.build/pool={class.name}` label; ceiling is `class.
///     max_replicas` (falling back to `spec.replicas.max`).
pub(super) async fn reconcile_ephemeral(fp: &FetcherPool, ctx: &Ctx) -> Result<Action> {
    let ns = fp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("FetcherPool has no namespace".into()))?;
    let name = fp.name_any();
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);

    let oref = fp.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("FetcherPool has no metadata.uid (not from apiserver?)".into())
    })?;
    let scheduler = SchedulerAddrs {
        addr: ctx.scheduler_addr.clone(),
        balance_host: ctx.scheduler_balance_host.clone(),
        balance_port: ctx.scheduler_balance_port,
    };
    let store = ctx.store_addrs();

    // ── queue signals ───────────────────────────────────────────────
    // Classed pools need per-class depth from GetSizeClassStatus;
    // unclassed pools need the flat queued_fod_derivations from
    // ClusterStatus. Both polled best-effort (scheduler_err recorded
    // for the SchedulerUnreachable status condition).
    let (signals, scheduler_err) = fetch_queue_signals(ctx, fp).await;

    // ── per-class spawn loop ────────────────────────────────────────
    // Unclassed → single iteration with class=None.
    let mut total_active = 0i32;
    let mut total_spawned = 0u32;
    let mut total_queued = 0u32;
    let mut total_reaped = 0u32;
    let class_iter: Vec<Option<&FetcherSizeClass>> = if fp.spec.classes.is_empty() {
        vec![None]
    } else {
        fp.spec.classes.iter().map(Some).collect()
    };
    for (idx, class) in class_iter.into_iter().enumerate() {
        // r[impl ctrl.fetcherpool.multiarch]
        // pool_name doubles as the `rio.build/pool` label value
        // (executor_labels via executor_params) — per-class active-
        // Job selector. `{fp.name}-{class.name}` so two FetcherPools
        // (per-arch) with the same class don't count each other's
        // Jobs. MUST match executor_params' pool_name derivation.
        let pool_name = class
            .map(|c| format!("{name}-{}", c.name))
            .unwrap_or_else(|| name.clone());
        let ceiling = class
            .and_then(|c| c.max_replicas)
            .unwrap_or(fp.spec.replicas.max);
        let queued = signals.queued_for(class.map(|c| c.name.as_str()), idx);

        let jobs = jobs_api
            .list(&ListParams::default().labels(&format!("{POOL_LABEL}={pool_name}")))
            .await?;
        let active: i32 = jobs
            .items
            .iter()
            .filter(|j| is_active_job(j))
            .count()
            .try_into()
            .unwrap_or(i32::MAX);

        let headroom = ceiling.saturating_sub(active).max(0) as u32;
        let to_spawn = spawn_count(queued, active as u32, headroom);
        total_active += active;
        total_queued += queued;
        total_spawned += to_spawn;

        for _ in 0..to_spawn {
            let job = build_job(fp, class, oref.clone(), &scheduler, &store)?;
            let job_name = job
                .metadata
                .name
                .clone()
                .ok_or_else(|| Error::InvalidSpec("job name missing".into()))?;
            match try_spawn_job(&jobs_api, &job).await {
                SpawnOutcome::Spawned => {
                    info!(
                        pool = %name, class = %pool_name, job = %job_name,
                        queued, active, ceiling,
                        "spawned ephemeral fetcher Job"
                    );
                }
                SpawnOutcome::NameCollision => {
                    debug!(pool = %name, job = %job_name, "Job name collision; will retry");
                }
                SpawnOutcome::Failed(e) => {
                    warn!(
                        pool = %name, class = %pool_name, job = %job_name,
                        queued, active, ceiling, error = %e,
                        "ephemeral fetcher Job spawn failed; continuing tick"
                    );
                }
            }
        }

        // I-183: same reap as builderpool/ephemeral.rs — when per-class
        // queued drops below per-class Pending, delete the excess.
        // FetcherPool's 300s deadline makes this less acute than
        // BuilderPool's 1h, but the Karpenter node-churn cost is the
        // same. `None` when scheduler unreachable (fail-closed).
        let queued_known = scheduler_err.is_none().then_some(queued);
        total_reaped +=
            reap_excess_pending(&jobs_api, &jobs.items, queued_known, &name, &pool_name).await;

        // I-165: same orphan-reap as builderpool/ephemeral.rs. Less
        // acute here (FOD_EPHEMERAL_DEADLINE_SECS=300s ≈ the grace
        // itself) but a stuck fetcher still holds a node for 5min of
        // nothing. Lazy RPC; fail-closed.
        total_reaped += reap_orphan_running(&jobs_api, &jobs.items, ctx, &name, &pool_name).await;
    }

    patch_status(
        ctx,
        fp,
        &ns,
        &name,
        total_active,
        fp.spec.replicas.max,
        scheduler_err.as_deref(),
    )
    .await?;

    // I-086: parity with STS-mode (mod.rs `reconciled FetcherPool`) and
    // builderpool/mod.rs:481. Without this, an idle ephemeral pool is
    // invisible at INFO and indistinguishable from a stuck reconciler.
    info!(
        pool = %name, queued = total_queued, active = total_active,
        spawned = total_spawned, reaped = total_reaped,
        classes = fp.spec.classes.len(),
        "reconciled FetcherPool (ephemeral)"
    );

    Ok(Action::requeue(EPHEMERAL_REQUEUE))
}

/// Queue signals for one ephemeral reconcile tick. Carries both the
/// flat count (unclassed fallback) and the per-class breakdown so
/// `reconcile_ephemeral` can serve either path from one fetch.
struct QueueSignals {
    /// Flat in-flight FOD demand (`queued_fod_derivations`). Fallback
    /// when `fod_classes` is empty (scheduler `[[fetcher_size_classes]]`
    /// unconfigured or out of sync) — applied to `classes[0]` only so
    /// the smallest class over-spawns rather than every class
    /// duplicating the flat count.
    flat: u32,
    /// `name → in-flight demand`, from `GetSizeClassStatus.fod_classes`.
    by_class: std::collections::HashMap<String, u32>,
}

impl QueueSignals {
    fn zero() -> Self {
        Self {
            flat: 0,
            by_class: std::collections::HashMap::new(),
        }
    }

    /// Queue depth for one class iteration. `class=None` (unclassed
    /// pool) → flat. `class=Some(name)` → per-class count; if absent
    /// from `by_class` (scheduler doesn't know this class), fall back
    /// to `flat` for the SMALLEST class only (`idx==0`), 0 otherwise —
    /// matches `r[ctrl.fetcherpool.ephemeral-per-class]`'s
    /// over-spawn-smallest posture.
    fn queued_for(&self, class: Option<&str>, idx: usize) -> u32 {
        match class {
            None => self.flat,
            Some(name) => match self.by_class.get(name) {
                Some(&q) => q,
                None if self.by_class.is_empty() && idx == 0 => self.flat,
                None => 0,
            },
        }
    }
}

/// Poll the scheduler for FOD queue depth. Best-effort: on error,
/// returns `(zero, Some(err))` so the reconcile tick still runs
/// (status condition `SchedulerUnreachable` surfaces the error;
/// next requeue retries).
async fn fetch_queue_signals(ctx: &Ctx, fp: &FetcherPool) -> (QueueSignals, Option<String>) {
    let flat = match ctx.admin.clone().cluster_status(()).await {
        Ok(resp) => resp.into_inner().queued_fod_derivations,
        Err(e) => {
            warn!(
                pool = %fp.name_any(), error = %e,
                "ClusterStatus poll failed; treating as queued_fod=0, will retry"
            );
            return (QueueSignals::zero(), Some(e.to_string()));
        }
    };
    if fp.spec.classes.is_empty() {
        // Unclassed pool: per-class breakdown not needed.
        return (
            QueueSignals {
                flat,
                by_class: std::collections::HashMap::new(),
            },
            None,
        );
    }
    // r[impl ctrl.fetcherpool.ephemeral-per-class]
    let by_class = match ctx
        .admin
        .clone()
        .get_size_class_status(rio_proto::types::GetSizeClassStatusRequest::default())
        .await
    {
        Ok(resp) => resp
            .into_inner()
            .fod_classes
            .into_iter()
            .map(|c| {
                // I-143 per-system filter (mirrors builderpool::
                // queued_for_pool). Same u64→u32 saturating cast as
                // scaling::per_class — pathological queue depth
                // shouldn't wrap to 0.
                let q = crate::scaling::class_queued_for_systems(&c, &fp.spec.systems);
                (c.name, q.min(u32::MAX as u64) as u32)
            })
            .collect(),
        Err(e) => {
            warn!(
                pool = %fp.name_any(), error = %e,
                "GetSizeClassStatus poll failed; falling back to flat \
                 queued_fod for smallest class"
            );
            return (
                QueueSignals {
                    flat,
                    by_class: std::collections::HashMap::new(),
                },
                Some(e.to_string()),
            );
        }
    };
    (QueueSignals { flat, by_class }, None)
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
// r[impl ctrl.fetcherpool.ephemeral-per-class]
pub(super) fn build_job(
    fp: &FetcherPool,
    class: Option<&FetcherSizeClass>,
    oref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    scheduler: &SchedulerAddrs,
    store: &sts::StoreAddrs,
) -> Result<Job> {
    let params = executor_params(fp, class)?;
    // P0556: pool_name (= `rio.build/pool` label) is `class.name` when
    // classed, `fp.name_any()` when not — set inside executor_params.
    // Reused for the Job name prefix so logs/metrics group per class.
    let pool = params.pool_name.clone();
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
                    // I-126: same as builderpool — pin against karpenter
                    // consolidation evictions for the (short) Job lifetime.
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
                classes: vec![],
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
            None,
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
            None,
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
            None,
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

    // r[verify ctrl.fetcherpool.ephemeral-per-class]
    /// Per-class Job stamping. `class=Some(small)` →
    /// `RIO_SIZE_CLASS=small` env, per-class resources, `rio.build/
    /// pool={fp.name}-small` label (per-arch collision-safe), Job
    /// name prefix `rio-fetcher-{fp.name}-small-`.
    #[test]
    fn build_job_per_class_stamps_class() {
        use k8s_openapi::api::core::v1::ResourceRequirements;
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let fp = test_fp();
        let class = FetcherSizeClass {
            name: "small".into(),
            resources: ResourceRequirements {
                limits: Some(std::collections::BTreeMap::from([(
                    "memory".into(),
                    Quantity("8Gi".into()),
                )])),
                ..Default::default()
            },
            min_replicas: None,
            max_replicas: Some(4),
        };
        let oref = fp.controller_owner_ref(&()).unwrap();
        let job = build_job(
            &fp,
            Some(&class),
            oref,
            &test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
        )
        .unwrap();

        let labels = job.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels.get(POOL_LABEL).map(String::as_str),
            Some("eph-fp-small"),
            "classed pool label is `{{fp.name}}-{{class}}` (per-arch collision-safe)"
        );
        assert!(
            job.metadata
                .name
                .as_deref()
                .unwrap()
                .starts_with("rio-fetcher-eph-fp-small-"),
            "Job name prefix `rio-fetcher-{{fp}}-{{class}}-`: {:?}",
            job.metadata.name
        );

        let pod = job.spec.unwrap().template.spec.unwrap();
        let c = &pod.containers[0];
        let envs: std::collections::BTreeMap<_, _> = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
            .collect();
        assert_eq!(envs.get("RIO_SIZE_CLASS"), Some(&"small"));
        assert_eq!(envs.get("RIO_EPHEMERAL"), Some(&"1"));
        assert_eq!(
            c.resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .and_then(|l| l.get("memory")),
            Some(&Quantity("8Gi".into())),
            "per-class resources applied"
        );
    }

    // r[verify ctrl.fetcherpool.multiarch]
    /// Per-arch active-Job selector: `fp{name=aarch64}, class=tiny` →
    /// `rio.build/pool=aarch64-tiny`. The per-class spawn loop lists
    /// Jobs by this label; without the fp-name segment, an x86-64
    /// pool's `active` count would include aarch64 Jobs (over-counts
    /// → under-spawns) and `reap_excess_pending` could delete the
    /// other pool's Jobs.
    #[test]
    fn ephemeral_job_label_includes_pool() {
        let mut fp = test_fp();
        fp.metadata.name = Some("aarch64".into());
        let class = FetcherSizeClass {
            name: "tiny".into(),
            resources: Default::default(),
            min_replicas: None,
            max_replicas: None,
        };
        let oref = fp.controller_owner_ref(&()).unwrap();
        let job = build_job(
            &fp,
            Some(&class),
            oref,
            &test_sched_addrs(),
            &crate::fixtures::test_store_addrs(),
        )
        .unwrap();
        assert_eq!(
            job.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(POOL_LABEL))
                .map(String::as_str),
            Some("aarch64-tiny"),
        );
        // The selector string the spawn loop uses to list active Jobs:
        assert_eq!(
            format!("{POOL_LABEL}=aarch64-tiny"),
            "rio.build/pool=aarch64-tiny"
        );
    }

    // r[verify ctrl.fetcherpool.ephemeral-per-class]
    /// `QueueSignals::queued_for` routing: per-class count when known;
    /// flat fallback to smallest only; unclassed → flat.
    #[test]
    fn queue_signals_routing() {
        // Per-class breakdown known.
        let s = QueueSignals {
            flat: 5,
            by_class: std::collections::HashMap::from([("tiny".into(), 3), ("small".into(), 2)]),
        };
        assert_eq!(s.queued_for(Some("tiny"), 0), 3);
        assert_eq!(s.queued_for(Some("small"), 1), 2);
        assert_eq!(s.queued_for(None, 0), 5, "unclassed pool → flat");
        // Class the scheduler doesn't know → 0 (don't double-count).
        assert_eq!(s.queued_for(Some("huge"), 2), 0);

        // Breakdown empty (scheduler [[fetcher_size_classes]] off):
        // smallest class falls back to flat, others get 0.
        let s = QueueSignals {
            flat: 5,
            by_class: std::collections::HashMap::new(),
        };
        assert_eq!(s.queued_for(Some("tiny"), 0), 5, "smallest gets flat");
        assert_eq!(s.queued_for(Some("small"), 1), 0, "non-smallest gets 0");
    }
}
