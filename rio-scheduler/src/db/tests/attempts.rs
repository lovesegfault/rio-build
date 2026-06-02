//! Attempt-ledger (`drv_attempts`, migration 068) integration tests:
//! append/fill/load round-trips, the exec_id one-row-per-execution
//! schema property, the suffix cut at the last reset row, and the
//! alphabet⇄CHECK-constraint lockstep.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::SchedulerDb;
use crate::db::attempts::AttemptRow;
use crate::state::{
    AttemptEventKind, AttemptKind, DerivationStatus, DrvHash, ExecutorId, OutcomeClass,
    ReportingParty,
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
    for kind in AttemptKind::ALL {
        let parsed: AttemptKind = kind.as_str().parse().expect("round-trip");
        assert_eq!(parsed, *kind);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The attempt-kind plumbing (substitution-replacement Phase A): the
// suffix load joins drv_executions.attempt_kind onto each ledger row,
// and the fold input carries it into the kernel's kind partition.
// ─────────────────────────────────────────────────────────────────────────

/// Insert a `drv_executions` row carrying an explicit `attempt_kind` —
/// the shape the materialization mint will write (build mints rely on
/// the column DEFAULT and never name it).
async fn insert_execution_with_kind(
    pool: &sqlx::PgPool,
    exec_id: Uuid,
    drv_hash32: &str,
    executor_id: &str,
    attempt_kind: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO drv_executions \
             (exec_id, drv_hash, executor_id, started_at, attempt_kind) \
         VALUES ($1, $2, $3, now(), $4)",
    )
    .bind(exec_id)
    .bind(drv_hash32)
    .bind(executor_id)
    .bind(attempt_kind)
    .execute(pool)
    .await?;
    Ok(())
}

/// The suffix load discriminates kind: a `drv_attempts` row whose exec
/// joins a `drv_executions` row with `attempt_kind='materialization'`
/// loads as [`AttemptKind::Materialization`]; rows with no exec_id or a
/// build execution load as [`AttemptKind::Build`]. And the loaded
/// suffix passed to `decide()` produces a verdict identical to the
/// build-rows-only suffix — the scheduler half of the invisibility
/// property (the kernel half is the
/// `check_materialization_rows_invisible_to_build_decision` CBMC
/// harness).
// r[verify sched.materialize.routing+3]
#[tokio::test]
async fn test_suffix_load_carries_attempt_kind_and_partition() -> anyhow::Result<()> {
    let (test_db, db, drv_id) = setup("attempt-kind-hash").await?;

    // Exec A: a build execution (attempt_kind='build', the default the
    // pull mint writes implicitly — written explicitly here to pin the
    // literal).
    let exec_a = Uuid::now_v7();
    insert_execution_with_kind(
        &test_db.pool,
        exec_a,
        &format!("{:0>32}", "kindbuild"),
        "builder-1",
        "build",
    )
    .await?;

    // Exec B: a materialization execution.
    let exec_b = Uuid::now_v7();
    insert_execution_with_kind(
        &test_db.pool,
        exec_b,
        &format!("{:0>32}", "kindmat"),
        "intent@store-0",
        "materialization",
    )
    .await?;

    // Three ledger rows: a transient build attempt (exec A), a
    // materialization-infra row (exec B), and a cascade row (no exec —
    // loads as Build by the COALESCE default).
    let mut build_attempt =
        AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    build_attempt.exec_id = Some(exec_a);
    build_attempt.executor_id = Some(ExecutorId::from("builder-1"));
    build_attempt.source_node = Some("node-1".into());

    let mut mat_attempt = AttemptRow::new(
        drv_id,
        OutcomeClass::MaterializationInfra,
        ReportingParty::Worker,
    );
    mat_attempt.exec_id = Some(exec_b);
    mat_attempt.executor_id = Some(ExecutorId::from("intent@store-0"));

    let cascade = AttemptRow::new(drv_id, OutcomeClass::Cascade, ReportingParty::Scheduler);

    let mut tx = db.pool().begin().await?;
    assert!(SchedulerDb::append_attempt(&mut tx, &build_attempt).await?);
    assert!(SchedulerDb::append_attempt(&mut tx, &mat_attempt).await?);
    assert!(SchedulerDb::append_attempt(&mut tx, &cascade).await?);
    tx.commit().await?;

    // The loaded suffix carries the joined kinds in ledger order.
    let loaded = db.load_attempt_suffix(&[drv_id]).await?;
    let rows = loaded.get(&drv_id).expect("derivation has rows");
    assert_eq!(rows.len(), 3);
    let kinds: Vec<AttemptKind> = rows.iter().map(|r| r.attempt_kind).collect();
    assert_eq!(
        kinds,
        vec![
            AttemptKind::Build,
            AttemptKind::Materialization,
            AttemptKind::Build
        ],
        "exec-A row joins 'build', exec-B row joins 'materialization', \
         the no-exec cascade row defaults to Build"
    );

    // The in-tx single-derivation load carries the same kinds.
    let mut tx = db.pool().begin().await?;
    let in_tx_rows = SchedulerDb::load_attempt_suffix_one_in_tx(&mut tx, drv_id).await?;
    tx.commit().await?;
    let in_tx_kinds: Vec<AttemptKind> = in_tx_rows.iter().map(|r| r.attempt_kind).collect();
    assert_eq!(in_tx_kinds, kinds, "both suffix loads carry the same join");

    // The fold-input partition: decide() over the loaded records equals
    // decide() over the build-kind records only — the materialization
    // row is invisible to the build verdict.
    let records: Vec<crate::state::AttemptRecord> = rows.iter().map(|r| r.to_record()).collect();
    let build_only: Vec<crate::state::AttemptRecord> = records
        .iter()
        .filter(|r| r.attempt_kind == AttemptKind::Build)
        .cloned()
        .collect();
    assert_eq!(
        build_only.len(),
        2,
        "the materialization record is filtered"
    );

    let budget = crate::retry_policy::Budget::default();
    let now = rows
        .last()
        .map(|r| r.recorded_at_epoch_secs as crate::retry_policy::AbsTime)
        .unwrap_or_default();
    let full_decision = crate::retry_policy::decide(&records, &budget, now);
    let build_only_decision = crate::retry_policy::decide(&build_only, &budget, now);
    assert_eq!(
        full_decision, build_only_decision,
        "the loaded materialization row must be invisible to the build decision"
    );
    Ok(())
}

/// The Rust-side `AttemptKind` alphabet and the migration-078
/// `drv_executions.attempt_kind` CHECK constraint stay in lockstep —
/// the same discipline as the outcome-class alphabet test above.
#[tokio::test]
async fn test_attempt_kind_alphabet_matches_check_constraint() -> anyhow::Result<()> {
    let (test_db, _db, _drv_id) = setup("attempt-kind-alphabet-hash").await?;

    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) \
         FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
         WHERE t.relname = 'drv_executions' AND c.contype = 'c'",
    )
    .fetch_all(&test_db.pool)
    .await?;

    let def = defs
        .iter()
        .find(|d| d.contains("attempt_kind"))
        .expect("drv_executions has an attempt_kind CHECK constraint");
    let check_kinds: std::collections::BTreeSet<String> = def
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();

    let rust_kinds: std::collections::BTreeSet<String> = AttemptKind::ALL
        .iter()
        .map(|k| k.as_str().to_string())
        .collect();
    assert_eq!(
        rust_kinds, check_kinds,
        "AttemptKind and the 078 attempt_kind CHECK must carry the same alphabet \
         (extending it is a new migration plus a variant)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The attempt-ledger GC sweep (sched.db.attempts-gc)
// ---------------------------------------------------------------------------

/// 24 h in seconds — the floor value the tests pass as the horizon
/// (the housekeeping tick passes `sweep_horizon_secs(budget, floor)`;
/// with the default budget that IS the floor).
const TEST_HORIZON_SECS: f64 = 86_400.0;

/// Append `rows` in one committed transaction.
async fn append_committed(db: &SchedulerDb, rows: &[AttemptRow]) -> anyhow::Result<()> {
    let mut tx = db.pool.begin().await?;
    SchedulerDb::append_attempts_batch(&mut tx, rows).await?;
    tx.commit().await?;
    Ok(())
}

/// Backdate `recorded_at` of the given rows by 3 days (past the 24 h
/// horizon). `recorded_at` is `DEFAULT now()` at insert (068), so tests
/// age rows explicitly.
async fn backdate(db: &SchedulerDb, ids: &[Uuid]) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE drv_attempts SET recorded_at = recorded_at - interval '3 days' \
         WHERE attempt_id = ANY($1)",
    )
    .bind(ids)
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// All attempt_ids for one derivation, in ledger order.
async fn ledger_ids(db: &SchedulerDb, drv_id: Uuid) -> anyhow::Result<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT attempt_id FROM drv_attempts WHERE derivation_id = $1 \
         ORDER BY recorded_at, attempt_id",
    )
    .bind(drv_id)
    .fetch_all(&db.pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

// r[verify sched.db.attempts-gc]
/// Live arm shape: only attempt-kind rows strictly before the last
/// reset and past the horizon are deleted. R0 (an OLD reset) proves
/// reset rows of a live derivation are never deleted; A3 (old but
/// post-reset) proves age alone never deletes; A4 (fresh) proves the
/// horizon binds.
#[tokio::test]
async fn test_attempts_gc_deletes_only_pre_reset_rows_past_horizon() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("gc-shape-hash").await?;

    let r0 = AttemptRow::new_reset(
        drv_id,
        OutcomeClass::CacheHitClear,
        ReportingParty::Admin,
        0,
    );
    let a1 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    let a2 = AttemptRow::new(drv_id, OutcomeClass::Infra, ReportingParty::Scheduler);
    let r1 = AttemptRow::new_reset(
        drv_id,
        OutcomeClass::ResubmitReset,
        ReportingParty::Scheduler,
        1,
    );
    let a3 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    let a4 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    let keep = [r0.attempt_id, r1.attempt_id, a3.attempt_id, a4.attempt_id];
    let victims = [a1.attempt_id, a2.attempt_id];
    append_committed(&db, &[r0, a1, a2, r1, a3, a4]).await?;
    // Everything except A4 is ancient; A3 is old-but-post-reset.
    let mut old: Vec<Uuid> = victims.to_vec();
    old.extend([keep[0], keep[1], keep[2]]);
    backdate(&db, &old).await?;

    let deleted = db.gc_attempt_ledger(TEST_HORIZON_SECS, 1000).await?;
    assert_eq!(deleted, 2, "exactly the two pre-reset old attempt rows");

    let remaining = ledger_ids(&db, drv_id).await?;
    assert_eq!(remaining.len(), 4, "R0, R1, A3, A4 survive: {remaining:?}");
    for id in keep {
        assert!(remaining.contains(&id), "survivor {id} missing");
    }
    for id in victims {
        assert!(!remaining.contains(&id), "victim {id} survived");
    }
    Ok(())
}

