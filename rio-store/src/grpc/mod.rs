//! StoreService + ChunkService gRPC server implementations.
//!
//! Submodules:
//! - `put_path` — write-ahead upload flow (Steps 1-6) + `common` shared
//!   with `put_path_batch`
//! - `put_path_batch` — atomic multi-output upload
//! - `get_path` — streaming NAR download (inline/chunked reassembly)
//! - `queries` — read RPCs (QueryPathInfo, FindMissingPaths, …)
//! - `sign` — narinfo signing + sig-visibility gate
//! - `chunk` — ChunkService (GetChunk)
//! - `admin` — StoreAdminService (GC, VerifyChunks, …)
//!
//! This file holds [`StoreServiceImpl`] (struct + builders), shared
//! status-mapping helpers, and the `StoreService` trait impl. Every
//! trait method delegates to an inherent `_impl` method in a submodule
//! so `self.pool`/`self.chunk_backend` field access works without
//! making fields `pub(super)`.

use std::sync::Arc;

use sqlx::PgPool;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, instrument};

use rio_proto::StoreService;
use rio_proto::types::{
    AddSignaturesRequest, AddSignaturesResponse, AppendHwPerfSampleRequest,
    BatchGetManifestRequest, BatchGetManifestResponse, BatchQueryPathInfoRequest,
    BatchQueryPathInfoResponse, FindMissingPathsRequest, FindMissingPathsResponse, GetPathRequest,
    PathInfo, PutPathBatchRequest, PutPathBatchResponse, PutPathRequest, PutPathResponse,
    QueryPathFromHashPartRequest, QueryPathInfoRequest, QueryRealisationRequest, Realisation,
    RegisterRealisationRequest, RegisterRealisationResponse, TenantQuotaRequest,
    TenantQuotaResponse,
};
use rio_proto::validated::ValidatedPathInfo;

use rio_common::grpc::StatusExt;
use rio_common::limits::MAX_NAR_SIZE;

use crate::backend::ChunkBackend;
use crate::cas::{self, ChunkCache};
use crate::metadata;
use crate::signing::TenantSigner;
use crate::substitute::{SubstituteError, Substituter};

mod admin;
mod chunk;
mod get_path;
mod put_path;
mod put_path_batch;
mod queries;
mod sign;

pub use admin::StoreAdminServiceImpl;
pub use chunk::ChunkServiceImpl;

/// Default cap on paths in a FindMissingPaths request (DoS guard).
/// Matches `rio_nix::protocol::wire::MAX_COLLECTION_COUNT` — the gateway
/// is the trust boundary and already enforces 1M at wire-read time, so a
/// tighter store-side cap only rejects batches the gateway already
/// admitted (I-016: 10k→100k; I-130: 100k→1M after hello-deep-1024x sent
/// 153,934). 1M × ~80 bytes ≈ 80 MB worst case. Runtime-configurable via
/// `RIO_MAX_BATCH_PATHS` (StoreServiceImpl field). GC `extra_roots`
/// has its own separate cap (`MAX_GC_EXTRA_ROOTS` in `admin`).
pub const DEFAULT_MAX_BATCH_PATHS: usize = 1_048_576;

/// Validate a store path string: must parse as a well-formed Nix store path
/// (`/nix/store/<32-char-nixbase32>-<name>`). Rejects malformed paths, path
/// traversal attempts, and oversized strings at the RPC boundary.
pub(crate) fn validate_store_path(s: &str) -> Result<(), Status> {
    rio_nix::store_path::StorePath::parse(s)
        .map(|_| ())
        .status_invalid(&format!("invalid store path {s:?}"))
}

/// Map a storage-backend anyhow error to a Status, distinguishing
/// permanent auth/config failures from transient ones.
///
/// [`rio_common::grpc::internal`] maps everything to `Internal`, which a client
/// treats as retriable. For an STS AccessDenied (IRSA misconfigured,
/// IAM policy missing s3:PutObject) that means the builder retries
/// forever: the scheduler sees InfrastructureFailure, re-dispatches,
/// the builder rebuilds, the upload fails the same way, loop.
/// Observed: 12 derivations × 146 cycles in 6 minutes before manual
/// intervention.
///
/// Inspects the anyhow chain for [`BackendAuthError`] (set by
/// `S3ChunkBackend` ops when the SDK error matches known auth
/// signatures). If present → `FailedPrecondition` with a message that
/// names the fix. Otherwise → same as [`rio_common::grpc::internal`].
///
/// [`BackendAuthError`]: crate::backend::BackendAuthError
pub(crate) fn storage_error(context: &str, e: anyhow::Error) -> Status {
    error!(context, error = %e, "storage backend error");
    // downcast_ref checks the innermost source; BackendAuthError is
    // always the root (anyhow::Error::new(BackendAuthError).context(...)).
    if e.downcast_ref::<crate::backend::BackendAuthError>()
        .is_some()
    {
        Status::failed_precondition(
            "storage backend authentication failed; check S3 credentials/IAM permissions",
        )
    } else {
        Status::internal("storage operation failed")
    }
}

