//! ChunkService gRPC implementation.
//!
//! Chunking is server-side only: PutPath drives `cas::put_chunked`
//! (FastCDC + dedup via the `refcount==1` RETURNING clause), and
//! GetPath streams whole NARs back. The chunk-level RPCs are
//! `GetChunk` (one digest → one chunk; no production caller today,
//! exercised only by tests) and `GetChunks` (P0568 — bidi-stream
//! batch; the castore-FUSE fill task pipelines local-cache misses to
//! the server, which fans out [`GET_CHUNKS_K`] concurrent backend
//! reads and streams chunks back in completion order).
//!
//! Every RPC requires an authenticated caller (see
//! [`ChunkServiceImpl::require_caller_identity`]). Chunk *storage* is
//! global (dedup at rest by design) and the retrieval RPCs
//! (`GetChunk`/`GetChunks`) stay identity-gated but unscoped — knowing
//! a 32-byte BLAKE3 digest is the read capability, and digests travel
//! separately from the content they name (manifests, `StatBlob` chunk
//! lists, logs), so none of it may be anonymous. Chunk *presence*
//! (`HasChunks`) is tenant-scoped since ADR-024 P2: the probe answers
//! only for chunks the calling tenant has seen, closing the
//! cross-tenant build-activity oracle that the previous
//! global-namespace answer accepted as a trade-off.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::instrument;

use rio_proto::ChunkService;
use rio_proto::types::{
    ChunkData, GetChunkRequest, GetChunkResponse, GetChunksRequest, HasBitmap, HasChunksRequest,
};

use super::directory::parse_digest;
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
    /// `chunks` table reads for `HasChunks`. Same pool as
    /// `StoreServiceImpl` — one PG, two services.
    pool: sqlx::PgPool,
    /// Cache for GetChunk. Same cache as GetPath uses — a chunk fetched
    /// by either RPC warms the other. `None` = ChunkService effectively
    /// disabled (all RPCs return FAILED_PRECONDITION); main.rs only
    /// constructs this when a chunk backend is configured.
    chunk_cache: Option<Arc<ChunkCache>>,
    /// Verifier for the builder's HMAC assignment token — the
    /// caller-identity gate every ChunkService RPC passes through.
    /// Same instance as `DirectoryServiceImpl`'s.
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// IDLE budget for the `GetChunks` bidi stream — NOT an absolute
    /// lifetime cap. The builder's castore-FUSE fill (`RemoteChunks`
    /// in `rio-builder/src/castore_fuse/stream.rs`) reuses ONE stream
    /// for a whole file's fill, so total stream lifetime scales with
    /// file size; an absolute cap of `GRPC_STREAM_TIMEOUT` (300 s)
    /// killed any fill larger than budget × wire-rate (~4.4 GiB)
    /// mid-transfer, forever. Instead each await in the drain loop
    /// (next request frame / backend fetch, response send) gets a
    /// fresh budget: active streams never trip, half-open clients
    /// still do. Default [`rio_common::grpc::GRPC_STREAM_TIMEOUT`];
    /// tests shrink it via [`Self::with_stream_idle_timeout`].
    stream_idle_timeout: Duration,
}

impl ChunkServiceImpl {
    pub fn new(
        pool: sqlx::PgPool,
        chunk_cache: Option<Arc<ChunkCache>>,
        hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    ) -> Self {
        Self {
            pool,
            chunk_cache,
            hmac_verifier,
            stream_idle_timeout: rio_common::grpc::GRPC_STREAM_TIMEOUT,
        }
    }

