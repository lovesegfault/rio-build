// r[verify proto.stream.bidi]

use super::*;
use crate::actor::tests::{make_test_node, setup_actor};
use rio_proto::SchedulerServiceServer;
use rio_proto::WorkerServiceClient;
use rio_proto::WorkerServiceServer;
use rio_test_support::TestDb;
use rio_test_support::fixtures::test_drv_path;
use std::time::Duration;
use tokio_stream::StreamExt;

use crate::MIGRATOR;

/// End-to-end BuildExecution bidirectional stream.
///
/// Spins up an in-process WorkerServiceServer backed by a real actor,
/// connects a mock worker via gRPC, sends WorkerRegister + Heartbeat,
/// submits a build via SchedulerService, receives WorkAssignment on the
/// stream, sends CompletionReport, verifies build completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_build_execution_stream_end_to_end() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _actor_task) = setup_actor(db.pool.clone());

    // Spin up in-process gRPC server (SchedulerService + WorkerService).
    let grpc = SchedulerGrpc::new_for_tests(handle.clone());
    let router = tonic::transport::Server::builder()
        .add_service(SchedulerServiceServer::new(grpc.clone()))
        .add_service(WorkerServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = WorkerServiceClient::new(channel.clone());
    let mut sched_client = rio_proto::SchedulerServiceClient::new(channel);

    // Open BuildExecution stream. First message MUST be WorkerRegister.
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::WorkerMessage>(32);
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::Register(
                rio_proto::types::WorkerRegister {
                    worker_id: "e2e-worker".into(),
                },
            )),
        })
        .await?;

    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client
        .build_execution(outbound)
        .await
        .expect("BuildExecution stream should open")
        .into_inner();

    // Send Heartbeat to fully register (stream + heartbeat).
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            worker_id: "e2e-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 1,
            running_builds: vec![],
            resources: None,
            local_paths: None,
            size_class: String::new(),
            store_degraded: false,
        })
        .await
        .expect("heartbeat should succeed");

    // Submit a build via SchedulerService.
    let submit_req = rio_proto::types::SubmitBuildRequest {
        tenant_name: String::new(),
        priority_class: "scheduled".into(),
        nodes: vec![make_test_node("e2e-hash", "x86_64-linux")],
        edges: vec![],
        max_silent_time: 0,
        build_timeout: 0,
        build_cores: 0,
        keep_going: false,
    };
    let mut event_stream = sched_client
        .submit_build(submit_req)
        .await
        .expect("SubmitBuild should succeed")
        .into_inner();

    // Worker should receive WorkAssignment on the BuildExecution stream.
    let assignment = tokio::time::timeout(Duration::from_secs(5), inbound.next())
        .await
        .expect("assignment should arrive within 5s")
        .expect("stream should not close")
        .expect("assignment should not be an error");
    let work = match assignment.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
        other => panic!("expected WorkAssignment, got {other:?}"),
    };
    assert_eq!(work.drv_path, test_drv_path("e2e-hash"));

    // Send CompletionReport back on the stream.
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::Completion(
                rio_proto::types::CompletionReport {
                    drv_path: work.drv_path.clone(),
                    result: Some(rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::Built.into(),
                        error_msg: String::new(),
                        times_built: 1,
                        start_time: None,
                        stop_time: None,
                        built_outputs: vec![rio_proto::types::BuiltOutput {
                            output_name: "out".into(),
                            output_path: rio_test_support::fixtures::test_store_path("e2e-output"),
                            output_hash: vec![0u8; 32],
                        }],
                    }),
                    assignment_token: work.assignment_token.clone(),
                    peak_memory_bytes: 0,
                    output_size_bytes: 0,
                    peak_cpu_cores: 0.0,
                },
            )),
        })
        .await
        .expect("completion send should succeed");

    // Build event stream should emit BuildCompleted.
    let mut saw_completed = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), event_stream.next()).await;
        match ev {
            Ok(Some(Ok(event))) => {
                if let Some(rio_proto::types::build_event::Event::Completed(_)) = event.event {
                    saw_completed = true;
                    break;
                }
            }
            Ok(Some(Err(e))) => panic!("event stream error: {e}"),
            Ok(None) => break, // stream closed
            Err(_) => panic!("timed out waiting for BuildCompleted"),
        }
    }
    assert!(
        saw_completed,
        "BuildCompleted event should be emitted after worker sends CompletionReport"
    );
    Ok(())
}

