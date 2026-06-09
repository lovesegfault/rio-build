//! Lazy chunk collector: liveness derived from the durable manifests.
//!
//! A collect cycle derives the set of live chunk hashes from every
//! existing manifest's `manifest_data.chunk_list` (any status — an
//! `'uploading'` placeholder counts exactly as its write-ahead upsert
//! counts it today) inside PostgreSQL, then soft-deletes + enqueues
//! the chunks that no manifest references and that are older than the
//! grace window measured from `GREATEST(created_at,
//! last_referenced_at)`. No maintained per-chunk counter exists or is
//! consulted for the eligibility decision.
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
//! polarity the design forbids (the retired decrement paths'
//! warn-and-skip behavior is exactly what the collector must NOT do).
//!
//! # Capped collect
//!
//! The live arm collects at most [`COLLECT_CYCLE_VICTIM_CAP`] victims
//! per cycle and carries a process-local keyset cursor across cycles
//! (durable in `gc_collect_state.cursor`, migration 090) so a backlog drains across cycles instead of
//! stretching one cycle past the GC-lock-held budget. A cycle that
//! stops at the cap leaves the remainder for the next cycle (the next
//! GC run's phase 3 or the daily backstop); stopping early only
//! retains garbage longer, never collects more.

use std::sync::Arc;
use std::sync::LazyLock;
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
/// cycle (run_gc phase 3 or the backstop) via the durable cursor;
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

/// Cadence of the backstop CHECK tick (bug_174): each replica wakes
/// hourly and asks the DURABLE row whether a live cycle is due
/// (`now() - last_live_cycle_at >= COLLECT_BACKSTOP_INTERVAL` on the
/// DB clock). COLLECT_BACKSTOP_INTERVAL is the cluster-wide cadence
/// bound; this is merely how often any replica looks — N replicas
/// checking hourly still yield at most one live backstop cycle per
/// interval, because the decision lives in `gc_collect_state`, not in
/// process timers.
#[cfg(not(test))]
pub(crate) const COLLECT_BACKSTOP_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Test override (mirrors COLLECT_BACKSTOP_INTERVAL's rationale).
#[cfg(test)]
pub(crate) const COLLECT_BACKSTOP_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Per-cycle cap on reaped (hard-deleted) tombstone rows
/// (merged_bug_336): same retention-erring shape as
/// [`COLLECT_CYCLE_VICTIM_CAP`] — stopping early only keeps tombstone
/// rows longer, never deletes more.
#[cfg(not(test))]
pub(crate) const REAP_CYCLE_CAP: u64 = 500_000;
/// Test override: two test-sized batches.
#[cfg(test)]
pub(crate) const REAP_CYCLE_CAP: u64 = 50;

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
/// `super::try_parse_unique_chunk_hashes` (the Rust parser remains
/// the definition of corrupt-vs-valid; the differential pinning test
/// holds the SQL expansion to it): version byte, 36-byte entry
/// alignment, `MAX_CHUNKS`. The version probe uses `substring` (empty
/// on an empty blob) rather than `get_byte` so a zero-length blob is
/// reported as malformed instead of erroring mid-scan. 36 = the
/// serialized entry size (32-byte BLAKE3 + u32 LE size), `\x01` = the
/// format version byte; both fixed by `rio_store::manifest`.
///
/// Shared with `gc::mark_scan_bench` (gate (b)).
/// THE statement builder over the corruption population
/// (merged_bug_170): one body owns the malformed-`chunk_list`
/// predicate AND the shadow-exclusion splice, so every statement over
/// that population (the validation count, the offenders listing)
/// shares the same `WHERE` by construction. Pre-fix the offenders
/// query carried its own un-excluded copy: with ≥10 excluded corrupt
/// manifests sorting earlier, `ORDER BY … LIMIT 10` could crowd out
/// the real survivor and point the operator at manifests the
/// simulated sweep deletes anyway — the count and the hashes came
/// from different populations.
fn corruption_population_sql(select: &str, shadow_excluded: bool, tail: &str) -> String {
    format!(
        "SELECT {select} \
           FROM manifest_data md \
           JOIN manifests m USING (store_path_hash) \
          WHERE (octet_length(md.chunk_list) < 1 \
             OR substring(md.chunk_list FROM 1 FOR 1) <> '\\x01'::bytea \
             OR (octet_length(md.chunk_list) - 1) % 36 <> 0 \
             OR (octet_length(md.chunk_list) - 1) / 36 > {max}) \
          {exclusion} {tail}",
        max = crate::manifest::MAX_CHUNKS,
        exclusion = if shadow_excluded {
            SHADOW_SWEPT_EXCLUSION_AND
        } else {
            ""
        },
    )
}

pub(crate) fn mark_validation_sql(shadow_excluded: bool) -> String {
    corruption_population_sql("COUNT(*)", shadow_excluded, "")
}

/// The offending `store_path_hash`es behind a failed validation pass —
/// fetched only on the abort path (the happy path never pays for it),
/// hex-encoded for the error log and the runbook's lookup query.
/// Same population as [`mark_validation_sql`] by construction.
/// Validation + expansion as ONE unit (merged_bug_147). The mark
/// expansion is total on arbitrary bytes (it floors a misaligned
/// length and slices garbage "hashes"), so expanding a population the
/// fail-closed validation did not cover turns corrupt input into
/// phantom mark entries. The expansion SQL is therefore PRIVATE to
/// [`ValidatedMark`], whose only mint site is
/// [`MarkStatements::validate`] over the SAME `shadow_excluded`
/// parameter: an expansion whose population was not first validated
/// under the identical exclusion does not typecheck.
struct MarkStatements {
    shadow_excluded: bool,
}

/// Outcome of the fail-closed validation pass.
enum MarkValidation {
    Valid(ValidatedMark),
    Malformed(i64),
}

impl MarkStatements {
    fn for_population(shadow_excluded: bool) -> Self {
        Self { shadow_excluded }
    }

    /// Run the fail-closed validation over exactly the population the
    /// paired expansion will cover.
    async fn validate(self, tx: &mut sqlx::PgConnection) -> Result<MarkValidation, sqlx::Error> {
        let malformed: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_sql(
            self.shadow_excluded,
        )))
        .fetch_one(&mut *tx)
        .await?;
        Ok(if malformed > 0 {
            MarkValidation::Malformed(malformed)
        } else {
            MarkValidation::Valid(ValidatedMark {
                shadow_excluded: self.shadow_excluded,
            })
        })
    }
}

/// Proof that the population about to be expanded passed the
/// fail-closed validation under the same exclusion parameter.
struct ValidatedMark {
    shadow_excluded: bool,
}

impl ValidatedMark {
    /// Materialize the mark product — the ONLY route to the expansion
    /// SQL. `on_commit_drop` scopes the temp table to the read
    /// transaction (shadow arms); the live arm persists it for the
    /// batch loop and drops it explicitly in the post-drain tail.
    async fn create(
        &self,
        tx: &mut sqlx::PgConnection,
        table: &str,
        on_commit_drop: bool,
    ) -> Result<(), sqlx::Error> {
        let expansion_body = MARK_EXPANSION_SQL
            .strip_prefix("CREATE TEMP TABLE live_chunks AS ")
            .expect("MARK_EXPANSION_SQL starts with the live_chunks CTAS prefix");
        let drop_clause = if on_commit_drop {
            " ON COMMIT DROP"
        } else {
            ""
        };
        let exclusion = if self.shadow_excluded {
            // The exclusion has no existing WHERE to extend (the
            // shared constant is a pure join) — append one.
            format!(" {SHADOW_SWEPT_EXCLUSION_WHERE}")
        } else {
            String::new()
        };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TEMP TABLE {table}{drop_clause} AS {expansion_body}{exclusion}"
        )))
        .execute(tx)
        .await?;
        Ok(())
    }
}

fn mark_validation_offenders_sql(shadow_excluded: bool) -> String {
    corruption_population_sql(
        "encode(md.store_path_hash, 'hex')",
        shadow_excluded,
        "ORDER BY md.store_path_hash LIMIT 10",
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
/// `live_chunks`) plus the grace term; no other liveness signal is
/// consulted.
///
/// Shared with `gc::mark_scan_bench` so the bench's EXPLAIN plan-shape
/// guard (design §5b gate (b)) exercises this exact statement, not a
/// copy.
// r[impl store.chunk.liveness-derived]
// r[impl store.chunk.grace-ttl+2]
pub(crate) static COLLECT_BATCH_SELECT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT c.blake3_hash FROM chunks c \
     WHERE c.blake3_hash > $1 \
       AND {not_deleted} \
       AND NOT EXISTS (SELECT 1 FROM live_chunks lc \
                        WHERE lc.blake3_hash = c.blake3_hash AND lc.blake3_hash > $1) \
       AND {grace} \
     ORDER BY c.blake3_hash \
     LIMIT $3",
        not_deleted = render_collect_not_deleted("c."),
        grace = render_collect_grace("c.")
    )
});

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
/// window. This is the predicate-swapped descendant of the retired
/// orphan-chunk sweep's batch UPDATE, which re-checked its full
/// liveness predicate the same way. The bind is the candidate scan's result,
/// which is already in ascending hash order.
///
/// Shared with `gc::mark_scan_bench` (gate (c) measurement runs and
/// the structural predicate pin exercise this exact statement).
// r[impl store.chunk.lock-order+2]
pub(crate) static COLLECT_BATCH_UPDATE_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE chunks SET deleted = TRUE, uploaded_at = NULL, deleted_at = now() \
     WHERE blake3_hash = ANY($1) AND {not_deleted} \
       AND {grace} \
     RETURNING blake3_hash, size",
        not_deleted = render_collect_not_deleted(""),
        grace = render_collect_grace("")
    )
});

/// The collect predicate's row-local conjuncts, rendered ONCE and
/// spliced into BOTH the candidate scan and the soft-delete UPDATE
/// (merged_bug_026's class fix applied to the collect pair): dropping
/// a row-local conjunct from either statement now requires deleting
/// the splice itself, which the predicate-pin battery and the bench's
/// EXPLAIN plan-shape guard both catch. The rendered text is
/// byte-identical to the pre-splice literals, so plan shapes are
/// unaffected.
fn render_collect_not_deleted(a: &str) -> String {
    format!("{a}deleted = FALSE")
}
/// `GREATEST(created_at, last_referenced_at) < $2` — the grace term;
/// see [`render_collect_not_deleted`].
fn render_collect_grace(a: &str) -> String {
    format!("GREATEST({a}created_at, {a}last_referenced_at) < $2::timestamptz")
}

/// The simulated-sweep exclusion (bug_199), spliced into the shadow
/// arm\'s validation (`AND`-form — the validation has a WHERE) and
/// expansion (`WHERE`-form — the shared expansion constant is a pure
/// join). The shared constants stay bare: the bench\'s EXPLAIN
/// plan-shape guard exercises the LIVE statements.
const SHADOW_SWEPT_EXCLUSION_AND: &str = "AND NOT EXISTS \
    (SELECT 1 FROM shadow_swept ss WHERE ss.store_path_hash = m.store_path_hash)";
/// `WHERE`-prefixed twin of [`SHADOW_SWEPT_EXCLUSION_AND`].
const SHADOW_SWEPT_EXCLUSION_WHERE: &str = "WHERE NOT EXISTS \
    (SELECT 1 FROM shadow_swept ss WHERE ss.store_path_hash = m.store_path_hash)";

