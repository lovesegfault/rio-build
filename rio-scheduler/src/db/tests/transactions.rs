//! Remediation 12: PG transaction safety — drift checks + schema probes.

use rio_test_support::TestDb;

use super::TERMINAL_STATUSES;
use crate::db::TERMINAL_STATUS_SQL;

// r[verify sched.db.partial-index-literal]
/// Drift guard: `TERMINAL_STATUSES` const ⇔ `DerivationStatus::is_terminal()`.
///
/// Exhaustive over all variants. If you add a variant without
/// updating the match arm below, `non_exhaustive_patterns` fires.
/// If you add it to the match but flip terminality without updating
/// `TERMINAL_STATUSES`, the assertion fires.
///
/// ALSO asserts `TERMINAL_STATUS_SQL` is the `(a,b,c)`-joined form
/// of `TERMINAL_STATUSES` — catches the case where someone edits
/// one but not the other.
#[test]
fn test_terminal_statuses_match_is_terminal() {
    use crate::state::DerivationStatus::*;
    // Exhaustive binding — not `_ =>` — so adding a variant to
    // the enum is a compile error here until you handle it.
    let all = [
        Created,
        Queued,
        Ready,
        Assigned,
        Running,
        Completed,
        Failed,
        Poisoned,
        DependencyFailed,
        Cancelled,
        Skipped,
    ];
    // Compile-time exhaustiveness: this match has no wildcard.
    // Add a variant → this function stops compiling.
    for v in all {
        match v {
            Created | Queued | Ready | Assigned | Running | Completed | Failed | Poisoned
            | DependencyFailed | Cancelled | Skipped => {}
        }
    }

    let terminal_set: std::collections::HashSet<&str> = TERMINAL_STATUSES.iter().copied().collect();

    for v in all {
        let in_const = terminal_set.contains(v.as_str());
        let is_term = v.is_terminal();
        assert_eq!(
            in_const, is_term,
            "TERMINAL_STATUSES drift: {v:?}.is_terminal()={is_term} but \
             presence in TERMINAL_STATUSES={in_const}. Update db/mod.rs const \
             AND migrations/004_recovery.sql:85 partial index predicate."
        );
    }

    // SQL literal must be the quoted, comma-joined form of the const.
    // Order matters here only in the sense that both sides agree —
    // PG doesn't care about IN-list order.
    let expected_sql = format!(
        "({})",
        TERMINAL_STATUSES
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert_eq!(
        TERMINAL_STATUS_SQL, expected_sql,
        "TERMINAL_STATUS_SQL out of sync with TERMINAL_STATUSES const"
    );
}

// r[verify sched.db.partial-index-literal]
/// PG-side drift check: the partial index predicate in
/// `pg_indexes.indexdef` must match `TERMINAL_STATUSES`.
///
/// PG normalizes the predicate (adds `::text` casts, may reorder),
/// so we check that every status in `TERMINAL_STATUSES` appears as
/// a quoted literal in the indexdef, not that the strings are equal.
#[tokio::test]
async fn test_partial_index_predicate_matches_const() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let (indexdef,): (String,) = sqlx::query_as(
        "SELECT indexdef FROM pg_indexes \
         WHERE tablename = 'derivations' AND indexname = 'derivations_status_idx'",
    )
    .fetch_one(&test_db.pool)
    .await?;

    for status in TERMINAL_STATUSES {
        assert!(
            indexdef.contains(&format!("'{status}'")),
            "partial index predicate missing '{status}': {indexdef}"
        );
    }
    // And nothing extra — count quoted literals. PG normalizes
    // NOT IN (a,b) to <> ALL(ARRAY[a,b]) in indexdef output, so
    // count via the ::text cast suffix that PG attaches.
    let literal_count = indexdef.matches("'::text").count();
    assert_eq!(
        literal_count,
        TERMINAL_STATUSES.len(),
        "partial index has {literal_count} status literals, \
         TERMINAL_STATUSES has {}: {indexdef}",
        TERMINAL_STATUSES.len()
    );
    Ok(())
}

/// Schema probe for migrations 014/015/016. Every TestDb::new already
/// runs the full migrate chain (174 call sites), so apply-cleanly is
/// implicitly proven by any PG test passing. This probe asserts the
/// *shape* — tables exist, FKs reference the right targets, partial
/// index predicate is what the plan doc said.
///
/// P0256/P0253/P0259 land the consuming Rust; until then nothing
/// queries these tables, so shape-drift wouldn't surface organically.
#[tokio::test]
async fn test_migrations_014_015_016_schema() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;

    // --- 014: tenant_keys ---
    let (col_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.columns
         WHERE table_name = 'tenant_keys'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(col_count, 6, "tenant_keys: expected 6 columns");

    // Partial index predicate: pg_indexes.indexdef embeds the WHERE.
    let (idx_def,): (String,) = sqlx::query_as(
        "SELECT indexdef FROM pg_indexes
         WHERE tablename = 'tenant_keys'
           AND indexname = 'tenant_keys_active_idx'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!(
        idx_def.contains("WHERE") && idx_def.contains("revoked_at IS NULL"),
        "tenant_keys_active_idx missing partial predicate: {idx_def}"
    );

    // --- 014 batched: derivations.is_ca ---
    let (is_ca_default,): (String,) = sqlx::query_as(
        "SELECT column_default FROM information_schema.columns
         WHERE table_name = 'derivations' AND column_name = 'is_ca'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!(
        is_ca_default.to_lowercase().contains("false"),
        "derivations.is_ca default: {is_ca_default}"
    );

    // --- 015: realisation_deps ---
    // Composite FK count: two FKs, each two-column, both → realisations.
    let (fk_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.table_constraints
         WHERE table_name = 'realisation_deps'
           AND constraint_type = 'FOREIGN KEY'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(fk_count, 2, "realisation_deps: expected 2 FK constraints");

    // ON DELETE RESTRICT on both (not CASCADE — see migration comment).
    let (restrict_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.referential_constraints rc
         JOIN information_schema.table_constraints tc
           ON rc.constraint_name = tc.constraint_name
         WHERE tc.table_name = 'realisation_deps'
           AND rc.delete_rule = 'RESTRICT'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(
        restrict_count, 2,
        "realisation_deps: both FKs must be ON DELETE RESTRICT"
    );

    let (rev_idx,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM pg_indexes
         WHERE tablename = 'realisation_deps'
           AND indexname = 'realisation_deps_reverse_idx')",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!(rev_idx, "realisation_deps_reverse_idx missing");

    // --- 016: jwt_revoked + builds.jwt_jti ---
    let (jwt_cols,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.columns
         WHERE table_name = 'jwt_revoked'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(jwt_cols, 3, "jwt_revoked: expected 3 columns");

    let (jti_nullable,): (String,) = sqlx::query_as(
        "SELECT is_nullable FROM information_schema.columns
         WHERE table_name = 'builds' AND column_name = 'jwt_jti'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(jti_nullable, "YES", "builds.jwt_jti must be nullable");

    Ok(())
}
