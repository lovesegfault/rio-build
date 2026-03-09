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
/// Returns `true` if inserted, `false` if another upload already holds a
/// placeholder (caller should re-check `check_manifest_complete` — the race
/// winner may have finished).
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn insert_manifest_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
) -> Result<bool> {
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
#[instrument(skip(pool, info, nar_data), fields(store_path = %info.store_path.as_str(), nar_size = nar_data.len()))]
pub async fn complete_manifest_inline(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    nar_data: Bytes,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    if update_narinfo_complete(&mut tx, info).await? == 0 {
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
    .execute(&mut *tx)
    .await?;

    if manifest_result.rows_affected() == 0 {
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
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