/// The post-pass tombstone reap (merged_bug_336): hard-delete chunk
/// rows that are (a) soft-deleted at least the grace term ago —
/// `deleted_at`, stamped by [`COLLECT_BATCH_UPDATE_SQL`], NULLed by
/// the resurrect upsert — and (b) fully drained (no pending outbox
/// row; the conjunct keeps the drain\'s resurrect-skip exact). Runs
/// only after a COMPLETE pass (nothing eligible to collect remained),
/// batched and capped like the collect loop. `$1` = grace seconds,
/// `$2` = batch limit.
pub(crate) static REAP_BATCH_DELETE_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "DELETE FROM chunks WHERE blake3_hash IN ( \
       SELECT c.blake3_hash FROM chunks c \
        WHERE {inner} \
        ORDER BY c.blake3_hash LIMIT $2) \
       AND {outer}",
        inner = render_reap_row_local_pred("c."),
        outer = render_reap_row_local_pred("chunks.")
    )
});

/// The reap's row-local eligibility conjuncts (merged_bug_026),
/// rendered ONCE with `{a}` alias substitution into BOTH the
/// IN-subquery (`c.`) and the OUTER DELETE qual (`chunks.`).
///
/// EPQ soundness: under READ COMMITTED, a DELETE blocked on a
/// concurrent row lock re-evaluates ONLY its own outer WHERE on the
/// new row version (EvalPlanQual does not re-run the IN-subquery).
/// `deleted` / `deleted_at` are therefore the load-bearing re-checks —
/// a tombstone resurrected between the candidate snapshot and the lock
/// grant (deleted = FALSE, deleted_at NULL) fails the outer qual and
/// survives. The repeated `NOT EXISTS pending_s3_deletes` conjunct is
/// symmetry/defense (the drain's resurrect-skip makes it non-load-
/// bearing, but dropping it from one qual and not the other is exactly
/// the divergence class this splice forecloses).
fn render_reap_row_local_pred(a: &str) -> String {
    format!(
        "{a}deleted AND {a}deleted_at < now() - make_interval(secs => $1) \
          AND NOT EXISTS (SELECT 1 FROM pending_s3_deletes p \
                           WHERE p.blake3_hash = {a}blake3_hash)"
    )
}

/// Which arm of the collector runs (P6: code-staged, no config field).
///
/// The shadow arm CARRIES its sweep composition (bug_199): a dry-run
/// GC sweeps nothing, so the chunk estimate must be computed against
/// SIMULATED post-sweep state — the would-be-swept manifests are
/// excluded from the mark (their now-unreferenced chunks then count
/// as collectible, exactly as a live run would leave them). The
/// payload makes every shadow caller state that composition; an
/// estimate that structurally excludes the swept paths\' chunks is no
/// longer writable.
#[derive(Debug, Clone)]
pub(crate) enum CollectMode {
    /// Mark + report only. No row anywhere is modified. Used by
    /// dry-run GC runs (and available for diagnostics); also the only
    /// arm the pre-cutover release shipped. `simulated_swept` = the
    /// store_path_hashes the paired (dry-run) sweep WOULD have deleted
    /// ([`super::sweep::SweepOutcome::swept_paths`]); empty for a
    /// standalone observation cycle.
    Shadow { simulated_swept: Vec<Vec<u8>> },
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

// r[impl store.gc.observation-basis]
/// The durable observation of one collect cycle — REAL basis only
/// (bug_226). Private fields, module-private constructor: the sole
/// mint site is the real-basis computation inside [`collect_cycle`],
/// so a counterfactual (simulated-sweep-excluded) mark size or
/// backlog anchor is unrepresentable in the durable
/// `gc_collect_state` row by construction — `CycleCommit::{Shadow,
/// Live}` accept only this type.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DurableObservation {
    mark_set_size: i64,
    would_collect: i64,
    unmarked_backlog_seed: i64,
}

impl DurableObservation {
    /// Mint from the exclusion-free computation. NOT pub(crate): only
    /// collect_cycle's real-basis arm can call this.
    fn from_real_basis(mark_set_size: i64, would_collect: i64, unmarked_backlog_seed: i64) -> Self {
        Self {
            mark_set_size,
            would_collect,
            unmarked_backlog_seed,
        }
    }

    pub(crate) fn mark_set_size(&self) -> i64 {
        self.mark_set_size
    }

    /// Real would-collect count (shadow cycles; always 0 for live
    /// cycles — the live arm does not re-run the full anti-join count,
    /// and the Live commit does not consume it).
    pub(crate) fn would_collect(&self) -> i64 {
        self.would_collect
    }

    /// Coarse backlog seed: not-deleted rows MINUS the real mark, on
    /// the cycle snapshot (bug_306). An overestimate of the eligible
    /// set (within-grace unmarked rows are counted; they age in), used
    /// by [`super::state::CycleCommit::Live`] ONLY to establish an
    /// absent anchor -- an existing anchor (a dry run's precise
    /// would-collect, or a prior seed under decrement) is never
    /// overwritten. Non-optional BY TYPE: every constructor produces a
    /// seed, so the total-anchor obligation has no NULL path.
    pub(crate) fn unmarked_backlog_seed(&self) -> i64 {
        self.unmarked_backlog_seed
    }
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
    /// What the live drain proved about the keyspace (bug_174) — the
    /// ONE completion authority; `None` in shadow mode and on the
    /// parse-failure abort. The legacy `pass_complete`/`cursor_at_stop`
    /// views are derived accessors so they can never disagree with it.
    pub(crate) disposition: Option<PassDisposition>,
    /// Tombstone rows hard-deleted by the post-drain reap
    /// (merged_bug_336): drained, grace-aged tombstones, reaped on
    /// every live cycle bounded by [`REAP_CYCLE_CAP`] (bug_193).
    pub(crate) chunks_reaped: u64,
    /// Wall-clock of the cycle (snapshot through report/collect).
    pub(crate) cycle_seconds: f64,
    /// The real-basis observation the caller commits durably
    /// ([`super::state::CycleCommit`]). `None` on a parse-failure
    /// abort, and on a dry run whose REAL-BASIS validation found
    /// corruption inside the simulated-swept set (merged_bug_147: the
    /// durable observation is withheld; the preview lane still
    /// reports). The PREVIEW fields above stay simulated for dry runs
    /// (bug_199); this field is what may touch `gc_collect_state`
    /// (bug_226).
    pub(crate) durable: Option<DurableObservation>,
}

impl CollectReport {
    /// Derived view: did the live drain complete (full-scan OR
    /// resumed-remainder)? Shadow/abort arms answer false.
    pub(crate) fn pass_complete(&self) -> bool {
        self.disposition
            .as_ref()
            .is_some_and(PassDisposition::pass_complete)
    }

    /// Derived view: the resume cursor a capped stop persists.
    pub(crate) fn cursor_at_stop(&self) -> Option<&[u8]> {
        self.disposition
            .as_ref()
            .and_then(PassDisposition::cursor_at_stop)
    }
}

/// What the live drain proved about the keyspace — the ONLY authority
/// [`super::state::CycleCommit::Live`] accepts for the cursor/backlog
/// decision (bug_174). `CompleteFullScan` is constructible only from
/// an UNRESUMED pass that exhausted the candidate scan: a
/// cursor-resumed drain that exhausts the remainder proves nothing
/// about `[0, cursor)` under this cycle's mark (grace runs from
/// `GREATEST(created_at, last_referenced_at)`, so chunks below the
/// persisted cursor become eligible between cycles), so it resets the
/// cursor but can never anchor the durable backlog estimate at zero.
/// The tombstone reap is NOT disposition-gated (bug_193): its qual is
/// row-local, so it runs on every live cycle regardless of what the
/// scan proved.
// r[impl store.gc.completion-witness+2]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PassDisposition {
    /// An unresumed pass scanned the full keyspace under this mark.
    CompleteFullScan,
    /// A cursor-resumed pass exhausted `[cursor, top]` — completion of
    /// the REMAINDER, not of the keyspace.
    CompleteResumed,
    /// Stopped at the victim cap; the cursor persists for resumption.
    Capped { cursor_at_stop: Vec<u8> },
}

impl PassDisposition {
    pub(crate) fn pass_complete(&self) -> bool {
        !matches!(self, PassDisposition::Capped { .. })
    }

    pub(crate) fn cursor_at_stop(&self) -> Option<&[u8]> {
        match self {
            PassDisposition::Capped { cursor_at_stop } => Some(cursor_at_stop.as_slice()),
            _ => None,
        }
    }