/// End-to-end log pipeline over the gRPC wire: worker sends LogBatch on
/// the BuildExecution stream → SchedulerGrpc recv task writes ring buffer
/// + try_sends ForwardLogBatch → actor emits BuildEvent::Log on the
/// broadcast channel → bridge_build_events delivers it on the gateway-
/// facing SubmitBuild stream.
///
/// This is the FULL pipeline, touching every hop:
///   1. gRPC wire decode (tonic)
///   2. Ring buffer push (grpc/mod.rs LogBatch arm)
///   3. Actor drv_path→hash→interested_builds resolution (ForwardLogBatch)
///   4. Broadcast channel (emit_build_event)
///   5. bridge_build_events (SubmitBuild stream bridge)
///
/// The ring-buffer write (hop 2) is also asserted — proves the
/// SAME-Arc<LogBuffers> sharing between the recv task and the rest of
/// the system works (the old "don't use new() in prod" footgun — now
/// prevented by cfg(test) on new_for_tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_log_pipeline_grpc_wire_end_to_end() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _actor_task) = setup_actor(db.pool.clone());

    // In-process gRPC server. Same setup as test_build_execution_stream_end_to_end.
    let grpc = SchedulerGrpc::new_for_tests(handle.clone());
    // Grab the ring buffers BEFORE the server moves grpc — we assert on
    // them after sending the LogBatch.
    let log_buffers = grpc.log_buffers();

    let router = tonic::transport::Server::builder()
        .add_service(SchedulerServiceServer::new(grpc.clone()))
        .add_service(WorkerServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = WorkerServiceClient::new(channel.clone());
    let mut sched_client = rio_proto::SchedulerServiceClient::new(channel);

    // Open BuildExecution stream with WorkerRegister.
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::WorkerMessage>(32);
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::Register(
                rio_proto::types::WorkerRegister {
                    worker_id: "log-e2e-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

    // Heartbeat to fully register.
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            worker_id: "log-e2e-worker".into(),
            systems: vec!["x86_64-linux".into()],
            max_builds: 1,
            ..Default::default()
        })
        .await?;

    // Submit a build → worker gets WorkAssignment.
    let mut event_stream = sched_client
        .submit_build(rio_proto::types::SubmitBuildRequest {
            priority_class: "scheduled".into(),
            nodes: vec![make_test_node("log-pipeline-drv", "x86_64-linux")],
            ..Default::default()
        })
        .await?
        .into_inner();

    let assignment = tokio::time::timeout(Duration::from_secs(5), inbound.next())
        .await?
        .expect("assignment")
        .expect("not an error");
    let work = match assignment.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
        other => panic!("expected WorkAssignment, got {other:?}"),
    };

    // ═══════════ THE TEST ═══════════
    // Worker sends a LogBatch on the stream. This is the real gRPC wire
    // path, not a direct actor send.
    let log_batch = rio_proto::types::BuildLogBatch {
        derivation_path: work.drv_path.clone(),
        lines: vec![b"wire-line-0".to_vec(), b"wire-line-1".to_vec()],
        first_line_number: 0,
        worker_id: "log-e2e-worker".into(),
    };
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::LogBatch(log_batch)),
        })
        .await?;

    // Assert 1: The gateway-facing event stream receives BuildEvent::Log.
    // Drain through Started/DerivationStarted first. If Log never arrives,
    // the 5s timeout unwinds via `?` and the test fails with a clear
    // "Elapsed" error — no separate `saw_log` bool needed.
    let received_lines = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), event_stream.next())
            .await?
            .expect("event")
            .expect("not an error");
        if let Some(rio_proto::types::build_event::Event::Log(log)) = ev.event {
            assert_eq!(log.derivation_path, work.drv_path);
            break log.lines;
        }
        // Other events (Started, DerivationStarted) are expected — drain.
    };
    assert_eq!(received_lines.len(), 2);
    assert_eq!(received_lines[0], b"wire-line-0");
    assert_eq!(received_lines[1], b"wire-line-1");

    // Assert 2: Ring buffer was written. This proves the recv-task's
    // log_buffers.push() call sees the same DashMap we do (the shared-Arc
    // invariant). If the recv task had a separate buffer, THIS one
    // would be empty. new_for_tests() makes a fresh DashMap but we
    // grabbed a handle to it via log_buffers() above, so we're
    // asserting against the same one the recv task writes to.
    let buffered = log_buffers.read_since(&work.drv_path, 0);
    assert_eq!(
        buffered.len(),
        2,
        "ring buffer should have been written by the recv task; \
         if empty, the Arc<LogBuffers> sharing is broken"
    );
    assert_eq!(buffered[0].1, b"wire-line-0");

    Ok(())
}

/// SubmitBuild with an empty drv_hash in a node should be rejected at
/// the gRPC boundary (proto types have no validation; an empty hash
/// would become a DAG primary key).
#[tokio::test]
async fn test_submit_build_rejects_empty_drv_hash() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let mut bad_node = make_test_node("h", "x86_64-linux");
    bad_node.drv_hash = String::new(); // empty!

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![bad_node],
        edges: vec![],
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "empty drv_hash should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("drv_hash"),
        "error should mention drv_hash: {}",
        status.message()
    );
}

