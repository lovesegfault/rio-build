//! Protobuf/gRPC service definitions for the rio workspace.
//!
//! Generated tonic stubs for `StoreService`, `SchedulerService`,
//! `WorkerService`, `ChunkService`, and `AdminService`, plus
//! [`ValidatedPathInfo`](validated::ValidatedPathInfo) for proto→domain
//! validation and [`interceptor`] for W3C traceparent propagation.

/// Default max gRPC message size: 32 MB.
///
/// A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes (~12MB serialized).
/// Configurable at runtime via `RIO_GRPC_MAX_MESSAGE_SIZE` environment variable.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 32 * 1024 * 1024;

/// Read the max message size from the `RIO_GRPC_MAX_MESSAGE_SIZE` environment
/// variable, falling back to [`DEFAULT_MAX_MESSAGE_SIZE`] if not set or invalid.
///
/// Single underscore (not `__`): this is a direct env read, not figment.
/// The double underscore is figment's nesting separator — misleading here.
pub fn max_message_size() -> usize {
    std::env::var("RIO_GRPC_MAX_MESSAGE_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_MESSAGE_SIZE)
}

pub mod client;
pub mod interceptor;
pub mod validated;

/// Shared protobuf types (messages, enums) used across all services.
pub mod types {
    tonic::include_proto!("rio.types");
}

/// Scheduler service: gateway-facing RPCs (SubmitBuild, WatchBuild, etc.).
pub mod scheduler {
    tonic::include_proto!("rio.scheduler");
}

/// Worker service: worker-facing RPCs (BuildExecution, Heartbeat).
pub mod worker {
    tonic::include_proto!("rio.worker");
}

/// Store service: NAR storage and path metadata RPCs.
/// Also includes ChunkService (GetChunk/FindMissingChunks; PutChunk deferred to phase3a).
pub mod store {
    tonic::include_proto!("rio.store");
}

/// Admin service: dashboard and CLI RPCs.
pub mod admin {
    tonic::include_proto!("rio.admin");
}

// ---------------------------------------------------------------------------
// Flattened service re-exports
//
// tonic-build emits `store::store_service_client::StoreServiceClient` —
// deep nesting that's an artifact of codegen, not design. Flatten to
// crate root so callers write `rio_proto::StoreServiceClient` instead
// of `rio_proto::store::store_service_client::StoreServiceClient`.
// ---------------------------------------------------------------------------

pub use admin::admin_service_client::AdminServiceClient;
pub use admin::admin_service_server::{AdminService, AdminServiceServer};
pub use scheduler::scheduler_service_client::SchedulerServiceClient;
pub use scheduler::scheduler_service_server::{SchedulerService, SchedulerServiceServer};
pub use store::chunk_service_client::ChunkServiceClient;
pub use store::chunk_service_server::{ChunkService, ChunkServiceServer};
pub use store::store_service_client::StoreServiceClient;
pub use store::store_service_server::{StoreService, StoreServiceServer};
pub use worker::worker_service_client::WorkerServiceClient;
pub use worker::worker_service_server::{WorkerService, WorkerServiceServer};
