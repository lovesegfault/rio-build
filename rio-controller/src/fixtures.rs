//! Reconciler integration test fixtures.
//!
//! The generic scenario-driven mock apiserver lives in
//! `rio-test-support::kube_mock` (shared with rio-scheduler's
//! lease election tests). This module keeps the rio-controller-
//! specific scenario builders that know about BuilderPool/
//! StatefulSet/Service/PDB shapes.
//!
//! This tests the WIRING, not the business logic — that's what
//! the pure unit tests on `build_statefulset` etc are for. Here
//! we prove: apply() patches Service then StatefulSet then
//! BuilderPool/status, in that order, with server-side-apply
//! params. Get the order wrong → test fails.
//!
//! # Why not mock the WHOLE thing (scheduler too)
//!
//! Reconcile's K8s calls are the meat — that's what finalizer()
//! and server-side apply make tricky. The scheduler calls in
//! cleanup() are one RPC (DrainExecutor). A MockScheduler is a
//! separate thing (rio-test-support already has one, but not
//! wired for AdminService). Scope: K8s mocks only. Scheduler
//! integration is what vm-phase3a is for.
//!
//! `#[cfg(test)]` is on the `mod fixtures;` in lib.rs — not
//! here (stable clippy flags the duplicate).

pub use rio_test_support::kube_mock::{ApiServerVerifier, Scenario};

use crate::crds::builderpool::{BuilderPool, BuilderPoolSpec, Sizing};
use crate::reconcilers::common::pod::{SchedulerAddrs, StoreAddrs};

/// Minimal BuilderPoolSpec with all CEL-required fields explicit
/// and optional fields `None`. Used by [`test_builderpool`] and
/// directly by tests that need to mutate a field before wrapping
/// in a `BuilderPool`.
///
/// NEXT FIELD ADD: touch THIS fn + the production literal at
/// `reconcilers/builderpoolset/builders.rs::build_child_builderpool`
/// — 2 sites (down from the previous 4-5 test literals each
/// hitting E0063 on every field add). CEL-exhaustiveness is the
/// point; don't `#[derive(Default)]` on `BuilderPoolSpec`.
pub fn test_builderpool_spec() -> BuilderPoolSpec {
    BuilderPoolSpec {
        max_concurrent: 10,
        sizing: Sizing::Static,
        deadline_seconds: None,
        size_class_cutoff_secs: None,
        resources: None,
        fuse_cache_size: "50Gi".into(),
        fuse_threads: None,
        fuse_passthrough: None,
        daemon_timeout_secs: None,
        features: vec!["kvm".into()],
        systems: vec!["x86_64-linux".into()],
        size_class: "small".into(),
        image: "rio-builder:test".into(),
        image_pull_policy: None,
        node_selector: None,
        tolerations: None,
        termination_grace_period_seconds: None,
        privileged: None,
        seccomp_profile: None,
        host_network: None,
        host_users: None,
        tls_secret_name: None,
        topology_spread: None,
    }
}

/// Wrap a [`test_builderpool_spec`] in a `BuilderPool` with name +
/// UID + namespace set. `controller_owner_ref` needs UID; the
/// apiserver sets it in prod, tests fake it.
pub fn test_builderpool(name: &str) -> BuilderPool {
    let mut wp = BuilderPool::new(name, test_builderpool_spec());
    wp.metadata.uid = Some(format!("{name}-uid"));
    wp.metadata.namespace = Some("rio".into());
    wp
}

/// SchedulerAddrs for builder tests. Dedup of the previous
/// `test_sched_addrs` / `test_sched` local helpers in
/// `reconcilers/builderpool/{tests,ephemeral}.rs`.
pub fn test_sched_addrs() -> SchedulerAddrs {
    SchedulerAddrs {
        addr: "sched:9001".into(),
        balance_host: Some("sched-headless".into()),
        balance_port: 9001,
    }
}

/// Store address fixture mirroring `test_sched_addrs`.
pub fn test_store_addrs() -> StoreAddrs {
    StoreAddrs {
        addr: "store:9002".into(),
        balance_host: Some("store-headless".into()),
        balance_port: 9002,
    }
}
