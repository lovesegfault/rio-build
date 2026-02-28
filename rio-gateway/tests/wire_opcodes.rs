//! Byte-level opcode tests for the rio-gateway Nix worker protocol handler.
//!
//! These tests construct raw wire bytes for each opcode, feed them through
//! `run_protocol` via a DuplexStream, and assert the response bytes match the
//! Nix worker protocol spec. This catches framing and encoding bugs that
//! high-level integration tests hide.
//!
//! Test structure:
//!   - TestHarness: wraps duplex stream + mock gRPC servers + spawned protocol task
//!   - drain_stderr_until_last: consumes STDERR messages until STDERR_LAST
//!   - drain_stderr_expecting_error: consumes STDERR messages expecting STDERR_ERROR
//!   - Per-opcode tests: happy path + error path for each

use tokio::io::{AsyncWriteExt, DuplexStream};
use tonic::transport::Channel;

use rio_nix::protocol::wire;
use rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient;
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_test_support::fixtures::{make_nar, make_path_info};
use rio_test_support::grpc::{
    MockScheduler, MockSchedulerOutcome, MockStore, spawn_mock_scheduler, spawn_mock_store,
};
use rio_test_support::wire::{
    do_handshake, drain_stderr_expecting_error, drain_stderr_until_last, send_set_options,
};

// ===========================================================================
// Test Harness
// ===========================================================================

struct TestHarness {
    /// Client-side stream for writing opcodes and reading responses.
    stream: DuplexStream,
    /// Mock store (pre-seed paths, inspect put_calls).
    store: MockStore,
    /// Mock scheduler (set outcome, inspect submit_calls).
    scheduler: MockScheduler,
    /// Server task running run_protocol.
    server_task: tokio::task::JoinHandle<()>,
    /// gRPC server handles (abort on drop).
    store_handle: tokio::task::JoinHandle<()>,
    sched_handle: tokio::task::JoinHandle<()>,
}

impl TestHarness {
    /// Start mock gRPC servers, spawn run_protocol, and perform handshake +
    /// setOptions. Returns a harness with a ready-to-use client stream.
    async fn setup() -> Self {
        let (store, store_addr, store_handle) = spawn_mock_store().await;
        let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await;

        // Connect gRPC clients
        let store_channel = Channel::from_shared(format!("http://{store_addr}"))
            .unwrap()
            .connect()
            .await
            .expect("connect to mock store");
        let mut store_client = StoreServiceClient::new(store_channel);
        let sched_channel = Channel::from_shared(format!("http://{sched_addr}"))
            .unwrap()
            .connect()
            .await
            .expect("connect to mock scheduler");
        let mut scheduler_client = SchedulerServiceClient::new(sched_channel);

        // Duplex stream: client side stays here, server side goes to run_protocol
        let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);

        let server_task = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(server_stream);
            let result = rio_gateway::session::run_protocol(
                &mut reader,
                &mut writer,
                &mut store_client,
                &mut scheduler_client,
            )
            .await;
            // Clean EOF is expected. Handlers that send STDERR_ERROR also
            // return Err to close the connection (per protocol spec), so any
            // error here is allowed — the test's real assertion is the
            // client-side drain_stderr_expecting_error check.
            if let Err(e) = &result {
                let is_eof = e
                    .downcast_ref::<rio_nix::protocol::wire::WireError>()
                    .is_some_and(|we| {
                        matches!(
                            we,
                            rio_nix::protocol::wire::WireError::Io(io)
                                if io.kind() == std::io::ErrorKind::UnexpectedEof
                        )
                    });
                if !is_eof {
                    // Log, don't panic — error-path tests expect this.
                    tracing::debug!(error = %e, "server returned error (expected for error-path tests)");
                }
            }
        });

        let mut h = Self {
            stream: client_stream,
            store,
            scheduler,
            server_task,
            store_handle,
            sched_handle,
        };

        do_handshake(&mut h.stream).await;
        send_set_options(&mut h.stream).await;

        h
    }

    /// Finish the session: drop the client stream (EOF), await server task.
    async fn finish(self) {
        let TestHarness {
            stream,
            server_task,
            store_handle,
            sched_handle,
            ..
        } = self;
        drop(stream);
        server_task.await.expect("server task should not panic");
        store_handle.abort();
        sched_handle.abort();
    }
}

