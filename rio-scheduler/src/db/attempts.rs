//! The durable attempt ledger — `drv_attempts` table (migration 066).
//!
//! One row per attempt or reset event in a derivation's failure
//! history, keyed by the DAG key (`derivations.derivation_id`). The
//! scheduler is the only reader and writer; retention is
//! scheduler-owned (deliberately NOT under rio-store's log TTL sweep).
//!
//! Write discipline (Phase 1a): every appending site owns one PG
//! transaction — append the attempt row(s), move the site's existing
//! status persist onto the `_in_tx` variants in `db/derivations.rs`,
//! commit — and pushes the in-memory [`AttemptRecord`] mirror only
//! after the commit. Controller-reported attempts are two-installment:
//! the disconnect appends the row (`outcome_class = 'disconnected'`,
//! `termination_reason` NULL) and the later classifying report fills
//! it via [`SchedulerDb::fill_termination`] — an UPDATE guarded
//! `WHERE termination_reason IS NULL`, never a second INSERT. The
//! partial unique index on `exec_id` makes one-row-per-execution a
//! schema property regardless of arrival order.
//!
//! Nothing reads these rows for decisions in Phase 1a: the RAM
//! counters in `RetryState` stay authoritative until the Phase-1b
//! collapse routes the nine entry points through `decide()`.

use std::collections::HashMap;

use sqlx::PgConnection;
use uuid::Uuid;

use super::SchedulerDb;
use crate::state::{
    AttemptEventKind, AttemptRecord, ExecutorId, OutcomeClass, POISON_TTL, ReportingParty,
};

/// Phase-1 ledger retention floor (decision P8): there is NO TTL sweep
/// on `drv_attempts` yet, so retention is effectively unbounded and the
/// suffix bound (rows since the last reset event) is what keeps reads
/// O(per-cycle attempts). Any future sweep MUST retain rows for at
/// least this long — the largest decision window computed as a fold
/// over ledger rows: the infra retry window
/// (`RetryPolicy::infra_retry_window_secs`, 300 s default — re-check
/// against the configured value when a sweep is added) and the 24 h
/// poison TTL.
// TODO: consult this floor from the (Phase-2) ledger GC sweep; until a
// sweep exists it is the documentation hook only.
#[allow(dead_code)]
pub(crate) const LEDGER_RETENTION_FLOOR: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

// Compile-time guard for the retention floor: it must dominate the
// poison TTL and the (default) 300 s infra retry window. If either
// constant grows past the floor, this stops compiling and the floor —
// and any sweep that was sized off it — must be revisited.
const _: () = assert!(
    LEDGER_RETENTION_FLOOR.as_secs() >= POISON_TTL.as_secs()
        && LEDGER_RETENTION_FLOOR.as_secs() >= 300
);

/// One `drv_attempts` row, as written by the appending sites and as
/// read back by [`SchedulerDb::load_attempt_suffix`]. Field-for-field
/// the migration-066 schema; `recorded_at_epoch_secs` is PG-assigned
/// (`DEFAULT now()`) and meaningful only on rows read back from the
/// ledger.
#[derive(Debug, Clone)]
pub(crate) struct AttemptRow {
    /// Primary key (UUIDv7, minted at construction).
    pub attempt_id: Uuid,
    /// The DAG key (`derivations.derivation_id`).
    pub derivation_id: Uuid,
    /// Execution this attempt corresponds to, when one was dispatched.
    /// Covered by the partial unique index — at most one row per
    /// execution.
    pub exec_id: Option<Uuid>,
    /// Executor that ran (or was assigned) the attempt.
    pub executor_id: Option<ExecutorId>,
    /// Attempt event or reset event.
    pub event_kind: AttemptEventKind,
    /// Outcome classification (the `classify()` alphabet; CHECK-bound).
    pub outcome_class: OutcomeClass,
    /// Second-installment classification detail; NULL until established.
    pub termination_reason: Option<String>,
    /// Who observed the event.
    pub reporting_party: ReportingParty,
    /// E2's `exempt_from_cap` at append time.
    pub exempt: bool,
    /// `FloorOutcome::promoted` at append time.
    pub floor_promoted: bool,
    /// `FloorOutcome::at_cap` at append time.
    pub floor_at_cap: bool,
    /// Worker/controller error message, where the path carries one.
    pub error_msg: Option<String>,
    /// `CompletionReport.final_line_count` for report-bearing failures.
    pub final_line_count: Option<i64>,
    /// Resubmit cycle index this row belongs to.
    pub resubmit_cycle: i32,
    /// When the event occurred (epoch seconds, append-site clock).
    pub occurred_at_epoch_secs: f64,
    /// When the row was committed (epoch seconds, PG clock). 0.0 until
    /// the row has been read back from the ledger.
    pub recorded_at_epoch_secs: f64,
}

