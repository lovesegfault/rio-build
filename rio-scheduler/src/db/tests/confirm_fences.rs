//! `executor_confirm_fences` (migration 097) integration tests: the
//! confirm-exit fence rows — write-ahead insert idempotency, the
//! generation-fenced write refusal (bug_015), the DeliverNew screen's
//! read, and the housekeeping TTL rider's sweep.

use rio_test_support::TestDb;

use crate::db::confirm_fences::ConfirmFenceWrite;
use crate::db::{SchedulerDb, ServingGeneration};

/// Fresh ephemeral PG + a SchedulerDb handle.
async fn setup() -> (TestDb, SchedulerDb) {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    (test_db, db)
}

/// Insert is idempotent (re-confirms upsert nothing), the read sees
/// the fence, an unrelated hash stays unfenced. Witness-strength note:
/// this certifies IDEMPOTENCE — the deposed-writer refusal is
/// certified by `insert_confirm_fence_below_floor_writes_nothing`
/// below and by the actor-level
/// `deposed_replica_cannot_mint_the_exit0_license`.
#[tokio::test]
async fn fence_insert_idempotent_and_read() -> anyhow::Result<()> {
    let (_pg, db) = setup().await;
    let hash = "a".repeat(64);
    let g = ServingGeneration::stamp_from_claim(1);

    assert!(db.confirm_fence_exists(&hash).await?.is_none());
    let w = db.insert_confirm_fence(&hash, "intent-1", g).await?;
    assert!(matches!(w, ConfirmFenceWrite::Durable(_)));
    // The builder's confirm regime retries: the second insert is the
    // same license, not an error.
    let w = db.insert_confirm_fence(&hash, "intent-1", g).await?;
    assert!(matches!(w, ConfirmFenceWrite::Durable(_)));
    assert!(db.confirm_fence_exists(&hash).await?.is_some());
    assert!(db.confirm_fence_exists(&"b".repeat(64)).await?.is_none());
    Ok(())
}

/// bug_015 db-layer companion (DISCLOSED STRAWMAN: the pre-fix
/// signature had no generation input, so this exact call is
/// post-fix-expressible only — the behavioral pre-fix proof is the
/// actor-level red `deposed_replica_cannot_mint_the_exit0_license`,
/// which compiles pre-fix via the injection hook). Witness-strength:
/// certifies the claim's own proposition at the db boundary — a
/// below-floor replica's license write commits NOTHING (`Fenced`
/// outcome; no row for the read to launder).
#[tokio::test]
async fn insert_confirm_fence_below_floor_writes_nothing() -> anyhow::Result<()> {
    let (_pg, db) = setup().await;
    let hash = "d".repeat(64);

    // A successor's durable claim moves the floor to 2.
    db.claim_generation(2, "successor").await?;

    // The deposed replica (generation 1) tries to mint the license.
    let w = db
        .insert_confirm_fence(
            &hash,
            "intent-deposed",
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    match w {
        ConfirmFenceWrite::Fenced { floor } => {
            assert_eq!(floor, 2, "the refusing floor is the successor's claim");
        }
        ConfirmFenceWrite::Durable(_) => {
            panic!("a below-floor write must be Fenced, never Durable")
        }
    }
    assert!(
        db.confirm_fence_exists(&hash).await?.is_none(),
        "nothing written: the fenced transaction rolled back at the door"
    );

    // At the floor, the same write mints the witness (the green half).
    let w = db
        .insert_confirm_fence(
            &hash,
            "intent-current",
            ServingGeneration::stamp_from_claim(2),
        )
        .await?;
    assert!(matches!(w, ConfirmFenceWrite::Durable(_)));
    assert!(db.confirm_fence_exists(&hash).await?.is_some());
    Ok(())
}

/// The TTL rider deletes only rows past the horizon: a zero horizon
/// sweeps the fence, the shipped credential-derived horizon
/// (`CONFIRM_FENCE_GC_SECS` = `MAX_HMAC_LIFETIME_SECS` + slack)
/// keeps a fresh one.
#[tokio::test]
async fn fence_gc_respects_horizon() -> anyhow::Result<()> {
    let (_pg, db) = setup().await;
    let hash = "c".repeat(64);
    let _w = db
        .insert_confirm_fence(&hash, "intent-gc", ServingGeneration::stamp_from_claim(1))
        .await?;

    let kept = db
        .gc_confirm_fences(crate::db::confirm_fences::CONFIRM_FENCE_GC_SECS, 100)
        .await?;
    assert_eq!(
        kept, 0,
        "a fresh fence survives the credential-derived horizon"
    );
    assert!(db.confirm_fence_exists(&hash).await?.is_some());

    let swept = db.gc_confirm_fences(0.0, 100).await?;
    assert_eq!(swept, 1, "a zero horizon sweeps the fence");
    assert!(db.confirm_fence_exists(&hash).await?.is_none());
    Ok(())
}