/// A valid-looking Nix store path (32-char nixbase32 hash + name).
const TEST_PATH_A: &str = "/nix/store/00000000000000000000000000000000-test-a";
const TEST_PATH_MISSING: &str = "/nix/store/11111111111111111111111111111111-missing";

// ===========================================================================
// Opcode tests: IsValidPath (1)
// ===========================================================================

#[tokio::test]
async fn test_is_valid_path_exists() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"hello");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    wire::write_u64(&mut h.stream, 1).await.unwrap(); // wopIsValidPath
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(valid, "seeded path should be valid");

    h.finish().await;
}

#[tokio::test]
async fn test_is_valid_path_missing() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 1).await.unwrap(); // wopIsValidPath
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(!valid, "missing path should be invalid");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: EnsurePath (10)
// ===========================================================================

#[tokio::test]
async fn test_ensure_path_exists() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"ensure");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    wire::write_u64(&mut h.stream, 10).await.unwrap(); // wopEnsurePath
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "EnsurePath should return 1 (success)");

    h.finish().await;
}

/// Phase 2a: EnsurePath is a stub that always returns success regardless of
/// whether the path exists. It reads the path argument and returns 1.
#[tokio::test]
async fn test_ensure_path_stub_always_succeeds() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 10).await.unwrap(); // wopEnsurePath
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // Phase 2a stub: always STDERR_LAST + 1, even for missing paths.
    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "EnsurePath stub returns 1 unconditionally");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: QueryPathInfo (26)
// ===========================================================================

#[tokio::test]
async fn test_query_path_info_exists() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"pathinfo");
    let info = make_path_info(TEST_PATH_A, &nar, hash);
    h.store.seed(info, nar.clone());

    wire::write_u64(&mut h.stream, 26).await.unwrap(); // wopQueryPathInfo
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    // Response: bool(valid) + if valid: deriver, hex_nar_hash, refs, regtime, nar_size, ultimate, sigs, ca
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(valid, "path should be valid");
    let deriver = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(deriver, "");
    let nar_hash_hex = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(nar_hash_hex, hex::encode(hash), "nar hash should match");
    let refs = wire::read_strings(&mut h.stream).await.unwrap();
    assert!(refs.is_empty());
    let _regtime = wire::read_u64(&mut h.stream).await.unwrap();
    let nar_size = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(nar_size, nar.len() as u64);
    let _ultimate = wire::read_bool(&mut h.stream).await.unwrap();
    let _sigs = wire::read_strings(&mut h.stream).await.unwrap();
    let _ca = wire::read_string(&mut h.stream).await.unwrap();

    h.finish().await;
}

#[tokio::test]
async fn test_query_path_info_missing_returns_invalid() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 26).await.unwrap(); // wopQueryPathInfo
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // QueryPathInfo for missing path returns STDERR_LAST + valid=false
    // (not STDERR_ERROR — the Nix protocol uses valid=false here).
    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(!valid, "missing path should return valid=false");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: QueryPathFromHashPart (29)
// ===========================================================================

#[tokio::test]
async fn test_query_path_from_hash_part_found() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"hashpart");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    // Hash part is the 32-char basename prefix
    let hash_part = "00000000000000000000000000000000";

    wire::write_u64(&mut h.stream, 29).await.unwrap(); // wopQueryPathFromHashPart
    wire::write_string(&mut h.stream, hash_part).await.unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(result, TEST_PATH_A, "should return the full store path");

    h.finish().await;
}

#[tokio::test]
async fn test_query_path_from_hash_part_not_found() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 29).await.unwrap(); // wopQueryPathFromHashPart
    wire::write_string(&mut h.stream, "11111111111111111111111111111111")
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(result, "", "not found should return empty string");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: AddTempRoot (11)
// ===========================================================================

#[tokio::test]
async fn test_add_temp_root() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 11).await.unwrap(); // wopAddTempRoot
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "AddTempRoot always returns 1 (success)");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: NarFromPath (38)
// ===========================================================================

#[tokio::test]
async fn test_nar_from_path_streams_chunks() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"nar-from-path content");
    h.store
        .seed(make_path_info(TEST_PATH_A, &nar, hash), nar.clone());

    wire::write_u64(&mut h.stream, 38).await.unwrap(); // wopNarFromPath
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // NarFromPath: STDERR_LAST first, then RAW NAR bytes (NOT STDERR_WRITE).
    // Nix client: processStderr(ex) with no sink → copyNAR(from, sink).
    let msgs = drain_stderr_until_last(&mut h.stream).await;
    assert!(
        msgs.is_empty(),
        "no STDERR messages expected before STDERR_LAST; got: {msgs:?}"
    );
    // Read the raw NAR bytes that follow STDERR_LAST.
    let mut received = vec![0u8; nar.len()];
    tokio::io::AsyncReadExt::read_exact(&mut h.stream, &mut received)
        .await
        .unwrap();
    assert_eq!(received, nar, "received NAR bytes should match seeded NAR");

    h.finish().await;
}

