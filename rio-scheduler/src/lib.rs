//! DAG-aware build scheduler for rio-build.
//!
//! Receives derivation build requests, analyzes the DAG, and publishes
//! work to workers via a bidirectional streaming RPC.
//!
//! ## Architecture
//!
//! The scheduler uses a single-owner actor model. All mutable state is owned
//! by a single Tokio task (the DAG actor) that processes commands from a
//! bounded mpsc channel. gRPC handlers send commands and await responses.
//!
//! ## Modules
//!
//! - [`actor`]: DAG actor (single-owner event loop, dispatch)
//! - [`dag`]: In-memory derivation graph
//! - [`state`]: Derivation and build state machines
//! - [`queue`]: FIFO ready queue
//! - [`db`]: PostgreSQL persistence (sqlx)
//! - [`grpc`]: SchedulerService + WorkerService gRPC implementations

pub mod actor;
pub mod admin;
pub mod dag;
pub mod db;
pub mod estimator;
pub mod grpc;
pub mod logs;
pub mod queue;
pub mod state;
