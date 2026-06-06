//! Cross-service row types — compile-time schema contract.
//!
//! rio-scheduler and rio-store share one Postgres database. Some
//! tables are scheduler-OWNED but store-READ: `scheduler_live_pins`
//! (GC mark/sweep seeds from it) and `tenants` (cache-server auth +
//! GC quota lookup). Before this module the contract between the two
//! services was the column shape only, checked at RUNTIME by
//! `rio-migrations/tests/migrations.rs::cross_service_schema_contract` —
//! a scheduler-side migration that renamed/retyped a column would
//! pass `cargo build` and fail in a test (or worse, production).
//!
//! Defining the row structs HERE and having both crates `query_as!`
//! into them upgrades the contract to compile-time: a column rename
//! or retype breaks `cargo sqlx prepare` (and therefore `cargo build`
//! under `SQLX_OFFLINE`), not production. The runtime test stays as
//! defense-in-depth — it catches a `query_as!` → runtime `query_as`
//! regression that would silently drop the compile-time check.

use uuid::Uuid;

/// `scheduler_live_pins` row.
///
/// Scheduler INSERTs at dispatch (`rio-scheduler/src/db/live_pins.rs`
/// → `pin_live_inputs`); store seeds the mark CTE from it and
/// re-checks during sweep (`rio-store/src/gc/{mark,sweep}.rs`).
///
/// `pinned_at` deliberately omitted: neither service reads it (it's
/// observability-only). Including it would force every `query_as!`
/// site to project it.
#[derive(Debug, sqlx::FromRow)]
pub struct LivePin {
    /// SHA-256 of the full store path. Same keying as
    /// `narinfo.store_path_hash` — mark.rs JOINs the two on this.
    pub store_path_hash: Vec<u8>,
    /// Scheduler's derivation hash; the unpin key on terminal status.
    pub drv_hash: String,
}

/// `tenants` row.
///
/// Scheduler owns CRUD (`rio-scheduler/src/db/tenants.rs`); store
/// reads for GC quota (`rio-store/src/gc/tenant.rs`, lookup by
/// `tenant_name`).
///
/// `cache_token` is intentionally NOT a field: round-tripping the
/// secret through every list result is a foot-gun. `has_cache_token`
/// is the safe projection.
/// `created_at` is epoch seconds via `EXTRACT(EPOCH FROM
/// created_at)::bigint` — keeps this module chrono-free.
#[derive(Debug, sqlx::FromRow)]
pub struct TenantRow {
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub gc_retention_hours: i32,
    pub gc_max_store_bytes: Option<i64>,
    pub has_cache_token: bool,
    pub created_at: i64,
}

/// The `drv_executions.status` vocabulary. The scheduler's terminal UPDATE
/// writes one of these; the store's completeness predicate tests membership
/// in [`EXEC_STATUS_TERMINAL`]. This is deliberately NOT the same vocabulary
/// as `assignments.status` (which spells success "completed") — do not
/// unify them by accident.
pub const EXEC_STATUS_SUCCEEDED: &str = "succeeded";
/// See [`EXEC_STATUS_SUCCEEDED`].
pub const EXEC_STATUS_FAILED: &str = "failed";
/// See [`EXEC_STATUS_SUCCEEDED`].
pub const EXEC_STATUS_CANCELLED: &str = "cancelled";
/// The statuses that mean "this execution will never produce another log
/// line". The store's completeness predicate tests `status` membership
/// here; a NULL or unlisted status reads as still-running (incomplete).
pub const EXEC_STATUS_TERMINAL: &[&str] = &[
    EXEC_STATUS_SUCCEEDED,
    EXEC_STATUS_FAILED,
    EXEC_STATUS_CANCELLED,
];

/// `drv_executions` row (063).
///
/// Scheduler-OWNED, store-READ: rio-scheduler INSERTs at dispatch and
/// UPDATEs `status`/`finished_at`/`final_line_count` at terminal;
/// rio-store resolves the latest BUILD execution through the
/// kind-filtered `latest_build_exec` view (M_089; raw exec-ordered
/// reads are banned by `log-no-raw-latest-exec`) and reads the
/// log-completeness predicate
/// (`status` is terminal AND `final_line_count` known AND the chunk
/// manifest covers `[0, final_line_count)`), and its TTL sweep
/// DELETEs by `started_at` age.
///
/// `drv_hash` is the `drv_log_hash()` form (32-char bare hash,
/// CHAR(32)/`bpchar`) — NOT `derivations.drv_hash` (the DAG key,
/// TEXT). Nothing joins the two; see M_061/M_066.
///
/// Timestamps are epoch seconds via `EXTRACT(EPOCH FROM …)::bigint`
/// at the query site — keeps this module chrono-free (the `TenantRow`
/// convention).
#[derive(Debug, sqlx::FromRow)]
pub struct DrvExecutionRow {
    pub exec_id: Uuid,
    pub drv_hash: String,
    pub executor_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// One of [`EXEC_STATUS_SUCCEEDED`] | [`EXEC_STATUS_FAILED`] |
    /// [`EXEC_STATUS_CANCELLED`]; NULL while running. Writers MUST use
    /// the constants — see [`EXEC_STATUS_TERMINAL`].
    pub status: Option<String>,
    /// Total lines incl. the banner header/footer; NULL until the
    /// builder's CompletionReport carries it. The completeness
    /// predicate's upper bound.
    pub final_line_count: Option<i64>,
}

/// `drv_log_chunks` row (063).
///
/// Store-OWNED, store-written (INSERT-only, idempotent on the
/// `(exec_id, session_id, chunk_seq)` PK): the line-range manifest
/// for one execution's log. Two ingest sessions for the same
/// execution may record overlapping `[first_line, first_line +
/// line_count)` ranges; readers dedup by line number. Defined here
/// (rather than in rio-store) so a future scheduler/dashboard
/// manifest read inherits the compile-time column contract.
#[derive(Debug, sqlx::FromRow)]
pub struct DrvLogChunkRow {
    pub exec_id: Uuid,
    pub session_id: Uuid,
    pub chunk_seq: i32,
    pub first_line: i64,
    pub line_count: i64,
    pub byte_size: i64,
    pub s3_key: String,
    /// Epoch seconds (`EXTRACT(EPOCH FROM …)::bigint` at the query
    /// site).
    pub created_at: i64,
}

/// `log_ingest_sessions` row (063).
///
/// Store-OWNED, store-written: the live-ingest routing registry. At
/// most one live session per execution (PK on `exec_id`); `acquire`
/// steals a row whose `heartbeat_at` is older than 30 s. Readers
/// treat a row with a stale heartbeat as dead and serve history-only.
#[derive(Debug, sqlx::FromRow)]
pub struct LogIngestSessionRow {
    pub exec_id: Uuid,
    pub session_id: Uuid,
    pub replica_pod: String,
    /// Epoch seconds (`EXTRACT(EPOCH FROM …)::bigint` at the query
    /// site).
    pub started_at: i64,
    /// Epoch seconds; the 15 s heartbeat / 30 s staleness lease.
    pub heartbeat_at: i64,
}
