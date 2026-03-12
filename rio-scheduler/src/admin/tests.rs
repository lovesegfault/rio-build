use super::*;
use crate::actor::tests::setup_actor;
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_mocks::{RuleMode, mock, mock_client};
use rio_proto::types::BuildLogBatch;
use rio_test_support::TestDb;
use tokio_stream::StreamExt;

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
async fn setup_svc(
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
    );
    (svc, actor, task, db)
}

fn mk_batch(drv_path: &str, first_line: u64, lines: &[&[u8]]) -> BuildLogBatch {
    BuildLogBatch {
        derivation_path: drv_path.to_string(),
        lines: lines.iter().map(|l| l.to_vec()).collect(),
        first_line_number: first_line,
        worker_id: "test-worker".into(),
    }
}

async fn collect_stream(
    stream: ReceiverStream<Result<BuildLogChunk, Status>>,
) -> Vec<BuildLogChunk> {
    stream.filter_map(|r| r.ok()).collect::<Vec<_>>().await
}

#[tokio::test]
async fn get_build_logs_from_ring_buffer() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    buffers.push(&mk_batch(
        "/nix/store/abc-test.drv",
        0,
        &[b"line0", b"line1", b"line2"],
    ));

    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let resp = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: String::new(), // not needed for ring buffer
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;

    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1, "3 lines < CHUNK_LINES → one chunk");
    assert_eq!(chunks[0].lines.len(), 3);
    assert_eq!(chunks[0].lines[0], b"line0");
    assert_eq!(chunks[0].first_line_number, 0);
    assert!(
        !chunks[0].is_complete,
        "ring buffer serve → still active, is_complete=false"
    );
    Ok(())
}

#[tokio::test]
async fn get_build_logs_since_line_filters() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    buffers.push(&mk_batch(
        "/nix/store/abc-test.drv",
        0,
        &[b"l0", b"l1", b"l2", b"l3", b"l4"],
    ));

    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let resp = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: String::new(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 3,
        }))
        .await?;

    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 2, "since_line=3 → only lines 3,4");
    assert_eq!(chunks[0].first_line_number, 3);
    Ok(())
}

#[tokio::test]
async fn get_build_logs_from_s3_fallback() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let build_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'succeeded')")
        .bind(build_id)
        .execute(&db.pool)
        .await?;

    // Gzip a test log the same way the flusher does.
    let gzipped = {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        for line in ["from-s3-0", "from-s3-1", "from-s3-2"] {
            enc.write_all(line.as_bytes())?;
            enc.write_all(b"\n")?;
        }
        enc.finish()?
    };

    // Seed the PG row the flusher would have written.
    sqlx::query(
        "INSERT INTO build_logs (build_id, drv_hash, s3_key, line_count, byte_size, is_complete)
         VALUES ($1, $2, $3, $4, $5, true)",
    )
    .bind(build_id)
    .bind("abc-test.drv")
    .bind(format!("logs/{build_id}/abc-test.drv.log.gz"))
    .bind(3_i64)
    .bind(gzipped.len() as i64)
    .execute(&db.pool)
    .await?;

    // Mock S3 to return the gzipped blob.
    let rule = mock!(S3Client::get_object).then_output(move || {
        GetObjectOutput::builder()
            .body(ByteStream::from(gzipped.clone()))
            .build()
    });
    let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);

    // Ring buffer is EMPTY — forces S3 fallback.
    //
    // Can't use setup_svc here: this test seeds PG rows BEFORE
    // constructing svc (the flusher-written build_logs row), and
    // setup_svc creates its own TestDb. Wire manually.
    let buffers = Arc::new(LogBuffers::new());
    let (actor, _task) = setup_actor(db.pool.clone());
    let svc = AdminServiceImpl::new(
        buffers,
        Some((s3, "test-bucket".into())),
        db.pool.clone(),
        actor,
        "127.0.0.1:1".into(),
    );

    let resp = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: build_id.to_string(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;

    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 3);
    assert_eq!(chunks[0].lines[0], b"from-s3-0");
    assert_eq!(chunks[0].lines[2], b"from-s3-2");
    assert!(
        chunks[0].is_complete,
        "S3 serve → derivation finished, is_complete=true"
    );
    Ok(())
}

#[tokio::test]
async fn get_build_logs_not_found_in_either() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    // No S3 configured, buffer empty.
    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let result = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: uuid::Uuid::new_v4().to_string(),
            derivation_path: "/nix/store/nowhere.drv".into(),
            since_line: 0,
        }))
        .await;

    let status = result.expect_err("should be NotFound");
    assert_eq!(status.code(), tonic::Code::NotFound);
    Ok(())
}

#[tokio::test]
async fn get_build_logs_empty_drv_path_invalid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    let result = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: String::new(),
            derivation_path: String::new(),
            since_line: 0,
        }))
        .await;

    let status = result.expect_err("should be InvalidArgument");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("derivation_path"));
    Ok(())
}

