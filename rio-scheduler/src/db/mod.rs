//! PostgreSQL persistence for scheduler state.
//!
//! Synchronous writes: state transitions, assignment changes, build terminal status.
//! Async/batched: build_samples inserts (SLA fit feed).
//!
//! UUIDs are bound natively via the sqlx `uuid` feature — no `::uuid` casts or
//! `.to_string()` conversions needed.
//!
//! `query!(...)` macros (compile-time SQL checking) read from `.sqlx/`
//! (committed, regenerated via `cargo xtask regen sqlx`). The
//! `terminal_status_sql!`-spliced callsites are permanent exceptions —
//! `query!` requires a string literal, and the planner needs the literal
//! for partial-index proof; the splice macro keeps those queries
//! `&'static str` so they still satisfy sqlx 0.9's `SqlSafeStr` bound.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::state::DerivationStatus;

mod assignments;
pub(crate) mod attempts;
mod batch;
mod builds;
pub(crate) use builds::BuildTerminalRow;
pub(crate) mod confirm_fences;
mod derivations;
pub(crate) use derivations::{ReplayResidual, StatusReplay};
mod executions;
mod history;
pub(crate) mod live_pins;
pub(crate) mod materialization;
pub(crate) mod open_attempts;
mod recovery;
mod tenants;
pub(crate) mod wanted;

#[cfg(test)]
mod tests;

pub use history::{BuildSampleRow, SlaOverrideRow};

// r[impl sched.db.partial-index-literal]
/// Terminal statuses as a SQL `IN`/`NOT IN` tuple literal, spliced
/// between two literal SQL fragments at compile time:
/// `terminal_status_sql!("… WHERE status NOT IN ", " …")` (the second
/// fragment may be omitted). The expansion is a single `&'static str`,
/// which is what sqlx 0.9's `SqlSafeStr` bound on `query()`/`query_as()`
/// requires — `concat!` can't expand a `const`, so the splice has to
/// happen in macro position for the composed SQL to stay a
/// compile-time literal.
///
/// MUST match both:
///   - [`DerivationStatus::is_terminal`] (enum ground truth)
///   - `migrations/004_recovery.sql:85` partial index predicate
///
/// Inlined as a literal (not bound as `$1::text[]`) so the planner
/// can prove the query predicate implies the partial index predicate.
/// With a bind parameter, that proof is impossible at plan time —
/// the planner doesn't know what `$1` will contain — so the partial
/// index is never chosen and recovery seq-scans the whole table.
///
/// The drift test `tests::transactions::test_terminal_statuses_match_is_terminal`
/// iterates all `DerivationStatus` variants and asserts that
/// `is_terminal() ⇔ as_str() ∈ TERMINAL_STATUSES` (via
/// `TERMINAL_STATUS_SQL`, which is derived from this macro). Adding a
/// new terminal status without updating this list fails that test.
/// Updating this list without updating the migration fails the
/// PG-side check (`test_partial_index_predicate_matches_const`).
macro_rules! terminal_status_sql {
    ($before:literal) => {
        terminal_status_sql!($before, "")
    };
    ($before:literal, $after:literal) => {
        concat!(
            $before,
            "('completed', 'poisoned', 'dependency_failed', 'cancelled', 'skipped')",
            $after
        )
    };
}
pub(super) use terminal_status_sql;

/// String form of [`terminal_status_sql!`] for the drift tests
/// (`tests/transactions.rs` compares it against `TERMINAL_STATUSES` and
/// the migration's partial-index predicate). The macro is the source of
/// truth; this is just its expansion with empty surroundings. Test-only
/// since the production queries splice the macro directly.
#[cfg(test)]
pub(super) const TERMINAL_STATUS_SQL: &str = terminal_status_sql!("");

/// Encode a `&[String]` as a PostgreSQL text-array literal: `{a,b,c}`.
/// Used for the nested-array columns in `batch_upsert_derivations` —
/// PG multidim arrays must be rectangular, so we can't bind
/// `Vec<Vec<String>>` directly. Instead: bind as flat `text[]` of
/// literals, cast back to `text[]` in the SELECT.
///
/// Escaping: double-quote each element, backslash-escape embedded
/// `"` and `\`. PG array-literal syntax, not SQL string syntax —
/// single quotes are literal, double quotes delimit.
pub(super) fn encode_pg_text_array(items: &[String]) -> String {
    let mut out = String::with_capacity(2 + items.iter().map(|s| s.len() + 3).sum::<usize>());
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in item.chars() {
            match ch {
                '"' | '\\' => {
                    out.push('\\');
                    out.push(ch);
                }
                _ => out.push(ch),
            }
        }
        out.push('"');
    }
    out.push('}');
    out
}

