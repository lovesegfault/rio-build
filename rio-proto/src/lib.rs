//! Protobuf/gRPC service definitions for the rio workspace.
//!
//! Generated tonic stubs for `StoreService`, `SchedulerService`,
//! `ExecutorService`, `ChunkService`, and `AdminService`, plus
//! [`ValidatedPathInfo`](validated::ValidatedPathInfo) for proto→domain
//! validation and [`interceptor`] for W3C traceparent propagation.

// `x-rio-*` header constants live in `rio-common::grpc` (proto-agnostic).
// Re-exported here so callers across the workspace reference one path
// (`rio_proto::BUILD_ID_HEADER`) regardless of which crate defines them.
// r[impl proto.metadata.build-id]
// r[impl proto.metadata.assignment-token]
// r[impl proto.metadata.tenant-token]
pub use rio_common::grpc::{
    ASSIGNMENT_TOKEN_HEADER, BUILD_ID_HEADER, EXECUTOR_TOKEN_HEADER, PROBE_TENANT_ID_HEADER,
    SERVICE_TOKEN_HEADER, TENANT_TOKEN_HEADER, TRACE_ID_HEADER,
};

/// Substring carried in the `Status::aborted` message when PutPath /
/// PutPathBatch find a live `'uploading'` placeholder for the same
/// store path. This is a wire-protocol contract: rio-builder
/// (`is_concurrent_put_path`, I-125b wait-then-adopt) and
/// rio-scheduler (`exempt_from_cap`, I-127 infra-retry exemption) both
/// `.contains()`-match it. Single source of truth — both store emit
/// sites and both consumer match sites use this constant so the string
/// can't drift again.
pub const CONCURRENT_PUTPATH_MSG: &str = "concurrent PutPath in progress";

/// Substring carried in `ExecutorError::CgroupOom`'s Display impl and
/// matched by rio-scheduler's `handle_infrastructure_failure` to trigger
/// `r[sched.sla.reactive-floor]`. Single source of truth so the
/// `#[error]` attr and the `.contains()` consumer can't drift. The
/// `cgroup_oom_display_contains_proto_constant` test in rio-builder
/// pins the Display side; the 8 scheduler test fixtures reference this
/// constant so a rename forces all sites to update in lockstep.
pub const CGROUP_OOM_MSG: &str = "cgroup OOM during build";

/// The scheduler's build-level first-failure summary, recorded as
/// `error_summary` by `handle_derivation_failure`
/// (rio-scheduler/src/actor/completion.rs) and carried verbatim into the
/// `BuildFailed` event's `error_message`.
///
/// This is a wire-format contract with two producers and a text-shape
/// consumer, kept as one function so none of them can drift again:
///
/// - **Content**: `drv_key` is the scheduler DAG's node key
///   (`DrvHash`). The gateway mints it as the FULL derivation store path
///   (`/nix/store/{hash}-{name}.drv`) — `build_node` in
///   rio-gateway/src/translate.rs sets `drv_hash: drv_path` — and the
///   scheduler ingest carries it verbatim into the DAG, so the
///   interpolated token is a store path, NOT the bare `{hash}-{name}.drv`
///   basename the field name suggests.
/// - **Format**: this function is the only producer of the summary text.
/// - **Consumer**: the gateway's per-root DAG fallback relays the summary
///   byte-for-byte onto roots without a recorded terminal of their own,
///   and the replay engine's blanket detector
///   (`is_dag_fallback_blanket` in rio-replay/src/run/collect.rs) parses
///   exactly this shape to recover cascade triggers from outside the row.
///   Detector fixtures MUST be built through this function — a
///   hand-written consumer-side fixture once encoded a producer shape
///   production never emits and certified a dead recovery arm green.
pub fn dag_first_failure_summary(drv_key: &str) -> String {
    format!("derivation {drv_key} failed")
}

pub mod client;
pub mod interceptor;
// Trait impls (`From<NixStatus> for BuildResultStatus` and inverse) are
// visible globally regardless of module visibility — `pub` would only
// add an empty namespace to the docs.
mod status;
pub mod validated;

