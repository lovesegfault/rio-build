//! Lazy chunk collector: liveness derived from the durable manifests.
//!
//! A collect cycle derives the set of live chunk hashes from every
//! existing manifest's `manifest_data.chunk_list` (any status — an
//! `'uploading'` placeholder counts exactly as its write-ahead upsert
//! counts it today) inside PostgreSQL, then reports — and, in a later
//! release, soft-deletes + enqueues — the chunks that no manifest
//! references and that are older than the grace window measured from
//! `GREATEST(created_at, last_referenced_at)`.
//!
//! This release ships the cycle in **shadow mode only**
//! (`CollectMode::Shadow`): mark + report, no `UPDATE` anywhere. The
//! shadow numbers (mark-set size, would-collect, the refcount drift
//! pair, cycle duration) are the production calibration the cutover
//! decision needs; the collecting arm is a separate release.
//!
//! # Fail-closed mark
//!
//! The mark phase is all-or-nothing: a validation pass checks every
//! joined `chunk_list` (version byte, entry alignment, chunk-count
//! bound) and ANY violation aborts the cycle — counted by
//! `rio_store_gc_collect_parse_failures_total`, offending
//! `store_path_hash` logged at error level, no verdict produced.
//! Treating corrupt input as "references nothing" would turn a storage
//! leak into collected live data, which is the polarity the design
//! forbids; the legacy decrement paths' warn-and-skip behavior
//! (`super::parse_unique_chunk_hashes`) is exactly what the collector
//! must NOT do.
//!
//! # Capped collect (scaffolding)
//!
//! The live arm collects at most [`COLLECT_CYCLE_VICTIM_CAP`] victims
//! per cycle and carries a process-local keyset cursor across cycles
//! ([`CollectCursor`]) so a backlog drains across cycles instead of
//! stretching one cycle past the GC-lock-held budget. This module
//! lands the constant, the cursor plumbing, and the drain-visibility
//! metrics; the shadow arm never reaches them.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tracing::{error, info, instrument, warn};

/// Per-cycle victim cap for the live collect arm (design §4.1 step 3).
///
/// Derived from measurement, not chosen: the combined lock-held budget
/// for one cycle is five minutes; measured mark + prepare at the
/// 1.5 M-path design point leaves ~46 s of collect headroom; measured
/// all-in collect cost is ~37 µs/victim (~27 k victims/s in 10,000-row
/// batches); a 2× safety factor on the per-victim cost (per-batch
/// outbox enqueue outside the bench window, production degradation vs
/// the tmpfs/fsync-off bench, density/batch variance) and rounding
/// down gives 500_000 — exactly 50 batches at the existing
/// `LIMIT 10_000`. Figures and the gate verdicts are recorded in
/// `docs/spec/models/refcount-invariant-map.md` ("Phase 1a
/// measurements and adjudications", T-1a.1b / T-1a.1c).
///
/// A cycle that stops at the cap leaves the remainder for the next
/// cycle (run_gc phase 3 or the daily backstop) via [`CollectCursor`];
/// stopping early only retains garbage longer, never collects more.
/// Not a config field (rollback is by release); changing it is a
/// one-line, measurement-justified edit.
#[cfg(not(test))]
pub const COLLECT_CYCLE_VICTIM_CAP: u64 = 500_000;
/// Test override: small enough that cap/cursor structural tests can
/// exercise a multi-cycle drain on a few dozen seeded rows.
#[cfg(test)]
pub const COLLECT_CYCLE_VICTIM_CAP: u64 = 50;

/// Daily backstop cadence for the collect cycle. The collect also runs
/// as phase 3 of every `run_gc`; the backstop covers stores that never
/// trigger GC so bounded garbage retention has a worst-case clock
/// (24 h + grace + drain lag once the live arm is enabled).
///
/// The backstop's first tick fires one full interval after spawn —
/// process boot deliberately does NOT trigger a cycle (see
/// [`spawn_collect_backstop`]).
#[cfg(not(test))]
pub(crate) const COLLECT_BACKSTOP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Test override: long enough that the no-cycle-at-boot assertion has
/// real-time margin under CI load, short enough that the
/// tick-after-one-interval half completes quickly.
#[cfg(test)]
pub(crate) const COLLECT_BACKSTOP_INTERVAL: Duration = Duration::from_secs(1);

/// `work_mem` for the mark expansion's set-based dedup (and
/// `maintenance_work_mem` for the one-time index build on the mark
/// product). The expansion's `SELECT DISTINCT` hash-aggregates the
/// expanded reference stream and spills to temp files past this bound,
/// so the value states the cycle's server-memory envelope explicitly
/// instead of inheriting whatever the cluster default happens to be.
/// 4GB matches the measured configuration of the gate-(b)/(c) bench
/// records (the 1.5 M-path design point spilled ~10 GB to temp files
/// under it — bounded, disk-backed, not resident). Applied with
/// `SET LOCAL` inside the cycle's transaction, so the budget is
/// transaction-scoped: it reverts on commit, rollback, and cancellation
/// alike and can never leak into the shared pool that serves normal
/// store traffic. One cycle runs at a time (GC advisory lock), so the
/// budget is never multiplied across concurrent cycles either.
const COLLECT_WORK_MEM: &str = "4GB";

