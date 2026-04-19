//! Direct DB integration tests via TestDb.
//!
//! Submodules mirror the production split: one test file per domain.
//! Shared helpers (`insert_test_derivation`, `TERMINAL_STATUSES`) live
//! here and are `pub(super)` so the domain test files can import them.

use uuid::Uuid;

use super::*;
use crate::state::{BuildState, BuildStateExt, DerivationStatus};

mod assignments;
mod batch;
mod builds;
mod derivations;
mod history;
mod tenants;
mod transactions;

/// Terminal status set as a test-time `&[&str]`. Drift-checked against
/// both `TERMINAL_STATUS_SQL` (same-file string form) and
/// `DerivationStatus::is_terminal()` (enum ground truth) by
/// [`transactions::test_terminal_statuses_match_is_terminal`].
pub(super) const TERMINAL_STATUSES: &[&str] = &[
    "completed",
    "poisoned",
    "dependency_failed",
    "cancelled",
    "skipped",
];

/// Upsert a single derivation and return its db_id. Helper for tests
/// that need a valid derivation_id foreign key.
pub(super) async fn insert_test_derivation(
    db: &SchedulerDb,
    drv_hash: &str,
) -> anyhow::Result<Uuid> {
    let mut tx = db.pool.begin().await?;
    let row = DerivationRow {
        drv_hash: drv_hash.into(),
        drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
        pname: Some("test-pkg".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
    };
    let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
    tx.commit().await?;
    Ok(ids.get(drv_hash).expect("just inserted").0)
}

#[test]
fn test_derivation_status_roundtrip() -> anyhow::Result<()> {
    // Iterate the macro-generated `ALL` so a new variant is covered
    // automatically — hand-maintained arrays drifted (`Substituting`
    // was missing for months).
    for status in DerivationStatus::ALL {
        let s = status.as_str();
        let parsed: DerivationStatus = s.parse()?;
        assert_eq!(parsed, *status);
    }
    Ok(())
}

#[test]
fn test_build_state_roundtrip() -> anyhow::Result<()> {
    // BuildState is a proto enum, NOT db_str_enum! — no `ALL`
    // generated. Hand list intentional; this is not exhaustive.
    for state in [
        BuildState::Pending,
        BuildState::Active,
        BuildState::Succeeded,
        BuildState::Failed,
        BuildState::Cancelled,
    ] {
        let s = state.as_str();
        let parsed = BuildState::parse_db(s)?;
        assert_eq!(parsed, state);
    }
    Ok(())
}

#[test]
fn test_assignment_status_as_str_exhaustive() {
    // AssignmentStatus is write-only to PG (no FromStr), so the
    // roundtrip is `as_str()` ↔ the schema literals. Iterate `ALL` and
    // assert each maps to its expected string, AND assert `ALL` covers
    // exactly the expected set — a new variant trips the second.
    let expected = [
        (AssignmentStatus::Pending, "pending"),
        (AssignmentStatus::Completed, "completed"),
        (AssignmentStatus::Failed, "failed"),
        (AssignmentStatus::Cancelled, "cancelled"),
    ];
    for (variant, lit) in expected {
        assert_eq!(variant.as_str(), lit);
    }
    assert_eq!(
        AssignmentStatus::ALL.len(),
        expected.len(),
        "new AssignmentStatus variant: pin its literal above"
    );
}
