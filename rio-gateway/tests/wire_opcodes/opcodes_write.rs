use super::*;

// ===========================================================================
// Opcode tests: AddToStoreNar (39)
// ===========================================================================

#[tokio::test]
async fn test_add_to_store_nar_accepts_valid() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"add-to-store-nar");

    wire::write_u64(&mut h.stream, 39).await.unwrap(); // wopAddToStoreNar
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap(); // path
    wire::write_string(&mut h.stream, "").await.unwrap(); // deriver
    // narHash is hex-encoded SHA-256 (no algorithm prefix!)
    wire::write_string(&mut h.stream, &hex::encode(hash))
        .await
        .unwrap();
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // references
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // registration_time
    wire::write_u64(&mut h.stream, nar.len() as u64)
        .await
        .unwrap(); // nar_size
    wire::write_bool(&mut h.stream, false).await.unwrap(); // ultimate
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // sigs
    wire::write_string(&mut h.stream, "").await.unwrap(); // ca
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_bool(&mut h.stream, true).await.unwrap(); // dont_check_sigs
    // Framed NAR data: chunks of u64(len)+data, terminated by u64(0)
    wire::write_framed_stream(&mut h.stream, &nar, 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    // AddToStoreNar has no result data after STDERR_LAST.

    // Verify the mock store received the PutPath call.
    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1, "store should receive one PutPath call");
    assert_eq!(calls[0].store_path, TEST_PATH_A);
    assert_eq!(calls[0].nar_hash, hash.to_vec());

    h.finish().await;
}

/// Gateway trusts client-declared narHash and passes it to the store.
/// Hash verification is the store's responsibility (validate.rs). This test
/// verifies the gateway passes the declared hash through unchanged.
#[tokio::test]
async fn test_add_to_store_nar_passes_declared_hash() {
    let mut h = TestHarness::setup().await;
    let (nar, _actual_hash) = make_nar(b"trust-test");
    let declared_hash = [0xABu8; 32]; // deliberately different from actual

    wire::write_u64(&mut h.stream, 39).await.unwrap();
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    wire::write_string(&mut h.stream, "").await.unwrap();
    wire::write_string(&mut h.stream, &hex::encode(declared_hash))
        .await
        .unwrap();
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    wire::write_u64(&mut h.stream, nar.len() as u64)
        .await
        .unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_string(&mut h.stream, "").await.unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_bool(&mut h.stream, true).await.unwrap();
    wire::write_framed_stream(&mut h.stream, &nar, 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    // Verify the DECLARED hash (not actual) was passed to the store.
    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].nar_hash,
        declared_hash.to_vec(),
        "gateway should pass client-declared hash unchanged"
    );

    h.finish().await;
}

// ===========================================================================
// Opcode tests: AddMultipleToStore (44)
// ===========================================================================

