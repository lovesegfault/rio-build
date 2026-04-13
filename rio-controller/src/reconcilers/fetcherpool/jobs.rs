//! FetcherPool Job-per-FOD reconciler.
//!
//! Same Job lifecycle as `builderpool/jobs.rs` (see that
//! module's header for the full design rationale — push-vs-poll,
//! Job naming, zero cross-build state). Differences here:
//!
//!   - Queue signal is `queued_fod_derivations` (in-flight FOD
//!     demand), not `queued_derivations`.
//!   - Pod spec comes from `executor_params(fp)` →
//!     `pod::build_executor_pod_spec` (the fetcher-hardened params:
//!     `readOnlyRootFilesystem`, stricter seccomp, fetcher node
//!     affinity).
//!   - Default `activeDeadlineSeconds` is 300 not 3600 — fetches
//!     are network-bound and short; the failure mode is a stuck
//!     download, not a 90min compile.
//!
//! The Job-lifecycle plumbing (`is_active_job`, `try_spawn_job`,
//! `random_suffix`, `spawn_count`, `JOB_REQUEUE`, `JOB_TTL_SECS`,
//! `JobReconcilePrologue`, `patch_job_pool_status`, both reap
//! helpers) is shared with the builder reconcilers via
//! `common::job`. Only `build_job` (the per-role pod spec) and the
//! per-class spawn loop remain FetcherPool-specific.

use k8s_openapi::api::batch::v1::Job;
use kube::ResourceExt;
use kube::api::ListParams;
use kube::runtime::controller::Action;
use tracing::{info, warn};

use crate::error::Result;
use crate::reconcilers::Ctx;
#[cfg(test)]
use crate::reconcilers::common::job::JOB_TTL_SECS;
use crate::reconcilers::common::job::{
    JOB_REQUEUE, JobReconcilePrologue, ephemeral_job, is_active_job, job_reconcile_prologue,
    patch_job_pool_status, random_suffix, reap_excess_pending, reap_orphan_running,
    report_deadline_exceeded_jobs, report_terminated_pods, spawn_count, spawn_n,
};
use crate::reconcilers::common::pod::{self, ExecutorKind, POOL_LABEL, UpstreamAddrs};
use rio_crds::fetcherpool::{FetcherPool, FetcherSizeClass};

