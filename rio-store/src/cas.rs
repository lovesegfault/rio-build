//! Chunked content-addressable storage orchestration.
//!
//! Write path: `put_chunked()` — FastCDC + write-ahead + parallel upload.
//! Read path: `ChunkCache` — moka LRU + singleflight + BLAKE3 verify.
// r[impl store.singleflight+2]
// r[impl store.integrity.verify-on-get]
//!
//! The gRPC layer owns request parsing and the inline/chunked branch;
//! this module owns everything below that.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use futures_util::FutureExt;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use sqlx::PgPool;
use tracing::{debug, instrument, warn};

use rio_proto::validated::ValidatedPathInfo;

use crate::backend::ChunkBackend;
use crate::chunker;
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::metadata;

/// NARs below this size bypass chunking and go into `manifests.inline_blob`.
///
/// 256 KiB = CHUNK_MAX. A NAR smaller than one max-chunk gains nothing from
/// chunking: it'd be 1-2 chunks at most, with manifest overhead + refcount
/// bookkeeping for no dedup benefit. The `.drv` files that dominate nixpkgs
/// closures by count are typically <10 KiB — all of those stay inline.
pub const INLINE_THRESHOLD: usize = 256 * 1024;

/// Decide whether a NAR should go through the chunked CAS.
///
/// Returns `Some(backend)` when a chunk backend is configured AND the
/// NAR is at least [`INLINE_THRESHOLD`] bytes; `None` means take the
/// inline path. Centralises the predicate so a per-tenant override or
/// composefs-mode preference only needs touching here.
pub fn should_chunk(
    backend: Option<&Arc<dyn ChunkBackend>>,
    nar_len: usize,
) -> Option<&Arc<dyn ChunkBackend>> {
    backend.filter(|_| nar_len >= INLINE_THRESHOLD)
}

/// Default max concurrent S3 chunk uploads per `put_chunked` call.
///
/// Per-replica `r[store.substitute.admission]` bounds how many
/// `put_chunked` calls run at once; this bounds fan-out WITHIN one.
/// `substitute_admission_permits × this` is the per-replica in-flight
/// PutObject ceiling — keep it under the aws-sdk's ~1024 connection
/// pool with headroom. At helm-default `pg_max=20` → admission=64 →
/// 64×8=512. Operators raising admission past ~128 should lower this
/// proportionally.
///
/// Unbounded fan-out on large NARs (python3: 374 MB → ~1900 chunks)
/// saturates the pool → `DispatchFailure` cascades → substitution
/// rolls back and the rsb user sees `nix build` fail on a path that
/// exists upstream. The 8 bound keeps throughput without the spike.
///
/// Overridable via `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` (`RIO_` env layer).
// r[impl store.cas.upload-bounded]
pub const DEFAULT_CHUNK_UPLOAD_CONCURRENCY: usize = 8;

/// Run CPU-bound work without stalling the tokio worker. Uses
/// `block_in_place` on the multi-thread runtime (production); falls
/// back to inline execution on `current_thread` (the `#[tokio::test]`
/// default) where `block_in_place` would panic. The fallback is fine
/// for tests — they hash kilobyte-scale NARs, not 4 GiB.
pub(crate) fn cpu_bound<R>(f: impl FnOnce() -> R) -> R {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() != RuntimeFlavor::CurrentThread => {
            tokio::task::block_in_place(f)
        }
        // No runtime (sync #[test]) or current_thread → inline.
        _ => f(),
    }
}

/// Heartbeat interval (time): bump `manifests.updated_at` at least
/// this often during long chunk uploads. Paired with
/// [`HEARTBEAT_CHUNK_INTERVAL`] — whichever fires first.
const HEARTBEAT_TIME_INTERVAL: Duration = Duration::from_secs(30);

/// Heartbeat interval (chunk count): bump `manifests.updated_at` at
/// least every N uploaded chunks. Paired with
/// [`HEARTBEAT_TIME_INTERVAL`] — whichever fires first.
const HEARTBEAT_CHUNK_INTERVAL: u32 = 64;

/// Bump `manifests.updated_at` for an in-progress upload so the
/// orphan scanner's stale-threshold check sees "last progress" not
/// "insert time".
///
/// Fire-and-forget: errors are swallowed. If the row's gone, either
/// the scanner already reaped us (upload will fail at
/// `complete_manifest` anyway) or a concurrent uploader completed
/// the path (our upload will no-op via idempotency). Neither case
/// needs a heartbeat.
///
/// Guards the reap race introduced by P0483's 15min STALE_THRESHOLD:
/// a 6GB NAR over 50Mbps takes ~16 minutes — without heartbeat the
/// scanner would reap it mid-flight, decrement chunk refcounts, and
/// the uploader's `complete_manifest` would point at chunks already
/// enqueued to `pending_s3_deletes`.
///
/// `claim` is the ownership token from `insert_manifest_uploading`:
/// if our row was stale-reaped and a fresh re-upload now holds the
/// slot, the `claim_id = $2` filter makes this a no-op instead of
/// keeping the foreign placeholder artificially fresh
/// (`r[store.put.placeholder-claim+2]`).
// r[impl store.gc.orphan-heartbeat]
// r[impl store.put.placeholder-claim+2]
pub(crate) async fn heartbeat_uploading(pool: &PgPool, store_path_hash: &[u8], claim: uuid::Uuid) {
    let _ = sqlx::query(
        "UPDATE manifests SET updated_at = now() \
         WHERE store_path_hash = $1 AND status = 'uploading' AND claim_id = $2",
    )
    .bind(store_path_hash)
    .bind(claim)
    .execute(pool)
    .await;
}

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
/// 3. **Upload new chunks**: step 2 atomically returns which hashes
///    need upload (RETURNING refcount=1). Parallel S3 PUTs for those only.
/// 5. **Complete**: fill narinfo + flip status='complete'.
///
/// On error in 3-5: `delete_manifest_chunked_uploading` rolls back
/// refcounts + placeholders. Caller doesn't need to clean up (we consumed
/// their placeholder; we clean up our own mess).
#[instrument(skip(pool, backend, info, nar), fields(
    store_path = %info.store_path.as_str(),
    nar_size = nar.len(),
))]
pub async fn put_chunked(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
    nar: &crate::ingest::AdmittedNar,
    max_concurrent: usize,
) -> anyhow::Result<PutChunkedStats> {
    let stats = stage_chunked(pool, backend, info, claim, nar, max_concurrent).await?;

    // --- Step 5: Complete ---
    if let Err(e) = metadata::complete_manifest_chunked(pool, info, claim).await {
        warn!(error = %e, "complete_manifest_chunked failed; rolling back");
        // Chunks are uploaded to S3. reap_one decrements refcounts →
        // GC-eligible. We DON'T delete from S3 — GC sweep's job.
        // Deleting now races with a concurrent uploader that just
        // incremented the same chunk. ReapBy::Claim: stage_chunked's
        // own rollback may have already deleted OUR row and a fresh
        // uploader may now hold the slot — claim_id mismatch makes
        // this a no-op there (M_052).
        if let Err(e2) = crate::gc::orphan::reap_one(
            pool,
            &info.store_path_hash,
            crate::gc::orphan::ReapBy::Claim(claim),
            Some(backend),
        )
        .await
        {
            warn!(error = %e2, "rollback after complete failure also failed; orphan scanner will clean up");
        }
        return Err(e.into());
    }

    Ok(stats)
}

