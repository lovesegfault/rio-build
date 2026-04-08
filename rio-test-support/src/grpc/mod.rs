//! Mock gRPC services and server spawn helpers for tests.
//!
//! [`MockStore`] stores NAR bytes in-memory, records PutPath calls, and
//! supports prefix-match QueryPathInfo (for hash-part lookups). Its
//! fields are grouped into [`MockStoreState`] (in-memory data),
//! [`MockStoreCalls`] (call recorders for assertions), and
//! [`MockStoreFaults`] (fault injection knobs).
//!
//! [`MockScheduler`] has a configurable [`MockSchedulerOutcome`] and
//! records SubmitBuild + CancelBuild calls.
//!
//! [`MockAdmin`] returns empty-but-valid responses for all unary RPCs;
//! streaming RPCs return a single terminal message so client drain loops
//! exit cleanly.

mod admin;
mod scheduler;
mod spawn;
mod store;

pub use admin::{MockAdmin, spawn_mock_admin};
pub use scheduler::{MockScheduler, MockSchedulerOutcome, spawn_mock_scheduler};
pub use spawn::{
    spawn_grpc_server, spawn_grpc_server_layered, spawn_mock_store, spawn_mock_store_inproc,
    spawn_mock_store_with_client,
};
pub use store::{MockStore, MockStoreCalls, MockStoreFaults, MockStoreState};
