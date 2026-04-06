//! AdminService test suite.
//!
//! Shared fixtures (`setup_svc`, `setup_svc_default`, `collect_stream`)
//! live here and are `pub(super)` for the per-domain submodules. Tests
//! for handlers that remain inline in `admin/mod.rs` post-P0383
//! (`ClusterStatus`, `DrainExecutor`, `ClearPoison`, `admin_rpcs_are_wired`
//! smoke test) stay in this file — everything else mirrors the
//! `admin/{logs,gc,tenants,builds,workers,graph,sizeclass}.rs` submodule
//! seams.

use super::*;
use crate::actor::tests::setup_actor;
use rio_test_support::TestDb;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

mod builds_tests;
mod gc_tests;
mod graph_tests;
mod logs_tests;
mod manifest_tests;
mod sizeclass_tests;
mod tenants_tests;
mod workers_tests;

/// Set up `AdminServiceImpl` with a live actor but no S3.
///
/// The GetBuildLogs tests don't exercise the actor (they hit ring
/// buffer or S3 directly), but the constructor needs a handle.
/// `setup_actor` gives a real actor backed by the same PG — no
/// mocks needed. The `_task` keeps the actor task alive; dropping
/// the returned tuple drops the handle → channel closes → actor
/// shuts down cleanly.
///
/// Returns `(svc, actor_handle, task)`. The handle is separate so
/// ClusterStatus tests can also send actor commands directly.
pub(super) async fn setup_svc(
    buffers: Arc<LogBuffers>,
    s3: Option<(S3Client, String)>,
) -> (
    AdminServiceImpl,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    TestDb,
) {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let (actor, task) = setup_actor(db.pool.clone());
    let svc = AdminServiceImpl::new(
        buffers,
        s3,
        db.pool.clone(),
        actor.clone(),
        // store_addr: unreachable in tests. TriggerGC would fail
        // the proxy connect with a clear error. Use :1 (fails
        // fast, never listened on) not a timeout-prone addr.
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        rio_common::signal::Token::new(),
    );
    (svc, actor, task, db)
}

/// `setup_svc` with the common defaults (empty log buffers, no S3).
pub(super) async fn setup_svc_default() -> (
    AdminServiceImpl,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    TestDb,
) {
    let buffers = Arc::new(LogBuffers::new());
    setup_svc(buffers, None).await
}

pub(super) async fn collect_stream(
    stream: ReceiverStream<Result<BuildLogChunk, Status>>,
) -> Vec<BuildLogChunk> {
    stream.filter_map(|r| r.ok()).collect::<Vec<_>>().await
}

/// Unwrap an `Ok(Response)` whose stream yields exactly one `Err(Status)`.
///
/// `get_build_logs` returns errors as stream items (not handler-level
/// `Err`) for grpc-web compatibility — see `err_stream` in `logs.rs`.
pub(super) async fn expect_stream_err(
    result: Result<Response<ReceiverStream<Result<BuildLogChunk, Status>>>, Status>,
) -> Status {
    let mut stream = result
        .expect("handler should return Ok(stream), error is in-stream")
        .into_inner();
    let status = stream
        .next()
        .await
        .expect("stream should yield one item")
        .expect_err("stream item should be Err(Status)");
    assert!(
        stream.next().await.is_none(),
        "stream should end after the error"
    );
    status
}

