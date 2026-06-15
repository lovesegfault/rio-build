//! Poison/retry/status transition tests.

use crate::db::ServingGeneration;
use rio_test_support::TestDb;
use uuid::Uuid;

use super::{TERMINAL_STATUSES, insert_test_derivation};
use crate::db::{FencedOutcome, SchedulerDb};
use crate::state::{DrvHash, ExecutorId};

// r[verify sched.poison.ttl-persist]
// r[verify obs.log.failure-reason-persisted]
/// Roundtrip: persist_poisoned → load_poisoned_derivations → clear_poison.
/// Catches the `.as_bytes()` vs `.as_str()` binding regression — PG rejects
/// BYTEA against a TEXT column, but call sites swallow the error as best-effort.
///
/// Also verifies atomicity: a single `persist_poisoned` call sets BOTH
/// status AND poisoned_at (no crash window between two UPDATEs).
#[tokio::test]
async fn test_poison_persistence_roundtrip() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let drv_hash: DrvHash = "poison-rt-hash".into();
    let _ = insert_test_derivation(&db, drv_hash.as_str()).await?;

    // Single atomic call: sets status='poisoned' AND poisoned_at=now()
    // AND assigned_builder_id=NULL AND the failure reason (M_117).
    // No separate status update needed. (No claims rows in this test ->
    // any serving generation passes the claims-floor fence; same for
    // every write below.)
    let exec = Uuid::now_v7();
    let fenced_outcome = db
        .persist_poisoned(
            &drv_hash,
            Some("builder exited 1: cc not found"),
            Some(exec),
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert!(fenced_outcome.settled());

    // Verify all columns updated in one statement.
    let (status, has_ts, worker, failure_msg, failure_exec): (
        String,
        bool,
        Option<String>,
        Option<String>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL, assigned_builder_id, \
                failure_msg, failure_exec_id \
         FROM derivations WHERE drv_hash=$1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "poisoned");
    assert!(has_ts, "poisoned_at must be set in the same statement");
    assert!(worker.is_none(), "assigned_builder_id must be NULLed");
    assert_eq!(
        failure_msg.as_deref(),
        Some("builder exited 1: cc not found"),
        "failure_msg must persist the builder error"
    );
    assert_eq!(failure_exec, Some(exec), "failure_exec_id must persist");

    // The fail-fast read path sees the same attribution.
    let reason = db
        .load_failure_reason(&drv_hash)
        .await?
        .expect("derivation row exists");
    assert_eq!(
        reason.failure_msg.as_deref(),
        Some("builder exited 1: cc not found")
    );
    assert_eq!(reason.failure_exec_id, Some(exec));
    assert!(
        reason.poisoned_epoch.is_some_and(|e| e > 0.0),
        "poisoned_epoch must reflect poisoned_at"
    );

    let rows = db.load_poisoned_derivations().await?;
    assert_eq!(rows.len(), 1, "persist_poisoned should make row loadable");
    assert_eq!(rows[0].drv_hash, drv_hash.as_str());
    assert_ne!(rows[0].derivation_id, Uuid::nil());
    assert!(
        rows[0].elapsed_secs >= 0.0 && rows[0].elapsed_secs < 5.0,
        "elapsed should be ~0s, got {}",
        rows[0].elapsed_secs
    );

    // clear_poison → no longer loadable; status reset to 'created'.
    let fenced_outcome = db
        .clear_poison(&drv_hash, ServingGeneration::stamp_from_claim(1))
        .await?;
    assert!(fenced_outcome.settled());
    let rows = db.load_poisoned_derivations().await?;
    assert!(
        rows.is_empty(),
        "clear_poison should remove from poisoned set"
    );

    let (status, poisoned_at, failure_msg, failure_exec): (
        String,
        Option<f64>,
        Option<String>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM poisoned_at)::float8, \
                failure_msg, failure_exec_id \
         FROM derivations WHERE drv_hash=$1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "created");
    assert!(poisoned_at.is_none());
    assert!(
        failure_msg.is_none() && failure_exec.is_none(),
        "clear_poison must NULL the persisted failure reason"
    );
    Ok(())
}

// r[verify sched.db.clear-poison-batch+3]
/// `clear_poison_batch` clears N rows in one round-trip. Asserts the
/// rows_affected count (N hashes in → N rows touched, single statement)
/// and the column set: the poison lifecycle state is cleared
/// (status='created', poisoned_at NULL). The retry counters are not
/// derivations columns (migration 075) — the budget reset is carried by
/// the `resubmit_reset` ledger row appended in the same transaction by
/// the production caller.
#[tokio::test]
async fn test_clear_poison_batch() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // 100 poisoned rows.
    let hashes: Vec<DrvHash> = (0..100)
        .map(|i| format!("batch-poison-{i}").into())
        .collect();
    for h in &hashes {
        insert_test_derivation(&db, h.as_str()).await?;
        let fenced_outcome = db
            .persist_poisoned(
                h,
                Some("batch poison reason"),
                None,
                ServingGeneration::stamp_from_claim(1),
            )
            .await?;
        assert!(fenced_outcome.settled());
    }
    assert_eq!(db.load_poisoned_derivations().await?.len(), 100);

    // One call → 100 rows affected. The single-round-trip property is
    // structural (one `execute()`), so the assertion is on
    // `rows_affected`, not a query-count mock.
    let affected = db
        .clear_poison_batch(&hashes, ServingGeneration::stamp_from_claim(1))
        .await?;
    assert_eq!(
        affected,
        FencedOutcome::Applied(100),
        "one ANY($1) UPDATE should touch all 100"
    );

    // Poison lifecycle state cleared: status='created', poisoned_at
    // NULL. Nothing else on the derivations row carries retry state any
    // more (migration 075 dropped the mirror columns).
    assert!(db.load_poisoned_derivations().await?.is_empty());
    let (n_created, n_clean): (i64, i64) = sqlx::query_as(
        "SELECT
             COUNT(*) FILTER (WHERE status = 'created'),
             COUNT(*) FILTER (WHERE poisoned_at IS NULL
                              AND failure_msg IS NULL
                              AND failure_exec_id IS NULL)
         FROM derivations WHERE drv_hash = ANY($1)",
    )
    .bind(hashes.iter().map(DrvHash::as_str).collect::<Vec<_>>())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(n_created, 100);
    assert_eq!(n_clean, 100);

    // Empty input: no-op, no PG round-trip.
    assert_eq!(
        db.clear_poison_batch(&[], ServingGeneration::stamp_from_claim(1))
            .await?,
        FencedOutcome::Applied(0)
    );
    Ok(())
}