impl AttemptRow {
    /// A new attempt-event row for `derivation_id` with a freshly
    /// minted UUIDv7 `attempt_id`, `occurred_at` = now, and every
    /// optional column empty. Callers fill the fields the path knows
    /// (exec_id, executor, floor flags, error message, …) before
    /// appending.
    pub(crate) fn new(
        derivation_id: Uuid,
        outcome_class: OutcomeClass,
        reporting_party: ReportingParty,
    ) -> Self {
        Self {
            attempt_id: Uuid::now_v7(),
            derivation_id,
            exec_id: None,
            executor_id: None,
            event_kind: AttemptEventKind::Attempt,
            outcome_class,
            termination_reason: None,
            reporting_party,
            exempt: false,
            floor_promoted: false,
            floor_at_cap: false,
            error_msg: None,
            final_line_count: None,
            resubmit_cycle: 0,
            occurred_at_epoch_secs: epoch_now(),
            recorded_at_epoch_secs: 0.0,
        }
    }

    /// A new reset-event row (`event_kind = 'reset'`). `outcome_class`
    /// must be one of the reset classes (`resubmit_reset`,
    /// `cache_hit_clear`, `poison_cleared`); `resubmit_cycle` carries
    /// the cycle index the reset starts.
    // TODO: callers land with the reset-event sites (T-1a.6).
    #[allow(dead_code)]
    pub(crate) fn new_reset(
        derivation_id: Uuid,
        outcome_class: OutcomeClass,
        reporting_party: ReportingParty,
        resubmit_cycle: i32,
    ) -> Self {
        Self {
            event_kind: AttemptEventKind::Reset,
            resubmit_cycle,
            ..Self::new(derivation_id, outcome_class, reporting_party)
        }
    }

    /// The in-memory mirror of this row for the node's attempt history.
    pub(crate) fn to_record(&self) -> AttemptRecord {
        AttemptRecord {
            attempt_id: self.attempt_id,
            event_kind: self.event_kind,
            outcome_class: self.outcome_class,
            exec_id: self.exec_id,
            executor_id: self.executor_id.clone(),
            termination_reason: self.termination_reason.clone(),
            reporting_party: self.reporting_party,
            exempt: self.exempt,
            floor_promoted: self.floor_promoted,
            floor_at_cap: self.floor_at_cap,
            error_msg: self.error_msg.clone(),
            final_line_count: self.final_line_count,
            resubmit_cycle: self.resubmit_cycle,
            occurred_at_epoch_secs: self.occurred_at_epoch_secs,
            recorded_at_epoch_secs: self.recorded_at_epoch_secs,
        }
    }
}

/// Wall-clock now as epoch seconds (the append-site `occurred_at`).
fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Raw FromRow shape for the suffix load: TEXT enums come back as
/// `String` and are parsed into the typed vocabulary in
/// [`AttemptRow::try_from`] — the CHECK constraints make a parse
/// failure a schema/code drift bug, surfaced as a decode error rather
/// than silently skipped.
#[derive(Debug, sqlx::FromRow)]
struct RawAttemptRow {
    attempt_id: Uuid,
    derivation_id: Uuid,
    exec_id: Option<Uuid>,
    executor_id: Option<String>,
    event_kind: String,
    outcome_class: String,
    termination_reason: Option<String>,
    reporting_party: String,
    exempt: bool,
    floor_promoted: bool,
    floor_at_cap: bool,
    error_msg: Option<String>,
    final_line_count: Option<i64>,
    resubmit_cycle: i32,
    occurred_at_epoch_secs: f64,
    recorded_at_epoch_secs: f64,
}

