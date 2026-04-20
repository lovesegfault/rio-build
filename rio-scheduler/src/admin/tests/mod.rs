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
mod spawn_intents_tests;
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
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        // service_verifier=None → dev-mode pass-through. Tests that
        // exercise the gate construct svc with Some(verifier) below.
        None,
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

/// `append_interrupt_sample` is idempotent on `event_uid`. The
/// controller's spot-interrupt watcher consumes `.applied_objects()`,
/// which re-yields every still-extant `SpotInterrupted` Event on
/// relist (controller restart, watch reconnect). Without dedup each
/// relist double-counts into λ's numerator (`refresh_lambda` SUMs
/// `kind='interrupt'` rows) → `solve_full` biases away from spot.
///
/// Exposure rows pass `event_uid=None` and are unconstrained — the
/// M_047 partial unique index only covers non-NULL uids.
#[tokio::test]
async fn append_interrupt_sample_idempotent_on_event_uid() {
    let (svc, _actor, _task, db) = setup_svc_default().await;

    let interrupt = |uid: Option<&str>| AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "interrupt".into(),
        value: 1.0,
        event_uid: uid.map(String::from),
    };

    // Same Event uid twice → one row.
    svc.append_interrupt_sample(Request::new(interrupt(Some("ev-abc"))))
        .await
        .unwrap();
    svc.append_interrupt_sample(Request::new(interrupt(Some("ev-abc"))))
        .await
        .unwrap();
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM interrupt_samples WHERE event_uid = 'ev-abc'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(n, 1, "same event_uid re-delivered → deduped to one row");

    // Distinct uid → second row.
    svc.append_interrupt_sample(Request::new(interrupt(Some("ev-def"))))
        .await
        .unwrap();
    // NULL uid (exposure path) twice → both inserted (partial index
    // only covers non-NULL).
    let exposure = AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "exposure".into(),
        value: 60.0,
        event_uid: None,
    };
    svc.append_interrupt_sample(Request::new(exposure.clone()))
        .await
        .unwrap();
    svc.append_interrupt_sample(Request::new(exposure))
        .await
        .unwrap();

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM interrupt_samples")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(total, 4, "ev-abc(1) + ev-def(1) + 2×NULL exposure = 4 rows");
}

/// Defense-in-depth input validation: lands regardless of the
/// service-token gate (dev-mode pass-through here).
#[tokio::test]
async fn append_interrupt_sample_rejects_invalid_inputs() {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let base = AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "interrupt".into(),
        value: 1.0,
        event_uid: None,
    };

    let bad_kind = AppendInterruptSampleRequest {
        kind: "garbage".into(),
        ..base.clone()
    };
    assert_eq!(
        svc.append_interrupt_sample(Request::new(bad_kind))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );

    let bad_value = AppendInterruptSampleRequest {
        value: f64::NAN,
        ..base.clone()
    };
    assert_eq!(
        svc.append_interrupt_sample(Request::new(bad_value))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );

    let neg_value = AppendInterruptSampleRequest {
        value: -1.0,
        ..base.clone()
    };
    assert_eq!(
        svc.append_interrupt_sample(Request::new(neg_value))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );

    let bad_hw = AppendInterruptSampleRequest {
        hw_class: "no-band".into(),
        ..base
    };
    assert_eq!(
        svc.append_interrupt_sample(Request::new(bad_hw))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
}

/// Construct svc with a real `service_verifier`. Other defaults match
/// [`setup_svc`]. Returns the signer so tests can mint valid tokens.
async fn setup_svc_with_service_verifier() -> (
    AdminServiceImpl,
    rio_auth::hmac::HmacSigner,
    tokio::task::JoinHandle<()>,
    TestDb,
) {
    let key = b"test-service-hmac-key-32-bytes!!".to_vec();
    let signer = rio_auth::hmac::HmacSigner::from_key(key.clone());
    let verifier = Arc::new(rio_auth::hmac::HmacVerifier::from_key(key));
    let db = TestDb::new(&crate::MIGRATOR).await;
    let (actor, task) = setup_actor(db.pool.clone());
    let svc = AdminServiceImpl::new(
        Arc::new(LogBuffers::new()),
        None,
        db.pool.clone(),
        actor,
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Some(verifier),
    );
    (svc, signer, task, db)
}

