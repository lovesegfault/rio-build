use super::*;

// ===========================================================================
// Unknown opcode test
// ===========================================================================

#[tokio::test]
async fn test_unknown_opcode_returns_stderr_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream; u64: 99); // unknown opcode

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("99")
            || err.message.to_lowercase().contains("unknown")
            || err.message.to_lowercase().contains("unimplemented"),
        "error should mention unknown/unimplemented opcode: {}",
        err.message
    );

    h.finish().await;
    Ok(())
}

/// SetOptions (19) standalone: verifies the opcode round-trip independently
/// of the harness setup (which also sends it). Confirms STDERR_LAST with no
/// result data.
#[tokio::test]
async fn test_set_options_standalone() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    // Send a second SetOptions with different values.
    wire_send!(&mut h.stream;
        u64: 19,                                 // wopSetOptions
        bool: true,                              // keepFailed
        bool: true,                              // keepGoing
        bool: false,                             // tryFallback
        u64: 5,                                  // verbosity
        u64: 4,                                  // maxBuildJobs
        u64: 600,                                // maxSilentTime
        bool: false,                             // useBuildHook
        u64: 0,                                  // verboseBuild
        u64: 0,                                  // logType
        u64: 0,                                  // printBuildTrace
        u64: 8,                                  // buildCores
        bool: true,                              // useSubstitutes
        u64: 0,                                  // overrides count
    );

    drain_stderr_until_last(&mut h.stream).await?;
    // SetOptions has no result data.

    h.finish().await;
    Ok(())
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
///
/// Uses bare `GatewaySession::new()` (NOT `new_with_handshake`) so we can
/// drive the handshake manually with a bad version.
#[tokio::test]
async fn test_version_too_old_sends_stderr_error() -> anyhow::Result<()> {
    use rio_nix::protocol::handshake::{WORKER_MAGIC_1, WORKER_MAGIC_2};

    let mut h = GatewaySession::new().await?;

    // Phase 1: send magic + old version (1.32 = 0x120)
    wire_send!(&mut h.stream;
        u64: WORKER_MAGIC_1,
        u64: 0x120,
    );

    // Read server magic + server version
    let magic2 = wire::read_u64(&mut h.stream).await?;
    assert_eq!(magic2, WORKER_MAGIC_2);
    let _server_version = wire::read_u64(&mut h.stream).await?;

    // Server should now send STDERR_ERROR (version rejected before feature exchange)
    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("1.37+"),
        "error should mention '1.37+', got: {}",
        err.message
    );

    h.finish().await;
    Ok(())
}

/// Session stays open across multiple opcodes. Gateway processes each
/// sequentially and returns to the opcode loop after STDERR_LAST.
#[tokio::test]
async fn test_multi_opcode_sequence() -> anyhow::Result<()> {
    use rio_nix::protocol::stderr::STDERR_LAST;

    let mut h = GatewaySession::new_with_handshake().await?;
    h.store.seed_with_content(TEST_PATH_A, b"multi-op");

    // Op 1: IsValidPath (found)
    wire_send!(&mut h.stream; u64: 1, string: TEST_PATH_A);
    assert_eq!(wire::read_u64(&mut h.stream).await?, STDERR_LAST);
    assert_eq!(wire::read_u64(&mut h.stream).await?, 1, "found");

    // Op 2: IsValidPath (not found)
    wire_send!(&mut h.stream; u64: 1, string: TEST_PATH_MISSING);
    assert_eq!(wire::read_u64(&mut h.stream).await?, STDERR_LAST);
    assert_eq!(wire::read_u64(&mut h.stream).await?, 0, "missing");

    // Op 3: AddTempRoot
    wire_send!(&mut h.stream; u64: 11, string: TEST_PATH_A);
    assert_eq!(wire::read_u64(&mut h.stream).await?, STDERR_LAST);
    assert_eq!(wire::read_u64(&mut h.stream).await?, 1);

    // Op 4: QueryPathFromHashPart (found — prefix match)
    wire_send!(&mut h.stream; u64: 29, string: "00000000000000000000000000000000");
    assert_eq!(wire::read_u64(&mut h.stream).await?, STDERR_LAST);
    let path = wire::read_string(&mut h.stream).await?;
    assert_eq!(path, TEST_PATH_A);

    h.finish().await;
    Ok(())
}
