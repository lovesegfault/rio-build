//! Attempt-ledger (`drv_attempts`, migration 066) integration tests:
//! append/fill/load round-trips, the exec_id one-row-per-execution
//! schema property, the suffix cut at the last reset row, and the
//! alphabet⇄CHECK-constraint lockstep.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::SchedulerDb;
use crate::db::attempts::AttemptRow;
use crate::state::{
    AttemptEventKind, DerivationStatus, DrvHash, ExecutorId, OutcomeClass, ReportingParty,
};

/// Fresh ephemeral PG + one inserted derivation to hang rows off.
async fn setup(hash: &str) -> anyhow::Result<(TestDb, SchedulerDb, Uuid)> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let derivation_id = insert_test_derivation(&db, hash).await?;
    Ok((test_db, db, derivation_id))
}

/// Append → suffix-load round-trip preserves every column.
#[tokio::test]
async fn test_attempt_append_load_roundtrip() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("attempt-rt-hash").await?;

    let exec_id = Uuid::now_v7();
    let mut row = AttemptRow::new(drv_id, OutcomeClass::Infra, ReportingParty::Worker);
    row.exec_id = Some(exec_id);
    row.executor_id = Some(ExecutorId::from("builder-1"));
    row.exempt = true;
    row.floor_promoted = true;
    row.error_msg = Some("cgroup oom".into());
    row.final_line_count = Some(42);
    row.resubmit_cycle = 3;

    let mut tx = db.pool().begin().await?;
    let inserted = SchedulerDb::append_attempt(&mut tx, &row).await?;
    tx.commit().await?;
    assert!(inserted, "fresh append must insert");

    let loaded = db.load_attempt_suffix(&[drv_id]).await?;
    let rows = loaded.get(&drv_id).expect("derivation has rows");
    assert_eq!(rows.len(), 1);
    let got = &rows[0];
    assert_eq!(got.attempt_id, row.attempt_id);
    assert_eq!(got.derivation_id, drv_id);
    assert_eq!(got.exec_id, Some(exec_id));
    assert_eq!(
        got.executor_id.as_ref().map(|e| e.as_str()),
        Some("builder-1")
    );
    assert_eq!(got.event_kind, AttemptEventKind::Attempt);
    assert_eq!(got.outcome_class, OutcomeClass::Infra);
    assert_eq!(got.termination_reason, None);
    assert_eq!(got.reporting_party, ReportingParty::Worker);
    assert!(got.exempt);
    assert!(got.floor_promoted);
    assert!(!got.floor_at_cap);
    assert_eq!(got.error_msg.as_deref(), Some("cgroup oom"));
    assert_eq!(got.final_line_count, Some(42));
    assert_eq!(got.resubmit_cycle, 3);
    assert!(
        (got.occurred_at_epoch_secs - row.occurred_at_epoch_secs).abs() < 1.0,
        "occurred_at survives the to_timestamp/EXTRACT round-trip"
    );
    assert!(
        got.recorded_at_epoch_secs > 0.0,
        "recorded_at is PG-assigned on insert"
    );
    Ok(())
}

