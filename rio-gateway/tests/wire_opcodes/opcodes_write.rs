// r[verify gw.opcode.add-to-store-nar+2]
// r[verify gw.opcode.add-to-store-nar.framing+2]
// r[verify gw.opcode.add-multiple.batch+2]
// r[verify gw.opcode.add-multiple.unaligned-frames]
// r[verify gw.opcode.add-multiple.dont-check-sigs-ignored]
// r[verify gw.wire.framed-no-padding]
// r[verify gw.stderr.error-format]

use super::*;

// ===========================================================================
// Opcode tests: AddToStoreNar (39)
// ===========================================================================

#[tokio::test]
async fn test_add_to_store_nar_accepts_valid() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let (nar, hash) = make_nar(b"add-to-store-nar");

    wire_send!(&mut h.stream;
        u64: 39,                           // wopAddToStoreNar
        string: TEST_PATH_A,               // path
        string: "",                        // deriver
        // narHash is hex-encoded SHA-256 (no algorithm prefix!)
        string: &hex::encode(hash),
        strings: wire::NO_STRINGS,         // references
        u64: 0,                            // registration_time
        u64: nar.len() as u64,             // nar_size
        bool: false,                       // ultimate
        strings: wire::NO_STRINGS,         // sigs
        string: "",                        // ca
        bool: false, bool: true,           // repair, dont_check_sigs
        // Framed NAR data: chunks of u64(len)+data, terminated by u64(0)
        framed: &nar,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    // AddToStoreNar has no result data after STDERR_LAST.

    // Verify the mock store received the PutPath call.
    let calls = h.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1, "store should receive one PutPath call");
    assert_eq!(calls[0].store_path, TEST_PATH_A);
    assert_eq!(calls[0].nar_hash, hash.to_vec());

    h.finish().await;
    Ok(())
}

/// Gateway passes client-declared narHash to the store unchanged.
/// Hash verification is the store's responsibility (validate.rs). This test
/// proves pass-through: send the RIGHT hash, assert it arrives unmodified.
///
/// (The mock now verifies the trailer hash matches sha256(nar) — mirroring
/// rio-store. A bogus declared hash would be rejected by both, so a
/// "pass bogus hash unchanged" test is testing against a mock that doesn't
/// match prod. See remediation 13 §8.)
#[tokio::test]
async fn test_add_to_store_nar_passes_declared_hash() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let (nar, actual_hash) = make_nar(b"trust-test");
    // Correct hash — prove the gateway is a dumb pipe (passes through unchanged).
    let declared_hash = actual_hash;

    wire_send!(&mut h.stream;
        u64: 39,
        string: TEST_PATH_A,
        string: "",
        string: &hex::encode(declared_hash),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        bool: false, bool: true,
        framed: &nar,
    );

    drain_stderr_until_last(&mut h.stream).await?;

    // Verify the declared hash was passed to the store unchanged.
    let calls = h.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].nar_hash,
        declared_hash.to_vec(),
        "gateway should pass client-declared hash unchanged"
    );

    h.finish().await;
    Ok(())
}

// ===========================================================================
// Opcode tests: AddMultipleToStore (44)
// ===========================================================================

