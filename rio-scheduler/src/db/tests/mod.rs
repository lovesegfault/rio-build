//! Direct DB integration tests via TestDb.
//!
//! Submodules mirror the production split: one test file per domain.
//! Shared helpers (`insert_test_derivation`, `TERMINAL_STATUSES`) live
//! here and are `pub(super)` so the domain test files can import them.

use uuid::Uuid;

use super::*;
use crate::state::{BuildState, DerivationStatus};

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
fn test_ema_alpha_range() {
    const { assert!(EMA_ALPHA > 0.0 && EMA_ALPHA < 1.0) };
}

#[test]
fn test_derivation_status_roundtrip() -> anyhow::Result<()> {
    for status in [
        DerivationStatus::Created,
        DerivationStatus::Queued,
        DerivationStatus::Ready,
        DerivationStatus::Assigned,
        DerivationStatus::Running,
        DerivationStatus::Completed,
        DerivationStatus::Failed,
        DerivationStatus::Poisoned,
        DerivationStatus::DependencyFailed,
        DerivationStatus::Cancelled,
        DerivationStatus::Skipped,
    ] {
        let s = status.as_str();
        let parsed: DerivationStatus = s.parse()?;
        assert_eq!(parsed, status);
    }
    Ok(())
}

#[test]
fn test_build_state_roundtrip() -> anyhow::Result<()> {
    for state in [
        BuildState::Pending,
        BuildState::Active,
        BuildState::Succeeded,
        BuildState::Failed,
        BuildState::Cancelled,
    ] {
        let s = state.as_str();
        let parsed: BuildState = s.parse()?;
        assert_eq!(parsed, state);
    }
    Ok(())
}

#[test]
fn test_assignment_status_as_str_exhaustive() {
    assert_eq!(AssignmentStatus::Pending.as_str(), "pending");
    assert_eq!(AssignmentStatus::Completed.as_str(), "completed");
}