// r[verify sched.db.attempts-gc]
/// THE decide()-invariance pin (red-first; see the commit body for the
/// recorded red run against the naive age-only DELETE, which deletes
/// the reset row, drops the cycle seed, and changes the Decision):
/// a poisoned derivation's Decision over `load_attempt_suffix` is
/// bit-identical before/after the guarded sweep, and the verdict stays
/// a Poison.
#[tokio::test]
async fn test_attempts_gc_decide_invariant_for_poisoned_history() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("gc-decide-hash").await?;

    // Earlier cycle: two old attempts, then the cycle-2 resubmit reset
    // (also old). Current cycle: three fresh distinct-source failures —
    // a poisoned suffix.
    let a1 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    let a2 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    let r1 = AttemptRow::new_reset(
        drv_id,
        OutcomeClass::ResubmitReset,
        ReportingParty::Scheduler,
        2,
    );
    let mut f1 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    f1.source_node = Some("node-w1".into());
    let mut f2 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    f2.source_node = Some("node-w2".into());
    let mut f3 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    f3.source_node = Some("node-w3".into());
    let old = [a1.attempt_id, a2.attempt_id, r1.attempt_id];
    append_committed(&db, &[a1, a2, r1, f1, f2, f3]).await?;
    backdate(&db, &old).await?;

    let budget = crate::retry_policy::Budget::default();
    let now = crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime;
    fn suffix_records(rows: &[AttemptRow]) -> Vec<crate::state::AttemptRecord> {
        rows.iter().map(AttemptRow::to_record).collect()
    }

    let before_rows = db
        .load_attempt_suffix(&[drv_id])
        .await?
        .remove(&drv_id)
        .unwrap_or_default();
    let before = crate::retry_policy::decide(&suffix_records(&before_rows), &budget, now);
    assert!(
        matches!(before.verdict, crate::retry_policy::Verdict::Poison(_)),
        "setup must produce a poisoned suffix, got {:?}",
        before.verdict
    );
    assert_eq!(
        before.counters.resubmit_cycles, 2,
        "the reset row's cycle index seeds the fold"
    );

    let deleted = db.gc_attempt_ledger(TEST_HORIZON_SECS, 1000).await?;
    assert_eq!(deleted, 2, "the two pre-reset old attempts");

    let after_rows = db
        .load_attempt_suffix(&[drv_id])
        .await?
        .remove(&drv_id)
        .unwrap_or_default();
    let after = crate::retry_policy::decide(&suffix_records(&after_rows), &budget, now);
    assert_eq!(
        before, after,
        "Decision must be bit-identical across the sweep"
    );
    assert!(
        matches!(after.verdict, crate::retry_policy::Verdict::Poison(_)),
        "the poisoned verdict survives the sweep"
    );
    Ok(())
}

