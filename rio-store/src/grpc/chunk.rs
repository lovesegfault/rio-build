//! ChunkService gRPC implementation.
//!
//! Chunking is server-side only: PutPath drives `cas::put_chunked`
//! (FastCDC + dedup via the upsert's `RETURNING (uploaded_at IS NULL)`
//! verdict), and GetPath streams whole NARs back. The chunk-level RPCs are
//! `GetChunk` (one digest → one chunk; no production caller today,
//! exercised only by tests) and `GetChunks` (P0568 — bidi-stream
//! batch; the castore-FUSE fill task pipelines local-cache misses to
//! the server, which fans out [`GET_CHUNKS_K`] concurrent backend
//! reads and streams chunks back in completion order).
//!
//! Both are unscoped: knowing a BLAKE3 hash already proves you have
//! (or had) the bytes.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::instrument;

use rio_proto::ChunkService;
use rio_proto::types::{ChunkData, GetChunkRequest, GetChunkResponse, GetChunksRequest};

use crate::cas::{self, ChunkCache};

/// `GetChunks` server-side fan-out width: how many `cas::get_verified()`
/// futures are in flight per stream.
///
/// Sibling of `chunk_prefetch_k` (`GetPath`, default 64) but with a
/// different ordering contract: `GetPath` MUST preserve NAR byte order
/// (`.buffered()`), while `GetChunks` carries the digest in each
/// `ChunkData` and uses `buffer_unordered()` — completion order, not
/// request order. That removes head-of-line blocking on a slow chunk,
/// which is why the spike-validated K is 4× higher.
///
/// Per-stream peak memory is bounded by `K × CHUNK_MAX` ≈ 64 MiB. The
/// moka L1 resolves most fetches sub-millisecond so the steady-state
/// peak is far lower; the bound matters during cold-start fan-out
/// against S3-standard. If a multi-tenant deployment shows memory
/// pressure here before P0563's dashboard lands, lower this and/or
/// move to a server-wide semaphore — neither is needed for the single
/// build-fleet deployment this ships into.
pub const GET_CHUNKS_K: usize = 256;

/// Response channel depth for `GetChunks`. Bounds the number of
/// resolved-but-not-yet-written `ChunkData` between the fan-out task
/// and the `ReceiverStream`. Small on purpose — the K in-flight fetches
/// already dominate memory; a deep channel just hides h2 backpressure.
const GET_CHUNKS_CHANNEL_DEPTH: usize = 8;

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

/// 32-byte parse + cache miss/corrupt → gRPC status. Shared by
/// `GetChunk` and `GetChunks` so the two RPCs surface identical errors
/// for the same failure (the client retry logic keys on the status
/// code).
fn classify_chunk_err(hash: [u8; 32], e: cas::ChunkError) -> Status {
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
}

fn parse_digest(digest: &[u8]) -> Result<[u8; 32], Status> {
    digest.try_into().map_err(|_| {
        Status::invalid_argument(format!(
            "digest must be 32 bytes (BLAKE3), got {}",
            digest.len()
        ))
    })
}

#[tonic::async_trait]
impl ChunkService for ChunkServiceImpl {
    type GetChunkStream = ReceiverStream<Result<GetChunkResponse, Status>>;
    type GetChunksStream = ReceiverStream<Result<ChunkData, Status>>;

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
        let hash = parse_digest(&digest)?;

        // Synchronous: await here (not in a spawned task). Chunks are
        // small (≤256 KiB) and the cache is fast (moka hit = instant,
        // miss = one S3 GET). The GetPath streaming-task pattern is
        // for large NARs where the stream outlives the handler call;
        // GetChunk's "stream" is one message.
        let bytes = cache
            .get_verified(&hash)
            .await
            .map_err(|e| classify_chunk_err(hash, e))?;

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

    /// GetChunks: bidi-stream batch fetch.
    ///
    /// Reads `GetChunksRequest` frames, flattens the `digests` lists
    /// into a single fetch sequence, runs `cache.get_verified()` for up
    /// to [`GET_CHUNKS_K`] of them concurrently, and streams each
    /// `ChunkData` in completion order. The first chunk error (bad
    /// digest length, miss, corruption) is sent as a stream error and
    /// the remaining work is dropped — a missing chunk means the file
    /// can't be assembled, and the client retries only the digests it
    /// didn't receive (chunks are content-addressed; arrival order
    /// carries no information).
    // r[impl proto.chunk.batch-bidi]
    #[instrument(skip(self, request), fields(rpc = "GetChunks"))]
    async fn get_chunks(
        &self,
        request: Request<Streaming<GetChunksRequest>>,
    ) -> Result<Response<Self::GetChunksStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let cache = Arc::clone(self.require_cache()?);
        let requests = request.into_inner();

        // Flatten the request frames into a flat digest sequence.
        // Length validation happens here, before the fetch fan-out, so
        // a client bug (truncated digest) is INVALID_ARGUMENT, not a
        // backend miss.
        let digests = requests.flat_map(|frame| {
            let items: Vec<Result<[u8; 32], Status>> = match frame {
                Ok(req) => req.digests.iter().map(|d| parse_digest(d)).collect(),
                Err(s) => vec![Err(s)],
            };
            futures_util::stream::iter(items)
        });

        // Fan out. buffer_unordered drives K futures and yields each
        // result as it completes — fast moka hits don't queue behind a
        // cold S3 read elsewhere in the batch. Resolved-but-not-yielded
        // results are held inside FuturesUnordered, so per-stream peak
        // memory is K × CHUNK_MAX regardless of channel depth.
        let fetches = digests
            .map(move |res| {
                let cache = Arc::clone(&cache);
                async move {
                    let hash = res?;
                    let data = cache
                        .get_verified(&hash)
                        .await
                        .map_err(|e| classify_chunk_err(hash, e))?;
                    Ok::<_, Status>(ChunkData {
                        digest: hash.to_vec(),
                        data,
                    })
                }
            })
            .buffer_unordered(GET_CHUNKS_K);

        let (tx, rx) = tokio::sync::mpsc::channel(GET_CHUNKS_CHANNEL_DEPTH);
        // Like get_path: a half-open client otherwise parks this task
        // on `tx.send()` forever, pinning ~K×CHUNK_MAX of in-flight
        // chunk data (tonic_builder() sets no h2 keepalive — the
        // stream timeout is the only backstop).
        rio_common::task::spawn_monitored("get-chunks-stream", async move {
            let drain = async {
                futures_util::pin_mut!(fetches);
                while let Some(item) = fetches.next().await {
                    let is_err = item.is_err();
                    if tx.send(item).await.is_err() {
                        // Receiver gone — client disconnected. Dropping
                        // `fetches` cancels in-flight backend reads.
                        break;
                    }
                    if is_err {
                        // First error closes the stream: the file can't
                        // be assembled, so there's no value finishing
                        // the remaining fetches. The K in-flight futures
                        // are dropped (cooperative cancel — bounded by
                        // one S3 GET each).
                        break;
                    }
                }
            };
            if tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, drain)
                .await
                .is_err()
            {
                tracing::warn!(
                    timeout = ?rio_common::grpc::GRPC_STREAM_TIMEOUT,
                    "GetChunks stream timed out"
                );
                let _ = tx
                    .send(Err(Status::deadline_exceeded("GetChunks stream timeout")))
                    .await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
