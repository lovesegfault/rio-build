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
    let mut node = make_test_node("cached-hash", "x86_64-linux");
    node.expected_output_paths = vec![cached_output.to_string()];

    let _event_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Derivation should have gone Created → Completed (scheduler cache hit).
    let info = handle
        .debug_query_derivation("cached-hash")
        .await?
        .expect("derivation should exist in DAG");
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
    let (_db, handle, _task, _rx) = setup_with_worker("test-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut node = make_test_node("uncached-hash", "x86_64-linux");
    // expected_output_paths set but store client is None — should NOT short-circuit
    node.expected_output_paths = vec![test_store_path("uncached-out")];

    let _event_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Without store client, derivation should proceed normally to dispatch.
    let info = handle
        .debug_query_derivation("uncached-hash")
        .await?
        .expect("derivation should exist");
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

/// Verify that DB failures during completion are logged but do not block
/// the in-memory state machine. DB writes are best-effort; the actor
/// must not stall on DB unavailability.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_db_failure_during_completion_logged() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("test-worker", "x86_64-linux").await?;
    let build_id = Uuid::new_v4();
    let drv_hash = "db-fault-hash";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Sanity check: derivation was dispatched.
    let pre = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(pre.status, DerivationStatus::Assigned);

    // Close the DB pool — subsequent DB writes will fail.
    db.pool.close().await;

    // Send successful completion. DB write will fail but in-memory
    // transition should succeed.
    complete_success(
        &handle,
        "test-worker",
        &drv_path,
        &test_store_path("fake-output"),
    )
    .await?;

    // In-memory state should have transitioned despite DB failure.
    let post = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should still exist");
    assert_eq!(
        post.status,
        DerivationStatus::Completed,
        "in-memory transition should succeed despite DB unavailability"
    );

    // DB failure should have been logged at error level — both for the
    // derivation status update AND for the build completion transition.
    assert!(
        logs_contain("failed to update derivation status in DB")
            || logs_contain("failed to persist"),
        "DB failure during derivation completion should be logged"
    );
    assert!(
        logs_contain("failed to persist build completion"),
        "DB failure in complete_build (transition_build) should be logged, not silently discarded"
    );

    // TestDb::drop uses a separate admin connection, so closing the test
    // pool here doesn't prevent database cleanup.
    Ok(())
}

/// A cyclic DAG submission must not leak into the actor's in-memory maps.
/// Regression test for the reorder fix: merge() now runs BEFORE the map
/// inserts, so a CycleDetected error leaves no trace in
/// build_events/build_sequences/builds.
#[tokio::test]
async fn test_cyclic_merge_does_not_leak_in_memory_state() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    // A depends on B, B depends on A — cycle.
    let nodes = vec![
        make_test_node("cycA", "x86_64-linux"),
        make_test_node("cycB", "x86_64-linux"),
    ];
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
/// assign_to_worker must remove drv_hash from worker.running_builds.
/// Without cleanup: phantom capacity leak (worker appears busy forever).
///
/// P0537: with single-build, channel-FULL during one dispatch pass is
/// no longer reachable (dispatch sends at most one per worker). The
/// channel-CLOSED case exercises the same cleanup path: drop the
/// receiver before dispatch so try_send fails.
#[tokio::test]
async fn test_assign_send_failure_cleans_running_builds() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Connect worker, then drop the receiver so try_send sees Closed.
    let (stream_tx, stream_rx) = mpsc::channel(1);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "tight-worker".into(),
            stream_tx,
        })
        .await?;
    send_heartbeat(&handle, "tight-worker", "x86_64-linux").await?;
    drop(stream_rx);

    // Merge 1 leaf derivation. Dispatch picks tight-worker, try_send
    // fails (receiver gone) — this triggers the recovery path.
    let build_id = Uuid::new_v4();
    let _event_rx = merge_dag(
        &handle,
        build_id,
        vec![make_test_node("drvA", "x86_64-linux")],
        vec![],
        false,
    )
    .await?;

    // Worker should have ZERO running builds — the failed send must
    // have cleaned up running_builds, not leaked the phantom entry.
    let workers = handle.debug_query_workers().await?;
    let worker = workers
        .iter()
        .find(|w| w.executor_id == "tight-worker")
        .expect("tight-worker registered");
    assert_eq!(
        worker.running_count, 0,
        "failed try_send must clean up running_builds; got {:?}",
        worker.running_builds
    );

    // The derivation should be back in Ready (not stuck Assigned).
    let unsent = handle
        .debug_query_derivation("drvA")
        .await?
        .expect("exists");
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
        })
        .await?;

    // A fresh worker picks it up — proves it's actually re-dispatchable,
    // not stuck.
    let mut stream_rx2 = connect_executor(&handle, "fresh-worker", "x86_64-linux").await?;
    let assignment = recv_assignment(&mut stream_rx2).await;
    assert!(assignment.drv_path.contains("drvA"));
    Ok(())
}

