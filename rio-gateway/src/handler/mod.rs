//! Opcode dispatch and handler implementations for the Nix worker protocol.
//!
//! Each handler reads its opcode-specific payload from the stream, performs
//! the operation via gRPC delegation, and writes the response via the STDERR
//! streaming loop.
//!
//! Store operations delegate to rio-store via `StoreServiceClient` gRPC calls;
//! build operations delegate to rio-scheduler via `SchedulerServiceClient`.

use std::collections::HashMap;

use rio_nix::derivation::Derivation;
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::transport::Channel;
use tracing::{instrument, warn};

use rio_common::tenant::NormalizedName;

use crate::quota::QuotaCache;
use crate::ratelimit::TenantLimiter;

const PROGRAM_NAME: &str = "rio-gateway";

// r[impl gw.jwt.issue]
// r[impl gw.jwt.dual-mode+2]
/// Wrap a request body in `tonic::Request`, injecting trace-context and
/// the session JWT as `x-rio-tenant-token` metadata.
///
/// The JWT carries the tenant identity to rio-store. Without it, store
/// handlers see an anonymous request and tenant-scoped operations
/// (`try_substitute_on_miss`, narinfo visibility gate) short-circuit.
/// Prior to P0465 only `build.rs` (SubmitBuild → scheduler) attached
/// the JWT; store-bound read opcodes didn't, so ssh-ng → wopQueryMissing
/// → store saw no tenant → substitution never fired.
///
/// `None` token → no header attached. Store's jwt_interceptor treats
/// absent-header as pass-through per `r[gw.jwt.dual-mode]` (SSH-comment
/// fallback); tenant-scoped features degrade gracefully, tenant-agnostic
/// ones work unchanged.
///
/// `try_from` on a JWT can't actually fail — jsonwebtoken emits
/// `base64url.base64url.base64url` (pure ASCII, no control chars, well
/// under the 8KB MetadataValue limit). The `?` is defensive: if
/// `rio_auth::jwt` ever changes encoding, we'd rather error than
/// silently drop auth on the floor.
///
/// Explicit per-call, not a tonic `Interceptor` layer — matches the
/// trace-context design in `rio-proto/src/interceptor.rs` (keeps
/// `StoreServiceClient<Channel>` as the concrete type; no 33-site
/// type churn to `InterceptedService<Channel, F>`).
pub(crate) fn with_jwt<T>(body: T, jwt_token: Option<&str>) -> anyhow::Result<tonic::Request<T>> {
    let mut req = tonic::Request::new(body);
    rio_proto::interceptor::inject_current(req.metadata_mut());
    for (k, v) in jwt_metadata(jwt_token) {
        req.metadata_mut()
            .insert(k, tonic::metadata::MetadataValue::try_from(v)?);
    }
    Ok(req)
}

/// Build the `x-rio-tenant-token` metadata pair list. Single source of
/// truth for JWT-header injection: [`with_jwt`] iterates this into a
/// `tonic::Request`, and `rio_proto::client::*` helpers take it as
/// `extra_metadata: &[(&str, &str)]` directly. Empty when `jwt_token`
/// is `None` (dual-mode fallback — store's interceptor treats absent
/// header as pass-through per `r[gw.jwt.dual-mode]`).
pub(crate) fn jwt_metadata(jwt_token: Option<&str>) -> Vec<(&'static str, &str)> {
    match jwt_token {
        Some(t) => vec![(rio_proto::TENANT_TOKEN_HEADER, t)],
        None => vec![],
    }
}

