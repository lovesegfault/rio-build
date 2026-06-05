//! Runner-owned role and grant management for `rio_app`.
//!
//! Roles and grants are **desired state**, not schema history: they
//! are cluster-wide (a migration runs once per *database*), they are
//! environment-dependent (`rds_iam` exists only on RDS/Aurora), and
//! they drift (manual incident recoveries, RDS surgery). Shipping
//! them as checksum-frozen migrations put state-reconciliation logic
//! into an append-only log, and that shape produced two live
//! incidents in two weeks:
//!
//! 1. **Master PAM lockout.** A frozen role migration granted
//!    `rio_app` (a member of `rds_iam`) to the master user for an
//!    ownership transfer. With `iam_database_authentication_enabled`
//!    on the Aurora cluster, RDS PAM classifies a role as IAM-only
//!    when it holds `rds_iam` directly OR BY INHERITANCE — the master
//!    lost password auth (`PAM authentication failed for user "rio"`)
//!    and the migration runner itself was locked out.
//! 2. **REASSIGN ACL strip.** The frozen follow-up fix ran
//!    `REASSIGN OWNED BY rio_app TO <master>`, which rewrites the
//!    owner entries of the transferred objects' ACLs — silently
//!    stripping ALL of rio_app's table/sequence privileges. App pods
//!    crash-looped on `permission denied for table _sqlx_migrations`
//!    while the migrate Job itself reported success; recovery was a
//!    manual `GRANT ALL`.
//!
//! Both fixes were themselves unfixable in place (checksum freeze),
//! and each fix-attempt shipped a new frozen bug. This module is the
//! structural fix: an idempotent reconciliation pass that re-asserts
//! the role and its grants on EVERY migrate run, so manual drift
//! (like the incident-2 recovery) self-heals on the next deploy and a
//! grant bug is a one-line code fix, not a new frozen migration.
//!
//! Invariants:
//! - `rio_app` has LOGIN and (on RDS) `rds_iam` membership — that
//!   membership is what flips it from password auth to IAM-token
//!   auth.
//! - The master is NEVER a member of `rio_app` (incident 1). The
//!   runner always connects as the master, so rio_app needs no
//!   ownership and the master needs no membership; a legacy
//!   membership found on an old database is detached
//!   (REASSIGN-then-REVOKE — REASSIGN acts through the very
//!   membership the REVOKE removes), and the unconditional re-grants
//!   below close incident 2's ACL-strip window in the same run.
//! - Grants mirror the master's reach over application objects
//!   (`ALL` on tables/sequences + default privileges for BOTH object
//!   kinds — a tables-only default leaves serial/identity sequences
//!   permission-denied on the first post-migration insert).
//!
//! Degradation contract: where the runner lacks privileges — k3s
//! migrates as the unprivileged bitnami app user, which cannot
//! `CREATE ROLE` — every privileged step degrades to a WARNING and
//! the pass returns cleanly. Deployments that cannot set up IAM auth
//! are exactly the deployments that never use it. (A bare CREATE
//! ROLE once crash-looped every store/scheduler pod on k3s.)
//!
//! When the runner IS superuser (rio-test-support ephemeral PG,
//! standalone NixOS with a privileged DB user), the role and grants
//! are created in test databases too. Intentional and harmless: the
//! behavior is pinned by the idempotency test, not accidental.
//!
//! Concurrency: call through [`crate::migrate::run_with_roles`],
//! which holds `MIGRATE_LOCK_ID` across migrations AND this pass. PG
//! advisory locks are server-wide (not per-database), so concurrent
//! runners — including old-tag/new-tag migrate Jobs racing during an
//! upgrade, and the llvm-cov shared-PG-server test topology — cannot
//! interleave cluster-wide role DDL (`tuple concurrently updated` on
//! pg_authid, duplicate CREATE ROLE) or re-open a transient
//! ACL-strip window between one run's detach and its re-grants.

use sqlx::PgConnection;
use tracing::{info, warn};

