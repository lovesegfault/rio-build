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

use crate::backend::NarBackend;
use crate::metadata;
use crate::validate::{HashingReader, validate_nar_digest};

/// NAR chunk size for streaming GetPath responses (64 KB).
const NAR_CHUNK_SIZE: usize = 64 * 1024;

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
}

#[tonic::async_trait]
impl StoreService for StoreServiceImpl {
    /// Upload a store path (streaming NAR data with metadata).
    ///
    /// PutPath flow (write-ahead pattern):
    /// 1. Receive first message: PutPathMetadata with PathInfo
    /// 2. Check idempotency: if path already complete, return success
    /// 3. Insert nar_blobs row with status='uploading'
    /// 4. Stream NAR data through HashingReader to backend
    /// 5. Verify SHA-256 matches declared nar_hash
    /// 6. Complete upload: update narinfo + flip status to 'complete'
    #[instrument(skip(self, request), fields(rpc = "PutPath"))]
    async fn put_path(
        &self,
        request: Request<Streaming<PutPathRequest>>,
    ) -> Result<Response<PutPathResponse>, Status> {
        let start = std::time::Instant::now();
        let mut stream = request.into_inner();

        // Step 1: Receive the first message (must be metadata)
        let first_msg = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty PutPath stream"))?;

        let info = match first_msg.msg {
            Some(put_path_request::Msg::Metadata(meta)) => meta
                .info
                .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo"))?,
            Some(put_path_request::Msg::NarChunk(_)) => {
                return Err(Status::invalid_argument(
                    "first PutPath message must be metadata, not nar_chunk",
                ));
            }
            None => {
                return Err(Status::invalid_argument("PutPath message has no content"));
            }
        };

        // Validate required fields
        validate_store_path(&info.store_path)?;
        if info.nar_hash.len() != 32 {
            return Err(Status::invalid_argument(format!(
                "nar_hash must be 32 bytes (SHA-256), got {}",
                info.nar_hash.len()
            )));
        }
        // Bound nar_size BEFORE allocation. A malicious client declaring
        // nar_size=u64::MAX would otherwise attempt a huge Vec::with_capacity.
        if info.nar_size > MAX_NAR_SIZE {
            return Err(Status::invalid_argument(format!(
                "nar_size {} exceeds maximum {} bytes",
                info.nar_size, MAX_NAR_SIZE
            )));
        }

        // Compute store_path_hash if not provided
        let store_path_hash = if info.store_path_hash.is_empty() {
            compute_store_path_hash(&info.store_path)
        } else {
            info.store_path_hash.clone()
        };
        let sha256_hex = hex::encode(&info.nar_hash);

        debug!(
            store_path = %info.store_path,
            nar_size = info.nar_size,
            sha256 = %sha256_hex,
            "PutPath: received metadata"
        );

        // Step 2: Check idempotency — if path already complete, return success
        match metadata::check_complete(&self.pool, &store_path_hash).await {
            Ok(Some(_)) => {
                debug!(store_path = %info.store_path, "PutPath: path already complete, returning success");
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

        // Step 3: Insert nar_blobs row with status='uploading'
        let blob_key = format!("{sha256_hex}.nar");
        if let Err(e) =
            metadata::insert_uploading(&self.pool, &store_path_hash, &info.store_path, &blob_key)
                .await
        {
            drain_stream(&mut stream).await;
            return Err(internal_error("PutPath: insert_uploading", e));
        }

        // Step 4: Stream NAR data through HashingReader to backend.
        // Bound accumulation by declared nar_size + tolerance to prevent a
        // malicious/buggy client from OOMing the server.
        const NAR_SIZE_TOLERANCE: u64 = 4096;
        let max_allowed = info.nar_size.saturating_add(NAR_SIZE_TOLERANCE);
        let mut nar_data = Vec::with_capacity(info.nar_size as usize);
        while let Some(msg) = stream.message().await? {
            match msg.msg {
                Some(put_path_request::Msg::NarChunk(chunk)) => {
                    let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
                    if new_len > max_allowed {
                        warn!(
                            store_path = %info.store_path,
                            declared = info.nar_size,
                            received = new_len,
                            "PutPath: NAR chunks exceed declared size, rejecting"
                        );
                        return Err(Status::invalid_argument(format!(
                            "NAR chunks exceed declared nar_size {} (received {}+ bytes)",
                            info.nar_size, new_len
                        )));
                    }
                    nar_data.extend_from_slice(&chunk);
                }
                Some(put_path_request::Msg::Metadata(_)) => {
                    warn!("PutPath: received duplicate metadata message, ignoring");
                }
                None => {
                    // Empty message, skip
                }
            }
        }

        // Step 5: Verify SHA-256 via HashingReader
        let mut hashing = HashingReader::new(std::io::Cursor::new(&nar_data));
        let mut buf = Vec::with_capacity(nar_data.len());
        if let Err(e) = hashing.read_to_end(&mut buf).await {
            return Err(internal_error("NAR read", e));
        }
        let digest = hashing.into_digest();

        if let Err(e) = validate_nar_digest(&digest, &info.nar_hash, info.nar_size) {
            warn!(
                store_path = %info.store_path,
                error = %e,
                "PutPath: NAR validation failed"
            );
            // Clean up the placeholder rows so a retry can succeed.
            if let Err(cleanup_err) = metadata::delete_uploading(&self.pool, &store_path_hash).await
            {
                error!(error = %cleanup_err, "PutPath: failed to clean up placeholder after validation failure");
            }
            return Err(Status::invalid_argument(format!(
                "NAR validation failed: {e}"
            )));
        }

        // Write to backend
        if let Err(e) = self.backend.put(&sha256_hex, Bytes::from(nar_data)).await {
            error!(error = %e, "PutPath: failed to write to backend");
            // Clean up placeholder so retry can succeed.
            if let Err(cleanup_err) = metadata::delete_uploading(&self.pool, &store_path_hash).await
            {
                error!(error = %cleanup_err, "PutPath: failed to clean up placeholder after backend failure");
            }
            return Err(internal_error("backend write", e));
        }

        // Step 6: Complete upload — update narinfo + flip status to 'complete'
        let full_info = PathInfo {
            store_path_hash,
            ..info
        };
        if let Err(e) = metadata::complete_upload(&self.pool, &full_info, &blob_key).await {
            // Clean up placeholder rows AND backend blob (unlike validation/backend
            // failures which only clean metadata). The blob is now orphaned —
            // metadata never flipped to 'complete', so GetPath can't serve it.
            if let Err(cleanup_err) =
                metadata::delete_uploading(&self.pool, &full_info.store_path_hash).await
            {
                error!(error = %cleanup_err, "PutPath: failed to clean up placeholder after complete_upload failure");
            }
            if let Err(backend_err) = self.backend.delete(&blob_key).await {
                warn!(error = %backend_err, "PutPath: failed to delete orphaned blob after complete_upload failure");
            }
            return Err(internal_error("PutPath: complete_upload", e));
        }

        debug!(store_path = %full_info.store_path, "PutPath: upload completed successfully");
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

        let expected_hash = info.nar_hash.clone();
        let expected_size = info.nar_size;

        // Stream response via a channel
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        // Send metadata as first message
        let info_clone = info;
        tokio::spawn(async move {
            // First message: PathInfo
            if tx
                .send(Ok(GetPathResponse {
                    msg: Some(get_path_response::Msg::Info(info_clone)),
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
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        let info = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| internal_error("QueryPathInfo: query_path_info", e))?
            .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?;

        Ok(Response::new(info))
    }

    /// Batch check which paths are missing from the store.
    ///
    /// Only completed paths (nar_blobs.status='complete') count as "present".
    #[instrument(skip(self, request), fields(rpc = "FindMissingPaths"))]
    async fn find_missing_paths(
        &self,
        request: Request<FindMissingPathsRequest>,
    ) -> Result<Response<FindMissingPathsResponse>, Status> {
        let req = request.into_inner();

        // Bound request size to prevent DoS via huge path lists.
        if req.store_paths.len() > MAX_BATCH_PATHS {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {})",
                req.store_paths.len(),
                MAX_BATCH_PATHS
            )));
        }
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
/// unconsumed data on the gRPC transport.
async fn drain_stream(stream: &mut Streaming<PutPathRequest>) {
    while let Ok(Some(_)) = stream.message().await {
        // discard
    }
}