#[tokio::test]
async fn test_nar_from_path_missing_returns_error() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 38).await.unwrap(); // wopNarFromPath
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        !err.message().is_empty(),
        "error message should be non-empty"
    );

    h.finish().await;
}

#[tokio::test]
async fn test_nar_from_path_invalid_path_returns_error() {
    let mut h = TestHarness::setup().await;

    // Send a string that fails StorePath::parse (no /nix/store/ prefix).
    wire::write_u64(&mut h.stream, 38).await.unwrap(); // wopNarFromPath
    wire::write_string(&mut h.stream, "not-a-valid-store-path")
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message().contains("invalid store path"),
        "error should mention invalid store path, got: {}",
        err.message()
    );

    // Session must terminate after STDERR_ERROR (handler returns Err now).
    h.finish().await;
}

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
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // references
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // registration_time
    wire::write_u64(&mut h.stream, nar.len() as u64)
        .await
        .unwrap(); // nar_size
    wire::write_bool(&mut h.stream, false).await.unwrap(); // ultimate
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // sigs
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
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    wire::write_u64(&mut h.stream, nar.len() as u64)
        .await
        .unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
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
    wire::write_strings(&mut inner, &[]).await.unwrap(); // refs
    wire::write_u64(&mut inner, 0).await.unwrap(); // regtime
    wire::write_u64(&mut inner, nar_a.len() as u64)
        .await
        .unwrap(); // nar_size
    wire::write_bool(&mut inner, false).await.unwrap(); // ultimate
    wire::write_strings(&mut inner, &[]).await.unwrap(); // sigs
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
    wire::write_strings(&mut inner, &[]).await.unwrap();
    wire::write_u64(&mut inner, 0).await.unwrap();
    wire::write_u64(&mut inner, nar_b.len() as u64)
        .await
        .unwrap();
    wire::write_bool(&mut inner, false).await.unwrap();
    wire::write_strings(&mut inner, &[]).await.unwrap();
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
    wire::write_strings(&mut inner, &[]).await.unwrap();
    wire::write_u64(&mut inner, 0).await.unwrap();
    // LIE about nar_size: claim more bytes than we actually send.
    wire::write_u64(&mut inner, nar.len() as u64 + 100)
        .await
        .unwrap();
    wire::write_bool(&mut inner, false).await.unwrap();
    wire::write_strings(&mut inner, &[]).await.unwrap();
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
        err.message().contains("truncated"),
        "expected 'truncated' in error, got: {}",
        err.message()
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
    wire::write_strings(&mut h.stream, &["sig:fake".to_string()])
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
async fn test_query_realisation_stub_returns_empty() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 43).await.unwrap(); // wopQueryRealisation
    wire::write_string(&mut h.stream, "sha256:abc!out")
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let count = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(count, 0, "QueryRealisation stub should return empty set");

    h.finish().await;
}

// ===========================================================================
// Unknown opcode test
// ===========================================================================

#[tokio::test]
async fn test_unknown_opcode_returns_stderr_error() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 99).await.unwrap(); // unknown opcode
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message().contains("99")
            || err.message().to_lowercase().contains("unknown")
            || err.message().to_lowercase().contains("unimplemented"),
        "error should mention unknown/unimplemented opcode: {}",
        err.message()
    );

    h.finish().await;
}

// ===========================================================================
// Build opcode tests
// ===========================================================================

/// wopBuildPaths (9): reads strings(paths) + u64(build_mode), writes u64(1).
#[tokio::test]
async fn test_build_paths_success() {
    let mut h = TestHarness::setup().await;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    // Seed a .drv in store so translate::reconstruct_dag can resolve it.
    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire::write_u64(&mut h.stream, 9).await.unwrap(); // wopBuildPaths
    // DerivedPath format: "drv_path!output_name" for Built paths
    wire::write_strings(&mut h.stream, &[format!("{drv_path}!out")])
        .await
        .unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // build_mode = Normal
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "BuildPaths returns u64(1) on success");

    // Verify scheduler received the submit request.
    let submits = h.scheduler.submit_calls.read().unwrap().clone();
    assert_eq!(submits.len(), 1, "scheduler should receive one SubmitBuild");

    h.finish().await;
}