#[tokio::test]
async fn test_add_multiple_to_store_batch() {
    let mut h = TestHarness::setup().await;
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
    let mut inner = Vec::new();

    // Count prefix
    wire::write_u64(&mut inner, 2).await.unwrap();

    // Entry 1
    wire::write_string(&mut inner, TEST_PATH_A).await.unwrap();
    wire::write_string(&mut inner, "").await.unwrap(); // deriver
    wire::write_string(&mut inner, &hex::encode(hash_a))
        .await
        .unwrap();
    wire::write_strings(&mut inner, wire::NO_STRINGS)
        .await
        .unwrap(); // refs
    wire::write_u64(&mut inner, 0).await.unwrap(); // regtime
    wire::write_u64(&mut inner, nar_a.len() as u64)
        .await
        .unwrap(); // nar_size
    wire::write_bool(&mut inner, false).await.unwrap(); // ultimate
    wire::write_strings(&mut inner, wire::NO_STRINGS)
        .await
        .unwrap(); // sigs
    wire::write_string(&mut inner, "").await.unwrap(); // ca
    // NAR: narSize plain bytes (NOT framed)
    inner.extend_from_slice(&nar_a);

    // Entry 2
    let test_path_b = "/nix/store/22222222222222222222222222222222-multi-b";
    wire::write_string(&mut inner, test_path_b).await.unwrap();
    wire::write_string(&mut inner, "").await.unwrap();
    wire::write_string(&mut inner, &hex::encode(hash_b))
        .await
        .unwrap();
    wire::write_strings(&mut inner, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_u64(&mut inner, 0).await.unwrap();
    wire::write_u64(&mut inner, nar_b.len() as u64)
        .await
        .unwrap();
    wire::write_bool(&mut inner, false).await.unwrap();
    wire::write_strings(&mut inner, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_string(&mut inner, "").await.unwrap();
    inner.extend_from_slice(&nar_b);

    // Send opcode + outer framing
    wire::write_u64(&mut h.stream, 44).await.unwrap(); // wopAddMultipleToStore
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_bool(&mut h.stream, true).await.unwrap(); // dont_check_sigs
    wire::write_framed_stream(&mut h.stream, &inner, 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 2, "store should receive 2 PutPath calls");
    let paths: Vec<&str> = calls.iter().map(|c| c.store_path.as_str()).collect();
    assert!(paths.contains(&TEST_PATH_A));
    assert!(paths.contains(&test_path_b));

    h.finish().await;
}

/// Regression: handler must reject truncated NAR (nar_size claims more bytes
/// than remain in the framed stream) instead of panicking on slice OOB.
#[tokio::test]
async fn test_add_multiple_to_store_truncated_nar() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"truncated");

    let mut inner = Vec::new();
    wire::write_u64(&mut inner, 1).await.unwrap(); // num_paths
    wire::write_string(&mut inner, TEST_PATH_A).await.unwrap();
    wire::write_string(&mut inner, "").await.unwrap();
    wire::write_string(&mut inner, &hex::encode(hash))
        .await
        .unwrap();
    wire::write_strings(&mut inner, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_u64(&mut inner, 0).await.unwrap();
    // LIE about nar_size: claim more bytes than we actually send.
    wire::write_u64(&mut inner, nar.len() as u64 + 100)
        .await
        .unwrap();
    wire::write_bool(&mut inner, false).await.unwrap();
    wire::write_strings(&mut inner, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_string(&mut inner, "").await.unwrap();
    inner.extend_from_slice(&nar); // actual NAR, 100 bytes short of claimed size

    wire::write_u64(&mut h.stream, 44).await.unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_bool(&mut h.stream, true).await.unwrap();
    wire::write_framed_stream(&mut h.stream, &inner, 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // Handler should send STDERR_ERROR (not crash).
    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message.contains("truncated"),
        "expected 'truncated' in error, got: {}",
        err.message
    );

    // No PutPath calls should have been made.
    assert_eq!(h.store.put_calls.read().unwrap().len(), 0);
}

// ===========================================================================
// Stub opcode tests: AddSignatures (37), RegisterDrvOutput (42),
// QueryRealisation (43)
// ===========================================================================

#[tokio::test]
async fn test_add_signatures_stub_returns_success() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 37).await.unwrap(); // wopAddSignatures
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    wire::write_strings(&mut h.stream, &["sig:fake"])
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "AddSignatures stub should return 1 (success)");

    h.finish().await;
}

#[tokio::test]
async fn test_register_drv_output_stub_reads_and_returns() {
    let mut h = TestHarness::setup().await;

    let realisation_json = r#"{"id":"sha256:abc!out","outPath":"/nix/store/xyz","signatures":[],"dependentRealisations":{}}"#;
    wire::write_u64(&mut h.stream, 42).await.unwrap(); // wopRegisterDrvOutput
    wire::write_string(&mut h.stream, realisation_json)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    // RegisterDrvOutput stub has no result data.

    h.finish().await;
}

#[tokio::test]
async fn test_add_text_to_store() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 8).await.unwrap(); // wopAddTextToStore
    wire::write_string(&mut h.stream, "my-text").await.unwrap(); // name
    wire::write_string(&mut h.stream, "hello world")
        .await
        .unwrap(); // text
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // references
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let path = wire::read_string(&mut h.stream).await.unwrap();
    assert!(
        path.starts_with("/nix/store/") && path.ends_with("-my-text"),
        "computed path should be a store path ending in -my-text: {path}"
    );

    // Verify store received the upload.
    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].store_path, path);

    h.finish().await;
}

/// wopAddToStore (7) with cam_str="text:sha256": text-method CA path.
/// Reads name + cam_str + references + repair + framed dump, returns
/// a 9-field ValidPathInfo. The computed path should match
/// StorePath::make_text (content-addressed text import).
#[tokio::test]
async fn test_add_to_store_text_method() {
    let mut h = TestHarness::setup().await;

    let content = b"hello from text method";

    wire::write_u64(&mut h.stream, 7).await.unwrap(); // wopAddToStore
    wire::write_string(&mut h.stream, "text-test")
        .await
        .unwrap(); // name
    wire::write_string(&mut h.stream, "text:sha256")
        .await
        .unwrap(); // cam_str
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // references
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_framed_stream(&mut h.stream, content, 8192)
        .await
        .unwrap(); // dump data
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    // 9-field ValidPathInfo response
    let path = wire::read_string(&mut h.stream).await.unwrap();
    let _deriver = wire::read_string(&mut h.stream).await.unwrap();
    let nar_hash_hex = wire::read_string(&mut h.stream).await.unwrap();
    let _references = wire::read_strings(&mut h.stream).await.unwrap();
    let _reg_time = wire::read_u64(&mut h.stream).await.unwrap();
    let nar_size = wire::read_u64(&mut h.stream).await.unwrap();
    let _ultimate = wire::read_bool(&mut h.stream).await.unwrap();
    let _sigs = wire::read_strings(&mut h.stream).await.unwrap();
    let ca = wire::read_string(&mut h.stream).await.unwrap();

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
}

