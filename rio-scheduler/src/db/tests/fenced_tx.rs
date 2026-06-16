//! Structural tests for the [`FencedTx`] capability — the fence's own
//! contract, independent of any particular writer: below-floor refusal
//! writes nothing, equal-generation passes (same-epoch re-acquire
//! keep), the fresh-cluster `None` floor passes, drop-without-commit
//! rolls back, `commit_refenced` refuses on a mid-transaction claim,
//! and the mint upsert's server-side statement guard closes the
//! READ-COMMITTED TOCTOU the begin-time floor read leaves open
//! (bug_261).

use crate::db::ServingGeneration;
use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::{AssignmentCloseStatus, FencedBegin, FencedCommit, FencedOutcome, SchedulerDb};
use crate::state::{AttemptKind, ExecutorId};

/// Insert a leader generation claim row (the floor's second arm).
async fn insert_claim(pool: &sqlx::PgPool, generation: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, claimed_at) \
         VALUES ($1, now()) ON CONFLICT DO NOTHING",
    )
    .bind(generation)
    .execute(pool)
    .await?;
    Ok(())
}

/// Below-floor begin refuses with nothing written and the connection
/// returned.
// r[verify sched.evidence.durability+4]
#[tokio::test]
async fn begin_fenced_below_floor_writes_nothing() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    insert_claim(&test_db.pool, 5).await?;

    match db
        .begin_fenced(ServingGeneration::stamp_from_claim(4))
        .await?
    {
        FencedBegin::Fenced { floor } => assert_eq!(floor, 5),
        FencedBegin::Open(_) => panic!("generation 4 must not pass a floor of 5"),
    }
    Ok(())
}

/// Equal generation passes — the same-epoch re-acquire keep.
#[tokio::test]
async fn begin_fenced_equal_generation_passes() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    insert_claim(&test_db.pool, 5).await?;

    match db
        .begin_fenced(ServingGeneration::stamp_from_claim(5))
        .await?
    {
        FencedBegin::Open(ftx) => ftx.commit().await?,
        FencedBegin::Fenced { .. } => panic!("equal generation must pass (>= comparison)"),
    }
    Ok(())
}

/// Fresh cluster (no claims, no assignments): `None` floor admits any
/// generation.
#[tokio::test]
async fn begin_fenced_fresh_cluster_passes() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    match db
        .begin_fenced(ServingGeneration::stamp_from_claim(1))
        .await?
    {
        FencedBegin::Open(ftx) => ftx.commit().await?,
        FencedBegin::Fenced { .. } => panic!("fresh cluster must admit"),
    }
    Ok(())
}

/// Dropping the capability without commit rolls the transaction back.
#[tokio::test]
async fn fenced_tx_drop_rolls_back() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let drv_id = insert_test_derivation(&db, "droproll").await?;
    let exec_id = Uuid::now_v7();
    let outcome = db
        .mint_pull_attempt_fenced(
            drv_id,
            &ExecutorId::from("w-drop"),
            ServingGeneration::stamp_from_claim(1),
            exec_id,
            "droproll",
            None,
            None,
            AttemptKind::Build,
            None,
        )
        .await?;
    assert!(outcome.settled());

    {
        let mut ftx = match db
            .begin_fenced(ServingGeneration::stamp_from_claim(1))
            .await?
        {
            FencedBegin::Open(ftx) => ftx,
            FencedBegin::Fenced { .. } => panic!("must admit"),
        };
        let n = ftx
            .close_assignment(exec_id, AssignmentCloseStatus::Failed)
            .await?;
        assert_eq!(n, 1);
        // ftx dropped here WITHOUT commit.
    }

    let status: String = sqlx::query_scalar("SELECT status FROM assignments WHERE exec_id = $1")
        .bind(exec_id)
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(
        status, "pending",
        "drop-without-commit must roll back the close"
    );
    Ok(())
}

