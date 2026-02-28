use super::*;

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
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
            },
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
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
            },
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
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes,
                edges,
                options: BuildOptions::default(),
                keep_going: false,
            },
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
            req: MergeDagRequest {
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
            },
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
