//! Inline write-ahead: small NARs stored directly in `manifests.inline_blob`.
//!
//! Step 1 (`insert_manifest_uploading`) writes a placeholder with
//! `nar_size = 0` + `status = 'uploading'`. Step 3 (`complete_manifest_inline`)
//! fills real narinfo and stores the NAR blob atomically. On failure,
// r[impl store.inline.threshold]
//! `delete_manifest_uploading` reclaims the placeholder (guarded by
//! `nar_size = 0` so a concurrent successful upload is never touched).

use super::*;
use sqlx::PgPool;
use tracing::{debug, instrument};

/// Begin a new upload: insert placeholder narinfo + manifest rows.
///
/// The placeholder narinfo has `nar_hash = [0;32]` and `nar_size = 0`.
/// `nar_size = 0` is the placeholder marker: the minimum valid NAR is ~100
/// bytes, so 0 unambiguously means "not a real upload yet". This lets
/// `delete_manifest_uploading` identify placeholders without touching a
/// concurrent successful upload of the same path.
///
/// `references` is populated on the placeholder so the closure is protected
/// from GC at the instant this tx commits — no advisory lock needed
/// (I-192). Mark's CTE may or may not see this row depending on snapshot
/// timing; either way the references reach sweep:
///
/// - Placeholder commits BEFORE mark's CTE snapshot → seed (b) walks it.
/// - Placeholder commits AFTER mark's CTE snapshot → sweep's per-path
///   re-check (`narinfo."references" @> ARRAY[Q]`, fresh READ-COMMITTED
///   snapshot, scans `'uploading'` rows too) finds it and resurrects Q.
///
/// See `r[store.gc.sweep-recheck]` for the full race trace.
///
/// Returns `true` if inserted, `false` if another upload already holds a
/// placeholder (caller should re-check `check_manifest_complete` — the race
/// winner may have finished).
#[instrument(skip(pool, references), fields(store_path_hash = hex::encode(store_path_hash), refs = references.len()))]
pub async fn insert_manifest_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    references: &[String],
) -> Result<bool> {
    let mut tx = pool.begin().await?;

    // narinfo placeholder first (manifests has FK to narinfo). ON CONFLICT
    // DO NOTHING: if another uploader already inserted, we don't clobber.
    // REFERENCES POPULATED HERE — this is what makes the placeholder itself
    // protect its closure (via mark seed (b) or sweep re-check) without an
    // advisory lock.
    // r[impl store.put.placeholder-refs]
    sqlx::query(
        r#"
        INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size,
                             "references")
        VALUES ($1, $2, $3, 0, $4)
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .bind(store_path)
    .bind(&[0u8; 32] as &[u8])
    .bind(references)
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
#[instrument(skip(pool, info, nar_data), fields(store_path = %info.store_path.as_str(), nar_size = nar_data.len()))]
pub async fn complete_manifest_inline(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    nar_data: Bytes,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    complete_manifest_inline_in_tx(&mut tx, info, nar_data).await?;
    tx.commit().await?;
    debug!(store_path = %info.store_path.as_str(), "inline upload completed");
    Ok(())
}

/// Transaction-body variant of [`complete_manifest_inline`]: runs the
/// narinfo UPDATE + manifests UPDATE inside a caller-owned transaction.
///
/// `PutPathBatch` calls this N times inside ONE `pool.begin()` to achieve
/// cross-output atomicity (all outputs flip to 'complete' or none do).
/// [`complete_manifest_inline`] is a thin wrapper that begins/commits
/// around one call.
///
/// Takes `&mut PgConnection` (not `&mut Transaction`) to match
/// [`update_narinfo_complete`]'s signature — `&mut *tx` deref-coerces
/// a `Transaction` to `PgConnection`.
pub async fn complete_manifest_inline_in_tx(
    conn: &mut sqlx::PgConnection,
    info: &ValidatedPathInfo,
    nar_data: Bytes,
) -> Result<()> {
    if update_narinfo_complete(conn, info).await? == 0 {
        // insert_manifest_uploading MUST have run first. If rows_affected
        // is 0, delete_manifest_uploading raced us and won. The caller's
        // placeholder is gone; bailing here prevents a half-complete write.
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
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
    .execute(&mut *conn)
    .await?;

    if manifest_result.rows_affected() == 0 {
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }

    Ok(())
}

/// Age of an existing `'uploading'` placeholder, or `None` if no such
/// placeholder exists (already completed, already cleaned up, or never
/// inserted).
///
/// Test-only since I-040: `Substituter::ingest`'s reclaim now uses
/// [`crate::gc::orphan::reap_one`], which does the stale check
/// in-SQL. This survives as a test helper for asserting "placeholder
/// still present" after a non-reclaiming flow.
#[cfg(test)]
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn manifest_uploading_age(
    pool: &PgPool,
    store_path_hash: &[u8],
) -> Result<Option<std::time::Duration>> {
    // PG INTERVAL → sqlx PgInterval. Microsecond precision; we floor to
    // whole seconds (stale-threshold comparison is in the minutes range,
    // sub-second precision irrelevant). `updated_at` not `created_at`:
    // manifests only has updated_at (002_store.sql:62); same column the
    // orphan scanner checks.
    let interval: Option<sqlx::postgres::types::PgInterval> = sqlx::query_scalar(
        r#"
        SELECT now() - updated_at
          FROM manifests
         WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .fetch_optional(pool)
    .await?;

    Ok(interval.map(|iv| {
        // PgInterval = months*30d + days*24h + microseconds. For
        // now()-updated_at on a recent placeholder, months=days=0 in
        // practice. Include them anyway for correctness if a placeholder
        // somehow survives that long. saturating_add: negative age
        // (clock skew, manual row tweak) clamps to zero → treated as
        // young → not reclaimed, which is the safe direction.
        let secs = (iv.months as i64 * 30 * 86400)
            .saturating_add(iv.days as i64 * 86400)
            .saturating_add(iv.microseconds / 1_000_000)
            .max(0) as u64;
        std::time::Duration::from_secs(secs)
    }))
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
pub async fn delete_manifest_uploading(pool: &PgPool, store_path_hash: &[u8]) -> Result<()> {
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
pub async fn check_manifest_complete(pool: &PgPool, store_path_hash: &[u8]) -> Result<bool> {
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