/// SubmitBuild with an empty drv_path should be rejected.
#[tokio::test]
async fn test_submit_build_rejects_empty_drv_path() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let mut bad_node = make_test_node("h", "x86_64-linux");
    bad_node.drv_path = String::new(); // empty!

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![bad_node],
        edges: vec![],
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "empty drv_path should be rejected");
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

/// SubmitBuild with an empty system should be rejected. An empty system
/// never matches any worker's system (e.g., "x86_64-linux"), so the
/// derivation would sit in Ready forever with no feedback.
#[tokio::test]
async fn test_submit_build_rejects_empty_system() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let mut bad_node = make_test_node("h", "x86_64-linux");
    bad_node.system = String::new(); // empty!

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![bad_node],
        edges: vec![],
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "empty system should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("system"),
        "error should mention system: {}",
        status.message()
    );
}

/// Oversized drv_content (>256 KB) should be rejected at gRPC ingress.
/// The gateway caps at 64 KB, but a buggy/hostile client could bypass
/// that — this is the defensive bound.
#[tokio::test]
async fn test_submit_build_rejects_oversized_drv_content() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let mut bad_node = make_test_node("h", "x86_64-linux");
    // 256 KB + 1 byte → over limit.
    bad_node.drv_content = vec![b'a'; 256 * 1024 + 1];

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![bad_node],
        edges: vec![],
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "oversized drv_content should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("drv_content"),
        "error should mention drv_content: {}",
        status.message()
    );
}

/// SubmitBuild with an unrecognized priority_class should be rejected
/// at the gRPC boundary (PriorityClass::FromStr). Without gRPC-level
/// validation, this leaks as a PostgreSQL CHECK constraint violation
/// in Status::internal.
#[tokio::test]
async fn test_submit_build_rejects_invalid_priority_class() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_test_node("h", "x86_64-linux")],
        edges: vec![],
        priority_class: "urgent".into(), // not in {ci, interactive, scheduled}
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "invalid priority_class should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("priority_class"),
        "error should mention priority_class: {}",
        status.message()
    );
}

// r[verify sched.tenant.resolve]
/// SubmitBuild with a tenant name not in the tenants table → InvalidArgument.
/// Proto field carries tenant NAME (from gateway's authorized_keys comment);
/// scheduler resolves to UUID via PG lookup.
#[tokio::test]
async fn test_submit_build_rejects_unknown_tenant() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests_with_pool(handle, db.pool.clone());

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_test_node("h", "x86_64-linux")],
        edges: vec![],
        tenant_name: "nonexistent-team".into(),
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "unknown tenant should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("unknown tenant"),
        "error should mention 'unknown tenant': {}",
        status.message()
    );
    assert!(
        status.message().contains("nonexistent-team"),
        "error should include the tenant name: {}",
        status.message()
    );
}

/// SubmitBuild with a tenant name that IS in the tenants table → resolves
/// to the UUID and the build is submitted successfully.
#[tokio::test]
async fn test_submit_build_resolves_known_tenant() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests_with_pool(handle, db.pool.clone());

    // Seed the tenants table.
    let tenant_uuid: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id")
            .bind("team-alpha")
            .fetch_one(&db.pool)
            .await
            .expect("tenant insert");

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_test_node("resolve-tenant-drv", "x86_64-linux")],
        edges: vec![],
        tenant_name: "team-alpha".into(),
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(
        result.is_ok(),
        "known tenant should be accepted: {result:?}"
    );

    // Verify the build row has the resolved UUID.
    let db_tenant: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .expect("build lookup");
    assert_eq!(db_tenant, Some(tenant_uuid));
}

/// SubmitBuild with empty tenant_name (single-tenant mode) → None, no PG lookup.
/// This is the common case and must work even without a pool.
#[tokio::test]
async fn test_submit_build_empty_tenant_is_none() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    // Intentionally pool-less to assert no PG hit for empty tenant_name.
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_test_node("no-tenant-drv", "x86_64-linux")],
        edges: vec![],
        tenant_name: String::new(), // empty = single-tenant mode
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(
        result.is_ok(),
        "empty tenant_name should succeed without PG: {result:?}"
    );

    // Verify tenant_id is NULL in the build row.
    let db_tenant: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .expect("build lookup");
    assert_eq!(db_tenant, None);
}

