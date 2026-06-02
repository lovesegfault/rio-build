//! Pull-mode dispatch: the `PullAssignment` red-first battery
//! (idempotency, Gone/NotYetReady semantics, the generation fence, the
//! token binding).

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
            // Mechanical flag-off defaults (carve-out 1c): build kind,
            // no per-replica instance — the as-built pull shape.
            kind: rio_evidence_kernel::pull::PullKind::Build,
            executor_instance: None,
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

// r[verify sched.executor.pull-transaction+2]
/// (a) Double pull returns the identical payload and exec_id and mints
/// exactly one assignments + drv_executions row pair.
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
    assert_eq!(first.is_fixed_output, second.is_fixed_output);

    let (assignments, executions) = row_counts(&db.pool, "pull-a").await;
    assert_eq!(assignments, 1, "exactly one assignments row");
    assert_eq!(executions, 1, "exactly one drv_executions row");

    // The drv is Running and bound to the intent identity.
    let info = expect_drv(&handle, "pull-a").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Running);
    Ok(())
}

// r[verify sched.executor.pull-gone]
/// (b) A pull for a completed drv returns Gone and writes nothing new.
#[tokio::test]
async fn pull_completed_drv_returns_gone() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "pull-b", PriorityClass::Scheduled).await?;
    pull_complete_success(
        &handle,
        "pull-b",
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

// r[verify sched.executor.pull-not-ready+2]
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

// D4-retarget: flips when the must_substitute arm dies (T-D4.1) — refusal
// becomes the JobView arm; re-assert NotYetReady-while-job-unresolved.
// (The sweep-settlement half this test carried — "the next Tick's batch
// probe takes the resubmit-directing fail-fast" — asserted the
// dispatch-partition fail-fast cell deleted with the walk spawner; the
// settlement successor is the origin-keyed consumption fail-fast,
// pinned by the routing-matrix and T-D2.1 batteries.)
// r[verify sched.merge.substitute-topdown+12]
/// A Ready node that may only complete via substitution (topdown-pruned
/// with Broken closure evidence — childless here) is never served from
/// source by the pull path: the pull answers NotYetReady, mints nothing
/// (no assignments / drv_executions rows, no Ready→Assigned
/// transition).
#[tokio::test]
async fn pull_must_substitute_node_refused() -> TestResult {
    let (db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // A childless target whose output is not in the store and not
    // substitutable; the merge seeds it Ready.
    let mut node = make_node("pull-tdms");
    node.expected_output_paths = vec![rio_test_support::fixtures::test_store_path("pull-tdms-out")];
    let build_id = Uuid::new_v4();
    // Hold the event receiver: the test-build orphan watcher (zero
    // grace) auto-cancels an unwatched Active build on the second Tick.
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Stage the post-prune / post-failover shape: childless + marked.
    assert!(handle.debug_set_topdown_pruned("pull-tdms", true).await?);

    // The pull refuses the mint: NotYetReady, nothing written, no
    // Ready→Assigned transition, no exec minted.
    let outcome = pull(&handle, "pull-tdms", Some("pull-tdms")).await;
    assert!(
        matches!(outcome, Ok(PullOutcome::NotYetReady { .. })),
        "a must-substitute node must not be served from source; got {outcome:?}"
    );
    let (assignments, executions) = row_counts(&db.pool, "pull-tdms").await;
    assert_eq!(
        (assignments, executions),
        (0, 0),
        "the refused pull writes nothing"
    );
    let d = expect_drv(&handle, "pull-tdms").await;
    assert_eq!(
        d.status,
        crate::state::DerivationStatus::Ready,
        "the refused pull must not move the node out of Ready"
    );

    let _ = build_id;
    Ok(())
}

// r[verify sched.executor.pull-transaction+2]
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

// r[verify sched.executor.pull-transaction+2]
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

/// Send one `ReportOutcome` through the actor and return the reply.
async fn report(
    handle: &ActorHandle,
    exec_id: uuid::Uuid,
    auth_intent: Option<&str>,
    status: rio_proto::types::BuildResultStatus,
    output_path: Option<&str>,
) -> Result<(), PullRejection> {
    let built_outputs = output_path
        .map(|p| {
            vec![rio_proto::types::BuiltOutput {
                output_name: "out".into(),
                output_path: p.into(),
                output_hash: vec![0u8; 32],
            }]
        })
        .unwrap_or_default();
    let error_msg = if built_outputs.is_empty() {
        "reported failure".to_string()
    } else {
        String::new()
    };
    handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: auth_intent.map(Into::into),
            payload: crate::actor::pull::PullReportPayload {
                result: rio_proto::types::BuildResult {
                    status: status.into(),
                    error_msg,
                    built_outputs,
                    ..Default::default()
                },
                peak_memory_bytes: 0,
                peak_cpu_cores: 0.0,
                node_name: None,
                hw_class: None,
                final_resources: None,
                final_line_count: 0,
                // Mechanical flag-off default (carve-out 1c).
                materialization_outcome: None,
            },
            reply,
        })
        .await
        .expect("actor alive")
}

