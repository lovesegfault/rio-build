//! Object builders (pure: BuilderPool → K8s objects)
//!
//! Thin wrapper over [`common::sts`](crate::reconcilers::common::sts)
//! since the ADR-019 builder/fetcher split. The 600-line pod-spec
//! lives in `common/sts.rs`; this file converts `BuilderPool` →
//! `ExecutorStsParams` and keeps the builder-only bits (headless
//! Service, PDB).

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    PodSpec, ResourceRequirements, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::ResourceExt;
use kube::api::ObjectMeta;

use crate::crds::builderpool::BuilderPool;
use crate::error::Result;
use crate::reconcilers::common::sts::{self, ExecutorRole, ExecutorStsParams};

// Re-exports for ephemeral.rs + tests. The types live in common/sts.rs
// now; re-exporting keeps the existing `use super::builders::X` paths
// working.
pub use crate::reconcilers::common::sts::{SchedulerAddrs, env, parse_quantity_to_gb};

/// Labels applied to the StatefulSet, Service, and pods.
pub(super) fn labels(wp: &BuilderPool) -> BTreeMap<String, String> {
    sts::executor_labels(&executor_params_for_labels(wp))
}

/// Convert `BuilderPool` → `ExecutorStsParams`. The builder-specific
/// tuning knobs (size_class, bloom, daemon_timeout, fuse_passthrough)
/// become `extra_env` entries — keeps them out of the shared params
/// struct where the fetcher reconciler would have to supply dummies.
fn executor_params(wp: &BuilderPool, cache_gb: u64, cache_quantity: Quantity) -> ExecutorStsParams {
    let mut extra_env = vec![sts::env("RIO_SIZE_CLASS", &wp.spec.size_class)];
    if let Some(p) = wp.spec.fuse_passthrough {
        extra_env.push(sts::env(
            "RIO_FUSE_PASSTHROUGH",
            if p { "true" } else { "false" },
        ));
    }
    if let Some(s) = wp.spec.daemon_timeout_secs {
        extra_env.push(sts::env("RIO_DAEMON_TIMEOUT_SECS", &s.to_string()));
    }
    // r[impl ctrl.pool.bloom-knob]
    if let Some(n) = wp.spec.bloom_expected_items {
        extra_env.push(sts::env("RIO_BLOOM_EXPECTED_ITEMS", &n.to_string()));
    }

    ExecutorStsParams {
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
        host_network: wp.spec.host_network,
        host_users: wp.spec.host_users,
        tls_secret_name: wp.spec.tls_secret_name.clone(),
        termination_grace_period_seconds: wp.spec.termination_grace_period_seconds,
    }
}

/// Minimal params for label computation only. `labels()` is called
/// before cache parsing (headless Service, PDB) so it can't depend
/// on the full params. The cache fields don't affect labels anyway.
fn executor_params_for_labels(wp: &BuilderPool) -> ExecutorStsParams {
    executor_params(wp, 0, Quantity("0".into()))
}

/// Headless Service. `clusterIP: None` + no ports (workers don't
/// serve). Exists purely to satisfy StatefulSet's `serviceName`.
pub(super) fn build_headless_service(wp: &BuilderPool, oref: OwnerReference) -> Service {
    let name = sts::sts_name(&wp.name_any(), ExecutorRole::Builder);
    let labels = labels(wp);
    Service {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".into()),
            selector: Some(labels),
            ports: Some(vec![ServicePort {
                name: Some("metrics".into()),
                port: 9093,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The StatefulSet. Delegates to [`sts::build_executor_statefulset`]
/// after converting `BuilderPool` → `ExecutorStsParams`.
///
/// Returns `Err(InvalidSpec)` if `fuse_cache_size` doesn't parse
/// as a K8s Quantity.
pub(super) fn build_statefulset(
    wp: &BuilderPool,
    oref: OwnerReference,
    scheduler: &SchedulerAddrs,
    store_addr: &str,
    replicas: Option<i32>,
) -> Result<StatefulSet> {
    let cache_quantity = Quantity(wp.spec.fuse_cache_size.clone());
    let cache_gb = parse_quantity_to_gb(&wp.spec.fuse_cache_size)?;
    let params = executor_params(wp, cache_gb, cache_quantity);
    Ok(sts::build_executor_statefulset(
        &params, oref, scheduler, store_addr, replicas,
    ))
}

// r[impl ctrl.pdb.workers]
/// PodDisruptionBudget for this pool. `maxUnavailable: 1` so at most
/// one worker is evicted at a time during node drain.
pub(super) fn build_pdb(wp: &BuilderPool, oref: OwnerReference) -> PodDisruptionBudget {
    let name = format!("{}-pdb", wp.name_any());
    let labels = labels(wp);
    PodDisruptionBudget {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(PodDisruptionBudgetSpec {
            max_unavailable: Some(IntOrString::Int(1)),
            selector: Some(LabelSelector {
                match_labels: Some(labels),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The pod spec. Re-exported for `ephemeral::build_job` and
/// `manifest::build_manifest_job` — the Job pod is the same
/// executor container with env/resource tweaks.
///
/// `resources_override`: `None` reads `wp.spec.resources`
/// (STS path, ephemeral path — today's behavior). `Some(r)`
/// replaces it (manifest path — per-bucket `ResourceRequirements`
/// from `GetCapacityManifest`, ADR-020).
pub(super) fn build_pod_spec(
    wp: &BuilderPool,
    scheduler: &SchedulerAddrs,
    store_addr: &str,
    cache_gb: u64,
    cache_quantity: Quantity,
    resources_override: Option<ResourceRequirements>,
) -> PodSpec {
    let mut params = executor_params(wp, cache_gb, cache_quantity);
    if let Some(r) = resources_override {
        params.resources = Some(r);
    }
    sts::build_executor_pod_spec(&params, scheduler, store_addr)
}