use super::executor_params;

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
/// Per-class loop (P0556, `r[ctrl.fetcherpool.ephemeral-per-class]`;
/// CEL enforces `classes` non-empty). Queue signal is `GetSizeClass
/// Status.fod_classes[class.name].queued` (in-flight FOD demand
/// bucketed by `size_class_floor`); active Jobs counted per class
/// via `rio.build/pool={class.name}` label; ceiling is `class.
/// max_replicas` (falling back to `spec.replicas.max`).
pub(super) async fn reconcile(fp: &FetcherPool, ctx: &Ctx) -> Result<Action> {
    let JobReconcilePrologue {
        ns,
        name,
        jobs_api,
        oref,
        scheduler,
        store,
    } = job_reconcile_prologue(fp, ctx)?;

    // ── queue signals ───────────────────────────────────────────────
    // Per-class depth from GetSizeClassStatus, with the flat
    // queued_fod_derivations from ClusterStatus as fallback. Both
    // polled best-effort (scheduler_err recorded for the
    // SchedulerUnreachable status condition).
    let (signals, scheduler_err) = fetch_queue_signals(ctx, fp).await;

    // ── per-class spawn loop ────────────────────────────────────────
    let mut total_active = 0i32;
    let mut total_spawned = 0u32;
    let mut total_queued = 0u32;
    let mut total_reaped = 0u32;
    for (idx, class) in fp.spec.classes.iter().enumerate() {
        // r[impl ctrl.fetcherpool.multiarch]
        // pool_name doubles as the `rio.build/pool` label value
        // (executor_labels via executor_params) — per-class active-
        // Job selector. `{fp.name}-{class.name}` so two FetcherPools
        // (per-arch) with the same class don't count each other's
        // Jobs. MUST match executor_params' pool_name derivation.
        let pool_name = format!("{name}-{}", class.name);
        let ceiling = class.max_concurrent.unwrap_or(fp.spec.max_concurrent) as i32;
        let queued = signals.queued_for(&class.name, idx);

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

        total_spawned += spawn_n(&jobs_api, to_spawn, &name, &pool_name, || {
            build_job(fp, class, oref.clone(), &scheduler, &store)
        })
        .await;

        // I-183: same reap as builderpool/jobs.rs — when per-class
        // queued drops below per-class Pending, delete the excess.
        // FetcherPool's 300s deadline makes this less acute than
        // BuilderPool's 1h, but the Karpenter node-churn cost is the
        // same. `None` when scheduler unreachable (fail-closed).
        let queued_known = scheduler_err.is_none().then_some(queued);
        total_reaped +=
            reap_excess_pending(&jobs_api, &jobs.items, queued_known, &name, &pool_name).await;

        // I-165: same orphan-reap as builderpool/jobs.rs. Less
        // acute here (FOD_EPHEMERAL_DEADLINE_SECS=300s ≈ the grace
        // itself) but a stuck fetcher still holds a node for 5min of
        // nothing. Lazy RPC; fail-closed.
        total_reaped += reap_orphan_running(&jobs_api, &jobs.items, ctx, &name, &pool_name).await;

        // Gate scheduler-side `size_class_floor` promotion on actual
        // k8s OOMKilled/DiskPressure (not bare disconnect). Per-class
        // pool_name + class.name so the scheduler can resolve
        // next_fetcher_class. Best-effort; scheduler-side dedup makes
        // re-reporting every tick a no-op.
        report_terminated_pods(ctx, &ns, &pool_name, &class.name).await;
        report_deadline_exceeded_jobs(ctx, &jobs.items, &class.name).await;
    }

    patch_job_pool_status::<FetcherPool, _>(
        ctx,
        fp.status.as_ref(),
        &ns,
        &name,
        None,
        total_active,
        fp.spec.max_concurrent as i32,
        scheduler_err.as_deref(),
    )
    .await?;

    // I-086: without this, an idle pool is invisible at INFO and
    // indistinguishable from a stuck reconciler.
    info!(
        pool = %name, queued = total_queued, active = total_active,
        spawned = total_spawned, reaped = total_reaped,
        classes = fp.spec.classes.len(),
        "reconciled FetcherPool (ephemeral)"
    );

    Ok(Action::requeue(JOB_REQUEUE))
}

/// Queue signals for one ephemeral reconcile tick. Carries both the
/// flat count (unclassed fallback) and the per-class breakdown so
/// `reconcile` can serve either path from one fetch.
struct QueueSignals {
    /// Flat in-flight FOD demand (`queued_fod_derivations`). Fallback
    /// when `fod_classes` is empty (scheduler `[[size_classes]]`
    /// unconfigured) — applied to `classes[0]` only so
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

