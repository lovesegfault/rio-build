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
    AddSignaturesRequest, AddSignaturesResponse, ContentLookupRequest, ContentLookupResponse,
    FindMissingChunksRequest, FindMissingChunksResponse, FindMissingPathsRequest,
    FindMissingPathsResponse, GetChunkRequest, GetChunkResponse, GetPathRequest, GetPathResponse,
    PathInfo, PutChunkRequest, PutChunkResponse, PutPathRequest, PutPathResponse,
    QueryPathFromHashPartRequest, QueryPathInfoRequest, QueryRealisationRequest, Realisation,
    RegisterRealisationRequest, RegisterRealisationResponse, get_path_response, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;

use rio_common::limits::MAX_NAR_SIZE;

use crate::backend::chunk::ChunkBackend;
use crate::cas::{self, ChunkCache};
use crate::metadata::{self, ManifestKind};
use crate::realisations;
use crate::signing::Signer;
use crate::validate::validate_nar_digest;

mod admin;
mod chunk;
mod get_path;
mod put_path;

pub use admin::StoreAdminServiceImpl;
pub use chunk::ChunkServiceImpl;

/// Maximum number of paths in a FindMissingPaths request.
pub(crate) const MAX_BATCH_PATHS: usize = 10_000;

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
        M::PlaceholderMissing { .. } => {
            Status::aborted("upload placeholder concurrently deleted; retry")
        }
        M::CorruptManifest { .. } => Status::data_loss("stored manifest data is corrupt"),
        M::InvariantViolation(_) | M::MalformedRow(_) | M::Other(_) => {
            Status::internal("storage operation failed")
        }
    }
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
    /// ed25519 signing key for narinfo. `None` = signing disabled (paths
    /// stored without our signature; still serveable, just unverified).
    /// Arc because both PutPath branches need it and the inline branch
    /// doesn't have a good place to hold a reference across the await.
    signer: Option<Arc<Signer>>,
    /// HMAC verifier for assignment tokens on PutPath. When Some, a
    /// PutPath without a valid `x-rio-assignment-token` metadata
    /// header → PERMISSION_DENIED. When Some + valid token: the
    /// uploaded path must be in `claims.expected_outputs`.
    ///
    /// mTLS bypass: if `request.peer_certs()` is present AND the
    /// cert CN matches "rio-gateway" → skip HMAC (gateway uploads
    /// don't have assignment tokens — `nix copy` just sends paths).
    /// This ties HMAC to mTLS: you can only bypass if you have a
    /// CA-signed gateway cert.
    ///
    /// None = accept all callers (dev mode, same as pre-Phase-3b).
    hmac_verifier: Option<Arc<rio_common::hmac::HmacVerifier>>,
    /// Global budget for in-flight NAR bytes across ALL concurrent PutPath
    /// handlers. Each handler acquires `chunk.len()` permits before extending
    /// its `nar_data: Vec<u8>`; permits release on handler drop. Default
    /// `8 * MAX_NAR_SIZE` (32 GiB) — lets 8× max-size uploads run in parallel
    /// before the 9th blocks. Configurable via `.with_nar_budget()` for tests.
    /// TODO(phase4b): plumb `store.toml nar_buffer_budget_bytes`.
    ///
    /// NOT shared with GetPath's chunk cache — that's moka-bounded separately
    /// (chunk_cache above). This bounds ONLY the per-request accumulation
    /// Vec, which is the OOM vector: 10 × 4 GiB = 40 GiB RSS.
    nar_bytes_budget: Arc<tokio::sync::Semaphore>,
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
            nar_bytes_budget: Arc::new(tokio::sync::Semaphore::new(DEFAULT_NAR_BUDGET)),
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
            nar_bytes_budget: Arc::new(tokio::sync::Semaphore::new(DEFAULT_NAR_BUDGET)),
        }
    }

    /// Enable HMAC verification on PutPath assignment tokens.
    /// Builder-style — chains after `new()` or `with_chunk_cache()`.
    pub fn with_hmac_verifier(mut self, verifier: rio_common::hmac::HmacVerifier) -> Self {
        self.hmac_verifier = Some(Arc::new(verifier));
        self
    }

    /// Enable narinfo signing with the given key.
    ///
    /// Builder-style: `StoreServiceImpl::new(pool).with_signer(key)`.
    /// Chains after either `new()` or `with_chunk_cache()`.
    pub fn with_signer(mut self, signer: Signer) -> Self {
        self.signer = Some(Arc::new(signer));
        self
    }

    /// Override the global NAR buffer budget (total permits across all
    /// concurrent PutPath handlers). Builder-style. Tests use small values
    /// (e.g., `10 * 4096`) to exercise backpressure without 32 GiB of RAM.
    pub fn with_nar_budget(mut self, bytes: usize) -> Self {
        self.nar_bytes_budget = Arc::new(tokio::sync::Semaphore::new(bytes));
        self
    }

    /// Accessor for tests: inspect the budget semaphore directly to
    /// assert backpressure behavior without mocking the full PutPath
    /// streaming protocol.
    #[cfg(test)]
    pub(crate) fn nar_bytes_budget(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.nar_bytes_budget
    }

    /// If a signer is configured, compute the narinfo fingerprint and
    /// push a signature onto `info.signatures`.
    ///
    /// Called just before complete_manifest_* writes narinfo to PG —
    /// the signature goes into the DB, and the HTTP cache server serves
    /// it as a `Sig:` line without ever touching the privkey.
    ///
    /// No-op if signer is None. No error path: signing can't fail
    /// (ed25519 signing is pure math on valid inputs, and we control
    /// all inputs).
    fn maybe_sign(&self, info: &mut ValidatedPathInfo) {
        let Some(signer) = &self.signer else {
            return;
        };

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
        debug!(key = %signer.key_name(), "signed narinfo fingerprint");
        info.signatures.push(sig);
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
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        let info = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("QueryPathInfo: query_path_info", e))?
            .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?;

        Ok(Response::new(info.into()))
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
        let req = request.into_inner();

        // Bound request size to prevent DoS via huge path lists.
        rio_common::grpc::check_bound("paths", req.store_paths.len(), MAX_BATCH_PATHS)?;
        // Validate each path format. Reject the whole batch on any malformed
        // path (client bug indicator).
        for p in &req.store_paths {
            validate_store_path(p)?;
        }

        let missing = metadata::find_missing_paths(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("FindMissingPaths: find_missing_paths", e))?;

        Ok(Response::new(FindMissingPathsResponse {
            missing_paths: missing,
        }))
    }

    /// Content-addressed lookup: "have we ever seen these bytes?"
    ///
    /// Queries the content_index populated by PutPath. Empty
    /// `store_path` in the response = not found (proto convention;
    /// caller checks `.is_empty()` not Option).
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

        match crate::content_index::lookup(&self.pool, &req.content_hash).await {
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
            .map_err(|e| internal_error("RegisterRealisation: insert", e))?;

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
            .map_err(|e| internal_error("QueryRealisation: query", e))?
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::fixtures::{make_path_info, test_store_path};
    use tracing_test::traced_test;

    /// Build a StoreServiceImpl with a test signer but no DB/backend.
    /// `maybe_sign` only touches the signer, so pool can be dangling —
    /// we construct one that's never awaited (lazy connect).
    fn svc_with_signer() -> StoreServiceImpl {
        // 32-byte seed → Signer::parse accepts `name:base64(seed)` (seed-only
        // form; ed25519 derives pubkey deterministically).
        let seed_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0x42u8; 32]);
        let signer = Signer::parse(&format!("test-key-1:{seed_b64}")).expect("valid test signer");
        // Pool is lazy — never connects since maybe_sign doesn't touch it.
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool never connects");
        StoreServiceImpl::new(pool).with_signer(signer)
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

        svc.maybe_sign(&mut info);

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

        svc.maybe_sign(&mut info);

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

        svc.maybe_sign(&mut info);

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

        svc.maybe_sign(&mut info);

        assert!(!logs_contain("suspicious"));
        assert!(info.signatures.is_empty(), "no signer → no signature");
    }
}