async fn assignment_status_of(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT a.status FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = $1 ORDER BY a.assigned_at",
    )
    .bind(drv_hash)
    .fetch_all(pool)
    .await
    .expect("assignment status")
}

// r[verify sched.executor.report-idempotent]
/// (a) A duplicate ReportOutcome for the same exec_id is
/// acknowledged-and-ignored: one terminal state, no second verdict, no
/// new rows — for a successful first report.
#[tokio::test]
async fn report_outcome_success_duplicate_ignored() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "rep-a", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "rep-a", Some("rep-a")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    let out = rio_test_support::fixtures::test_store_path("rep-a-out");

    report(
        &handle,
        exec_id,
        Some("rep-a"),
        rio_proto::types::BuildResultStatus::Built,
        Some(&out),
    )
    .await
    .expect("first report acked");
    let info = expect_drv(&handle, "rep-a").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Completed);
    assert_eq!(
        assignment_status_of(&db.pool, "rep-a").await,
        vec!["completed"]
    );
    assert!(
        ledger_rows(&db.pool, "rep-a").await.is_empty(),
        "success appends no attempt row"
    );

    // Duplicate (now claiming failure — must not overwrite the outcome).
    report(
        &handle,
        exec_id,
        Some("rep-a"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        None,
    )
    .await
    .expect("duplicate report acked");
    let info = expect_drv(&handle, "rep-a").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Completed,
        "duplicate report must not change the terminal outcome"
    );
    assert_eq!(
        assignment_status_of(&db.pool, "rep-a").await,
        vec!["completed"]
    );
    assert!(
        ledger_rows(&db.pool, "rep-a").await.is_empty(),
        "duplicate writes nothing"
    );
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// (a) The failure flavor: one attempt row, one verdict; the duplicate
/// adds nothing.
#[tokio::test]
async fn report_outcome_failure_duplicate_single_charge() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "rep-b", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "rep-b", Some("rep-b")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    report(
        &handle,
        exec_id,
        Some("rep-b"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        None,
    )
    .await
    .expect("first report acked");
    let rows = ledger_rows(&db.pool, "rep-b").await;
    assert_eq!(rows.len(), 1, "exactly one attempt row for the failure");
    let info = expect_drv(&handle, "rep-b").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "transient failure requeues the drv"
    );

    report(
        &handle,
        exec_id,
        Some("rep-b"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        None,
    )
    .await
    .expect("duplicate report acked");
    let rows = ledger_rows(&db.pool, "rep-b").await;
    assert_eq!(rows.len(), 1, "the duplicate charges nothing");
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// (b) A report arriving after the attempt was already established as
/// an executor crash is acknowledged and ignored: the terminal row is
/// not overwritten and no completion is fabricated.
#[tokio::test]
async fn report_outcome_after_establishment_ignored() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "rep-c", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "rep-c", Some("rep-c")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // Simulate the establishment having already classified the attempt
    // (terminal fill present; assignment row deliberately left active
    // so the terminal-row-wins arm — not the closed-assignment arm —
    // is what ignores the report).
    let derivation_id: uuid::Uuid =
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind("rep-c")
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO drv_attempts \
             (attempt_id, derivation_id, exec_id, executor_id, event_kind, outcome_class, \
              termination_reason, reporting_party, occurred_at) \
         VALUES ($1, $2, $3, 'rep-c', 'attempt', 'executor_crash', 'unreported', \
                 'scheduler', now())",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(derivation_id)
    .bind(exec_id)
    .execute(&db.pool)
    .await?;

    let out = rio_test_support::fixtures::test_store_path("rep-c-out");
    report(
        &handle,
        exec_id,
        Some("rep-c"),
        rio_proto::types::BuildResultStatus::Built,
        Some(&out),
    )
    .await
    .expect("late report acked");

    let rows = ledger_rows(&db.pool, "rep-c").await;
    assert_eq!(rows.len(), 1, "no second row");
    assert_eq!(rows[0].outcome_class, "executor_crash");
    assert_eq!(
        rows[0].termination_reason.as_deref(),
        Some("unreported"),
        "the established terminal row is never overwritten"
    );
    let info = expect_drv(&handle, "rep-c").await;
    assert_ne!(
        info.status,
        crate::state::DerivationStatus::Completed,
        "no completion is fabricated from a post-establishment report"
    );
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// (c) A report whose exec_id matches no open attempt (never pulled) is
/// acknowledged and writes nothing.
#[tokio::test]
async fn report_outcome_unknown_exec_writes_nothing() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "rep-d", PriorityClass::Scheduled).await?;

    let out = rio_test_support::fixtures::test_store_path("rep-d-out");
    report(
        &handle,
        uuid::Uuid::now_v7(),
        Some("rep-d"),
        rio_proto::types::BuildResultStatus::Built,
        Some(&out),
    )
    .await
    .expect("unknown-exec report acked");

    let total_attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_attempts")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total_attempts, 0, "no attempt rows are created");
    let (assignments, executions) = row_counts(&db.pool, "rep-d").await;
    assert_eq!((assignments, executions), (0, 0), "no rows are created");
    let info = expect_drv(&handle, "rep-d").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "the drv is untouched by a never-pulled report"
    );
    Ok(())
}