// r[verify sched.db.attempts-gc]
/// E4: a pre-reset old row whose exec_id still has an ACTIVE
/// (`pending`) assignments row is kept — the row doubles as the
/// report-idempotency record; close the assignment and the next pass
/// sweeps it.
#[tokio::test]
async fn test_attempts_gc_skips_rows_with_active_assignment() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("gc-active-assignment-hash").await?;

    let exec_id = Uuid::now_v7();
    let mut a1 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    a1.exec_id = Some(exec_id);
    let r1 = AttemptRow::new_reset(
        drv_id,
        OutcomeClass::ResubmitReset,
        ReportingParty::Scheduler,
        1,
    );
    let a1_id = a1.attempt_id;
    let old = [a1.attempt_id, r1.attempt_id];
    append_committed(&db, &[a1, r1]).await?;
    backdate(&db, &old).await?;

    db.insert_assignment(drv_id, &ExecutorId::from("builder-1"), 1, exec_id)
        .await?;

    let deleted = db.gc_attempt_ledger(TEST_HORIZON_SECS, 1000).await?;
    assert_eq!(deleted, 0, "active assignment exempts the row (E4)");
    assert!(ledger_ids(&db, drv_id).await?.contains(&a1_id));

    // Close the assignment — the next pass sweeps the row.
    sqlx::query(
        "UPDATE assignments SET status = 'completed', completed_at = now() \
         WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(&db.pool)
    .await?;
    let deleted = db.gc_attempt_ledger(TEST_HORIZON_SECS, 1000).await?;
    assert_eq!(deleted, 1, "closed assignment unblocks the sweep");
    assert!(!ledger_ids(&db, drv_id).await?.contains(&a1_id));
    Ok(())
}