crate::state::db_str_enum! {
    /// Assignment lifecycle status (assignments table).
    ///
    /// `Pending` is the insert-time value. Every CompletionReport
    /// transitions it to one of the terminal values — `Completed` on
    /// `Built`, `Failed` on any failure status, `Cancelled` when the
    /// scheduler cancelled the build. I-209: leaving the row at `pending`
    /// after a derivation goes terminal blocks the tick-DELETE pruner
    /// (`NOT EXISTS (SELECT 1 FROM assignments …)`), leaking
    /// `derivations` rows unbounded.
    ///
    /// The schema also defines `'acknowledged'` (the phase2c worker-ack
    /// that shipped a different design); SQL paths that reference it use
    /// the literal directly in `insert_assignment`'s ON CONFLICT predicate.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AssignmentStatus {
        Pending = "pending",
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
    }
}
/// Database operations for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerDb {
    pool: PgPool,
}

/// Row from `list_builds` / `list_builds_keyset`. Single-table read
/// (builds only, I-103) — counts are denormalized columns maintained
/// by `persist_build_counts` at merge + completion time. Replaces the
/// old 4-table join which was O(Σ drvs across all builds) regardless
/// of LIMIT.
///
/// `submitted_at_micros` + `build_id` together form the keyset cursor
/// tuple. Micros via `EXTRACT(EPOCH)*1e6::bigint` — avoids chrono dep
/// (see TenantRow pattern below). Kept as the raw `Uuid` (not `::text`)
/// so cursor encoding doesn't round-trip through strings.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct BuildListRow {
    pub build_id: Uuid,
    pub tenant_id: Option<String>,
    pub priority_class: String,
    pub status: String,
    pub error_summary: Option<String>,
    pub total_derivations: i64,
    pub completed_derivations: i64,
    pub cached_derivations: i64,
    pub submitted_at_micros: i64,
}

/// Re-export of the cross-service `tenants` row. Defined in
/// `rio-common` so rio-store's reads `query_as!` into the SAME
/// struct — a column rename/retype is then a compile error in both
/// crates, not a runtime tripwire.
pub(crate) use rio_migrations::schema::TenantRow;

/// Row from `load_nonterminal_builds`. FromRow for named-column
/// mapping (tuples at this arity are error-prone).
///
/// `options_json`: `Option<Json<BuildOptions>>` because the column
/// is NULLable (rows from before migration 004). Caller unwraps
/// with `.map(|j| j.0).unwrap_or_default()`.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RecoveryBuildRow {
    pub build_id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub status: String,
    pub priority_class: String,
    pub keep_going: bool,
    pub options_json: Option<sqlx::types::Json<crate::state::BuildOptions>>,
    /// I-111: denormalized counts (migration 030). Recovery seeds the
    /// in-memory `BuildInfo` from these — the DB is authoritative since
    /// completed drvs aren't loaded into the DAG, so recomputing from
    /// DAG state would undercount.
    pub total_drvs: i32,
    pub completed_drvs: i32,
    pub cached_drvs: i32,
    /// PG-side `now() - submitted_at` so the caller can reconstruct an
    /// `Instant` (same pattern as [`PoisonedDerivationRow`]). Seeds
    /// `BuildInfo::submitted_at` so `r[sched.timeout.per-build]` and
    /// `rio_scheduler_build_duration_seconds` survive failover instead
    /// of resetting to recovery-time.
    pub submitted_age_secs: f64,
}

/// Row from `load_poisoned_derivations`. Minimal — poisoned rows
/// aren't dispatched, just TTL-tracked + resubmit-bound checked (the
/// resubmit bound and the exclusion set are rebuilt from the
/// attempt-ledger fold after the suffix load). `elapsed_secs`
/// is computed PG-side (`now() - poisoned_at`) so the caller can
/// reconstruct an `Instant` via
/// `Instant::now() - Duration::from_secs_f64(elapsed)`.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct PoisonedDerivationRow {
    pub derivation_id: Uuid,
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub elapsed_secs: f64,
    /// I-057: previously hardcoded false in `from_poisoned_row`. A
    /// poison-recovered FOD with `is_fixed_output: false` would route
    /// to a builder via the kind XOR in `hard_filter`, hit `WrongKind`
    /// at executor/mod.rs:390, and re-poison. Thread it through.
    pub is_fixed_output: bool,
}

