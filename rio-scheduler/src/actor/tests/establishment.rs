//! Establishment sweep for open pull-mode attempts: the red-first
//! battery (window, store-probe adopt, charge+requeue, and the
//! generation fence).

use super::*;
use crate::actor::pull::PullOutcome;
use crate::state::OutcomeClass;

/// Pull helper (same as the pull battery's, local to this module).
async fn pull_deliver(handle: &ActorHandle, intent: &str) -> rio_proto::types::WorkAssignment {
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: intent.into(),
            auth_intent: Some(intent.into()),
            // Mechanical flag-off defaults (carve-out 1c).
            kind: rio_evidence_kernel::pull::PullKind::Build,
            executor_instance: None,
            resume_exec_id: None,
            reply,
        })
        .await
        .expect("actor alive");
    match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("expected Deliver, got {other:?}"),
    }
}

/// Backdate the attempt's assignment row so its age exceeds any
/// deadline + slack the sweep can compute.
async fn backdate_assignment(pool: &sqlx::PgPool, exec_id: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn attempt_rows_for(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<LedgerRow> {
    ledger_rows(pool, drv_hash).await
}

async fn assignment_statuses(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT a.status FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = $1 ORDER BY a.assigned_at",
    )
    .bind(drv_hash)
    .fetch_all(pool)
    .await
    .expect("assignment statuses")
}

// r[verify sched.attempt.establishment-window+5]
/// (a) An open pull-mode attempt past deadline + slack with no terminal
/// row is established exactly once as executor_crash/unreported,
/// charged to failure_count, and the drv requeues.
/// No node attribution ever arrives here (no binding ack, no
/// controller report), so the established charge carries NO exclusion
/// key (decision P12: the budget key is the controller-authoritative
/// source node only — an unattributed crash cannot occupy a
/// distinct-source slot); the node-keyed cases live in the AD2
/// battery (`establishment_charge_carries_node_*`).
#[tokio::test]
async fn establishment_charges_and_requeues_after_window() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-a", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-a").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, exec_id).await?;

    tick(&handle).await?;

    let rows = attempt_rows_for(&db.pool, "est-a").await;
    assert_eq!(rows.len(), 1, "established exactly once");
    assert_eq!(rows[0].outcome_class, OutcomeClass::ExecutorCrash.as_str());
    assert_eq!(rows[0].termination_reason.as_deref(), Some("unreported"));
    assert_eq!(rows[0].exec_id, Some(exec_id));

    let info = expect_drv(&handle, "est-a").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Ready,
        "the drv requeues after establishment"
    );
    assert_eq!(info.retry.failure_count, 1, "charged once (C2)");
    assert!(
        info.retry.failed_builders.is_empty(),
        "an unattributed establishment contributes no exclusion key \
         (P12: source-node keys only), got {:?}",
        info.retry.failed_builders
    );
    assert_eq!(
        assignment_statuses(&db.pool, "est-a").await,
        vec!["failed"],
        "the assignment row minted by the pull is closed"
    );

    // A second sweep pass establishes nothing further.
    tick(&handle).await?;
    let rows = attempt_rows_for(&db.pool, "est-a").await;
    assert_eq!(rows.len(), 1, "the establishment is idempotent");
    assert_eq!(expect_drv(&handle, "est-a").await.retry.failure_count, 1);
    Ok(())
}

