//! Object builders (pure: BuilderPool → K8s objects)
//!
//! Thin wrapper over [`common::pod`](crate::reconcilers::common::pod)
//! since the ADR-019 builder/fetcher split. The 600-line pod-spec
//! lives in `common/pod.rs`; this file converts `BuilderPool` →
//! `ExecutorPodParams`.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::ResourceExt;

use crate::crds::builderpool::BuilderPool;
use crate::reconcilers::common::pod::{self, ExecutorPodParams, ExecutorRole};

// Re-exports for ephemeral.rs + tests.
pub use crate::reconcilers::common::pod::{SchedulerAddrs, StoreAddrs, parse_quantity_to_gb};

/// Labels applied to Jobs and pods for this pool.
pub(super) fn labels(wp: &BuilderPool) -> BTreeMap<String, String> {
    pod::executor_labels(&executor_params_for_labels(wp))
}

/// Convert `BuilderPool` → `ExecutorPodParams`. The builder-specific
/// tuning knobs (size_class, bloom, daemon_timeout, fuse_passthrough)
/// become `extra_env` entries — keeps them out of the shared params
/// struct where the fetcher reconciler would have to supply dummies.
fn executor_params(wp: &BuilderPool, cache_gb: u64, cache_quantity: Quantity) -> ExecutorPodParams {
    let mut extra_env = vec![pod::env("RIO_SIZE_CLASS", &wp.spec.size_class)];
    if let Some(p) = wp.spec.fuse_passthrough {
        extra_env.push(pod::env(
            "RIO_FUSE_PASSTHROUGH",
            if p { "true" } else { "false" },
        ));
    }
    if let Some(s) = wp.spec.daemon_timeout_secs {
        extra_env.push(pod::env("RIO_DAEMON_TIMEOUT_SECS", &s.to_string()));
    }
    // r[impl ctrl.pool.bloom-knob]
    if let Some(n) = wp.spec.bloom_expected_items {
        extra_env.push(pod::env("RIO_BLOOM_EXPECTED_ITEMS", &n.to_string()));
    }

    ExecutorPodParams {
        role: ExecutorRole::Builder,
        read_only_root_fs: false,
        extra_env,
        pool_name: wp.name_any(),
        namespace: wp.namespace().unwrap_or_default(),
        node_selector: wp.spec.node_selector.clone(),
        tolerations: wp.spec.tolerations.clone(),
        topology_spread: wp.spec.topology_spread,
        image: wp.spec.image.clone(),
        image_pull_policy: wp.spec.image_pull_policy.clone(),
        systems: wp.spec.systems.clone(),
        features: wp.spec.features.clone(),
        resources: wp.spec.resources.clone(),
        fuse_cache_gb: cache_gb,
        fuse_cache_quantity: cache_quantity,
        fuse_threads: wp.spec.fuse_threads,
        privileged: wp.spec.privileged == Some(true),
        seccomp_profile: wp.spec.seccomp_profile.clone(),
        seccomp_preinstalled: pod::seccomp_preinstalled(),
        host_network: wp.spec.host_network,
        host_users: wp.spec.host_users,
        tls_secret_name: wp.spec.tls_secret_name.clone(),
        termination_grace_period_seconds: wp.spec.termination_grace_period_seconds,
    }
}

/// Minimal params for label computation only. `labels()` is called
/// before cache parsing so it can't depend on the full params. The
/// cache fields don't affect labels anyway.
fn executor_params_for_labels(wp: &BuilderPool) -> ExecutorPodParams {
    executor_params(wp, 0, Quantity("0".into()))
}

/// Tests need to construct params directly so they can set fields
/// `executor_params` reads from the controller's environment
/// (`seccomp_preinstalled`). Going through `build_pod_spec` would
/// need `set_var`, which is parallel-test-unsafe.
#[cfg(test)]
pub(super) fn executor_params_for_test(wp: &BuilderPool) -> ExecutorPodParams {
    executor_params_for_labels(wp)
}

/// The pod spec. Re-exported for `ephemeral::build_job` and
/// `manifest::build_manifest_job` — the Job pod is the same
/// executor container with env/resource tweaks.
///
/// `resources_override`: `None` reads `wp.spec.resources`
/// (static-sizing path). `Some(r)` replaces it (manifest path —
/// per-bucket `ResourceRequirements` from `GetCapacityManifest`,
/// ADR-020).
pub(super) fn build_pod_spec(
    wp: &BuilderPool,
    scheduler: &SchedulerAddrs,
    store: &StoreAddrs,
    cache_gb: u64,
    cache_quantity: Quantity,
    resources_override: Option<ResourceRequirements>,
) -> PodSpec {
    let mut params = executor_params(wp, cache_gb, cache_quantity);
    if let Some(r) = resources_override {
        params.resources = Some(r);
    }
    pod::build_executor_pod_spec(&params, scheduler, store)
}
