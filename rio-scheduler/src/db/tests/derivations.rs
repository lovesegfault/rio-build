//! Poison/retry/status transition tests.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::{TERMINAL_STATUSES, insert_test_derivation};
use crate::db::{FencedWrite, SchedulerDb};
use crate::state::DrvHash;

// r[verify sched.poison.ttl-persist]
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
    // AND assigned_builder_id=NULL. No separate status update needed.
    // (No claims rows in this test -> any serving generation passes the
    // claims-floor fence; same for every write below.)
    db.persist_poisoned(&drv_hash, 1).await?;

    // Verify all three columns updated in one statement.
    let (status, has_ts, worker): (String, bool, Option<String>) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL, assigned_builder_id \
         FROM derivations WHERE drv_hash=$1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "poisoned");
    assert!(has_ts, "poisoned_at must be set in the same statement");
    assert!(worker.is_none(), "assigned_builder_id must be NULLed");

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
    db.clear_poison(&drv_hash, 1).await?;
    let rows = db.load_poisoned_derivations().await?;
    assert!(
        rows.is_empty(),
        "clear_poison should remove from poisoned set"
    );

    let (status, poisoned_at): (String, Option<f64>) = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM poisoned_at)::float8 \
         FROM derivations WHERE drv_hash=$1",
    )
    .bind(drv_hash.as_str())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(status, "created");
    assert!(poisoned_at.is_none());
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
        db.persist_poisoned(h, 1).await?;
    }
    assert_eq!(db.load_poisoned_derivations().await?.len(), 100);

    // One call → 100 rows affected. The single-round-trip property is
    // structural (one `execute()`), so the assertion is on
    // `rows_affected`, not a query-count mock.
    let affected = db.clear_poison_batch(&hashes, 1).await?;
    assert_eq!(
        affected,
        FencedWrite::Applied(100),
        "one ANY($1) UPDATE should touch all 100"
    );

    // Poison lifecycle state cleared: status='created', poisoned_at
    // NULL. Nothing else on the derivations row carries retry state any
    // more (migration 075 dropped the mirror columns).
    assert!(db.load_poisoned_derivations().await?.is_empty());
    let (n_created, n_clean): (i64, i64) = sqlx::query_as(
        "SELECT
             COUNT(*) FILTER (WHERE status = 'created'),
             COUNT(*) FILTER (WHERE poisoned_at IS NULL)
         FROM derivations WHERE drv_hash = ANY($1)",
    )
    .bind(hashes.iter().map(DrvHash::as_str).collect::<Vec<_>>())
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(n_created, 100);
    assert_eq!(n_clean, 100);

    // Empty input: no-op, no PG round-trip.
    assert_eq!(
        db.clear_poison_batch(&[], 1).await?,
        FencedWrite::Applied(0)
    );
    Ok(())
}

// r[verify sched.db.derivations-gc+3]
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

// r[verify sched.db.derivations-gc+3]
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
    let swept = db.sweep_stale_assignments().await?;
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
    assert_eq!(db.sweep_stale_assignments().await?, 0);
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
// r[verify sched.evidence.durability+2]
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
        db.update_derivation_status(&plain, DerivationStatus::Cancelled, None, 1)
            .await?,
        FencedWrite::Fenced,
        "the status persist must be fenced below the floor"
    );
    // Batch status persist.
    assert_eq!(
        db.update_derivation_status_batch(&[plain.as_str()], DerivationStatus::DependencyFailed, 1)
            .await?,
        FencedWrite::Fenced,
        "the batch status persist must be fenced below the floor"
    );
    // Poison stamp on the plain row.
    assert_eq!(
        db.persist_poisoned(&plain, 1).await?,
        FencedWrite::Fenced,
        "the poison stamp must be fenced below the floor"
    );
    // Poison clear (single + batch) on the successor's poisoned row.
    assert_eq!(
        db.clear_poison(&poisoned, 1).await?,
        FencedWrite::Fenced,
        "the poison clear must be fenced below the floor"
    );
    assert_eq!(
        db.clear_poison_batch(std::slice::from_ref(&poisoned), 1)
            .await?,
        FencedWrite::Fenced,
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
// r[verify sched.evidence.durability+2]
#[tokio::test]
async fn current_tenure_status_and_poison_writes_apply() -> anyhow::Result<()> {
    use crate::state::DerivationStatus;

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let h: DrvHash = "fence-cur-status".into();
    insert_test_derivation(&db, h.as_str()).await?;

    // Fresh cluster (no claims): everything applies.
    assert_eq!(db.persist_poisoned(&h, 1).await?, FencedWrite::Applied(0));
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
    assert_eq!(db.clear_poison(&h, 3).await?, FencedWrite::Applied(0));
    assert_eq!(
        db.update_derivation_status(&h, DerivationStatus::Ready, None, 3)
            .await?,
        FencedWrite::Applied(0)
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