#[tokio::test]
async fn test_add_multiple_to_store_batch() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let (nar_a, hash_a) = make_nar(b"multi-a");
    let (nar_b, hash_b) = make_nar(b"multi-b");

    // Build the inner framed payload per the REAL Nix protocol
    // (`Store::addMultipleToStore(Source &)` in store-api.cc):
    //   [num_paths: u64]
    //   for each: ValidPathInfo (9 fields) + NAR as narSize PLAIN bytes
    // The NAR is NOT nested-framed — `addToStore(info, source)` reads narSize
    // bytes directly from the already-framed outer stream.
    let test_path_b = "/nix/store/22222222222222222222222222222222-multi-b";
    let inner = wire_bytes![
        u64: 2,                            // Count prefix
        // Entry 1
        string: TEST_PATH_A,
        string: "",                        // deriver
        string: &hex::encode(hash_a),
        strings: wire::NO_STRINGS,         // refs
        u64: 0,                            // regtime
        u64: nar_a.len() as u64,           // nar_size
        bool: false,                       // ultimate
        strings: wire::NO_STRINGS,         // sigs
        string: "",                        // ca
        // NAR: narSize plain bytes (NOT framed)
        raw: &nar_a,
        // Entry 2
        string: test_path_b,
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

    // Send opcode + outer framing
    wire_send!(&mut h.stream;
        u64: 44,                           // wopAddMultipleToStore
        bool: false,                       // repair
        bool: true,                        // dont_check_sigs
        framed: &inner,
    );

    drain_stderr_until_last(&mut h.stream).await?;

    let calls = h.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 2, "store should receive 2 PutPath calls");
    let paths: Vec<&str> = calls.iter().map(|c| c.store_path.as_str()).collect();
    assert!(paths.contains(&TEST_PATH_A));
    assert!(paths.contains(&test_path_b));

    h.finish().await;
    Ok(())
}

/// I-068: store returns `Aborted` when another upload holds the
/// placeholder for this path. Gateway must retry instead of surfacing
/// it as a hard wopAddMultipleToStore failure.
///
/// Arms abort_next_puts=2 so the entry's first two attempts hit
/// Aborted; the third succeeds. Asserts: STDERR_LAST (success), the
/// entry landed in put_calls, and the retry budget was actually
/// consumed (proves the gateway looped, not that the mock skipped the
/// hook).
#[tokio::test]
async fn test_add_multiple_retries_store_aborted() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.store
        .faults
        .abort_next_puts
        .store(2, std::sync::atomic::Ordering::SeqCst);

    let (nar_a, hash_a) = make_nar(b"aborted-retry");
    let inner = wire_bytes![
        u64: 1,
        string: TEST_PATH_A,
        string: "",
        string: &hex::encode(hash_a),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar_a.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar_a,
    ];
    wire_send!(&mut h.stream;
        u64: 44,
        bool: false,
        bool: true,
        framed: &inner,
    );

    drain_stderr_until_last(&mut h.stream).await?;

    let calls = h.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(
        calls.len(),
        1,
        "the successful retry should record exactly one PutPath"
    );
    assert_eq!(calls[0].store_path, TEST_PATH_A);
    assert_eq!(
        h.store
            .faults
            .abort_next_puts
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "both injected aborts should have been consumed by retries"
    );

    h.finish().await;
    Ok(())
}

/// I-068 negative: exhausting the retry budget surfaces the Aborted
/// to the client as STDERR_ERROR (proves the loop is bounded and the
/// final error is propagated, not swallowed).
#[tokio::test]
async fn test_add_multiple_aborted_exhausts_budget() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    // Well over PUT_PATH_ABORTED_MAX_ATTEMPTS — every attempt aborts.
    h.store
        .faults
        .abort_next_puts
        .store(100, std::sync::atomic::Ordering::SeqCst);

    let (nar_a, hash_a) = make_nar(b"aborted-exhaust");
    let inner = wire_bytes![
        u64: 1,
        string: TEST_PATH_A,
        string: "",
        string: &hex::encode(hash_a),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar_a.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar_a,
    ];
    wire_send!(&mut h.stream;
        u64: 44,
        bool: false,
        bool: true,
        framed: &inner,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("concurrent PutPath") || err.message.contains("Aborted"),
        "client should see the store's Aborted message, got: {err:?}"
    );
    assert!(
        h.store.calls.put_calls.read().unwrap().is_empty(),
        "no attempt should have reached the store's success path"
    );

    h.finish().await;
    Ok(())
}

