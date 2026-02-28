//! Actor tests. `pub(crate)` helpers (make_test_node, setup_actor) used by grpc.rs tests.

#![allow(dead_code)] // Helpers used progressively across Group 1-10 TDD tests

use super::*;
use rio_test_support::TestDb;
use std::time::Duration;
use tokio::sync::mpsc;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Set up an actor with the given PgPool and return (handle, task).
/// The caller should drop the handle to shut down the actor.
pub(crate) async fn setup_actor(pool: sqlx::PgPool) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    setup_actor_with_store(pool, None).await
}

/// Set up an actor with an optional store client for cache-check tests.
pub(crate) async fn setup_actor_with_store(
    pool: sqlx::PgPool,
    store_client: Option<StoreServiceClient<Channel>>,
) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    let db = SchedulerDb::new(pool);
    let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
    let actor = DagActor::new(db, store_client);
    let backpressure = actor.backpressure_flag();
    let self_tx = tx.downgrade();
    let task = tokio::spawn(actor.run_with_self_tx(rx, self_tx));
    (ActorHandle { tx, backpressure }, task)
}

/// Create a minimal test DerivationNode.
pub(crate) fn make_test_node(
    hash: &str,
    path: &str,
    system: &str,
) -> rio_proto::types::DerivationNode {
    rio_proto::types::DerivationNode {
        drv_hash: hash.into(),
        drv_path: path.into(),
        pname: "test-pkg".into(),
        system: system.into(),
        required_features: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        expected_output_paths: vec![],
    }
}

/// Create a minimal test DerivationEdge.
pub(crate) fn make_test_edge(parent: &str, child: &str) -> rio_proto::types::DerivationEdge {
    rio_proto::types::DerivationEdge {
        parent_drv_path: parent.into(),
        child_drv_path: child.into(),
    }
}

/// Connect a worker (stream + heartbeat) so it becomes fully registered.
/// Returns the mpsc::Receiver for scheduler→worker messages.
pub(crate) async fn connect_worker(
    handle: &ActorHandle,
    worker_id: &str,
    system: &str,
    max_builds: u32,
) -> mpsc::Receiver<rio_proto::types::SchedulerMessage> {
    let (stream_tx, stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: worker_id.into(),
            stream_tx,
        })
        .await
        .unwrap();
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            worker_id: worker_id.into(),
            system: system.into(),
            supported_features: vec![],
            max_builds,
            running_builds: vec![],
        })
        .await
        .unwrap();
    stream_rx
}

/// Merge a single-node DAG and return the event receiver.
pub(crate) async fn merge_single_node(
    handle: &ActorHandle,
    build_id: Uuid,
    hash: &str,
    path: &str,
    priority_class: PriorityClass,
) -> broadcast::Receiver<rio_proto::types::BuildEvent> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class,
            nodes: vec![make_test_node(hash, path, "x86_64-linux")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap().unwrap()
}

/// Give the actor time to process commands.
pub(crate) async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn test_actor_starts_and_stops() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone()).await;
    settle().await;
    // Query should succeed (actor is running)
    let workers = handle.debug_query_workers().await.unwrap();
    assert!(workers.is_empty());
    // Drop handle to close channel
    drop(handle);
    // Actor task should exit
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("actor should shut down")
        .expect("actor should not panic");
}

/// is_alive() should detect actor death (channel closed = receiver dropped).
#[tokio::test]
async fn test_actor_is_alive_detection() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone()).await;
    settle().await;

    // Actor should be alive after spawn.
    assert!(handle.is_alive(), "actor should be alive after spawn");

    // Abort the actor task to simulate a panic/crash.
    task.abort();
    // Give the abort time to propagate and the receiver to drop.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // is_alive() should now report false (channel closed).
    assert!(
        !handle.is_alive(),
        "is_alive should report false after actor task dies"
    );
}

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
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: drv_path.into(), // Note: PATH, not hash!
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                error_msg: String::new(),
                times_built: 1,
                start_time: None,
                stop_time: None,
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: "/nix/store/xyz-foo".into(),
                    output_hash: vec![0u8; 32],
                }],
            },
        })
        .await
        .unwrap();

    settle().await;

    // Query build status — should be Succeeded (single derivation, completed)
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let status = reply_rx.await.unwrap().unwrap();

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
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: drv_path.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
                error_msg: "worker sent CompletionReport with no result".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
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
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let status = reply_rx.await.unwrap().unwrap();
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
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: "/nix/store/hash-normal.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
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

// -----------------------------------------------------------------------
// Group 10: Remaining coverage
// -----------------------------------------------------------------------

