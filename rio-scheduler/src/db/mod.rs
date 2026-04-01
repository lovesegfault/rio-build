//! PostgreSQL persistence for scheduler state.
//!
//! Synchronous writes: state transitions, assignment changes, build terminal status.
//! Async/batched: build_history EMA updates.
//!
//! UUIDs are bound natively via the sqlx `uuid` feature — no `::uuid` casts or
//! `.to_string()` conversions needed.
//!
//! `query!(...)` macros (compile-time SQL checking) read from `.sqlx/`
//! (committed, regenerated via `just sqlx-prepare`). The `TERMINAL_STATUS_SQL`
//! `format!`-interpolated callsites are permanent exceptions — the macro
//! requires a string literal, and the planner needs the literal for
//! partial-index proof. See remediations/phase4a/12-pg-transaction-safety.md §6.

use sqlx::PgPool;
use uuid::Uuid;

use crate::state::DerivationStatus;

mod assignments;
mod batch;
mod builds;
mod derivations;
mod history;
mod live_pins;
mod recovery;
mod tenants;

#[cfg(test)]
mod tests;

// Free fn — see `recovery.rs` for definition. Re-exported here so
// callers (grpc/mod.rs, grpc/tests/bridge_tests.rs) keep using
// `crate::db::read_event_log` without knowing the internal layout.
pub use recovery::read_event_log;

// r[impl sched.db.partial-index-literal]
/// Terminal statuses as a SQL `NOT IN` literal fragment.
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
/// `is_terminal() ⇔ as_str() ∈ TERMINAL_STATUSES`. Adding a new
/// terminal status without updating this list fails that test.
/// Updating this list without updating the migration fails the
/// PG-side check (`test_partial_index_predicate_matches_const`).
pub(super) const TERMINAL_STATUS_SQL: &str =
    "('completed', 'poisoned', 'dependency_failed', 'cancelled', 'skipped')";

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

/// One row from `build_history`. Fields in SELECT order:
/// `(pname, system, ema_duration_secs, ema_peak_memory_bytes, ema_peak_cpu_cores)`.
///
/// Type alias not struct: `sqlx::query_as` maps to tuples by ordinal
/// position, and a struct would need `#[derive(FromRow)]` + named
/// columns — more ceremony than a tuple for what's just a pipe from
/// the DB read into `Estimator::refresh()`. The alias exists because
/// 5-element tuples trip clippy's type-complexity lint, and naming
/// it documents the field order where it matters (3 float-ish fields
/// are easy to mix up).
pub type BuildHistoryRow = (String, String, f64, Option<f64>, Option<f64>);

/// Assignment lifecycle status (assignments table).
///
/// The schema also defines `acknowledged`/`failed`/`cancelled` string
/// values, but nothing in Rust ever constructs them — the phase2c
/// worker-ack that would have used them shipped a different design.
/// SQL paths that reference those strings use literals directly
/// (`'pending'`, `'acknowledged'` in `insert_assignment`'s ON CONFLICT
/// predicate). Keeping dead variants here bloats match arms and
/// confuses readers about what the actor actually sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentStatus {
    Pending,
    Completed,
}

impl AssignmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
        }
    }
}
/// Database operations for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerDb {
    pool: PgPool,
}

/// EMA alpha for duration estimation updates.
pub(super) const EMA_ALPHA: f64 = 0.3;