fn req_with_token<T>(signer: &rio_auth::hmac::HmacSigner, caller: &str, body: T) -> Request<T> {
    let claims = rio_auth::hmac::ServiceClaims {
        caller: caller.into(),
        expiry_unix: rio_auth::now_unix().unwrap() + 60,
    };
    let mut req = Request::new(body);
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        signer.sign(&claims).parse().unwrap(),
    );
    req
}

/// Every mutating AdminService RPC is service-token gated. Builders
/// share port 9001 with this service (CCNP allows scheduler:9001 at L4
/// only) and the JWT interceptor is permissive-on-absent-header — so
/// without `ensure_service_caller` a compromised builder reaches every
/// handler. Per the threat model "the worker is NOT trusted".
///
/// **Adding a new RPC:** add it to either this table (mutating) or the
/// read-only allowlist comment below — never neither. The table is the
/// structural lock that makes "forgot the gate on a new RPC" surface as
/// an obvious review diff (bug_013: 7 of 11 mutating RPCs were ungated
/// before this test became the canonical list).
///
/// Read-only RPCs (intentionally ungated — dashboard/grpc-web reach
/// them with no HMAC): `GetBuildLogs`, `ClusterStatus`, `ListExecutors`,
/// `ListBuilds`, `ListPoisoned`, `ListTenants`, `GetBuildGraph`,
/// `GetSpawnIntents`, `InspectBuildDag`, `DebugListExecutors`,
/// `ListSlaOverrides`, `SlaStatus`, `SlaExplain`, `ExportSlaCorpus`.
// r[verify sec.authz.service-token]
#[tokio::test]
async fn mutating_rpcs_require_service_token() {
    let (svc, _signer, _task, _db) = setup_svc_with_service_verifier().await;
    let denied = tonic::Code::PermissionDenied;

    macro_rules! assert_gated {
        ($name:literal, $call:expr) => {{
            let code = $call.await.unwrap_err().code();
            assert_eq!(code, denied, "{} must be service-token gated", $name);
        }};
    }

    assert_gated!(
        "DrainExecutor",
        svc.drain_executor(Request::new(DrainExecutorRequest {
            executor_id: "victim".into(),
            force: true,
        }))
    );
    assert_gated!(
        "ReportExecutorTermination",
        svc.report_executor_termination(Request::new(ReportExecutorTerminationRequest {
            executor_id: "victim".into(),
            reason: TerminationReason::OomKilled as i32,
        }))
    );
    assert_gated!(
        "AckSpawnedIntents",
        svc.ack_spawned_intents(Request::new(AckSpawnedIntentsRequest { spawned: vec![] }))
    );
    assert_gated!(
        "AppendInterruptSample",
        svc.append_interrupt_sample(Request::new(AppendInterruptSampleRequest {
            hw_class: "aws-8-nvme-hi".into(),
            kind: "interrupt".into(),
            value: 1.0,
            event_uid: None,
        }))
    );
    assert_gated!(
        "ClearPoison",
        svc.clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "h".into(),
        }))
    );
    assert_gated!(
        "CreateTenant",
        svc.create_tenant(Request::new(CreateTenantRequest::default()))
    );
    assert_gated!(
        "SetSlaOverride",
        svc.set_sla_override(Request::new(SetSlaOverrideRequest::default()))
    );
    assert_gated!(
        "ClearSlaOverride",
        svc.clear_sla_override(Request::new(ClearSlaOverrideRequest::default()))
    );
    assert_gated!(
        "ResetSlaModel",
        svc.reset_sla_model(Request::new(ResetSlaModelRequest::default()))
    );
    assert_gated!(
        "ImportSlaCorpus",
        svc.import_sla_corpus(Request::new(ImportSlaCorpusRequest::default()))
    );
    assert_gated!(
        "InjectBuildSample",
        svc.inject_build_sample(Request::new(InjectBuildSampleRequest::default()))
    );
    assert_gated!(
        "CancelBuild",
        svc.cancel_build(Request::new(CancelBuildRequest {
            build_id: uuid::Uuid::nil().to_string(),
            reason: "test".into(),
        }))
    );

    // TriggerGC is server-streaming with the grpc-web Trailers-Only
    // convention: error is yielded IN-STREAM, not as handler-level Err.
    let mut stream = svc
        .trigger_gc(Request::new(GcRequest::default()))
        .await
        .expect("server-streaming handler returns Ok(stream)")
        .into_inner();
    let status = stream
        .next()
        .await
        .expect("one item")
        .expect_err("first item is Err(PermissionDenied)");
    assert_eq!(
        status.code(),
        denied,
        "TriggerGC must be service-token gated"
    );
}