// r[verify sched.db.derivations-gc+4]
/// I-169.2: orphan-terminal rows are deleted; rows with a live
/// `build_derivations` link, an `assignments` row, or non-terminal
/// status are kept. LIMIT respected.
#[tokio::test]
async fn test_gc_orphan_terminal_derivations() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // (a) Orphan-terminal: one row per terminal status, no links → deleted.
    let mut orphan_ids = Vec::new();
    for status in TERMINAL_STATUSES {
        let h = format!("gc-orphan-{status}");
        let id = insert_test_derivation(&db, &h).await?;
        sqlx::query("UPDATE derivations SET status = $1 WHERE derivation_id = $2")
            .bind(*status)
            .bind(id)
            .execute(&test_db.pool)
            .await?;
        orphan_ids.push(id);
    }

    // (b) Terminal but linked via build_derivations → KEPT.
    let linked_id = insert_test_derivation(&db, "gc-linked").await?;
    sqlx::query("UPDATE derivations SET status = 'dependency_failed' WHERE derivation_id = $1")
        .bind(linked_id)
        .execute(&test_db.pool)
        .await?;
    let build_id = Uuid::new_v4();
    db.insert_build(
        build_id,
        None,
        crate::state::PriorityClass::Scheduled,
        false,
        &crate::state::BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(build_id, linked_id).await?;

    // (c) Terminal with an ACTIVE (pending) assignment → KEPT.
    // Recovery still needs that row to know what was dispatched; the
    // active-status filter is the only assignment gate now.
    let assigned_id = insert_test_derivation(&db, "gc-assigned").await?;
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE derivation_id = $1")
        .bind(assigned_id)
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status)
         VALUES ($1, 'w-test', 1, 'pending')",
    )
    .bind(assigned_id)
    .execute(&test_db.pool)
    .await?;

    // (c') Terminal with a TERMINAL ('completed') assignment → DELETED.
    // I-209/I-210: terminal assignment rows no longer block; 034's
    // CASCADE FK takes them with the derivation.
    let cascade_id = insert_test_derivation(&db, "gc-cascade").await?;
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE derivation_id = $1")
        .bind(cascade_id)
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status)
         VALUES ($1, 'w-test', 1, 'completed')",
    )
    .bind(cascade_id)
    .execute(&test_db.pool)
    .await?;

    // (d) Non-terminal, no links → KEPT.
    let live_id = insert_test_derivation(&db, "gc-live").await?;

    // (e) Edges: orphan→orphan + orphan→linked. Both reference an
    // orphan id, so both must be deleted in the same CTE. linked→live
    // references no victim → kept. bug_073: without the del_edges CTE,
    // these accumulate unbounded (028 dropped the FKs, no cascade).
    sqlx::query(
        "INSERT INTO derivation_edges (parent_id, child_id)
         VALUES ($1, $2), ($1, $3), ($3, $4)",
    )
    .bind(orphan_ids[0])
    .bind(orphan_ids[1])
    .bind(linked_id)
    .bind(live_id)
    .execute(&test_db.pool)
    .await?;

    // Sweep with generous limit.
    let deleted = db.gc_orphan_terminal_derivations(1000).await?;
    assert_eq!(
        deleted,
        orphan_ids.len() as u64 + 1,
        "orphan-terminal set + cascade case deleted"
    );

    let remaining: Vec<Uuid> = sqlx::query_scalar("SELECT derivation_id FROM derivations")
        .fetch_all(&test_db.pool)
        .await?;
    for id in &orphan_ids {
        assert!(!remaining.contains(id), "orphan {id} should be GC'd");
    }
    assert!(remaining.contains(&linked_id), "build-linked row kept");
    assert!(
        remaining.contains(&assigned_id),
        "row with ACTIVE assignment kept"
    );
    assert!(
        !remaining.contains(&cascade_id),
        "I-209: terminal assignment row no longer blocks GC (CASCADE)"
    );
    assert!(remaining.contains(&live_id), "non-terminal row kept");

    // CASCADE FK removed the assignment row too.
    let cascade_assigns: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM assignments WHERE derivation_id = $1")
            .bind(cascade_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(cascade_assigns, 0, "034: assignment row CASCADEd");

    // (e) Edges referencing GC'd ids deleted; edge between kept rows survives.
    let remaining_edges: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT parent_id, child_id FROM derivation_edges")
            .fetch_all(&test_db.pool)
            .await?;
    assert_eq!(
        remaining_edges,
        vec![(linked_id, live_id)],
        "edges referencing victims deleted; non-victim edge kept"
    );

    // Second sweep: nothing left to delete.
    assert_eq!(db.gc_orphan_terminal_derivations(1000).await?, 0);
    Ok(())
}

// r[verify sched.db.derivations-gc+4]
/// LIMIT batches the sweep: 5 orphans, limit=2 → 2, 2, 1, 0.
#[tokio::test]
async fn test_gc_orphan_terminal_derivations_limit() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    for i in 0..5 {
        let id = insert_test_derivation(&db, &format!("gc-lim-{i}")).await?;
        sqlx::query("UPDATE derivations SET status = 'dependency_failed' WHERE derivation_id = $1")
            .bind(id)
            .execute(&test_db.pool)
            .await?;
    }

    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 2);
    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 2);
    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 1);
    assert_eq!(db.gc_orphan_terminal_derivations(2).await?, 0);
    Ok(())
}

