//! Pool reconciler test suite.
//!
//! Shared fixtures (`test_wp`, `test_pod_spec`, `test_ctx`) live
//! here and are `pub(crate)` so sibling modules can reuse them.
//!
//! - `builders_tests` — Job pod-spec coverage + quantity parsing
//!   (pure struct-to-struct, no K8s interaction)
//! - `disruption_tests` — env-propagation via rio_test_support::Jail + the
//!   `warn_on_spec_degrades` event-reason reachability tests

use super::*;
use crate::fixtures::{ApiServerVerifier, Scenario, test_sched_addrs, test_store_addrs};
use k8s_openapi::api::core::v1::{Pod, PodSpec};
use rio_crds::pool::SeccompProfileKind;

mod builders_tests;
mod disruption_tests;
mod jobs_tests;

/// Construct a minimal builder Pool for tests. No K8s
/// interaction — pure struct-to-struct.
///
/// Delegates to the shared fixture. Local wrapper kept so the
/// 39 call sites across the split test modules don't need a
/// signature change.
pub(crate) fn test_wp() -> Pool {
    crate::fixtures::test_pool("test-pool", ExecutorKind::Builder)
}

/// Shorthand for tests: builds the Job pod spec with default
/// scheduler/store addrs and an empty `HwClassConfig` (`wants_metal`
/// falls back to the literal `kvm` feature).
pub(crate) fn test_pod_spec(pool: &Pool) -> PodSpec {
    pod::build_executor_pod_spec(
        pool,
        &test_sched_addrs(),
        &test_store_addrs(),
        &crate::reconcilers::node_informer::HwClassConfig::default(),
    )
}

/// Build a `Ctx` wired to the mock apiserver client.
pub(crate) fn test_ctx(client: kube::Client) -> Arc<Ctx> {
    test_ctx_with_admin(client, rio_test_support::grpc::dead_channel())
}

/// [`test_ctx`] with an explicit admin-side channel — the
/// synthesize-on-delete tests point this at an in-process
/// `MockAdmin` so the `ReportAttemptOutcome` ack path is reachable;
/// everything else uses the dead channel (admin RPCs fail fast).
pub(crate) fn test_ctx_with_admin(
    client: kube::Client,
    admin_channel: tonic::transport::Channel,
) -> Arc<Ctx> {
    let recorder = kube::runtime::events::Recorder::new(
        client.clone(),
        kube::runtime::events::Reporter {
            controller: "rio-controller-test".into(),
            instance: None,
        },
    );
    Arc::new(Ctx {
        client,
        admin: rio_proto::AdminServiceClient::with_interceptor(
            admin_channel,
            crate::reconcilers::fence::GenerationStamp::new(
                rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller"),
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ),
        ),
        scheduler: rio_common::config::UpstreamAddrs {
            addr: "http://127.0.0.1:1".into(),
            ..rio_common::config::UpstreamAddrs::with_port(9001)
        },
        store: rio_common::config::UpstreamAddrs {
            addr: "http://127.0.0.1:1".into(),
            ..rio_common::config::UpstreamAddrs::with_port(9002)
        },
        recorder,
        service_interceptor: rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller"),
        error_counts: Default::default(),
        scaler: Default::default(),
        hw_bench_mem_floor: 8 * (1 << 30),
        placeable: Some(crate::reconcilers::nodeclaim_pool::PlaceableGate::unarmed()),
        // r40 bug_018: matches `NodeClaimPoolConfig::default()` so test
        // pools route through the second scheduler unless a test
        // explicitly disables it.
        kube_build_scheduler_enabled: true,
        hw_config: crate::reconcilers::node_informer::HwClassConfig::default(),
        terminal_report_sampled: Default::default(),
        exhausted_streak: Default::default(),
    })
}

