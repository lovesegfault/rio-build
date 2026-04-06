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
//! No per-test helpers are shared across ≥2 clusters, so this file
//! holds only the common `use` block. Submodules pull it in via
//! `use super::*;` (transitive import — the P0386 pattern).

use super::*;
use crate::MIGRATOR;
use crate::actor::tests::{make_test_node, setup_actor};
// P0356: the trait impls moved to scheduler_service.rs / worker_service.rs.
// `use super::*` no longer pulls in `SchedulerService` / `WorkerService` /
// `Request` as a side effect; tests call the trait methods on
// `SchedulerGrpc` directly so the traits must be in scope.
use rio_proto::SchedulerService;
use rio_test_support::TestDb;
use tonic::Request;

mod bridge_tests;
mod guards_tests;
mod stream_tests;
mod submit_tests;