/// Typed errors for the handler layer.
///
/// Converts to `anyhow::Error` at the boundary via `thiserror`'s
/// `std::error::Error` impl + anyhow's blanket `From<E: Error>`.
/// The outer handler signature remains `anyhow::Result<()>` — use
/// `.into()` or `?` to convert.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Client sent an opcode we don't recognize. Connection must close
    /// to avoid stream desync (the payload shape is unknown).
    #[error("unknown opcode {0}, closing connection to avoid stream desynchronization")]
    UnknownOpcode(u64),

    /// Error already reported to the client via `STDERR_ERROR`.
    /// Produced by the `stderr_err!` macro — the message was shown to the
    /// client and this error terminates the session.
    #[error("{0}")]
    ClientReported(String),

    /// Invalid store path from wire input.
    #[error("invalid store path '{path}': {source}")]
    InvalidStorePath {
        path: String,
        #[source]
        source: rio_nix::store_path::StorePathError,
    },

    /// Invalid reference path in a reference list.
    #[error("invalid reference path '{path}' for {context}: {source}")]
    InvalidReference {
        path: String,
        context: String,
        #[source]
        source: rio_nix::store_path::StorePathError,
    },

    /// NAR size exceeds configured limit.
    #[error("{context}: nar_size {got} exceeds maximum {max}")]
    NarTooLarge { context: String, got: u64, max: u64 },

    /// PathInfo validation rejected wire input.
    #[error("{context}: {source}")]
    InvalidPathInfo {
        context: String,
        #[source]
        source: rio_proto::validated::PathInfoValidationError,
    },

    /// NAR read error (short read, IO error).
    #[error("{context}: NAR read: {source}")]
    NarRead {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// gRPC call to rio-store failed.
    #[error("store gRPC: {0}")]
    Store(String),

    /// gRPC call to rio-scheduler failed.
    #[error("scheduler gRPC: {0}")]
    Scheduler(String),

    /// gRPC stream/channel error (channel closed, task panicked).
    #[error("gRPC stream: {0}")]
    GrpcStream(String),

    /// Derivation lookup failed.
    #[error("derivation '{0}' not found in store")]
    DerivationNotFound(String),

    /// Derivation ATerm parse failed.
    #[error("failed to parse .drv '{path}': {msg}")]
    DerivationParse { path: String, msg: String },

    /// Per-session drv cache at cap.
    #[error("per-session derivation cache full ({count} entries, cap {cap})")]
    DrvCacheFull { count: usize, cap: usize },
}

/// Send a formatted error via STDERR_ERROR and return Err.
///
/// Expands to: send the message to the client, then
/// `return Err(GatewayError::ClientReported(msg).into())`.
/// The same message is used for both the client-visible error and the
/// internal error. Use inside `match ... { Err(e) => stderr_err!(stderr, "... {e}") }`.
///
/// Send-failure semantics: if writing the STDERR_ERROR frame itself fails
/// (client disconnected, SSH channel closed), that I/O error is propagated
/// via `?` — both paths terminate the session, so there is nothing useful
/// to be gained by swallowing it. This is the SOLE error-reporting
/// mechanism for the handler layer; do not add a function variant.
macro_rules! stderr_err {
    ($stderr:expr, $($arg:tt)*) => {{
        let __msg = format!($($arg)*);
        $stderr
            .error(&::rio_nix::protocol::stderr::StderrError::simple(
                PROGRAM_NAME,
                __msg.clone(),
            ))
            .await?;
        return Err($crate::handler::GatewayError::ClientReported(__msg).into());
    }};
}

/// Read a store-path string off the wire, record it in the current
/// span's `path` field, and parse it.
///
/// Collapses the `read_string → Span::record → StorePath::parse →
/// GatewayError::InvalidStorePath` sequence that every hard-fail
/// handler open-coded. Handlers that want soft-fail behaviour
/// (return `false`/empty instead of erroring) still read + parse
/// inline — they each do something different on parse failure.
pub(crate) async fn read_store_path<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> anyhow::Result<(String, StorePath)> {
    let path_str = wire::read_string(reader).await?;
    tracing::Span::current().record("path", path_str.as_str());
    let path = StorePath::parse(&path_str).map_err(|e| GatewayError::InvalidStorePath {
        path: path_str.clone(),
        source: e,
    })?;
    Ok((path_str, path))
}