    /// Test hook: shrink the `GetChunks` idle budget so the watchdog is
    /// testable without 300 s waits. See `stream_idle_timeout`.
    #[must_use]
    pub fn with_stream_idle_timeout(mut self, idle: Duration) -> Self {
        self.stream_idle_timeout = idle;
        self
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

    /// Caller-identity gate for the retrieval RPCs (`GetChunk`/
    /// `GetChunks`): a JWT (gateway path) or a verified HMAC
    /// assignment token (builder path). Retrieval stays unscoped —
    /// chunk dedup at rest is global by design and knowing the digest
    /// is the read capability — but no request may be anonymous:
    /// digests travel separately from the bytes they name (chunk
    /// manifests, `StatBlob` chunk lists, debug logs), so serving
    /// retrieval anonymously turns any leaked digest into the
    /// plaintext — an anonymous cross-tenant read oracle (the zot
    /// CVE-2024-39897 shape). `HasChunks` no longer uses this gate —
    /// it resolves a full tenant (ADR-024 P2 tenant-scoped presence,
    /// see `has_chunks`).
    // r[impl store.chunk.has-chunks-authenticated+1]
    fn require_caller_identity<T>(&self, request: &Request<T>) -> Result<(), Status> {
        // The verified identity is discarded: the chunk namespace is
        // deliberately cross-tenant, so only the *existence* of an
        // identity matters.
        super::directory::caller_identity(
            request,
            self.hmac_verifier.as_ref(),
            "ChunkService requires a caller identity: send a JWT or an HMAC \
             assignment token",
        )
        .map(|_| ())
    }
}

/// Log a `GetChunks` idle-timeout trip and push DEADLINE_EXCEEDED to a
/// client that may still be reading. `try_send`, not `send`: when the
/// trip came from a send that itself stalled, the channel is full and
/// a blocking send would park this task on the very client that
/// stopped reading.
fn report_idle_timeout(
    stage: &'static str,
    idle: Duration,
    tx: &tokio::sync::mpsc::Sender<Result<ChunkData, Status>>,
) {
    tracing::warn!(rpc = "GetChunks", stage, timeout = ?idle, "stream idle timeout");
    let _ = tx.try_send(Err(Status::deadline_exceeded(format!(
        "GetChunks idle timeout ({stage})"
    ))));
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
        // Identity before anything else — an anonymous caller learns
        // neither chunk contents nor whether a chunk backend exists.
        self.require_caller_identity(&request)?;
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
    ///
    /// Lifetime contract: the stream is bounded by an IDLE timeout
    /// (`stream_idle_timeout`), not an absolute one — the
    /// client reuses one stream for a whole file's fill, so total
    /// lifetime scales with file size and must not be capped.
    // r[impl proto.chunk.batch-bidi]
    #[instrument(skip(self, request), fields(rpc = "GetChunks"))]
    async fn get_chunks(
        &self,
        request: Request<Streaming<GetChunksRequest>>,
    ) -> Result<Response<Self::GetChunksStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Same identity-first ordering as GetChunk: the gate fires at
        // stream-open, before any request frame is consumed.
        self.require_caller_identity(&request)?;
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
        // A drain parked by a half-open client would pin ~K×CHUNK_MAX
        // of in-flight chunk data, and `tonic_builder()` sets no h2
        // keepalive, so the idle watchdog below is the only backstop.
        // NOT `drain_with_timeout`: its budget is absolute, which
        // would kill a reused GetChunks stream mid-fill on large files
        // (see `stream_idle_timeout`).
        let idle = self.stream_idle_timeout;
        rio_common::task::spawn_monitored("get-chunks-stream", async move {
            futures_util::pin_mut!(fetches);
            loop {
                // Covers both "waiting for the client's next request
                // frame" and "backend fetch in flight" — the latter is
                // bounded by one S3 GET, far inside any sane budget.
                let item = match tokio::time::timeout(idle, fetches.next()).await {
                    Err(_elapsed) => {
                        report_idle_timeout("recv", idle, &tx);
                        break;
                    }
                    // Request side closed and every fetch drained:
                    // normal end of stream.
                    Ok(None) => break,
                    Ok(Some(item)) => item,
                };
                let is_err = item.is_err();
                match tokio::time::timeout(idle, tx.send(item)).await {
                    Err(_elapsed) => {
                        // Client stopped reading — h2 backpressure
                        // propagated all the way into the channel.
                        report_idle_timeout("send", idle, &tx);
                        break;
                    }
                    // Receiver gone — client disconnected. Dropping
                    // `fetches` cancels in-flight backend reads.
                    Ok(Err(_)) => break,
                    Ok(Ok(())) => {
                        if is_err {
                            // First error closes the stream: the file
                            // can't be assembled, so there's no value
                            // finishing the remaining fetches. The K
                            // in-flight futures are dropped
                            // (cooperative cancel — bounded by one S3
                            // GET each).
                            break;
                        }
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// HasChunks: batch durable-presence probe (ADR-022 §6.2, P0586).
    ///
    /// Bit i set ⇔ `digests[i]` is **durable** — referenced by at least
    /// one `'complete'` manifest — not merely `refcount ≥ 1`. The
    /// refcount is bumped *before* the S3 PutObject, so a SIGKILL in
    /// that window leaves a refcount-1 row with no object behind it; a
    /// presence probe that trusted refcount would let the next uploader
    /// skip a chunk that doesn't exist, permanently stranding the
    /// digest (I-201). Under durable-only presence two builders racing
    /// on the same novel chunk both upload; the second S3 PutObject is
    /// an idempotent overwrite of identical content.
    ///
    /// Tenant-scoped (ADR-024 P2, `r[store.chunk.has-chunks-tenant]`):
    /// the bit additionally requires a `chunk_tenants` row for the
    /// calling tenant — written when one of the tenant's manifests
    /// completes (`insert_chunk_tenants_in_conn`). Presence means
    /// "this tenant has seen this chunk", never "someone has".
    /// Storage stays digest-keyed global, so the only cost of a false
    /// negative is a re-upload that lands as an idempotent overwrite:
    /// chunks ingested before the junction existed (or adopted via an
    /// idempotent path skip, or substituted with no tenant context)
    /// answer absent and re-bind on the tenant's next completed
    /// upload.
    ///
    /// No cache requirement: the probe is a pure PG read (an
    /// inline-only store correctly answers "nothing is durable").
    // r[impl store.chunk.has-chunks-durable]
    // r[impl store.chunk.has-chunks-tenant]
    #[instrument(skip(self, request), fields(rpc = "HasChunks"))]
    async fn has_chunks(
        &self,
        request: Request<HasChunksRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = super::directory::resolve_castore_tenant(
            &request,
            self.hmac_verifier.as_ref(),
            "HasChunks requires a tenant: send a JWT or an HMAC \
             assignment token",
        )?;
        let raw = request.into_inner().digests;
        // Bound BEFORE the per-digest parse: the per-manifest chunk cap
        // is the natural ceiling on how many digests one upload's probe
        // can legitimately contain.
        rio_common::grpc::check_bound("digests", raw.len(), crate::manifest::MAX_CHUNKS)?;
        let digests: Vec<[u8; 32]> = raw
            .iter()
            .map(|d| parse_digest(d))
            .collect::<Result<_, _>>()?;
        if digests.is_empty() {
            return Ok(Response::new(HasBitmap { bitmap: Vec::new() }));
        }

        // The partial index `chunks_present_idx (blake3_hash) WHERE
        // durable AND NOT deleted` covers the durable predicate; the
        // `chunk_tenants` PK `(blake3_hash, tenant_id)` covers the
        // visibility join. `raw` is byte-identical to `digests` (the
        // parse only checks lengths), so bind it directly instead of
        // re-allocating.
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT c.blake3_hash FROM chunks c \
               JOIN chunk_tenants ct ON ct.blake3_hash = c.blake3_hash \
              WHERE c.blake3_hash = ANY($1) AND c.durable AND NOT c.deleted \
                AND ct.tenant_id = $2",
        )
        .bind(&raw)
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "HasChunks: chunks query failed");
            Status::internal("HasChunks: database error")
        })?;
        let present: std::collections::HashSet<[u8; 32]> = rows
            .into_iter()
            .filter_map(|h| h.as_slice().try_into().ok())
            .collect();

        metrics::histogram!("rio_store_directory_has_batch_size", "rpc" => "HasChunks")
            .record(digests.len() as f64);
        Ok(Response::new(HasBitmap {
            bitmap: super::directory::build_bitmap(&digests, &present),
        }))
    }
}