/// Steps 1–4b of [`put_chunked`]: chunk, upgrade-manifest, S3 upload,
/// mark-uploaded. Does NOT flip status to `'complete'` — caller does
/// that (via `metadata::complete_manifest_chunked` or its `_in_tx`
/// variant).
///
/// On internal error this rolls back its OWN refcount increments + the
/// caller's `'uploading'` placeholder (same rollback contract as
/// [`put_chunked`] — caller doesn't clean up).
///
/// PutPathBatch calls this per-output BEFORE its atomic completion tx
/// so the visibility flip for N outputs (inline + chunked) commits
/// together. On batch-tx failure, `abort_batch` → `reap_one` (chunk-
/// aware) decrements the staged refcounts; S3 blobs orphan and GC
/// sweeps them.
#[instrument(skip(pool, backend, info, nar), fields(
    store_path = %info.store_path.as_str(),
    nar_size = nar.len(),
))]
pub async fn stage_chunked(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
    nar: &crate::ingest::AdmittedNar,
    max_concurrent: usize,
) -> anyhow::Result<PutChunkedStats> {
    // The admission witness is the only byte carrier the chunker
    // accepts (round-17 RC17-05 c3) — staging is a persistence
    // primitive: PutPathBatch routes here directly, so taking the
    // witness HERE (not only in persist_nar) is what keeps the batch
    // route governed.
    let nar_data: &[u8] = nar.bytes();
    let store_path_hash = &info.store_path_hash;

    // --- Step 1: Chunk ---
    // Borrows from nar_data — zero-copy. The slices stay valid until
    // after step 4's uploads (nar_data outlives this function body).
    //
    // cpu_bound (block_in_place): FastCDC + per-chunk BLAKE3 over
    // ≤ MAX_NAR_SIZE (4 GiB) is multi-second CPU-bound work; running
    // it inline on a tokio worker thread stalls every other future
    // scheduled on that worker. spawn_blocking would require
    // nar_data: 'static (it's a borrow); block_in_place keeps the
    // borrow and tells the runtime to hand off other work.
    let chunks = cpu_bound(|| chunker::chunk_nar(nar_data));
    debug!(chunks = chunks.len(), "NAR chunked");

    // Trust-boundary guard: reject before any PG/S3 commit. MAX_CHUNKS
    // is sized ≥ MAX_NAR_SIZE/CHUNK_MIN (compile-asserted in
    // manifest.rs), so this can only fire on a NAR that already
    // violated MAX_NAR_SIZE — but enforcing here means a future limit
    // bump can't silently produce a manifest that deserialize rejects.
    if chunks.len() > manifest::MAX_CHUNKS {
        anyhow::bail!(
            "NAR produced {} chunks, exceeds MAX_CHUNKS {} \
             (likely adversarial CHUNK_MIN-forcing input)",
            chunks.len(),
            manifest::MAX_CHUNKS,
        );
    }

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
    // Dedup chunk_hashes/sizes for the UNNEST upsert. FastCDC CAN
    // produce duplicate chunks (identical 16KB+ runs, e.g., zero-
    // filled pages). PG rejects `INSERT ... ON CONFLICT DO UPDATE`
    // with duplicate PKs in the SAME batch: "ON CONFLICT DO UPDATE
    // command cannot affect row a second time" (SQLSTATE 21000).
    //
    // Deduping here also fixes refcount semantics: 1 ref per UNIQUE
    // chunk per manifest, matching decrement_and_enqueue's HashSet
    // dedup. The manifest serialization above still has dups
    // (chunk_list_bytes) — reassembly needs the full in-order chunk
    // list. Only the refcount arrays dedup.
    //
    // `chunks` vec stays undeduped — manifest serialization needs the
    // full in-order list. do_upload dedups intra-NAR repeats itself.
    let (chunk_hashes, chunk_sizes): (Vec<Vec<u8>>, Vec<i64>) = {
        let mut seen = std::collections::HashSet::<[u8; 32]>::new();
        chunks
            .iter()
            .filter(|c| seen.insert(c.hash))
            .map(|c| (c.hash.to_vec(), c.data.len() as i64))
            .unzip()
    };

    // --- Step 2: Upgrade write-ahead ---
    // Caller owns the 'uploading' placeholder from step 3. We add
    // manifest_data + refcounts to it. If this fails (placeholder
    // missing — shouldn't happen but defensive), bail WITHOUT rollback:
    // we haven't touched refcounts yet.
    //
    // Returns the set of hashes that need upload — `(uploaded_at IS
    // NULL)` per chunk, atomic with the upsert. A chunk that another
    // PutPath has already CONFIRMED in S3 (via `mark_chunks_uploaded`)
    // is skipped; one that's merely refcounted (upload in flight or
    // interrupted) is re-uploaded — see M_033.
    let (needs_upload, token) = metadata::upgrade_manifest_to_chunked(
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

    let stats = match do_upload(
        Some((pool, store_path_hash, claim)),
        backend,
        nar_data,
        &chunks,
        &needs_upload,
        max_concurrent,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "chunk upload failed; rolling back");
            rollback(pool, store_path_hash, token, &chunk_hashes).await;
            return Err(e);
        }
    };

    // --- Step 4b: Commit S3 presence ---
    // Uploads succeeded → record `uploaded_at` so later PutPaths can
    // safely skip these hashes. If THIS write fails the chunks are in
    // S3 but PG says NULL — next PutPath re-uploads (idempotent), so
    // rollback here is for refcount hygiene, not data safety.
    let needs_upload: Vec<Vec<u8>> = needs_upload.into_iter().collect();
    if let Err(e) = metadata::mark_chunks_uploaded(pool, &needs_upload).await {
        warn!(error = %e, "mark_chunks_uploaded failed; rolling back");
        rollback(pool, store_path_hash, token, &chunk_hashes).await;
        return Err(e.into());
    }

    Ok(stats)
}

