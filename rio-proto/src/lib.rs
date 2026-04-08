//! Protobuf/gRPC service definitions for the rio workspace.
//!
//! Generated tonic stubs for `StoreService`, `SchedulerService`,
//! `ExecutorService`, `ChunkService`, and `AdminService`, plus
//! [`ValidatedPathInfo`](validated::ValidatedPathInfo) for proto→domain
//! validation and [`interceptor`] for W3C traceparent propagation.

// `x-rio-*` header constants live in `rio-common::grpc` (proto-agnostic).
// Re-exported here so existing `rio_proto::BUILD_ID_HEADER` paths keep
// resolving across the workspace.
pub use rio_common::grpc::{ASSIGNMENT_TOKEN_HEADER, BUILD_ID_HEADER, TRACE_ID_HEADER};

pub mod client;
pub mod interceptor;
pub mod validated;

/// Shared protobuf types (messages, enums) used across all services.
///
/// The underlying `.proto` definitions are spread across `types.proto`
/// (shared primitives: store, chunk, GC, ResourceUsage, BuildResultStatus),
/// `dag.proto` (DAG + derivation events + GraphNode/Edge),
/// `build_types.proto` (build lifecycle, executor stream, heartbeat), and
/// `admin_types.proto` (admin RPC data types). All four share
/// `package rio.types;`, so prost merges them into ONE module here.
pub mod types {
    tonic::include_proto!("rio.types");
}

/// Scheduler service: gateway-facing RPCs (SubmitBuild, WatchBuild, etc.).
pub mod scheduler {
    tonic::include_proto!("rio.scheduler");
}

/// Executor service: builder/fetcher-facing RPCs (BuildExecution, Heartbeat).
pub mod builder {
    tonic::include_proto!("rio.builder");
}

/// Store service: NAR storage and path metadata RPCs.
///
/// Also includes `ChunkService` (`GetChunk` only). Chunking is
/// **server-side**: executors stream full NARs via `StoreService::
/// PutPath`; rio-store does the FastCDC cut internally and dedupes
/// chunks via the `chunks` table refcount. The builder fans out
/// `GetChunk` to reassemble NARs from their manifests.
///
/// `ChunkServiceClient` is **not** re-exported at crate root: the
/// only production caller (the builder's reassembly loop) reaches it
/// via the deep codegen path
/// `store::chunk_service_client::ChunkServiceClient`.
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
pub use builder::executor_service_client::ExecutorServiceClient;
pub use builder::executor_service_server::{ExecutorService, ExecutorServiceServer};
pub use scheduler::scheduler_service_client::SchedulerServiceClient;
pub use scheduler::scheduler_service_server::{SchedulerService, SchedulerServiceServer};
pub use store::chunk_service_server::{ChunkService, ChunkServiceServer};
pub use store::store_admin_service_client::StoreAdminServiceClient;
pub use store::store_admin_service_server::{StoreAdminService, StoreAdminServiceServer};
pub use store::store_service_client::StoreServiceClient;
pub use store::store_service_server::{StoreService, StoreServiceServer};
