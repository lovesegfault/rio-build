//! Lazy chunk collector: liveness derived from the durable manifests.
//!
//! A collect cycle derives the set of live chunk hashes from every
//! existing manifest's `manifest_data.chunk_list` (any status — an
//! `'uploading'` placeholder counts exactly as its write-ahead upsert
//! counts it today) inside PostgreSQL, then soft-deletes + enqueues
//! the chunks that no manifest references and that are older than the
//! grace window measured from `GREATEST(created_at,
//! last_referenced_at)`. The maintained `chunks.refcount` counter is
//! never consulted for the eligibility decision.
//!
//! Two arms share the cycle (`CollectMode`, crate-private):
//!
//! - `Live`: mark + report + the capped, per-batch soft-delete/enqueue
//!   loop. This is what `run_gc` phase 3 and the daily backstop run.
//! - `Shadow`: mark + report only, no `UPDATE` anywhere. A dry-run
//!   GC's phase 3 runs this arm so a dry run stays observation-only;
//!   it also reports the would-collect count that anchors the backlog
//!   estimate.
//!
//! # Fail-closed mark
//!
//! The mark phase is all-or-nothing: a validation pass checks every
//! joined `chunk_list` (version byte, entry alignment, chunk-count
//! bound) and ANY violation aborts the cycle — counted by
//! `rio_store_gc_collect_parse_failures_total`, offending
//! `store_path_hash` logged at error level, no verdict produced and
//! nothing collected. Treating corrupt input as "references nothing"
//! would turn a storage leak into collected live data, which is the
//! polarity the design forbids; the legacy decrement paths'
//! warn-and-skip behavior (`super::parse_unique_chunk_hashes`) is
//! exactly what the collector must NOT do.
//!
//! # Capped collect
//!
//! The live arm collects at most [`COLLECT_CYCLE_VICTIM_CAP`] victims
//! per cycle and carries a process-local keyset cursor across cycles
//! ([`CollectCursor`]) so a backlog drains across cycles instead of
//! stretching one cycle past the GC-lock-held budget. A cycle that
//! stops at the cap leaves the remainder for the next cycle (the next
//! GC run's phase 3 or the daily backstop); stopping early only
//! retains garbage longer, never collects more.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tracing::{error, info, instrument, warn};

use crate::backend::ChunkBackend;

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

/// Per-batch `LIMIT` for the live arm's candidate scan (and therefore
/// the per-batch transaction size of the soft-delete + enqueue). The
/// design value is the orphan-sweep-derived 10,000 the cap derivation
/// assumes (`COLLECT_CYCLE_VICTIM_CAP` = exactly 50 of these batches);
/// large enough to amortize per-statement overhead, small enough that
/// one batch's row locks and WAL stay bounded. Mirrors the bench's
/// `COLLECT_BATCH_DEFAULT`.
#[cfg(not(test))]
pub(crate) const COLLECT_BATCH_LIMIT: u64 = 10_000;
/// Test override: small enough that the multi-batch and cap/cursor
/// structural tests exercise real batch boundaries on a few dozen
/// seeded rows (the cfg(test) cap is exactly two of these batches).
#[cfg(test)]
pub(crate) const COLLECT_BATCH_LIMIT: u64 = 25;

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

/// One collect batch's candidate scan: the next `LIMIT` unmarked
/// chunks past grace, in `blake3_hash` order, resuming from a keyset
/// cursor ($1). This is the design §4.1 step-3 predicate (anti-join
/// against the mark product, `GREATEST(created_at, last_referenced_at)`
/// against the cycle snapshot) in the orphan-chunk-sweep skeleton the
/// design names (candidate SELECT, then a sorted `= ANY` soft-delete in
/// the same per-batch transaction — [`COLLECT_BATCH_UPDATE_SQL`]). The
/// keyset cursor bounds both sides: without it every batch re-probes
/// all marked rows that precede its candidates (quadratic in batch
/// count at the design-point scale), and mirroring the cursor into the
/// anti-join's inner side means each index is walked once across the
/// whole pass. Eligibility is the manifest fold (absence from
/// `live_chunks`) plus the grace term; `chunks.refcount` is never
/// consulted.
///
/// Shared with `gc::mark_scan_bench` so the bench's EXPLAIN plan-shape
/// guard (design §5b gate (b)) exercises this exact statement, not a
/// copy.
// r[impl store.chunk.liveness-derived]
// r[impl store.chunk.grace-ttl+2]
pub(crate) const COLLECT_BATCH_SELECT_SQL: &str = "SELECT c.blake3_hash FROM chunks c \
     WHERE c.blake3_hash > $1 \
       AND c.deleted = FALSE \
       AND NOT EXISTS (SELECT 1 FROM live_chunks lc \
                        WHERE lc.blake3_hash = c.blake3_hash AND lc.blake3_hash > $1) \
       AND GREATEST(c.created_at, c.last_referenced_at) < $2::timestamptz \
     ORDER BY c.blake3_hash \
     LIMIT $3";

/// One collect batch's soft-delete. The UPDATE re-evaluates the collect
/// predicate's row-local conjuncts — `deleted = FALSE` plus the
/// `GREATEST(created_at, last_referenced_at) < cutoff` grace term — in
/// its own WHERE clause, per the T-1a.8 consequence recorded in
/// `docs/spec/models/refcount-invariant-map.md` (chunkCollect encoding
/// notes, the row-lock / READ-COMMITTED bullet) and design §4.1's
/// in-flight-overlap sentence: a READ-COMMITTED row-lock wait re-checks
/// only the conjuncts present in this WHERE, so the touch/grace
/// re-check here is what protects a chunk re-referenced and touched
/// between the candidate scan and this UPDATE; the anti-join is not
/// re-evaluated. The model's writer-bounded HOLDS verdict is stated
/// against this predicate-re-checking shape — a hash-only or
/// deleted-only WHERE re-opens the §4.6(i) mark-stale data-loss
/// window. This mirrors `sweep_orphan_batch`, whose UPDATE re-checks
/// its full liveness predicate; this statement is its
/// predicate-swapped analog. The bind is the candidate scan's result,
/// which is already in ascending hash order.
///
/// Shared with `gc::mark_scan_bench` (gate (c) measurement runs and
/// the structural predicate pin exercise this exact statement).
// r[impl store.chunk.lock-order]
pub(crate) const COLLECT_BATCH_UPDATE_SQL: &str = "UPDATE chunks SET deleted = TRUE, uploaded_at = NULL \
     WHERE blake3_hash = ANY($1) AND deleted = FALSE \
       AND GREATEST(created_at, last_referenced_at) < $2::timestamptz \
     RETURNING blake3_hash, size";