/// Step 3: parallel upload. Extracted so put_chunked's error handling
/// has one call site to wrap.
///
/// `needs_upload` is the set of hashes that need upload — computed
/// atomically by the upsert's RETURNING clause (chunked.rs).
///
/// `heartbeat_target` is `Some((pool, store_path_hash, claim))` in
/// production — every 30s/64 chunks the upload loop fires
/// [`heartbeat_uploading`] so the orphan scanner doesn't reap a live
/// long-running upload. `None` in tests that exercise upload
/// mechanics without PG.
///
/// `nar_data` is the buffer that every `chunks[i].data` borrows from
/// (by construction in `chunker::chunk_nar`). It's threaded explicitly
/// so the upfront collect can store `(hash, Range<usize>)` and defer
/// the owned `Bytes` copy into the stream `.map()` closure — bounding
/// unbudgeted overshoot to `max_concurrent × CHUNK_MAX` instead of
/// ~the full to-upload set (`r[store.put.nar-bytes-budget+3]`).
// r[impl store.cas.upload-bounded]
async fn do_upload(
    heartbeat_target: Option<(&PgPool, &[u8], uuid::Uuid)>,
    backend: &Arc<dyn ChunkBackend>,
    nar_data: &[u8],
    chunks: &[chunker::Chunk<'_>],
    needs_upload: &std::collections::HashSet<Vec<u8>>,
    max_concurrent: usize,
) -> anyhow::Result<PutChunkedStats> {
    let total = chunks.len();

    debug!(
        total,
        need_upload = needs_upload.len(),
        deduped = total - needs_upload.len(),
        max_concurrent,
        "chunk dedup check"
    );

    // --- Step 3: Upload missing chunks ---
    // buffer_unordered keeps exactly `max_concurrent` PUTs in flight,
    // replacing the old batched join_all which had a barrier between
    // batches (all N must complete before the next N start — slow
    // stragglers serialize the pipeline). Filter to just the hashes
    // that need upload so dedup'd chunks don't occupy slots.
    //
    // Materialize (hash, range) pairs into an owned Vec before
    // streaming — keeping borrowed `chunks` slices inside the stream
    // trips rustc's HRTB Send-inference (the future's Send bound
    // doesn't generalize over the borrowed lifetime). The owned
    // `Bytes` copy (S3 wants owned) is DEFERRED to the stream
    // `.map()` closure so at most `max_concurrent × CHUNK_MAX` of
    // owned `Bytes` is in flight; collecting `Bytes` upfront here
    // would hold a second ~full-NAR allocation alongside the
    // semaphore-charged `nar_data` (r[store.put.nar-bytes-budget+3]).
    //
    // `needs_upload.contains()` does NOT dedup: `chunks` is the raw
    // in-order list, so an intra-NAR repeat (zero-filled pages etc.)
    // would issue N redundant Bytes::copy_from_slice + S3 PUTs and
    // miscount `uploaded` (feeds rio_store_chunk_dedup_ratio). The
    // `seen` set collapses repeats to one upload per unique hash.
    let base = nar_data.as_ptr() as usize;
    let mut seen = std::collections::HashSet::<[u8; 32]>::new();
    let to_upload: Vec<([u8; 32], std::ops::Range<usize>)> = chunks
        .iter()
        .filter(|c| needs_upload.contains(c.hash.as_slice()) && seen.insert(c.hash))
        .map(|c| {
            // c.data is a subslice of nar_data by construction
            // (chunker::chunk_nar returns borrows into its input).
            let off = c.data.as_ptr() as usize - base;
            debug_assert!(
                off + c.data.len() <= nar_data.len(),
                "chunk slice must be within nar_data"
            );
            (c.hash, off..off + c.data.len())
        })
        .collect();
    let uploaded = to_upload.len();

    // Heartbeat state: (last_heartbeat, chunks_since_heartbeat).
    // Shared across the concurrent upload futures via Arc<Mutex>.
    // Cloned into the owned tuple so the per-future closures don't
    // borrow `heartbeat_target` (lifetime would be tied to this fn's
    // stack, which trips Send-inference through buffer_unordered).
    let hb = heartbeat_target.map(|(pool, hash, claim)| {
        (
            pool.clone(),
            hash.to_vec(),
            claim,
            Arc::new(Mutex::new((Instant::now(), 0u32))),
        )
    });

    // Any single failed PUT aborts the whole upload (try_for_each
    // short-circuits on first Err). We don't try to upload the rest —
    // if S3 is having a bad time, piling on more PUTs won't help. The
    // rollback decrements refcounts, and the next PutPath attempt
    // retries the whole thing.
    stream::iter(to_upload)
        .map(|(hash, range)| {
            // Owned copy deferred to here: ≤ max_concurrent in flight.
            let data = Bytes::copy_from_slice(&nar_data[range]);
            let backend = Arc::clone(backend);
            let hb = hb.clone();
            async move {
                backend.put(&hash, data).await?;
                // Heartbeat check — after each successful PUT,
                // increment the chunk counter and fire if either
                // threshold hit. Fire-and-forget via spawn (don't
                // block upload progress on the PG round-trip).
                if let Some((pool, sp_hash, claim, state)) = &hb {
                    let fire = {
                        let mut g = state.lock().unwrap_or_else(|e| e.into_inner());
                        g.1 += 1;
                        if g.1 >= HEARTBEAT_CHUNK_INTERVAL
                            || g.0.elapsed() >= HEARTBEAT_TIME_INTERVAL
                        {
                            *g = (Instant::now(), 0);
                            true
                        } else {
                            false
                        }
                    };
                    if fire {
                        let pool = pool.clone();
                        let sp_hash = sp_hash.clone();
                        let claim = *claim;
                        rio_common::task::spawn_monitored("chunk-heartbeat", async move {
                            heartbeat_uploading(&pool, &sp_hash, claim).await;
                        });
                    }
                }
                anyhow::Ok(())
            }
        })
        // .max(1): buffer_unordered(0) is Pending-forever; Config::validate
        // rejects 0 at startup, but library callers (tests, embedders)
        // bypass Config.
        .buffer_unordered(max_concurrent.max(1))
        .try_for_each(|()| async { Ok(()) })
        .await?;

    debug!(uploaded, "chunk uploads complete");

    Ok(PutChunkedStats {
        total_chunks: total,
        deduped_chunks: total - uploaded,
    })
}

/// Best-effort rollback. Errors are logged, not propagated — the caller
/// is already returning an error; a rollback failure shouldn't mask it.
/// The orphan scanner (gc/orphan.rs) catches any leaked state.
async fn rollback(
    pool: &PgPool,
    store_path_hash: &[u8],
    token: metadata::PlaceholderToken,
    chunk_hashes: &[Vec<u8>],
) {
    if let Err(e) =
        metadata::delete_manifest_chunked_uploading(pool, store_path_hash, token, chunk_hashes)
            .await
    {
        warn!(error = %e, "rollback of chunked upload failed; orphan scanner will clean up");
    }
}

// ============================================================================
// ChunkCache: read-path caching + singleflight + verification
// ============================================================================

/// In-process chunk cache with singleflight coalescing and BLAKE3 verify.
///
/// Wraps a `ChunkBackend`. GetPath uses this instead of the backend
/// directly. Three layers:
///
/// 1. **moka LRU** — hot chunks stay in memory. Weight-based: tracks
///    byte-size per entry, 2 GiB cap is a real memory bound (not just
///    an entry count that might be 2 GiB or might be 100 MiB depending
///    on chunk-size distribution).
/// 2. **Singleflight** — if N concurrent GetPaths all need chunk X, one
///    backend GET runs; N-1 await the same future. `store.typ`
///    calls this the thundering-herd fix: cold start with 100 builds
///    needing overlapping closures would be O(100×M) S3 GETs without
///    this; with it, O(M).
/// 3. **BLAKE3 verify** — EVERY returned chunk is hashed against the
///    requested hash. This is `store.typ`: "corrupt chunks are re-
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
    /// Output is `Result<Option<Bytes>, String>`: `Ok(None)` is the
    /// backend's authoritative not-found; `Err` is a backend error or
    /// task panic. The distinction is load-bearing (round-16 bug_027):
    /// callers stamp `NotFound` as data-loss/corruption verdicts, so a
    /// transient S3 blip must surface as the retriable
    /// [`ChunkError::Backend`] instead.
    ///
    /// Why BoxFuture instead of `Shared<JoinHandle<...>>` directly:
    /// Shared requires Output: Clone. JoinHandle's output is
    /// `Result<T, JoinError>`; JoinError isn't Clone. So we map the
    /// JoinHandle into `Result<Option<Bytes>, FetchFail>` BEFORE
    /// sharing — the mapped output IS Clone. BoxFuture erases the
    /// unnamable `Map<JoinHandle, closure>` type.
    ///
    /// `Arc` because the PRODUCER owns cleanup: the spawned fetch task
    /// removes its own entry at terminal, so a completed `Shared` is
    /// never discoverable in the map (round-17 merged_bug_061 — the
    /// per-awaiter remove leaked a completed entry when the sole
    /// awaiter was cancelled, and the "self-heal" served that entry's
    /// MEMOIZED verdict to the next caller with zero backend I/O: a
    /// stale `Ok(None)` became a spurious non-retriable NotFound for a
    /// chunk uploaded since; a stale `Err` re-served an S3 blip after
    /// recovery).
    inflight: Arc<DashMap<[u8; 32], InflightFetch>>,
}