// r[verify sched.db.assignment-stale-sweep]
/// bug_138: torn terminal write (derivation=poisoned, assignment=
/// pending) is permanently un-GC-able. `sweep_stale_assignments`
/// repairs it; then GC can delete. The torn state is now structurally
/// impossible via the tx-wrap chokepoint, so this test seeds it via
/// raw SQL (simulating a row leaked by a pre-tx-wrap binary).
#[tokio::test]
async fn test_sweep_stale_assignments_repairs_torn_terminal() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Seed: derivation terminal, assignment pending — the torn state.
    let torn_id = insert_test_derivation(&db, "torn-poisoned").await?;
    sqlx::query("UPDATE derivations SET status = 'poisoned' WHERE derivation_id = $1")
        .bind(torn_id)
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status)
         VALUES ($1, 'w-crash', 1, 'pending')",
    )
    .bind(torn_id)
    .execute(&test_db.pool)
    .await?;
    // Control: non-terminal derivation with pending assignment → untouched.
    let live_id = insert_test_derivation(&db, "torn-live").await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status)
         VALUES ($1, 'w-live', 1, 'pending')",
    )
    .bind(live_id)
    .execute(&test_db.pool)
    .await?;

    // GC blocked: NOT EXISTS assignments…pending is forever false.
    assert_eq!(
        db.gc_orphan_terminal_derivations(1000).await?,
        0,
        "torn row blocks GC"
    );

    // Sweep repairs the torn row only.
    let swept = match db
        .sweep_stale_assignments(ServingGeneration::stamp_from_claim(1))
        .await?
    {
        crate::db::FencedOutcome::Applied(n) => n,
        other => panic!("expected Applied, got {other:?}"),
    };
    assert_eq!(swept, 1, "exactly the torn row repaired");
    let (torn_status,): (String,) =
        sqlx::query_as("SELECT status FROM assignments WHERE derivation_id = $1")
            .bind(torn_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(torn_status, "failed", "torn assignment closed → 'failed'");
    let (live_status,): (String,) =
        sqlx::query_as("SELECT status FROM assignments WHERE derivation_id = $1")
            .bind(live_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(live_status, "pending", "non-terminal derivation untouched");

    // GC now succeeds.
    assert_eq!(
        db.gc_orphan_terminal_derivations(1000).await?,
        1,
        "post-sweep, GC deletes the repaired row"
    );
    // Idempotent.
    assert_eq!(
        db.sweep_stale_assignments(ServingGeneration::stamp_from_claim(1))
            .await?,
        crate::db::FencedOutcome::Applied(0)
    );
    Ok(())
}

// -----------------------------------------------------------------------
// Claims-floor fence for the status/poison pool-variant writers
// (`sched.evidence.durability`): a deposed tenure's late status or
// poison write must be rolled back having written nothing; the current
// tenure's writes must apply.
// -----------------------------------------------------------------------

/// A17 / `sched.evidence.durability`: every status/poison pool-variant
/// writer is claims-floor fenced. The durable floor is 2 (a successor
/// claimed); a deposed tenure-1 replica's late writes — a terminal
/// status persist, a poison stamp, a poison clear (single and batch),
/// and a batch status persist — must ALL be fenced: rolled back having
/// written nothing, so the successor's view of those rows survives.
///
/// Pre-fence these writes applied unconditionally — the A17
/// stale-override window for status/poison evidence (red transcript in
/// the introducing commit).
// r[verify sched.evidence.durability+4]
#[tokio::test]
async fn stale_tenure_status_and_poison_writes_are_fenced() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Two rows owned by the successor's view: one non-terminal (the
    // status-write target), one poisoned (the clear target).
    let plain: DrvHash = "fence-status".into();
    let poisoned: DrvHash = "fence-poisoned".into();
    insert_test_derivation(&db, plain.as_str()).await?;
    insert_test_derivation(&db, poisoned.as_str()).await?;
    sqlx::query(
        "UPDATE derivations SET status = 'poisoned', poisoned_at = now() WHERE drv_hash = $1",
    )
    .bind(poisoned.as_str())
    .execute(&test_db.pool)
    .await?;

    // The successor has claimed generation 2.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'successor')",
    )
    .execute(&test_db.pool)
    .await?;

    // --- The deposed tenure-1 replica's late writes: all must be fenced ---
    // Terminal status persist (would also close assignment rows).
    assert_eq!(
        db.update_derivation_status(
            &plain,
            DerivationStatus::Cancelled,
            None,
            ServingGeneration::stamp_from_claim(1)
        )
        .await?,
        FencedOutcome::Fenced,
        "the status persist must be fenced below the floor"
    );
    // Batch status persist.
    assert_eq!(
        db.update_derivation_status_batch(
            &[plain.as_str()],
            DerivationStatus::DependencyFailed,
            ServingGeneration::stamp_from_claim(1)
        )
        .await?,
        FencedOutcome::Fenced,
        "the batch status persist must be fenced below the floor"
    );
    // Poison stamp on the plain row.
    assert_eq!(
        db.persist_poisoned(&plain, None, None, ServingGeneration::stamp_from_claim(1))
            .await?,
        FencedOutcome::Fenced,
        "the poison stamp must be fenced below the floor"
    );
    // Poison clear (single + batch) on the successor's poisoned row.
    assert_eq!(
        db.clear_poison(&poisoned, ServingGeneration::stamp_from_claim(1))
            .await?,
        FencedOutcome::Fenced,
        "the poison clear must be fenced below the floor"
    );
    assert_eq!(
        db.clear_poison_batch(
            std::slice::from_ref(&poisoned),
            ServingGeneration::stamp_from_claim(1)
        )
        .await?,
        FencedOutcome::Fenced,
        "the batch poison clear must be fenced below the floor"
    );

    // The successor's view survives every stale write.
    let (plain_status, plain_poisoned_at): (String, Option<f64>) = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM poisoned_at)::float8 FROM derivations \
         WHERE drv_hash = $1",
    )
    .bind(plain.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(
        plain_status, "created",
        "a deposed tenure's status persist must be fenced (status overwritten)"
    );
    assert!(
        plain_poisoned_at.is_none(),
        "a deposed tenure's poison stamp must be fenced (poisoned_at written)"
    );
    let (poisoned_status, poisoned_at): (String, Option<f64>) = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM poisoned_at)::float8 FROM derivations \
         WHERE drv_hash = $1",
    )
    .bind(poisoned.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(
        poisoned_status, "poisoned",
        "a deposed tenure's poison clear must be fenced (the successor's poison erased)"
    );
    assert!(
        poisoned_at.is_some(),
        "a deposed tenure's poison clear must be fenced (poisoned_at erased)"
    );
    Ok(())
}