/// Map a [`MetadataError`] to a gRPC status with a precise code.
///
/// The key value of the typed error: retriable failures
/// (connection/serialization/placeholder-race) get retriable codes
/// (`unavailable`/`aborted`) so clients back off and retry; corruption
/// (invariant/malformed/corrupt-manifest) gets non-retriable codes so
/// clients fail fast. A flat everything-is-internal mapping would make a
/// transient PG hiccup look the same as a corrupt database.
///
/// Logs the full error (including sqlx source chain) server-side; the
/// gRPC message is a scrubbed summary.
pub(crate) fn metadata_status(context: &str, e: metadata::MetadataError) -> Status {
    use metadata::MetadataError as M;
    match &e {
        // I-145: serialization failure is an EXPECTED outcome under
        // concurrent write contention. Client retries on `aborted`;
        // logging at ERROR floods the log with spurious entries.
        M::Serialization => debug!(
            context,
            error = %e,
            "metadata layer: serialization conflict (client retries)"
        ),
        _ => error!(context, error = %e, "metadata layer error"),
    }
    match e {
        M::NotFound => Status::not_found("not found"),
        M::Conflict(_) => Status::already_exists("conflict: path already exists"),
        M::RealisationConflict {
            existing,
            attempted,
            ..
        } => Status::already_exists(format!(
            "realisation conflict: existing {existing}, attempted {attempted}"
        )),
        M::Connection(_) => Status::unavailable("database connection failed; retry"),
        M::Serialization => Status::aborted("transaction serialization failure; retry"),
        M::Deadlock(_) => Status::aborted("transaction deadlock detected; retry"),
        M::PlaceholderMissing { .. } => {
            Status::aborted("upload placeholder concurrently deleted; retry")
        }
        M::CorruptManifest { .. } => Status::data_loss("stored manifest data is corrupt"),
        // Backpressure: PG pool exhausted, signature count cap, etc.
        // Client should retry with backoff. Distinct from Connection
        // (unavailable → try-another-replica): this is "slow down",
        // not "go elsewhere".
        M::ResourceExhausted(msg) => Status::resource_exhausted(msg),
        M::InvariantViolation(_) | M::MalformedRow(_) | M::Other(_) => {
            Status::internal("storage operation failed")
        }
    }
}

/// PutPath-scoped wrapper around [`metadata_status`]: increments
/// `rio_store_putpath_retries_total{reason}` for retriable variants
/// (the ones that map to `aborted`/`unavailable` and which the worker
/// upload loop retries) before delegating. Same I-145 site as the
/// log-level special-case above; separate fn because `metadata_status`
/// is called from read RPCs (QueryPathInfo etc.) where the counter
/// would be a misnomer.
pub(crate) fn putpath_metadata_status(context: &str, e: metadata::MetadataError) -> Status {
    use metadata::MetadataError as M;
    let reason = match &e {
        M::Serialization => Some("serialization"),
        M::Deadlock(_) => Some("deadlock"),
        M::PlaceholderMissing { .. } => Some("placeholder_missing"),
        M::Connection(_) => Some("connection"),
        M::ResourceExhausted(_) => Some("resource_exhausted"),
        // Non-retriable (NotFound/Conflict/Invariant/Malformed/Corrupt/
        // Other) — not counted; the client won't retry an `internal`/
        // `data_loss`/`already_exists`.
        _ => None,
    };
    if let Some(reason) = reason {
        metrics::counter!("rio_store_putpath_retries_total", "reason" => reason).increment(1);
    }
    metadata_status(context, e)
}

