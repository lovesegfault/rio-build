//! Recovery-query tests: row-level semantics of the SQL helpers in
//! `db/recovery.rs` that `recover_from_pg` consults. Actor-level
//! failover behavior lives in `actor/tests/recovery.rs`.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::SchedulerDb;
use crate::state::{BuildOptions, BuildState, PriorityClass};

// r[verify sched.merge.substitute-topdown+14]
/// `load_parents_with_all_children_produced` only counts produced
/// children that are vouched for (via `build_derivations`) by a LIVE
/// (`pending`/`active`) build that ALSO owns the parent: parent A's
/// completed child rides a still-pending build that links both A and
/// the child → A is returned (its restored mark may be dropped);
/// parent B's completed child is linked only to a build that has since
/// `succeeded` (the previous-generation shape) → B is NOT returned;
/// parent C's completed child is linked to a live build that never
/// owned C (cross-build evidence — the laundering shape) → C is NOT
/// returned either. B and C keep their restored `topdown_pruned` marks.
#[tokio::test]
async fn test_load_parents_with_all_children_produced_requires_live_build_link()
-> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Parent A / child A: the child completed under a build that is
    // still live ('pending' — insert_build's initial status — is in
    // the live set alongside 'active') and that owns parent A too (the
    // realistic live-flow shape: the build that re-requests the parent
    // submitted the child as part of the same closure).
    let a_id = insert_test_derivation(&db, "rec-live-parent").await?;
    let a_child = insert_test_derivation(&db, "rec-live-child").await?;
    // Parent B / child B: the child completed under a build that has
    // since gone terminal — historical evidence only.
    let b_id = insert_test_derivation(&db, "rec-hist-parent").await?;
    let b_child = insert_test_derivation(&db, "rec-hist-child").await?;
    // Parent C / child C: the child completed under a build that is
    // still live but never owned parent C — cross-build evidence.
    let c_id = insert_test_derivation(&db, "rec-xown-parent").await?;
    let c_child = insert_test_derivation(&db, "rec-xown-child").await?;

    // All parents carry the restored mark; all children are produced.
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE derivation_id = ANY($1)")
        .bind(vec![a_id, b_id, c_id])
        .execute(&test_db.pool)
        .await?;
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE derivation_id = ANY($1)")
        .bind(vec![a_child, b_child, c_child])
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO derivation_edges (parent_id, child_id) \
         VALUES ($1, $2), ($3, $4), ($5, $6)",
    )
    .bind(a_id)
    .bind(a_child)
    .bind(b_id)
    .bind(b_child)
    .bind(c_id)
    .bind(c_child)
    .execute(&test_db.pool)
    .await?;

    // Child A's build stays live ('pending') and co-owns parent A.
    let live_build = Uuid::new_v4();
    db.insert_build(
        live_build,
        None,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(live_build, a_id).await?;
    db.insert_build_derivation(live_build, a_child).await?;

    // Child B's only build goes terminal. Parent B itself stays linked
    // to a live build (the re-requesting one) — that link must NOT
    // count: the voucher is per CHILD, and it must be the same build
    // that owns the parent.
    let hist_build = Uuid::new_v4();
    db.insert_build(
        hist_build,
        None,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(hist_build, b_child).await?;
    db.update_build_status(hist_build, BuildState::Succeeded, None)
        .await?;
    let rerequest_build = Uuid::new_v4();
    db.insert_build(
        rerequest_build,
        None,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(rerequest_build, b_id).await?;

    // Child C's build is live but never owned parent C; parent C is
    // owned by a different live build (the re-requesting one) that
    // never owned the child. Live parent owner + live child voucher
    // must not combine across builds.
    let noncoowner_build = Uuid::new_v4();
    db.insert_build(
        noncoowner_build,
        None,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    db.insert_build_derivation(noncoowner_build, c_child)
        .await?;
    db.insert_build_derivation(rerequest_build, c_id).await?;

    let cleared = db
        .load_parents_with_all_children_produced(&[a_id, b_id, c_id])
        .await?;
    assert_eq!(
        cleared,
        vec![a_id],
        "only the parent whose produced child is vouched for by a live build \
         that also owns the parent may be returned; evidence owned solely by \
         terminal builds or by builds that never owned the parent keeps the mark"
    );
    Ok(())
}

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

// r[verify sched.merge.substitute-topdown+14]
/// `load_parents_with_unproduced_terminal_children` returns exactly the
/// recovered parents with ≥1 persisted child in a non-produced terminal
/// status (`cancelled`/`dependency_failed`/`poisoned`) — the children
/// whose edges recovery drops without the child having been produced,
/// i.e. the parents whose recovered child set is silently truncated and
/// must carry the closure-hole breadcrumb. Produced (`completed`) and
/// still-unbuilt (`queued`) children do not qualify on their own, and —
/// unlike the failed-dep cascade — no build-liveness/co-ownership
/// evidence is required: the truncation is recorded regardless of which
/// build demanded the dropped child (parent A here has no build links at
/// all and is still returned). A `'failed'` child does not qualify
/// either (parent C): `failed` is a transient retry status, not
/// terminal — recovery loads such a child and keeps its edge, so there
/// is no truncation to record; the exclusion is deliberate and pinned
/// here so it cannot be silently widened later.
#[tokio::test]
async fn test_load_parents_with_unproduced_terminal_children_ignores_produced_and_unbuilt()
-> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Parent A: one cancelled child (qualifies) plus one completed and
    // one queued child (do not qualify on their own).
    let a = insert_test_derivation(&db, "uptc-holed-parent").await?;
    let a_cancelled = insert_test_derivation(&db, "uptc-cancelled-child").await?;
    let a_completed = insert_test_derivation(&db, "uptc-completed-child").await?;
    let a_queued = insert_test_derivation(&db, "uptc-queued-child").await?;
    // Parent B: children are produced or unbuilt only — never returned.
    let b = insert_test_derivation(&db, "uptc-clean-parent").await?;
    let b_completed = insert_test_derivation(&db, "uptc-clean-completed").await?;
    let b_queued = insert_test_derivation(&db, "uptc-clean-queued").await?;
    // Parent C: its only terminal-looking child is 'failed' (transient
    // retry, non-terminal, edge kept at recovery) — never returned.
    let c = insert_test_derivation(&db, "uptc-retry-parent").await?;
    let c_failed = insert_test_derivation(&db, "uptc-retry-failed-child").await?;

    sqlx::query("UPDATE derivations SET status = 'cancelled' WHERE derivation_id = $1")
        .bind(a_cancelled)
        .execute(&test_db.pool)
        .await?;
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE derivation_id = ANY($1)")
        .bind(vec![a_completed, b_completed])
        .execute(&test_db.pool)
        .await?;
    sqlx::query("UPDATE derivations SET status = 'queued' WHERE derivation_id = ANY($1)")
        .bind(vec![a_queued, b_queued])
        .execute(&test_db.pool)
        .await?;
    sqlx::query("UPDATE derivations SET status = 'failed' WHERE derivation_id = $1")
        .bind(c_failed)
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO derivation_edges (parent_id, child_id) \
         VALUES ($1, $2), ($3, $4), ($5, $6), ($7, $8), ($9, $10), ($11, $12)",
    )
    .bind(a)
    .bind(a_cancelled)
    .bind(a)
    .bind(a_completed)
    .bind(a)
    .bind(a_queued)
    .bind(b)
    .bind(b_completed)
    .bind(b)
    .bind(b_queued)
    .bind(c)
    .bind(c_failed)
    .execute(&test_db.pool)
    .await?;

    let holed = db
        .load_parents_with_unproduced_terminal_children(&[a, b, c])
        .await?;
    let holed_parents: Vec<uuid::Uuid> = holed.iter().map(|(p, _)| *p).collect();
    assert_eq!(
        holed_parents,
        vec![a],
        "only parents with ≥1 non-produced terminal child are returned; produced \
         and unbuilt children alone never mark a truncation"
    );
    assert_eq!(
        holed.len(),
        1,
        "exactly one (parent, missing-child) pair: the cancelled child"
    );
    Ok(())
}