// -----------------------------------------------------------------------
// Log forwarding (ForwardLogBatch → gateway via BuildEvent::Log)
// -----------------------------------------------------------------------

/// ForwardLogBatch with a drv_path the DAG knows → BuildEvent::Log arrives
/// on the broadcast rx for the interested build.
///
/// This tests the actor-side half of the log pipeline. The ring buffer
/// (LogBuffers) is written directly by the gRPC recv task, not tested here —
/// see logs.rs tests. The end-to-end gRPC wire test is in grpc/tests.rs.
///
/// Uses `setup()` (no worker): we send ForwardLogBatch directly to the actor,
/// not via the gRPC BuildExecution stream. With no worker, no dispatch happens,
/// so the event stream is quiet after InputsResolved — makes the drain loop
/// deterministic. (With a worker, DerivationStarted would arrive AFTER
/// InputsResolved when dispatch fires, which is a perfectly valid ordering but
/// complicates the drain.)
#[tokio::test]
async fn test_forward_log_batch_reaches_interested_build() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let mut events =
        merge_single_node(&handle, build_id, "logtest", PriorityClass::Scheduled).await?;

    // Drain merge-time events. With no worker present, dispatch is a no-op,
    // so after InputsResolved the stream goes quiet until we push a log
    // batch. Merge-time sequence: [DerivationCached*] → Started →
    // InputsResolved. Breaking on InputsResolved (not Started) ensures
    // we've consumed the full merge-time burst.
    loop {
        let ev = events.recv().await?;
        if matches!(
            ev.event,
            Some(rio_proto::types::build_event::Event::InputsResolved(_))
        ) {
            break;
        }
    }

    let batch = rio_proto::types::BuildLogBatch {
        derivation_path: test_drv_path("logtest"),
        lines: vec![b"hello from worker".to_vec(), b"second line".to_vec()],
        first_line_number: 0,
        executor_id: "w1".into(),
    };
    handle
        .send_unchecked(ActorCommand::ForwardLogBatch {
            drv_path: test_drv_path("logtest"),
            batch: batch.clone(),
        })
        .await?;

    // The actor resolves drv_path → hash → interested_builds, then emits
    // BuildEvent::Log on the broadcast channel. Bounded wait — if the
    // event never arrives (bug), timeout after 5s rather than hanging.
    let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await??;
    match ev.event {
        Some(rio_proto::types::build_event::Event::Log(got)) => {
            assert_eq!(got.derivation_path, batch.derivation_path);
            assert_eq!(got.lines, batch.lines);
            assert_eq!(got.first_line_number, 0);
        }
        other => panic!("expected Log event, got {other:?}"),
    }
    assert_eq!(ev.build_id, build_id.to_string());
    Ok(())
}