/// The status/poison fence is not over-eager: the current tenure (at
/// the floor) and a fresh cluster (empty floor) apply normally.
// r[verify sched.evidence.durability+4]
#[tokio::test]
async fn current_tenure_status_and_poison_writes_apply() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let h: DrvHash = "fence-cur-status".into();
    insert_test_derivation(&db, h.as_str()).await?;

    // Fresh cluster (no claims): everything applies.
    assert_eq!(
        db.persist_poisoned(&h, None, None, ServingGeneration::stamp_from_claim(1))
            .await?,
        FencedOutcome::Applied(0)
    );
    let (status,): (String,) = sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
        .bind(h.as_str())
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(status, "poisoned", "fresh-cluster poison stamp must apply");

    // Current tenure at its own claim (floor == serving generation).
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (3, 'tenure-cur')",
    )
    .execute(&test_db.pool)
    .await?;
    assert_eq!(
        db.clear_poison(&h, ServingGeneration::stamp_from_claim(3))
            .await?,
        FencedOutcome::Applied(0)
    );
    assert_eq!(
        db.update_derivation_status(
            &h,
            DerivationStatus::Ready,
            None,
            ServingGeneration::stamp_from_claim(3)
        )
        .await?,
        FencedOutcome::Applied(0)
    );
    let (status, poisoned_at): (String, Option<f64>) = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM poisoned_at)::float8 FROM derivations \
         WHERE drv_hash = $1",
    )
    .bind(h.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "ready", "the current tenure's writes must apply");
    assert!(
        poisoned_at.is_none(),
        "the current tenure's clear must apply"
    );
    Ok(())
}

/// bug_158: a latched batch whose EVERY drv was dropped at flush-time
/// re-derivation must still close its latched exec rows — the
/// exec-scoped assignment close is unconditional on the latched
/// exec_ids; only the derivation-status UPDATE is kept-set-scoped
/// (the partial-kept arm always passed ALL latched exec_ids, proving
/// the close was meant to be unconditional). Pre-fix, the empty-drv
/// early return discarded the close: a cancel latched during a PG
/// blip followed by a resubmit left the attempt durably open until it
/// aged into ChargeExecutorCrash against a healthily-rebuilding
/// derivation.
// r[verify sched.attempt.cancel-close-driven+3]
#[tokio::test]
async fn replay_with_all_drvs_dropped_still_closes_latched_execs() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_id = insert_test_derivation(&db, "dropall-resubmitted").await?;
    let exec = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assignments \
             (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, 'builder-0', 1, 'acknowledged', $2)",
    )
    .bind(drv_id)
    .bind(exec)
    .execute(&test_db.pool)
    .await?;

    // Flush-time re-derivation dropped every drv (the node was
    // resubmitted and advanced past the latch): kept = [], latched
    // exec_ids non-empty.
    let outcome = db
        .replay_status_batch_guarded(
            &[],
            DerivationStatus::Cancelled,
            &[exec],
            std::time::Instant::now(),
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert!(
        matches!(&outcome, crate::db::StatusReplay::Applied { replayed, .. } if replayed.is_empty()),
        "no derivation row may be updated by an all-dropped batch, got {outcome:?}"
    );

    let (status,): (String,) = sqlx::query_as("SELECT status FROM assignments WHERE exec_id = $1")
        .bind(exec)
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(
        status, "cancelled",
        "latched exec row must close even when every drv was dropped"
    );
    Ok(())
}

/// merged_bug_025 red B — the resubmitted-non-terminal cell that PINS
/// the timestamp form of the precedence guard over the status-set
/// form: a drv resubmitted AFTER the latch sits Running with a newer
/// `updated_at`. The in-memory re-derivation cannot drop it when the
/// node was also reaped from the DAG (terminal latch keeps it), so the
/// SQL conjunct `updated_at <= now() - make_interval(secs => age)` is
/// the row-local refusal (PG-domain form, merged_bug_017). A
/// status-set guard ("only overwrite rows still in X") diverges
/// exactly here -- Running is not in any latched status set the
/// flusher could enumerate without re-deriving, and the absolute
/// UPDATE would regress it to the stale terminal status.
#[tokio::test]
async fn replay_refuses_rows_updated_after_the_latch() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    insert_test_derivation(&db, "post-latch-resubmit").await?;
    // The world advanced after the latch: the resubmitted drv is
    // Running, updated_at = now().
    sqlx::query(
        "UPDATE derivations SET status = 'running', updated_at = now(), \
                                status_changed_at = now() \
         WHERE drv_hash = 'post-latch-resubmit'",
    )
    .execute(&test_db.pool)
    .await?;

    // The latch predates the resubmit by a minute (a 60s-aged
    // monotonic enqueue anchor).
    let enqueued_at = std::time::Instant::now() - std::time::Duration::from_secs(60);
    let outcome = db
        .replay_status_batch_guarded(
            &["post-latch-resubmit"],
            DerivationStatus::Cancelled,
            &[],
            enqueued_at,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert!(
        matches!(&outcome, crate::db::StatusReplay::Applied { replayed, .. } if replayed.is_empty()),
        "left: {outcome:?} / right: Applied {{ replayed: [] }} (a row \
         updated after the latch refuses the replay)"
    );
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'post-latch-resubmit'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        status, "running",
        "left: {status} / right: running (the stale Cancelled latch \
         must not regress the resubmitted row)"
    );
    Ok(())
}

