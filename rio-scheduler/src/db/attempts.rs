//! The durable attempt ledger — `drv_attempts` table (migration 068).
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
//! after the commit. The controller's pod-terminal report is a
//! reason-only second installment on the worker-reported row
//! ([`SchedulerDb::fill_termination_reason_only`]) — an UPDATE guarded
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
use super::ServingGeneration;
use crate::state::{
    AttemptEventKind, AttemptKind, AttemptRecord, ExecutorId, OutcomeClass, POISON_TTL,
    ReportingParty,
};

/// Ledger retention floor (decision P8), consulted by the live sweep:
/// `tick_gc_attempt_ledger` computes its horizon as
/// `sweep_horizon_secs(decision_budget(), LEDGER_RETENTION_FLOOR) =
/// max(floor, POISON_TTL)` — the retired infra window's term is gone
/// (live059-c: the consecutive-streak law needs the post-reset
/// suffix, which the per-lane reset cut bounds independent of wall
/// age; the 300 s term was floor-dominated, horizon unchanged). The
/// sweep
/// ([`SchedulerDb::gc_attempt_ledger`]) deletes only the suffix
/// complement: attempt-kind rows strictly before their derivation's
/// last reset row, past the horizon, with no active assignment for
/// their exec_id — plus orphaned histories whose derivation row is
/// gone. The suffix bound (rows since the last reset event) is what
/// keeps reads O(per-cycle attempts); the sweep preserves that suffix
/// bit-identically (sched.db.attempts-gc, kernel-proved).
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
/// the 068_drv_attempts schema; `recorded_at_epoch_secs` is PG-assigned
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
    /// Work class of the attempt's execution (substitution-replacement).
    /// NOT a `drv_attempts` column: joined from
    /// `drv_executions.attempt_kind` on read-back (COALESCE'd to
    /// `'build'` for rows with no execution), defaulted to Build at
    /// construction, and never written by the append statements. The
    /// kind is persisted only via the execution row.
    pub attempt_kind: AttemptKind,
    /// Controller-authoritative source node the attempt ran on (071,
    /// AD2c). Stamped only for pull-mode attempts, from the spawn-ack
    /// binding / the execution row / the controller's report — never
    /// from worker-supplied identity. The exclusion fold keys the row
    /// on this and ONLY this (decision P12): a row without it charges
    /// flat counters but contributes no exclusion key.
    pub source_node: Option<String>,
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
    /// optional column empty. The LANE (`kind`) is a constructor
    /// parameter — migration 084 made it a durable column on every
    /// row, and forcing every caller to state it makes a
    /// materialization row that forgets its kind uncompilable. Callers
    /// fill the fields the path knows (exec_id, executor, floor flags,
    /// error message, …) before appending.
    pub(crate) fn new(
        derivation_id: Uuid,
        outcome_class: OutcomeClass,
        reporting_party: ReportingParty,
        kind: AttemptKind,
    ) -> Self {
        Self {
            attempt_id: Uuid::now_v7(),
            derivation_id,
            exec_id: None,
            executor_id: None,
            attempt_kind: kind,
            source_node: None,
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
    /// `cache_hit_clear`, `poison_cleared`, `materialization_reset`);
    /// `resubmit_cycle` carries the cycle index the reset starts (0
    /// for the mat-lane job-creation resets — not a resubmit). The
    /// reset CARRIES ITS LANE (migration 084): a build reset cuts only
    /// the build suffix, a materialization reset only the
    /// materialization suffix — the loaders and the GC sweep key their
    /// cuts on it. The materialization lane's production writer is
    /// `create_materialization_jobs_in_tx` (migration 085: one reset
    /// per created job, same transaction) — see the `M_085` doc-const
    /// in rio-migrations for the lane rationale.
    pub(crate) fn new_reset(
        derivation_id: Uuid,
        outcome_class: OutcomeClass,
        reporting_party: ReportingParty,
        resubmit_cycle: i32,
        kind: AttemptKind,
    ) -> Self {
        Self {
            event_kind: AttemptEventKind::Reset,
            resubmit_cycle,
            ..Self::new(derivation_id, outcome_class, reporting_party, kind)
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
            attempt_kind: self.attempt_kind,
            source_node: self.source_node.clone(),
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

/// Wall-clock now as epoch seconds (the append-site `occurred_at`, and
/// the `now` the Phase-1b appending transactions hand to `decide()`).
pub(crate) fn epoch_now() -> f64 {
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
    attempt_kind: String,
    source_node: Option<String>,
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
            attempt_kind: raw
                .attempt_kind
                .parse()
                .map_err(|_| parse_err("attempt_kind", &raw.attempt_kind))?,
            source_node: raw.source_node,
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
    /// [`Self::fill_termination_reason_only`]'s `WHERE termination_reason IS NULL`
    /// guard — there is deliberately no `DO UPDATE` arm here.
    /// NULL-exec_id rows never conflict and always insert.
    pub(crate) async fn append_attempt(
        tx: &mut PgConnection,
        row: &AttemptRow,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO drv_attempts \
                 (attempt_id, derivation_id, exec_id, executor_id, source_node, event_kind, \
                  outcome_class, termination_reason, reporting_party, exempt, \
                  floor_promoted, floor_at_cap, error_msg, final_line_count, \
                  resubmit_cycle, occurred_at, attempt_kind) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                     to_timestamp($16), $17) \
             ON CONFLICT (exec_id) WHERE exec_id IS NOT NULL DO NOTHING",
        )
        .bind(row.attempt_id)
        .bind(row.derivation_id)
        .bind(row.exec_id)
        .bind(row.executor_id.as_ref().map(ExecutorId::as_str))
        .bind(row.source_node.as_deref())
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
        .bind(row.attempt_kind.as_str())
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
        let mut source_node = Vec::with_capacity(rows.len());
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
        let mut attempt_kind = Vec::with_capacity(rows.len());
        for r in rows {
            attempt_id.push(r.attempt_id);
            derivation_id.push(r.derivation_id);
            exec_id.push(r.exec_id);
            executor_id.push(r.executor_id.as_ref().map(|e| e.as_str().to_string()));
            source_node.push(r.source_node.clone());
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
            attempt_kind.push(r.attempt_kind.as_str());
        }
        let result = sqlx::query(
            "INSERT INTO drv_attempts \
                 (attempt_id, derivation_id, exec_id, executor_id, source_node, event_kind, \
                  outcome_class, termination_reason, reporting_party, exempt, \
                  floor_promoted, floor_at_cap, error_msg, final_line_count, \
                  resubmit_cycle, occurred_at, attempt_kind) \
             SELECT attempt_id, derivation_id, exec_id, executor_id, source_node, event_kind, \
                    outcome_class, termination_reason, reporting_party, exempt, \
                    floor_promoted, floor_at_cap, error_msg, final_line_count, \
                    resubmit_cycle, to_timestamp(occurred_at), attempt_kind \
             FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::text[], $5::text[], \
                         $6::text[], $7::text[], $8::text[], $9::bool[], $10::bool[], \
                         $11::bool[], $12::text[], $13::bigint[], $14::int[], \
                         $15::float8[], $16::text[], $17::text[]) \
                  AS t(attempt_id, derivation_id, exec_id, executor_id, event_kind, \
                       outcome_class, termination_reason, reporting_party, exempt, \
                       floor_promoted, floor_at_cap, error_msg, final_line_count, \
                       resubmit_cycle, occurred_at, source_node, attempt_kind) \
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
        .bind(&source_node)
        .bind(&attempt_kind)
        .execute(&mut *tx)
        .await?;
        Ok(result.rows_affected())
    }

    /// The second installment of a two-installment attempt: fill
    /// `termination_reason` (and reclassify `outcome_class`) on the
    /// row identified by `(derivation_id, exec_id)`.
    ///
    /// Reason-only second installment: fill `termination_reason` on the
    /// row identified by `(derivation_id, exec_id)` WITHOUT touching its
    /// `outcome_class` or floor flags — the unified pod-terminal report
    /// (`ReportAttemptOutcome`) enriches an already-classified row, it
    /// never reclassifies it. First-writer-wins via the same
    /// `WHERE termination_reason IS NULL` guard; returns whether THIS
    /// call filled it.
    /// `source_node` (the controller-reported kube-authoritative node,
    /// AD2c) is stamped only when the row does not already carry one —
    /// the pull-mint / worker-report attribution wins when present.
    /// Claims-floor fenced: a deposed replica's late controller
    /// installment writes nothing (`Fenced`); `Applied(1)` = THIS call
    /// filled it; `AlreadyResolved` = the first-writer-wins guard found
    /// it already filled (or no matching row).
    pub(crate) async fn fill_termination_reason_only(
        &self,
        derivation_id: Uuid,
        exec_id: Uuid,
        termination_reason: &str,
        source_node: Option<&str>,
        serving_generation: ServingGeneration,
    ) -> Result<super::FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            super::FencedBegin::Fenced { .. } => return Ok(super::FencedOutcome::Fenced),
            super::FencedBegin::Open(ftx) => ftx,
        };
        let result = sqlx::query(
            "UPDATE drv_attempts \
             SET termination_reason = $3, \
                 source_node = COALESCE(source_node, $4) \
             WHERE derivation_id = $1 AND exec_id = $2 \
               AND termination_reason IS NULL",
        )
        .bind(derivation_id)
        .bind(exec_id)
        .bind(termination_reason)
        .bind(source_node)
        .execute(tx.conn())
        .await?;
        tx.commit().await?;
        if result.rows_affected() == 1 {
            Ok(super::FencedOutcome::Applied(1))
        } else {
            Ok(super::FencedOutcome::AlreadyResolved)
        }
    }

    /// Single-derivation suffix load **inside the caller's transaction**
    /// — the read half of the Phase-1b appending transaction (append the
    /// observation's row, read the post-reset suffix it now belongs to,
    /// fold it through `decide()`, persist the verdict, commit). Same
    /// query shape as [`Self::load_attempt_suffix`], scoped to one
    /// derivation and running on the transaction's connection so it sees
    /// the row appended moments earlier.
    pub(crate) async fn load_attempt_suffix_one_in_tx(
        tx: &mut PgConnection,
        derivation_id: Uuid,
    ) -> Result<Vec<AttemptRow>, sqlx::Error> {
        let raw: Vec<RawAttemptRow> = sqlx::query_as(
            "SELECT a.attempt_id, a.derivation_id, a.exec_id, a.executor_id, a.source_node, \
                    a.event_kind, a.outcome_class, a.termination_reason, \
                    a.reporting_party, a.exempt, a.floor_promoted, a.floor_at_cap, \
                    a.error_msg, a.final_line_count, a.resubmit_cycle, \
                    a.attempt_kind, \
                    EXTRACT(EPOCH FROM a.occurred_at)::float8 AS occurred_at_epoch_secs, \
                    EXTRACT(EPOCH FROM a.recorded_at)::float8 AS recorded_at_epoch_secs \
             FROM drv_attempts a \
             LEFT JOIN LATERAL ( \
                 SELECT r.recorded_at, r.attempt_id \
                 FROM drv_attempts r \
                 WHERE r.derivation_id = a.derivation_id \
                   AND r.event_kind = 'reset' \
                   AND r.attempt_kind = a.attempt_kind \
                 ORDER BY r.recorded_at DESC, r.attempt_id DESC \
                 LIMIT 1 \
             ) last_lane_reset ON TRUE \
             WHERE a.derivation_id = $1 \
               AND (last_lane_reset.recorded_at IS NULL \
                    OR (a.recorded_at, a.attempt_id) \
                       >= (last_lane_reset.recorded_at, last_lane_reset.attempt_id)) \
             ORDER BY a.recorded_at, a.attempt_id",
        )
        .bind(derivation_id)
        .fetch_all(&mut *tx)
        .await?;
        raw.into_iter().map(AttemptRow::try_from).collect()
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
    pub(crate) async fn load_attempt_suffix(
        &self,
        derivation_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttemptRow>>, sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // The LATERAL picks each ROW'S OWN LANE's most recent reset row
        // (served by the partial index on `WHERE event_kind = 'reset'`,
        // correlated on `r.attempt_kind = a.attempt_kind` — migration
        // 084 put the lane on every row, resets included); the
        // row-wise tuple comparison keeps everything at-or-after it,
        // with `attempt_id` (UUIDv7, append-ordered) breaking
        // `recorded_at` ties. This is the verbatim transcription of
        // `rio_retry_kernel::row_survives_load`: a build reset cuts
        // only the build lane, a materialization reset only the
        // materialization lane — cross-lane truncation is structurally
        // impossible. The kind column is read DIRECTLY off the row
        // (the drv_executions join died with 084: reset rows carry
        // their lane themselves). One `&'static str` literal — sqlx
        // 0.9's `SqlSafeStr` bound on `query_as()` rejects
        // runtime-composed SQL. Timestamps come back as epoch seconds
        // (no chrono/time dependency — same pattern as the recovery
        // rows).
        let raw: Vec<RawAttemptRow> = sqlx::query_as(
            "SELECT a.attempt_id, a.derivation_id, a.exec_id, a.executor_id, a.source_node, \
                    a.event_kind, a.outcome_class, a.termination_reason, \
                    a.reporting_party, a.exempt, a.floor_promoted, a.floor_at_cap, \
                    a.error_msg, a.final_line_count, a.resubmit_cycle, \
                    a.attempt_kind, \
                    EXTRACT(EPOCH FROM a.occurred_at)::float8 AS occurred_at_epoch_secs, \
                    EXTRACT(EPOCH FROM a.recorded_at)::float8 AS recorded_at_epoch_secs \
             FROM drv_attempts a \
             LEFT JOIN LATERAL ( \
                 SELECT r.recorded_at, r.attempt_id \
                 FROM drv_attempts r \
                 WHERE r.derivation_id = a.derivation_id \
                   AND r.event_kind = 'reset' \
                   AND r.attempt_kind = a.attempt_kind \
                 ORDER BY r.recorded_at DESC, r.attempt_id DESC \
                 LIMIT 1 \
             ) last_lane_reset ON TRUE \
             WHERE a.derivation_id = ANY($1) \
               AND (last_lane_reset.recorded_at IS NULL \
                    OR (a.recorded_at, a.attempt_id) \
                       >= (last_lane_reset.recorded_at, last_lane_reset.attempt_id)) \
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

    // r[impl sched.db.attempts-gc]
    /// The execution-row GC sweep — the second deleter of the retention
    /// story (`store.log.sweep-ownership`; the store's log TTL sweep
    /// owns log artifacts and never touches `drv_executions`). The SQL
    /// twin of the kernel conjunction
    /// `rio_retry_kernel::exec_row_sweep_eligible`: delete a lifecycle
    /// row only when it is
    ///
    /// 1. **terminal** (a non-terminal row may still receive its
    ///    report; its exec_id is a live idempotency key),
    /// 2. with **no active assignment** (the report-idempotency probes
    ///    stay sound — same E4 shape as the ledger sweep),
    /// 3. **referenced by no `drv_attempts` row** (an exec row outlives
    ///    every ledger row that needs its kind: the COALESCE re-kinding
    ///    decay is unreachable; the ledger GC bounds attempt-row
    ///    lifetime so exec rows stay eventually collectable — a
    ///    derivation parked past retention keeps its post-reset charge
    ///    rows, those rows keep their exec rows, the kind survives the
    ///    park),
    /// 4. with **no surviving `drv_log_chunks` rows** (artifact before
    ///    row: deleting the lifecycle row first orphans its chunks
    ///    forever — the store's TTL sweep selects victims through this
    ///    row; data-structural, holds under ANY retention config),
    /// 5. with **no live `log_ingest_sessions` row** (still producing
    ///    artifacts; the row anchors the routing registry). LIVE per
    ///    THE shared definition (`rio_migrations::sql` — bug_234): a
    ///    stale row is a dead replica's leftover, routes nothing, and
    ///    does not pin,
    /// 6. and **older than `retention_secs`**.
    ///
    /// One statement, one MVCC snapshot, for the same stability
    /// argument as [`Self::gc_attempt_ledger`] below; subselect-LIMIT
    /// per the same precedent. Deliberately stronger than
    /// "not-in-suffix" — see the kernel doc.
    // r[impl store.log.sweep-ownership+2]
    pub(crate) async fn gc_exec_rows(
        &self,
        retention_secs: f64,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        // Conjunct 5 is the LIVENESS predicate, not bare existence
        // (bug_234): a stale row is a dead replica's leftover — it
        // routes nothing (`lookup_live` ignores it) and anchors no
        // in-flight artifacts, so it must not pin the exec row for the
        // remaining `log_retention − exec_retention` days. Predicate
        // text and staleness constant are THE shared definitions from
        // rio-migrations — this query cannot drift from the store's
        // reads.
        let sql = format!(
            "DELETE FROM drv_executions e \
             WHERE e.exec_id IN ( \
                 SELECT v.exec_id FROM drv_executions v \
                 WHERE v.status = ANY($3) \
                   AND v.started_at < now() - make_interval(secs => $1) \
                   AND NOT EXISTS (SELECT 1 FROM assignments a \
                                   WHERE a.exec_id = v.exec_id \
                                     AND a.status IN ('pending', 'acknowledged')) \
                   AND NOT EXISTS (SELECT 1 FROM drv_attempts t \
                                   WHERE t.exec_id = v.exec_id) \
                   AND NOT EXISTS (SELECT 1 FROM drv_log_chunks c \
                                   WHERE c.exec_id = v.exec_id) \
                   AND NOT EXISTS (SELECT 1 FROM log_ingest_sessions s \
                                   WHERE s.exec_id = v.exec_id \
                                     AND {live}) \
                 LIMIT $2)",
            live = rio_migrations::sql::live_ingest_session_sql("s.heartbeat_at", "$4")
        );
        // AssertSqlSafe: const-fragment composition, no runtime data.
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(retention_secs)
            .bind(limit)
            .bind(rio_migrations::schema::EXEC_STATUS_TERMINAL)
            .bind(rio_migrations::sql::SESSION_STALE_AFTER_SECS)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// The attempt-ledger GC sweep: delete (live arm) attempt-kind rows
    /// strictly before their derivation's last-reset cut OF THEIR OWN
    /// LANE (migration 084: `last_resets` is keyed per
    /// `(derivation_id, attempt_kind)` and the victim join matches
    /// `r.attempt_kind = a.attempt_kind` — a build reset structurally
    /// cannot make materialization evidence eligible, mirroring
    /// `rio_retry_kernel::sweep_eligible`), older than `horizon_secs`,
    /// with no ACTIVE assignment for their `exec_id`; plus (orphan arm)
    /// any-kind rows whose `derivation_id` has no `derivations` row,
    /// older than the horizon. At most `2 × limit` rows per pass (one
    /// `LIMIT` per arm; subselect-LIMIT shape — PG has no
    /// `DELETE .. LIMIT` — per the derivations-GC precedent).
    ///
    /// ONE statement, deliberately and load-bearingly: a single MVCC
    /// snapshot evaluates the whole eligibility predicate — including
    /// the `NOT EXISTS` active-assignment probe — and the deletion, so
    /// the probe and the DELETE cannot disagree. Do NOT split this into
    /// SELECT-victims-then-DELETE without re-deriving the
    /// sched.db.attempts-gc stability argument (single-snapshot
    /// report-idempotency probes; closed assignments never reopen;
    /// exec_ids never re-bind). Lock-free on purpose: the only feared
    /// concurrent event is an `assignments` transition, which row locks
    /// on `drv_attempts` cannot express and which the closure-monotone
    /// plus fresh-exec_id invariants already exclude; a closure committing
    /// after this snapshot only defers its rows to the next pass, and a
    /// deposed leader's concurrent sweep is an idempotent PK DELETE
    /// over the same stable set.
    ///
    /// Index reliance (no new migration): `last_resets` is served by
    /// the partial index `drv_attempts_reset (derivation_id,
    /// recorded_at) WHERE event_kind='reset'` (068; the `attempt_id`
    /// tiebreak sorts within the few equal-`recorded_at` resets per
    /// derivation); the victim scan probes
    /// `drv_attempts_derivation_recorded (derivation_id, recorded_at)`
    /// per candidate derivation; the assignments anti-join hash-builds
    /// once per pass (no exec_id index exists — same as the existing
    /// exec_id probes, acceptable at LIMIT-1000 batch scale); the
    /// orphan arm is the same bounded anti-join class the accepted
    /// derivations-GC statement runs every pass. DELETE by PK.
    pub(crate) async fn gc_attempt_ledger(
        &self,
        horizon_secs: f64,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "WITH last_resets AS (
                 SELECT DISTINCT ON (derivation_id, attempt_kind)
                        derivation_id, attempt_kind, recorded_at, attempt_id
                 FROM drv_attempts WHERE event_kind = 'reset'
                 ORDER BY derivation_id, attempt_kind, recorded_at DESC, attempt_id DESC),
             pre_reset AS (
                 SELECT a.attempt_id FROM drv_attempts a
                 JOIN last_resets r ON r.derivation_id = a.derivation_id
                                   AND r.attempt_kind = a.attempt_kind
                 WHERE a.event_kind = 'attempt'
                   AND (a.recorded_at, a.attempt_id) < (r.recorded_at, r.attempt_id)
                   AND a.recorded_at < now() - make_interval(secs => $1)
                   AND NOT EXISTS (SELECT 1 FROM assignments x
                                   WHERE x.exec_id = a.exec_id
                                     AND x.status IN ('pending', 'acknowledged'))
                 LIMIT $2),
             orphaned AS (
                 SELECT a.attempt_id FROM drv_attempts a
                 WHERE NOT EXISTS (SELECT 1 FROM derivations d
                                   WHERE d.derivation_id = a.derivation_id)
                   AND a.recorded_at < now() - make_interval(secs => $1)
                 LIMIT $2)
             DELETE FROM drv_attempts a
             USING (SELECT attempt_id FROM pre_reset
                    UNION SELECT attempt_id FROM orphaned) v
             WHERE a.attempt_id = v.attempt_id",
        )
        .bind(horizon_secs)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
