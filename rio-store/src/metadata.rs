//! Store path metadata persistence in PostgreSQL.
//!
//! CRUD operations for the `narinfo` and `manifests` tables defined in
//! `migrations/006_phase2c.sql`.
//!
//! # Storage model (phase 2c)
//!
//! NAR content lives in one of two places, determined by `manifests.inline_blob`:
//!
//! - **Inline** (`inline_blob IS NOT NULL`): whole NAR stored directly in the
//!   manifests row. No `manifest_data` row. Used for small NARs (<256KB by
//!   default; E1 interim: ALL NARs, chunking lands in C3).
//! - **Chunked** (`inline_blob IS NULL`): NAR split by FastCDC; chunks in S3;
//!   `manifest_data.chunk_list` holds the ordered (blake3, size) list.
//!
//! This invariant (`inline_blob IS NOT NULL <=> no manifest_data row`) is
//! enforced by code, not by a CHECK constraint — PG can't express "row in
//! another table must not exist".
//!
//! # Write-ahead pattern
//!
//! 1. `insert_manifest_uploading()` — writes placeholder narinfo + manifest
//!    with `status='uploading'`. Protects the upload from concurrent GC.
//! 2. Caller writes inline_blob or uploads chunks.
//! 3. `complete_manifest_inline()` / `complete_manifest_chunked()` — fills
//!    real narinfo metadata + flips `status='complete'` atomically.
//!
//! On failure between 1 and 3, `delete_manifest_uploading()` reclaims the
//! placeholder. It only touches rows where `nar_size = 0` (the placeholder
//! marker — real NARs are always >0), so it's safe even if a concurrent
//! upload already succeeded.
//!
//! `query_path_info()` and `find_missing_paths()` filter on
//! `manifests.status = 'complete'`, so placeholders are never exposed.

use bytes::Bytes;
use rio_proto::validated::{PathInfoValidationError, ValidatedPathInfo};
use sqlx::PgPool;
use tracing::{debug, instrument};

/// How a NAR's content is stored. Returned by [`get_manifest`].
///
/// This is the one place callers branch on inline-vs-chunked. GetPath reads
/// this; the binary cache HTTP server reads this; future GC reads this.
/// Encapsulating the branch here means the "check inline_blob FIRST, only
/// then query manifest_data" rule lives in exactly one SQL query.
#[derive(Debug)]
pub enum ManifestKind {
    /// Whole NAR stored in `manifests.inline_blob`.
    Inline(Bytes),
    /// NAR chunked; reassemble from this ordered list.
    /// Each entry is `(blake3_digest, chunk_size_bytes)`.
    ///
    /// E1 returns an empty Vec here (chunking lands in C3) — this is
    /// future-proofing the return type so GetPath doesn't need a second
    /// rewrite when chunking lands.
    Chunked(Vec<([u8; 32], u32)>),
}

