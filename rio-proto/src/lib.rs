/// Default max gRPC message size: 32 MB.
///
/// A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes (~12MB serialized).
/// Configurable at runtime via `RIO_GRPC__MAX_MESSAGE_SIZE` environment variable.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 32 * 1024 * 1024;

/// Read the max message size from the `RIO_GRPC__MAX_MESSAGE_SIZE` environment
/// variable, falling back to [`DEFAULT_MAX_MESSAGE_SIZE`] if not set or invalid.
pub fn max_message_size() -> usize {
    std::env::var("RIO_GRPC__MAX_MESSAGE_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_MESSAGE_SIZE)
}

pub mod client;

// TODO(phase2b): add validated.rs with newtype wrappers around generated proto
// types. Currently PathInfo.nar_hash: Vec<u8> and store_path: String propagate
// unvalidated through the whole system, with checks only at the gRPC ingress.
// NarinfoRow::into_path_info (DB egress) has no validation at all. A
// ValidatedPathInfo { store_path: StorePath, nar_hash: [u8; 32], ... } with
// TryFrom<PathInfo> would centralize validation.

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
/// Also includes ChunkService (stub UNIMPLEMENTED in Phase 2a).
pub mod store {
    tonic::include_proto!("rio.store");
}

/// Admin service: dashboard and CLI RPCs.
pub mod admin {
    tonic::include_proto!("rio.admin");
}