/// SubmitBuild with more edges than MAX_DAG_EDGES should be rejected
/// (DoS prevention: O(edges) merge loop).
#[tokio::test]
async fn test_submit_build_rejects_too_many_edges() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    // Construct MAX_DAG_EDGES+1 edges. Content doesn't matter — rejection
    // happens before any path validation.
    let too_many: Vec<_> = (0..rio_common::limits::MAX_DAG_EDGES + 1)
        .map(|i| rio_proto::types::DerivationEdge {
            parent_drv_path: format!("/nix/store/{i}-parent.drv"),
            child_drv_path: format!("/nix/store/{i}-child.drv"),
        })
        .collect();

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_test_node("h", "x86_64-linux")],
        edges: too_many,
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "too many edges should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("edges"),
        "error should mention edges: {}",
        status.message()
    );
}

/// Heartbeat with too many running_builds entries should be rejected.
/// Heartbeats bypass backpressure (send_unchecked), so unbounded payload
/// would stall the actor event loop with no backpressure signal.
#[tokio::test]
async fn test_heartbeat_rejects_too_many_running_builds() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let too_many: Vec<String> = (0..1001).map(|i| format!("/nix/store/{i}.drv")).collect();

    let req = Request::new(rio_proto::types::HeartbeatRequest {
        worker_id: "test-worker".into(),
        systems: vec!["x86_64-linux".into()],
        supported_features: vec![],
        max_builds: 1,
        running_builds: too_many,
        resources: None,
        local_paths: None,
        size_class: String::new(),
        store_degraded: false,
    });

    let result = grpc.heartbeat(req).await;
    assert!(
        result.is_err(),
        "heartbeat with >1000 running_builds should be rejected"
    );
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("running_builds"),
        "error should mention running_builds: {}",
        status.message()
    );
}

// ===========================================================================
// Error-mapping + bridge coverage
// ===========================================================================

/// Each ActorError variant maps to the expected tonic::Code.
#[test]
fn test_actor_error_to_status_all_arms() {
    use tonic::Code;
    let cases = [
        (
            ActorError::BuildNotFound(Uuid::nil()),
            Code::NotFound,
            "build not found",
        ),
        (
            ActorError::Backpressure,
            Code::ResourceExhausted,
            "overloaded",
        ),
        (ActorError::ChannelSend, Code::Unavailable, "unavailable"),
        (
            ActorError::Database(sqlx::Error::PoolClosed),
            Code::Internal,
            "database",
        ),
        (
            ActorError::Dag(crate::dag::DagError::CycleDetected),
            Code::Internal,
            "cycle",
        ),
        (
            ActorError::MissingDbId {
                drv_path: "/nix/store/x".into(),
            },
            Code::Internal,
            "unpersisted",
        ),
    ];
    for (err, expected_code, expected_substr) in cases {
        let status = SchedulerGrpc::actor_error_to_status(err);
        assert_eq!(status.code(), expected_code);
        assert!(
            status.message().contains(expected_substr),
            "expected '{expected_substr}' in '{}'",
            status.message()
        );
    }
}

#[test]
fn test_parse_build_id_invalid() {
    let err = SchedulerGrpc::parse_build_id("not-a-uuid").expect_err("should reject");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("invalid build_id"));
}

#[test]
fn test_parse_build_id_valid() {
    let id = Uuid::new_v4();
    let parsed = SchedulerGrpc::parse_build_id(&id.to_string()).expect("should parse");
    assert_eq!(parsed, id);
}

/// When a broadcast receiver lags (permanently misses events), the bridge
/// sends DATA_LOSS and stops. Without this, a missed BuildCompleted would
/// leave the client hanging forever.
#[tokio::test]
async fn test_bridge_build_events_lagged_sends_data_loss() {
    // Capacity 1 + send 3 before receiver subscribes → lag guaranteed.
    let (tx, _keepalive_rx) = broadcast::channel(1);
    let rx = tx.subscribe();
    // Fill the channel past capacity so rx is lagged.
    for i in 0..3u64 {
        let _ = tx.send(rio_proto::types::BuildEvent {
            build_id: format!("build-{i}"),
            sequence: i,
            timestamp: None,
            event: None,
        });
    }

    let mut stream = bridge_build_events("test-bridge", rx, None);
    // First poll: the bridge task's first recv() hits Lagged.
    let first = stream.next().await.expect("should yield one item");
    let status = first.expect_err("should be DATA_LOSS");
    assert_eq!(status.code(), tonic::Code::DataLoss);
    assert!(
        status.message().contains("missed"),
        "got: {}",
        status.message()
    );
    // Stream should then end (bridge task broke out of the loop).
    assert!(stream.next().await.is_none());
}