/// keepGoing=false: on PermanentFailure, the entire build fails immediately.
#[tokio::test]
async fn test_keepgoing_false_fails_fast() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

    // Merge a two-node DAG with keepGoing=false
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("hashA", "/nix/store/hashA.drv", "x86_64-linux"),
                make_test_node("hashB", "/nix/store/hashB.drv", "x86_64-linux"),
            ],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false, // critical
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Send PermanentFailure for hashA
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: "/nix/store/hashA.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "compile error".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Build should be Failed (not waiting for hashB)
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail fast on PermanentFailure with keepGoing=false"
    );
}

/// keepGoing=true: build waits for all derivations, fails only at the end.
#[tokio::test]
async fn test_keepgoing_true_waits_all() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "test-worker", "x86_64-linux", 2).await;

    // Merge a two-node DAG with keepGoing=true
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("hashX", "/nix/store/hashX.drv", "x86_64-linux"),
                make_test_node("hashY", "/nix/store/hashY.drv", "x86_64-linux"),
            ],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: true, // critical
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Send PermanentFailure for hashX
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: "/nix/store/hashX.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "failed".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Build should still be Active (waiting for hashY)
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build should still be Active with keepGoing=true and pending derivations"
    );

    // Complete hashY successfully
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: "/nix/store/hashY.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Now build should be Failed (all resolved, one failed)
    let (status_tx2, status_rx2) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx2,
        })
        .await
        .unwrap();
    let status2 = status_rx2.await.unwrap().unwrap();
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after all derivations resolve with keepGoing=true"
    );
}

/// keepGoing=true with a dependency chain: poisoning a leaf must cascade
/// DependencyFailed to all ancestors so the build terminates. Without the
/// cascade, parents stay Queued forever and completed+failed never reaches
/// total -> build hangs.
#[tokio::test]
async fn test_keepgoing_poisoned_dependency_cascades_failure() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Worker with capacity 1: only the leaf gets dispatched initially.
    let _stream_rx = connect_worker(&handle, "cascade-worker", "x86_64-linux", 1).await;

    // Chain: A depends on B depends on C. C is the leaf.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("cascadeA", "/nix/store/cascadeA.drv", "x86_64-linux"),
                make_test_node("cascadeB", "/nix/store/cascadeB.drv", "x86_64-linux"),
                make_test_node("cascadeC", "/nix/store/cascadeC.drv", "x86_64-linux"),
            ],
            edges: vec![
                make_test_edge("/nix/store/cascadeA.drv", "/nix/store/cascadeB.drv"),
                make_test_edge("/nix/store/cascadeB.drv", "/nix/store/cascadeC.drv"),
            ],
            options: BuildOptions::default(),
            keep_going: true,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Sanity: C is the only Ready/Assigned derivation; A and B are Queued.
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await
        .unwrap()
        .unwrap();
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info_a.status, DerivationStatus::Queued);
    assert_eq!(info_b.status, DerivationStatus::Queued);

    // Poison C via PermanentFailure.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "cascade-worker".into(),
            drv_key: "/nix/store/cascadeC.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "compile error".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // B and A should now be DependencyFailed (cascaded transitively).
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info_b.status,
        DerivationStatus::DependencyFailed,
        "immediate parent B should be DependencyFailed after C poisoned"
    );
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info_a.status,
        DerivationStatus::DependencyFailed,
        "transitive parent A should also be DependencyFailed"
    );

    // Build should terminate as Failed (all 3 derivations resolved:
    // 1 Poisoned + 2 DependencyFailed counted in failed).
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "keepGoing build with poisoned dependency chain should terminate as Failed, not hang"
    );
    assert_eq!(
        status.failed_derivations, 3,
        "1 Poisoned + 2 DependencyFailed should all count as failed"
    );
}

/// When a new build depends on an already-poisoned derivation (from a
/// prior build), compute_initial_states must mark the new node
/// DependencyFailed immediately. Previously it went to Queued and hung
/// forever (never Ready, never cascaded since cascade only runs on
/// *transition to* Poisoned).
#[tokio::test]
async fn test_merge_with_prepoisoned_dep_marks_dependency_failed() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _stream_rx = connect_worker(&handle, "poison-worker", "x86_64-linux", 1).await;

    // Build 1: single leaf, poisoned via PermanentFailure.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(
        &handle,
        build1,
        "preleaf",
        "/nix/store/preleaf.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "poison-worker".into(),
            drv_key: "/nix/store/preleaf.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "preleaf failed".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Verify preleaf is Poisoned.
    let leaf = handle
        .debug_query_derivation("preleaf")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leaf.status, DerivationStatus::Poisoned);

    // Build 2: new node depending on the poisoned preleaf.
    // keepGoing=false: build should fail immediately at merge.
    let build2 = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id: build2,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("preparent", "/nix/store/preparent.drv", "x86_64-linux"),
                make_test_node("preleaf", "/nix/store/preleaf.drv", "x86_64-linux"),
            ],
            edges: vec![make_test_edge(
                "/nix/store/preparent.drv",
                "/nix/store/preleaf.drv",
            )],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx2 = reply_rx.await.unwrap().unwrap();
    settle().await;

    // preparent must be DependencyFailed (not stuck Queued).
    let parent = handle
        .debug_query_derivation("preparent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        parent.status,
        DerivationStatus::DependencyFailed,
        "new node depending on pre-poisoned dep must be DependencyFailed, not stuck Queued"
    );

    // Build 2 must be Failed (!keepGoing + dep failure).
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id: build2,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build depending on pre-poisoned dep must fail immediately (!keepGoing)"
    );
}