// r[verify sched.attempt.establishment-window+5]
/// (b) The same attempt with its outputs present in the store is
/// adopted as completed (store-probe arm) and never charged.
#[tokio::test]
async fn establishment_store_probe_adopts_completed_attempt() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let db_pool = {
        // setup_with_mock_store returns the TestDb first; rebind for clarity.
        _db.pool.clone()
    };
    let out_path = test_store_path("est-b-out");

    let mut node = make_node("est-b");
    node.expected_output_paths = vec![out_path.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let assignment = pull_deliver(&handle, "est-b").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db_pool, exec_id).await?;
    // The pod uploaded its outputs but died before its report landed:
    // the outputs appear in the store only after the pull.
    store.seed_with_content(&out_path, b"est-b output");

    tick(&handle).await?;

    let info = expect_drv(&handle, "est-b").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Completed,
        "outputs present in the store → adopted as completed"
    );
    assert!(
        attempt_rows_for(&db_pool, "est-b").await.is_empty(),
        "the adopt arm never charges"
    );
    assert_eq!(info.retry.failure_count, 0);
    assert_eq!(
        assignment_statuses(&db_pool, "est-b").await,
        vec!["completed"],
        "the assignment row is closed as completed"
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+5]
/// (c) An attempt inside the window is never established.
#[tokio::test]
async fn establishment_never_fires_inside_window() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-c", PriorityClass::Scheduled).await?;
    let _assignment = pull_deliver(&handle, "est-c").await;

    tick(&handle).await?;
    tick(&handle).await?;

    assert!(
        attempt_rows_for(&db.pool, "est-c").await.is_empty(),
        "no establishment inside the window"
    );
    let info = expect_drv(&handle, "est-c").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Running);
    assert_eq!(info.retry.failure_count, 0);
    assert_eq!(
        assignment_statuses(&db.pool, "est-c").await,
        vec!["pending"]
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+5]
/// (d) The establishment transaction at a below-floor serving
/// generation writes nothing (the fence applies to establishment too).
#[tokio::test]
async fn establishment_below_floor_writes_nothing() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-d", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-d").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, exec_id).await?;
    // A successor's claim raises the durable floor above this
    // replica's serving generation (1).
    sqlx::query("INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'next')")
        .execute(&db.pool)
        .await?;

    tick(&handle).await?;

    assert!(
        attempt_rows_for(&db.pool, "est-d").await.is_empty(),
        "below-floor establishment writes nothing"
    );
    let info = expect_drv(&handle, "est-d").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Running,
        "the attempt is left untouched for the real leader to resolve"
    );
    assert_eq!(info.retry.failure_count, 0, "no charge");
    assert_eq!(
        assignment_statuses(&db.pool, "est-d").await,
        vec!["pending"],
        "the assignment row stays open"
    );
    Ok(())
}

// r[verify sched.attempt.establishment-window+5]
/// (g) The window is anchored to the deadline the attempt was
/// dispatched with: a sweep-time re-solve that is smaller than the
/// persisted deadline must NOT shrink the window (no establishment
/// while the attempt is inside the dispatched deadline + slack), and
/// the attempt still establishes once the persisted anchor has passed.
#[tokio::test]
async fn establishment_window_anchored_to_dispatched_deadline() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-g", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-g").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // The mint persisted the deadline this attempt was dispatched
    // under (the same solve that sizes activeDeadlineSeconds).
    let minted: Option<f64> =
        sqlx::query_scalar("SELECT deadline_secs FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert!(
        minted.is_some_and(|d| d > 0.0),
        "the pull mint persists the dispatched deadline, got {minted:?}"
    );

    // Simulate the dispatched deadline being far LARGER than anything
    // the sweep can re-solve now (the inverse of an estimator/hw-table
    // shrink): the attempt is backdated past the re-solved window but
    // not past the dispatched one — it must stay open and uncharged.
    sqlx::query("UPDATE drv_executions SET deadline_secs = $2 WHERE exec_id = $1")
        .bind(exec_id)
        .bind(20_000_000.0_f64)
        .execute(&db.pool)
        .await?;
    backdate_assignment(&db.pool, exec_id).await?;
    tick(&handle).await?;
    assert!(
        attempt_rows_for(&db.pool, "est-g").await.is_empty(),
        "no establishment while the attempt is inside its dispatched deadline + slack"
    );
    let info = expect_drv(&handle, "est-g").await;
    assert_eq!(info.status, crate::state::DerivationStatus::Running);
    assert_eq!(info.retry.failure_count, 0, "no charge inside the window");
    assert_eq!(
        assignment_statuses(&db.pool, "est-g").await,
        vec!["pending"],
        "the assignment row stays open"
    );

    // Once the dispatched anchor is genuinely in the past the sweep
    // establishes exactly as before.
    sqlx::query("UPDATE drv_executions SET deadline_secs = 1.0 WHERE exec_id = $1")
        .bind(exec_id)
        .execute(&db.pool)
        .await?;
    tick(&handle).await?;
    let rows = attempt_rows_for(&db.pool, "est-g").await;
    assert_eq!(rows.len(), 1, "established once the window truly closed");
    assert_eq!(rows[0].outcome_class, OutcomeClass::ExecutorCrash.as_str());
    Ok(())
}