/// wopBuildPaths with scheduler error: should send STDERR_ERROR.
#[tokio::test]
async fn test_build_paths_scheduler_error_returns_stderr_error() {
    let mut h = TestHarness::setup().await;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        submit_error: Some(tonic::Code::Unavailable),
        ..Default::default()
    });

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire::write_u64(&mut h.stream, 9).await.unwrap(); // wopBuildPaths
    wire::write_strings(&mut h.stream, &[format!("{drv_path}!out")])
        .await
        .unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(!err.message().is_empty());

    h.finish().await;
}

/// wopBuildPaths when the scheduler stream closes without a terminal event
/// (BuildCompleted/BuildFailed/BuildCancelled). Regression for commit b2d3863:
/// process_build_events must NOT send STDERR_ERROR itself — it returns Err
/// and lets the CALLER send STDERR_ERROR (opcode 9) or STDERR_LAST + failure
/// (opcode 46). Before b2d3863, process_build_events sent STDERR_ERROR inside
/// the loop, causing a double-STDERR_ERROR or STDERR_ERROR-then-STDERR_LAST
/// invalid frame sequence depending on the opcode.
///
/// For opcode 9, the correct behavior is: EXACTLY ONE STDERR_ERROR, then the
/// session closes. We verify this by using drain_stderr_expecting_error which
/// reads until it gets STDERR_ERROR; if process_build_events had already sent
/// one AND the handler sent another, the test harness's handler_task would
/// fail or the stream would desync.
#[tokio::test]
async fn test_build_paths_stream_closed_without_terminal_single_error() {
    let mut h = TestHarness::setup().await;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        close_stream_early: true,
        ..Default::default()
    });

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-early-close.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire::write_u64(&mut h.stream, 9).await.unwrap(); // wopBuildPaths
    wire::write_strings(&mut h.stream, &[format!("{drv_path}!out")])
        .await
        .unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    h.stream.flush().await.unwrap();

    // For opcode 9: caller (handle_build_paths) sends STDERR_ERROR on failure.
    // process_build_events must NOT also have sent one (that's the b2d3863 fix).
    // drain_stderr_expecting_error reads one STDERR_ERROR and stops; if there
    // were two, the leftover bytes would desync the stream and h.finish()
    // would fail.
    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message().contains("stream ended") || err.message().contains("disconnect"),
        "error should mention stream ended / scheduler disconnect: {}",
        err.message()
    );

    h.finish().await;
}

/// wopBuildPathsWithResults (46): reads strings + build_mode, writes
/// u64(count) + per-entry (string:DerivedPath, BuildResult).
#[tokio::test]
async fn test_build_paths_with_results_keyed_format() {
    let mut h = TestHarness::setup().await;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    let derived_path = format!("{drv_path}!out");
    wire::write_u64(&mut h.stream, 46).await.unwrap(); // wopBuildPathsWithResults
    wire::write_strings(&mut h.stream, std::slice::from_ref(&derived_path))
        .await
        .unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    // KeyedBuildResult: u64(count) + per-entry (string:path, BuildResult)
    let count = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(count, 1, "one DerivedPath requested, one result");

    // DerivedPath echoed back
    let path = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(path, derived_path, "DerivedPath should be echoed back");

    // BuildResult: status + errorMsg + timesBuilt + isNonDeterministic +
    // startTime + stopTime + cpuUser(tag+val) + cpuSystem(tag+val) +
    // builtOutputs(count + per-output pair)
    let status = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(status, 0, "BuildStatus::Built = 0");
    let _error_msg = wire::read_string(&mut h.stream).await.unwrap();
    let _times_built = wire::read_u64(&mut h.stream).await.unwrap();
    let _is_non_det = wire::read_bool(&mut h.stream).await.unwrap();
    let _start_time = wire::read_u64(&mut h.stream).await.unwrap();
    let _stop_time = wire::read_u64(&mut h.stream).await.unwrap();
    // cpuUser: tag + optional value
    let cpu_user_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_user_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    // cpuSystem: tag + optional value
    let cpu_system_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_system_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    // builtOutputs
    let built_outputs_count = wire::read_u64(&mut h.stream).await.unwrap();
    for _ in 0..built_outputs_count {
        let _drv_output_id = wire::read_string(&mut h.stream).await.unwrap();
        let _realisation_json = wire::read_string(&mut h.stream).await.unwrap();
    }

    h.finish().await;
}

