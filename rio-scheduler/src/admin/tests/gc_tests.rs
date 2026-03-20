//! `TriggerGC` RPC tests.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/gc.rs` submodule seam introduced by P0383.

use super::*;
use tokio_stream::StreamExt;

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