// r[verify sched.attempt.synthesized-verdict+3]
/// (f) An attempt closed charge-free by a controller-synthesized verdict
/// is never re-established: the sweep adds no executor_crash row and no
/// charge for that exec, even far past the window.
#[tokio::test]
async fn establishment_skips_synthesized_closed_attempt() -> TestResult {
    let (db, handle, _task) = setup().await;
    let _ev = merge_single_node(&handle, Uuid::new_v4(), "est-f", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-f").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;

    // The controller synthesizes Preempted for the open attempt (the
    // AD5/C6 successor), exec-pinned (merged_bug_135): closed
    // charge-free, requeued at that fold.
    handle
        .query_unchecked(|reply| ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: Some("est-f".into()),
                job_name: None,
                exec_id: Some(exec_id),
            },
            reason: rio_proto::types::AttemptTerminalReason::Preempted,
            node_name: Some("node-est-f".into()),
            resubmit_cycle: 0,
            reply,
        })
        .await
        .expect("actor alive")
        .expect("synthesized report acked");
    let rows = attempt_rows_for(&db.pool, "est-f").await;
    assert_eq!(rows.len(), 1, "the synthesized close wrote one row");
    assert_eq!(rows[0].outcome_class, OutcomeClass::Disconnected.as_str());

    // Even far past any window the sweep establishes nothing further.
    backdate_assignment(&db.pool, exec_id).await?;
    tick(&handle).await?;
    let rows = attempt_rows_for(&db.pool, "est-f").await;
    assert_eq!(
        rows.len(),
        1,
        "no executor_crash establishment lands on a synthesized-closed attempt"
    );
    assert_eq!(rows[0].outcome_class, OutcomeClass::Disconnected.as_str());
    let info = expect_drv(&handle, "est-f").await;
    assert_eq!(
        info.retry.failure_count, 0,
        "still uncharged after the sweep"
    );
    Ok(())
}

// r[verify sched.attempt.cancel-close-driven+1]
/// Cancelled work is NEVER charged, even when the cancel's terminal
/// persist fails: the failed batch latches in the status outbox
/// (assignment stays open meanwhile), the next tick's flush re-drives
/// it to durability, and the end state is the assignment closed
/// `cancelled` with ZERO attempt-ledger rows —
/// `pull_establishments_total` never moves for this drv.
///
/// Pre-fix red: the failed persist was dropped (best-effort error
/// log), the open attempt aged past the window, and the establishment
/// sweep charged it `executor_crash` — an exclusion-ledger and
/// OA2-clustering verdict about work nobody wanted.
#[tokio::test]
async fn cancelled_attempt_is_never_charged_and_close_is_redriven() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "est-cancel", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-cancel").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, exec_id).await?;

    // Deterministic persist failure: hide the derivations table so the
    // cancel's terminal status batch (and everything else touching it)
    // fails. The cancel still transitions in-memory.
    sqlx::query("ALTER TABLE derivations RENAME TO derivations_hidden")
        .execute(&db.pool)
        .await?;
    cancel_build(&handle, build_id).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM assignments WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?,
        "pending",
        "the failed persist leaves the assignment open (latched, not lost)"
    );

    // One tick while PG is still broken: nothing can flush, nothing
    // may charge.
    tick(&handle).await?;

    // Heal PG: the next tick's flush re-drives the latched batch.
    sqlx::query("ALTER TABLE derivations_hidden RENAME TO derivations")
        .execute(&db.pool)
        .await?;
    tick(&handle).await?;

    assert_eq!(
        attempt_rows_for(&db.pool, "est-cancel").await.len(),
        0,
        "cancelled work must never be charged (no executor_crash establishment)"
    );
    assert_eq!(
        assignment_statuses(&db.pool, "est-cancel").await,
        vec!["cancelled"],
        "the re-driven persist closes the assignment as cancelled"
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'est-cancel'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(status, "cancelled", "the derivation status persisted");

    // Subsequent sweeps stay silent.
    tick(&handle).await?;
    assert_eq!(attempt_rows_for(&db.pool, "est-cancel").await.len(), 0);
    Ok(())
}

