//! Object builders (pure: BuilderPool → K8s objects)
//!
//! Thin wrapper over [`common::pod`](crate::reconcilers::common::pod)
//! since the ADR-019 builder/fetcher split. The 600-line pod-spec
//! lives in `common/pod.rs`; this file converts `BuilderPool` →
//! `ExecutorPodParams`.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::ResourceExt;

use crate::reconcilers::common::pod::{self, ExecutorKind, ExecutorPodParams};
use rio_crds::builderpool::BuilderPool;

// Re-export for jobs.rs + tests.
pub use crate::reconcilers::common::pod::UpstreamAddrs;

/// FUSE cache emptyDir sizeLimit for builder pods. Kubelet evicts on
/// overshoot. No CRD knob: pods are one-shot so the cache never
/// outlives one build's input closure.
pub(crate) const BUILDER_FUSE_CACHE: &str = "50Gi";

/// Labels applied to Jobs and pods for this pool.
pub(super) fn labels(wp: &BuilderPool) -> BTreeMap<String, String> {
    pod::executor_labels(&executor_params(wp))
}

/// Convert `BuilderPool` → `ExecutorPodParams`. The builder-specific
/// tuning knobs (daemon_timeout, fuse_passthrough) become `extra_env`
/// entries — keeps them out of the shared params struct where the
/// fetcher reconciler would have to supply dummies.
fn executor_params(wp: &BuilderPool) -> ExecutorPodParams {
    let mut extra_env = vec![];
    if let Some(p) = wp.spec.fuse_passthrough {
        extra_env.push(pod::env(
            "RIO_FUSE_PASSTHROUGH",
            if p { "true" } else { "false" },
        ));
    }
    if let Some(s) = wp.spec.daemon_timeout_secs {
        extra_env.push(pod::env("RIO_DAEMON_TIMEOUT_SECS", &s.to_string()));
    }

    ExecutorPodParams {
        role: ExecutorKind::Builder,
        read_only_root_fs: false,
        extra_env,
        pool_name: wp.name_any(),
        deploy: wp.spec.common.deploy.clone(),
        image_pull_policy: wp.spec.image_pull_policy.clone(),
        features: wp.spec.features.clone(),
        resources: wp.spec.resources.clone(),
        fuse_cache_quantity: Quantity(BUILDER_FUSE_CACHE.into()),
        fuse_threads: wp.spec.fuse_threads,
        privileged: wp.spec.privileged == Some(true),
        seccomp_profile: wp.spec.seccomp_profile.clone(),
        host_network: wp.spec.host_network,
        termination_grace_period_seconds: wp.spec.termination_grace_period_seconds,
    }
}

/// The pod spec. Re-exported for `jobs::build_job` — the
/// Job pod is the executor container with `wp.spec.resources` applied.
pub(super) fn build_pod_spec(
    wp: &BuilderPool,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
) -> PodSpec {
    let params = executor_params(wp);
    pod::build_executor_pod_spec(&params, scheduler, store)
}
