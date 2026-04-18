//! Reconciler integration test fixtures.
//!
//! The generic scenario-driven mock apiserver lives in
//! `rio-test-support::kube_mock` (shared with rio-scheduler's
//! lease election tests). This module keeps the rio-controller-
//! specific scenario builders that know about Pool/Job/PodSpec
//! shapes.
//!
//! `#[cfg(test)]` is on the `mod fixtures;` in lib.rs — not
//! here (stable clippy flags the duplicate).

pub use rio_test_support::kube_mock::{ApiServerVerifier, Scenario};

use crate::reconcilers::pool::pod::UpstreamAddrs;
use rio_crds::pool::{ExecutorKind, Pool, PoolSpec};

/// Minimal `PoolSpec` with all CEL-required fields explicit and
/// optional fields `None`.
///
/// NEXT FIELD ADD: touch THIS fn — 1 site (down from the previous
/// 4-5 test literals each hitting E0063 on every field add).
/// CEL-exhaustiveness is the point; don't `#[derive(Default)]` on
/// `PoolSpec`.
pub fn test_pool_spec(kind: ExecutorKind) -> PoolSpec {
    PoolSpec {
        kind,
        image: "rio-builder:test".into(),
        systems: vec!["x86_64-linux".into()],
        max_concurrent: Some(10),
        node_selector: None,
        tolerations: None,
        host_users: None,
        fuse_threads: None,
        fuse_passthrough: None,
        fuse_cache_bytes: None,
        features: vec!["kvm".into()],
        image_pull_policy: None,
        termination_grace_period_seconds: None,
        privileged: None,
        seccomp_profile: None,
        host_network: None,
    }
}

/// Wrap a [`test_pool_spec`] in a `Pool` with name + UID + namespace
/// set. `controller_owner_ref` needs UID; the apiserver sets it in
/// prod, tests fake it.
pub fn test_pool(name: &str, kind: ExecutorKind) -> Pool {
    let mut p = Pool::new(name, test_pool_spec(kind));
    p.metadata.uid = Some(format!("{name}-uid"));
    p.metadata.namespace = Some("rio".into());
    p
}

/// Collect a slice of `EnvVar` into a `name → value` map for test
/// asserts. Skips entries with `value: None` (e.g. `valueFrom`
/// downward-API refs).
pub fn env_map(
    env: &[k8s_openapi::api::core::v1::EnvVar],
) -> std::collections::BTreeMap<&str, &str> {
    env.iter()
        .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
        .collect()
}

/// `controller_owner_ref` for a test CR. The fixture constructors
/// above all set `metadata.uid` so this never returns `None`.
pub fn oref<K: kube::Resource<DynamicType = ()>>(
    obj: &K,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
    obj.controller_owner_ref(&())
        .expect("test fixture missing metadata.uid — set it in the constructor")
}

/// UpstreamAddrs for builder tests.
pub fn test_sched_addrs() -> UpstreamAddrs {
    UpstreamAddrs {
        addr: "sched:9001".into(),
        balance_host: Some("sched-headless".into()),
        balance_port: 9001,
    }
}

/// Store address fixture mirroring `test_sched_addrs`.
pub fn test_store_addrs() -> UpstreamAddrs {
    UpstreamAddrs {
        addr: "store:9002".into(),
        balance_host: Some("store-headless".into()),
        balance_port: 9002,
    }
}