/// Row from `load_nonterminal_derivations`. Mirrors the INSERT
/// columns from `batch_upsert_derivations` plus live-state fields
/// (assigned_builder_id, the persisted resource floor, the active
/// exec_id). Retry counters are not derivations columns (the attempt
/// ledger is the only failure-history record since migration 075);
/// recovery rebuilds the retry view from the ledger fold after the
/// suffix load.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RecoveryDerivationRow {
    pub derivation_id: Uuid,
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub status: String,
    pub required_features: Vec<String>,
    pub assigned_builder_id: Option<String>,
    pub expected_output_paths: Vec<String>,
    pub output_names: Vec<String>,
    pub is_fixed_output: bool,
    pub is_ca: bool,
    /// D4: persisted reactive resource floor (`M_044`). All `bigint`
    /// (`i64`) — saturating-cast to `u64`/`u32` at hydration.
    pub floor_mem_bytes: i64,
    pub floor_disk_bytes: i64,
    pub floor_deadline_secs: i64,
    /// Per-execution identifier from the active `assignments` row
    /// (`migrations/061`). `None` unless the drv is currently dispatched
    /// (`assigned_builder_id IS NOT NULL`) — a reset drv's assignments row
    /// stays open at `pending`, so "has an active assignment row" is NOT
    /// "has a live execution"; the JOIN filters on the builder column to
    /// preserve `reset_to_ready()`'s exec_id clear across failover.
    pub exec_id: Option<Uuid>,
    /// bug_251 (rule-4b): the open attempt's persisted claim nonce
    /// (`assignments.claim_nonce`, migration 096), riding the SAME
    /// guarded join as `exec_id` — `None` whenever `exec_id` is, so
    /// the reset-clear is preserved for the credential too.
    pub claim_nonce: Option<Uuid>,
    /// Work class of the open execution (`drv_executions.attempt_kind`,
    /// joined on the SAME guarded exec_id), recovering the kinded
    /// running surface across failover. `None` exactly when `exec_id`
    /// is `None` — the join rides the assignment row's guard, so the
    /// reset-clear is preserved for the kind too.
    pub attempt_kind: Option<String>,
}

#[cfg(test)]
impl RecoveryDerivationRow {
    /// Minimal Ready row for `DagActor::test_inject_ready_row`. Callers
    /// override fields via struct-update.
    pub fn test_default(hash: &str, system: &str) -> Self {
        Self {
            derivation_id: Uuid::new_v4(),
            drv_hash: hash.to_string(),
            drv_path: rio_test_support::fixtures::test_drv_path(hash),
            pname: None,
            system: system.to_string(),
            status: "ready".into(),
            required_features: vec![],
            assigned_builder_id: None,
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            is_ca: false,
            floor_mem_bytes: 0,
            floor_disk_bytes: 0,
            floor_deadline_secs: 0,
            exec_id: None,
            claim_nonce: None,
            attempt_kind: None,
        }
    }
}

/// Row from `load_build_graph` nodes query. Thin — ~200B.
/// Mirrors proto `GraphNode` (NOT `DerivationNode`, which carries
/// ≤64KB `drv_content`). `pname` and `assigned_builder_id` are COALESCE'd
/// to empty-string SQL-side to match proto3's non-optional string fields.
///
/// `derivation_id` is NOT in the proto — it's collected here so the edge
/// query can filter to the returned node set (truncation correctness).
///
/// `exec_id` comes from the `build_derivations` edge (the build↔exec
/// observation), not `derivations` — the same drv can have a different
/// `exec_id` per build (rebuilt after GC, retried). NULL until the
/// completion handler (or recovery's orphan adoption) records it after
/// a terminal where an execution actually ran: `Completed`, `Poisoned`,
/// `Cancelled` reached from `Assigned`/`Running`, or any terminal
/// reached while a prior, reset execution's stamped log buffer is
/// retained (build-cancel sweep, failed-substitute revert,
/// dependency-failure cascade).
/// Stays NULL for cache-hit `Completed`, never-dispatched cascaded
/// `DependencyFailed`, `Skipped`, never-dispatched terminals, and
/// non-terminal drvs (no execution to record). The dashboard falls
/// back to "latest exec" when empty.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct GraphNodeRow {
    pub derivation_id: Uuid,
    pub drv_path: String,
    pub pname: String,
    pub system: String,
    pub status: String,
    pub assigned_builder_id: String,
    pub exec_id: Option<Uuid>,
}

