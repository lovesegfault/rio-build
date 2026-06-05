//! Shared sqlx migration runner — try-then-wait advisory lock.
//!
//! Production migrations run in exactly one place — `rio-store
//! migrate` ([`run_with_roles`]); app startup only verifies via
//! [`assert_current`]. The lock still matters: concurrent runner
//! invocations (old-tag/new-tag migrate Jobs racing during an
//! upgrade, a runner racing a legacy in-pod migrator during
//! mid-upgrade skew) MUST serialize via the same key and the same
//! non-blocking strategy: sqlx's default blocking `pg_advisory_lock`
//! deadlocks against `CREATE INDEX CONCURRENTLY` (migrations 011,
//! 022) when two runners start together (I-194), and sqlx's default
//! lock key is a hash of the database name — so two callers of raw
//! `Migrator::run` against the same DB would mutually exclude, but a
//! caller using [`run`] and one using raw `Migrator::run` would NOT.

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
// r[impl store.db.migrate-try-lock+2]
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
/// `Migrator` is not `Clone`. Callers pass [`crate::migrator()`]
/// to get a fresh owned value.
pub async fn run(pool: &PgPool, mut migrator: Migrator) -> Result<(), MigrateError> {
    let mut lock_conn = acquire_lock(pool).await?;
    apply(pool, &mut migrator).await?;
    release_lock(&mut lock_conn).await;
    Ok(())
}

/// [`run`] plus [`crate::ensure_roles::ensure_roles`], BOTH under the
/// same advisory-lock hold. This is the production entry point
/// (`rio-store migrate`).
///
/// The role pass MUST run before the unlock: the migrate Job design
/// deliberately allows concurrent old-tag/new-tag runner invocations,
/// and role DDL is CLUSTER-wide — two unserialized ensure_roles
/// passes would race on pg_authid (`tuple concurrently updated`,
/// duplicate CREATE ROLE), and one run's defensive REASSIGN+REVOKE
/// detach could interleave with another run's grants, re-opening a
/// transient ACL-strip window. Serialized, the "grants re-asserted
/// every run" recurrence-proof holds. (PG advisory locks are
/// server-wide, not per-database, so this also serializes role DDL
/// across the shared-PG-server llvm-cov test topology.)
pub async fn run_with_roles(pool: &PgPool, mut migrator: Migrator) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let mut lock_conn = acquire_lock(pool).await?;
    apply(pool, &mut migrator)
        .await
        .context("applying migrations")?;
    crate::ensure_roles::ensure_roles(&mut lock_conn)
        .await
        .context("ensure_roles (rio_app role/grants reconciliation)")?;
    release_lock(&mut lock_conn).await;
    Ok(())
}

/// Acquire `MIGRATE_LOCK_ID` on a dedicated connection, detached so
/// dropping it closes the socket (releasing the session lock) on ANY
/// exit path (`?`, panic, cancel). NOT the connection that runs
/// migrations — `migrator.run(pool)` acquires its own from the pool.
async fn acquire_lock(pool: &PgPool) -> Result<sqlx::postgres::PgConnection, MigrateError> {
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
            info!("another runner is migrating; polling advisory lock");
            waited = true;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Ok(lock_conn)
}

/// Run the migrator with sqlx's own lock OFF (`MIGRATE_LOCK_ID`
/// serializes instead; the caller holds it — its lock connection is
/// idle, so the migrator's CIC won't wait on it).
///
/// `ignore_missing`: applied-but-not-embedded versions are accepted.
/// Two callers depend on this:
/// - the documented binary-rollback path (deploy the previous binary
///   against a newer schema — without the flag, sqlx's
///   `validate_applied_migrations` fails `VersionMissing`);
/// - the un-embedding of the retired role migrations (069/070 on the
///   deployed lineage): their applied rows stay in
///   `_sqlx_migrations` as inert history. Reverting this flag would
///   brick those deploys with `VersionMissing(69)`.
///
/// Checksum protection is unaffected — `VersionMismatch` on content
/// drift of EMBEDDED versions still fails, and the CI freeze test
/// pins content. Residual hard-fail: sqlx's dirty check aborts on ANY
/// `success=false` row (embedded or not) BEFORE validation; an
/// un-embedded failed row can never be re-applied, so the remedy is a
/// manual row delete (see deployment.typ rollback notes).
async fn apply(pool: &PgPool, migrator: &mut Migrator) -> Result<(), MigrateError> {
    migrator.set_locking(false);
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    debug!("migrations applied");
    Ok(())
}

/// Polite explicit unlock; failure is harmless (the connection drops
/// soon after, closing the socket → PG releases the session lock).
async fn release_lock(lock_conn: &mut sqlx::postgres::PgConnection) {
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATE_LOCK_ID)
        .execute(lock_conn)
        .await;
}

/// Verify every embedded migration has been applied, without running
/// any. Services call this at startup instead of [`run`] — migrations
/// execute out-of-band (`rio-store migrate`: the helm `rio-migrate`
/// Job on k8s, the `rio-migrate` systemd oneshot on standalone
/// NixOS), so a pod that comes up against a missing or stale schema
/// fails HERE with a message naming the runner, not minutes later
/// with "relation does not exist" mid-request.
///
/// Only `embedded ⊆ applied` is checked. Applied-but-not-embedded
/// versions are EXPECTED during a rolling upgrade: the pre-upgrade
/// hook migrates first, then old-binary replicas may still restart
/// against the newer schema (migrations are forward-compatible by
/// policy — see `docs/spec/system/deployment.typ`). Checksums are
/// not compared either; `migration_checksums_frozen` pins them at CI
/// time and [`run`] rejects mismatches at apply time — for EMBEDDED
/// versions only: with `ignore_missing` on, applied-but-not-embedded
/// rows are skipped entirely (no checksum to compare against).
// r[impl store.db.schema-current+2]
pub async fn assert_current(pool: &PgPool) -> anyhow::Result<()> {
    const RUNNER_HINT: &str = "run `rio-store migrate` (helm: the rio-migrate Job, \
         standalone NixOS: rio-migrate.service) against this database";

    // `_sqlx_migrations` is created by the first migrator run; its
    // absence means no migration has ever been applied here. ONLY
    // undefined_table (42P01) gets the "schema missing — run the
    // migration runner" context: a transient connection drop, an
    // exhausted pool, or a permission error here is NOT a missing
    // schema, and pointing the operator at the runner for those sends
    // the diagnosis in exactly the wrong direction.
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
            .fetch_all(pool)
            .await
            .map_err(|e| {
                let undefined_table = matches!(
                    &e,
                    sqlx::Error::Database(db) if db.code().as_deref() == Some("42P01")
                );
                if undefined_table {
                    anyhow::anyhow!(e).context(format!(
                        "database schema missing (no _sqlx_migrations table); {RUNNER_HINT}"
                    ))
                } else {
                    anyhow::anyhow!(e).context("schema check query failed (_sqlx_migrations)")
                }
            })?;

    let missing: Vec<i64> = crate::MIGRATOR
        .iter()
        .map(|m| m.version)
        .filter(|v| !applied.contains(v))
        .collect();
    anyhow::ensure!(
        missing.is_empty(),
        "database schema is stale: migrations {missing:?} are not applied; {RUNNER_HINT}"
    );
    Ok(())
}
