//! `BuildExecution` bidi-stream + malformed-message handling tests.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395) to mirror
//! the `grpc/worker_service.rs` seam (P0356). Covers the worker-facing
//! stream: end-to-end assignment flow, log pipeline, heartbeat payload
//! bounds, and malformed-message paths (duplicate register, None-result
//! completion, empty stream).

use super::*;
use rio_proto::{ExecutorServiceClient, ExecutorServiceServer, SchedulerServiceServer};
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
    // Spin up in-process gRPC server (SchedulerService + ExecutorService).
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
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
            ..Default::default()
        })
        .await
        .expect("heartbeat should succeed");

    // Submit a build via SchedulerService.
    let submit_req = rio_proto::types::SubmitBuildRequest {
        tenant_name: String::new(),
        priority_class: "scheduled".into(),
        nodes: vec![make_node("e2e-hash")],
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
                    result: Some(rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::Built.into(),
                        error_msg: String::new(),
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
                    peak_cpu_cores: 0.0,
                    node_name: None,
                    hw_class: None,
                    final_resources: None,
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
    // In-process gRPC server. Same setup as test_build_execution_stream_end_to_end.
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
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
            ..Default::default()
        })
        .await?;

    // Submit a build → worker gets WorkAssignment.
    let mut event_stream = sched_client
        .submit_build(rio_proto::types::SubmitBuildRequest {
            priority_class: "scheduled".into(),
            nodes: vec![make_node("log-pipeline-drv")],
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
    let buffered = log_buffers
        .read_since(&work.drv_path, 0)
        .expect("ring buffer should have been written by the recv task");
    assert_eq!(
        buffered.len(),
        2,
        "ring buffer should hold both lines; \
         if empty, the Arc<LogBuffers> sharing is broken"
    );
    assert_eq!(buffered[0].1, b"wire-line-0");

    Ok(())
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
    let (db, grpc, handle, actor_task) = setup_grpc().await;
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
/// Synchronization: the dup-Register handler sends nothing to the
/// actor, so an actor-mpsc round-trip (`barrier()`) proves nothing.
/// The one observable that IS synchronized with the recv task is the
/// server→client stream: if the recv loop breaks, the spawned task
/// drops `output_tx` → `inbound.next()` yields `None`. If correctly
/// ignored, `inbound` stays open+silent → poll times out.
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
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

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

    // Structural sync: if duplicate Register broke the recv loop, the
    // server task drops output_tx → inbound.next() returns None within
    // loopback RTT. If correctly ignored, inbound stays open+silent →
    // timeout fires. 200ms >> loopback RTT (~µs) but << test budget.
    // barrier() does NOT work here: the dup-Register handler sends
    // nothing to the actor, so an actor round-trip proves nothing.
    let poll = tokio::time::timeout(Duration::from_millis(200), inbound.next()).await;
    assert!(
        poll.is_err(),
        "inbound should remain open+silent after duplicate Register; got {poll:?} \
         (Ok(None) = stream closed = recv loop broke — the regression this test guards; \
          Ok(Some) = unexpected server message)"
    );

    // Now that 200ms has elapsed, any ExecutorDisconnected from a
    // (hypothetically broken) recv loop has reached the actor; barrier
    // to drain it before querying.
    crate::actor::tests::barrier(&handle).await;
    let workers = handle.debug_query_workers().await?;
    assert!(
        workers.iter().any(|w| w.executor_id == "dup-worker"),
        "worker should still be registered after duplicate Register"
    );

    Ok(())
}

/// merged_bug_039 (TOCTOU): the recv task MUST NOT discard a buffer at
/// stream-exit just because `is_sealed` is false at that instant —
/// `send_unchecked(ProcessCompletion)` returns on enqueue, not on the
/// actor's `seal()`, so under any actor backlog the recv task observed
/// `is_sealed=false` and discarded a completed build's buffer before
/// the flusher drained it. With cleanup moved into the actor's
/// epoch-gated `ExecutorDisconnected` handler, the recv task no longer
/// touches `LogBuffers` on exit — the buffer survives stream-close
/// regardless of seal timing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_log_buffer_survives_stream_close_before_actor_seal() -> anyhow::Result<()> {
    let (_db, grpc, handle, _actor_task) = setup_grpc().await;
    let log_buffers = grpc.log_buffers();
    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = ExecutorServiceClient::new(channel);

    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(8);
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "seal-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

    // Push a LogBatch so the recv task records this drv in seen_drvs.
    let drv_path = test_drv_path("seal-reap");
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::LogBatch(
                rio_proto::types::BuildLogBatch {
                    derivation_path: drv_path.clone(),
                    lines: vec![b"line".to_vec()],
                    first_line_number: 0,
                    executor_id: "seal-worker".into(),
                },
            )),
        })
        .await?;
    // Wait until the push landed (recv task is on a worker thread).
    tokio::time::timeout(Duration::from_secs(2), async {
        while log_buffers.read_since(&drv_path, 0).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("LogBatch push should land in log_buffers");

    // NOT sealed (the TOCTOU window: completion enqueued but actor
    // hasn't processed it yet). Close the stream immediately.
    assert!(!log_buffers.is_sealed(&drv_path));
    drop(stream_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), inbound.next()).await;
    crate::actor::tests::barrier(&handle).await;

    // Buffer survives. Pre-fix: `is_sealed=false → discard()` wiped it.
    // The actor's disconnect-cleanup also leaves it alone — the gRPC
    // and actor `log_buffers` Arcs aren't shared in this setup, but
    // even if they were, the path is DAG-unknown ONLY because no merge
    // happened; the actor-level
    // `test_disconnect_discards_only_unknown_drvs` covers that branch.
    assert!(
        log_buffers
            .read_since(&drv_path, 0)
            .is_some_and(|v| !v.is_empty()),
        "stream-close MUST NOT discard a buffer based on un-synchronized is_sealed"
    );
    Ok(())
}

