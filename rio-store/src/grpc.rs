//! StoreService gRPC server implementation.
//!
//! Implements PutPath, GetPath, QueryPathInfo, FindMissingPaths, ContentLookup.

use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, instrument, warn};

use rio_proto::ChunkService;
use rio_proto::StoreService;
use rio_proto::types::{
    AddSignaturesRequest, AddSignaturesResponse, ContentLookupRequest, ContentLookupResponse,
    FindMissingChunksRequest, FindMissingChunksResponse, FindMissingPathsRequest,
    FindMissingPathsResponse, GetChunkRequest, GetChunkResponse, GetPathRequest, GetPathResponse,
    PathInfo, PutChunkRequest, PutChunkResponse, PutPathRequest, PutPathResponse,
    QueryPathFromHashPartRequest, QueryPathInfoRequest, QueryRealisationRequest, Realisation,
    RegisterRealisationRequest, RegisterRealisationResponse, get_path_response, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::backend::chunk::ChunkBackend;
use crate::cas::{self, ChunkCache};
use crate::metadata::{self, ManifestKind};
use crate::realisations;
use crate::signing::Signer;
use crate::validate::validate_nar_digest;

/// Stream a Bytes value to the GetPath channel in NAR_CHUNK_SIZE pieces.
///
/// Returns `false` if the client disconnected (send failed) — caller
/// should stop streaming. This is the one place the "slice into wire-
/// sized pieces + send" loop lives; both the inline and chunked paths
/// call it.
///
/// `.to_vec()` is one copy into the proto bytes field (protobuf needs
/// owned `Vec<u8>`, no way around it). The input `Bytes` stays valid
/// (Arc-refcounted), so for the chunked path this is the only copy
/// between ChunkCache and the wire.
async fn stream_bytes(
    tx: &tokio::sync::mpsc::Sender<Result<GetPathResponse, Status>>,
    bytes: &Bytes,
) -> bool {
    for slice in bytes.chunks(NAR_CHUNK_SIZE) {
        if tx
            .send(Ok(GetPathResponse {
                msg: Some(get_path_response::Msg::NarChunk(slice.to_vec())),
            }))
            .await
            .is_err()
        {
            return false; // client disconnected
        }
    }
    true
}

use std::sync::Arc;

use rio_proto::client::NAR_CHUNK_SIZE;

use rio_common::limits::MAX_NAR_SIZE;

/// Maximum number of paths in a FindMissingPaths request.
const MAX_BATCH_PATHS: usize = 10_000;

/// Validate a store path string: must parse as a well-formed Nix store path
/// (`/nix/store/<32-char-nixbase32>-<name>`). Rejects malformed paths, path
/// traversal attempts, and oversized strings at the RPC boundary.
fn validate_store_path(s: &str) -> Result<(), Status> {
    rio_nix::store_path::StorePath::parse(s)
        .map(|_| ())
        .map_err(|e| Status::invalid_argument(format!("invalid store path {s:?}: {e}")))
}

/// Log the full error server-side but return a generic message to the client.
/// Prevents leaking sqlx internals (connection strings, schema details) in
/// gRPC responses.
fn internal_error(context: &str, e: impl std::fmt::Display) -> Status {
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
}

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
        }
    }

    /// Create a StoreService with chunked storage enabled.
    ///
    /// NARs below `INLINE_THRESHOLD` (256 KiB) still go inline; larger
    /// ones are FastCDC-chunked. The cache wraps `backend` for reads.
    pub fn with_chunk_backend(pool: PgPool, backend: Arc<dyn ChunkBackend>) -> Self {
        // Cache holds its own Arc clone of the backend. `backend` is
        // also kept directly for the write path (PutPath's put_chunked
        // calls backend.put(), not cache — no point caching freshly-
        // written chunks that nothing has asked for yet).
        let cache = Arc::new(ChunkCache::new(Arc::clone(&backend)));
        Self {
            pool,
            chunk_backend: Some(backend),
            chunk_cache: Some(cache),
            signer: None,
        }
    }

    /// Enable narinfo signing with the given key.
    ///
    /// Builder-style: `StoreServiceImpl::new(pool).with_signer(key)`.
    /// Chains after either `new()` or `with_chunk_backend()`.
    pub fn with_signer(mut self, signer: Signer) -> Self {
        self.signer = Some(Arc::new(signer));
        self
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
    /// Upload a store path (streaming NAR data with metadata).
    ///
    /// PutPath flow (write-ahead pattern, phase 2c inline storage):
    /// 1. Receive first message: PutPathMetadata with PathInfo
    /// 2. Check idempotency: if path already complete, return success
    /// 3. Insert manifest placeholder with status='uploading'
    /// 4. Accumulate NAR chunks (bounded by declared size + tolerance)
    /// 5. Verify SHA-256 matches declared nar_hash
    /// 6. Store NAR in manifests.inline_blob + flip status to 'complete'
    ///
    /// E1 interim: ALL NARs go into inline_blob regardless of size. C3 adds
    /// the chunked path for NARs ≥ 256KB (FastCDC + S3 chunks + manifest_data).
    #[instrument(skip(self, request), fields(rpc = "PutPath"))]
    async fn put_path(
        &self,
        request: Request<Streaming<PutPathRequest>>,
    ) -> Result<Response<PutPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let start = std::time::Instant::now();
        let mut stream = request.into_inner();

        // Step 1: Receive the first message (must be metadata)
        let first_msg = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty PutPath stream"))?;

        let raw_info = match first_msg.msg {
            Some(put_path_request::Msg::Metadata(meta)) => meta
                .info
                .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo"))?,
            Some(put_path_request::Msg::NarChunk(_)) => {
                return Err(Status::invalid_argument(
                    "first PutPath message must be metadata, not nar_chunk",
                ));
            }
            Some(put_path_request::Msg::Trailer(_)) => {
                return Err(Status::invalid_argument(
                    "first PutPath message must be metadata, not trailer",
                ));
            }
            None => {
                return Err(Status::invalid_argument("PutPath message has no content"));
            }
        };

        // Trailer-mode is the ONLY mode: metadata.nar_hash must be empty,
        // hash arrives in the PutPathTrailer after all chunks. Both the
        // gateway (chunk_nar_for_put) and worker (single-pass tee upload)
        // send trailers. Hash-upfront was deleted in the pre-phase3a
        // cleanup — a non-empty nar_hash means an un-updated client.
        //
        // ValidatedPathInfo::try_from hard-fails on empty nar_hash, so we
        // fill a placeholder, validate, then overwrite after the trailer
        // arrives. The server computes the digest either way
        // (NarDigest::from_bytes below), so the security property
        // (client-declared hash matches server-computed) is unchanged.
        if !raw_info.nar_hash.is_empty() {
            return Err(Status::invalid_argument(
                "PutPath metadata.nar_hash must be empty (hash-upfront mode removed; \
                 send hash in PutPathTrailer)",
            ));
        }
        let mut raw_info = raw_info;
        // Placeholder so TryFrom passes. Overwritten after trailer.
        // 32 zero bytes — unambiguously NOT a real SHA-256 (would be
        // the hash of a specific ~2^256-rare preimage).
        raw_info.nar_hash = vec![0u8; 32];
        // nar_size is also 0 here — real value arrives in trailer.

        // Bound repeated fields BEFORE validation (TryFrom validates each
        // reference's syntax but doesn't bound the count; an attacker could
        // send 10M valid references and we'd parse them all before failing).
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

        // Centralized validation: store_path parses, nar_hash is 32 bytes
        // (placeholder), each reference parses.
        let mut info = ValidatedPathInfo::try_from(raw_info)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        // Compute store_path_hash if not provided
        let store_path_hash = if info.store_path_hash.is_empty() {
            info.store_path.sha256_digest().to_vec()
        } else {
            info.store_path_hash.clone()
        };
        debug!(
            store_path = %info.store_path.as_str(),
            "PutPath: received metadata"
        );

        // Step 2: Check idempotency — if path already complete, return success
        match metadata::check_manifest_complete(&self.pool, &store_path_hash).await {
            Ok(true) => {
                debug!(store_path = %info.store_path.as_str(), "PutPath: path already complete, returning success");
                // Drain remaining stream messages (protocol contract)
                drain_stream(&mut stream).await;
                metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
                return Ok(Response::new(PutPathResponse { created: false }));
            }
            Ok(false) => {} // Not yet complete, proceed
            Err(e) => {
                // Drain remaining stream messages before returning error
                drain_stream(&mut stream).await;
                return Err(internal_error("PutPath: check_manifest_complete", e));
            }
        }

        // Step 3: Insert manifest placeholder with status='uploading'.
        // Returns false (ON CONFLICT DO NOTHING no-op) if another uploader
        // already holds a placeholder. In that case we must NOT proceed: if
        // we do and later fail validation, delete_manifest_uploading would
        // delete the OTHER uploader's placeholder, losing their valid upload.
        let inserted = match metadata::insert_manifest_uploading(
            &self.pool,
            &store_path_hash,
            &info.store_path,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                drain_stream(&mut stream).await;
                return Err(metadata_status("PutPath: insert_manifest_uploading", e));
            }
        };
        if !inserted {
            // Another upload is in progress (or just completed in the window
            // between check_manifest_complete above and now). Re-check: if
            // it flipped to complete, return success; else tell client retry.
            drain_stream(&mut stream).await;
            match metadata::check_manifest_complete(&self.pool, &store_path_hash).await {
                Ok(true) => {
                    debug!(store_path = %info.store_path, "PutPath: concurrent upload won the race");
                    metrics::counter!("rio_store_put_path_total", "result" => "exists")
                        .increment(1);
                    return Ok(Response::new(PutPathResponse { created: false }));
                }
                _ => {
                    debug!(store_path = %info.store_path, "PutPath: concurrent upload in progress, aborting");
                    return Err(Status::aborted(
                        "concurrent PutPath in progress for this path; retry",
                    ));
                }
            }
        }

        // Step 4: Accumulate NAR chunks into a buffer.
        // Bound accumulation to prevent a malicious/buggy client OOMing us.
        // nar_size arrives in the trailer, so bound by MAX_NAR_SIZE during
        // accumulation; the trailer value is checked after.
        let mut nar_data = Vec::new();
        let mut trailer: Option<rio_proto::types::PutPathTrailer> = None;
        loop {
            let msg = match stream.message().await {
                Ok(Some(m)) => m,
                Ok(None) => break, // stream closed
                Err(e) => {
                    warn!(store_path = %info.store_path, error = %e, "PutPath: stream read error");
                    self.abort_upload(&store_path_hash).await;
                    return Err(e);
                }
            };
            match msg.msg {
                Some(put_path_request::Msg::NarChunk(chunk)) => {
                    // Chunk after trailer is a protocol violation — trailer
                    // MUST be last. Catches buggy clients that keep streaming
                    // after finalize() (would corrupt the hash validation).
                    if trailer.is_some() {
                        self.abort_upload(&store_path_hash).await;
                        return Err(Status::invalid_argument(
                            "PutPath: nar_chunk after trailer (trailer must be last)",
                        ));
                    }
                    let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
                    if new_len > MAX_NAR_SIZE {
                        warn!(
                            store_path = %info.store_path,
                            received = new_len,
                            "PutPath: NAR chunks exceed size bound, rejecting"
                        );
                        self.abort_upload(&store_path_hash).await;
                        return Err(Status::invalid_argument(format!(
                            "NAR chunks exceed size bound {MAX_NAR_SIZE} (received {new_len}+ bytes)",
                        )));
                    }
                    nar_data.extend_from_slice(&chunk);
                }
                Some(put_path_request::Msg::Trailer(t)) => {
                    if trailer.is_some() {
                        self.abort_upload(&store_path_hash).await;
                        return Err(Status::invalid_argument("PutPath: duplicate trailer"));
                    }
                    trailer = Some(t);
                    // Don't break — keep reading to catch chunk-after-trailer.
                    // A well-behaved client closes the stream right after
                    // sending the trailer, so this is one extra recv()
                    // that immediately returns None.
                }
                Some(put_path_request::Msg::Metadata(_)) => {
                    // Protocol violation: metadata must be first-message-only.
                    // A buggy client sending duplicate metadata with different
                    // nar_hash would have its "correction" silently ignored.
                    warn!(
                        store_path = %info.store_path,
                        "PutPath: duplicate metadata mid-stream, rejecting"
                    );
                    self.abort_upload(&store_path_hash).await;
                    return Err(Status::invalid_argument(
                        "PutPath stream contained duplicate metadata (protocol violation)",
                    ));
                }
                None => {
                    // Empty message, skip
                }
            }
        }

        // Trailer resolution: overwrite the placeholder hash/size with the
        // trailer values. Trailer is MANDATORY — stream close without one
        // is a protocol violation.
        let Some(t) = trailer else {
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(
                "PutPath: no trailer received \
                 (PutPathTrailer is required as the last message)",
            ));
        };
        let Ok(hash): Result<[u8; 32], _> = t.nar_hash.as_slice().try_into() else {
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(format!(
                "PutPath trailer nar_hash must be 32 bytes (SHA-256), got {}",
                t.nar_hash.len()
            )));
        };
        if t.nar_size > MAX_NAR_SIZE {
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(format!(
                "PutPath trailer nar_size {} exceeds maximum {MAX_NAR_SIZE}",
                t.nar_size
            )));
        }
        info.nar_hash = hash;
        info.nar_size = t.nar_size;

        // Step 5: Verify SHA-256. The NAR is already fully buffered in memory,
        // so use from_bytes (single pass over the slice) instead of wrapping in
        // a HashingReader + read_to_end into a second Vec (peak ~8 GiB for a
        // 4 GiB NAR).
        let digest = crate::validate::NarDigest::from_bytes(&nar_data);

        if let Err(e) = validate_nar_digest(&digest, &info.nar_hash, info.nar_size) {
            warn!(
                store_path = %info.store_path,
                error = %e,
                "PutPath: NAR validation failed"
            );
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(format!(
                "NAR validation failed: {e}"
            )));
        }

        // Step 6: Complete upload. Branches on size.
        let mut full_info = ValidatedPathInfo {
            store_path_hash,
            ..info
        };

        // Sign BEFORE writing narinfo. The signature goes into PG
        // alongside the other narinfo fields; the HTTP cache server
        // serves it from there without touching the privkey. Signing
        // now (not at serve time) means key rotation doesn't re-sign
        // old paths — they keep their old-key sig, which stays valid
        // as long as the old pubkey is in trusted-public-keys.
        self.maybe_sign(&mut full_info);

        // Size gate: small NARs inline, large NARs chunked (if backend
        // configured). `None` backend forces inline always — test
        // harnesses rely on this.
        //
        // The gate is on `nar_data.len()`, not `info.nar_size`. They should
        // match (validate_nar_digest above checks) but nar_data.len() is
        // what we actually have in hand — no chance of a drift where
        // info.nar_size says 200KB but we chunk 300KB.
        let use_chunked = self.chunk_backend.is_some() && nar_data.len() >= cas::INLINE_THRESHOLD;

        if use_chunked {
            // Chunked path: FastCDC + S3 + refcounts. cas::put_chunked
            // handles the whole write-ahead flow INCLUDING rollback on
            // its own errors. It consumed our step-3 placeholder; we
            // don't call abort_upload() on failure (that'd delete
            // a placeholder that no longer exists, or worse, one that
            // put_chunked's rollback just cleaned up).
            let backend = self.chunk_backend.as_ref().expect("checked is_some above");
            match cas::put_chunked(&self.pool, backend, &full_info, &nar_data).await {
                Ok(stats) => {
                    debug!(
                        store_path = %full_info.store_path.as_str(),
                        total_chunks = stats.total_chunks,
                        deduped = stats.deduped_chunks,
                        ratio = stats.dedup_ratio(),
                        "PutPath: chunked upload completed"
                    );
                    // The milestone metric. Gauge (per-upload ratio, not a
                    // running average — Prometheus rate() handles that).
                    metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
                }
                Err(e) => {
                    // put_chunked already rolled back. Just the error metric.
                    metrics::counter!("rio_store_put_path_total", "result" => "error").increment(1);
                    return Err(internal_error("PutPath: put_chunked", e));
                }
            }
        } else {
            // Inline path: single-tx NAR-in-inline_blob. Atomic — no
            // orphan cleanup needed (unlike the chunked path's two-step).
            if let Err(e) =
                metadata::complete_manifest_inline(&self.pool, &full_info, Bytes::from(nar_data))
                    .await
            {
                self.abort_upload(&full_info.store_path_hash).await;
                return Err(metadata_status("PutPath: complete_manifest_inline", e));
            }
            debug!(store_path = %full_info.store_path.as_str(), "PutPath: inline upload completed");
        }

        // Content-index the upload. nar_hash IS the content identity for
        // CA purposes: same bytes → same SHA-256. Two input-addressed
        // builds producing identical output get the same content_hash
        // entry, pointing at different store_paths (both rows kept, PK
        // is the pair). Best-effort — failure here doesn't fail the
        // upload (path is still addressable by store_path); log only.
        // Done AFTER the manifest commits so lookup's INNER JOIN on
        // manifests.status='complete' always sees a complete row.
        if let Err(e) = crate::content_index::insert(
            &self.pool,
            &full_info.nar_hash,
            &full_info.store_path_hash,
        )
        .await
        {
            tracing::warn!(
                store_path = %full_info.store_path.as_str(),
                error = %e,
                "content_index insert failed (path still addressable by store_path)"
            );
        }

        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        metrics::histogram!("rio_store_put_path_duration_seconds")
            .record(start.elapsed().as_secs_f64());
        Ok(Response::new(PutPathResponse { created: true }))
    }

    type GetPathStream = ReceiverStream<Result<GetPathResponse, Status>>;

    /// Download a store path's NAR data (streaming).
    ///
    /// Flow:
    /// 1. Look up narinfo + manifest from PostgreSQL
    /// 2. First response: PathInfo metadata
    /// 3. Stream NAR bytes — branch on inline vs chunked
    /// 4. Verify whole-NAR SHA-256 (belt-and-suspenders over per-chunk BLAKE3)
    ///
    /// The chunked path streams chunk-by-chunk without materializing the
    /// full NAR in memory — that's the whole point. K=8 parallel prefetch
    /// via `buffered()` (NOT `buffer_unordered` — chunk order matters for
    /// correct NAR reconstruction).
    #[instrument(skip(self, request), fields(rpc = "GetPath"))]
    async fn get_path(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<Self::GetPathStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Step 1: narinfo + manifest.
        let info = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: query_path_info", e))?
            .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?;

        // `None` here is defense-in-depth for a race where query_path_info
        // found the narinfo but get_manifest doesn't. Both filter on
        // manifests.status='complete', so in practice they agree.
        let manifest = metadata::get_manifest(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: get_manifest", e))?
            .ok_or_else(|| {
                Status::not_found(format!("manifest not found for: {}", req.store_path))
            })?;

        // Pre-flight: chunked manifest but no cache configured = we can't
        // serve this path. Inline-only stores (tests, or a misconfigured
        // deployment) hitting this means a PREVIOUS store instance wrote
        // chunked data and this one can't read it. Fail clearly rather
        // than the spawned task erroring with no context.
        if matches!(manifest, ManifestKind::Chunked(_)) && self.chunk_cache.is_none() {
            return Err(Status::failed_precondition(
                "path is stored chunked but this store instance has no chunk backend configured",
            ));
        }

        let expected_hash = info.nar_hash;
        let expected_size = info.nar_size;
        // Clone for the spawned task. Arc-clone is cheap; the cache
        // itself (moka + DashMap) is shared.
        let cache = self.chunk_cache.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let info_raw: PathInfo = info.into();

        rio_common::task::spawn_monitored("get-path-stream", async move {
            // Bound the entire streaming task. A slow client otherwise
            // keeps this alive forever.
            let stream_fut = async {
                // Step 2: First message is PathInfo.
                if tx
                    .send(Ok(GetPathResponse {
                        msg: Some(get_path_response::Msg::Info(info_raw)),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }

                // Step 3+4: stream + verify. Both branches feed the hasher
                // incrementally and check at the end. The chunked path
                // streams chunk-by-chunk; the inline path is one blob.
                let mut hasher = Sha256::new();
                let mut total_bytes = 0u64;

                match manifest {
                    ManifestKind::Inline(bytes) => {
                        hasher.update(&bytes);
                        total_bytes = bytes.len() as u64;
                        if !stream_bytes(&tx, &bytes).await {
                            return; // client disconnected
                        }
                    }
                    ManifestKind::Chunked(entries) => {
                        // Pre-flight checked cache is Some.
                        let cache = cache.expect("pre-flight checked chunk_cache is Some");

                        // K=8 parallel prefetch. `buffered()` preserves
                        // order — chunk i arrives before chunk i+1 even if
                        // i+1's fetch finishes first. `buffer_unordered`
                        // would scramble the NAR.
                        //
                        // Each future is a cache.get_verified() call.
                        // BLAKE3 verify happens inside that; any corrupt
                        // chunk surfaces as ChunkError here.
                        use futures_util::stream::{self, StreamExt};
                        const PREFETCH_K: usize = 8;

                        let mut chunk_stream = stream::iter(entries)
                            .map(|(hash, _size)| {
                                let cache = Arc::clone(&cache);
                                async move { cache.get_verified(&hash).await }
                            })
                            .buffered(PREFETCH_K);

                        while let Some(result) = chunk_stream.next().await {
                            let chunk_bytes = match result {
                                Ok(b) => b,
                                Err(e) => {
                                    error!(error = %e, "GetPath: chunk fetch/verify failed");
                                    // DATA_LOSS: the manifest says this
                                    // chunk exists, but we can't get
                                    // good bytes for it. S3 lost it,
                                    // or it's corrupt.
                                    let _ = tx
                                        .send(Err(Status::data_loss(format!(
                                            "chunk reassembly failed: {e}"
                                        ))))
                                        .await;
                                    return;
                                }
                            };
                            hasher.update(&chunk_bytes);
                            total_bytes += chunk_bytes.len() as u64;
                            if !stream_bytes(&tx, &chunk_bytes).await {
                                return; // client disconnected
                            }
                        }
                    }
                }

                // Step 4: whole-NAR SHA-256 verify. The chunked path
                // already BLAKE3-verified each chunk, so this is belt-
                // and-suspenders: catches (a) the manifest being WRONG
                // (right chunks, wrong order / missing one), (b) a bug
                // in our reassembly, (c) narinfo.nar_hash being stale.
                //
                // For inline, this is the PRIMARY check (no per-piece
                // verify for inline blobs).
                let actual: [u8; 32] = hasher.finalize().into();
                if actual != expected_hash || total_bytes != expected_size {
                    error!(
                        expected_hash = %hex::encode(expected_hash),
                        actual_hash = %hex::encode(actual),
                        expected_size,
                        total_bytes,
                        "GetPath: whole-NAR integrity check failed"
                    );
                    metrics::counter!("rio_store_integrity_failures_total").increment(1);
                    let _ = tx
                        .send(Err(Status::data_loss(
                            "whole-NAR integrity check failed (SHA-256 or size mismatch)",
                        )))
                        .await;
                }
            };

            if tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, stream_fut)
                .await
                .is_err()
            {
                warn!(
                    timeout = ?rio_common::grpc::GRPC_STREAM_TIMEOUT,
                    "GetPath streaming task timed out"
                );
                let _ = tx
                    .send(Err(Status::deadline_exceeded(
                        "GetPath streaming timed out",
                    )))
                    .await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
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

// ---------------------------------------------------------------------------
// ChunkService (phase2c C6)
// ---------------------------------------------------------------------------

/// Cap on FindMissingChunks batch size. 100k digests × 32 bytes = 3.2 MiB
/// of request body — well under the 32 MiB gRPC limit, generous enough
/// for any real PutPath (even a 4 GiB NAR at 16 KiB min chunks is 256k
/// chunks, which the client should split into multiple calls).
const MAX_CHUNK_DIGESTS: usize = 100_000;

/// ChunkService implementation.
///
/// These RPCs are **infrastructure** — not used by the current PutPath
/// flow (which calls `metadata::find_missing_chunks` directly, and uploads
/// via `ChunkBackend` inside `cas::put_chunked`). They exist so future
/// clients (a hypothetical worker-side chunker, or a store-to-store
/// replication tool) can interact at the chunk level without going
/// through PutPath's NAR-shaped API.
///
/// Shares `pool` and `chunk_cache` with `StoreServiceImpl` — one gRPC
/// server process, two services, same state. `Arc` lets main.rs construct
/// both from the same backing pieces.
pub struct ChunkServiceImpl {
    pool: PgPool,
    /// Cache for GetChunk. Same cache as GetPath uses — a chunk fetched
    /// by either RPC warms the other. `None` = ChunkService effectively
    /// disabled (all RPCs return FAILED_PRECONDITION); main.rs only
    /// constructs this when a chunk backend is configured.
    chunk_cache: Option<Arc<ChunkCache>>,
}

impl ChunkServiceImpl {
    pub fn new(pool: PgPool, chunk_cache: Option<Arc<ChunkCache>>) -> Self {
        Self { pool, chunk_cache }
    }

    /// Shared guard: all ChunkService RPCs need a cache. Without a
    /// backend, there's nothing to do at the chunk level.
    fn require_cache(&self) -> Result<&Arc<ChunkCache>, Status> {
        self.chunk_cache.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "ChunkService requires a chunk backend; this store is inline-only",
            )
        })
    }
}

#[tonic::async_trait]
impl ChunkService for ChunkServiceImpl {
    /// PutChunk: upload a single chunk directly.
    ///
    /// Currently returns UNIMPLEMENTED. Rationale: our PutPath flow is
    /// the ONLY chunk writer today — it chunks server-side and calls
    /// `ChunkBackend::put` directly (in `cas::put_chunked`). No client
    /// ever sends PutChunk. Implementing it would mean deciding how
    /// refcounts interact with standalone chunk uploads (a chunk with
    /// no manifest referencing it is immediately GC-eligible — useless).
    ///
    /// If/when a client-side chunker lands (worker chunks locally, sends
    /// chunks + manifest separately), this becomes real. Tag for that.
    ///
    /// TODO(phase3a): implement if client-side chunking lands. Needs
    /// refcount policy for standalone chunks (probably: chunk without
    /// manifest gets a short grace TTL before GC).
    async fn put_chunk(
        &self,
        _request: Request<Streaming<PutChunkRequest>>,
    ) -> Result<Response<PutChunkResponse>, Status> {
        Err(Status::unimplemented(
            "PutChunk not implemented; PutPath handles chunking server-side",
        ))
    }

    type GetChunkStream = ReceiverStream<Result<GetChunkResponse, Status>>;

    /// GetChunk: fetch a single chunk by BLAKE3 hash.
    ///
    /// Goes through the same `ChunkCache` as GetPath — a chunk warmed
    /// by GetPath is served from moka here, and vice versa. BLAKE3-
    /// verified (ChunkCache does that). Streams the chunk in one
    /// GetChunkResponse message (chunks are ≤256 KiB, no need to
    /// multi-message — GetPath's NAR_CHUNK_SIZE slicing is for the
    /// whole-NAR stream, not per-chunk).
    #[instrument(skip(self, request), fields(rpc = "GetChunk"))]
    async fn get_chunk(
        &self,
        request: Request<GetChunkRequest>,
    ) -> Result<Response<Self::GetChunkStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let cache = self.require_cache()?;
        let digest = request.into_inner().digest;

        let hash: [u8; 32] = digest.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "digest must be 32 bytes (BLAKE3), got {}",
                digest.len()
            ))
        })?;

        // Synchronous: await here (not in a spawned task). Chunks are
        // small (≤256 KiB) and the cache is fast (moka hit = instant,
        // miss = one S3 GET). The GetPath streaming-task pattern is
        // for large NARs where the stream outlives the handler call;
        // GetChunk's "stream" is one message.
        let bytes = cache.get_verified(&hash).await.map_err(|e| {
            use cas::ChunkError;
            match e {
                ChunkError::NotFound(_) => {
                    Status::not_found(format!("chunk {} not found", hex::encode(hash)))
                }
                ChunkError::Corrupt { .. } => Status::data_loss(format!(
                    "chunk {} failed BLAKE3 verification: {e}",
                    hex::encode(hash)
                )),
            }
        })?;

        // Single-message "stream". Channel buffer of 1 is enough; 2 is
        // belt-and-suspenders (one for the data, one in case tonic
        // wants to peek before forwarding).
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        // The send can't fail here (fresh channel, rx not dropped).
        // If it somehow does, the client gets an empty stream — same
        // as a disconnect, which they handle.
        let _ = tx
            .send(Ok(GetChunkResponse {
                data: bytes.to_vec(),
            }))
            .await;

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// FindMissingChunks: batch check which chunks the store has.
    ///
    /// Returns the subset of input digests that are NOT in the `chunks`
    /// table. The client calls this before PutChunk (or before deciding
    /// which chunks to send alongside a manifest) to skip re-uploading.
    ///
    /// Checks PG, not S3 — same reasoning as `metadata::find_missing_chunks`:
    /// PG is the source of truth for "chunks we know about", one
    /// roundtrip beats N S3 HeadObject calls.
    ///
    /// Also updates `rio_store_chunks_total` as a side effect. This is
    /// an infrequent RPC (once per PutPath-equivalent), so the count
    /// query piggybacking here is cheap. More accurate would be a
    /// periodic background task, but that's machinery for one gauge.
    #[instrument(skip(self, request), fields(rpc = "FindMissingChunks"))]
    async fn find_missing_chunks(
        &self,
        request: Request<FindMissingChunksRequest>,
    ) -> Result<Response<FindMissingChunksResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // No cache needed for this one (PG-only), but require it anyway:
        // if this store is inline-only, the ChunkService is conceptually
        // disabled, and "find missing chunks" is meaningless.
        let _ = self.require_cache()?;

        let req = request.into_inner();
        rio_common::grpc::check_bound("digests", req.digests.len(), MAX_CHUNK_DIGESTS)?;

        // Validate each digest length. A single bad one fails the batch
        // (client bug indicator — don't silently skip).
        for (i, d) in req.digests.iter().enumerate() {
            if d.len() != 32 {
                return Err(Status::invalid_argument(format!(
                    "digest[{i}] must be 32 bytes (BLAKE3), got {}",
                    d.len()
                )));
            }
        }

        // find_missing_chunks returns Vec<bool> (parallel missing-flags).
        // We need the actual missing DIGESTS. Zip + filter.
        let missing_flags = metadata::find_missing_chunks(&self.pool, &req.digests)
            .await
            .map_err(|e| metadata_status("FindMissingChunks", e))?;

        let missing_digests: Vec<Vec<u8>> = req
            .digests
            .into_iter()
            .zip(missing_flags)
            .filter_map(|(d, is_missing)| is_missing.then_some(d))
            .collect();

        // Piggyback: update the chunks_total gauge. Best-effort — if
        // the count query fails, log and move on (the client doesn't
        // care about our metrics).
        match metadata::count_chunks(&self.pool).await {
            Ok(count) => {
                metrics::gauge!("rio_store_chunks_total").set(count as f64);
            }
            Err(e) => {
                warn!(error = %e, "count_chunks failed; rio_store_chunks_total gauge stale");
            }
        }

        Ok(Response::new(FindMissingChunksResponse { missing_digests }))
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Drain remaining messages from a streaming request.
///
/// Must be called before returning early from PutPath to avoid leaving
/// unconsumed data on the gRPC transport. Bounded by DEFAULT_GRPC_TIMEOUT
/// to prevent a slow client from holding the handler indefinitely.
async fn drain_stream(stream: &mut Streaming<PutPathRequest>) {
    let drain = async {
        while let Ok(Some(_)) = stream.message().await {
            // discard
        }
    };
    if tokio::time::timeout(rio_common::grpc::DEFAULT_GRPC_TIMEOUT, drain)
        .await
        .is_err()
    {
        warn!(
            timeout = ?rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            "drain_stream timed out; client may be sending slowly"
        );
    }
}
