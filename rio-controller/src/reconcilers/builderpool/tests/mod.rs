//! BuilderPool reconciler test suite.
//!
//! Shared fixtures (`test_wp`, `test_pod_spec`, `test_ctx`) live
//! here and are `pub(crate)` so sibling reconcilers
//! (builderpoolset/tests) can reuse them.
//!
//! - `builders_tests` — Job pod-spec coverage + quantity parsing
//!   (pure struct-to-struct, no K8s interaction)
//! - `disruption_tests` — env-propagation via figment::Jail + the
//!   `warn_on_spec_degrades` event-reason reachability tests

use std::collections::BTreeMap;

use super::builders::*;
use super::*;
use crate::fixtures::{ApiServerVerifier, Scenario, test_sched_addrs, test_store_addrs};
use k8s_openapi::api::core::v1::PodSpec;
use rio_crds::builderpool::SeccompProfileKind;

mod builders_tests;
mod disruption_tests;
mod ephemeral_tests;
mod manifest_tests;

/// Construct a minimal BuilderPool for builder tests. No K8s
/// interaction — pure struct-to-struct.
///
/// Delegates to the shared fixture. Local wrapper kept so the
/// 39 call sites across the split test modules don't need a
/// signature change.
pub(crate) fn test_wp() -> BuilderPool {
    crate::fixtures::test_builderpool("test-pool")
}

/// Shorthand for tests: builds the Job pod spec with default
/// scheduler/store addrs. Tests that need a full Job object
/// use `build_job` directly.
pub(crate) fn test_pod_spec(wp: &BuilderPool) -> PodSpec {
    build_pod_spec(wp, &test_sched_addrs(), &test_store_addrs(), None)
}

/// Build a `Ctx` wired to the mock apiserver client. Scheduler/
/// store/admin addresses point at `127.0.0.1:1` (fails fast —
/// port 1 is never listened on). `connect_lazy` defers the TCP
/// connect until the first RPC, so `apply()` (which never calls
/// the scheduler) works; `cleanup()` treats RPC failure as
/// best-effort skip.
///
/// Shared between `apply_tests` (reconcile-loop wiring) and
/// `disruption_tests` (warn_on_spec_degrades event emission).
pub(crate) fn test_ctx(client: kube::Client) -> Arc<Ctx> {
    let recorder = kube::runtime::events::Recorder::new(
        client.clone(),
        kube::runtime::events::Reporter {
            controller: "rio-controller-test".into(),
            instance: None,
        },
    );
    let dead_ch = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
    Arc::new(Ctx {
        client,
        admin: rio_proto::AdminServiceClient::new(dead_ch),
        scheduler: rio_common::config::UpstreamAddrs {
            addr: "http://127.0.0.1:1".into(),
            ..rio_common::config::UpstreamAddrs::with_port(9001)
        },
        store: rio_common::config::UpstreamAddrs {
            addr: "http://127.0.0.1:1".into(),
            ..rio_common::config::UpstreamAddrs::with_port(9002)
        },
        recorder,
        error_counts: Default::default(),
        manifest_idle: Default::default(),
        size_class_cache: Default::default(),
        component_low_ticks: Default::default(),
        scale_down_window: std::time::Duration::from_secs(600),
    })
}
