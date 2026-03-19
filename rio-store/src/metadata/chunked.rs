//! Chunked write-ahead: large NARs split by FastCDC, chunks in S3,
//! manifest_data holds the ordered (blake3, size) list.
//!
//! `upgrade_manifest_to_chunked` takes an EXISTING placeholder (from
//! `insert_manifest_uploading`) and adds manifest_data + increments chunk
// r[impl store.chunk.refcount-txn]
// r[impl store.put.wal-manifest]
//! refcounts — the placeholder is the idempotency lock, created BEFORE the
//! NAR stream is consumed, before we know the size. Refcounts go up here
//! (not at complete) so GC between upload and complete sees count > 0.

use super::*;
use sqlx::PgPool;
use std::collections::HashSet;
use tracing::{debug, instrument};

// ---------------------------------------------------------------------------
// Chunked manifest ops
// ---------------------------------------------------------------------------

/// Upgrade an existing 'uploading' manifest to chunked: write manifest_data
/// + increment chunk refcounts.
///
/// # Why this takes an EXISTING placeholder
///
/// grpc.rs PutPath runs `insert_manifest_uploading()` at step 3, BEFORE
/// the NAR stream is consumed (it's the idempotency lock — prevents
/// concurrent uploaders). Only at step 6, after buffering + validating,
/// do we know the size. At that point we already OWN the placeholder;
/// this function adds the chunked metadata to it.
///
/// A standalone `insert_manifest_chunked_uploading` that creates its own
/// placeholder would either (a) need to know the size upfront (can't —
/// stream isn't consumed yet), or (b) delete+recreate the placeholder
/// (window for another uploader to slip in). Upgrade-in-place avoids both.
///
/// # Why refcounts are incremented here (before upload), not at complete
///
/// Per `store.md:94`: incrementing before upload protects chunks from GC
/// sweep immediately. If a GC pass runs between upload and complete, it
/// sees refcount > 0 and skips. If we waited until complete, a GC between
/// "chunks uploaded to S3" and "status flipped" would sweep → orphaned.
///
/// The tradeoff: if the upload fails and we forget to decrement, refcounts
/// are leaked. The orphan scanner (future phase) catches this via stale
/// 'uploading' manifests.
///
/// # Refcount UPSERT
///
/// `INSERT ... ON CONFLICT DO UPDATE` is row-level atomic — no explicit
/// SELECT FOR UPDATE needed. Two concurrent PutPaths referencing the same
/// chunk both increment correctly (PG resolves the conflict, second one
/// sees the first's row and runs the UPDATE clause).
#[instrument(skip(pool, chunk_list, chunk_hashes, chunk_sizes), fields(store_path_hash = hex::encode(store_path_hash), chunks = chunk_hashes.len()))]
pub async fn upgrade_manifest_to_chunked(
    pool: &PgPool,
    store_path_hash: &[u8],
    chunk_list: &[u8],        // serialized Manifest
    chunk_hashes: &[Vec<u8>], // each is a 32-byte BLAKE3
    chunk_sizes: &[i64],      // parallel to chunk_hashes
) -> Result<HashSet<Vec<u8>>> {
    let mut tx = pool.begin().await?;

    // Sanity: the manifests row MUST exist with status='uploading'.
    // If it doesn't, the caller's step-3 placeholder was deleted
    // (concurrent cleanup? bug?) — fail loudly.
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM manifests
            WHERE store_path_hash = $1 AND status = 'uploading'
        )
        "#,
    )
    .bind(store_path_hash)
    .fetch_one(&mut *tx)
    .await?;
    if !exists {
        return Err(MetadataError::PlaceholderMissing {
            store_path: hex::encode(store_path_hash),
        });
    }

    // manifest_data: the chunk list. No ON CONFLICT — the placeholder
    // from step 3 didn't write manifest_data, so this row shouldn't
    // exist. If it does (caller called us twice?), PG errors on PK
    // conflict — that's a bug, let it fail.
    sqlx::query(
        r#"
        INSERT INTO manifest_data (store_path_hash, chunk_list)
        VALUES ($1, $2)
        "#,
    )
    .bind(store_path_hash)
    .bind(chunk_list)
    .execute(&mut *tx)
    .await?;

    // Refcount UPSERT. UNNEST over parallel arrays (PG errors if lengths
    // differ — caller guarantees equal, this is a sanity check).
    //
    // The array-of-1s for initial refcount: can't use a literal `1` in
    // the UNNEST position (not an array). Materializing N×1 is mildly
    // silly but cleaner than CROSS JOIN with a single-row constant.
    //
    // ON CONFLICT DO UPDATE is atomic per-row. PG's conflict resolution
    // serializes INSERT vs UPDATE — two concurrent PutPaths with
    // overlapping chunk lists both increment correctly.
    //
    // `deleted = false`: resurrects a chunk that GC sweep marked for
    // deletion (refcount hit 0) between sweep and drain. Without
    // this, PutPath would bump refcount but leave `deleted=true` →
    // chunk still looks dead. The drain re-check (drain.rs) is the
    // PRIMARY guard; this is defense-in-depth so `chunks` row state
    // is self-consistent (refcount>0 implies deleted=false).
    //
    // r[impl store.cas.upsert-inserted]
    // RETURNING (refcount = 1) AS inserted: atomic with the upsert —
    // no re-query race. `refcount = 1` on the returned row means
    // either (a) fresh insert, or (b) resurrection from refcount=0
    // (soft-deleted, awaiting GC drain). BOTH need upload — for (b),
    // S3 may have already deleted the object.
    //
    // Audit B1 #8: `xmax = 0` would be WRONG here. Resurrection
    // fires the CONFLICT clause → xmax != 0 → xmax says "skip". But
    // the chunk is NOT in S3 anymore. `refcount = 1` says "upload"
    // in both cases. Also doesn't depend on PG system columns.
    //
    // RETURNING sees the POST-update row state (SQL standard). So:
    //   fresh INSERT         → refcount = 1 (from UNNEST)  → inserted = true
    //   CONFLICT, prev rc=0  → refcount = 0+1 = 1          → inserted = true
    //   CONFLICT, prev rc≥1  → refcount = rc+1 ≥ 2         → inserted = false
    let rows: Vec<(Vec<u8>, bool)> = sqlx::query_as(
        r#"
        INSERT INTO chunks (blake3_hash, refcount, size)
        SELECT * FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[])
               AS t(hash, one, size)
        ON CONFLICT (blake3_hash) DO UPDATE
            SET refcount = chunks.refcount + 1, deleted = false
        RETURNING blake3_hash, (refcount = 1) AS inserted
        "#,
    )
    .bind(chunk_hashes)
    .bind(vec![1i64; chunk_hashes.len()])
    .bind(chunk_sizes)
    .fetch_all(&mut *tx)
    .await?;

    let inserted: HashSet<Vec<u8>> = rows
        .into_iter()
        .filter_map(|(h, ins)| ins.then_some(h))
        .collect();

    tx.commit().await?;
    Ok(inserted)
}