#[tokio::test]
async fn admin_rpcs_are_wired() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    // All phase4a RPCs are wired (no remaining stubs). This test proves
    // each returns a non-Unimplemented error or success — NOT full
    // behavior coverage (see dedicated tests below).

    // ListExecutors is implemented — no workers → empty list, not error.
    let lw = svc
        .list_executors(Request::new(ListExecutorsRequest::default()))
        .await?
        .into_inner();
    assert!(
        lw.executors.is_empty(),
        "no workers registered → empty list"
    );

    // ListBuilds is implemented — no builds → empty list, not error.
    let lb = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert!(lb.builds.is_empty(), "no builds → empty list");
    assert_eq!(lb.total_count, 0);
    // TriggerGC: now implemented (proxy to store). Test separately.
    // With the test store_addr (127.0.0.1:1), proxy connect fails
    // with Unavailable (not Unimplemented) — proves it's wired.
    let gc_err = svc
        .trigger_gc(Request::new(GcRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(
        gc_err.code(),
        tonic::Code::Unavailable,
        "TriggerGC implemented but store unreachable in tests"
    );
    // ClearPoison is now implemented. Default request has empty
    // derivation_hash → InvalidArgument. Proves it's wired (no
    // longer Unimplemented).
    assert_eq!(
        svc.clear_poison(Request::new(ClearPoisonRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    // Non-empty but non-poisoned → cleared=false (not an error).
    let resp = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "nonexistent-drv-hash".into(),
        }))
        .await?
        .into_inner();
    assert!(!resp.cleared, "non-poisoned drv → cleared=false");
    // ListPoisoned on empty DAG → empty list (not an error).
    let resp = svc.list_poisoned(Request::new(())).await?.into_inner();
    assert!(resp.derivations.is_empty());
    Ok(())
}

// -----------------------------------------------------------------------
// ClusterStatus
// -----------------------------------------------------------------------

#[tokio::test]
async fn cluster_status_empty() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let resp = svc.cluster_status(Request::new(())).await?.into_inner();

    assert_eq!(resp.total_executors, 0);
    assert_eq!(resp.active_executors, 0);
    assert_eq!(resp.draining_executors, 0);
    assert_eq!(resp.pending_builds, 0);
    assert_eq!(resp.active_builds, 0);
    assert_eq!(resp.queued_derivations, 0);
    assert_eq!(resp.running_derivations, 0);
    assert_eq!(
        resp.store_size_bytes, 0,
        "store_size bg refresh not spawned in tests → stays at initial 0"
    );

    // uptime_since is "now minus elapsed since construction" → within
    // a few hundred ms of now. Wide tolerance (10s) to survive slow CI.
    let uptime = resp.uptime_since.expect("uptime_since always set");
    let now = prost_types::Timestamp::from(SystemTime::now());
    let delta = now.seconds - uptime.seconds;
    assert!(
        (0..10).contains(&delta),
        "uptime_since should be recent (within 10s), got delta={delta}s"
    );
    Ok(())
}

#[tokio::test]
async fn cluster_status_counts_registered_workers() -> anyhow::Result<()> {
    use crate::actor::tests::connect_executor;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Stream-only worker (no heartbeat) → total=1, active=0.
    // is_registered() requires BOTH stream_tx AND system; this has
    // only the stream. The autoscaler should NOT count it as
    // available capacity.
    let (stream_tx, _rx1) = mpsc::channel(16);
    actor
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "stream-only".into(),
            stream_tx,
        })
        .await?;

    // Fully registered worker (stream + heartbeat) → active.
    let _rx2 = connect_executor(&actor, "full", "x86_64-linux", 4).await?;

    let resp = svc.cluster_status(Request::new(())).await?.into_inner();

    assert_eq!(resp.total_executors, 2);
    assert_eq!(
        resp.active_executors, 1,
        "only 'full' is registered (stream+heartbeat); 'stream-only' has no heartbeat"
    );
    assert_eq!(resp.draining_executors, 0, "no workers draining");
    Ok(())
}

#[tokio::test]
async fn cluster_status_counts_queued_and_running() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_executor, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Worker with max_builds=1: will accept exactly one assignment,
    // leaving the second derivation in ready_queue.
    let mut worker_rx = connect_executor(&actor, "w1", "x86_64-linux", 1).await?;

    // Two independent single-node DAGs. First dispatches (worker has
    // capacity 1), second stays queued.
    let _ev1 =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let _ev2 =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

    // Drain the assignment for 'a' — dispatch happened synchronously
    // during merge (dispatch_ready is called after merge completes),
    // but the message is in the channel. Receiving it doesn't change
    // actor state (worker ack → Running requires a separate
    // ProcessCompletion roundtrip we're not doing here), it just
    // proves the assignment went out.
    let msg = worker_rx.recv().await.expect("assignment for first drv");
    assert!(matches!(
        msg.msg,
        Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
    ));

    let resp = svc.cluster_status(Request::new(())).await?.into_inner();

    // First derivation is Assigned (worker slot reserved, not yet acked
    // → Running). Second is in ready_queue (no capacity). BOTH builds
    // transition to Active on merge (merge.rs sets Active as soon as
    // derivations are tracked, not waiting for dispatch).
    assert_eq!(
        resp.active_builds, 2,
        "both builds transitioned to Active on merge"
    );
    assert_eq!(resp.pending_builds, 0);
    assert_eq!(
        resp.queued_derivations, 1,
        "second drv waiting for capacity (worker max_builds=1 full)"
    );
    assert_eq!(
        resp.running_derivations, 1,
        "first drv is Assigned → counts as running (slot reserved)"
    );
    Ok(())
}

