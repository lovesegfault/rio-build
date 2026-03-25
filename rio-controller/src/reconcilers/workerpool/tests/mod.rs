//! WorkerPool reconciler test suite.
//!
//! Shared fixtures (`test_wp`, `test_sts`, `test_ctx`) live here and
//! are `pub(crate)` so sibling reconcilers (workerpoolset/tests) can
//! reuse them. Tests mirror the
//! prod seams in `workerpool/{builders,disruption,ephemeral,mod}.rs`:
//!
//! - `builders_tests` — StatefulSet/PDB spec coverage + quantity
//!   parsing (pure struct-to-struct, no K8s interaction)
//! - `apply_tests` — mock-apiserver reconcile loop wiring (apply,
//!   cleanup, migrate_finalizer ordering + SSA params)
//! - `disruption_tests` — env-propagation via figment::Jail + the
//!   `warn_on_spec_degrades` event-reason reachability tests
//!
//! Split from the 1716L monolith (P0396) following the P0386 pattern.

use std::collections::BTreeMap;

use super::builders::*;
use super::*;
use crate::crds::workerpool::SeccompProfileKind;
use crate::fixtures::{ApiServerVerifier, Scenario, apply_ok_scenarios, test_sched_addrs};

mod apply_tests;
mod builders_tests;
mod disruption_tests;

/// Construct a minimal WorkerPool for builder tests. No K8s
/// interaction — pure struct-to-struct.
///
/// Delegates to the shared fixture. Local wrapper kept so the
/// 39 call sites across the split test modules don't need a
/// signature change.
pub(crate) fn test_wp() -> WorkerPool {
    crate::fixtures::test_workerpool("test-pool")
}

/// Shorthand for tests: builds with default scheduler/store
/// addrs and replicas=Some(min). Use `build_statefulset`
/// directly for tests that care about those params.
pub(crate) fn test_sts(wp: &WorkerPool) -> StatefulSet {
    build_statefulset(
        wp,
        wp.controller_owner_ref(&()).unwrap(),
        &test_sched_addrs(),
        "store:9002",
        Some(wp.spec.replicas.min),
    )
    .unwrap()
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
        scheduler_addr: "http://127.0.0.1:1".into(),
        store_addr: "http://127.0.0.1:1".into(),
        scheduler_balance_host: None,
        scheduler_balance_port: 9001,
        recorder,
        error_counts: Default::default(),
    })
}