/// Begin a new upload: insert placeholder narinfo + manifest rows.
///
/// The placeholder narinfo has `nar_hash = [0;32]` and `nar_size = 0`.
/// `nar_size = 0` is the placeholder marker: the minimum valid NAR is ~100
/// bytes, so 0 unambiguously means "not a real upload yet". This lets
/// `delete_manifest_uploading` identify placeholders without touching a
/// concurrent successful upload of the same path.
///
/// Returns `true` if inserted, `false` if another upload already holds a
/// placeholder (caller should re-check `check_manifest_complete` — the race
/// winner may have finished).
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn insert_manifest_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;

    // narinfo placeholder first (manifests has FK to narinfo). ON CONFLICT
    // DO NOTHING: if another uploader already inserted, we don't clobber
    // their (possibly real, possibly placeholder) row.
    sqlx::query(
        r#"
        INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size)
        VALUES ($1, $2, $3, 0)
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .bind(store_path)
    .bind(&[0u8; 32] as &[u8])
    .execute(&mut *tx)
    .await?;

    // manifests placeholder. ON CONFLICT DO NOTHING for the same reason.
    // rows_affected = 0 means another uploader owns this slot.
    let result = sqlx::query(
        r#"
        INSERT INTO manifests (store_path_hash, status)
        VALUES ($1, 'uploading')
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

/// Finalize an inline upload: fill real narinfo + store the NAR in
/// `manifests.inline_blob` + flip status to 'complete'.
///
/// Single transaction: either the path becomes fully visible to
/// `query_path_info` or it stays a placeholder. No partial-complete state.
///
/// `registration_time` and `ultimate` are now persisted (resolves the
/// phase2c-tagged deferral at the old metadata.rs:329 — previously dropped
/// on write and returned as 0/false on read, which was observable via
/// `nix path-info --json` but didn't break clients).
#[instrument(skip(pool, info, nar_data), fields(store_path = %info.store_path.as_str(), nar_size = nar_data.len()))]
pub async fn complete_manifest_inline(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    nar_data: Bytes,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    let deriver_str = info.deriver.as_ref().map(|d| d.to_string());
    let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
    let ca_str = info.content_address.as_deref();

    let narinfo_result = sqlx::query(
        r#"
        UPDATE narinfo SET
            deriver           = $2,
            nar_hash          = $3,
            nar_size          = $4,
            "references"      = $5,
            signatures        = $6,
            ca                = $7,
            registration_time = $8,
            ultimate          = $9
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(deriver_str)
    .bind(info.nar_hash.as_slice())
    .bind(info.nar_size as i64)
    .bind(&refs_str)
    .bind(&info.signatures)
    .bind(ca_str)
    .bind(info.registration_time as i64)
    .bind(info.ultimate)
    .execute(&mut *tx)
    .await?;

    if narinfo_result.rows_affected() == 0 {
        // insert_manifest_uploading MUST have run first. If rows_affected
        // is 0, delete_manifest_uploading raced us and won. The caller's
        // placeholder is gone; bailing here prevents a half-complete write.
        anyhow::bail!(
            "complete_manifest_inline: narinfo placeholder missing for {} (concurrently deleted?)",
            info.store_path.as_str()
        );
    }

    // Store NAR + flip status. nar_data is Bytes (Arc-refcounted); sqlx binds
    // &[u8], so .as_ref() — no copy.
    let manifest_result = sqlx::query(
        r#"
        UPDATE manifests SET
            status      = 'complete',
            inline_blob = $2,
            updated_at  = now()
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(nar_data.as_ref())
    .execute(&mut *tx)
    .await?;

    if manifest_result.rows_affected() == 0 {
        anyhow::bail!(
            "complete_manifest_inline: manifest placeholder missing for {} (concurrently deleted?)",
            info.store_path.as_str()
        );
    }

    tx.commit().await?;

    debug!(store_path = %info.store_path.as_str(), "inline upload completed");
    Ok(())
}

/// Reclaim placeholder rows from a failed upload.
///
/// Only deletes rows where `narinfo.nar_size = 0` AND
/// `manifests.status = 'uploading'`. Both conditions together: if a
/// concurrent upload succeeded, its nar_size is >0 and status is 'complete',
/// so we don't touch it. Safe to call even if no placeholder exists (no-op).
///
/// manifests deleted first (FK dependency: manifests → narinfo). ON DELETE
/// CASCADE on the FK would also work but explicit ordering makes intent
/// clear and doesn't depend on schema details.
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn delete_manifest_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        DELETE FROM manifests
        WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    // nar_size = 0 is the placeholder marker. A successful upload ALWAYS
    // has nar_size > 0 (min valid NAR is ~100 bytes).
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

/// Check if a store path already has a completed upload.
///
/// Idempotency pre-check for PutPath: if `true`, the path exists and the
/// caller should return `created: false` without touching anything.
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn check_manifest_complete(
    pool: &PgPool,
    store_path_hash: &[u8],
) -> anyhow::Result<bool> {
    // EXISTS returns a PG bool, which sqlx decodes cleanly. `SELECT 1` would
    // return int4 and need i32 (not i64) — a type-width footgun that turns
    // into an opaque "ColumnDecode" runtime error. EXISTS sidesteps it
    // entirely and is also the idiomatic existence-check query.
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM manifests
            WHERE store_path_hash = $1 AND status = 'complete'
        )
        "#,
    )
    .bind(store_path_hash)
    .fetch_one(pool)
    .await?;

    Ok(exists)
}

/// Fetch the storage kind + content for a completed path.
///
/// This is THE one place the inline/chunked branch is implemented. Callers
/// (GetPath, cache_server) match on the result; they never query manifests
/// or manifest_data directly.
///
/// `None` means the path has no complete manifest (either never uploaded,
/// or stuck in 'uploading' from a crashed PutPath).
///
/// E1 interim: chunked arm is a stub. When `inline_blob IS NULL` we return
/// `Chunked(vec![])` — unreachable in practice since E1's PutPath always
/// writes inline, but the type is in place for C3/C5.
#[instrument(skip(pool))]
pub async fn get_manifest(pool: &PgPool, store_path: &str) -> anyhow::Result<Option<ManifestKind>> {
    // Single query: join narinfo→manifests, filter status='complete',
    // pull inline_blob. manifest_data is a second query only if needed
    // (avoids pulling a potentially-large chunk_list we won't use).
    let row: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
        r#"
        SELECT m.inline_blob
        FROM manifests m
        INNER JOIN narinfo n ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path = $1 AND m.status = 'complete'
        "#,
    )
    .bind(store_path)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some((Some(blob),)) => Ok(Some(ManifestKind::Inline(Bytes::from(blob)))),
        // inline_blob NULL → chunked. Fetch + deserialize manifest_data.
        Some((None,)) => {
            let data: Option<(Vec<u8>,)> = sqlx::query_as(
                r#"
                SELECT md.chunk_list
                FROM manifest_data md
                INNER JOIN narinfo n ON n.store_path_hash = md.store_path_hash
                WHERE n.store_path = $1
                "#,
            )
            .bind(store_path)
            .fetch_optional(pool)
            .await?;

            // manifest exists + inline_blob NULL but NO manifest_data row:
            // invariant violation (store.md:222 says inline_blob NULL ⇔
            // manifest_data exists). Possible causes: manual DB surgery,
            // a bug in delete_manifest_chunked_uploading ordering, or a
            // CASCADE we didn't expect. Surface it — don't silently return
            // None (that would look like "path not found", masking corruption).
            let (chunk_list,) = data.ok_or_else(|| {
                anyhow::anyhow!(
                    "invariant violation: manifest for {store_path} has NULL inline_blob \
                     but no manifest_data row (corrupted state)"
                )
            })?;

            let manifest = crate::manifest::Manifest::deserialize(&chunk_list)
                .map_err(|e| anyhow::anyhow!("corrupt manifest_data for {store_path}: {e}"))?;

            // Convert Manifest → Vec<([u8;32], u32)> for the enum variant.
            // ManifestKind predates Manifest (E1 vs C1) so the representation
            // is slightly different; this is one allocation, cheap.
            let entries: Vec<([u8; 32], u32)> = manifest
                .entries
                .into_iter()
                .map(|e| (e.hash, e.size))
                .collect();

            Ok(Some(ManifestKind::Chunked(entries)))
        }
    }
}