/// Regression: handler must reject truncated NAR (nar_size claims more bytes
/// than remain in the framed stream) instead of panicking on slice OOB.
///
/// With streaming, the handler sends metadata to the store before the
/// short read is detected — the gateway's read_exact hits EOF on the
/// framed stream (sentinel consumed, 100 bytes short). The assertion
/// here is that the client gets STDERR_ERROR from the GATEWAY, not
/// from the store (the gateway's error wins the race).
#[tokio::test]
async fn test_add_multiple_to_store_truncated_nar() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let (nar, hash) = make_nar(b"truncated");

    let inner = wire_bytes![
        u64: 1,                            // num_paths
        string: TEST_PATH_A,
        string: "",
        string: &hex::encode(hash),
        strings: wire::NO_STRINGS,
        u64: 0,
        // LIE about nar_size: claim more bytes than we actually send.
        u64: nar.len() as u64 + 100,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar,                         // actual NAR, 100 bytes short of claimed size
    ];

    wire_send!(&mut h.stream;
        u64: 44,
        bool: false,
        bool: true,
        framed: &inner,
    );

    // Handler should send STDERR_ERROR (not crash). The streaming read_exact
    // hits EOF on the framed stream (sentinel consumed, but 100 bytes short).
    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("NAR read") || err.message.contains("eof"),
        "expected NAR read/eof error, got: {}",
        err.message
    );
    Ok(())
}

/// Mixed batch: one `.drv` entry (buffered, cached) + one non-`.drv` entry
/// (streamed). Both must reach the store. Verifies the `.drv`-branch in
/// `stream_one_entry` doesn't break batch processing.
#[tokio::test]
async fn test_add_multiple_to_store_mixed_drv_and_streaming() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let (nar_a, hash_a) = make_nar(b"regular-path-streamed");
    let (nar_drv, hash_drv) = make_nar(b"Derive([],[],[],\"x86_64-linux\",\"/bin/sh\",[],[])");

    let test_path_drv = "/nix/store/33333333333333333333333333333333-test.drv";
    let inner = wire_bytes![
        u64: 2,
        // Entry 1: .drv (buffered path)
        string: test_path_drv,
        string: "",
        string: &hex::encode(hash_drv),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar_drv.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar_drv,
        // Entry 2: non-.drv (streaming path)
        string: TEST_PATH_A,
        string: "",
        string: &hex::encode(hash_a),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar_a.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar_a,
    ];

    wire_send!(&mut h.stream;
        u64: 44,
        bool: false,
        bool: true,
        framed: &inner,
    );

    drain_stderr_until_last(&mut h.stream).await?;

    let calls = h.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 2, "both entries should reach the store");
    let paths: Vec<&str> = calls.iter().map(|c| c.store_path.as_str()).collect();
    assert!(
        paths.contains(&test_path_drv),
        ".drv entry should be uploaded"
    );
    assert!(
        paths.contains(&TEST_PATH_A),
        "streamed entry should be uploaded"
    );
    // Both should have their declared hashes in the trailer.
    for call in &calls {
        assert_eq!(call.nar_hash.len(), 32, "nar_hash should be 32 bytes");
    }

    h.finish().await;
    Ok(())
}

/// Trailing data after num_paths entries: protocol error. The client sent
/// more bytes than num_paths claimed. Handler must detect this at the
/// drain-to-sentinel step and send STDERR_ERROR.
#[tokio::test]
async fn test_add_multiple_to_store_trailing_data() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let (nar, hash) = make_nar(b"trailing");

    let inner = wire_bytes![
        u64: 1,                            // num_paths = 1
        string: TEST_PATH_A,
        string: "",
        string: &hex::encode(hash),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        raw: &nar,
        // Extra bytes past the declared entry count
        raw: b"unexpected trailing garbage",
    ];

    wire_send!(&mut h.stream;
        u64: 44,
        bool: false,
        bool: true,
        framed: &inner,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("trailing data"),
        "expected 'trailing data' error, got: {}",
        err.message
    );

    // The one valid entry SHOULD have been uploaded before the trailing-data
    // check (streaming processes entries as they arrive).
    assert_eq!(h.store.calls.put_calls.read().unwrap().len(), 1);
    Ok(())
}

