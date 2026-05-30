//! End-to-end claims-floor fencing of a deposed actor's evidence
//! writes (`sched.evidence.durability`).
//!
//! The db-layer fence tests (db/tests/batch.rs, db/tests/derivations.rs)
//! pin each fenced statement in isolation; the test here pins the
//! actor-level composition: a real actor, deposed by a successor's
//! claim it never observes, drives a real evidence-writing command and
//! the whole write is a fenced no-op.

use super::*;

/// A deposed actor's evidence writes are fenced no-ops END-TO-END: a
/// successor claims a higher generation while the tenure-1 actor keeps
/// running (it never observes a lease transition — the deposed-believer
/// shape); the deposed actor's next evidence write — the admin
/// ClearPoison reset transaction here (ledger row + poison clear) —
/// must be refused by the claims-floor fence, leave PG exactly as the
/// successor's view (the poison survives), and increment
/// `rio_scheduler_evidence_write_fenced_total`.
///
/// What this test does NOT pin: the capture-point semantics. This
/// actor's lease never re-acquires, so a tenure-tracking field and a
/// fresh per-command atomic read are indistinguishable here. The
/// capture design is pinned structurally instead:
///
///  - the saturated-floor recovery test
///    (`saturated_floor_recovery_evidence_writes_land`) proves a new
///    leader's own recovery writes pass the fence (they would not,
///    under a per-command lease-atomic capture, in the saturated
///    regime);
///  - the field has exactly TWO write sites and zero per-command ones —
///    the structural grep
///    `grep -rEn "self\.serving_generation = |serving_generation: i64::try_from" rio-scheduler/src/actor/`
///    returns exactly DagActor::new's struct-literal init and
///    handle_leader_acquired's claim stamp.
// r[verify sched.evidence.durability]
#[tokio::test]
async fn deposed_actor_evidence_writes_are_fenced() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Tenure 1's own work lands normally: merge + poison a node (the
    // floor at this point is tenure 1's own assignment row).
    seed_poisoned(&handle, "fence-e2e").await?;
    let (status_before,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'fence-e2e'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status_before, "poisoned",
        "precondition: tenure 1's own poison evidence landed (it is at the floor)"
    );
    let fenced_before = handle.debug_counters().await?.evidence_writes_fenced;
    assert_eq!(
        fenced_before, 0,
        "precondition: nothing is fenced during normal single-leader operation \
         (a nonzero count here means the fence rejects a live leader's writes — \
         the permissiveness violation the saturated-regime tripwire also guards)"
    );

    // A successor claims generation 2: the durable floor moves above
    // this actor's tenure stamp of 1, BETWEEN two of this actor's
    // commands. The actor itself never observes any lease transition.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'successor')",
    )
    .execute(&db.pool)
    .await?;

    // The deposed believer's evidence write: admin ClearPoison drives
    // the poison-clear reset transaction. The fence must refuse it —
    // cleared=false is the admin contract for "nothing was cleared;
    // retry-safe".
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ClearPoison {
            drv_hash: "fence-e2e".into(),
            reply: tx,
        })
        .await?;
    assert!(
        !rx.await?,
        "a deposed actor's ClearPoison must report cleared=false (the write was fenced)"
    );

    // PG kept the successor's view: the poison survives the stale clear.
    let (status, has_ts): (String, bool) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash = 'fence-e2e'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        status, "poisoned",
        "the deposed actor's fenced clear must not erase the poison evidence"
    );
    assert!(has_ts, "poisoned_at must survive the fenced clear");

    // The fence observable moved: the test-counter mirror of
    // rio_scheduler_evidence_write_fenced_total.
    let fenced_after = handle.debug_counters().await?.evidence_writes_fenced;
    assert!(
        fenced_after > fenced_before,
        "rio_scheduler_evidence_write_fenced_total must increment on a fenced write \
         (before={fenced_before}, after={fenced_after})"
    );

    // The deposed actor's in-memory state still has the node Poisoned:
    // handle_clear_poison's PG-first contract means a fenced (= failed)
    // clear leaves the in-memory state untouched too — consistent with
    // the PG view the successor owns.
    assert_eq!(
        handle
            .debug_query_derivation("fence-e2e")
            .await?
            .expect("the node must still be in the deposed actor's DAG")
            .status,
        DerivationStatus::Poisoned,
        "a fenced clear must not remove the node from the deposed actor's DAG"
    );
    Ok(())
}

/// The fence never rejects a live single leader: drive the same
/// ClearPoison shape with NO successor claim and assert it applies and
/// the fenced counter stays at zero. This is the permissiveness
/// companion to the deposed-actor test above (the same property the
/// recovery batteries pin at scale — stop-and-report condition 2 if it
/// ever regresses).
// r[verify sched.evidence.durability]
#[tokio::test]
async fn live_leader_evidence_writes_are_never_fenced() -> TestResult {
    let (db, handle, _task) = setup().await;

    seed_poisoned(&handle, "fence-live").await?;

    // No successor: the floor is this tenure's own assignment row.
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ClearPoison {
            drv_hash: "fence-live".into(),
            reply: tx,
        })
        .await?;
    assert!(
        rx.await?,
        "a live leader's ClearPoison must apply (cleared=true)"
    );

    let (status, has_ts): (String, bool) = sqlx::query_as(
        "SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash = 'fence-live'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        status, "created",
        "the live leader's clear must apply in PG"
    );
    assert!(
        !has_ts,
        "poisoned_at must be cleared by the live leader's clear"
    );

    assert_eq!(
        handle.debug_counters().await?.evidence_writes_fenced,
        0,
        "the fence must never reject a live single leader's writes"
    );
    Ok(())
}