/// `commit_refenced` refuses when a newer claim landed after begin —
/// the settlement writers' pre-commit re-check, deterministically
/// recreated.
#[tokio::test]
async fn commit_refenced_refuses_on_mid_tx_claim() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let ftx = match db
        .begin_fenced(ServingGeneration::stamp_from_claim(1))
        .await?
    {
        FencedBegin::Open(ftx) => ftx,
        FencedBegin::Fenced { .. } => panic!("must admit at begin"),
    };
    // A successor claims a higher generation on a SECOND connection
    // while the first transaction is open.
    insert_claim(&test_db.pool, 2).await?;

    match ftx.commit_refenced().await? {
        FencedCommit::Refenced { floor } => assert_eq!(floor, 2),
        FencedCommit::Committed => panic!("mid-tx claim must refuse the commit"),
    }
    Ok(())
}

/// bug_261 red-first: the mint upsert's destructive half must carry a
/// server-side generation predicate. A deposed replica whose begin-time
/// floor read raced a successor's claim+mint commits its upsert under
/// READ COMMITTED — without the statement guard the `DO UPDATE`
/// overwrites the successor's newer row (generation regresses, the
/// successor's exec_id is clobbered). With the guard, the conflict
/// arm's WHERE evaluates against the row's latest committed version
/// (EvalPlanQual) and updates zero rows.
///
/// The race is made deterministic by handing the "deposed" replica a
/// transaction whose floor read happened BEFORE the successor's rows
/// committed: we open the fenced tx first, then commit the successor's
/// claim + mint on a second connection, then run the production upsert
/// statement inside the first transaction.
// r[verify sched.lease.fence-statement-guard]
#[tokio::test]
async fn mint_statement_guard_blocks_generation_regression() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let drv_id = insert_test_derivation(&db, "guardrace").await?;

    // Deposed replica at generation 4: floor read sees the gen-4 world.
    let mut deposed = match db
        .begin_fenced(ServingGeneration::stamp_from_claim(4))
        .await?
    {
        FencedBegin::Open(ftx) => ftx,
        FencedBegin::Fenced { .. } => panic!("gen 4 must admit in a gen-4 world"),
    };
    // Force the snapshot/floor read to happen now.
    sqlx::query("SELECT 1").execute(deposed.conn()).await?;

    // Successor: claim 5 + mint E1 commit on a second connection.
    insert_claim(&test_db.pool, 5).await?;
    let successor_exec = Uuid::now_v7();
    let minted = db
        .mint_pull_attempt_fenced(
            drv_id,
            &ExecutorId::from("w-successor"),
            ServingGeneration::stamp_from_claim(5),
            successor_exec,
            "guardrace",
            None,
            None,
            AttemptKind::Build,
            None,
        )
        .await?;
    assert!(minted.settled());

    // The deposed replica now runs the PRODUCTION upsert at gen 4 with
    // its own exec id, inside the transaction whose floor read predates
    // the successor.
    let deposed_exec = Uuid::now_v7();
    let rows = SchedulerDb::mint_assignment_upsert_in_tx(
        deposed.conn(),
        drv_id,
        "w-deposed",
        4,
        deposed_exec,
        None,
    )
    .await?;
    assert_eq!(
        rows, 0,
        "the statement guard must refuse the lower-generation overwrite"
    );
    deposed.commit().await?;

    let (generation, exec_id): (i64, Uuid) = sqlx::query_as(
        "SELECT generation, exec_id FROM assignments WHERE derivation_id = $1 \
         AND status IN ('pending','acknowledged')",
    )
    .bind(drv_id)
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(generation, 5, "successor's generation must survive");
    assert_eq!(exec_id, successor_exec, "successor's exec must survive");
    Ok(())
}

/// Green companion to the guard: a same-generation re-mint (the
/// legitimate re-pull refresh) still applies through `<=`.
#[tokio::test]
async fn mint_same_generation_remint_applies() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let drv_id = insert_test_derivation(&db, "remint").await?;

    let first = Uuid::now_v7();
    let outcome = db
        .mint_pull_attempt_fenced(
            drv_id,
            &ExecutorId::from("w-1"),
            ServingGeneration::stamp_from_claim(3),
            first,
            "remint",
            None,
            None,
            AttemptKind::Build,
            None,
        )
        .await?;
    assert!(outcome.settled());

    let second = Uuid::now_v7();
    let outcome = db
        .mint_pull_attempt_fenced(
            drv_id,
            &ExecutorId::from("w-2"),
            ServingGeneration::stamp_from_claim(3),
            second,
            "remint",
            None,
            None,
            AttemptKind::Build,
            None,
        )
        .await?;
    assert!(outcome.settled(), "equal-generation re-mint must apply");

    let exec_id: Uuid = sqlx::query_scalar(
        "SELECT exec_id FROM assignments WHERE derivation_id = $1 \
         AND status IN ('pending','acknowledged')",
    )
    .bind(drv_id)
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(exec_id, second);
    Ok(())
}

