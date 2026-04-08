//! Protobuf/gRPC service definitions for the rio workspace.
//!
//! Generated tonic stubs for `StoreService`, `SchedulerService`,
//! `ExecutorService`, `ChunkService`, and `AdminService`, plus
//! [`ValidatedPathInfo`](validated::ValidatedPathInfo) for proto→domain
//! validation and [`interceptor`] for W3C traceparent propagation.

/// gRPC initial-metadata key carrying the scheduler-assigned build_id
/// on `SubmitBuild` responses. Server-streaming RPCs send initial
/// metadata (headers) BEFORE any stream message, so the client has
/// `build_id` even if the stream delivers zero events (scheduler
/// SIGTERM between MergeDag commit and first BuildEvent send).
///
/// Value: UUID v7 stringified (always ASCII, always a valid
/// `MetadataValue<Ascii>`). Always set by the scheduler.
pub const BUILD_ID_HEADER: &str = "x-rio-build-id";

/// gRPC initial-metadata key carrying the scheduler handler span's
/// trace_id on `SubmitBuild` responses.
///
/// Set by the scheduler AFTER `link_parent()` so it reflects the actual
/// trace the handler is in — which, due to the `#[instrument]` +
/// `set_parent` ordering, is a NEW trace LINKED to the gateway's, not a
/// child of it. Jaeger shows two traces connected by an OTel span link.
///
/// The gateway emits THIS id in `STDERR_NEXT` (`rio trace_id: <32-hex>`)
/// so operators grep the trace that actually spans scheduler→builder (via
/// the `WorkAssignment.traceparent` data-carry). The gateway's own
/// trace_id only reaches gateway spans.
///
/// Value: 32 lowercase-hex characters (128-bit W3C trace_id). Always
/// ASCII. Empty/absent → legacy scheduler; gateway falls back to its
/// own `current_trace_id_hex()`.
pub const TRACE_ID_HEADER: &str = "x-rio-trace-id";

/// gRPC metadata key for HMAC-signed assignment tokens.
///
/// Scheduler signs at dispatch (executor_id + drv_hash + expiry);
/// store verifies on PutPath to gate which executor can upload which
/// path. See rio-common::hmac for the token format. Value is
/// base64-encoded bytes (always ASCII).
pub const ASSIGNMENT_TOKEN_HEADER: &str = "x-rio-assignment-token";

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

/// Re-export of DAG-domain types from [`types`]. Sourced from
/// `proto/dag.proto`. Callers MAY use either `rio_proto::types::DerivationNode`
/// or `rio_proto::dag::DerivationNode` — both resolve to the same struct.
pub mod dag {
    pub use crate::types::{
        DerivationCached, DerivationCompleted, DerivationEdge, DerivationEvent, DerivationFailed,
        DerivationNode, DerivationQueued, DerivationStarted, GetBuildGraphRequest,
        GetBuildGraphResponse, GraphEdge, GraphNode, derivation_event,
    };
}

/// Re-export of build-lifecycle-domain types from [`types`]. Sourced
/// from `proto/build_types.proto`. Same dual-path semantics as [`dag`].
pub mod build_types {
    pub use crate::types::{
        BuildCancelled, BuildCompleted, BuildEvent, BuildFailed, BuildInputsResolved,
        BuildLogBatch, BuildOptions, BuildProgress, BuildResult, BuildResultStatus, BuildStarted,
        BuildState, BuildStatus, BuiltOutput, CancelBuildRequest, CancelBuildResponse,
        CancelSignal, CompletionReport, ExecutorKind, ExecutorMessage, ExecutorRegister,
        HeartbeatRequest, HeartbeatResponse, PrefetchComplete, PrefetchHint, ProgressUpdate,
        QueryBuildRequest, SchedulerMessage, SubmitBuildRequest, WatchBuildRequest, WorkAssignment,
        WorkAssignmentAck, build_event, executor_message, scheduler_message,
    };
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