// ─── ReportAttemptOutcome (the unified pod-terminal intake) ─────────────

/// Send one `ReportAttemptOutcome` through the actor.
async fn report_attempt_outcome(
    handle: &ActorHandle,
    intent_id: Option<&str>,
    exec_id: Option<uuid::Uuid>,
    reason: rio_proto::types::AttemptTerminalReason,
) -> Result<(), PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: intent_id.map(Into::into),
                job_name: None,
                exec_id,
            },
            reason,
            node_name: Some("node-9".into()),
            reply,
        })
        .await
        .expect("actor alive")
}

// r[verify sched.attempt.no-attempt-no-op]
/// A terminal report for an attempt identity with no attempt row (a pod
/// that died without ever completing a pull) is acknowledged and
/// charges nothing: no insert, no budget consumption, no floor bump, no
/// establishment — and the still-wanted drv stays Ready so the spawn
/// intent re-arms naturally.
#[tokio::test]
async fn attempt_outcome_no_attempt_is_charge_free() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "rao-a", PriorityClass::Scheduled).await?;

    report_attempt_outcome(
        &handle,
        Some("rao-a"),
        None,
        rio_proto::types::AttemptTerminalReason::Reaped,
    )
    .await
    .expect("no-attempt report acked");

    let total_attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_attempts")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total_attempts, 0, "no insert");
    let (assignments, executions) = row_counts(&db.pool, "rao-a").await;
    assert_eq!(
        (assignments, executions),
        (0, 0),
        "no establishment, no rows"
    );
    let info = expect_drv(&handle, "rao-a").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "the drv stays Ready (spawnable) — no budget or floor was consumed"
    );
    assert_eq!(info.retry.failure_count, 0, "no budget consumption");
    assert!(info.retry.failed_builders.is_empty(), "no exclusion charge");
    Ok(())
}

// r[verify ctrl.report.attempt-outcome]
/// A report for an already worker-reported attempt fills only
/// termination_reason (the second installment), never creates a new
/// row, and a duplicate is a no-op.
#[tokio::test]
async fn attempt_outcome_second_installment_fills_reason_only() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "rao-b", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "rao-b", Some("rao-b")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // The worker reports a transient failure first (the first
    // installment: a classified row with no termination_reason).
    report(
        &handle,
        exec_id,
        Some("rao-b"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        None,
    )
    .await
    .expect("worker report acked");
    let rows = ledger_rows(&db.pool, "rao-b").await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].termination_reason.is_none());
    let worker_class = rows[0].outcome_class.clone();

    // The controller's pod-terminal classification arrives second.
    report_attempt_outcome(
        &handle,
        None,
        Some(exec_id),
        rio_proto::types::AttemptTerminalReason::OomKilled,
    )
    .await
    .expect("second installment acked");
    let rows = ledger_rows(&db.pool, "rao-b").await;
    assert_eq!(rows.len(), 1, "the second installment never creates a row");
    assert_eq!(
        rows[0].termination_reason.as_deref(),
        Some("oom_killed"),
        "termination_reason is filled"
    );
    assert_eq!(
        rows[0].outcome_class, worker_class,
        "the worker's classification is never overwritten"
    );

    // A duplicate report is a no-op (first writer wins).
    report_attempt_outcome(
        &handle,
        None,
        Some(exec_id),
        rio_proto::types::AttemptTerminalReason::Error,
    )
    .await
    .expect("duplicate acked");
    let rows = ledger_rows(&db.pool, "rao-b").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].termination_reason.as_deref(), Some("oom_killed"));
    Ok(())
}