#[tokio::test]
async fn cluster_status_actor_dead_returns_unavailable() -> anyhow::Result<()> {
    // Set up, then drop the handle + abort the task → actor channel closes.
    // check_actor_alive() catches this before the oneshot would hang.
    let (svc, actor, task, _db) = setup_svc_default().await;
    drop(actor);
    task.abort();
    // Give tokio a tick to process the abort.
    tokio::task::yield_now().await;

    // The svc still holds an ActorHandle clone. is_alive() checks
    // tx.is_closed() which becomes true when the receiver drops
    // (actor task gone). Abort + drop(actor) both contribute — abort
    // kills the task (receiver drops), drop(actor) removes one of
    // the two senders. The svc's clone is the last sender.
    //
    // Poll is_closed via the svc's handle until it flips. Bounded
    // loop (if it never flips in 100 yields, something's wrong).
    for _ in 0..100 {
        if !svc.actor.is_alive() {
            break;
        }
        tokio::task::yield_now().await;
    }

    let result = svc.cluster_status(Request::new(())).await;
    let status = result.expect_err("should be Unavailable");
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert!(status.message().contains("actor"));
    Ok(())
}

// -----------------------------------------------------------------------
// DrainExecutor
// -----------------------------------------------------------------------

#[tokio::test]
async fn drain_worker_empty_id_invalid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let result = svc
        .drain_executor(Request::new(DrainExecutorRequest {
            executor_id: String::new(),
            force: false,
        }))
        .await;

    let status = result.expect_err("empty executor_id should be InvalidArgument");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("executor_id"));
    Ok(())
}

#[tokio::test]
async fn drain_worker_unknown_not_error() -> anyhow::Result<()> {
    // Unknown worker → accepted=false, running=0. NOT gRPC error:
    // preStop may race with ExecutorDisconnected (SIGTERM → select!
    // break → stream drop → actor removes entry → preStop's drain
    // call arrives to an empty slot). The worker proceeds as if
    // drain succeeded — nothing to wait for.
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let resp = svc
        .drain_executor(Request::new(DrainExecutorRequest {
            executor_id: "ghost".into(),
            force: false,
        }))
        .await?
        .into_inner();

    assert!(!resp.accepted, "unknown worker → accepted=false");
    assert_eq!(resp.running_builds, 0);
    Ok(())
}

#[tokio::test]
async fn drain_worker_stops_dispatch() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_executor, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Worker with max_builds=4: plenty of capacity.
    let mut worker_rx = connect_executor(&actor, "w1", "x86_64-linux", 4).await?;

    // First drv: dispatches normally.
    let _ev1 =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let msg1 = worker_rx.recv().await.expect("first assignment");
    assert!(matches!(
        msg1.msg,
        Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
    ));

    // Drain. running=1 (the drv we just dispatched is Assigned on w1).
    let resp = svc
        .drain_executor(Request::new(DrainExecutorRequest {
            executor_id: "w1".into(),
            force: false,
        }))
        .await?
        .into_inner();
    assert!(resp.accepted);
    assert_eq!(
        resp.running_builds, 1,
        "the first drv is in-flight (Assigned)"
    );

    // Second drv: should NOT dispatch. Worker has capacity (1/4 slots
    // used) but has_capacity() now returns false (draining).
    let _ev2 =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

    // Can't easily assert "nothing arrived" without a timeout. Instead,
    // check ClusterStatus: the second drv should be queued, not running.
    let status = svc.cluster_status(Request::new(())).await?.into_inner();
    assert_eq!(status.queued_derivations, 1, "second drv waiting (drained)");
    assert_eq!(status.running_derivations, 1, "only first drv on worker");
    assert_eq!(status.draining_executors, 1);
    assert_eq!(
        status.active_executors, 0,
        "draining worker is NOT active — controller sees capacity=0"
    );

    // Idempotent: second drain → same running count, still accepted.
    let resp2 = svc
        .drain_executor(Request::new(DrainExecutorRequest {
            executor_id: "w1".into(),
            force: false,
        }))
        .await?
        .into_inner();
    assert!(resp2.accepted);
    assert_eq!(resp2.running_builds, 1);
    Ok(())
}

