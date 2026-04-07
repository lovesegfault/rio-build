//! ChunkService gRPC implementation.
//!
//! These RPCs are infrastructure — not used by the server-side PutPath
//! flow (which chunks inside `cas::put_chunked` and dedups via the
//! `refcount==1` RETURNING clause, no separate probe). They exist so
//! future clients (a worker-side chunker, store-to-store replication)
//! can interact at the chunk level without going through PutPath's
//! NAR-shaped API.
//!
//! Tenant-scoped: FindMissingChunks and PutChunk both require a JWT
//! `Claims` in request extensions (attached by `jwt_interceptor` in
//! main.rs). Dedup is per-tenant — tenant A cannot infer what B has
//! uploaded by probing chunk hashes. GetChunk is unscoped: knowing a
//! BLAKE3 hash already proves you have (or had) the bytes.

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

/// Extract tenant UUID from JWT Claims; FAIL-CLOSED if absent.
///
/// Free function (not a method on `ChunkServiceImpl`) so it borrows
/// only the request — no `&self` entanglement with `require_cache`'s
/// borrow. Called before `request.into_inner()` which drops extensions.
///
/// The `jwt_interceptor` in main.rs attaches `Claims` only when
/// (a) a pubkey is configured AND (b) the `x-rio-tenant-token` header
/// is present and valid. `None` here means one of those is false. For
/// FindMissingChunks and PutChunk — the two RPCs that read/write the
/// tenant junction — there is no safe unscoped fallback: unscoped
/// FindMissing leaks cross-tenant, unscoped PutChunk writes a chunk
/// the tenant can never dedup against (no junction row). Refuse.
fn require_tenant<T>(request: &Request<T>, rpc: &str) -> Result<uuid::Uuid, Status> {
    request
        .extensions()
        .get::<rio_common::jwt::TenantClaims>()
        .map(|c| c.sub)
        .ok_or_else(|| {
            Status::unauthenticated(format!(
                "{rpc} is tenant-scoped; no JWT Claims in request \
                 (x-rio-tenant-token absent or jwt_interceptor pubkey unconfigured)"
            ))
        })
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
        // Tenant BEFORE into_inner() — same ordering as
        // find_missing_chunks below. PutChunk without a tenant would
        // write a `chunks` row with no junction, which the tenant's
        // next FindMissingChunks would report as still-missing →
        // infinite re-upload loop. Fail-closed surfaces the config
        // gap immediately instead of after the third identical upload.
        let tenant_id = require_tenant(&request, "PutChunk")?;
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
            .map_err(|e| storage_error("PutChunk backend put", e))?;

        // PG row at refcount=0. `created_at` DEFAULT now() is the
        // grace-TTL clock start. backend.put() already succeeded, so
        // `uploaded_at = now()` records the S3-presence commit point.
        //
        // ON CONFLICT: only touch uploaded_at via COALESCE — keeps an
        // existing timestamp, fills NULL (e.g. row created by a crashed
        // cas::put_chunked) since WE just uploaded the bytes. Leaves
        // created_at/refcount alone so retries don't extend the grace
        // window or double-count.
        //
        // i64 cast for PG BIGINT (declared_size fits — we capped it
        // at 256 KiB above).
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, uploaded_at) \
             VALUES ($1, 0, $2, now()) \
             ON CONFLICT (blake3_hash) DO UPDATE \
                 SET uploaded_at = COALESCE(chunks.uploaded_at, EXCLUDED.uploaded_at)",
        )
        .bind(digest.as_slice())
        .bind(declared_size as i64)
        .execute(&self.pool)
        .await
        .status_internal("PutChunk chunks insert")?;

        // Junction row. AFTER the chunks row — chunk_tenants FK
        // references chunks(blake3_hash). The chunks insert above
        // DO-NOTHINGs on conflict, so the row exists either way
        // (freshly inserted by us OR previously by another tenant);
        // the FK is satisfied in both cases.
        //
        // Not in a single transaction with the chunks insert: the
        // chunks row has already committed (autocommit). If THIS
        // insert fails, we're left with a chunk attributed to nobody
        // — same state as a pre-migration-018 chunk. The tenant's
        // next FindMissingChunks says "missing", they retry PutChunk,
        // the chunks insert no-ops (already there), this insert runs
        // again. Self-healing on retry; no need for txn overhead on
        // the happy path.
        //
        // ON CONFLICT DO NOTHING on (blake3_hash, tenant_id): same
        // tenant re-uploading same bytes is a pure no-op. DIFFERENT
        // tenant, same bytes → fresh junction row. That's the point:
        // glibc uploaded by both A and B gets two rows here, one
        // chunks row, both tenants see "present" on probe.
        metadata::record_chunk_tenant(&self.pool, digest.as_slice(), tenant_id)
            .await
            .map_err(|e| metadata_status("PutChunk chunk_tenants insert", e))?;

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

    /// FindMissingChunks: batch check which chunks THIS TENANT has uploaded.
    ///
    /// Returns the subset of input digests that are NOT attributed to the
    /// caller's tenant in `chunk_tenants`. The client calls this before
    /// PutChunk to skip re-uploading chunks IT has already contributed.
    ///
    /// # Tenant scoping (fail-closed)
    ///
    /// Unscoped dedup leaks: if tenant A probes for a hash and gets
    /// "already present", A learns that SOMEONE (maybe B) has uploaded
    /// bytes with that hash. Probing for known-package chunk hashes
    /// reveals B's closure. The `chunk_tenants` junction scopes dedup
    /// to "have I uploaded this before?" — no cross-tenant signal.
    ///
    /// `Claims` absent → `UNAUTHENTICATED`. This is FAIL-CLOSED: the
    /// interceptor layer at `main.rs:496` is unconditionally wired,
    /// so a missing `Claims` means either (a) no JWT pubkey configured
    /// yet (dev mode — ChunkService has no prod callers today; the
    /// server-side PutPath flow dedups inside `cas::put_chunked` via
    /// the upsert RETURNING, bypassing this RPC entirely), or (b) caller
    /// didn't send `x-rio-tenant-token` (worker bug — the worker-side
    /// chunker, when it lands, must propagate the tenant JWT). Either
    /// way: no tenant identity → no scoped answer → refuse rather than
    /// leak.
    ///
    /// Checks PG (`chunk_tenants`), not S3 — one roundtrip for the whole
    /// batch, covered by the (tenant_id, blake3_hash) index.
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

        // Extract tenant BEFORE into_inner() consumes the request. Same
        // read-extensions-first pattern as scheduler/grpc/mod.rs:374 —
        // extensions are dropped by into_inner(). `sub` is `Copy` (Uuid),
        // no clone needed; just lift it out of the borrow.
        let tenant_id = require_tenant(&request, "FindMissingChunks")?;

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

        // Scoped variant: checks chunk_tenants junction, not chunks.
        // Same Vec<bool> (parallel missing-flags) shape as the unscoped
        // variant — the zip + filter below is unchanged.
        let missing_flags =
            metadata::find_missing_chunks_for_tenant(&self.pool, &req.digests, tenant_id)
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