// r[verify ctrl.report.attempt-outcome]
/// When the fill wins on an attempt whose derivation is still in
/// flight on that exec (no other observer requeued it), the
/// pod-terminal classification requeues the drv.
#[tokio::test]
async fn attempt_outcome_requeues_still_inflight_attempt() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "rao-c", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "rao-c", Some("rao-c")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    let info = expect_drv(&handle, "rao-c").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Running);

    // A first-installment row exists (classified, unfilled) but nothing
    // has requeued the drv yet.
    let derivation_id: uuid::Uuid =
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind("rao-c")
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO drv_attempts \
             (attempt_id, derivation_id, exec_id, executor_id, event_kind, outcome_class, \
              termination_reason, reporting_party, occurred_at) \
         VALUES ($1, $2, $3, 'rao-c', 'attempt', 'disconnected', NULL, 'controller', now())",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(derivation_id)
    .bind(exec_id)
    .execute(&db.pool)
    .await?;

    report_attempt_outcome(
        &handle,
        None,
        Some(exec_id),
        rio_proto::types::AttemptTerminalReason::Preempted,
    )
    .await
    .expect("pod-terminal report acked");

    let rows = ledger_rows(&db.pool, "rao-c").await;
    assert_eq!(rows.len(), 1, "fill, not insert");
    assert_eq!(rows[0].termination_reason.as_deref(), Some("preempted"));
    let info = expect_drv(&handle, "rao-c").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "the pod-terminal classification requeues the still-in-flight drv"
    );
    Ok(())
}

/// (outcome_class, termination_reason, reporting_party, source_node) of
/// every drv_attempts row for one exec_id.
async fn exec_charge_facts(
    pool: &sqlx::PgPool,
    exec_id: uuid::Uuid,
) -> Vec<(String, Option<String>, String, Option<String>)> {
    sqlx::query_as(
        "SELECT outcome_class, termination_reason, reporting_party, source_node \
         FROM drv_attempts WHERE exec_id = $1 ORDER BY recorded_at",
    )
    .bind(exec_id)
    .fetch_all(pool)
    .await
    .expect("exec charge facts")
}

// r[verify sched.attempt.synthesized-verdict]
// r[verify sched.reassign.no-promote-on-ephemeral-disconnect+5]
/// A controller-synthesized Preempted verdict (intent-keyed, no exec_id
/// — the disruption-watcher shape) for an open, never-worker-reported
/// pull attempt closes it charge-free at this fold: exactly one
/// uncharged terminal row, the assignment closed, the drv requeued, no
/// budget or exclusion consumed, the resource floor untouched — and a
/// duplicate appends nothing. The requeue-because-executor-gone path
/// never bumps `resource_floor` and never records into
/// `failed_builders`/`failure_count`/`retry_count`; only the explicit
/// resource-exhaustion classifications promote, at their own sites.
#[tokio::test]
async fn attempt_outcome_synthesized_preempted_closes_uncharged_and_requeues() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "syn-a", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "syn-a", Some("syn-a")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    report_attempt_outcome(
        &handle,
        Some("syn-a"),
        None,
        rio_proto::types::AttemptTerminalReason::Preempted,
    )
    .await
    .expect("synthesized preempted report acked");

    let facts = exec_charge_facts(&db.pool, exec_id).await;
    assert_eq!(facts.len(), 1, "exactly one terminal row for the exec");
    let (class, reason, party, source_node) = &facts[0];
    assert_eq!(class, "disconnected", "the close is the uncharged class");
    assert_eq!(reason.as_deref(), Some("preempted"));
    assert_eq!(party, "controller");
    assert_eq!(
        source_node.as_deref(),
        Some("node-9"),
        "the synthesized close carries the controller-reported node"
    );
    assert_eq!(
        assignment_status_of(&db.pool, "syn-a").await,
        vec!["failed"],
        "the assignment row is closed in the same transaction"
    );
    let info = expect_drv(&handle, "syn-a").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "the still-wanted drv requeues at this fold, not the establishment sweep"
    );
    assert_eq!(info.retry.failure_count, 0, "charge-free (no budget)");
    assert!(
        info.retry.failed_builders.is_empty(),
        "charge-free (no exclusion entry)"
    );
    assert_eq!(
        info.retry.count, 0,
        "no retry budget consumed at the requeue site"
    );
    assert_eq!(
        info.sched.resource_floor,
        Default::default(),
        "an executor-gone requeue never bumps the floor — loss is not a sizing signal"
    );

    // A duplicate synthesized report resolves as attempt-terminal and
    // appends nothing.
    report_attempt_outcome(
        &handle,
        Some("syn-a"),
        None,
        rio_proto::types::AttemptTerminalReason::Preempted,
    )
    .await
    .expect("duplicate acked");
    assert_eq!(exec_charge_facts(&db.pool, exec_id).await.len(), 1);
    Ok(())
}

