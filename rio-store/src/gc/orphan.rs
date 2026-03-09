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
    // Find stale uploading manifests. SELECT hash only — chunk_list
    // is re-read INSIDE the tx (see X3 race below).
    let threshold_secs = STALE_THRESHOLD.as_secs() as i64;
    let stale: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT m.store_path_hash
          FROM manifests m
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
    for (store_path_hash,) in stale {
        // Single-path transaction (not batched — orphans are rare,
        // batching isn't worth the complexity).
        let mut tx = pool.begin().await?;

        // Re-read chunk_list INSIDE the tx with FOR UPDATE (X3 fix)
        // AND re-check the stale threshold (Y3 fix).
        //
        // X3: the outer SELECT is OUTSIDE any tx. Reading chunk_list
        // INSIDE the tx (with FOR UPDATE locking the manifest row)
        // guarantees we decrement the chunk_list that we DELETE.
        //
        // Y3: status='uploading' alone doesn't catch the reap-then-
        // reupload race. store-0 + store-1 both outer-SELECT the
        // same stale hash; store-0 reaps; worker re-uploads (NEW
        // row, same hash, status='uploading', updated_at=now());
        // store-1's FOR UPDATE would match the NEW row (status is
        // 'uploading' ✓) and reap a FRESH upload. Re-checking the
        // stale threshold inside the tx catches this — fresh
        // re-uploads have updated_at=now() → don't match.
        //
        // The FOR UPDATE blocks any concurrent re-upload until this
        // tx commits — same pattern as sweep.rs:64-76.
        let chunk_list: Option<Vec<u8>> = sqlx::query_scalar(
            r#"
            SELECT md.chunk_list
              FROM manifests m
              LEFT JOIN manifest_data md USING (store_path_hash)
             WHERE m.store_path_hash = $1
               AND m.status = 'uploading'
               AND m.updated_at < now() - make_interval(secs => $2)
               FOR UPDATE OF m
            "#,
        )
        .bind(&store_path_hash)
        .bind(threshold_secs)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();

        // DELETE narinfo → CASCADE to manifests/manifest_data.
        //
        // Status + stale-threshold guards in EXISTS: atomic re-check
        // at DELETE time. rows_affected()==0 catches: (a) another
        // replica already reaped (gone), (b) upload completed since
        // outer SELECT (status='complete' → EXISTS false), (c) Y3:
        // reap-then-reupload left a FRESH 'uploading' row (updated_at
        // recent → stale clause false → EXISTS false).
        //
        // The FOR UPDATE above already re-checked stale+status and
        // locked the row — the EXISTS guard is defense-in-depth for
        // the case where FOR UPDATE returned 0 rows (chunk_list=None)
        // but the DELETE would otherwise match a fresh row.
        let deleted = sqlx::query(
            r#"
            DELETE FROM narinfo n
             WHERE n.store_path_hash = $1
               AND EXISTS (
                   SELECT 1 FROM manifests m
                    WHERE m.store_path_hash = $1
                      AND m.status = 'uploading'
                      AND m.updated_at < now() - make_interval(secs => $2)
               )
            "#,
        )
        .bind(&store_path_hash)
        .bind(threshold_secs)
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
        // sweep::sweep — see gc::decrement_and_enqueue. chunk_list
        // was read INSIDE the tx above (X3 fix) — it's the CURRENT
        // value for the manifest we just deleted.
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

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// Helper: insert an 'uploading' placeholder AND backdate
    /// updated_at so the stale-threshold check deterministically
    /// matches (test STALE_THRESHOLD.as_secs()==0 means the query
    /// needs updated_at < now(), which is fragile if set to now()
    /// in the same statement — backdating avoids the race).
    async fn seed_stale_uploading(pool: &PgPool, hash: &[u8], path: &str) {
        crate::metadata::insert_manifest_uploading(pool, hash, path)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - interval '1 hour' \
             WHERE store_path_hash = $1",
        )
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn orphan_reaps_stale_uploading() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x01u8; 32];
        let path = rio_test_support::fixtures::test_store_path("orphan-stale");
        seed_stale_uploading(&db.pool, &hash, &path).await;

        let reaped = scan_once(&db.pool, None).await.unwrap();
        assert_eq!(reaped, 1, "stale uploading manifest reaped");

        // narinfo gone (CASCADE took manifests too).
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 0, "narinfo deleted");
    }

    /// TOCTOU regression: upload completes between scan's SELECT
    /// and DELETE. The status guard in the DELETE's WHERE must
    /// catch this and SKIP the delete.
    #[tokio::test]
    async fn orphan_skips_completed_upload_toctou() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed stale uploading placeholder — it WOULD be reaped.
        let hash = vec![0x02u8; 32];
        let path = rio_test_support::fixtures::test_store_path("orphan-raced");
        seed_stale_uploading(&db.pool, &hash, &path).await;

        // Simulate upload completing BETWEEN the outer SELECT and
        // the per-path DELETE. In the real race, this happens while
        // scan_once is iterating. Here we flip status directly
        // before calling scan_once — the DELETE's WHERE EXISTS
        // (status='uploading') should see status='complete' → no
        // match → rows_affected()==0 → skipped.
        //
        // Also set nar_size>0 so it's clearly a real completed path
        // (not that the DELETE checks this — the status guard is
        // what matters).
        sqlx::query("UPDATE manifests SET status = 'complete' WHERE store_path_hash = $1")
            .bind(&hash)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE narinfo SET nar_size = 42 WHERE store_path_hash = $1")
            .bind(&hash)
            .execute(&db.pool)
            .await
            .unwrap();

        // Key point: the outer SELECT in scan_once filters on
        // status='uploading' + updated_at. Since we already flipped
        // status, scan_once's SELECT won't even find this hash. To
        // test the DELETE guard SPECIFICALLY (not the SELECT), we
        // need the SELECT to find it but the DELETE to skip it.
        //
        // We can't easily interleave with scan_once's internal loop
        // from a unit test. Instead, we assert the INVARIANT
        // directly: run the same DELETE query with status+stale
        // guard (Y3), verify rows_affected==0.
        let deleted = sqlx::query(
            r#"
            DELETE FROM narinfo n
             WHERE n.store_path_hash = $1
               AND EXISTS (
                   SELECT 1 FROM manifests m
                    WHERE m.store_path_hash = $1
                      AND m.status = 'uploading'
                      AND m.updated_at < now() - make_interval(secs => $2)
               )
            "#,
        )
        .bind(&hash)
        .bind(0i64)
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            deleted.rows_affected(),
            0,
            "status guard prevented delete of completed upload"
        );

        // narinfo still present.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "completed upload NOT deleted by status guard");

        // And scan_once itself finds nothing (status already
        // complete → SELECT filters it out).
        let reaped = scan_once(&db.pool, None).await.unwrap();
        assert_eq!(reaped, 0, "scan_once found nothing (status=complete)");
    }

    #[tokio::test]
    async fn orphan_skips_fresh_uploading() {
        // Fresh (not stale) upload in progress → NOT reaped.
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x03u8; 32];
        let path = rio_test_support::fixtures::test_store_path("orphan-fresh");
        // Insert WITHOUT backdating — updated_at = now(). With test
        // STALE_THRESHOLD.as_secs()==0, query is `updated_at < now()`.
        // Set updated_at slightly in the future to guarantee NOT stale.
        crate::metadata::insert_manifest_uploading(&db.pool, &hash, &path)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() + interval '10 seconds' \
             WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();

        let reaped = scan_once(&db.pool, None).await.unwrap();
        assert_eq!(reaped, 0, "fresh upload not reaped");

        // narinfo still present.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "fresh upload narinfo preserved");
    }

    /// Y3: reap-then-reupload race. store-0 + store-1 both
    /// outer-SELECT the same stale hash. store-0 reaps it. Worker
    /// re-uploads (NEW row, same hash, status='uploading',
    /// updated_at fresh). store-1's inner FOR UPDATE + DELETE must
    /// NOT reap the fresh re-upload.
    ///
    /// We can't race two scan_once calls; instead we simulate
    /// store-1's inner-loop state: we already HAVE the hash from
    /// a stale outer SELECT, but the DB now has a FRESH re-upload
    /// at that hash. Running the inner queries directly (FOR UPDATE
    /// + DELETE with the stale-threshold re-check) must return 0.
    #[tokio::test]
    async fn orphan_skips_fresh_reupload_after_another_replicas_reap() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = vec![0x04u8; 32];
        let path = rio_test_support::fixtures::test_store_path("y3-reupload");

        // --- store-0's turn: seed stale + reap ---
        seed_stale_uploading(&db.pool, &hash, &path).await;
        let reaped = scan_once(&db.pool, None).await.unwrap();
        assert_eq!(reaped, 1, "store-0 reaped the stale upload");

        // --- Worker re-uploads same path (FRESH placeholder) ---
        // updated_at = now() + 10s guarantees NOT stale under the
        // test threshold (0s → `updated_at < now()`).
        crate::metadata::insert_manifest_uploading(&db.pool, &hash, &path)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() + interval '10 seconds' \
             WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();

        // --- store-1's inner loop: has the hash from its OWN outer
        // SELECT (which ran BEFORE store-0's reap, saw stale). ---
        //
        // Run the FOR UPDATE query directly. With the Y3 stale-
        // threshold re-check, it should return None (fresh re-upload
        // has updated_at > now()-threshold → doesn't match).
        let mut tx = db.pool.begin().await.unwrap();
        let chunk_list: Option<Vec<u8>> = sqlx::query_scalar(
            r#"
            SELECT md.chunk_list
              FROM manifests m
              LEFT JOIN manifest_data md USING (store_path_hash)
             WHERE m.store_path_hash = $1
               AND m.status = 'uploading'
               AND m.updated_at < now() - make_interval(secs => $2)
               FOR UPDATE OF m
            "#,
        )
        .bind(&hash)
        .bind(0i64)
        .fetch_optional(&mut *tx)
        .await
        .unwrap()
        .flatten();
        assert!(
            chunk_list.is_none(),
            "Y3: FOR UPDATE must NOT match fresh re-upload (stale re-check)"
        );

        // The DELETE should also skip (EXISTS with stale clause
        // → false for fresh row).
        let deleted = sqlx::query(
            r#"
            DELETE FROM narinfo n
             WHERE n.store_path_hash = $1
               AND EXISTS (
                   SELECT 1 FROM manifests m
                    WHERE m.store_path_hash = $1
                      AND m.status = 'uploading'
                      AND m.updated_at < now() - make_interval(secs => $2)
               )
            "#,
        )
        .bind(&hash)
        .bind(0i64)
        .execute(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            deleted.rows_affected(),
            0,
            "Y3: DELETE must NOT reap fresh re-upload"
        );
        tx.rollback().await.unwrap();

        // Fresh re-upload's narinfo still present.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "Y3: fresh re-upload narinfo survived");
    }
}