/// The StoreService gRPC server.
///
/// NAR content lives in `manifests.inline_blob` (small NARs) or as
/// FastCDC chunks (large NARs). Inline blobs are stored directly in PG.
pub struct StoreServiceImpl {
    pool: PgPool,
    /// Chunk storage for NARs ≥ INLINE_THRESHOLD. `None` disables chunking
    /// entirely (all NARs go inline, regardless of size).
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    /// Cache for chunk reads (GetPath). Created once at construction;
    /// shared across all GetPath calls (the moka LRU and singleflight map
    /// are process-wide). `None` iff `chunk_backend` is None — they're
    /// paired.
    ///
    /// `Arc` because the spawned GetPath streaming task needs an owned
    /// handle (the task outlives the `&self` method call).
    chunk_cache: Option<Arc<ChunkCache>>,
    /// Tenant-aware ed25519 signer for narinfo. Wraps the cluster
    /// `Signer` + PG pool for per-tenant key lookup. `None` = signing
    /// disabled (paths stored without our signature; still serveable,
    /// just unverified). Arc because both PutPath branches need it and
    /// the inline branch doesn't have a good place to hold a reference
    /// across the await.
    signer: Option<Arc<TenantSigner>>,
    /// HMAC verifier for assignment tokens on PutPath. When Some, a
    /// PutPath without a valid `x-rio-assignment-token` metadata
    /// header → PERMISSION_DENIED. When Some + valid token: the
    /// uploaded path must be in `claims.expected_outputs`.
    ///
    /// None = accept all callers (dev mode, same as pre-Phase-3b).
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// HMAC verifier for `x-rio-service-token` (SEPARATE key from
    /// `hmac_verifier`). When Some + token verifies + `caller` is in
    /// `service_bypass_callers` → skip the assignment-token check.
    /// Transport-agnostic — see [`rio_auth::hmac::ServiceClaims`].
    service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// Service-token callers that may skip the assignment-token check
    /// via a valid `x-rio-service-token`. Default
    /// `["rio-gateway", "rio-scheduler"]`.
    service_bypass_callers: Vec<String>,
    /// Global budget for in-flight NAR bytes across ALL concurrent PutPath
    /// AND upstream-substitution handlers. Each acquires `chunk.len()`
    /// permits before extending its `nar_data: Vec<u8>`; permits release on
    /// handler drop. Default `8 * MAX_NAR_SIZE` (32 GiB) — lets 8× max-size
    /// uploads run in parallel before the 9th blocks. Configurable via
    /// `store.toml nar_buffer_budget_bytes` (or `.with_nar_budget()` in
    /// tests). main.rs wires this same `Arc<Semaphore>` into the
    /// `Substituter` so both ingest paths draw from one pool.
    ///
    /// NOT shared with GetPath's chunk cache — that's moka-bounded separately
    /// (chunk_cache above). This bounds ONLY the per-request accumulation
    /// Vec, which is the OOM vector: 10 × 4 GiB = 40 GiB RSS.
    // r[impl store.put.nar-bytes-budget+3]
    nar_bytes_budget: Arc<tokio::sync::Semaphore>,
    /// Upstream binary-cache substituter. `None` disables substitution
    /// (QueryPathInfo/GetPath miss → NotFound immediately, pre-P0462
    /// behavior). `Some` → on miss, try each of the requesting tenant's
    /// configured upstreams before returning NotFound.
    substituter: Option<Arc<Substituter>>,
    /// Max concurrent S3 chunk uploads per `put_chunked` call. Bounds
    /// the PutPath→S3 fan-out so a single large NAR (>1000 chunks)
    /// doesn't saturate the aws-sdk connection pool. Default
    /// [`cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY`] (32); override via
    /// `.with_chunk_upload_max_concurrent()`.
    chunk_upload_max_concurrent: usize,
    /// Cap on paths in a FindMissingPaths request (DoS guard). Default
    /// [`DEFAULT_MAX_BATCH_PATHS`] (1M); override via
    /// `.with_max_batch_paths()`.
    max_batch_paths: usize,
}

/// Default global NAR buffer budget: 8 × MAX_NAR_SIZE (32 GiB on 64-bit).
/// `tokio::sync::Semaphore` max permits is `usize::MAX >> 3`; this fits
/// comfortably on 64-bit.
const DEFAULT_NAR_BUDGET: usize = (8 * MAX_NAR_SIZE) as usize;

