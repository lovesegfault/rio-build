//! Single source of truth for database migrations.
//!
//! Owns `migrations/*.sql`, the embedded `Migrator`, the advisory-lock
//! migration runner ([`migrate`]), the cross-service row types
//! ([`schema`]), the per-migration commentary doc-consts
//! ([`migrations`]), and the checksum-freeze test
//! (`tests/migrations.rs`).
//!
//! Consuming crates (`rio-store`, `rio-scheduler`, `rio-controller`,
//! `rio-gateway`) re-export [`MIGRATOR`] so existing
//! `crate::MIGRATOR` / `TestDb::new(&...MIGRATOR)` callsites keep
//! resolving. Production startup callers that need an *owned*
//! `Migrator` use [`migrator`].

// Out-of-band macro inputs surfaced as tracked env-deps (see build.rs):
// the env! reads record dep-info `# env-dep:` lines, so cargo AND
// content-keyed rustc-wrapper caches (kache) re-key this crate when
// migrations/ or .sqlx/ change without any .rs edit.
const _: &str = env!("RIO_MIGRATIONS_HASH");
const _: &str = env!("RIO_SQLX_HASH");

/// Embedded migrator. Use `&MIGRATOR` for test fixtures (`TestDb::new`).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Fresh `Migrator` value for callers that need ownership.
///
/// `sqlx::migrate::Migrator` is not `Clone` (derives `Debug` only).
/// [`migrate::run`] takes `Migrator` by value because `set_locking`
/// needs `&mut`. This function re-invokes the macro to produce a
/// fresh owned value, sidestepping the missing `Clone`.
pub fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

pub mod migrate;
pub mod migrations;
pub mod schema;