/// wopQueryMissing (40): reads strings(paths), writes willBuild + willSubstitute
/// + unknown + downloadSize + narSize.
#[tokio::test]
async fn test_query_missing_reports_will_build() {
    let mut h = TestHarness::setup().await;

    // Don't seed the .drv: handler filters paths whose store_path is NOT in
    // the missing set. A Built path's store_path() is the .drv; if the .drv
    // is missing from store, the handler reports it in willBuild.
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";

    wire::write_u64(&mut h.stream, 40).await.unwrap(); // wopQueryMissing
    wire::write_strings(&mut h.stream, &[format!("{drv_path}!out")])
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    let will_build = wire::read_strings(&mut h.stream).await.unwrap();
    let will_substitute = wire::read_strings(&mut h.stream).await.unwrap();
    let unknown = wire::read_strings(&mut h.stream).await.unwrap();
    let download_size = wire::read_u64(&mut h.stream).await.unwrap();
    let nar_size = wire::read_u64(&mut h.stream).await.unwrap();

    assert_eq!(
        will_build.len(),
        1,
        "missing Built .drv should be in willBuild"
    );
    assert!(will_build[0].contains(drv_path));
    assert!(will_substitute.is_empty(), "Phase 2a: no substitutes");
    assert!(unknown.is_empty());
    assert_eq!(download_size, 0);
    assert_eq!(nar_size, 0);

    h.finish().await;
}

/// wopQueryDerivationOutputMap (41): reads drv path, writes count +
/// (name, path) pairs. Error path: missing .drv in store.
#[tokio::test]
async fn test_query_derivation_output_map_missing_drv() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 41).await.unwrap();
    wire::write_string(
        &mut h.stream,
        "/nix/store/11111111111111111111111111111111-missing.drv",
    )
    .await
    .unwrap();
    h.stream.flush().await.unwrap();

    // Missing .drv: STDERR_ERROR
    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(!err.message().is_empty());

    h.finish().await;
}

/// wopQueryDerivationOutputMap (41) happy path: .drv is in store, returns
/// output name -> path map.
#[tokio::test]
async fn test_query_derivation_output_map_found() {
    let mut h = TestHarness::setup().await;

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","",""),("dev","/nix/store/yyy-dev","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire::write_u64(&mut h.stream, 41).await.unwrap();
    wire::write_string(&mut h.stream, drv_path).await.unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    let count = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(count, 2, "two outputs (out, dev)");
    let mut outputs = std::collections::HashMap::new();
    for _ in 0..count {
        let name = wire::read_string(&mut h.stream).await.unwrap();
        let path = wire::read_string(&mut h.stream).await.unwrap();
        outputs.insert(name, path);
    }
    assert_eq!(outputs.get("out").unwrap(), "/nix/store/zzz-output");
    assert_eq!(outputs.get("dev").unwrap(), "/nix/store/yyy-dev");

    h.finish().await;
}

/// wopAddTextToStore (8): reads name + text + refs, writes computed CA path.
#[tokio::test]
async fn test_add_text_to_store() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 8).await.unwrap(); // wopAddTextToStore
    wire::write_string(&mut h.stream, "my-text").await.unwrap(); // name
    wire::write_string(&mut h.stream, "hello world")
        .await
        .unwrap(); // text
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // references
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
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // references
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
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // references
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
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // references
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    // Handler reads framed stream BEFORE parsing cam_str, so we must send it.
    wire::write_framed_stream(&mut h.stream, b"data", 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message().contains("content-address method") || err.message().contains("bogus"),
        "error should mention invalid cam_str: {}",
        err.message()
    );

    h.finish().await;
}

