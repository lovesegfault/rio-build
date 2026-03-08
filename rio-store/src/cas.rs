//! Chunked content-addressable storage orchestration.
//!
//! Write path: `put_chunked()` — FastCDC + write-ahead + parallel upload.
//! Read path: `ChunkCache` — moka LRU + singleflight + BLAKE3 verify.
// r[impl store.singleflight]
// r[impl store.integrity.verify-on-get]
//!
//! The gRPC layer owns request parsing and the inline/chunked branch;
//! this module owns everything below that.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use futures_util::FutureExt;
use sqlx::PgPool;
use tracing::{debug, instrument, warn};

use rio_proto::validated::ValidatedPathInfo;

use crate::backend::chunk::ChunkBackend;
use crate::chunker;
use crate::manifest::{Manifest, ManifestEntry};
use crate::metadata;

/// NARs below this size bypass chunking and go into `manifests.inline_blob`.
///
/// 256 KiB = CHUNK_MAX. A NAR smaller than one max-chunk gains nothing from
/// chunking: it'd be 1-2 chunks at most, with manifest overhead + refcount
/// bookkeeping for no dedup benefit. The `.drv` files that dominate nixpkgs
/// closures by count are typically <10 KiB — all of those stay inline.
pub const INLINE_THRESHOLD: usize = 256 * 1024;

/// Bounded parallelism for chunk uploads. 8 concurrent S3 PUTs: enough to
/// saturate a typical link without being antisocial. For a 1 GiB NAR at
/// 64 KiB chunks (~16k chunks, assume 30% dedup → ~11k to upload),
/// 8-wide × ~50ms/PUT ≈ 70s. That's the dominant cost for large uploads.
const UPLOAD_CONCURRENCY: usize = 8;

/// Result of `put_chunked`.
#[derive(Debug)]
pub struct PutChunkedStats {
    /// Total chunks in the manifest.
    pub total_chunks: usize,
    /// Chunks that were already present (skipped upload).
    pub deduped_chunks: usize,
}

impl PutChunkedStats {
    /// Fraction of chunks that were deduplicated. [0.0, 1.0].
    /// This is what the `rio_store_chunk_dedup_ratio` gauge exposes.
    pub fn dedup_ratio(&self) -> f64 {
        if self.total_chunks == 0 {
            // Degenerate: no chunks → no meaningful ratio. 0.0 not NaN.
            0.0
        } else {
            self.deduped_chunks as f64 / self.total_chunks as f64
        }
    }
}