/// Which arm of the collector runs (P6: code-staged, no config field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectMode {
    /// Mark + report only. No row anywhere is modified. Used by
    /// dry-run GC runs (and available for diagnostics); also the only
    /// arm the pre-cutover release shipped.
    Shadow,
    /// Mark + report + the capped soft-delete/enqueue loop — the
    /// production arm from the cutover release on.
    Live,
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
    /// `GREATEST(created_at, last_referenced_at)`. Computed by the
    /// shadow arm only (the live arm does not re-run the full
    /// anti-join count — that scan term is exactly what the cap
    /// manages); always 0 in live mode.
    pub(crate) would_collect: u64,
    /// Sum of `chunks.size` over the would-collect set (shadow arm
    /// only — the dry-run "bytes that would be freed" estimate).
    pub(crate) would_collect_bytes: u64,
    /// Drift, leak direction: `refcount > 0`, unmarked, not deleted.
    pub(crate) drift_leaked: u64,
    /// Drift, under-count direction: marked live, `refcount = 0`, not
    /// deleted. Never expected while the increment still fires.
    pub(crate) drift_undercount: u64,
    /// Victims soft-deleted this cycle. Always 0 in shadow mode.
    pub(crate) victims_collected: u64,
    /// Sum of `chunks.size` over the victims soft-deleted this cycle.
    pub(crate) victim_bytes: u64,
    /// S3 keys enqueued to `pending_s3_deletes` this cycle (0 when the
    /// store has no chunk backend).
    pub(crate) s3_keys_enqueued: u64,
    /// Soft-delete batches run this cycle. Always 0 in shadow mode.
    pub(crate) batches_run: u64,
    /// True when the cycle stopped at [`COLLECT_CYCLE_VICTIM_CAP`]
    /// with backlog remaining. Always false in shadow mode.
    pub(crate) cap_reached: bool,
    /// Keyset cursor at the stop point (None when the pass completed
    /// or in shadow mode).
    pub(crate) cursor_at_stop: Option<Vec<u8>>,
    /// Wall-clock of the cycle (snapshot through report/collect).
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

/// Process-local backlog estimate behind the
/// `rio_store_gc_collect_backlog_chunks` gauge (P15: a decremental
/// estimate for drain visibility, not accounting). A shadow cycle
/// anchors it at that cycle's would-collect count; each live cycle
/// subtracts its own collected count; a completed pass re-anchors it
/// at zero. While unanchored (`None` — e.g. a fresh pod mid-pass) live
/// cycles leave the gauge alone rather than publish an invented
/// number; the next pass completion (or dry-run/shadow cycle)
/// re-anchors it. Never persisted; restarts only delay the estimate,
/// never the drain.
static COLLECT_BACKLOG_ESTIMATE: Mutex<Option<u64>> = Mutex::new(None);

/// Anchor the backlog estimate (shadow cycle's would-collect) and
/// publish the gauge.
fn backlog_anchor(value: u64) {
    *COLLECT_BACKLOG_ESTIMATE
        .lock()
        .expect("backlog estimate poisoned") = Some(value);
    metrics::gauge!("rio_store_gc_collect_backlog_chunks").set(value as f64);
}

/// Subtract a live cycle's collected count from the backlog estimate
/// (saturating) and publish the gauge if an anchor exists.
fn backlog_consume(collected: u64) {
    let mut guard = COLLECT_BACKLOG_ESTIMATE
        .lock()
        .expect("backlog estimate poisoned");
    if let Some(estimate) = guard.as_mut() {
        *estimate = estimate.saturating_sub(collected);
        metrics::gauge!("rio_store_gc_collect_backlog_chunks").set(*estimate as f64);
    }
}

/// A pass completed: nothing eligible remains at this cycle's
/// snapshot, so the estimate re-anchors at zero.
fn backlog_pass_complete() {
    backlog_anchor(0);
}