// ===========================================================================
// AddSignatures (37), RegisterDrvOutput (42), QueryRealisation (43)
// ===========================================================================

/// AddSignatures on a seeded path: appends to MockStore and returns success.
#[tokio::test]
async fn test_add_signatures_appends_and_returns_success() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.store.seed_with_content(TEST_PATH_A, b"addsig test");

    wire_send!(&mut h.stream;
        u64: 37,                           // wopAddSignatures
        string: TEST_PATH_A,
        strings: &["cache.test:SIG_A", "cache.test:SIG_B"],
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "AddSignatures should return 1 (success)");

    // Verify MockStore actually recorded the sigs (not just the wire parse).
    let stored_sigs = h
        .store
        .state
        .paths
        .read()
        .unwrap()
        .get(TEST_PATH_A)
        .map(|(info, _)| info.signatures.clone())
        .unwrap_or_default();
    assert_eq!(
        stored_sigs,
        vec!["cache.test:SIG_A", "cache.test:SIG_B"],
        "sigs should reach MockStore"
    );

    h.finish().await;
    Ok(())
}

/// AddSignatures on unknown path: STDERR_ERROR (not silent success).
/// Unknown paths fail (STDERR_ERROR), matching the real daemon's behavior.
#[tokio::test]
async fn test_add_signatures_unknown_path_errors() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    // TEST_PATH_A NOT seeded — MockStore returns NOT_FOUND.

    wire_send!(&mut h.stream;
        u64: 37,                           // wopAddSignatures
        string: TEST_PATH_A,
        strings: &["sig:fake"],
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("not found") || err.message.contains("NotFound"),
        "error should mention not found, got: {}",
        err.message
    );

    h.finish().await;
    Ok(())
}

// r[verify store.realisation.register+2]
/// RegisterDrvOutput is rejected with STDERR_ERROR (CppNix-parity:
/// gated on trusted-user; rio has no trusted-user). The payload is
/// drained so the stream stays in sync. MockStore is NOT touched —
/// realisations are scheduler-written, never via this opcode.
#[tokio::test]
async fn test_register_drv_output_rejected() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    let drv_hash_hex = "aa".repeat(32);
    let realisation_json = format!(
        r#"{{"id":"sha256:{drv_hash_hex}!out","outPath":"00000000000000000000000000000000-ca-test-out","signatures":["sig:test"],"dependentRealisations":{{}}}}"#
    );

    wire_send!(&mut h.stream;
        u64: 42,                           // wopRegisterDrvOutput
        string: &realisation_json,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("trusted-user"),
        "expected trusted-user message, got: {}",
        err.message
    );

    // MockStore NOT touched — opcode never reaches gRPC.
    assert!(
        h.store.state.realisations.read().unwrap().is_empty(),
        "rejected opcode must not write to store"
    );

    Ok(())
}

/// Payload is drained even when rejected — malformed JSON doesn't matter
/// (we never parse it), but the wire string MUST be read or the next
/// opcode would desync.
#[tokio::test]
async fn test_register_drv_output_drains_payload() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream;
        u64: 42,
        string: "not valid json {",
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(err.message.contains("trusted-user"));
    assert!(h.store.state.realisations.read().unwrap().is_empty());

    Ok(())
}

#[tokio::test]
async fn test_add_text_to_store() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream;
        u64: 8,                            // wopAddTextToStore
        string: "my-text",                 // name
        string: "hello world",             // text
        strings: wire::NO_STRINGS,         // references
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let path = wire::read_string(&mut h.stream).await?;
    assert!(
        path.starts_with("/nix/store/") && path.ends_with("-my-text"),
        "computed path should be a store path ending in -my-text: {path}"
    );

    // Verify store received the upload.
    let calls = h.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].store_path, path);

    h.finish().await;
    Ok(())
}

