//! Open attempt view tests (the OA5 / busy-bridge read).

use crate::db::ServingGeneration;
use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::{AssignmentStatus, SchedulerDb};
use crate::state::ExecutorId;

/// Insert a `drv_executions` row the way the *pull transaction* writes
/// it: the source-node binding when known. Direct SQL on purpose — the
/// production writer lands with the `PullAssignment` handler; this
/// fixture pins the row shape the view must read.
async fn insert_pull_execution(
    pool: &sqlx::PgPool,
    exec_id: Uuid,
    drv_hash32: &str,
    executor_id: &str,
    source_node: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO drv_executions \
             (exec_id, drv_hash, executor_id, started_at, source_node) \
         VALUES ($1, $2, $3, now(), $4)",
    )
    .bind(exec_id)
    .bind(drv_hash32)
    .bind(executor_id)
    .bind(source_node)
    .execute(pool)
    .await?;
    Ok(())
}

/// 32-char drv_hash for the `drv_executions.drv_hash` CHAR(32) column
/// (the log-hash form, not the DAG key).
fn log_hash(tag: &str) -> String {
    format!("{tag:0>32}")
}

// r[verify sched.admin.list-open-attempts+4]
/// The open-attempt view returns exactly the open attempt — not the
/// terminal-filled one, and not an assignment that never got its
/// execution row (the join requires the pull-minted pair).
#[tokio::test]
async fn open_attempt_view_filters_terminal_and_unminted_rows() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // (1) Open attempt: active assignment + execution row.
    let open_drv = insert_test_derivation(&db, "oa-open").await?;
    let open_exec = Uuid::now_v7();
    db.insert_assignment(open_drv, &ExecutorId::from("oa-open"), 7, open_exec)
        .await?;
    insert_pull_execution(
        &test_db.pool,
        open_exec,
        &log_hash("oaopen"),
        "oa-open",
        Some("node-1"),
    )
    .await?;

    // (2) Terminal-filled attempt: assignment closed AND the attempt
    // row carries a terminal fill — excluded on both counts.
    let term_drv = insert_test_derivation(&db, "oa-term").await?;
    let term_exec = Uuid::now_v7();
    db.insert_assignment(term_drv, &ExecutorId::from("oa-term"), 7, term_exec)
        .await?;
    insert_pull_execution(
        &test_db.pool,
        term_exec,
        &log_hash("oaterm"),
        "oa-term",
        None,
    )
    .await?;
    db.update_assignment_status(term_drv, AssignmentStatus::Completed)
        .await?;
    sqlx::query(
        "INSERT INTO drv_attempts \
             (attempt_id, derivation_id, exec_id, executor_id, event_kind, outcome_class, \
              termination_reason, reporting_party, occurred_at) \
         VALUES ($1, $2, $3, 'oa-term', 'attempt', 'executor_crash', 'unreported', \
                 'scheduler', now())",
    )
    .bind(Uuid::now_v7())
    .bind(term_drv)
    .bind(term_exec)
    .execute(&test_db.pool)
    .await?;

    // (3) Assignment with NO execution row: the pull mint writes the
    // pair in one transaction, so a lone assignment row (a test
    // shortcut, or a partial write that never committed) is not an
    // attempt and must not appear in the view.
    let bare_drv = insert_test_derivation(&db, "oa-bare").await?;
    db.insert_assignment(bare_drv, &ExecutorId::from("oa-bare"), 7, Uuid::now_v7())
        .await?;

    let rows = db.list_open_pull_attempts().await?.build;
    assert_eq!(
        rows.len(),
        1,
        "exactly the open attempt is listed, got {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(row.derivation_id, open_drv);
    assert_eq!(row.drv_hash, "oa-open");
    assert_eq!(
        row.drv_path,
        rio_test_support::fixtures::test_drv_path("oa-open")
    );
    assert_eq!(row.exec_id, open_exec);
    assert_eq!(row.executor_id, "oa-open");
    assert_eq!(row.system, "x86_64-linux");
    assert!(!row.is_fixed_output, "test derivation defaults to non-FOD");
    assert_eq!(row.source_node.as_deref(), Some("node-1"));
    assert_eq!(row.generation, 7);
    assert!(
        row.assigned_at_epoch_secs > 0.0,
        "assigned_at must be populated"
    );
    assert!(
        row.age_secs >= 0.0 && row.age_secs < 600.0,
        "age must be small for a row written moments ago, got {}",
        row.age_secs
    );
    Ok(())
}

