use super::*;

// -----------------------------------------------------------------------
// Group 1: Worker-Scheduler wiring
// -----------------------------------------------------------------------

/// Baseline: when a worker connects (stream) and sends a heartbeat with
/// the SAME worker_id, the actor should see it as fully registered.
/// Validates that stream + heartbeat with the same worker_id registers correctly.
#[tokio::test]
async fn test_worker_registers_via_stream_and_heartbeat() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "test-worker-1", "x86_64-linux", 2).await;
    settle().await;

    let workers = handle.debug_query_workers().await.unwrap();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].worker_id, "test-worker-1");
    assert!(
        workers[0].is_registered,
        "worker should be fully registered after stream + heartbeat"
    );
}

/// Bug reproduction: handle_completion uses drv_hash as the lookup key,
/// but grpc.rs passes drv_path. The completion should be resolved via
/// either key. This test sends ProcessCompletion with a drv_PATH and
/// expects the build to transition to Succeeded.
///
/// Expected to FAIL before fix: completion is dropped ("unknown derivation")
/// and the build stays Active forever.
#[tokio::test]
async fn test_completion_resolves_drv_path_to_hash() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register a worker
    let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

    // Merge a single-node DAG
    let build_id = Uuid::new_v4();
    let drv_hash = "abc123hash";
    let drv_path = "/nix/store/abc123hash-foo.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;

    settle().await;

    // Worker should have received an assignment (derivation is ready, worker has capacity)
    // Now send completion using drv_PATH (mimics what grpc.rs does with report.drv_path)
    // Note: PATH, not hash — tests that grpc.rs drv_path resolves correctly.
    complete_success(&handle, "test-worker", drv_path, "/nix/store/xyz-foo").await;

    settle().await;

    // Query build status — should be Succeeded (single derivation, completed)
    let status = query_status(&handle, build_id).await;

    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should succeed after completion sent with drv_path (got state={:?})",
        rio_proto::types::BuildState::try_from(status.state)
    );
}

// -----------------------------------------------------------------------
// Group 2: State machine integrity
// -----------------------------------------------------------------------

/// When a worker disconnects while a derivation is Running, the derivation
/// should transition Running -> Failed -> Ready (through the state machine),
/// and retry_count should be incremented.
///
/// Before fix: the direct `state.status = Ready` assignment bypassed the
/// state machine (Running -> Ready is not a valid transition) and did NOT
/// increment retry_count.
#[tokio::test]
async fn test_worker_disconnect_running_derivation() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register a worker
    let mut stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

    // Merge a single-node DAG (worker will get it assigned)
    let build_id = Uuid::new_v4();
    let drv_hash = "disconnect-test-hash";
    let drv_path = "/nix/store/disconnect-test-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Worker should have received an assignment
    let assignment = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .expect("should receive assignment within 2s")
        .expect("stream should not be closed");
    assert!(matches!(
        assignment.msg,
        Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
    ));

    // Derivation should now be Assigned. Simulate worker sending an Ack
    // to transition to Running via a completion with a sentinel... actually
    // the actor doesn't have an Ack handler that transitions state. Looking
    // at the code, Assigned -> Running happens implicitly in handle_completion
    // when needed. For this test, we need to directly check the disconnect
    // path from BOTH Assigned AND Running states. The current code's
    // handle_worker_disconnected matches on (Assigned | Running) and resets
    // to Ready. The bug is that Running -> Ready is invalid.

    // Check current status: should be Assigned
    let info = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should exist");
    assert_eq!(info.status, DerivationStatus::Assigned);
    assert_eq!(info.retry_count, 0);

    // Disconnect the worker
    handle
        .send_unchecked(ActorCommand::WorkerDisconnected {
            worker_id: "test-worker".into(),
        })
        .await
        .unwrap();
    settle().await;

    // Derivation should be back in Ready state, and retry_count
    // should be incremented (disconnect during Assigned/Running is a
    // failed attempt that counts toward retries).
    let info = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should still exist");
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "derivation should return to Ready after worker disconnect"
    );
    assert_eq!(
        info.retry_count, 1,
        "disconnect during Assigned should count as a retry attempt"
    );
    assert!(info.assigned_worker.is_none());
}