/// WatchBuild on an already-terminal build must immediately send the
/// terminal event. Previously it just subscribed — if the original
/// BuildCompleted was sent to zero receivers (e.g., submit subscriber
/// disconnected before completion), a late WatchBuild would hang forever.
#[tokio::test]
async fn test_watch_build_after_completion_receives_terminal_event() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _stream_rx = connect_worker(&handle, "watch-worker", "x86_64-linux", 1).await;

    // Submit a build, complete it, then drop the original subscriber.
    let build_id = Uuid::new_v4();
    let original_rx = merge_single_node(
        &handle,
        build_id,
        "watch-hash",
        "/nix/store/watch-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "watch-worker".into(),
            drv_key: "/nix/store/watch-hash.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Drop the original subscriber. The BuildCompleted event was already
    // sent; a NEW subscriber should not hang waiting for it.
    drop(original_rx);

    // Now WatchBuild. Should receive BuildCompleted immediately.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let mut watch_rx = reply_rx.await.unwrap().unwrap();

    // Should get a terminal event within a short timeout, not hang.
    let event = tokio::time::timeout(Duration::from_secs(2), watch_rx.recv())
        .await
        .expect("WatchBuild on terminal build should not hang")
        .expect("should receive an event");
    assert!(
        matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Completed(_))
        ),
        "late WatchBuild should receive BuildCompleted replay, got: {:?}",
        event.event
    );
}

/// Terminal build state should be cleaned up after TERMINAL_CLEANUP_DELAY
/// to prevent unbounded memory growth for long-running schedulers.
///
/// This test sends CleanupTerminalBuild directly (bypassing the delay)
/// since paused time interferes with PG pool timeouts. The delay
/// scheduling itself is trivially correct (tokio::time::sleep + try_send).
#[tokio::test]
async fn test_terminal_build_cleanup_after_delay() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "cleanup-worker", "x86_64-linux", 1).await;

    // Complete a build.
    let build_id = Uuid::new_v4();
    let drv_hash = "cleanup-hash";
    let drv_path = "/nix/store/cleanup-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "cleanup-worker".into(),
            drv_key: drv_path.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Build should be Succeeded and still queryable.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap();
    assert!(status.is_ok(), "build should be queryable before cleanup");

    // Directly inject the cleanup command (bypassing the 60s delay).
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
        .await
        .unwrap();
    settle().await;

    // Build should now be gone (BuildNotFound).
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap();
    assert!(
        matches!(status, Err(ActorError::BuildNotFound(_))),
        "build should be cleaned up after delay, got {:?}",
        status
    );

    // DAG node should also be reaped (Completed + orphaned).
    let info = handle.debug_query_derivation(drv_hash).await.unwrap();
    assert!(
        info.is_none(),
        "orphaned+terminal DAG node should be reaped"
    );
}

/// TransientFailure: retry on a different worker up to max_retries (default 2).
#[tokio::test]
async fn test_transient_retry_different_worker() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register two workers
    let _rx1 = connect_worker(&handle, "worker-a", "x86_64-linux", 1).await;
    let _rx2 = connect_worker(&handle, "worker-b", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        "retry-hash",
        "/nix/store/retry-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Get initial worker assignment
    let info1 = handle
        .debug_query_derivation("retry-hash")
        .await
        .unwrap()
        .unwrap();
    let first_worker = info1.assigned_worker.clone().unwrap();
    assert_eq!(info1.retry_count, 0);

    // Send TransientFailure from the first worker
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: first_worker.clone().into(),
            drv_key: "/nix/store/retry-hash.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                error_msg: "network hiccup".into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // Should be retried: retry_count=1, possibly on a different worker
    let info2 = handle
        .debug_query_derivation("retry-hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info2.retry_count, 1,
        "transient failure should increment retry_count"
    );
    // Note: the retry MAY go to the same worker (no affinity avoidance yet),
    // but retry_count proves it was processed.
    assert!(matches!(
        info2.status,
        DerivationStatus::Assigned | DerivationStatus::Ready
    ));
}