/// The server-side mark statement: expand every `chunk_list` joined to
/// an existing manifests row (any status) inside the server and
/// deduplicate with one set-based aggregate into a session temp table.
/// The `OFFSET 0` lateral materializes, once per manifest row, a
/// detoasted copy of the entry block (version byte dropped) plus the
/// entry count, so the per-entry `substring` slices an in-memory value
/// instead of re-fetching TOAST data per entry. Entry `i` (1-based)
/// starts at body offset `36·i − 35`.
///
/// Shared with `gc::mark_scan_bench` so the bench's EXPLAIN plan-shape
/// guard (design §5b gate (b)) exercises this exact statement, not a
/// copy.
pub(crate) const MARK_EXPANSION_SQL: &str = "CREATE TEMP TABLE live_chunks AS \
     SELECT DISTINCT substring(d.body FROM 36 * g.i - 35 FOR 32) AS blake3_hash \
       FROM manifest_data md \
       JOIN manifests m USING (store_path_hash) \
       CROSS JOIN LATERAL \
         (SELECT substring(md.chunk_list FROM 2) AS body, \
                 (octet_length(md.chunk_list) - 1) / 36 AS n \
          OFFSET 0) AS d \
       CROSS JOIN LATERAL \
         generate_series(1, d.n) AS g(i)";

/// The fail-closed validation pass over every `chunk_list` joined to an
/// existing manifests row. Mirrors the acceptance criteria of
/// [`super::try_parse_unique_chunk_hashes`] (the Rust parser remains
/// the definition of corrupt-vs-valid; the differential pinning test
/// holds the SQL expansion to it): version byte, 36-byte entry
/// alignment, `MAX_CHUNKS`. The version probe uses `substring` (empty
/// on an empty blob) rather than `get_byte` so a zero-length blob is
/// reported as malformed instead of erroring mid-scan. 36 = the
/// serialized entry size (32-byte BLAKE3 + u32 LE size), `\x01` = the
/// format version byte; both fixed by `rio_store::manifest`.
///
/// Shared with `gc::mark_scan_bench` (gate (b)).
pub(crate) fn mark_validation_sql() -> String {
    format!(
        "SELECT COUNT(*) \
           FROM manifest_data md \
           JOIN manifests m USING (store_path_hash) \
          WHERE octet_length(md.chunk_list) < 1 \
             OR substring(md.chunk_list FROM 1 FOR 1) <> '\\x01'::bytea \
             OR (octet_length(md.chunk_list) - 1) % 36 <> 0 \
             OR (octet_length(md.chunk_list) - 1) / 36 > {}",
        crate::manifest::MAX_CHUNKS
    )
}

/// The offending `store_path_hash`es behind a failed validation pass —
/// fetched only on the abort path (the happy path never pays for it),
/// hex-encoded for the error log and the runbook's lookup query.
fn mark_validation_offenders_sql() -> String {
    format!(
        "SELECT encode(md.store_path_hash, 'hex') \
           FROM manifest_data md \
           JOIN manifests m USING (store_path_hash) \
          WHERE octet_length(md.chunk_list) < 1 \
             OR substring(md.chunk_list FROM 1 FOR 1) <> '\\x01'::bytea \
             OR (octet_length(md.chunk_list) - 1) % 36 <> 0 \
             OR (octet_length(md.chunk_list) - 1) / 36 > {} \
          ORDER BY md.store_path_hash \
          LIMIT 10",
        crate::manifest::MAX_CHUNKS
    )
}

/// Which arm of the collector runs. This release ships only
/// `CollectMode::Shadow`; the live (soft-delete + enqueue) arm is
/// added by the cutover release — deliberately not pre-built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectMode {
    /// Mark + report only. No row anywhere is modified.
    Shadow,
}

/// Terminal state of one collect cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectOutcome {
    /// The cycle completed and produced a report.
    Ok,
    /// The fail-closed validation pass found at least one unparseable
    /// `chunk_list`; the cycle aborted without producing any verdict.
    ParseFailure,
}

/// What one collect cycle observed (and, for the live arm, did).
#[derive(Debug, Clone)]
pub(crate) struct CollectReport {
    pub(crate) outcome: CollectOutcome,
    /// Distinct chunk hashes referenced by at least one existing
    /// manifest at the cycle snapshot (the mark-set size).
    pub(crate) mark_set_size: u64,
    /// Chunks eligible for collection at the snapshot: not deleted,
    /// absent from the mark set, older than grace measured from
    /// `GREATEST(created_at, last_referenced_at)`.
    pub(crate) would_collect: u64,
    /// Drift, leak direction: `refcount > 0`, unmarked, not deleted.
    pub(crate) drift_leaked: u64,
    /// Drift, under-count direction: marked live, `refcount = 0`, not
    /// deleted. Never expected while the increment still fires.
    pub(crate) drift_undercount: u64,
    /// Victims soft-deleted this cycle. Always 0 in shadow mode.
    pub(crate) victims_collected: u64,
    /// Soft-delete batches run this cycle. Always 0 in shadow mode.
    pub(crate) batches_run: u64,
    /// True when the cycle stopped at [`COLLECT_CYCLE_VICTIM_CAP`]
    /// with backlog remaining. Always false in shadow mode.
    pub(crate) cap_reached: bool,
    /// Keyset cursor at the stop point (None when the pass completed
    /// or in shadow mode).
    pub(crate) cursor_at_stop: Option<Vec<u8>>,
    /// Wall-clock of the cycle (snapshot through report).
    pub(crate) cycle_seconds: f64,
}

/// Process-local keyset cursor carried across capped collect cycles
/// (the collector's only long-lived state). Reset when a pass
/// completes; never persisted — losing it on restart costs at most a
/// cheap re-scan of already-collected keyspace (the candidate scan's
/// `deleted = FALSE` conjunct skips collected rows) and never stalls
/// the drain.
pub struct CollectCursor(Mutex<Option<Vec<u8>>>);

impl CollectCursor {
    /// A cursor at the start of the keyspace.
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Cursor to resume the next cycle's candidate scan from (`None` ⇒
    /// start of keyspace).
    pub fn get(&self) -> Option<Vec<u8>> {
        self.0.lock().expect("collect cursor poisoned").clone()
    }

