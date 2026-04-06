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

/// gRPC initial-metadata key carrying the scheduler-assigned build_id
/// on `SubmitBuild` responses. Server-streaming RPCs send initial
/// metadata (headers) BEFORE any stream message, so the client has
/// `build_id` even if the stream delivers zero events (scheduler
/// SIGTERM between MergeDag commit and first BuildEvent send).
///
/// Value: UUID v7 stringified (always ASCII, always a valid
/// `MetadataValue<Ascii>`).
///
/// Introduced phase4a (remediation 20). Absent header = legacy
/// scheduler; callers fall back to first-event peek.
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
/// so operators grep the trace that actually spans scheduler→worker (via
/// the `WorkAssignment.traceparent` data-carry). The gateway's own
/// trace_id only reaches gateway spans.
///
/// Value: 32 lowercase-hex characters (128-bit W3C trace_id). Always
/// ASCII. Empty/absent → legacy scheduler; gateway falls back to its
/// own `current_trace_id_hex()`.
pub const TRACE_ID_HEADER: &str = "x-rio-trace-id";

/// gRPC metadata key for HMAC-signed assignment tokens.
///
/// Scheduler signs at dispatch (worker_id + drv_hash + expiry);
/// store verifies on PutPath to gate which worker can upload which
/// path. See rio-common::hmac for the token format. Value is
/// base64-encoded bytes (always ASCII).
pub const ASSIGNMENT_TOKEN_HEADER: &str = "x-rio-assignment-token";

/// Read the max message size from the `RIO_GRPC_MAX_MESSAGE_SIZE` environment
/// variable, falling back to [`DEFAULT_MAX_MESSAGE_SIZE`] if not set or invalid.
///
/// Single underscore (not `__`): this is a direct env read, not figment.
/// The double underscore is figment's nesting separator — misleading here.
pub fn max_message_size() -> usize {
    match std::env::var("RIO_GRPC_MAX_MESSAGE_SIZE") {
        Ok(val) => match val.parse::<usize>() {
            Ok(size) => size,
            Err(_) => {
                // Direct env read, pre-tracing-init — eprintln not warn!.
                eprintln!(
                    "warning: invalid RIO_GRPC_MAX_MESSAGE_SIZE={val:?}, expected bytes as a positive integer; defaulting to {DEFAULT_MAX_MESSAGE_SIZE}"
                );
                DEFAULT_MAX_MESSAGE_SIZE
            }
        },
        Err(_) => DEFAULT_MAX_MESSAGE_SIZE,
    }
}

pub mod client;
pub mod interceptor;
pub mod validated;

/// Shared protobuf types (messages, enums) used across all services.
///
/// P0376 domain split: the underlying `.proto` definitions are spread across
/// `types.proto` (shared primitives: store, chunk, GC, bloom, ResourceUsage,
/// BuildResultStatus), `dag.proto` (DAG + derivation events + GraphNode/Edge),
/// `build_types.proto` (build lifecycle, worker stream, heartbeat), and
/// `admin_types.proto` (admin RPC data types). All four share
/// `package rio.types;`, so prost merges them into ONE module here. The
/// file-level split is for plan-DAG collision tracking; this Rust module is
/// the single flattened namespace.
pub mod types {
    tonic::include_proto!("rio.types");
}

/// Re-export of DAG-domain types from [`types`]. Sourced from
/// `proto/dag.proto`. Callers MAY use either `rio_proto::types::DerivationNode`
/// or `rio_proto::dag::DerivationNode` — both resolve to the same struct.
/// The domain-scoped path is encouraged for new code (makes file-level
/// collision tracking in plan docs meaningful).
pub mod dag {
    pub use crate::types::{
        DerivationCached, DerivationCompleted, DerivationEdge, DerivationEvent, DerivationFailed,
        DerivationNode, DerivationQueued, DerivationStarted, GetBuildGraphRequest,
        GetBuildGraphResponse, GraphEdge, GraphNode, derivation_event,
    };
}

/// Re-export of build-lifecycle-domain types from [`types`]. Sourced
/// from `proto/build_types.proto`. Same dual-path semantics as [`dag`].
///
/// `dag::` migration is complete (zero `types::Derivation*` refs
/// remain). `build_types::` migration is opportunistic — existing
/// `types::` paths are valid, new code SHOULD use `build_types::`.
pub mod build_types {
    pub use crate::types::{
        BuildCancelled, BuildCompleted, BuildEvent, BuildFailed, BuildInputsResolved,
        BuildLogBatch, BuildOptions, BuildProgress, BuildResult, BuildResultStatus, BuildStarted,
        BuildState, BuildStatus, BuiltOutput, CancelBuildRequest, CancelBuildResponse,
        CancelSignal, CompletionReport, HeartbeatRequest, HeartbeatResponse, PrefetchComplete,
        PrefetchHint, ProgressUpdate, QueryBuildRequest, SchedulerMessage, SubmitBuildRequest,
        WatchBuildRequest, WorkAssignment, WorkAssignmentAck, WorkerMessage, WorkerRegister,
        build_event, scheduler_message, worker_message,
    };
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
///
/// Also includes `ChunkService` (`GetChunk`/`FindMissingChunks`/
/// `PutChunk`). Chunking is **server-side only** — workers stream
/// full NARs via `StoreService::PutPath`; rio-store does the FastCDC
/// cut internally. `GetChunk`/`FindMissingChunks` exist so the store
/// can dedupe across tenants without clients knowing chunk
/// boundaries. `PutChunk` is stubbed `UNIMPLEMENTED`.
///
/// `ChunkServiceClient` is **not** re-exported at crate root (P0430):
/// client-side chunking was descoped in favor of P0434's
/// manifest-mode (`StoreService::PutPathManifest`), where the worker
/// sends a manifest and the store resolves chunks server-side. The
/// store's own tests reach the client stub via the deep codegen path
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
pub use scheduler::scheduler_service_client::SchedulerServiceClient;
pub use scheduler::scheduler_service_server::{SchedulerService, SchedulerServiceServer};
pub use store::chunk_service_server::{ChunkService, ChunkServiceServer};
pub use store::store_admin_service_client::StoreAdminServiceClient;
pub use store::store_admin_service_server::{StoreAdminService, StoreAdminServiceServer};
pub use store::store_service_client::StoreServiceClient;
pub use store::store_service_server::{StoreService, StoreServiceServer};
pub use worker::worker_service_client::WorkerServiceClient;
pub use worker::worker_service_server::{WorkerService, WorkerServiceServer};
