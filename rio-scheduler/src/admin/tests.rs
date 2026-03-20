use super::*;
use crate::actor::tests::setup_actor;
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_mocks::{RuleMode, mock, mock_client};
use rio_proto::types::BuildLogBatch;
use rio_test_support::{TestDb, seed_tenant};
use tokio::sync::oneshot;
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
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        rio_common::signal::Token::new(),
    );
    (svc, actor, task, db)
}

/// `setup_svc` with the common defaults (empty log buffers, no S3).
async fn setup_svc_default() -> (
    AdminServiceImpl,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    TestDb,
) {
    let buffers = Arc::new(LogBuffers::new());
    setup_svc(buffers, None).await
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
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        rio_common::signal::Token::new(),
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
    let (svc, _actor, _task, _db) = setup_svc_default().await;

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
async fn admin_rpcs_are_wired() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    // All phase4a RPCs are wired (no remaining stubs). This test proves
    // each returns a non-Unimplemented error or success — NOT full
    // behavior coverage (see dedicated tests below).

    // ListWorkers is implemented — no workers → empty list, not error.
    let lw = svc
        .list_workers(Request::new(ListWorkersRequest::default()))
        .await?
        .into_inner();
    assert!(lw.workers.is_empty(), "no workers registered → empty list");

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
    let (svc, _actor, _task, _db) = setup_svc_default().await;

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
    use crate::actor::tests::connect_worker;

    let (svc, actor, _task, _db) = setup_svc_default().await;

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

    let (svc, actor, _task, _db) = setup_svc_default().await;

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
// DrainWorker
// -----------------------------------------------------------------------

#[tokio::test]
async fn drain_worker_empty_id_invalid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

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
    let (svc, _actor, _task, _db) = setup_svc_default().await;

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

    let (svc, actor, _task, _db) = setup_svc_default().await;

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
    let (svc, _actor, _task, _db) = setup_svc_default().await;

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
    let (svc, _actor, _task, _db) = setup_svc_default().await;

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

    let (svc, actor, _task, _db) = setup_svc_default().await;

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
            store_degraded: false,
            resources: None,
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

// r[verify sched.admin.list-tenants]
// r[verify sched.admin.create-tenant]
#[tokio::test]
async fn test_create_and_list_tenants() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;

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
    assert!(
        t.created_at.is_some_and(|ts| ts.seconds > 0),
        "created_at should be populated (epoch seconds via EXTRACT)"
    );

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

    // Whitespace-only name → InvalidArgument (same as empty).
    let ws_name = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "   ".into(),
            ..Default::default()
        }))
        .await;
    assert_eq!(ws_name.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Interior whitespace ("team a") → InvalidArgument. Almost
    // certainly a misconfigured authorized_keys comment (space where a
    // dash was intended). If this stored successfully, no read path
    // would ever find it — they all look up the dashed form.
    let interior_ws = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team a".into(),
            ..Default::default()
        }))
        .await;
    let err = interior_ws.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("interior whitespace"),
        "error should name the InteriorWhitespace reason so the \
         operator knows it's a space-vs-dash typo, not an empty name: {}",
        err.message()
    );

    // Empty cache_token → InvalidArgument (round-3 fix; this test was
    // missing — the validation existed but was never exercised).
    let empty_tok = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-gamma".into(),
            cache_token: Some("".into()),
            ..Default::default()
        }))
        .await;
    assert_eq!(empty_tok.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Whitespace-only cache_token → InvalidArgument (round-4 fix;
    // same bypass class as empty-token, just with "   " instead of "").
    let ws_tok = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-delta".into(),
            cache_token: Some("   ".into()),
            ..Default::default()
        }))
        .await;
    assert_eq!(ws_tok.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Surrounding whitespace is TRIMMED before storage. Read paths
    // trim (gateway comment().trim(), cache auth str::trim), so an
    // untrimmed PG row makes WHERE tenant_name = 'team-trim' never
    // match — invisible-whitespace 'unknown tenant' bug.
    let trimmed = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "  team-trim  ".into(),
            cache_token: Some("  trim-secret  ".into()),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(
        trimmed.tenant.as_ref().unwrap().tenant_name,
        "team-trim",
        "stored tenant_name must be trimmed"
    );
    // cache_token isn't returned (has_cache_token bool only) — verify PG directly.
    let stored_tok: Option<String> =
        sqlx::query_scalar("SELECT cache_token FROM tenants WHERE tenant_name = 'team-trim'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(stored_tok.as_deref(), Some("trim-secret"));

    // gc_retention_hours > i32::MAX → InvalidArgument (was silently
    // wrapping to negative via `as i32` and storing in PG INTEGER).
    let oor = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-oor".into(),
            gc_retention_hours: Some(u32::MAX),
            ..Default::default()
        }))
        .await;
    let err = oor.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("out of range"));

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

