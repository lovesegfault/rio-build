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
//! The victim predicate carries its exclusions structurally
//! (merged_bug_071): only TERMINAL executions (a non-terminal row is
//! an open attempt that may legally stream until the scheduler's
//! deadline cap) with NO ingest-session row inside the reap grace
//! (the sibling reap's 10× discipline) are sweepable — and config
//! validation refuses retention values that collapse the
//! retention-vs-deadline separation, so a live near-cap build can
//! never be mid-stream when its logs expire. The candidate SELECT
//! takes `FOR UPDATE SKIP LOCKED` (the drain idiom) so concurrent
//! replicas sweep disjoint batches (bug_104).
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
//! surfaces as data loss (the read path’s loss counter). An
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
    /// Expired executions claimed by this pass's candidate SELECT. A
    /// late-revived execution (a session admitted into the statement
    /// gap) is claimed but its artifacts are SPARED by the
    /// per-statement exclusions (merged_bug_034); it re-qualifies at
    /// a later pass once truly dead.
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
///
/// Every batch demands fresh authority (merged_bug_006): the
/// until-short loop is a multi-batch destructive body — committed PG
/// deletes plus irreversible post-commit S3 object deletes — so a
/// global gc hold landing mid-pass (or a drain-bound-aged clearance)
/// stops the NEXT batch at its boundary instead of riding the
/// tick-start consult through the whole backlog. The wave-10 spawn
/// discarded the lane clearance (`move |_clearance|`) and this loop
/// never re-authorized — the lone violator of the per-batch law its
/// own lane registered under.
// r[impl store.gc.hold-lanes+1]
// r[impl store.gc.clearance-expiry]
// r[impl store.gc.batch-authority]
pub async fn sweep_expired_logs(
    pool: &PgPool,
    store: &dyn LogChunkStore,
    retention: Duration,
    batch: i64,
    clearance: &mut crate::gc::hold::HoldClearance,
) -> Result<SweepStats, sqlx::Error> {
    let mut stats = SweepStats::default();
    loop {
        let authority = match clearance.authorize_batch(pool).await? {
            crate::gc::hold::BatchAuthorize::Authorized(a) => a,
            crate::gc::hold::BatchAuthorize::Held(h) => {
                info!(
                    hold_id = %h.hold_id,
                    reason = %h.reason,
                    created_by = %h.created_by,
                    executions_swept = stats.executions_swept,
                    "log TTL sweep: global hold landed mid-pass; \
                     stopping at the batch boundary"
                );
                break;
            }
            crate::gc::hold::BatchAuthorize::Expired => {
                warn!(
                    executions_swept = stats.executions_swept,
                    "log TTL sweep: clearance aged past the drain bound; \
                     stopping at the batch boundary (next tick re-gates)"
                );
                break;
            }
        };
        // Spent on this batch: the two PG DELETEs in the transaction
        // below plus its post-commit object deletes are the one batch
        // this token funds.
        authority.spend();
        // One transaction per batch: the candidate SELECT takes
        // `FOR UPDATE OF e SKIP LOCKED` (the drain.rs idiom — two
        // destructive-lane coordination idioms collapse to one,
        // bug_104) so concurrent replicas claim DISJOINT batches, and
        // the row locks span only the two PG DELETEs below (the
        // object deletes run post-commit). The locked rows are
        // terminal executions the scheduler no longer updates, so the
        // lock window contends with nothing hot.
        let mut tx = pool.begin().await?;

        // The victim predicate (merged_bug_071): age alone is NOT a
        // liveness proof — destructive sweeps carry their exclusions
        // STRUCTURALLY, never by assumed magnitude ordering between
        // independently tunable constants. A victim must be (a) past
        // retention, (b) TERMINAL — a non-terminal row is an open
        // attempt whose stream may still legally run up to the
        // scheduler's deadline cap (config validation refuses
        // retention values at or under that cap, the other half of
        // this close), and (c) free of any ingest-session row inside
        // the reap grace — the sibling sweep_stale_sessions'
        // discipline: a session row younger than 10× the staleness
        // bound might belong to a live stream, and the sweep is
        // structurally incapable of racing one. The EXISTS
        // disjunction is load-bearing as before: a stripped expired
        // execution never re-selects, so the loop terminates and the
        // counters cannot inflate.
        let live_grace = rio_migrations::sql::live_ingest_session_sql("s.heartbeat_at", "$4");
        let sql = format!(
            "SELECT exec_id FROM drv_executions e \
             WHERE e.started_at < now() - make_interval(secs => $1) \
               AND e.status = ANY($3) \
               AND NOT EXISTS (SELECT 1 FROM log_ingest_sessions s \
                               WHERE s.exec_id = e.exec_id AND {live_grace}) \
               AND (EXISTS (SELECT 1 FROM drv_log_chunks c \
                            WHERE c.exec_id = e.exec_id) \
                 OR EXISTS (SELECT 1 FROM log_ingest_sessions s2 \
                            WHERE s2.exec_id = e.exec_id)) \
             LIMIT $2 \
             FOR UPDATE OF e SKIP LOCKED"
        );
        // AssertSqlSafe: const-fragment composition, no runtime data.
        let expired: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(retention.as_secs_f64())
            .bind(batch)
            .bind(rio_migrations::schema::EXEC_STATUS_TERMINAL)
            .bind(SESSION_REAP_GRACE_SECS)
            .fetch_all(&mut *tx)
            .await?;
        if expired.is_empty() {
            break;
        }

        // Manifest rows first (RETURNING the keys we then delete from
        // the backend, post-commit). See the module doc for the
        // ordering argument. THE EXCLUSION TRAVELS (merged_bug_034):
        // under READ COMMITTED each destructive statement gets a
        // FRESH snapshot, and `FOR UPDATE OF e` locks only
        // drv_executions — a session admitted between the candidate
        // SELECT and this statement (the just-admitted late replay)
        // would otherwise lose the chunk rows its gate just read as
        // durable. The predicate is the same shared fragment the
        // SELECT used (one definition, repeated per statement — the
        // sibling sweep_stale_sessions' house discipline).
        let chunk_delete = format!(
            "DELETE FROM drv_log_chunks c WHERE c.exec_id = ANY($1) \
               AND NOT EXISTS (SELECT 1 FROM log_ingest_sessions s \
                               WHERE s.exec_id = c.exec_id AND {live_grace}) \
             RETURNING s3_key",
            live_grace = rio_migrations::sql::live_ingest_session_sql("s.heartbeat_at", "$2")
        );
        // AssertSqlSafe: const-fragment composition, no runtime data.
        let keys: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(chunk_delete))
            .bind(&expired)
            .bind(SESSION_REAP_GRACE_SECS)
            .fetch_all(&mut *tx)
            .await?;

        // Session rows of the swept executions: the victim predicate
        // proved none was inside the reap grace AT SELECT TIME; this
        // statement's own snapshot must re-prove it (merged_bug_034)
        // — a lease committed into the statement gap is excluded
        // STRUCTURALLY, exactly as the sibling sweep_stale_sessions
        // carries NOT(live) in its own DELETE.
        let session_delete = format!(
            "DELETE FROM log_ingest_sessions WHERE exec_id = ANY($1) \
               AND NOT ({live_grace})",
            live_grace = rio_migrations::sql::live_ingest_session_sql("heartbeat_at", "$2")
        );
        // AssertSqlSafe: const-fragment composition, no runtime data.
        sqlx::query(sqlx::AssertSqlSafe(session_delete))
            .bind(&expired)
            .bind(SESSION_REAP_GRACE_SECS)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

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

        // r[impl store.log.sweep-ownership+2]
        // The drv_executions row is NOT deleted: scheduler-owned
        // lifecycle state (see the module doc). This count derives
        // from the DB's partitioning primitive (bug_104): the
        // candidate SELECT held `FOR UPDATE SKIP LOCKED` row locks
        // through the deletes, so concurrent replicas' batches are
        // disjoint and each expired execution is counted by exactly
        // one replica's pass — the predecessor's bare SELECT let
        // overlapping ticks double-accumulate this field (the chunk
        // and object counters were always RETURNING-derived and
        // exact).
        stats.executions_swept += expired.len() as u64;

        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            let hold_after = SWEEP_HOLD_AFTER_BATCHES.load(Ordering::SeqCst);
            if hold_after > 0 {
                let fired = SWEEP_HOLD_AFTER_BATCHES.fetch_sub(1, Ordering::SeqCst);
                if fired == 1 {
                    // The mid-pass hold schedule (W12-O2): landed
                    // through the PRODUCTION set_hold statement
                    // between two committed log-sweep batches.
                    crate::gc::hold::set_hold(
                        pool,
                        crate::gc::hold::GcHoldScope::Global,
                        "w12-o2 mid-pass hold (test interpose)",
                        "log-sweep-test-hook",
                        None,
                    )
                    .await
                    .expect("test interpose: set_hold");
                }
            }
        }

        if expired.len() < batch as usize {
            break;
        }
    }
    Ok(stats)
}

