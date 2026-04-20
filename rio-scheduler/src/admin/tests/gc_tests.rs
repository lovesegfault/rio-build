//! `TriggerGC` RPC tests.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/gc.rs` submodule seam introduced by P0383.

use super::*;
use crate::admin::gc::forward_gc_progress;

/// Unwrap an `Ok(Response)` whose `TriggerGCStream` yields exactly one
/// `Err(Status)`. Mirror of [`expect_stream_err`] for `GcProgress`.
async fn expect_gc_stream_err(
    result: Result<Response<ReceiverStream<Result<GcProgress, Status>>>, Status>,
) -> Status {
    result
        .expect("handler should return Ok(stream), error is in-stream")
        .into_inner()
        .next()
        .await
        .expect("stream should yield one item")
        .expect_err("stream item should be Err(Status)")
}

/// TriggerGC with store unreachable → in-stream `Err(Unavailable)`.
/// The store_addr in setup_svc is `127.0.0.1:1` which never listens
/// → store-admin connect fails → UNAVAILABLE. Handler now returns
/// `Ok(stream-yielding-Err)` (grpc-web Trailers-Only constraint —
/// see `logs::err_stream`), not `Err(Status)`.
#[tokio::test]
async fn test_trigger_gc_store_unreachable() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let result = svc.trigger_gc(Request::new(GcRequest::default())).await;

    let status = expect_gc_stream_err(result).await;
    assert_eq!(
        status.code(),
        tonic::Code::Unavailable,
        "store unreachable → UNAVAILABLE (not Unimplemented, not Internal)"
    );
    assert!(
        status.message().contains("store admin") || status.message().contains("connect"),
        "error should mention store connect: {}",
        status.message()
    );
    Ok(())
}

/// Regression: standby (`is_leader=false`) returning `Err(Status)`
/// → tonic Trailers-Only → grpc-web dashboard sees silent 200. Now
/// the leader guard is wrapped in `err_stream` so the status arrives
/// as an in-stream `Err`.
#[tokio::test]
async fn test_trigger_gc_standby_yields_err_in_stream() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;
    svc.force_standby();

    let result = svc.trigger_gc(Request::new(GcRequest::default())).await;

    let status = expect_gc_stream_err(result).await;
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert!(
        status.message().contains("standby") || status.message().contains("leader"),
        "error should mention leader/standby: {}",
        status.message()
    );
    Ok(())
}

/// `forward_gc_progress` exits promptly on shutdown even when the store
/// stream is silent. Calls the PRODUCTION fn directly — previously the
/// test inlined a hand-copy of the loop body and a refactor that dropped
/// the `shutdown.cancelled()` arm passed it. Now structurally coupled:
/// deleting that arm fails this test.
#[tokio::test]
async fn trigger_gc_forward_exits_on_shutdown() {
    // Mock store stream that never yields: hold the sender, never send.
    // .next().await blocks until sender drops — simulating the store in
    // its sweep phase (can take minutes on a large store).
    let (_store_tx, store_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(1);
    let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(8);
    let shutdown = rio_common::signal::Token::new();

    let task = tokio::spawn(forward_gc_progress(
        tokio_stream::wrappers::ReceiverStream::new(store_rx),
        client_tx,
        shutdown.clone(),
    ));

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

/// `forward_gc_progress` forwards items and propagates upstream errors.
/// Proves the generic-stream extraction preserves the happy-path shape.
#[tokio::test]
async fn trigger_gc_forward_propagates_items_and_errors() {
    let (store_tx, store_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(4);
    let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(8);
    let shutdown = rio_common::signal::Token::new();

    let task = tokio::spawn(forward_gc_progress(
        tokio_stream::wrappers::ReceiverStream::new(store_rx),
        client_tx,
        shutdown,
    ));

    store_tx.send(Ok(GcProgress::default())).await.unwrap();
    store_tx
        .send(Err(Status::resource_exhausted("mid-sweep")))
        .await
        .unwrap();

    assert!(client_rx.recv().await.unwrap().is_ok());
    let err = client_rx.recv().await.unwrap().unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    // Error → break → task exits → client sees EOF.
    assert!(client_rx.recv().await.is_none());
    task.await.unwrap();
}
