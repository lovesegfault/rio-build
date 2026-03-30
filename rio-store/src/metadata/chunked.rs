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
use uuid::Uuid;

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
    // r[impl store.chunk.refcount-txn]
    // Co-sort (hash, size) pairs by hash before UNNEST: same deadlock
    // prevention as the rollback path (see delete_manifest_chunked_
    // uploading). ON CONFLICT DO UPDATE acquires row locks on the
    // conflicted rows in UNNEST input order; two concurrent upgrades
    // with reversed-order overlapping sets would otherwise deadlock.
    // The co-sort keeps each hash paired with its size.
    let mut pairs: Vec<(Vec<u8>, i64)> = chunk_hashes
        .iter()
        .cloned()
        .zip(chunk_sizes.iter().copied())
        .collect();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let (chunk_hashes, chunk_sizes): (Vec<Vec<u8>>, Vec<i64>) = pairs.into_iter().unzip();
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
    .bind(&chunk_hashes)
    .bind(vec![1i64; chunk_hashes.len()])
    .bind(&chunk_sizes)
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
    // M023 `CHECK (refcount >= 0)` makes a would-be-negative refcount a
    // constraint violation → transaction rolls back → surfaces as
    // `MetadataError::Other` → gRPC INTERNAL. A negative here means the
    // caller passed wrong hashes (or double-decremented) — fail loud at
    // the source, don't silently leak the chunk. See migrations.rs M_023.
    //
    // r[impl store.chunk.refcount-txn]
    // r[impl store.put.wal-manifest]
    // Sort before UPDATE: consistent lock-acquisition order prevents
    // deadlock (SQLSTATE 40P01) when concurrent rollbacks have
    // overlapping chunk sets. PG acquires row locks in ANY() scan
    // order; without sorting, array A=[h1,h2,h3] and B=[h3,h2,h1] →
    // txn A locks h1 waits for h3, txn B locks h3 waits for h1 →
    // circular wait. Sorting makes lock order deterministic across
    // all callers.
    let mut hashes = chunk_hashes.to_vec();
    hashes.sort_unstable();
    sqlx::query(
        r#"
        UPDATE chunks SET refcount = refcount - 1
        WHERE blake3_hash = ANY($1)
        "#,
    )
    .bind(&hashes)
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

/// Find chunks NOT attributed to `tenant_id` in the `chunk_tenants` junction.
///
/// Returns a `Vec<bool>` parallel to the input: `result[i] == true`
/// means `hashes[i]` is missing FOR THIS TENANT (upload it).
///
/// # Tenant-scoped, not chunks-table-scoped
///
/// An unscoped "is there a `chunks` row?" query would leak cross-tenant: Tenant A's worker probes for glibc's chunk;
/// if only tenant B has uploaded glibc, A sees it as missing and must
/// upload it themselves. The upload (PutChunk) then adds A's junction
/// row, so subsequent probes by A hit.
///
/// The security property: A can only learn "I have uploaded this hash
/// before", never "someone else has uploaded this hash." The latter
/// would leak one bit per probe — enough to fingerprint what B is
/// building (probe for known-package chunk hashes → infer B's closure).
///
/// # Legacy chunks (pre-migration-018)
///
/// A chunk in `chunks` with zero `chunk_tenants` rows is reported as
/// MISSING here. First tenant to PutChunk it gets a junction row and
/// subsequently sees it as present — self-healing on first touch. No
/// special "shared legacy" carve-out: simpler query, and the one-time
/// re-upload cost is bounded (content-addressed, idempotent).
#[instrument(skip(pool, hashes), fields(count = hashes.len(), tenant = %tenant_id))]
pub async fn find_missing_chunks_for_tenant(
    pool: &PgPool,
    hashes: &[Vec<u8>],
    tenant_id: Uuid,
) -> Result<Vec<bool>> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }

    // chunk_tenants, NOT chunks. The junction is the source of truth
    // for "tenant X can dedup against this hash". The (tenant_id,
    // blake3_hash) index covers this — equality on tenant_id + ANY
    // probe on blake3_hash is an index-only scan.
    //
    // PG-not-S3: the `chunks` row (and thus its junction row) is
    // populated BEFORE S3 upload, so a hash can be "present" here
    // while the upload is in flight. Fine — another of THIS TENANT'S
    // workers is handling it; if their upload fails, they decrement
    // and we see it missing on retry. One roundtrip for N hashes.
    let present: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT blake3_hash FROM chunk_tenants
        WHERE tenant_id = $1 AND blake3_hash = ANY($2)
        "#,
    )
    .bind(tenant_id)
    .bind(hashes)
    .fetch_all(pool)
    .await?;

    // Invert: present → missing. HashSet for O(1) membership instead
    // of O(N) scan per input hash (N² total without the set).
    let present_set: HashSet<&[u8]> = present.iter().map(|(h,)| h.as_slice()).collect();

    Ok(hashes
        .iter()
        .map(|h| !present_set.contains(h.as_slice()))
        .collect())
}

