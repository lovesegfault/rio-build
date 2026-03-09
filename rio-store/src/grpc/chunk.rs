//! ChunkService gRPC implementation.
//!
//! These RPCs are infrastructure — not used by the current PutPath flow
//! (which calls `metadata::find_missing_chunks` directly and uploads via
//! `ChunkBackend` inside `cas::put_chunked`). They exist so future clients
//! (a hypothetical worker-side chunker, or a store-to-store replication
//! tool) can interact at the chunk level without going through PutPath's
//! NAR-shaped API.

use super::*;

/// Cap on FindMissingChunks batch size. 100k digests × 32 bytes = 3.2 MiB
/// of request body — well under the 32 MiB gRPC limit, generous enough
/// for any real PutPath (even a 4 GiB NAR at 16 KiB min chunks is 256k
/// chunks, which the client should split into multiple calls).
const MAX_CHUNK_DIGESTS: usize = 100_000;

/// ChunkService implementation.
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
    /// TODO(phase4): implement if client-side chunking lands. Needs
    /// refcount policy for standalone chunks (probably: chunk without
    /// manifest gets a short grace TTL before GC). No client-side
    /// chunker exists yet so this stays UNIMPLEMENTED.
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