/// Shared protobuf types (messages, enums) used across all services.
///
/// The underlying `.proto` definitions are spread across `types.proto`
/// (shared primitives: store, chunk, GC, ResourceUsage, BuildResultStatus),
/// `dag.proto` (DAG + derivation events + GraphNode/Edge),
/// `build_types.proto` (build lifecycle, executor stream, heartbeat), and
/// `admin_types.proto` (admin RPC data types). All four share
/// `package rio.types;`, so prost merges them into ONE module here.
// r[impl proto.executor.kind]
// r[impl proto.heartbeat.capability-fields]
// (Tracey doesn't scan .proto — the documentary annotations live in
// build_types.proto next to the wire definitions; these markers are the
// scannable anchors at the Rust point-of-generation.)
pub mod types {
    tonic::include_proto!("rio.types");
}

/// `DerivationEvent` helper ctors. The proto is flat (`kind` enum +
/// per-kind payload fields) rather than a oneof of five near-empty
/// messages; these restore the one-ctor-per-variant ergonomics at the
/// 5 scheduler emit sites and keep "which fields are valid for which
/// kind" documented in one place.
impl types::DerivationEvent {
    pub fn started(derivation_path: String, executor_id: String) -> Self {
        Self {
            derivation_path,
            kind: types::DerivationEventKind::Started as i32,
            executor_id,
            ..Default::default()
        }
    }
    pub fn completed(derivation_path: String, output_paths: Vec<String>) -> Self {
        Self {
            derivation_path,
            kind: types::DerivationEventKind::Completed as i32,
            output_paths,
            ..Default::default()
        }
    }
    pub fn cached(derivation_path: String, output_paths: Vec<String>) -> Self {
        Self {
            derivation_path,
            kind: types::DerivationEventKind::Cached as i32,
            output_paths,
            ..Default::default()
        }
    }
    pub fn substituting(derivation_path: String, output_paths: Vec<String>) -> Self {
        Self {
            derivation_path,
            kind: types::DerivationEventKind::Substituting as i32,
            output_paths,
            ..Default::default()
        }
    }
    pub fn failed(
        derivation_path: String,
        error_message: String,
        status: types::BuildResultStatus,
    ) -> Self {
        Self {
            derivation_path,
            kind: types::DerivationEventKind::Failed as i32,
            error_message,
            failure_status: status as i32,
            ..Default::default()
        }
    }
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
/// chunks via the `chunks` table refcount. Reassembly is also
/// server-side — `StoreService::GetPath` streams whole NARs back, so
/// `GetChunk` has no production caller today; it is registered and
/// served as the chunk-level retrieval surface for future
/// out-of-process reassembly (and exercised by tests against the
/// `ChunkServiceImpl` cache wiring).
///
/// `ChunkServiceClient` is **not** re-exported at crate root: with no
/// production caller, only tests reach it, via the deep codegen path
/// `store::chunk_service_client::ChunkServiceClient`.
// r[impl proto.store.batch-rpc]
pub mod store {
    tonic::include_proto!("rio.store");
}

/// Admin service: dashboard and CLI RPCs.
// r[impl proto.admin.diag-rpc]
pub mod admin {
    tonic::include_proto!("rio.admin");
}

/// Binary `FileDescriptorSet` covering every `.proto` file compiled by
/// `build.rs` (all six services + shared `rio.types`, with transitive
/// imports — `prost_build` always passes `--include_imports`).
///
/// Consumed by `rio-test-support/build.rs` to generate `MockAdmin`
/// default-stub macros: it decodes this with `prost_types::
/// FileDescriptorSet::decode` and iterates `AdminService.method[]`.
/// Exporting the descriptor set keeps the workspace's protoc invocation
/// in one crate — without this, `rio-test-support/build.rs` ran its own
/// protoc on `../rio-proto/proto/admin.proto`, a cross-directory
/// compile-time read that needed a fileset + symlink hack under
/// crate2nix's sandboxed per-crate builds.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/file_descriptor_set.bin"));

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