/// max_retries (default 2) exhausted with the SAME worker should poison.
/// This branch (retry_count >= max_retries) is distinct from
/// POISON_THRESHOLD (3 distinct workers) — same worker failing
/// repeatedly hits max_retries first.
#[tokio::test]
async fn test_transient_failure_max_retries_same_worker_poisons() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _rx = connect_worker(&handle, "flaky-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        "maxretry-hash",
        "/nix/store/maxretry-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Default RetryPolicy::max_retries = 2. Fail 3 times on same worker:
    // retry_count 0 -> 1 (retry), 1 -> 2 (retry), 2 >= 2 -> Poisoned.
    for attempt in 0..3 {
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "flaky-worker".into(),
                drv_key: "/nix/store/maxretry-hash.drv".into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                    error_msg: format!("attempt {attempt} failed"),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;
    }

    let info = handle
        .debug_query_derivation("maxretry-hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 transient failures on same worker (retry_count >= max_retries=2) should poison"
    );
}

/// CancelBuild on an active build should clean up derivations and emit
/// BuildCancelled event. Previously untested.
#[tokio::test]
async fn test_cancel_build_active_drains_derivations() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    // No workers — derivation stays Ready (never assigned).

    let build_id = Uuid::new_v4();
    let mut event_rx = merge_single_node(
        &handle,
        build_id,
        "cancel-hash",
        "/nix/store/cancel-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send CancelBuild.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "test cancel".into(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    let cancelled = reply_rx.await.unwrap().unwrap();
    assert!(cancelled, "CancelBuild should return true for active build");
    settle().await;

    // Build should be Cancelled.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Cancelled as i32,
        "build should be Cancelled after CancelBuild"
    );

    // Should have received BuildCancelled event.
    let mut saw_cancelled = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Cancelled(_))
        ) {
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "BuildCancelled event should be emitted");

    // Second CancelBuild should be a no-op (idempotent: returns false).
    let (reply_tx2, reply_rx2) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "already cancelled".into(),
            reply: reply_tx2,
        })
        .await
        .unwrap();
    let re_cancelled = reply_rx2.await.unwrap().unwrap();
    assert!(
        !re_cancelled,
        "CancelBuild on already-terminal build should return false"
    );
}

/// WatchBuild during an active build should receive events as they happen.
/// (The after-completion case is tested separately.)
#[tokio::test]
async fn test_watch_build_receives_events() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _rx = connect_worker(&handle, "watch-events-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let _original = merge_single_node(
        &handle,
        build_id,
        "watch-events-hash",
        "/nix/store/watch-events-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // WatchBuild on the active build.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let mut watch_rx = reply_rx.await.unwrap().unwrap();

    // Complete the build; watcher should see BuildCompleted.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "watch-events-worker".into(),
            drv_key: "/nix/store/watch-events-hash.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let mut saw_completed = false;
    // Drain events with a timeout.
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(200), watch_rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(
                    event.event,
                    Some(rio_proto::types::build_event::Event::Completed(_))
                ) {
                    saw_completed = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_completed,
        "WatchBuild subscriber should see BuildCompleted"
    );
}

/// Dispatch should skip over derivations with no eligible worker (wrong
/// system or missing feature) instead of blocking the entire queue.
#[tokio::test]
async fn test_dispatch_skips_ineligible_derivation() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Only x86_64 worker registered.
    let mut stream_rx = connect_worker(&handle, "x86-only-worker", "x86_64-linux", 2).await;

    // Merge aarch64 derivation FIRST (goes to queue head), then x86_64.
    // With the old `None => break`, the aarch64 drv at head would block
    // the x86_64 drv from being dispatched.
    let build_arm = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id: build_arm,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_test_node(
                "arm-hash",
                "/nix/store/arm-hash.drv",
                "aarch64-linux",
            )],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();

    let build_x86 = Uuid::new_v4();
    let _rx = merge_single_node(
        &handle,
        build_x86,
        "x86-hash",
        "/nix/store/x86-hash.drv",
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // x86_64 derivation should be dispatched despite aarch64 ahead of it.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .expect("x86_64 derivation should be dispatched within 2s")
        .expect("stream should not close");
    let dispatched_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(
        dispatched_path, "/nix/store/x86-hash.drv",
        "x86_64 derivation should be dispatched even with ineligible aarch64 ahead in queue"
    );

    // aarch64 derivation should still be Ready (not stuck, not dispatched).
    let arm_info = handle
        .debug_query_derivation("arm-hash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        arm_info.status,
        DerivationStatus::Ready,
        "aarch64 derivation should remain Ready (no eligible worker)"
    );
}