/// The Shared-future type stored in `inflight`. Type alias because the
/// full type is 3 lines of generics that would obscure the struct.
///
/// Output: `Ok(None)` = backend authoritatively says not-found;
/// `Err(FetchFail)` = backend ERROR, auth refusal, or task panic. The
/// not-found/error split is round-16 bug_027; the typed error carrier
/// is round-17 merged_bug_061.
type InflightFetch = futures_util::future::Shared<
    futures_util::future::BoxFuture<'static, Result<Option<Bytes>, FetchFail>>,
>;

/// `Clone`-able error carrier across the singleflight `Shared`
/// boundary, classified AT THE PRODUCING STATEMENT — inside the
/// spawned task, where the live `anyhow` chain (and its
/// [`BackendAuthError`] root, when present) is still in hand. The old
/// carrier was `e.to_string()`, which flattened the chain BEFORE
/// `ChunkError` construction: the auth root was unrecoverable
/// downstream, so `storage_error`'s BackendAuthError→FailedPrecondition
/// fail-fast was structurally unreachable for every read — a read-side
/// IAM/KMS misconfiguration retried as UNAVAILABLE forever (round-17
/// merged_bug_061).
///
/// [`BackendAuthError`]: crate::backend::BackendAuthError
#[derive(Debug, Clone)]
enum FetchFail {
    /// [`crate::backend::BackendAuthError`] was in the chain:
    /// deterministic until an operator fixes the role.
    Auth(String),
    /// Everything else (S3 5xx/timeout/connect, task panic): transient.
    Transient(String),
}