/// Row from `load_build_graph` edges query. Mirrors proto `GraphEdge`.
/// `is_cutoff` deliberately dropped (retro P0027: always FALSE; Skipped
/// is a node status, not an edge flag).
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct GraphEdgeRow {
    pub parent_drv_path: String,
    pub child_drv_path: String,
}

/// Row for [`SchedulerDb::batch_upsert_derivations`].
#[derive(Debug)]
pub(crate) struct DerivationRow {
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub status: DerivationStatus,
    pub required_features: Vec<String>,
    // Phase 3b recovery columns. These are written at merge time
    // so recover_from_pg() can fully reconstruct DerivationState.
    pub expected_output_paths: Vec<String>,
    pub output_names: Vec<String>,
    pub is_fixed_output: bool,
    pub is_ca: bool,
}

/// Shared SELECT / FROM clause for `list_builds` and
/// `list_builds_keyset`, spliced ahead of each method's pagination
/// tail at compile time: `list_builds_select!("WHERE … LIMIT $3")`.
/// Single-table since I-103 — counts are denormalized columns; the old
/// 4-table join was O(Σ drvs) (see `M_030`). The two methods differ
/// only in their WHERE pagination clause and LIMIT/OFFSET tail. A
/// macro (not a `format!`-ed const) since sqlx 0.9: `query_as()` only
/// accepts `&'static str` SQL (`SqlSafeStr`), so the composition has
/// to be a compile-time literal.
///
/// `submitted_at_micros`: `EXTRACT(EPOCH)*1e6` gives fractional seconds
/// with µs precision; `::bigint` truncates to integer microseconds.
/// Matches PG's native TIMESTAMPTZ resolution. `list_builds_keyset`'s
/// WHERE reconstructs the timestamp via integer-seconds-plus-microsecond
/// -remainder so the round-trip is exact (see its doc comment — direct
/// bigint÷1e6 through `to_timestamp(float8)` would lose precision near
/// the 16-significant-figure IEEE754 limit).
macro_rules! list_builds_select {
    ($tail:literal) => {
        concat!(
            r#"
    SELECT
        b.build_id,
        b.tenant_id::text,
        b.priority_class,
        b.status,
        b.error_summary,
        (EXTRACT(EPOCH FROM b.submitted_at) * 1e6)::bigint AS submitted_at_micros,
        b.total_drvs::bigint     AS total_derivations,
        b.completed_drvs::bigint AS completed_derivations,
        b.cached_drvs::bigint    AS cached_derivations
    FROM builds b
    "#,
            $tail
        )
    };
}
pub(super) use list_builds_select;

/// Outcome of a claims-floor-fenced decision-state write
/// (`sched.evidence.durability`): the write applied (with the number
/// of rows it touched, where the statement reports one), the target
/// row was already terminally resolved (the at-most-once writers'
/// idempotent arm), or the fence rolled the transaction back having
/// written nothing because the caller's serving generation sits below
/// the durable claims floor.
///
/// `Fenced` is NOT an error: it is the fence working on a deposed
/// replica whose in-memory state is garbage awaiting the queued
/// LeaderLost wipe. Callers log it at `warn!` with the floor and
/// generation, increment `rio_scheduler_evidence_write_fenced_total`,
/// and continue.
///
/// `#[must_use]`: silently discarding a fence outcome is exactly the
/// class where a caller mutates its in-memory view without branching
/// on the durable result (a deposed replica then acts on state the
/// database refused). The lint turns the discard into a build error
/// under the gate's `--deny warnings`.
///
/// `pub` (not `pub(crate)`) only because the fenced status/poison
/// writers (`update_derivation_status`, `persist_poisoned`, …) are
/// `pub` and the private-interfaces lint denies the mismatch; nothing
/// outside the crate consumes it.
// r[impl sched.evidence.durability+4]
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FencedOutcome {
    /// The write committed; the payload is `rows_affected()` for
    /// statements that report it (0 for fixed-shape single-row writes
    /// whose callers don't consume a count).
    Applied(u64),
    /// The transaction committed but the target row was already in a
    /// terminal state, so this write changed nothing — the idempotent
    /// arm of at-most-once writers. Settled, but NOT the at-most-once
    /// edge: side effects keyed on "this call resolved it" (counters,
    /// completion fan-out) belong to `Applied` only.
    AlreadyResolved,
    /// The serving generation is below the claims floor: the
    /// transaction rolled back having written nothing.
    Fenced,
}