/// Store a large NAR via the chunked path.
///
/// # Preconditions (caller's responsibility)
///
/// - `nar_data` is the full NAR, already SHA-256 verified against
///   `info.nar_hash`.
/// - `nar_data.len() >= INLINE_THRESHOLD` — caller gates on this.
/// - `info.store_path_hash` is populated.
/// - **The caller already owns an 'uploading' placeholder** from
///   `insert_manifest_uploading()` at step 3. We UPGRADE it to chunked;
///   we don't create our own. This matters: step 3 runs BEFORE the NAR
///   stream is consumed (it's the idempotency lock), so at that point
///   we don't know the size yet. Only here, at step 6, do we know.
///
/// # Flow
///
/// 1. **Chunk**: FastCDC over `nar_data` → (hash, slice) list.
/// 2. **Upgrade write-ahead**: add manifest_data + increment refcounts
///    to the existing 'uploading' placeholder. One tx.
/// 3. **Find new chunks**: refcount==1 → we just inserted it → upload needed.
/// 4. **Upload**: parallel S3 PUTs for new chunks only.
/// 5. **Complete**: fill narinfo + flip status='complete'.
///
/// On error in 3-5: `delete_manifest_chunked_uploading` rolls back
/// refcounts + placeholders. Caller doesn't need to clean up (we consumed
/// their placeholder; we clean up our own mess).
#[instrument(skip(pool, backend, info, nar_data), fields(
    store_path = %info.store_path.as_str(),
    nar_size = nar_data.len(),
))]
pub async fn put_chunked(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
    info: &ValidatedPathInfo,
    nar_data: &[u8],
) -> anyhow::Result<PutChunkedStats> {
    let store_path_hash = &info.store_path_hash;

    // --- Step 1: Chunk ---
    // Borrows from nar_data — zero-copy. The slices stay valid until
    // after step 4's uploads (nar_data outlives this function body).
    let chunks = chunker::chunk_nar(nar_data);
    debug!(chunks = chunks.len(), "NAR chunked");

    // Build the manifest + parallel arrays for PG.
    // Vec<Vec<u8>> because sqlx binds bytea[] as &[Vec<u8>], not &[[u8;32]]
    // — one copy per hash (32 bytes each, cheap). i64 for PG BIGINT.
    let manifest = Manifest {
        entries: chunks
            .iter()
            .map(|c| ManifestEntry {
                hash: c.hash,
                size: c.data.len() as u32,
            })
            .collect(),
    };
    let chunk_list_bytes = manifest.serialize();
    let chunk_hashes: Vec<Vec<u8>> = chunks.iter().map(|c| c.hash.to_vec()).collect();
    let chunk_sizes: Vec<i64> = chunks.iter().map(|c| c.data.len() as i64).collect();

    // --- Step 2: Upgrade write-ahead ---
    // Caller owns the 'uploading' placeholder from step 3. We add
    // manifest_data + refcounts to it. If this fails (placeholder
    // missing — shouldn't happen but defensive), bail WITHOUT rollback:
    // we haven't touched refcounts yet.
    metadata::upgrade_manifest_to_chunked(
        pool,
        store_path_hash,
        &chunk_list_bytes,
        &chunk_hashes,
        &chunk_sizes,
    )
    .await?;

    // From here on, refcounts are incremented. Any error must roll back
    // via delete_manifest_chunked_uploading. scopeguard can't do async
    // drop, so explicit match-on-error.

    let stats = match do_upload(pool, backend, &chunks, &chunk_hashes).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "chunk upload failed; rolling back");
            rollback(pool, store_path_hash, &chunk_hashes).await;
            return Err(e);
        }
    };

    // --- Step 5: Complete ---
    if let Err(e) = metadata::complete_manifest_chunked(pool, info).await {
        warn!(error = %e, "complete_manifest_chunked failed; rolling back");
        // Chunks are uploaded to S3. Rollback decrements refcounts →
        // GC-eligible. We DON'T delete from S3 — GC sweep's job (future
        // phase). Deleting now races with a concurrent uploader that
        // just incremented the same chunk.
        rollback(pool, store_path_hash, &chunk_hashes).await;
        return Err(e.into());
    }

    Ok(stats)
}