impl TryFrom<RawAttemptRow> for AttemptRow {
    type Error = sqlx::Error;

    fn try_from(raw: RawAttemptRow) -> Result<Self, sqlx::Error> {
        let parse_err = |col: &str, val: &str| {
            sqlx::Error::Decode(
                format!("drv_attempts.{col}: value {val:?} not in the rust-side alphabet").into(),
            )
        };
        Ok(Self {
            attempt_id: raw.attempt_id,
            derivation_id: raw.derivation_id,
            exec_id: raw.exec_id,
            executor_id: raw.executor_id.map(ExecutorId::from),
            event_kind: raw
                .event_kind
                .parse()
                .map_err(|_| parse_err("event_kind", &raw.event_kind))?,
            outcome_class: raw
                .outcome_class
                .parse()
                .map_err(|_| parse_err("outcome_class", &raw.outcome_class))?,
            termination_reason: raw.termination_reason,
            reporting_party: raw
                .reporting_party
                .parse()
                .map_err(|_| parse_err("reporting_party", &raw.reporting_party))?,
            exempt: raw.exempt,
            floor_promoted: raw.floor_promoted,
            floor_at_cap: raw.floor_at_cap,
            error_msg: raw.error_msg,
            final_line_count: raw.final_line_count,
            resubmit_cycle: raw.resubmit_cycle,
            occurred_at_epoch_secs: raw.occurred_at_epoch_secs,
            recorded_at_epoch_secs: raw.recorded_at_epoch_secs,
        })
    }
}