    /// May this disposition anchor the durable backlog estimate at
    /// zero? True ONLY for the full-keyspace scan.
    pub(crate) fn anchors_backlog_zero(&self) -> bool {
        matches!(self, PassDisposition::CompleteFullScan)
    }
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
/// window cannot skew the report — every count is taken against the
/// same snapshot the verdict was computed on. The
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
/// reported in `cursor_at_stop` and persisted by the caller's CycleCommit; the next cycle resumes there).
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
    resume_cursor: Option<Vec<u8>>,
) -> Result<CollectReport, sqlx::Error> {
    let cycle_started = Instant::now();
    // The shadow arm\'s simulated-sweep exclusion set (None = live).
    let simulated_swept: Option<&[Vec<u8>]> = match &mode {
        CollectMode::Shadow { simulated_swept } => Some(simulated_swept.as_slice()),
        CollectMode::Live => None,
    };
    // SessionConn from the FIRST await (merged_bug_223): every exit
    // before the explicit release detaches, so the session temp table
    // can never ride a pooled connection back to a sibling task.
    let mut conn = super::lock::SessionConn::acquire(pool).await?;

    // One read phase = one transaction = one MVCC snapshot (see the
    // function doc). Not READ ONLY: PostgreSQL forbids CREATE TEMP
    // TABLE in read-only transactions. Every early `?` return drops
    // the Transaction, which queues a ROLLBACK — the SET LOCAL budget
    // and the (uncommitted) temp table die with it on every exit path
    // before the commit, so nothing leaks back into the shared pool.
    let mut tx = sqlx::Connection::begin(&mut **conn.conn()).await?;
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

    // --- Shadow arm: materialize the simulated-sweep exclusion set
    // (bug_199) --- ON COMMIT DROP: the table exists exactly as long
    // as the read transaction (the only statements that reference it).
    if let Some(swept) = simulated_swept {
        sqlx::query(
            "CREATE TEMP TABLE shadow_swept (store_path_hash BYTEA PRIMARY KEY) ON COMMIT DROP",
        )
        .execute(&mut *tx)
        .await?;
        if !swept.is_empty() {
            sqlx::query(
                "INSERT INTO shadow_swept \
                 SELECT * FROM UNNEST($1::bytea[]) ON CONFLICT DO NOTHING",
            )
            .bind(swept)
            .execute(&mut *tx)
            .await?;
        }
    }

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
    // Shadow: a corrupt manifest the simulated sweep removes must not
    // abort a dry run it would not abort live — the exclusion applies
    // to the validation exactly as it applies to the expansion, and
    // (merged_bug_170) to the offenders listing: count and hashes come
    // from the SAME population by construction (one statement builder).
    let shadow_excluded = simulated_swept.is_some();
    let preview_mark = match MarkStatements::for_population(shadow_excluded)
        .validate(&mut tx)
        .await?
    {
        MarkValidation::Valid(vm) => vm,
        MarkValidation::Malformed(malformed) => {
            let offenders: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(
                mark_validation_offenders_sql(shadow_excluded),
            ))
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
                victims_collected: 0,
                victim_bytes: 0,
                s3_keys_enqueued: 0,
                batches_run: 0,
                cap_reached: false,
                disposition: None,
                chunks_reaped: 0,
                cycle_seconds: cycle_started.elapsed().as_secs_f64(),
                durable: None,
            });
        }
    };

    // --- Mark (ii): server-side set-based expansion ---
    // Through the ValidatedMark ONLY (merged_bug_147): the expansion
    // covers exactly the population the validation above passed.
    // Shadow mode adds ON COMMIT DROP so the mark product cannot
    // outlive the read transaction on any exit path; the live arm's
    // batches anti-join against the table in their own transactions
    // after this one commits, so it is created without the clause and
    // dropped explicitly at the end of the cycle (with detach-on-error
    // covering the window in between). The shared constant stays free
    // of the clause: the bench's EXPLAIN plan-shape guard (gate (b))
    // runs it outside a transaction, where ON COMMIT DROP would drop
    // the table at statement end.
    preview_mark
        .create(&mut tx, "live_chunks", simulated_swept.is_some())
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

    // --- Report (shadow arm only): one pass over the not-deleted
    // chunk rows, on the cycle snapshot ---
    // would-collect: unmarked AND past grace (the collect predicate)
    // plus its byte sum. The live arm does NOT re-run the
    // would-collect anti-join count — that scan term is exactly what
    // the per-cycle cap exists to manage (P15); its backlog
    // visibility comes from the decremental estimate below.
    let (would_collect, would_collect_bytes): (i64, i64) = match simulated_swept {
        Some(_) => {
            sqlx::query_as(
                "SELECT \
                   COUNT(*) FILTER (WHERE NOT in_mark \
                                      AND GREATEST(created_at, last_referenced_at) < $1::timestamptz), \
                   COALESCE(SUM(size) FILTER (WHERE NOT in_mark \
                                      AND GREATEST(created_at, last_referenced_at) < $1::timestamptz), 0)::bigint \
                 FROM (SELECT c.size, c.created_at, c.last_referenced_at, \
                              EXISTS (SELECT 1 FROM live_chunks lc \
                                       WHERE lc.blake3_hash = c.blake3_hash) AS in_mark \
                         FROM chunks c \
                        WHERE c.deleted = FALSE) AS s",
            )
            .bind(&cutoff)
            .fetch_one(&mut *tx)
            .await?
        }
        None => (0, 0),
    };

    // --- Durable observation: REAL basis only (bug_226) ---
    // The durable row (and through it every replica's gauges and the
    // next cycle's cadence/backlog decisions) records what IS, not the
    // dry-run counterfactual. With a non-empty simulated sweep the
    // mark above is exclusion-filtered, so materialize a SECOND,
    // exclusion-free product on the same REPEATABLE READ snapshot
    // (2x mark cost, operator dry runs only) and count against it.
    // With an empty exclusion the product above IS the real basis.
    // bug_306: the seed term every arm shares -- one COUNT on the
    // cycle snapshot (index/seq scan over not-deleted rows, once per
    // cycle at the daily cadence; the shadow report and the mark
    // expansion are both heavier).
    let not_deleted_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted = FALSE")
            .fetch_one(&mut *tx)
            .await?;

    let durable = match simulated_swept {
        Some(swept) if !swept.is_empty() => {
            // The real basis covers ALL manifests — including the ones
            // the preview validation deliberately excluded — so it
            // gets its OWN fail-closed validation over the exclusion-
            // free population (merged_bug_147). Corruption inside the
            // simulated-swept set withholds the durable observation:
            // the dry run stays preview-only, the durable row is
            // untouched, and phantom hashes never reach
            // gc_collect_state.
            match MarkStatements::for_population(false)
                .validate(&mut tx)
                .await?
            {
                MarkValidation::Malformed(malformed) => {
                    let offenders: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(
                        mark_validation_offenders_sql(false),
                    ))
                    .fetch_all(&mut *tx)
                    .await?;
                    warn!(
                        malformed,
                        offenders = %offenders.join(","),
                        "chunk-collect: corrupt chunk_list inside the simulated-swept \
                         set; durable observation WITHHELD (preview-only dry run; the \
                         live cycle over this data will abort fail-closed until the \
                         manifest is repaired)"
                    );
                    None
                }
                MarkValidation::Valid(real_mark_stmt) => {
                    real_mark_stmt
                        .create(&mut tx, "live_chunks_real", true)
                        .await?;
                    let real_mark: i64 =
                        sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks_real")
                            .fetch_one(&mut *tx)
                            .await?;
                    let real_would: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM chunks c \
                          WHERE c.deleted = FALSE \
                            AND NOT EXISTS (SELECT 1 FROM live_chunks_real lc \
                                             WHERE lc.blake3_hash = c.blake3_hash) \
                            AND GREATEST(c.created_at, c.last_referenced_at) < $1::timestamptz",
                    )
                    .bind(&cutoff)
                    .fetch_one(&mut *tx)
                    .await?;
                    Some(DurableObservation::from_real_basis(
                        real_mark,
                        real_would,
                        (not_deleted_rows - real_mark).max(0),
                    ))
                }
            }
        }
        // Standalone shadow observation: no exclusion was applied —
        // the preview numbers ARE the real basis.
        Some(_) => Some(DurableObservation::from_real_basis(
            mark_set_size,
            would_collect,
            (not_deleted_rows - mark_set_size).max(0),
        )),
        // Live: the mark is exclusion-free by construction; the live
        // arm never computes a would-collect count.
        None => Some(DurableObservation::from_real_basis(
            mark_set_size,
            0,
            (not_deleted_rows - mark_set_size).max(0),
        )),
    };

    // Commit ends the cycle's snapshot. In shadow mode the temp table
    // dies here (ON COMMIT DROP) before the connection returns to the
    // pool; in live mode it survives for the batch loop below.
    tx.commit().await?;

    // Gauge publication moved off the cycle (merged_bug_211): the
    // observation lands in the durable row via the caller\'s
    // CycleCommit, and EVERY replica\'s gauge publisher reads it back
    // within 60s — the cycle winner is no longer the only pod whose
    // gauges move.

    if simulated_swept.is_some() {
        let cycle_seconds = cycle_started.elapsed().as_secs_f64();
        metrics::histogram!("rio_store_gc_collect_cycle_seconds").record(cycle_seconds);
        // outcome="ok" rides the caller's CycleCommitted witness
        // (merged_bug_218): a cycle is "ok" only once its commit lands.

        info!(
            mark_set_size,
            would_collect, cycle_seconds, "chunk-collect shadow cycle complete"
        );

        // merged_bug_170: the session is CLEAN here (every shadow temp
        // table was ON COMMIT DROP and the commit above ended the
        // transaction), so the connection goes back to the pool — the
        // pre-fix return dropped it (detach + close), burning a pooled
        // connection on every dry-run cycle.
        conn.release_to_pool();

        return Ok(CollectReport {
            outcome: CollectOutcome::Ok,
            mark_set_size: mark_set_size as u64,
            would_collect: would_collect as u64,
            would_collect_bytes: would_collect_bytes as u64,
            victims_collected: 0,
            victim_bytes: 0,
            s3_keys_enqueued: 0,
            batches_run: 0,
            cap_reached: false,
            disposition: None,
            chunks_reaped: 0,
            cycle_seconds,
            durable,
        });
    }

    // --- Live arm: the capped, cursor-resumable soft-delete loop ---
    // From here until the explicit temp-table drop the session carries
    // the mark product outside any transaction, so any exit that does
    // not reach the cleanup detaches (closes) the connection — the
    // temp table dies with the session instead of leaking into the
    // shared pool.
    let resumed = resume_cursor.is_some();
    let mut cursor: Vec<u8> = resume_cursor.unwrap_or_default();
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
        let mut batch_tx = sqlx::Connection::begin(&mut **conn.conn()).await?;
        let candidates: Vec<Vec<u8>> = sqlx::query_scalar(COLLECT_BATCH_SELECT_SQL.as_str())
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
        let collected: Vec<(Vec<u8>, i64)> = sqlx::query_as(COLLECT_BATCH_UPDATE_SQL.as_str())
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
                // No durable cursor write: a failed cycle commits
                // nothing to gc_collect_state — the next cycle
                // re-scans (the candidate scan\'s deleted = FALSE
                // conjunct skips the rows prior batches committed).
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

    // Completion bookkeeping (bug_174): the disposition is the ONE
    // authority — `CompleteFullScan` only from an unresumed pass, so a
    // cursor-resumed drain that exhausts the remainder commits a
    // cursor reset WITHOUT anchoring the backlog at zero (chunks below
    // the resume point that became eligible between cycles were never
    // scanned under this mark). A lost cursor is never a correctness
    // problem — the candidate scan's `deleted = FALSE` conjunct skips
    // already-collected rows.
    let disposition = if cap_reached {
        PassDisposition::Capped {
            cursor_at_stop: cursor,
        }
    } else if resumed {
        PassDisposition::CompleteResumed
    } else {
        debug_assert!(pass_complete, "uncapped live loop exits only on completion");
        PassDisposition::CompleteFullScan
    };

    // --- Post-drain tail (bug_137): reap + cleanup, error-contained
    // by SIGNATURE — the tail fn has no error channel, so a reap or
    // cleanup failure structurally cannot un-commit an already-drained
    // cycle (the pre-fix `?`s propagated AFTER every batch had
    // committed; the caller's Err arm skipped `commit_cycle`,
    // `last_live_cycle_at` stayed unstamped, and a persistent reap
    // failure re-ran the full mark expansion up to 24x/day against the
    // documented once-per-24h bound while the backlog estimate never
    // decremented).
    let chunks_reaped = run_post_drain_tail(conn, grace_secs).await;

    // Backlog visibility (P15) is durable now: the caller\'s
    // CycleCommit decrements the row estimate (full-scan completion ⇒
    // re-anchor at 0) and every replica\'s publisher converges on it.
    if cap_reached {
        metrics::counter!("rio_store_gc_collect_cycles_capped_total").increment(1);
    }

    let cycle_seconds = cycle_started.elapsed().as_secs_f64();
    metrics::histogram!("rio_store_gc_collect_cycle_seconds").record(cycle_seconds);
    // outcome="ok" rides the caller's CycleCommitted witness
    // (merged_bug_218): a cycle is "ok" only once its commit lands.

    info!(
        mark_set_size,
        victims_collected,
        victim_bytes,
        s3_keys_enqueued,
        batches_run,
        cap_reached,
        chunks_reaped,
        cycle_seconds,
        "chunk-collect live cycle complete"
    );

    Ok(CollectReport {
        outcome: CollectOutcome::Ok,
        mark_set_size: mark_set_size as u64,
        would_collect: 0,
        would_collect_bytes: 0,
        victims_collected,
        victim_bytes,
        s3_keys_enqueued,
        batches_run,
        cap_reached,
        disposition: Some(disposition),
        chunks_reaped,
        cycle_seconds,
        durable,
    })
}