impl FencedOutcome {
    /// The durable state is settled (either by this write or an
    /// earlier one) — in-memory bookkeeping that mirrors the durable
    /// row may proceed.
    pub fn settled(self) -> bool {
        matches!(self, Self::Applied(_) | Self::AlreadyResolved)
    }

    /// THIS call performed the resolution — the at-most-once edge for
    /// counters and completion fan-out.
    pub fn applied(self) -> bool {
        matches!(self, Self::Applied(_))
    }
}

/// Claim-stamped tenure authority (merged_bug_338 class close).
///
/// The generation this leader STAMPED at claim time — as opposed to
/// the live lease atomic (`leader.generation()`), which a re-acquire
/// can advance MID-MAILBOX. A fenced write stamped from a fresh atomic
/// read can carry a tenure the actor never recovered under: the floor
/// admits it (the new claim just became the floor) while the in-memory
/// DAG still reflects the old tenure. Every fenced db entry point
/// therefore takes `ServingGeneration`, and the only constructors are
/// the boot stamp and the claim stamp (`stamp_from_claim` — exactly
/// two production call sites, pinned by
/// `tenure_stamp_exactly_two_production_sites`). The three historical
/// fresh-atomic-read sites are uncompilable against these signatures
/// and pinned to zero by
/// `tenure_authority_no_fresh_atomic_reads_in_write_paths`.
// r[impl sched.lease.tenure-stamp-type]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ServingGeneration(i64);

impl ServingGeneration {
    /// Sole constructor: stamp from a durable generation claim.
    /// Saturating — claims above `i64::MAX` pin to `i64::MAX` (the
    /// fence compares in i64 domain; saturation can only under-claim,
    /// never over-claim — fail-closed).
    pub fn stamp_from_claim(generation: u64) -> Self {
        Self(i64::try_from(generation).unwrap_or(i64::MAX))
    }

    /// Kernel-facing projection. Negative stamps are impossible via
    /// the constructor but the projection still saturates to 0 —
    /// 0 is below every floor, i.e. always-fenced fail-closed.
    pub fn to_kernel_u64(self) -> u64 {
        u64::try_from(self.0).unwrap_or(0)
    }

    /// SQL-bind / tracing projection, crate-internal.
    pub(crate) fn as_i64(self) -> i64 {
        self.0
    }
}

/// The fenced-transaction capability: holding a [`FencedTx`] proves
/// the claims-floor check ran on this transaction's own connection at
/// construction time (`SchedulerDb::begin_fenced`). It is the ONLY
/// way to open a decision-state write transaction — the floor helpers
/// are private to `db`, so an open-coded actor-side floor compare no
/// longer compiles.
///
/// Drop without commit = rollback = fail-safe: an abandoned fenced
/// transaction writes nothing.
// r[impl sched.evidence.durability+4]
// r[impl sched.lease.generation-fence+3]
pub struct FencedTx {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    serving_generation: i64,
}

/// Result of `SchedulerDb::begin_fenced`: the fence either admitted
/// the transaction or refused it at the door (nothing written, the
/// connection returned).
pub enum FencedBegin {
    /// The serving generation is at or above the durable claims floor;
    /// the capability is live.
    Open(FencedTx),
    /// Below the floor: rolled back, nothing written. `floor` is the
    /// durable floor that refused the write (for the caller's `warn!`).
    Fenced { floor: i64 },
}

/// Outcome of `FencedTx::commit_refenced` — the pre-commit floor
/// re-check used by settlement-class writers whose transactions span
/// long reads (the merge settlement pattern, encapsulated).
pub enum FencedCommit {
    Committed,
    /// A newer claim landed mid-transaction: rolled back at commit
    /// time, nothing written.
    Refenced {
        floor: i64,
    },
}

/// Terminal statuses [`FencedTx::close_assignment`] may stamp. The
/// closed set keeps the unique closer total — a new terminal status
/// extends this enum, not a new open-coded UPDATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignmentCloseStatus {
    Completed,
    Failed,
    /// The zero-interest cancel path's status
    /// ([`SchedulerDb::cancel_job_and_close_attempt_fenced`]).
    Cancelled,
}