/// Positive path: valid token with allowlisted `caller` passes the
/// gate; wrong caller is rejected. One representative RPC per
/// allowlist shape — the tokenless-reject coverage above is
/// per-RPC.
#[tokio::test]
async fn service_token_allowlist_enforced() {
    let (svc, signer, _task, db) = setup_svc_with_service_verifier().await;

    // controller-only allowlist (`["rio-controller"]`).
    let r = AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "interrupt".into(),
        value: 1.0,
        event_uid: None,
    };
    svc.append_interrupt_sample(req_with_token(&signer, "rio-controller", r.clone()))
        .await
        .expect("rio-controller allowed");
    let err = svc
        .append_interrupt_sample(req_with_token(&signer, "rio-gateway", r))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("allowlist"));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM interrupt_samples")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "exactly one accepted insert");

    // controller+cli allowlist (`["rio-controller","rio-cli"]`).
    let drain = DrainExecutorRequest {
        executor_id: "victim".into(),
        force: true,
    };
    svc.drain_executor(req_with_token(&signer, "rio-controller", drain.clone()))
        .await
        .expect("rio-controller allowed");
    svc.drain_executor(req_with_token(&signer, "rio-cli", drain))
        .await
        .expect("rio-cli allowed");

    // cli-only allowlist (`["rio-cli"]`).
    let cp = ClearPoisonRequest {
        derivation_hash: "h".into(),
    };
    svc.clear_poison(req_with_token(&signer, "rio-cli", cp.clone()))
        .await
        .expect("rio-cli allowed");
    let err = svc
        .clear_poison(req_with_token(&signer, "rio-controller", cp))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
}

/// `AdminService.CancelBuild` is service-token gated. Builders share
/// port 9001; without this gate a compromised builder could cancel
/// arbitrary builds. rio-cli reaches the actor with `caller_tenant:
/// None` (operator override) — this is the path `rio-cli cancel-build`
/// uses; `SchedulerService.CancelBuild` (tenant-JWT gated) is
/// unreachable from the CLI.
// r[verify admin.rpc.cancel-build]
#[tokio::test]
async fn admin_cancel_build_gated_on_service_token() {
    let (svc, signer, _task, _db) = setup_svc_with_service_verifier().await;
    let r = CancelBuildRequest {
        build_id: Uuid::new_v4().to_string(),
        reason: "test".into(),
    };

    // No token → PermissionDenied (NOT Unauthenticated — that's the
    // tenant-JWT gate on SchedulerService).
    let err = svc.cancel_build(Request::new(r.clone())).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains(rio_proto::SERVICE_TOKEN_HEADER));

    // Non-allowlisted caller (a builder identity) → PermissionDenied.
    let err = svc
        .cancel_build(req_with_token(&signer, "rio-builder", r.clone()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("allowlist"));

    // rio-cli → reaches the actor. Unknown build_id → actor returns
    // BuildNotFound, which maps to NotFound — proves the request got
    // PAST the gate and into `ActorCommand::CancelBuild{caller_tenant:
    // None}`.
    let err = svc
        .cancel_build(req_with_token(&signer, "rio-cli", r.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "rio-cli token must reach actor (operator override path)"
    );

    // rio-controller also allowlisted (e.g. orphan-watcher sweep).
    let err = svc
        .cancel_build(req_with_token(&signer, "rio-controller", r))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
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
    // TriggerGC: now implemented (proxy to store). With the test
    // store_addr (127.0.0.1:1), proxy connect fails with Unavailable
    // (not Unimplemented) — proves it's wired. Error arrives IN-STREAM
    // (grpc-web Trailers-Only constraint).
    let mut gc_stream = svc
        .trigger_gc(Request::new(GcRequest::default()))
        .await?
        .into_inner();
    let gc_err = gc_stream.next().await.unwrap().unwrap_err();
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

/// Tick (publish cached snapshot) then call `cluster_status`. I-163:
/// `cluster_status` reads the watch-cached snapshot, which is Default
/// until the actor's first Tick. Tests drive Tick explicitly.
async fn cluster_status_now(
    svc: &AdminServiceImpl,
    actor: &crate::actor::ActorHandle,
) -> anyhow::Result<ClusterStatusResponse> {
    actor.send_unchecked(ActorCommand::Tick).await?;
    crate::actor::tests::barrier(actor).await;
    Ok(svc.cluster_status(Request::new(())).await?.into_inner())
}

#[tokio::test]
async fn cluster_status_empty() -> anyhow::Result<()> {
    let (svc, actor, _task, _db) = setup_svc_default().await;

    let resp = cluster_status_now(&svc, &actor).await?;

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
            stream_epoch: crate::actor::tests::next_stream_epoch_for("stream-only"),
            auth_intent: None,
            reply: crate::actor::tests::noop_connect_reply(),
        })
        .await?;

    // Fully registered worker (stream + heartbeat) → active.
    let _rx2 = connect_executor(&actor, "full", "x86_64-linux").await?;

    let resp = cluster_status_now(&svc, &actor).await?;

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

    // One-shot worker: will accept exactly one assignment, leaving
    // the second derivation in ready_queue.
    let mut worker_rx = connect_executor(&actor, "w1", "x86_64-linux").await?;

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

    let resp = cluster_status_now(&svc, &actor).await?;

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
        "second drv waiting for capacity (one-shot worker is busy)"
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
    assert!(!resp.busy);
    Ok(())
}