/// SQLSTATE for `insufficient_privilege` — the "unprivileged runner"
/// degradation signal (k3s bitnami user).
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// Re-assert the `rio_app` role, its `rds_iam` membership, and its
/// grants. Idempotent; degrades to warnings where the connected user
/// lacks privileges. See the module docs for the full contract.
///
/// Production callers go through [`crate::migrate::run_with_roles`]
/// so the pass runs under the migration advisory lock; calling this
/// bare is only safe when the connected user cannot perform DDL at
/// all (the unprivileged-degradation test).
// r[impl store.db.ensure-roles]
pub async fn ensure_roles(conn: &mut PgConnection) -> anyhow::Result<()> {
    // 1. Role exists with LOGIN. Existence-checked first: roles are
    //    cluster-wide while this pass runs once per database, so a
    //    parallel runner on a sibling database may have just created
    //    it (the duplicate_object arm covers the remaining race).
    if !role_exists(conn, "rio_app").await? {
        match sqlx::query("CREATE ROLE rio_app WITH LOGIN")
            .execute(&mut *conn)
            .await
        {
            Ok(_) => info!("created rio_app role"),
            Err(e) if sqlstate(&e).as_deref() == Some(INSUFFICIENT_PRIVILEGE) => {
                warn!(
                    "ensure_roles: connected user cannot CREATE ROLE; skipping \
                     rio_app setup (expected on k3s/local postgres — those \
                     deployments never use IAM auth)"
                );
                return Ok(());
            }
            // duplicate_object / unique_violation: raced a sibling
            // runner's create between our check and this statement.
            Err(e) if matches!(sqlstate(&e).as_deref(), Some("42710" | "23505")) => {}
            Err(e) => return Err(anyhow::Error::new(e).context("CREATE ROLE rio_app")),
        }
    }

    // 2. rds_iam membership. The role only exists on RDS/Aurora;
    //    membership is what switches rio_app from password auth to
    //    IAM-token auth there. Checked via pg_auth_members, NOT
    //    pg_has_role(..., 'SET'): the 'SET' privilege type is
    //    PG16-only while deployment.typ promises PG 15+.
    if role_exists(conn, "rds_iam").await? {
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1 FROM pg_auth_members m
               JOIN pg_roles g ON g.oid = m.roleid
               JOIN pg_roles mem ON mem.oid = m.member
               WHERE g.rolname = 'rds_iam' AND mem.rolname = 'rio_app')",
        )
        .fetch_one(&mut *conn)
        .await?;
        if !member {
            if let Err(e) = sqlx::query("GRANT rds_iam TO rio_app")
                .execute(&mut *conn)
                .await
            {
                if sqlstate(&e).as_deref() == Some(INSUFFICIENT_PRIVILEGE) {
                    warn!("ensure_roles: cannot GRANT rds_iam TO rio_app; skipping");
                    return Ok(());
                }
                return Err(anyhow::Error::new(e).context("GRANT rds_iam TO rio_app"));
            }
            info!("granted rds_iam to rio_app");
        }
    }

    // 3. Defensive detach (legacy databases where a frozen role
    //    migration granted rio_app to the master — incident 1).
    //    Matched on an INHERITING pg_auth_members row: on PG16+ the
    //    creator of a role holds an implicit ADMIN-only membership
    //    (INHERIT FALSE) that cannot trip RDS PAM and whose
    //    revocation would strip the creator's ADMIN — only the
    //    explicit legacy grant had INHERIT. Pre-16 there is no
    //    implicit creator row at all, and pg_auth_members has no
    //    inherit_option column, so any row is the legacy grant.
    let pg16: bool =
        sqlx::query_scalar("SELECT current_setting('server_version_num')::int >= 160000")
            .fetch_one(&mut *conn)
            .await?;
    let detach_sql = if pg16 {
        "SELECT EXISTS (
           SELECT 1 FROM pg_auth_members m
           JOIN pg_roles g ON g.oid = m.roleid
           JOIN pg_roles mem ON mem.oid = m.member
           WHERE g.rolname = 'rio_app' AND mem.rolname = current_user
             AND m.inherit_option)"
    } else {
        "SELECT EXISTS (
           SELECT 1 FROM pg_auth_members m
           JOIN pg_roles g ON g.oid = m.roleid
           JOIN pg_roles mem ON mem.oid = m.member
           WHERE g.rolname = 'rio_app' AND mem.rolname = current_user)"
    };
    let inherits_rio_app: bool = sqlx::query_scalar(detach_sql).fetch_one(&mut *conn).await?;
    if inherits_rio_app {
        // REASSIGN before REVOKE: REASSIGN acts with rio_app's
        // privileges through the membership the REVOKE removes.
        // REASSIGN rewrites owner-ACL entries (incident 2) — the
        // unconditional re-grants in step 4 repair that in the same
        // lock-held run, so the strip is never observable.
        for stmt in [
            "REASSIGN OWNED BY rio_app TO CURRENT_USER",
            "REVOKE rio_app FROM CURRENT_USER",
        ] {
            if let Err(e) = sqlx::query(stmt).execute(&mut *conn).await {
                if sqlstate(&e).as_deref() == Some(INSUFFICIENT_PRIVILEGE) {
                    warn!(
                        stmt,
                        "ensure_roles: cannot detach master from rio_app; skipping"
                    );
                    return Ok(());
                }
                return Err(anyhow::Error::new(e).context(stmt));
            }
        }
        warn!(
            "ensure_roles: detached {} from legacy rio_app membership \
             (REASSIGN + REVOKE); grants re-asserted below",
            "current_user"
        );
    }

    // 4. Grants — unconditional, every run. This is the self-healing
    //    half of the contract: a database whose rio_app grants were
    //    stripped (incident 2) or manually repaired converges on the
    //    next deploy. ALTER DEFAULT PRIVILEGES covers objects created
    //    by future migrations, for BOTH tables and sequences.
    for stmt in [
        "GRANT USAGE, CREATE ON SCHEMA public TO rio_app",
        "GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO rio_app",
        "GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public TO rio_app",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL PRIVILEGES ON TABLES TO rio_app",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL PRIVILEGES ON SEQUENCES TO rio_app",
    ] {
        if let Err(e) = sqlx::query(stmt).execute(&mut *conn).await {
            if sqlstate(&e).as_deref() == Some(INSUFFICIENT_PRIVILEGE) {
                warn!(
                    stmt,
                    "ensure_roles: insufficient privilege; skipping remaining grants"
                );
                return Ok(());
            }
            return Err(anyhow::Error::new(e).context(stmt));
        }
    }
    Ok(())
}

async fn role_exists(conn: &mut PgConnection, role: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
        .bind(role)
        .fetch_one(conn)
        .await
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Database(db) => db.code().map(|c| c.into_owned()),
        _ => None,
    }
}
