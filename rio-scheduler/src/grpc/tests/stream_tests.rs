//! `BuildExecution` bidi-stream + malformed-message handling tests.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395) to mirror
//! the `grpc/worker_service.rs` seam (P0356). Covers the worker-facing
//! stream: end-to-end assignment flow, log pipeline, heartbeat payload
//! bounds, and malformed-message paths (duplicate register, None-result
//! completion, empty stream).

use super::*;
use crate::grpc::executor_service::{
    MAX_DERIVATION_PATH_LEN, MAX_ERROR_MSG_LEN, MAX_IDENT_LEN, MAX_PHASE_LEN,
};
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
                    final_line_count: 0,
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
                    final_line_count: 0,
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

// ===========================================================================
// Worker-supplied field bounds (sched.executor.input-bounds)
// ===========================================================================

/// `phase.phase` is the sibling worker-supplied string of the round-8
/// `derivation_path` bound — and unlike the path it is accumulated
/// (`Event::Phase` is not display-only: `build_event_log` row + state
/// ring slot + `SetPhase` terminal render, × interested builds). An
/// oversized phase text must be rejected at the recv arm before the
/// `ForwardPhase` actor send; an oversized *path* is already rejected by
/// the round-8 check (asserted here too — it previously had no verify
/// marker on the Phase arm); a phase of exactly `MAX_PHASE_LEN` is
/// accepted (boundary).
///
/// Red-first: pre-fix, send 2's oversized phase passes the recv arm (the
/// drv is Assigned to this executor so it passes `handle_forward_phase`'s
/// gate) and arrives as the FIRST `Event::Phase` on the build stream —
/// the `len == MAX_PHASE_LEN` assertion fails. Post-fix only send 3's
/// boundary phase arrives.
// r[verify sched.executor.input-bounds+2]
// r[verify sched.log.path-length+2]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_phase_oversized_text_rejected_before_forward() -> anyhow::Result<()> {
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

    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(32);
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "phaselen-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            executor_id: "phaselen-worker".into(),
            kind: rio_proto::types::ExecutorKind::Builder as i32,
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        })
        .await?;

    let mut event_stream = sched_client
        .submit_build(rio_proto::types::SubmitBuildRequest {
            priority_class: "scheduled".into(),
            nodes: vec![make_node("phaselen-hash")],
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

    let send_phase = |path: String, phase: String| {
        let stream_tx = stream_tx.clone();
        async move {
            stream_tx
                .send(rio_proto::types::ExecutorMessage {
                    msg: Some(rio_proto::types::executor_message::Msg::Phase(
                        rio_proto::types::BuildPhase {
                            derivation_path: path,
                            phase,
                        },
                    )),
                })
                .await
        }
    };

    // 1. Oversized PATH → rejected by the round-8 check (path_too_long).
    //    Closes the round-8 test gap: the Phase arm's derivation_path
    //    check had no verify-marker coverage until now.
    send_phase(
        format!("/nix/store/{}.drv", "p".repeat(600)),
        "unpackPhase".into(),
    )
    .await?;
    // 2. Oversized PHASE TEXT for the assigned drv → must be rejected by
    //    the NEW check. Pre-fix this passes the recv arm and the actor's
    //    binding gate (the drv IS assigned to this executor) and arrives
    //    first on the event stream.
    send_phase(work.drv_path.clone(), "x".repeat(MAX_PHASE_LEN + 1)).await?;
    // 3. Phase of exactly MAX_PHASE_LEN → accepted (boundary). FIFO
    //    ordering means if either rejected phase had been forwarded it
    //    would arrive before this one.
    send_phase(work.drv_path.clone(), "b".repeat(MAX_PHASE_LEN)).await?;

    let got = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), event_stream.next())
            .await?
            .expect("event")
            .expect("not an error");
        if let Some(rio_proto::types::build_event::Event::Phase(p)) = ev.event {
            break p;
        }
    };
    assert_eq!(
        got.phase.len(),
        MAX_PHASE_LEN,
        "first Event::Phase on the build stream must be the boundary-length one — \
         an oversized phase text arriving here means the recv-arm bound did not fire"
    );
    assert!(got.phase.starts_with('b'));
    Ok(())
}