// r[verify sched.attempt.synthesized-verdict]
/// The same close keyed by exec_id with reason Reaped (the
/// synthesize-on-delete shape used by the controller's reap arms).
#[tokio::test]
async fn attempt_outcome_synthesized_reaped_by_exec_id_closes_uncharged() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "syn-b", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "syn-b", Some("syn-b")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    report_attempt_outcome(
        &handle,
        None,
        Some(exec_id),
        rio_proto::types::AttemptTerminalReason::Reaped,
    )
    .await
    .expect("synthesized reaped report acked");

    let facts = exec_charge_facts(&db.pool, exec_id).await;
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].0, "disconnected");
    assert_eq!(facts[0].1.as_deref(), Some("reaped"));
    let info = expect_drv(&handle, "syn-b").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Ready);
    assert_eq!(info.retry.failure_count, 0, "charge-free");
    assert!(info.retry.failed_builders.is_empty());
    Ok(())
}

// r[verify sched.attempt.synthesized-verdict]
/// Pod-terminal reasons that are NOT controller-synthesized verdicts
/// (OOM, eviction, deadline, plain error) keep the as-built behavior on
/// an unclassified open attempt: acknowledged, nothing written — the
/// establishment sweep stays their classifier.
#[tokio::test]
async fn attempt_outcome_pod_terminal_reason_still_waits_for_establishment() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "syn-c", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "syn-c", Some("syn-c")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    report_attempt_outcome(
        &handle,
        Some("syn-c"),
        None,
        rio_proto::types::AttemptTerminalReason::OomKilled,
    )
    .await
    .expect("pod-terminal report acked");

    assert!(
        exec_charge_facts(&db.pool, exec_id).await.is_empty(),
        "no row is written for a pod-terminal reason without a worker classification"
    );
    let info = expect_drv(&handle, "syn-c").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Running,
        "the attempt stays open for the establishment sweep"
    );
    Ok(())
}

// r[verify sched.attempt.synthesized-verdict]
/// The builder's AD5 SIGTERM-abort report (`BuildResultStatus::Cancelled`
/// on a still-wanted derivation) resolves the pull attempt charge-free
/// and requeues it — never an infrastructure-failure charge.
#[tokio::test]
async fn report_outcome_worker_abort_still_wanted_closes_uncharged() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "syn-d", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "syn-d", Some("syn-d")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    report(
        &handle,
        exec_id,
        Some("syn-d"),
        rio_proto::types::BuildResultStatus::Cancelled,
        None,
    )
    .await
    .expect("abort report acked");

    let facts = exec_charge_facts(&db.pool, exec_id).await;
    assert_eq!(facts.len(), 1, "exactly one terminal row for the abort");
    let (class, reason, party, _node) = &facts[0];
    assert_eq!(
        class, "disconnected",
        "the abort of still-wanted work is uncharged, never infra"
    );
    assert_eq!(reason.as_deref(), Some("worker_abort"));
    assert_eq!(party, "worker");
    let info = expect_drv(&handle, "syn-d").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "the aborted still-wanted drv requeues at this fold"
    );
    assert_eq!(info.retry.failure_count, 0, "no budget consumed");
    assert_eq!(info.retry.infra_count, 0, "no infra budget consumed");
    assert!(info.retry.failed_builders.is_empty(), "no exclusion entry");
    assert_eq!(
        assignment_status_of(&db.pool, "syn-d").await,
        vec!["failed"],
        "the assignment row is closed"
    );

    // Duplicate abort report: terminal row wins, nothing more is written.
    report(
        &handle,
        exec_id,
        Some("syn-d"),
        rio_proto::types::BuildResultStatus::Cancelled,
        None,
    )
    .await
    .expect("duplicate abort acked");
    assert_eq!(exec_charge_facts(&db.pool, exec_id).await.len(), 1);
    Ok(())
}