/// Record tenant attribution for a chunk. Called after PutChunk has
/// written the `chunks` row (FK requires it).
///
/// `ON CONFLICT DO NOTHING`: composite PK is (blake3_hash, tenant_id).
/// Same tenant re-uploading same bytes → idempotent no-op. DIFFERENT
/// tenant, same bytes → new row (the many-to-many case the junction
/// exists for). No race: two concurrent PutChunks by the same tenant
/// both hit the PK, second one no-ops cleanly.
#[instrument(skip(pool, blake3_hash), fields(hash = hex::encode(blake3_hash), tenant = %tenant_id))]
pub async fn record_chunk_tenant(pool: &PgPool, blake3_hash: &[u8], tenant_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO chunk_tenants (blake3_hash, tenant_id)
        VALUES ($1, $2)
        ON CONFLICT (blake3_hash, tenant_id) DO NOTHING
        "#,
    )
    .bind(blake3_hash)
    .bind(tenant_id)
    .execute(pool)
    .await?;
    Ok(())
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
        let path = rio_test_support::fixtures::test_store_path("dup-chunks");
        crate::metadata::insert_manifest_uploading(&db.pool, &store_path_hash, &path, &[])
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
        let path = rio_test_support::fixtures::test_store_path("deduped");
        crate::metadata::insert_manifest_uploading(&db.pool, &store_path_hash, &path, &[])
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
            std::slice::from_ref(&chunk),
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

    // r[verify store.cas.upsert-inserted]
    /// True-concurrent upserts via `tokio::join!`: two PutPaths share
    /// one chunk hash. PG serializes the ON CONFLICT — the first tx to
    /// win sees refcount=1 (fresh INSERT), the second sees refcount=2
    /// (CONFLICT → UPDATE from the committed first tx). Exactly one
    /// gets the shared chunk in its inserted set.
    ///
    /// This is the race the RETURNING clause closes. With the old
    /// re-query approach, both PutPaths could upsert (both increment),
    /// then both re-SELECT and see refcount=2 → both skip upload →
    /// chunk never hits S3. With RETURNING atomic to the upsert, PG's
    /// ON CONFLICT serialization guarantees exactly one winner.
    ///
    /// The assertion is symmetric (XOR) — which side wins depends on
    /// PG's lock acquisition order, not test code. Both outcomes pass.
    #[tokio::test]
    async fn upsert_returning_concurrent_exactly_one_inserted() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let shared = vec![0x5Au8; 32]; // the contested chunk
        let unique_a = vec![0xA0u8; 32];
        let unique_b = vec![0xB0u8; 32];

        // Two store paths (separate placeholders — no contention there).
        let sph_a = vec![0xAAu8; 32];
        let sph_b = vec![0xBBu8; 32];
        seed_placeholder(&db.pool, &sph_a).await;
        seed_placeholder(&db.pool, &sph_b).await;

        // Chunk-array bindings must outlive the join! — inline
        // `&[...]` temporaries drop at end-of-statement, but the
        // futures borrow them across await points.
        let hashes_a = [shared.clone(), unique_a.clone()];
        let hashes_b = [shared.clone(), unique_b.clone()];
        let sizes_a = [1024i64, 2048];
        let sizes_b = [1024i64, 4096];

        // PgPool hands out distinct connections for concurrent calls;
        // each upgrade_manifest_to_chunked runs in its own tx.
        //
        // Under READ COMMITTED (PG default), ON CONFLICT is special-
        // cased: if tx A inserts the shared row and tx B tries to
        // insert the same PK before A commits, B BLOCKS on A's row
        // lock. Once A commits, B re-reads the committed row and runs
        // the UPDATE clause (refcount 1→2). B's RETURNING sees
        // refcount=2 → inserted=false. A's RETURNING saw refcount=1.
        //
        // If the pool has only one connection, the futures serialize
        // at connection acquisition — same XOR outcome, just
        // deterministic (A wins).
        let (ins_a, ins_b) = tokio::join!(
            upgrade_manifest_to_chunked(&db.pool, &sph_a, b"manifest-a", &hashes_a, &sizes_a),
            upgrade_manifest_to_chunked(&db.pool, &sph_b, b"manifest-b", &hashes_b, &sizes_b),
        );
        let ins_a = ins_a.unwrap();
        let ins_b = ins_b.unwrap();

        // Each side's unique chunk is always fresh.
        assert!(ins_a.contains(&unique_a), "A's unique chunk is fresh");
        assert!(ins_b.contains(&unique_b), "B's unique chunk is fresh");

        // THE KEY ASSERTION: exactly one side sees the shared chunk as
        // inserted. refcount goes 0→1→2; only the 0→1 hop yields
        // (refcount=1)=true. XOR — we don't care which side.
        let a_has = ins_a.contains(&shared);
        let b_has = ins_b.contains(&shared);
        assert!(
            a_has ^ b_has,
            "exactly one concurrent upsert sees shared chunk as inserted \
             (got A={a_has}, B={b_has}; both-true = race not closed, \
             both-false = old re-query bug)"
        );

        // Ground truth: final refcount = 2.
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&shared)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 2, "shared chunk referenced by both manifests");
    }

    /// Regression: concurrent rollbacks with reverse-ordered overlapping
    /// chunk sets MUST NOT deadlock.
    ///
    /// Before the sort in `delete_manifest_chunked_uploading`, PG acquired
    /// row locks in the ANY($1) array's scan order. Array A = [h1..h50],
    /// array B = [h50..h1] → txn A locks h1 waits for h50, txn B locks
    /// h50 waits for h1 → circular wait → SQLSTATE 40P01.
    ///
    /// After the sort, both txns acquire locks in the same canonical
    /// order — no circular wait possible. The 5s timeout makes a
    /// regression fail fast rather than hang the suite.
    // r[verify store.chunk.refcount-txn]
    // r[verify store.put.wal-manifest]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rollback_overlapping_no_deadlock() {
        use std::time::Duration;
        use tokio::time::timeout;

        let db = TestDb::new(&crate::MIGRATOR).await;

        // 50 overlapping chunk hashes. Each manifest references all 50
        // (refcount starts at 2 so both rollbacks can decrement without
        // hitting the CHECK (refcount >= 0) constraint).
        let hashes: Vec<Vec<u8>> = (0u8..50).map(|i| vec![i; 32]).collect();
        let sizes: Vec<i64> = vec![1024; 50];

        // Seed via two upgrade_manifest_to_chunked calls → refcount=2
        // for every chunk.
        let sph_a = vec![0xAAu8; 32];
        let sph_b = vec![0xBBu8; 32];
        seed_placeholder(&db.pool, &sph_a).await;
        seed_placeholder(&db.pool, &sph_b).await;
        upgrade_manifest_to_chunked(&db.pool, &sph_a, b"ml-a", &hashes, &sizes)
            .await
            .unwrap();
        upgrade_manifest_to_chunked(&db.pool, &sph_b, b"ml-b", &hashes, &sizes)
            .await
            .unwrap();

        // Forward order for A, reversed for B. Before the fix this is
        // the pathological lock-order inversion.
        let hashes_fwd = hashes.clone();
        let mut hashes_rev = hashes.clone();
        hashes_rev.reverse();

        let pool_a = db.pool.clone();
        let pool_b = db.pool.clone();

        let task_a = tokio::spawn(async move {
            delete_manifest_chunked_uploading(&pool_a, &sph_a, &hashes_fwd).await
        });
        let task_b = tokio::spawn(async move {
            delete_manifest_chunked_uploading(&pool_b, &sph_b, &hashes_rev).await
        });

        let (ra, rb) = timeout(Duration::from_secs(5), async {
            tokio::try_join!(task_a, task_b).expect("tasks should not panic")
        })
        .await
        .expect("concurrent rollbacks must complete within 5s — deadlock detected");

        ra.expect("rollback A should succeed");
        rb.expect("rollback B should succeed");

        // All refcounts back to 0.
        let sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(refcount),0) FROM chunks WHERE blake3_hash = ANY($1)",
        )
        .bind(&hashes)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(sum, 0, "both rollbacks decremented all 50 chunks to zero");
    }
}