// ---------------------------------------------------------------------------
// ListBuilds
// ---------------------------------------------------------------------------

// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_filter_and_pagination() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    // Seed 3 builds directly via db helper (bypasses the actor).
    use crate::state::{BuildOptions, PriorityClass};
    for i in 0..3 {
        sched_db
            .insert_build(
                uuid::Uuid::new_v4(),
                None,
                PriorityClass::Scheduled,
                false,
                &BuildOptions::default(),
            )
            .await?;
        // Small sleep so submitted_at ordering is deterministic.
        if i < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    // Transition one to succeeded.
    let builds: Vec<(uuid::Uuid,)> =
        sqlx::query_as("SELECT build_id FROM builds ORDER BY submitted_at LIMIT 1")
            .fetch_all(&db.pool)
            .await?;
    sqlx::query("UPDATE builds SET status = 'succeeded', finished_at = now() WHERE build_id = $1")
        .bind(builds[0].0)
        .execute(&db.pool)
        .await?;

    // No filter → 3 builds, total_count=3.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 3);
    assert_eq!(resp.total_count, 3);

    // filter=pending → 2.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            status_filter: "pending".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 2);
    assert_eq!(resp.total_count, 2);

    // Pagination: limit=1 offset=1 → 1 build (second-newest), total=3.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            limit: 1,
            offset: 1,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1);
    assert_eq!(resp.total_count, 3, "total_count unaffected by pagination");

    // Pagination: offset past end → empty page, total still correct.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            limit: 10,
            offset: 99,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert!(
        resp.builds.is_empty(),
        "offset >= total_count → empty page, got {} builds",
        resp.builds.len()
    );
    assert_eq!(
        resp.total_count, 3,
        "total_count unaffected by offset-past-end"
    );

    // tenant_filter: seed a tenant + one build tagged with it.
    let tenant_id = seed_tenant(&db.pool, "filter-test").await;
    sched_db
        .insert_build(
            uuid::Uuid::new_v4(),
            Some(tenant_id),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
        )
        .await?;
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "filter-test".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "only the tenant-tagged build");
    assert_eq!(resp.total_count, 1);

    // Unknown tenant → InvalidArgument.
    let err = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "nonexistent-tenant".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// Cross-tenant isolation: filtering by tenant A must NOT return tenant B's
/// builds. The test above (`test_list_builds_filter_and_pagination`) proves
/// "tenant filter excludes NULL-tenant builds" (1 tagged vs 3 untagged);
/// this test proves "tenant A filter excludes tenant B builds" — the actual
/// multi-tenant safety property.
// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_cross_tenant_isolation() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());
    use crate::state::{BuildOptions, PriorityClass};

    // Seed two tenants.
    let tenant_a = seed_tenant(&db.pool, "tenant-a").await;
    let tenant_b = seed_tenant(&db.pool, "tenant-b").await;

    // Seed one build per tenant. Capture build_id so we can assert
    // WHICH build appears (not just count — a buggy filter that
    // always returns the first row would pass a count-only check).
    let build_a = uuid::Uuid::new_v4();
    sched_db
        .insert_build(
            build_a,
            Some(tenant_a),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
        )
        .await?;
    let build_b = uuid::Uuid::new_v4();
    sched_db
        .insert_build(
            build_b,
            Some(tenant_b),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
        )
        .await?;

    // Filter by tenant A → exactly build_a, NOT build_b.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "tenant-a".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "tenant-a filter → exactly one build");
    assert_eq!(
        resp.builds[0].build_id,
        build_a.to_string(),
        "tenant-a filter must return build_a, not build_b"
    );
    assert_eq!(resp.total_count, 1);

    // Filter by tenant B → exactly build_b, NOT build_a.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "tenant-b".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "tenant-b filter → exactly one build");
    assert_eq!(
        resp.builds[0].build_id,
        build_b.to_string(),
        "tenant-b filter must return build_b, not build_a"
    );
    assert_eq!(resp.total_count, 1);

    // No filter → both builds visible.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 2, "no filter → both tenants' builds");
    assert_eq!(resp.total_count, 2);
    let ids: std::collections::HashSet<_> =
        resp.builds.iter().map(|b| b.build_id.as_str()).collect();
    assert!(ids.contains(build_a.to_string().as_str()));
    assert!(ids.contains(build_b.to_string().as_str()));

    Ok(())
}