// r[verify sched.db.attempts-gc]
/// Orphan arm: rows (including reset rows) of a derivation_id with no
/// derivations row are reaped past the horizon; equally old rows of a
/// live no-reset derivation are all kept (no cut → all live suffix).
/// Then the unreachability premise is PINNED, not comment-cited:
/// re-inserting the same drv_hash through the production insert path
/// mints a DIFFERENT derivation_id with an empty suffix — a
/// partially-swept orphan history can never resurface inside a fresh
/// derivation's suffix.
#[tokio::test]
async fn test_attempts_gc_reaps_orphaned_histories() -> anyhow::Result<()> {
    let (_test_db, db, orphan_id) = setup("gc-orphan-hash").await?;

    // Orphan-to-be: attempt + reset rows, all old.
    let o1 = AttemptRow::new(orphan_id, OutcomeClass::Transient, ReportingParty::Worker);
    let o2 = AttemptRow::new_reset(
        orphan_id,
        OutcomeClass::ResubmitReset,
        ReportingParty::Scheduler,
        1,
    );
    let old = [o1.attempt_id, o2.attempt_id];
    append_committed(&db, &[o1, o2]).await?;
    backdate(&db, &old).await?;

    // A live derivation with NO reset row and equally old rows.
    let live_id = insert_test_derivation(&db, "gc-orphan-live-hash").await?;
    let l1 = AttemptRow::new(live_id, OutcomeClass::Transient, ReportingParty::Worker);
    let l2 = AttemptRow::new(live_id, OutcomeClass::Infra, ReportingParty::Scheduler);
    let live_old = [l1.attempt_id, l2.attempt_id];
    append_committed(&db, &[l1, l2]).await?;
    backdate(&db, &live_old).await?;

    // Orphan the first derivation (what derivations-GC does; 068 has no
    // FK so the history outlives the row).
    sqlx::query("DELETE FROM derivations WHERE derivation_id = $1")
        .bind(orphan_id)
        .execute(&db.pool)
        .await?;

    let deleted = db.gc_attempt_ledger(TEST_HORIZON_SECS, 1000).await?;
    assert_eq!(deleted, 2, "the whole orphaned history, reset row included");
    assert!(ledger_ids(&db, orphan_id).await?.is_empty());
    assert_eq!(
        ledger_ids(&db, live_id).await?.len(),
        2,
        "a live no-reset derivation keeps its whole (suffix) history however old"
    );

    // The unreachability premise: same drv_hash, fresh UUID, empty
    // suffix.
    let reborn_id = insert_test_derivation(&db, "gc-orphan-hash").await?;
    assert_ne!(
        reborn_id, orphan_id,
        "a re-submitted drv_hash mints a FRESH derivation UUID"
    );
    assert!(
        !db.load_attempt_suffix(&[reborn_id])
            .await?
            .contains_key(&reborn_id),
        "the reborn derivation's suffix is empty"
    );
    Ok(())
}