    /// Persist the stop point of a capped cycle.
    pub fn store(&self, cursor: Vec<u8>) {
        *self.0.lock().expect("collect cursor poisoned") = Some(cursor);
    }

    /// A pass completed: the next cycle starts from the beginning.
    pub fn reset(&self) {
        *self.0.lock().expect("collect cursor poisoned") = None;
    }
}

impl Default for CollectCursor {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-global collector cursor (see [`CollectCursor`]). The
/// shadow arm never touches it; the live arm resumes capped cycles
/// from it and resets it when a pass completes.
pub static COLLECT_CURSOR: CollectCursor = CollectCursor::new();

/// One collect cycle (design §4.1): snapshot → fail-closed mark →
/// prepare → report (shadow) — the live soft-delete arm is a later
/// release. Uses its own pooled connection for the session temp table;
/// callers hold [`super::GC_LOCK_ID`] on a different connection
/// (run_gc phase 3, [`collect_backstop_once`]).
///
/// The whole read phase — cutoff, fail-closed validation, mark
/// expansion, prepare, and the shadow report — runs in one
/// REPEATABLE READ transaction, i.e. on one MVCC snapshot. That makes
/// the [`CollectReport`] "at the cycle snapshot" semantics true as
/// written: the validation pass and the expansion see the same manifest
/// set (no TOCTOU inside the fail-closed guarantee), and uploads or
/// PutPath rollbacks that commit during the multi-minute mark→report
/// window cannot surface in the drift gauges — a nonzero drift reading
/// is real refcount drift, not cycle-concurrent traffic. The
/// transaction also scopes the cycle's session state (`SET LOCAL`
/// memory budget, `ON COMMIT DROP` temp table) so nothing outlives the
/// cycle on any exit path. The snapshot (and with it the xmin horizon)
/// is held for the full mark+prepare+report span — the same order the
/// expansion statement alone already held — and takes no locks that
/// block writers; acceptable at the once-per-GC + daily-backstop
/// cadence (measured spans: invariant map T-1a.1b/T-1a.1c records).
///
/// The cycle-duration histogram records completed cycles only; an
/// aborted (parse-failure) cycle is counted by its own counter and the
/// `outcome="parse_failure"` cycle counter instead.
#[instrument(skip(pool))]
pub(crate) async fn collect_cycle(
    pool: &PgPool,
    grace_secs: i64,
    mode: CollectMode,
) -> Result<CollectReport, sqlx::Error> {
    // Single-variant today; the binding documents that the live arm
    // must thread `mode` through rather than fork the function.
    let CollectMode::Shadow = mode;

    let cycle_started = Instant::now();
    let mut conn = pool.acquire().await?;

    // One cycle = one transaction = one MVCC snapshot (see the function
    // doc). Not READ ONLY: PostgreSQL forbids CREATE TEMP TABLE in
    // read-only transactions. Every early `?` return drops the
    // Transaction, which queues a ROLLBACK — the SET LOCAL budget and
    // the ON COMMIT DROP temp table below die with it on every exit
    // path, so nothing leaks back into the shared pool.
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;

    // Transaction-scoped memory bounds for the expansion + index build;
    // see COLLECT_WORK_MEM. AssertSqlSafe: interpolates only that const.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL work_mem = '{COLLECT_WORK_MEM}'"
    )))
    .execute(&mut *tx)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL maintenance_work_mem = '{COLLECT_WORK_MEM}'"
    )))
    .execute(&mut *tx)
    .await?;

    // Defensive, no longer load-bearing: the ON COMMIT DROP expansion
    // below cannot leave the temp table behind, but a session poisoned
    // by a binary predating the transaction-scoped cycle may still
    // carry one (same rationale as the sweep's setup_sweep_unreachable).
    sqlx::query("DROP TABLE IF EXISTS live_chunks")
        .execute(&mut *tx)
        .await?;

    // --- Snapshot ---
    // The eligibility cutoff is anchored at the cycle snapshot on the
    // DB clock (the predicate compares against DB-written timestamps),
    // never re-evaluated per batch. now() is the cycle transaction's
    // start time — the snapshot anchor the design's §4.1 derivation
    // names cycle_started_at.
    let cutoff: String = sqlx::query_scalar("SELECT (now() - make_interval(secs => $1))::text")
        .bind(grace_secs)
        .fetch_one(&mut *tx)
        .await?;

    // --- Mark (i): fail-closed validation pass ---
    let malformed: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_sql()))
        .fetch_one(&mut *tx)
        .await?;
    if malformed > 0 {
        let offenders: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_offenders_sql()))
                .fetch_all(&mut *tx)
                .await?;
        error!(
            malformed,
            offenders = %offenders.join(","),
            "chunk-collect: unparseable chunk_list, aborting the cycle (fail-closed); \
             chunk collection is suspended until the manifest is repaired, deleted, or quarantined"
        );
        metrics::counter!("rio_store_gc_collect_parse_failures_total").increment(1);
        metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "parse_failure")
            .increment(1);
        // Dropping `tx` rolls the cycle transaction back; the abort
        // wrote nothing and leaves nothing on the session.
        return Ok(CollectReport {
            outcome: CollectOutcome::ParseFailure,
            mark_set_size: 0,
            would_collect: 0,
            drift_leaked: 0,
            drift_undercount: 0,
            victims_collected: 0,
            batches_run: 0,
            cap_reached: false,
            cursor_at_stop: None,
            cycle_seconds: cycle_started.elapsed().as_secs_f64(),
        });
    }

    // --- Mark (ii): server-side set-based expansion ---
    // The cycle runs the shared statement with ON COMMIT DROP added, so
    // the mark product cannot outlive the cycle transaction on any exit
    // path. The shared constant stays free of the clause: the bench's
    // EXPLAIN plan-shape guard (gate (b)) runs it outside a transaction,
    // where ON COMMIT DROP would drop the table at statement end.
    // AssertSqlSafe: splices only the shared constant.
    let expansion_on_commit_drop = format!(
        "CREATE TEMP TABLE live_chunks ON COMMIT DROP AS {}",
        MARK_EXPANSION_SQL
            .strip_prefix("CREATE TEMP TABLE live_chunks AS ")
            .expect("MARK_EXPANSION_SQL starts with the live_chunks CTAS prefix")
    );
    sqlx::query(sqlx::AssertSqlSafe(expansion_on_commit_drop))
        .execute(&mut *tx)
        .await?;

    // --- Prepare: unique index + stats on the mark product, so the
    // anti-join probes an index instead of hashing/sorting the whole
    // mark set per query (the same step the live arm's batches need).
    sqlx::query("CREATE UNIQUE INDEX live_chunks_hash_idx ON live_chunks (blake3_hash)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("ANALYZE live_chunks").execute(&mut *tx).await?;

    let mark_set_size: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks")
        .fetch_one(&mut *tx)
        .await?;

    // --- Shadow report: one pass over the not-deleted chunk rows ---
    // would-collect: unmarked AND past grace (the collect predicate);
    // drift, leak direction: refcount > 0 AND unmarked;
    // drift, under-count direction: refcount = 0 AND marked.
    let (would_collect, drift_leaked, drift_undercount): (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           COUNT(*) FILTER (WHERE NOT in_mark \
                              AND GREATEST(created_at, last_referenced_at) < $1::timestamptz), \
           COUNT(*) FILTER (WHERE refcount > 0 AND NOT in_mark), \
           COUNT(*) FILTER (WHERE refcount = 0 AND in_mark) \
         FROM (SELECT c.refcount, c.created_at, c.last_referenced_at, \
                      EXISTS (SELECT 1 FROM live_chunks lc \
                               WHERE lc.blake3_hash = c.blake3_hash) AS in_mark \
                 FROM chunks c \
                WHERE c.deleted = FALSE) AS s",
    )
    .bind(&cutoff)
    .fetch_one(&mut *tx)
    .await?;

    // Commit ends the cycle's snapshot and drops the temp table (ON
    // COMMIT DROP) before the connection returns to the pool.
    tx.commit().await?;

    let cycle_seconds = cycle_started.elapsed().as_secs_f64();

    metrics::gauge!("rio_store_gc_chunks_live").set(mark_set_size as f64);
    metrics::gauge!("rio_store_gc_chunks_would_collect").set(would_collect as f64);
    metrics::gauge!("rio_store_gc_refcount_drift_leaked").set(drift_leaked as f64);
    metrics::gauge!("rio_store_gc_refcount_drift_undercount").set(drift_undercount as f64);
    // Shadow mode: the backlog estimate IS the would-collect count.
    // The live arm maintains it decrementally from its own collected
    // counts instead of re-running this full anti-join every cycle.
    metrics::gauge!("rio_store_gc_collect_backlog_chunks").set(would_collect as f64);
    metrics::histogram!("rio_store_gc_collect_cycle_seconds").record(cycle_seconds);
    metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "ok").increment(1);

    info!(
        mark_set_size,
        would_collect,
        drift_leaked,
        drift_undercount,
        cycle_seconds,
        "chunk-collect shadow cycle complete"
    );

    Ok(CollectReport {
        outcome: CollectOutcome::Ok,
        mark_set_size: mark_set_size as u64,
        would_collect: would_collect as u64,
        drift_leaked: drift_leaked as u64,
        drift_undercount: drift_undercount as u64,
        victims_collected: 0,
        batches_run: 0,
        cap_reached: false,
        cursor_at_stop: None,
        cycle_seconds,
    })
}