// ---------------------------------------------------------------------------
// ListWorkers
// ---------------------------------------------------------------------------

// r[verify sched.admin.list-workers]
#[tokio::test]
async fn test_list_workers_with_filter() -> anyhow::Result<()> {
    use crate::actor::tests::connect_worker;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Fully registered worker.
    let _rx1 = connect_worker(&actor, "alive-worker", "x86_64-linux", 4).await?;

    // Drain a second worker.
    let _rx2 = connect_worker(&actor, "drain-worker", "aarch64-linux", 2).await?;
    let (tx, rx) = oneshot::channel();
    actor
        .send_unchecked(ActorCommand::DrainWorker {
            worker_id: "drain-worker".into(),
            force: false,
            reply: tx,
        })
        .await?;
    let _ = rx.await?;

    // No filter → both.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 2);

    // filter=alive → only alive-worker.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest {
            status_filter: "alive".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 1);
    let w = &resp.workers[0];
    assert_eq!(w.worker_id, "alive-worker");
    assert_eq!(w.status, "alive");
    assert_eq!(w.systems, vec!["x86_64-linux".to_string()]);
    assert_eq!(w.max_builds, 4);
    assert_eq!(w.running_builds, 0);
    assert!(w.connected_since.is_some());
    assert!(w.last_heartbeat.is_some());
    // size_class: connect_worker doesn't set it → empty string.
    // (Proves the field is wired — a worker heartbeating with
    // size_class="medium" would round-trip it here.)
    assert_eq!(w.size_class, "");

    // filter=draining → only drain-worker.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest {
            status_filter: "draining".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 1);
    assert_eq!(resp.workers[0].worker_id, "drain-worker");
    assert_eq!(resp.workers[0].status, "draining");

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
    use crate::actor::tests::{complete_failure, connect_worker, merge_single_node, test_drv_path};
    use crate::state::PriorityClass;

    let (svc, actor, _task, db) = setup_svc_default().await;
    let mut worker_rx = connect_worker(&actor, "poison-w", "x86_64-linux", 1).await?;

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
        rio_proto::types::BuildResultStatus::PermanentFailure,
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
    use crate::actor::tests::{complete_failure, connect_worker, merge_single_node, test_drv_path};
    use crate::state::PriorityClass;

    let (svc, actor, _task, db) = setup_svc_default().await;
    let mut worker_rx = connect_worker(&actor, "pg-blip-w", "x86_64-linux", 1).await?;

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
        rio_proto::types::BuildResultStatus::PermanentFailure,
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

/// TriggerGC forward task exits promptly on shutdown even when the
/// store stream is silent. Without the select! arm, store_stream.
/// message().await blocks indefinitely and the task outlives main().
#[tokio::test]
async fn trigger_gc_forward_exits_on_shutdown() {
    // Mock store_stream that never yields: a mpsc channel where we
    // hold the sender but never send. .next().await on the receiver-
    // backed stream blocks until sender drops — simulating the store
    // in its sweep phase (can take minutes on a large store).
    let (_store_tx, store_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(1);
    let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(8);
    let shutdown = rio_common::signal::Token::new();

    // Inline the forward-task body (matches admin/mod.rs post-fix).
    let task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut store_stream = tokio_stream::wrappers::ReceiverStream::new(store_rx);
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    m = store_stream.next() => m,
                };
                match msg {
                    Some(Ok(p)) => {
                        if client_tx.send(Ok(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = client_tx.send(Err(e)).await;
                        break;
                    }
                    None => break,
                }
            }
        })
    };

    // No messages sent — task is blocked on store_stream.next().
    // Without the shutdown arm this would hang the test.
    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("task should exit within 1s of shutdown")
        .expect("task should not panic");

    // Client stream sees EOF (forward task dropped client_tx).
    assert!(client_rx.recv().await.is_none());
}