impl StoreServiceImpl {
    /// Create a new StoreService with inline-only storage (no chunking).
    ///
    /// All NARs go into `manifests.inline_blob` regardless of size.
    /// Existing test harnesses call this; they don't need a chunk backend.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            chunk_backend: None,
            chunk_cache: None,
            signer: None,
            hmac_verifier: None,
            service_verifier: None,
            service_bypass_callers: vec!["rio-gateway".to_string(), "rio-scheduler".to_string()],
            nar_bytes_budget: Arc::new(tokio::sync::Semaphore::new(DEFAULT_NAR_BUDGET)),
            substituter: None,
            chunk_upload_max_concurrent: cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            max_batch_paths: DEFAULT_MAX_BATCH_PATHS,
        }
    }

    /// Attach an externally-owned `ChunkCache`. Builder-style.
    ///
    /// The cache carries its backend inside (accessible via
    /// `ChunkCache::backend()`). StoreServiceImpl extracts it for
    /// the write path — PutPath calls `backend.put()` directly
    /// (no point caching freshly-written chunks nothing asked for).
    ///
    /// Use this when you want ONE cache shared across multiple
    /// services. main.rs constructs one `Arc<ChunkCache>`, passes
    /// clones here + to `ChunkServiceImpl::new` — a chunk warmed by
    /// either is hot for both. Without this call, the service is inline-only (all
    /// NARs go into `manifests.inline_blob` regardless of size).
    pub fn with_chunk_cache(mut self, cache: Arc<ChunkCache>) -> Self {
        self.chunk_backend = Some(cache.backend());
        self.chunk_cache = Some(cache);
        self
    }

    /// Enable upstream binary-cache substitution. Builder-style.
    /// Without this, QueryPathInfo/GetPath miss → NotFound directly.
    pub fn with_substituter(mut self, substituter: Arc<Substituter>) -> Self {
        self.substituter = Some(substituter);
        self
    }

    /// Enable HMAC verification on PutPath assignment tokens.
    /// Builder-style — chains after `new()` or `with_chunk_cache()`.
    pub fn with_hmac_verifier(mut self, verifier: rio_auth::hmac::HmacVerifier) -> Self {
        self.hmac_verifier = Some(Arc::new(verifier));
        self
    }

    /// Enable `x-rio-service-token` verification on PutPath. Builder-
    /// style. Verifier is keyed with `RIO_SERVICE_HMAC_KEY_PATH`
    /// (NOT the assignment key — separate secret). Takes `Arc` so
    /// `main.rs` can share one verifier with `StoreAdminServiceImpl`.
    pub fn with_service_hmac_verifier(
        mut self,
        verifier: Arc<rio_auth::hmac::HmacVerifier>,
    ) -> Self {
        self.service_verifier = Some(verifier);
        self
    }

    /// Set the `ServiceClaims.caller` allowlist for service-token
    /// bypass. Replaces the constructor default
    /// (`["rio-gateway", "rio-scheduler"]`).
    pub fn with_service_bypass_callers(mut self, callers: Vec<String>) -> Self {
        self.service_bypass_callers = callers;
        self
    }

    /// Enable narinfo signing with the given tenant-aware signer.
    ///
    /// Builder-style: `StoreServiceImpl::new(pool).with_signer(ts)`.
    /// Chains after either `new()` or `with_chunk_cache()`. The
    /// `TenantSigner` wraps the cluster key + pool — per-tenant key
    /// lookup happens at sign time, not construction time.
    pub fn with_signer(mut self, signer: TenantSigner) -> Self {
        self.signer = Some(Arc::new(signer));
        self
    }

    /// Borrow the signer Arc (PutPathBatch resolves tenant→key once
    /// per stream, not per output). Returning `Option<&Arc>` keeps
    /// the Arc-wrapping detail internal.
    pub fn signer(&self) -> Option<&Arc<TenantSigner>> {
        self.signer.as_ref()
    }

    /// Override the global NAR buffer budget (total permits across all
    /// concurrent PutPath handlers). Builder-style. Tests use small values
    /// (e.g., `10 * 4096`) to exercise backpressure without 32 GiB of RAM.
    pub fn with_nar_budget(mut self, bytes: usize) -> Self {
        self.nar_bytes_budget = Arc::new(tokio::sync::Semaphore::new(bytes));
        self
    }

    /// Override the per-call chunk-upload concurrency bound. Builder-style.
    /// main.rs threads `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` here. Tests can
    /// pass small N to exercise the bound without thousands of chunks.
    pub fn with_chunk_upload_max_concurrent(mut self, n: usize) -> Self {
        self.chunk_upload_max_concurrent = n;
        self
    }

    /// Override the FindMissingPaths batch-size cap. Builder-style.
    /// main.rs threads `RIO_MAX_BATCH_PATHS` here.
    pub fn with_max_batch_paths(mut self, n: usize) -> Self {
        self.max_batch_paths = n;
        self
    }

    /// Borrow the NAR-bytes budget semaphore. main.rs clones this into
    /// the `Substituter` so PutPath and substitution share ONE pool;
    /// tests inspect it to assert backpressure.
    pub fn nar_bytes_budget(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.nar_bytes_budget
    }

    /// Verify `x-rio-service-token` and return the allowlisted caller
    /// name. `None` when: no verifier configured, no header present,
    /// signature/expiry invalid, or `caller` not in
    /// [`Self::service_bypass_callers`]. Used by both the PutPath
    /// HMAC-bypass and the [`Self::request_tenant_id`]
    /// `x-rio-probe-tenant-id` gate.
    fn verified_service_caller<T>(&self, request: &Request<T>) -> Option<String> {
        let sv = self.service_verifier.as_ref()?;
        let tok = request
            .metadata()
            .get(rio_proto::SERVICE_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())?;
        let claims = sv.verify::<rio_auth::hmac::ServiceClaims>(tok).ok()?;
        self.service_bypass_callers
            .iter()
            .any(|a| a == &claims.caller)
            .then_some(claims.caller)
    }

    /// Extract `tenant_id` for substitution / tenant-filtering.
    ///
    /// Precedence:
    /// 1. JWT-interceptor extension (`x-rio-tenant-token` verified by
    ///    `rio_auth::jwt_interceptor`) — the gateway-forwarded path.
    /// 2. `x-rio-probe-tenant-id` header — ONLY honoured when the
    ///    request also carries a valid `x-rio-service-token` from an
    ///    allowlisted caller. Lets the scheduler (which can't forward a
    ///    JWT at dispatch time — `r[sched.dispatch.fod-substitute]`)
    ///    assert tenant context without opening an unauthenticated
    ///    self-select gap.
    ///
    /// `None` when neither path applies; downstream skips tenant-scoped
    /// behaviour (substitution probe, tenant-visibility filtering).
    fn request_tenant_id<T>(&self, request: &Request<T>) -> Option<uuid::Uuid> {
        if let Some(jwt) = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub)
        {
            return Some(jwt);
        }
        // r[impl sched.dispatch.fod-substitute]
        if self.verified_service_caller(request).is_some()
            && let Some(hdr) = request
                .metadata()
                .get(rio_proto::PROBE_TENANT_ID_HEADER)
                .and_then(|v| v.to_str().ok())
        {
            return hdr.parse().ok();
        }
        None
    }

    // r[impl store.api.batch-query+2]
    // r[impl store.api.batch-manifest+2]
    /// Reject gateway-forwarded end-user tenant tokens on the
    /// builder-internal batch RPCs (`BatchQueryPathInfo`,
    /// `BatchGetManifest`). These intentionally skip
    /// `r[store.substitute.tenant-sig-visibility]` (the gate would add
    /// per-path PG hits and defeat I-110); turning the documented
    /// "builder is the only caller, sends no token" into an enforced
    /// invariant means the skip can't be exploited as a tenant-side
    /// gate bypass.
    ///
    /// Anonymous (builder, no token) and service-token-with-probe
    /// (scheduler) pass through — only the JWT-interceptor extension
    /// (an `x-rio-tenant-token` the gateway forwarded from an ssh-ng
    /// client) is rejected. Zero PG cost.
    fn reject_end_user_tenant<T>(
        &self,
        request: &Request<T>,
        rpc: &'static str,
    ) -> Result<(), Status> {
        if request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .is_some()
        {
            return Err(Status::permission_denied(format!(
                "{rpc} is builder-internal; use the per-path RPC for tenant-scoped reads"
            )));
        }
        Ok(())
    }

    // r[impl store.substitute.upstream]
    /// On local miss: if the tenant has upstreams configured, try
    /// substituting. Returns `Ok(Some)` if fetched+ingested, `Ok(None)`
    /// on miss or if substitution is disabled/tenant-less.
    async fn try_substitute_on_miss(
        &self,
        tenant_id: Option<uuid::Uuid>,
        store_path: &str,
    ) -> Result<Option<ValidatedPathInfo>, Status> {
        let (Some(sub), Some(tid)) = (&self.substituter, tenant_id) else {
            return Ok(None);
        };
        sub.try_substitute(tid, store_path).await.map_err(|e| {
            tracing::warn!(error = %e, store_path, "substitution failed");
            // HashMismatch is intentionally NOT mapped here: per-upstream
            // hash mismatches are caught (and the integrity metric
            // emitted) inside `do_substitute`'s loop and swallowed as
            // try-next-upstream — they never escape `try_substitute`.
            // Only errors that abort the whole substitution reach this
            // arm.
            match e {
                SubstituteError::Fetch(_) => {
                    Status::unavailable("upstream substitute fetch failed")
                }
                SubstituteError::TooLarge { what, limit } => Status::resource_exhausted(format!(
                    "upstream substitute {what} exceeds {limit}-byte cap"
                )),
                // Transient: another uploader holds the placeholder.
                // Same observable client behavior as a miss on the
                // FIRST call; the SECOND call re-runs `do_substitute`
                // (moka didn't cache `Err`) and reaches `AlreadyComplete`.
                SubstituteError::Busy => {
                    Status::not_found("path not found (concurrent upload in progress)")
                }
                SubstituteError::HashMismatch { .. }
                | SubstituteError::SizeMismatch { .. }
                | SubstituteError::NarInfo(_)
                | SubstituteError::Ingest(_) => Status::internal("substitute ingest failed"),
            }
        })
    }

    /// Clean up an uploading placeholder after a PutPath error and record
    /// the error metric. Call this on any error path AFTER
    /// `insert_manifest_uploading` returned `Some(claim)` (i.e., we own the
    /// placeholder).
    async fn abort_upload(&self, store_path_hash: &[u8], claim: uuid::Uuid) {
        crate::ingest::abort_placeholder(
            &self.pool,
            self.chunk_backend.as_ref(),
            store_path_hash,
            claim,
        )
        .await;
        metrics::counter!("rio_store_put_path_total", "result" => "error").increment(1);
    }
}