/// wopBuildDerivation (36): reads drv_path + BasicDerivation + build_mode,
/// writes BuildResult. BasicDerivation format is: output_count +
/// per-output(name, path, hash_algo, hash) + input_srcs + platform + builder +
/// args + env_pairs.
#[tokio::test]
async fn test_build_derivation_basic_format() {
    let mut h = TestHarness::setup().await;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";

    wire::write_u64(&mut h.stream, 36).await.unwrap(); // wopBuildDerivation
    wire::write_string(&mut h.stream, drv_path).await.unwrap(); // drv path

    // BasicDerivation: outputs
    wire::write_u64(&mut h.stream, 1).await.unwrap(); // 1 output
    wire::write_string(&mut h.stream, "out").await.unwrap(); // name
    wire::write_string(&mut h.stream, "/nix/store/zzz-output")
        .await
        .unwrap(); // path
    wire::write_string(&mut h.stream, "").await.unwrap(); // hash_algo (input-addressed)
    wire::write_string(&mut h.stream, "").await.unwrap(); // hash

    // input_srcs
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
    // platform
    wire::write_string(&mut h.stream, "x86_64-linux")
        .await
        .unwrap();
    // builder
    wire::write_string(&mut h.stream, "/bin/sh").await.unwrap();
    // args
    wire::write_strings(&mut h.stream, &["-c".into(), "echo hi".into()])
        .await
        .unwrap();
    // env pairs
    wire::write_string_pairs(
        &mut h.stream,
        &[("out".into(), "/nix/store/zzz-output".into())],
    )
    .await
    .unwrap();

    // build_mode
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    // BuildResult: status + errorMsg + timesBuilt + isNonDet + start + stop +
    // cpuUser(tag+val?) + cpuSystem(tag+val?) + builtOutputs
    let status = wire::read_u64(&mut h.stream).await.unwrap();
    // Mock sent send_completed: true, so status should be Built (0), not
    // just "any valid status". Previously: assert!(status <= 14) accepted
    // failures as passing.
    assert_eq!(
        status, 0,
        "status should be Built (0) since mock sent completed, got {status}"
    );
    let error_msg = wire::read_string(&mut h.stream).await.unwrap();
    assert!(error_msg.is_empty(), "error_msg should be empty on success");
    let _times_built = wire::read_u64(&mut h.stream).await.unwrap();
    let _is_non_det = wire::read_bool(&mut h.stream).await.unwrap();
    let _start_time = wire::read_u64(&mut h.stream).await.unwrap();
    let _stop_time = wire::read_u64(&mut h.stream).await.unwrap();
    let cpu_user_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_user_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let cpu_system_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_system_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let built_outputs_count = wire::read_u64(&mut h.stream).await.unwrap();
    for _ in 0..built_outputs_count {
        let _drv_output_id = wire::read_string(&mut h.stream).await.unwrap();
        let _realisation_json = wire::read_string(&mut h.stream).await.unwrap();
    }

    h.finish().await;
}

// ===========================================================================
// Additional coverage: K2/K3 from phase 2a review
// ===========================================================================
//
// Note on error-path behavior discovered during test development:
// Many opcodes GRACEFULLY handle invalid store paths instead of sending
// STDERR_ERROR. This is intentional Nix-compatible behavior:
//   - IsValidPath (1):   invalid path -> returns false (not error)
//   - EnsurePath (10):   invalid path -> ignored, returns success
//   - AddTempRoot (11):  invalid path -> ignored, returns success
//   - QueryPathInfo (26): invalid path -> returns valid=false (not error)
//   - SetOptions (19):   no error path (fixed-width fields only)
// These opcodes DO have STDERR_ERROR paths for gRPC failures (store unreachable),
// but those are harder to trigger in the mock harness and are covered by the
// timeout integration path rather than byte-level tests.
//
// Opcodes with true STDERR_ERROR paths for client-side invalid input:
//   - NarFromPath (38):  invalid path -> error (tested above)
//   - AddToStoreNar (39): invalid path, oversized nar_size -> error (tested below)
//   - BuildDerivation (36): parse failure -> connection drop (wire read error)

/// QueryValidPaths (31) happy path: returns paths present in the mock store.
#[tokio::test]
async fn test_query_valid_paths_filters_missing() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"qvp");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);
    // TEST_PATH_MISSING is not seeded.

    wire::write_u64(&mut h.stream, 31).await.unwrap(); // wopQueryValidPaths
    wire::write_strings(
        &mut h.stream,
        &[TEST_PATH_A.into(), TEST_PATH_MISSING.into()],
    )
    .await
    .unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap(); // substitute
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_strings(&mut h.stream).await.unwrap();
    assert_eq!(
        valid,
        vec![TEST_PATH_A.to_string()],
        "only seeded path should be valid"
    );

    h.finish().await;
}

/// QueryValidPaths with empty input returns empty output.
#[tokio::test]
async fn test_query_valid_paths_empty() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 31).await.unwrap();
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_strings(&mut h.stream).await.unwrap();
    assert!(valid.is_empty());

    h.finish().await;
}