/// A `CompletionReport` with oversized `error_msg` / `node_name` /
/// `hw_class` is BOUNDED, NOT DROPPED: the completion still terminates
/// the build (a dropped completion strands the drv in Running), the
/// error_msg arrives truncated to `MAX_ERROR_MSG_LEN` on the build event
/// stream, and the pod-identity stamps fall back to `None`.
// r[verify sched.executor.input-bounds+2]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_completion_oversized_fields_bounded_not_dropped() -> anyhow::Result<()> {
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

    let (stream_tx, stream_rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(32);
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Register(
                rio_proto::types::ExecutorRegister {
                    executor_id: "complen-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            executor_id: "complen-worker".into(),
            kind: rio_proto::types::ExecutorKind::Builder as i32,
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        })
        .await?;

    let mut event_stream = sched_client
        .submit_build(rio_proto::types::SubmitBuildRequest {
            priority_class: "scheduled".into(),
            nodes: vec![make_node("complen-hash")],
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

    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Completion(
                rio_proto::types::CompletionReport {
                    drv_path: work.drv_path.clone(),
                    result: Some(rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                        error_msg: "e".repeat(MAX_ERROR_MSG_LEN + 100),
                        ..Default::default()
                    }),
                    assignment_token: work.assignment_token.clone(),
                    node_name: Some("n".repeat(MAX_IDENT_LEN + 1)),
                    hw_class: Some("h".repeat(MAX_IDENT_LEN + 1)),
                    ..Default::default()
                },
            )),
        })
        .await?;

    // The completion must be PROCESSED (not dropped): the build event
    // stream yields a Failed DerivationEvent whose error_message is the
    // truncated (not original, not empty) error_msg, then the build
    // terminates.
    let mut saw_failed_derivation = false;
    let mut saw_build_terminal = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), event_stream.next()).await;
        let Ok(Some(Ok(event))) = ev else {
            break;
        };
        match event.event {
            Some(rio_proto::types::build_event::Event::Derivation(d))
                if d.kind == rio_proto::types::DerivationEventKind::Failed as i32 =>
            {
                assert_eq!(
                    d.error_message.len(),
                    MAX_ERROR_MSG_LEN,
                    "error_message must be truncated to MAX_ERROR_MSG_LEN — the original \
                     was MAX_ERROR_MSG_LEN+100; an untruncated or empty value means the \
                     recv-arm bound did not fire or the report was dropped"
                );
                saw_failed_derivation = true;
            }
            Some(rio_proto::types::build_event::Event::Failed(_)) => {
                saw_build_terminal = true;
                break;
            }
            _ => {}
        }
    }
    assert!(
        saw_failed_derivation,
        "completion with oversized fields must still be processed (bounded, not dropped)"
    );
    assert!(
        saw_build_terminal,
        "build must reach a terminal state — a dropped CompletionReport strands it"
    );
    Ok(())
}