/// merged_bug_017 red R1: a fresh terminal latch must land whatever
/// the POD clock reads — the precedence comparison must live entirely
/// in the PG clock domain. Pre-fix the conjunct bound
/// `latched_at_epoch = epoch_now()` (pod `SystemTime`) against
/// `updated_at` stamped by PG `now()`: with PG ahead of the pod (the
/// pod 1h behind), the row's PG stamp postdated the latch's pod
/// stamp, the UPDATE zero-rowed, and the flusher's `Ok(_)` arm popped
/// the batch with an info "batch flushed" — the terminal latch
/// silently lost, the cancelled drv resurrecting from durable rows at
/// recovery. Red recorded verbatim against the old f64 API
/// (`epoch_now() - 3600.0`): `left: Applied(0) (terminal latch
/// silently refused by clock skew) / right: Applied(1)`. Post-fix the
/// latch anchor is the batch's MONOTONIC enqueue instant mapped into
/// the PG domain at flush time (`updated_at <= now() -
/// make_interval(secs => age)`) — the pod epoch is no longer an
/// input, so the skew shape this red exercised is UNWRITABLE through
/// the enqueue-instant parameter (the boundary-witnessed `LatchAge`
/// is minted inside the replay, merged_bug_004): this migrated form
/// pins the fresh latch landing, with the skew immunity carried by
/// the type.
#[tokio::test]
async fn replay_lands_under_pg_ahead_skew() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Seed: PG stamps `updated_at = now()` at insert; the cancel is
    // latched LOGICALLY AFTER that write (the batch enqueues now —
    // whatever epoch the pod clock would have read at that moment).
    insert_test_derivation(&db, "skew-cancel").await?;
    let enqueued_at = std::time::Instant::now();

    let outcome = db
        .replay_status_batch_guarded(
            &["skew-cancel"],
            DerivationStatus::Cancelled,
            &[],
            enqueued_at,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert!(
        matches!(
            &outcome,
            crate::db::StatusReplay::Applied { replayed, .. } if replayed == &["skew-cancel".to_string()]
        ),
        "left: {outcome:?} (terminal latch silently refused by clock \
         skew) / right: Applied {{ replayed: [\"skew-cancel\"] }} — a \
         fresh latch must replay whatever the pod clock reads"
    );
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'skew-cancel'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        status, "cancelled",
        "left: {status} / right: cancelled (the latched terminal status \
         must land)"
    );
    Ok(())
}

/// merged_bug_017 red R2: a row the world advanced AFTER the latch
/// must be refused row-locally AND the refusal must be a LOUD, named
/// outcome — the caller needs the refused drv set to warn/count, not
/// an `Ok(rows_affected)` shadow it never consults. Red recorded
/// verbatim against the old API (no refusal surface existed — the
/// only observable was the info "batch flushed"): `left: Applied(0)
/// — an anonymous count; the refused drv set is inexpressible /
/// right: a named replayed set the flusher can subtract from kept`.
/// The actor-side rider in actor/tests/misc.rs pins the flusher's
/// warn + counter emission and the batch-pop semantics.
#[tokio::test]
async fn advanced_row_refusal_is_returned_loud() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // The latch is 60s old; the row advanced (fresh PG write) after it.
    insert_test_derivation(&db, "advanced-row").await?;
    sqlx::query(
        "UPDATE derivations SET status = 'running', updated_at = now(), \
                                status_changed_at = now() \
         WHERE drv_hash = 'advanced-row'",
    )
    .execute(&test_db.pool)
    .await?;
    let enqueued_at = std::time::Instant::now() - std::time::Duration::from_secs(60);

    let outcome = db
        .replay_status_batch_guarded(
            &["advanced-row"],
            DerivationStatus::Cancelled,
            &[],
            enqueued_at,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    // Row-local refusal: the advanced row is never regressed.
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'advanced-row'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        status, "running",
        "the advanced row must not be regressed by the stale latch"
    );
    // The refusal is NAMED: the kept drv is absent from the replayed
    // set, so the caller computes refused = kept − replayed.
    match outcome {
        crate::db::StatusReplay::Applied { replayed, .. } => assert!(
            replayed.is_empty(),
            "left: {replayed:?} / right: [] (the advanced row must be \
             refused row-locally and absent from the named set)"
        ),
        other => panic!("unexpected outcome {other:?}"),
    }
    Ok(())
}