/// bug_319: `log_buffers.push()` must not accept unbounded distinct
/// `derivation_path` keys from an untrusted worker. One stream may
/// create at most `MAX_DRVS_PER_STREAM` entries; on stream close,
/// un-sealed entries are discarded so periodic-flush stops iterating
/// them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_log_batch_distinct_paths_capped_per_stream() -> anyhow::Result<()> {
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
    let log_buffers = grpc.log_buffers();
    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = ExecutorServiceClient::new(channel);

    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(32);
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "cap-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

    // 12 distinct fake paths; cap is 8. Distinct HASH portions —
    // LogBuffers keys on `drv_log_hash` (bug_126), so `test_drv_path`
    // (one shared TEST_HASH) would collapse to a single buffer.
    for i in 0..12 {
        stream_tx
            .send(rio_proto::types::ExecutorMessage {
                msg: Some(rio_proto::types::executor_message::Msg::LogBatch(
                    rio_proto::types::BuildLogBatch {
                        derivation_path: format!("/nix/store/cap{i}-test.drv"),
                        lines: vec![b"x".to_vec()],
                        first_line_number: 0,
                        executor_id: "cap-worker".into(),
                    },
                )),
            })
            .await?;
    }
    // recv task is on a worker thread; poll until cap is reached.
    tokio::time::timeout(Duration::from_secs(2), async {
        while log_buffers.active_count() < 8 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("active_count should reach 8");
    assert_eq!(
        log_buffers.active_count(),
        8,
        "MAX_DRVS_PER_STREAM caps distinct buffer keys per stream"
    );

    // Stream-exit cleanup is now actor-side (epoch-gated +
    // ownership-aware via `ExecutorDisconnected.seen_drvs`); this
    // test's actor doesn't share the gRPC's `log_buffers` Arc, so the
    // discard is asserted by the actor-level
    // `test_disconnect_discards_only_unknown_drvs` instead. Here we
    // only assert the per-stream CAP held.
    drop(stream_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), inbound.next()).await;
    Ok(())
}

