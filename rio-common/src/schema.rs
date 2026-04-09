//! Cross-service row types — compile-time schema contract.
//!
//! rio-scheduler and rio-store share one Postgres database. Some
//! tables are scheduler-OWNED but store-READ: `scheduler_live_pins`
//! (GC mark/sweep seeds from it) and `tenants` (cache-server auth +
//! GC quota lookup). Before this module the contract between the two
//! services was the column shape only, checked at RUNTIME by
//! `rio-store/tests/migrations.rs::cross_service_schema_contract` —
//! a scheduler-side migration that renamed/retyped a column would
//! pass `cargo build` and fail in a test (or worse, production).
//!
//! Defining the row structs HERE and having both crates `query_as!`
//! into them upgrades the contract to compile-time: a column rename
//! or retype breaks `cargo sqlx prepare` (and therefore `cargo build`
//! under `SQLX_OFFLINE`), not production. The runtime test stays as
//! defense-in-depth — it catches a `query_as!` → runtime `query_as`
//! regression that would silently drop the compile-time check.
//!
//! Feature-gated on `postgres` (same as [`crate::migrate`]) so
//! gateway/builder/controller stay sqlx-free.

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
