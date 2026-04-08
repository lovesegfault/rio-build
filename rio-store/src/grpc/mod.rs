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

use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, instrument, warn};

use rio_proto::ChunkService;
use rio_proto::StoreService;
use rio_proto::client::NAR_CHUNK_SIZE;
use rio_proto::types::{
    AddSignaturesRequest, AddSignaturesResponse, BatchGetManifestRequest, BatchGetManifestResponse,
    BatchQueryPathInfoRequest, BatchQueryPathInfoResponse, ChunkRef, FindMissingPathsRequest,
    FindMissingPathsResponse, GetChunkRequest, GetChunkResponse, GetPathRequest, GetPathResponse,
    ManifestEntry, ManifestHint, PathInfo, PathInfoEntry, PutPathBatchRequest,
    PutPathBatchResponse, PutPathRequest, PutPathResponse, PutPathTrailer,
    QueryPathFromHashPartRequest, QueryPathInfoRequest, QueryRealisationRequest, Realisation,
    RegisterRealisationRequest, RegisterRealisationResponse, TenantQuotaRequest,
    TenantQuotaResponse, get_path_response, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;

use rio_common::grpc::StatusExt;
use rio_common::limits::MAX_NAR_SIZE;
use rio_common::tenant::NormalizedName;

use crate::backend::chunk::ChunkBackend;
use crate::cas::{self, ChunkCache};
use crate::metadata::{self, ManifestKind};
use crate::realisations;
use crate::signing::TenantSigner;
use crate::substitute::{SubstituteError, Substituter};
use crate::validate::validate_nar_digest;

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
/// `S3ChunkBackend::put` when the SDK error matches known auth
/// signatures). If present → `FailedPrecondition` with a message that
/// names the fix. Otherwise → same as [`rio_common::grpc::internal`].
///
/// [`BackendAuthError`]: crate::backend::chunk::BackendAuthError
pub(crate) fn storage_error(context: &str, e: anyhow::Error) -> Status {
    error!(context, error = %e, "storage backend error");
    // downcast_ref checks the innermost source; BackendAuthError is
    // always the root (anyhow::Error::new(BackendAuthError).context(...)).
    if e.downcast_ref::<crate::backend::chunk::BackendAuthError>()
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

// `validate_put_metadata` + `apply_trailer` live in
// `put_path::common` now (the fault-line NOTE that was here said to
// extract them once mod.rs crossed 1000L — it did). Re-exported via
// `put_path` so `put_path_batch`'s `use super::*` still resolves.
use put_path::{PlaceholderClaim, apply_trailer, validate_put_metadata};

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
    /// mTLS bypass: if `request.peer_certs()` is present AND the
    /// cert CN or any SAN DNSName is in `hmac_bypass_cns` → skip
    /// HMAC (gateway uploads don't have assignment tokens — `nix
    /// copy` just sends paths). This ties HMAC to mTLS: you can
    /// only bypass if you have a CA-signed cert whose identity is
    /// explicitly allowlisted.
    ///
    /// None = accept all callers (dev mode, same as pre-Phase-3b).
    hmac_verifier: Option<Arc<rio_common::hmac::HmacVerifier>>,
    /// Client-cert identities (CN or SAN DNSName) that bypass the
    /// HMAC check above. Default `["rio-gateway"]`. The bypass check
    /// reads peer_certs()[0], parses CN + SAN DNSNames, returns true
    /// if ANY of them appears in this list. Empty Vec = no bypass at
    /// all (every PutPath needs a token, including the gateway —
    /// not a supported deployment shape but the config allows it).
    ///
    /// Not `Arc<>` — `Vec<String>` is small (typically 1-3 entries)
    /// and `StoreServiceImpl` isn't cloned per-request (tonic shares
    /// one instance across all handlers via `&self`).
    hmac_bypass_cns: Vec<String>,
    /// Global budget for in-flight NAR bytes across ALL concurrent PutPath
    /// handlers. Each handler acquires `chunk.len()` permits before extending
    /// its `nar_data: Vec<u8>`; permits release on handler drop. Default
    /// `8 * MAX_NAR_SIZE` (32 GiB) — lets 8× max-size uploads run in parallel
    /// before the 9th blocks. Configurable via `store.toml
    /// nar_buffer_budget_bytes` (or `.with_nar_budget()` in tests).
    ///
    /// NOT shared with GetPath's chunk cache — that's moka-bounded separately
    /// (chunk_cache above). This bounds ONLY the per-request accumulation
    /// Vec, which is the OOM vector: 10 × 4 GiB = 40 GiB RSS.
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
    /// [`DEFAULT_MAX_BATCH_PATHS`] (100k); override via
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
            hmac_bypass_cns: vec!["rio-gateway".to_string()],
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
    /// clones here + to `ChunkServiceImpl::new` + to
    /// `CacheServerState` — a chunk warmed by any service is hot
    /// for all. Without this call, the service is inline-only (all
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
    pub fn with_hmac_verifier(mut self, verifier: rio_common::hmac::HmacVerifier) -> Self {
        self.hmac_verifier = Some(Arc::new(verifier));
        self
    }

    /// Set the CN/SAN-DNSName allowlist for HMAC bypass on PutPath.
    /// Replaces the constructor default (`["rio-gateway"]`). main.rs
    /// always calls this with the loaded config — config is the
    /// single source of truth. Tests can pass `vec![]` to disable
    /// bypass entirely or custom CNs to exercise the allowlist.
    pub fn with_hmac_bypass_cns(mut self, cns: Vec<String>) -> Self {
        self.hmac_bypass_cns = cns;
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

    /// Accessor for tests: inspect the budget semaphore directly to
    /// assert backpressure behavior without mocking the full PutPath
    /// streaming protocol.
    #[cfg(test)]
    pub(crate) fn nar_bytes_budget(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.nar_bytes_budget
    }

    /// Extract `tenant_id` from a request's JWT-interceptor extension.
    /// `None` when no interceptor is wired, no token was sent, or the
    /// caller is mTLS-bypassed. All three cases correctly skip
    /// substitution / tenant-filtering.
    fn request_tenant_id<T>(request: &Request<T>) -> Option<uuid::Uuid> {
        request
            .extensions()
            .get::<rio_common::jwt::TenantClaims>()
            .map(|c| c.sub)
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
            match e {
                SubstituteError::Fetch(_) => {
                    Status::unavailable("upstream substitute fetch failed")
                }
                SubstituteError::HashMismatch { .. } => {
                    metrics::counter!("rio_store_substitute_integrity_failures_total").increment(1);
                    Status::data_loss("upstream substitute NAR hash mismatch")
                }
                SubstituteError::NarInfo(_)
                | SubstituteError::Ingest(_)
                | SubstituteError::Chunked(_) => Status::internal("substitute ingest failed"),
            }
        })
    }

    /// Clean up an uploading placeholder after a PutPath error and record
    /// the error metric. Call this on any error path AFTER
    /// `insert_manifest_uploading` returned true (i.e., we own the placeholder).
    async fn abort_upload(&self, store_path_hash: &[u8]) {
        if let Err(e) = crate::gc::orphan::reap_one(
            &self.pool,
            store_path_hash,
            None,
            self.chunk_backend.as_ref(),
        )
        .await
        {
            error!(error = %e, "PutPath: failed to clean up placeholder during abort");
        }
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
        use crate::backend::chunk::BackendAuthError;

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
}