/// Epoch read of the precedence comparand — tests compare advance/
/// stasis only, so the float crosses the boundary as a NUMBER, never
/// as a timestamp bound back into a PG comparison.
async fn stamp_epoch(pool: &sqlx::PgPool, drv_hash: &str) -> anyhow::Result<f64> {
    let (epoch,): (f64,) = sqlx::query_as(
        "SELECT EXTRACT(EPOCH FROM status_changed_at)::float8 \
         FROM derivations WHERE drv_hash = $1",
    )
    .bind(drv_hash)
    .fetch_one(pool)
    .await?;
    Ok(epoch)
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_004/merged_bug_006: the ADVANCE half of the comparand
/// law. PROPOSITION CERTIFIED: driving EVERY production derivations
/// writer (the list pinned by `derivations_status_stamp_census`) with
/// a value-CHANGING write, the `status_changed_at` stamp advances —
/// the migration 102 trigger reproduces the old per-writer stamp on
/// every genuine transition — and every non-status writer leaves the
/// comparand stationary. The STASIS half for value-PRESERVING status
/// writes is the sibling test
/// `status_writers_hold_the_comparand_on_value_preserving_writes`.
/// The match below is total over the census consts: a writer added to
/// the census without a drive arm panics here.
#[tokio::test]
async fn status_writers_stamp_status_changed_at_biconditional() -> anyhow::Result<()> {
    use super::fence_coverage::{NON_STATUS_DERIVATIONS_WRITER_FNS, STATUS_WRITER_FNS};
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let generation = ServingGeneration::stamp_from_claim(1);

    for writer in STATUS_WRITER_FNS {
        let hash = format!("stamp-bicond-{writer}");
        let drv_hash: DrvHash = hash.as_str().into();
        insert_test_derivation(&db, &hash).await?;
        // Backdate both stamps so the replay writer's cut admits the
        // row and an advance is unambiguous.
        sqlx::query(
            "UPDATE derivations SET \
               updated_at = now() - interval '120 seconds', \
               status_changed_at = now() - interval '120 seconds' \
             WHERE drv_hash = $1",
        )
        .bind(&hash)
        .execute(&test_db.pool)
        .await?;
        // World setup: the clear arms need a poisoned row to clear
        // (setup runs BEFORE the before-read so the drive itself is
        // the only status transition under measurement).
        if matches!(*writer, "clear_poison_in_tx" | "clear_poison_batch_in_tx") {
            assert!(
                db.persist_poisoned(&drv_hash, None, None, generation)
                    .await?
                    .settled(),
                "world setup poison must apply"
            );
            sqlx::query(
                "UPDATE derivations SET \
                   status_changed_at = now() - interval '120 seconds' \
                 WHERE drv_hash = $1",
            )
            .bind(&hash)
            .execute(&test_db.pool)
            .await?;
        }
        let before = stamp_epoch(&test_db.pool, &hash).await?;
        let (status_before,): (String,) =
            sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(&hash)
                .fetch_one(&test_db.pool)
                .await?;
        match *writer {
            "update_derivation_status_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::update_derivation_status_in_tx(
                    &mut tx,
                    &drv_hash,
                    DerivationStatus::Ready,
                    None,
                )
                .await?;
                tx.commit().await?;
            }
            "update_derivation_status_batch_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::update_derivation_status_batch_in_tx(
                    &mut tx,
                    &[hash.as_str()],
                    DerivationStatus::Cancelled,
                )
                .await?;
                tx.commit().await?;
            }
            "replay_status_batch_guarded" => {
                let outcome = db
                    .replay_status_batch_guarded(
                        &[hash.as_str()],
                        DerivationStatus::Cancelled,
                        &[],
                        std::time::Instant::now() - std::time::Duration::from_secs(60),
                        generation,
                    )
                    .await?;
                assert!(
                    matches!(
                        &outcome,
                        crate::db::StatusReplay::Applied { replayed, .. } if replayed == std::slice::from_ref(&hash)
                    ),
                    "replay drive must apply (got {outcome:?})"
                );
            }
            "persist_poisoned_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::persist_poisoned_in_tx(&mut tx, &drv_hash, None, None).await?;
                tx.commit().await?;
            }
            "clear_poison_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::clear_poison_in_tx(&mut tx, &drv_hash).await?;
                tx.commit().await?;
            }
            "clear_poison_batch_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::clear_poison_batch_in_tx(&mut tx, std::slice::from_ref(&drv_hash))
                    .await?;
                tx.commit().await?;
            }
            other => panic!(
                "census names status writer `{other}` but the runtime \
                 biconditional does not drive it — add a drive arm"
            ),
        }
        let after = stamp_epoch(&test_db.pool, &hash).await?;
        let (status_after,): (String,) =
            sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(&hash)
                .fetch_one(&test_db.pool)
                .await?;
        assert_ne!(
            status_before, status_after,
            "drive for `{writer}` must actually change status"
        );
        assert!(
            after > before,
            "status changed but status_changed_at did not advance \
             (writer: {writer}): left: {after} / right: > {before}"
        );
    }

    for writer in NON_STATUS_DERIVATIONS_WRITER_FNS {
        let hash = format!("stamp-bicond-{writer}");
        let drv_hash: DrvHash = hash.as_str().into();
        insert_test_derivation(&db, &hash).await?;
        let before = stamp_epoch(&test_db.pool, &hash).await?;
        match *writer {
            "update_resource_floor" => {
                let outcome = db
                    .update_resource_floor(
                        &drv_hash,
                        &crate::state::ResourceFloor {
                            mem_bytes: 1 << 30,
                            disk_bytes: 1 << 31,
                            deadline_secs: 600,
                            cores: 0,
                        },
                        generation,
                    )
                    .await?;
                assert!(outcome.settled(), "floor ratchet must apply");
            }
            "batch_upsert_derivations" => {
                // Re-merge the same drv: the ON CONFLICT DO UPDATE arm.
                insert_test_derivation(&db, &hash).await?;
            }
            other => panic!(
                "census names non-status writer `{other}` but the \
                 runtime biconditional does not drive it — add an arm"
            ),
        }
        let after = stamp_epoch(&test_db.pool, &hash).await?;
        assert!(
            (after - before).abs() < 1e-9,
            "non-status writer `{writer}` moved status_changed_at: \
             left: {after} / right: {before} (comparand purity broken)"
        );
    }
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_006 R1: the STASIS half of the comparand law.
/// PROPOSITION CERTIFIED: for EVERY member of the machine-derived
/// `STATUS_WRITER_FNS` census, a value-PRESERVING drive leaves
/// `status_changed_at` byte-stationary — the exact invariant the spec
/// sentence quantifies over ("the comparand moves iff the status
/// VALUE changes"), not column presence. The match is total over the
/// census consts: a writer added to the census without a
/// value-preserving drive arm panics here. The replay arm is the one
/// writer whose own WHERE already excludes the same-value row (it
/// classifies `AlreadyApplied`); the five siblings pre-fix stamped
/// unconditionally.
#[tokio::test]
async fn status_writers_hold_the_comparand_on_value_preserving_writes() -> anyhow::Result<()> {
    use super::fence_coverage::STATUS_WRITER_FNS;
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let generation = ServingGeneration::stamp_from_claim(1);

    for writer in STATUS_WRITER_FNS {
        let hash = format!("stamp-stasis-{writer}");
        let drv_hash: DrvHash = hash.as_str().into();
        insert_test_derivation(&db, &hash).await?;

        // World setup: a value-CHANGING drive to the target each
        // writer re-asserts, so the measured drive below is
        // value-preserving by construction.
        match *writer {
            "update_derivation_status_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::update_derivation_status_in_tx(
                    &mut tx,
                    &drv_hash,
                    DerivationStatus::Ready,
                    None,
                )
                .await?;
                tx.commit().await?;
            }
            "update_derivation_status_batch_in_tx" | "replay_status_batch_guarded" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::update_derivation_status_batch_in_tx(
                    &mut tx,
                    &[hash.as_str()],
                    DerivationStatus::Cancelled,
                )
                .await?;
                tx.commit().await?;
            }
            "persist_poisoned_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::persist_poisoned_in_tx(&mut tx, &drv_hash, None, None).await?;
                tx.commit().await?;
            }
            // The clear arms re-assert 'created' — the fresh insert's
            // own status; no transition needed.
            "clear_poison_in_tx" | "clear_poison_batch_in_tx" => {}
            other => panic!(
                "census names status writer `{other}` but the stasis \
                 test does not classify its world setup — add an arm"
            ),
        }
        // Backdate so a spurious re-stamp is unambiguous (and the
        // replay arm's age cut admits the row).
        sqlx::query(
            "UPDATE derivations SET \
               updated_at = now() - interval '120 seconds', \
               status_changed_at = now() - interval '120 seconds' \
             WHERE drv_hash = $1",
        )
        .bind(&hash)
        .execute(&test_db.pool)
        .await?;
        let before = stamp_epoch(&test_db.pool, &hash).await?;
        let (status_before,): (String,) =
            sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(&hash)
                .fetch_one(&test_db.pool)
                .await?;

        // The measured drive: the SAME writer, the SAME target value.
        match *writer {
            "update_derivation_status_in_tx" => {
                // Same status, new builder id — the same-status
                // re-assignment shape from the merged_bug_006 trace.
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::update_derivation_status_in_tx(
                    &mut tx,
                    &drv_hash,
                    DerivationStatus::Ready,
                    Some(&ExecutorId::from("builder-reassigned")),
                )
                .await?;
                tx.commit().await?;
            }
            "update_derivation_status_batch_in_tx" => {
                // Duplicate cancel via the batch writer.
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::update_derivation_status_batch_in_tx(
                    &mut tx,
                    &[hash.as_str()],
                    DerivationStatus::Cancelled,
                )
                .await?;
                tx.commit().await?;
            }
            "replay_status_batch_guarded" => {
                // Replay of an already-at-target row: the WHERE's
                // `status IS DISTINCT FROM $2` excludes it — the one
                // writer that was guarded pre-102. Classifies
                // AlreadyApplied, never re-stamps.
                let outcome = db
                    .replay_status_batch_guarded(
                        &[hash.as_str()],
                        DerivationStatus::Cancelled,
                        &[],
                        std::time::Instant::now() - std::time::Duration::from_secs(60),
                        generation,
                    )
                    .await?;
                assert!(
                    matches!(
                        &outcome,
                        crate::db::StatusReplay::Applied { replayed, residual }
                            if replayed.is_empty()
                                && matches!(
                                    residual.as_slice(),
                                    [(h, crate::db::ReplayResidual::AlreadyApplied)] if h == &hash
                                )
                    ),
                    "same-value replay must classify AlreadyApplied (got {outcome:?})"
                );
            }
            "persist_poisoned_in_tx" => {
                // Re-poison the poisoned row.
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::persist_poisoned_in_tx(&mut tx, &drv_hash, None, None).await?;
                tx.commit().await?;
            }
            "clear_poison_in_tx" => {
                // Clear of an already-'created' row.
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::clear_poison_in_tx(&mut tx, &drv_hash).await?;
                tx.commit().await?;
            }
            "clear_poison_batch_in_tx" => {
                let mut tx = test_db.pool.begin().await?;
                SchedulerDb::clear_poison_batch_in_tx(&mut tx, std::slice::from_ref(&drv_hash))
                    .await?;
                tx.commit().await?;
            }
            other => panic!(
                "census names status writer `{other}` but the stasis \
                 test does not drive it — add a value-preserving arm"
            ),
        }
        let after = stamp_epoch(&test_db.pool, &hash).await?;
        let (status_after,): (String,) =
            sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(&hash)
                .fetch_one(&test_db.pool)
                .await?;
        assert_eq!(
            status_before, status_after,
            "drive for `{writer}` must be value-preserving"
        );
        assert!(
            (after - before).abs() < 1e-9,
            "left: epoch advanced on a value-preserving write (the comparand \
             re-asserted a non-change) / right: stationary \
             (writer: {writer}, before: {before}, after: {after})"
        );
    }
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_006 R2: the end-to-end form of the false-refusal trace.
/// PROPOSITION CERTIFIED: a kept DAG-absent terminal latch survives an
/// interposed value-preserving write — replayed and named in the
/// RETURNING set, not `RefusedNewer` — i.e.
/// `rio_scheduler_status_outbox_replay_refused_total` counts evidenced
/// foreign precedence ONLY, the metric's documented semantics. Pre-102
/// the interposed no-op write advanced the comparand past the latch
/// cut and the batch popped as final with the durable row stale
/// forever (the terminal-KEEP latch is the node's LAST truth).
#[tokio::test]
async fn kept_terminal_latch_survives_value_preserving_interposition() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let generation = ServingGeneration::stamp_from_claim(1);

    let hash = "latch-survives-interposition";
    let drv_hash: DrvHash = hash.into();
    insert_test_derivation(&db, hash).await?;
    let mut tx = test_db.pool.begin().await?;
    SchedulerDb::update_derivation_status_in_tx(&mut tx, &drv_hash, DerivationStatus::Queued, None)
        .await?;
    tx.commit().await?;

    // The row's last STATUS event was 120s ago; the terminal latch
    // was enqueued 60s ago (PG was down at persist time; the node
    // left the DAG, so flush-time re-derivation KEEPS the latch).
    sqlx::query(
        "UPDATE derivations SET \
           updated_at = now() - interval '120 seconds', \
           status_changed_at = now() - interval '120 seconds' \
         WHERE drv_hash = $1",
    )
    .bind(hash)
    .execute(&test_db.pool)
    .await?;

    // Interpose a value-preserving write inside the latch->flush
    // window: a duplicate Queued re-assertion via the batch writer.
    let mut tx = test_db.pool.begin().await?;
    SchedulerDb::update_derivation_status_batch_in_tx(&mut tx, &[hash], DerivationStatus::Queued)
        .await?;
    tx.commit().await?;

    // Replay the kept terminal latch.
    let outcome = db
        .replay_status_batch_guarded(
            &[hash],
            DerivationStatus::Cancelled,
            &[],
            std::time::Instant::now() - std::time::Duration::from_secs(60),
            generation,
        )
        .await?;
    match &outcome {
        crate::db::StatusReplay::Applied { replayed, residual } => {
            assert_eq!(
                replayed.as_slice(),
                std::slice::from_ref(&hash.to_string()),
                "left: replayed=[] and residual={residual:?} (a no-op write \
                 popped the latched terminal as final) / right: replayed=[drv]"
            );
        }
        other => panic!("replay must commit (got {other:?})"),
    }
    let (status,): (String,) = sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
        .bind(hash)
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(status, "cancelled", "the latched terminal must land");
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_004 hole 2 red: a NON-status write between latch and
/// flush must not refuse a latched terminal replay. PROPOSITION
/// CERTIFIED: the precedence conjunct's comparand is writable only by
/// status events — a resource-floor ratchet (an `updated_at`-bumping,
/// status-preserving write) leaves the latched terminal persist
/// applicable. Pre-fix the conjunct read `updated_at`, so the floor
/// bump permanently cancelled the persist: replayed == [].
#[tokio::test]
async fn outbox_floor_bump_does_not_refuse_latched_terminal_replay() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let generation = ServingGeneration::stamp_from_claim(1);

    // The row's last STATUS event was 120s ago; the terminal latch
    // was enqueued 60s ago (PG was down at persist time).
    insert_test_derivation(&db, "floor-bump-latched").await?;
    sqlx::query(
        "UPDATE derivations SET \
           updated_at = now() - interval '120 seconds', \
           status_changed_at = now() - interval '120 seconds' \
         WHERE drv_hash = 'floor-bump-latched'",
    )
    .execute(&test_db.pool)
    .await?;
    let enqueued_at = std::time::Instant::now() - std::time::Duration::from_secs(60);

    // Between latch and flush: an OOM report ratchets the floor — a
    // production write that bumps updated_at and PRESERVES status.
    let floor_outcome = db
        .update_resource_floor(
            &DrvHash::from("floor-bump-latched"),
            &crate::state::ResourceFloor {
                mem_bytes: 4 << 30,
                disk_bytes: 8 << 30,
                deadline_secs: 900,
                cores: 0,
            },
            generation,
        )
        .await?;
    assert!(floor_outcome.settled(), "floor ratchet must apply");

    let outcome = db
        .replay_status_batch_guarded(
            &["floor-bump-latched"],
            DerivationStatus::Cancelled,
            &[],
            enqueued_at,
            generation,
        )
        .await?;
    assert!(
        matches!(
            &outcome,
            crate::db::StatusReplay::Applied { replayed, .. }
                if replayed == &["floor-bump-latched".to_string()]
        ),
        "left: {outcome:?} / right: Applied {{ replayed: \
         [\"floor-bump-latched\"] }} (a floor bump is not a status \
         event; it must not refuse the latched terminal replay)"
    );
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'floor-bump-latched'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(status, "cancelled", "the latched terminal status lands");
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+3]
/// merged_bug_004 hole 3 polarity guard — DISCLOSED, not a behavioral
/// red: at test speeds the pre-fix sampling skew is microseconds, so
/// this test cannot distinguish the boundary-sampled cut from the
/// argument-construction cut by outcome. PROPOSITION CERTIFIED
/// (direction only): a row whose status advanced AFTER the enqueue
/// instant is REFUSED, never overwritten — the conservative polarity
/// the boundary-witnessed constructor's envelope law claims (realized
/// cut <= enqueue instant; the structural half is the constructor
/// type, which demands the open replay transaction).
#[tokio::test]
async fn outbox_replay_cut_is_conservative_to_the_enqueue_instant() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let generation = ServingGeneration::stamp_from_claim(1);

    insert_test_derivation(&db, "cut-conservative").await?;
    let enqueued_at = std::time::Instant::now();
    // The row's status advances AFTER the enqueue instant, through a
    // production status writer (stamp = its commit's now()).
    let mut tx = test_db.pool.begin().await?;
    SchedulerDb::update_derivation_status_in_tx(
        &mut tx,
        &DrvHash::from("cut-conservative"),
        DerivationStatus::Running,
        None,
    )
    .await?;
    tx.commit().await?;

    let outcome = db
        .replay_status_batch_guarded(
            &["cut-conservative"],
            DerivationStatus::Cancelled,
            &[],
            enqueued_at,
            generation,
        )
        .await?;
    assert!(
        matches!(&outcome, crate::db::StatusReplay::Applied { replayed, .. } if replayed.is_empty()),
        "left: {outcome:?} / right: Applied {{ replayed: [] }} (a row \
         advanced after the enqueue instant must refuse the replay — \
         the cut may never land ahead of the enqueue)"
    );
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'cut-conservative'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        status, "running",
        "left: {status} / right: running (refuse, never overwrite)"
    );
    Ok(())
}