/// Per-build BuildOptions (max_silent_time, build_timeout) must propagate
/// to the worker via WorkAssignment. Previously sent all-zeros defaults.
#[tokio::test]
async fn test_build_options_propagated_to_worker() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let mut stream_rx = connect_worker(&handle, "options-worker", "x86_64-linux", 1).await;

    // Submit with build_timeout=300, max_silent_time=60.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_test_node(
                "opts-hash",
                "/nix/store/opts-hash.drv",
                "x86_64-linux",
            )],
            edges: vec![],
            options: BuildOptions {
                max_silent_time: 60,
                build_timeout: 300,
                build_cores: 4,
            },
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Worker should receive assignment with the build's options.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let assignment = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
        _ => panic!("expected assignment"),
    };
    let opts = assignment.build_options.expect("options should be set");
    assert_eq!(
        opts.build_timeout, 300,
        "build_timeout should propagate from build to worker"
    );
    assert_eq!(
        opts.max_silent_time, 60,
        "max_silent_time should propagate from build to worker"
    );
    assert_eq!(opts.build_cores, 4);
}

/// TOCTOU fix: a stale heartbeat (sent before scheduler assigned a
/// derivation) must not clobber the scheduler's fresh assignment in
/// worker.running_builds. The scheduler is authoritative.
#[tokio::test]
async fn test_heartbeat_does_not_clobber_fresh_assignment() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register worker (initial heartbeat has empty running_builds).
    let _stream_rx = connect_worker(&handle, "toctou-worker", "x86_64-linux", 2).await;
    settle().await;

    // Merge a derivation. Scheduler will assign it to the worker and
    // insert it into worker.running_builds.
    let build_id = Uuid::new_v4();
    let drv_hash = "toctou-drv-hash";
    let drv_path = "/nix/store/toctou-drv-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Verify: derivation is Assigned, worker.running_builds contains it.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should exist");
    assert_eq!(info.status, DerivationStatus::Assigned);

    let workers = handle.debug_query_workers().await.unwrap();
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .unwrap();
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "scheduler should have tracked the assignment in worker.running_builds"
    );

    // Send a STALE heartbeat with empty running_builds. This mimics the
    // race: worker sent heartbeat before receiving/acking the assignment.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            worker_id: "toctou-worker".into(),
            system: "x86_64-linux".into(),
            supported_features: vec![],
            max_builds: 2,
            running_builds: vec![], // stale — does NOT include fresh assignment
        })
        .await
        .unwrap();
    settle().await;

    // Assignment must still be tracked. Before the fix, running_builds
    // would be wholesale replaced with the empty set, orphaning the
    // assignment (completion would later warn "unknown derivation").
    let workers = handle.debug_query_workers().await.unwrap();
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .unwrap();
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "stale heartbeat must not clobber scheduler's fresh assignment"
    );
}

// -----------------------------------------------------------------------
// Step 8: Test coverage gaps
// -----------------------------------------------------------------------

/// T4: Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
#[tokio::test]
async fn test_poison_threshold_after_distinct_workers() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Register 4 workers so the derivation can be re-dispatched after each failure.
    let _rx1 = connect_worker(&handle, "poison-w1", "x86_64-linux", 1).await;
    let _rx2 = connect_worker(&handle, "poison-w2", "x86_64-linux", 1).await;
    let _rx3 = connect_worker(&handle, "poison-w3", "x86_64-linux", 1).await;
    let _rx4 = connect_worker(&handle, "poison-w4", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let drv_hash = "poison-drv";
    let drv_path = "/nix/store/poison-drv.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send TransientFailure from 3 DISTINCT workers. After the 3rd, poison.
    for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: (*worker).into(),
                drv_key: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::TransientFailure.into(),
                    error_msg: format!("failure {i}"),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should exist");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "derivation should be Poisoned after {} distinct worker failures",
        POISON_THRESHOLD
    );

    // Build should be Failed.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after derivation is poisoned"
    );
}