/// Test-only mid-pass hold interposition (W12-O2, the merged_bug_006
/// red): when set to N > 0, a GLOBAL hold is inserted through the
/// production `hold::set_hold` statement immediately after the
/// until-short loop commits its Nth batch — the exact "hold lands
/// between two committed batches" schedule, which no external caller
/// can time deterministically.
#[cfg(test)]
pub(crate) static SWEEP_HOLD_AFTER_BATCHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Grace before a dead session row is reaped outright: 10× the
/// staleness bound. Generous on purpose — liveness consumers
/// (`lookup_live`, the scheduler's `gc_exec_rows` conjunct 5) already
/// ignore stale rows at [`super::sessions::SESSION_STALE_AFTER`], so
/// this reap is pure convergence hygiene: existence converges to
/// liveness instead of dead rows dangling until the 30-day log TTL
/// (bug_234 — mirroring the predicate alone would leave them forever).
/// The wide margin keeps the reap structurally incapable of racing a
/// live session's heartbeat cadence.
pub const SESSION_REAP_GRACE_SECS: f64 = rio_migrations::sql::SESSION_STALE_AFTER_SECS * 10.0;

/// Reap `log_ingest_sessions` rows whose owner died uncleanly: rows
/// stale past [`SESSION_REAP_GRACE_SECS`]. Returns the reap count.
///
/// Runs on the same periodic task as [`sweep_expired_logs`] — the
/// hourly cadence is fine because nothing *waits* on this deletion
/// (every liveness consumer already ignores stale rows); it only
/// bounds dead-row accumulation.
pub async fn sweep_stale_sessions(pool: &PgPool) -> Result<u64, sqlx::Error> {
    // Staleness is NOT(live) of the ONE shared predicate (bug_234),
    // with the grace substituted for the staleness bound.
    let sql = format!(
        "DELETE FROM log_ingest_sessions WHERE NOT ({live})",
        live = rio_migrations::sql::live_ingest_session_sql("heartbeat_at", "$1")
    );
    // AssertSqlSafe: const-fragment composition, no runtime data.
    let result = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(SESSION_REAP_GRACE_SECS)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
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
    // r[impl store.gc.hold-lanes+1]
    // Registered through DestructiveLane (merged_bug_050): the
    // wrapper consults the gc-hold gate fail-closed each tick AND the
    // minted clearance is CONSUMED by the sweep body (merged_bug_006
    // — the wave-10 `move |_clearance|` discarded it, so the
    // until-short loop never re-authorized and a hold landing
    // mid-pass could not stop batches 2..N): `sweep_expired_logs`
    // demands per-batch authority at every committed-transaction
    // boundary. During an incident freeze, build-log evidence is
    // exactly what the hold preserves. The stale-session reap is a
    // single-statement single-batch body under the same tick
    // clearance (one batch in flight at hold-land is the R17 bound).
    let lane_pool = pool.clone();
    crate::gc::lane::DestructiveLane::spawn_periodic(
        "log-ttl-sweep",
        SWEEP_INTERVAL,
        pool,
        shutdown,
        Box::new(move |clearance| {
            let pool = lane_pool.clone();
            let store = Arc::clone(&store);
            Box::pin(async move {
                match sweep_expired_logs(&pool, store.as_ref(), retention, SWEEP_BATCH, clearance)
                    .await
                {
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
                // Dead-session convergence (bug_234): reap rows whose
                // owner died uncleanly, independent of the 30-day log TTL.
                match sweep_stale_sessions(&pool).await {
                    Ok(0) => {}
                    Ok(reaped) => {
                        info!(reaped, "log TTL sweep: reaped stale ingest sessions");
                        metrics::counter!("rio_store_log_sweep_stale_sessions_reaped_total")
                            .increment(reaped);
                    }
                    Err(e) => {
                        warn!(error = %e, "stale-session reap failed (will retry next interval)");
                    }
                }
            })
        }),
    )
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
        // The session row is DEAD (past the reap grace): its owner
        // crashed long ago. (The pre-fix helper seeded a fresh
        // heartbeat — the liveness-blind sweep never noticed; the
        // structural exclusion does.)
        sqlx::query(
            "INSERT INTO log_ingest_sessions \
                 (exec_id, session_id, replica_pod, started_at, heartbeat_at) \
             VALUES ($1, $2, 'store-test', \
                     now() - make_interval(secs => $3), \
                     now() - make_interval(secs => $3))",
        )
        .bind(exec_id)
        .bind(session_id)
        .bind(SESSION_REAP_GRACE_SECS + 60.0)
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

    /// W11-N (merged_bug_071). Proposition: **a live session is
    /// structurally unsweepable** — at the bug's own configuration:
    /// minimum legal retention with a near-deadline-cap build still
    /// streaming. Pre-fix the victim SELECT filtered on started_at age
    /// alone; with log_retention_days = 1 (86,400 s == the scheduler's
    /// deadline cap exactly — zero margin), a near-cap build was
    /// mid-stream when the hourly sweep fired: permanent interior log
    /// loss plus a mid-session Lost-latch lease kill, recurring
    /// hourly. The exclusion is structural — NOT(open attempt ∨
    /// session-within-grace) — never an assumed magnitude ordering
    /// between independently tunable constants; the config-validation
    /// floor (config.rs) refuses the zero-margin retention outright as
    /// the other half of the close.
    #[tokio::test]
    async fn live_near_cap_session_is_structurally_unsweepable() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();

        // A RUNNING execution 1.5 days old (legal under the 1-day
        // deadline-cap clock skewed by queue time; past the
        // retention=1d horizon) with a LIVE session actively cutting.
        let exec = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO drv_executions \
                 (exec_id, drv_hash, executor_id, started_at, status) \
             VALUES ($1, $2, 'builder-0', now() - make_interval(secs => $3), NULL)",
        )
        .bind(exec)
        .bind(DRV_HASH_32)
        .bind(1.5 * 86_400.0)
        .execute(&db.pool)
        .await
        .unwrap();
        let session = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO log_ingest_sessions (exec_id, session_id, replica_pod) \
             VALUES ($1, $2, 'store-live')",
        )
        .bind(exec)
        .bind(session)
        .execute(&db.pool)
        .await
        .unwrap();
        let key = log_chunk_key(DRV_HASH_32, &exec, &session, 0);
        store.put(&key, b"mid-stream chunk".to_vec()).await.unwrap();
        sqlx::query(
            "INSERT INTO drv_log_chunks \
                 (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, s3_key) \
             VALUES ($1, $2, 0, 0, 5, 16, $3)",
        )
        .bind(exec)
        .bind(session)
        .bind(&key)
        .execute(&db.pool)
        .await
        .unwrap();

        // The hourly sweep fires at retention = 1 day.
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(86_400),
            SWEEP_BATCH,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();

        assert_eq!(
            (stats.executions_swept, counts(&db.pool, exec).await),
            (0, (1, 1, 1)),
            "left (pre-fix): the age-only victim SELECT deletes the live \
             stream's chunk rows, objects, and lease row mid-session — \
             permanent interior log loss + a Lost-latch kill, recurring \
             hourly / right: an open attempt with a live session is \
             structurally outside the victim predicate"
        );
        assert_eq!(store.len(), 1, "the chunk object survives");

        // The terminal-but-graced cell: a finished execution whose
        // session row is still inside the reap grace (the stream
        // closed seconds ago) is also protected — the sibling reap's
        // discipline, not a special case.
        sqlx::query("UPDATE drv_executions SET status = 'succeeded' WHERE exec_id = $1")
            .bind(exec)
            .execute(&db.pool)
            .await
            .unwrap();
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(86_400),
            SWEEP_BATCH,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(
            stats.executions_swept, 0,
            "a session row inside the grace still shields its execution"
        );

        // The convergence cell: once the session row ages past the
        // grace, the same execution sweeps normally — the exclusion is
        // a grace, not immortality (the exit edge is the sibling
        // reap's own clock).
        sqlx::query(
            "UPDATE log_ingest_sessions \
             SET heartbeat_at = now() - make_interval(secs => $2) \
             WHERE exec_id = $1",
        )
        .bind(exec)
        .bind(SESSION_REAP_GRACE_SECS + 60.0)
        .execute(&db.pool)
        .await
        .unwrap();
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(86_400),
            SWEEP_BATCH,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(
            (stats.executions_swept, counts(&db.pool, exec).await),
            (1, (1, 0, 0)),
            "past the grace the sweep proceeds — bounded exclusion, not a leak"
        );
    }

    /// W11-O (bug_104). Proposition: **executions_swept is exact under
    /// replica overlap** — the count derives from the DB's
    /// partitioning primitive (FOR UPDATE SKIP LOCKED, the drain
    /// idiom), not from selection-side reasoning that only holds for
    /// sequential passes. The test plays replica A by holding FOR
    /// UPDATE locks on the candidate rows in an open transaction, then
    /// runs the production sweep as replica B: pre-fix B's bare SELECT
    /// ignored A's locks, double-selected the same batch, and both
    /// replicas accumulated it (the :149 comment claimed "exact" with
    /// nothing enforcing it); post-fix B skips A's rows entirely and
    /// a later pass (after A releases) sweeps exactly once.
    /// W12-H (merged_bug_034, red-first): the sweep's exclusions must
    /// hold at EACH destructive statement's snapshot, not only at the
    /// candidate SELECT. Under READ COMMITTED every statement gets a
    /// fresh snapshot, and `FOR UPDATE OF e` locks only
    /// drv_executions — so a session committed AFTER the SELECT
    /// evaluated its exclusion (the just-admitted late replay:
    /// `accepts_terminal_but_incomplete_execution` keeps the
    /// sweepable-AND-openable population legal) sat exposed to the
    /// un-qualified statements. The fixture parks the sweep at the
    /// chunk DELETE on row locks, commits a live session into the
    /// statement gap (the steal arm refreshes the dead row), releases,
    /// and asserts the live lease survived. Pre-fix RED: the session
    /// DELETE killed the fresh lease (a transient Lost-abort of a
    /// just-admitted replay) and the chunk DELETE erased manifest rows
    /// a concurrently-opened gate read as durable.
    #[tokio::test]
    async fn sweep_statement_gap_spares_a_late_admitted_session() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = std::sync::Arc::new(MemoryLogChunkStore::default());
        let exec = Uuid::now_v7();
        seed_aged_execution(&db.pool, &store, exec, 31.0, 2).await;

        // Park the sweep at its chunk DELETE: row-lock the chunk rows
        // from an open transaction (the candidate SELECT locks only
        // drv_executions, so it runs; the DELETE then waits on us).
        let lock_pool = db.reopen().await;
        let mut tx = lock_pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM drv_log_chunks WHERE exec_id = $1 FOR UPDATE")
            .bind(exec)
            .fetch_all(&mut *tx)
            .await
            .unwrap();

        let sweep_pool = db.pool.clone();
        let sweep_store = std::sync::Arc::clone(&store);
        let sweep = tokio::spawn(async move {
            sweep_expired_logs(
                &sweep_pool,
                &*sweep_store,
                Duration::from_secs(30 * 86_400),
                SWEEP_BATCH,
            )
            .await
        });

        // Ordering proof, not a sleep: wait until the sweep is
        // OBSERVABLY blocked at the chunk DELETE (pg_stat_activity
        // shows the waiting statement), so the candidate SELECT has
        // provably evaluated its exclusion already.
        let mut parked = false;
        for _ in 0..200 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE wait_event_type = 'Lock' \
                   AND query LIKE 'DELETE FROM drv_log_chunks%'",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            if waiting > 0 {
                parked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(parked, "the sweep must be parked at its chunk DELETE");

        // The statement gap: a live session is admitted NOW — the
        // production steal arm refreshes the dead row to a fresh
        // lease (the late replay's open).
        let fresh_session = Uuid::now_v7();
        match crate::logs::sessions::acquire(&db.pool, exec, fresh_session, "late-pod")
            .await
            .unwrap()
        {
            crate::logs::sessions::Acquire::Acquired => {}
            other => panic!("the late replay must acquire the stale row, got {other:?}"),
        }

        // Release the park; the sweep finishes its transaction.
        drop(tx);
        sweep.await.unwrap().unwrap();

        // The law, at each statement's snapshot: the live lease
        // survives the sweep (pre-fix RED: the un-qualified session
        // DELETE killed it — the row was gone and the next heartbeat
        // observed Lost).
        assert!(
            matches!(
                crate::logs::sessions::lookup_live(&db.pool, exec)
                    .await
                    .unwrap(),
                crate::logs::sessions::LiveLookup::Live(_)
            ),
            "a session committed into the sweep's statement gap must survive \
             every destructive statement (the exclusion travels per statement)"
        );
    }

    #[tokio::test]
    async fn overlapping_replicas_sweep_disjoint_batches() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        seed_aged_execution(&db.pool, &store, exec, 31.0, 2).await;

        // Replica A: mid-pass, holding the candidate row locks.
        let mut replica_a = db.pool.begin().await.unwrap();
        let locked: Vec<Uuid> =
            sqlx::query_scalar("SELECT exec_id FROM drv_executions WHERE exec_id = $1 FOR UPDATE")
                .bind(exec)
                .fetch_all(&mut *replica_a)
                .await
                .unwrap();
        assert_eq!(locked.len(), 1);

        // Replica B: the production sweep, concurrent with A.
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            SWEEP_BATCH,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(
            stats.executions_swept, 0,
            "left (pre-fix): the bare candidate SELECT ignores replica A's \
             locks — both replicas select, delete, and count the same batch \
             (executions_swept double-accumulates across the fleet) / \
             right: SKIP LOCKED partitions the candidates; B claims nothing \
             A holds"
        );

        // A releases; the next pass sweeps exactly once.
        replica_a.rollback().await.unwrap();
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            SWEEP_BATCH,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(
            stats,
            SweepStats {
                executions_swept: 1,
                chunks: 2,
                objects: 2,
            },
            "each expired execution is swept and counted exactly once"
        );
    }

    // r[verify store.log.sweep-ownership+2]
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
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
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
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
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

    /// bug_234: the stale-session reap deletes rows whose owner died
    /// uncleanly (heartbeat past the grace) and ONLY those — a live
    /// session and a stale-but-within-grace session both survive (the
    /// latter is already invisible to every liveness consumer; the
    /// grace exists so this reap can never race a heartbeat).
    #[tokio::test]
    async fn stale_session_reap_respects_grace() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        for (pod, hb_age_secs) in [
            ("store-live", 0.0),
            ("store-stale-in-grace", 120.0),
            ("store-dead", SESSION_REAP_GRACE_SECS + 100.0),
        ] {
            sqlx::query(
                "INSERT INTO log_ingest_sessions \
                     (exec_id, session_id, replica_pod, heartbeat_at) \
                 VALUES ($1, $2, $3, now() - make_interval(secs => $4))",
            )
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .bind(pod)
            .bind(hb_age_secs)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        let reaped = sweep_stale_sessions(&db.pool).await.unwrap();
        assert_eq!(reaped, 1, "only the past-grace row is reaped");

        let survivors: Vec<String> =
            sqlx::query_scalar("SELECT replica_pod FROM log_ingest_sessions ORDER BY replica_pod")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(survivors, vec!["store-live", "store-stale-in-grace"]);
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
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
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
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            2,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(stats.executions_swept, 3);
        assert_eq!(stats.chunks, 3);
        assert!(store.is_empty());
    }

    // r[verify store.gc.batch-authority]
    /// W12-O2 (merged_bug_006): a global hold landing MID-PASS —
    /// between two committed batches of the log TTL sweep's
    /// until-short loop — stops the NEXT batch at its boundary. The
    /// wave-10 spawn discarded the lane clearance
    /// (`move |_clearance|`), so pre-fix the loop never re-authorized
    /// and a hold could not stop batches 2..N: committed PG deletes
    /// plus irreversible post-commit S3 object deletes kept running
    /// through the freeze — during an incident, build-log evidence
    /// is exactly what the hold preserves.
    ///
    /// Schedule: three expired executions, batch = 1 (three
    /// batches); the interpose lands a GLOBAL hold through the
    /// production `set_hold` statement immediately after batch 1
    /// commits. Post-fix: exactly one execution's artifacts are
    /// swept; the other two survive (rows AND objects); release +
    /// rerun drains the remainder.
    #[tokio::test]
    async fn mid_pass_hold_stops_log_sweep_at_the_batch_boundary() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let mut execs = Vec::new();
        for _ in 0..3 {
            let id = Uuid::now_v7();
            seed_aged_execution(&db.pool, &store, id, 31.0, 1).await;
            execs.push(id);
        }

        SWEEP_HOLD_AFTER_BATCHES.store(1, std::sync::atomic::Ordering::SeqCst);
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            1,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(
            stats.executions_swept, 1,
            "left: post-hold batches kept deleting build-log evidence \
             through the freeze / right: exactly the pre-hold batch is \
             swept; batch 2 refuses at its boundary"
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_log_chunks")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rows, 2, "two executions' manifest rows survive the freeze");
        assert!(
            !store.is_empty(),
            "the surviving executions' objects are untouched"
        );

        // The heal edge: release the hold; the next pass (fresh tick
        // clearance) drains the remainder.
        let hold_id: uuid::Uuid = sqlx::query_scalar(
            "SELECT hold_id FROM gc_holds WHERE created_by = 'log-sweep-test-hook'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(
            crate::gc::hold::release_hold(&db.pool, hold_id)
                .await
                .unwrap()
        );
        let stats = sweep_expired_logs(
            &db.pool,
            &store,
            Duration::from_secs(30 * 86_400),
            1,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(stats.executions_swept, 2, "release ⇒ the remainder sweeps");
        assert!(store.is_empty(), "all expired objects eventually deleted");
    }

    // r[verify store.gc.batch-authority]
    /// W12-P (the R32 named-type witness, stated at its honest tier):
    /// the lawful path to a batch runs authorize → token → sink, and
    /// the refusal arms structurally CANNOT yield a token — this
    /// match is the type-level proof (a `Held`/`Expired` arm has no
    /// `BatchAuthority` to produce; the compiler enforces what the
    /// wave-11 unit variant left advisory). The population face — no
    /// destructive loop ships WITHOUT the demand — is the commit-2
    /// census's claim, not this test's.
    #[tokio::test]
    async fn refusal_arms_cannot_mint_batch_authority() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let mut clearance = crate::test_helpers::gc_clearance(&db.pool).await;

        // Authorized: the one mint flows to the sink and is spent.
        match clearance.authorize_batch(&db.pool).await.unwrap() {
            crate::gc::hold::BatchAuthorize::Authorized(authority) => authority.spend(),
            refused => panic!("fresh clearance refused: {refused:?}"),
        }

        // Held: no token exists in this arm — the emergency stop's
        // type-level face (constructing one here would not compile).
        let hold_id = crate::gc::hold::set_hold(
            &db.pool,
            crate::gc::hold::GcHoldScope::Global,
            "w12-p refusal face",
            "test",
            None,
        )
        .await
        .unwrap();
        match clearance.authorize_batch(&db.pool).await.unwrap() {
            crate::gc::hold::BatchAuthorize::Held(h) => {
                assert_eq!(h.created_by, "test");
            }
            other => panic!("expected Held under an active global hold, got {other:?}"),
        }
        assert!(
            crate::gc::hold::release_hold(&db.pool, hold_id)
                .await
                .unwrap()
        );
    }
}