/// Test-only injection: fail the next post-drain reap statement
/// (cleared on trip). Proves a tail failure cannot un-commit the
/// drained cycle (bug_137).
#[cfg(test)]
pub(crate) static REAP_FAIL_INJECT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The post-drain tail (bug_137): tombstone reap + mark-table cleanup,
/// strictly lower priority than the drain they trail. NO ERROR
/// CHANNEL — the signature returns the reap count and consumes the
/// session, so a failure here is structurally unable to fail the
/// cycle: reap errors warn + count and stop reaping; a cleanup error
/// detaches the session (the temp table dies with it, never in the
/// shared pool). The reap runs on EVERY live cycle, bounded by
/// [`REAP_CYCLE_CAP`] (bug_193): its DELETE qual is entirely
/// row-local — `deleted AND deleted_at < now() - grace AND NOT
/// EXISTS pending_s3_deletes` — so no scan-completion fact enters
/// the decision; gating it on the full-scan proof starved the reap
/// permanently under cap saturation (`CompleteFullScan` unreachable
/// while daily eligible-garbage production exceeds the victim cap)
/// and tombstones accumulated without bound. The full-scan proof
/// remains the backlog anchor's gate and only that
/// ([`PassDisposition::anchors_backlog_zero`]).
// r[impl store.gc.completion-witness+2]
async fn run_post_drain_tail(mut conn: super::lock::SessionConn, grace_secs: i64) -> u64 {
    let mut chunks_reaped: u64 = 0;
    loop {
        let remaining = REAP_CYCLE_CAP.saturating_sub(chunks_reaped);
        if remaining == 0 {
            break;
        }
        let limit = COLLECT_BATCH_LIMIT.min(remaining) as i64;
        #[cfg(test)]
        let injected: Result<sqlx::postgres::PgQueryResult, sqlx::Error> =
            if REAP_FAIL_INJECT.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Err(sqlx::Error::Protocol(
                    "chunk-collect: injected reap failure (test only)".into(),
                ))
            } else {
                sqlx::query(REAP_BATCH_DELETE_SQL.as_str())
                    .bind(grace_secs)
                    .bind(limit)
                    .execute(&mut **conn.conn())
                    .await
            };
        #[cfg(not(test))]
        let injected = sqlx::query(REAP_BATCH_DELETE_SQL.as_str())
            .bind(grace_secs)
            .bind(limit)
            .execute(&mut **conn.conn())
            .await;
        match injected {
            Ok(r) => {
                let reaped = r.rows_affected();
                if reaped == 0 {
                    break;
                }
                metrics::counter!("rio_store_gc_chunks_reaped_total").increment(reaped);
                chunks_reaped += reaped;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    chunks_reaped,
                    "post-drain tombstone reap failed (the drained cycle still \
                     commits; the next live cycle retries the reap)"
                );
                metrics::counter!(
                    "rio_store_gc_collect_tail_errors_total",
                    "stage" => "reap"
                )
                .increment(1);
                break;
            }
        }
    }

    // Cleanup: drop the mark product in-session, then hand the (clean)
    // connection back to the pool. On failure the session is detached
    // instead — the temp table dies with it, never in the shared pool.
    match sqlx::query("DROP TABLE IF EXISTS live_chunks")
        .execute(&mut **conn.conn())
        .await
    {
        Ok(_) => conn.release_to_pool(),
        Err(e) => {
            warn!(
                error = %e,
                "mark-table cleanup failed; detaching the cycle session \
                 (the temp table dies with the session)"
            );
            metrics::counter!(
                "rio_store_gc_collect_tail_errors_total",
                "stage" => "cleanup"
            )
            .increment(1);
            drop(conn);
        }
    }
    chunks_reaped
}

/// Run one backstop CHECK (bug_174): consult the DURABLE row for
/// whether a live cycle is due (`now() - last_live_cycle_at >=`
/// [`COLLECT_BACKSTOP_INTERVAL`], DB clock), and only then take the
/// cycle lease and run. Double-checked: the cheap unlocked pre-read
/// keeps N replicas\' hourly ticks nearly free; the due re-check under
/// the lock makes the decision exact (two replicas passing the
/// pre-check race to one lease; the loser skips; the winner re-checks
/// so a cycle that JUST committed is not repeated). Returns `Ok(None)`
/// when not due or when another holder has the lease.
// r[impl store.gc.collect-cadence]
pub(crate) async fn collect_backstop_once(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    grace_secs: i64,
) -> Result<Option<CollectReport>, sqlx::Error> {
    if !super::state::backstop_due_unlocked(pool, COLLECT_BACKSTOP_INTERVAL).await? {
        return Ok(None);
    }
    let Some(mut lease) = super::state::GcCycleLease::try_acquire(pool).await? else {
        info!("chunk-collect backstop: GC already running, skipping this tick");
        return Ok(None);
    };
    if !lease.backstop_due(COLLECT_BACKSTOP_INTERVAL).await? {
        // Lost the pre-check race to a cycle that just committed (or
        // to a recent ATTEMPT — bug_284's throttle conjunct).
        lease.release().await?;
        return Ok(None);
    }

    // bug_284: stamp the attempt BEFORE the cycle runs, so every
    // outcome arm (Ok, ParseFailure, Err) inherits the throttle — a
    // persistent fail-closed abort re-runs the heavy validation scan
    // once per backstop interval, not once per hourly check.
    lease.stamp_attempt().await?;

    let resume_cursor = lease.state.cursor.clone();
    match collect_cycle(
        pool,
        chunk_backend,
        grace_secs,
        CollectMode::Live,
        resume_cursor,
    )
    .await
    {
        Ok(report) => {
            match report.outcome {
                CollectOutcome::Ok => {
                    // Stamp the durable row: the cluster ran its live
                    // cycle; every replica\'s next hourly check sees it.
                    // The ok tick rides the commit witness — a lost
                    // commit ticks commit_failed, never ok
                    // (merged_bug_218); the C3 attempt stamp already
                    // throttles the re-run.
                    match lease
                        .commit_cycle(super::state::CycleCommit::Live {
                            disposition: report
                                .disposition
                                .clone()
                                .expect("live Ok report carries a disposition"),
                            victims_collected: report.victims_collected,
                            observation: report.durable.expect("Ok cycle carries an observation"),
                        })
                        .await
                    {
                        Ok(committed) => committed.record_ok_outcome(),
                        Err(e) => {
                            metrics::counter!(
                                "rio_store_gc_collect_cycles_total",
                                "outcome" => "commit_failed"
                            )
                            .increment(1);
                            warn!(
                                error = %e,
                                "chunk-collect backstop: cycle drained but the \
                                 commit was lost (stamp/cursor/backlog not updated)"
                            );
                        }
                    }
                }
                CollectOutcome::ParseFailure => {
                    // Fail-closed: an aborted cycle is NOT a live cycle
                    // — no stamp, retention stays visibly stalled until
                    // the corrupt manifest is repaired.
                    lease.release().await?;
                }
            }
            Ok(Some(report))
        }
        Err(e) => {
            // A DB-error cycle would otherwise be invisible to metrics
            // for up to a full backstop interval (the parse-failure
            // abort carries its own outcome and is NOT an error here).
            // run_gc phase 3 counts its own failures, so the outcomes
            // partition.
            metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "error")
                .increment(1);
            // lease drops here: detach frees the lock with the session.
            Err(e)
        }
    }
}