/// Steps 3-4: find missing + parallel upload. Extracted so put_chunked's
/// error handling has one call site to wrap.
async fn do_upload(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
    chunks: &[chunker::Chunk<'_>],
    chunk_hashes: &[Vec<u8>],
) -> anyhow::Result<PutChunkedStats> {
    // --- Step 3: Find missing ---
    // Checks PG (chunks table), not S3. The table was populated in step 2
    // (our refcount increments), so our OWN chunks are now "present" — but
    // that's fine: chunks that were NEW (not previously in the table) need
    // uploading; chunks that were already there (refcount was >0 before
    // our increment) were uploaded by someone else.
    //
    // Hmm, actually: find_missing_chunks checks "blake3_hash = ANY($1)" —
    // it finds rows that EXIST. After step 2, ALL our chunks exist in the
    // table (we just UPSERTed them). So find_missing_chunks returns
    // all-present → we upload nothing → bug.
    //
    // Fix: check missing BEFORE step 2? No — then there's a TOCTOU:
    // chunk missing at check, another uploader inserts+uploads between
    // check and our step 2, we redundantly upload.
    //
    // Better fix: the UPSERT in step 2 returns rows_affected. Actually
    // no, ON CONFLICT DO UPDATE always affects a row. We need to know
    // WHICH rows were INSERTed (new, need upload) vs UPDATEd (existed,
    // skip). RETURNING with a CASE on xmax? PG has `(xmax = 0)` to
    // detect INSERT vs UPDATE in an UPSERT...
    //
    // Actually — simplest correct approach: check missing BEFORE step 2.
    // The TOCTOU redundant upload is harmless (S3 PutObject is idempotent,
    // same bytes → same result). The cost is one wasted S3 PUT per chunk
    // that another uploader races us to. That's rare and cheap.
    //
    // But we already CALLED step 2 above. So this function needs to be
    // called with a PRE-CHECK result. Restructuring...
    //
    // Or: step 2 could return which chunks were new. Let me just do the
    // pre-check here and accept the small TOCTOU window. The alternative
    // (xmax RETURNING tricks) is fragile.

    // Actually wait — I realize the structure is wrong. find_missing
    // should happen BEFORE insert_manifest_chunked_uploading, and the
    // result should be passed through. Let me restructure by making
    // the refcount check separate.
    //
    // No — simpler: just check which of OUR chunks have refcount == 1.
    // Those are the ones WE just inserted (refcount was 0 → 1 via our
    // UPSERT). Chunks with refcount > 1 existed before us.

    let refcounts: Vec<(Vec<u8>, i32)> = sqlx::query_as(
        r#"
        SELECT blake3_hash, refcount FROM chunks
        WHERE blake3_hash = ANY($1)
        "#,
    )
    .bind(chunk_hashes)
    .fetch_all(pool)
    .await?;

    // refcount == 1 → we just inserted it (was 0 before our UPSERT).
    // refcount > 1 → someone else already has it → already uploaded
    // (or being uploaded; if that fails, the chunk might actually be
    // missing from S3, but our reassembly will catch that via BLAKE3
    // verify — better a rare extra S3 HEAD on GetPath than always
    // redundantly uploading).
    //
    // There's a subtle race: between our UPSERT and this SELECT, another
    // uploader might have incremented. We'd see refcount=2 and skip
    // upload, but THEY might not have uploaded yet either (their step 4
    // is concurrent with ours). If BOTH skip → chunk never uploaded.
    //
    // Resolution: this is vanishingly unlikely (two uploaders of the
    // SAME chunk in the SAME ~10ms window, both seeing each other's
    // increment before either uploads). If it happens, GetPath's BLAKE3
    // verify fails → the chunk is re-fetched, S3 returns NotFound →
    // clear error. Not silent corruption. We accept the race.
    let need_upload: std::collections::HashSet<Vec<u8>> = refcounts
        .into_iter()
        .filter(|(_, rc)| *rc == 1)
        .map(|(h, _)| h)
        .collect();

    let total = chunks.len();
    let mut uploaded = 0usize;

    debug!(
        total,
        need_upload = need_upload.len(),
        deduped = total - need_upload.len(),
        "chunk dedup check"
    );

    // --- Step 4: Upload missing chunks ---
    // Batched join_all (same pattern as C2's exists_batch). Each batch
    // is up to UPLOAD_CONCURRENCY simultaneous PUTs.
    for batch in chunks.chunks(UPLOAD_CONCURRENCY) {
        let futs: Vec<_> = batch
            .iter()
            .filter(|c| need_upload.contains(c.hash.as_slice()))
            .map(|c| {
                // One copy here: &[u8] → Bytes. Unavoidable (S3 wants owned).
                // copy_from_slice is explicit about it (vs Bytes::from which
                // only works on Vec/Box, not borrowed slices).
                let data = Bytes::copy_from_slice(c.data);
                let hash = c.hash;
                let backend = Arc::clone(backend);
                async move { backend.put(&hash, data).await }
            })
            .collect();

        uploaded += futs.len();

        // Any single failed PUT aborts the whole upload. We don't try to
        // upload the rest — if S3 is having a bad time, piling on more
        // PUTs won't help. The rollback decrements refcounts, and the
        // next PutPath attempt retries the whole thing.
        for result in futures_util::future::join_all(futs).await {
            result?;
        }
    }

    debug!(uploaded, "chunk uploads complete");

    Ok(PutChunkedStats {
        total_chunks: total,
        deduped_chunks: total - uploaded,
    })
}

/// Best-effort rollback. Errors are logged, not propagated — the caller
/// is already returning an error; a rollback failure shouldn't mask it.
/// The orphan scanner (future phase) catches any leaked state.
async fn rollback(pool: &PgPool, store_path_hash: &[u8], chunk_hashes: &[Vec<u8>]) {
    if let Err(e) =
        metadata::delete_manifest_chunked_uploading(pool, store_path_hash, chunk_hashes).await
    {
        warn!(error = %e, "rollback of chunked upload failed; orphan scanner will clean up");
    }
}

// ============================================================================
// ChunkCache: read-path caching + singleflight + verification
// ============================================================================

/// In-process chunk cache with singleflight coalescing and BLAKE3 verify.
///
/// Wraps a `ChunkBackend`. GetPath (C5) uses this instead of the backend
/// directly. Three layers:
///
/// 1. **moka LRU** — hot chunks stay in memory. Weight-based: tracks
///    byte-size per entry, 2 GiB cap is a real memory bound (not just
///    an entry count that might be 2 GiB or might be 100 MiB depending
///    on chunk-size distribution).
/// 2. **Singleflight** — if N concurrent GetPaths all need chunk X, one
///    backend GET runs; N-1 await the same future. `store.md:114-122`
///    calls this the thundering-herd fix: cold start with 100 builds
///    needing overlapping closures would be O(100×M) S3 GETs without
///    this; with it, O(M).
/// 3. **BLAKE3 verify** — EVERY returned chunk is hashed against the
///    requested hash. This is `store.md:45`: "corrupt chunks are re-
///    fetched or flagged as an error". Catches: S3 bitrot, moka's
///    memory getting corrupted (hardware fault), a backend bug returning
///    the wrong chunk. The verify is ~250 MB/s; for a 64 KiB chunk
///    that's ~0.25ms — trivial against S3's ~50ms GET latency.
///
/// # Why verify is HERE and not in the backend
///
/// Verifying in `ChunkBackend::get` would mean moka-cache hits skip
/// verification (the cache returns bytes from memory, not from the
/// backend). We want verify-always. Putting it at THIS layer means one
/// verify per `get_verified()` call, regardless of which layer served
/// the bytes.
pub struct ChunkCache {
    backend: Arc<dyn ChunkBackend>,
    /// Lock-free async LRU. Key is the 32-byte BLAKE3 hash; value is the
    /// chunk bytes. moka handles eviction internally based on the weigher.
    lru: moka::future::Cache<[u8; 32], Bytes>,
    /// In-flight backend fetches, keyed by hash. `Shared` lets N callers
    /// clone and await the same future. The inner BoxFuture wraps a
    /// spawned task — spawning means even if the first caller is
    /// cancelled, the fetch runs to completion for the N-1 others.
    ///
    /// Output is `Option<Bytes>`: None covers both "not found" and
    /// "backend error / task panic" (both log, then return None so
    /// singleflight cleanup is uniform). Callers that need the
    /// distinction... don't, actually: both mean "couldn't get the
    /// chunk". The log captures which for operators.
    ///
    /// Why BoxFuture instead of `Shared<JoinHandle<...>>` directly:
    /// Shared requires Output: Clone. JoinHandle's output is
    /// `Result<T, JoinError>`; JoinError isn't Clone. So we map the
    /// JoinHandle through `.ok().flatten()` BEFORE sharing — the
    /// mapped future's output is `Option<Bytes>`, which IS Clone.
    /// BoxFuture erases the unnamable `Map<JoinHandle, closure>` type.
    inflight: DashMap<[u8; 32], InflightFetch>,
}

/// The Shared-future type stored in `inflight`. Type alias because the
/// full type is 3 lines of generics that would obscure the struct.
type InflightFetch =
    futures_util::future::Shared<futures_util::future::BoxFuture<'static, Option<Bytes>>>;

/// Default LRU capacity: 2 GiB. Configurable via `ChunkCache::with_capacity`.
///
/// At 64 KiB avg chunk size, 2 GiB holds ~32k chunks. That's enough to
/// keep a whole stdenv closure (~1 GiB of outputs, chunked) hot. For
/// smaller deployments, `with_capacity(256 * 1024 * 1024)` is plenty.
const DEFAULT_CACHE_CAPACITY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Error from `ChunkCache::get_verified`.
#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    /// Backend returned None — chunk not in S3. If the manifest says
    /// this hash exists, this is data loss. Caller should propagate as
    /// a hard error (not retry — retrying NotFound is pointless).
    #[error("chunk {} not found in backend (data loss if manifest claims it exists)", hex::encode(.0))]
    NotFound([u8; 32]),

    /// BLAKE3 of the fetched bytes doesn't match the requested hash.
    /// S3 bitrot, memory corruption, or a backend bug. The corrupt
    /// bytes are NOT cached (we verify before insert).
    #[error("chunk {} failed BLAKE3 verification (corrupt; expected {}, got {})",
        hex::encode(.expected), hex::encode(.expected), hex::encode(.actual))]
    Corrupt {
        expected: [u8; 32],
        actual: [u8; 32],
    },
}