#[tonic::async_trait]
impl StoreService for StoreServiceImpl {
    /// Upload a store path. See the `put_path` module for the write-ahead flow.
    #[instrument(skip(self, request), fields(rpc = "PutPath"))]
    async fn put_path(
        &self,
        request: Request<Streaming<PutPathRequest>>,
    ) -> Result<Response<PutPathResponse>, Status> {
        self.put_path_impl(request).await
    }

    /// Upload multiple store paths atomically. See the `put_path_batch`
    /// module for the one-transaction flow.
    #[instrument(skip(self, request), fields(rpc = "PutPathBatch"))]
    async fn put_path_batch(
        &self,
        request: Request<Streaming<PutPathBatchRequest>>,
    ) -> Result<Response<PutPathBatchResponse>, Status> {
        self.put_path_batch_impl(request).await
    }

    type GetPathStream = get_path::GetPathStream;

    /// Download a store path's NAR. See the `get_path` module for the streaming flow.
    #[instrument(skip(self, request), fields(rpc = "GetPath"))]
    async fn get_path(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<Self::GetPathStream>, Status> {
        self.get_path_impl(request).await
    }

    /// Query metadata for a single store path. See the `queries` module.
    #[instrument(skip(self, request), fields(rpc = "QueryPathInfo"))]
    async fn query_path_info(
        &self,
        request: Request<QueryPathInfoRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        self.query_path_info_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "BatchQueryPathInfo"))]
    async fn batch_query_path_info(
        &self,
        request: Request<BatchQueryPathInfoRequest>,
    ) -> Result<Response<BatchQueryPathInfoResponse>, Status> {
        self.batch_query_path_info_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "BatchGetManifest"))]
    async fn batch_get_manifest(
        &self,
        request: Request<BatchGetManifestRequest>,
    ) -> Result<Response<BatchGetManifestResponse>, Status> {
        self.batch_get_manifest_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "FindMissingPaths"))]
    async fn find_missing_paths(
        &self,
        request: Request<FindMissingPathsRequest>,
    ) -> Result<Response<FindMissingPathsResponse>, Status> {
        self.find_missing_paths_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "QueryPathFromHashPart"))]
    async fn query_path_from_hash_part(
        &self,
        request: Request<QueryPathFromHashPartRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        self.query_path_from_hash_part_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "AddSignatures"))]
    async fn add_signatures(
        &self,
        request: Request<AddSignaturesRequest>,
    ) -> Result<Response<AddSignaturesResponse>, Status> {
        self.add_signatures_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "RegisterRealisation"))]
    async fn register_realisation(
        &self,
        request: Request<RegisterRealisationRequest>,
    ) -> Result<Response<RegisterRealisationResponse>, Status> {
        self.register_realisation_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "QueryRealisation"))]
    async fn query_realisation(
        &self,
        request: Request<QueryRealisationRequest>,
    ) -> Result<Response<Realisation>, Status> {
        self.query_realisation_impl(request).await
    }

    #[instrument(skip(self, request), fields(rpc = "TenantQuota"))]
    async fn tenant_quota(
        &self,
        request: Request<TenantQuotaRequest>,
    ) -> Result<Response<TenantQuotaResponse>, Status> {
        self.tenant_quota_impl(request).await
    }

    /// ADR-023 phase-10: builder writes one `hw_perf_samples` row.
    /// Upsert on `(hw_class, pod_id)` — the `hw_perf_factors` view's
    /// median is over ALL rows (only the `HAVING` is distinct-pod), so
    /// without the UNIQUE constraint (M_046) a single pod spamming N
    /// inserts would dominate the median once 3 honest pods exist.
    ///
    /// **Identity from claims, not body.** The "one rank in a median"
    /// defense only holds if `pod_id` identifies the caller. Builders
    /// are untrusted (`r[sec.boundary.grpc-hmac]`), so `pod_id` is
    /// derived from the verified assignment-token's
    /// `claims.executor_id` (scheduler-signed at dispatch); the body
    /// `pod_id` is IGNORED. Without this gate a compromised builder
    /// could fabricate N distinct `pod_id` values and own the median
    /// for any `hw_class`. Service-token callers (gateway) have no
    /// business here and are rejected. Dev mode (`hmac_verifier` is
    /// None) falls back to the body field — same as PutPath dev-mode.
    ///
    /// `hw_class` and `factor` remain body-supplied: a valid token
    /// holder can write its one row to a foreign `hw_class`, but
    /// that's one rank in that class's median, bounded by
    /// `HW_FACTOR_SANITY_CEIL` in `HwTable`. `hw_class` is bounded at
    /// [`rio_common::limits::MAX_HW_CLASS_LEN`] chars of `[a-z0-9-]` —
    /// the unique key is `(hw_class, pod_id)`, so without a format
    /// bound a compromised builder could spam distinct multi-MB strings
    /// and fill the table (M_041's "one row per pod start" assumed
    /// honest callers).
    // r[impl sched.sla.hw-bench-append-only]
    // r[impl sec.boundary.grpc-hmac]
    #[instrument(skip(self, request), fields(rpc = "AppendHwPerfSample"))]
    async fn append_hw_perf_sample(
        &self,
        request: Request<AppendHwPerfSampleRequest>,
    ) -> Result<Response<()>, Status> {
        let pod_id = match self.verify_assignment_token(&request)? {
            Some(claims) => claims.executor_id,
            None if self.hmac_verifier.is_none() => request.get_ref().pod_id.clone(),
            None => {
                metrics::counter!("rio_store_hmac_rejected_total",
                                  "reason" => "service_caller_not_permitted")
                .increment(1);
                return Err(Status::permission_denied(
                    "AppendHwPerfSample: service-token callers not permitted; \
                     pod_id must come from an assignment token",
                ));
            }
        };
        let req = request.into_inner();
        if req.hw_class.is_empty()
            || req.hw_class.len() > rio_common::limits::MAX_HW_CLASS_LEN
            || !req
                .hw_class
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || pod_id.is_empty()
        {
            return Err(Status::invalid_argument(
                "hw_class must be 1-64 chars of [a-z0-9-]; pod_id required",
            ));
        }
        if !req.factor.is_finite() || req.factor <= 0.0 {
            return Err(Status::invalid_argument("factor must be finite and > 0"));
        }
        sqlx::query!(
            "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) VALUES ($1, $2, $3) \
             ON CONFLICT (hw_class, pod_id) \
             DO UPDATE SET factor = EXCLUDED.factor, measured_at = now()",
            req.hw_class,
            pod_id,
            req.factor,
        )
        .execute(&self.pool)
        .await
        .status_internal("AppendHwPerfSample: insert")?;
        Ok(Response::new(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `storage_error` maps `BackendAuthError` anywhere in the anyhow
    /// chain to `FailedPrecondition`. This is the contract the builder
    /// relies on to distinguish "retry" (`Internal`) from "give up"
    /// (`FailedPrecondition`).
    #[test]
    fn storage_error_auth_maps_to_failed_precondition() {
        use crate::backend::BackendAuthError;

        // The shape S3ChunkBackend::put() produces: marker at the root,
        // detailed message as context.
        let e = anyhow::Error::new(BackendAuthError)
            .context("S3 PutObject failed for chunks/ab/abab...: AccessDenied");
        let status = storage_error("test", e);
        assert_eq!(
            status.code(),
            tonic::Code::FailedPrecondition,
            "BackendAuthError must map to FailedPrecondition (non-retriable)"
        );
        // Message names the fix per feedback policy: "if code knows the
        // right value, put it in the error verbatim".
        assert!(
            status.message().contains("S3 credentials")
                || status.message().contains("IAM permissions"),
            "auth error message should name the fix, got: {}",
            status.message()
        );
    }

    /// Non-auth errors fall through to `Internal` — same behavior as
    /// the old `internal_error` (retriable).
    #[test]
    fn storage_error_other_maps_to_internal() {
        let e = anyhow::anyhow!("S3 PutObject failed: connection reset");
        let status = storage_error("test", e);
        assert_eq!(
            status.code(),
            tonic::Code::Internal,
            "non-auth error must map to Internal (retriable)"
        );
        assert_eq!(status.message(), "storage operation failed");
    }

    // r[verify sched.dispatch.fod-substitute]
    /// `x-rio-probe-tenant-id` is honoured ONLY behind a valid
    /// allowlisted service-token. An unauthenticated request (or one
    /// from a non-allowlisted caller) cannot self-select a tenant.
    #[tokio::test]
    async fn request_tenant_id_probe_header_gated_on_service_token() {
        use rio_auth::hmac::{HmacSigner, HmacVerifier, ServiceClaims};
        let key = b"probe-gate-test-key-32-bytes!!!!".to_vec();
        let pool = sqlx::PgPool::connect_lazy("postgres://unused").unwrap();
        let svc = StoreServiceImpl::new(pool)
            .with_service_hmac_verifier(Arc::new(HmacVerifier::from_key(key.clone())));
        let tid = uuid::Uuid::new_v4();
        let mk = |caller: Option<&str>| {
            let mut r = Request::new(());
            r.metadata_mut().insert(
                rio_proto::PROBE_TENANT_ID_HEADER,
                tid.to_string().parse().unwrap(),
            );
            if let Some(c) = caller {
                let tok = HmacSigner::from_key(key.clone()).sign(&ServiceClaims {
                    caller: c.into(),
                    expiry_unix: u64::MAX,
                });
                r.metadata_mut()
                    .insert(rio_proto::SERVICE_TOKEN_HEADER, tok.parse().unwrap());
            }
            r
        };
        // No service-token → ignored.
        assert_eq!(svc.request_tenant_id(&mk(None)), None);
        // Allowlisted caller → honoured.
        assert_eq!(svc.request_tenant_id(&mk(Some("rio-scheduler"))), Some(tid));
        // Non-allowlisted caller → ignored.
        assert_eq!(svc.request_tenant_id(&mk(Some("rogue"))), None);
    }

    /// `AppendHwPerfSample` is one-row-per-`(hw_class, pod_id)`: a
    /// second call upserts, it does not append. Regression for the
    /// "one rank in a median" claim — pre-M_046 a single pod could
    /// stuff the `hw_perf_factors` median with N rows.
    #[tokio::test]
    async fn append_hw_perf_sample_upserts_on_duplicate_pod() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreServiceImpl::new(db.pool.clone());
        let mk = |f: f64| {
            Request::new(AppendHwPerfSampleRequest {
                hw_class: "aws-8-ebs-hi".into(),
                pod_id: "p0".into(),
                factor: f,
            })
        };
        svc.append_hw_perf_sample(mk(0.9)).await.unwrap();
        svc.append_hw_perf_sample(mk(1.1)).await.unwrap();
        let (n, factor): (i64, f64) = sqlx::query_as(
            "SELECT count(*), max(factor) FROM hw_perf_samples \
             WHERE hw_class = 'aws-8-ebs-hi' AND pod_id = 'p0'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "duplicate (hw_class, pod_id) must upsert, not append");
        assert!((factor - 1.1).abs() < 1e-9, "upsert keeps latest factor");
    }
}