// r[verify sched.attempt.synthesized-verdict]
/// A genuinely-cancelled (no-longer-wanted) derivation keeps the cancel
/// arm's exact shape: the worker's abort report after CancelBuild writes
/// nothing, charges nothing, and the drv stays Cancelled (never
/// requeued).
#[tokio::test]
async fn report_outcome_abort_after_cancel_build_stays_cancelled_and_uncharged() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "syn-e", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "syn-e", Some("syn-e")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // Scheduler-side cancel verdict (the genuine cancel arm).
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: reply_tx,
        })
        .await?;
    assert!(reply_rx.await??, "CancelBuild succeeds");
    let info = expect_drv(&handle, "syn-e").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Cancelled);

    // The pod's SIGTERM-abort report arrives after the cancel.
    report(
        &handle,
        exec_id,
        Some("syn-e"),
        rio_proto::types::BuildResultStatus::Cancelled,
        None,
    )
    .await
    .expect("abort report acked");

    assert!(
        exec_charge_facts(&db.pool, exec_id).await.is_empty(),
        "a cancelled drv's abort report writes nothing (the cancel arm shape)"
    );
    let info = expect_drv(&handle, "syn-e").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Cancelled,
        "the no-longer-wanted drv is never requeued"
    );
    assert_eq!(info.retry.failure_count, 0);
    Ok(())
}

// ─── AD2 scheduler half: source-keyed exclusion (T-1b.1) ────────────────

/// `source_node` of every drv_attempts row for one drv hash, in append
/// order.
async fn ledger_source_nodes(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<Option<String>> {
    sqlx::query_scalar(
        "SELECT a.source_node FROM drv_attempts a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = $1 ORDER BY a.recorded_at, a.attempt_id",
    )
    .bind(drv_hash)
    .fetch_all(pool)
    .await
    .expect("drv_attempts source_node query")
}

/// Spawn intents as the controller would read them (unfiltered).
async fn spawn_intents(handle: &ActorHandle) -> Vec<rio_proto::types::SpawnIntent> {
    handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(crate::actor::AdminQuery::GetSpawnIntents {
                req: crate::actor::SpawnIntentsRequest::default(),
                reply,
            })
        })
        .await
        .expect("actor alive")
        .intents
}

// r[verify sched.retry.per-executor-budget+4]
/// (a) An attempt appended for a pull-mode pod carries `source_node`
/// from the controller-authoritative spawn-ack binding, and the
/// requeued intent advertises that node in `excluded_nodes` (the AD2
/// node-keyed exclusion, end to end on the scheduler half).
#[tokio::test]
async fn pull_attempt_failure_stamps_source_node_and_excludes_node() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "psn-a", PriorityClass::Scheduled).await?;

    // Controller-authoritative pod→node binding for the intent.
    handle
        .send_unchecked(ActorCommand::AckSpawnedIntents {
            spawned: vec![],
            unfulfillable_cells: vec![],
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![rio_proto::types::BoundIntent {
                intent_id: "psn-a".into(),
                node_name: "node-7".into(),
            }],
        })
        .await?;
    barrier(&handle).await;

    let assignment = expect_deliver(pull(&handle, "psn-a", Some("psn-a")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    report(
        &handle,
        exec_id,
        Some("psn-a"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        None,
    )
    .await
    .expect("failure report acked");

    assert_eq!(
        ledger_source_nodes(&db.pool, "psn-a").await,
        vec![Some("node-7".to_string())],
        "the pull-mode attempt row carries the spawn-ack node attribution"
    );
    let info = expect_drv(&handle, "psn-a").await;
    assert!(
        info.retry.failed_builders.contains("node-7"),
        "the exclusion view is keyed by the source node, got {:?}",
        info.retry.failed_builders
    );
    let intents = spawn_intents(&handle).await;
    let intent = intents
        .iter()
        .find(|i| i.intent_id == "psn-a")
        .expect("requeued drv re-advertised as a spawn intent");
    assert_eq!(
        intent.excluded_nodes,
        vec!["node-7".to_string()],
        "the intent advertises the node-keyed exclusion for the controller's anti-affinity"
    );
    Ok(())
}

/// Backdate one attempt's assignment row past any establishment window.
async fn backdate(pool: &sqlx::PgPool, exec_id: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `ReportAttemptOutcome` with an explicit controller-reported node.
async fn report_attempt_outcome_with_node(
    handle: &ActorHandle,
    intent_id: &str,
    reason: rio_proto::types::AttemptTerminalReason,
    node: &str,
) -> Result<(), PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: Some(intent_id.into()),
                job_name: None,
                exec_id: None,
            },
            reason,
            node_name: Some(node.into()),
            reply,
        })
        .await
        .expect("actor alive")
}

