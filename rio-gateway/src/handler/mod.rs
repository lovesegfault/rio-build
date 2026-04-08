//! Opcode dispatch and handler implementations for the Nix worker protocol.
//!
//! Each handler reads its opcode-specific payload from the stream, performs
//! the operation via gRPC delegation, and writes the response via the STDERR
//! streaming loop.
//!
//! Store operations delegate to rio-store via `StoreServiceClient` gRPC calls;
//! build operations delegate to rio-scheduler via `SchedulerServiceClient`.

use std::collections::{HashMap, HashSet};

use rio_nix::derivation::Derivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::nar::{self, NarNode};
use rio_nix::protocol::build::{
    BuildMode, BuildResult, BuildStatus, read_basic_derivation, write_build_result,
};
use rio_nix::protocol::derived_path::{DerivedPath, OutputSpec};
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::stderr::{
    ActivityType, ResultField, ResultType, StderrError, StderrWriter, verbosity,
};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use rio_proto::types;
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::transport::Channel;
use tracing::{debug, error, info, instrument, warn};

use rio_common::grpc::{DEFAULT_GRPC_TIMEOUT, GRPC_STREAM_TIMEOUT};
use rio_common::limits::MAX_NAR_SIZE;
use rio_common::tenant::NormalizedName;

use crate::quota::{QuotaCache, QuotaVerdict, human_bytes};
use crate::ratelimit::TenantLimiter;
use crate::translate;

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
/// `rio_common::jwt` ever changes encoding, we'd rather error than
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

    let jwt = ctx.jwt_token.as_deref();
    let result = match op {
        Some(WorkerOp::IsValidPath) => {
            handle_is_valid_path(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::AddToStore) => {
            handle_add_to_store(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                jwt,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::AddTextToStore) => {
            handle_add_text_to_store(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                jwt,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::EnsurePath) => {
            handle_ensure_path(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::QueryPathInfo) => {
            handle_query_path_info(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::QueryValidPaths) => {
            handle_query_valid_paths(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::AddTempRoot) => handle_add_temp_root(reader, &mut stderr).await,
        Some(WorkerOp::SetOptions) => handle_set_options(reader, &mut stderr).await,
        Some(WorkerOp::NarFromPath) => {
            handle_nar_from_path(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::QueryPathFromHashPart) => {
            handle_query_path_from_hash_part(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::AddSignatures) => {
            handle_add_signatures(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::QueryMissing) => {
            handle_query_missing(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                jwt,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::AddToStoreNar) => {
            handle_add_to_store_nar(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                jwt,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::AddMultipleToStore) => {
            handle_add_multiple_to_store(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                jwt,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::QueryDerivationOutputMap) => {
            handle_query_derivation_output_map(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                jwt,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::BuildDerivation) => handle_build_derivation(reader, &mut stderr, ctx).await,
        Some(WorkerOp::BuildPaths) => handle_build_paths(reader, &mut stderr, ctx).await,
        Some(WorkerOp::BuildPathsWithResults) => {
            ctx.has_seen_build_paths_with_results = true;
            handle_build_paths_with_results(reader, &mut stderr, ctx).await
        }
        Some(WorkerOp::RegisterDrvOutput) => {
            handle_register_drv_output(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
        Some(WorkerOp::QueryRealisation) => {
            handle_query_realisation(reader, &mut stderr, &mut ctx.store_client, jwt).await
        }
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

/// If `path` is a `.drv`, parse the ATerm from NAR data and cache it.
/// Cap drv_cache at [`translate::max_transitive_inputs()`]. The cache
/// is session-scoped; a client uploading >cap .drv files would consume
/// cap * (avg drv size ~1KB parsed) per session. The cap matches the
/// BFS limit in translate::reconstruct_dag — a DAG bigger than that
/// would be rejected anyway, so caching more .drvs is wasted.
///
/// Returns true if inserted, false if cap hit. Caller decides what
/// to do (try_cache_drv logs + continues; resolve_derivation
/// returns an error to the client).
fn insert_drv_bounded(
    drv_cache: &mut HashMap<StorePath, Derivation>,
    path: StorePath,
    drv: Derivation,
) -> bool {
    if drv_cache.len() >= crate::translate::max_transitive_inputs()
        && !drv_cache.contains_key(&path)
    {
        return false;
    }
    drv_cache.insert(path, drv);
    true
}

/// Maximum NAR size to buffer for `.drv` caching. Above this, the NAR
/// streams directly to the store and [`try_cache_drv`] is skipped —
/// `resolve_derivation` fetches from the store later during DAG
/// reconstruction (one extra round-trip, correctness unchanged).
/// 16 MiB covers observed outliers (`options.json.drv` ≈ 9.7MB) with
/// headroom; typical `.drv` NARs are <10KB.
pub(super) const DRV_NAR_BUFFER_LIMIT: u64 = 16 * 1024 * 1024;

fn try_cache_drv(
    path: &StorePath,
    nar_data: &[u8],
    drv_cache: &mut HashMap<StorePath, Derivation>,
) {
    if !path.is_derivation() {
        return;
    }
    match Derivation::parse_from_nar(nar_data) {
        Ok(drv) => {
            if insert_drv_bounded(drv_cache, path.clone(), drv) {
                debug!(path = %path, "cached parsed derivation");
            } else {
                // Log at warn (not per-insert spam): every subsequent insert
                // at cap also fails. The upload itself still succeeds.
                warn!(
                    path = %path,
                    cap = crate::translate::max_transitive_inputs(),
                    "drv_cache at cap; not caching (upload still proceeds)"
                );
            }
        }
        Err(e) => {
            warn!(path = %path, error = %e, "failed to parse .drv from NAR");
        }
    }
}

/// Look up a derivation from session cache, or fetch from store via gRPC,
/// parse the ATerm, and cache it.
///
/// NOTE: `.drv` lookups use ANONYMOUS store access (no JWT). A `.drv`
/// is a build INPUT — it may have been uploaded via a different tenant
/// context (e.g., `nix copy` with default key, then `nix build` with
/// tenant key). Tenant-scoping input resolution breaks cross-context
/// build flows: the store's `path_tenants` table has no row for the
/// `.drv` under the building tenant, so tenant-filtered `GetPath`
/// returns NotFound → "not a valid store path". JWT propagation is
/// for OUTPUT reads (`handle_query_path_info`, `handle_nar_from_path`,
/// etc.) where tenant-scoped visibility is the correct semantics.
pub(crate) async fn resolve_derivation(
    drv_path: &StorePath,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<Derivation> {
    if let Some(cached) = drv_cache.get(drv_path) {
        return Ok(cached.clone());
    }

    let (_info, nar_data) = grpc_get_path(store_client, None, drv_path.as_str())
        .await?
        .ok_or_else(|| GatewayError::DerivationNotFound(drv_path.to_string()))?;

    let drv = Derivation::parse_from_nar(&nar_data).map_err(|e| GatewayError::DerivationParse {
        path: drv_path.to_string(),
        msg: e.to_string(),
    })?;

    // Bound drv_cache. resolve_derivation is called from BFS in
    // translate::reconstruct_dag — cap hit means the DAG is too large
    // (the BFS enforces the same cap, but the cache could grow beyond
    // it across multiple builds in one session). Error propagates as
    // DAG failure.
    if !insert_drv_bounded(drv_cache, drv_path.clone(), drv.clone()) {
        return Err(GatewayError::DrvCacheFull {
            count: drv_cache.len(),
            cap: crate::translate::max_transitive_inputs(),
        }
        .into());
    }
    Ok(drv)
}

/// Max in-flight `GetPath` calls during BFS .drv resolution. The store's
/// `inline_blob` reads are tiny (.drv NARs are KB-range) so the bound is
/// mostly to cap connection-pool fan-out, same rationale as the
/// scheduler's `DEFAULT_SUBSTITUTE_CONCURRENCY`. 32 matches I-052's
/// `wopAddMultipleToStore` pipeline depth.
pub(crate) const BFS_FETCH_CONCURRENCY: usize = 32;

/// Batch counterpart to [`resolve_derivation`] for the BFS in
/// `translate::reconstruct_dag`. Fires up to [`BFS_FETCH_CONCURRENCY`]
/// concurrent `GetPath` calls for the cache MISSES in `paths`, parses
/// each NAR, inserts into `drv_cache`, and returns `(StorePath,
/// Derivation)` for EVERY requested path (hits and freshly fetched) so
/// the caller can enqueue the next BFS level without re-probing the
/// cache. Order is NOT preserved (buffer_unordered) — the BFS only needs
/// the set.
///
/// P0539: the per-child `resolve_derivation().await` in the old BFS was
/// ~1085 sequential RTTs to rio-store for a hello-shallow closure
/// (~210s). Level-batching collapses that to roughly DAG-depth ×
/// ceil(level-width / 32) RTTs.
///
/// Same anonymous-lookup semantics as [`resolve_derivation`] (no JWT —
/// `.drv`s are build inputs).
pub(crate) async fn resolve_derivations_batch(
    paths: Vec<StorePath>,
    store_client: &StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<Vec<(StorePath, Derivation)>> {
    use futures_util::{StreamExt, stream};

    let mut resolved = Vec::with_capacity(paths.len());
    let mut to_fetch = Vec::new();
    for p in paths {
        match drv_cache.get(&p) {
            Some(d) => resolved.push((p, d.clone())),
            None => to_fetch.push(p),
        }
    }
    if to_fetch.is_empty() {
        return Ok(resolved);
    }

    // tonic clients are cheap clones over a shared Channel; each
    // concurrent task gets its own clone so calls don't serialize on a
    // &mut. Results stream back unordered.
    let mut fetched: Vec<(StorePath, Derivation)> = stream::iter(to_fetch)
        .map(|sp| {
            let mut client = store_client.clone();
            async move {
                let (_info, nar) = grpc_get_path(&mut client, None, sp.as_str())
                    .await?
                    .ok_or_else(|| GatewayError::DerivationNotFound(sp.to_string()))?;
                let drv = Derivation::parse_from_nar(&nar).map_err(|e| {
                    GatewayError::DerivationParse {
                        path: sp.to_string(),
                        msg: e.to_string(),
                    }
                })?;
                Ok::<_, anyhow::Error>((sp, drv))
            }
        })
        .buffer_unordered(BFS_FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<_>>()?;

    for (sp, drv) in &fetched {
        if !insert_drv_bounded(drv_cache, sp.clone(), drv.clone()) {
            return Err(GatewayError::DrvCacheFull {
                count: drv_cache.len(),
                cap: crate::translate::max_transitive_inputs(),
            }
            .into());
        }
    }
    resolved.append(&mut fetched);
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Submodules (defined AFTER stderr_err! macro so it is visible to them)
// ---------------------------------------------------------------------------

mod build;
mod grpc;
mod opcodes_read;
mod opcodes_write;

use build::*;
pub(crate) use grpc::grpc_query_path_info;
use grpc::*;
use opcodes_read::*;
use opcodes_write::*;