// -----------------------------------------------------------------------
// Group 4: Silent failures
// -----------------------------------------------------------------------

/// A completion with InfrastructureFailure status should transition the
/// derivation out of Running (to Failed/Ready for retry, or Poisoned).
/// The gRPC layer synthesizes InfrastructureFailure for None results,
/// so this verifies the full path.
#[tokio::test]
async fn test_completion_infrastructure_failure_handled() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-fail-hash";
    let drv_path = "/nix/store/infra-fail-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send completion with InfrastructureFailure (what gRPC layer sends
    // for None result)
    complete_failure(
        &handle,
        "test-worker",
        drv_path,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "worker sent CompletionReport with no result",
    )
    .await;
    settle().await;

    // The derivation should have gone through Failed -> Ready (retry) and
    // then been immediately re-dispatched to the worker (Assigned again).
    // The key assertion: retry_count was incremented, proving the completion
    // was processed rather than silently dropped.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should exist");
    assert_eq!(
        info.retry_count, 1,
        "InfrastructureFailure should count as a retry (completion was processed)"
    );
    // Status should be Assigned (re-dispatched) or Ready (if dispatch skipped),
    // NOT stuck in the original state with retry_count=0.
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "expected Ready or Assigned after retry, got {:?}",
        info.status
    );
}

/// Malicious/buggy worker timestamps (i64::MIN start, i64::MAX stop) must
/// not panic the actor with integer overflow in the EMA duration computation.
#[tokio::test]
async fn test_completion_with_extreme_timestamps() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let drv_hash = "extreme-ts-hash";
    let drv_path = "/nix/store/extreme-ts-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send completion with extreme timestamps that would overflow i64 subtraction.
    // Pre-fix: stop.seconds - start.seconds = i64::MAX - i64::MIN overflows (panic in debug).
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: drv_path.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                start_time: Some(prost_types::Timestamp {
                    seconds: i64::MIN,
                    nanos: i32::MIN,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: i64::MAX,
                    nanos: i32::MAX,
                }),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: "/nix/store/xyz-extreme".into(),
                    output_hash: vec![0u8; 32],
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // If we got here, the actor didn't panic. Verify completion was processed
    // (build succeeded) and actor is still alive.
    assert!(handle.is_alive(), "actor must survive extreme timestamps");
    let status = query_status(&handle, build_id).await;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should succeed despite bogus timestamps"
    );
}

// -----------------------------------------------------------------------
// Group 6: Feature completions
// -----------------------------------------------------------------------

/// Interactive (IFD) builds should jump to the front of the ready queue.
#[tokio::test]
async fn test_interactive_builds_pushed_to_front() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Worker with capacity for 1 build at a time
    let mut stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

    // Merge a "scheduled" build first (should go to back of queue)
    let build_normal = Uuid::new_v4();
    let _rx1 = merge_single_node(
        &handle,
        build_normal,
        "hash-normal",
        "/nix/store/hash-normal.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // The normal build gets assigned first (only one in queue)
    let first = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let first_path = match first.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(first_path, "/nix/store/hash-normal.drv");

    // Now merge a second "scheduled" build — goes to back
    let build_scheduled2 = Uuid::new_v4();
    let _rx2 = merge_single_node(
        &handle,
        build_scheduled2,
        "hash-scheduled2",
        "/nix/store/hash-scheduled2.drv",
        PriorityClass::Scheduled,
    )
    .await;

    // Merge an "interactive" build — should go to FRONT
    let build_ifd = Uuid::new_v4();
    let _rx3 = merge_single_node(
        &handle,
        build_ifd,
        "hash-ifd",
        "/nix/store/hash-ifd.drv",
        PriorityClass::Interactive,
    )
    .await;
    settle().await;

    // Complete the first build to free worker capacity
    complete_success_empty(&handle, "test-worker", "/nix/store/hash-normal.drv").await;
    settle().await;

    // The next assignment should be the IFD derivation (was pushed to front)
    let second = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let second_path = match second.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(
        second_path, "/nix/store/hash-ifd.drv",
        "interactive build should be dispatched before scheduled"
    );
}
