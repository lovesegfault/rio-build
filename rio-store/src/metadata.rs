//! Store path metadata persistence in PostgreSQL.
//!
//! CRUD operations for the `narinfo` and `nar_blobs` tables defined in
//! `migrations/002_store_tables.sql`.
//!
//! Phase 2a uses full NARs (no chunking), so `nar_blobs` tracks the storage
//! key and upload status for each NAR blob.

use rio_proto::types::PathInfo;
use sqlx::PgPool;
use tracing::{debug, instrument};

/// Insert a `nar_blobs` row with `status='uploading'` for a new upload.
///
/// Write-ahead pattern step 1: mark the blob as "uploading" before writing
/// data to the backend.
///
/// Implementation detail: `nar_blobs` has a FK to `narinfo`, so we insert a
/// placeholder `narinfo` row (with zero nar_hash and nar_size=0) in the same
/// transaction. The placeholder is overwritten with real metadata at
/// `complete_upload()` time. `QueryPathInfo` and `find_missing_paths` filter
/// on `nar_blobs.status='complete'`, so the placeholder is never exposed to
/// clients.
///
/// The `nar_size=0` placeholder is a safe marker: the minimum valid NAR is
/// ~100 bytes, so nar_size=0 can never represent a real uploaded path. This
/// enables `delete_uploading` to safely identify and clean up orphaned
/// placeholders.
///
/// Returns `true` if inserted, `false` if a row already exists (idempotent).
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn insert_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    blob_key: &str,
) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;

    // Insert narinfo placeholder (ON CONFLICT DO NOTHING for idempotency)
    let narinfo_result = sqlx::query(
        r#"
        INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size)
        VALUES ($1, $2, $3, 0)
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .bind(store_path)
    .bind(&[0u8; 32] as &[u8]) // placeholder hash, overwritten at complete time
    .execute(&mut *tx)
    .await?;

    debug!(
        narinfo_inserted = narinfo_result.rows_affected() > 0,
        "narinfo placeholder insert"
    );

    // Insert nar_blobs with status='uploading'
    let result = sqlx::query(
        r#"
        INSERT INTO nar_blobs (store_path_hash, status, blob_key)
        VALUES ($1, 'uploading', $2)
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .bind(blob_key)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

/// Complete an upload: update `nar_blobs.status` to `'complete'` and fill in
/// the full `narinfo` metadata.
///
/// This is the final step of the write-ahead pattern. Only after the NAR has
/// been fully written and SHA-256 verified do we flip the status and populate
/// the narinfo row with real metadata. This ensures `QueryPathInfo` never
/// returns metadata for incomplete uploads.
#[instrument(skip(pool, info), fields(store_path = %info.store_path))]
pub async fn complete_upload(pool: &PgPool, info: &PathInfo, blob_key: &str) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    // Update narinfo with full metadata
    let narinfo_result = sqlx::query(
        r#"
        UPDATE narinfo SET
            deriver = $2,
            nar_hash = $3,
            nar_size = $4,
            "references" = $5,
            signatures = $6,
            ca = $7
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(&info.deriver)
    .bind(&info.nar_hash)
    .bind(info.nar_size as i64)
    .bind(&info.references)
    .bind(&info.signatures)
    .bind(&info.content_address)
    .execute(&mut *tx)
    .await?;

    // Placeholder must exist. If rows_affected==0, another process deleted it
    // (via delete_uploading) between insert_uploading and now. Fail explicitly.
    if narinfo_result.rows_affected() == 0 {
        anyhow::bail!(
            "complete_upload: narinfo placeholder missing for {} (concurrently deleted?)",
            info.store_path
        );
    }

    // Flip nar_blobs status to 'complete' and update blob_key
    let blobs_result = sqlx::query(
        r#"
        UPDATE nar_blobs SET
            status = 'complete',
            blob_key = $2,
            updated_at = now()
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(blob_key)
    .execute(&mut *tx)
    .await?;

    if blobs_result.rows_affected() == 0 {
        anyhow::bail!(
            "complete_upload: nar_blobs placeholder missing for {} (concurrently deleted?)",
            info.store_path
        );
    }

    tx.commit().await?;

    debug!(store_path = %info.store_path, "upload completed");
    Ok(())
}

/// Delete the placeholder rows created by `insert_uploading` for a failed upload.
///
/// Safe to call even if no placeholder exists. Only deletes rows where
/// `nar_size=0` (the placeholder marker — real uploads always have nar_size > 0)
/// AND `nar_blobs.status='uploading'`. This makes it safe even if a concurrent
/// upload succeeded for the same path (that upload would have nar_size > 0).
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn delete_uploading(pool: &PgPool, store_path_hash: &[u8]) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    // Delete nar_blobs first (FK constraint: nar_blobs references narinfo)
    sqlx::query(
        r#"
        DELETE FROM nar_blobs
        WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    // Delete narinfo placeholder (nar_size=0 is the placeholder marker)
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
/// Returns `Some(blob_key)` if the path exists with `status='complete'`,
/// `None` otherwise.
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn check_complete(
    pool: &PgPool,
    store_path_hash: &[u8],
) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT blob_key FROM nar_blobs
        WHERE store_path_hash = $1 AND status = 'complete'
        "#,
    )
    .bind(store_path_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(key,)| key))
}