/// UUID v7 build_ids are time-ordered: two submissions ~apart in time
/// produce lexicographically ordered IDs. This is the property we rely
/// on for S3 log key prefix-scanning and PG index locality.
///
/// We don't assert strict monotonicity within the same millisecond —
/// v7's counter field handles that, but testing it requires contriving
/// >1 call per ms which is flaky. Instead: sleep > 1ms between
/// submissions and assert lexicographic order. This tests the property
/// we actually care about (chronological ordering at human timescales),
/// not the RFC's intra-ms counter edge case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_build_ids_are_time_ordered_v7() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _actor_task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let mk_req = |tag: &str| rio_proto::types::SubmitBuildRequest {
        tenant_name: String::new(),
        priority_class: String::new(),
        nodes: vec![make_test_node(tag, "x86_64-linux")],
        edges: vec![],
        max_silent_time: 0,
        build_timeout: 0,
        build_cores: 0,
        keep_going: false,
    };

    // First submission.
    let mut s1 = grpc
        .submit_build(tonic::Request::new(mk_req("v7-first")))
        .await?
        .into_inner();
    let id1 = s1.next().await.expect("first event").expect("ok").build_id;

    // > 1ms gap guarantees a different v7 timestamp prefix. 2ms is
    // plenty; tokio's time granularity is ~1ms on most systems.
    tokio::time::sleep(Duration::from_millis(2)).await;

    // Second submission.
    let mut s2 = grpc
        .submit_build(tonic::Request::new(mk_req("v7-second")))
        .await?
        .into_inner();
    let id2 = s2.next().await.expect("first event").expect("ok").build_id;

    // v7 IDs sort lexicographically by creation time. The string
    // representation is the canonical UUID format (8-4-4-4-12 hex
    // with lowercase a-f), and lex-order on that matches timestamp
    // order for v7 (the timestamp is in the high bits).
    assert!(
        id1 < id2,
        "v7 build_ids should be time-ordered: {id1} should sort before {id2}"
    );

    // Also verify they parse as v7 (version nibble = 7). The version
    // is the first nibble of the third hyphen-delimited group.
    let parse = |s: &str| -> Uuid { s.parse().expect("valid UUID") };
    assert_eq!(
        parse(&id1).get_version_num(),
        7,
        "build_id should be UUID v7"
    );
    assert_eq!(
        parse(&id2).get_version_num(),
        7,
        "build_id should be UUID v7"
    );

    Ok(())
}

// ===========================================================================
// since_sequence replay (PG event log + subscribe-first dedup)
// ===========================================================================

/// Minimal BuildEvent for replay tests. Prost-encoded (same as
/// emit_build_event does via encode_to_vec).
fn mk_event(build_id: Uuid, seq: u64) -> rio_proto::types::BuildEvent {
    use rio_proto::types::build_event::Event;
    rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        sequence: seq,
        timestamp: None,
        event: Some(Event::Cancelled(rio_proto::types::BuildCancelled {
            reason: format!("seq-{seq}"),
        })),
    }
}

/// Insert one event into PG directly (bypassing the persister).
/// Tests control exact PG state to assert replay behavior.
async fn insert_event(pool: &sqlx::PgPool, build_id: Uuid, seq: u64) -> anyhow::Result<()> {
    use prost::Message;
    sqlx::query(
        "INSERT INTO build_event_log (build_id, sequence, event_bytes) VALUES ($1, $2, $3)",
    )
    .bind(build_id)
    .bind(seq as i64)
    .bind(mk_event(build_id, seq).encode_to_vec())
    .execute(pool)
    .await?;
    Ok(())
}

/// Drain N events from the bridge with a timeout. Collects just
/// sequences — that's what we assert on (order + gaps + dedup).
async fn collect_seqs(
    stream: &mut ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>,
    n: usize,
) -> anyhow::Result<Vec<u64>> {
    let mut seqs = Vec::with_capacity(n);
    for _ in 0..n {
        let ev = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("stream ended early"))??;
        seqs.push(ev.sequence);
    }
    Ok(seqs)
}

