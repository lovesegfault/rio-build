use super::*;
use crate::actor::tests::{make_test_node, setup_actor};
// P0356: the trait impls moved to scheduler_service.rs / worker_service.rs.
// `use super::*` no longer pulls in `SchedulerService` / `WorkerService` /
// `Request` as a side effect; tests call the trait methods on
// `SchedulerGrpc` directly so the traits must be in scope.
use rio_proto::SchedulerService;
use rio_test_support::TestDb;
use tonic::Request;

use crate::MIGRATOR;

mod bridge_tests;
mod guards_tests;
mod stream_tests;
mod submit_tests;