/// Query path info for a store path. Only returns paths with `status='complete'`
/// in `nar_blobs` to exclude the placeholder narinfo rows created by
/// `insert_uploading`.
#[instrument(skip(pool))]
pub async fn query_path_info(pool: &PgPool, store_path: &str) -> anyhow::Result<Option<PathInfo>> {
    let row: Option<NarinfoRow> = sqlx::query_as(
        r#"
        SELECT n.store_path, n.store_path_hash, n.deriver, n.nar_hash, n.nar_size,
               n."references", n.signatures, n.ca
        FROM narinfo n
        INNER JOIN nar_blobs b ON n.store_path_hash = b.store_path_hash
        WHERE n.store_path = $1 AND b.status = 'complete'
        "#,
    )
    .bind(store_path)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.into_path_info()))
}

/// Query path info by store path hash. Only returns completed paths.
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn query_path_info_by_hash(
    pool: &PgPool,
    store_path_hash: &[u8],
) -> anyhow::Result<Option<PathInfo>> {
    let row: Option<NarinfoRow> = sqlx::query_as(
        r#"
        SELECT n.store_path, n.store_path_hash, n.deriver, n.nar_hash, n.nar_size,
               n."references", n.signatures, n.ca
        FROM narinfo n
        INNER JOIN nar_blobs b ON n.store_path_hash = b.store_path_hash
        WHERE n.store_path_hash = $1 AND b.status = 'complete'
        "#,
    )
    .bind(store_path_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.into_path_info()))
}

/// Batch check which store paths are missing (not in the store with `status='complete'`).
///
/// Only paths with `nar_blobs.status = 'complete'` are considered present.
/// Paths stuck in `status='uploading'` (from an interrupted upload) are
/// treated as missing so the client can retry.
#[instrument(skip(pool, store_paths), fields(count = store_paths.len()))]
pub async fn find_missing_paths(
    pool: &PgPool,
    store_paths: &[String],
) -> anyhow::Result<Vec<String>> {
    if store_paths.is_empty() {
        return Ok(Vec::new());
    }

    // Query all completed paths from the input set
    let complete: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT n.store_path
        FROM narinfo n
        INNER JOIN nar_blobs b ON n.store_path_hash = b.store_path_hash
        WHERE n.store_path = ANY($1) AND b.status = 'complete'
        "#,
    )
    .bind(store_paths)
    .fetch_all(pool)
    .await?;

    let complete_set: std::collections::HashSet<&str> =
        complete.iter().map(|(p,)| p.as_str()).collect();

    let missing = store_paths
        .iter()
        .filter(|p| !complete_set.contains(p.as_str()))
        .cloned()
        .collect();

    Ok(missing)
}

/// Get the blob key for a completed store path.
#[instrument(skip(pool))]
pub async fn get_blob_key(pool: &PgPool, store_path: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT b.blob_key
        FROM nar_blobs b
        INNER JOIN narinfo n ON n.store_path_hash = b.store_path_hash
        WHERE n.store_path = $1 AND b.status = 'complete'
        "#,
    )
    .bind(store_path)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(key,)| key))
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Row type for narinfo queries.
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
}

impl NarinfoRow {
    fn into_path_info(self) -> PathInfo {
        PathInfo {
            store_path: self.store_path,
            store_path_hash: self.store_path_hash,
            deriver: self.deriver.unwrap_or_default(),
            nar_hash: self.nar_hash,
            nar_size: self.nar_size as u64,
            references: self.references,
            // TODO(phase2c): persist registration_time/ultimate columns.
            // Currently dropped (narinfo table doesn't have them); gateway
            // returns 0/false which is observable via nix store --query but
            // doesn't break clients.
            registration_time: 0,
            ultimate: false,
            signatures: self.signatures,
            content_address: self.ca.unwrap_or_default(),
        }
    }
}
