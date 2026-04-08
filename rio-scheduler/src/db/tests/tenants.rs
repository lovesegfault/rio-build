//! Tenant CRUD integration tests.

use rio_store::test_helpers::{TenantSeed, seed_tenant};
use rio_test_support::TestDb;

/// TenantSeed: columns not `.with_*()`'d take their schema default.
/// Proves the Option-bind plumbing — `gc_retention_hours` is `NOT
/// NULL DEFAULT 168`, so bare seeds get 168 (not NULL); `cache_token`
/// is nullable, so bare seeds get NULL. Lives here (not in
/// rio-test-support) because rio-scheduler already dev-depends on
/// rio-test-support — reversing that for a smoke test isn't worth it.
#[tokio::test]
async fn tenant_seed_optional_columns() {
    let db = TestDb::new(&crate::MIGRATOR).await;

    let bare = seed_tenant(&db.pool, "bare").await;
    let full = TenantSeed::new("full")
        .with_retention_hours(48)
        .with_cache_token("sekrit")
        .seed(&db.pool)
        .await;

    let (bare_retention, bare_token): (i32, Option<String>) =
        sqlx::query_as("SELECT gc_retention_hours, cache_token FROM tenants WHERE tenant_id = $1")
            .bind(bare)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        bare_retention, 168,
        "bare seed → schema DEFAULT 168 (NOT NULL)"
    );
    assert_eq!(bare_token, None, "bare seed → cache_token NULL");

    let (full_retention, full_token): (i32, Option<String>) =
        sqlx::query_as("SELECT gc_retention_hours, cache_token FROM tenants WHERE tenant_id = $1")
            .bind(full)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(full_retention, 48);
    assert_eq!(full_token.as_deref(), Some("sekrit"));
}

/// Migration 020 adds a CHECK constraint that rejects tenant
/// names violating the `NormalizedName` invariant (untrimmed,
/// empty, or containing interior whitespace). This makes the
/// rio-store auth.rs "PG-stored name failed normalization"
/// branch provably dead for post-migration rows.
///
/// Probe via direct INSERT — bypasses CreateTenant's Rust-side
/// validation, so a failing INSERT proves the *database* layer
/// rejects bad input, not just the application layer.
// r[verify sched.admin.create-tenant]
#[tokio::test]
async fn migration_020_check_rejects_non_normalized_name() {
    let test_db = TestDb::new(&crate::MIGRATOR).await;

    // Each bad case should fail the CHECK with a constraint-name
    // mention in the error. Positive control at the end: a good
    // name still inserts. Without the positive control, a
    // universally-rejecting CHECK (e.g., `CHECK (false)`) would
    // pass all the negative assertions.
    let bad_cases = [
        ("  team  ", "leading+trailing whitespace (trim mismatch)"),
        ("team a", "interior space"),
        ("team\tb", "interior tab"),
        ("", "empty string"),
        ("   ", "whitespace-only"),
    ];

    for (bad, why) in bad_cases {
        let result = sqlx::query("INSERT INTO tenants (tenant_name) VALUES ($1)")
            .bind(bad)
            .execute(&test_db.pool)
            .await;
        let err = result.expect_err(&format!(
            "CHECK should reject {bad:?} ({why}); INSERT succeeded"
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("tenant_name_normalized"),
            "error for {bad:?} should name the constraint \
             `tenant_name_normalized` — got: {msg}"
        );
    }

    // Positive control: a normalized name inserts cleanly.
    // Proves the constraint isn't over-broad.
    sqlx::query("INSERT INTO tenants (tenant_name) VALUES ($1)")
        .bind("team-a")
        .execute(&test_db.pool)
        .await
        .expect("normalized name must pass the CHECK constraint");
}
