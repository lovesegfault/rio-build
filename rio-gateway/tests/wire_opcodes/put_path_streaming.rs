// r[verify gw.put.aborted-retry]
//! sh-004: `grpc_put_path_streaming` (>16 MiB lane) wait-then-adopt on
//! `Aborted("concurrent PutPath in progress")`.
//!
//! The buffered lane (`grpc_put_path`) has retried I-068 placeholder
//! contention since I-068 itself; the streaming lane surfaced the same
//! `Aborted` as a hard `STDERR_ERROR` because the reader is consumed
//! and there is nothing to replay. Wait-then-adopt closes the gap
//! without replay: the pump already drains exactly `nar_size` bytes
//! regardless (the early-Ok wire-positioning contract), so on a
//! concurrent-PutPath `Aborted` the lane backs off, polls
//! `QueryPathInfo`, and adopts the concurrent uploader's result once
//! the path exists.

use super::*;

/// sh-004 red-first: a >16 MiB `wopAddMultipleToStore` entry whose
/// `PutPath` hits the store's I-068 placeholder-contention `Aborted`
/// once must succeed end-to-end (the concurrent uploader wins; the
/// path now exists). A small follow-up entry proves the framed reader
/// stayed positioned across the drained-then-adopted big entry.
///
/// RED at base: client receives `STDERR_ERROR` (`"store error: ...
/// Aborted ... concurrent PutPath in progress"`), `abort_next_puts`
/// stays at 1 (the streaming lane never reaches the mock's fault hook
/// before the rpc task completes — actually it does: the hook fires,
/// returns `Aborted`, the lane surfaces it terminally), and the
/// follow-up entry never reaches the store.
#[tokio::test]
async fn test_add_multiple_streaming_aborted_wait_then_adopt() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;

    let mut h = GatewaySession::new_with_handshake().await?;

    // Entry 1: >16 MiB → streaming lane. The mock's first PutPath
    // returns the I-068 placeholder-contention Aborted; the path is
    // seeded so the wait-then-adopt poll finds it on attempt 1.
    let big = vec![0x5Au8; 17 * 1024 * 1024];
    let (nar_big, hash_big) = make_nar(&big);
    let path_big = "/nix/store/77777777777777777777777777777777-streaming-aborted";
    h.store.seed_with_content(path_big, b"concurrent-winner");
    h.store.faults.abort_next_puts.store(1, Ordering::SeqCst);

    // Entry 2: small → buffered lane. Proves the framed reader was
    // left at entry 2's header after the big entry's pump drained
    // `nar_size` bytes on the Aborted path.
    let (nar_b, hash_b) = make_nar(b"after-adopt");
    let path_b = "/nix/store/88888888888888888888888888888888-after-adopt";

    let inner = wire_bytes![
        u64: 2,
        string: path_big,
        string: "",
        string: &hex::encode(hash_big),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar_big.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar_big,
        string: path_b,
        string: "",
        string: &hex::encode(hash_b),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar_b.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar_b,
    ];

    wire_send!(&mut h.stream;
        u64: 44,
        bool: false,
        bool: true,
        framed: &inner,
    );

    // RED at base: the streaming lane surfaces the Aborted as
    // STDERR_ERROR and the handler returns; `drain_stderr_until_last`
    // dies on STDERR_ERROR. After the fix: STDERR_LAST.
    drain_stderr_until_last(&mut h.stream).await?;

    assert_eq!(
        h.store.faults.abort_next_puts.load(Ordering::SeqCst),
        0,
        "the injected Aborted should have been consumed by the streaming lane"
    );
    let calls = h.store.calls.put_calls.read().unwrap().clone();
    let paths: Vec<&str> = calls.iter().map(|c| c.store_path.as_str()).collect();
    assert!(
        paths.contains(&path_b),
        "follow-up entry must reach the store — proves the framed reader \
         was positioned at entry 2's header after the drained-then-adopted \
         big entry; got {paths:?}"
    );
    assert!(
        !paths.contains(&path_big),
        "the big entry was adopted (concurrent uploader won), not uploaded; \
         a recorded put_call means the lane retried the upload instead of \
         polling for existence"
    );

    h.finish().await;
    Ok(())
}