/// wopAddToStore (7) with cam_str="text:sha256": text-method CA path.
/// Reads name + cam_str + references + repair + framed dump, returns
/// a 9-field ValidPathInfo. The computed path should match
/// StorePath::make_text (content-addressed text import).
#[tokio::test]
async fn test_add_to_store_text_method() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    let content = b"hello from text method";

    wire_send!(&mut h.stream;
        u64: 7,                            // wopAddToStore
        string: "text-test",               // name
        string: "text:sha256",             // cam_str
        strings: wire::NO_STRINGS,         // references
        bool: false,                       // repair
        framed: content,                   // dump data
    );

    drain_stderr_until_last(&mut h.stream).await?;

    // 9-field ValidPathInfo response: path + 8-field PathInfoWire
    let path = wire::read_string(&mut h.stream).await?;
    let info = read_path_info(&mut h.stream).await?;

    assert!(
        path.starts_with("/nix/store/") && path.ends_with("-text-test"),
        "computed path should be a store path ending in -text-test: {path}"
    );
    assert_eq!(info.nar_hash.len(), 64, "nar_hash should be hex sha256");
    assert!(info.nar_size > 0);
    assert!(
        info.ca.starts_with("text:sha256:"),
        "ca should start with text:sha256:, got: {}",
        info.ca
    );

    // Verify store received the upload at the computed path.
    let calls = h.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].store_path, path);

    h.finish().await;
    Ok(())
}

/// wopAddToStore (7) with cam_str="fixed:sha256": flat-file fixed-output.
/// The dump data is raw file content (not NAR); handler wraps it in NAR.
#[tokio::test]
async fn test_add_to_store_fixed_flat() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    let content = b"raw file content for flat fixed-output";

    wire_send!(&mut h.stream;
        u64: 7,                            // wopAddToStore
        string: "flat-test",               // name
        string: "fixed:sha256",            // cam_str (flat, no r:)
        strings: wire::NO_STRINGS,         // references
        bool: false,                       // repair
        framed: content,                   // dump data (raw, not NAR)
    );

    drain_stderr_until_last(&mut h.stream).await?;

    let path = wire::read_string(&mut h.stream).await?;
    let info = read_path_info(&mut h.stream).await?;

    assert!(path.ends_with("-flat-test"));
    // Flat: NAR wraps the raw content, so nar_size > content.len()
    assert!(
        info.nar_size > content.len() as u64,
        "NAR wrapping should increase size: nar_size={}, content={}",
        info.nar_size,
        content.len()
    );
    assert!(
        info.ca.starts_with("fixed:sha256:"),
        "flat ca should be fixed:sha256: (no r:), got: {}",
        info.ca
    );

    h.finish().await;
    Ok(())
}

/// wopAddToStore (7) with an invalid cam_str should send STDERR_ERROR
/// (not crash or silently succeed).
#[tokio::test]
async fn test_add_to_store_invalid_cam_str_returns_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream;
        u64: 7,                            // wopAddToStore
        string: "bad-test",                // name
        string: "bogus:sha256",            // INVALID cam_str
        strings: wire::NO_STRINGS,         // references
        bool: false,                       // repair
        // Handler reads framed stream BEFORE parsing cam_str, so we must send it.
        framed: b"data",
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("content-address method") || err.message.contains("bogus"),
        "error should mention invalid cam_str: {}",
        err.message
    );

    h.finish().await;
    Ok(())
}

/// AddToStoreNar with invalid store path should send STDERR_ERROR.
#[tokio::test]
async fn test_add_to_store_nar_invalid_path_returns_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream;
        u64: 39,
        string: "not-a-valid-store-path",  // path — INVALID
        string: "",                        // deriver
        string: &hex::encode([0u8; 32]),   // narHash
        strings: wire::NO_STRINGS,         // references
        u64: 0,                            // reg_time
        u64: 100,                          // nar_size
        bool: false,                       // ultimate
        strings: wire::NO_STRINGS,         // sigs
        string: "",                        // ca
        bool: false, bool: true,           // repair, dontCheckSigs
        // No framed data — handler should error before reading it.
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("invalid store path"),
        "error should mention invalid path: {}",
        err.message
    );

    h.finish().await;
    Ok(())
}