/// wopAddToStore (7) with cam_str="fixed:sha256": flat-file fixed-output.
/// The dump data is raw file content (not NAR); handler wraps it in NAR.
#[tokio::test]
async fn test_add_to_store_fixed_flat() {
    let mut h = TestHarness::setup().await;

    let content = b"raw file content for flat fixed-output";

    wire::write_u64(&mut h.stream, 7).await.unwrap(); // wopAddToStore
    wire::write_string(&mut h.stream, "flat-test")
        .await
        .unwrap(); // name
    wire::write_string(&mut h.stream, "fixed:sha256")
        .await
        .unwrap(); // cam_str (flat, no r:)
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // references
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_framed_stream(&mut h.stream, content, 8192)
        .await
        .unwrap(); // dump data (raw, not NAR)
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    let path = wire::read_string(&mut h.stream).await.unwrap();
    let _deriver = wire::read_string(&mut h.stream).await.unwrap();
    let _nar_hash = wire::read_string(&mut h.stream).await.unwrap();
    let _refs = wire::read_strings(&mut h.stream).await.unwrap();
    let _reg_time = wire::read_u64(&mut h.stream).await.unwrap();
    let nar_size = wire::read_u64(&mut h.stream).await.unwrap();
    let _ult = wire::read_bool(&mut h.stream).await.unwrap();
    let _sigs = wire::read_strings(&mut h.stream).await.unwrap();
    let ca = wire::read_string(&mut h.stream).await.unwrap();

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
}

/// wopAddToStore (7) with an invalid cam_str should send STDERR_ERROR
/// (not crash or silently succeed).
#[tokio::test]
async fn test_add_to_store_invalid_cam_str_returns_error() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 7).await.unwrap(); // wopAddToStore
    wire::write_string(&mut h.stream, "bad-test").await.unwrap(); // name
    wire::write_string(&mut h.stream, "bogus:sha256")
        .await
        .unwrap(); // INVALID cam_str
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // references
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    // Handler reads framed stream BEFORE parsing cam_str, so we must send it.
    wire::write_framed_stream(&mut h.stream, b"data", 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message.contains("content-address method") || err.message.contains("bogus"),
        "error should mention invalid cam_str: {}",
        err.message
    );

    h.finish().await;
}

/// AddToStoreNar with invalid store path should send STDERR_ERROR.
#[tokio::test]
async fn test_add_to_store_nar_invalid_path_returns_error() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 39).await.unwrap();
    wire::write_string(&mut h.stream, "not-a-valid-store-path")
        .await
        .unwrap(); // path — INVALID
    wire::write_string(&mut h.stream, "").await.unwrap(); // deriver
    wire::write_string(&mut h.stream, &hex::encode([0u8; 32]))
        .await
        .unwrap(); // narHash
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // references
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // reg_time
    wire::write_u64(&mut h.stream, 100).await.unwrap(); // nar_size
    wire::write_bool(&mut h.stream, false).await.unwrap(); // ultimate
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap(); // sigs
    wire::write_string(&mut h.stream, "").await.unwrap(); // ca
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_bool(&mut h.stream, true).await.unwrap(); // dontCheckSigs
    // No framed data — handler should error before reading it.
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message.contains("invalid store path"),
        "error should mention invalid path: {}",
        err.message
    );

    h.finish().await;
}

/// AddToStoreNar with nar_size > MAX_FRAMED_TOTAL should send STDERR_ERROR
/// before reading any NAR data (prevents DoS).
#[tokio::test]
async fn test_add_to_store_nar_oversized_returns_error() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 39).await.unwrap();
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    wire::write_string(&mut h.stream, "").await.unwrap(); // deriver
    wire::write_string(&mut h.stream, &hex::encode([0u8; 32]))
        .await
        .unwrap();
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    wire::write_u64(&mut h.stream, u64::MAX).await.unwrap(); // nar_size HUGE
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_strings(&mut h.stream, wire::NO_STRINGS)
        .await
        .unwrap();
    wire::write_string(&mut h.stream, "").await.unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_bool(&mut h.stream, true).await.unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message.contains("exceeds maximum"),
        "error should mention size limit: {}",
        err.message
    );

    h.finish().await;
}