/// ForwardLogBatch with a drv_path the DAG does NOT know → silently dropped.
/// No event emitted, no panic. See actor/mod.rs handler comment for the two
/// legitimate causes (post-cleanup race, buggy worker).
#[tokio::test]
async fn test_forward_log_batch_unknown_drv_path_dropped() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let mut events =
        merge_single_node(&handle, build_id, "knowndrv", PriorityClass::Scheduled).await?;

    // Drain merge-time events (through InputsResolved — last merge-time event).
    loop {
        let ev = events.recv().await?;
        if matches!(
            ev.event,
            Some(rio_proto::types::build_event::Event::InputsResolved(_))
        ) {
            break;
        }
    }

    // Send a log batch for a drv_path that is NOT in the DAG.
    handle
        .send_unchecked(ActorCommand::ForwardLogBatch {
            drv_path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-not-in-dag.drv".into(),
            batch: rio_proto::types::BuildLogBatch {
                derivation_path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-not-in-dag.drv"
                    .into(),
                lines: vec![b"orphan".to_vec()],
                first_line_number: 0,
                executor_id: "w1".into(),
            },
        })
        .await?;

    // Then send a log for the KNOWN drv to prove the actor is still alive
    // and processing — if the unknown batch panicked it, this would hang.
    handle
        .send_unchecked(ActorCommand::ForwardLogBatch {
            drv_path: test_drv_path("knowndrv"),
            batch: rio_proto::types::BuildLogBatch {
                derivation_path: test_drv_path("knowndrv"),
                lines: vec![b"sentinel".to_vec()],
                first_line_number: 0,
                executor_id: "w1".into(),
            },
        })
        .await?;

    // Only the known-drv batch produces an event. If we got the orphan's
    // event, it would have a different derivation_path.
    let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await??;
    match ev.event {
        Some(rio_proto::types::build_event::Event::Log(got)) => {
            assert_eq!(
                got.derivation_path,
                test_drv_path("knowndrv"),
                "orphan batch should have been dropped, not forwarded"
            );
            assert_eq!(got.lines[0], b"sentinel");
        }
        other => panic!("expected Log event, got {other:?}"),
    }
    Ok(())
}

// r[verify sched.merge.dedup]
/// Two builds interested in the same derivation (DAG-merged) → ForwardLogBatch
/// emits on BOTH broadcast channels. This is the "derivation built once, N
/// builds care" case that makes DAG merging valuable.
#[tokio::test]
async fn test_forward_log_batch_fanout_to_multiple_interested_builds() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Two builds, SAME derivation tag → DAG merge dedupes to one node with
    // interested_builds = {build1, build2}.
    let build1 = Uuid::new_v4();
    let mut events1 =
        merge_single_node(&handle, build1, "shared-drv", PriorityClass::Scheduled).await?;
    let build2 = Uuid::new_v4();
    let mut events2 =
        merge_single_node(&handle, build2, "shared-drv", PriorityClass::Scheduled).await?;

    // Drain merge-time events on both (through InputsResolved).
    for events in [&mut events1, &mut events2] {
        loop {
            let ev = events.recv().await?;
            if matches!(
                ev.event,
                Some(rio_proto::types::build_event::Event::InputsResolved(_))
            ) {
                break;
            }
        }
    }

    handle
        .send_unchecked(ActorCommand::ForwardLogBatch {
            drv_path: test_drv_path("shared-drv"),
            batch: rio_proto::types::BuildLogBatch {
                derivation_path: test_drv_path("shared-drv"),
                lines: vec![b"fanout-line".to_vec()],
                first_line_number: 0,
                executor_id: "w1".into(),
            },
        })
        .await?;

    // BOTH streams should receive the log.
    for (label, events, expected_build) in [
        ("build1", &mut events1, build1),
        ("build2", &mut events2, build2),
    ] {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap_or_else(|_| panic!("{label} timed out waiting for Log event"))?;
        match ev.event {
            Some(rio_proto::types::build_event::Event::Log(got)) => {
                assert_eq!(got.lines[0], b"fanout-line", "{label}");
            }
            other => panic!("{label}: expected Log event, got {other:?}"),
        }
        assert_eq!(
            ev.build_id,
            expected_build.to_string(),
            "{label}: event should carry its own build_id, not the other's"
        );
    }
    Ok(())
}