/// Query path info for a store path.
///
/// Only returns paths with `manifests.status = 'complete'`. Placeholders
/// (status = 'uploading') and orphans (narinfo row but no manifests row)
/// are invisible.
#[instrument(skip(pool))]
pub async fn query_path_info(
    pool: &PgPool,
    store_path: &str,
) -> anyhow::Result<Option<ValidatedPathInfo>> {
    let row: Option<NarinfoRow> = sqlx::query_as(
        r#"
        SELECT n.store_path, n.store_path_hash, n.deriver, n.nar_hash, n.nar_size,
               n."references", n.signatures, n.ca, n.registration_time, n.ultimate
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path = $1 AND m.status = 'complete'
        "#,
    )
    .bind(store_path)
    .fetch_optional(pool)
    .await?;

    // DB-egress validation: a malformed row (garbage store_path, wrong-length
    // nar_hash) would otherwise propagate silently. Caught here at the trust
    // boundary — PG doesn't enforce these as CHECK constraints.
    row.map(|r| r.try_into_validated())
        .transpose()
        .map_err(|e| anyhow::anyhow!("malformed narinfo row for {store_path}: {e}"))
}

/// Batch check which store paths are missing.
///
/// "Missing" means no manifests row with `status = 'complete'`. Paths stuck
/// in 'uploading' (crashed PutPath) are missing — the client should retry.
#[instrument(skip(pool, store_paths), fields(count = store_paths.len()))]
pub async fn find_missing_paths(
    pool: &PgPool,
    store_paths: &[String],
) -> anyhow::Result<Vec<String>> {
    if store_paths.is_empty() {
        return Ok(Vec::new());
    }

    let complete: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT n.store_path
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path = ANY($1) AND m.status = 'complete'
        "#,
    )
    .bind(store_paths)
    .fetch_all(pool)
    .await?;

    let complete_set: std::collections::HashSet<&str> =
        complete.iter().map(|(p,)| p.as_str()).collect();

    Ok(store_paths
        .iter()
        .filter(|p| !complete_set.contains(p.as_str()))
        .cloned()
        .collect())
}

