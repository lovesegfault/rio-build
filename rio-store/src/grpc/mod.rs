//! StoreService + ChunkService gRPC server implementations.
//!
//! Submodules:
//! - `put_path` — write-ahead upload flow (Steps 1-6)
//! - `get_path` — streaming NAR download (inline/chunked reassembly)
//! - `chunk` — ChunkService (GetChunk, FindMissingChunks, PutChunk stub)
//!
//! This file holds shared helpers, StoreServiceImpl, and the small RPCs
//! (QueryPathInfo, FindMissingPaths, etc.). put_path/get_path delegate
//! from the trait impl here to inherent methods in their submodules so
//! `self.pool`/`self.chunk_backend` field access works without making
//! fields `pub(super)`.

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
    BatchQueryPathInfoRequest, BatchQueryPathInfoResponse, ChunkRef, ContentLookupRequest,
    ContentLookupResponse, FindMissingChunksRequest, FindMissingChunksResponse,
    FindMissingPathsRequest, FindMissingPathsResponse, GetChunkRequest, GetChunkResponse,
    GetPathRequest, GetPathResponse, ManifestEntry, ManifestHint, PathInfo, PathInfoEntry,
    PutChunkRequest, PutChunkResponse, PutPathBatchRequest, PutPathBatchResponse, PutPathRequest,
    PutPathResponse, PutPathTrailer, QueryPathFromHashPartRequest, QueryPathInfoRequest,
    QueryRealisationRequest, Realisation, RegisterRealisationRequest, RegisterRealisationResponse,
    TenantQuotaRequest, TenantQuotaResponse, get_path_response, put_chunk_request,
    put_path_request,
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
use crate::substitute::Substituter;
use crate::validate::validate_nar_digest;

mod admin;
mod chunk;
mod get_path;
mod put_path;
mod put_path_batch;

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
        .map_err(|e| Status::invalid_argument(format!("invalid store path {s:?}: {e}")))
}

/// Log the full error server-side but return a generic message to the client.
/// Prevents leaking sqlx internals (connection strings, schema details) in
/// gRPC responses.
pub(crate) fn internal_error(context: &str, e: impl std::fmt::Display) -> Status {
    error!(context, error = %e, "internal error");
    Status::internal("storage operation failed")
}

/// Map a storage-backend anyhow error to a Status, distinguishing
/// permanent auth/config failures from transient ones.
///
/// [`internal_error`] maps everything to `Internal`, which a client
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
/// names the fix. Otherwise → same as `internal_error`.
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
/// clients fail fast. The old `internal_error()` everything-is-internal
/// mapping made a transient PG hiccup look the same as a corrupt database.
///
/// Logs the full error (including sqlx source chain) server-side; the
/// gRPC message is a scrubbed summary.
pub(crate) fn metadata_status(context: &str, e: metadata::MetadataError) -> Status {
    use metadata::MetadataError as M;
    error!(context, error = %e, "metadata layer error");
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

/// Validate a raw PathInfo message for PutPath/PutPathBatch.
///
/// Shared validation shared by both upload RPCs: (1) nar_hash-empty
/// enforcement (trailer-only mode), (2) references bound,
/// (3) signatures bound, (4) placeholder hash fill,
/// (5) ValidatedPathInfo::try_from, (6) HMAC path-in-claims check.
///
/// `ctx_label` goes into error messages for client-side disambiguation
/// ("PutPath" vs "output N").
///
/// Returns the validated info; on HMAC path-not-in-claims failure,
/// increments the `hmac_rejected_total{reason=path_not_in_claims}`
/// counter before erroring.
// NOTE(fault-line): validate_put_metadata and apply_trailer are
// pub(super), called ONLY from put_path.rs + put_path_batch.rs.
// Extraction to grpc/put_common.rs makes sense IF: a 3rd PUT-variant
// RPC lands, OR mod.rs crosses 1000L, OR collision count hits 20+.
// NOT doing it now: churn-on-churn post-P0345. The fault line is
// validate_put_metadata+apply_trailer → put_common.rs; tenant_quota
// and StoreService trait impls stay.
// r[impl sec.boundary.grpc-hmac]
pub(super) fn validate_put_metadata(
    mut raw_info: rio_proto::types::PathInfo,
    hmac_claims: Option<&rio_common::hmac::AssignmentClaims>,
    ctx_label: &str,
) -> Result<ValidatedPathInfo, Status> {
    // Step 1: trailer-only enforcement. metadata.nar_hash must be empty;
    // hash arrives in the PutPathTrailer after all chunks. Both gateway
    // (chunk_nar_for_put) and worker (single-pass tee upload) send
    // trailers. A non-empty nar_hash means an un-updated client.
    if !raw_info.nar_hash.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: metadata.nar_hash must be empty (hash-upfront mode removed; \
             send hash in PutPathTrailer)"
        )));
    }

    // Step 4: placeholder so TryFrom passes (it hard-fails on empty
    // nar_hash). Overwritten after trailer. 32 zero bytes — unambiguously
    // NOT a real SHA-256 (would be the hash of a specific ~2^256-rare
    // preimage). nar_size is also 0 here — real value from trailer.
    raw_info.nar_hash = vec![0u8; 32];

    // Steps 2-3: bound repeated fields BEFORE per-element validation
    // (TryFrom validates each reference's syntax but doesn't bound the
    // count; an attacker could send 10M valid references and we'd parse
    // them all before failing).
    rio_common::grpc::check_bound(
        "references",
        raw_info.references.len(),
        rio_common::limits::MAX_REFERENCES,
    )?;
    rio_common::grpc::check_bound(
        "signatures",
        raw_info.signatures.len(),
        rio_common::limits::MAX_SIGNATURES,
    )?;

    // Step 5: centralized validation — store_path parses, nar_hash is
    // 32 bytes (placeholder), each reference parses.
    let info = ValidatedPathInfo::try_from(raw_info).status_invalid(ctx_label)?;

    // Step 6: HMAC path-in-claims check. None = verifier disabled OR
    // mTLS bypass (gateway) → no check. Floating-CA (claims.is_ca) →
    // skip the membership check: the output path is computed post-build
    // from the NAR hash, so expected_outputs is [""] at sign time.
    // Threat model holds — token is still bound to drv_hash (worker
    // can't upload for a derivation it wasn't assigned), and
    // r[store.integrity.verify-on-put] below hashes the NAR stream
    // independently.
    if let Some(claims) = hmac_claims
        && !claims.is_ca
    {
        let path_str = info.store_path.as_str();
        if !claims.expected_outputs.iter().any(|o| o == path_str) {
            warn!(
                store_path = %path_str,
                executor_id = %claims.executor_id,
                drv_hash = %claims.drv_hash,
                "{ctx_label}: path not in assignment's expected_outputs",
            );
            metrics::counter!(
                "rio_store_hmac_rejected_total",
                "reason" => "path_not_in_claims"
            )
            .increment(1);
            return Err(Status::permission_denied(format!(
                "{ctx_label}: path not authorized by assignment token"
            )));
        }
    }

    Ok(info)
}

