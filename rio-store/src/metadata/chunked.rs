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
    // r[impl store.cas.upsert-inserted+2]
    // RETURNING (uploaded_at IS NULL) AS needs_upload: a chunk needs
    // (re-)upload iff no prior PutPath has confirmed S3 presence via
    // `mark_chunks_uploaded`. Contrast with the previous heuristic
    // `(refcount = 1)`, which assumed "rc≥1 before me ⇒ someone else
    // already uploaded" — false when that someone is mid-upload and
    // gets SIGKILLed (helm rolling update). M_033 has the full race.
    //
    // RETURNING sees the POST-update row state (SQL standard). So:
    //   fresh INSERT             → uploaded_at = NULL          → needs_upload = true
    //   CONFLICT, uploaded_at NULL (in-flight or interrupted)  → needs_upload = true
    //   CONFLICT, uploaded_at set (S3-confirmed)               → needs_upload = false
    //
    // Two concurrent PutPaths sharing a chunk now BOTH upload —
    // S3 PutObject is idempotent (same key, same bytes), so the
    // duplicate write is wasted bandwidth, not a correctness hazard.
    let rows: Vec<(Vec<u8>, bool)> = sqlx::query_as(
        r#"
        INSERT INTO chunks (blake3_hash, refcount, size)
        SELECT * FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[])
               AS t(hash, one, size)
        ON CONFLICT (blake3_hash) DO UPDATE
            SET refcount = chunks.refcount + 1, deleted = false
        RETURNING blake3_hash, (uploaded_at IS NULL) AS needs_upload
        "#,
    )
    .bind(&chunk_hashes)
    .bind(vec![1i64; chunk_hashes.len()])
    .bind(&chunk_sizes)
    .fetch_all(&mut *tx)
    .await?;

    let needs_upload: HashSet<Vec<u8>> = rows
        .into_iter()
        .filter_map(|(h, need)| need.then_some(h))
        .collect();

    tx.commit().await?;
    Ok(needs_upload)
}

/// Record that the given chunk hashes are now durably present in the
/// backend. Called by `cas::put_chunked` AFTER `do_upload` succeeds,
/// BEFORE the manifest is flipped to `complete`.
///
/// `WHERE uploaded_at IS NULL` makes this idempotent: two concurrent
/// PutPaths that both uploaded the same chunk both call here; the
/// second one is a no-op (0 rows updated). The timestamp is the FIRST
/// confirmed upload, not the last.
///
/// Hashes are sorted before binding — same lock-order discipline as
/// every other `chunks` writer (`r[store.chunk.lock-order]`).
// r[impl store.cas.chunk-upload-committed]
#[instrument(skip(pool, hashes), fields(count = hashes.len()))]
pub async fn mark_chunks_uploaded(pool: &PgPool, hashes: &[Vec<u8>]) -> Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let mut sorted = hashes.to_vec();
    sorted.sort_unstable();
    sqlx::query(
        "UPDATE chunks SET uploaded_at = now() \
         WHERE blake3_hash = ANY($1) AND uploaded_at IS NULL",
    )
    .bind(&sorted)
    .execute(pool)
    .await?;
    Ok(())
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
    // r[impl store.chunk.refcount-txn]
    // r[impl store.put.wal-manifest]
    // Sort-before-ANY() + single-retry-on-40P01 via the shared helper.
    // See with_sorted_retry doc for the deadlock-prevention rationale.
    with_sorted_retry(chunk_hashes.to_vec(), |hashes| async move {
        delete_manifest_chunked_uploading_inner(pool, store_path_hash, &hashes).await
    })
    .await
}