// ---------------------------------------------------------------------------
// GetBuildGraph
// ---------------------------------------------------------------------------

/// Seed a derivation row directly. Returns `derivation_id`.
/// Raw INSERT — the graph query only needs 5 columns (drv_path, pname,
/// system, status, assigned_worker_id). Everything else defaults.
async fn seed_drv(
    pool: &sqlx::PgPool,
    drv_hash: &str,
    drv_path: &str,
    status: &str,
    worker: Option<&str>,
) -> anyhow::Result<uuid::Uuid> {
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status, assigned_worker_id)
         VALUES ($1, $2, 'pkg', 'x86_64-linux', $3, $4)
         RETURNING derivation_id",
    )
    .bind(drv_hash)
    .bind(drv_path)
    .bind(status)
    .bind(worker)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn seed_build(pool: &sqlx::PgPool) -> anyhow::Result<uuid::Uuid> {
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn link(pool: &sqlx::PgPool, build_id: uuid::Uuid, drv_id: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(drv_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn edge(pool: &sqlx::PgPool, parent: uuid::Uuid, child: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO derivation_edges (parent_id, child_id) VALUES ($1, $2)")
        .bind(parent)
        .bind(child)
        .execute(pool)
        .await?;
    Ok(())
}

/// 3 nodes, 2 edges, one build → correct shape, truncated=false.
/// Verifies: node count, edge count, status round-trip, assigned_worker_id
/// COALESCE, edge direction.
#[tokio::test]
async fn get_build_graph_basic_shape() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let pool = &db.pool;

    //   a
    //  / \     a depends on b and c (parent=a, child=b/c)
    // b   c
    let build = seed_build(pool).await?;
    let a = seed_drv(
        pool,
        "hash-a",
        "/nix/store/aaa-a.drv",
        "running",
        Some("w1"),
    )
    .await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "completed", None).await?;
    let c = seed_drv(pool, "hash-c", "/nix/store/ccc-c.drv", "queued", None).await?;
    link(pool, build, a).await?;
    link(pool, build, b).await?;
    link(pool, build, c).await?;
    edge(pool, a, b).await?;
    edge(pool, a, c).await?;

    let resp = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: build.to_string(),
        }))
        .await?
        .into_inner();

    assert_eq!(resp.nodes.len(), 3);
    assert_eq!(resp.edges.len(), 2);
    assert_eq!(resp.total_nodes, 3);
    assert!(!resp.truncated);

    // ORDER BY drv_path → a, b, c.
    assert_eq!(resp.nodes[0].drv_path, "/nix/store/aaa-a.drv");
    assert_eq!(resp.nodes[0].status, "running");
    assert_eq!(resp.nodes[0].assigned_worker_id, "w1");
    assert_eq!(resp.nodes[0].pname, "pkg");
    assert_eq!(resp.nodes[0].system, "x86_64-linux");

    assert_eq!(resp.nodes[1].drv_path, "/nix/store/bbb-b.drv");
    assert_eq!(resp.nodes[1].status, "completed");
    assert_eq!(
        resp.nodes[1].assigned_worker_id, "",
        "NULL assigned_worker_id → COALESCE to empty string"
    );

    // Both edges have parent=a. Find each.
    let to_b = resp
        .edges
        .iter()
        .find(|e| e.child_drv_path == "/nix/store/bbb-b.drv")
        .expect("edge a→b");
    assert_eq!(to_b.parent_drv_path, "/nix/store/aaa-a.drv");
    let to_c = resp
        .edges
        .iter()
        .find(|e| e.child_drv_path == "/nix/store/ccc-c.drv")
        .expect("edge a→c");
    assert_eq!(to_c.parent_drv_path, "/nix/store/aaa-a.drv");

    Ok(())
}

