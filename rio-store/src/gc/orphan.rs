//! Orphan scanner: reap stale 'uploading' manifests.
//!
//! If a worker crashes mid-PutPath (between insert_manifest_
//! placeholder and complete_manifest), the manifest stays in
//! `status='uploading'` forever. It's a GC root (mark.rs seeds
//! uploading manifests) so its narinfo is never swept. And its
//! chunks have refcounts from insert_manifest_chunked but the
//! manifest never completes to make them reachable via a real
//! path.
//!
//! This scanner runs periodically (15min default), finds
//! 'uploading' manifests older than `STALE_THRESHOLD` (2h default
//! — longer than any legitimate upload should take), and removes
//! them via the existing `delete_manifest_chunked_uploading`
//! (which also decrements chunk refcounts). Chunks hitting 0 get
//! enqueued to pending_s3_deletes for the drain task.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::backend::chunk::ChunkBackend;

/// How old an 'uploading' manifest must be before we reap it.
/// 2h: longer than any legitimate PutPath (even 10GB NARs on a
/// slow link). Crashed workers are detected and cleaned up within
/// this window.
#[cfg(not(test))]
const STALE_THRESHOLD: Duration = Duration::from_secs(2 * 3600);
#[cfg(test)]
const STALE_THRESHOLD: Duration = Duration::from_millis(100);

/// Scan interval. 15min: stale uploads accumulate slowly (only on
/// worker crashes), no need to scan aggressively.
#[cfg(not(test))]
const SCAN_INTERVAL: Duration = Duration::from_secs(15 * 60);
#[cfg(test)]
const SCAN_INTERVAL: Duration = Duration::from_millis(200);

/// Run one scan iteration. Returns count of orphans reaped.
///
/// For each stale uploading manifest:
/// 1. Load its chunk_list (if chunked).
/// 2. In a tx: DELETE narinfo (CASCADE), decrement chunks,
///    enqueue 0-refcount chunks to pending_s3_deletes.
///
/// Same transaction semantics as sweep::sweep (the two are
/// structurally similar — orphan is "sweep for uploading-status"
/// with different selection criteria).
pub async fn scan_once(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
) -> Result<u64, sqlx::Error> {
    // Find stale uploading manifests. SELECT hash + chunk_list.
    // `updated_at` is set on insert and on status flip; for
    // never-completed uploads it's the insert time.
    //
    // LEFT JOIN manifest_data: inline uploads have no row there
    // (chunk_list NULL → no chunk decrement, just CASCADE delete).
    let threshold_secs = STALE_THRESHOLD.as_secs() as i64;
    let stale: Vec<(Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
        r#"
        SELECT m.store_path_hash, md.chunk_list
          FROM manifests m
          LEFT JOIN manifest_data md USING (store_path_hash)
         WHERE m.status = 'uploading'
           AND m.updated_at < now() - make_interval(secs => $1)
        "#,
    )
    .bind(threshold_secs)
    .fetch_all(pool)
    .await?;

    if stale.is_empty() {
        debug!("orphan scan: no stale uploading manifests");
        return Ok(0);
    }

    let mut reaped = 0u64;
    for (store_path_hash, chunk_list) in stale {
        // Single-path transaction (not batched — orphans are rare,
        // batching isn't worth the complexity).
        let mut tx = pool.begin().await?;

        // DELETE narinfo → CASCADE to manifests/manifest_data.
        // If rowcount 0, someone else (concurrent scan, or the
        // upload actually completed between our SELECT and now)
        // got to it first. Skip.
        let deleted = sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1")
            .bind(&store_path_hash)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() == 0 {
            // Gone or completed — either way, not an orphan
            // anymore. Rollback (no-op, nothing changed) and
            // continue.
            tx.rollback().await?;
            continue;
        }

        // Chunk decrement + enqueue (if chunked). Same helper as
        // sweep::sweep — see gc::decrement_and_enqueue.
        if let Some(bytes) = chunk_list {
            super::decrement_and_enqueue(&mut tx, &bytes, chunk_backend).await?;
        }

        tx.commit().await?;
        reaped += 1;
    }

    if reaped > 0 {
        info!(
            count = reaped,
            "orphan scan: reaped stale uploading manifests"
        );
    }
    Ok(reaped)
}

/// Spawn the periodic orphan scanner. Runs `scan_once` every
/// SCAN_INTERVAL. Errors logged; next iteration retries.
pub fn spawn_scanner(
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
) -> tokio::task::JoinHandle<()> {
    rio_common::task::spawn_monitored("gc-orphan-scanner", async move {
        let mut interval = tokio::time::interval(SCAN_INTERVAL);
        // Skip: if one scan is slow (large orphan backlog), don't
        // fire twice immediately. Interval drifts; fine for a 15min
        // background task.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(e) = scan_once(&pool, chunk_backend.as_ref()).await {
                warn!(error = %e, "orphan scan failed (will retry next interval)");
            }
        }
    })
}
