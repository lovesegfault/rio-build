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
use rio_nix::protocol::derived_path::DerivedPath;
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::stderr::{ActivityType, StderrError, StderrWriter};
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
// r[impl gw.jwt.dual-mode]
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
    if let Some(token) = jwt_token {
        req.metadata_mut().insert(
            rio_common::jwt_interceptor::TENANT_TOKEN_HEADER,
            tonic::metadata::MetadataValue::try_from(token)?,
        );
    }
    Ok(req)
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

    /// Invalid hex encoding from wire input.
    #[error("{context}: invalid hex: {source}")]
    InvalidHex {
        context: String,
        #[source]
        source: hex::FromHexError,
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

/// Client build options received via wopSetOptions.
///
/// Propagated to the scheduler via gRPC `SubmitBuildRequest`.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub keep_failed: bool,
    pub keep_going: bool,
    pub try_fallback: bool,
    pub verbosity: u64,
    pub max_build_jobs: u64,
    pub max_silent_time: u64,
    pub verbose_build: bool,
    pub build_cores: u64,
    pub use_substitutes: bool,
    pub overrides: Vec<(String, String)>,
}

/// UNREACHABLE VIA ssh-ng:// — these helpers run only for daemon-socket clients.
///
/// Nix `SSHStore` overrides `RemoteStore::setOptions()` with an **empty body**
/// (ssh-store.cc, unchanged since 088ef8175 "ssh-ng: Don't forward options to
/// the daemon", 2018-03-05). `RemoteStore::initConnection` calls `setOptions(conn)`
/// via virtual dispatch → the empty override wins → `wopSetOptions` never hits
/// the wire for `ssh-ng://` stores. Verified in our pinned flake input
/// (ssh-store.cc:81-88) and by the `setoptions-unreachable` VM subtest in
/// scheduling.nix.
///
/// The empty override is **intentional** upstream (see NixOS/nix#1713, #1935 —
/// forwarding options broke shared builders). Upstream fix 32827b9fb (NixOS/nix
/// #5600) adds selective forwarding, but (a) not in our pinned rev, and (b)
/// gated on the daemon advertising a `set-options-map-only` protocol feature
/// that rio-gateway does not implement.
///
/// For `unix://` daemon-socket clients (NOT our production path), the base
/// `RemoteStore::setOptions()` at remote-store.cc:115 DOES fire. But note:
///
/// - `max-silent-time` is sent **positionally** (wire slot 6), then **erased**
///   from the overrides map at remote-store.cc:131. So [`Self::max_silent_time`]'s
///   override-wins logic would NOT find it in overrides — only the positional
///   fallback works. The earlier "Empirically: ... overrides=[("max-silent-time",
///   "5")]" comment here was wrong on both axes (ssh-ng doesn't send; daemon
///   sends positionally not in overrides).
///
/// - `timeout` (canonical) is NOT positionally encoded, so it stays in overrides.
///   But `Config::getSettings` (configuration.cc:91 `!isAlias` filter) emits the
///   **canonical** name `"timeout"`, not the alias `"build-timeout"` — so
///   [`Self::build_timeout`] keying on `"build-timeout"` below would miss it.
///
/// The `sched.timeout.per-build` spec requirement is therefore reachable
/// **only via gRPC `SubmitBuildRequest.build_timeout`** (rio-cli, direct
/// API consumers), not via `nix-build --option`.
///
/// WONTFIX(P0310): If rio-gateway ever advertises `set-options-map-only` AND the
/// flake's nix input is bumped past 32827b9fb, this block goes live. Fix the
/// `build_timeout` key (`"timeout"` not `"build-timeout"`) and delete the
/// override-wins logic in `max_silent_time()` before relying on either.
impl ClientOptions {
    /// Extract the build timeout from overrides, defaulting to 0 (no timeout).
    ///
    /// **Dead code for ssh-ng** (see impl-level doc). Also keys on wrong name
    /// for daemon-socket clients: wire sends canonical `"timeout"`, not alias
    /// `"build-timeout"`. Kept for gRPC-path symmetry (SubmitBuildRequest
    /// doesn't go through this; it sets the proto field directly).
    pub fn build_timeout(&self) -> u64 {
        self.overrides
            .iter()
            .find(|(k, _)| k == "build-timeout")
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Effective maxSilentTime: positional wire slot 6.
    ///
    /// **Dead code for ssh-ng** (see impl-level doc). For daemon-socket
    /// clients: `max-silent-time` is sent positionally AND erased from
    /// overrides (remote-store.cc:131), so the override-wins branch never
    /// matches — only the `self.max_silent_time` fallback is live. Kept
    /// as-is: correct code, unreachable branch.
    pub fn max_silent_time(&self) -> u64 {
        self.overrides
            .iter()
            .find(|(k, _)| k == "max-silent-time")
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or(self.max_silent_time)
    }
}

/// Per-session mutable state, threaded through all opcode handlers.
///
/// Holds the gRPC clients and protocol-session-scoped state (options,
/// drv cache, build tracking). Constructed once per session
/// in [`crate::session::run_protocol`].
pub struct SessionContext {
    pub store_client: StoreServiceClient<Channel>,
    pub scheduler_client: SchedulerServiceClient<Channel>,
    pub options: Option<ClientOptions>,
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
            options: None,
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
        Some(WorkerOp::SetOptions) => {
            handle_set_options(reader, &mut stderr, &mut ctx.options).await
        }
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

/// Send a store error as STDERR_ERROR to the client, then return the error.
async fn send_store_error<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    err: anyhow::Error,
) -> anyhow::Result<()> {
    if let Err(send_err) = stderr
        .error(&StderrError::simple(
            PROGRAM_NAME,
            format!("store error: {err}"),
        ))
        .await
    {
        warn!(error = %send_err, "failed to send store error to client");
    }
    Err(err)
}

/// If `path` is a `.drv`, parse the ATerm from NAR data and cache it.
/// Cap drv_cache at MAX_TRANSITIVE_INPUTS. The cache
/// is session-scoped; a client uploading 100k .drv files would
/// consume ~100k * (avg drv size ~1KB parsed) = ~100MB per session.
/// MAX_TRANSITIVE_INPUTS (10k) matches the BFS limit in
/// translate::reconstruct_dag — a DAG bigger than that would be
/// rejected anyway, so caching more .drvs is wasted.
///
/// Returns true if inserted, false if cap hit. Caller decides what
/// to do (try_cache_drv logs + continues; resolve_derivation
/// returns an error to the client).
fn insert_drv_bounded(
    drv_cache: &mut HashMap<StorePath, Derivation>,
    path: StorePath,
    drv: Derivation,
) -> bool {
    if drv_cache.len() >= crate::translate::MAX_TRANSITIVE_INPUTS && !drv_cache.contains_key(&path)
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
                    cap = crate::translate::MAX_TRANSITIVE_INPUTS,
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
pub(crate) async fn resolve_derivation(
    drv_path: &StorePath,
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<Derivation> {
    if let Some(cached) = drv_cache.get(drv_path) {
        return Ok(cached.clone());
    }

    let (_info, nar_data) = grpc_get_path(store_client, jwt_token, drv_path.as_str())
        .await?
        .ok_or_else(|| GatewayError::DerivationNotFound(drv_path.to_string()))?;

    let drv = Derivation::parse_from_nar(&nar_data).map_err(|e| GatewayError::DerivationParse {
        path: drv_path.to_string(),
        msg: e.to_string(),
    })?;

    // Bound drv_cache. resolve_derivation is called from BFS in
    // translate::reconstruct_dag — cap hit means the DAG is too large
    // (MAX_TRANSITIVE_INPUTS enforced by the BFS itself, but the cache
    // could grow beyond that across multiple builds in one session).
    // Error propagates as DAG failure.
    if !insert_drv_bounded(drv_cache, drv_path.clone(), drv.clone()) {
        return Err(GatewayError::DrvCacheFull {
            count: drv_cache.len(),
            cap: crate::translate::MAX_TRANSITIVE_INPUTS,
        }
        .into());
    }
    Ok(drv)
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

#[cfg(test)]
mod tests {
    use super::ClientOptions;

    fn opts(positional_max_silent: u64, overrides: Vec<(String, String)>) -> ClientOptions {
        ClientOptions {
            keep_failed: false,
            keep_going: false,
            try_fallback: false,
            verbosity: 0,
            max_build_jobs: 0,
            max_silent_time: positional_max_silent,
            verbose_build: false,
            build_cores: 0,
            use_substitutes: false,
            overrides,
        }
    }

    // These tests exercise the ClientOptions accessors DESPITE the code
    // path being UNREACHABLE via ssh-ng:// (see impl-block doc above).
    // ssh-ng clients NEVER send wopSetOptions (ssh-store.cc empty
    // override since 088ef8175, P0310 source-verified) and NEVER put
    // max-silent-time in argv. These accessors go live only if rio-gateway
    // advertises `set-options-map-only` AND the flake nix input bumps
    // past 32827b9fb (WONTFIX(P0310)). Tests kept to pin the accessor
    // semantics for that future path.

    /// Override-wins logic reads `max-silent-time` from overrides.
    /// **Unreachable via ssh-ng** — and also NOT the daemon-socket path:
    /// remote-store.cc:131 erases max-silent-time from overrides before
    /// sending (positional-only). This branch is speculative for a future
    /// protocol rev.
    #[test]
    fn max_silent_time_reads_from_overrides() {
        let o = opts(0, vec![("max-silent-time".into(), "5".into())]);
        assert_eq!(o.max_silent_time(), 5);
    }

    /// Override wins even when positional is nonzero. Same unreachability
    /// caveat as above.
    #[test]
    fn max_silent_time_override_wins_over_positional() {
        let o = opts(60, vec![("max-silent-time".into(), "5".into())]);
        assert_eq!(o.max_silent_time(), 5);
    }

    /// No override → fall back to positional. This is the ONLY live
    /// branch for `unix://` daemon-socket clients (remote-store.cc:119
    /// sends maxSilentTime positionally, :131 erases from overrides).
    #[test]
    fn max_silent_time_falls_back_to_positional() {
        let o = opts(30, vec![("other-key".into(), "x".into())]);
        assert_eq!(o.max_silent_time(), 30);
    }

    /// Both zero → 0 (disabled).
    #[test]
    fn max_silent_time_zero_when_neither_set() {
        let o = opts(0, vec![]);
        assert_eq!(o.max_silent_time(), 0);
    }

    /// Unparseable override falls back to positional (not panic).
    #[test]
    fn max_silent_time_bad_override_falls_back() {
        let o = opts(30, vec![("max-silent-time".into(), "not-a-number".into())]);
        assert_eq!(o.max_silent_time(), 30);
    }
}