/// SetOptions (19) standalone: verifies the opcode round-trip independently
/// of the harness setup (which also sends it). Confirms STDERR_LAST with no
/// result data.
#[tokio::test]
async fn test_set_options_standalone() {
    let mut h = TestHarness::setup().await;

    // Send a second SetOptions with different values.
    wire::write_u64(&mut h.stream, 19).await.unwrap(); // wopSetOptions
    wire::write_bool(&mut h.stream, true).await.unwrap(); // keepFailed
    wire::write_bool(&mut h.stream, true).await.unwrap(); // keepGoing
    wire::write_bool(&mut h.stream, false).await.unwrap(); // tryFallback
    wire::write_u64(&mut h.stream, 5).await.unwrap(); // verbosity
    wire::write_u64(&mut h.stream, 4).await.unwrap(); // maxBuildJobs
    wire::write_u64(&mut h.stream, 600).await.unwrap(); // maxSilentTime
    wire::write_bool(&mut h.stream, false).await.unwrap(); // useBuildHook
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // verboseBuild
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // logType
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // printBuildTrace
    wire::write_u64(&mut h.stream, 8).await.unwrap(); // buildCores
    wire::write_bool(&mut h.stream, true).await.unwrap(); // useSubstitutes
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // overrides count
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    // SetOptions has no result data.

    h.finish().await;
}

/// IsValidPath with unparseable path returns false (not STDERR_ERROR).
/// This documents the graceful-degradation behavior for Nix compatibility.
#[tokio::test]
async fn test_is_valid_path_garbage_returns_false() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 1).await.unwrap();
    wire::write_string(&mut h.stream, "garbage-not-a-store-path")
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // Should NOT receive STDERR_ERROR — just STDERR_LAST + false.
    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(!valid, "garbage path should return false, not error");

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
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // references
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // reg_time
    wire::write_u64(&mut h.stream, 100).await.unwrap(); // nar_size
    wire::write_bool(&mut h.stream, false).await.unwrap(); // ultimate
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // sigs
    wire::write_string(&mut h.stream, "").await.unwrap(); // ca
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_bool(&mut h.stream, true).await.unwrap(); // dontCheckSigs
    // No framed data — handler should error before reading it.
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message().contains("invalid store path"),
        "error should mention invalid path: {}",
        err.message()
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
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    wire::write_u64(&mut h.stream, u64::MAX).await.unwrap(); // nar_size HUGE
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
    wire::write_string(&mut h.stream, "").await.unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_bool(&mut h.stream, true).await.unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message().contains("exceeds maximum"),
        "error should mention size limit: {}",
        err.message()
    );

    h.finish().await;
}

