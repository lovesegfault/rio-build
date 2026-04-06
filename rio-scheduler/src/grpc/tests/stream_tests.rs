//! `BuildExecution` bidi-stream + malformed-message handling tests.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395) to mirror
//! the `grpc/worker_service.rs` seam (P0356). Covers the worker-facing
//! stream: end-to-end assignment flow, log pipeline, heartbeat payload
//! bounds, and malformed-message paths (duplicate register, None-result
//! completion, empty stream).

use super::*;
use rio_proto::{
    ExecutorService, ExecutorServiceClient, ExecutorServiceServer, SchedulerServiceServer,
};
use rio_test_support::fixtures::test_drv_path;
use std::time::Duration;
use tokio_stream::StreamExt;

// r[verify proto.stream.bidi]
/// End-to-end BuildExecution bidirectional stream.
///
/// Spins up an in-process ExecutorServiceServer backed by a real actor,
/// connects a mock worker via gRPC, sends ExecutorRegister + Heartbeat,
/// submits a build via SchedulerService, receives WorkAssignment on the
/// stream, sends CompletionReport, verifies build completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_build_execution_stream_end_to_end() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _actor_task) = setup_actor(db.pool.clone());

    // Spin up in-process gRPC server (SchedulerService + ExecutorService).
    let grpc = SchedulerGrpc::new_for_tests(handle.clone());
    let router = tonic::transport::Server::builder()
        .add_service(SchedulerServiceServer::new(grpc.clone()))
        .add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = ExecutorServiceClient::new(channel.clone());
    let mut sched_client = rio_proto::SchedulerServiceClient::new(channel);

    // Open BuildExecution stream. First message MUST be ExecutorRegister.
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(32);
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "e2e-worker".into(),
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
            executor_id: "e2e-worker".into(),
            kind: rio_proto::types::ExecutorKind::Builder as i32,
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 1,
            running_builds: vec![],
            resources: None,
            local_paths: None,
            size_class: String::new(),
            store_degraded: false,
            draining: false,
        })
        .await
        .expect("heartbeat should succeed");

    // Submit a build via SchedulerService.
    let submit_req = rio_proto::build_types::SubmitBuildRequest {
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
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Completion(
                rio_proto::types::CompletionReport {
                    drv_path: work.drv_path.clone(),
                    result: Some(rio_proto::build_types::BuildResult {
                        status: rio_proto::build_types::BuildResultStatus::Built.into(),
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
        .add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = ExecutorServiceClient::new(channel.clone());
    let mut sched_client = rio_proto::SchedulerServiceClient::new(channel);

    // Open BuildExecution stream with ExecutorRegister.
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(32);
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "log-e2e-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

    // Heartbeat to fully register.
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            executor_id: "log-e2e-worker".into(),
            kind: rio_proto::types::ExecutorKind::Builder as i32,
            systems: vec!["x86_64-linux".into()],
            max_builds: 1,
            ..Default::default()
        })
        .await?;

    // Submit a build → worker gets WorkAssignment.
    let mut event_stream = sched_client
        .submit_build(rio_proto::build_types::SubmitBuildRequest {
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
        executor_id: "log-e2e-worker".into(),
    };
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::LogBatch(log_batch)),
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
        executor_id: "test-worker".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        systems: vec!["x86_64-linux".into()],
        supported_features: vec![],
        max_builds: 1,
        running_builds: too_many,
        resources: None,
        local_paths: None,
        size_class: String::new(),
        store_degraded: false,
        draining: false,
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
// BuildExecution stream: malformed-message handling
// ===========================================================================

/// Helper: set up an in-process ExecutorService server backed by a
/// live actor. Returns (actor_handle, worker_client, _server, _db).
/// The server task + actor task are held alive via returned guards.
async fn setup_worker_svc() -> anyhow::Result<(
    ActorHandle,
    ExecutorServiceClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>, // server guard
    tokio::task::JoinHandle<()>, // actor guard
    TestDb,
)> {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, actor_task) = setup_actor(db.pool.clone());

    let grpc = SchedulerGrpc::new_for_tests(handle.clone());
    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    Ok((
        handle,
        ExecutorServiceClient::new(channel),
        server,
        actor_task,
        db,
    ))
}

/// Duplicate ExecutorRegister on an established stream → warn + ignore,
/// stream stays open. A buggy/retrying worker that re-sends Register
/// after stream open shouldn't be kicked — the executor_id is already
/// bound, a re-Register is a no-op. Kicking would cause a disconnect
/// + reassign cascade for no good reason.
///
/// Note: can't use `#[traced_test]` with multi_thread flavor — the
/// recv task (spawn_monitored) runs on a worker thread, and
/// traced_test's subscriber is thread-local to the test thread. We
/// assert on observable state instead: if the duplicate Register
/// were NOT ignored, the recv loop would break → stream close →
/// ExecutorDisconnected → worker removed from actor.executors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_build_execution_duplicate_register_ignored() -> anyhow::Result<()> {
    let (handle, mut worker_client, _srv, _actor, _db) = setup_worker_svc().await?;

    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(8);
    // First Register (opens stream).
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "dup-worker".into(),
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
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "dup-worker".into(),
                },
            )),
        })
        .await?;

    // barrier to ensure the recv task processed the duplicate.
    crate::actor::tests::barrier(&handle).await;

    // Stream should still be open: worker is still in the actor's
    // workers map. If the duplicate Register caused a break/error,
    // ExecutorDisconnected would have fired and removed the entry.
    let workers = handle.debug_query_workers().await?;
    assert!(
        workers.iter().any(|w| w.executor_id == "dup-worker"),
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
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(8);
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "none-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

    // Heartbeat to fully register so dispatch works.
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            executor_id: "none-worker".into(),
            kind: rio_proto::types::ExecutorKind::Builder as i32,
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
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Completion(
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
    // InfrastructureFailure does NOT insert into failed_builders and
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
    // handler). failed_builders empty = handle_infrastructure_failure;
    // if it had "none-worker" = wrong match arm (regression).
    crate::actor::tests::barrier(&handle).await;
    let info = handle
        .debug_query_derivation("none-drv")
        .await?
        .expect("drv exists");
    assert!(
        info.failed_builders.is_empty(),
        "synthesized InfrastructureFailure must route to handle_infrastructure_failure \
         (NOT handle_transient_failure), got failed_builders={:?}",
        info.failed_builders
    );
    assert_eq!(
        info.retry_count, 0,
        "InfrastructureFailure carries no retry_count penalty (separate infra_retry_count)"
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
    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(1);
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
