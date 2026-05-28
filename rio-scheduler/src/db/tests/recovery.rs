//! Recovery-query tests: row-level semantics of the SQL helpers in
//! `db/recovery.rs` that `recover_from_pg` consults. Actor-level
//! failover behavior lives in `actor/tests/recovery.rs`.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::SchedulerDb;
use crate::state::{BuildOptions, BuildState, PriorityClass};

// r[verify sched.merge.substitute-topdown+10]
/// `load_parents_with_all_children_produced` only counts produced
/// children that are linked (via `build_derivations`) to a LIVE
/// (`pending`/`active`) build: parent A's completed child rides a
/// still-pending build → A is returned (its restored mark may be
/// dropped); parent B's completed child is linked only to a build that
/// has since `succeeded` (the previous-generation shape) → B is NOT
/// returned, so its restored `topdown_pruned` mark is kept.
#[tokio::test]
async fn test_load_parents_with_all_children_produced_requires_live_build_link()
-> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Parent A / child A: the child completed under a build that is
    // still live ('pending' — insert_build's initial status — is in
    // the live set alongside 'active').
    let a_id = insert_test_derivation(&db, "rec-live-parent").await?;
    let a_child = insert_test_derivation(&db, "rec-live-child").await?;
    // Parent B / child B: the child completed under a build that has
    // since gone terminal — historical evidence only.
    let b_id = insert_test_derivation(&db, "rec-hist-parent").await?;
    let b_child = insert_test_derivation(&db, "rec-hist-child").await?;

    // Both parents carry the restored mark; both children are produced.
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE derivation_id = ANY($1)")
        .bind(vec![a_id, b_id])
        .execute(&test_db.pool)
        .await?;
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE derivation_id = ANY($1)")
        .bind(vec![a_child, b_child])
        .execute(&test_db.pool)
        .await?;
    sqlx::query("INSERT INTO derivation_edges (parent_id, child_id) VALUES ($1, $2), ($3, $4)")
        .bind(a_id)
        .bind(a_child)
        .bind(b_id)
        .bind(b_child)
        .execute(&test_db.pool)
        .await?;

    // Child A's build stays live ('pending').
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
    db.insert_build_derivation(live_build, a_child).await?;

    // Child B's only build goes terminal. Parent B itself stays linked
    // to a live build (the re-requesting one) — that link must NOT
    // count: the liveness evidence is per CHILD, not per parent.
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

    let cleared = db
        .load_parents_with_all_children_produced(&[a_id, b_id])
        .await?;
    assert_eq!(
        cleared,
        vec![a_id],
        "only the parent whose produced child is linked to a live build may be \
         returned; produced evidence owned solely by terminal builds keeps the mark"
    );
    Ok(())
}
