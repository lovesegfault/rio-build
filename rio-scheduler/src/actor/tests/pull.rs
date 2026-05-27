//! Pull-mode dispatch: the `PullAssignment` red-first battery
//! (idempotency, Gone/NotYetReady semantics, the generation fence, the
//! token binding, coexistence with stream-dispatched attempts).

use super::*;
use crate::actor::pull::{PullOutcome, PullRejection};

/// Send one `PullAssignment` through the actor and return the reply.
async fn pull(
    handle: &ActorHandle,
    intent_id: &str,
    auth_intent: Option<&str>,
) -> Result<PullOutcome, PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: intent_id.into(),
            auth_intent: auth_intent.map(Into::into),
            reply,
        })
        .await
        .expect("actor alive")
}

/// Unwrap a Deliver outcome.
fn expect_deliver(outcome: Result<PullOutcome, PullRejection>) -> rio_proto::types::WorkAssignment {
    match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("expected Deliver, got {other:?}"),
    }
}

/// (assignments rows, drv_executions rows) for one drv hash.
async fn row_counts(pool: &sqlx::PgPool, drv_hash: &str) -> (i64, i64) {
    let assignments: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = $1",
    )
    .bind(drv_hash)
    .fetch_one(pool)
    .await
    .expect("assignments count");
    let executions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM drv_executions e \
         WHERE e.exec_id IN ( \
             SELECT a.exec_id FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             WHERE d.drv_hash = $1 AND a.exec_id IS NOT NULL)",
    )
    .bind(drv_hash)
    .fetch_one(pool)
    .await
    .expect("executions count");
    (assignments, executions)
}

async fn dispatch_mode_of(pool: &sqlx::PgPool, exec_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT dispatch_mode FROM drv_executions WHERE exec_id = $1")
        .bind(exec_id)
        .fetch_one(pool)
        .await
        .expect("dispatch_mode")
}

// r[verify sched.executor.pull-transaction]
/// (a) Double pull returns the identical payload and exec_id and mints
/// exactly one assignments + drv_executions row pair; (g) the minted
/// execution row carries dispatch_mode = 'pull'.
#[tokio::test]
async fn pull_double_pull_is_idempotent() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "pull-a", PriorityClass::Scheduled).await?;

    let first = expect_deliver(pull(&handle, "pull-a", Some("pull-a")).await);
    let second = expect_deliver(pull(&handle, "pull-a", Some("pull-a")).await);

    assert_eq!(
        first.exec_id, second.exec_id,
        "re-pull returns the same exec_id"
    );
    assert!(!first.exec_id.is_empty(), "exec_id must be minted");
    // Identity fields are identical on re-pull (traceparent is
    // observability and excluded from the comparison).
    assert_eq!(first.drv_path, second.drv_path);
    assert_eq!(first.drv_content, second.drv_content);
    assert_eq!(first.output_names, second.output_names);
    assert_eq!(first.assignment_token, second.assignment_token);
    assert_eq!(first.generation, second.generation);
    assert_eq!(first.is_fixed_output, second.is_fixed_output);

    let (assignments, executions) = row_counts(&db.pool, "pull-a").await;
    assert_eq!(assignments, 1, "exactly one assignments row");
    assert_eq!(executions, 1, "exactly one drv_executions row");
    let exec_id: uuid::Uuid = first.exec_id.parse().expect("exec_id is a uuid");
    assert_eq!(dispatch_mode_of(&db.pool, exec_id).await, "pull");

    // The drv is Running and bound to the intent identity.
    let info = expect_drv(&handle, "pull-a").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Running);
    Ok(())
}

// r[verify sched.executor.pull-gone]
/// (b) A pull for a completed drv returns Gone and writes nothing new.
#[tokio::test]
async fn pull_completed_drv_returns_gone() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("w-gone", "x86_64-linux").await?;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "pull-b", PriorityClass::Scheduled).await?;
    let assignment = recv_assignment(&mut rx).await;
    complete_success(
        &handle,
        "w-gone",
        &assignment.drv_path,
        &rio_test_support::fixtures::test_store_path("pull-b-out"),
    )
    .await?;
    barrier(&handle).await;

    let before = row_counts(&db.pool, "pull-b").await;
    let outcome = pull(&handle, "pull-b", Some("pull-b")).await;
    assert!(
        matches!(outcome, Ok(PullOutcome::Gone)),
        "completed drv must answer Gone, got {outcome:?}"
    );
    let after = row_counts(&db.pool, "pull-b").await;
    assert_eq!(before, after, "Gone writes nothing");

    // A drv that was never submitted answers Gone too.
    let outcome = pull(&handle, "never-submitted", Some("never-submitted")).await;
    assert!(matches!(outcome, Ok(PullOutcome::Gone)));
    Ok(())
}

// r[verify sched.executor.pull-not-ready]
/// (c) A pull for a wanted-but-not-Ready drv answers
/// NotYetReady{retry_after} and writes nothing (the OA6 consequence).
#[tokio::test]
async fn pull_unbuilt_deps_returns_not_yet_ready() -> TestResult {
    let (db, handle, _task) = setup().await;
    let child = make_node("pull-c-child");
    let parent = make_node("pull-c-parent");
    let edge = make_test_edge("pull-c-parent", "pull-c-child");
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![edge],
        false,
    )
    .await?;

    let outcome = pull(&handle, "pull-c-parent", Some("pull-c-parent")).await;
    match outcome {
        Ok(PullOutcome::NotYetReady { retry_after_secs }) => {
            assert_eq!(retry_after_secs, 5, "the plan-owned retry_after default");
        }
        other => panic!("expected NotYetReady for unbuilt deps, got {other:?}"),
    }
    let (assignments, executions) = row_counts(&db.pool, "pull-c-parent").await;
    assert_eq!(
        (assignments, executions),
        (0, 0),
        "NotYetReady writes nothing"
    );
    Ok(())
}

