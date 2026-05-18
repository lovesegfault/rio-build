//! Single source of truth for database migrations.
//!
//! Owns `migrations/*.sql`, the embedded `Migrator`, the per-migration
//! commentary doc-consts (`migrations` module), and the checksum-freeze
//! test (`tests/migrations.rs`).
//!
//! Consuming crates (`rio-store`, `rio-scheduler`, `rio-controller`,
//! `rio-gateway`) re-export [`MIGRATOR`] so existing
//! `crate::MIGRATOR` / `TestDb::new(&...MIGRATOR)` callsites keep
//! resolving. Production startup callers that need an *owned*
//! `Migrator` use [`migrator`].

/// Embedded migrator. Use `&MIGRATOR` for test fixtures (`TestDb::new`).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Fresh `Migrator` value for callers that need ownership.
///
/// `sqlx::migrate::Migrator` is NOT `Clone` in sqlx 0.8.x (derives
/// `Debug` only). `rio_common::migrate::run` takes `Migrator` by value
/// because `set_locking` needs `&mut`. This function re-invokes the
/// macro to produce a fresh owned value, sidestepping the missing
/// `Clone`.
pub fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

pub mod migrations;