/// Per-session mutable state, threaded through all opcode handlers.
///
/// Holds the gRPC clients and protocol-session-scoped state (options,
/// drv cache, build tracking). Constructed once per session
/// in [`crate::session::run_protocol`].
pub struct SessionContext {
    pub store_client: StoreServiceClient<Channel>,
    pub scheduler_client: SchedulerServiceClient<Channel>,
    pub drv_cache: HashMap<StorePath, Derivation>,
    /// IFD detection: wopBuildDerivation without prior wopBuildPathsWithResults
    /// is likely an IFD or build-hook request.
    pub has_seen_build_paths_with_results: bool,
    /// Active build IDs for scheduler failover: build_id → last_sequence.
    pub active_build_ids: HashMap<String, u64>,
    /// Tenant name from the matched `authorized_keys` entry's comment.
    /// Sent in `SubmitBuildRequest.tenant_name`; scheduler resolves to UUID.
    /// `None` = single-tenant mode (empty or malformed comment). The
    /// [`NormalizedName`] type guarantees the `Some` case is trimmed
    /// and whitespace-free — no downstream `.trim()` needed.
    pub tenant_name: Option<NormalizedName>,
    /// Per-session JWT minted at SSH auth time. Injected as
    /// `x-rio-tenant-token` on outbound gRPC calls. `None` when
    /// the gateway's signing key is unconfigured → dual-mode
    /// fallback (downstream reads `tenant_name` from the proto
    /// body instead). See `r[gw.jwt.issue]` / `r[gw.jwt.dual-mode]`.
    pub jwt_token: Option<String>,
    /// Per-tenant build-submit rate limiter. Checked in the build
    /// opcode handlers before `SubmitBuild`. Disabled by default
    /// (the disabled variant's `check()` is a no-op). Shared state
    /// via inner `Arc` — all sessions on all connections drain the
    /// same per-tenant bucket. See `r[gw.rate.per-tenant]`.
    pub limiter: TenantLimiter,
    /// Per-tenant store-quota cache. Checked alongside `limiter`
    /// before `SubmitBuild`. Shared state via inner `Arc` — a quota
    /// fetched by one session is warm for all within the 30s TTL.
    /// See `r[store.gc.tenant-quota-enforce]`.
    pub quota_cache: QuotaCache,
}

impl SessionContext {
    pub fn new(
        store_client: StoreServiceClient<Channel>,
        scheduler_client: SchedulerServiceClient<Channel>,
        tenant_name: Option<NormalizedName>,
        jwt_token: Option<String>,
        limiter: TenantLimiter,
        quota_cache: QuotaCache,
    ) -> Self {
        Self {
            store_client,
            scheduler_client,
            drv_cache: HashMap::new(),
            has_seen_build_paths_with_results: false,
            active_build_ids: HashMap::new(),
            tenant_name,
            jwt_token,
            limiter,
            quota_cache,
        }
    }
}