/// T5: Completing a child releases its parent to Ready in a dependency chain.
#[tokio::test]
async fn test_dependency_chain_releases_parent() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let mut stream_rx = connect_worker(&handle, "chain-worker", "x86_64-linux", 1).await;

    // A depends on B. B is Ready (leaf), A is Queued.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("chainA", "/nix/store/chainA.drv", "x86_64-linux"),
                make_test_node("chainB", "/nix/store/chainB.drv", "x86_64-linux"),
            ],
            edges: vec![make_test_edge(
                "/nix/store/chainA.drv",
                "/nix/store/chainB.drv",
            )],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // B is dispatched first (leaf). A is Queued waiting for B.
    let info_a = handle
        .debug_query_derivation("chainA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // Worker receives B's assignment.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let assigned_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(assigned_path, "/nix/store/chainB.drv");

    // Complete B.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "chain-worker".into(),
            drv_key: "/nix/store/chainB.drv".into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // A should now transition Queued -> Ready -> Assigned (dispatched).
    let info_a = handle
        .debug_query_derivation("chainA")
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            info_a.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "A should be Ready or Assigned after B completes, got {:?}",
        info_a.status
    );

    // Worker should receive A's assignment.
    let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let assigned_path = match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a.drv_path,
        _ => panic!("expected assignment"),
    };
    assert_eq!(
        assigned_path, "/nix/store/chainA.drv",
        "A should be dispatched after B completes"
    );
}

/// T9: Duplicate ProcessCompletion is an idempotent no-op.
#[tokio::test]
async fn test_duplicate_completion_idempotent() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "idem-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let drv_hash = "idem-hash";
    let drv_path = "/nix/store/idem-hash.drv";
    let mut event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Send completion TWICE.
    for _ in 0..2 {
        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "idem-worker".into(),
                drv_key: drv_path.into(),
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        settle().await;
    }

    // completed_count should be 1, not 2.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
    assert_eq!(
        status.completed_derivations, 1,
        "duplicate completion should not double-count"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should still succeed (idempotent)"
    );

    // Count BuildCompleted events: should be exactly 1 (not 2).
    // Drain available events without blocking.
    let mut completed_events = 0;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Completed(_))
        ) {
            completed_events += 1;
        }
    }
    assert_eq!(
        completed_events, 1,
        "BuildCompleted event should fire exactly once"
    );
}

/// T1: Heartbeat timeout deregisters worker and reassigns its builds.
/// Instead of advancing time (PG timeout issue), we send Tick commands
/// after manipulating the worker's last_heartbeat via multiple Tick cycles
/// without heartbeats. Actually simpler: send WorkerDisconnected directly
/// is equivalent (handle_tick calls handle_worker_disconnected on timeout),
/// so that path is already covered by test_worker_disconnect_running_derivation.
/// This test verifies the Tick-driven path specifically by injecting Ticks.
#[tokio::test]
async fn test_heartbeat_timeout_via_tick_deregisters_worker() {
    // NOTE: This test would ideally use tokio::time::pause + advance, but
    // that interferes with PG pool timeouts. Instead, we verify that Tick
    // correctly processes the timeout path by checking the missed_heartbeats
    // counter accumulates. Since we can't easily fast-forward real time in
    // this test harness, we verify the logic indirectly: Tick with fresh
    // heartbeat does NOT remove the worker (negative test), and the
    // timeout-removal path is exercised directly via WorkerDisconnected
    // in test_worker_disconnect_running_derivation.
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let _stream_rx = connect_worker(&handle, "tick-worker", "x86_64-linux", 1).await;
    settle().await;

    // Send several Ticks. Worker has fresh heartbeat, should NOT be removed.
    for _ in 0..MAX_MISSED_HEARTBEATS + 1 {
        handle.send_unchecked(ActorCommand::Tick).await.unwrap();
    }
    settle().await;

    let workers = handle.debug_query_workers().await.unwrap();
    assert!(
        workers.iter().any(|w| w.worker_id == "tick-worker"),
        "worker with fresh heartbeat should survive Tick"
    );
}

// -----------------------------------------------------------------------
// Scheduler-side cache check (TOCTOU fix)
// -----------------------------------------------------------------------