/// bug_077 / `r[sec.executor.identity-token]`: when the HMAC key is
/// configured, `BuildExecution` rejects without a valid
/// `x-rio-executor-token`, and `Heartbeat` rejects when the body
/// `intent_id` doesn't match the token's.
// r[verify sec.executor.identity-token+2]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_executor_service_rejects_missing_or_mismatched_token() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let key = std::sync::Arc::new(HmacKey::from_key(
        b"test-key-32-bytes-long-here!!!!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&key));
    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = ExecutorServiceClient::new(channel);

    // 1. BuildExecution without token → Unauthenticated.
    let (tx, rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(1);
    tx.send(rio_proto::types::ExecutorMessage {
        msg: Some(rio_proto::types::executor_message::Msg::Register(
            rio_proto::types::ExecutorRegister {
                executor_id: "victim".into(),
            },
        )),
    })
    .await?;
    let err = worker_client
        .build_execution(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .expect_err("token-less BuildExecution should be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 2. Heartbeat with token for intent A, body intent_id = B → reject.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token_a = key.sign(&ExecutorClaims {
        intent_id: "intent-A".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });
    let mut hb = tonic::Request::new(rio_proto::types::HeartbeatRequest {
        executor_id: "spoof".into(),
        intent_id: "intent-B".into(),
        systems: vec!["x86_64-linux".into()],
        ..Default::default()
    });
    hb.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, token_a.parse()?);
    let err = worker_client
        .heartbeat(hb)
        .await
        .expect_err("mismatched-intent heartbeat should be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 3. Heartbeat with matching intent + kind → accepted.
    let token_b = key.sign(&ExecutorClaims {
        intent_id: "intent-B".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });
    let mut hb_ok = tonic::Request::new(rio_proto::types::HeartbeatRequest {
        executor_id: "spoof".into(),
        intent_id: "intent-B".into(),
        systems: vec!["x86_64-linux".into()],
        ..Default::default()
    });
    hb_ok
        .metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, token_b.parse()?);
    worker_client
        .heartbeat(hb_ok)
        .await
        .expect("matching-intent heartbeat accepted");

    // 4. bug_038: Heartbeat with token kind=Fetcher, body kind=Builder
    //    → Unauthenticated. A compromised Fetcher (open-egress CNP)
    //    self-promoting to Builder would otherwise receive non-FOD
    //    builds with secret inputs on an open-egress pod.
    let token_fetcher = key.sign(&ExecutorClaims {
        intent_id: "intent-C".into(),
        kind: rio_proto::types::ExecutorKind::Fetcher as i32,
        expiry_unix: now + 600,
    });
    let mut hb_kind = tonic::Request::new(rio_proto::types::HeartbeatRequest {
        executor_id: "fetch-spoof".into(),
        intent_id: "intent-C".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        systems: vec!["x86_64-linux".into()],
        ..Default::default()
    });
    hb_kind
        .metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, token_fetcher.parse()?);
    let err = worker_client
        .heartbeat(hb_kind)
        .await
        .expect_err("kind-mismatch heartbeat should be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(
        err.message().contains("kind"),
        "should name the mismatched field: {}",
        err.message()
    );
    Ok(())
}

