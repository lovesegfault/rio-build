use super::*;
use crate::actor::tests::{make_test_node, setup_actor};
use rio_proto::scheduler::scheduler_service_server::SchedulerServiceServer;
use rio_proto::worker::worker_service_client::WorkerServiceClient;
use rio_proto::worker::worker_service_server::WorkerServiceServer;
use rio_test_support::TestDb;
use rio_test_support::fixtures::test_drv_path;
use std::time::Duration;
use tokio_stream::StreamExt;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// T6: End-to-end BuildExecution bidirectional stream.
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
    let grpc = SchedulerGrpc::new(handle.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    // Fire-and-forget: never joined.
    let _server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SchedulerServiceServer::new(grpc.clone()))
            .add_service(WorkerServiceServer::new(grpc))
            .serve_with_incoming(incoming)
            .await
            .expect("test gRPC server should run");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let endpoint = format!("http://{addr}");
    let channel = tonic::transport::Channel::from_shared(endpoint)?
        .connect()
        .await?;
    let mut worker_client = WorkerServiceClient::new(channel.clone());
    let mut sched_client =
        rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient::new(channel);

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
            system: "x86_64-linux".into(),
            supported_features: vec![],
            max_builds: 1,
            running_builds: vec![],
            resources: None,
            local_paths: None,
        })
        .await
        .expect("heartbeat should succeed");

    // Submit a build via SchedulerService.
    let submit_req = rio_proto::types::SubmitBuildRequest {
        tenant_id: String::new(),
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
/// the system works (the "don't use SchedulerGrpc::new() in prod" gotcha).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_log_pipeline_grpc_wire_end_to_end() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _actor_task) = setup_actor(db.pool.clone());

    // In-process gRPC server. Same setup as test_build_execution_stream_end_to_end.
    let grpc = SchedulerGrpc::new(handle.clone());
    // Grab the ring buffers BEFORE the server moves grpc — we assert on
    // them after sending the LogBatch.
    let log_buffers = grpc.log_buffers();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let _server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SchedulerServiceServer::new(grpc.clone()))
            .add_service(WorkerServiceServer::new(grpc))
            .serve_with_incoming(incoming)
            .await
            .expect("test gRPC server should run");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let endpoint = format!("http://{addr}");
    let channel = tonic::transport::Channel::from_shared(endpoint)?
        .connect()
        .await?;
    let mut worker_client = WorkerServiceClient::new(channel.clone());
    let mut sched_client =
        rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient::new(channel);

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
            system: "x86_64-linux".into(),
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
    // invariant). If SchedulerGrpc::new() had allocated a separate buffer,
    // THIS one would be empty.
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
    let grpc = SchedulerGrpc::new(handle);

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
    let grpc = SchedulerGrpc::new(handle);

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
    let grpc = SchedulerGrpc::new(handle);

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

/// SubmitBuild with an unrecognized priority_class should be rejected
/// at the gRPC boundary (PriorityClass::FromStr). Previously this leaked
/// as a PostgreSQL CHECK constraint violation in Status::internal.
#[tokio::test]
async fn test_submit_build_rejects_invalid_priority_class() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new(handle);

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

/// SubmitBuild with a non-empty tenant_id that's not a valid UUID should
/// be rejected at the gRPC boundary, not leak as a PostgreSQL cast error.
#[tokio::test]
async fn test_submit_build_rejects_invalid_tenant_id() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new(handle);

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_test_node("h", "x86_64-linux")],
        edges: vec![],
        tenant_id: "customer-foo".into(), // not a UUID
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "invalid tenant_id should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("tenant_id"),
        "error should mention tenant_id: {}",
        status.message()
    );
}

/// SubmitBuild with more edges than MAX_DAG_EDGES should be rejected
/// (DoS prevention: O(edges) merge loop).
#[tokio::test]
async fn test_submit_build_rejects_too_many_edges() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new(handle);

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
    let grpc = SchedulerGrpc::new(handle);

    let too_many: Vec<String> = (0..1001).map(|i| format!("/nix/store/{i}.drv")).collect();

    let req = Request::new(rio_proto::types::HeartbeatRequest {
        worker_id: "test-worker".into(),
        system: "x86_64-linux".into(),
        supported_features: vec![],
        max_builds: 1,
        running_builds: too_many,
        resources: None,
        local_paths: None,
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
        (ActorError::ChannelSend, Code::Internal, "unavailable"),
        (
            ActorError::Database(sqlx::Error::PoolClosed),
            Code::Internal,
            "database",
        ),
        (ActorError::Internal("boom".into()), Code::Internal, "boom"),
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

    let mut stream = bridge_build_events("test-bridge", rx);
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
    let grpc = SchedulerGrpc::new(handle);

    let mk_req = |tag: &str| rio_proto::types::SubmitBuildRequest {
        tenant_id: String::new(),
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
