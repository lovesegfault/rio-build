use super::*;

use rio_store::grpc::StoreServiceImpl;
use rio_store::test_helpers::{put_path, spawn_store_service};

// -----------------------------------------------------------------------
// Scheduler-side cache check (TOCTOU fix)
// -----------------------------------------------------------------------

/// Spin up an in-process rio-store on an ephemeral port.
pub(super) async fn setup_inproc_store(
    pool: sqlx::PgPool,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    // Inline storage in manifests.inline_blob (no chunk backend needed).
    spawn_store_service(StoreServiceImpl::new(pool)).await
}

/// Build a minimal single-file NAR and upload it to the store (trailer mode).
pub(super) async fn put_test_path(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<()> {
    let (nar, _hash) = rio_test_support::fixtures::make_nar(b"hello");
    let info = rio_test_support::fixtures::make_path_info_for_nar(store_path, &nar);
    put_path(client, info, nar).await?;
    Ok(())
}

#[tokio::test]
async fn test_scheduler_cache_check_skips_build() -> TestResult {
    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;

    // Start in-process store and pre-populate the expected output path.
    let (mut store_client, _store_server) = setup_inproc_store(store_db.pool.clone()).await?;
    let cached_output = test_store_path("cached-output");
    put_test_path(&mut store_client, &cached_output).await?;

    // Spawn actor WITH the store client — cache check will run.
    let (handle, _task) = setup_actor_with_store(sched_db.pool.clone(), Some(store_client.clone()));

    // Merge a single-node DAG with expected_output_paths pointing at the
    // pre-populated path. No worker needed — scheduler should find it
    // cached and complete immediately.
    let build_id = Uuid::new_v4();
    let mut node = make_node("cached-hash");
    node.expected_output_paths = vec![cached_output.to_string()];

    let _event_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Derivation should have gone Created → Completed (scheduler cache hit).
    let info = expect_drv(&handle, "cached-hash").await;
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "scheduler cache check should mark derivation as Completed"
    );
    assert_eq!(info.output_paths, vec![cached_output]);

    // Build should be Succeeded (all 1 derivation cached).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.cached_derivations, 1);
    assert_eq!(
        status.completed_derivations, 1,
        "completed should count cached exactly once (no double-counting)"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build with all-cached derivations should be Succeeded"
    );
    Ok(())
}

#[tokio::test]
async fn test_scheduler_cache_check_skipped_without_store() -> TestResult {
    // No store client — setup() uses setup_actor(pool, None).
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let mut node = make_node("uncached-hash");
    // expected_output_paths set but store client is None — should NOT short-circuit
    node.expected_output_paths = vec![test_store_path("uncached-out")];

    let _event_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Without store client, derivation should proceed normally to dispatch.
    let info = expect_drv(&handle, "uncached-hash").await;
    assert!(
        matches!(
            info.status,
            DerivationStatus::Assigned | DerivationStatus::Ready
        ),
        "derivation should be dispatched normally without store client, got {:?}",
        info.status
    );
    Ok(())
}

// -----------------------------------------------------------------------
// DB fault injection
// -----------------------------------------------------------------------

/// A cyclic DAG submission must not leak into the actor's in-memory maps.
/// Regression test for the reorder fix: merge() now runs BEFORE the map
/// inserts, so a CycleDetected error leaves no trace in
/// build_events/build_sequences/builds.
#[tokio::test]
async fn test_cyclic_merge_does_not_leak_in_memory_state() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    // A depends on B, B depends on A — cycle.
    let nodes = vec![make_node("cycA"), make_node("cycB")];
    let edges = vec![
        make_test_edge("cycA", "cycB"),
        make_test_edge("cycB", "cycA"),
    ];

    let result = merge_dag(&handle, build_id, nodes, edges, false).await;
    assert!(
        result.is_err(),
        "cyclic DAG should be rejected with an error"
    );

    // The build must NOT be in the actor's maps (it was never inserted,
    // or it was rolled back). QueryBuildStatus should return NotFound.
    let status_result = try_query_status(&handle, build_id).await?;
    assert!(
        matches!(status_result, Err(ActorError::BuildNotFound(_))),
        "build should not be in actor maps after cyclic merge failure; got {status_result:?}"
    );

    // The DAG should have no trace of the cyclic nodes.
    let drv_a = handle.debug_query_derivation("cycA").await?;
    assert!(
        drv_a.is_none(),
        "cycA should not exist in DAG after cycle rollback"
    );
    let drv_b = handle.debug_query_derivation("cycB").await?;
    assert!(
        drv_b.is_none(),
        "cycB should not exist in DAG after cycle rollback"
    );
    Ok(())
}

