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
mod derivations;
mod executions;
mod history;
mod live_pins;
pub(crate) mod open_attempts;
mod recovery;
mod tenants;

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
/// The drift test [`tests::transactions::test_terminal_statuses_match_is_terminal`]
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
    /// scheduler sent a CancelSignal. I-209: leaving the row at `pending`
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
    /// Demand-driven wanted-output set (`migrations/062`). Empty = all
    /// declared outputs wanted (also the pre-migration default, so old
    /// rows recover with the conservative all-outputs criterion).
    pub wanted_output_names: Vec<String>,
    pub is_fixed_output: bool,
    pub is_ca: bool,
    /// Roots-only-prune marker (`migrations/063`): the node was kept by
    /// a topdown prune and its dependency closure was never merged (or
    /// never produced), so it MUST complete via substitution. Restored
    /// verbatim by `from_recovery_row` — resetting it to false is what
    /// allowed the post-failover doomed from-source dispatch.
    /// `load_dag_from_rows` then drops the restored mark when the row's
    /// persisted children are all produced and vouched for by a live
    /// (`pending`/`active`) build that also owns the parent (see
    /// `load_parents_with_all_children_produced`).
    pub topdown_pruned: bool,
    /// Closure-hole breadcrumb (`migrations/064`): an un-produced child
    /// was removed out from under the node (a terminal build's cleanup
    /// reap, a poison-clear removal, or a recovery-time edge drop), so
    /// its persisted children are a truncated view of its pruned input
    /// closure. Written best-effort via `set_closure_hole_by_hashes`
    /// (the leader's reap hook, the recovery-time stamp, and the two
    /// poison-clear paths), restored verbatim by `from_recovery_row`
    /// (`from_poisoned_row` keeps `false`), and consulted by the
    /// recovery-time gate: a flagged row that also carries the
    /// breadcrumb is never enrolled as a clear candidate, so the
    /// produced survivors cannot launder the mark away after a failover
    /// (the un-produced child's own row may have been GC'd by then).
    pub closure_hole: bool,
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
            wanted_output_names: vec![],
            is_fixed_output: false,
            is_ca: false,
            topdown_pruned: false,
            closure_hole: false,
            floor_mem_bytes: 0,
            floor_disk_bytes: 0,
            floor_deadline_secs: 0,
            exec_id: None,
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
    /// Demand-driven wanted-output set (`migrations/062`). Empty = all
    /// declared outputs wanted. UNIONED on conflict (with empty
    /// saturating to empty = "all") — see `batch_upsert_derivations`.
    pub wanted_output_names: Vec<String>,
    /// Roots-only-prune marker (`migrations/063`): true for kept
    /// (demanded) nodes of a topdown-fired merge whose dependency
    /// closure the prune dropped and whose existing DAG children (if
    /// any) the closure classifier does not vouch for at stamp time
    /// (`DerivationDag::closure_evidence` via `closure_vouched`);
    /// false otherwise (including dep-less demanded leaves, which
    /// never had a closure to drop, and nodes whose children are all
    /// Completed/Skipped with no closure hole). Childless kept
    /// nodes ARE stamped; present-but-unbuilt children do not exempt
    /// the node. OR-combined on conflict so an unrelated non-pruned
    /// merge of the same drv never clears it; cleared only once its
    /// children are all produced (the post-reconciliation pass in
    /// `handle_merge_dag` via `clear_topdown_pruned_by_hashes`, the
    /// completion-time `clear_topdown_pruned_for_produced_parents`,
    /// the recovery-time gate in `load_dag_from_rows`, and the lazy
    /// walk-failure clear in `handle_substitute_complete`) and when
    /// the topdown fail-fast consumes it.
    pub topdown_pruned: bool,
    /// Closure-hole breadcrumb (`migrations/064`). Merge-time rows
    /// always bind `false` — the upsert is never a stamping site for
    /// the breadcrumb (the setters, all via
    /// `set_closure_hole_by_hashes`, are the leader-gated reap hook,
    /// the recovery-time stamp in `load_dag_from_rows`, and the
    /// poison-clear paths — admin ClearPoison and the poison-TTL
    /// sweep) — and the OR-on-conflict SET keeps any persisted hole,
    /// so a later merge of the same drv can never launder it away.
    /// Cleared together with `topdown_pruned` by the batched
    /// Vouched-keyed `clear_topdown_pruned_by_hashes` helper and on
    /// its own by the merge-time heal (`clear_closure_hole_by_hashes`);
    /// the single-row `clear_topdown_pruned_by_hash` is mark-only, so
    /// the topdown fail-fast retains the hole it leaves behind.
    pub closure_hole: bool,
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

/// Outcome of a claims-floor-fenced evidence write
/// (`sched.evidence.durability`): either the write applied (with the
/// number of rows it touched, where the statement reports one) or the
/// fence rolled the transaction back having written nothing because
/// the caller's serving generation sits below the durable claims
/// floor.
///
/// `Fenced` is NOT an error: it is the fence working on a deposed
/// replica whose in-memory state is garbage awaiting the queued
/// LeaderLost wipe. Callers log it at `warn!` with the floor and
/// generation, increment `rio_scheduler_evidence_write_fenced_total`,
/// and continue.
///
/// `pub` (not `pub(crate)`) only because the fenced status/poison
/// writers (`update_derivation_status`, `persist_poisoned`, …) are
/// `pub` and the private-interfaces lint denies the mismatch; nothing
/// outside the crate consumes it.
// r[impl sched.evidence.durability+2]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FencedWrite {
    /// The write committed; the payload is `rows_affected()` for
    /// statements that report it (0 for fixed-shape single-row writes
    /// whose callers don't consume a count).
    Applied(u64),
    /// The serving generation is below the claims floor: the
    /// transaction rolled back having written nothing.
    Fenced,
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

    // r[impl sched.evidence.durability+2]
    /// The durable claims floor: `GREATEST` over
    /// `assignments.generation` and `leader_generation_claims.generation`
    /// — the same two arms as [`Self::max_known_generation`], read on
    /// the CALLER's connection so the comparison happens inside the
    /// same transaction as the write it fences (the pull-mint pattern,
    /// `mint_pull_attempt_fenced`). `None` = fresh cluster (no
    /// assignments, no claims): nothing to fence against.
    pub(crate) async fn claims_floor(conn: &mut PgConnection) -> Result<Option<i64>, sqlx::Error> {
        let row: (Option<i64>,) = sqlx::query_as(
            "SELECT GREATEST( \
                 (SELECT MAX(generation) FROM assignments), \
                 (SELECT MAX(generation) FROM leader_generation_claims))",
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(row.0)
    }

    // r[impl sched.evidence.durability+2]
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
    pub(crate) fn at_or_above_floor(floor: Option<i64>, serving_generation: i64) -> bool {
        match floor {
            Some(f) => serving_generation >= f,
            None => true,
        }
    }
}
