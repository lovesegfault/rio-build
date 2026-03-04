//! StoreService gRPC server implementation.
//!
//! Implements PutPath, GetPath, QueryPathInfo, FindMissingPaths.
//! ContentLookup returns UNIMPLEMENTED (Phase 2a stub).

use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, instrument, warn};

use rio_proto::store::chunk_service_server::ChunkService;
use rio_proto::store::store_service_server::StoreService;
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
use crate::cas;
use crate::metadata::{self, ManifestKind};
use crate::realisations;
use crate::validate::validate_nar_digest;

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

/// The StoreService gRPC server.
///
/// Phase 2c: NAR content lives in `manifests.inline_blob` (small NARs) or
/// as FastCDC chunks (large NARs). The `NarBackend` field from phase 2a
/// is gone — inline blobs are stored directly in PG.
pub struct StoreServiceImpl {
    pool: PgPool,
    /// Chunk storage for NARs ≥ INLINE_THRESHOLD. `None` disables chunking
    /// entirely (all NARs go inline, regardless of size). Tests use `None`
    /// or `MemoryChunkBackend`; prod uses `S3ChunkBackend`.
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
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
        }
    }

    /// Create a StoreService with chunked storage enabled.
    ///
    /// NARs below `INLINE_THRESHOLD` (256 KiB) still go inline; larger
    /// ones are FastCDC-chunked and uploaded via `backend`.
    pub fn with_chunk_backend(pool: PgPool, backend: Arc<dyn ChunkBackend>) -> Self {
        Self {
            pool,
            chunk_backend: Some(backend),
        }
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

        // Two upload modes (see types.proto PutPathRequest doc):
        //   - Hash-upfront: metadata.nar_hash non-empty. Trailer ignored.
        //     Used by gateway (wopAddToStoreNar has the hash before bytes).
        //   - Hash-trailer: metadata.nar_hash empty. Hash arrives in the
        //     trailer message after all chunks. Used by worker single-pass
        //     tee upload (C15).
        //
        // Detect mode NOW — ValidatedPathInfo::try_from hard-fails on empty
        // nar_hash, so in trailer mode we must fill a placeholder hash,
        // validate, then overwrite after the trailer arrives. The server
        // computes the digest either way (NarDigest::from_bytes below), so
        // the security property (client-declared hash matches server-
        // computed) holds in both modes.
        let is_trailer_mode = raw_info.nar_hash.is_empty();
        let mut raw_info = raw_info;
        if is_trailer_mode {
            // Placeholder so TryFrom passes. Overwritten after trailer.
            // 32 zero bytes — unambiguously NOT a real SHA-256 (would be
            // the hash of a specific ~2^256-rare preimage).
            raw_info.nar_hash = vec![0u8; 32];
            // nar_size=0 in trailer mode too — comes from trailer. Set a
            // safe upper bound for the chunk-accumulation check below.
            // We don't use raw_info.nar_size as the bound (client set it
            // to 0); use MAX_NAR_SIZE directly.
        }

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

        // Bound nar_size BEFORE allocation. A malicious client declaring
        // nar_size=u64::MAX would otherwise attempt a huge Vec::with_capacity.
        // In trailer mode, nar_size is 0 here — fine, we allocate nothing
        // upfront and grow. The MAX_NAR_SIZE check moves to the trailer.
        if raw_info.nar_size > MAX_NAR_SIZE {
            return Err(Status::invalid_argument(format!(
                "nar_size {} exceeds maximum {} bytes",
                raw_info.nar_size, MAX_NAR_SIZE
            )));
        }

        // Centralized validation: store_path parses, nar_hash is 32 bytes
        // (placeholder in trailer mode), each reference parses.
        let mut info = ValidatedPathInfo::try_from(raw_info)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        // Compute store_path_hash if not provided
        let store_path_hash = if info.store_path_hash.is_empty() {
            compute_store_path_hash(info.store_path.as_str())
        } else {
            info.store_path_hash.clone()
        };
        debug!(
            store_path = %info.store_path.as_str(),
            nar_size = info.nar_size,
            is_trailer_mode,
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
                return Err(internal_error("PutPath: insert_manifest_uploading", e));
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
        // Hash-upfront mode: declared nar_size + tolerance.
        // Trailer mode: nar_size is 0 (placeholder) → use MAX_NAR_SIZE.
        const NAR_SIZE_TOLERANCE: u64 = 4096;
        let max_allowed = if is_trailer_mode {
            MAX_NAR_SIZE
        } else {
            info.nar_size.saturating_add(NAR_SIZE_TOLERANCE)
        };
        // nar_size is already bounded by MAX_NAR_SIZE (4 GiB) above, which
        // fits in usize on 64-bit. try_from is defensive for 32-bit platforms
        // where MAX_NAR_SIZE (4_294_967_296) exceeds u32::MAX (4_294_967_295).
        let Ok(capacity) = usize::try_from(info.nar_size) else {
            // Practically unreachable on 64-bit (MAX_NAR_SIZE check above),
            // but clean up defensively.
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(
                "nar_size too large for this platform",
            ));
        };
        let mut nar_data = Vec::with_capacity(capacity);
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
                    if new_len > max_allowed {
                        warn!(
                            store_path = %info.store_path,
                            declared = info.nar_size,
                            received = new_len,
                            is_trailer_mode,
                            "PutPath: NAR chunks exceed size bound, rejecting"
                        );
                        self.abort_upload(&store_path_hash).await;
                        return Err(Status::invalid_argument(format!(
                            "NAR chunks exceed size bound {max_allowed} (received {new_len}+ bytes)",
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

        // Trailer-mode resolution: overwrite the placeholder hash/size with
        // the trailer values. If metadata had a real hash, the trailer is
        // IGNORED (backward compat — gateway still uses hash-upfront).
        if is_trailer_mode {
            let Some(t) = trailer else {
                self.abort_upload(&store_path_hash).await;
                return Err(Status::invalid_argument(
                    "PutPath: metadata.nar_hash is empty but no trailer received \
                     (trailer-mode upload requires a PutPathTrailer as the last message)",
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
        }

        // Step 5: Verify SHA-256. The NAR is already fully buffered in memory,
        // so use from_bytes (single pass over the slice) instead of wrapping in
        // a HashingReader + read_to_end into a second Vec (peak ~8 GiB for a
        // 4 GiB NAR).
        let digest = crate::validate::NarDigest::from_bytes(&nar_data);

        if let Err(e) = validate_nar_digest(&digest, &info.nar_hash, info.nar_size) {
            warn!(
                store_path = %info.store_path,
                error = %e,
                is_trailer_mode,
                "PutPath: NAR validation failed"
            );
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(format!(
                "NAR validation failed: {e}"
            )));
        }

        // Step 6: Complete upload. Branches on size.
        let full_info = ValidatedPathInfo {
            store_path_hash,
            ..info
        };

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
                return Err(internal_error("PutPath: complete_manifest_inline", e));
            }
            debug!(store_path = %full_info.store_path.as_str(), "PutPath: inline upload completed");
        }

        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        metrics::histogram!("rio_store_put_path_duration_seconds")
            .record(start.elapsed().as_secs_f64());
        Ok(Response::new(PutPathResponse { created: true }))
    }

    type GetPathStream = ReceiverStream<Result<GetPathResponse, Status>>;

    /// Download a store path's NAR data (streaming).
    ///
    /// GetPath flow (phase 2c inline storage):
    /// 1. Look up narinfo + manifest from PostgreSQL
    /// 2. First response message: PathInfo metadata
    /// 3. Subsequent messages: NAR data sliced into NAR_CHUNK_SIZE pieces
    /// 4. Verify content integrity via SHA-256 (detects DB corruption/bitrot)
    ///
    /// E1 interim: handles `ManifestKind::Inline` only. The `Chunked` arm is
    /// unreachable (PutPath always writes inline) but compiles so C5's
    /// chunked reassembly is purely additive.
    #[instrument(skip(self, request), fields(rpc = "GetPath"))]
    async fn get_path(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<Self::GetPathStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Step 1: Look up narinfo (for the first response message).
        let info = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| internal_error("GetPath: query_path_info", e))?
            .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?;

        // Look up the manifest (inline blob or chunk list).
        //
        // `None` here is defense-in-depth for a race where query_path_info
        // found the narinfo but get_manifest doesn't find the manifest. Both
        // queries filter on manifests.status='complete', so in practice they
        // agree. If they don't, the narinfo is orphaned — treat as NOT_FOUND.
        let manifest = metadata::get_manifest(&self.pool, &req.store_path)
            .await
            .map_err(|e| internal_error("GetPath: get_manifest", e))?
            .ok_or_else(|| {
                Status::not_found(format!("manifest not found for: {}", req.store_path))
            })?;

        let expected_hash = info.nar_hash;
        let expected_size = info.nar_size;

        let (tx, rx) = tokio::sync::mpsc::channel(16);

        let info_raw: PathInfo = info.into();
        rio_common::task::spawn_monitored("get-path-stream", async move {
            // Bound the entire streaming task. A slow-consuming client can
            // otherwise keep this task alive forever, leaking resources.
            let stream_fut = async {
                // First message: PathInfo
                if tx
                    .send(Ok(GetPathResponse {
                        msg: Some(get_path_response::Msg::Info(info_raw)),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }

                // Step 3: Stream the NAR content. Branch on storage kind.
                let nar_bytes = match manifest {
                    ManifestKind::Inline(bytes) => bytes,
                    ManifestKind::Chunked(_chunks) => {
                        // E1 interim: unreachable (PutPath always inline).
                        // C5 replaces this with reassemble_nar(chunks).
                        let _ = tx
                            .send(Err(Status::internal(
                                "chunked NAR reassembly not yet implemented (phase2c C5)",
                            )))
                            .await;
                        return;
                    }
                };

                // Slice into NAR_CHUNK_SIZE pieces. `Bytes::slice()` is
                // zero-copy (refcounted Arc-bump + offset); `.to_vec()` is
                // one copy into the outgoing proto bytes field (protobuf
                // needs owned Vec<u8>, no way around this one).
                for slice in nar_bytes.chunks(NAR_CHUNK_SIZE) {
                    if tx
                        .send(Ok(GetPathResponse {
                            msg: Some(get_path_response::Msg::NarChunk(slice.to_vec())),
                        }))
                        .await
                        .is_err()
                    {
                        return; // Client disconnected
                    }
                }

                // Step 4: Verify content integrity — the inline_blob in PG
                // may have been corrupted (TOAST storage bitrot, manual DB
                // tampering). SHA-256 over the whole blob; if it doesn't
                // match narinfo.nar_hash, send DATA_LOSS.
                //
                // This is cheap: the blob is already in memory (no disk
                // re-read), and SHA-256 over a few MB is sub-millisecond.
                // For the chunked path (C5), per-chunk BLAKE3 verification
                // happens during reassembly, and this whole-NAR SHA-256
                // stays as belt-and-suspenders.
                let digest = crate::validate::NarDigest::from_bytes(&nar_bytes);
                if let Err(e) = validate_nar_digest(&digest, &expected_hash, expected_size) {
                    error!(error = %e, "GetPath: content integrity check failed");
                    metrics::counter!("rio_store_integrity_failures_total").increment(1);
                    let _ = tx
                        .send(Err(Status::data_loss(format!(
                            "content integrity check failed: {e}"
                        ))))
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
            .map_err(|e| internal_error("QueryPathInfo: query_path_info", e))?
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
            .map_err(|e| internal_error("FindMissingPaths: find_missing_paths", e))?;

        Ok(Response::new(FindMissingPathsResponse {
            missing_paths: missing,
        }))
    }

    /// Content-addressed lookup: Phase 2a stub, returns UNIMPLEMENTED.
    #[instrument(skip(self, _request), fields(rpc = "ContentLookup"))]
    async fn content_lookup(
        &self,
        _request: Request<ContentLookupRequest>,
    ) -> Result<Response<ContentLookupResponse>, Status> {
        Err(Status::unimplemented(
            "ContentLookup is not implemented in Phase 2a",
        ))
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
            .map_err(|e| internal_error("QueryPathFromHashPart: query_by_hash_part", e))?
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
            .map_err(|e| internal_error("AddSignatures: append_signatures", e))?;

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
// ChunkService stub (Phase 2a: all RPCs return UNIMPLEMENTED)
// ---------------------------------------------------------------------------

/// Stub ChunkService that returns UNIMPLEMENTED for all RPCs.
///
/// Phase 2a stores full NARs (no chunking). This stub satisfies the proto
/// definition for spec compatibility.
pub struct ChunkServiceStub;

#[tonic::async_trait]
impl ChunkService for ChunkServiceStub {
    async fn put_chunk(
        &self,
        _request: Request<Streaming<PutChunkRequest>>,
    ) -> Result<Response<PutChunkResponse>, Status> {
        Err(Status::unimplemented(
            "ChunkService is not implemented in Phase 2a",
        ))
    }

    type GetChunkStream = ReceiverStream<Result<GetChunkResponse, Status>>;

    async fn get_chunk(
        &self,
        _request: Request<GetChunkRequest>,
    ) -> Result<Response<Self::GetChunkStream>, Status> {
        Err(Status::unimplemented(
            "ChunkService is not implemented in Phase 2a",
        ))
    }

    async fn find_missing_chunks(
        &self,
        _request: Request<FindMissingChunksRequest>,
    ) -> Result<Response<FindMissingChunksResponse>, Status> {
        Err(Status::unimplemented(
            "ChunkService is not implemented in Phase 2a",
        ))
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Compute SHA-256 hash of the store path string (used as primary key).
fn compute_store_path_hash(store_path: &str) -> Vec<u8> {
    Sha256::digest(store_path.as_bytes()).to_vec()
}

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