/// Test-only failure injection for the per-batch isolation tests: when
/// set to N > 0, the live collect loop returns an error after
/// committing N batches (and clears the injection). Prior batches'
/// soft-deletes and enqueues are already committed — exactly the
/// mid-cycle DB-failure shape the isolation test pins.
#[cfg(test)]
pub(crate) static COLLECT_FAIL_AFTER_BATCHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// One collect cycle (design §4.1): snapshot → fail-closed mark →
/// prepare → report → (live arm) the capped soft-delete/enqueue loop.
/// Uses its own pooled connection for the session temp table; callers
/// hold [`super::GC_LOCK_ID`] on a different connection (run_gc
/// phase 3, [`collect_backstop_once`]).
///
/// The whole read phase — cutoff, fail-closed validation, mark
/// expansion, prepare, and the report — runs in one
/// REPEATABLE READ transaction, i.e. on one MVCC snapshot. That makes
/// the [`CollectReport`] "at the cycle snapshot" semantics true as
/// written: the validation pass and the expansion see the same manifest
/// set (no TOCTOU inside the fail-closed guarantee), and uploads or
/// PutPath rollbacks that commit during the multi-minute mark→report
/// window cannot surface in the drift gauges — a nonzero drift reading
/// is real refcount drift, not cycle-concurrent traffic. The
/// transaction also scopes the cycle's session state (`SET LOCAL`
/// memory budget) so nothing outlives the cycle on any exit path. The
/// snapshot (and with it the xmin horizon) is held for the full
/// mark+prepare+report span — the same order the expansion statement
/// alone already held — and takes no locks that block writers;
/// acceptable at the once-per-GC + daily-backstop cadence (measured
/// spans: invariant map T-1a.1b/T-1a.1c records).
///
/// Temp-table lifetime differs by arm. The shadow arm's mark product
/// is `ON COMMIT DROP`, so it dies with the read transaction on every
/// exit path. The live arm's mark product must outlive the read
/// transaction — the per-batch collect transactions anti-join against
/// it — so it is created without the clause, dropped explicitly at the
/// end of the cycle, and any exit path between the read commit and
/// that cleanup detaches (closes) the connection so the temp table and
/// session die together instead of leaking into the shared pool (the
/// same choreography run_gc uses for its lock connection).
///
/// The collect loop runs per-batch READ COMMITTED transactions: a
/// keyset-cursor candidate scan ([`COLLECT_BATCH_SELECT_SQL`]), a
/// sorted `= ANY` soft-delete that re-checks the row-local collect
/// predicate in its own WHERE ([`COLLECT_BATCH_UPDATE_SQL`]), and the
/// outbox enqueue, all in the same transaction. The loop stops when a
/// short candidate scan ends the pass (cursor reset) or when
/// [`COLLECT_CYCLE_VICTIM_CAP`] victims have been collected (cursor
/// persisted in [`COLLECT_CURSOR`]; the next cycle resumes there).
///
/// The cycle-duration histogram records completed cycles only; an
/// aborted (parse-failure) cycle is counted by its own counter and the
/// `outcome="parse_failure"` cycle counter instead.
// r[impl store.gc.chunk-collect]
#[instrument(skip(pool, chunk_backend))]
pub(crate) async fn collect_cycle(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    grace_secs: i64,
    mode: CollectMode,
) -> Result<CollectReport, sqlx::Error> {
    let cycle_started = Instant::now();
    let mut conn = pool.acquire().await?;

    // One read phase = one transaction = one MVCC snapshot (see the
    // function doc). Not READ ONLY: PostgreSQL forbids CREATE TEMP
    // TABLE in read-only transactions. Every early `?` return drops
    // the Transaction, which queues a ROLLBACK — the SET LOCAL budget
    // and the (uncommitted) temp table die with it on every exit path
    // before the commit, so nothing leaks back into the shared pool.
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

    // Defensive, no longer load-bearing: the cycle's own cleanup (ON
    // COMMIT DROP in shadow mode, the explicit drop / detach-on-error
    // in live mode) cannot leave the temp table behind, but a session
    // poisoned by a binary predating the transaction-scoped cycle may
    // still carry one (same rationale as the sweep's
    // setup_sweep_unreachable).
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
        // wrote nothing and leaves nothing on the session. The live
        // arm is fail-closed through this same return: no batch loop
        // runs, so a cycle that observed a parse failure soft-deletes
        // nothing.
        return Ok(CollectReport {
            outcome: CollectOutcome::ParseFailure,
            mark_set_size: 0,
            would_collect: 0,
            would_collect_bytes: 0,
            drift_leaked: 0,
            drift_undercount: 0,
            victims_collected: 0,
            victim_bytes: 0,
            s3_keys_enqueued: 0,
            batches_run: 0,
            cap_reached: false,
            cursor_at_stop: None,
            cycle_seconds: cycle_started.elapsed().as_secs_f64(),
        });
    }

    // --- Mark (ii): server-side set-based expansion ---
    // Shadow mode runs the shared statement with ON COMMIT DROP added,
    // so the mark product cannot outlive the read transaction on any
    // exit path. The live arm's batches anti-join against the table in
    // their own transactions after this one commits, so it is created
    // without the clause and dropped explicitly at the end of the
    // cycle (with detach-on-error covering the window in between). The
    // shared constant stays free of the clause: the bench's EXPLAIN
    // plan-shape guard (gate (b)) runs it outside a transaction, where
    // ON COMMIT DROP would drop the table at statement end.
    // AssertSqlSafe: splices only the shared constant.
    let expansion_body = MARK_EXPANSION_SQL
        .strip_prefix("CREATE TEMP TABLE live_chunks AS ")
        .expect("MARK_EXPANSION_SQL starts with the live_chunks CTAS prefix");
    let expansion = match mode {
        CollectMode::Shadow => {
            format!("CREATE TEMP TABLE live_chunks ON COMMIT DROP AS {expansion_body}")
        }
        CollectMode::Live => format!("CREATE TEMP TABLE live_chunks AS {expansion_body}"),
    };
    sqlx::query(sqlx::AssertSqlSafe(expansion))
        .execute(&mut *tx)
        .await?;

    // --- Prepare: unique index + stats on the mark product, so the
    // anti-join probes an index instead of hashing/sorting the whole
    // mark set per query (per batch in the live arm).
    sqlx::query("CREATE UNIQUE INDEX live_chunks_hash_idx ON live_chunks (blake3_hash)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("ANALYZE live_chunks").execute(&mut *tx).await?;

    let mark_set_size: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks")
        .fetch_one(&mut *tx)
        .await?;

    // --- Report: one pass over the not-deleted chunk rows, on the
    // cycle snapshot ---
    // drift, leak direction: refcount > 0 AND unmarked;
    // drift, under-count direction: refcount = 0 AND marked;
    // would-collect (shadow arm only): unmarked AND past grace (the
    // collect predicate) plus its byte sum. The live arm does NOT
    // re-run the would-collect anti-join count — that scan term is
    // exactly what the per-cycle cap exists to manage (P15); its
    // backlog visibility comes from the decremental estimate below.
    let (would_collect, would_collect_bytes, drift_leaked, drift_undercount): (
        i64,
        i64,
        i64,
        i64,
    ) = match mode {
        CollectMode::Shadow => {
            sqlx::query_as(
                "SELECT \
                   COUNT(*) FILTER (WHERE NOT in_mark \
                                      AND GREATEST(created_at, last_referenced_at) < $1::timestamptz), \
                   COALESCE(SUM(size) FILTER (WHERE NOT in_mark \
                                      AND GREATEST(created_at, last_referenced_at) < $1::timestamptz), 0)::bigint, \
                   COUNT(*) FILTER (WHERE refcount > 0 AND NOT in_mark), \
                   COUNT(*) FILTER (WHERE refcount = 0 AND in_mark) \
                 FROM (SELECT c.refcount, c.size, c.created_at, c.last_referenced_at, \
                              EXISTS (SELECT 1 FROM live_chunks lc \
                                       WHERE lc.blake3_hash = c.blake3_hash) AS in_mark \
                         FROM chunks c \
                        WHERE c.deleted = FALSE) AS s",
            )
            .bind(&cutoff)
            .fetch_one(&mut *tx)
            .await?
        }
        CollectMode::Live => {
            let (leaked, undercount): (i64, i64) = sqlx::query_as(
                "SELECT \
                   COUNT(*) FILTER (WHERE refcount > 0 AND NOT in_mark), \
                   COUNT(*) FILTER (WHERE refcount = 0 AND in_mark) \
                 FROM (SELECT c.refcount, \
                              EXISTS (SELECT 1 FROM live_chunks lc \
                                       WHERE lc.blake3_hash = c.blake3_hash) AS in_mark \
                         FROM chunks c \
                        WHERE c.deleted = FALSE) AS s",
            )
            .fetch_one(&mut *tx)
            .await?;
            (0, 0, leaked, undercount)
        }
    };

    // Commit ends the cycle's snapshot. In shadow mode the temp table
    // dies here (ON COMMIT DROP) before the connection returns to the
    // pool; in live mode it survives for the batch loop below.
    tx.commit().await?;

    metrics::gauge!("rio_store_gc_chunks_live").set(mark_set_size as f64);
    metrics::gauge!("rio_store_gc_refcount_drift_leaked").set(drift_leaked as f64);
    metrics::gauge!("rio_store_gc_refcount_drift_undercount").set(drift_undercount as f64);

    if mode == CollectMode::Shadow {
        let cycle_seconds = cycle_started.elapsed().as_secs_f64();
        metrics::gauge!("rio_store_gc_chunks_would_collect").set(would_collect as f64);
        // Shadow mode: the backlog estimate IS the would-collect count.
        // Live cycles maintain it decrementally from their own
        // collected counts instead of re-running this full anti-join.
        backlog_anchor(would_collect as u64);
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

        return Ok(CollectReport {
            outcome: CollectOutcome::Ok,
            mark_set_size: mark_set_size as u64,
            would_collect: would_collect as u64,
            would_collect_bytes: would_collect_bytes as u64,
            drift_leaked: drift_leaked as u64,
            drift_undercount: drift_undercount as u64,
            victims_collected: 0,
            victim_bytes: 0,
            s3_keys_enqueued: 0,
            batches_run: 0,
            cap_reached: false,
            cursor_at_stop: None,
            cycle_seconds,
        });
    }

    // --- Live arm: the capped, cursor-resumable soft-delete loop ---
    // From here until the explicit temp-table drop the session carries
    // the mark product outside any transaction, so any exit that does
    // not reach the cleanup detaches (closes) the connection — the
    // temp table dies with the session instead of leaking into the
    // shared pool.
    let mut conn = scopeguard::guard(conn, |c| {
        let _ = c.detach();
    });

    let mut cursor: Vec<u8> = COLLECT_CURSOR.get().unwrap_or_default();
    let mut victims_collected: u64 = 0;
    let mut victim_bytes: u64 = 0;
    let mut s3_keys_enqueued: u64 = 0;
    let mut batches_run: u64 = 0;
    let mut cap_reached = false;
    let mut pass_complete = false;

    loop {
        let remaining = COLLECT_CYCLE_VICTIM_CAP.saturating_sub(victims_collected);
        if remaining == 0 {
            // Stopping at the cap is the designed behavior for
            // backlogs: the remainder drains on subsequent cycles via
            // the persisted cursor. Retention-erring, never
            // over-collecting.
            cap_reached = true;
            break;
        }
        let this_limit = COLLECT_BATCH_LIMIT.min(remaining) as i64;

        // One batch = one transaction: candidate scan, predicate-
        // re-checking soft-delete, outbox enqueue. A failure here
        // leaves prior batches committed (per-batch isolation).
        let mut batch_tx = sqlx::Connection::begin(&mut **conn).await?;
        let candidates: Vec<Vec<u8>> = sqlx::query_scalar(COLLECT_BATCH_SELECT_SQL)
            .bind(&cursor)
            .bind(&cutoff)
            .bind(this_limit)
            .fetch_all(&mut *batch_tx)
            .await?;
        if candidates.is_empty() {
            batch_tx.commit().await?;
            pass_complete = true;
            break;
        }
        let short = candidates.len() < this_limit as usize;

        // The candidate scan returns hashes in ascending order, so the
        // `= ANY` soft-delete takes its row locks in sorted order
        // (store.chunk.lock-order) and the WHERE re-checks the
        // row-local collect predicate (see COLLECT_BATCH_UPDATE_SQL).
        let collected: Vec<(Vec<u8>, i64)> = sqlx::query_as(COLLECT_BATCH_UPDATE_SQL)
            .bind(&candidates)
            .bind(&cutoff)
            .fetch_all(&mut *batch_tx)
            .await?;
        let enqueued =
            super::enqueue_chunk_deletes(&mut batch_tx, &collected, chunk_backend).await?;
        batch_tx.commit().await?;

        // Per-batch, post-commit: every increment ↔ exactly one
        // committed transaction (same discipline as the path sweep's
        // counters).
        metrics::counter!("rio_store_gc_chunks_collected_total").increment(collected.len() as u64);
        metrics::counter!("rio_store_gc_s3_key_enqueued_total").increment(enqueued);

        batches_run += 1;
        victims_collected += collected.len() as u64;
        victim_bytes += collected.iter().map(|(_, s)| *s as u64).sum::<u64>();
        s3_keys_enqueued += enqueued;
        cursor = candidates
            .last()
            .expect("non-empty batch has a last hash")
            .clone();

        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            let inject_after = COLLECT_FAIL_AFTER_BATCHES.load(Ordering::SeqCst);
            if inject_after > 0 && batches_run >= inject_after {
                COLLECT_FAIL_AFTER_BATCHES.store(0, Ordering::SeqCst);
                COLLECT_CURSOR.store(cursor.clone());
                return Err(sqlx::Error::Protocol(
                    "chunk-collect: injected post-batch failure (test only)".into(),
                ));
            }
        }

        if short {
            pass_complete = true;
            break;
        }
    }

    // Cursor bookkeeping: a capped stop persists the resume point; a
    // completed pass resets to the start of the keyspace. (A lost
    // cursor is never a correctness problem — the candidate scan's
    // `deleted = FALSE` conjunct skips already-collected rows.)
    let cursor_at_stop = if cap_reached {
        COLLECT_CURSOR.store(cursor.clone());
        Some(cursor)
    } else {
        COLLECT_CURSOR.reset();
        None
    };

    // Cleanup: drop the mark product and hand the connection back to
    // the pool (defusing the detach guard).
    sqlx::query("DROP TABLE IF EXISTS live_chunks")
        .execute(&mut **conn)
        .await?;
    drop(scopeguard::ScopeGuard::into_inner(conn));

    // Drain visibility (P15): decrement the backlog estimate by what
    // this cycle collected; a completed pass re-anchors it at zero.
    if pass_complete {
        backlog_pass_complete();
    } else {
        backlog_consume(victims_collected);
    }
    if cap_reached {
        metrics::counter!("rio_store_gc_collect_cycles_capped_total").increment(1);
    }

    let cycle_seconds = cycle_started.elapsed().as_secs_f64();
    metrics::histogram!("rio_store_gc_collect_cycle_seconds").record(cycle_seconds);
    metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "ok").increment(1);

    info!(
        mark_set_size,
        drift_leaked,
        drift_undercount,
        victims_collected,
        victim_bytes,
        s3_keys_enqueued,
        batches_run,
        cap_reached,
        cycle_seconds,
        "chunk-collect live cycle complete"
    );

    Ok(CollectReport {
        outcome: CollectOutcome::Ok,
        mark_set_size: mark_set_size as u64,
        would_collect: 0,
        would_collect_bytes: 0,
        drift_leaked: drift_leaked as u64,
        drift_undercount: drift_undercount as u64,
        victims_collected,
        victim_bytes,
        s3_keys_enqueued,
        batches_run,
        cap_reached,
        cursor_at_stop,
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
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
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

    let result = collect_cycle(pool, chunk_backend, grace_secs, CollectMode::Live).await;
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
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + COLLECT_BACKSTOP_INTERVAL,
        COLLECT_BACKSTOP_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    rio_common::task::spawn_periodic_with("gc-collect-backstop", ticker, shutdown, move || {
        let pool = pool.clone();
        let chunk_backend = chunk_backend.clone();
        async move {
            if let Err(e) = collect_backstop_once(
                &pool,
                chunk_backend.as_ref(),
                super::sweep::CHUNK_GRACE_SECS,
            )
            .await
            {
                warn!(error = %e, "chunk-collect backstop failed (will retry next interval)");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::test_helpers::{ChunkSeed, StoreSeed, mem_backend};
    use rio_test_support::TestDb;
    use rio_test_support::metrics::CountingRecorder;
    use rstest::rstest;

    /// Reset the collector's process-local state (cursor + backlog
    /// estimate). nextest runs each test in its own process, but the
    /// statics are shared within one test's multiple cycles — and under
    /// plain `cargo test` across tests — so every live-arm test starts
    /// from a known state.
    fn reset_collector_state() {
        COLLECT_CURSOR.reset();
        *COLLECT_BACKLOG_ESTIMATE
            .lock()
            .expect("backlog estimate poisoned") = None;
    }

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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
                None,
                super::super::sweep::CHUNK_GRACE_SECS,
                CollectMode::Shadow,
            )
            .await
            .expect("cycle");
            assert_eq!(report.outcome, CollectOutcome::Ok);
            assert_eq!(report.mark_set_size, 1);
        }
    }

    // r[verify store.gc.chunk-collect]
    /// run_gc phase 3 runs the LIVE cycle while the GC lock is held:
    /// an old unreferenced chunk is soft-deleted by the GC run and the
    /// run's chunk-level stats are sourced from the collect cycle.
    #[tokio::test]
    async fn run_gc_phase3_runs_live_cycle() {
        use tokio::sync::mpsc;

        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let h = ChunkSeed::new(0x42)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(&db.pool, "phase3", "complete", &make_chunk_list(&[h])).await;
        // An old, unreferenced victim the live phase 3 must collect.
        ChunkSeed::new(0x43)
            .with_size(2048)
            .age_secs(3600)
            .seed(&db.pool)
            .await;

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
            "phase 3 ran exactly one live cycle"
        );
        assert_eq!(rec.gauge_value("rio_store_gc_chunks_live{}"), Some(1.0));
        assert_eq!(
            stats.chunks_deleted, 1,
            "chunk-level GC stats are sourced from the collect cycle"
        );
        assert_eq!(stats.bytes_freed, 2048);
        let deleted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(deleted, 1, "the unreferenced victim was soft-deleted");
    }

    /// A dry-run GC keeps phase 3 observation-only: nothing is
    /// soft-deleted or enqueued, and the reported chunk stats are the
    /// would-collect estimate.
    #[tokio::test]
    async fn run_gc_dry_run_keeps_collect_shadow() {
        use tokio::sync::mpsc;

        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        // An old, unreferenced victim that a LIVE cycle would collect.
        ChunkSeed::new(0x44)
            .with_size(512)
            .age_secs(3600)
            .seed(&db.pool)
            .await;
        let before = chunks_table_digest(&db.pool).await;

        let (tx, mut rx) = mpsc::channel(8);
        let stats = super::super::run_gc(
            &db.pool,
            None,
            super::super::GcParams {
                dry_run: true,
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

        assert_eq!(
            stats.chunks_deleted, 1,
            "dry run reports the would-collect estimate"
        );
        assert_eq!(stats.bytes_freed, 512);
        assert_eq!(
            chunks_table_digest(&db.pool).await,
            before,
            "a dry-run GC's phase 3 modifies nothing"
        );
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 0);
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

        let skipped = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
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

        let ran = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
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

        let result =
            collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS).await;
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
        let handle = spawn_collect_backstop(db.pool.clone(), None, shutdown.clone());

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

    // ----------------------------------------------------------------
    // Live arm (the cutover release's collect arm)
    // ----------------------------------------------------------------

    /// Seed `n` old, untouched, unreferenced chunks (the collectable
    /// population) with distinct hashes and the given size.
    async fn seed_collectable_chunks(pool: &PgPool, n: u16, size: i64) -> Vec<[u8; 32]> {
        let mut hashes = Vec::with_capacity(n as usize);
        for i in 0..n {
            let mut hash = [0u8; 32];
            hash[0] = (i >> 8) as u8;
            hash[1] = (i & 0xFF) as u8;
            hash[2] = 0xC0;
            sqlx::query(
                "INSERT INTO chunks (blake3_hash, refcount, size, created_at, uploaded_at) \
                 VALUES ($1, 0, $2, now() - interval '1 hour', now() - interval '1 hour')",
            )
            .bind(&hash[..])
            .bind(size)
            .execute(pool)
            .await
            .expect("seed collectable chunk");
            hashes.push(hash);
        }
        hashes
    }

    // r[verify store.chunk.liveness-derived]
    // r[verify store.gc.chunk-collect]
    /// The historical-leak shape: an unreferenced chunk past grace with
    /// a stale `refcount > 0` is soft-deleted and enqueued by a live
    /// cycle — eligibility is the manifest fold, the counter is never
    /// consulted. Collected chunks land in `pending_s3_deletes` exactly
    /// once, and a follow-up cycle finds nothing left to collect.
    #[tokio::test]
    async fn live_cycle_collects_stale_refcount_leak() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // The leaked chunk: unreferenced, past grace, stale refcount=3.
        let leaked = ChunkSeed::new(0xB1)
            .with_refcount(3)
            .with_size(4096)
            .age_secs(3600)
            .uploaded()
            .seed(&db.pool)
            .await;
        // A referenced control chunk that must survive.
        let live = ChunkSeed::new(0xA7)
            .with_refcount(1)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(&db.pool, "leak-live", "complete", &make_chunk_list(&[live])).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("live cycle");

        assert_eq!(report.outcome, CollectOutcome::Ok);
        assert_eq!(report.victims_collected, 1, "exactly the leaked chunk");
        assert_eq!(report.victim_bytes, 4096);
        assert_eq!(report.s3_keys_enqueued, 1);
        assert!(!report.cap_reached);
        assert!(report.cursor_at_stop.is_none(), "pass completed");
        assert_eq!(
            report.drift_leaked, 1,
            "the stale counter is reported as leak-direction drift, not consulted"
        );

        let (deleted, uploaded_cleared): (bool, bool) = sqlx::query_as(
            "SELECT deleted, uploaded_at IS NULL FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&leaked[..])
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(deleted, "leaked chunk soft-deleted despite refcount > 0");
        assert!(uploaded_cleared, "soft-delete clears uploaded_at");
        let live_deleted: bool =
            sqlx::query_scalar("SELECT deleted FROM chunks WHERE blake3_hash = $1")
                .bind(&live[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(!live_deleted, "referenced chunk untouched");

        // Enqueued exactly once.
        let pending: Vec<(Vec<u8>,)> = sqlx::query_as("SELECT blake3_hash FROM pending_s3_deletes")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, leaked.to_vec());
        assert_eq!(rec.get("rio_store_gc_chunks_collected_total{}"), 1);
        assert_eq!(rec.get("rio_store_gc_s3_key_enqueued_total{}"), 1);
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 1);
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_capped_total{}"),
            0,
            "a short-batch pass is not a capped cycle"
        );

        // A second live cycle finds nothing: no re-collection, no
        // duplicate enqueue.
        let again = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("second live cycle");
        assert_eq!(again.victims_collected, 0);
        let pending_again: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending_again, 1, "no duplicate outbox rows");
    }

    // r[verify store.chunk.liveness-derived]
    // r[verify store.chunk.grace-ttl+2]
    /// The live arm's protection partition: a chunk referenced only by
    /// an `'uploading'` placeholder, a younger-than-grace chunk, and an
    /// old chunk with a recent `last_referenced_at` touch all survive a
    /// live cycle; the old, untouched, unreferenced control is
    /// collected (so the survivals are not vacuous).
    #[tokio::test]
    async fn live_cycle_spares_uploading_grace_and_touched() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // Referenced ONLY by an 'uploading' placeholder, old.
        let uploading_ref = ChunkSeed::new(0x01).age_secs(3600).seed(&db.pool).await;
        seed_chunked_manifest(
            &db.pool,
            "spare-uploading",
            "uploading",
            &make_chunk_list(&[uploading_ref]),
        )
        .await;
        // Unreferenced but younger than grace.
        let young = ChunkSeed::new(0x02).seed(&db.pool).await;
        // Unreferenced, old, but freshly touched (the 068 column).
        let touched = ChunkSeed::new(0x03).age_secs(3600).seed(&db.pool).await;
        sqlx::query("UPDATE chunks SET last_referenced_at = now() WHERE blake3_hash = $1")
            .bind(&touched[..])
            .execute(&db.pool)
            .await
            .unwrap();
        // Control: unreferenced, old, untouched — must be collected.
        let control = ChunkSeed::new(0x04).age_secs(3600).seed(&db.pool).await;

        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("live cycle");
        assert_eq!(report.victims_collected, 1, "only the control is eligible");

        let survivors: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT blake3_hash FROM chunks WHERE deleted = FALSE ORDER BY blake3_hash",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert!(survivors.contains(&uploading_ref.to_vec()));
        assert!(survivors.contains(&young.to_vec()));
        assert!(survivors.contains(&touched.to_vec()));
        assert!(!survivors.contains(&control.to_vec()), "control collected");
    }

    // r[verify store.gc.chunk-collect]
    /// Fail-closed against the live arm: a cycle that observes a parse
    /// failure aborts before the collect loop — zero soft-deletes, zero
    /// outbox rows — even though an eligible victim exists.
    #[tokio::test]
    async fn live_cycle_parse_failure_collects_nothing() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // An eligible victim that WOULD be collected.
        ChunkSeed::new(0x20).age_secs(3600).seed(&db.pool).await;
        // A corrupt manifest (wrong version byte).
        seed_chunked_manifest(&db.pool, "live-corrupt", "complete", &[0xFFu8, 0, 0]).await;

        let before = chunks_table_digest(&db.pool).await;
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("abort is not an error");

        assert_eq!(report.outcome, CollectOutcome::ParseFailure);
        assert_eq!(report.victims_collected, 0);
        assert_eq!(rec.get("rio_store_gc_collect_parse_failures_total{}"), 1);
        assert_eq!(rec.get("rio_store_gc_chunks_collected_total{}"), 0);
        assert_eq!(
            chunks_table_digest(&db.pool).await,
            before,
            "an aborted live cycle modifies nothing"
        );
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 0);
    }

    // r[verify store.gc.chunk-collect]
    /// Post-collect resurrection: an upsert after the live cycle's
    /// soft-delete flips `deleted = false`, reports `needs_upload =
    /// true` (uploaded_at was cleared at soft-delete), and the drain
    /// re-check then skips the stale outbox row and drops it — the
    /// backend object is never deleted out from under the resurrected
    /// chunk.
    #[tokio::test]
    async fn live_cycle_resurrected_chunk_survives_drain() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // An old, unreferenced, S3-confirmed chunk; object present.
        let hash = ChunkSeed::new(0x30)
            .with_size(64)
            .age_secs(3600)
            .uploaded()
            .seed(&db.pool)
            .await;
        backend
            .put(&hash, bytes::Bytes::from_static(b"resurrect-me"))
            .await
            .unwrap();

        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("live cycle");
        assert_eq!(report.victims_collected, 1);

        // A new upload re-references the chunk: the upsert resurrects
        // the row and must see needs_upload = true (uploaded_at was
        // cleared by the soft-delete).
        let path = rio_test_support::fixtures::test_store_path("resurrect-after-collect");
        let path_hash = crate::test_helpers::path_hash(&path);
        crate::metadata::insert_manifest_uploading(&db.pool, &path_hash, &path, &[])
            .await
            .unwrap()
            .expect("placeholder inserted");
        let chunk_list = make_chunk_list(&[hash]);
        let (needs_upload, _token) = crate::metadata::upgrade_manifest_to_chunked(
            &db.pool,
            &path_hash,
            &chunk_list,
            &[hash.to_vec()],
            &[64i64],
        )
        .await
        .unwrap();
        assert!(
            needs_upload.contains(hash.as_slice()),
            "resurrected chunk must be re-uploaded (uploaded_at was cleared)"
        );
        let (deleted, refcount): (bool, i32) =
            sqlx::query_as("SELECT deleted, refcount FROM chunks WHERE blake3_hash = $1")
                .bind(&hash[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(!deleted, "upsert flips deleted back to false");
        assert!(refcount > 0);

        // The stale outbox row from the collect cycle is skipped and
        // dropped by the drain; the backend object survives.
        let (s3_deleted, failed) = super::super::drain::drain_once(&db.pool, &backend)
            .await
            .unwrap();
        assert_eq!(s3_deleted, 0, "resurrected chunk is not S3-deleted");
        assert_eq!(failed, 0);
        assert!(
            backend.get(&hash).await.unwrap().is_some(),
            "backend object preserved for the resurrected chunk"
        );
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending, 0, "stale outbox row dropped");
    }

    // r[verify store.gc.chunk-collect]
    /// A multi-batch collect below the cap: more candidates than one
    /// LIMIT batch, fewer than the cap — the loop runs multiple batch
    /// transactions, terminates on the short batch, collects all of
    /// them, and leaves no session state in the pool.
    #[tokio::test]
    async fn live_cycle_multi_batch_below_cap_collects_all() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (COLLECT_BATCH_LIMIT + 5) as u16; // 2 batches: full + short
        seed_collectable_chunks(&db.pool, n, 100).await;

        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("live cycle");

        assert_eq!(report.victims_collected, u64::from(n), "all collected");
        assert_eq!(report.batches_run, 2, "one full batch + one short batch");
        assert!(!report.cap_reached);
        assert!(report.cursor_at_stop.is_none(), "pass completed");
        let deleted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(deleted, i64::from(n));
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending, i64::from(n), "every victim enqueued exactly once");

        // The live arm's explicit cleanup leaves nothing on the pool.
        for t in temp_table_on_any_pool_connection(&db.pool).await {
            assert!(t.is_none(), "live cycle leaked live_chunks into the pool");
        }
    }

    // r[verify store.gc.chunk-collect]
    /// Per-batch isolation: a failure after one committed batch leaves
    /// that batch's soft-deletes and outbox rows committed (per-batch
    /// transactions, not one cycle transaction), and the next cycle
    /// finishes the remainder without re-collecting the first batch.
    #[tokio::test]
    async fn live_cycle_per_batch_isolation_on_midcycle_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (COLLECT_BATCH_LIMIT + 5) as u16; // 2 batches worth
        seed_collectable_chunks(&db.pool, n, 100).await;

        // Inject a failure after the first committed batch.
        COLLECT_FAIL_AFTER_BATCHES.store(1, std::sync::atomic::Ordering::SeqCst);
        let result = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await;
        assert!(result.is_err(), "the injected failure surfaces as an error");

        let deleted_after_failure: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            deleted_after_failure, COLLECT_BATCH_LIMIT as i64,
            "the committed batch's soft-deletes survive the mid-cycle failure"
        );
        let pending_after_failure: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            pending_after_failure, COLLECT_BATCH_LIMIT as i64,
            "the committed batch's enqueues survive too"
        );
        // The failed cycle's connection was detached, so no session
        // temp table can be left in the shared pool.
        for t in temp_table_on_any_pool_connection(&db.pool).await {
            assert!(t.is_none(), "failed live cycle leaked live_chunks");
        }

        // The next cycle (no injection) finishes the remainder only.
        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("recovery cycle");
        assert_eq!(report.victims_collected, 5, "only the remainder");
        let deleted_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(deleted_total, i64::from(n));
    }

    // r[verify store.gc.chunk-collect]
    /// The cap/cursor structural pair (P15, design §4.1 step 3): a
    /// backlog larger than the cap is collected across two consecutive
    /// cycles. The first stops exactly at the cap (no further
    /// soft-deletes that cycle), bumps the capped-cycles counter,
    /// persists the cursor, and leaves the remainder untouched; the
    /// second resumes past the stop point (no re-collection, no skipped
    /// victims) and finishes the pass. The backlog gauge decreases by
    /// the collected count each cycle and reads 0 after the pass
    /// completes.
    #[tokio::test]
    async fn live_cycle_cap_stop_then_cursor_resume_drains_backlog() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (COLLECT_CYCLE_VICTIM_CAP + 10) as u16; // 60 with the test consts
        seed_collectable_chunks(&db.pool, n, 100).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        // Anchor the backlog estimate the way production does: a
        // shadow (dry-run) cycle reports the would-collect count.
        let shadow = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow,
        )
        .await
        .expect("anchor cycle");
        assert_eq!(shadow.would_collect, u64::from(n));
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(f64::from(n))
        );

        // Cycle 1: stops exactly at the cap.
        let first = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("capped cycle");
        assert!(first.cap_reached);
        assert_eq!(first.victims_collected, COLLECT_CYCLE_VICTIM_CAP);
        assert_eq!(
            first.batches_run,
            COLLECT_CYCLE_VICTIM_CAP.div_ceil(COLLECT_BATCH_LIMIT)
        );
        assert!(first.cursor_at_stop.is_some(), "cursor persisted");
        assert_eq!(rec.get("rio_store_gc_collect_cycles_capped_total{}"), 1);
        let deleted_after_first: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            deleted_after_first, COLLECT_CYCLE_VICTIM_CAP as i64,
            "no soft-deletes beyond the cap; the remainder is untouched"
        );
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(f64::from(n) - COLLECT_CYCLE_VICTIM_CAP as f64),
            "backlog gauge decreased by the collected count"
        );

        // Cycle 2: resumes from the cursor and finishes the pass.
        let second = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("resume cycle");
        assert!(!second.cap_reached);
        assert_eq!(
            second.victims_collected,
            u64::from(n) - COLLECT_CYCLE_VICTIM_CAP,
            "the resume cycle collects exactly the remainder (no re-collection)"
        );
        assert!(second.cursor_at_stop.is_none(), "pass completed");
        assert!(
            COLLECT_CURSOR.get().is_none(),
            "pass completion resets the cursor"
        );
        let deleted_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(deleted_total, i64::from(n), "no skipped victims");
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending, i64::from(n), "every victim enqueued exactly once");
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(0.0),
            "backlog gauge reads 0 once the pass completes"
        );
        assert_eq!(rec.get("rio_store_gc_collect_cycles_capped_total{}"), 1);
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 3);
    }

    // r[verify store.gc.chunk-collect]
    /// Cursor loss is harmless: a cap stop followed by a simulated
    /// restart (process-local cursor lost) still drains the remainder
    /// on the next cycle — the candidate scan's `deleted = FALSE`
    /// conjunct skips the already-collected prefix.
    #[tokio::test]
    async fn live_cycle_cap_stop_survives_cursor_loss() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state();
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (COLLECT_CYCLE_VICTIM_CAP + 10) as u16;
        seed_collectable_chunks(&db.pool, n, 100).await;

        let first = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("capped cycle");
        assert!(first.cap_reached);

        // Simulated restart: the process-local cursor is gone.
        reset_collector_state();

        let second = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
        )
        .await
        .expect("post-restart cycle");
        assert_eq!(
            second.victims_collected,
            u64::from(n) - COLLECT_CYCLE_VICTIM_CAP,
            "the remainder still drains without the cursor"
        );
        let deleted_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(deleted_total, i64::from(n));
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending, i64::from(n), "no duplicate enqueues either");
    }
}