/// A second append bearing an already-recorded exec_id is rejected by
/// the partial unique index (the schema property), not by caller
/// discipline.
#[tokio::test]
async fn test_attempt_duplicate_exec_id_append_is_noop() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("attempt-dup-hash").await?;

    let exec_id = Uuid::now_v7();
    let mut first = AttemptRow::new(
        drv_id,
        OutcomeClass::Disconnected,
        ReportingParty::Scheduler,
    );
    first.exec_id = Some(exec_id);

    // A different attempt_id for the same execution — e.g. a stray
    // duplicate append racing the two-installment discipline.
    let mut dup = AttemptRow::new(drv_id, OutcomeClass::Timeout, ReportingParty::Controller);
    dup.exec_id = Some(exec_id);

    let mut tx = db.pool().begin().await?;
    assert!(SchedulerDb::append_attempt(&mut tx, &first).await?);
    assert!(
        !SchedulerDb::append_attempt(&mut tx, &dup).await?,
        "duplicate exec_id append must be a no-op"
    );
    tx.commit().await?;

    // NULL-exec_id rows are outside the partial index: two of them for
    // the same derivation both insert.
    let mut tx = db.pool().begin().await?;
    assert!(
        SchedulerDb::append_attempt(
            &mut tx,
            &AttemptRow::new(drv_id, OutcomeClass::Cascade, ReportingParty::Scheduler),
        )
        .await?
    );
    assert!(
        SchedulerDb::append_attempt(
            &mut tx,
            &AttemptRow::new(
                drv_id,
                OutcomeClass::FleetExhaust,
                ReportingParty::Scheduler
            ),
        )
        .await?
    );
    tx.commit().await?;

    let loaded = db.load_attempt_suffix(&[drv_id]).await?;
    let rows = loaded.get(&drv_id).expect("derivation has rows");
    assert_eq!(rows.len(), 3, "dup exec_id row must not appear");
    assert_eq!(
        rows.iter().filter(|r| r.exec_id == Some(exec_id)).count(),
        1,
        "exactly one row per execution"
    );
    // The surviving exec row keeps the FIRST append's classification.
    let exec_row = rows.iter().find(|r| r.exec_id == Some(exec_id)).unwrap();
    assert_eq!(exec_row.attempt_id, first.attempt_id);
    assert_eq!(exec_row.outcome_class, OutcomeClass::Disconnected);
    Ok(())
}

/// The appending transaction carries the status persist: an attempt
/// append plus `update_derivation_status_in_tx` commit (or roll back)
/// together.
#[tokio::test]
async fn test_attempt_append_and_status_persist_share_tx() -> anyhow::Result<()> {
    let (test_db, db, drv_id) = setup("attempt-tx-hash").await?;
    let drv_hash: DrvHash = "attempt-tx-hash".into();

    let mut row = AttemptRow::new(drv_id, OutcomeClass::Timeout, ReportingParty::Worker);
    row.exec_id = Some(Uuid::now_v7());

    let mut tx = db.pool().begin().await?;
    assert!(SchedulerDb::append_attempt(&mut tx, &row).await?);
    SchedulerDb::update_derivation_status_in_tx(
        &mut tx,
        &drv_hash,
        DerivationStatus::Cancelled,
        None,
    )
    .await?;
    tx.commit().await?;

    let status: String = sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = $1")
        .bind(drv_hash.as_str())
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(status, "cancelled");
    let loaded = db.load_attempt_suffix(&[drv_id]).await?;
    assert_eq!(loaded.get(&drv_id).map(Vec::len), Some(1));

    // The poison variant joins a caller-owned transaction the same way.
    let mut row2 = AttemptRow::new(drv_id, OutcomeClass::Permanent, ReportingParty::Worker);
    row2.exec_id = Some(Uuid::now_v7());
    let mut tx = db.pool().begin().await?;
    assert!(SchedulerDb::append_attempt(&mut tx, &row2).await?);
    SchedulerDb::persist_poisoned_in_tx(&mut tx, &drv_hash).await?;
    tx.commit().await?;

    let (status, has_poisoned_at): (String, bool) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash = $1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "poisoned");
    assert!(has_poisoned_at);
    let loaded = db.load_attempt_suffix(&[drv_id]).await?;
    assert_eq!(loaded.get(&drv_id).map(Vec::len), Some(2));
    Ok(())
}