    /// Queue depth for one class iteration. Per-class count; if absent
    /// from `by_class` (scheduler doesn't know this class), fall back
    /// to `flat` for the SMALLEST class only (`idx==0`), 0 otherwise —
    /// matches `r[ctrl.fetcherpool.ephemeral-per-class]`'s
    /// over-spawn-smallest posture.
    fn queued_for(&self, class: &str, idx: usize) -> u32 {
        match self.by_class.get(class) {
            Some(&q) => q,
            None if self.by_class.is_empty() && idx == 0 => self.flat,
            None => 0,
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

/// Build a K8s Job for one ephemeral fetcher pod. Same Job-level
/// settings as `builderpool/jobs::build_job` (`backoffLimit: 0`,
/// `restartPolicy: Never`, `ttlSecondsAfterFinished`); the pod spec
/// comes from the fetcher-hardened `executor_params`.
// r[impl ctrl.fetcherpool.ephemeral-per-class]
pub(super) fn build_job(
    fp: &FetcherPool,
    class: &FetcherSizeClass,
    oref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
) -> Result<Job> {
    let params = executor_params(fp, class);
    // P0556: pool_name (= `rio.build/pool` label) is `{fp.name}-{class.
    // name}` — set inside executor_params. Reused for the Job name
    // prefix so logs/metrics group per class.
    let pool = params.pool_name.clone();
    let job_name = pod::job_name(&pool, ExecutorKind::Fetcher, &random_suffix());
    let labels = pod::executor_labels(&params);
    let pod_spec = pod::build_executor_pod_spec(&params, scheduler, store);

    Ok(ephemeral_job(
        job_name,
        fp.namespace(),
        oref,
        labels,
        fp.spec
            .deadline_seconds
            .map(i64::from)
            .unwrap_or(FOD_EPHEMERAL_DEADLINE_SECS),
        pod_spec,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{test_fetcherpool, test_sched_addrs, test_store_addrs};

    fn test_fp() -> FetcherPool {
        test_fetcherpool("eph-fp")
    }

    fn cls(fp: &FetcherPool) -> &FetcherSizeClass {
        &fp.spec.classes[0]
    }

    /// Job carries the fetcher-hardened pod spec (not the builder
    /// one): `RIO_EXECUTOR_KIND=fetcher`, `readOnlyRootFilesystem:
    /// true`, fetcher seccomp.
    #[test]
    fn build_job_uses_fetcher_params() {
        let fp = test_fp();
        let oref = crate::fixtures::oref(&fp);
        let job = build_job(
            &fp,
            cls(&fp),
            oref,
            &test_sched_addrs(),
            &test_store_addrs(),
        )
        .unwrap();
        let spec = job.spec.unwrap();
        let pod = spec.template.spec.unwrap();
        let c = &pod.containers[0];
        let envs = crate::fixtures::env_map(c.env.as_deref().unwrap());
        assert_eq!(envs.get("RIO_EXECUTOR_KIND"), Some(&"fetcher"));
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
        fp.spec.deadline_seconds = Some(900);
        let oref = crate::fixtures::oref(&fp);
        let job = build_job(
            &fp,
            cls(&fp),
            oref,
            &test_sched_addrs(),
            &test_store_addrs(),
        )
        .unwrap();
        assert_eq!(job.spec.unwrap().active_deadline_seconds, Some(900));
    }

    /// Labels include `rio.build/pool=<name>` so the active-Job list
    /// in `reconcile` finds them.
    #[test]
    fn build_job_labels_include_pool() {
        let fp = test_fp();
        let oref = crate::fixtures::oref(&fp);
        let job = build_job(
            &fp,
            cls(&fp),
            oref,
            &test_sched_addrs(),
            &test_store_addrs(),
        )
        .unwrap();
        let labels = job.metadata.labels.unwrap();
        assert_eq!(
            labels.get(POOL_LABEL).map(String::as_str),
            Some("eph-fp-default")
        );
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
        let class: FetcherSizeClass = rio_crds::common::SizeClassCommon {
            name: "small".into(),
            resources: ResourceRequirements {
                limits: Some(std::collections::BTreeMap::from([(
                    "memory".into(),
                    Quantity("8Gi".into()),
                )])),
                ..Default::default()
            },
            max_concurrent: Some(4),
        }
        .into();
        let oref = crate::fixtures::oref(&fp);
        let job = build_job(&fp, &class, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();

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
        let envs = crate::fixtures::env_map(c.env.as_deref().unwrap());
        assert_eq!(envs.get("RIO_SIZE_CLASS"), Some(&"small"));
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
        let class: FetcherSizeClass = rio_crds::common::SizeClassCommon {
            name: "tiny".into(),
            resources: Default::default(),
            max_concurrent: None,
        }
        .into();
        let oref = crate::fixtures::oref(&fp);
        let job = build_job(&fp, &class, oref, &test_sched_addrs(), &test_store_addrs()).unwrap();
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
    /// flat fallback to smallest only.
    #[test]
    fn queue_signals_routing() {
        // Per-class breakdown known.
        let s = QueueSignals {
            flat: 5,
            by_class: std::collections::HashMap::from([("tiny".into(), 3), ("small".into(), 2)]),
        };
        assert_eq!(s.queued_for("tiny", 0), 3);
        assert_eq!(s.queued_for("small", 1), 2);
        // Class the scheduler doesn't know → 0 (don't double-count).
        assert_eq!(s.queued_for("huge", 2), 0);

        // Breakdown empty (scheduler [[size_classes]] off):
        // smallest class falls back to flat, others get 0.
        let s = QueueSignals {
            flat: 5,
            by_class: std::collections::HashMap::new(),
        };
        assert_eq!(s.queued_for("tiny", 0), 5, "smallest gets flat");
        assert_eq!(s.queued_for("small", 1), 0, "non-smallest gets 0");
    }
}