/// Core property: gateway reconnects with since_sequence=2 after
/// the actor has emitted seq 1..5. PG has all 5 (persister ran).
/// Broadcast ring ALSO has all 5 (cap 1024). Without dedup, the
/// gateway sees 3,4,5 from PG then 1..5 again from broadcast.
/// With dedup, exactly 3,4,5 once.
///
/// Test uses a bare broadcast channel (not the full actor) to
/// control exactly what's in the ring vs PG. The real subscribe-
/// first ordering is tested separately (test_bridge_no_gap_on_race).
#[tokio::test]
async fn test_bridge_replays_from_pg_and_dedups_broadcast() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // PG: seq 1..5 persisted (simulates what emit_build_event did).
    for seq in 1..=5 {
        insert_event(&db.pool, build_id, seq).await?;
    }

    // Broadcast: same 5 events still in the ring (1024 cap, they
    // haven't been pushed out). This is the DUPLICATE the dedup
    // protects against.
    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    for seq in 1..=5 {
        bcast_tx.send(mk_event(build_id, seq))?;
    }

    // Gateway reconnects: saw up to seq=2 before disconnect.
    // last_seq=5 (actor's watermark at subscribe time).
    let mut stream = bridge_build_events(
        "test-replay",
        bcast_rx,
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 2,
            last_seq: 5,
        }),
    );

    // Expect exactly 3,4,5 — from PG, in order. Broadcast's 1..5
    // all skipped (seq ≤ last_seq=5).
    let seqs = collect_seqs(&mut stream, 3).await?;
    assert_eq!(seqs, vec![3, 4, 5], "PG replay fills (since, last_seq]");

    // And NO MORE. Post-subscribe events (seq > 5) would come next,
    // but we sent none. A 4th event = dedup failed (broadcast leak).
    let extra = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
    assert!(
        extra.is_err(),
        "no 4th event — broadcast's 1..5 all deduped. Got: {extra:?}"
    );

    Ok(())
}

/// Post-subscribe events (seq > last_seq) flow through normally.
/// This is what the gateway sees AFTER the replay catches up.
#[tokio::test]
async fn test_bridge_post_subscribe_events_pass_dedup() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // PG: seq 1,2 (pre-subscribe history).
    for seq in 1..=2 {
        insert_event(&db.pool, build_id, seq).await?;
    }

    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    // Broadcast ring: the same 1,2 (still in buffer) PLUS 3,4
    // which arrived AFTER subscribe (seq > last_seq=2).
    for seq in 1..=4 {
        bcast_tx.send(mk_event(build_id, seq))?;
    }

    let mut stream = bridge_build_events(
        "test-post-sub",
        bcast_rx,
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 0,
            last_seq: 2,
        }),
    );

    // PG replay: 1,2. Then broadcast: 1,2 skipped (≤2), 3,4 pass.
    let seqs = collect_seqs(&mut stream, 4).await?;
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4],
        "replay then live: PG gives 1,2; broadcast dedups 1,2, passes 3,4"
    );
    Ok(())
}

/// PG down → replay fails → fall through to broadcast WITHOUT dedup.
/// A double is better than a hole — if we deduped without having
/// actually delivered from PG, the gateway would miss events.
#[tokio::test]
async fn test_bridge_pg_failure_falls_through_no_dedup() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();
    // Close the pool → read_event_log fails immediately.
    // TestDb::Drop uses a fresh admin connection so this is safe.
    db.pool.close().await;

    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    // Broadcast has 1,2,3. With dedup (last_seq=3) they'd ALL be
    // skipped. Without dedup (PG failed) they all pass — the
    // gateway gets SOMETHING instead of silence.
    for seq in 1..=3 {
        bcast_tx.send(mk_event(build_id, seq))?;
    }

    let mut stream = bridge_build_events(
        "test-pg-fail",
        bcast_rx,
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 0,
            last_seq: 3,
        }),
    );

    // PG failed → dedup_watermark stays 0 → all 3 pass.
    let seqs = collect_seqs(&mut stream, 3).await?;
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "PG failure → no dedup → broadcast delivers (safety net)"
    );
    Ok(())
}

/// `read_event_log` range is half-open `(since, until]`. Boundary
/// check: since=2, until=4 → returns 3,4 (not 2, not 5).
#[tokio::test]
async fn test_read_event_log_half_open_range() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();
    for seq in 1..=5 {
        insert_event(&db.pool, build_id, seq).await?;
    }
    // Noise: another build, same seq range. Scoping check.
    for seq in 1..=5 {
        insert_event(&db.pool, Uuid::new_v4(), seq).await?;
    }

    let rows = crate::db::read_event_log(&db.pool, build_id, 2, 4).await?;
    let seqs: Vec<u64> = rows.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        seqs,
        vec![3, 4],
        "(since, until] — excludes since, includes until"
    );
    Ok(())
}

// ===========================================================================
// BuildExecution stream: malformed-message handling
// ===========================================================================