// ---------------------------------------------------------------------------
// Chunked manifest ops (phase2c C3)
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
) -> anyhow::Result<()> {
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
        anyhow::bail!(
            "upgrade_manifest_to_chunked: no 'uploading' manifest for {} \
             (placeholder deleted concurrently?)",
            hex::encode(store_path_hash)
        );
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
    sqlx::query(
        r#"
        INSERT INTO chunks (blake3_hash, refcount, size)
        SELECT * FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[])
               AS t(hash, one, size)
        ON CONFLICT (blake3_hash) DO UPDATE SET refcount = chunks.refcount + 1
        "#,
    )
    .bind(chunk_hashes)
    .bind(vec![1i64; chunk_hashes.len()])
    .bind(chunk_sizes)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
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
pub async fn complete_manifest_chunked(
    pool: &PgPool,
    info: &ValidatedPathInfo,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    let deriver_str = info.deriver.as_ref().map(|d| d.to_string());
    let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
    let ca_str = info.content_address.as_deref();

    let narinfo_result = sqlx::query(
        r#"
        UPDATE narinfo SET
            deriver           = $2,
            nar_hash          = $3,
            nar_size          = $4,
            "references"      = $5,
            signatures        = $6,
            ca                = $7,
            registration_time = $8,
            ultimate          = $9
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(deriver_str)
    .bind(info.nar_hash.as_slice())
    .bind(info.nar_size as i64)
    .bind(&refs_str)
    .bind(&info.signatures)
    .bind(ca_str)
    .bind(info.registration_time as i64)
    .bind(info.ultimate)
    .execute(&mut *tx)
    .await?;

    if narinfo_result.rows_affected() == 0 {
        anyhow::bail!(
            "complete_manifest_chunked: narinfo placeholder missing for {}",
            info.store_path.as_str()
        );
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
        anyhow::bail!(
            "complete_manifest_chunked: manifest placeholder missing for {}",
            info.store_path.as_str()
        );
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
) -> anyhow::Result<()> {
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
pub async fn find_missing_chunks(pool: &PgPool, hashes: &[Vec<u8>]) -> anyhow::Result<Vec<bool>> {
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

/// Resolve a store path from its 32-char nixbase32 hash part.
///
/// Nix store paths look like `/nix/store/{hash-part}-{name}`. The daemon's
/// wopQueryPathFromHashPart hands us just `{hash-part}`; we need to find the
/// full path. The hash-part uniquely identifies a path (it's a truncated hash
/// of the full fingerprint), so a LIKE prefix match is correct, not just
/// convenient.
///
/// `hash_part` is expected to be exactly 32 nixbase32 chars. The caller
/// (gRPC layer) validates length and charset — we don't re-check here.
/// An unvalidated `hash_part` with `%` or `_` in it would be a LIKE-
/// injection (matches anything / anything-one-char); the gRPC validation
/// prevents that by rejecting non-nixbase32 chars upfront.
#[instrument(skip(pool))]
pub async fn query_by_hash_part(
    pool: &PgPool,
    hash_part: &str,
) -> anyhow::Result<Option<ValidatedPathInfo>> {
    // LIKE pattern: the store prefix + hash + dash + anything.
    // The dash is important — without it, a hash-part "aaa...a" would also
    // match a hypothetical "aaa...ab-name" (32-char hash that happens to
    // prefix-extend). Nix store paths always have the dash separator, so
    // including it makes the match exact.
    let pattern = format!("/nix/store/{hash_part}-%");

    let row: Option<NarinfoRow> = sqlx::query_as(
        r#"
        SELECT n.store_path, n.store_path_hash, n.deriver, n.nar_hash, n.nar_size,
               n."references", n.signatures, n.ca, n.registration_time, n.ultimate
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path LIKE $1 AND m.status = 'complete'
        "#,
    )
    .bind(&pattern)
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into_validated())
        .transpose()
        .map_err(|e| anyhow::anyhow!("malformed narinfo row for hash-part {hash_part}: {e}"))
}

/// Append signatures to an existing narinfo.
///
/// Does NOT deduplicate — if the client sends the same sig twice, we store
/// it twice. Nix's own AddSignatures has the same behavior; dedup is the
/// client's responsibility. The `signatures` column is TEXT[], and PG's
/// `||` (array concat) preserves order.
///
/// Returns the number of rows updated (0 = path not found, 1 = appended).
/// Caller maps 0 to NOT_FOUND.
#[instrument(skip(pool, sigs), fields(count = sigs.len()))]
pub async fn append_signatures(
    pool: &PgPool,
    store_path: &str,
    sigs: &[String],
) -> anyhow::Result<u64> {
    // WHERE ... = $1 (not LIKE): this takes a full path. Only
    // query_by_hash_part does prefix matching.
    //
    // No manifests-join here: signatures are metadata on narinfo, independent
    // of whether the NAR content is complete. In practice clients sign AFTER
    // uploading (they need the nar_hash), so the manifest is always complete
    // when this is called — but coupling this function to that assumption
    // would break `nix store sign` against a path whose upload got stuck.
    let result = sqlx::query(
        r#"
        UPDATE narinfo SET signatures = signatures || $2
        WHERE store_path = $1
        "#,
    )
    .bind(store_path)
    .bind(sigs)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct NarinfoRow {
    store_path: String,
    store_path_hash: Vec<u8>,
    deriver: Option<String>,
    nar_hash: Vec<u8>,
    nar_size: i64,
    references: Vec<String>,
    signatures: Vec<String>,
    ca: Option<String>,
    registration_time: i64,
    ultimate: bool,
}

impl NarinfoRow {
    fn try_into_validated(self) -> Result<ValidatedPathInfo, PathInfoValidationError> {
        use rio_proto::types::PathInfo;
        // Build raw PathInfo then delegate to the centralized TryFrom —
        // keeps validation logic in one place (rio-proto::validated), not
        // duplicated here.
        ValidatedPathInfo::try_from(PathInfo {
            store_path: self.store_path,
            store_path_hash: self.store_path_hash,
            deriver: self.deriver.unwrap_or_default(),
            nar_hash: self.nar_hash,
            nar_size: self.nar_size as u64,
            references: self.references,
            // Now actually roundtrip (was 0/false before phase2c).
            // `as u64` cast: registration_time is Unix epoch seconds,
            // non-negative in practice. A negative value in the DB would
            // be corruption; the cast wraps, which is detectable downstream.
            registration_time: self.registration_time as u64,
            ultimate: self.ultimate,
            signatures: self.signatures,
            content_address: self.ca.unwrap_or_default(),
        })
    }
}