/// Run one backstop-triggered collect cycle if no GC (or other
/// backstop tick) is in flight: takes [`super::GC_LOCK_ID`]
/// non-blocking and skips when held. Returns `Ok(None)` on skip.
/// Split out from [`spawn_collect_backstop`] so tests drive it
/// directly.
pub(crate) async fn collect_backstop_once(
    pool: &PgPool,
    grace_secs: i64,
) -> Result<Option<CollectReport>, sqlx::Error> {
    let mut lock_conn = pool.acquire().await?;
    let lock_acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(super::GC_LOCK_ID)
        .fetch_one(&mut *lock_conn)
        .await?;
    if !lock_acquired {
        info!("chunk-collect backstop: GC already running, skipping this tick");
        return Ok(None);
    }
    // Same choreography as run_gc: any exit that does not go through
    // the explicit unlock below detaches the connection, closing it so
    // PG releases the session-scoped lock instead of leaking it back
    // into the pool with the lock held.
    let lock_conn = scopeguard::guard(lock_conn, |c| {
        let _ = c.detach();
    });

    let result = collect_cycle(pool, grace_secs, CollectMode::Shadow).await;
    if result.is_err() {
        // A DB-error cycle would otherwise be invisible to metrics for
        // up to a full backstop interval (the parse-failure abort
        // carries its own outcome and is NOT an error here). run_gc
        // phase 3 counts its own failures, so the outcomes partition.
        metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "error").increment(1);
    }

    let mut lock_conn = scopeguard::ScopeGuard::into_inner(lock_conn);
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(super::GC_LOCK_ID)
        .execute(&mut *lock_conn)
        .await
    {
        warn!(error = %e, "chunk-collect backstop: advisory unlock failed");
    }

    result.map(Some)
}

