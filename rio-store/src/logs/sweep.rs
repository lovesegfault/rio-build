//! TTL sweep for expired build logs.
//!
//! Build logs are retained for `log_retention_days` (default 30) after
//! the execution *started* — `drv_executions.started_at` is the only
//! timestamp guaranteed to exist for every execution (a builder that
//! dies before reporting completion never sets `finished_at`). Once an
//! hour the sweep deletes, in batches:
//!
//! 1. the execution's `drv_log_chunks` manifest rows (`RETURNING
//!    s3_key`),
//! 2. the chunk objects those rows pointed at,
//! 3. and any stale `log_ingest_sessions` row.
//!
//! The `drv_executions` row itself is deliberately NOT deleted here
//! (`store.log.sweep-ownership`): it is the scheduler-owned execution
//! lifecycle row — terminality, active assignments, and retry-ledger
//! references are scheduler facts this sweep cannot see, and deleting
//! the row by age alone destroyed kind/attribution state behind the
//! scheduler's back. The scheduler's `gc_exec_rows` pass collects it
//! once it is terminal, unreferenced, and past `exec_retention_days`.
//! A stripped execution row is never re-selected: the victim SELECT
//! requires remaining chunk or session rows.
//!
//! **Ordering is load-bearing.** The manifest rows go before the *objects* so
//! a crash between the two leaves orphaned objects (unreachable from
//! PG, bounded by the S3 lifecycle rule on `logs/`) rather than
//! manifest rows pointing at deleted objects — which the read path
//! surfaces as data loss (`rio_store_log_read_data_loss_total`). An
//! object-delete failure is logged and skipped for the same reason: the
//! rows are already gone, so the orphaned objects are invisible to
//! readers and the lifecycle rule collects them.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use super::chunks::LogChunkStore;

/// How often the sweep runs. Hourly: retention is measured in days,
/// so anything finer than an hour is pointless PG load.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Executions per batch. Each batch is one indexed SELECT, two `= ANY`
/// DELETEs, one session DELETE, and up to `batch × chunks-per-exec`
/// object deletes — small enough to never hold the sweep task hostage,
/// large enough that the steady-state pass count is ~1.
pub const SWEEP_BATCH: i64 = 1000;

/// What one [`sweep_expired_logs`] pass deleted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    /// Expired executions whose log artifacts this pass swept.
    pub executions_swept: u64,
    /// `drv_log_chunks` manifest rows deleted.
    pub chunks: u64,
    /// Chunk objects successfully deleted from the backend. Lags
    /// `chunks` when a backend delete fails (the orphans are bounded by
    /// the S3 lifecycle rule).
    pub objects: u64,
}