/// Transaction body for `delete_manifest_chunked_uploading`. Split out
/// so the outer function can retry the whole txn on 40P01.
async fn delete_manifest_chunked_uploading_inner(
    pool: &PgPool,
    store_path_hash: &[u8],
    hashes: &[Vec<u8>],
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
    sqlx::query(
        r#"
        UPDATE chunks SET refcount = refcount - 1
        WHERE blake3_hash = ANY($1)
        "#,
    )
    .bind(hashes)
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
    // S3-presence: PutChunk (the only writer of chunk_tenants) inserts
    // the junction row AFTER backend.put() succeeds (chunk.rs:219), so
    // a "present" hit here implies S3 has the bytes. No `uploaded_at`
    // join needed — that column guards the cas::put_chunked path, which
    // upserts `chunks` BEFORE upload but never writes chunk_tenants.
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
    /// uploads, marks-uploaded; second inserts {A,C}. The needs_upload
    /// set is driven by `uploaded_at`, not refcount.
    ///
    /// First call: A,B both new (uploaded_at NULL) → both need upload.
    /// After mark_chunks_uploaded({A,B}): A,B uploaded_at set.
    /// Second call: A uploaded_at set → NOT in set. C new → in set.
    #[tokio::test]
    async fn upsert_returning_sequential_needs_upload_set() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk_a = vec![0xA1u8; 32];
        let chunk_b = vec![0xB2u8; 32];
        let chunk_c = vec![0xC3u8; 32];

        // --- First upsert: {A, B} ---
        let sph1 = vec![0x11u8; 32];
        seed_placeholder(&db.pool, &sph1).await;
        let need1 = upgrade_manifest_to_chunked(
            &db.pool,
            &sph1,
            b"manifest-1",
            &[chunk_a.clone(), chunk_b.clone()],
            &[1024, 2048],
        )
        .await
        .unwrap();

        assert_eq!(need1.len(), 2, "first upsert: both A and B need upload");
        assert!(need1.contains(&chunk_a), "A: uploaded_at NULL");
        assert!(need1.contains(&chunk_b), "B: uploaded_at NULL");

        // Simulate the S3 upload + commit point.
        mark_chunks_uploaded(&db.pool, &[chunk_a.clone(), chunk_b.clone()])
            .await
            .unwrap();

        // --- Second upsert: {A, C} ---
        // A is at refcount=1, uploaded_at set → bumps to 2, but
        // uploaded_at IS NOT NULL → NOT in needs_upload. C is fresh.
        let sph2 = vec![0x22u8; 32];
        seed_placeholder(&db.pool, &sph2).await;
        let need2 = upgrade_manifest_to_chunked(
            &db.pool,
            &sph2,
            b"manifest-2",
            &[chunk_a.clone(), chunk_c.clone()],
            &[1024, 4096],
        )
        .await
        .unwrap();

        assert_eq!(need2.len(), 1, "second upsert: only C needs upload");
        assert!(
            !need2.contains(&chunk_a),
            "A uploaded_at set — NOT in needs_upload set"
        );
        assert!(need2.contains(&chunk_c), "C: uploaded_at NULL");

        // Ground truth: refcounts as expected.
        let rc_a: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk_a)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc_a, 2, "A referenced by two manifests");
    }

    /// The resurrection case (Audit B1 #8): chunk at refcount=0,
    /// deleted=true, uploaded_at NULL — the post-sweep, pre-drain
    /// state (`decrement_and_enqueue` clears uploaded_at when it sets
    /// deleted). An upsert resurrects it — refcount 0→1, deleted
    /// flips false, uploaded_at stays NULL → MUST be in needs_upload.
    /// S3 may have already deleted the object between sweep and now.
    #[tokio::test]
    async fn upsert_returning_resurrection_needs_upload() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0xDEu8; 32];

        // Seed the chunk at refcount=0, deleted=true, uploaded_at NULL
        // — the post-sweep, pre-drain state. Directly INSERT (bypassing
        // the upsert path) to set up the exact precondition.
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 1024, true)",
        )
        .bind(&chunk)
        .execute(&db.pool)
        .await
        .unwrap();

        // Precondition: confirm the seeded state.
        let (rc0, del0, up0): (i32, bool, bool) = sqlx::query_as(
            "SELECT refcount, deleted, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(rc0, 0, "precondition: refcount=0 (soft-deleted)");
        assert!(del0, "precondition: deleted=true (awaiting drain)");
        assert!(!up0, "precondition: uploaded_at NULL");

        // Upsert resurrects: ON CONFLICT → refcount 0+1=1, deleted=false.
        let sph = vec![0xDDu8; 32];
        seed_placeholder(&db.pool, &sph).await;
        let need = upgrade_manifest_to_chunked(
            &db.pool,
            &sph,
            b"manifest-resurrect",
            std::slice::from_ref(&chunk),
            &[1024],
        )
        .await
        .unwrap();

        // THE KEY ASSERTION: resurrected chunk IS in needs_upload.
        assert!(
            need.contains(&chunk),
            "resurrected chunk (uploaded_at NULL) MUST be in needs_upload \
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

    // r[verify store.cas.upsert-inserted+2]
    /// True-concurrent upserts via `tokio::join!`: two PutPaths share
    /// one chunk hash. Neither has called `mark_chunks_uploaded` yet,
    /// so BOTH see uploaded_at IS NULL → both get the shared chunk in
    /// their needs_upload set. S3 PutObject is idempotent — both
    /// uploading the same bytes to the same key is wasted bandwidth,
    /// not a correctness hazard.
    ///
    /// This is intentionally weaker than the old XOR property
    /// (`refcount = 1` gave exactly-one-uploader). The trade is one
    /// duplicate PUT under contention vs. surviving SIGKILL of the
    /// first uploader — see `sigkill_race_second_uploader_covers`.
    ///
    /// The assertion is `a_has && b_has` — both sides upload regardless
    /// of which won the ON CONFLICT serialization.
    #[tokio::test]
    async fn upsert_returning_concurrent_both_need_upload() {
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
        // the UPDATE clause (refcount 1→2). Both see uploaded_at NULL.
        let (need_a, need_b) = tokio::join!(
            upgrade_manifest_to_chunked(&db.pool, &sph_a, b"manifest-a", &hashes_a, &sizes_a),
            upgrade_manifest_to_chunked(&db.pool, &sph_b, b"manifest-b", &hashes_b, &sizes_b),
        );
        let need_a = need_a.unwrap();
        let need_b = need_b.unwrap();

        // Each side's unique chunk is always fresh.
        assert!(need_a.contains(&unique_a), "A's unique chunk needs upload");
        assert!(need_b.contains(&unique_b), "B's unique chunk needs upload");

        // THE KEY ASSERTION: both sides see the shared chunk as
        // needs_upload. Neither has called mark_chunks_uploaded yet,
        // so uploaded_at is NULL for both reads. Idempotent S3 PUT
        // makes the duplicate upload harmless; the alternative
        // (exactly-one via refcount=1) loses data when the winner is
        // SIGKILLed mid-upload.
        let a_has = need_a.contains(&shared);
        let b_has = need_b.contains(&shared);
        assert!(
            a_has && b_has,
            "both concurrent upserts see shared chunk as needs_upload \
             (got A={a_has}, B={b_has}; either-false = M033 regression)"
        );

        // Ground truth: final refcount = 2.
        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&shared)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 2, "shared chunk referenced by both manifests");
    }

    // r[verify store.cas.chunk-upload-committed]
    /// The SIGKILL race that motivated `uploaded_at` (M_033):
    ///
    /// 1. PutPath A: upsert chunk X (rc=1, uploaded_at NULL). Starts
    ///    upload. Process is SIGKILLed (helm rolling update) — no
    ///    rollback runs, no mark_chunks_uploaded runs. Manifest A
    ///    left at status='uploading'.
    /// 2. PutPath B (different path, same chunk X): upsert (rc=2).
    ///    Under the OLD `(refcount=1)` heuristic, B would skip upload
    ///    here — permanent data loss (X never reaches S3, rc never
    ///    drops to 0 once B completes). Under `uploaded_at IS NULL`,
    ///    B uploads.
    /// 3. B's upload succeeds → mark_chunks_uploaded → uploaded_at
    ///    set. Manifest B completes.
    /// 4. PutPath C (third path, same chunk X): upsert (rc=3,
    ///    uploaded_at set) → skips upload. Correct dedup.
    ///
    /// We don't run a real `cas::put_chunked` for A — just the upsert
    /// step, then nothing (the SIGKILL happens before any further PG
    /// or S3 write, so dropping the future after the upsert tx commits
    /// is the exact post-kill state).
    #[tokio::test]
    async fn sigkill_race_second_uploader_covers() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk_x = vec![0x51u8; 32];
        let one_chunk = std::slice::from_ref(&chunk_x);
        // Real serialized chunk_list — `reap_one` deserializes it to
        // know which chunks to decrement. All three manifests share
        // the same single-chunk list.
        let chunk_list = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: chunk_x.as_slice().try_into().unwrap(),
                size: 1024,
            }],
        }
        .serialize();

        // --- Step 1: PutPath A's upsert, then SIGKILL ---
        let sph_a = vec![0xAAu8; 32];
        seed_placeholder(&db.pool, &sph_a).await;
        let need_a = upgrade_manifest_to_chunked(&db.pool, &sph_a, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert!(need_a.contains(&chunk_x), "A: fresh insert needs upload");
        // SIGKILL: drop here. PG state committed (rc=1, uploaded_at
        // NULL, manifest A status='uploading'), S3 has nothing.

        // --- Step 2+3: PutPath B sees needs_upload, uploads, marks ---
        let sph_b = vec![0xBBu8; 32];
        seed_placeholder(&db.pool, &sph_b).await;
        let need_b = upgrade_manifest_to_chunked(&db.pool, &sph_b, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert!(
            need_b.contains(&chunk_x),
            "B: rc=2 but uploaded_at NULL → needs upload \
             (refcount-based heuristic would skip here → data loss)"
        );
        // B uploads to S3 (omitted — backend.put is idempotent), then:
        mark_chunks_uploaded(&db.pool, one_chunk).await.unwrap();

        let (rc, up): (i32, bool) = sqlx::query_as(
            "SELECT refcount, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk_x)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(rc, 2, "A leaked + B = 2");
        assert!(up, "B's mark_chunks_uploaded set uploaded_at");

        // --- Step 4: PutPath C dedups against B's confirmed upload ---
        let sph_c = vec![0xCCu8; 32];
        seed_placeholder(&db.pool, &sph_c).await;
        let need_c = upgrade_manifest_to_chunked(&db.pool, &sph_c, &chunk_list, one_chunk, &[1024])
            .await
            .unwrap();
        assert!(
            !need_c.contains(&chunk_x),
            "C: uploaded_at set → skip upload (dedup works post-commit)"
        );

        // --- Epilogue: orphan reaper cleans manifest A ---
        // reap_one decrements rc 3→2. uploaded_at stays set (B's
        // upload is real). A future PutPath still dedups correctly.
        let no_backend: Option<&std::sync::Arc<dyn crate::backend::chunk::ChunkBackend>> = None;
        let reaped = crate::gc::orphan::reap_one(&db.pool, &sph_a, None, no_backend)
            .await
            .unwrap();
        assert!(reaped, "A's stale 'uploading' placeholder reaped");
        let (rc, up): (i32, bool) = sqlx::query_as(
            "SELECT refcount, (uploaded_at IS NOT NULL) FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&chunk_x)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(rc, 2, "reaper decremented A's leaked ref");
        assert!(
            up,
            "reaper does NOT clear uploaded_at when rc>0 — B+C still reference it"
        );
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

    /// I-040 chain, post-M033: the inline `delete_manifest_uploading`
    /// on a chunked placeholder still leaks refcounts (it doesn't
    /// decrement — correct for inline placeholders, wrong for chunked),
    /// but the leak NO LONGER causes upload-skip on retry. The retry's
    /// upsert sees `uploaded_at IS NULL` and re-uploads regardless of
    /// the leaked refcount.
    ///
    /// substitute.rs's call site still uses `gc::orphan::reap_one`
    /// (which DOES decrement) for refcount hygiene; this test asserts
    /// that even if a future caller gets that wrong, the data-loss
    /// chain stays broken at the upsert level.
    #[tokio::test]
    async fn i040_inline_delete_leaked_refcount_still_reuploads() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let chunk = vec![0x40u8; 32];
        let path = rio_test_support::fixtures::test_store_path("i040-chain");
        // path-derived hash (matches what real callers compute) so the
        // narinfo placeholder's store_path column round-trips.
        let sph = rio_nix::store_path::StorePath::parse(&path)
            .unwrap()
            .sha256_digest()
            .to_vec();

        // --- Step 1: prior upload's upgrade_manifest_to_chunked ---
        // Simulates: cas::put_chunked got past upgrade, then crashed.
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let chunk_list = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: chunk.as_slice().try_into().unwrap(),
                size: 100,
            }],
        }
        .serialize();
        let one_chunk = std::slice::from_ref(&chunk);
        let ins1 = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[100i64])
            .await
            .unwrap();
        assert!(ins1.contains(&chunk), "step 1: chunk fresh → would upload");
        // Crash here: chunk MAY or may not have made it to S3. PG state
        // is committed (refcount=1).

        // --- Step 2: inline delete (the I-040 bug path) ---
        // This deletes manifests (CASCADE → manifest_data) but does NOT
        // touch chunk refcounts. Correct for inline placeholders, WRONG
        // for chunked — substitute.rs called this unconditionally.
        crate::metadata::delete_manifest_uploading(&db.pool, &sph)
            .await
            .unwrap();

        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            rc, 1,
            "step 2: refcount LEAKED at 1 (inline delete ≠ decrement)"
        );

        // --- Step 3: retry's upgrade_manifest_to_chunked ---
        crate::metadata::insert_manifest_uploading(&db.pool, &sph, &path, &[])
            .await
            .unwrap();
        let need2 = upgrade_manifest_to_chunked(&db.pool, &sph, &chunk_list, one_chunk, &[100i64])
            .await
            .unwrap();

        // POST-M033: refcount went 1→2 (leak), but uploaded_at is
        // still NULL (step 1 never reached mark_chunks_uploaded) →
        // chunk IS in needs_upload → do_upload re-uploads. Data-loss
        // chain broken at the upsert.
        assert!(
            need2.contains(&chunk),
            "step 3: leaked refcount but uploaded_at NULL → re-upload \
             (data-loss chain broken regardless of call-site hygiene)"
        );

        let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(&chunk)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 2, "1 leaked + 1 real = 2");
    }
}
