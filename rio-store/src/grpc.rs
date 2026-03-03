//! StoreService gRPC server implementation.
//!
//! Implements PutPath, GetPath, QueryPathInfo, FindMissingPaths.
//! ContentLookup returns UNIMPLEMENTED (Phase 2a stub).

use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, instrument, warn};

use rio_proto::store::chunk_service_server::ChunkService;
use rio_proto::store::store_service_server::StoreService;
use rio_proto::types::{
    ContentLookupRequest, ContentLookupResponse, FindMissingChunksRequest,
    FindMissingChunksResponse, FindMissingPathsRequest, FindMissingPathsResponse, GetChunkRequest,
    GetChunkResponse, GetPathRequest, GetPathResponse, PathInfo, PutChunkRequest, PutChunkResponse,
    PutPathRequest, PutPathResponse, QueryPathInfoRequest, get_path_response, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::backend::NarBackend;
use crate::metadata;
use crate::validate::{HashingReader, validate_nar_digest};

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
/// Holds a reference to the NAR backend and the PostgreSQL pool for metadata.
pub struct StoreServiceImpl {
    backend: Arc<dyn NarBackend>,
    pool: PgPool,
}

impl StoreServiceImpl {
    /// Create a new StoreService.
    pub fn new(backend: Arc<dyn NarBackend>, pool: PgPool) -> Self {
        Self { backend, pool }
    }

    /// Clean up an uploading placeholder after a PutPath error and record
    /// the error metric. Call this on any error path AFTER insert_uploading
    /// returned true (i.e., we own the placeholder).
    async fn abort_upload(&self, store_path_hash: &[u8]) {
        if let Err(e) = metadata::delete_uploading(&self.pool, store_path_hash).await {
            error!(error = %e, "PutPath: failed to clean up placeholder during abort");
        }
        metrics::counter!("rio_store_put_path_total", "result" => "error").increment(1);
    }
}

#[tonic::async_trait]
impl StoreService for StoreServiceImpl {
    /// Upload a store path (streaming NAR data with metadata).
    ///
    /// PutPath flow (write-ahead pattern):
    /// 1. Receive first message: PutPathMetadata with PathInfo
    /// 2. Check idempotency: if path already complete, return success
    /// 3. Insert nar_blobs row with status='uploading'
    /// 4. Accumulate NAR chunks (bounded by declared size + tolerance)
    /// 5. Verify SHA-256 matches declared nar_hash
    /// 6. Complete upload: update narinfo + flip status to 'complete'
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
        // In trailer mode this is hex(zeros) — a placeholder. Only used for
        // the insert_uploading lock row below; the REAL hex is recomputed
        // after trailer resolution for backend.put() and complete_upload().
        let placeholder_sha256_hex = hex::encode(info.nar_hash);

        debug!(
            store_path = %info.store_path.as_str(),
            nar_size = info.nar_size,
            is_trailer_mode,
            "PutPath: received metadata"
        );

        // Step 2: Check idempotency — if path already complete, return success
        match metadata::check_complete(&self.pool, &store_path_hash).await {
            Ok(Some(_)) => {
                debug!(store_path = %info.store_path.as_str(), "PutPath: path already complete, returning success");
                // Drain remaining stream messages (protocol contract)
                drain_stream(&mut stream).await;
                metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
                return Ok(Response::new(PutPathResponse { created: false }));
            }
            Ok(None) => {} // Not yet complete, proceed
            Err(e) => {
                // Drain remaining stream messages before returning error
                drain_stream(&mut stream).await;
                return Err(internal_error("PutPath: check_complete", e));
            }
        }

        // Step 3: Insert nar_blobs row with status='uploading'.
        // insert_uploading returns false (ON CONFLICT DO NOTHING no-op) if
        // another uploader already holds a placeholder for this path.
        // In that case we must NOT proceed: if we do and later fail
        // validation, delete_uploading would delete the OTHER uploader's
        // placeholder, losing their valid upload.
        let blob_key = format!("{placeholder_sha256_hex}.nar");
        let inserted = match metadata::insert_uploading(
            &self.pool,
            &store_path_hash,
            &info.store_path,
            &blob_key,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                drain_stream(&mut stream).await;
                return Err(internal_error("PutPath: insert_uploading", e));
            }
        };
        if !inserted {
            // Another upload is in progress (or just completed in the window
            // between check_complete above and now). Re-check: if it flipped
            // to complete, return success; otherwise tell the client to retry.
            drain_stream(&mut stream).await;
            match metadata::check_complete(&self.pool, &store_path_hash).await {
                Ok(Some(_)) => {
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

        // In trailer mode, the sha256_hex we computed BEFORE the chunk loop
        // (line ~186) was from the placeholder [0;32]. The blob_key derived
        // from it is wrong. Recompute now that info.nar_hash is real.
        let sha256_hex = hex::encode(info.nar_hash);

        // Write to backend. Capture the actual storage key returned by put()
        // instead of using the pre-computed blob_key: if a backend adds
        // sharding (e.g., "ab/abcd...nar"), the two would silently drift and
        // break GetPath. Currently both produce "{sha}.nar" so this is
        // future-proofing.
        let actual_blob_key = match self.backend.put(&sha256_hex, Bytes::from(nar_data)).await {
            Ok(key) => key,
            Err(e) => {
                error!(error = %e, "PutPath: failed to write to backend");
                self.abort_upload(&store_path_hash).await;
                return Err(internal_error("backend write", e));
            }
        };

        // Step 6: Complete upload — update narinfo + flip status to 'complete'.
        // `info` is already ValidatedPathInfo; just fill in the computed hash.
        let full_info = ValidatedPathInfo {
            store_path_hash,
            ..info
        };
        if let Err(e) = metadata::complete_upload(&self.pool, &full_info, &actual_blob_key).await {
            // Clean up placeholder rows AND backend blob (unlike validation/backend
            // failures which only clean metadata). The blob is now orphaned —
            // metadata never flipped to 'complete', so GetPath can't serve it.
            self.abort_upload(&full_info.store_path_hash).await;
            if let Err(backend_err) = self.backend.delete(&actual_blob_key).await {
                warn!(error = %backend_err, "PutPath: failed to delete orphaned blob after complete_upload failure");
            }
            return Err(internal_error("PutPath: complete_upload", e));
        }

        debug!(store_path = %full_info.store_path.as_str(), "PutPath: upload completed successfully");
        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        metrics::histogram!("rio_store_put_path_duration_seconds")
            .record(start.elapsed().as_secs_f64());
        Ok(Response::new(PutPathResponse { created: true }))
    }

    type GetPathStream = ReceiverStream<Result<GetPathResponse, Status>>;

    /// Download a store path's NAR data (streaming).
    ///
    /// GetPath flow:
    /// 1. Look up narinfo + nar_blobs from PostgreSQL
    /// 2. First response message: PathInfo metadata
    /// 3. Subsequent messages: NAR data chunks (64 KB each)
    /// 4. Verify content integrity via HashingReader (detects on-disk corruption)
    #[instrument(skip(self, request), fields(rpc = "GetPath"))]
    async fn get_path(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<Self::GetPathStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Step 1: Look up narinfo
        let info = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| internal_error("GetPath: query_path_info", e))?
            .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?;

        // Look up blob key
        let blob_key = metadata::get_blob_key(&self.pool, &req.store_path)
            .await
            .map_err(|e| internal_error("GetPath: get_blob_key", e))?
            .ok_or_else(|| {
                Status::not_found(format!("NAR blob not found for: {}", req.store_path))
            })?;

        // Step 2: Open blob from backend
        let reader = self
            .backend
            .get(&blob_key)
            .await
            .map_err(|e| internal_error("backend read", e))?
            .ok_or_else(|| {
                Status::not_found(format!("NAR blob missing from backend: {blob_key}"))
            })?;

        let expected_hash = info.nar_hash;
        let expected_size = info.nar_size;

        // Stream response via a channel
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        // Send metadata as first message (convert back to raw PathInfo for the wire)
        let info_raw: PathInfo = info.into();
        rio_common::task::spawn_monitored("get-path-stream", async move {
            // Bound the entire streaming task. A stalled backend read or a
            // slow-consuming client can otherwise keep this task alive forever,
            // leaking resources.
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

                // Step 3: Stream NAR data through HashingReader for integrity verification
                let mut hashing = HashingReader::new(reader);
                let mut buf = vec![0u8; NAR_CHUNK_SIZE];

                loop {
                    match hashing.read(&mut buf).await {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            let chunk = buf[..n].to_vec();
                            if tx
                                .send(Ok(GetPathResponse {
                                    msg: Some(get_path_response::Msg::NarChunk(chunk)),
                                }))
                                .await
                                .is_err()
                            {
                                return; // Client disconnected
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(internal_error("NAR stream read", e))).await;
                            return;
                        }
                    }
                }

                // Step 4: Verify content integrity — the NAR on disk may have been
                // corrupted (bitrot, partial write) since it was originally stored.
                // If the hash doesn't match, send DATA_LOSS so the client knows not
                // to trust the data.
                let digest = hashing.into_digest();
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
    /// Only returns paths with nar_blobs.status='complete'.
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
    /// Only completed paths (nar_blobs.status='complete') count as "present".
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