/// Spawn the hourly backstop CHECK tick. Errors are logged and the
/// next tick retries (`MissedTickBehavior::Skip`, like the other
/// periodic GC tasks).
///
/// The tick is a cheap durable-row read; the heavy cycle runs only
/// when the CLUSTER is due (bug_174: pre-090 each replica armed its
/// own daily `interval_at(boot + 24h)` timer, so N replicas ⇒ up to N
/// heavy cycles/day — mutual exclusion, not rate limiting). The first
/// check fires one full check-interval after spawn: the collect cycle
/// is the heaviest query pattern in the system (full manifest_data
/// expansion + chunks anti-join, multi-GB temp spill at the design
/// point — invariant map T-1a.1b/T-1a.1c), and rolling deploys,
/// scale-outs, and crash-loops must not even CHECK on every pod boot
/// — exactly the moments the database is already under stress. A
/// fleet where neither this nor run_gc phase 3 completes a cycle is
/// detected by the `RioStoreGcCollectStalled` alert.
pub fn spawn_collect_backstop(
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + COLLECT_BACKSTOP_CHECK_INTERVAL,
        COLLECT_BACKSTOP_CHECK_INTERVAL,
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

    /// Reset the durable collector state row (cursor + backlog +
    /// stamps) to its 090 initial form, so every live-arm test starts
    /// from a known state regardless of what earlier cycles in the
    /// same TestDb committed.
    async fn reset_collector_state(pool: &sqlx::PgPool) {
        sqlx::query(
            "UPDATE gc_collect_state SET cycle_epoch = 0, last_live_cycle_at = NULL, \
             cursor = NULL, backlog_estimate = NULL, last_mark_set_size = NULL, \
             last_would_collect = NULL, last_attempt_at = NULL WHERE singleton",
        )
        .execute(pool)
        .await
        .expect("reset gc_collect_state");
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
    /// chunk is would-collect; and the shadow cycle modifies nothing
    /// anywhere.
    #[tokio::test]
    async fn shadow_cycle_reports_fold_and_modifies_nothing() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Chunk A: referenced by a complete manifest.
        let a = ChunkSeed::new(0xA1).uploaded().seed(&db.pool).await;
        // Chunk B: referenced ONLY by an 'uploading' placeholder.
        let b = ChunkSeed::new(0xB2).seed(&db.pool).await;
        // Chunk C: unreferenced and old -> would-collect.
        ChunkSeed::new(0xC3).age_secs(3600).seed(&db.pool).await;
        // Chunk D: referenced by the complete manifest.
        let d = ChunkSeed::new(0xD4).uploaded().seed(&db.pool).await;

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
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
        )
        .await
        .expect("shadow cycle");

        assert_eq!(report.outcome, CollectOutcome::Ok);
        assert_eq!(report.mark_set_size, 3, "a, b, d are referenced (dedup'd)");
        assert_eq!(report.would_collect, 1, "only the old unreferenced chunk");
        assert_eq!(report.victims_collected, 0);
        assert_eq!(report.batches_run, 0);
        assert!(!report.cap_reached);
        assert!(report.cursor_at_stop().is_none());

        // merged_bug_211: gauges no longer move on the cycle — the
        // caller commits the observation to gc_collect_state and EVERY
        // replica's publisher reads it back. Simulate the caller +
        // one publisher tick.
        let lease = super::super::state::GcCycleLease::try_acquire(&db.pool)
            .await
            .unwrap()
            .expect("lock free");
        lease
            .commit_cycle(super::super::state::CycleCommit::Shadow {
                observation: report.durable.expect("Ok cycle carries an observation"),
            })
            .await
            .unwrap()
            .record_ok_outcome();
        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        super::super::state::publish_gauges(&row);
        assert_eq!(rec.gauge_value("rio_store_gc_chunks_live{}"), Some(3.0));
        assert_eq!(
            rec.gauge_value("rio_store_gc_chunks_would_collect{}"),
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
        let malformed: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_sql(false)))
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
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
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
        let good = ChunkSeed::new(0x10).uploaded().seed(&db.pool).await;
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
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
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
        let h = ChunkSeed::new(0x91).uploaded().seed(&db.pool).await;
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
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
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
        let h = ChunkSeed::new(0x92).uploaded().seed(&db.pool).await;
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
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
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
        let h = ChunkSeed::new(0x77).uploaded().seed(&db.pool).await;
        seed_chunked_manifest(&db.pool, "leak-a", "complete", &make_chunk_list(&[h])).await;

        for _ in 0..2 {
            let report = collect_cycle(
                &db.pool,
                None,
                super::super::sweep::CHUNK_GRACE_SECS,
                CollectMode::Shadow {
                    simulated_swept: Vec::new(),
                },
                None,
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
        reset_collector_state(&db.pool).await;
        let h = ChunkSeed::new(0x42).uploaded().seed(&db.pool).await;
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
        // run_gc committed the cycle to the durable row; the publisher
        // (any replica) renders it.
        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        assert_eq!(row.last_mark_set_size, Some(1));
        super::super::state::publish_gauges(&row);
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
        reset_collector_state(&db.pool).await;
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

    // r[verify store.gc.dry-run+3]
    /// bug_199 differential: on identical seeds, the DRY-RUN estimate
    /// (shadow cycle fed the dry sweep\'s settled swept set) equals
    /// what a REAL run actually collects. The seed: an unreachable
    /// closure whose manifests exclusively reference past-grace
    /// chunks — pre-fix RED: the dry run reported 0 for them (the
    /// rolled-back sweep left the manifests in place, so their chunks
    /// stayed marked live).
    #[tokio::test]
    async fn dry_run_estimate_matches_real_collection() {
        // --- World A: dry run ---
        let db_a = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db_a.pool).await;
        // --- World B: identical seeds, real run ---
        let db_b = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db_b.pool).await;

        for db in [&db_a, &db_b] {
            // An unreachable manifest exclusively referencing two old
            // chunks (the would-be-swept closure). Backdate the path so
            // the mark phase (grace_hours=1) finds it unreachable.
            let c1 = ChunkSeed::new(0xF1)
                .age_secs(7200)
                .uploaded()
                .seed(&db.pool)
                .await;
            let c2 = ChunkSeed::new(0xF2)
                .age_secs(7200)
                .uploaded()
                .seed(&db.pool)
                .await;
            seed_chunked_manifest(
                &db.pool,
                "unreachable-root",
                "complete",
                &make_chunk_list(&[c1, c2]),
            )
            .await;
            sqlx::query("UPDATE narinfo SET created_at = now() - interval '3 hours'")
                .execute(&db.pool)
                .await
                .unwrap();
        }

        let params = super::super::GcParams {
            dry_run: true,
            grace_hours: 1,
            extra_roots: vec![],
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let dry = super::super::run_gc(
            &db_a.pool,
            None,
            params,
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .expect("dry run_gc")
        .expect("lock free");
        while rx.recv().await.is_some() {}

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let real = super::super::run_gc(
            &db_b.pool,
            None,
            super::super::GcParams {
                dry_run: false,
                grace_hours: 1,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .expect("real run_gc")
        .expect("lock free");
        while rx.recv().await.is_some() {}

        assert_eq!(
            dry.paths_deleted, real.paths_deleted,
            "path estimates agree"
        );
        assert!(real.chunks_deleted >= 2, "the closure\'s chunks collected");
        assert_eq!(
            dry.chunks_deleted, real.chunks_deleted,
            "the dry-run chunk estimate equals the real collection \
             (pre-fix RED: dry reported 0 for swept manifests\' chunks)"
        );
        assert_eq!(dry.bytes_freed, real.bytes_freed, "byte estimates agree");
        // And the dry-run world is untouched.
        let deleted_a: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted")
            .fetch_one(&db_a.pool)
            .await
            .unwrap();
        assert_eq!(deleted_a, 0, "dry run modified nothing");
    }

    /// bug_199 unit: Shadow{[h]} excludes h\'s manifest from the mark —
    /// its exclusively-referenced chunk counts as collectible; an empty
    /// exclusion preserves the old observation semantics.
    #[tokio::test]
    async fn shadow_exclusion_unit() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let c = ChunkSeed::new(0xF3)
            .age_secs(7200)
            .uploaded()
            .seed(&db.pool)
            .await;
        seed_chunked_manifest(&db.pool, "excl-path", "complete", &make_chunk_list(&[c])).await;
        let h: Vec<u8> = sqlx::query_scalar("SELECT store_path_hash FROM manifests LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();

        let keep = collect_cycle(
            &db.pool,
            None,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
        )
        .await
        .expect("baseline shadow");
        assert_eq!(keep.would_collect, 0, "referenced chunk not collectible");

        let excl = collect_cycle(
            &db.pool,
            None,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: vec![h],
            },
            None,
        )
        .await
        .expect("excluding shadow");
        assert_eq!(
            excl.would_collect, 1,
            "excluding the manifest exposes its chunk as collectible"
        );
    }

    /// bug_226: the DURABLE row must anchor the REAL basis — the
    /// dry-run preview stays simulated (bug_199), but what lands in
    /// gc_collect_state (and from there every replica's gauges and
    /// the next cycle's backlog estimate) is the exclusion-free
    /// observation. Three referenced chunks, two of their manifests
    /// simulated-swept: preview mark=1/would=2; durable mark=3/would=0.
    /// RED (recorded, pre-fix routing of the simulated numbers into
    /// the commit): durable carried 1/2 — the counterfactual.
    // r[verify store.gc.observation-basis]
    #[tokio::test]
    async fn dry_run_commit_anchors_real_basis() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let mut hashes = Vec::new();
        for (i, name) in [(0xA1u8, "real-a"), (0xA2, "real-b"), (0xA3, "real-c")] {
            let c = ChunkSeed::new(i)
                .age_secs(7200)
                .uploaded()
                .seed(&db.pool)
                .await;
            let h = seed_chunked_manifest(&db.pool, name, "complete", &make_chunk_list(&[c])).await;
            hashes.push(h);
        }

        let report = collect_cycle(
            &db.pool,
            None,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: hashes[..2].to_vec(),
            },
            None,
        )
        .await
        .expect("excluding shadow cycle");

        // Preview: simulated post-sweep state (bug_199 preserved).
        assert_eq!(
            report.mark_set_size, 1,
            "preview mark is exclusion-filtered"
        );
        assert_eq!(
            report.would_collect, 2,
            "preview counts the would-be-freed chunks"
        );

        // Durable: real basis.
        let durable = report.durable.expect("Ok cycle carries an observation");
        assert_eq!(durable.mark_set_size(), 3, "durable mark is the REAL mark");
        assert_eq!(
            durable.would_collect(),
            0,
            "durable backlog anchor is the REAL count"
        );
    }

    /// bug_199: a CORRUPT manifest the simulated sweep removes must
    /// not abort a dry run it would not abort live — the validation
    /// applies the same exclusion as the expansion.
    #[tokio::test]
    async fn corrupt_swept_manifest_does_not_abort_shadow() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        // A chunked manifest whose chunk_list we then CORRUPT in place.
        let c = ChunkSeed::new(0xF7).uploaded().seed(&db.pool).await;
        seed_chunked_manifest(&db.pool, "corrupt-path", "complete", &make_chunk_list(&[c])).await;
        let h: Vec<u8> = sqlx::query_scalar("SELECT store_path_hash FROM manifests LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE manifest_data SET chunk_list = $2 WHERE store_path_hash = $1")
            .bind(&h)
            .bind(vec![0xFFu8; 7]) // wrong version byte + misaligned
            .execute(&db.pool)
            .await
            .unwrap();

        // Unexcluded: the fail-closed validation aborts.
        let aborted = collect_cycle(
            &db.pool,
            None,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
        )
        .await
        .expect("cycle runs");
        assert_eq!(aborted.outcome, CollectOutcome::ParseFailure);

        // Swept-excluded: the dry run proceeds (a live run after the
        // real sweep would not see this manifest either).
        let ok = collect_cycle(
            &db.pool,
            None,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: vec![h],
            },
            None,
        )
        .await
        .expect("cycle runs");
        assert_eq!(
            ok.outcome,
            CollectOutcome::Ok,
            "a corrupt manifest the sweep removes cannot abort the dry run"
        );
    }

    /// merged_bug_147: the real-basis expansion (dry runs with a
    /// non-empty simulated sweep) covers ALL manifests -- including
    /// the corrupt one the preview validation deliberately excluded.
    /// MARK_EXPANSION_SQL is total on arbitrary bytes (it floors a
    /// misaligned length and slices garbage 32-byte "hashes"), so the
    /// pre-fix code committed a DurableObservation built over phantom
    /// hashes: live-count wrong, real referenced chunks counted as
    /// collectible. The expansion is now obtainable only through
    /// ValidatedMark (validation and expansion paired in one builder,
    /// parameterized identically); when the exclusion-free validation
    /// finds corruption, the durable observation is WITHHELD -- the
    /// dry run stays preview-only and the durable row is untouched.
    #[tokio::test]
    async fn dry_run_with_corrupt_swept_manifest_withholds_durable() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let c = ChunkSeed::new(0xF8).uploaded().seed(&db.pool).await;
        seed_chunked_manifest(&db.pool, "corrupt-real", "complete", &make_chunk_list(&[c])).await;
        let h: Vec<u8> = sqlx::query_scalar("SELECT store_path_hash FROM manifests LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE manifest_data SET chunk_list = $2 WHERE store_path_hash = $1")
            .bind(&h)
            .bind(vec![0xFFu8; 7])
            .execute(&db.pool)
            .await
            .unwrap();

        let report = collect_cycle(
            &db.pool,
            None,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: vec![h],
            },
            None,
        )
        .await
        .expect("cycle runs");
        assert_eq!(report.outcome, CollectOutcome::Ok, "preview still served");
        assert!(
            report.durable.is_none(),
            "corrupt manifest inside simulated_swept: the durable \
             observation is withheld, never built over phantom hashes"
        );
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

    // r[verify store.gc.collect-cadence]
    /// bug_174: the backstop is CLUSTER-cadenced via the durable row.
    /// Two consecutive checks ⇒ the first runs a live cycle (never-ran
    /// cluster: last_live_cycle_at NULL = due) and stamps the row; the
    /// second SKIPS (not due). Pre-090 RED: both ran — every replica\'s
    /// boot timer fired its own heavy cycle (mutual exclusion, not
    /// rate limiting).
    #[tokio::test]
    async fn backstop_check_is_cluster_cadenced() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let first = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .unwrap();
        assert!(first.is_some(), "never-ran cluster: first check is due");
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 1);

        let second = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .unwrap();
        assert!(
            second.is_none(),
            "the cluster just ran a live cycle — a second replica\'s check skips"
        );
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"),
            1,
            "exactly one cycle cluster-wide"
        );

        // Backdate BOTH stamps past the interval ⇒ due again
        // (bug_284: the due predicate is success-stale AND
        // attempt-stale; a cluster interval-old on both runs).
        sqlx::query(
            "UPDATE gc_collect_state SET \
               last_live_cycle_at = now() - make_interval(secs => $1), \
               last_attempt_at   = now() - make_interval(secs => $1) \
             WHERE singleton",
        )
        .bind(COLLECT_BACKSTOP_INTERVAL.as_secs() as f64 + 5.0)
        .execute(&db.pool)
        .await
        .unwrap();
        let third = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .unwrap();
        assert!(
            third.is_some(),
            "past the interval the backstop is due again"
        );
    }

    // r[verify store.gc.collect-cadence]
    /// A live run_gc stamps the durable row, so the backstop\'s next
    /// check skips: GC-schedule runs and the backstop share ONE
    /// cluster cadence.
    #[tokio::test]
    async fn backstop_skips_after_run_gc_live_cycle() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        super::super::run_gc(
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

        let check = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .unwrap();
        assert!(
            check.is_none(),
            "run_gc\'s live cycle satisfied the cluster cadence"
        );
    }

    /// merged_bug_211: the publisher renders ONLY non-NULL row fields —
    /// an unanchored cluster leaves the pre-registered 0 standing
    /// instead of inventing a number.
    #[tokio::test]
    async fn gauge_publisher_skips_null_fields() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        super::super::state::publish_gauges(&row);
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            None,
            "NULL backlog ⇒ gauge untouched"
        );
        assert_eq!(rec.gauge_value("rio_store_gc_chunks_live{}"), None);

        sqlx::query(
            "UPDATE gc_collect_state SET backlog_estimate = 7, last_mark_set_size = 3, \
             last_would_collect = 7 WHERE singleton",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        super::super::state::publish_gauges(&row);
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(7.0)
        );
        assert_eq!(rec.gauge_value("rio_store_gc_chunks_live{}"), Some(3.0));
        assert_eq!(
            rec.gauge_value("rio_store_gc_chunks_would_collect{}"),
            Some(7.0)
        );
    }

    // r[verify store.gc.bounded-garbage-retention+3]
    /// merged_bug_336 reap battery: a soft-deleted, drained, past-grace
    /// tombstone is hard-deleted by the post-pass reap; pending-outbox,
    /// young, and resurrected rows are all kept; the counter emits.
    /// Pre-091 RED: no reaper existed — tombstones were permanent.
    #[tokio::test]
    async fn pass_complete_reap_deletes_drained_aged_tombstones_only() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        // (1) Reapable: deleted, aged past grace, no outbox row.
        let reapable = ChunkSeed::new(0xE1).uploaded().seed(&db.pool).await;
        // (2) Pending outbox: deleted + aged, but the drain has not
        //     finished (pending_s3_deletes row exists).
        let pending = ChunkSeed::new(0xE2).uploaded().seed(&db.pool).await;
        // (3) Young tombstone: deleted recently.
        let young = ChunkSeed::new(0xE3).uploaded().seed(&db.pool).await;
        // (4) Resurrected: was deleted, then re-referenced (deleted
        //     FALSE, deleted_at NULL via the upsert).
        let resurrected = ChunkSeed::new(0xE4).uploaded().seed(&db.pool).await;

        let grace = super::super::sweep::CHUNK_GRACE_SECS;
        for (h, age, outbox) in [
            (&reapable, grace + 100, false),
            (&pending, grace + 100, true),
            (&young, 10, false),
        ] {
            sqlx::query(
                "UPDATE chunks SET deleted = TRUE, \
                 deleted_at = now() - make_interval(secs => $2) \
                 WHERE blake3_hash = $1",
            )
            .bind(&h[..])
            .bind(age as f64)
            .execute(&db.pool)
            .await
            .unwrap();
            if outbox {
                sqlx::query("INSERT INTO pending_s3_deletes (blake3_hash, s3_key) VALUES ($1, $2)")
                    .bind(&h[..])
                    .bind("k/pending")
                    .execute(&db.pool)
                    .await
                    .unwrap();
            }
        }
        // The resurrected row: tombstone state fully cleared.
        sqlx::query("UPDATE chunks SET deleted = FALSE, deleted_at = NULL WHERE blake3_hash = $1")
            .bind(&resurrected[..])
            .execute(&db.pool)
            .await
            .unwrap();

        // A live cycle over an otherwise-clean keyspace: completes the
        // pass (nothing left to collect except our seeds, which are
        // either deleted already or referenced/young) and reaps.
        let report = collect_cycle(&db.pool, Some(&backend), grace, CollectMode::Live, None)
            .await
            .expect("live cycle");
        assert!(report.pass_complete(), "clean keyspace ⇒ pass completes");
        assert_eq!(
            report.chunks_reaped, 1,
            "exactly the drained aged tombstone"
        );
        assert_eq!(rec.get("rio_store_gc_chunks_reaped_total{}"), 1);

        let remaining: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT blake3_hash FROM chunks ORDER BY blake3_hash")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert!(
            !remaining.contains(&reapable.to_vec()),
            "the drained aged tombstone is GONE"
        );
        assert!(remaining.contains(&pending.to_vec()), "pending outbox kept");
        assert!(remaining.contains(&young.to_vec()), "young tombstone kept");
        assert!(
            remaining.contains(&resurrected.to_vec()),
            "resurrected row never reaped"
        );
    }

    /// merged_bug_218: outcome="ok" must mean "the durable commit
    /// landed". Pre-fix the ok tick ran inside collect_cycle BEFORE
    /// commit_cycle; a commit failure (the lock connection sits idle
    /// through the whole multi-minute cycle -- exactly what
    /// pgbouncer/NLB idle killers sever) propagated to warn-only
    /// handlers: metrics green, stamp/cursor/backlog lost, and the
    /// next backstop re-ran the full mark expansion.
    #[tokio::test]
    async fn failing_commit_ticks_commit_failed_not_ok() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        seed_collectable_chunks(&db.pool, 3, 100).await;

        // Fail the primary commit AND the epoch-guarded retry.
        super::super::state::COMMIT_FAIL_INJECT.store(2, std::sync::atomic::Ordering::SeqCst);
        let report = collect_backstop_once(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
        )
        .await
        .expect("backstop check runs")
        .expect("due -> cycle ran");
        assert_eq!(
            report.outcome,
            CollectOutcome::Ok,
            "the cycle itself drained"
        );

        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"),
            0,
            "a cycle whose commit was lost must NOT tick ok"
        );
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=commit_failed}"),
            1,
            "the lost commit is visible as commit_failed"
        );
        let live_null: bool =
            sqlx::query_scalar("SELECT last_live_cycle_at IS NULL FROM gc_collect_state")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(live_null, "no stamp landed (metrics agree with the row)");
    }

    /// merged_bug_218 resilience half: when only the lock session is
    /// dead (the common idle-kill), the epoch-guarded retry on a fresh
    /// pooled connection lands the commit -- ok ticks, stamp present,
    /// epoch advanced exactly once.
    #[tokio::test]
    async fn commit_retries_epoch_guarded_on_fresh_connection() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        seed_collectable_chunks(&db.pool, 3, 100).await;

        // Fail only the primary (lock-session) commit.
        super::super::state::COMMIT_FAIL_INJECT.store(1, std::sync::atomic::Ordering::SeqCst);
        let _ = collect_backstop_once(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
        )
        .await
        .expect("backstop check runs")
        .expect("due -> cycle ran");

        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"),
            1,
            "the retried commit landed -> ok"
        );
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=commit_failed}"),
            0
        );
        let (epoch, live_set): (i64, bool) = sqlx::query_as(
            "SELECT cycle_epoch, last_live_cycle_at IS NOT NULL FROM gc_collect_state",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(epoch, 1, "exactly one committed cycle");
        assert!(live_set, "the stamp landed via the retry");
    }

    /// bug_284: a persistent fail-closed abort (corrupt chunk_list)
    /// must not re-run the heavy validation scan on every backstop
    /// CHECK tick. The attempt stamp is written before the cycle, so
    /// two due-checks inside one backstop interval run exactly ONE
    /// cycle; pre-fix the due predicate keyed only on
    /// last_live_cycle_at (never stamped by an abort), so every
    /// hourly check re-ran the aborted cycle: 24 heavy scans/day
    /// against the documented once-per-24h cadence, until a human
    /// repaired the manifest.
    #[tokio::test]
    async fn backstop_throttles_repeat_attempts_after_parse_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        // One manifest with a corrupt chunk_list: every cycle aborts
        // fail-closed and commits nothing.
        seed_chunked_manifest(&db.pool, "corrupt-attempt", "complete", &[0xFFu8; 7]).await;

        let first = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .expect("first check runs")
            .expect("due -> a cycle ran");
        assert_eq!(first.outcome, CollectOutcome::ParseFailure);

        // Second check, still inside the backstop interval: the
        // attempt stamp throttles it -- no second heavy cycle.
        let second = collect_backstop_once(&db.pool, None, super::super::sweep::CHUNK_GRACE_SECS)
            .await
            .expect("second check runs");
        assert!(
            second.is_none(),
            "an attempt inside the interval is throttled (got a second cycle)"
        );
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=parse_failure}"),
            1,
            "two due-checks inside the interval run exactly ONE cycle"
        );

        // The abort did NOT masquerade as a live cycle: the success
        // stamp stays NULL (the stalled alert's signal), only the
        // attempt stamp moved.
        let (live_null, attempt_set): (bool, bool) = sqlx::query_as(
            "SELECT last_live_cycle_at IS NULL, last_attempt_at IS NOT NULL \
             FROM gc_collect_state WHERE singleton",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(live_null, "ParseFailure never stamps the live cadence");
        assert!(attempt_set, "the attempt stamp is durable");
    }

    /// bug_306: live-only operation must anchor the backlog estimate.
    /// Pre-fix the Live commit's `NULL THEN NULL` arm meant only a
    /// dry-run (Shadow) commit could ever establish the anchor, so on
    /// a cluster that never dry-runs, `rio_store_gc_collect_backlog_chunks`
    /// read the boot 0 through an entire multi-day capped drain while
    /// cycles_capped ticked -- the contradictory pair the gauge exists
    /// to resolve. Every CycleCommit now carries an unmarked-rows seed
    /// (non-optional on DurableObservation: the anchor obligation is
    /// total by type) and the Live CASE seeds it when no anchor exists.
    #[tokio::test]
    async fn capped_live_cycle_seeds_backlog_anchor() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // Backlog larger than the cap; NO shadow cycle ever runs.
        let n = (COLLECT_CYCLE_VICTIM_CAP + 10) as u16;
        seed_collectable_chunks(&db.pool, n, 100).await;

        // The production path: the backstop runs the live cycle and
        // commits it (last_live_cycle_at is NULL, so it is due).
        let report = collect_backstop_once(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
        )
        .await
        .expect("backstop cycle")
        .expect("due -> a cycle ran");
        assert!(report.cap_reached, "the cycle stops at the cap");

        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        let anchored = row
            .backlog_estimate
            .expect("a capped live cycle anchors a backlog estimate (bug_306)");
        assert!(
            anchored > 0,
            "the anchor reflects the unmarked remainder, got {anchored}"
        );
    }

    /// bug_193: the tombstone reap's qual is entirely row-local
    /// (deleted + aged past grace + drained), so it MUST run on every
    /// live cycle bounded by REAP_CYCLE_CAP — a capped or
    /// cursor-resumed cycle reaps exactly like a full scan. Pre-fix
    /// the reap was gated on `anchors_backlog_zero()` (full scan
    /// only): under cap saturation `CompleteFullScan` is unreachable
    /// and tombstones accumulate without bound while
    /// `rio_store_gc_chunks_reaped_total` flatlines.
    #[tokio::test]
    async fn capped_and_resumed_cycles_reap_row_local_tombstones() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let grace = super::super::sweep::CHUNK_GRACE_SECS;

        let seed_reapable = |pool: sqlx::PgPool, tag: u8| async move {
            let h = ChunkSeed::new(tag).uploaded().seed(&pool).await;
            sqlx::query(
                "UPDATE chunks SET deleted = TRUE, \
                 deleted_at = now() - make_interval(secs => $2) \
                 WHERE blake3_hash = $1",
            )
            .bind(&h[..])
            .bind((grace + 100) as f64)
            .execute(&pool)
            .await
            .unwrap();
            h
        };

        // A backlog larger than the cap, so cycle 1 stops Capped.
        let n = (COLLECT_CYCLE_VICTIM_CAP + 10) as u16;
        seed_collectable_chunks(&db.pool, n, 100).await;
        let reapable_1 = seed_reapable(db.pool.clone(), 0xF1).await;

        // Cycle 1: unresumed, caps — and still reaps the row-local
        // eligible tombstone.
        let report = collect_cycle(&db.pool, Some(&backend), grace, CollectMode::Live, None)
            .await
            .expect("capped cycle");
        assert!(report.cap_reached, "cycle 1 stops at the cap");
        assert_eq!(
            report.chunks_reaped, 1,
            "a CAPPED cycle reaps the drained aged tombstone (row-local qual)"
        );

        // Cycle 2: cursor-resumed completion — also reaps.
        let reapable_2 = seed_reapable(db.pool.clone(), 0xF2).await;
        let report2 = collect_cycle(
            &db.pool,
            Some(&backend),
            grace,
            CollectMode::Live,
            report.cursor_at_stop().map(<[u8]>::to_vec),
        )
        .await
        .expect("resumed cycle");
        assert_eq!(
            report2.disposition,
            Some(PassDisposition::CompleteResumed),
            "cycle 2 exhausts the remainder from the cursor"
        );
        assert_eq!(
            report2.chunks_reaped, 1,
            "a RESUMED completion reaps the drained aged tombstone"
        );

        let remaining: Vec<Vec<u8>> = sqlx::query_scalar("SELECT blake3_hash FROM chunks")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert!(!remaining.contains(&reapable_1.to_vec()));
        assert!(!remaining.contains(&reapable_2.to_vec()));
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

    #[tokio::test]
    async fn live_cycle_collects_unreferenced_chunk_exactly_once() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // The leaked chunk: unreferenced, past grace.
        let leaked = ChunkSeed::new(0xB1)
            .with_size(4096)
            .age_secs(3600)
            .uploaded()
            .seed(&db.pool)
            .await;
        // A referenced control chunk that must survive.
        let live = ChunkSeed::new(0xA7).uploaded().seed(&db.pool).await;
        seed_chunked_manifest(&db.pool, "leak-live", "complete", &make_chunk_list(&[live])).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
            None,
        )
        .await
        .expect("live cycle");

        assert_eq!(report.outcome, CollectOutcome::Ok);
        assert_eq!(report.victims_collected, 1, "exactly the leaked chunk");
        assert_eq!(report.victim_bytes, 4096);
        assert_eq!(report.s3_keys_enqueued, 1);
        assert!(!report.cap_reached);
        assert!(report.cursor_at_stop().is_none(), "pass completed");

        let (deleted, uploaded_cleared): (bool, bool) = sqlx::query_as(
            "SELECT deleted, uploaded_at IS NULL FROM chunks WHERE blake3_hash = $1",
        )
        .bind(&leaked[..])
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(deleted, "unreferenced chunk soft-deleted");
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
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"),
            0,
            "merged_bug_218: ok rides the caller's commit witness; a bare \
             collect_cycle (no commit) ticks nothing"
        );
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
            None,
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
                "INSERT INTO chunks (blake3_hash, size, created_at, uploaded_at) \
                 VALUES ($1, $2, now() - interval '1 hour', now() - interval '1 hour')",
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
    // r[verify store.chunk.grace-ttl+2]
    /// The live arm's protection partition: a chunk referenced only by
    /// an `'uploading'` placeholder, a younger-than-grace chunk, and an
    /// old chunk with a recent `last_referenced_at` touch all survive a
    /// live cycle; the old, untouched, unreferenced control is
    /// collected (so the survivals are not vacuous).
    #[tokio::test]
    async fn live_cycle_spares_uploading_grace_and_touched() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
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
            None,
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
        reset_collector_state(&db.pool).await;
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
            None,
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
        reset_collector_state(&db.pool).await;
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
            None,
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
        let needs_upload = crate::metadata::upgrade_manifest_to_chunked(
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
        let deleted: bool = sqlx::query_scalar("SELECT deleted FROM chunks WHERE blake3_hash = $1")
            .bind(&hash[..])
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(!deleted, "upsert flips deleted back to false");

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
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (COLLECT_BATCH_LIMIT + 5) as u16; // 2 batches: full + short
        seed_collectable_chunks(&db.pool, n, 100).await;

        let report = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
            None,
        )
        .await
        .expect("live cycle");

        assert_eq!(report.victims_collected, u64::from(n), "all collected");
        assert_eq!(report.batches_run, 2, "one full batch + one short batch");
        assert!(!report.cap_reached);
        assert!(report.cursor_at_stop().is_none(), "pass completed");
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
        reset_collector_state(&db.pool).await;
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
            None,
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
            None,
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
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (COLLECT_CYCLE_VICTIM_CAP + 10) as u16; // 60 with the test consts
        seed_collectable_chunks(&db.pool, n, 100).await;

        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);

        // Anchor the backlog estimate the way production does: a
        // shadow (dry-run) cycle reports the would-collect count and
        // its caller commits the observation to the durable row.
        let shadow = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: Vec::new(),
            },
            None,
        )
        .await
        .expect("anchor cycle");
        assert_eq!(shadow.would_collect, u64::from(n));
        let lease = super::super::state::GcCycleLease::try_acquire(&db.pool)
            .await
            .unwrap()
            .expect("lock free");
        lease
            .commit_cycle(super::super::state::CycleCommit::Shadow {
                observation: shadow.durable.expect("Ok cycle carries an observation"),
            })
            .await
            .unwrap()
            .record_ok_outcome();
        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        super::super::state::publish_gauges(&row);
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(f64::from(n))
        );

        // Cycle 1 (replica A): lease -> run with the durable cursor
        // (None: fresh keyspace) -> commit, exactly the production
        // choreography.
        let lease_a = super::super::state::GcCycleLease::try_acquire(&db.pool)
            .await
            .unwrap()
            .expect("lock free");
        let cursor_a = lease_a.state.cursor.clone();
        assert!(cursor_a.is_none(), "fresh row: cursor at keyspace start");
        let first = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
            cursor_a,
        )
        .await
        .expect("capped cycle");
        assert!(first.cap_reached);
        assert_eq!(first.victims_collected, COLLECT_CYCLE_VICTIM_CAP);
        assert_eq!(
            first.batches_run,
            COLLECT_CYCLE_VICTIM_CAP.div_ceil(COLLECT_BATCH_LIMIT)
        );
        assert!(
            first.cursor_at_stop().is_some(),
            "cursor reported at the cap stop"
        );
        lease_a
            .commit_cycle(super::super::state::CycleCommit::Live {
                disposition: first
                    .disposition
                    .clone()
                    .expect("live Ok report carries a disposition"),
                victims_collected: first.victims_collected,
                observation: first.durable.expect("Ok cycle carries an observation"),
            })
            .await
            .unwrap()
            .record_ok_outcome();
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
        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        super::super::state::publish_gauges(&row);
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(f64::from(n) - COLLECT_CYCLE_VICTIM_CAP as f64),
            "durable backlog decreased by the collected count"
        );

        // Cycle 2 (replica B — a FRESH lease on a different "process"):
        // resumes from the DURABLE cursor and finishes the pass.
        // Pre-090 RED: the cursor was a process static, so a different
        // replica restarted from scratch.
        let lease_b = super::super::state::GcCycleLease::try_acquire(&db.pool)
            .await
            .unwrap()
            .expect("lock free");
        let cursor_b = lease_b.state.cursor.clone();
        assert!(
            cursor_b.is_some(),
            "replica B resumes from the durable stop point"
        );
        assert_eq!(
            cursor_b.as_deref(),
            first.cursor_at_stop(),
            "round-trip exact"
        );
        let second = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
            cursor_b,
        )
        .await
        .expect("resume cycle");
        assert!(!second.cap_reached);
        assert_eq!(
            second.victims_collected,
            u64::from(n) - COLLECT_CYCLE_VICTIM_CAP,
            "the resume cycle collects exactly the remainder (no re-collection)"
        );
        assert!(second.cursor_at_stop().is_none(), "pass completed");
        lease_b
            .commit_cycle(super::super::state::CycleCommit::Live {
                disposition: second
                    .disposition
                    .clone()
                    .expect("live Ok report carries a disposition"),
                victims_collected: second.victims_collected,
                observation: second.durable.expect("Ok cycle carries an observation"),
            })
            .await
            .unwrap()
            .record_ok_outcome();
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
        let row = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            row.cursor, None,
            "pass completion clears the durable cursor"
        );
        super::super::state::publish_gauges(&row);
        assert_eq!(
            rec.gauge_value("rio_store_gc_collect_backlog_chunks{}"),
            Some(0.0),
            "durable backlog reads 0 once the pass completes"
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
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let n = (COLLECT_CYCLE_VICTIM_CAP + 10) as u16;
        seed_collectable_chunks(&db.pool, n, 100).await;

        let first = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
            None,
        )
        .await
        .expect("capped cycle");
        assert!(first.cap_reached);

        // Simulated restart: the process-local cursor is gone.
        reset_collector_state(&db.pool).await;

        let second = collect_cycle(
            &db.pool,
            Some(&backend),
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Live,
            None,
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

    /// merged_bug_026 splice pin: the rendered statements are
    /// byte-identical to the pre-splice literals (plan shapes
    /// unchanged), and the reap's OUTER DELETE qual carries every
    /// row-local eligibility conjunct (the EPQ re-check set).
    #[test]
    fn reap_and_collect_quals_carry_row_local_conjuncts() {
        assert_eq!(
            COLLECT_BATCH_SELECT_SQL.as_str(),
            "SELECT c.blake3_hash FROM chunks c \
     WHERE c.blake3_hash > $1 \
       AND c.deleted = FALSE \
       AND NOT EXISTS (SELECT 1 FROM live_chunks lc \
                        WHERE lc.blake3_hash = c.blake3_hash AND lc.blake3_hash > $1) \
       AND GREATEST(c.created_at, c.last_referenced_at) < $2::timestamptz \
     ORDER BY c.blake3_hash \
     LIMIT $3",
        );
        assert_eq!(
            COLLECT_BATCH_UPDATE_SQL.as_str(),
            "UPDATE chunks SET deleted = TRUE, uploaded_at = NULL, deleted_at = now() \
     WHERE blake3_hash = ANY($1) AND deleted = FALSE \
       AND GREATEST(created_at, last_referenced_at) < $2::timestamptz \
     RETURNING blake3_hash, size",
        );
        // The outer DELETE qual (after the IN (...) close paren) must
        // carry the full row-local predicate, chunks-qualified.
        let outer = REAP_BATCH_DELETE_SQL
            .rsplit_once("LIMIT $2)")
            .expect("reap shape")
            .1;
        for conjunct in [
            "chunks.deleted",
            "chunks.deleted_at < now() - make_interval(secs => $1)",
            "p.blake3_hash = chunks.blake3_hash",
        ] {
            assert!(
                outer.contains(conjunct),
                "reap OUTER qual lost row-local conjunct {conjunct:?}: {outer}"
            );
        }
    }

    // r[verify store.gc.bounded-garbage-retention+3]
    /// merged_bug_026: a tombstone resurrected AFTER the reap's
    /// candidate snapshot but BEFORE its row lock is granted must
    /// survive the EvalPlanQual re-check. Connection A resurrects the
    /// row in an open transaction (holding the row lock); the reap
    /// DELETE on connection B blocks on that lock (observed via
    /// pg_stat_activity); A commits; B's EPQ re-evaluates the OUTER
    /// qual on the new row version.
    ///
    /// RED (pre-fix, recorded): the outer qual was hash-only — the
    /// resurrected (deleted = FALSE) live row was hard-deleted.
    #[tokio::test]
    async fn reap_resurrected_row_survives_epq_recheck() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let grace = super::super::sweep::CHUNK_GRACE_SECS;

        let hash = ChunkSeed::new(0xEC).uploaded().seed(&db.pool).await;
        sqlx::query(
            "UPDATE chunks SET deleted = TRUE, \
             deleted_at = now() - make_interval(secs => $2) \
             WHERE blake3_hash = $1",
        )
        .bind(&hash[..])
        .bind((grace + 100) as f64)
        .execute(&db.pool)
        .await
        .unwrap();

        // A: resurrect in an open tx — row lock held, uncommitted.
        let mut conn_a = db.pool.acquire().await.unwrap();
        let mut tx_a = sqlx::Connection::begin(&mut *conn_a).await.unwrap();
        sqlx::query("UPDATE chunks SET deleted = FALSE, deleted_at = NULL WHERE blake3_hash = $1")
            .bind(&hash[..])
            .execute(&mut *tx_a)
            .await
            .unwrap();

        // B: the reap DELETE — blocks on A's row lock.
        let pool_b = db.pool.clone();
        let reap = tokio::spawn(async move {
            sqlx::query(REAP_BATCH_DELETE_SQL.as_str())
                .bind(grace as f64)
                .bind(10i64)
                .execute(&pool_b)
                .await
                .map(|r| r.rows_affected())
        });

        // Lock-wait gate: wait until B is provably blocked on a lock.
        let mut waited = false;
        for _ in 0..200 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE wait_event_type = 'Lock' AND query LIKE 'DELETE FROM chunks%'",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            if waiting > 0 {
                waited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(waited, "reap DELETE never blocked on the resurrect lock");

        tx_a.commit().await.unwrap();
        let reaped = reap.await.unwrap().expect("reap statement");
        assert_eq!(reaped, 0, "EPQ re-check must veto the resurrected row");

        let alive: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chunks WHERE blake3_hash = $1 AND NOT deleted",
        )
        .bind(&hash[..])
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(alive, 1, "resurrected row must survive the reap");
    }

    /// bug_174 (recorded red, pre-fix): a cursor-RESUMED pass that
    /// exhausts the remainder set `pass_complete = true`, which (a)
    /// re-anchored the durable backlog estimate at 0 and (b) ran the
    /// tombstone reap — both on a cycle that never scanned
    /// `[0, cursor)` under its mark. Post-fix the typed
    /// `PassDisposition::CompleteResumed` resets the cursor, KEEPS the
    /// decremented estimate, and skips the reap; only an unresumed
    /// full-keyspace scan anchors zero.
    // r[verify store.gc.completion-witness+2]
    #[tokio::test]
    async fn resumed_completion_keeps_decremented_backlog_and_reaps() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let grace = super::super::sweep::CHUNK_GRACE_SECS;

        // Eligible chunks straddling the resume cursor: `low` is below
        // it (skipped by the resumed scan), `high` above (drained).
        let low = ChunkSeed::new(0x10)
            .age_secs(grace + 100)
            .uploaded()
            .seed(&db.pool)
            .await;
        let _high = ChunkSeed::new(0xF0)
            .age_secs(grace + 100)
            .uploaded()
            .seed(&db.pool)
            .await;
        // A drained, aged tombstone a (false) completion would reap.
        let tombstone = ChunkSeed::new(0x20).uploaded().seed(&db.pool).await;
        sqlx::query(
            "UPDATE chunks SET deleted = TRUE, \
             deleted_at = now() - make_interval(secs => $2) \
             WHERE blake3_hash = $1",
        )
        .bind(&tombstone[..])
        .bind((grace + 100) as f64)
        .execute(&db.pool)
        .await
        .unwrap();

        // Durable state: a mid-keyspace resume cursor and a known
        // backlog estimate; no live stamp, so the backstop is due.
        sqlx::query(
            "UPDATE gc_collect_state SET cursor = $1, backlog_estimate = 7 \
             WHERE singleton",
        )
        .bind(vec![0x80u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();

        let report = collect_backstop_once(&db.pool, Some(&backend), grace)
            .await
            .expect("backstop")
            .expect("due ⇒ the cycle ran");

        assert_eq!(report.victims_collected, 1, "drained the remainder (high)");
        assert!(
            matches!(report.disposition, Some(PassDisposition::CompleteResumed)),
            "resumed pass exhausting the remainder is CompleteResumed, got {:?}",
            report.disposition
        );
        assert!(report.pass_complete(), "derived view: the drain completed");
        assert_eq!(
            report.chunks_reaped, 1,
            "bug_193: the reap is row-local, so a resumed completion reaps \
             (the cursor proof gates only the backlog anchor)"
        );

        let state = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        assert_eq!(state.cursor, None, "completion resets the cursor");
        assert_eq!(
            state.backlog_estimate,
            Some(6),
            "decremented (7 - 1 victim), never the zero anchor"
        );

        // The skipped-below-cursor chunk and the tombstone both survive.
        let still_there: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chunks WHERE blake3_hash = $1 AND deleted = FALSE",
        )
        .bind(&low[..])
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(still_there, 1, "below-cursor chunk was not scanned");
        let tomb: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE blake3_hash = $1")
            .bind(&tombstone[..])
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            tomb, 0,
            "tombstone reaped on the resumed completion (bug_193)"
        );

        // And the NEXT (unresumed) cycle anchors zero: the full-scan
        // completion remains the only ZERO-ANCHOR authority (the reap
        // already ran -- nothing left for it here). Clear BOTH stamps
        // (bug_284: the due predicate is also attempt-gated).
        sqlx::query(
            "UPDATE gc_collect_state SET last_live_cycle_at = NULL, \
             last_attempt_at = NULL WHERE singleton",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let second = collect_backstop_once(&db.pool, Some(&backend), grace)
            .await
            .expect("backstop")
            .expect("due again");
        assert!(matches!(
            second.disposition,
            Some(PassDisposition::CompleteFullScan)
        ));
        assert_eq!(second.victims_collected, 1, "low drained from the top");
        assert_eq!(
            second.chunks_reaped, 0,
            "nothing left to reap (the resumed cycle already did, bug_193)"
        );
        let state = super::super::state::read_state_unlocked(&db.pool)
            .await
            .unwrap();
        assert_eq!(state.backlog_estimate, Some(0), "full scan anchors zero");
    }

    /// bug_137 (strawman-disclosed red: the injection hook lands WITH
    /// the containment; reverting `run_post_drain_tail` to `?`
    /// propagation turns this red — outcome=error, stamp missing): a
    /// post-drain tail failure must not un-commit the drained cycle.
    // r[verify store.gc.completion-witness+2]
    #[tokio::test]
    async fn tail_failure_cannot_uncommit_the_drained_cycle() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        reset_collector_state(&db.pool).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let grace = super::super::sweep::CHUNK_GRACE_SECS;

        // A reapable tombstone so the reap loop runs and trips the
        // injected failure.
        let tombstone = ChunkSeed::new(0x33).uploaded().seed(&db.pool).await;
        sqlx::query(
            "UPDATE chunks SET deleted = TRUE, \
             deleted_at = now() - make_interval(secs => $2) \
             WHERE blake3_hash = $1",
        )
        .bind(&tombstone[..])
        .bind((grace + 100) as f64)
        .execute(&db.pool)
        .await
        .unwrap();

        REAP_FAIL_INJECT.store(true, std::sync::atomic::Ordering::SeqCst);
        let report = collect_backstop_once(&db.pool, Some(&backend), grace)
            .await
            .expect("the cycle must NOT propagate the tail failure")
            .expect("due ⇒ the cycle ran");

        assert!(matches!(report.outcome, CollectOutcome::Ok));
        assert_eq!(report.chunks_reaped, 0, "the reap failed and was contained");
        assert_eq!(
            rec.get("rio_store_gc_collect_tail_errors_total{stage=reap}"),
            1
        );
        assert_eq!(rec.get("rio_store_gc_collect_cycles_total{outcome=ok}"), 1);
        assert_eq!(
            rec.get("rio_store_gc_collect_cycles_total{outcome=error}"),
            0
        );

        // THE point: the durable stamp landed despite the tail failure
        // — the cadence bound holds and the backstop is no longer due.
        let stamped: bool =
            sqlx::query_scalar("SELECT last_live_cycle_at IS NOT NULL FROM gc_collect_state")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(stamped, "the drained cycle committed its stamp");
        assert!(
            collect_backstop_once(&db.pool, Some(&backend), grace)
                .await
                .unwrap()
                .is_none(),
            "not due again — no 24x/day re-mark"
        );
    }

    /// merged_bug_170 (a): the offenders listing and the validation
    /// count come from ONE statement builder, so the shadow exclusion
    /// applies to both — count and hashes describe the same
    /// population. (Strawman-disclosed red: reverting the offenders
    /// query to the un-excluded form lists the excluded manifest and
    /// breaks the count/listing agreement.)
    #[tokio::test]
    async fn offender_listing_shares_the_shadow_exclusion() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Two corrupt manifests: `inside` will be in the simulated
        // sweep's exclusion set, `outside` is the real survivor.
        let inside =
            seed_chunked_manifest(&db.pool, "/nix/store/m170-inside", "complete", b"").await;
        let outside =
            seed_chunked_manifest(&db.pool, "/nix/store/m170-outside", "complete", b"").await;

        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query(
            "CREATE TEMP TABLE shadow_swept (store_path_hash BYTEA PRIMARY KEY) ON COMMIT DROP",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO shadow_swept VALUES ($1)")
            .bind(&inside)
            .execute(&mut *tx)
            .await
            .unwrap();

        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_sql(true)))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let offenders: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_offenders_sql(true)))
                .fetch_all(&mut *tx)
                .await
                .unwrap();
        assert_eq!(count, 1, "validation count excludes the swept manifest");
        assert_eq!(
            offenders,
            vec![hex::encode(&outside)],
            "offenders list the SAME population as the count"
        );
        // Un-excluded form: both corrupt manifests, for contrast.
        let all: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(mark_validation_offenders_sql(false)))
                .fetch_all(&mut *tx)
                .await
                .unwrap();
        assert_eq!(all.len(), 2);
        drop(tx);
    }

    /// merged_bug_170 (b): the shadow-arm success return releases the
    /// CLEAN session back to the pool instead of detaching (closing)
    /// it on every dry-run cycle. (Strawman-disclosed red: reverting
    /// the `release_to_pool` drops the pool size by one per shadow
    /// cycle.)
    #[tokio::test]
    async fn shadow_cycle_releases_the_clean_session() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Warm exactly one pooled connection and settle.
        {
            let c = db.pool.acquire().await.unwrap();
            drop(c);
        }
        let size_before = db.pool.size();
        let report = collect_cycle(
            &db.pool,
            None,
            super::super::sweep::CHUNK_GRACE_SECS,
            CollectMode::Shadow {
                simulated_swept: vec![],
            },
            None,
        )
        .await
        .expect("shadow cycle");
        assert!(matches!(report.outcome, CollectOutcome::Ok));
        // Settle: a detached connection closes asynchronously; poll
        // briefly so a pre-fix run reliably shows the shrink.
        let mut size_after = db.pool.size();
        for _ in 0..20 {
            size_after = db.pool.size();
            if size_after == size_before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            size_after, size_before,
            "the clean shadow session goes back to the pool, not detached"
        );
        assert!(db.pool.num_idle() >= 1, "the released connection is idle");
    }
}