// -----------------------------------------------------------------------
// Backpressure hysteresis
// -----------------------------------------------------------------------

/// Backpressure should activate at 80%, stay active, and only deactivate
/// at 60% (hysteresis). Before the fix, ActorHandle used a simple 80%
/// threshold with no hysteresis -> flapping under load near 80%.
#[tokio::test]
async fn test_backpressure_hysteresis() {
    let db = TestDb::new(&MIGRATOR).await;
    let scheduler_db = SchedulerDb::new(db.pool.clone());
    let mut actor = DagActor::new(
        scheduler_db,
        DagActorConfig::default(),
        DagActorPlumbing::default(),
    );
    let flag = actor.backpressure_flag();

    // Simulate queue at 50% — below high watermark, not active.
    actor.update_backpressure(5000, 10000);
    assert!(!flag.is_active(), "50%: should not be active");

    // 85% — above high watermark, activates.
    actor.update_backpressure(8500, 10000);
    assert!(flag.is_active(), "85%: should activate");

    // 70% — between watermarks, STILL active (hysteresis).
    actor.update_backpressure(7000, 10000);
    assert!(
        flag.is_active(),
        "70%: should STAY active (hysteresis between 60% and 80%)"
    );

    // 55% — below low watermark, deactivates.
    actor.update_backpressure(5500, 10000);
    assert!(
        !flag.is_active(),
        "55%: should deactivate (below 60% low watermark)"
    );

    // 70% again — below high watermark, STILL inactive (hysteresis).
    actor.update_backpressure(7000, 10000);
    assert!(
        !flag.is_active(),
        "70%: should STAY inactive (hysteresis between 60% and 80%)"
    );
}

/// ActorHandle::send() and ::is_backpressured() should honor the shared
/// hysteresis flag, not compute their own threshold.
#[tokio::test]
async fn test_handle_uses_shared_backpressure_flag() {
    let (_db, handle, _task) = setup().await;

    // Initially not backpressured (empty queue).
    assert!(!handle.is_backpressured());

    // Directly set the shared flag (simulating actor's hysteresis decision).
    handle.backpressure.set_for_test(true);
    assert!(
        handle.is_backpressured(),
        "handle should read the shared flag"
    );

    // send() should reject under backpressure.
    let (reply_tx, _) = oneshot::channel();
    let result = handle
        .send(ActorCommand::QueryBuildStatus {
            build_id: Uuid::new_v4(),
            caller_tenant: None,
            reply: reply_tx,
        })
        .await;
    assert!(
        matches!(result, Err(ActorError::Backpressure)),
        "send() should reject when shared flag is set"
    );

    // Clear flag; send() should succeed.
    handle.backpressure.set_for_test(false);
    assert!(!handle.is_backpressured());
}