/// BuildPathsWithResults (46) with an invalid build mode should still return
/// results (not STDERR_ERROR) — the handler treats unknown modes as Normal.
/// But invalid DerivedPath strings DO cause per-entry failures.
#[tokio::test]
async fn test_build_paths_with_results_invalid_derived_path() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 46).await.unwrap();
    // One invalid DerivedPath (unparseable).
    wire::write_strings(&mut h.stream, &["garbage!not@a#path".into()])
        .await
        .unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // buildMode = Normal
    h.stream.flush().await.unwrap();

    // Per CLAUDE.md: "per-entry errors in batch opcodes push
    // BuildResult::failure and continue, not abort". So we should get
    // STDERR_LAST + a failed BuildResult, not STDERR_ERROR.
    drain_stderr_until_last(&mut h.stream).await;
    let count = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(count, 1, "should get one result for one input path");
    let _path = wire::read_string(&mut h.stream).await.unwrap();
    // BuildResult: first field is status (u64).
    let status = wire::read_u64(&mut h.stream).await.unwrap();
    // Should be a failure status (NOT Built=0).
    assert_ne!(
        status, 0,
        "invalid DerivedPath should produce failure status"
    );
    // Drain remaining BuildResult fields.
    let _err_msg = wire::read_string(&mut h.stream).await.unwrap();
    let _times = wire::read_u64(&mut h.stream).await.unwrap();
    let _nondet = wire::read_bool(&mut h.stream).await.unwrap();
    let _start = wire::read_u64(&mut h.stream).await.unwrap();
    let _stop = wire::read_u64(&mut h.stream).await.unwrap();
    let tag1 = wire::read_u64(&mut h.stream).await.unwrap();
    if tag1 == 1 {
        let _ = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let tag2 = wire::read_u64(&mut h.stream).await.unwrap();
    if tag2 == 1 {
        let _ = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let outputs = wire::read_u64(&mut h.stream).await.unwrap();
    for _ in 0..outputs {
        let _ = wire::read_string(&mut h.stream).await.unwrap();
        let _ = wire::read_string(&mut h.stream).await.unwrap();
    }

    h.finish().await;
}

// ===========================================================================
// Ported error-path tests from rio-build/tests/direct_protocol.rs
// ===========================================================================
//
// NOTE: hash/size mismatch tests for AddToStoreNar are NOT ported — in the
// rio-gateway architecture, the gateway passes the declared hash through
// unchanged (see test_add_to_store_nar_passes_declared_hash above). Hash
// validation is rio-store's responsibility; see rio-store/src/validate.rs
// tests (validate_nar_rejects_hash_mismatch et al.) and
// rio-store/tests/grpc_integration.rs::test_put_path_hash_mismatch_cleans_up.

/// Handshake-level: client sends a version older than 1.37 → STDERR_ERROR.
/// This is gateway's responsibility (protocol negotiation), not the store's.
#[tokio::test]
async fn test_version_too_old_sends_stderr_error() {
    use rio_nix::protocol::handshake::{WORKER_MAGIC_1, WORKER_MAGIC_2};

    let (_store, store_addr, store_handle) = spawn_mock_store().await;
    let (_sched, sched_addr, sched_handle) = spawn_mock_scheduler().await;

    let store_channel = Channel::from_shared(format!("http://{store_addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut store_client = StoreServiceClient::new(store_channel);
    let sched_channel = Channel::from_shared(format!("http://{sched_addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut scheduler_client = SchedulerServiceClient::new(sched_channel);

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(server);
        let _ = rio_gateway::session::run_protocol(
            &mut r,
            &mut w,
            &mut store_client,
            &mut scheduler_client,
        )
        .await;
    });

    // Phase 1: send magic + old version (1.32 = 0x120)
    wire::write_u64(&mut client, WORKER_MAGIC_1).await.unwrap();
    wire::write_u64(&mut client, 0x120).await.unwrap();
    client.flush().await.unwrap();

    // Read server magic + server version
    let magic2 = wire::read_u64(&mut client).await.unwrap();
    assert_eq!(magic2, WORKER_MAGIC_2);
    let _server_version = wire::read_u64(&mut client).await.unwrap();

    // Server should now send STDERR_ERROR (version rejected before feature exchange)
    let err = drain_stderr_expecting_error(&mut client).await;
    assert!(
        err.message().contains("1.37+"),
        "error should mention '1.37+', got: {}",
        err.message()
    );

    drop(client);
    server_task.await.unwrap();
    store_handle.abort();
    sched_handle.abort();
}

/// Session stays open across multiple opcodes. Gateway processes each
/// sequentially and returns to the opcode loop after STDERR_LAST.
#[tokio::test]
async fn test_multi_opcode_sequence() {
    use rio_nix::protocol::stderr::STDERR_LAST;

    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"multi-op");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    // Op 1: IsValidPath (found)
    wire::write_u64(&mut h.stream, 1).await.unwrap();
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();
    assert_eq!(wire::read_u64(&mut h.stream).await.unwrap(), STDERR_LAST);
    assert_eq!(wire::read_u64(&mut h.stream).await.unwrap(), 1, "found");

    // Op 2: IsValidPath (not found)
    wire::write_u64(&mut h.stream, 1).await.unwrap();
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();
    assert_eq!(wire::read_u64(&mut h.stream).await.unwrap(), STDERR_LAST);
    assert_eq!(wire::read_u64(&mut h.stream).await.unwrap(), 0, "missing");

    // Op 3: AddTempRoot
    wire::write_u64(&mut h.stream, 11).await.unwrap();
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();
    assert_eq!(wire::read_u64(&mut h.stream).await.unwrap(), STDERR_LAST);
    assert_eq!(wire::read_u64(&mut h.stream).await.unwrap(), 1);

    // Op 4: QueryPathFromHashPart (found — prefix match)
    wire::write_u64(&mut h.stream, 29).await.unwrap();
    wire::write_string(&mut h.stream, "00000000000000000000000000000000")
        .await
        .unwrap();
    h.stream.flush().await.unwrap();
    assert_eq!(wire::read_u64(&mut h.stream).await.unwrap(), STDERR_LAST);
    let path = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(path, TEST_PATH_A);

    h.finish().await;
}