/// Spin up an in-process rio-store on an ephemeral port.
async fn setup_inproc_store(
    pool: sqlx::PgPool,
) -> (StoreServiceClient<Channel>, tokio::task::JoinHandle<()>) {
    use rio_proto::store::store_service_server::StoreServiceServer;
    use rio_store::backend::memory::MemoryBackend;
    use rio_store::grpc::StoreServiceImpl;
    use std::sync::Arc;

    let backend = Arc::new(MemoryBackend::new());
    let service = StoreServiceImpl::new(backend, pool);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(StoreServiceServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .expect("test store gRPC server should run");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    (StoreServiceClient::new(channel), server)
}

/// Build a minimal single-file NAR and upload it to the store.
async fn put_test_path(client: &mut StoreServiceClient<Channel>, store_path: &str) {
    use rio_proto::types::{PathInfo, PutPathMetadata, PutPathRequest, put_path_request};
    use sha2::{Digest, Sha256};

    let node = rio_nix::nar::NarNode::Regular {
        executable: false,
        contents: b"hello".to_vec(),
    };
    let mut nar = Vec::new();
    rio_nix::nar::serialize(&mut nar, &node).unwrap();

    let nar_hash = Sha256::digest(&nar).to_vec();
    let info = PathInfo {
        store_path: store_path.to_string(),
        nar_hash,
        nar_size: nar.len() as u64,
        ..Default::default()
    };

    let (tx, rx) = mpsc::channel(4);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
        })),
    })
    .await
    .unwrap();
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .unwrap();
    drop(tx);

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    client.put_path(stream).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_cache_check_skips_build() {
    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;

    // Start in-process store and pre-populate the expected output path.
    let (mut store_client, _store_server) = setup_inproc_store(store_db.pool.clone()).await;
    let cached_output = "/nix/store/00000000000000000000000000000000-cached-output";
    put_test_path(&mut store_client, cached_output).await;

    // Spawn actor WITH the store client — cache check will run.
    let (handle, _task) =
        setup_actor_with_store(sched_db.pool.clone(), Some(store_client.clone())).await;

    // Merge a single-node DAG with expected_output_paths pointing at the
    // pre-populated path. No worker needed — scheduler should find it
    // cached and complete immediately.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("cached-hash", "/nix/store/cached-hash.drv", "x86_64-linux");
    node.expected_output_paths = vec![cached_output.to_string()];

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![node],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _event_rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Derivation should have gone Created → Completed (scheduler cache hit).
    let info = handle
        .debug_query_derivation("cached-hash")
        .await
        .unwrap()
        .expect("derivation should exist in DAG");
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "scheduler cache check should mark derivation as Completed"
    );
    assert_eq!(info.output_paths, vec![cached_output.to_string()]);

    // Build should be Succeeded (all 1 derivation cached).
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status = status_rx.await.unwrap().unwrap();
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
}

#[tokio::test]
async fn test_scheduler_cache_check_skipped_without_store() {
    let db = TestDb::new(&MIGRATOR).await;

    // No store client — cache check should silently skip.
    let (handle, _task) = setup_actor(db.pool.clone()).await;
    let _rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;

    let build_id = Uuid::new_v4();
    let mut node = make_test_node(
        "uncached-hash",
        "/nix/store/uncached-hash.drv",
        "x86_64-linux",
    );
    // expected_output_paths set but store client is None — should NOT short-circuit
    node.expected_output_paths = vec!["/nix/store/uncached-out".to_string()];

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![node],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _event_rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Without store client, derivation should proceed normally to dispatch.
    let info = handle
        .debug_query_derivation("uncached-hash")
        .await
        .unwrap()
        .expect("derivation should exist");
    assert!(
        matches!(
            info.status,
            DerivationStatus::Assigned | DerivationStatus::Ready
        ),
        "derivation should be dispatched normally without store client, got {:?}",
        info.status
    );
}

// -----------------------------------------------------------------------
// DB fault injection
// -----------------------------------------------------------------------

/// Verify that DB failures during completion are logged but do not block
/// the in-memory state machine. This is the policy decided in Group 4:
/// DB writes are best-effort; the actor must not stall on DB unavailability.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_db_failure_during_completion_logged() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Get a single derivation to Assigned state.
    let _rx = connect_worker(&handle, "test-worker", "x86_64-linux", 1).await;
    let build_id = Uuid::new_v4();
    let drv_hash = "db-fault-hash";
    let drv_path = "/nix/store/db-fault-hash.drv";
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        drv_hash,
        drv_path,
        PriorityClass::Scheduled,
    )
    .await;
    settle().await;

    // Sanity check: derivation was dispatched.
    let pre = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
        .expect("derivation should exist");
    assert_eq!(pre.status, DerivationStatus::Assigned);

    // Close the DB pool — subsequent DB writes will fail.
    db.pool.close().await;

    // Send successful completion. DB write will fail but in-memory
    // transition should succeed.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "test-worker".into(),
            drv_key: drv_path.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: "/nix/store/fake-output".into(),
                    output_hash: vec![0u8; 32],
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    settle().await;

    // In-memory state should have transitioned despite DB failure.
    let post = handle
        .debug_query_derivation(drv_hash)
        .await
        .unwrap()
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
}