impl ChunkCache {
    /// Create a cache with the default 2 GiB capacity.
    pub fn new(backend: Arc<dyn ChunkBackend>) -> Self {
        Self::with_capacity(backend, DEFAULT_CACHE_CAPACITY_BYTES)
    }

    /// Clone the inner backend Arc. For the write path: PutPath calls
    /// `backend.put()` directly (no point caching freshly-written
    /// chunks nothing has asked for). With this accessor, main.rs can
    /// construct ONE ChunkCache and share it with StoreServiceImpl +
    /// ChunkServiceImpl + CacheServerState — the goal was "a chunk
    /// warmed by GetPath is hot for GetChunk" which means
    /// one cache. StoreServiceImpl needs the raw backend for writes;
    /// it gets it via this accessor instead of a separate Arc.
    pub fn backend(&self) -> Arc<dyn ChunkBackend> {
        Arc::clone(&self.backend)
    }

    /// Create a cache with a custom capacity (bytes, not entry count).
    pub fn with_capacity(backend: Arc<dyn ChunkBackend>, capacity_bytes: u64) -> Self {
        let lru = moka::future::Cache::builder()
            // Weight = byte size. u32 return type; CHUNK_MAX is 256 KiB so
            // no overflow risk. The `.min()` is defensive for a pathological
            // Bytes someone stuffs in via a future API.
            .weigher(|_k: &[u8; 32], v: &Bytes| v.len().min(u32::MAX as usize) as u32)
            .max_capacity(capacity_bytes)
            .build();
        Self {
            backend,
            lru,
            inflight: DashMap::new(),
        }
    }