// r[verify sched.retry.per-executor-budget+4]
/// Mint-before-binding: when the pull lost the race against the
/// controller's binding ack, the controller's pod-terminal report
/// delivers the node and the later establishment charge still carries
/// the AD2 node key (exclusion + anti-affinity keyed by the node, not
/// by the intent identity).
#[tokio::test]
async fn establishment_charge_carries_node_from_pod_terminal_report() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "psn-b", PriorityClass::Scheduled).await?;

    // The pull happens with NO binding known (the ack lost the race).
    let assignment = expect_deliver(pull(&handle, "psn-b", Some("psn-b")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    let minted_node: Option<String> =
        sqlx::query_scalar("SELECT source_node FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(minted_node, None, "the mint-time race was lost");

    // The pod dies; the controller's classification report (a
    // non-synthesized reason — the establishment sweep stays the
    // classifier) carries the kube-authoritative node.
    report_attempt_outcome_with_node(
        &handle,
        "psn-b",
        rio_proto::types::AttemptTerminalReason::OomKilled,
        "node-8",
    )
    .await
    .expect("pod-terminal report acked");

    backdate(&db.pool, exec_id).await?;
    tick(&handle).await?;

    assert_eq!(
        ledger_source_nodes(&db.pool, "psn-b").await,
        vec![Some("node-8".to_string())],
        "the establishment charge carries the controller-reported node"
    );
    let info = expect_drv(&handle, "psn-b").await;
    assert!(
        info.retry.failed_builders.contains("node-8"),
        "the exclusion entry is keyed by the node, got {:?}",
        info.retry.failed_builders
    );
    assert!(
        !info.retry.failed_builders.contains("psn-b"),
        "the intent-identity fallback key must not be used when the node is known"
    );
    let intents = spawn_intents(&handle).await;
    let intent = intents
        .iter()
        .find(|i| i.intent_id == "psn-b")
        .expect("requeued drv re-advertised as a spawn intent");
    assert_eq!(
        intent.excluded_nodes,
        vec!["node-8".to_string()],
        "the respawned intent advertises the node-keyed exclusion"
    );
    Ok(())
}

// r[verify sched.retry.per-executor-budget+4]
/// A binding ack that arrives only after the mint still attributes the
/// establishment charge: the sweep falls back to the in-memory
/// controller-authoritative binding at establishment time.
#[tokio::test]
async fn establishment_charge_falls_back_to_late_binding_ack() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "psn-c", PriorityClass::Scheduled).await?;

    let assignment = expect_deliver(pull(&handle, "psn-c", Some("psn-c")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // The binding ack lands AFTER the mint (no pod-terminal report is
    // ever delivered — the establishment sweep is the only observer).
    handle
        .send_unchecked(ActorCommand::AckSpawnedIntents {
            spawned: vec![],
            unfulfillable_cells: vec![],
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![rio_proto::types::BoundIntent {
                intent_id: "psn-c".into(),
                node_name: "node-10".into(),
            }],
        })
        .await?;
    barrier(&handle).await;

    backdate(&db.pool, exec_id).await?;
    tick(&handle).await?;

    assert_eq!(
        ledger_source_nodes(&db.pool, "psn-c").await,
        vec![Some("node-10".to_string())],
        "the establishment charge picks up the late binding ack"
    );
    let info = expect_drv(&handle, "psn-c").await;
    assert!(
        info.retry.failed_builders.contains("node-10"),
        "the exclusion entry is keyed by the node, got {:?}",
        info.retry.failed_builders
    );
    Ok(())
}

// r[verify sched.retry.per-executor-budget+4]
/// The crash→establish→respawn loop is bounded: three unreported
/// crashes attributed to three distinct nodes reach Poison(Threshold)
/// instead of collapsing onto one intent-keyed exclusion entry forever.
#[tokio::test]
async fn unreported_crash_loop_reaches_poison_threshold_with_node_keys() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "psn-d", PriorityClass::Scheduled).await?;

    for (i, node) in ["node-a1", "node-a2", "node-a3"].iter().enumerate() {
        let assignment = expect_deliver(pull(&handle, "psn-d", Some("psn-d")).await);
        let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
        report_attempt_outcome_with_node(
            &handle,
            "psn-d",
            rio_proto::types::AttemptTerminalReason::OomKilled,
            node,
        )
        .await
        .expect("pod-terminal report acked");
        backdate(&db.pool, exec_id).await?;
        tick(&handle).await?;
        let info = expect_drv(&handle, "psn-d").await;
        assert_eq!(
            info.retry.failure_count,
            u32::try_from(i + 1).unwrap(),
            "each establishment charges exactly once"
        );
    }

    let info = expect_drv(&handle, "psn-d").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Poisoned,
        "three node-keyed unreported crashes reach the poison threshold (bounded loop)"
    );
    assert_eq!(
        info.retry.failed_builders.len(),
        3,
        "three distinct node keys"
    );
    Ok(())
}