/// Spawn the daily collect backstop. Errors are logged and the next
/// tick retries (`MissedTickBehavior::Skip`, like
/// [`super::sweep::spawn_orphan_chunk_sweep`]).
///
/// Unlike `spawn_periodic` (whose first tick fires immediately), the
/// ticker here is armed one full interval after spawn: the collect
/// cycle is the heaviest query pattern in the system (full
/// manifest_data expansion + chunks anti-join, multi-GB temp spill at
/// the design point — invariant map T-1a.1b/T-1a.1c), and rolling
/// deploys, scale-outs, and crash-loops must not trigger it on every
/// pod boot — exactly the moments the database is already under
/// stress. Each store replica arms its own daily timer; concurrent
/// ticks (and ticks during a GC run) are deduplicated by the
/// non-blocking [`super::GC_LOCK_ID`] try-lock in
/// `collect_backstop_once`, so at most one cycle runs cluster-wide
/// at a time. Accepted trade-off: a store that restarts more often
/// than once per interval gets its cycles only from `run_gc` phase 3
/// (the controller GC schedule); the `RioStoreGcCollectStalled` alert
/// is the detector for a fleet where neither trigger completes.
pub fn spawn_collect_backstop(
    pool: PgPool,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + COLLECT_BACKSTOP_INTERVAL,
        COLLECT_BACKSTOP_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    rio_common::task::spawn_periodic_with("gc-collect-backstop", ticker, shutdown, move || {
        let pool = pool.clone();
        async move {
            if let Err(e) = collect_backstop_once(&pool, super::sweep::CHUNK_GRACE_SECS).await {
                warn!(error = %e, "chunk-collect backstop failed (will retry next interval)");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::test_helpers::{ChunkSeed, StoreSeed};
    use rio_test_support::TestDb;
    use rio_test_support::metrics::CountingRecorder;
    use rstest::rstest;

    /// Serialized manifest referencing the given hashes (duplicates kept).
    fn make_chunk_list(hashes: &[[u8; 32]]) -> Vec<u8> {
        Manifest {
            entries: hashes
                .iter()
                .map(|h| ManifestEntry {
                    hash: *h,
                    size: 4096,
                })
                .collect(),
        }
        .serialize()
    }

    /// Seed a narinfo + manifests row (given status) plus a
    /// manifest_data row with the given chunk_list bytes.
    async fn seed_chunked_manifest(
        pool: &PgPool,
        name: &str,
        status: &'static str,
        chunk_list: &[u8],
    ) -> Vec<u8> {
        let hash = StoreSeed::path(name)
            .with_manifest_status(status)
            .seed(pool)
            .await;
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&hash)
            .bind(chunk_list)
            .execute(pool)
            .await
            .expect("seed manifest_data");
        hash
    }

    /// Stable digest of every chunks row (all columns), for
    /// byte-identical before/after comparisons.
    async fn chunks_table_digest(pool: &PgPool) -> String {
        sqlx::query_scalar(
            "SELECT COALESCE(md5(string_agg(c::text, '|' ORDER BY blake3_hash)), 'empty') \
               FROM chunks c",
        )
        .fetch_one(pool)
        .await
        .expect("chunks digest")
    }

    /// Mark-fold correctness: chunks referenced by 'complete' AND
    /// 'uploading' manifests are both live; only the unreferenced old
    /// chunk is would-collect; the drift gauges see the seeded shapes;
    /// and the shadow cycle modifies nothing anywhere.
    #[tokio::test]
    async fn shadow_cycle_reports_fold_and_modifies_nothing() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Chunk A: referenced by a complete manifest (refcount honest).
        let a = ChunkSeed::new(0xA1)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        // Chunk B: referenced ONLY by an 'uploading' placeholder.
        let b = ChunkSeed::new(0xB2).with_refcount(1).seed(&db.pool).await;
        // Chunk C: unreferenced, old, stale refcount > 0 (the
        // historical-leak shape) -> would-collect + drift_leaked.
        ChunkSeed::new(0xC3)
            .with_refcount(2)
            .age_secs(3600)
            .seed(&db.pool)
            .await;
        // Chunk D: referenced by the complete manifest but refcount=0
        // (the under-count shape, M_023 direction) -> drift_undercount.
        let d = ChunkSeed::new(0xD4)
            .with_refcount(0)
            .uploaded()
            .seed(&db.pool)
            .await;

        seed_chunked_manifest(
            &db.pool,
            "collect-complete",
            "complete",
            &make_chunk_list(&[a, d, a]),
        )
        .await;
        seed_chunked_manifest(
            &db.pool,
            "collect-uploading",
            "uploading",
            &make_chunk_list(&[b]),
        )
        .await;

        let before = chunks_table_digest(&db.pool).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let report = collect_cycle(
            &db.pool,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow,
        )
        .await
        .expect("shadow cycle");

        assert_eq!(report.outcome, CollectOutcome::Ok);
        assert_eq!(report.mark_set_size, 3, "a, b, d are referenced (dedup'd)");
        assert_eq!(report.would_collect, 1, "only the old unreferenced chunk");
        assert_eq!(
            report.drift_leaked, 1,
            "stale refcount>0 on the unreferenced chunk"
        );
        assert_eq!(
            report.drift_undercount, 1,
            "marked-live chunk at refcount=0"
        );
        assert_eq!(report.victims_collected, 0);
        assert_eq!(report.batches_run, 0);
        assert!(!report.cap_reached);
        assert!(report.cursor_at_stop.is_none());

        // Gauges mirror the report; the cycle counter and duration
        // histogram fire for the ok outcome.
        assert_eq!(rec.gauge_value("rio_store_gc_chunks_live{}"), Some(3.0));
        assert_eq!(
            rec.gauge_value("rio_store_gc_chunks_would_collect{}"),
            Some(1.0)
        );
        assert_eq!(
            rec.gauge_value("rio_store_gc_refcount_drift_leaked{}"),
            Some(1.0)
        );
        assert_eq!(
            rec.gauge_value("rio_store_gc_refcount_drift_undercount{}"),
            Some(1.0)
        );
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(1.0)
        );
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 1);
        assert!(rec.histogram_touched("rio_store_gc_collect_cycle_seconds"));

        // Shadow mode writes nothing: every chunks row byte-identical,
        // and nothing was enqueued.
        assert_eq!(chunks_table_digest(&db.pool).await, before);
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 0);
    }

    /// Differential pinning (the semantic half of gate (b)): the SQL
    /// expansion's live set equals the union of
    /// `try_parse_unique_chunk_hashes` over the same manifest_data rows
    /// — shared/duplicate hashes and an 'uploading' manifest included —
    /// so the SQL slicing cannot drift from Manifest::deserialize
    /// silently.
    #[tokio::test]
    async fn mark_expansion_matches_rust_parser() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let h1 = [0x11u8; 32];
        let h2 = [0x22u8; 32];
        let h3 = [0x33u8; 32];
        let h4 = [0x44u8; 32];
        // Shared (h2 in both), duplicated (h1 twice in one), and an
        // 'uploading' manifest contributing h4.
        seed_chunked_manifest(
            &db.pool,
            "diff-a",
            "complete",
            &make_chunk_list(&[h1, h2, h1, h3]),
        )
        .await;
        seed_chunked_manifest(&db.pool, "diff-b", "uploading", &make_chunk_list(&[h2, h4])).await;

        // Rust-parser union over the same rows.
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT md.chunk_list FROM manifest_data md JOIN manifests m USING (store_path_hash)",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        let mut expected: Vec<[u8; 32]> = Vec::new();
        for (blob,) in &rows {
            expected
                .extend(super::super::try_parse_unique_chunk_hashes(blob).expect("valid fixture"));
        }
        expected.sort_unstable();
        expected.dedup();
        let expected: Vec<Vec<u8>> = expected.iter().map(|h| h.to_vec()).collect();

        // SQL expansion via the shipped statements.
        let mut conn = db.pool.acquire().await.unwrap();
        let malformed: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_sql()))
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(malformed, 0);
        sqlx::query(MARK_EXPANSION_SQL)
            .execute(&mut *conn)
            .await
            .unwrap();
        let got: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT blake3_hash FROM live_chunks ORDER BY blake3_hash")
                .fetch_all(&mut *conn)
                .await
                .unwrap();

        assert_eq!(
            got, expected,
            "SQL expansion equals the try_parse_unique_chunk_hashes union"
        );
    }

    /// would-collect respects grace and the 068 touch column: an old
    /// unreferenced chunk counts; a young one does not; an old one with
    /// a recent last_referenced_at does not.
    #[tokio::test]
    async fn would_collect_respects_grace_and_touch() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Old + untouched + unreferenced -> counted.
        ChunkSeed::new(0x01).age_secs(3600).seed(&db.pool).await;
        // Young + unreferenced -> not counted.
        ChunkSeed::new(0x02).seed(&db.pool).await;
        // Old + unreferenced but freshly touched -> not counted.
        let touched = ChunkSeed::new(0x03).age_secs(3600).seed(&db.pool).await;
        sqlx::query("UPDATE chunks SET last_referenced_at = now() WHERE blake3_hash = $1")
            .bind(&touched[..])
            .execute(&db.pool)
            .await
            .unwrap();

        let report = collect_cycle(
            &db.pool,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow,
        )
        .await
        .unwrap();
        assert_eq!(report.outcome, CollectOutcome::Ok);
        assert_eq!(report.mark_set_size, 0, "no manifests seeded");
        assert_eq!(
            report.would_collect, 1,
            "only the old, untouched, unreferenced chunk"
        );
    }

    /// Fail-closed mark: any unparseable chunk_list aborts the cycle —
    /// failure counter + parse_failure outcome only, no gauges, no
    /// UPDATE anywhere — across the three corrupt classes.
    #[rstest]
    #[case::wrong_version(vec![0xFFu8, 0, 0])]
    #[case::misaligned({ let mut b = make_chunk_list(&[[0x55u8; 32]]); b.pop(); b })]
    #[case::over_max_chunks(vec![0x01u8; 1 + (crate::manifest::MAX_CHUNKS + 1) * 36])]
    #[tokio::test]
    async fn validation_failure_aborts_cycle(#[case] corrupt: Vec<u8>) {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // A valid manifest + its chunk, plus an old unreferenced chunk
        // that WOULD be reported if the cycle ran.
        let good = ChunkSeed::new(0x10)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(
            &db.pool,
            "abort-good",
            "complete",
            &make_chunk_list(&[good]),
        )
        .await;
        ChunkSeed::new(0x20).age_secs(3600).seed(&db.pool).await;
        // The corrupt manifest.
        seed_chunked_manifest(&db.pool, "abort-corrupt", "complete", &corrupt).await;

        let before = chunks_table_digest(&db.pool).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let report = collect_cycle(
            &db.pool,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow,
        )
        .await
        .expect("abort is not an error");

        assert_eq!(report.outcome, CollectOutcome::ParseFailure);
        assert_eq!(rec.get("rio_store_gc_collect_parse_failures_total{}"), 1);
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=parse_failure}"),
            1
        );
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 0);
        // No gauges for the aborted cycle.
        for g in [
            "rio_store_gc_chunks_live",
            "rio_store_gc_chunks_would_collect",
            "rio_store_gc_refcount_drift_leaked",
            "rio_store_gc_refcount_drift_undercount",
            "rio_store_gc_collect_backlog_chunks",
        ] {
            assert!(
                !rec.gauge_touched(g),
                "aborted cycle must not emit {g}; touched: {:?}",
                rec.gauge_names()
            );
        }
        assert!(!rec.histogram_touched("rio_store_gc_collect_cycle_seconds"));
        // Zero UPDATEs to chunks.
        assert_eq!(chunks_table_digest(&db.pool).await, before);
    }

    /// `SHOW <setting>` on every connection the pool can hand out (the
    /// pool is fully drained, so the cycle's connection is necessarily
    /// among them).
    async fn show_on_all_pool_connections(pool: &PgPool, setting: &str) -> Vec<String> {
        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(pool.acquire().await.expect("drain pool"));
        }
        let mut values = Vec::new();
        for conn in &mut held {
            let v: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SHOW {setting}")))
                .fetch_one(&mut **conn)
                .await
                .expect("show setting");
            values.push(v);
        }
        values
    }

    /// `to_regclass('pg_temp.live_chunks')` on every connection the
    /// pool can hand out — Some(name) on any session still carrying the
    /// cycle's temp table, None everywhere once nothing leaked.
    async fn temp_table_on_any_pool_connection(pool: &PgPool) -> Vec<Option<String>> {
        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(pool.acquire().await.expect("drain pool"));
        }
        let mut values = Vec::new();
        for conn in &mut held {
            let v: Option<String> =
                sqlx::query_scalar("SELECT to_regclass('pg_temp.live_chunks')::text")
                    .fetch_one(&mut **conn)
                    .await
                    .expect("temp-table probe");
            values.push(v);
        }
        values
    }

    /// The cycle's session state (the 4GB work_mem/maintenance_work_mem
    /// budget and the live_chunks temp table) is transaction-scoped: a
    /// completed cycle returns its pooled connection with the server
    /// defaults restored and no temp table, so the shared pool serving
    /// PutPath/GetPath traffic never inherits the cycle's memory budget.
    #[tokio::test]
    async fn cycle_leaves_no_session_state_in_pool() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h = ChunkSeed::new(0x91)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(&db.pool, "guc-leak", "complete", &make_chunk_list(&[h])).await;

        let baseline = show_on_all_pool_connections(&db.pool, "work_mem").await[0].clone();
        assert_ne!(
            baseline, COLLECT_WORK_MEM,
            "vacuity guard: the server default must differ from the cycle budget"
        );

        let report = collect_cycle(
            &db.pool,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow,
        )
        .await
        .expect("cycle");
        assert_eq!(report.outcome, CollectOutcome::Ok);

        for setting in ["work_mem", "maintenance_work_mem"] {
            for v in show_on_all_pool_connections(&db.pool, setting).await {
                assert_ne!(
                    v, COLLECT_WORK_MEM,
                    "{setting} leaked into the shared pool after a completed cycle"
                );
            }
        }
        for t in temp_table_on_any_pool_connection(&db.pool).await {
            assert!(
                t.is_none(),
                "live_chunks leaked into the shared pool after a completed cycle"
            );
        }
    }

    /// A cycle that fails mid-flight (after the mark expansion) leaves
    /// nothing behind either: the transaction rollback discards the
    /// SET LOCAL budget and the ON COMMIT DROP temp table on the same
    /// pooled connection that ran the failed cycle.
    #[tokio::test]
    async fn failed_cycle_leaves_no_session_state_in_pool() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h = ChunkSeed::new(0x92)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(&db.pool, "guc-leak-err", "complete", &make_chunk_list(&[h])).await;

        let baseline = show_on_all_pool_connections(&db.pool, "work_mem").await[0].clone();
        assert_ne!(
            baseline, COLLECT_WORK_MEM,
            "vacuity guard: the server default must differ from the cycle budget"
        );

        // Make the cycle fail AFTER the validation pass and the mark
        // expansion: the shadow report reads `chunks`, so dropping the
        // table forces the failure exactly in the mid-cycle window the
        // leak lived in.
        sqlx::query("DROP TABLE chunks CASCADE")
            .execute(&db.pool)
            .await
            .expect("drop chunks");

        let result = collect_cycle(
            &db.pool,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow,
        )
        .await;
        assert!(result.is_err(), "the report query must fail without chunks");

        for setting in ["work_mem", "maintenance_work_mem"] {
            for v in show_on_all_pool_connections(&db.pool, setting).await {
                assert_ne!(
                    v, COLLECT_WORK_MEM,
                    "{setting} leaked into the shared pool after a failed cycle"
                );
            }
        }
        for t in temp_table_on_any_pool_connection(&db.pool).await {
            assert!(
                t.is_none(),
                "live_chunks leaked into the shared pool after a failed cycle"
            );
        }
    }

    /// The session temp table never leaks across cycles: two
    /// consecutive cycles on the same pool succeed (the second would
    /// fail on CREATE TEMP TABLE if the first left it behind on the
    /// pooled connection).
    #[tokio::test]
    async fn temp_table_does_not_leak_across_cycles() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let h = ChunkSeed::new(0x77)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(&db.pool, "leak-a", "complete", &make_chunk_list(&[h])).await;

        for _ in 0..2 {
            let report = collect_cycle(
                &db.pool,
                super::super::sweep::CHUNK_GRACE_SECS,
                CollectMode::Shadow,
            )
            .await
            .expect("cycle");
            assert_eq!(report.outcome, CollectOutcome::Ok);
            assert_eq!(report.mark_set_size, 1);
        }
    }

    /// run_gc phase 3 runs the shadow cycle while the GC lock is held.
    #[tokio::test]
    async fn run_gc_phase3_runs_shadow_cycle() {
        use tokio::sync::mpsc;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let h = ChunkSeed::new(0x42)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(&db.pool, "phase3", "complete", &make_chunk_list(&[h])).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let (tx, mut rx) = mpsc::channel(8);
        let stats = super::super::run_gc(
            &db.pool,
            None,
            super::super::GcParams {
                dry_run: false,
                grace_hours: 2,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .expect("run_gc")
        .expect("lock free");
        while rx.recv().await.is_some() {}

        assert_eq!(stats.paths_deleted, 0, "fresh path is grace-protected");
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"),
            1,
            "phase 3 ran exactly one shadow cycle"
        );
        assert_eq!(rec.gauge_value("rio_store_gc_chunks_live{}"), Some(1.0));
    }

    /// The backstop skips when GC_LOCK_ID is held, runs a cycle when it
    /// is free, and releases the lock afterwards.
    #[tokio::test]
    async fn backstop_skips_when_gc_lock_held() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        // Hold the lock on a dedicated connection.
        let mut holder = db.pool.acquire().await.unwrap();
        let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(super::super::GC_LOCK_ID)
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        assert!(got);

        let skipped = collect_backstop_once(&db.pool, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .unwrap();
        assert!(
            skipped.is_none(),
            "backstop must skip while GC holds the lock"
        );
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 0);

        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(super::super::GC_LOCK_ID)
            .execute(&mut *holder)
            .await
            .unwrap();
        drop(holder);

        let ran = collect_backstop_once(&db.pool, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .unwrap();
        assert!(ran.is_some(), "backstop runs once the lock is free");
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 1);

        // And the lock was released by the backstop itself: a fresh
        // try-lock succeeds immediately.
        let mut probe = db.pool.acquire().await.unwrap();
        let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(super::super::GC_LOCK_ID)
            .fetch_one(&mut *probe)
            .await
            .unwrap();
        assert!(free, "backstop released GC_LOCK_ID");
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(super::super::GC_LOCK_ID)
            .execute(&mut *probe)
            .await
            .unwrap();
    }

    /// A cycle that fails against PostgreSQL is counted by its caller
    /// under outcome="error" (the parse-failure abort keeps its own
    /// outcome), so DB-error cycles are visible immediately instead of
    /// only via the 25h stalled alert.
    #[tokio::test]
    async fn backstop_counts_error_outcome() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        // Force a mid-cycle DB failure: the shadow report reads
        // `chunks`, so dropping the table fails the cycle after the
        // mark expansion.
        sqlx::query("DROP TABLE chunks CASCADE")
            .execute(&db.pool)
            .await
            .expect("drop chunks");

        let result = collect_backstop_once(&db.pool, super::super::sweep::CHUNK_GRACE_SECS).await;
        assert!(result.is_err(), "the cycle must fail without chunks");

        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=error}"),
            1,
            "a failed cycle is counted under outcome=error"
        );
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 0);
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=parse_failure}"),
            0,
            "a DB error is not a parse failure"
        );

        // The backstop still released the GC lock on the error path.
        let mut probe = db.pool.acquire().await.unwrap();
        let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(super::super::GC_LOCK_ID)
            .fetch_one(&mut *probe)
            .await
            .unwrap();
        assert!(free, "GC_LOCK_ID is released after a failed cycle");
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(super::super::GC_LOCK_ID)
            .execute(&mut *probe)
            .await
            .unwrap();
    }

    /// The backstop's first tick fires one full interval after spawn,
    /// never at process boot: a freshly spawned backstop must NOT have
    /// run a cycle while well inside the first interval (the heaviest
    /// query pattern in the system must not fire on every pod
    /// boot/scale-up/crash-loop), and must then run cycles once the
    /// interval elapses (the daily cadence still exists). The settle
    /// window is a small fraction of the cfg(test) interval, so the
    /// boot-side assertion has generous real-time margin; the liveness
    /// side polls with a long deadline rather than a tight gate.
    #[tokio::test]
    async fn backstop_first_cycle_waits_one_interval_after_spawn() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let shutdown = rio_common::signal::Token::new();
        let handle = spawn_collect_backstop(db.pool.clone(), shutdown.clone());

        // Settle: ample real time for a boot-fired cycle to have
        // completed (the fixture is empty, a cycle is a handful of
        // trivial statements), while staying far inside the first
        // interval so a fixed backstop cannot have ticked yet.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"),
            0,
            "the backstop must not run a collect cycle at process boot"
        );

        // Liveness: after the first interval elapses the backstop does
        // run cycles. Poll with a generous deadline (structural
        // assertion on the counter, not a tight wall-clock gate).
        let mut ran = 0;
        for _ in 0..200 {
            ran = rec.get("rio_store_gc_collect_cycles_total{outcome=ok}");
            if ran >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            ran >= 1,
            "the backstop must run a cycle once its interval elapses"
        );

        shutdown.cancel();
        handle.await.expect("backstop task shuts down cleanly");
    }

    /// Capped-collect scaffolding: the process-local cursor round-trips
    /// and resets (the live arm persists/loads it across cycles).
    #[test]
    fn collect_cursor_roundtrip_and_reset() {
        let cursor = CollectCursor::new();
        assert!(
            cursor.get().is_none(),
            "fresh cursor starts at the beginning"
        );
        cursor.store(vec![0xAB; 32]);
        assert_eq!(cursor.get(), Some(vec![0xAB; 32]));
        cursor.store(vec![0xCD; 32]);
        assert_eq!(
            cursor.get(),
            Some(vec![0xCD; 32]),
            "a later stop point replaces the earlier one"
        );
        cursor.reset();
        assert!(
            cursor.get().is_none(),
            "pass completion resets to the beginning"
        );
    }
}