/// Helper: set up an in-process WorkerService server backed by a
/// live actor. Returns (actor_handle, worker_client, _server, _db).
/// The server task + actor task are held alive via returned guards.
async fn setup_worker_svc() -> anyhow::Result<(
    ActorHandle,
    WorkerServiceClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>, // server guard
    tokio::task::JoinHandle<()>, // actor guard
    TestDb,
)> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, actor_task) = setup_actor(db.pool.clone());

    let grpc = SchedulerGrpc::new_for_tests(handle.clone());
    let router = tonic::transport::Server::builder().add_service(WorkerServiceServer::new(grpc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    Ok((
        handle,
        WorkerServiceClient::new(channel),
        server,
        actor_task,
        db,
    ))
}

/// Duplicate WorkerRegister on an established stream → warn + ignore,
/// stream stays open. A buggy/retrying worker that re-sends Register
/// after stream open shouldn't be kicked — the worker_id is already
/// bound, a re-Register is a no-op. Kicking would cause a disconnect
/// + reassign cascade for no good reason.
///
/// Note: can't use `#[traced_test]` with multi_thread flavor — the
/// recv task (spawn_monitored) runs on a worker thread, and
/// traced_test's subscriber is thread-local to the test thread. We
/// assert on observable state instead: if the duplicate Register
/// were NOT ignored, the recv loop would break → stream close →
/// WorkerDisconnected → worker removed from actor.workers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_build_execution_duplicate_register_ignored() -> anyhow::Result<()> {
    let (handle, mut worker_client, _srv, _actor, _db) = setup_worker_svc().await?;

    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::WorkerMessage>(8);
    // First Register (opens stream).
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::Register(
                rio_proto::types::WorkerRegister {
                    worker_id: "dup-worker".into(),
                },
            )),
        })
        .await?;

    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let _inbound = worker_client.build_execution(outbound).await?.into_inner();

    // Second Register — should be logged (from the spawned recv task)
    // + ignored. We can't check the log (thread-local subscriber) so
    // we assert the post-condition: stream stays open.
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::Register(
                rio_proto::types::WorkerRegister {
                    worker_id: "dup-worker".into(),
                },
            )),
        })
        .await?;

    // barrier to ensure the recv task processed the duplicate.
    crate::actor::tests::barrier(&handle).await;

    // Stream should still be open: worker is still in the actor's
    // workers map. If the duplicate Register caused a break/error,
    // WorkerDisconnected would have fired and removed the entry.
    let workers = handle.debug_query_workers().await?;
    assert!(
        workers.iter().any(|w| w.worker_id == "dup-worker"),
        "worker should still be connected after duplicate Register \
         (stream stayed open, no spurious disconnect)"
    );

    // Stronger: the stream_tx should still be usable (channel open).
    // is_closed() = false proves the recv task didn't drop its rx end.
    assert!(
        !stream_tx.is_closed(),
        "stream_tx should still be open after duplicate Register"
    );

    Ok(())
}

/// CompletionReport with result: None → synthesizes InfrastructureFailure.
/// A malformed completion must not silently drop — the drv would hang
/// Running forever. The recv task's `.unwrap_or_else` synthesizes a
/// failure result so the actor transitions the drv out of Running.
///
/// Note: same multi_thread + traced_test limitation as
/// test_build_execution_duplicate_register_ignored. We assert on
/// the derivation's post-state instead of the log message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_build_execution_completion_none_result_synthesizes_failure() -> anyhow::Result<()> {
    let (handle, mut worker_client, _srv, _actor, _db) = setup_worker_svc().await?;

    // Open stream + Register.
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::WorkerMessage>(8);
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::Register(
                rio_proto::types::WorkerRegister {
                    worker_id: "none-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

    // Heartbeat to fully register so dispatch works.
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            worker_id: "none-worker".into(),
            systems: vec!["x86_64-linux".into()],
            max_builds: 1,
            ..Default::default()
        })
        .await?;

    // Merge + dispatch a drv → Assigned to none-worker.
    let build_id = Uuid::new_v4();
    let _ev = crate::actor::tests::merge_single_node(
        &handle,
        build_id,
        "none-drv",
        crate::state::PriorityClass::Scheduled,
    )
    .await?;

    // Drain the WorkAssignment (proves dispatch happened).
    let assignment = tokio::time::timeout(Duration::from_secs(5), inbound.next())
        .await?
        .expect("assignment")
        .expect("not an error");
    let work = match assignment.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
        other => panic!("expected Assignment, got {other:?}"),
    };

    // Send CompletionReport with result: None.
    stream_tx
        .send(rio_proto::types::WorkerMessage {
            msg: Some(rio_proto::types::worker_message::Msg::Completion(
                rio_proto::types::CompletionReport {
                    drv_path: work.drv_path.clone(),
                    result: None, // malformed!
                    assignment_token: work.assignment_token,
                    peak_memory_bytes: 0,
                    output_size_bytes: 0,
                    peak_cpu_cores: 0.0,
                },
            )),
        })
        .await?;

    // InfrastructureFailure → handle_infrastructure_failure →
    // reset_to_ready → re-dispatch. Proof-of-processing: a SECOND
    // WorkAssignment arrives on the stream. If the None-result were
    // silently dropped, the drv would stay stuck Assigned from the
    // first dispatch and no second assignment would ever come.
    //
    // InfrastructureFailure does NOT insert into failed_workers and
    // does NOT set backoff — so the same worker is immediately
    // re-eligible and re-dispatch is synchronous in the actor.
    let reassignment = tokio::time::timeout(Duration::from_secs(5), inbound.next())
        .await
        .expect(
            "None-result completion should be synthesized as InfrastructureFailure \
             → reset_to_ready → re-dispatch → second WorkAssignment on stream \
             (if this times out, the completion was silently dropped — the \
             'stuck Assigned' state this test guards against)",
        )
        .expect("stream not closed")
        .expect("not a gRPC error");
    let reassigned = match reassignment.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
        other => panic!("expected second Assignment (re-dispatch), got {other:?}"),
    };
    assert_eq!(
        reassigned.drv_path, work.drv_path,
        "re-dispatched drv should be the same one"
    );

    // Barrier + verify the infra handler ran (not the transient
    // handler). failed_workers empty = handle_infrastructure_failure;
    // if it had "none-worker" = wrong match arm (regression).
    crate::actor::tests::barrier(&handle).await;
    let info = handle
        .debug_query_derivation("none-drv")
        .await?
        .expect("drv exists");
    assert!(
        info.failed_workers.is_empty(),
        "synthesized InfrastructureFailure must route to handle_infrastructure_failure \
         (NOT handle_transient_failure), got failed_workers={:?}",
        info.failed_workers
    );
    assert_eq!(
        info.retry_count, 0,
        "InfrastructureFailure carries no retry penalty"
    );

    Ok(())
}