/// bug_081 / `r[sec.executor.identity-token]`: `BuildExecution` MUST
/// learn the actor's accept/reject decision BEFORE spawning the
/// `worker-stream-reader`. A spoofed `Register{executor_id=E_victim}`
/// while E_victim's stream is live is rejected by the actor's
/// live-stream guard; without the accept-gate, the reader would still
/// be spawned and forward `ProcessCompletion{E_victim, D}` — forging a
/// terminal result for E_victim's in-flight build. With the gate, the
/// gRPC handler returns `PermissionDenied` and the reader never spawns.
// r[verify sec.executor.identity-token+2]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_build_execution_accept_gate_rejects_spoofed_executor_id() -> anyhow::Result<()> {
    let (handle, mut worker_client, _srv, _actor, _db) = setup_worker_svc().await?;

    // Victim: legit BuildExecution stream as "victim".
    let (vtx, vrx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(8);
    vtx.send(rio_proto::types::ExecutorMessage {
        msg: Some(rio_proto::types::executor_message::Msg::Register(
            rio_proto::types::ExecutorRegister {
                executor_id: "victim".into(),
            },
        )),
    })
    .await?;
    let mut victim_inbound = worker_client
        .build_execution(tokio_stream::wrappers::ReceiverStream::new(vrx))
        .await?
        .into_inner();
    crate::actor::tests::barrier(&handle).await;

    // Attacker: open a SECOND stream with the SAME executor_id while
    // the victim's stream is live. Dev mode (no HMAC key in
    // setup_worker_svc) so the token bind is None — the actor's
    // live-stream guard is what rejects.
    let (atx, arx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(8);
    atx.send(rio_proto::types::ExecutorMessage {
        msg: Some(rio_proto::types::executor_message::Msg::Register(
            rio_proto::types::ExecutorRegister {
                executor_id: "victim".into(),
            },
        )),
    })
    .await?;
    let err = worker_client
        .build_execution(tokio_stream::wrappers::ReceiverStream::new(arx))
        .await
        .expect_err("spoofed executor_id with live victim stream → PermissionDenied");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("live stream"),
        "actor's reject reason surfaced: {}",
        err.message()
    );

    // Reader was never spawned: attacker's request half is dropped
    // (handler returned Err before consuming `stream`). Anything sent
    // on `atx` goes nowhere — but more importantly, the victim's
    // stream is intact and the actor never saw a forged completion.
    drop(atx);

    // Victim's stream is intact: still open, no spurious close.
    let poll = tokio::time::timeout(Duration::from_millis(200), victim_inbound.next()).await;
    assert!(
        poll.is_err() || poll.as_ref().is_ok_and(|m| m.is_some()),
        "victim stream stayed open (timeout or got a message, not None)"
    );
    drop(vtx);
    let _ = tokio::time::timeout(Duration::from_secs(2), victim_inbound.next()).await;
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
                    peak_cpu_cores: 0.0,
                    node_name: None,
                    hw_class: None,
                    final_resources: None,
                },
            )),
        })
        .await?;

    // InfrastructureFailure → handle_infrastructure_failure →
    // reset_to_ready → re-dispatch. Proof-of-processing: a SECOND
    // WorkAssignment arrives. If the None-result were silently
    // dropped, the drv would stay stuck Assigned from the first
    // dispatch and no second assignment would ever come.
    //
    // One-shot workers drain on completion (any terminal status), so
    // re-dispatch goes to a fresh worker. Register one.
    let (stream_tx2, stream_rx2) = mpsc::channel::<rio_proto::types::ExecutorMessage>(8);
    stream_tx2
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "none-worker-2".into(),
                },
            )),
        })
        .await?;
    let outbound2 = tokio_stream::wrappers::ReceiverStream::new(stream_rx2);
    let mut inbound2 = worker_client.build_execution(outbound2).await?.into_inner();
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            executor_id: "none-worker-2".into(),
            kind: rio_proto::types::ExecutorKind::Builder as i32,
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        })
        .await?;

    let reassignment = tokio::time::timeout(Duration::from_secs(5), inbound2.next())
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
        info.retry.failed_builders.is_empty(),
        "synthesized InfrastructureFailure must route to handle_infrastructure_failure \
         (NOT handle_transient_failure), got failed_builders={:?}",
        info.retry.failed_builders
    );
    assert_eq!(
        info.retry.count, 0,
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
