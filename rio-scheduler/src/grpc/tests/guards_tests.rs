//! `actor_guards.rs` coverage — error-mapping + leader-gate.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395) to mirror
//! the `grpc/actor_guards.rs` seam (P0356). Three synchronous unit
//! tests for helper fns (`actor_error_to_status`, `parse_build_id`)
//! plus the leader-guard RPC-rejection end-to-end.

use super::*;
use rio_proto::{ExecutorService, ExecutorServiceClient, SchedulerService};

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
    // any async work). Traits already in scope at file level.
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

    // ExecutorService handlers.
    let s = grpc
        .heartbeat(tonic::Request::new(Default::default()))
        .await
        .expect_err("standby should reject heartbeat");
    assert_eq!(s.code(), tonic::Code::Unavailable);

    // BuildExecution: the request is a Streaming<ExecutorMessage>. We
    // can't easily construct one synthetically outside a real gRPC
    // call. Spin up a server for this one.
    let router = tonic::transport::Server::builder()
        .add_service(rio_proto::ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut worker_client = ExecutorServiceClient::new(channel);
    let (_tx, rx) = mpsc::channel::<rio_proto::types::ExecutorMessage>(1);
    let s = worker_client
        .build_execution(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .expect_err("standby should reject build_execution");
    assert_eq!(s.code(), tonic::Code::Unavailable);

    Ok(())
}