/// An attempt whose drv_attempts row got a terminal fill while its
/// assignment row is still active is NOT open (terminality is decided
/// by the fill, not only by the assignment status).
#[tokio::test]
async fn open_attempt_view_respects_terminal_fill_alone() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv = insert_test_derivation(&db, "oa-filled").await?;
    let exec = Uuid::now_v7();
    db.insert_assignment(drv, &ExecutorId::from("oa-filled"), 3, exec)
        .await?;
    insert_pull_execution(
        &test_db.pool,
        exec,
        &log_hash("oafilled"),
        "oa-filled",
        None,
    )
    .await?;
    // Terminal fill lands (e.g. establishment) but the assignment row
    // close is still in flight.
    sqlx::query(
        "INSERT INTO drv_attempts \
             (attempt_id, derivation_id, exec_id, executor_id, event_kind, outcome_class, \
              termination_reason, reporting_party, occurred_at) \
         VALUES ($1, $2, $3, 'oa-filled', 'attempt', 'executor_crash', 'unreported', \
                 'scheduler', now())",
    )
    .bind(Uuid::now_v7())
    .bind(drv)
    .bind(exec)
    .execute(&test_db.pool)
    .await?;

    let rows = db.list_open_pull_attempts().await?;
    assert!(
        rows.build.is_empty() && rows.materialization.is_empty(),
        "terminal-filled attempt must not be listed as open, got {rows:?}"
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+6]
/// The dispatched deadline persisted by the fenced mint round-trips
/// through the open-attempt view (the establishment window's anchor).
#[tokio::test]
async fn mint_persists_dispatched_deadline_and_view_returns_it() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv = insert_test_derivation(&db, "oa-deadline").await?;
    let exec = Uuid::now_v7();
    let committed = db
        .mint_pull_attempt_fenced(
            drv,
            &ExecutorId::from("oa-deadline"),
            ServingGeneration::stamp_from_claim(1),
            exec,
            &log_hash("oadeadline"),
            Some("node-1"),
            Some(1234.0),
            // Mechanical flag-off default (carve-out 1c): build kind.
            crate::state::AttemptKind::Build,
            None,
        )
        .await?;
    assert!(
        committed.settled(),
        "the fenced mint commits on a fresh cluster"
    );

    let rows = db.list_open_pull_attempts().await?.build;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].exec_id, exec);
    assert_eq!(
        rows[0].deadline_secs,
        Some(1234.0),
        "the dispatched deadline round-trips through the view"
    );
    assert_eq!(rows[0].source_node.as_deref(), Some("node-1"));

    // C2/008: the wire mapping carries the dispatched deadline and the
    // work class — pinned against this REALLY-MINTED row so the
    // consumer-fixture vacuity (wedge tests inventing deadline_secs the
    // producer never sent) cannot recur.
    let proto = crate::admin::open_attempt_row_to_proto(rows.into_iter().next().unwrap());
    assert_eq!(
        proto.deadline_secs, 1234,
        "ListOpenAttempts must carry the dispatched deadline (the OA2 wedge consumer skips 0)"
    );
    assert_eq!(
        proto.attempt_kind,
        rio_proto::types::AttemptKind::Build as i32
    );
    Ok(())
}

/// Coordinator-flagged absence-as-verdict sibling (the B2 security
/// sweep, wave-log §B2): `find_attempt_by_exec_id` must REQUIRE the
/// execution lifecycle row. Pre-fix the LEFT JOIN +
/// `COALESCE(attempt_kind,'build')` defaulted a row-less assignment to
/// the BUILD lane — an attempt whose kind is unknowable reached the
/// build close/completion arms. Deny-by-default: no execution row ⇒ no
/// resolution ⇒ the report intake's existing acknowledged-and-ignored
/// (superseded) posture.
#[tokio::test]
async fn attempt_resolution_requires_the_execution_row() -> anyhow::Result<()> {
    let test_db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let drv = super::insert_test_derivation(&db, "rowless-exec-hash").await?;
    let exec_id = uuid::Uuid::now_v7();
    // An assignments row WITHOUT its drv_executions row (the schema
    // allows it: no FK binds assignments.exec_id to drv_executions).
    db.insert_assignment(
        drv,
        &crate::state::ExecutorId::from("rowless-exec-hash"),
        1,
        exec_id,
    )
    .await?;

    let resolved = db.find_attempt_by_exec_id(exec_id).await?;
    assert!(
        resolved.is_none(),
        "a row-less assignment must not resolve to any kind (got {resolved:?})"
    );
    drop(test_db);
    Ok(())
}