impl AssignmentCloseStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    // r[impl sched.db.exec-stamp-on-close]
    /// THE assignment-close → `drv_executions.status` mapping — the
    /// single site where the two deliberately distinct vocabularies
    /// meet (`completed` vs `succeeded`; see
    /// `rio_migrations::schema::EXEC_STATUS_SUCCEEDED`). Every closer
    /// stamps through [`close_assignments_sql`], which binds this.
    fn exec_status(self) -> &'static str {
        match self {
            Self::Completed => rio_migrations::schema::EXEC_STATUS_SUCCEEDED,
            Self::Failed => rio_migrations::schema::EXEC_STATUS_FAILED,
            Self::Cancelled => rio_migrations::schema::EXEC_STATUS_CANCELLED,
        }
    }
}

// r[impl sched.db.exec-stamp-on-close]
/// Render THE production assignment-close statement: one CTE pair that
/// closes the selected active (`pending`/`acknowledged`) assignments
/// AND stamps each closed row's `drv_executions` lifecycle status — in
/// the same statement, same snapshot. Closing an assignment without
/// stamping its execution row is unwritable through this family
/// (bug_047: an unstamped row keeps `status = NULL` forever, reads as
/// "still running" to the store's completeness predicate, never
/// satisfies `gc_exec_rows`' terminality conjunct, and is immortal).
///
/// `selector` filters `assignments` rows (unqualified columns resolve
/// to `assignments`); `first_free_bind` is the ordinal of the first
/// unused `$n` placeholder — the renderer appends TWO binds: `$n` =
/// the assignment status string, `$n+1` = the execution status string
/// ([`AssignmentCloseStatus::exec_status`], the single mapping site).
/// The stamp's `status IS NULL` guard keeps first-verdict-wins: a row
/// already stamped by the terminal-log epilogue is not overwritten
/// (and the epilogue commutes on equal status — see
/// `terminal_log_epilogue`). The statement returns the CLOSED row
/// count (`SELECT count(*) FROM closed`) — fetch it with
/// `query_scalar::<_, i64>`; `rows_affected()` of a CTE statement is
/// NOT the close count.
///
/// `db/tests/fence_coverage.rs` pins this as the only production
/// `UPDATE assignments` site; the fence discipline is unchanged (every
/// caller is itself a `FencedTx` owner or `*_in_tx` body).
pub(crate) fn close_assignments_sql(selector: &str, first_free_bind: u32) -> String {
    let sb = first_free_bind;
    let eb = first_free_bind + 1;
    format!(
        "WITH closed AS ( \
             UPDATE assignments SET status = ${sb}, completed_at = now() \
             WHERE ({selector}) AND status IN ('pending', 'acknowledged') \
             RETURNING exec_id), \
         stamped AS ( \
             UPDATE drv_executions e \
             SET status = ${eb}, finished_at = COALESCE(e.finished_at, now()) \
             WHERE e.exec_id IN (SELECT exec_id FROM closed) \
               AND e.status IS NULL) \
         SELECT count(*) FROM closed"
    )
}

impl FencedTx {
    /// Escape hatch for the `*_in_tx` statement bodies: the underlying
    /// connection. The capability stays with the `FencedTx`; the
    /// borrow ends before `commit`.
    pub(crate) fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// Commit the fenced transaction.
    pub(crate) async fn commit(self) -> Result<(), sqlx::Error> {
        self.tx.commit().await
    }

    /// Re-check the floor immediately before commit and refuse if a
    /// newer claim landed mid-transaction (the settlement writers'
    /// pre-commit re-check, encapsulated so the floor SQL stays
    /// private to this module).
    pub(crate) async fn commit_refenced(mut self) -> Result<FencedCommit, sqlx::Error> {
        let floor = claims_floor(&mut self.tx).await?;
        if !at_or_above_floor(floor, self.serving_generation) {
            self.tx.rollback().await?;
            return Ok(FencedCommit::Refenced {
                floor: floor.unwrap_or(i64::MIN),
            });
        }
        self.tx.commit().await?;
        Ok(FencedCommit::Committed)
    }

