// r[verify gw.opcode.add-to-store-nar]
// r[verify gw.opcode.add-to-store-nar.framing]
// r[verify gw.opcode.add-multiple.batch]
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
    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1, "store should receive one PutPath call");
    assert_eq!(calls[0].store_path, TEST_PATH_A);
    assert_eq!(calls[0].nar_hash, hash.to_vec());

    h.finish().await;
    Ok(())
}

/// Gateway trusts client-declared narHash and passes it to the store.
/// Hash verification is the store's responsibility (validate.rs). This test
/// verifies the gateway passes the declared hash through unchanged.
#[tokio::test]
async fn test_add_to_store_nar_passes_declared_hash() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let (nar, _actual_hash) = make_nar(b"trust-test");
    let declared_hash = [0xABu8; 32]; // deliberately different from actual

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

    // Verify the DECLARED hash (not actual) was passed to the store.
    let calls = h.store.put_calls.read().unwrap().clone();
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
    //
    // Previous test wrote NO count prefix + inner-framed NAR, matching a buggy
    // parser rather than the spec. Fixed after VM test caught it running real
    // `nix copy --to ssh-ng://`.
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

    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 2, "store should receive 2 PutPath calls");
    let paths: Vec<&str> = calls.iter().map(|c| c.store_path.as_str()).collect();
    assert!(paths.contains(&TEST_PATH_A));
    assert!(paths.contains(&test_path_b));

    h.finish().await;
    Ok(())
}

/// Regression: handler must reject truncated NAR (nar_size claims more bytes
/// than remain in the framed stream) instead of panicking on slice OOB.
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

    // Handler should send STDERR_ERROR (not crash).
    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("truncated"),
        "expected 'truncated' in error, got: {}",
        err.message
    );

    // No PutPath calls should have been made.
    assert_eq!(h.store.put_calls.read().unwrap().len(), 0);
    Ok(())
}

// ===========================================================================
// Stub opcode tests: AddSignatures (37), RegisterDrvOutput (42),
// QueryRealisation (43)
// ===========================================================================

/// AddSignatures on a seeded path: appends to MockStore and returns success.
/// No longer a stub (phase2c E2) — actually calls the store RPC now.
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

    // Verify MockStore actually recorded the sigs (the old stub discarded
    // them silently — this assertion would have caught that).
    let stored_sigs = h
        .store
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
/// The old stub accepted everything; now unknown paths fail, matching
/// the real daemon's behavior.
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

/// RegisterDrvOutput with valid Realisation JSON: parses, calls store,
/// MockStore records it. No longer a stub (phase2c E4).
#[tokio::test]
async fn test_register_drv_output_stores_realisation() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    // 64-char hex = 32-byte SHA-256. All-AA for test determinism.
    let drv_hash_hex = "aa".repeat(32);
    let out_path = "/nix/store/00000000000000000000000000000000-ca-test-out";
    let realisation_json = format!(
        r#"{{"id":"sha256:{drv_hash_hex}!out","outPath":"{out_path}","signatures":["sig:test"],"dependentRealisations":{{}}}}"#
    );

    wire_send!(&mut h.stream;
        u64: 42,                           // wopRegisterDrvOutput
        string: &realisation_json,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    // No result data after STDERR_LAST.

    // Verify MockStore recorded it. The old stub discarded silently — this
    // assertion would have caught that.
    let drv_hash = hex::decode(&drv_hash_hex)?;
    let key = (drv_hash, "out".to_string());
    let stored = h.store.realisations.read().unwrap().get(&key).cloned();
    let stored = stored.expect("MockStore should have the realisation");
    assert_eq!(stored.output_path, out_path);
    assert_eq!(stored.signatures, vec!["sig:test"]);

    h.finish().await;
    Ok(())
}

/// Malformed JSON: soft-fail (accept + discard, log). The old stub did this
/// implicitly; now it's explicit. Hard-failing would regress buggy clients.
#[tokio::test]
async fn test_register_drv_output_malformed_json_soft_fails() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream;
        u64: 42,
        string: "not valid json {",
    );

    // STDERR_LAST, no error. Session continues.
    drain_stderr_until_last(&mut h.stream).await?;
    assert!(
        h.store.realisations.read().unwrap().is_empty(),
        "malformed JSON should not reach MockStore"
    );

    h.finish().await;
    Ok(())
}

/// Malformed DrvOutput id (wrong prefix, bad hex, no !, etc): soft-fail.
#[tokio::test]
async fn test_register_drv_output_malformed_id_soft_fails() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    // "abc" is 3 hex chars — not 64, so hex::decode → wrong length.
    // Previously the stub test used exactly this and it "passed" because
    // the stub didn't parse the id at all.
    let bad_json = r#"{"id":"sha256:abc!out","outPath":"/nix/store/xyz","signatures":[],"dependentRealisations":{}}"#;
    wire_send!(&mut h.stream;
        u64: 42,
        string: bad_json,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    assert!(h.store.realisations.read().unwrap().is_empty());

    h.finish().await;
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
    let calls = h.store.put_calls.read().unwrap().clone();
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

    // 9-field ValidPathInfo response
    let path = wire::read_string(&mut h.stream).await?;
    let _deriver = wire::read_string(&mut h.stream).await?;
    let nar_hash_hex = wire::read_string(&mut h.stream).await?;
    let _references = wire::read_strings(&mut h.stream).await?;
    let _reg_time = wire::read_u64(&mut h.stream).await?;
    let nar_size = wire::read_u64(&mut h.stream).await?;
    let _ultimate = wire::read_bool(&mut h.stream).await?;
    let _sigs = wire::read_strings(&mut h.stream).await?;
    let ca = wire::read_string(&mut h.stream).await?;

    assert!(
        path.starts_with("/nix/store/") && path.ends_with("-text-test"),
        "computed path should be a store path ending in -text-test: {path}"
    );
    assert_eq!(nar_hash_hex.len(), 64, "nar_hash should be hex sha256");
    assert!(nar_size > 0);
    assert!(
        ca.starts_with("text:sha256:"),
        "ca should start with text:sha256:, got: {ca}"
    );

    // Verify store received the upload at the computed path.
    let calls = h.store.put_calls.read().unwrap().clone();
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
    let _deriver = wire::read_string(&mut h.stream).await?;
    let _nar_hash = wire::read_string(&mut h.stream).await?;
    let _refs = wire::read_strings(&mut h.stream).await?;
    let _reg_time = wire::read_u64(&mut h.stream).await?;
    let nar_size = wire::read_u64(&mut h.stream).await?;
    let _ult = wire::read_bool(&mut h.stream).await?;
    let _sigs = wire::read_strings(&mut h.stream).await?;
    let ca = wire::read_string(&mut h.stream).await?;

    assert!(path.ends_with("-flat-test"));
    // Flat: NAR wraps the raw content, so nar_size > content.len()
    assert!(
        nar_size > content.len() as u64,
        "NAR wrapping should increase size: nar_size={nar_size}, content={}",
        content.len()
    );
    assert!(
        ca.starts_with("fixed:sha256:"),
        "flat ca should be fixed:sha256: (no r:), got: {ca}"
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

/// AddToStoreNar with nar_size > MAX_FRAMED_TOTAL should send STDERR_ERROR
/// before reading any NAR data (prevents DoS).
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
