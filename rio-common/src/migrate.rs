//! Shared sqlx migration runner — try-then-wait advisory lock.
//!
//! rio-store and rio-scheduler both run the same workspace-root
//! `migrations/` set against the same Postgres database. Both MUST
//! serialize via the same lock and the same non-blocking strategy:
//! sqlx's default blocking `pg_advisory_lock` deadlocks against
//! `CREATE INDEX CONCURRENTLY` (migrations 011, 022) when ≥2 replicas
//! start together (I-194), and sqlx's default lock key is a hash of
//! the database name — so two services calling raw `Migrator::run`
//! against the same DB would mutually exclude, but a service using
//! [`run`] and one using raw `Migrator::run` would NOT.
//!
//! Feature-gated on `postgres` so rio-common stays sqlx-free for
//! consumers that don't touch the DB (gateway, builder, controller).

use std::time::Duration;

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};
use tracing::{debug, info};

/// PG advisory-lock key serializing [`run`] across replicas AND
/// services. `0x724F_4D47_0001` = `"rOMG\0\1"` (rio MiGrate). Disjoint
/// from `rio_store::gc::GC_LOCK_ID` and from sqlx's own migrator lock
/// key (a hash of the database name — disabled here anyway).
pub const MIGRATE_LOCK_ID: i64 = 0x724F_4D47_0001;

/// Follower poll interval. Short enough that a follower resumes
/// within ~¼s of the leader finishing; long enough that a follower's
/// `pg_try_advisory_lock` SELECT (a sub-ms virtualxid) clears well
/// before a leader's `CREATE INDEX CONCURRENTLY` phase-3 wait could
/// stall on it for more than one tick.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Run `migrator` under a try-then-wait advisory lock instead of
/// sqlx's default blocking `pg_advisory_lock`.
///
// r[impl store.db.migrate-try-lock]
/// **Why not sqlx's built-in lock (I-194):** `Migrator::run` calls
/// blocking `pg_advisory_lock(...)` and holds it for the whole run.
/// Migrations 011 and 022 do `CREATE INDEX CONCURRENTLY` (under
/// `-- no-transaction`), whose final phase waits for every
/// virtualxid older than the index build to release. With ≥2
/// replicas starting together: replica A holds the advisory lock and
/// runs CIC; replica B sits in a blocked `SELECT
/// pg_advisory_lock(...)` — an in-progress statement holding a
/// virtualxid. A's CIC waits on B's vxid; B waits on A's advisory
/// lock → deadlock. PG's detector does NOT catch it (advisory-lock
/// waits and CIC's `WaitForOlderSnapshots` aren't in the same lock
/// graph), so both replicas wedge until a liveness probe kills one.
///
/// **Fix:** disable sqlx's lock (`set_locking(false)`) and serialize
/// via `pg_try_advisory_lock` + sleep-poll. A follower holds NO
/// long-lived vxid while waiting — each try is a sub-ms SELECT that
/// returns `false` and completes; between polls the follower is
/// asleep in tokio with zero PG state. The leader's CIC sees at
/// most one 250ms poll-tick of follower vxid, never a hold-forever.
/// Once the leader releases, each follower in turn acquires the
/// lock and re-runs the migrator — a no-op (sqlx skips applied
/// versions; the CIC indexes are `IF NOT EXISTS`).
///
/// The lock connection is `detach()`ed from the pool: on ANY exit
/// (`?`, panic, cancel) the raw `PgConnection` drops → socket
/// closes → PG releases the session-scoped lock. No
/// scopeguard-unlock dance needed.
///
/// `migrator` is taken by value: `set_locking` needs `&mut`, and
/// keeping the `sqlx::migrate!()` macro at the call site (each
/// crate's `main.rs` / tests) instead of in this shared crate
/// sidesteps the source-filter issue where rio-common's compile
/// unit may not see `migrations/` (it's a sibling, not a child).
pub async fn run(pool: &PgPool, mut migrator: Migrator) -> Result<(), MigrateError> {
    // Dedicated lock connection, detached so dropping it closes the
    // socket (releasing the session lock) on ANY exit path. NOT the
    // connection that runs migrations — `migrator.run(pool)`
    // acquires its own from the pool.
    let mut lock_conn = pool.acquire().await?.detach();

    let mut waited = false;
    loop {
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(MIGRATE_LOCK_ID)
            .fetch_one(&mut lock_conn)
            .await?;
        if acquired {
            break;
        }
        if !waited {
            info!("another replica is migrating; polling advisory lock");
            waited = true;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Leader (or follower that woke after leader released). sqlx's
    // own lock OFF — MIGRATE_LOCK_ID serializes instead. lock_conn
    // is now idle (SELECT completed → no vxid held), so the
    // migrator's CIC won't wait on it.
    migrator.set_locking(false);
    migrator.run(pool).await?;
    debug!(waited, "migrations applied");

    // Polite explicit unlock; failure is harmless (lock_conn drops
    // next, closing the socket → PG releases the session lock).
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATE_LOCK_ID)
        .execute(&mut lock_conn)
        .await;

    Ok(())
}