/// Regression guard, NOT red-first: an oversized `CompletionReport.drv_path`
/// is rejected with `continue`, not `break` — the stream stays open and a
/// subsequent legitimate batch is still processed. Pre-fix the oversized
/// report was forwarded to the actor and dropped there as "unknown
/// derivation", so this test passes before AND after the fix; it guards
/// against the one way the new check could be wrong (`break` would
/// disconnect the worker and strand its real build).
// r[verify sched.executor.input-bounds+2]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_completion_oversized_path_rejected() -> anyhow::Result<()> {
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
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
                    executor_id: "cplen-worker".into(),
                },
            )),
        })
        .await?;
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let mut inbound = worker_client.build_execution(outbound).await?.into_inner();

    // Oversized completion path → rejected at the recv arm.
    stream_tx
        .send(rio_proto::types::ExecutorMessage {
            msg: Some(rio_proto::types::executor_message::Msg::Completion(
                rio_proto::types::CompletionReport {
                    drv_path: format!("/nix/store/cplen-{}.drv", "e".repeat(600)),
                    result: Some(rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::Built.into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        })
        .await?;

    // The stream must still be open: `inbound.next()` times out rather
    // than returning None. A `break` at the recv arm would close the
    // server→worker half and `next()` would yield None immediately;
    // the 200ms budget is orders of magnitude past the recv loop's
    // processing of the (already-channel-delivered) completion.
    let poll = tokio::time::timeout(Duration::from_millis(200), inbound.next()).await;
    assert!(
        poll.is_err(),
        "stream must remain open after an oversized completion path (continue, not break); \
         got {poll:?}"
    );
    Ok(())
}

/// Heartbeat element-length bounds: each worker-supplied string field
/// over its bound → `InvalidArgument`; every field AT its bound → Ok.
/// Rejecting a hostile heartbeat is the designed recovery (the worker
/// times out and is reaped); the payload otherwise lives on
/// `ExecutorState` for the executor's lifetime.
// r[verify sched.executor.input-bounds+2]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_heartbeat_oversized_strings_rejected() -> anyhow::Result<()> {
    let (_handle, mut worker_client, _srv, _actor, _db) = setup_worker_svc().await?;

    let base = || rio_proto::types::HeartbeatRequest {
        executor_id: "hb-len-worker".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        systems: vec!["x86_64-linux".into()],
        ..Default::default()
    };

    // Each field over its bound, one at a time → InvalidArgument.
    let cases: Vec<(&str, rio_proto::types::HeartbeatRequest)> = vec![
        (
            "executor_id",
            rio_proto::types::HeartbeatRequest {
                executor_id: "x".repeat(MAX_IDENT_LEN + 1),
                ..base()
            },
        ),
        (
            "intent_id",
            rio_proto::types::HeartbeatRequest {
                intent_id: "x".repeat(MAX_IDENT_LEN + 1),
                ..base()
            },
        ),
        (
            "running_build",
            rio_proto::types::HeartbeatRequest {
                running_build: Some("x".repeat(MAX_DERIVATION_PATH_LEN + 1)),
                ..base()
            },
        ),
        (
            "systems element",
            rio_proto::types::HeartbeatRequest {
                systems: vec!["x".repeat(MAX_IDENT_LEN + 1)],
                ..base()
            },
        ),
        (
            "supported_features element",
            rio_proto::types::HeartbeatRequest {
                supported_features: vec!["x".repeat(MAX_IDENT_LEN + 1)],
                ..base()
            },
        ),
    ];
    for (field, req) in cases {
        let err = worker_client
            .heartbeat(req)
            .await
            .expect_err(&format!("oversized {field} must be rejected"));
        assert_eq!(
            err.code(),
            tonic::Code::InvalidArgument,
            "oversized {field} → InvalidArgument, got {err:?}"
        );
    }

    // Every field AT its bound → accepted.
    worker_client
        .heartbeat(rio_proto::types::HeartbeatRequest {
            executor_id: "x".repeat(MAX_IDENT_LEN),
            intent_id: "y".repeat(MAX_IDENT_LEN),
            running_build: Some("z".repeat(MAX_DERIVATION_PATH_LEN)),
            systems: vec!["s".repeat(MAX_IDENT_LEN)],
            supported_features: vec!["f".repeat(MAX_IDENT_LEN)],
            kind: rio_proto::types::ExecutorKind::Builder as i32,
            ..Default::default()
        })
        .await
        .expect("every field at its bound must be accepted (boundary)");
    Ok(())
}

/// The heartbeat reply must not advertise the lease-derived generation
/// while recovery is incomplete: an advertised-but-unclaimed generation
/// latched by a worker from a leader that dies mid-recovery is recorded
/// nowhere, so after a Lease deletion the surviving previous holder
/// legitimately retains its lower claimed generation and the latched
/// workers silently reject every assignment of the active leader. The
/// reply carries the 0 sentinel until `recovery_complete`, then the
/// post-recovery generation. The other half of the end-to-end property
/// — the claim landing before recovery completes — is owned and
/// verified by `sched.lease.generation-claim` (see the actor recovery
/// seeding/claim tests).
// r[verify sched.lease.claim-before-advertise]
#[tokio::test]
async fn heartbeat_advertises_generation_only_after_recovery_complete() -> anyhow::Result<()> {
    use rio_proto::ExecutorService;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    let db = TestDb::new(&MIGRATOR).await;
    // Simulate the recovery window: the acquire edge has raised the
    // generation Arc to 7 and flipped is_leader, but recovery has not
    // completed yet. Keep a LeaderState clone so the test can complete
    // recovery for the current acquire-epoch mid-test.
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(AtomicU64::new(7)),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
    });
    // `new_for_tests` keeps its own always-true `is_leader` Arc — that
    // is what lets the heartbeat pass `ensure_leader` while the
    // fixture's `recovery_complete` is false; do not "fix" the apparent
    // mismatch.
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let resp = grpc
        .heartbeat(Request::new(rio_proto::types::HeartbeatRequest {
            executor_id: "fence-w1".into(),
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert!(
        resp.accepted,
        "the heartbeat RPC stays available during recovery (executor re-registration \
         and readiness must proceed); only the generation payload is withheld"
    );
    assert_eq!(
        resp.generation, 0,
        "an incomplete recovery must advertise the 0 sentinel, not the raw generation"
    );

    // Recovery completes → the post-recovery generation is advertised
    // (also pins that generation_reader() is wired to the leader's
    // actual recovery-completion state).
    leader.set_recovery_complete(leader.acquired_transitions());
    let resp = grpc
        .heartbeat(Request::new(rio_proto::types::HeartbeatRequest {
            executor_id: "fence-w1".into(),
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert!(resp.accepted);
    assert_eq!(
        resp.generation, 7,
        "after recovery completes the post-recovery generation is advertised"
    );
    Ok(())
}