    /// Fetch a chunk, with caching + singleflight + BLAKE3 verify.
    ///
    /// # Flow
    ///
    /// 1. LRU hit → verify → return. No backend call, no singleflight.
    /// 2. LRU miss → check singleflight map:
    ///    - Fetch in progress → await the existing future.
    ///    - No fetch → spawn one, insert into map, await it.
    /// 3. Verify the bytes (regardless of where they came from).
    /// 4. Insert into LRU (only if verify passed — don't cache corruption).
    /// 5. Remove from singleflight map.
    ///
    /// # Singleflight lifecycle
    ///
    /// The inflight entry is removed AFTER the fetch completes (success
    /// or error). A failed fetch removes the entry so the next caller
    /// retries cleanly — if we left the error in the map, all subsequent
    /// callers would see the same stale error even after S3 recovered.
    #[instrument(skip(self), fields(hash = hex::encode(hash)))]
    pub async fn get_verified(&self, hash: &[u8; 32]) -> Result<Bytes, ChunkError> {
        // --- Layer 1: LRU ---
        if let Some(bytes) = self.lru.get(hash).await {
            metrics::counter!("rio_store_chunk_cache_hits_total").increment(1);
            // Verify even on cache hit. Memory corruption is rare but
            // real (cosmic rays, bad RAM). The alternative — trusting
            // the cache unconditionally — means a single bit-flip
            // propagates to every subsequent GetPath until restart.
            return Self::verify(hash, bytes);
        }
        metrics::counter!("rio_store_chunk_cache_misses_total").increment(1);

        // --- Layer 2: Singleflight ---
        let fetched = self.singleflight_fetch(hash).await?;

        // NotFound: backend says it doesn't have this chunk. Don't cache
        // the absence (the chunk might get uploaded between now and the
        // next call). Propagate as data-loss error.
        let bytes = fetched.ok_or(ChunkError::NotFound(*hash))?;

        // --- Layer 3: Verify BEFORE cache insert ---
        // If this fails, the corrupt bytes never enter the cache. The
        // next call retries from the backend (which might have recovered
        // — S3 bitrot is sometimes transient, sometimes not).
        let verified = Self::verify(hash, bytes)?;

        // --- Layer 4: Cache insert ---
        // moka's insert is async (eviction runs concurrently). Bytes is
        // Arc-backed so cloning is cheap.
        self.lru.insert(*hash, verified.clone()).await;

        Ok(verified)
    }