#[tokio::test]
async fn stubs_return_unimplemented() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    // ClusterStatus, DrainWorker, TriggerGC are no longer stubs.
    // TriggerGC is tested separately (proxy to store); here we just
    // confirm the remaining stubs.
    assert_eq!(
        svc.list_workers(Request::new(ListWorkersRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unimplemented
    );
    assert_eq!(
        svc.list_builds(Request::new(ListBuildsRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unimplemented
    );
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
    assert_eq!(
        svc.clear_poison(Request::new(ClearPoisonRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unimplemented
    );
    Ok(())
}

#[test]
fn extract_drv_hash_strips_store_prefix() {
    assert_eq!(extract_drv_hash("/nix/store/abc-foo.drv"), "abc-foo.drv");
    assert_eq!(extract_drv_hash("already-a-hash"), "already-a-hash");
}

#[test]
fn gunzip_and_chunk_roundtrip() -> anyhow::Result<()> {
    // Gzip → gunzip_and_chunk → lines match.
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    for i in 0..5 {
        enc.write_all(format!("line-{i}").as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let gz = enc.finish()?;

    let chunks = gunzip_and_chunk(&gz, "test", 0)?;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 5, "trailing \\n artifact stripped");
    assert_eq!(chunks[0].lines[0], b"line-0");
    assert_eq!(chunks[0].lines[4], b"line-4");
    assert!(chunks[0].is_complete);
    Ok(())
}

#[test]
fn gunzip_and_chunk_since_filtering() -> anyhow::Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    for i in 0..5 {
        enc.write_all(format!("l{i}").as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let gz = enc.finish()?;

    let chunks = gunzip_and_chunk(&gz, "test", 3)?;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 2, "since=3 → lines 3,4 only");
    assert_eq!(chunks[0].first_line_number, 3);
    Ok(())
}

// -----------------------------------------------------------------------
// ClusterStatus
// -----------------------------------------------------------------------

#[tokio::test]
async fn cluster_status_empty() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    let resp = svc.cluster_status(Request::new(())).await?.into_inner();

    assert_eq!(resp.total_workers, 0);
    assert_eq!(resp.active_workers, 0);
    assert_eq!(resp.draining_workers, 0);
    assert_eq!(resp.pending_builds, 0);
    assert_eq!(resp.active_builds, 0);
    assert_eq!(resp.queued_derivations, 0);
    assert_eq!(resp.running_derivations, 0);
    assert_eq!(
        resp.store_size_bytes, 0,
        "scheduler doesn't track store size"
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
    use crate::actor::tests::connect_worker;

    let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    // Stream-only worker (no heartbeat) → total=1, active=0.
    // is_registered() requires BOTH stream_tx AND system; this has
    // only the stream. The autoscaler should NOT count it as
    // available capacity.
    let (stream_tx, _rx1) = mpsc::channel(16);
    actor
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "stream-only".into(),
            stream_tx,
        })
        .await?;

    // Fully registered worker (stream + heartbeat) → active.
    let _rx2 = connect_worker(&actor, "full", "x86_64-linux", 4).await?;

    let resp = svc.cluster_status(Request::new(())).await?.into_inner();

    assert_eq!(resp.total_workers, 2);
    assert_eq!(
        resp.active_workers, 1,
        "only 'full' is registered (stream+heartbeat); 'stream-only' has no heartbeat"
    );
    assert_eq!(resp.draining_workers, 0, "no workers draining");
    Ok(())
}

#[tokio::test]
async fn cluster_status_counts_queued_and_running() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_worker, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    // Worker with max_builds=1: will accept exactly one assignment,
    // leaving the second derivation in ready_queue.
    let mut worker_rx = connect_worker(&actor, "w1", "x86_64-linux", 1).await?;

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
    let (svc, actor, task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;
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
// DrainWorker
// -----------------------------------------------------------------------

#[tokio::test]
async fn drain_worker_empty_id_invalid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    let result = svc
        .drain_worker(Request::new(DrainWorkerRequest {
            worker_id: String::new(),
            force: false,
        }))
        .await;

    let status = result.expect_err("empty worker_id should be InvalidArgument");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("worker_id"));
    Ok(())
}

#[tokio::test]
async fn drain_worker_unknown_not_error() -> anyhow::Result<()> {
    // Unknown worker → accepted=false, running=0. NOT gRPC error:
    // preStop may race with WorkerDisconnected (SIGTERM → select!
    // break → stream drop → actor removes entry → preStop's drain
    // call arrives to an empty slot). The worker proceeds as if
    // drain succeeded — nothing to wait for.
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    let resp = svc
        .drain_worker(Request::new(DrainWorkerRequest {
            worker_id: "ghost".into(),
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
    use crate::actor::tests::{connect_worker, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    // Worker with max_builds=4: plenty of capacity.
    let mut worker_rx = connect_worker(&actor, "w1", "x86_64-linux", 4).await?;

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
        .drain_worker(Request::new(DrainWorkerRequest {
            worker_id: "w1".into(),
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
    assert_eq!(status.draining_workers, 1);
    assert_eq!(
        status.active_workers, 0,
        "draining worker is NOT active — controller sees capacity=0"
    );

    // Idempotent: second drain → same running count, still accepted.
    let resp2 = svc
        .drain_worker(Request::new(DrainWorkerRequest {
            worker_id: "w1".into(),
            force: false,
        }))
        .await?
        .into_inner();
    assert!(resp2.accepted);
    assert_eq!(resp2.running_builds, 1);
    Ok(())
}

/// GetBuildLogs with a non-empty but malformed build_id → InvalidArgument.
/// The ring buffer is empty (no match on drv_path), so it falls through
/// to the S3 path, which parses build_id.
#[tokio::test]
async fn test_get_build_logs_invalid_uuid() -> anyhow::Result<()> {
    // Ring buffer empty → forces S3 fallback → build_id parse.
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    let result = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: "not-a-uuid".into(),
            derivation_path: "/nix/store/nowhere.drv".into(),
            since_line: 0,
        }))
        .await;

    let status = result.expect_err("invalid build_id should be InvalidArgument");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("build_id"),
        "error should mention build_id: {}",
        status.message()
    );
    Ok(())
}

/// TriggerGC with store unreachable → Status::Unavailable.
/// The store_addr in setup_svc is `127.0.0.1:1` which never listens
/// → connect_store_admin fails → UNAVAILABLE. Not UNIMPLEMENTED —
/// the RPC IS implemented, just the downstream dependency is down.
/// Client should retry.
#[tokio::test]
async fn test_trigger_gc_store_unreachable() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    let result = svc.trigger_gc(Request::new(GcRequest::default())).await;

    let status = result.expect_err("unreachable store should be Unavailable");
    assert_eq!(
        status.code(),
        tonic::Code::Unavailable,
        "store unreachable → UNAVAILABLE (not Unimplemented, not Internal)"
    );
    // Message should mention the store admin connect failure.
    assert!(
        status.message().contains("store admin") || status.message().contains("connect"),
        "error should mention store connect: {}",
        status.message()
    );
    Ok(())
}

#[tokio::test]
async fn drain_worker_force_reassigns() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_worker, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    // Two workers: w1 gets the first dispatch, then we force-drain it.
    // The reassigned drv should go to w2 on the next dispatch.
    let mut rx1 = connect_worker(&actor, "w1", "x86_64-linux", 2).await?;
    let mut rx2 = connect_worker(&actor, "w2", "x86_64-linux", 2).await?;

    let _ev =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;

    // ONE of them got it. With two equal-score workers (no bloom,
    // both 0/2 load), best_worker's tiebreak is HashMap iteration
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
        .drain_worker(Request::new(DrainWorkerRequest {
            worker_id: first_worker.into(),
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
    // isn't called from handle_drain_worker — it fires on the NEXT
    // heartbeat/merge/completion. Send a heartbeat to trigger it.
    actor
        .send_unchecked(ActorCommand::Heartbeat {
            worker_id: first_worker.into(),
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
    assert_eq!(status.draining_workers, 1);
    assert_eq!(status.active_workers, 1);
    assert_eq!(
        status.running_derivations, 1,
        "drv re-Assigned to other worker"
    );
    assert_eq!(status.queued_derivations, 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tenant RPCs: ListTenants, CreateTenant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_list_tenants() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

    // Initially empty.
    let resp = svc.list_tenants(Request::new(())).await?.into_inner();
    assert!(resp.tenants.is_empty());

    // Create a tenant.
    let created = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-alpha".into(),
            gc_retention_hours: Some(72),
            gc_max_store_bytes: Some(100 * 1024 * 1024 * 1024),
            cache_token: Some("secret-token".into()),
        }))
        .await?
        .into_inner();
    let t = created.tenant.expect("tenant should be set");
    assert_eq!(t.tenant_name, "team-alpha");
    assert_eq!(t.gc_retention_hours, 72);
    assert_eq!(t.gc_max_store_bytes, Some(100 * 1024 * 1024 * 1024));
    assert!(t.has_cache_token, "cache_token was set");
    assert!(!t.tenant_id.is_empty(), "UUID should be populated");

    // List shows it.
    let resp = svc.list_tenants(Request::new(())).await?.into_inner();
    assert_eq!(resp.tenants.len(), 1);
    assert_eq!(resp.tenants[0].tenant_name, "team-alpha");

    // Duplicate name → AlreadyExists.
    let dup = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-alpha".into(),
            ..Default::default()
        }))
        .await;
    assert_eq!(dup.unwrap_err().code(), tonic::Code::AlreadyExists);

    // Empty name → InvalidArgument.
    let empty = svc
        .create_tenant(Request::new(
            rio_proto::types::CreateTenantRequest::default(),
        ))
        .await;
    assert_eq!(empty.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Tenant with defaults (no optionals).
    let defaults = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-beta".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    let t = defaults.tenant.expect("tenant should be set");
    assert_eq!(t.gc_retention_hours, 168, "default 7 days");
    assert_eq!(t.gc_max_store_bytes, None);
    assert!(!t.has_cache_token);

    Ok(())
}