/// When try_send to a worker's stream fails (channel full/disconnected),
/// assign_to_worker must remove drv_hash from worker.running_build.
/// Without cleanup: phantom capacity leak (worker appears busy forever).
///
/// P0537: with single-build, channel-FULL during one dispatch pass is
/// no longer reachable (dispatch sends at most one per worker). The
/// channel-CLOSED case exercises the same cleanup path: drop the
/// receiver before dispatch so try_send fails.
#[tokio::test]
async fn test_assign_send_failure_cleans_running_build() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Connect worker, then drop the receiver so try_send sees Closed.
    let (stream_tx, stream_rx) = mpsc::channel(1);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "tight-worker".into(),
            stream_tx,
            stream_epoch: next_stream_epoch_for("tight-worker"),
            auth_intent: None,
            reply: noop_connect_reply(),
        })
        .await?;
    send_heartbeat(&handle, "tight-worker", "x86_64-linux").await?;
    drop(stream_rx);

    // Merge 1 leaf derivation. Dispatch picks tight-worker, try_send
    // fails (receiver gone) — this triggers the recovery path.
    let build_id = Uuid::new_v4();
    let _event_rx = merge_dag(&handle, build_id, vec![make_node("drvA")], vec![], false).await?;

    // Worker should have ZERO running builds — the failed send must
    // have cleaned up running_build, not leaked the phantom entry.
    let worker = expect_worker(&handle, "tight-worker").await;
    assert!(
        worker.running_build.is_none(),
        "failed try_send must clean up running_build; got {:?}",
        worker.running_build
    );

    // The derivation should be back in Ready (not stuck Assigned).
    let unsent = expect_drv(&handle, "drvA").await;
    assert_eq!(
        unsent.status,
        DerivationStatus::Ready,
        "unsent derivation should be reset to Ready"
    );

    // Disconnect tight-worker (its stream_tx is dead but is_registered
    // still true — would keep losing the dispatch coin-flip).
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "tight-worker".into(),
            stream_epoch: stream_epoch_for("tight-worker"),
        })
        .await?;

    // A fresh worker picks it up — proves it's actually re-dispatchable,
    // not stuck.
    let mut stream_rx2 = connect_executor(&handle, "fresh-worker", "x86_64-linux").await?;
    let assignment = recv_assignment(&mut stream_rx2).await;
    assert!(assignment.drv_path.contains("drvA"));
    Ok(())
}

// r[verify sched.log.phase-binding]
/// `ForwardPhase` from an executor not assigned the derivation MUST be
/// dropped. `handle_forward_phase` checks `state.assigned_executor`
/// before fanning the event out — the phase-path analogue of the
/// store-side log binding gate (`store.log.append-auth`), reading the
/// same authoritative record `ProcessCompletion`'s stale-report guard
/// reads (`sched.completion.idempotent`). Without it, a compromised
/// builder spoofs `BuildPhase{derivation_path: <victim>}` and the
/// gateway renders attacker-controlled `phase` text as `SetPhase` into
/// another tenant's `nix build -L` progress display.
///
/// Sentinel pattern (same as `test_forward_log_batch_unknown_drv_path_dropped`):
/// send the rogue phase first, then a legitimate one. The first `Phase`
/// event observed on the broadcast must be the legitimate one — if the
/// rogue had been forwarded, it would appear first.
#[tokio::test]
async fn test_forward_phase_rejects_unassigned_executor() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let drv_hash = "phasegate";
    let drv_path = test_drv_path(drv_hash);
    let mut events =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    let _assignment = pull_attempt(&handle, drv_hash).await;

    // Sanity: the pull mint opened the attempt on the intent identity.
    // The gate compares against this.
    let pre = expect_drv(&handle, drv_hash).await;
    assert_eq!(pre.status, DerivationStatus::Running);
    assert_eq!(pre.assigned_executor.as_deref(), Some(drv_hash));

    // Rogue executor "w2" — never registered, never assigned this drv —
    // spoofs a Phase. MUST be dropped before reaching the broadcast.
    handle
        .send_unchecked(ActorCommand::ForwardPhase {
            phase: rio_proto::types::BuildPhase {
                derivation_path: drv_path.clone(),
                phase: "rogue-injected-text".into(),
            },
            executor_id: "w2".into(),
        })
        .await?;

    // Legitimate executor's Phase IS forwarded. Sentinel: proves the
    // actor processed the rogue command (FIFO mailbox) and intentionally
    // dropped it, rather than the test racing the broadcast.
    handle
        .send_unchecked(ActorCommand::ForwardPhase {
            phase: rio_proto::types::BuildPhase {
                derivation_path: drv_path.clone(),
                phase: "buildPhase".into(),
            },
            executor_id: drv_hash.into(),
        })
        .await?;

    // `Event::Phase` is NOT display_only (event.rs:173) → state ring,
    // which `merge_single_node` returns. Drain merge/dispatch noise
    // (Started, InputsResolved, Derivation events, Progress) until the
    // first Phase appears; assert it's the sentinel.
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await??;
        if let Some(rio_proto::types::build_event::Event::Phase(got)) = ev.event {
            assert_eq!(
                got.phase, "buildPhase",
                "rogue executor's spoofed Phase must be dropped; only the \
                 assigned executor's Phase is fanned out to interested builds"
            );
            assert_eq!(got.derivation_path, drv_path);
            break;
        }
    }
    Ok(())
}