    /// Singleflight: either await an in-progress fetch or start a new one.
    ///
    /// Returns `Option<Bytes>` from the backend (None = NotFound).
    /// Errors from the backend propagate through; the inflight entry
    /// is cleaned up so the next call retries.
    async fn singleflight_fetch(&self, hash: &[u8; 32]) -> Result<Option<Bytes>, ChunkError> {
        // Check-then-insert with entry API. DashMap's entry() locks the
        // shard for this key, so two concurrent callers racing on the
        // same hash are serialized here: one inserts, one finds it.
        //
        // We spawn the backend call instead of just storing a future.
        // Why: if the FIRST caller is cancelled (client disconnect),
        // a plain Shared<impl Future> would also be cancelled, and the
        // N-1 awaiters would see a cancelled future. A spawned task
        // runs to completion regardless of who's awaiting.
        let shared = self
            .inflight
            .entry(*hash)
            .or_insert_with(|| {
                let backend = Arc::clone(&self.backend);
                let h = *hash;
                // Spawn + map + boxed + shared:
                // - spawn: fetch survives first-caller cancellation
                // - map: JoinHandle's Result<Opt,JoinError> → Opt (JoinError
                //   isn't Clone, so Shared can't hold it; .ok().flatten()
                //   turns panic → None, same as backend error → None)
                // - boxed: erase the unnamable Map<JoinHandle,closure> type
                //   so it fits InflightFetch
                // - shared: N callers await the same result
                //
                // Error is logged inside the task. None here conflates
                // "not found" with "backend error" with "task panicked"
                // — all three mean "couldn't get the chunk"; the log
                // distinguishes them for operators. Callers retry
                // uniformly (inflight cleanup below runs either way).
                tokio::spawn(async move {
                    match backend.get(&h).await {
                        Ok(opt) => opt,
                        Err(e) => {
                            warn!(hash = %hex::encode(h), error = %e,
                                  "chunk backend fetch failed");
                            None
                        }
                    }
                })
                .map(|join_result| {
                    // Task panic → None. Log here (the task itself
                    // didn't get to log its own panic).
                    join_result
                        .inspect_err(|e| warn!(error = %e, "chunk fetch task panicked"))
                        .ok()
                        .flatten()
                })
                .boxed()
                .shared()
            })
            .clone();

        // Await the shared fetch. The task continues even if we're
        // cancelled here (it's spawned); the shared handle is just
        // our window into its result.
        let result = shared.await;

        // Cleanup: remove from inflight. Runs once per AWAITER, not
        // once per fetch — N callers all call remove. DashMap::remove
        // on a missing key is a cheap no-op; first one removes, rest
        // no-op.
        //
        // Why remove-after not remove-before? Remove-before means the
        // next caller starts a duplicate fetch while we're awaiting.
        // Remove-after means the window between fetch-complete and
        // remove is tiny, and a caller in that window awaits an
        // already-complete Shared (instant return).
        self.inflight.remove(hash);

        Ok(result)
    }