#[tokio::test]
async fn drain_worker_stops_dispatch() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_executor, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    let mut worker_rx = connect_executor(&actor, "w1", "x86_64-linux").await?;

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
    assert!(resp.busy, "the first drv is in-flight (Assigned)");

    // Second drv: should NOT dispatch. Worker is busy with first drv
    // AND draining → has_capacity() false.
    let _ev2 =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

    // Can't easily assert "nothing arrived" without a timeout. Instead,
    // check ClusterStatus: the second drv should be queued, not running.
    let status = cluster_status_now(&svc, &actor).await?;
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
    assert!(resp2.busy);
    Ok(())
}

#[tokio::test]
async fn drain_worker_force_reassigns() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_executor, merge_single_node};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Two workers: w1 gets the first dispatch, then we force-drain it.
    // The reassigned drv should go to w2 on the next dispatch.
    let mut rx1 = connect_executor(&actor, "w1", "x86_64-linux").await?;
    let mut rx2 = connect_executor(&actor, "w2", "x86_64-linux").await?;

    let _ev =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;

    // ONE of them got it. With two idle one-shot workers,
    // best_executor picks the first HashMap-iteration entry →
    // nondeterministic. Poll both with try_recv to find which.
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
    assert!(
        !resp.busy,
        "force=true reassigned → idle (caller doesn't wait)"
    );

    // reassign_derivations pushes to ready_queue but dispatch_ready
    // isn't called from handle_drain_executor — it fires on the NEXT
    // Tick/merge/completion. Heartbeat sets dispatch_dirty, Tick
    // drains it (I-163).
    crate::actor::tests::send_heartbeat(&actor, first_worker, "x86_64-linux").await?;

    // The OTHER worker should now get the reassigned drv.
    let msg = crate::actor::tests::recv_assignment(other_rx).await;
    let _ = msg; // recv_assignment already asserts variant + 2s timeout

    // ClusterStatus: 1 draining, 1 active, 1 running (on the other
    // worker now), 0 queued.
    let status = cluster_status_now(&svc, &actor).await?;
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
    let mut worker_rx = connect_executor(&actor, "poison-w", "x86_64-linux").await?;

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
    let mut worker_rx = connect_executor(&actor, "pg-blip-w", "x86_64-linux").await?;

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