// r[verify sched.attempt.establishment-window+5]
/// merged_bug_232 (bughunt wave, A4): a probe FAILURE is not evidence.
/// An expired BUILD attempt swept while FindMissingPaths is failing
/// must DEFER — no executor_crash row, the attempt stays open — and a
/// later pass with a working probe decides it. Pre-fix the caller
/// conflated probe-failure with no-store-configured and charged an
/// irreversible crash on an RPC blip.
#[tokio::test]
async fn probe_unavailable_defers_build_establishment() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let out_path = test_store_path("est-d-out");
    let mut node = make_node("est-d");
    node.expected_output_paths = vec![out_path.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let assignment = pull_deliver(&handle, "est-d").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, exec_id).await?;

    // The store is up but FMP fails (RPC error): no evidence either way.
    store
        .faults
        .fail_find_missing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tick(&handle).await?;

    let rows = attempt_rows_for(&db.pool, "est-d").await;
    assert_eq!(
        rows.len(),
        0,
        "probe Unavailable must DEFER a build establishment (no charge), got {rows:?}"
    );
    assert_eq!(
        assignment_statuses(&db.pool, "est-d").await,
        vec!["pending"],
        "the attempt stays open across the deferred pass"
    );

    // The probe heals: the next pass decides (charge — outputs absent).
    store
        .faults
        .fail_find_missing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    tick(&handle).await?;
    let rows = attempt_rows_for(&db.pool, "est-d").await;
    assert_eq!(rows.len(), 1, "the healed pass establishes exactly once");
    assert_eq!(rows[0].outcome_class, OutcomeClass::ExecutorCrash.as_str());
    Ok(())
}

// r[verify sched.attempt.establishment-window+5]
/// merged_bug_232 green pin: the MATERIALIZATION arm still charges on a
/// failing probe — the kind axis decides before the probe axis (a
/// mid-walk crash leaves the closure incomplete; outputs-present is
/// not adoption evidence for a walk), so probe availability never
/// defers a materialization establishment.
#[tokio::test]
async fn establishment_mat_arm_charges_with_failing_probe() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("est-e-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("est-e");
    n.expected_output_paths = vec![out.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;
    let claimed = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "est-e".into(),
            auth_intent: Some("est-e".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-test-0".into()),
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    let assignment = match claimed {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db.pool, exec_id).await?;

    store
        .faults
        .fail_find_missing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tick(&handle).await?;

    let rows = attempt_rows_for(&db.pool, "est-e").await;
    let infra: Vec<_> = rows
        .iter()
        .filter(|r| r.outcome_class == OutcomeClass::MaterializationInfra.as_str())
        .collect();
    assert_eq!(
        infra.len(),
        1,
        "the mat establishment is probe-independent (kind axis decides \
         before the probe axis), got {rows:?}"
    );
    Ok(())
}

/// bug_148 (bughunt-2 wave): the adopt arm must stamp ONLY the
/// verified wanted subset. A node with two declared outputs where one
/// build wants just `out` adopts when `out` is present — stamping the
/// never-probed-or-absent `doc` path as a completed output fabricates
/// presence evidence (path_tenants upsert + DerivationCompleted event
/// + downstream input resolution all consume output_paths).
// r[verify sched.attempt.establishment-window+5]
#[tokio::test]
async fn establishment_adopt_stamps_only_verified_wanted_outputs() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let db_pool = _db.pool.clone();
    let out_path = test_store_path("est-w-out");
    let doc_path = test_store_path("est-w-doc");

    let mut node = make_node("est-w");
    node.output_names = vec!["out".into(), "doc".into()];
    node.expected_output_paths = vec![out_path.clone(), doc_path.clone()];
    node.wanted_output_names = vec!["out".into()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let assignment = pull_deliver(&handle, "est-w").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db_pool, exec_id).await?;
    // Only the WANTED output landed; `doc` is provably absent (the
    // probe reports it missing).
    store.seed_with_content(&out_path, b"est-w out");

    tick(&handle).await?;

    let info = expect_drv(&handle, "est-w").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Completed,
        "all verified wanted outputs present → adopted"
    );
    assert_eq!(
        info.output_paths,
        vec![out_path.clone()],
        "the adopt stamps exactly the verified wanted subset — never \
         the unverified expected_output_paths superset"
    );
    assert!(
        attempt_rows_for(&db_pool, "est-w").await.is_empty(),
        "the adopt arm never charges"
    );
    Ok(())
}

