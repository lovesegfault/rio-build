//! Actor tests, split by test group.
//!
//! `helpers` is `pub(crate)` so grpc.rs tests can import `make_test_node` and
//! `setup_actor` via `crate::actor::tests::{make_test_node, setup_actor}`.

#![allow(dead_code)] // Helpers used progressively across Group 1-10 TDD tests

// Bring actor types into this module's scope so submodules see them via `use super::*;`.
pub(crate) use super::*;

pub(crate) mod helpers;
pub(crate) use helpers::*; // Re-export for grpc.rs tests

mod coverage;
mod integration;
mod wiring;