// r[verify sched.executor.pull-transaction]
/// (d) The generation fence: a pull whose serving generation is below
/// the durable claims floor creates no row and is rejected with the
/// retryable not-leader class; a claim at N+1 means a server at N can
/// never mint an open attempt.
#[tokio::test]
async fn pull_below_floor_is_rejected_and_writes_nothing() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "pull-d", PriorityClass::Scheduled).await?;

    // A successor's claim lands (generation 2 > the test actor's
    // serving generation 1).
    sqlx::query("INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'next')")
        .execute(&db.pool)
        .await?;

    let outcome = pull(&handle, "pull-d", Some("pull-d")).await;
    assert_eq!(
        outcome.expect_err("below-floor pull must be rejected"),
        PullRejection::StaleGeneration
    );
    let (assignments, executions) = row_counts(&db.pool, "pull-d").await;
    assert_eq!(
        (assignments, executions),
        (0, 0),
        "fence aborts before any write"
    );
    // The drv is untouched (still Ready, no executor bound).
    let info = expect_drv(&handle, "pull-d").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Ready);
    Ok(())
}

// r[verify sched.executor.pull-transaction]
/// (e) Token↔intent mismatch is rejected per sec.executor.identity-token
/// and writes nothing.
#[tokio::test]
async fn pull_token_intent_mismatch_rejected() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "pull-e", PriorityClass::Scheduled).await?;

    let outcome = pull(&handle, "pull-e", Some("some-other-intent")).await;
    assert_eq!(
        outcome.expect_err("mismatched token must be rejected"),
        PullRejection::TokenMismatch
    );
    let (assignments, executions) = row_counts(&db.pool, "pull-e").await;
    assert_eq!((assignments, executions), (0, 0));
    Ok(())
}

// r[verify sched.executor.pull-not-ready]
/// (f) A pull for a drv whose open attempt belongs to a DIFFERENT
/// executor (a stream-dispatched build during coexistence) answers
/// NotYetReady, never re-points the existing assignment, and never
/// writes; after that attempt completes a re-pull answers Gone. (g) The
/// stream-dispatched execution row keeps the 'stream' default.
#[tokio::test]
async fn pull_open_attempt_on_other_executor_waits_then_gone() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("w-stream", "x86_64-linux").await?;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "pull-f", PriorityClass::Scheduled).await?;
    let stream_assignment = recv_assignment(&mut rx).await;
    let stream_exec: uuid::Uuid = stream_assignment.exec_id.parse().expect("exec uuid");

    // The stream attempt is open on w-stream; a pull-mode pod for the
    // same intent must wait, not steal and not duplicate.
    let outcome = pull(&handle, "pull-f", Some("pull-f")).await;
    assert!(
        matches!(outcome, Ok(PullOutcome::NotYetReady { .. })),
        "open-on-another-executor must answer NotYetReady, got {outcome:?}"
    );
    let (assignments, executions) = row_counts(&db.pool, "pull-f").await;
    assert_eq!(
        (assignments, executions),
        (1, 1),
        "no second attempt is minted"
    );
    let builder: String = sqlx::query_scalar(
        "SELECT a.builder_id FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = $1 AND a.status IN ('pending', 'acknowledged')",
    )
    .bind("pull-f")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        builder, "w-stream",
        "the existing assignment is never re-pointed"
    );
    assert_eq!(
        dispatch_mode_of(&db.pool, stream_exec).await,
        "stream",
        "the as-built dispatch path keeps the column default"
    );

    // After the stream attempt completes, the drv is no longer wanted.
    complete_success(
        &handle,
        "w-stream",
        &stream_assignment.drv_path,
        &rio_test_support::fixtures::test_store_path("pull-f-out"),
    )
    .await?;
    barrier(&handle).await;
    let outcome = pull(&handle, "pull-f", Some("pull-f")).await;
    assert!(matches!(outcome, Ok(PullOutcome::Gone)), "got {outcome:?}");
    Ok(())
}

// r[verify sched.executor.pull-not-ready]
/// (f, second half) After the open attempt fails and the drv requeues
/// to Ready, a re-pull Delivers a fresh attempt.
#[tokio::test]
async fn pull_after_failed_attempt_requeues_then_delivers() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("w-fail", "x86_64-linux").await?;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "pull-g", PriorityClass::Scheduled).await?;
    let stream_assignment = recv_assignment(&mut rx).await;

    // While open on the stream worker: wait.
    let outcome = pull(&handle, "pull-g", Some("pull-g")).await;
    assert!(matches!(outcome, Ok(PullOutcome::NotYetReady { .. })));

    // The worker reports a transient failure → the drv requeues.
    complete_failure(
        &handle,
        "w-fail",
        &stream_assignment.drv_path,
        rio_proto::types::BuildResultStatus::TransientFailure,
        "transient",
    )
    .await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, "pull-g").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "transient failure requeues the drv"
    );

    let delivered = expect_deliver(pull(&handle, "pull-g", Some("pull-g")).await);
    assert_ne!(
        delivered.exec_id, stream_assignment.exec_id,
        "a fresh attempt gets a fresh exec_id"
    );
    let exec_id: uuid::Uuid = delivered.exec_id.parse().expect("uuid");
    assert_eq!(dispatch_mode_of(&db.pool, exec_id).await, "pull");
    Ok(())
}