// r[verify ctrl.job.cancel-close-cause+2]
// r[verify sched.admin.list-open-attempts+4]
/// C2/120: a terminal close inside the window travels on
/// `recently_closed` WITH its cause — the controller's cancel arm
/// selects on `CLOSE_CAUSE_CANCELLED`, never on the absence of an
/// open row. An old close (beyond the 120 s window) is excluded.
#[tokio::test]
async fn recently_closed_window_carries_the_close_cause() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // A cancelled close moments ago.
    let drv = insert_test_derivation(&db, "rc-cancelled").await?;
    let exec = Uuid::now_v7();
    db.insert_assignment(drv, &ExecutorId::from("rc-cancelled"), 1, exec)
        .await?;
    insert_pull_execution(
        &test_db.pool,
        exec,
        &log_hash("rccancel"),
        "rc-cancelled",
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE assignments SET status = 'cancelled', completed_at = now() \
         WHERE exec_id = $1",
    )
    .bind(exec)
    .execute(&test_db.pool)
    .await?;

    // A completed close that aged OUT of the window.
    let old_drv = insert_test_derivation(&db, "rc-old").await?;
    let old_exec = Uuid::now_v7();
    db.insert_assignment(old_drv, &ExecutorId::from("rc-old"), 1, old_exec)
        .await?;
    sqlx::query(
        "UPDATE assignments \
         SET status = 'completed', completed_at = now() - interval '10 minutes' \
         WHERE exec_id = $1",
    )
    .bind(old_exec)
    .execute(&test_db.pool)
    .await?;

    let closed = db.list_recently_closed_pull_attempts().await?;
    assert_eq!(closed.len(), 1, "only the in-window close is served");
    assert_eq!(closed[0].exec_id, exec);
    assert_eq!(closed[0].status, "cancelled");
    assert!(closed[0].closed_age_secs < 60.0);

    // The wire mapping: cause + intent id (drv hash), pinned against
    // the really-closed row.
    let proto = crate::admin::closed_attempt_row_to_proto(closed.into_iter().next().unwrap());
    assert_eq!(
        proto.cause,
        rio_proto::types::CloseCause::Cancelled as i32,
        "the close cause travels with the close"
    );
    assert!(!proto.intent_id.is_empty());
    Ok(())
}

/// bug_113 red (scheduler half): the recently-closed window is the
/// controller cancel arm's evidence feed — a MATERIALIZATION close
/// (store-side fetch) for a drv whose builder Job is active must not
/// appear in it, or the cancel arm tears down the builder Job.
#[tokio::test]
async fn recently_closed_window_is_build_lane_only() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // A cancelled MATERIALIZATION close moments ago.
    let drv = insert_test_derivation(&db, "rc-mat").await?;
    let exec = Uuid::now_v7();
    db.insert_assignment(drv, &ExecutorId::from("rc-mat"), 1, exec)
        .await?;
    sqlx::query(
        "INSERT INTO drv_executions \
             (exec_id, drv_hash, executor_id, started_at, attempt_kind) \
         VALUES ($1, $2, $3, now(), 'materialization')",
    )
    .bind(exec)
    .bind(log_hash("rcmat"))
    .bind("rc-mat")
    .execute(&test_db.pool)
    .await?;
    sqlx::query(
        "UPDATE assignments SET status = 'cancelled', completed_at = now() \
         WHERE exec_id = $1",
    )
    .bind(exec)
    .execute(&test_db.pool)
    .await?;

    // An assignment with NO execution row at all (kind unknowable):
    // deny-by-default, same absence-as-verdict law as
    // `attempt_resolution_requires_the_execution_row`.
    let rowless = insert_test_derivation(&db, "rc-rowless").await?;
    let rowless_exec = Uuid::now_v7();
    db.insert_assignment(rowless, &ExecutorId::from("rc-rowless"), 1, rowless_exec)
        .await?;
    sqlx::query(
        "UPDATE assignments SET status = 'cancelled', completed_at = now() \
         WHERE exec_id = $1",
    )
    .bind(rowless_exec)
    .execute(&test_db.pool)
    .await?;

    let closed = db.list_recently_closed_pull_attempts().await?;
    assert!(
        closed.is_empty(),
        "non-build closes must not feed the cancel arm: {closed:?}"
    );
    Ok(())
}