/// merged_bug_210 trigger 1 (bughunt-2 wave): a terminal-settled node
/// must close its stale open attempt charge-free, never re-charge.
/// Shape: the adopt arm completes the node in-memory but its persist
/// and assignment close both fail (PG blip); the outputs are then
/// GC'd. The next sweep pass sees an expired open attempt whose node
/// is Completed with nothing verifiable — charging executor_crash
/// there seeds the exclusion ledger and the OA2 wedge clustering with
/// a crash verdict about work that is already done.
// r[verify sched.attempt.establishment-window+5]
#[tokio::test]
async fn establishment_terminal_settled_node_closes_charge_free() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let db_pool = _db.pool.clone();
    let out_path = test_store_path("est-ts-out");

    let mut node = make_node("est-ts");
    node.expected_output_paths = vec![out_path.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let assignment = pull_deliver(&handle, "est-ts").await;
    let exec_id: uuid::Uuid = assignment.exec_id.parse()?;
    backdate_assignment(&db_pool, exec_id).await?;
    store.seed_with_content(&out_path, b"est-ts output");

    // Pass 1: the adopt fires but every fenced PG write is refused (a
    // foreign claim raises the durable floor above this replica's
    // serving generation — the same injection as the below-floor
    // battery): node Completed in-memory, the durable attempt stays
    // open because the assignment close was fence-refused.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES (999, 'interloper')",
    )
    .execute(&db_pool)
    .await?;
    tick(&handle).await?;
    sqlx::query("DELETE FROM leader_generation_claims WHERE generation = 999")
        .execute(&db_pool)
        .await?;
    let info = expect_drv(&handle, "est-ts").await;
    assert_eq!(
        info.status,
        crate::state::DerivationStatus::Completed,
        "pass 1 adopted in-memory despite the failed close"
    );
    assert_eq!(
        assignment_statuses(&db_pool, "est-ts").await,
        vec!["pending"],
        "the failed close left the attempt durably open"
    );

    // The outputs are GC'd between passes (the adopt unpinned them).
    store.state.paths.write().unwrap().remove(&out_path);

    // Pass 2: the node is terminal-settled — close charge-free.
    tick(&handle).await?;

    let rows = attempt_rows_for(&db_pool, "est-ts").await;
    assert!(
        rows.is_empty(),
        "a terminal-settled node is never re-charged (got {rows:?})"
    );
    assert_eq!(
        assignment_statuses(&db_pool, "est-ts").await,
        vec!["completed"],
        "the stale attempt closes with the settled status's cause"
    );
    assert_eq!(
        expect_drv(&handle, "est-ts").await.retry.failure_count,
        0,
        "no crash verdict enters the retry fold of settled work"
    );
    Ok(())
}