/// Delete every execution older than `retention` along with its log
/// chunks, in batches of `batch`, until a batch comes back short.
///
/// A PG error aborts the pass (returned to the caller, retried next
/// tick). A backend object-delete error is logged and the pass
/// continues — see the module doc for why that is safe.
pub async fn sweep_expired_logs(
    pool: &PgPool,
    store: &dyn LogChunkStore,
    retention: Duration,
    batch: i64,
) -> Result<SweepStats, sqlx::Error> {
    let mut stats = SweepStats::default();
    loop {
        // The inner SELECT rides `drv_executions_started_at`; no ORDER
        // BY because any `batch` expired rows are equally good to
        // sweep. `make_interval` binds the retention directly so the
        // SQL cannot drift from the config value. The EXISTS
        // disjunction is load-bearing now that the execution row
        // itself survives the sweep (`store.log.sweep-ownership`):
        // without it, an already-stripped expired execution would be
        // re-selected on every pass — the loop would never observe an
        // empty batch once more than `batch` expired executions exist,
        // and the swept count would inflate without bound.
        let expired: Vec<Uuid> = sqlx::query_scalar(
            "SELECT exec_id FROM drv_executions e \
             WHERE e.started_at < now() - make_interval(secs => $1) \
               AND (EXISTS (SELECT 1 FROM drv_log_chunks c \
                            WHERE c.exec_id = e.exec_id) \
                 OR EXISTS (SELECT 1 FROM log_ingest_sessions s \
                            WHERE s.exec_id = e.exec_id)) \
             LIMIT $2",
        )
        .bind(retention.as_secs_f64())
        .bind(batch)
        .fetch_all(pool)
        .await?;
        if expired.is_empty() {
            break;
        }

        // Manifest rows first (RETURNING the keys we then delete from
        // the backend). See the module doc for the ordering argument.
        let keys: Vec<String> = sqlx::query_scalar(
            "DELETE FROM drv_log_chunks WHERE exec_id = ANY($1) RETURNING s3_key",
        )
        .bind(&expired)
        .fetch_all(pool)
        .await?;
        stats.chunks += keys.len() as u64;
        metrics::counter!("rio_store_log_sweep_chunks_deleted_total").increment(keys.len() as u64);

        if !keys.is_empty() {
            match store.delete_batch(&keys).await {
                Ok(()) => {
                    stats.objects += keys.len() as u64;
                    metrics::counter!("rio_store_log_sweep_objects_deleted_total")
                        .increment(keys.len() as u64);
                }
                Err(e) => {
                    // The manifest rows are already gone, so these
                    // objects are unreachable from PG and will not be
                    // re-tried; the S3 lifecycle rule on `logs/` is the
                    // backstop. Warn so an operator notices a
                    // persistently failing delete path.
                    warn!(
                        error = %e,
                        keys = keys.len(),
                        "log TTL sweep: chunk object delete failed; orphans are \
                         bounded by the S3 lifecycle rule"
                    );
                }
            }
        }

        // A live ingest session for an expired execution should not
        // exist (retention is days, a session is minutes) but a stale
        // row costs nothing to clear and would otherwise dangle
        // forever.
        sqlx::query("DELETE FROM log_ingest_sessions WHERE exec_id = ANY($1)")
            .bind(&expired)
            .execute(pool)
            .await?;

        // r[impl store.log.sweep-ownership]
        // The drv_executions row is NOT deleted: scheduler-owned
        // lifecycle state (see the module doc). The candidate SELECT's
        // EXISTS disjunction keeps a stripped row out of every later
        // pass, so this count is exact (each expired execution is
        // swept once).
        stats.executions_swept += expired.len() as u64;

        if expired.len() < batch as usize {
            break;
        }
    }
    Ok(stats)
}

