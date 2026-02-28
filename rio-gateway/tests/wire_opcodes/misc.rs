use super::*;

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