/// bug_184 + merged_bug_108: the typed skew-predicate view. The
/// backed check is (drv, holder) pair-keyed — a foreign executor's
/// open attempt backs NOTHING for another holder — and the wedge
/// conjunction is kind-aware: an open BUILD attempt on the drv
/// (documented-legitimate coexistence, bug_266) defeats the wedge.
#[test]
fn open_attempt_view_is_pair_keyed_and_kind_aware() {
    use crate::db::open_attempts::{OpenAttemptRow, OpenAttemptsByKind};
    fn row(drv: &str, executor: &str, kind: &str) -> OpenAttemptRow {
        use crate::db::open_attempts::OpenAttemptRow;
        OpenAttemptRow {
            derivation_id: Uuid::now_v7(),
            drv_hash: drv.into(),
            drv_path: format!("/nix/store/{drv}.drv"),
            exec_id: Uuid::now_v7(),
            executor_id: executor.into(),
            system: "x86_64-linux".into(),
            is_fixed_output: false,
            source_node: None,
            generation: 1,
            assigned_at_epoch_secs: 0.0,
            age_secs: 0.0,
            deadline_secs: Some(600.0),
            attempt_kind: kind.into(),
        }
    }
    let opens = OpenAttemptsByKind {
        build: vec![row("drv-coexist", "builder-0", "build")],
        materialization: vec![row("drv-claimed", "store-a", "materialization")],
    };
    let view = opens.view();

    // Pair-keyed backing (bug_184): only the holder's own attempt backs.
    assert!(view.backs_claim("drv-claimed", "store-a"));
    assert!(
        !view.backs_claim("drv-claimed", "store-b"),
        "a FOREIGN open attempt must not back another holder's claim"
    );

    // Kind-aware wedge conjunction (merged_bug_108), in the exact
    // form the sweep evaluates: dispatched && !mat && !build.
    let wedged = |drv: &str| !view.materialization_open(drv) && !view.build_open(drv);
    assert!(
        !wedged("drv-coexist"),
        "an open BUILD attempt is legitimate coexistence, never a wedge"
    );
    assert!(!wedged("drv-claimed"), "an open MAT attempt is not a wedge");
    assert!(
        wedged("drv-orphan"),
        "no attempt of either kind = wedge candidate"
    );
}

/// sh-007c S6 row-set parity: `find_attempts_by_exec_ids` over a set
/// returns the same `AttemptRef` per key as the singleton, and absent
/// keys (no assignment / no execution row) are simply not in the map.
#[tokio::test]
async fn find_attempts_by_exec_ids_parity_with_singleton() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Two attempts of distinct kinds, plus one unknown exec.
    let drv_b = insert_test_derivation(&db, "batch-find-build").await?;
    let exec_b = Uuid::now_v7();
    db.insert_assignment(drv_b, &ExecutorId::from("batch-find-build"), 1, exec_b)
        .await?;
    insert_pull_execution(&test_db.pool, exec_b, &log_hash("b"), "builder-0", None).await?;

    let drv_m = insert_test_derivation(&db, "batch-find-mat").await?;
    let exec_m = Uuid::now_v7();
    db.insert_assignment(drv_m, &ExecutorId::from("batch-find-mat"), 1, exec_m)
        .await?;
    sqlx::query(
        "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at, attempt_kind) \
         VALUES ($1, $2, 'store-0', now(), 'materialization')",
    )
    .bind(exec_m)
    .bind(log_hash("m"))
    .execute(&test_db.pool)
    .await?;

    let unknown = Uuid::now_v7();
    let batch = db
        .find_attempts_by_exec_ids(&[exec_b, exec_m, unknown])
        .await?;

    assert_eq!(batch.len(), 2, "absent keys are not in the map");
    let single_b = db.find_attempt_by_exec_id(exec_b).await?.unwrap();
    let single_m = db.find_attempt_by_exec_id(exec_m).await?.unwrap();
    assert!(matches!(
        &batch[&exec_b],
        crate::db::open_attempts::AttemptRef::Build(b)
            if b.core.derivation_id == single_b.core().derivation_id
    ));
    assert!(matches!(
        &batch[&exec_m],
        crate::db::open_attempts::AttemptRef::Materialization(m)
            if m.core.derivation_id == single_m.core().derivation_id
                && m.core.assignment_active
    ));
    drop(test_db);
    Ok(())
}
