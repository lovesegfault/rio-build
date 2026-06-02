//! Recovery-query tests: row-level semantics of the SQL helpers in
//! `db/recovery.rs` that `recover_from_pg` consults. Actor-level
//! failover behavior lives in `actor/tests/recovery.rs`.
//!
//! Also covers the generation floor + claim-ledger queries
//! (`db/recovery.rs`).

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::SchedulerDb;
use crate::state::{BuildOptions, BuildState, ExecutorId, PriorityClass};

// r[verify sched.recovery.failed-dep-cascade+2]
/// `load_parents_with_failed_deps` only counts a terminal-failure child
/// when a LIVE (`pending`/`active`) build that also owns the parent
/// vouches for it: parent P1's cancelled child is co-owned by a
/// still-pending build → P1 is returned (one qualifying child suffices
/// — P1's second cancelled child has no `build_derivations` rows at all
/// and contributes nothing); parent P2's cancelled child is owned only
/// by a build that was itself cancelled (the bug_009 shape: another
/// build's never-wanted child) → P2 is NOT returned, so recovery does
/// not condemn it — or its healthy live build — on dead cross-build
/// evidence.
#[tokio::test]
async fn test_load_parents_with_failed_deps_requires_live_co_owning_voucher() -> anyhow::Result<()>
{
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // P1: cancelled child co-owned (parent + child) by a live build,
    // plus a second cancelled child with no build links at all (its
    // owning build's row was deleted — the FK cascade removes links but
    // migration 028 dropped the derivations FK, so the row survives).
    let p1 = insert_test_derivation(&db, "fdep-live-parent").await?;
    let p1_child = insert_test_derivation(&db, "fdep-live-child").await?;
    let p1_orphan = insert_test_derivation(&db, "fdep-live-orphan").await?;
    // P2: cancelled child owned only by a build that is itself
    // cancelled (terminal) — the bug_009 shape.
    let p2 = insert_test_derivation(&db, "fdep-dead-parent").await?;
    let p2_child = insert_test_derivation(&db, "fdep-dead-child").await?;

    sqlx::query("UPDATE derivations SET status = 'cancelled' WHERE derivation_id = ANY($1)")
        .bind(vec![p1_child, p1_orphan, p2_child])
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO derivation_edges (parent_id, child_id) \
         VALUES ($1, $2), ($3, $4), ($5, $6)",
    )
    .bind(p1)
    .bind(p1_child)
    .bind(p1)
    .bind(p1_orphan)
    .bind(p2)
    .bind(p2_child)
    .execute(&test_db.pool)
    .await?;

    // P1's voucher: live ('pending') and owns BOTH the parent and the
    // failed child.
    let live_owner = Uuid::new_v4();
    db.insert_build(
        live_owner,
        None,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(live_owner, p1).await?;
    db.insert_build_derivation(live_owner, p1_child).await?;

    // P2's voucher owned both rows but has since been cancelled — its
    // child's failure is dead evidence. P2 itself stays owned by a live
    // re-requesting build; that link alone must not revive the child's
    // evidence.
    let dead_owner = Uuid::new_v4();
    db.insert_build(
        dead_owner,
        None,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(dead_owner, p2).await?;
    db.insert_build_derivation(dead_owner, p2_child).await?;
    db.update_build_status(dead_owner, BuildState::Cancelled, None)
        .await?;
    let p2_rerequest = Uuid::new_v4();
    db.insert_build(
        p2_rerequest,
        None,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(p2_rerequest, p2).await?;

    let condemned = db.load_parents_with_failed_deps(&[p1, p2]).await?;
    assert_eq!(
        condemned,
        vec![p1],
        "only the parent whose failed child is vouched for by a live co-owning \
         build may be cascaded to DependencyFailed; another build's dead child \
         must not condemn a recovered parent"
    );
    Ok(())
}

/// `claim_generation`'s PK-conflict-as-CAS contract: the first claim of
/// a generation wins (`true`), a second claim of the SAME generation
/// loses (`false`, zero rows affected via `ON CONFLICT DO NOTHING`),
/// and a claim of the next generation wins again. The retry loop in
/// `handle_leader_acquired` is built on exactly this `bool` — if
/// `ON CONFLICT DO NOTHING` ever became `DO UPDATE`, `rows_affected()`
/// would report 1 on conflict and two leaders would both believe they
/// own the same generation.
// r[verify sched.lease.generation-claim+2]
#[tokio::test]
async fn test_claim_generation_pk_conflict_is_the_cas() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    assert!(
        db.claim_generation(7, "holder-a").await?,
        "first claim of generation 7 wins"
    );
    assert!(
        !db.claim_generation(7, "holder-b").await?,
        "second claim of generation 7 loses (PK conflict)"
    );
    assert!(
        db.claim_generation(8, "holder-b").await?,
        "claim of the next generation wins"
    );

    // The losing claim must NOT have overwritten the winner's row.
    let (holder,): (String,) =
        sqlx::query_as("SELECT holder_id FROM leader_generation_claims WHERE generation = 7")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        holder, "holder-a",
        "the conflict loser must not clobber the winner"
    );

    // The max-row read-back carries the holder so the caller can
    // distinguish "the conflicting row is my own previous claim"
    // (idempotent re-claim) from "another holder owns it" (collide →
    // bump past it).
    assert_eq!(
        db.max_claimed_generation().await?,
        Some((8, "holder-b".to_string()))
    );
    Ok(())
}

/// The seed floor reads `GREATEST(assignments, claims)`: a generation
/// that exists ONLY in the claims ledger (a leader that was deposed
/// before persisting a single assignment, or whose assignment rows
/// have since been cascade-deleted by the orphan sweep) still raises
/// the floor. This is the depose-before-persist scenario the ledger
/// exists for: without the claims arm, `MAX(generation) FROM
/// assignments` is NULL here and the next leader would seed from
/// nothing.
// r[verify sched.lease.generation-claim+2]
#[tokio::test]
async fn test_max_known_generation_covers_unpersisted_claims() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Fresh cluster: no assignments, no claims.
    assert_eq!(db.max_known_generation().await?, None);

    // A leader claimed generation 5 but never dispatched anything.
    db.claim_generation(5, "deposed-before-persist").await?;
    assert_eq!(
        db.max_known_generation().await?,
        Some(5),
        "a claimed-but-never-persisted generation must raise the floor"
    );

    // An assignment at a higher generation dominates.
    let drv_id = insert_test_derivation(&db, "genfloor").await?;
    db.insert_assignment(
        drv_id,
        &ExecutorId::from("worker-1"),
        9,
        uuid::Uuid::now_v7(),
    )
    .await?;
    assert_eq!(
        db.max_known_generation().await?,
        Some(9),
        "GREATEST takes the higher of the two arms"
    );

    Ok(())
}