/// The suffix loader returns rows at-or-after the most recent reset row
/// only — pre-reset history is cut.
#[tokio::test]
async fn test_attempt_suffix_cuts_at_last_reset() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("attempt-suffix-hash").await?;

    let mut tx = db.pool().begin().await?;
    // Pre-reset history: two attempts (one with an exec).
    let mut a1 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    a1.exec_id = Some(Uuid::now_v7());
    SchedulerDb::append_attempt(&mut tx, &a1).await?;
    SchedulerDb::append_attempt(
        &mut tx,
        &AttemptRow::new(drv_id, OutcomeClass::Backstop, ReportingParty::Scheduler),
    )
    .await?;
    // The reset event (resubmit reset, cycle 1).
    let reset = AttemptRow::new_reset(
        drv_id,
        OutcomeClass::ResubmitReset,
        ReportingParty::Scheduler,
        1,
    );
    SchedulerDb::append_attempt(&mut tx, &reset).await?;
    // Post-reset history: one attempt.
    let mut a3 = AttemptRow::new(drv_id, OutcomeClass::Infra, ReportingParty::Worker);
    a3.exec_id = Some(Uuid::now_v7());
    SchedulerDb::append_attempt(&mut tx, &a3).await?;
    tx.commit().await?;

    let loaded = db.load_attempt_suffix(&[drv_id]).await?;
    let rows = loaded.get(&drv_id).expect("derivation has rows");
    let classes: Vec<OutcomeClass> = rows.iter().map(|r| r.outcome_class).collect();
    assert_eq!(
        classes,
        vec![OutcomeClass::ResubmitReset, OutcomeClass::Infra],
        "suffix must start AT the most recent reset row and cut everything before it"
    );
    assert_eq!(rows[0].event_kind, AttemptEventKind::Reset);
    assert_eq!(rows[0].resubmit_cycle, 1);
    Ok(())
}

/// Unknown derivations are simply absent from the batch-load result.
#[tokio::test]
async fn test_attempt_suffix_unknown_derivations_absent() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("attempt-unknown-hash").await?;

    // No rows appended for drv_id; a second id that does not exist at all.
    let ghost = Uuid::now_v7();
    let loaded = db.load_attempt_suffix(&[drv_id, ghost]).await?;
    assert!(
        loaded.is_empty(),
        "no rows → no entries (callers treat missing keys as empty history)"
    );
    Ok(())
}

/// The Rust-side alphabets and the migration-066 CHECK constraints stay
/// in lockstep: every enum variant is accepted by PG, and every literal
/// PG accepts has an enum variant.
#[tokio::test]
async fn test_attempt_outcome_class_alphabet_matches_check_constraint() -> anyhow::Result<()> {
    let (test_db, _db, _drv_id) = setup("attempt-alphabet-hash").await?;

    // Pull every CHECK constraint body on drv_attempts and extract the
    // single-quoted literals per constrained column.
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) \
         FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
         WHERE t.relname = 'drv_attempts' AND c.contype = 'c'",
    )
    .fetch_all(&test_db.pool)
    .await?;

    let literals = |needle: &str| -> std::collections::BTreeSet<String> {
        let def = defs
            .iter()
            .find(|d| d.contains(needle))
            .unwrap_or_else(|| panic!("no CHECK constraint mentioning {needle}"));
        def.split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    };

    let check_outcomes = literals("outcome_class");
    let rust_outcomes: std::collections::BTreeSet<String> = OutcomeClass::ALL
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();
    assert_eq!(
        rust_outcomes, check_outcomes,
        "OutcomeClass and the 066 CHECK constraint must carry the same alphabet \
         (extending it is a new migration plus a variant)"
    );

    let check_kinds = literals("event_kind");
    let rust_kinds: std::collections::BTreeSet<String> = AttemptEventKind::ALL
        .iter()
        .map(|k| k.as_str().to_string())
        .collect();
    assert_eq!(rust_kinds, check_kinds, "event_kind alphabet drifted");
    Ok(())
}

/// Pure round-trip of the three ledger vocabularies through their PG
/// TEXT representations (no DB).
#[test]
fn test_attempt_vocabulary_str_roundtrip() {
    for class in OutcomeClass::ALL {
        let parsed: OutcomeClass = class.as_str().parse().expect("round-trip");
        assert_eq!(parsed, *class);
    }
    for kind in AttemptEventKind::ALL {
        let parsed: AttemptEventKind = kind.as_str().parse().expect("round-trip");
        assert_eq!(parsed, *kind);
    }
    for party in ReportingParty::ALL {
        let parsed: ReportingParty = party.as_str().parse().expect("round-trip");
        assert_eq!(parsed, *party);
    }
}