/// Two builds sharing a derivation → each sees only its subgraph.
/// The shared drv appears in both node sets, but edges to drvs owned
/// ONLY by the other build are excluded (both-endpoints-in-build rule).
#[tokio::test]
async fn get_build_graph_subgraph_scoping() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let pool = &db.pool;

    // build1: {a, shared}   edge a→shared
    // build2: {b, shared}   edge b→shared
    // Global DAG also has edge a→b (cross-build). Build1 should NOT see it.
    let build1 = seed_build(pool).await?;
    let build2 = seed_build(pool).await?;
    let a = seed_drv(pool, "hash-a", "/nix/store/aaa-a.drv", "queued", None).await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "queued", None).await?;
    let shared = seed_drv(
        pool,
        "hash-s",
        "/nix/store/sss-shared.drv",
        "completed",
        None,
    )
    .await?;

    link(pool, build1, a).await?;
    link(pool, build1, shared).await?;
    link(pool, build2, b).await?;
    link(pool, build2, shared).await?;

    edge(pool, a, shared).await?; // in build1's subgraph
    edge(pool, b, shared).await?; // in build2's subgraph
    edge(pool, a, b).await?; // CROSS-BUILD: a in build1, b in build2 only

    // Build1: sees {a, shared}, edge a→shared. NOT edge a→b (b ∉ build1).
    let r1 = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: build1.to_string(),
        }))
        .await?
        .into_inner();
    assert_eq!(r1.nodes.len(), 2, "build1: a + shared");
    assert_eq!(r1.edges.len(), 1, "build1: only a→shared (a→b excluded)");
    assert_eq!(r1.edges[0].parent_drv_path, "/nix/store/aaa-a.drv");
    assert_eq!(r1.edges[0].child_drv_path, "/nix/store/sss-shared.drv");
    // Shared drv appears in build1's nodes.
    assert!(
        r1.nodes
            .iter()
            .any(|n| n.drv_path == "/nix/store/sss-shared.drv")
    );

    // Build2: sees {b, shared}, edge b→shared. NOT edge a→b (a ∉ build2).
    let r2 = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: build2.to_string(),
        }))
        .await?
        .into_inner();
    assert_eq!(r2.nodes.len(), 2, "build2: b + shared");
    assert_eq!(r2.edges.len(), 1, "build2: only b→shared");
    assert_eq!(r2.edges[0].parent_drv_path, "/nix/store/bbb-b.drv");

    Ok(())
}

/// limit=2 on 3-node build → truncated=true, total_nodes=3, len(nodes)=2.
/// Uses graph::get_build_graph directly (limit_override) rather than
/// seeding 5001 rows to trip DASHBOARD_GRAPH_NODE_LIMIT.
#[tokio::test]
async fn get_build_graph_truncation() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let pool = &db.pool;

    let build = seed_build(pool).await?;
    let a = seed_drv(pool, "hash-a", "/nix/store/aaa-a.drv", "created", None).await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "created", None).await?;
    let c = seed_drv(pool, "hash-c", "/nix/store/ccc-c.drv", "created", None).await?;
    link(pool, build, a).await?;
    link(pool, build, b).await?;
    link(pool, build, c).await?;

    let sched_db = crate::db::SchedulerDb::new(pool.clone());
    let resp = super::graph::get_build_graph(&sched_db, &build.to_string(), Some(2)).await?;

    assert_eq!(resp.nodes.len(), 2, "limit=2 caps returned nodes");
    assert_eq!(resp.total_nodes, 3, "total_nodes reports un-limited count");
    assert!(resp.truncated, "total(3) > limit(2) → truncated");

    // Boundary: limit == total → NOT truncated.
    let resp = super::graph::get_build_graph(&sched_db, &build.to_string(), Some(3)).await?;
    assert_eq!(resp.nodes.len(), 3);
    assert_eq!(resp.total_nodes, 3);
    assert!(!resp.truncated, "total(3) == limit(3) → not truncated");

    Ok(())
}