// r[verify sched.log.phase-binding]
/// `ForwardPhase` from the *same* executor for a drv that has reached a
/// terminal state MUST be dropped. The worker-completion terminals
/// (`handle_success_completion` → `Completed`, `terminal_failure_epilogue`
/// → `Poisoned`/timeout-`Cancelled`) do not clear `state.assigned_executor`
/// — re-dispatch paths (`reset_to_ready`, transient retry), user-cancel's
/// in-flight `Assigned|Running` arm (`cancel_build_derivations`), and
/// orphan adoption (`adopt_orphan_completion`) do — so the field stays
/// stamped for ~60s until `CleanupTerminalBuild` reaps the DAG node.
/// Without the `Assigned|Running` status precondition, the executor-match
/// gate would still pass for the just-finished executor in that window.
/// Companion to `test_forward_phase_rejects_unassigned_executor`, which
/// covers the cross-executor case.
///
/// Structural assertion via metric: a positive `not_active` increment
/// proves the precondition fired (rather than waiting for the absence
/// of a `Phase` event on the broadcast). The `assigned_executor`
/// pre-assert is a fixture-validity guard — if a future change clears
/// the field at terminal, this test stops exercising the residual
/// window and should be reviewed.
#[tokio::test]
async fn test_forward_phase_rejects_terminal_drv() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let drv_hash = "termphase";
    let drv_path = test_drv_path(drv_hash);
    let _events = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    let _assignment = pull_attempt(&handle, drv_hash).await;

    // Sanity: the pull mint opened the attempt on the intent identity.
    let pre = expect_drv(&handle, drv_hash).await;
    assert_eq!(pre.status, DerivationStatus::Running);
    assert_eq!(pre.assigned_executor.as_deref(), Some(drv_hash));

    // Drive the drv to a terminal state via the legitimate executor.
    pull_complete_success_empty(&handle, drv_hash).await?;

    // Fixture-validity guard: the residual window MUST exist for this
    // test to mean anything. `transition(Completed)` does not clear
    // `assigned_executor`; `CleanupTerminalBuild` runs ~60s later. If
    // either changes and the field is now cleared at terminal, the
    // status precondition is no longer load-bearing — re-evaluate
    // whether this test (and the precondition itself) is still needed.
    let post = expect_drv(&handle, drv_hash).await;
    assert_eq!(post.status, DerivationStatus::Completed);
    assert_eq!(
        post.assigned_executor.as_deref(),
        Some(drv_hash),
        "transition(Completed) must NOT clear assigned_executor in this \
         test's setup — otherwise the test no longer exercises the \
         post-terminal residual window"
    );

    // Late phase from the now-terminal executor. Same `executor_id` that
    // owned the assignment — the bare executor-match gate would pass.
    handle
        .send_unchecked(ActorCommand::ForwardPhase {
            phase: rio_proto::types::BuildPhase {
                derivation_path: drv_path.clone(),
                phase: "stale-post-terminal".into(),
            },
            executor_id: drv_hash.into(),
        })
        .await?;
    barrier(&handle).await;

    // Status precondition fired with the `not_active` reason. Asserting
    // the label proves the rejection branch ran — not the executor
    // mismatch (would be `executor_mismatch`) and not a missing
    // assignment (would be `no_assignment`).
    assert_eq!(
        recorder.get("rio_scheduler_phases_rejected_total{reason=not_active}"),
        1,
        "late phase from the just-finished executor must be rejected by \
         the Assigned|Running status precondition; recorded keys: {:?}",
        recorder.all_keys()
    );
    Ok(())
}
