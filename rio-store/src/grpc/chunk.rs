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
    /// PutChunk: upload a single chunk directly, independent of any
    /// manifest.
    ///
    /// # Stream shape
    ///
    /// First frame MUST be `Metadata` (declared digest + size). One or
    /// more `Data` frames follow. Mirrors PutPath's metadata-first-
    /// then-body shape — same client coding pattern, same early-reject
    /// for malformed streams (we know the expected size before any
    /// bytes arrive, so the CHUNK_MAX cap fires on the declaration,
    /// not after buffering 32 MiB of garbage).
    ///
    /// # Refcount policy
    ///
    /// The chunk is inserted at `refcount = 0`. It is NOT GC-eligible
    /// immediately: [`crate::gc::sweep::sweep_orphan_chunks`] only reaps
    /// refcount=0 chunks whose `created_at` is older than the grace
    /// window (5 min). The expected lifecycle is PutChunk → PutPath
    /// (whose manifest UPSERT bumps refcount to 1 and clears
    /// `deleted`) — the grace window covers the gap.
    ///
    /// `ON CONFLICT DO NOTHING`: if the chunk already exists (another
    /// manifest references it, or a concurrent PutChunk raced us),
    /// that's fine — the existing row's `created_at` stays put (we
    /// don't reset its grace clock), and the backend `put()` is
    /// idempotent (content-addressed).
    ///
    /// # Verification
    ///
    /// BLAKE3 is computed over the concatenated data frames and
    /// compared to `metadata.digest` before anything touches the
    /// backend or PG. A bad hash never persists — this is the ONLY
    /// place a chunk enters the system unverified by the server-side
    /// chunker, so verify-on-ingest is non-negotiable (matches
    /// `docs/src/components/store.md` "Worker NOT trusted").
    // r[impl store.chunk.put-standalone]
    #[instrument(skip(self, request), fields(rpc = "PutChunk"))]
    async fn put_chunk(
        &self,
        request: Request<Streaming<PutChunkRequest>>,
    ) -> Result<Response<PutChunkResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let cache = self.require_cache()?;
        let mut stream = request.into_inner();

        // --- Frame 1: Metadata ---
        // Same metadata-first discipline as PutPath (put_path.rs:252).
        // Rejecting data-before-metadata here means we never buffer
        // bytes whose declared size we haven't checked.
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty PutChunk stream"))?;
        let meta = match first.msg {
            Some(put_chunk_request::Msg::Metadata(m)) => m,
            Some(put_chunk_request::Msg::Data(_)) => {
                return Err(Status::invalid_argument(
                    "first PutChunk frame must be Metadata, not Data",
                ));
            }
            None => {
                return Err(Status::invalid_argument("PutChunk frame has no content"));
            }
        };

        // Validate declared digest + size up front. 32 bytes for
        // BLAKE3 — same check as GetChunk.
        let digest: [u8; 32] = meta.digest.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "digest must be 32 bytes (BLAKE3), got {}",
                meta.digest.len()
            ))
        })?;
        // CHUNK_MAX (256 KiB) is the FastCDC hard cap — no legitimate
        // chunk exceeds it. A client sending larger is either buggy or
        // trying to DoS the buffer. Reject before reading ANY data
        // bytes — that's why metadata comes first.
        let declared_size = meta.size as usize;
        if declared_size > crate::chunker::CHUNK_MAX as usize {
            return Err(Status::invalid_argument(format!(
                "declared chunk size {declared_size} exceeds CHUNK_MAX {}",
                crate::chunker::CHUNK_MAX
            )));
        }

        // --- Frames 2..N: Data ---
        // Pre-size to the declared length — we'll check exact match
        // after the loop. The `>` check inside the loop catches a
        // lying client (declared 100 KiB, sends 300 KiB) without
        // letting the buffer grow past CHUNK_MAX.
        let mut buf = Vec::with_capacity(declared_size);
        while let Some(frame) = stream.message().await? {
            match frame.msg {
                Some(put_chunk_request::Msg::Data(d)) => {
                    if buf.len() + d.len() > declared_size {
                        return Err(Status::invalid_argument(format!(
                            "received data ({} bytes) exceeds declared size {declared_size}",
                            buf.len() + d.len()
                        )));
                    }
                    buf.extend_from_slice(&d);
                }
                Some(put_chunk_request::Msg::Metadata(_)) => {
                    return Err(Status::invalid_argument(
                        "duplicate Metadata frame in PutChunk stream",
                    ));
                }
                None => {} // Empty frame — tolerate (proto3 quirk).
            }
        }
        if buf.len() != declared_size {
            return Err(Status::invalid_argument(format!(
                "declared size {declared_size}, received {}",
                buf.len()
            )));
        }

        // --- Verify ---
        // `*hash().as_bytes()` copies the 32-byte digest out of the
        // Hash wrapper — same idiom as chunker.rs:83. Compare as
        // [u8; 32] so the type system proves both sides are 32 bytes.
        let computed = *blake3::hash(&buf).as_bytes();
        if computed != digest {
            return Err(Status::invalid_argument(format!(
                "chunk hash mismatch: declared {}, computed {}",
                hex::encode(digest),
                hex::encode(computed)
            )));
        }

        // --- Persist: backend first, then PG ---
        // Ordering rationale: if backend.put() fails, we return Err
        // and NO PG row is written — the chunk doesn't exist as far
        // as the store is concerned. If PG fails after backend
        // succeeds, the S3 object is orphaned (no `chunks` row points
        // to it) — harmless leak, same as the put_chunked "two
        // concurrent uploaders both skip" case (cas.rs:220). A
        // retry writes the same bytes (idempotent) and the PG row.
        //
        // `Bytes::from(Vec)` is a move, no copy — buf's allocation is
        // handed to Bytes. We don't need buf after this.
        cache
            .backend()
            .put(&digest, Bytes::from(buf))
            .await
            .map_err(|e| internal_error("PutChunk backend put", e))?;

        // PG row at refcount=0. `created_at` DEFAULT now() is the
        // grace-TTL clock start. `ON CONFLICT DO NOTHING`: if another
        // manifest already references this chunk (refcount > 0), or a
        // concurrent PutChunk beat us, leave the existing row alone —
        // resetting created_at would LENGTHEN the grace window on
        // every PutChunk retry, which lets a slow-retrying client keep
        // a dead chunk alive indefinitely.
        //
        // i64 cast for PG BIGINT (declared_size fits — we capped it
        // at 256 KiB above).
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size) VALUES ($1, 0, $2) \
             ON CONFLICT (blake3_hash) DO NOTHING",
        )
        .bind(digest.as_slice())
        .bind(declared_size as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| internal_error("PutChunk chunks insert", e))?;

        Ok(Response::new(PutChunkResponse {
            digest: digest.to_vec(),
        }))
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