// r[verify dash.graph.degrade-threshold]
/// Truncation WITH edges seeded: edges referencing truncated nodes
/// MUST NOT appear in the response (no dangling references).
///
/// The plain `get_build_graph_truncation` test above seeds zero edges,
/// so the dangling-edge case — edge query was both-endpoints-in-build
/// but NOT both-endpoints-in-returned-set — was invisible to the suite.
///
/// 3-node chain a→b→c, limit=2. ORDER BY drv_path returns {a, b}, drops c.
/// Edge a→b: both endpoints in returned set → kept.
/// Edge b→c: c NOT in returned set → MUST be filtered.
/// Expect exactly 1 edge. If the edge query still filters on build_id
/// (not returned-set), this test gets 2 edges and the dangling assertion
/// fires on b→c.
#[tokio::test]
async fn get_build_graph_truncated_no_dangling_edges() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let pool = &db.pool;

    let build = seed_build(pool).await?;
    // drv_paths chosen so ORDER BY drv_path is deterministic: a < b < c.
    let a = seed_drv(pool, "hash-a", "/nix/store/aaa-a.drv", "created", None).await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "created", None).await?;
    let c = seed_drv(pool, "hash-c", "/nix/store/ccc-c.drv", "created", None).await?;
    link(pool, build, a).await?;
    link(pool, build, b).await?;
    link(pool, build, c).await?;
    // Chain: a→b, b→c. 3 nodes, 2 edges.
    edge(pool, a, b).await?;
    edge(pool, b, c).await?;

    let sched_db = crate::db::SchedulerDb::new(pool.clone());
    let resp = super::graph::get_build_graph(&sched_db, &build.to_string(), Some(2)).await?;

    assert!(resp.truncated);
    assert_eq!(resp.total_nodes, 3);
    assert_eq!(resp.nodes.len(), 2, "limit=2 caps returned nodes");

    // Every edge endpoint is in the returned node set — no dangling.
    let returned: std::collections::HashSet<&str> =
        resp.nodes.iter().map(|n| n.drv_path.as_str()).collect();
    for e in &resp.edges {
        assert!(
            returned.contains(e.parent_drv_path.as_str()),
            "dangling edge: parent {} not in returned nodes {returned:?}",
            e.parent_drv_path
        );
        assert!(
            returned.contains(e.child_drv_path.as_str()),
            "dangling edge: child {} not in returned nodes {returned:?}",
            e.child_drv_path
        );
    }

    // Exact count proves the fix filters ONLY the dangling edge, no more.
    // Chain of N nodes has N-1 edges; truncate 1 node → exactly 1 edge
    // touches the excluded node → N-2 edges returned. Here: 3-1-1 = 1.
    // Under the old query this would be 2 (both a→b and b→c, since both
    // pairs are in the BUILD even though c isn't in the RESPONSE).
    assert_eq!(
        resp.edges.len(),
        1,
        "chain of 3, limit 2: edge b→c is dangling (c excluded), \
         edge a→b survives. If this is 2, the edge query is still \
         filtering on build_id not returned-node-set."
    );
    assert_eq!(resp.edges[0].parent_drv_path, "/nix/store/aaa-a.drv");
    assert_eq!(resp.edges[0].child_drv_path, "/nix/store/bbb-b.drv");

    // Boundary: limit == total → full chain returned, no filtering.
    let resp = super::graph::get_build_graph(&sched_db, &build.to_string(), Some(3)).await?;
    assert!(!resp.truncated);
    assert_eq!(
        resp.edges.len(),
        2,
        "no truncation → node_ids IS the full build set → \
         all in-build edges returned (same as old behavior)"
    );

    Ok(())
}

#[tokio::test]
async fn get_build_graph_bad_uuid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;
    let err = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: "not-a-uuid".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn get_build_graph_unknown_build_empty() -> anyhow::Result<()> {
    // Unknown build_id → empty graph, not NotFound. The query just
    // returns zero rows. Dashboard interprets empty-with-total=0 as
    // "build doesn't exist or has no derivations" — same rendering.
    let (svc, _actor, _task, _db) = setup_svc_default().await;
    let resp = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: uuid::Uuid::new_v4().to_string(),
        }))
        .await?
        .into_inner();
    assert!(resp.nodes.is_empty());
    assert!(resp.edges.is_empty());
    assert_eq!(resp.total_nodes, 0);
    assert!(!resp.truncated);
    Ok(())
}