/// BuildExecution stream with no messages (client opens + immediately
/// closes) → InvalidArgument("empty BuildExecution stream"). The
/// first-message-must-be-Register handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_build_execution_empty_stream_rejected() -> anyhow::Result<()> {
    let (_handle, mut worker_client, _srv, _actor, _db) = setup_worker_svc().await?;

    // Open stream, immediately close (no Register sent).
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::WorkerMessage>(1);
    drop(stream_tx); // close before sending anything

    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let result = worker_client.build_execution(outbound).await;

    let status = result.expect_err("empty stream should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("empty"),
        "error should mention empty stream: {}",
        status.message()
    );

    Ok(())
}

// r[verify sched.grpc.leader-guard]
/// Standby replica rejects all RPCs with UNAVAILABLE. Constructs the
/// service with `is_leader=false` (simulating a pod that lost or never
/// acquired the lease) and hits each RPC via the trait method directly.
/// No actor interaction — the guard fires before `check_actor_alive`.
#[tokio::test]
async fn test_not_leader_rejects_all_rpcs() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _actor_task) = setup_actor(db.pool.clone());

    let mut grpc = SchedulerGrpc::new_for_tests(handle);
    // Flip to standby. The test constructor defaults to true;
    // this reaches in and swaps the Arc. `Arc::new(false)` would
    // leave the original true-Arc orphaned, which is fine —
    // nobody else holds it (fresh from new_for_tests).
    grpc.is_leader = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // SchedulerService handlers. Call trait methods directly (no
    // server spin-up needed — guard is synchronous, fires before
    // any async work).
    use rio_proto::{SchedulerService as _, WorkerService as _};

    let s = grpc
        .submit_build(tonic::Request::new(Default::default()))
        .await
        .expect_err("standby should reject submit_build");
    assert_eq!(s.code(), tonic::Code::Unavailable);
    assert!(s.message().contains("not leader"));

    let s = grpc
        .watch_build(tonic::Request::new(Default::default()))
        .await
        .expect_err("standby should reject watch_build");
    assert_eq!(s.code(), tonic::Code::Unavailable);

    let s = grpc
        .query_build_status(tonic::Request::new(Default::default()))
        .await
        .expect_err("standby should reject query_build_status");
    assert_eq!(s.code(), tonic::Code::Unavailable);

    let s = grpc
        .cancel_build(tonic::Request::new(Default::default()))
        .await
        .expect_err("standby should reject cancel_build");
    assert_eq!(s.code(), tonic::Code::Unavailable);

    // WorkerService handlers.
    let s = grpc
        .heartbeat(tonic::Request::new(Default::default()))
        .await
        .expect_err("standby should reject heartbeat");
    assert_eq!(s.code(), tonic::Code::Unavailable);

    // BuildExecution: the request is a Streaming<WorkerMessage>. We
    // can't easily construct one synthetically outside a real gRPC
    // call. Spin up a server for this one.
    let router =
        tonic::transport::Server::builder().add_service(rio_proto::WorkerServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = WorkerServiceClient::new(channel);
    let (_tx, rx) = mpsc::channel::<rio_proto::types::WorkerMessage>(1);
    let s = worker_client
        .build_execution(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .expect_err("standby should reject build_execution");
    assert_eq!(s.code(), tonic::Code::Unavailable);

    Ok(())
}