#[tokio::test]
async fn drain_worker_force_reassigns() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_executor, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Two workers: w1 gets the first dispatch, then we force-drain it.
    // The reassigned drv should go to w2 on the next dispatch.
    let mut rx1 = connect_executor(&actor, "w1", "x86_64-linux", 2).await?;
    let mut rx2 = connect_executor(&actor, "w2", "x86_64-linux", 2).await?;

    let _ev =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;

    // ONE of them got it. With two equal-score workers (no bloom,
    // both 0/2 load), best_executor's tiebreak is HashMap iteration
    // order → nondeterministic. Poll both with try_recv to find which.
    let (first_worker, other_rx) = if let Ok(msg) = rx1.try_recv() {
        assert!(matches!(
            msg.msg,
            Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
        ));
        ("w1", &mut rx2)
    } else {
        let msg = rx2
            .try_recv()
            .expect("one of w1/w2 must have the assignment");
        assert!(matches!(
            msg.msg,
            Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
        ));
        ("w2", &mut rx1)
    };

    // Force-drain the worker that got it. running=0 in response:
    // force reassigns then replies, so nothing is left.
    let resp = svc
        .drain_executor(Request::new(DrainExecutorRequest {
            executor_id: first_worker.into(),
            force: true,
        }))
        .await?
        .into_inner();
    assert!(resp.accepted);
    assert_eq!(
        resp.running_builds, 0,
        "force=true reassigned → running=0 (caller doesn't wait)"
    );

    // reassign_derivations pushes to ready_queue but dispatch_ready
    // isn't called from handle_drain_executor — it fires on the NEXT
    // heartbeat/merge/completion. Send a heartbeat to trigger it.
    actor
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            executor_id: first_worker.into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 2,
            running_builds: vec![],
            bloom: None,
            size_class: None,
        })
        .await?;

    // The OTHER worker should now get the reassigned drv.
    let msg = other_rx
        .recv()
        .await
        .expect("reassigned drv to other worker");
    assert!(matches!(
        msg.msg,
        Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
    ));

    // ClusterStatus: 1 draining, 1 active, 1 running (on the other
    // worker now), 0 queued.
    let status = svc.cluster_status(Request::new(())).await?.into_inner();
    assert_eq!(status.draining_executors, 1);
    assert_eq!(status.active_executors, 1);
    assert_eq!(
        status.running_derivations, 1,
        "drv re-Assigned to other worker"
    );
    assert_eq!(status.queued_derivations, 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// ClearPoison happy path
// ---------------------------------------------------------------------------

// r[verify sched.admin.clear-poison]
/// Poison a derivation via PermanentFailure, then ClearPoison it.
/// Verifies cleared=true, in-mem status reset, PG poisoned_at cleared.
#[tokio::test]
async fn test_clear_poison_happy_path() -> anyhow::Result<()> {
    use crate::actor::tests::{
        complete_failure, connect_executor, merge_single_node, test_drv_path,
    };
    use crate::state::PriorityClass;

    let (svc, actor, _task, db) = setup_svc_default().await;
    let mut worker_rx = connect_executor(&actor, "poison-w", "x86_64-linux", 1).await?;

    // Merge → dispatches to worker.
    let _ev = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "poison-me",
        PriorityClass::Scheduled,
    )
    .await?;
    let _ = worker_rx.recv().await.expect("assignment");

    // PermanentFailure → poisoned (both in-mem and PG).
    complete_failure(
        &actor,
        "poison-w",
        &test_drv_path("poison-me"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "test permanent failure",
    )
    .await?;

    // Verify poisoned (barrier via debug_query).
    let pre = actor
        .debug_query_derivation("poison-me")
        .await?
        .expect("exists");
    assert_eq!(pre.status, crate::state::DerivationStatus::Poisoned);
    // PG should have poisoned_at set (the as_bytes() bug would break this).
    let pg_poisoned: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM poisoned_at)::float8 FROM derivations WHERE drv_hash=$1",
    )
    .bind("poison-me")
    .fetch_one(&db.pool)
    .await?;
    assert!(
        pg_poisoned.is_some(),
        "poisoned_at should be persisted to PG"
    );

    // r[verify sched.admin.list-poisoned]
    // ListPoisoned should now return it with the full .drv path
    // (what ClearPoison takes as input).
    let listed = svc.list_poisoned(Request::new(())).await?.into_inner();
    assert_eq!(listed.derivations.len(), 1);
    assert_eq!(listed.derivations[0].drv_path, test_drv_path("poison-me"));
    // PermanentFailure poisons immediately without appending to
    // failed_builders (that's the transient-retry path), so this can
    // be empty. Just check the field is wired.
    let _ = &listed.derivations[0].failed_executors;

    // ClearPoison via RPC.
    let resp = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "poison-me".into(),
        }))
        .await?
        .into_inner();
    assert!(resp.cleared, "happy path → cleared=true");

    // In-mem: node removed from DAG (next submit re-inserts it fresh
    // with full proto fields — see Dag::remove_node rationale).
    let post = actor.debug_query_derivation("poison-me").await?;
    assert!(post.is_none(), "node removed from DAG on ClearPoison");

    // PG: poisoned_at cleared.
    let pg_poisoned: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM poisoned_at)::float8 FROM derivations WHERE drv_hash=$1",
    )
    .bind("poison-me")
    .fetch_one(&db.pool)
    .await?;
    assert!(
        pg_poisoned.is_none(),
        "clear_poison should NULL poisoned_at"
    );

    Ok(())
}