    /// The unique pull-mode assignment closer: exec_id-scoped, never
    /// derivation_id-keyed — a deposed replica's stale derivation view
    /// can never close a successor's assignment row through this
    /// surface. Returns the closed-row count (0 = already closed or
    /// re-assigned: the caller's idempotent arm).
    // r[impl sched.evidence.durability+4]
    pub(crate) async fn close_assignment(
        &mut self,
        exec_id: Uuid,
        status: AssignmentCloseStatus,
    ) -> Result<u64, sqlx::Error> {
        static SQL: std::sync::LazyLock<String> =
            std::sync::LazyLock::new(|| close_assignments_sql("exec_id = $1", 2));
        let n: i64 = sqlx::query_scalar(SQL.as_str())
            .bind(exec_id)
            .bind(status.as_str())
            .bind(status.exec_status())
            .fetch_one(&mut *self.tx)
            .await?;
        Ok(u64::try_from(n).expect("count(*) is non-negative"))
    }
}

// r[impl sched.evidence.durability+4]
/// The durable claims floor: `GREATEST` over
/// `assignments.generation` and `leader_generation_claims.generation`
/// — the same two arms as [`SchedulerDb::max_known_generation`], read
/// on the CALLER's connection so the comparison happens inside the
/// same transaction as the write it fences. `None` = fresh cluster
/// (no assignments, no claims): nothing to fence against.
///
/// PRIVATE to `db`: the only legitimate consumers are
/// `SchedulerDb::begin_fenced` / `FencedTx::commit_refenced` and
/// the in-module fenced writers. Actor code holds a [`FencedTx`]
/// instead of comparing floors.
async fn claims_floor(conn: &mut PgConnection) -> Result<Option<i64>, sqlx::Error> {
    let row: (Option<i64>,) = sqlx::query_as(
        "SELECT GREATEST( \
             (SELECT MAX(generation) FROM assignments), \
             (SELECT MAX(generation) FROM leader_generation_claims))",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.0)
}

// r[impl sched.evidence.durability+4]
/// The fence comparison: a write applies iff its serving generation
/// is at or above the claims floor. The comparison is `>=` (equal
/// passes) and this is load-bearing — a write carrying a generation
/// EQUAL to the floor is the same-epoch re-acquire keep that
/// `sched.lease.generation-claim` requires (same generation ⇔ no
/// holder change ⇔ no newer tenure's evidence exists), so it MUST
/// apply; tightening to `>` would fence a leader's own writes after
/// every same-epoch re-acquire. A negative floor row (hand-edited
/// anomaly) demands nothing, same trust posture as
/// `max_known_generation`'s callers.
fn at_or_above_floor(floor: Option<i64>, serving_generation: i64) -> bool {
    match floor {
        Some(f) => serving_generation >= f,
        None => true,
    }
}

impl SchedulerDb {
    /// Create a new database handle from a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get a reference to the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // r[impl sched.evidence.durability+4]
    // r[impl sched.lease.generation-fence+3]
    /// Open the fenced-transaction capability: begin a transaction,
    /// read the claims floor ON ITS CONNECTION, and admit the caller
    /// only at-or-above the floor. Every decision-state write path in
    /// the scheduler constructs its transaction here — the fence
    /// cannot be forgotten, reordered after the write, or compared
    /// against a floor read on a different connection.
    pub(crate) async fn begin_fenced(
        &self,
        serving_generation: ServingGeneration,
    ) -> Result<FencedBegin, sqlx::Error> {
        let serving_generation = serving_generation.as_i64();
        let mut tx = self.pool.begin().await?;
        let floor = claims_floor(&mut tx).await?;
        if !at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedBegin::Fenced {
                floor: floor.unwrap_or(i64::MIN),
            });
        }
        Ok(FencedBegin::Open(FencedTx {
            tx,
            serving_generation,
        }))
    }

    // r[impl sched.evidence.durability+4]
    /// Pool wrapper over [`FencedTx::close_assignment`] for callers
    /// with no surrounding statements: one fenced transaction, one
    /// exec_id-scoped close. The establishment adopt arm's closer —
    /// the derivation_id-keyed unfenced writer it replaces let a
    /// deposed replica close a successor's assignment row.
    pub(crate) async fn close_assignment_fenced(
        &self,
        exec_id: Uuid,
        status: AssignmentCloseStatus,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let n = tx.close_assignment(exec_id, status).await?;
        tx.commit().await?;
        if n == 0 {
            Ok(FencedOutcome::AlreadyResolved)
        } else {
            Ok(FencedOutcome::Applied(n))
        }
    }
}