/// merged_bug_011 trigger A (bughunt-2 wave): a latched stale batch
/// must never touch a resubmitted derivation's fresh attempt. The
/// cancel's persist fails (latched: Cancelled + exec1); the user
/// resubmits — the reset persists fresh state and the new pull's
/// upsert rewrites the active assignment row to exec2. The flush's
/// replay must DROP the entry (present-different: the node advanced),
/// never regress the row to cancelled or force-close exec2's pending
/// assignment (pre-fix the derivation-scoped absolute close hit it).
// r[verify sched.attempt.cancel-close-driven+1]
#[tokio::test]
async fn outbox_stale_replay_never_touches_resubmitted_attempt() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build1 = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build1, "est-rsb", PriorityClass::Scheduled).await?;
    let assignment1 = pull_deliver(&handle, "est-rsb").await;
    let exec1: uuid::Uuid = assignment1.exec_id.parse()?;

    // The cancel's terminal persist fails: batch latched with exec1.
    sqlx::query("ALTER TABLE derivations RENAME TO derivations_hidden")
        .execute(&db.pool)
        .await?;
    cancel_build(&handle, build1).await?;
    sqlx::query("ALTER TABLE derivations_hidden RENAME TO derivations")
        .execute(&db.pool)
        .await?;

    // Resubmit: the reset path revives the node; the new pull's
    // active-row upsert rewrites the assignment to exec2.
    let build2 = Uuid::new_v4();
    let _ev2 = merge_single_node(&handle, build2, "est-rsb", PriorityClass::Scheduled).await?;
    let assignment2 = pull_deliver(&handle, "est-rsb").await;
    let exec2: uuid::Uuid = assignment2.exec_id.parse()?;
    assert_ne!(exec1, exec2, "the resubmitted attempt is a fresh exec");

    // The flush tick: the stale Cancelled batch must be dropped.
    tick(&handle).await?;

    let drv_status: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'est-rsb'")
            .fetch_one(&db.pool)
            .await?;
    assert_ne!(
        drv_status, "cancelled",
        "a stale latched batch must never regress a resubmitted \
         derivation (the node advanced past the latch)"
    );
    let (row_exec, row_status): (uuid::Uuid, String) = sqlx::query_as(
        "SELECT a.exec_id, a.status FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = 'est-rsb'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        row_exec, exec2,
        "the active row belongs to the fresh attempt"
    );
    assert_eq!(
        row_status, "pending",
        "the fresh attempt's assignment must survive the stale replay \
         (pre-fix the derivation-scoped close cancelled it)"
    );
    Ok(())
}

/// merged_bug_011 trigger B (bughunt-2 wave): a terminal row never
/// regresses. Same latch shape as trigger A, but the resubmitted
/// attempt COMPLETES (durable 'completed') before the flush fires —
/// the stale Cancelled replay must be dropped, not rewrite completed
/// work back to cancelled (pre-fix fallout: a failover re-dispatches
/// already-built work, dual execution).
// r[verify sched.attempt.cancel-close-driven+1]
#[tokio::test]
async fn outbox_stale_replay_never_regresses_completed_row() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build1 = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build1, "est-cmp", PriorityClass::Scheduled).await?;
    let assignment1 = pull_deliver(&handle, "est-cmp").await;
    let exec1: uuid::Uuid = assignment1.exec_id.parse()?;
    let _ = exec1;

    sqlx::query("ALTER TABLE derivations RENAME TO derivations_hidden")
        .execute(&db.pool)
        .await?;
    cancel_build(&handle, build1).await?;
    sqlx::query("ALTER TABLE derivations_hidden RENAME TO derivations")
        .execute(&db.pool)
        .await?;

    // Resubmit and complete: durable row reaches 'completed'.
    let build2 = Uuid::new_v4();
    let _ev2 = merge_single_node(&handle, build2, "est-cmp", PriorityClass::Scheduled).await?;
    let assignment2 = pull_deliver(&handle, "est-cmp").await;
    let exec2: uuid::Uuid = assignment2.exec_id.parse()?;
    pull_report_exec(
        &handle,
        exec2,
        "est-cmp",
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::Built.into(),
            built_outputs: vec![rio_proto::types::BuiltOutput {
                output_name: "out".into(),
                output_path: test_store_path("est-cmp-out"),
                output_hash: vec![0u8; 32],
            }],
            ..Default::default()
        }),
    )
    .await?;
    let pre: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'est-cmp'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        pre, "completed",
        "the resubmitted attempt completed durably"
    );

    tick(&handle).await?;

    let post: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'est-cmp'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        post, "completed",
        "a stale latched Cancelled batch must never regress a \
         completed derivation"
    );
    Ok(())
}