/// AddToStoreNar with nar_size > MAX_NAR_SIZE should send STDERR_ERROR
/// before reading any NAR data (prevents DoS). MAX_FRAMED_TOTAL ==
/// MAX_NAR_SIZE (enforced by const assert in opcodes_write.rs), so
/// this gate is the effective limit — the framed reader clamp never
/// fires for a size the handler accepted.
// r[verify gw.wire.framed-max-total+3]
#[tokio::test]
async fn test_add_to_store_nar_oversized_returns_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream;
        u64: 39,
        string: TEST_PATH_A,
        string: "",                        // deriver
        string: &hex::encode([0u8; 32]),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: u64::MAX,                     // nar_size HUGE
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        bool: false, bool: true,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("exceeds maximum"),
        "error should mention size limit: {}",
        err.message
    );

    h.finish().await;
    Ok(())
}

/// Early store rejection of a streaming PutPath surfaces the store's
/// `Status` to the client, not the generic "channel closed mid-stream".
///
/// Mechanics: non-.drv path → `grpc_put_path_streaming`. NAR is 6 ×
/// `NAR_CHUNK_SIZE` so the pump's `tx.send` blocks on the 4-slot mpsc
/// buffer (metadata + 3 chunks fill it), yields, the spawned rpc task
/// runs, `fail_next_puts` fires before the mock reads anything → rx
/// dropped → pump's blocked send fails. Before fix that became
/// `Err(GrpcStream("channel closed mid-stream"))` and `pump_result?`
/// returned it, discarding the real `Status` in `rpc_result`.
///
/// The 1.5 MiB NAR doesn't fit in the 256 KiB DuplexStream buffer, so the
/// framed write runs in a spawned task on the split write-half while the
/// main task reads stderr from the read-half.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_add_to_store_nar_streaming_early_reject_surfaces_store_status() -> anyhow::Result<()>
{
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt;

    let mut h = GatewaySession::new_with_handshake().await?;
    h.store.faults.fail_next_puts.store(1, Ordering::SeqCst);

    // 6 × 256 KiB > 4-slot × 256 KiB mpsc buffer → pump blocks → yields.
    let (nar, hash) = make_nar(&vec![0xab; 6 * 256 * 1024]);

    // Split: write framed NAR concurrently while main reads stderr.
    // After stderr_err! the handler returns and the server-side duplex
    // half is dropped, so the writer task may see BrokenPipe — ignore.
    let stream = std::mem::replace(&mut h.stream, tokio::io::duplex(1).0);
    let (mut rd, mut wr) = tokio::io::split(stream);

    wire_send!(&mut wr;
        u64: 39,
        string: TEST_PATH_A,
        string: "",
        string: &hex::encode(hash),
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        bool: false, bool: true,
    );
    let writer = tokio::spawn(async move {
        let _ = wire::write_framed_stream(&mut wr, &nar, 8192).await;
        let _ = wr.flush().await;
    });

    // `drain_stderr_expecting_error` is `DuplexStream`-only; inline the
    // loop on `ReadHalf` (read_stderr_message is generic over AsyncRead).
    let err = loop {
        match rio_nix::protocol::client::read_stderr_message(&mut rd).await? {
            rio_nix::protocol::client::StderrMessage::Error(e) => break e,
            rio_nix::protocol::client::StderrMessage::Last => {
                panic!("expected STDERR_ERROR but got STDERR_LAST")
            }
            _ => {}
        }
    };
    assert!(
        err.message.contains("injected put failure"),
        "early store reject must surface the store's Status, got: {}",
        err.message
    );
    assert!(
        !err.message.contains("channel closed"),
        "must NOT surface the generic channel-closed pump error, got: {}",
        err.message
    );

    let _ = writer.await;
    Ok(())
}
