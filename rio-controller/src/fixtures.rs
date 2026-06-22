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

/// Minimal `PoolSpec` for reconciler tests. Delegates to
/// [`PoolSpec::test_fixture`] in rio-crds — the single E0063
/// touch-point — and overrides only the fields the reconciler tests
/// inspect (`max_concurrent` for cap logic, `features` for the kvm
/// resource-request branch).
pub fn test_pool_spec(kind: ExecutorKind) -> PoolSpec {
    PoolSpec {
        max_concurrent: Some(10),
        features: vec!["kvm".into()],
        ..PoolSpec::test_fixture(kind)
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
