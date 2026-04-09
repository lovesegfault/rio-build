//! `SchedulerGrpc` test suite — shared imports + submodule wiring.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395) to mirror
//! the `grpc/{scheduler_service,worker_service,actor_guards}.rs`
//! submodule seams (P0356). Each submodule covers one prod file:
//!   - `submit_tests` → `scheduler_service.rs` (SubmitBuild chain)
//!   - `stream_tests` → `worker_service.rs` (BuildExecution stream)
//!   - `bridge_tests` → `bridge_build_events` (replay + dedup)
//!   - `guards_tests` → `actor_guards.rs` (error-map + leader-gate)
//!
//! Submodules pull the common `use` block + setup helpers in via
//! `use super::*;` (transitive import — the P0386 pattern).

use super::*;
use crate::MIGRATOR;
use crate::actor::tests::{make_node, setup_actor};
// P0356: the trait impls moved to scheduler_service.rs / worker_service.rs.
// `use super::*` no longer pulls in `SchedulerService` / `ExecutorService` /
// `Request` as a side effect; tests call the trait methods on
// `SchedulerGrpc` directly so the traits must be in scope.
use rio_proto::SchedulerService;
use rio_test_support::TestDb;
use tonic::Request;

/// Bootstrap PG + actor + pool-less [`SchedulerGrpc`]. Absorbs the
/// `TestDb::new` → `setup_actor` → `SchedulerGrpc::new_for_tests`
/// preamble shared by every submodule. Pool-less means WatchBuild
/// replay and tenant resolution are unavailable — use
/// [`setup_grpc_with_pool`] for tests that exercise those.
///
/// Returns the [`ActorHandle`] separately for tests that drive the
/// actor directly alongside the gRPC surface (e.g. in-process server
/// setups in `stream_tests`).
pub(super) async fn setup_grpc() -> (
    TestDb,
    SchedulerGrpc,
    ActorHandle,
    tokio::task::JoinHandle<()>,
) {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle.clone());
    (db, grpc, handle, task)
}

/// Like [`setup_grpc`] but wires the PG pool into [`SchedulerGrpc`] so
/// WatchBuild replay / tenant resolution / jti revocation work.
pub(super) async fn setup_grpc_with_pool() -> (
    TestDb,
    SchedulerGrpc,
    ActorHandle,
    tokio::task::JoinHandle<()>,
) {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests_with_pool(handle.clone(), db.pool.clone());
    (db, grpc, handle, task)
}

mod bridge_tests;
mod guards_tests;
mod stream_tests;
mod submit_tests;