/// ClearPoison ordering regression: PG clear must precede in-mem
/// reset so that a PG failure leaves in-mem Poisoned for retry.
/// The old order (in-mem first) left status=Created on PG blip →
/// retry hit the not-poisoned guard → permanent no-op.
#[tokio::test]
async fn test_clear_poison_pg_failure_leaves_inmem_poisoned_for_retry() -> anyhow::Result<()> {
    use crate::actor::tests::{
        complete_failure, connect_executor, merge_single_node, test_drv_path,
    };
    use crate::state::PriorityClass;

    let (svc, actor, _task, db) = setup_svc_default().await;
    let mut worker_rx = connect_executor(&actor, "pg-blip-w", "x86_64-linux", 1).await?;

    let _ev = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "pg-blip",
        PriorityClass::Scheduled,
    )
    .await?;
    let _ = worker_rx.recv().await.expect("assignment");
    complete_failure(
        &actor,
        "pg-blip-w",
        &test_drv_path("pg-blip"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "test",
    )
    .await?;

    // Confirm poisoned.
    let pre = actor
        .debug_query_derivation("pg-blip")
        .await?
        .expect("exists");
    assert_eq!(pre.status, crate::state::DerivationStatus::Poisoned);

    // Simulate PG blip: close the pool. All subsequent queries fail.
    db.pool.close().await;

    // First attempt: PG fails → cleared=false, in-mem STILL Poisoned.
    let resp1 = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "pg-blip".into(),
        }))
        .await?
        .into_inner();
    assert!(!resp1.cleared, "PG failure → cleared=false");

    let post1 = actor
        .debug_query_derivation("pg-blip")
        .await?
        .expect("exists");
    assert_eq!(
        post1.status,
        crate::state::DerivationStatus::Poisoned,
        "PG failure must leave in-mem Poisoned so retry can proceed"
    );

    // Second attempt with PG still down: same result (proves retry
    // is still hitting the PG path, not the not-poisoned early-return).
    let resp2 = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "pg-blip".into(),
        }))
        .await?
        .into_inner();
    assert!(!resp2.cleared);
    let post2 = actor
        .debug_query_derivation("pg-blip")
        .await?
        .expect("exists");
    assert_eq!(post2.status, crate::state::DerivationStatus::Poisoned);

    Ok(())
}