/// Row from `list_builds` / `list_builds_keyset`. 4-table join
/// (builds / build_derivations / derivations / assignments).
/// `cached_derivations` heuristic: "completed with no assignment row"
/// — a cache-hit derivation transitions directly to Completed at merge
/// time without ever being dispatched.
///
/// `submitted_at_micros` + `build_id` together form the keyset cursor
/// tuple. Micros via `EXTRACT(EPOCH)*1e6::bigint` — avoids chrono dep
/// (see TenantRow pattern below). Kept as the raw `Uuid` (not `::text`)
/// so cursor encoding doesn't round-trip through strings.
#[derive(Debug, sqlx::FromRow)]
pub struct BuildListRow {
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

/// Row from `list_tenants` / `create_tenant`. `has_cache_token`
/// is a projection (`cache_token IS NOT NULL`) — the token itself
/// is never returned. `created_at` is epoch seconds via
/// `EXTRACT(EPOCH FROM created_at)::bigint` — avoids a chrono dep.
#[derive(Debug, sqlx::FromRow)]
pub struct TenantRow {
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub gc_retention_hours: i32,
    pub gc_max_store_bytes: Option<i64>,
    pub has_cache_token: bool,
    pub created_at: i64,
}

/// Row from `load_nonterminal_builds`. FromRow for named-column
/// mapping (tuples at this arity are error-prone).
///
/// `options_json`: `Option<Json<BuildOptions>>` because the column
/// is NULLable (rows from before migration 004). Caller unwraps
/// with `.map(|j| j.0).unwrap_or_default()`.
#[derive(Debug, sqlx::FromRow)]
pub struct RecoveryBuildRow {
    pub build_id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub status: String,
    pub priority_class: String,
    pub keep_going: bool,
    pub options_json: Option<sqlx::types::Json<crate::state::BuildOptions>>,
}

/// Row from `load_poisoned_derivations`. Minimal — poisoned rows
/// aren't dispatched, just TTL-tracked. `elapsed_secs` is computed
/// PG-side (`now() - poisoned_at`) so the caller can reconstruct
/// an `Instant` via `Instant::now() - Duration::from_secs_f64(elapsed)`.
#[derive(Debug, sqlx::FromRow)]
pub struct PoisonedDerivationRow {
    pub derivation_id: Uuid,
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub failed_builders: Vec<String>,
    pub elapsed_secs: f64,
    /// I-057: previously hardcoded false in `from_poisoned_row`. A
    /// poison-recovered FOD with `is_fixed_output: false` would route
    /// to a builder via the kind XOR in `hard_filter`, hit `WrongKind`
    /// at executor/mod.rs:390, and re-poison. Thread it through.
    pub is_fixed_output: bool,
}

/// Row from `load_nonterminal_derivations`. Mirrors the INSERT
/// columns from `batch_upsert_derivations` plus live-state fields
/// (retry_count, assigned_builder_id, failed_builders).
#[derive(Debug, sqlx::FromRow)]
pub struct RecoveryDerivationRow {
    pub derivation_id: Uuid,
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub status: String,
    pub required_features: Vec<String>,
    pub assigned_builder_id: Option<String>,
    pub retry_count: i32,
    pub expected_output_paths: Vec<String>,
    pub output_names: Vec<String>,
    pub is_fixed_output: bool,
    pub is_ca: bool,
    pub failed_builders: Vec<String>,
}

/// Row from `load_build_graph` nodes query. Thin — ~200B.
/// Mirrors proto `GraphNode` (NOT `DerivationNode`, which carries
/// ≤64KB `drv_content`). `pname` and `assigned_builder_id` are COALESCE'd
/// to empty-string SQL-side to match proto3's non-optional string fields.
///
/// `derivation_id` is NOT in the proto — it's collected here so the edge
/// query can filter to the returned node set (truncation correctness).
#[derive(Debug, sqlx::FromRow)]
pub struct GraphNodeRow {
    pub derivation_id: Uuid,
    pub drv_path: String,
    pub pname: String,
    pub system: String,
    pub status: String,
    pub assigned_builder_id: String,
}

/// Row from `load_build_graph` edges query. Mirrors proto `GraphEdge`.
/// `is_cutoff` deliberately dropped (retro P0027: always FALSE; Skipped
/// is a node status, not an edge flag).
#[derive(Debug, sqlx::FromRow)]
pub struct GraphEdgeRow {
    pub parent_drv_path: String,
    pub child_drv_path: String,
}

/// Row for [`SchedulerDb::batch_upsert_derivations`].
#[derive(Debug)]
pub struct DerivationRow {
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

/// Shared SELECT / FROM / JOIN clause for `list_builds` and
/// `list_builds_keyset`. The two methods differ only in their WHERE
/// pagination clause and LIMIT/OFFSET tail. Kept as a `&str` const
/// (not a macro) because the queries are runtime-built via `format!`
/// — sqlx compile-time checks don't apply to dynamic strings anyway.
///
/// `submitted_at_micros`: `EXTRACT(EPOCH)*1e6` gives fractional seconds
/// with µs precision; `::bigint` truncates to integer microseconds.
/// Matches PG's native TIMESTAMPTZ resolution. `list_builds_keyset`'s
/// WHERE reconstructs the timestamp via integer-seconds-plus-microsecond
/// -remainder so the round-trip is exact (see its doc comment — direct
/// bigint÷1e6 through `to_timestamp(float8)` would lose precision near
/// the 16-significant-figure IEEE754 limit).
pub(super) const LIST_BUILDS_SELECT: &str = r#"
    SELECT
        b.build_id,
        b.tenant_id::text,
        b.priority_class,
        b.status,
        b.error_summary,
        (EXTRACT(EPOCH FROM b.submitted_at) * 1e6)::bigint AS submitted_at_micros,
        COALESCE(COUNT(bd.derivation_id), 0)::bigint AS total_derivations,
        COALESCE(COUNT(*) FILTER (WHERE d.status = 'completed'), 0)::bigint
            AS completed_derivations,
        COALESCE(COUNT(*) FILTER (
            WHERE d.status = 'completed'
              AND NOT EXISTS (SELECT 1 FROM assignments a
                              WHERE a.derivation_id = d.derivation_id)
        ), 0)::bigint AS cached_derivations
    FROM builds b
    LEFT JOIN build_derivations bd USING (build_id)
    LEFT JOIN derivations d ON bd.derivation_id = d.derivation_id
"#;

impl SchedulerDb {
    /// Create a new database handle from a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get a reference to the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