/// Apply a PutPathTrailer to a ValidatedPathInfo: 32-byte hash check,
/// nar_size bound, then overwrite the placeholder hash+size on `info`.
/// Caller handles async cleanup on error (abort_upload / bail!).
/// Callers that need the hash read `info.nar_hash` after the call.
pub(super) fn apply_trailer(
    info: &mut ValidatedPathInfo,
    t: &PutPathTrailer,
    ctx_label: &str,
) -> Result<(), Status> {
    let hash: [u8; 32] = t.nar_hash.as_slice().try_into().map_err(|_| {
        Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_hash must be 32 bytes (SHA-256), got {}",
            t.nar_hash.len()
        ))
    })?;
    if t.nar_size > MAX_NAR_SIZE {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_size {} exceeds maximum {MAX_NAR_SIZE}",
            t.nar_size
        )));
    }
    info.nar_hash = hash;
    info.nar_size = t.nar_size;
    Ok(())
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

    /// Create a StoreService with an externally-owned `ChunkCache`.
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
    /// for all. Constructing a fresh `ChunkCache` per service (each
    /// service has its own moka LRU + singleflight map), which
    /// defeats the cross-service-warm benefit.
    pub fn with_chunk_cache(pool: PgPool, cache: Arc<ChunkCache>) -> Self {
        Self {
            pool,
            chunk_backend: Some(cache.backend()),
            chunk_cache: Some(cache),
            signer: None,
            hmac_verifier: None,
            hmac_bypass_cns: vec!["rio-gateway".to_string()],
            nar_bytes_budget: Arc::new(tokio::sync::Semaphore::new(DEFAULT_NAR_BUDGET)),
            substituter: None,
            chunk_upload_max_concurrent: cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
            max_batch_paths: DEFAULT_MAX_BATCH_PATHS,
        }
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

    /// Clone the signer Arc for sharing with StoreAdminServiceImpl.
    /// ResignPaths needs the SAME cluster key as PutPath so re-signed
    /// narinfos verify against the same `trusted-public-keys` entry.
    /// Admin re-signing uses `ts.cluster()` directly — backfilled
    /// historical paths have no per-tenant attribution. Returning an
    /// `Option<Arc>` (rather than exposing the field) keeps the
    /// Arc-wrapping detail internal.
    pub fn signer(&self) -> Option<Arc<TenantSigner>> {
        self.signer.clone()
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
        sub.try_substitute(tid, store_path)
            .await
            .map_err(|e| internal_error("substitute", e))
    }

    // r[impl store.substitute.tenant-sig-visibility]
    /// Cross-tenant sig-visibility gate. A substituted path (one that
    /// was NEVER built by any tenant — zero `path_tenants` rows) is
    /// visible to the requesting tenant only if one of its `signatures`
    /// verifies against the requesting tenant's trusted set: upstream
    /// `trusted_keys` ∪ the rio cluster key.
    ///
    /// Returns `true` if visible, `false` if hidden (caller returns
    /// NotFound). Unauthenticated requests (`tenant_id = None`) pass
    /// through — `r[store.tenant.narinfo-filter]` defines anonymous
    /// requests as unfiltered.
    ///
    /// # Substituted-path discriminator
    ///
    /// `path_tenants` is populated at build-completion by the scheduler
    /// (`upsert_path_tenants` in rio-scheduler/src/db/live_pins.rs).
    /// The Substituter does NOT populate it. So:
    ///   - ≥1 `path_tenants` row → someone built this → skip gate
    ///     (built paths are trusted regardless of signature)
    ///   - 0 rows → substitution-only → apply gate
    ///
    /// The PutPath→scheduler timing window (path IS built, count=0
    /// because the scheduler hasn't yet run `upsert_path_tenants`) is
    /// handled by the cluster-key union below: a rio-signed path
    /// always verifies against the cluster key, so it passes the gate
    /// regardless of the tenant's upstream config. Only paths signed
    /// ONLY by upstream keys the tenant doesn't trust are gated out.
    async fn sig_visibility_gate(
        &self,
        tenant_id: Option<uuid::Uuid>,
        info: &ValidatedPathInfo,
    ) -> Result<bool, Status> {
        let Some(tid) = tenant_id else {
            return Ok(true); // anonymous → unfiltered
        };
        // No substituter → no substituted paths to gate.
        if self.substituter.is_none() {
            return Ok(true);
        }

        let path_hash = info.store_path.sha256_digest();

        // Two checks in one round-trip: does this tenant own it, and
        // has ANY tenant ever built it?
        let (owned, any_built): (bool, bool) = sqlx::query_as(
            "SELECT \
               bool_or(tenant_id = $2), \
               count(*) > 0 \
             FROM path_tenants WHERE store_path_hash = $1",
        )
        .bind(path_hash.as_slice())
        .bind(tid)
        .fetch_one(&self.pool)
        .await
        .map(|(o, a): (Option<bool>, bool)| (o.unwrap_or(false), a))
        .map_err(|e| internal_error("sig_visibility_gate: path_tenants", e))?;

        if owned || any_built {
            // Built path (someone `path_tenants`'d it). Not
            // substitution-only → skip gate. The freshly-PutPath'd
            // case (count=0) falls through to the cluster-key union
            // below — NOT this branch.
            return Ok(true);
        }

        // Zero path_tenants rows → substitution-only path. Gate it.
        // r[impl store.substitute.tenant-sig-visibility]
        // Trusted = tenant's upstream pubkeys ∪ rio cluster key.
        // Without the cluster key, a freshly-built path (rio-signed,
        // path_tenants not yet populated by the scheduler) would be
        // gated as "untrusted substitution" and return NotFound to
        // its own tenant during the PutPath→scheduler window.
        let mut trusted = metadata::upstreams::tenant_trusted_keys(&self.pool, tid)
            .await
            .map_err(|e| metadata_status("sig_visibility_gate: trusted_keys", e))?;
        if let Some(ts) = &self.signer {
            trusted.push(ts.cluster().trusted_key_entry());
            // r[impl store.key.rotation-cluster-history]
            // Union prior cluster keys so paths signed under a
            // rotated-out key stay visible after CASCADE drops their
            // path_tenants rows. prior_cluster_entries is loaded once
            // at startup from cluster_key_history WHERE retired_at IS
            // NULL — no DB hit here.
            trusted.extend_from_slice(ts.prior_cluster_entries());
        }
        if trusted.is_empty() {
            // Tenant trusts no upstream keys AND no signer configured
            // → any substituted path is invisible. With a signer the
            // cluster key was pushed above, so this is the no-signer
            // edge only.
            return Ok(false);
        }

        let fp = rio_nix::narinfo::fingerprint(
            info.store_path.as_str(),
            &info.nar_hash,
            info.nar_size,
            &info
                .references
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
        );
        Ok(crate::signing::any_sig_trusted(&info.signatures, &trusted, &fp).is_some())
    }

    /// Sync signing given a pre-resolved [`Signer`]. No DB hit.
    ///
    /// Extracted so PutPathBatch can resolve once + sign N times without
    /// N `get_active_signer` queries inside its phase-3 transaction.
    /// Holds the signature logic that was inlined in `maybe_sign`:
    /// empty-refs warn, fingerprint computation, `signer.sign()`,
    /// key-label debug line, push onto `info.signatures`.
    ///
    /// `was_tenant` drives the `key=tenant` vs `key=cluster` debug line;
    /// the caller passes whatever [`TenantSigner::resolve_once`] returned.
    fn sign_with_resolved(
        &self,
        signer: &crate::signing::Signer,
        was_tenant: bool,
        info: &mut ValidatedPathInfo,
    ) {
        // r[impl store.signing.empty-refs-warn]
        // Defensive: a non-CA path with zero references is almost certainly
        // a worker that didn't scan (pre-fix upload.rs) or a scanning bug.
        // CA paths legitimately have empty refs (fetchurl, etc.). Don't block
        // the upload — just make noise so it's visible in logs/alerts.
        if info.content_address.is_none() && info.references.is_empty() {
            warn!(
                store_path = %info.store_path.as_str(),
                "signing non-CA path with zero references — suspicious for non-leaf derivation; \
                 GC will not protect deps (check worker ref-scanner)"
            );
            metrics::counter!("rio_store_sign_empty_refs_total").increment(1);
        }

        // References for the fingerprint are FULL store paths (not
        // basenames — that's a narinfo-text-format thing). ValidatedPathInfo
        // stores them as StorePath, which stringifies to full paths.
        let refs: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();

        let fp = rio_nix::narinfo::fingerprint(
            info.store_path.as_str(),
            &info.nar_hash,
            info.nar_size,
            &refs,
        );

        let sig = signer.sign(&fp);
        let key_label = if was_tenant { "tenant" } else { "cluster" };
        debug!(key = key_label, "signed narinfo fingerprint");
        info.signatures.push(sig);
    }

    // r[impl store.tenant.sign-key]
    /// If a signer is configured, compute the narinfo fingerprint and
    /// push a signature onto `info.signatures` using the tenant's key
    /// (or cluster fallback — see [`TenantSigner::resolve_once`]).
    ///
    /// Called just before complete_manifest_* writes narinfo to PG —
    /// the signature goes into the DB, and the HTTP cache server serves
    /// it as a `Sig:` line without ever touching the privkey.
    ///
    /// `tenant_id` comes from JWT `Claims.sub` (P0259 interceptor). `None`
    /// means: no JWT (dual-mode fallback), OR mTLS bypass (gateway cert,
    /// `nix copy` path — no per-build attribution), OR dev mode (no
    /// interceptor). All three correctly fall through to cluster key.
    ///
    /// Async because tenant-key resolution hits PG for the `tenant_keys`
    /// lookup when `tenant_id` is `Some`. For single-output paths
    /// (PutPath) that's fine — one query, not in a hot loop. Batch
    /// callers (PutPathBatch) should use [`TenantSigner::resolve_once`]
    /// then [`sign_with_resolved`](Self::sign_with_resolved) instead so
    /// the lookup happens once outside the transaction, not N times
    /// inside it.
    ///
    /// Error handling: `TenantKeyLookup` (the only failing variant — the
    /// `None` path is infallible) is logged + falls back to cluster key.
    /// A transient PG hiccup shouldn't fail the upload; the cluster sig
    /// is still valid, just not tenant-scoped. The caller gets a
    /// signature either way — `maybe_sign` itself stays infallible.
    async fn maybe_sign(&self, tenant_id: Option<uuid::Uuid>, info: &mut ValidatedPathInfo) {
        let Some(ts) = &self.signer else {
            return;
        };

        let (signer, was_tenant) = match ts.resolve_once(tenant_id).await {
            Ok(pair) => pair,
            Err(e) => {
                // Transient PG failure — don't fail the upload. Fall back
                // to cluster key (sync, no DB hit). Log loud so ops
                // notices: a tenant WITH a configured key is now getting
                // cluster-signed paths, which `nix store verify
                // --trusted-public-keys tenant:<pk>` will reject. The
                // upload succeeds; the tenant's verify chain breaks.
                warn!(error = %e, ?tenant_id, "tenant-key lookup failed; falling back to cluster key");
                (ts.cluster().clone(), false)
            }
        };

        self.sign_with_resolved(&signer, was_tenant, info);
    }

    /// Clean up an uploading placeholder after a PutPath error and record
    /// the error metric. Call this on any error path AFTER
    /// `insert_manifest_uploading` returned true (i.e., we own the placeholder).
    async fn abort_upload(&self, store_path_hash: &[u8]) {
        if let Err(e) = metadata::delete_manifest_uploading(&self.pool, store_path_hash).await {
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

    /// Query metadata for a single store path.
    ///
    /// Only returns paths with manifests.status='complete'.
    #[instrument(skip(self, request), fields(rpc = "QueryPathInfo"))]
    async fn query_path_info(
        &self,
        request: Request<QueryPathInfoRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = Self::request_tenant_id(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        let local = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("QueryPathInfo: query_path_info", e))?;

        let info = match local {
            Some(i) => {
                // r[impl store.substitute.tenant-sig-visibility]
                // Local hit — but is it visible to THIS tenant? A path
                // substituted by tenant A with sig_mode=keep is
                // invisible to tenant B unless B trusts A's upstream
                // key. Hide-as-NotFound on gate failure.
                if !self.sig_visibility_gate(tenant_id, &i).await? {
                    // Gate failed — but B's upstreams may ALSO have
                    // this path. Try substituting (which will
                    // append B-trusted sigs to the existing row).
                    if let Some(sub) = self
                        .try_substitute_on_miss(tenant_id, &req.store_path)
                        .await?
                    {
                        sub
                    } else {
                        return Err(Status::not_found(format!(
                            "path not found: {}",
                            req.store_path
                        )));
                    }
                } else {
                    i
                }
            }
            None => {
                // Local miss — try upstream substitution.
                self.try_substitute_on_miss(tenant_id, &req.store_path)
                    .await?
                    .ok_or_else(|| {
                        Status::not_found(format!("path not found: {}", req.store_path))
                    })?
            }
        };

        Ok(Response::new(info.into()))
    }

    /// Batch query metadata for many paths in one PG round-trip.
    ///
    /// I-110: builder closure-BFS path. Local-only — NO upstream
    /// substitution and NO sig-visibility gate (both add per-path
    /// round-trips, defeating the batch). Callers needing those use
    /// `query_path_info`. The builder (the only current caller) sends
    /// no tenant token, so neither would apply anyway.
    #[instrument(skip(self, request), fields(rpc = "BatchQueryPathInfo"))]
    async fn batch_query_path_info(
        &self,
        request: Request<BatchQueryPathInfoRequest>,
    ) -> Result<Response<BatchQueryPathInfoResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Same DoS bound as FindMissingPaths.
        if req.store_paths.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {}; raise RIO_MAX_BATCH_PATHS to allow larger batches)",
                req.store_paths.len(),
                self.max_batch_paths
            )));
        }
        for p in &req.store_paths {
            validate_store_path(p)?;
        }

        let entries = metadata::query_path_info_batch(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("BatchQueryPathInfo: query_path_info_batch", e))?
            .into_iter()
            .map(|(store_path, info)| PathInfoEntry {
                store_path,
                info: info.map(Into::into),
            })
            .collect();

        Ok(Response::new(BatchQueryPathInfoResponse { entries }))
    }

    /// Batch (PathInfo, manifest) lookup for many paths in ≤2 PG round-trips.
    ///
    /// I-110c: builder FUSE-warm prefetch. Local-only — same caveats as
    /// `batch_query_path_info` (no upstream substitution, no
    /// sig-visibility gate).
    #[instrument(skip(self, request), fields(rpc = "BatchGetManifest"))]
    async fn batch_get_manifest(
        &self,
        request: Request<BatchGetManifestRequest>,
    ) -> Result<Response<BatchGetManifestResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Same DoS bound as BatchQueryPathInfo / FindMissingPaths.
        if req.store_paths.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {}; raise RIO_MAX_BATCH_PATHS to allow larger batches)",
                req.store_paths.len(),
                self.max_batch_paths
            )));
        }
        for p in &req.store_paths {
            validate_store_path(p)?;
        }

        let entries = metadata::get_manifest_batch(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("BatchGetManifest: get_manifest_batch", e))?
            .into_iter()
            .map(|(store_path, found)| {
                let hint = found.map(|(info, kind)| {
                    let (chunks, inline_blob) = match kind {
                        ManifestKind::Inline(b) => (Vec::new(), b.to_vec()),
                        ManifestKind::Chunked(entries) => (
                            entries
                                .into_iter()
                                .map(|(hash, size)| ChunkRef {
                                    hash: hash.to_vec(),
                                    size,
                                })
                                .collect(),
                            Vec::new(),
                        ),
                    };
                    ManifestHint {
                        info: Some(info.into()),
                        chunks,
                        inline_blob,
                    }
                });
                ManifestEntry { store_path, hint }
            })
            .collect();

        Ok(Response::new(BatchGetManifestResponse { entries }))
    }

    /// Batch check which paths are missing from the store.
    ///
    /// Only completed paths (manifests.status='complete') count as "present".
    #[instrument(skip(self, request), fields(rpc = "FindMissingPaths"))]
    async fn find_missing_paths(
        &self,
        request: Request<FindMissingPathsRequest>,
    ) -> Result<Response<FindMissingPathsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = Self::request_tenant_id(&request);
        let req = request.into_inner();

        // Bound request size to prevent DoS via huge path lists.
        // Inline (not check_bound) so the message names the env var.
        if req.store_paths.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {}; raise RIO_MAX_BATCH_PATHS to allow larger batches)",
                req.store_paths.len(),
                self.max_batch_paths
            )));
        }
        // Validate each path format. Reject the whole batch on any malformed
        // path (client bug indicator).
        for p in &req.store_paths {
            validate_store_path(p)?;
        }

        let missing = metadata::find_missing_paths(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("FindMissingPaths: find_missing_paths", e))?;

        // HEAD-probe each missing path against the tenant's upstreams.
        // Fails-open on probe errors (a down upstream shouldn't hide
        // paths the scheduler can otherwise substitute). Empty if no
        // substituter / no tenant / no upstreams — the normal case.
        let substitutable = match (&self.substituter, tenant_id) {
            (Some(sub), Some(tid)) if !missing.is_empty() => sub
                .check_available(tid, &missing)
                .await
                .unwrap_or_else(|e| {
                    warn!(error = %e, "check_available failed; empty substitutable_paths");
                    Vec::new()
                }),
            _ => Vec::new(),
        };

        Ok(Response::new(FindMissingPathsResponse {
            missing_paths: missing,
            substitutable_paths: substitutable,
        }))
    }

    /// Content-addressed lookup: "have we ever seen these bytes?"
    ///
    /// Queries the content_index populated by PutPath. Empty
    /// `store_path` in the response = not found (proto convention;
    /// caller checks `.is_empty()` not Option).
    // WONTFIX(P0432): no production callers — scheduler CA cutoff
    // switched to realisation-based lookup because ContentLookup's
    // self-exclusion is broken for CA (same content → same path →
    // excludes the only matching row; see actor/completion.rs). Kept
    // because 4 integration tests use it to verify content_index
    // population after PutPath/PutPathBatch, and store.md still
    // documents it. P0432 decides whether content_index itself is
    // dead and can cascade-remove this RPC + tests + docs.
    #[instrument(skip(self, request), fields(rpc = "ContentLookup"))]
    async fn content_lookup(
        &self,
        request: Request<ContentLookupRequest>,
    ) -> Result<Response<ContentLookupResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Validate hash length. 32 bytes = SHA-256. Anything else is
        // a client bug — reject with INVALID_ARGUMENT rather than
        // silently missing on the PG index.
        if req.content_hash.len() != 32 {
            return Err(Status::invalid_argument(format!(
                "content_hash must be 32 bytes (SHA-256), got {}",
                req.content_hash.len()
            )));
        }

        // Self-exclusion (store.content.self-exclude): empty string →
        // None (proto3 has no optional-string, empty is the sentinel).
        let exclude = if req.exclude_store_path.is_empty() {
            None
        } else {
            Some(req.exclude_store_path.as_str())
        };

        match crate::content_index::lookup(&self.pool, &req.content_hash, exclude).await {
            Ok(Some(info)) => Ok(Response::new(ContentLookupResponse {
                store_path: info.store_path.to_string(),
                info: Some(info.into()),
            })),
            Ok(None) => Ok(Response::new(ContentLookupResponse {
                store_path: String::new(),
                info: None,
            })),
            Err(e) => Err(internal_error("ContentLookup", e)),
        }
    }

    /// Resolve a store path from its 32-char nixbase32 hash part.
    #[instrument(skip(self, request), fields(rpc = "QueryPathFromHashPart"))]
    async fn query_path_from_hash_part(
        &self,
        request: Request<QueryPathFromHashPartRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Validate BEFORE touching PG. The hash-part flows into a LIKE
        // pattern (metadata::query_by_hash_part builds `/nix/store/{hash}-%`);
        // an unvalidated `%` or `_` would be LIKE-injection. nixbase32's
        // alphabet has neither (0-9, a-z minus e/o/t/u), so a successful
        // decode blocks that.
        //
        // 32 chars = 20 bytes of hash (Nix's compressHash output). Anything
        // else is a client bug, not a missing path — INVALID_ARGUMENT, not
        // NOT_FOUND.
        //
        // nixbase32::decode() checks BOTH length-validity AND charset in one
        // call. We throw away the decoded bytes — it's purely a validator
        // here. 20-byte allocation + discard; negligible next to the PG query.
        if req.hash_part.len() != rio_nix::store_path::HASH_CHARS {
            return Err(Status::invalid_argument(format!(
                "hash_part must be {} chars (nixbase32), got {}",
                rio_nix::store_path::HASH_CHARS,
                req.hash_part.len()
            )));
        }
        if let Err(e) = rio_nix::store_path::nixbase32::decode(&req.hash_part) {
            return Err(Status::invalid_argument(format!(
                "hash_part is not valid nixbase32: {e}"
            )));
        }

        let info = metadata::query_by_hash_part(&self.pool, &req.hash_part)
            .await
            .map_err(|e| metadata_status("QueryPathFromHashPart: query_by_hash_part", e))?
            .ok_or_else(|| {
                Status::not_found(format!("no path with hash part: {}", req.hash_part))
            })?;

        Ok(Response::new(info.into()))
    }

    /// Append signatures to an existing store path.
    #[instrument(skip(self, request), fields(rpc = "AddSignatures"))]
    async fn add_signatures(
        &self,
        request: Request<AddSignaturesRequest>,
    ) -> Result<Response<AddSignaturesResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Bound the signatures list — a malicious client could send 1M sigs
        // and we'd append them all. MAX_SIGNATURES matches PutPath's bound.
        rio_common::grpc::check_bound(
            "signatures",
            req.signatures.len(),
            rio_common::limits::MAX_SIGNATURES,
        )?;

        // Empty sigs list: no-op. Don't hit PG for nothing. Not an error —
        // `nix store sign` with no configured keys can legitimately produce
        // this (it sends the opcode but with zero sigs).
        if req.signatures.is_empty() {
            return Ok(Response::new(AddSignaturesResponse {}));
        }

        let rows = metadata::append_signatures(&self.pool, &req.store_path, &req.signatures)
            .await
            .map_err(|e| metadata_status("AddSignatures: append_signatures", e))?;

        if rows == 0 {
            return Err(Status::not_found(format!(
                "path not found: {}",
                req.store_path
            )));
        }

        Ok(Response::new(AddSignaturesResponse {}))
    }

    /// Register a CA derivation realisation.
    #[instrument(skip(self, request), fields(rpc = "RegisterRealisation"))]
    async fn register_realisation(
        &self,
        request: Request<RegisterRealisationRequest>,
    ) -> Result<Response<RegisterRealisationResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let proto = request
            .into_inner()
            .realisation
            .ok_or_else(|| Status::invalid_argument("realisation field is required"))?;

        // Validate hash lengths at the trust boundary. Proto bytes fields
        // are unbounded Vec<u8>; the DB layer expects [u8; 32]. Doing the
        // try_into here (not in realisations::insert) keeps the DB layer
        // free of proto-specific validation and gives a useful gRPC status
        // back to the client instead of an internal error.
        let drv_hash: [u8; 32] = proto.drv_hash.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "drv_hash must be 32 bytes (SHA-256), got {}",
                proto.drv_hash.len()
            ))
        })?;
        let output_hash: [u8; 32] = proto.output_hash.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "output_hash must be 32 bytes (SHA-256), got {}",
                proto.output_hash.len()
            ))
        })?;

        if proto.output_name.is_empty() {
            return Err(Status::invalid_argument("output_name must not be empty"));
        }
        // output_path validation: must be a well-formed store path. Same
        // check as PutPath — rejects traversal, bad nixbase32, etc.
        validate_store_path(&proto.output_path)?;

        // Bound sigs list. Same limit as narinfo.signatures.
        rio_common::grpc::check_bound(
            "signatures",
            proto.signatures.len(),
            rio_common::limits::MAX_SIGNATURES,
        )?;

        let r = realisations::Realisation {
            drv_hash,
            output_name: proto.output_name,
            output_path: proto.output_path,
            output_hash,
            signatures: proto.signatures,
        };

        realisations::insert(&self.pool, &r)
            .await
            .map_err(|e| metadata_status("RegisterRealisation: insert", e))?;

        Ok(Response::new(RegisterRealisationResponse {}))
    }

    /// Look up a CA derivation realisation.
    #[instrument(skip(self, request), fields(rpc = "QueryRealisation"))]
    async fn query_realisation(
        &self,
        request: Request<QueryRealisationRequest>,
    ) -> Result<Response<Realisation>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        let drv_hash: [u8; 32] = req.drv_hash.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "drv_hash must be 32 bytes (SHA-256), got {}",
                req.drv_hash.len()
            ))
        })?;
        if req.output_name.is_empty() {
            return Err(Status::invalid_argument("output_name must not be empty"));
        }

        let r = realisations::query(&self.pool, &drv_hash, &req.output_name)
            .await
            .map_err(|e| metadata_status("QueryRealisation: query", e))?
            .ok_or_else(|| {
                // Cache miss, not an error. Gateway maps this to an
                // empty-set wire response.
                Status::not_found(format!(
                    "no realisation for ({}, {})",
                    hex::encode(drv_hash),
                    req.output_name
                ))
            })?;

        Ok(Response::new(Realisation {
            drv_hash: r.drv_hash.to_vec(),
            output_name: r.output_name,
            output_path: r.output_path,
            output_hash: r.output_hash.to_vec(),
            signatures: r.signatures,
        }))
    }

    /// Per-tenant store usage + configured quota. Backs the gateway's
    /// pre-SubmitBuild quota gate (`r[store.gc.tenant-quota-enforce]`).
    ///
    /// Takes `tenant_name` (not UUID) so the gateway can call in
    /// dual-mode fallback (JWT disabled → no tenant_id resolved). The
    /// store owns the `tenants` table; joining on name here keeps the
    /// gateway PG-free.
    ///
    /// NOT_FOUND on unknown tenant — the gateway treats that as "no
    /// quota, pass through" (same as single-tenant mode: the empty
    /// tenant_name never hits this RPC at all).
    #[instrument(skip(self, request), fields(rpc = "TenantQuota"))]
    async fn tenant_quota(
        &self,
        request: Request<TenantQuotaRequest>,
    ) -> Result<Response<TenantQuotaResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Invalid name is a gateway bug here — the quota gate
        // short-circuits single-tenant mode BEFORE hitting this RPC.
        // Reject explicitly. `NameError::InteriorWhitespace` catches
        // the `"team a"` case that an ad-hoc `.trim()` would miss —
        // a rejected InvalidArgument with the specific error beats a
        // NOT_FOUND from the PG lookup downstream.
        let name = NormalizedName::new(&req.tenant_name).map_err(|e| {
            Status::invalid_argument(format!(
                "tenant_name invalid: {e} (gateway should gate single-tenant mode before calling)"
            ))
        })?;

        let quota = crate::gc::tenant::tenant_quota_by_name(&self.pool, &name)
            .await
            .map_err(|e| internal_error("TenantQuota: tenant_quota_by_name", e))?
            .ok_or_else(|| Status::not_found(format!("unknown tenant: {name}")))?;

        let (used, limit) = quota;
        // i64 → u64: used is SUM of non-negative nar_size, so ≥ 0.
        // limit is operator-set (CreateTenant validates range), so ≥ 0.
        // Both casts are safe; clamp defensively anyway — a negative
        // value here would mean PG corruption, and sending u64::MAX
        // on that path is better than a silent wrap.
        Ok(Response::new(TenantQuotaResponse {
            used_bytes: used.max(0) as u64,
            limit_bytes: limit.map(|l| l.max(0) as u64),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::fixtures::{make_path_info, test_store_path};
    use tracing_test::traced_test;

    /// Build a StoreServiceImpl with a test signer but no DB/backend.
    /// These tests pass `tenant_id: None` to `maybe_sign`, so the pool
    /// inside `TenantSigner` is never queried — lazy connect stays lazy.
    /// (The Some-tenant path IS tested by the integration test at
    /// `tests/grpc/signing.rs`, which has a real PG.)
    fn svc_with_signer() -> StoreServiceImpl {
        // 32-byte seed → Signer::parse accepts `name:base64(seed)` (seed-only
        // form; ed25519 derives pubkey deterministically).
        let seed_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0x42u8; 32]);
        let cluster = crate::signing::Signer::parse(&format!("test-key-1:{seed_b64}"))
            .expect("valid test signer");
        // Pool is lazy — never connects since these tests pass tenant_id=None
        // (the cluster-key path in resolve_once skips the DB entirely).
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool never connects");
        let ts = TenantSigner::new(cluster, pool.clone());
        StoreServiceImpl::new(pool).with_signer(ts)
    }

    /// r[verify store.signing.empty-refs-warn]
    /// Signing a non-CA path with zero references emits a warn! log
    /// containing "suspicious". The signing still proceeds (no block).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_warns_on_empty_refs_non_ca() {
        let svc = svc_with_signer();
        // make_path_info gives: references=[], content_address=None. Exactly
        // the suspicious case.
        let mut info = make_path_info(&test_store_path("suspect"), b"nar", [0u8; 32]);
        assert!(info.references.is_empty());
        assert!(info.content_address.is_none());

        svc.maybe_sign(None, &mut info).await;

        assert!(
            logs_contain("suspicious"),
            "expected warn! with 'suspicious' in message"
        );
        assert!(
            logs_contain("zero references"),
            "expected warn! to mention zero references"
        );
        // Signing still happened — warn is observability only, not a block.
        assert_eq!(info.signatures.len(), 1, "signing should still proceed");
    }

    /// r[verify store.signing.empty-refs-warn]
    /// CA paths with empty refs do NOT warn (fetchurl etc. legitimately
    /// have no runtime deps).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_no_warn_for_ca_path() {
        let svc = svc_with_signer();
        let mut info = make_path_info(&test_store_path("ca-path"), b"nar", [0u8; 32]);
        info.content_address = Some("fixed:r:sha256:abc".into());

        svc.maybe_sign(None, &mut info).await;

        assert!(
            !logs_contain("suspicious"),
            "CA path with empty refs should NOT warn"
        );
        assert_eq!(info.signatures.len(), 1);
    }

    /// r[verify store.signing.empty-refs-warn]
    /// Non-CA path WITH references does NOT warn (normal case).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_no_warn_with_references() {
        let svc = svc_with_signer();
        let mut info = make_path_info(&test_store_path("normal"), b"nar", [0u8; 32]);
        info.references =
            vec![rio_nix::store_path::StorePath::parse(&test_store_path("dep-a")).unwrap()];

        svc.maybe_sign(None, &mut info).await;

        assert!(
            !logs_contain("suspicious"),
            "path with refs should NOT warn"
        );
        assert_eq!(info.signatures.len(), 1);
    }

    /// No signer configured → maybe_sign is a no-op. No warn emitted
    /// (the early return is BEFORE the check — intentional: unsigned
    /// stores don't cryptographically commit to the empty refs, so the
    /// blast radius is smaller).
    #[tokio::test]
    #[traced_test]
    async fn maybe_sign_noop_without_signer() {
        let pool = PgPool::connect_lazy("postgres://unused").unwrap();
        let svc = StoreServiceImpl::new(pool); // no .with_signer()
        let mut info = make_path_info(&test_store_path("unsigned"), b"nar", [0u8; 32]);

        svc.maybe_sign(None, &mut info).await;

        assert!(!logs_contain("suspicious"));
        assert!(info.signatures.is_empty(), "no signer → no signature");
    }

    // r[verify store.substitute.tenant-sig-visibility]
    /// The critical cross-tenant test: tenant A substitutes path P
    /// signed by key K. Tenant B (who also trusts K) sees P. Tenant C
    /// (who doesn't trust K) gets NotFound.
    #[tokio::test]
    async fn sig_visibility_gate_cross_tenant() {
        use crate::signing::Signer;
        use rio_test_support::{TestDb, seed_tenant};

        let db = TestDb::new(&crate::MIGRATOR).await;
        // Gate only applies with substituter wired (`.is_none()`
        // short-circuits). The substituter itself won't be hit — the
        // path is pre-seeded, not miss-then-fetch.
        let sub = Arc::new(Substituter::new(db.pool.clone(), None));
        let svc = StoreServiceImpl::new(db.pool.clone()).with_substituter(sub);

        let tid_a = seed_tenant(&db.pool, "sig-gate-a").await;
        let tid_b = seed_tenant(&db.pool, "sig-gate-b").await;
        let tid_c = seed_tenant(&db.pool, "sig-gate-c").await;

        // Seed a path with a signature from key K.
        let seed_k = [0x77u8; 32];
        let signer_k = Signer::from_seed("key-K", &seed_k);
        let pk_k = ed25519_dalek::SigningKey::from_bytes(&seed_k).verifying_key();
        let trusted_k = format!(
            "key-K:{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pk_k.as_bytes())
        );

        let path = test_store_path("cross-tenant-p");
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"xyz");
        let fp = rio_nix::narinfo::fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        let sig_k = signer_k.sign(&fp);

        // Seed the path in narinfo + manifests with K's sig — simulating
        // "tenant A substituted this from upstream K".
        let info = make_path_info(&path, &nar, nar_hash);
        let path_hash = info.store_path.sha256_digest();
        metadata::insert_manifest_uploading(&db.pool, &path_hash, &path, &[])
            .await
            .unwrap();
        let mut info_with_sig = info.clone();
        info_with_sig.signatures = vec![sig_k.clone()];
        info_with_sig.store_path_hash = path_hash.to_vec();
        metadata::complete_manifest_inline(&db.pool, &info_with_sig, nar.into())
            .await
            .unwrap();

        // — Tenant A substituted this (so: no path_tenants row, but
        //   A's upstream trusted_keys includes K) —
        // — Tenant B ALSO trusts K (different upstream URL, same key) —
        // — Tenant C trusts a DIFFERENT key J —
        // — Zero path_tenants rows: this is a substitution-only path —
        let _ = path_hash; // narinfo seeded above, hash no longer needed

        metadata::upstreams::insert(
            &db.pool,
            tid_a,
            "https://cache-k-a.example",
            50,
            std::slice::from_ref(&trusted_k),
            crate::metadata::SigMode::Keep,
        )
        .await
        .unwrap();

        // Tenant B trusts key K via an upstream config.
        metadata::upstreams::insert(
            &db.pool,
            tid_b,
            "https://cache-k.example",
            50,
            std::slice::from_ref(&trusted_k),
            crate::metadata::SigMode::Keep,
        )
        .await
        .unwrap();

        // Tenant C has an upstream but trusts a DIFFERENT key.
        metadata::upstreams::insert(
            &db.pool,
            tid_c,
            "https://cache-j.example",
            50,
            &["key-J:aaaa".into()],
            crate::metadata::SigMode::Keep,
        )
        .await
        .unwrap();

        let stored = metadata::query_path_info(&db.pool, &path)
            .await
            .unwrap()
            .unwrap();

        // A: trusts K (the substituting tenant) → sig verifies → visible.
        assert!(
            svc.sig_visibility_gate(Some(tid_a), &stored).await.unwrap(),
            "tenant A trusts K → visible"
        );

        // B: trusts K → sig verifies → visible.
        assert!(
            svc.sig_visibility_gate(Some(tid_b), &stored).await.unwrap(),
            "tenant B trusts K → visible"
        );

        // C: doesn't trust K → hidden.
        assert!(
            !svc.sig_visibility_gate(Some(tid_c), &stored).await.unwrap(),
            "tenant C doesn't trust K → NotFound"
        );

        // Anonymous → passes through (unfiltered per
        // r[store.tenant.narinfo-filter]).
        assert!(
            svc.sig_visibility_gate(None, &stored).await.unwrap(),
            "anonymous → unfiltered"
        );

        // — Built-path exemption: once ANY tenant has a path_tenants
        //   row, the gate is bypassed (built paths are trusted) —
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(stored.store_path.sha256_digest().as_slice())
            .bind(tid_a)
            .execute(&db.pool)
            .await
            .unwrap();

        // Now even C (who doesn't trust K) sees it — built paths skip
        // the gate.
        assert!(
            svc.sig_visibility_gate(Some(tid_c), &stored).await.unwrap(),
            "built path (any path_tenants row) → gate bypassed"
        );
    }

    // r[verify store.key.rotation-cluster-history]
    /// Cluster-key rotation: path signed under old key A stays visible
    /// after rotating to key B + CASCADE deleting the owning tenant.
    ///
    /// Pre-fix: step 4 returns false. Gate derives key B from the
    /// current Signer only; sig was made by A; no path_tenants row left
    /// to bypass → path goes dark for every other tenant.
    ///
    /// Post-fix: prior_cluster carries A's pubkey entry → gate unions
    /// {B, A} into trusted → A-sig verifies.
    #[tokio::test]
    async fn sig_gate_survives_cluster_key_rotation_with_cascaded_tenant() {
        use crate::signing::{Signer, TenantSigner};
        use rio_test_support::{TestDb, seed_tenant};

        let db = TestDb::new(&crate::MIGRATOR).await;
        let sub = Arc::new(Substituter::new(db.pool.clone(), None));

        // — Cluster key A: the OLD key. Sign the path with this. —
        let seed_a = [0xAAu8; 32];
        let cluster_a = Signer::from_seed("rio-cluster-1", &seed_a);
        let entry_a = cluster_a.trusted_key_entry();

        // — Cluster key B: the NEW key. Active Signer post-rotation. —
        let seed_b = [0xBBu8; 32];
        let cluster_b = Signer::from_seed("rio-cluster-2", &seed_b);

        assert_ne!(seed_a, seed_b, "precondition: distinct keys");
        assert_ne!(
            entry_a,
            cluster_b.trusted_key_entry(),
            "precondition: distinct trusted-key entries"
        );

        // 1. Seed path signed by cluster key A (no tenant sig, no
        //    upstream sig — pure rio-signed built path).
        let path = test_store_path("rotation-survivor");
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"rot");
        let fp = rio_nix::narinfo::fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        let sig_a = cluster_a.sign(&fp);

        let info = make_path_info(&path, &nar, nar_hash);
        let path_hash = info.store_path.sha256_digest();
        metadata::insert_manifest_uploading(&db.pool, &path_hash, &path, &[])
            .await
            .unwrap();
        let mut info_with_sig = info.clone();
        info_with_sig.signatures = vec![sig_a];
        info_with_sig.store_path_hash = path_hash.to_vec();
        metadata::complete_manifest_inline(&db.pool, &info_with_sig, nar.into())
            .await
            .unwrap();

        // 2. Seed path_tenants row for tenant T (path was "built by T").
        let tid_t = seed_tenant(&db.pool, "rotation-owner").await;
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(path_hash.as_slice())
            .bind(tid_t)
            .execute(&db.pool)
            .await
            .unwrap();

        // 3. Rotate: active Signer = B, prior_cluster = [A's entry].
        //    Route I — via with_prior_cluster (equivalent to what
        //    main.rs does via load_prior_cluster at startup after an
        //    operator inserts A into cluster_key_history).
        let ts_rotated =
            TenantSigner::new(cluster_b, db.pool.clone()).with_prior_cluster(vec![entry_a]);
        let svc = StoreServiceImpl::new(db.pool.clone())
            .with_substituter(sub.clone())
            .with_signer(ts_rotated);

        // 4. CASCADE: delete tenant T → path_tenants row drops. The
        //    path is now path_tenants-orphaned: gate re-fires on the
        //    next read.
        sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
            .bind(tid_t)
            .execute(&db.pool)
            .await
            .unwrap();
        // Verify CASCADE actually dropped the row (belt-and-suspenders —
        // migration 012's ON DELETE CASCADE is what we're relying on).
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants WHERE store_path_hash = $1")
                .bind(path_hash.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "CASCADE should have dropped path_tenants row");

        let stored = metadata::query_path_info(&db.pool, &path)
            .await
            .unwrap()
            .unwrap();

        // 5. Other tenant queries → visible (prior_cluster carries A).
        let tid_other = seed_tenant(&db.pool, "rotation-reader").await;
        assert!(
            svc.sig_visibility_gate(Some(tid_other), &stored)
                .await
                .unwrap(),
            "path signed under old cluster key A MUST stay visible after \
             rotation to B when A is in prior_cluster — this is the \
             CASCADE-survival property"
        );

        // — Negative control: same rotation WITHOUT prior_cluster →
        //   path goes dark. Proves the test isn't passing for the
        //   wrong reason (e.g. some other bypass). —
        let ts_no_history =
            TenantSigner::new(Signer::from_seed("rio-cluster-2", &seed_b), db.pool.clone());
        let svc_no_history = StoreServiceImpl::new(db.pool.clone())
            .with_substituter(sub)
            .with_signer(ts_no_history);
        assert!(
            !svc_no_history
                .sig_visibility_gate(Some(tid_other), &stored)
                .await
                .unwrap(),
            "negative control: WITHOUT prior_cluster, old-key path MUST \
             be invisible (this is the bug P0521 fixes)"
        );
    }

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