/// Default LRU capacity: 2 GiB. Configurable via `ChunkCache::with_capacity`.
///
/// At 64 KiB avg chunk size, 2 GiB holds ~32k chunks. That's enough to
/// keep a whole stdenv closure (~1 GiB of outputs, chunked) hot. For
/// smaller deployments, `with_capacity(256 * 1024 * 1024)` is plenty.
const DEFAULT_CACHE_CAPACITY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Error from `ChunkCache::get_verified`. Three variants, three wire
/// codes (round-16 bug_027; pattern R1 — an error variant whose
/// producers span transient and permanent causes is split, with the
/// code DERIVED from the variant): `NotFound`/`Corrupt` are
/// data-integrity verdicts (`DATA_LOSS`-class, retrying is pointless);
/// `Backend` is transient infrastructure (`UNAVAILABLE`-class, retry).
#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    /// Backend AUTHORITATIVELY returned None — the object is not in
    /// S3. If the manifest says this hash exists, this is data loss.
    /// Caller should propagate as a hard error (not retry — retrying
    /// NotFound is pointless).
    #[error("chunk {} not found in backend (data loss if manifest claims it exists)", hex::encode(.0))]
    NotFound([u8; 32]),

    /// BLAKE3 of the AUTHORITATIVE (backend) bytes doesn't match the
    /// requested hash: S3 bitrot or a backend bug — truthfully
    /// terminal (DATA_LOSS) at every reader. Process-local LRU
    /// corruption is NOT a producer: a corrupt cache hit invalidates
    /// and falls through to the backend in-request (round-17 bug_105),
    /// so this variant only ever describes the authoritative copy.
    /// The corrupt bytes are NOT cached (we verify before insert).
    #[error("chunk {} failed BLAKE3 verification (corrupt; expected {}, got {})",
        hex::encode(.expected), hex::encode(.expected), hex::encode(.actual))]
    Corrupt {
        expected: [u8; 32],
        actual: [u8; 32],
    },

    /// The backend ERRORED (S3 5xx/timeout/connect failure) or the
    /// fetch task panicked — nothing is known about the object's
    /// existence. Transient: retry. MUST NOT be read as data loss
    /// (the round-16 bug_027 conflation: this used to surface as
    /// `NotFound`).
    #[error("chunk {} backend fetch failed (transient): {message}", hex::encode(.hash))]
    Backend { hash: [u8; 32], message: String },

    /// The backend REFUSED the fetch with an authentication/authorization
    /// failure (IRSA/IAM/KMS misconfiguration — [`BackendAuthError`] was
    /// the root). Deterministic until an operator fixes the role:
    /// FAILED_PRECONDITION class at every reader, never silent-retried —
    /// the read-side twin of the write path's auth fail-fast
    /// (round-17 merged_bug_061; the incident class grpc/mod.rs cites:
    /// 12 derivations × 146 retry cycles in 6 minutes).
    ///
    /// [`BackendAuthError`]: crate::backend::BackendAuthError
    #[error(
        "chunk {} backend fetch auth-failed (check S3 credentials/IAM permissions): {message}",
        hex::encode(.hash)
    )]
    AuthFailed { hash: [u8; 32], message: String },
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
    /// ChunkServiceImpl — the goal was "a chunk warmed by GetPath is
    /// hot for GetChunk" which means one cache. StoreServiceImpl
    /// needs the raw backend for writes; it gets it via this accessor
    /// instead of a separate Arc.
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
            inflight: Arc::new(DashMap::new()),
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
            //
            // If verify fails: INVALIDATE the entry and FALL THROUGH to
            // the backend fetch below, in THIS request. The one failure
            // mode this arm exists for is a process-local bit-flip in
            // the cached copy — the designed recovery is a re-fetch
            // (verify-on-get: "corrupt chunks are re-fetched from S3 on
            // cache corruption"), and the backend fetch is right here.
            // The old shape returned `Corrupt` (DATA_LOSS-class,
            // documented "retrying is pointless" at every reader) after
            // performing the invalidation FOR the retry it then told
            // callers not to make (round-17 bug_105). A genuinely
            // corrupt BACKEND object still surfaces as `Corrupt` from
            // the Layer-3 verify below — `Corrupt` now truthfully means
            // "the authoritative copy is bad", terminal at every
            // producer.
            // r[impl store.integrity.verify-on-get]
            match Self::verify(hash, bytes) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    metrics::counter!("rio_store_chunk_cache_corrupt_hits_total").increment(1);
                    tracing::warn!(
                        hash = %hex::encode(hash),
                        error = %e,
                        "LRU hit failed verification — invalidating and re-fetching from backend"
                    );
                    self.lru.invalidate(hash).await;
                }
            }
        } else {
            metrics::counter!("rio_store_chunk_cache_misses_total").increment(1);
        }

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
    /// Returns `Ok(None)` ONLY for the backend's authoritative
    /// not-found; backend errors / task panics are
    /// [`ChunkError::Backend`] (transient) and auth refusals are
    /// [`ChunkError::AuthFailed`] (FAILED_PRECONDITION class). The
    /// PRODUCER (the spawned task) removes the inflight entry at
    /// terminal, so no completed verdict is ever re-served from the map.
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
                let inflight = Arc::clone(&self.inflight);
                let h = *hash;
                // Spawn + map + boxed + shared:
                // - spawn: fetch survives first-caller cancellation
                // - map: JoinHandle's Result<Result<Opt,FetchFail>,JoinError>
                //   → Result<Opt,FetchFail> (neither JoinError nor
                //   anyhow::Error is Clone; FetchFail is the typed
                //   Clone carrier — classified at the producing
                //   statement per R1(e), where the live anyhow chain
                //   still holds the BackendAuthError root)
                // - boxed: erase the unnamable Map<JoinHandle,closure>
                //   type so it fits InflightFetch
                // - shared: N callers await the same result
                //
                // Ok(None) is ONLY the backend's authoritative
                // not-found; errors/panics are Err (round-16 bug_027 —
                // the previous None-conflation surfaced S3 blips as
                // data-loss-class NotFound).
                //
                // PRODUCER-OWNED CLEANUP (round-17 merged_bug_061): the
                // task removes its own entry as its LAST act, before
                // any awaiter can observe the result through the map a
                // second time. The old per-awaiter remove leaked a
                // completed Shared when the sole awaiter was cancelled,
                // and the next caller was served that entry's MEMOIZED
                // verdict with zero backend I/O — a stale Ok(None)
                // became a spurious non-retriable NotFound for a chunk
                // uploaded since (reachable via the PutPath write-ahead
                // window); a stale Err re-served an S3 blip after
                // recovery. A caller racing the removal still awaits
                // the live Shared it already cloned (consistent — that
                // verdict was produced by a fetch that overlapped the
                // caller); a caller arriving after removal starts a
                // FRESH fetch.
                tokio::spawn(async move {
                    let result = match backend.get(&h).await {
                        Ok(opt) => Ok(opt),
                        // Producing statement: the auth bit is read off
                        // the LIVE anyhow chain (BackendAuthError is
                        // planted as the root by S3ChunkBackend), not
                        // off a flattened string.
                        Err(e)
                            if e.downcast_ref::<crate::backend::BackendAuthError>()
                                .is_some() =>
                        {
                            warn!(hash = %hex::encode(h), error = %e,
                                  "chunk backend fetch AUTH-failed (IAM/KMS misconfiguration)");
                            Err(FetchFail::Auth(format!("{e:#}")))
                        }
                        Err(e) => {
                            warn!(hash = %hex::encode(h), error = %e,
                                  "chunk backend fetch failed");
                            Err(FetchFail::Transient(e.to_string()))
                        }
                    };
                    inflight.remove(&h);
                    result
                })
                .map(|join_result| {
                    // Task panic → Err. Log here (the task itself
                    // didn't get to log its own panic). The panicked
                    // task never reached its own remove — but a panic
                    // also means the closure above unwound, and the
                    // entry is removed by the FIRST awaiter that sees
                    // the JoinError (below in singleflight_fetch).
                    match join_result {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(error = %e, "chunk fetch task panicked");
                            Err(FetchFail::Transient(format!(
                                "chunk fetch task panicked: {e}"
                            )))
                        }
                    }
                })
                .boxed()
                .shared()
            })
            .clone();

        // Await the shared fetch. The task continues even if we're
        // cancelled here (it's spawned); the shared handle is just
        // our window into its result. Cleanup is the PRODUCER's (the
        // spawned task removes its own entry at terminal — see above);
        // the one terminal the producer cannot cover is its own PANIC
        // (the closure unwound before the remove), so that single case
        // is swept here by whichever awaiter observes the JoinError
        // first. An awaiter cancelled during this await leaks nothing:
        // the producer's remove already ran or will run.
        let result = shared.await;
        if matches!(&result, Err(FetchFail::Transient(m)) if m.starts_with("chunk fetch task panicked"))
        {
            self.inflight.remove(hash);
        }

        // The typed carrier maps to the typed verdict 1:1 — no
        // re-classification at this boundary (the classification
        // happened at the producing statement inside the task).
        result.map_err(|fail| match fail {
            FetchFail::Auth(message) => ChunkError::AuthFailed {
                hash: *hash,
                message,
            },
            FetchFail::Transient(message) => ChunkError::Backend {
                hash: *hash,
                message,
            },
        })
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
}