/// merged_bug_011 drain half (the bug_291 component): a healed PG
/// drains the WHOLE outbox in one tick — the one-batch throttle is
/// for the Err path only (fail fast once per tick), not a 6/minute
/// trickle after recovery (each queued tick was another window for a
/// latched batch's derivations to advance).
// r[verify sched.attempt.cancel-close-driven+1]
#[tokio::test]
async fn outbox_drains_fully_on_ok_in_one_tick() -> TestResult {
    let (db, handle, _task) = setup().await;
    let mut builds = Vec::new();
    for tag in ["est-dr-a", "est-dr-b", "est-dr-c"] {
        let build_id = Uuid::new_v4();
        let _ev = merge_single_node(&handle, build_id, tag, PriorityClass::Scheduled).await?;
        let _assignment = pull_deliver(&handle, tag).await;
        builds.push(build_id);
    }
    // Three separate failed persists → three latched batches.
    sqlx::query("ALTER TABLE derivations RENAME TO derivations_hidden")
        .execute(&db.pool)
        .await?;
    for build_id in builds {
        cancel_build(&handle, build_id).await?;
    }
    sqlx::query("ALTER TABLE derivations_hidden RENAME TO derivations")
        .execute(&db.pool)
        .await?;

    // ONE tick on healed PG: every batch flushes.
    tick(&handle).await?;

    for tag in ["est-dr-a", "est-dr-b", "est-dr-c"] {
        let status: String =
            sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(tag)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            status, "cancelled",
            "{tag}: a healed PG drains the whole outbox in one tick \
             (got {status})"
        );
    }
    Ok(())
}

/// merged_bug_011 keep-when-reaped contract pin: a latched batch whose
/// node left the DAG (terminal cleanup reaped it) still flushes — the
/// close must land even though nothing in memory wants the node — and
/// the exec-scoped close touches exactly the latched attempt. Pins the
/// CleanupTerminalBuild contract the DROP rule depends on (reap only
/// removes in-memory-terminal nodes, so absent ⇒ the latched terminal
/// status is still the node's truth).
// r[verify sched.attempt.cancel-close-driven+1]
#[tokio::test]
async fn outbox_reaped_node_batch_still_flushes() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "est-rp", PriorityClass::Scheduled).await?;
    let assignment = pull_deliver(&handle, "est-rp").await;
    let exec1: uuid::Uuid = assignment.exec_id.parse()?;

    sqlx::query("ALTER TABLE derivations RENAME TO derivations_hidden")
        .execute(&db.pool)
        .await?;
    cancel_build(&handle, build_id).await?;
    sqlx::query("ALTER TABLE derivations_hidden RENAME TO derivations")
        .execute(&db.pool)
        .await?;
    // Reap the terminal build now (bypassing TERMINAL_CLEANUP_DELAY):
    // the node leaves the DAG entirely.
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
        .await?;
    barrier(&handle).await;
    let gone = handle.debug_query_derivation("est-rp").await?;
    assert!(gone.is_none(), "the reap removed the cancelled node");

    tick(&handle).await?;

    let drv_status: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'est-rp'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        drv_status, "cancelled",
        "an absent (reaped) node's latched batch still flushes"
    );
    let (row_exec, row_status): (uuid::Uuid, String) = sqlx::query_as(
        "SELECT a.exec_id, a.status FROM assignments a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = 'est-rp'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(row_exec, exec1);
    assert_eq!(
        row_status, "cancelled",
        "the exec-scoped close lands on exactly the latched attempt"
    );
    Ok(())
}