/// Spawn the hourly sweep task. Mirrors
/// [`crate::gc::orphan::spawn_scanner`]: a panic is logged and
/// the store keeps serving (degraded GC, not down); shutdown cancels the
/// next tick.
pub fn spawn_log_sweep(
    pool: PgPool,
    store: Arc<dyn LogChunkStore>,
    retention: Duration,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    rio_common::task::spawn_periodic("log-ttl-sweep", SWEEP_INTERVAL, shutdown, move || {
        let pool = pool.clone();
        let store = Arc::clone(&store);
        async move {
            match sweep_expired_logs(&pool, store.as_ref(), retention, SWEEP_BATCH).await {
                Ok(stats) if stats.executions_swept > 0 => {
                    info!(
                        executions_swept = stats.executions_swept,
                        chunks = stats.chunks,
                        objects = stats.objects,
                        "log TTL sweep: deleted expired build logs"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "log TTL sweep failed (will retry next interval)");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::chunks::{LogChunkError, MemoryLogChunkStore, log_chunk_key};

    const DRV_HASH_32: &str = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm";

    /// Seed one execution aged `age_days` with `n_chunks` manifest rows
    /// and objects plus a (stale) ingest-session row. Returns the chunk
    /// keys.
    async fn seed_aged_execution(
        pool: &PgPool,
        store: &MemoryLogChunkStore,
        exec_id: Uuid,
        age_days: f64,
        n_chunks: u32,
    ) -> Vec<String> {
        sqlx::query(
            "INSERT INTO drv_executions \
                 (exec_id, drv_hash, executor_id, started_at, status, final_line_count) \
             VALUES ($1, $2, 'builder-0', now() - make_interval(secs => $3), 'succeeded', 10)",
        )
        .bind(exec_id)
        .bind(DRV_HASH_32)
        .bind(age_days * 86_400.0)
        .execute(pool)
        .await
        .unwrap();
        let session_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO log_ingest_sessions (exec_id, session_id, replica_pod) \
             VALUES ($1, $2, 'store-test')",
        )
        .bind(exec_id)
        .bind(session_id)
        .execute(pool)
        .await
        .unwrap();
        let mut keys = Vec::new();
        for seq in 0..n_chunks {
            let key = log_chunk_key(DRV_HASH_32, &exec_id, &session_id, seq);
            store.put(&key, b"blob".to_vec()).await.unwrap();
            sqlx::query(
                "INSERT INTO drv_log_chunks \
                     (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, s3_key) \
                 VALUES ($1, $2, $3, $4, 5, 4, $5)",
            )
            .bind(exec_id)
            .bind(session_id)
            .bind(seq as i32)
            .bind((seq as i64) * 5)
            .bind(&key)
            .execute(pool)
            .await
            .unwrap();
            keys.push(key);
        }
        keys
    }

    /// Per-table row counts for one execution, for asserting exactly
    /// which tables the sweep touched. Literal SQL per table — sqlx
    /// (rightly) rejects dynamically-built query strings.
    async fn counts(pool: &PgPool, exec_id: Uuid) -> (i64, i64, i64) {
        let execs = sqlx::query_scalar("SELECT count(*) FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let chunks = sqlx::query_scalar("SELECT count(*) FROM drv_log_chunks WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let sessions =
            sqlx::query_scalar("SELECT count(*) FROM log_ingest_sessions WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(pool)
                .await
                .unwrap();
        (execs, chunks, sessions)
    }

    // r[verify store.log.sweep-ownership]
    /// merged_bug_086 (red-first): the log TTL sweep owns store-side
    /// log artifacts ONLY — chunks, objects, stale session rows.
    /// `drv_executions` is the scheduler-owned execution lifecycle row:
    /// age alone says nothing about whether it is terminal, whether an
    /// assignment is still active, or whether the retry ledger still
    /// references it for kind resolution — deleting it here destroyed
    /// cross-service state behind the scheduler's back. The recorded
    /// red: the pre-fix sweep deleted the row (execs count 0).
    #[tokio::test]
    async fn sweep_leaves_drv_executions_untouched() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        seed_aged_execution(&db.pool, &store, exec, 31.0, 2).await;

        sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            SWEEP_BATCH,
        )
        .await
        .unwrap();

        assert_eq!(
            counts(&db.pool, exec).await,
            (1, 0, 0),
            "chunks and sessions swept; the execution lifecycle row is \
             the scheduler's to collect (gc_exec_rows)"
        );
    }

    /// The 31-day-old execution's rows AND objects are gone; the
    /// 1-day-old execution's are untouched.
    #[tokio::test]
    async fn sweep_deletes_expired_and_spares_fresh() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let old_exec = Uuid::now_v7();
        let fresh_exec = Uuid::now_v7();
        let old_keys = seed_aged_execution(&db.pool, &store, old_exec, 31.0, 2).await;
        let fresh_keys = seed_aged_execution(&db.pool, &store, fresh_exec, 1.0, 2).await;

        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            SWEEP_BATCH,
        )
        .await
        .unwrap();

        assert_eq!(
            stats,
            SweepStats {
                executions_swept: 1,
                chunks: 2,
                objects: 2,
            }
        );
        // The expired execution: log artifacts gone (objects too); the
        // scheduler-owned lifecycle row survives.
        assert_eq!(counts(&db.pool, old_exec).await, (1, 0, 0));
        for key in &old_keys {
            assert!(
                matches!(store.get(key).await, Err(LogChunkError::NotFound { .. })),
                "expired object {key} should be deleted"
            );
        }
        // The fresh execution: untouched.
        assert_eq!(counts(&db.pool, fresh_exec).await, (1, 2, 1));
        for key in &fresh_keys {
            assert!(store.get(key).await.is_ok(), "fresh object {key} kept");
        }
    }

    /// An empty table is a no-op pass, not a panic or an error.
    #[tokio::test]
    async fn sweep_handles_empty_table() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            SWEEP_BATCH,
        )
        .await
        .unwrap();
        assert_eq!(stats, SweepStats::default());
    }

    /// The batch loop terminates and sweeps everything when the expired
    /// set is larger than one batch.
    #[tokio::test]
    async fn sweep_drains_multiple_batches() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        for _ in 0..3 {
            seed_aged_execution(&db.pool, &store, Uuid::now_v7(), 31.0, 1).await;
        }
        let stats = sweep_expired_logs(&db.pool, &store, Duration::from_secs(30 * 86_400), 2)
            .await
            .unwrap();
        assert_eq!(stats.executions_swept, 3);
        assert_eq!(stats.chunks, 3);
        assert!(store.is_empty());
    }
}