impl SchedulerDb {
    /// Append one attempt/reset row inside the caller's transaction.
    ///
    /// Returns whether a row was actually inserted. Rows that carry an
    /// `exec_id` use `ON CONFLICT (exec_id) WHERE exec_id IS NOT NULL
    /// DO NOTHING` (the predicate is what lets Postgres pick the
    /// partial unique index as arbiter), so a duplicate append for an
    /// already-recorded execution is a no-op by schema — the
    /// disconnect→report, report→disconnect (race-ahead), and
    /// duplicate-report orders are idempotent by construction.
    /// Classification updates go only through
    /// [`Self::fill_termination`]'s `WHERE termination_reason IS NULL`
    /// guard — there is deliberately no `DO UPDATE` arm here.
    /// NULL-exec_id rows never conflict and always insert.
    pub(crate) async fn append_attempt(
        tx: &mut PgConnection,
        row: &AttemptRow,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO drv_attempts \
                 (attempt_id, derivation_id, exec_id, executor_id, event_kind, \
                  outcome_class, termination_reason, reporting_party, exempt, \
                  floor_promoted, floor_at_cap, error_msg, final_line_count, \
                  resubmit_cycle, occurred_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
                     to_timestamp($15)) \
             ON CONFLICT (exec_id) WHERE exec_id IS NOT NULL DO NOTHING",
        )
        .bind(row.attempt_id)
        .bind(row.derivation_id)
        .bind(row.exec_id)
        .bind(row.executor_id.as_ref().map(ExecutorId::as_str))
        .bind(row.event_kind.as_str())
        .bind(row.outcome_class.as_str())
        .bind(row.termination_reason.as_deref())
        .bind(row.reporting_party.as_str())
        .bind(row.exempt)
        .bind(row.floor_promoted)
        .bind(row.floor_at_cap)
        .bind(row.error_msg.as_deref())
        .bind(row.final_line_count)
        .bind(row.resubmit_cycle)
        .bind(row.occurred_at_epoch_secs)
        .execute(&mut *tx)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Batch append (one statement) for rows produced by one event —
    /// the cascade's per-dependent `outcome_class='cascade'` rows.
    /// Same conflict semantics as [`Self::append_attempt`]; returns the
    /// number of rows actually inserted.
    pub(crate) async fn append_attempts_batch(
        tx: &mut PgConnection,
        rows: &[AttemptRow],
    ) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut attempt_id = Vec::with_capacity(rows.len());
        let mut derivation_id = Vec::with_capacity(rows.len());
        let mut exec_id = Vec::with_capacity(rows.len());
        let mut executor_id = Vec::with_capacity(rows.len());
        let mut event_kind = Vec::with_capacity(rows.len());
        let mut outcome_class = Vec::with_capacity(rows.len());
        let mut termination_reason = Vec::with_capacity(rows.len());
        let mut reporting_party = Vec::with_capacity(rows.len());
        let mut exempt = Vec::with_capacity(rows.len());
        let mut floor_promoted = Vec::with_capacity(rows.len());
        let mut floor_at_cap = Vec::with_capacity(rows.len());
        let mut error_msg = Vec::with_capacity(rows.len());
        let mut final_line_count = Vec::with_capacity(rows.len());
        let mut resubmit_cycle = Vec::with_capacity(rows.len());
        let mut occurred_at = Vec::with_capacity(rows.len());
        for r in rows {
            attempt_id.push(r.attempt_id);
            derivation_id.push(r.derivation_id);
            exec_id.push(r.exec_id);
            executor_id.push(r.executor_id.as_ref().map(|e| e.as_str().to_string()));
            event_kind.push(r.event_kind.as_str());
            outcome_class.push(r.outcome_class.as_str());
            termination_reason.push(r.termination_reason.clone());
            reporting_party.push(r.reporting_party.as_str());
            exempt.push(r.exempt);
            floor_promoted.push(r.floor_promoted);
            floor_at_cap.push(r.floor_at_cap);
            error_msg.push(r.error_msg.clone());
            final_line_count.push(r.final_line_count);
            resubmit_cycle.push(r.resubmit_cycle);
            occurred_at.push(r.occurred_at_epoch_secs);
        }
        let result = sqlx::query(
            "INSERT INTO drv_attempts \
                 (attempt_id, derivation_id, exec_id, executor_id, event_kind, \
                  outcome_class, termination_reason, reporting_party, exempt, \
                  floor_promoted, floor_at_cap, error_msg, final_line_count, \
                  resubmit_cycle, occurred_at) \
             SELECT attempt_id, derivation_id, exec_id, executor_id, event_kind, \
                    outcome_class, termination_reason, reporting_party, exempt, \
                    floor_promoted, floor_at_cap, error_msg, final_line_count, \
                    resubmit_cycle, to_timestamp(occurred_at) \
             FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::text[], $5::text[], \
                         $6::text[], $7::text[], $8::text[], $9::bool[], $10::bool[], \
                         $11::bool[], $12::text[], $13::bigint[], $14::int[], \
                         $15::float8[]) \
                  AS t(attempt_id, derivation_id, exec_id, executor_id, event_kind, \
                       outcome_class, termination_reason, reporting_party, exempt, \
                       floor_promoted, floor_at_cap, error_msg, final_line_count, \
                       resubmit_cycle, occurred_at) \
             ON CONFLICT (exec_id) WHERE exec_id IS NOT NULL DO NOTHING",
        )
        .bind(&attempt_id)
        .bind(&derivation_id)
        .bind(&exec_id)
        .bind(&executor_id)
        .bind(&event_kind)
        .bind(&outcome_class)
        .bind(&termination_reason)
        .bind(&reporting_party)
        .bind(&exempt)
        .bind(&floor_promoted)
        .bind(&floor_at_cap)
        .bind(&error_msg)
        .bind(&final_line_count)
        .bind(&resubmit_cycle)
        .bind(&occurred_at)
        .execute(&mut *tx)
        .await?;
        Ok(result.rows_affected())
    }

    /// The second installment of a two-installment attempt: fill
    /// `termination_reason` (and reclassify `outcome_class`) on the
    /// row identified by `(derivation_id, exec_id)`.
    ///
    /// Idempotent first-writer-wins via `WHERE termination_reason IS
    /// NULL`; returns whether THIS call performed the fill. Keyed on
    /// the released `(derivation_id, exec_id)` pair carried by the
    /// `recently_disconnected` entry so the establishment fill never
    /// needs a DAG lookup (the node may already be reaped or carry the
    /// next attempt's exec_id).
    // TODO: callers land with the no-report two-installment paths (T-1a.5).
    #[allow(dead_code)]
    pub(crate) async fn fill_termination(
        tx: &mut PgConnection,
        derivation_id: Uuid,
        exec_id: Uuid,
        termination_reason: &str,
        outcome_class: OutcomeClass,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE drv_attempts \
             SET termination_reason = $3, outcome_class = $4 \
             WHERE derivation_id = $1 AND exec_id = $2 \
               AND termination_reason IS NULL",
        )
        .bind(derivation_id)
        .bind(exec_id)
        .bind(termination_reason)
        .bind(outcome_class.as_str())
        .execute(&mut *tx)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Batched suffix load: for every requested derivation, the ledger
    /// rows at-or-after its most recent `event_kind='reset'` row
    /// (the reset row itself included), ordered by
    /// `(recorded_at, attempt_id)`. Derivations with no rows are simply
    /// absent from the returned map.
    ///
    /// Deliberately a SECOND batched query keyed on `derivation_id =
    /// ANY($1)` — NOT a widening of `load_nonterminal_derivations`,
    /// whose LEFT JOIN must stay one-row-per-derivation. The per-cycle
    /// suffix bound keeps this O(rows-since-last-reset × derivations).
    // TODO: the recovery-load caller lands with the attempt-history reload (T-1a.7).
    #[allow(dead_code)]
    pub(crate) async fn load_attempt_suffix(
        &self,
        derivation_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttemptRow>>, sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // The LATERAL picks each derivation's most recent reset row
        // (served by the partial index on `WHERE event_kind = 'reset'`);
        // the row-wise tuple comparison keeps everything at-or-after it,
        // with `attempt_id` (UUIDv7, append-ordered) breaking
        // `recorded_at` ties. One `&'static str` literal — sqlx 0.9's
        // `SqlSafeStr` bound on `query_as()` rejects runtime-composed
        // SQL. Timestamps come back as epoch seconds (no chrono/time
        // dependency — same pattern as the recovery rows).
        let raw: Vec<RawAttemptRow> = sqlx::query_as(
            "SELECT a.attempt_id, a.derivation_id, a.exec_id, a.executor_id, \
                    a.event_kind, a.outcome_class, a.termination_reason, \
                    a.reporting_party, a.exempt, a.floor_promoted, a.floor_at_cap, \
                    a.error_msg, a.final_line_count, a.resubmit_cycle, \
                    EXTRACT(EPOCH FROM a.occurred_at)::float8 AS occurred_at_epoch_secs, \
                    EXTRACT(EPOCH FROM a.recorded_at)::float8 AS recorded_at_epoch_secs \
             FROM drv_attempts a \
             LEFT JOIN LATERAL ( \
                 SELECT r.recorded_at, r.attempt_id \
                 FROM drv_attempts r \
                 WHERE r.derivation_id = a.derivation_id \
                   AND r.event_kind = 'reset' \
                 ORDER BY r.recorded_at DESC, r.attempt_id DESC \
                 LIMIT 1 \
             ) last_reset ON TRUE \
             WHERE a.derivation_id = ANY($1) \
               AND (last_reset.recorded_at IS NULL \
                    OR (a.recorded_at, a.attempt_id) \
                       >= (last_reset.recorded_at, last_reset.attempt_id)) \
             ORDER BY a.derivation_id, a.recorded_at, a.attempt_id",
        )
        .bind(derivation_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut out: HashMap<Uuid, Vec<AttemptRow>> = HashMap::new();
        for r in raw {
            let row = AttemptRow::try_from(r)?;
            out.entry(row.derivation_id).or_default().push(row);
        }
        Ok(out)
    }
}