// r[verify ctrl.pool.fetcher-hardening+3]
/// D3 belt-and-suspenders behind the CEL admission gate: a
/// `Pool{kind=Fetcher}` whose spec slips past CEL with
/// `seccompProfile: Unconfined` and `hostUsers: true` STILL
/// renders the ADR-019 hardening — the pod-spec builder is
/// authoritative regardless of spec.
#[test]
fn fetcher_hardening_ignores_spec() {
    let mut pool = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
    pool.spec.seccomp_profile = Some(SeccompProfileKind {
        type_: "Unconfined".into(),
        localhost_profile: None,
    });
    pool.spec.host_users = Some(true);
    pool.spec.privileged = Some(true);
    pool.spec.features = vec!["kvm".into()];

    let spec = test_pod_spec(&pool);
    let sc = spec.containers[0].security_context.as_ref().unwrap();
    assert_eq!(
        sc.read_only_root_filesystem,
        Some(true),
        "rootfs tampering blocked"
    );
    assert_ne!(sc.privileged, Some(true), "fetchers never privileged");
    assert_eq!(
        spec.host_users,
        Some(true),
        "spec hostUsers:true honored (k3s escape hatch)"
    );
    let env = spec.containers[0].env.as_ref().unwrap();
    // §13e: FODs derive `[fetcher]` from kind, ignoring spec.features.
    assert_eq!(
        env.iter()
            .find(|e| e.name == "RIO_FEATURES")
            .and_then(|e| e.value.as_deref()),
        Some(rio_common::k8s::FETCHER_FEATURE),
        "FODs derive [fetcher] from kind, ignoring spec.features (§13e)"
    );
    let cp = sc.seccomp_profile.as_ref().unwrap();
    assert_eq!(cp.type_, "Localhost");
    assert_eq!(
        cp.localhost_profile.as_deref(),
        Some("operator/rio-fetcher.json"),
        "spec seccomp ignored — Localhost rio-fetcher.json forced"
    );
    assert_eq!(
        spec.service_account_name.as_deref(),
        Some("rio-fetcher"),
        "role-SA wired (rbac.yaml renders it unconditionally)"
    );
    assert!(
        !spec
            .node_selector
            .as_ref()
            .is_some_and(|ns| ns.contains_key("rio.build/kvm")),
        "fetchers never want kvm even if spec.features lists it"
    );

    // Unset spec → ADR-019 default Some(false). Production EKS path.
    pool.spec.host_users = None;
    assert_eq!(
        test_pod_spec(&pool).host_users,
        Some(false),
        "Fetcher defaults hostUsers:false when spec is silent"
    );

    // §13e B4: the legacy `rio.build/node-role` pool-static nodeSelector
    // is DELETED, but `effective_node_selector` RESTORES a pool-static
    // selector keyed on `FETCHER_TAINT_KEY` (`rio.build/fetcher`) — the
    // last-resort restrictive constraint for builtin FODs whose
    // `hw_class_names=[]` carries no per-intent affinity. The pool-static
    // toleration stays (permissive, cold-start fallback).
    pool.spec.node_selector = None;
    pool.spec.tolerations = None;
    let spec = test_pod_spec(&pool);
    assert!(
        spec.node_selector
            .as_ref()
            .is_none_or(|ns| !ns.contains_key("rio.build/node-role")),
        "deleted `rio.build/node-role` pool-static fetcher nodeSelector \
         must not reappear post-§13e (got {:?})",
        spec.node_selector
    );
    let tol = &spec.tolerations.as_ref().unwrap()[0];
    assert_eq!(tol.key.as_deref(), Some(rio_common::k8s::FETCHER_TAINT_KEY));
}

/// D3a: `app.kubernetes.io/component` label is `rio-{kind}` so the
/// cluster-wide network policies select on it ns-agnostically.
#[test]
fn labels_include_component_for_ccnp() {
    let b = pod::executor_labels(&crate::fixtures::test_pool("b", ExecutorKind::Builder));
    assert_eq!(
        b.get("app.kubernetes.io/component"),
        Some(&"rio-builder".into())
    );
    assert_eq!(b.get("rio.build/role"), Some(&"builder".into()));

    let f = pod::executor_labels(&crate::fixtures::test_pool("f", ExecutorKind::Fetcher));
    assert_eq!(
        f.get("app.kubernetes.io/component"),
        Some(&"rio-fetcher".into())
    );
    assert_eq!(f.get("rio.build/pool"), Some(&"f".into()));
}
