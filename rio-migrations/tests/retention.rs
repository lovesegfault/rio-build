//! The table-lifecycle gate (bughunt wave D1, merged_bug_163).
//!
//! Diffs `pg_tables` (schema `public`, minus sqlx's own bookkeeping
//! table) against `rio_migrations::retention::RETENTION_REGISTRY`.
//! A migration that creates a table without declaring its row
//! lifecycle — a named sweeper or a written keep-forever rationale —
//! fails here, naming the table. Views are structurally exempt
//! (`pg_tables` lists tables only).

use std::collections::BTreeSet;

use rio_migrations::retention::RETENTION_REGISTRY;

// r[verify sched.db.table-retention+1]
#[tokio::test]
async fn every_public_table_has_a_retention_decision() {
    let db = rio_test_support::TestDb::new(&rio_migrations::MIGRATOR).await;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename FROM pg_tables \
         WHERE schemaname = 'public' AND tablename <> '_sqlx_migrations' \
         ORDER BY tablename",
    )
    .fetch_all(&db.pool)
    .await
    .expect("pg_tables enumeration");

    let live: BTreeSet<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let registered: BTreeSet<&str> = RETENTION_REGISTRY.iter().map(|(t, _)| *t).collect();

    let unregistered: Vec<&&str> = live.difference(&registered).collect();
    let phantom: Vec<&&str> = registered.difference(&live).collect();

    assert!(
        unregistered.is_empty(),
        "tables with NO row-lifecycle decision — add a RETENTION_REGISTRY row \
         (name the sweeper that deletes rows, or write the KeepForever rationale): \
         {unregistered:?}"
    );
    assert!(
        phantom.is_empty(),
        "registry rows for tables that no longer exist — remove them: {phantom:?}"
    );
}

/// The registry stays alphabetical so the failure diff above is stable.
#[test]
fn registry_is_sorted_and_unique() {
    let names: Vec<&str> = RETENTION_REGISTRY.iter().map(|(t, _)| *t).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        names, sorted,
        "RETENTION_REGISTRY must be alphabetical and duplicate-free"
    );
}