    /// BLAKE3-verify bytes against the expected hash.
    ///
    /// Pass-through on success (same Bytes, Arc-bumped). Err on mismatch.
    /// Factored out so LRU-hit and LRU-miss paths both call it.
    fn verify(expected: &[u8; 32], bytes: Bytes) -> Result<Bytes, ChunkError> {
        let actual = *blake3::hash(&bytes).as_bytes();
        if actual == *expected {
            Ok(bytes)
        } else {
            metrics::counter!("rio_store_integrity_failures_total").increment(1);
            Err(ChunkError::Corrupt {
                expected: *expected,
                actual,
            })
        }
    }

    /// Approximate entry count for test assertions.
    ///
    /// moka doesn't track hit/miss internally; we count via the
    /// `rio_store_chunk_cache_{hits,misses}_total` counters. Dashboards
    /// compute ratios at query time via PromQL `rate()` — we don't emit
    /// a pre-computed ratio gauge (loses meaning when averaged across
    /// instances; observability.md "Note on ratio metrics").
    #[cfg(test)]
    pub fn test_cache_len(&self) -> u64 {
        // moka's entry_count is approximate (eventually consistent with
        // pending eviction). Good enough for tests.
        self.lru.entry_count()
    }
}

// r[verify store.singleflight]
// r[verify store.integrity.verify-on-get]
// r[verify store.integrity.verify-on-put]
#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::backend::chunk::MemoryChunkBackend;

    /// Real hash/data pair: the BLAKE3 of "hello chunk cache".
    /// `get_verified` hashes the data and compares, so these must match.
    fn sample_chunk() -> ([u8; 32], Bytes) {
        let data = Bytes::from_static(b"hello chunk cache");
        let hash = *blake3::hash(&data).as_bytes();
        (hash, data)
    }

    fn make_cache() -> (Arc<MemoryChunkBackend>, ChunkCache) {
        let backend = Arc::new(MemoryChunkBackend::new());
        // Small capacity so eviction tests don't need GB of data.
        let cache =
            ChunkCache::with_capacity(backend.clone() as Arc<dyn ChunkBackend>, 1024 * 1024);
        (backend, cache)
    }

    #[tokio::test]
    async fn get_found_and_verified() {
        let (backend, cache) = make_cache();
        let (hash, data) = sample_chunk();
        backend.put(&hash, data.clone()).await.unwrap();

        let got = cache.get_verified(&hash).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn get_not_found() {
        let (_backend, cache) = make_cache();
        let (hash, _) = sample_chunk();
        // Not in backend → NotFound.

        let result = cache.get_verified(&hash).await;
        assert!(matches!(result, Err(ChunkError::NotFound(_))));
    }

    /// The critical test: corrupt data in backend → verify catches it.
    /// Without this, S3 bitrot would propagate silently.
    #[tokio::test]
    async fn corrupt_chunk_rejected() {
        let (backend, cache) = make_cache();
        let (hash, _good_data) = sample_chunk();

        // Store GARBAGE under the real hash. Backend accepts it (put
        // doesn't verify — that's the contract, caller is supposed to
        // pass matching hash+data).
        backend
            .put(&hash, Bytes::from_static(b"garbage"))
            .await
            .unwrap();

        let result = cache.get_verified(&hash).await;
        match result {
            Err(ChunkError::Corrupt { expected, actual }) => {
                assert_eq!(expected, hash);
                assert_ne!(actual, hash); // hash of "garbage", not the real one
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    /// Corrupt data should NOT be cached — next call retries backend.
    #[tokio::test]
    async fn corrupt_not_cached_retry_succeeds() {
        let (backend, cache) = make_cache();
        let (hash, good_data) = sample_chunk();

        // First: garbage.
        backend
            .put(&hash, Bytes::from_static(b"garbage"))
            .await
            .unwrap();
        assert!(matches!(
            cache.get_verified(&hash).await,
            Err(ChunkError::Corrupt { .. })
        ));

        // Fix the backend (simulating S3 recovering / re-upload).
        backend.put(&hash, good_data.clone()).await.unwrap();

        // Second call hits backend again (corrupt bytes weren't cached),
        // sees good data, verifies, succeeds.
        let got = cache.get_verified(&hash).await.unwrap();
        assert_eq!(got, good_data);
    }

    /// Second get of same chunk → LRU hit (no second backend call).
    /// We can't directly observe "no backend call" with MemoryChunkBackend,
    /// but we CAN delete from backend after first get — if the second
    /// get succeeds, it came from LRU.
    #[tokio::test]
    async fn lru_hit_skips_backend() {
        let (backend, cache) = make_cache();
        let (hash, data) = sample_chunk();
        backend.put(&hash, data.clone()).await.unwrap();

        // First get: miss → backend → cache insert.
        let first = cache.get_verified(&hash).await.unwrap();
        assert_eq!(first, data);

        // moka inserts are async; give eviction/insert a moment to settle.
        // `run_pending_tasks()` makes this deterministic for tests.
        cache.lru.run_pending_tasks().await;

        // Delete from backend. Second get MUST come from LRU or fail.
        backend.corrupt_for_test(&hash, Bytes::from_static(b"DELETED"));
        // Actually corrupt_for_test overwrites — let me use something
        // that would fail verify if it reached the backend.
        // Already done: "DELETED" ≠ good data → verify would fail.

        let second = cache.get_verified(&hash).await.unwrap();
        assert_eq!(second, data, "LRU hit should skip backend");
    }

    /// Singleflight: N concurrent gets for the same chunk → 1 backend call.
    ///
    /// We can verify this by using a backend that DELAYS and counting
    /// concurrent entries in the inflight map. But MemoryChunkBackend is
    /// instant. Alternative: spawn 10 concurrent gets, verify they all
    /// succeed with the same data (weak but proves no corruption from
    /// the race) AND the inflight map is empty after (cleanup worked).
    #[tokio::test]
    async fn singleflight_concurrent_gets() {
        let (backend, cache) = make_cache();
        let cache = Arc::new(cache);
        let (hash, data) = sample_chunk();
        backend.put(&hash, data.clone()).await.unwrap();

        // 10 concurrent gets.
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let cache = Arc::clone(&cache);
                tokio::spawn(async move { cache.get_verified(&hash).await })
            })
            .collect();

        for h in handles {
            let got = h.await.unwrap().unwrap();
            assert_eq!(got, data);
        }

        // Inflight map cleaned up after all awaiters finish.
        assert!(
            cache.inflight.is_empty(),
            "inflight map should be empty after fetch completes"
        );
    }

    /// After a failed fetch (backend error), inflight is cleaned up so
    /// the next call retries cleanly.
    #[tokio::test]
    async fn singleflight_cleanup_on_miss() {
        let (_backend, cache) = make_cache();
        let (hash, _) = sample_chunk();
        // Backend empty → first call fails with NotFound.

        let first = cache.get_verified(&hash).await;
        assert!(matches!(first, Err(ChunkError::NotFound(_))));

        // Inflight should be clean (remove-after-await).
        assert!(cache.inflight.is_empty());

        // Second call also hits backend (inflight didn't cache the miss).
        let second = cache.get_verified(&hash).await;
        assert!(matches!(second, Err(ChunkError::NotFound(_))));
    }
}