// r[verify store.singleflight+2]
// r[verify store.integrity.verify-on-get]
#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::backend::MemoryChunkBackend;
    use crate::test_helpers::mem_backend;

    /// Real hash/data pair: the BLAKE3 of "hello chunk cache".
    /// `get_verified` hashes the data and compares, so these must match.
    fn sample_chunk() -> ([u8; 32], Bytes) {
        let data = Bytes::from_static(b"hello chunk cache");
        let hash = *blake3::hash(&data).as_bytes();
        (hash, data)
    }

    fn make_cache() -> (Arc<MemoryChunkBackend>, ChunkCache) {
        let backend = mem_backend();
        // Small capacity so eviction tests don't need GB of data.
        let cache =
            ChunkCache::with_capacity(Arc::clone(&backend) as Arc<dyn ChunkBackend>, 1024 * 1024);
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
        // Overwrite backend with garbage. If the second get reaches the
        // backend (no LRU hit), BLAKE3 verify fails → test fails.

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

    /// A stale inflight entry SELF-HEALS on the next `get_verified()`.
    ///
    /// The scenario (from the comment at `self.inflight.remove(hash)`):
    /// an awaiter is cancelled during `shared.await`, so its `remove()`
    /// never ran. The spawned backend task completed anyway (it's
    /// detached), leaving a completed Shared in the inflight map.
    ///
    /// Rather than race the scheduler to trigger real cancellation
    /// (which proved fragile across tokio's yield-draining semantics),
    /// we insert the leaked state directly: a completed Shared under
    /// `hash`. Then we verify the next `get_verified()` for that hash:
    ///   1. Finds the existing entry via `entry().or_insert_with()`
    ///   2. Awaits the (already-complete) Shared instantly — no I/O
    ///   3. Runs `self.inflight.remove(hash)` — the self-heal
    ///
    /// The backend is EMPTY for this hash. If `or_insert_with` had
    /// somehow NOT found the entry (spawned a fresh fetch), the get
    /// would return `NotFound` — so getting the data proves the stale
    /// PRODUCER-OWNED cleanup (round-17 merged_bug_061): the spawned
    /// fetch task removes its own inflight entry at terminal, so a
    /// cancelled sole awaiter leaks nothing — and, decisively, no
    /// LATER caller can be served the completed fetch's MEMOIZED
    /// verdict from the map. The pre-fix "self-heal" did exactly that:
    /// the next get found the completed Shared and consumed its stale
    /// result with zero backend I/O.
    // r[verify store.singleflight+2]
    #[tokio::test]
    async fn producer_removes_entry_when_sole_awaiter_cancelled() {
        use std::sync::atomic::{AtomicU32, Ordering};

        /// Backend that counts fetches, answers None on the first and
        /// the real bytes afterwards — the PutPath write-ahead shape
        /// (chunk uploaded between two reads).
        struct FlakyCountingBackend {
            data: Bytes,
            hash: [u8; 32],
            gets: AtomicU32,
        }
        #[async_trait::async_trait]
        impl ChunkBackend for FlakyCountingBackend {
            async fn put(&self, _: &[u8; 32], _: Bytes) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
                let n = self.gets.fetch_add(1, Ordering::SeqCst);
                // Slow enough that the first awaiter can be cancelled
                // mid-await deterministically.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if n == 0 {
                    Ok(None)
                } else if hash == &self.hash {
                    Ok(Some(self.data.clone()))
                } else {
                    Ok(None)
                }
            }
            async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
                Ok(vec![false; hashes.len()])
            }
            fn key_for(&self, hash: &[u8; 32]) -> String {
                hex::encode(hash)
            }
            async fn delete_by_key(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let (hash, data) = sample_chunk();
        let backend = Arc::new(FlakyCountingBackend {
            data: data.clone(),
            hash,
            gets: AtomicU32::new(0),
        });
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));

        // First caller: starts the fetch, gets CANCELLED mid-await.
        let c = Arc::clone(&cache);
        let awaiter = tokio::spawn(async move { c.get_verified(&hash).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        awaiter.abort();
        let _ = awaiter.await;

        // The detached fetch task runs to completion and removes ITS
        // OWN entry — no awaiter needed.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !cache.inflight.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("producer must remove its own inflight entry after a cancelled awaiter");

        // STALE-VERDICT REGRESSION: the chunk now exists (second
        // backend answer). The next get must perform a FRESH fetch and
        // see it — under the pre-fix leak it consumed the completed
        // Shared's stale Ok(None) and returned a spurious
        // non-retriable NotFound with zero backend I/O.
        let got = cache
            .get_verified(&hash)
            .await
            .expect("fresh fetch must see the now-present chunk, not a stale NotFound");
        assert_eq!(got, data);
        assert_eq!(
            backend.gets.load(Ordering::SeqCst),
            2,
            "the second get performed a real backend fetch (no memoized verdict)"
        );
    }

    /// Read-side auth refusals carry their class through the Shared
    /// boundary: BackendAuthError at the root of the backend's error
    /// chain surfaces as ChunkError::AuthFailed (FAILED_PRECONDITION
    /// at every reader), never as the retriable Backend variant — the
    /// pre-fix e.to_string() carrier flattened the chain and made the
    /// auth fail-fast structurally unreachable for reads.
    // r[verify store.singleflight+2]
    #[tokio::test]
    async fn auth_refusal_is_typed_through_the_shared_boundary() {
        struct AuthRefusingBackend;
        #[async_trait::async_trait]
        impl ChunkBackend for AuthRefusingBackend {
            async fn put(&self, _: &[u8; 32], _: Bytes) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get(&self, _: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
                Err(anyhow::Error::new(crate::backend::BackendAuthError)
                    .context("GetObject: AccessDenied (simulated IRSA misconfiguration)"))
            }
            async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
                Ok(vec![false; hashes.len()])
            }
            fn key_for(&self, hash: &[u8; 32]) -> String {
                hex::encode(hash)
            }
            async fn delete_by_key(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let cache = ChunkCache::new(Arc::new(AuthRefusingBackend));
        let (hash, _) = sample_chunk();
        let err = cache.get_verified(&hash).await.unwrap_err();
        match err {
            ChunkError::AuthFailed { message, .. } => {
                assert!(
                    message.contains("AccessDenied"),
                    "the producing context survives the boundary: {message}"
                );
            }
            other => {
                panic!("auth refusal must be AuthFailed (FAILED_PRECONDITION class), got {other:?}")
            }
        }
        // And the producer cleaned up: the next call retries (fails the
        // same way, but through a FRESH fetch — not a memoized verdict).
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !cache.inflight.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok(),
            "producer removes the entry on the auth-failure terminal too"
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

    /// Process-local LRU corruption is recovered IN-REQUEST: the
    /// corrupt entry is invalidated and the SAME call falls through to
    /// the backend, whose intact bytes are served and re-cached. The
    /// pre-fix shape returned `Corrupt` (DATA_LOSS-class, "retrying is
    /// pointless" at every reader) after invalidating FOR the retry it
    /// told callers not to make — a bit-flip terminally failed a
    /// servable request (round-17 bug_105; the spec's verify-on-get
    /// already mandated the re-fetch).
    // r[verify store.integrity.verify-on-get]
    #[tokio::test]
    async fn lru_corrupt_hit_recovers_in_request() {
        let (backend, cache) = make_cache();
        let (hash, good_data) = sample_chunk();

        // Seed the backend with GOOD data.
        backend.put(&hash, good_data.clone()).await.unwrap();

        // Manually insert CORRUPT bytes into the LRU, simulating
        // memory corruption (cosmic ray bit-flip, bad RAM). In
        // production, good data went in; corruption happened later
        // inside the cache's memory.
        cache
            .lru
            .insert(hash, Bytes::from_static(b"bit-flipped garbage"))
            .await;
        cache.lru.run_pending_tasks().await;

        // THE KEY ASSERTION (inverted from the pre-fix pin): the FIRST
        // call already succeeds — invalidate, fall through, serve the
        // backend's intact bytes.
        let first = cache.get_verified(&hash).await.expect(
            "the corrupt-hit request itself must recover via the \
             backend fall-through",
        );
        assert_eq!(first, good_data, "served bytes are the backend's");

        // And the recovery re-cached the good bytes: a second call is
        // a clean LRU hit (corrupting the backend afterwards proves
        // the second call never re-fetches — a re-fetch would fail
        // layer-3 verification).
        backend.corrupt_for_test(&hash, Bytes::from_static(b"DELETED"));
        let second = cache
            .get_verified(&hash)
            .await
            .expect("second call is a clean LRU hit of the re-cached bytes");
        assert_eq!(second, good_data);
    }

    /// `Corrupt` is now truthfully terminal at every producer: when
    /// the AUTHORITATIVE (backend) copy is corrupt, the in-request
    /// fall-through still ends in `Corrupt` from the layer-3 verify —
    /// the LRU recovery path cannot launder backend corruption.
    // r[verify store.integrity.verify-on-get]
    #[tokio::test]
    async fn backend_corruption_still_terminal_after_lru_fallthrough() {
        let (backend, cache) = make_cache();
        let (hash, good_data) = sample_chunk();

        // Backend holds CORRUPT bytes for this hash.
        backend
            .put(&hash, Bytes::from_static(b"authoritative copy is bad"))
            .await
            .unwrap();
        let _ = good_data;

        // LRU also corrupt: the fall-through reaches the backend and
        // must STILL fail closed.
        cache
            .lru
            .insert(hash, Bytes::from_static(b"bit-flipped garbage"))
            .await;
        cache.lru.run_pending_tasks().await;

        let err = cache.get_verified(&hash).await.unwrap_err();
        assert!(
            matches!(err, ChunkError::Corrupt { .. }),
            "backend corruption surfaces as Corrupt from the layer-3 verify: {err:?}"
        );
    }

    /// Backend whose `get` ERRORS (S3 5xx / timeout class). Distinct
    /// from an empty backend, whose `get` returns `Ok(None)`.
    struct ErroringBackend;

    #[async_trait::async_trait]
    impl ChunkBackend for ErroringBackend {
        async fn put(&self, _hash: &[u8; 32], _data: Bytes) -> anyhow::Result<()> {
            anyhow::bail!("put not under test")
        }
        async fn get(&self, _hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
            anyhow::bail!("simulated S3 outage")
        }
        async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
            Ok(vec![false; hashes.len()])
        }
        fn key_for(&self, hash: &[u8; 32]) -> String {
            hex::encode(hash)
        }
        async fn delete_by_key(&self, _key: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // r[verify store.singleflight+2]
    /// THE bug_027 split: a backend ERROR surfaces as the transient
    /// `ChunkError::Backend`, NEVER as `NotFound` (which callers stamp
    /// into data-loss/corruption verdicts). Pre-fix, the singleflight
    /// task flattened errors into `None` and every S3 blip read as
    /// data loss. Inflight cleanup still runs, so recovery retries
    /// cleanly.
    #[tokio::test]
    async fn backend_error_is_transient_not_notfound() {
        let cache = ChunkCache::with_capacity(Arc::new(ErroringBackend), 1024 * 1024);
        let (hash, _) = sample_chunk();

        let result = cache.get_verified(&hash).await;
        match result {
            Err(ChunkError::Backend { hash: h, message }) => {
                assert_eq!(h, hash);
                assert!(
                    message.contains("simulated S3 outage"),
                    "carries the producing error: {message}"
                );
            }
            other => panic!("backend error must be ChunkError::Backend (transient), got {other:?}"),
        }
        assert!(
            cache.inflight.is_empty(),
            "inflight cleaned up after an erroring fetch — next call retries"
        );
    }
}

// r[verify store.cas.upload-bounded]
#[cfg(test)]
mod upload_tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Backend that tracks the high-water-mark of concurrent `put()`
    /// calls. Each put increments on entry, yields (so other tasks
    /// can stack up), records the max seen, then decrements.
    #[derive(Default)]
    struct HighWaterBackend {
        in_flight: AtomicUsize,
        high_water: AtomicUsize,
        put_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ChunkBackend for HighWaterBackend {
        async fn put(&self, _hash: &[u8; 32], _data: Bytes) -> anyhow::Result<()> {
            self.put_count.fetch_add(1, Ordering::SeqCst);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.high_water.fetch_max(now, Ordering::SeqCst);
            // Yield so the buffer_unordered driver can start more
            // futures before this one completes. Without this the
            // test would observe ~1 regardless of the bound (each
            // put would finish before the next starts under a
            // single-threaded runtime).
            tokio::task::yield_now().await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
        async fn get(&self, _: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
            unimplemented!("upload test uses put only")
        }
        async fn exists_batch(&self, _: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
            unimplemented!()
        }
        fn key_for(&self, _: &[u8; 32]) -> String {
            unimplemented!()
        }
        async fn delete_by_key(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    /// Shared backing buffer for synthetic chunks. `do_upload` computes
    /// each chunk's offset within `nar_data` by pointer arithmetic, so
    /// every `Chunk.data` here MUST be a subslice of the same buffer
    /// the test passes as `nar_data`.
    static SYNTH_DATA: &[u8] = b"x";

    /// Synthesize N distinct-hash chunks backed by [`SYNTH_DATA`].
    /// The data content doesn't matter (HighWaterBackend ignores it);
    /// hashes just need to be unique so the `inserted` filter passes.
    fn synth_chunks(n: usize) -> (Vec<chunker::Chunk<'static>>, HashSet<Vec<u8>>) {
        let mut chunks = Vec::with_capacity(n);
        let mut inserted = HashSet::with_capacity(n);
        for i in 0..n {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(i as u64).to_le_bytes());
            inserted.insert(hash.to_vec());
            chunks.push(chunker::Chunk {
                hash,
                data: SYNTH_DATA,
            });
        }
        (chunks, inserted)
    }

    /// The core assertion: with max_concurrent=8 and 200 chunks, the
    /// high-water-mark of concurrent puts never exceeds 8.
    /// Regression guard for the aws-sdk pool saturation that broke
    /// rsb on large NARs (python3 = 374MB ≈ 1900 chunks).
    #[tokio::test]
    async fn put_chunked_concurrency_bounded() {
        let backend = Arc::new(HighWaterBackend::default());
        let backend_dyn: Arc<dyn ChunkBackend> = backend.clone();
        let (chunks, inserted) = synth_chunks(200);

        let stats = do_upload(None, &backend_dyn, SYNTH_DATA, &chunks, &inserted, 8)
            .await
            .expect("do_upload should succeed");

        let hw = backend.high_water.load(Ordering::SeqCst);
        assert!(
            hw <= 8,
            "high-water-mark {hw} exceeds max_concurrent=8; \
             buffer_unordered is not bounding uploads"
        );
        // Sanity: all 200 uploaded (no dedup in this synthetic set).
        assert_eq!(stats.total_chunks, 200);
        assert_eq!(stats.deduped_chunks, 0);
    }

    /// `chunks` is the raw in-order list from `chunk_nar`, so an
    /// intra-NAR repeat (zero-filled pages, identical 16KB+ runs)
    /// appears N times. `needs_upload.contains()` is a predicate, not
    /// a dedup — without the `seen` set, do_upload issues N redundant
    /// PUTs and reports `deduped_chunks = 0` (feeds dedup_ratio gauge).
    #[tokio::test]
    async fn do_upload_dedupes_intra_nar_repeats() {
        let backend = Arc::new(HighWaterBackend::default());
        let backend_dyn: Arc<dyn ChunkBackend> = backend.clone();

        // 10× the same chunk hash + 1 distinct → 11 total, 2 unique.
        let dup = [0xAAu8; 32];
        let other = [0xBBu8; 32];
        let mut chunks: Vec<chunker::Chunk<'static>> = (0..10)
            .map(|_| chunker::Chunk {
                hash: dup,
                data: SYNTH_DATA,
            })
            .collect();
        chunks.push(chunker::Chunk {
            hash: other,
            data: SYNTH_DATA,
        });
        let needs: HashSet<Vec<u8>> = [dup.to_vec(), other.to_vec()].into_iter().collect();

        let stats = do_upload(None, &backend_dyn, SYNTH_DATA, &chunks, &needs, 4)
            .await
            .unwrap();

        assert_eq!(
            backend.put_count.load(Ordering::SeqCst),
            2,
            "exactly one PUT per unique hash"
        );
        assert_eq!(stats.total_chunks, 11);
        assert_eq!(
            stats.deduped_chunks, 9,
            "11 total - 2 uploaded = 9 deduped (intra-NAR repeats)"
        );
    }

    /// Bound=1 degrades to serial — high-water-mark exactly 1.
    /// Proves the knob actually threads through (not just defaulted).
    #[tokio::test]
    async fn put_chunked_concurrency_one_is_serial() {
        let backend = Arc::new(HighWaterBackend::default());
        let backend_dyn: Arc<dyn ChunkBackend> = backend.clone();
        let (chunks, inserted) = synth_chunks(50);

        do_upload(None, &backend_dyn, SYNTH_DATA, &chunks, &inserted, 1)
            .await
            .unwrap();

        assert_eq!(
            backend.high_water.load(Ordering::SeqCst),
            1,
            "max_concurrent=1 must serialize uploads"
        );
    }
}