/// Finalize a chunked upload: fill real narinfo + flip status to 'complete'.
///
/// Does NOT write inline_blob (stays NULL — that's the chunked marker).
/// Does NOT touch manifest_data (already written at uploading time).
/// Does NOT touch refcounts (already incremented at uploading time).
///
/// Just the narinfo UPDATE + status flip. Same atomic guarantees as the
/// inline variant.
#[instrument(skip(pool, info), fields(store_path = %info.store_path.as_str()))]
pub async fn complete_manifest_chunked(pool: &PgPool, info: &ValidatedPathInfo) -> Result<()> {
    let mut tx = pool.begin().await?;

    if update_narinfo_complete(&mut tx, info).await? == 0 {
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }

    // Flip status. inline_blob stays NULL — that's what makes get_manifest()
    // return Chunked instead of Inline.
    let manifest_result = sqlx::query(
        r#"
        UPDATE manifests SET
            status     = 'complete',
            updated_at = now()
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .execute(&mut *tx)
    .await?;

    if manifest_result.rows_affected() == 0 {
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }

    tx.commit().await?;
    debug!(store_path = %info.store_path.as_str(), "chunked upload completed");
    Ok(())
}

/// Reclaim a failed chunked upload: decrement refcounts + delete rows.
///
/// **Must be called with the SAME chunk_hashes that were passed to
/// insert_manifest_chunked_uploading.** Decrementing a different set
/// would corrupt refcounts. The caller (cas.rs) holds the Manifest
/// across the upload, so this invariant is easy to maintain.
///
/// Same safety guards as the inline variant: only deletes rows where
/// `nar_size = 0` / `status = 'uploading'` so a concurrent successful
/// upload isn't touched.
#[instrument(skip(pool, chunk_hashes), fields(store_path_hash = hex::encode(store_path_hash), chunks = chunk_hashes.len()))]
pub async fn delete_manifest_chunked_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    chunk_hashes: &[Vec<u8>],
) -> Result<()> {
    let mut tx = pool.begin().await?;

    // Decrement refcounts FIRST. If we deleted manifest_data first and
    // then crashed before decrementing, the refcounts would be leaked
    // forever (no manifest references them, but count > 0 so GC skips).
    // Decrementing first means a crash here leaves manifest_data
    // pointing at chunks with count=0 — the orphan scanner (later phase)
    // catches that by finding stale 'uploading' manifests.
    //
    // `refcount - 1` can go negative if the caller passes wrong hashes.
    // That's a bug and SHOULD be visible — no GREATEST(0, ...) clamp,
    // let the -1 show up in monitoring.
    sqlx::query(
        r#"
        UPDATE chunks SET refcount = refcount - 1
        WHERE blake3_hash = ANY($1)
        "#,
    )
    .bind(chunk_hashes)
    .execute(&mut *tx)
    .await?;

    // Delete manifest_data (via CASCADE from manifests, but explicit for
    // clarity and to not depend on schema details).
    sqlx::query(
        r#"
        DELETE FROM manifest_data
        WHERE store_path_hash = $1
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    // manifests + narinfo placeholders (same guards as inline variant).
    sqlx::query(
        r#"
        DELETE FROM manifests
        WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        DELETE FROM narinfo
        WHERE store_path_hash = $1 AND nar_size = 0
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Total chunks in the store. For the `rio_store_chunks_total` gauge.
///
/// Counts all rows regardless of refcount/deleted — the gauge answers
/// "how many distinct chunks exist in S3" (approximately; PG is the
/// source of truth for intent, S3 might have orphans). For "active"
/// chunks, filter `WHERE refcount > 0 AND deleted = FALSE` — but
/// that's a different metric (not this one).
#[instrument(skip(pool))]
pub async fn count_chunks(pool: &PgPool) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Find which chunks are NOT yet in the `chunks` table.
///
/// This is the dedup pre-check: PutPath calls this BEFORE uploading to
/// skip chunks that already exist. Returns a `Vec<bool>` parallel to the
/// input: `result[i] == true` means `hashes[i]` is missing (upload it).
///
/// Checks PG, not S3. The `chunks` table is the source of truth for
/// "which chunks do we know about" — it's populated at
/// insert_manifest_chunked_uploading time (step 1), BEFORE S3 upload.
/// So a chunk can be in PG but not yet in S3 (in-progress upload). That's
/// fine for the dedup check: another uploader is handling it, we can
/// skip. If their upload fails, they decrement and we'll see it missing
/// on the next attempt.
///
/// One PG roundtrip for N chunks. Beats N S3 HeadObject calls by ~10×
/// on latency alone.
#[instrument(skip(pool, hashes), fields(count = hashes.len()))]
pub async fn find_missing_chunks(pool: &PgPool, hashes: &[Vec<u8>]) -> Result<Vec<bool>> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }

    // ANY($1) with a bytea[] — returns the hashes that DO exist.
    let present: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT blake3_hash FROM chunks
        WHERE blake3_hash = ANY($1)
        "#,
    )
    .bind(hashes)
    .fetch_all(pool)
    .await?;

    // Invert: present → missing. HashSet for O(1) membership instead of
    // O(N) scan per input hash (N² total without the set).
    let present_set: std::collections::HashSet<&[u8]> =
        present.iter().map(|(h,)| h.as_slice()).collect();

    Ok(hashes
        .iter()
        .map(|h| !present_set.contains(h.as_slice()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// PG constraint: duplicate hashes in the UNNEST batch → PG error
    /// "ON CONFLICT DO UPDATE command cannot affect row a second time".
    /// cas.rs put_chunked dedups before calling this. This test documents
    /// the PG behavior that motivates the dedup.
    #[tokio::test]
    async fn upgrade_duplicate_hashes_pg_rejects() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed placeholder (upgrade requires an existing 'uploading'
        // manifest row).
        let store_path_hash = vec![0xAAu8; 32];
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dup-chunks";
        crate::metadata::insert_manifest_uploading(&db.pool, &store_path_hash, path, &[])
            .await
            .unwrap();

        // Duplicate hashes: same BLAKE3 twice. FastCDC can produce
        // this for identical content blocks (e.g., zero-filled pages).
        let dup_hash = vec![0xBBu8; 32];
        let chunk_hashes = vec![dup_hash.clone(), dup_hash.clone()];
        let chunk_sizes = vec![1024i64, 1024i64];

        let result = upgrade_manifest_to_chunked(
            &db.pool,
            &store_path_hash,
            b"dummy-chunk-list",
            &chunk_hashes,
            &chunk_sizes,
        )
        .await;

        // PG rejects with SQLSTATE 21000 (cardinality violation).
        // This documents WHY cas.rs must dedup before calling us.
        assert!(
            result.is_err(),
            "duplicate hashes in UNNEST batch MUST be rejected by PG"
        );
        let err = result.unwrap_err();
        let err_str = format!("{err}");
        assert!(
            err_str.contains("affect row a second time") || err_str.contains("21000"),
            "expected PG cardinality violation, got: {err_str}"
        );
    }

    /// Deduped hashes → upgrade succeeds. Proves the cas.rs dedup is correct.
    #[tokio::test]
    async fn upgrade_deduped_hashes_ok() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let store_path_hash = vec![0xCCu8; 32];
        let path = "/nix/store/cccccccccccccccccccccccccccccccc-deduped";
        crate::metadata::insert_manifest_uploading(&db.pool, &store_path_hash, path, &[])
            .await
            .unwrap();

        // One unique hash (cas.rs's dedup would produce this from
        // the duplicate input above).
        let chunk_hashes = vec![vec![0xDDu8; 32]];
        let chunk_sizes = vec![1024i64];

        upgrade_manifest_to_chunked(
            &db.pool,
            &store_path_hash,
            b"dummy-chunk-list",
            &chunk_hashes,
            &chunk_sizes,
        )
        .await
        .expect("deduped hashes should succeed");

        // refcount = 1 (one unique hash).
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk_hashes[0])
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 1, "deduped insert → refcount = 1");
    }

    /// Seed an 'uploading' placeholder for the given store-path hash.
    /// Path string is synthesized from the first hash byte (distinct
    /// enough for unit tests; the narinfo placeholder just needs a
    /// valid-shaped path).
    async fn seed_placeholder(pool: &PgPool, store_path_hash: &[u8]) {
        let b = store_path_hash[0];
        let path = format!("/nix/store/{}-p208-test", format!("{b:02x}").repeat(16));
        crate::metadata::insert_manifest_uploading(pool, store_path_hash, &path, &[])
            .await
            .unwrap();
    }

    /// Sequential upserts simulate two PutPaths: first inserts {A,B},
    /// second inserts {A,C}. RETURNING (refcount=1) tells each which
    /// chunks THEY freshly inserted — no re-query, no race window.
    ///
    /// First call: A,B both new → both in inserted set.
    /// Second call: A already at refcount=1 → bumps to 2 → NOT in set.
    ///              C new → in set.
    #[tokio::test]
    async fn upsert_returning_sequential_inserted_set() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk_a = vec![0xA1u8; 32];
        let chunk_b = vec![0xB2u8; 32];
        let chunk_c = vec![0xC3u8; 32];

        // --- First upsert: {A, B} ---
        let sph1 = vec![0x11u8; 32];
        seed_placeholder(&db.pool, &sph1).await;
        let ins1 = upgrade_manifest_to_chunked(
            &db.pool,
            &sph1,
            b"manifest-1",
            &[chunk_a.clone(), chunk_b.clone()],
            &[1024, 2048],
        )
        .await
        .unwrap();

        assert_eq!(ins1.len(), 2, "first upsert: both A and B fresh");
        assert!(ins1.contains(&chunk_a), "A freshly inserted");
        assert!(ins1.contains(&chunk_b), "B freshly inserted");

        // --- Second upsert: {A, C} ---
        // A is already at refcount=1; this upsert bumps it to 2.
        // RETURNING sees refcount=2 → (refcount=1) is false → A NOT
        // in the inserted set. C is fresh.
        let sph2 = vec![0x22u8; 32];
        seed_placeholder(&db.pool, &sph2).await;
        let ins2 = upgrade_manifest_to_chunked(
            &db.pool,
            &sph2,
            b"manifest-2",
            &[chunk_a.clone(), chunk_c.clone()],
            &[1024, 4096],
        )
        .await
        .unwrap();

        assert_eq!(ins2.len(), 1, "second upsert: only C is fresh");
        assert!(
            !ins2.contains(&chunk_a),
            "A already present (refcount 1→2) — NOT in inserted set"
        );
        assert!(ins2.contains(&chunk_c), "C freshly inserted");

        // Ground truth: refcounts as expected.
        let rc_a: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk_a)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc_a, 2, "A referenced by two manifests");
    }

    /// The resurrection case (Audit B1 #8): chunk at refcount=0,
    /// deleted=true (soft-deleted by GC sweep, awaiting drain). An
    /// upsert resurrects it — refcount goes 0→1, deleted flips false.
    ///
    /// This MUST show up in the inserted set: S3 may have already
    /// deleted the object between sweep and now. `xmax = 0` would miss
    /// this (CONFLICT fired → xmax != 0 → xmax says skip → data loss).
    /// `refcount = 1` says upload.
    #[tokio::test]
    async fn upsert_returning_resurrection_is_inserted() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0xDEu8; 32];

        // Seed the chunk at refcount=0, deleted=true — the post-sweep,
        // pre-drain state. Directly INSERT (bypassing the upsert path)
        // to set up the exact resurrection precondition.
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 1024, true)",
        )
        .bind(&chunk)
        .execute(&db.pool)
        .await
        .unwrap();

        // Precondition: confirm the seeded state. If the schema changes
        // (e.g., deleted column removed, refcount constraint), this
        // fails loudly here instead of making the main assertion
        // vacuously pass.
        let (rc0, del0): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(rc0, 0, "precondition: refcount=0 (soft-deleted)");
        assert!(del0, "precondition: deleted=true (awaiting drain)");

        // Upsert resurrects: ON CONFLICT → refcount 0+1=1, deleted=false.
        let sph = vec![0xDDu8; 32];
        seed_placeholder(&db.pool, &sph).await;
        let ins = upgrade_manifest_to_chunked(
            &db.pool,
            &sph,
            b"manifest-resurrect",
            &[chunk.clone()],
            &[1024],
        )
        .await
        .unwrap();

        // THE KEY ASSERTION: resurrected chunk IS in the inserted set.
        // xmax-based check would return empty here (CONFLICT fired).
        assert!(
            ins.contains(&chunk),
            "resurrected chunk (refcount 0→1) MUST be in inserted set \
             — S3 may have already deleted it"
        );

        // Ground truth: refcount=1, deleted=false.
        let (rc, del): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(rc, 1, "resurrected: 0→1");
        assert!(!del, "resurrected: deleted flipped false");
    }
}
