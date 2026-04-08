//! ChunkService gRPC implementation.
//!
//! Chunking is server-side only: PutPath drives `cas::put_chunked`
//! (FastCDC + dedup via the `refcount==1` RETURNING clause). The only
//! chunk-level RPC is `GetChunk`, which the builder fans out to
//! reassemble NARs from their manifests.
//!
//! GetChunk is unscoped: knowing a BLAKE3 hash already proves you have
//! (or had) the bytes.

use super::*;

/// ChunkService implementation.
///
/// Shares `chunk_cache` with `StoreServiceImpl` — one gRPC server
/// process, two services, same state. `Arc` lets main.rs construct
/// both from the same backing pieces.
pub struct ChunkServiceImpl {
    /// Cache for GetChunk. Same cache as GetPath uses — a chunk fetched
    /// by either RPC warms the other. `None` = ChunkService effectively
    /// disabled (all RPCs return FAILED_PRECONDITION); main.rs only
    /// constructs this when a chunk backend is configured.
    chunk_cache: Option<Arc<ChunkCache>>,
}

impl ChunkServiceImpl {
    pub fn new(chunk_cache: Option<Arc<ChunkCache>>) -> Self {
        Self { chunk_cache }
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
}