// r[verify sched.dispatch.fleet-exhaust+4]
/// The spawn-gate exhaustion arm: a `NoEligibleSource` report for a
/// still-Ready derivation poisons it through the fleet-exhaust arm
/// (one `fleet_exhaust` marker row, no charge), and re-reports are
/// idempotent no-ops.
#[tokio::test]
async fn attempt_outcome_no_eligible_source_poisons_ready_drv() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "nes-a", PriorityClass::Scheduled).await?;

    report_attempt_outcome(
        &handle,
        Some("nes-a"),
        None,
        rio_proto::types::AttemptTerminalReason::NoEligibleSource,
    )
    .await
    .expect("spawn-gate report acked");

    let info = expect_drv(&handle, "nes-a").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Poisoned,
        "excluded ⊇ spawnable maps to the fleet-exhaust poison"
    );
    assert_eq!(
        ledger_classes(&db.pool, "nes-a").await,
        vec!["fleet_exhaust".to_string()],
        "one verdict marker row, no charge rows"
    );
    let rows = ledger_rows(&db.pool, "nes-a").await;
    assert!(
        rows[0].exec_id.is_none() && rows[0].executor_id.is_none(),
        "a verdict marker is not an execution"
    );
    assert_eq!(info.retry.failure_count, 0, "no budget consumption");

    // Idempotent on re-tick: acked, nothing new written.
    report_attempt_outcome(
        &handle,
        Some("nes-a"),
        None,
        rio_proto::types::AttemptTerminalReason::NoEligibleSource,
    )
    .await
    .expect("duplicate spawn-gate report acked");
    assert_eq!(
        ledger_classes(&db.pool, "nes-a").await.len(),
        1,
        "re-report appends nothing"
    );
    let info = expect_drv(&handle, "nes-a").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Poisoned);

    // Unknown intent: acknowledged, nothing written anywhere.
    report_attempt_outcome(
        &handle,
        Some("nes-zzz"),
        None,
        rio_proto::types::AttemptTerminalReason::NoEligibleSource,
    )
    .await
    .expect("unknown-intent spawn-gate report acked");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_attempts")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total, 1, "only the nes-a marker row exists");
    Ok(())
}

// ─── C4/C5 unification: stream identities behind the unified RPC ────────

/// Send one `ReportAttemptOutcome` keyed by Job/pod name only (the
/// re-pointed controller's stream-mode shape).
async fn report_attempt_outcome_job(
    handle: &ActorHandle,
    job_name: &str,
    reason: rio_proto::types::AttemptTerminalReason,
) -> Result<(), PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: None,
                job_name: Some(job_name.into()),
                exec_id: None,
            },
            reason,
            node_name: None,
            reply,
        })
        .await
        .expect("actor alive")
}

/// Send one `ReportAttemptOutcome` carrying BOTH an intent id and a
/// job/pod name (the controller's pod-terminal shape).
async fn report_attempt_outcome_both(
    handle: &ActorHandle,
    intent_id: &str,
    job_name: &str,
    reason: rio_proto::types::AttemptTerminalReason,
    node_name: &str,
) -> Result<(), PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: Some(intent_id.into()),
                job_name: Some(job_name.into()),
                exec_id: None,
            },
            reason,
            node_name: Some(node_name.into()),
            reply,
        })
        .await
        .expect("actor alive")
}

// r[verify sched.attempt.synthesized-verdict]
/// The disruption-watcher shape (intent id + Job name, no exec) for a
/// PULL-mode pod still resolves and closes the pull attempt — the
/// stream-identity routing guard must not divert reports whose job
/// name the scheduler has never seen as a stream executor.
#[tokio::test]
async fn attempt_outcome_pull_pod_report_with_job_name_still_closes_attempt() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "uni-c", PriorityClass::Scheduled).await?;
    let assignment = expect_deliver(pull(&handle, "uni-c", Some("uni-c")).await);
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    report_attempt_outcome_both(
        &handle,
        "uni-c",
        "rio-builder-canary-abc12",
        rio_proto::types::AttemptTerminalReason::Preempted,
        "node-7",
    )
    .await
    .expect("pull-mode preemption report acked");

    let facts = exec_charge_facts(&db.pool, exec_id).await;
    assert_eq!(facts.len(), 1, "the pull attempt is closed");
    assert_eq!(facts[0].0, "disconnected");
    assert_eq!(facts[0].1.as_deref(), Some("preempted"));
    let info = expect_drv(&handle, "uni-c").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Ready);
    Ok(())
}