// r[verify sched.db.attempts-gc]
/// The per-arm batch limit bounds one pass; the next pass drains.
#[tokio::test]
async fn test_attempts_gc_respects_batch_limit() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("gc-batch-hash").await?;

    let v1 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    let v2 = AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker);
    let v3 = AttemptRow::new(drv_id, OutcomeClass::Infra, ReportingParty::Scheduler);
    let r1 = AttemptRow::new_reset(
        drv_id,
        OutcomeClass::ResubmitReset,
        ReportingParty::Scheduler,
        1,
    );
    let old = [v1.attempt_id, v2.attempt_id, v3.attempt_id, r1.attempt_id];
    append_committed(&db, &[v1, v2, v3, r1]).await?;
    backdate(&db, &old).await?;

    assert_eq!(db.gc_attempt_ledger(TEST_HORIZON_SECS, 2).await?, 2);
    assert_eq!(db.gc_attempt_ledger(TEST_HORIZON_SECS, 2).await?, 1);
    assert_eq!(db.gc_attempt_ledger(TEST_HORIZON_SECS, 2).await?, 0);
    assert_eq!(
        ledger_ids(&db, drv_id).await?.len(),
        1,
        "only the reset row remains"
    );
    Ok(())
}

// r[verify sched.db.attempts-gc]
/// Cross-layer pin: the SQL suffix cut (`load_attempt_suffix`) returns
/// exactly `rows[ledger_suffix_start(rows)..]` of the full ordered
/// history — the kernel mirror and the SQL agree on where the suffix
/// begins, which is the premise the sweep-invariance proofs rest on.
#[tokio::test]
async fn test_suffix_cut_matches_kernel_ledger_suffix_start() -> anyhow::Result<()> {
    let (_test_db, db, drv_id) = setup("gc-cut-pin-hash").await?;

    let rows = [
        AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker),
        AttemptRow::new_reset(
            drv_id,
            OutcomeClass::CacheHitClear,
            ReportingParty::Admin,
            0,
        ),
        AttemptRow::new(drv_id, OutcomeClass::Infra, ReportingParty::Scheduler),
        AttemptRow::new_reset(
            drv_id,
            OutcomeClass::ResubmitReset,
            ReportingParty::Scheduler,
            1,
        ),
        AttemptRow::new(drv_id, OutcomeClass::Transient, ReportingParty::Worker),
    ];
    append_committed(&db, &rows).await?;

    // Full ordered history straight from PG.
    let full: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT attempt_id, event_kind FROM drv_attempts \
         WHERE derivation_id = $1 ORDER BY recorded_at, attempt_id",
    )
    .bind(drv_id)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(full.len(), 5);

    // Project to kernel rows (only event_kind matters for the cut).
    let kernel_rows: Vec<rio_retry_kernel::LedgerRow<String>> = full
        .iter()
        .map(|(_, ek)| rio_retry_kernel::LedgerRow {
            event_kind: if ek == "reset" {
                rio_retry_kernel::AttemptEventKind::Reset
            } else {
                rio_retry_kernel::AttemptEventKind::Attempt
            },
            outcome_class: rio_retry_kernel::OutcomeClass::Transient,
            executor: None,
            reporting_party: rio_retry_kernel::ReportingParty::Scheduler,
            floor_promoted: false,
            floor_at_cap: false,
            resubmit_cycle: 0,
            at: 0,
            kind: rio_retry_kernel::AttemptKind::Build,
        })
        .collect();
    let cut = rio_retry_kernel::ledger_suffix_start(&kernel_rows);
    let expected: Vec<Uuid> = full[cut..].iter().map(|(id, _)| *id).collect();

    let suffix = db
        .load_attempt_suffix(&[drv_id])
        .await?
        .remove(&drv_id)
        .unwrap_or_default();
    let got: Vec<Uuid> = suffix.iter().map(|r| r.attempt_id).collect();
    assert_eq!(got, expected, "SQL cut == kernel ledger_suffix_start");
    Ok(())
}