// r[impl gw.opcode.mandatory-set]
// r[impl gw.compat.unknown-opcode-close]
/// Dispatch an opcode to the appropriate handler.
///
/// Returns `Ok(())` on success. On errors, sends `STDERR_ERROR` to the
/// client and returns `Err`, which terminates the session.
///
/// Every handler takes the same `(reader, &mut stderr, ctx)` shape —
/// even the ones that ignore most of `ctx` — so this match stays
/// uniform and adding a handler doesn't mean inventing a new parameter
/// shape.
#[instrument(skip_all, fields(opcode))]
pub async fn handle_opcode<R, W>(
    opcode: u64,
    reader: &mut R,
    writer: &mut W,
    ctx: &mut SessionContext,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin,
{
    let mut stderr = StderrWriter::new(writer);
    let start = std::time::Instant::now();

    let op = WorkerOp::from_u64(opcode);
    let op_name = op.map(|o| o.name()).unwrap_or("unknown");
    tracing::Span::current().record("opcode", op_name);
    metrics::counter!("rio_gateway_opcodes_total", "opcode" => op_name).increment(1);

    use WorkerOp::*;
    let result = match op {
        Some(IsValidPath) => handle_is_valid_path(reader, &mut stderr, ctx).await,
        Some(AddToStore) => handle_add_to_store(reader, &mut stderr, ctx).await,
        Some(AddTextToStore) => handle_add_text_to_store(reader, &mut stderr, ctx).await,
        Some(EnsurePath) => handle_ensure_path(reader, &mut stderr, ctx).await,
        Some(QueryPathInfo) => handle_query_path_info(reader, &mut stderr, ctx).await,
        Some(QueryValidPaths) => handle_query_valid_paths(reader, &mut stderr, ctx).await,
        Some(AddTempRoot) => handle_add_temp_root(reader, &mut stderr, ctx).await,
        Some(SetOptions) => handle_set_options(reader, &mut stderr, ctx).await,
        Some(NarFromPath) => handle_nar_from_path(reader, &mut stderr, ctx).await,
        Some(QueryPathFromHashPart) => {
            handle_query_path_from_hash_part(reader, &mut stderr, ctx).await
        }
        Some(AddSignatures) => handle_add_signatures(reader, &mut stderr, ctx).await,
        Some(QueryMissing) => handle_query_missing(reader, &mut stderr, ctx).await,
        Some(AddToStoreNar) => handle_add_to_store_nar(reader, &mut stderr, ctx).await,
        Some(AddMultipleToStore) => handle_add_multiple_to_store(reader, &mut stderr, ctx).await,
        Some(QueryDerivationOutputMap) => {
            handle_query_derivation_output_map(reader, &mut stderr, ctx).await
        }
        Some(BuildDerivation) => handle_build_derivation(reader, &mut stderr, ctx).await,
        Some(BuildPaths) => handle_build_paths(reader, &mut stderr, ctx).await,
        Some(BuildPathsWithResults) => {
            ctx.has_seen_build_paths_with_results = true;
            handle_build_paths_with_results(reader, &mut stderr, ctx).await
        }
        Some(RegisterDrvOutput) => handle_register_drv_output(reader, &mut stderr, ctx).await,
        Some(QueryRealisation) => handle_query_realisation(reader, &mut stderr, ctx).await,
        None => {
            warn!(opcode = opcode, "unknown opcode, closing connection");
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("unsupported operation {opcode}"),
                ))
                .await?;
            Err(GatewayError::UnknownOpcode(opcode).into())
        }
    };

    let elapsed = start.elapsed();
    metrics::histogram!("rio_gateway_opcode_duration_seconds", "opcode" => op_name)
        .record(elapsed.as_secs_f64());

    if result.is_err() {
        metrics::counter!("rio_gateway_errors_total", "type" => "protocol").increment(1);
    }

    result
}

// ---------------------------------------------------------------------------
// Submodules (defined AFTER stderr_err! macro so it is visible to them)
// ---------------------------------------------------------------------------

mod build;
pub(crate) mod grpc;
mod opcodes_read;
mod opcodes_write;

use build::{handle_build_derivation, handle_build_paths, handle_build_paths_with_results};
pub(crate) use grpc::grpc_query_path_info;
use opcodes_read::{
    handle_add_signatures, handle_add_temp_root, handle_ensure_path, handle_is_valid_path,
    handle_nar_from_path, handle_query_derivation_output_map, handle_query_missing,
    handle_query_path_from_hash_part, handle_query_path_info, handle_query_realisation,
    handle_query_valid_paths, handle_register_drv_output, handle_set_options,
};
use opcodes_write::{
    handle_add_multiple_to_store, handle_add_text_to_store, handle_add_to_store,
    handle_add_to_store_nar,
};