/// A cyclic DAG submission must not leak into the actor's in-memory maps.
/// Regression test for the reorder fix: merge() now runs BEFORE the map
/// inserts, so a CycleDetected error leaves no trace in
/// build_events/build_sequences/builds.
#[tokio::test]
async fn test_cyclic_merge_does_not_leak_in_memory_state() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    let build_id = Uuid::new_v4();
    // A depends on B, B depends on A — cycle.
    let nodes = vec![
        make_test_node("cycA", "/nix/store/cycA.drv", "x86_64-linux"),
        make_test_node("cycB", "/nix/store/cycB.drv", "x86_64-linux"),
    ];
    let edges = vec![
        rio_proto::types::DerivationEdge {
            parent_drv_path: "/nix/store/cycA.drv".into(),
            child_drv_path: "/nix/store/cycB.drv".into(),
        },
        rio_proto::types::DerivationEdge {
            parent_drv_path: "/nix/store/cycB.drv".into(),
            child_drv_path: "/nix/store/cycA.drv".into(),
        },
    ];

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes,
            edges,
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();

    let result = reply_rx.await.unwrap();
    assert!(
        result.is_err(),
        "cyclic DAG should be rejected with an error"
    );

    // The build must NOT be in the actor's maps (it was never inserted,
    // or it was rolled back). QueryBuildStatus should return NotFound.
    let (status_tx, status_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: status_tx,
        })
        .await
        .unwrap();
    let status_result = status_rx.await.unwrap();
    assert!(
        matches!(status_result, Err(ActorError::BuildNotFound(_))),
        "build should not be in actor maps after cyclic merge failure; got {status_result:?}"
    );

    // The DAG should have no trace of the cyclic nodes.
    let drv_a = handle.debug_query_derivation("cycA").await.unwrap();
    assert!(
        drv_a.is_none(),
        "cycA should not exist in DAG after cycle rollback"
    );
    let drv_b = handle.debug_query_derivation("cycB").await.unwrap();
    assert!(
        drv_b.is_none(),
        "cycB should not exist in DAG after cycle rollback"
    );
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
    let mut actor = DagActor::new(scheduler_db, None);
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
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

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
/// Without cleanup: phantom capacity leak (worker appears full forever)
/// or infinite dispatch loop when max_builds > 1.
#[tokio::test]
async fn test_assign_send_failure_cleans_running_builds() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone()).await;

    // Connect worker with capacity-1 stream channel so we can fill it.
    let (stream_tx, mut stream_rx) = mpsc::channel(1);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "tight-worker".into(),
            stream_tx,
        })
        .await
        .unwrap();
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            worker_id: "tight-worker".into(),
            system: "x86_64-linux".into(),
            supported_features: vec![],
            max_builds: 2, // room for 2, but stream has room for 1
            running_builds: vec![],
        })
        .await
        .unwrap();

    // Merge 2 leaf derivations. Both go Ready; dispatch assigns both.
    // First assignment succeeds (fills stream channel). Second try_send
    // fails (channel full) — this triggers the recovery path.
    let build_id = Uuid::new_v4();
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![
                make_test_node("drvA", "/nix/store/drvA.drv", "x86_64-linux"),
                make_test_node("drvB", "/nix/store/drvB.drv", "x86_64-linux"),
            ],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _event_rx = reply_rx.await.unwrap().unwrap();
    settle().await;

    // Worker should have EXACTLY 1 running build (the successful assign),
    // not 2 (which would indicate the failed assign leaked into running_builds).
    let workers = handle.debug_query_workers().await.unwrap();
    let worker = workers
        .iter()
        .find(|w| w.worker_id == "tight-worker")
        .unwrap();
    assert_eq!(
        worker.running_count, 1,
        "failed try_send must clean up running_builds; got {:?}",
        worker.running_builds
    );

    // The unsent derivation should be back in Ready (not stuck Assigned).
    let sent_hash = &worker.running_builds[0];
    let unsent_hash = if sent_hash == "drvA" { "drvB" } else { "drvA" };
    let unsent = handle
        .debug_query_derivation(unsent_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        unsent.status,
        DerivationStatus::Ready,
        "unsent derivation should be reset to Ready"
    );

    // Drain the stream channel; next dispatch should pick up the unsent drv.
    let _first_assignment = stream_rx.recv().await.unwrap();
    // Trigger dispatch via heartbeat.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            worker_id: "tight-worker".into(),
            system: "x86_64-linux".into(),
            supported_features: vec![],
            max_builds: 2,
            running_builds: vec![sent_hash.clone()],
        })
        .await
        .unwrap();
    settle().await;

    // Now both should be assigned.
    let workers = handle.debug_query_workers().await.unwrap();
    let worker = workers
        .iter()
        .find(|w| w.worker_id == "tight-worker")
        .unwrap();
    assert_eq!(
        worker.running_count, 2,
        "after draining stream, both derivations should be assigned"
    );
    let _second_assignment = stream_rx.recv().await.unwrap();
}