/// bug_273 red-first: the resource floor writer is fenced AND
/// server-side monotone per dimension.
#[tokio::test]
async fn resource_floor_is_fenced_and_monotone() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let _drv_id = insert_test_derivation(&db, "floorx").await?;
    let drv_hash = crate::state::DrvHash::from("floorx");

    // Promote to 16G at generation 2.
    insert_claim(&test_db.pool, 2).await?;
    let promoted = crate::state::ResourceFloor {
        mem_bytes: 16 << 30,
        ..Default::default()
    };
    assert!(
        db.update_resource_floor(&drv_hash, &promoted, ServingGeneration::stamp_from_claim(2))
            .await?
            .settled()
    );

    // (a) A deposed gen-1 replica's late 8G write is FENCED: still 16G.
    let stale = crate::state::ResourceFloor {
        mem_bytes: 8 << 30,
        ..Default::default()
    };
    assert_eq!(
        db.update_resource_floor(&drv_hash, &stale, ServingGeneration::stamp_from_claim(1))
            .await?,
        FencedOutcome::Fenced
    );
    let mem: i64 =
        sqlx::query_scalar("SELECT floor_mem_bytes FROM derivations WHERE drv_hash = $1")
            .bind(drv_hash.as_str())
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(mem, 16 << 30, "deposed write must not regress the floor");

    // (b) A LIVE gen-2 write from a stale in-memory base (8G) applies
    // but the GREATEST ratchet keeps 16G — the same-tenure regression
    // the fence cannot see.
    assert!(
        db.update_resource_floor(&drv_hash, &stale, ServingGeneration::stamp_from_claim(2))
            .await?
            .settled()
    );
    let mem: i64 =
        sqlx::query_scalar("SELECT floor_mem_bytes FROM derivations WHERE drv_hash = $1")
            .bind(drv_hash.as_str())
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(mem, 16 << 30, "GREATEST must keep the promoted dimension");

    // (c) A genuine promotion to 32G goes through.
    let bigger = crate::state::ResourceFloor {
        mem_bytes: 32 << 30,
        ..Default::default()
    };
    assert!(
        db.update_resource_floor(&drv_hash, &bigger, ServingGeneration::stamp_from_claim(2))
            .await?
            .settled()
    );
    let mem: i64 =
        sqlx::query_scalar("SELECT floor_mem_bytes FROM derivations WHERE drv_hash = $1")
            .bind(drv_hash.as_str())
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(mem, 32 << 30);

    // (d) sh-012: the cores dimension (M_106) ratchets independently
    // — a cores=8 write at gen-2 lands GREATEST(0,8)=8; a stale
    // gen-2 cores=4 base keeps 8; mem (32G) is untouched.
    let cores_promote = crate::state::ResourceFloor {
        cores: 8,
        ..Default::default()
    };
    assert!(
        db.update_resource_floor(
            &drv_hash,
            &cores_promote,
            ServingGeneration::stamp_from_claim(2)
        )
        .await?
        .settled()
    );
    let cores_stale = crate::state::ResourceFloor {
        cores: 4,
        ..Default::default()
    };
    assert!(
        db.update_resource_floor(
            &drv_hash,
            &cores_stale,
            ServingGeneration::stamp_from_claim(2)
        )
        .await?
        .settled()
    );
    let (cores, mem): (i32, i64) =
        sqlx::query_as("SELECT floor_cores, floor_mem_bytes FROM derivations WHERE drv_hash = $1")
            .bind(drv_hash.as_str())
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        cores, 8,
        "floor_cores GREATEST keeps the promoted dimension"
    );
    assert_eq!(mem, 32 << 30, "the cores write left mem untouched");
    Ok(())
}
