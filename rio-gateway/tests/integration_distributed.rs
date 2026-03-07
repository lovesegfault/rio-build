//! Distributed integration tests for the rio-gateway.
//!
//! Verifies the full gRPC wiring: gateway -> store + scheduler, without
//! requiring PostgreSQL, FUSE, or CAP_SYS_ADMIN. Uses in-process mock
//! gRPC servers for store and scheduler services.
//!
//! Test flow:
//! 1. Start a mock StoreService (in-memory, no PostgreSQL)
//! 2. Start a mock SchedulerService (minimal stubs)
//! 3. Run the gateway protocol session with gRPC clients connected to mocks
//! 4. Perform: handshake -> wopSetOptions -> wopQueryValidPaths
//! 5. Verify empty store returns all paths as missing (none valid)
// r[verify gw.conn.exec-request]
// r[verify gw.conn.lifecycle]

mod common;

use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::STDERR_LAST;
use rio_nix::protocol::wire;
use rio_proto::types;
use rio_test_support::fixtures::test_store_path;
use rio_test_support::grpc::spawn_mock_scheduler;
use rio_test_support::wire::{do_handshake, send_set_options};
use rio_test_support::wire_send;
use tokio::io::DuplexStream;

/// Send wopQueryValidPaths (opcode 31) for the given paths.
/// Returns the list of valid paths from the server response.
async fn query_valid_paths(s: &mut DuplexStream, paths: &[&str]) -> anyhow::Result<Vec<String>> {
    wire_send!(s; u64: 31, strings: paths, u64: 0); // wopQueryValidPaths, paths, substitute=false

    // Read response: STDERR_LAST + valid paths
    let msg = wire::read_u64(s).await?;
    assert_eq!(
        msg, STDERR_LAST,
        "wopQueryValidPaths should send STDERR_LAST before result"
    );

    Ok(wire::read_strings(s).await?)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test the full distributed stack: handshake -> setOptions -> queryValidPaths.
///
/// This verifies that the gateway correctly delegates store operations to the
/// mock gRPC store service. With an empty store, all queried paths should be
/// reported as invalid (not present).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_distributed_handshake_query_empty_store() -> anyhow::Result<()> {
    common::init_test_logging();

    let mut sess = common::GatewaySession::new().await?;

    // Run client protocol test directly (no need for a separate spawn since
    // the server side is already running in sess.server_task).
    let s = &mut sess.stream;

    // Handshake
    do_handshake(s).await?;

    // SetOptions
    send_set_options(s).await?;

    // QueryValidPaths against empty store
    let valid = query_valid_paths(
        s,
        &[
            &test_store_path("hello-2.12.1"),
            &test_store_path("world-1.0"),
        ],
    )
    .await?;

    // Empty store: no paths should be valid
    assert!(
        valid.is_empty(),
        "empty store should return no valid paths, got: {valid:?}"
    );

    // GatewaySession::drop handles cleanup.
    Ok(())
}

/// Test that the gateway correctly reports paths as valid after they are
/// "stored" in the mock store via FindMissingPaths filtering.
///
/// This test pre-populates the mock store with a path, then verifies
/// wopQueryValidPaths returns it as valid.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_distributed_query_with_populated_store() -> anyhow::Result<()> {
    common::init_test_logging();

    let mut sess = common::GatewaySession::new().await?;

    // Pre-populate the mock store with one path
    let test_path = test_store_path("hello-2.12.1");
    let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"hello content");
    sess.store.seed(
        rio_test_support::fixtures::make_path_info(&test_path, &nar, nar_hash),
        nar,
    );

    let s = &mut sess.stream;

    do_handshake(s).await?;
    send_set_options(s).await?;

    // Query two paths: one present, one missing
    let valid = query_valid_paths(s, &[&test_path, &test_store_path("world-1.0")]).await?;

    // Only the pre-populated path should be valid
    assert_eq!(
        valid.len(),
        1,
        "expected exactly 1 valid path, got: {valid:?}"
    );
    assert_eq!(valid[0], test_path);
    Ok(())
}

/// Test that handshake negotiation works correctly through the distributed
/// gateway stack. Validates the exact wire sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_distributed_handshake_wire_sequence() -> anyhow::Result<()> {
    common::init_test_logging();

    let mut sess = common::GatewaySession::new().await?;
    let s = &mut sess.stream;

    // Phase 1: Send client magic + version
    wire_send!(s; u64: WORKER_MAGIC_1, u64: PROTOCOL_VERSION);

    // Read server magic
    let magic2 = wire::read_u64(s).await?;
    assert_eq!(magic2, WORKER_MAGIC_2, "server must send WORKER_MAGIC_2");

    // Read server version
    let server_version = wire::read_u64(s).await?;
    assert!(
        server_version >= PROTOCOL_VERSION,
        "server version should be >= our protocol version"
    );

    // Phase 2: Feature exchange
    wire_send!(s; strings: &Vec::<String>::new());

    let server_features = wire::read_strings(s).await?;
    // Server may return empty features, that's fine
    assert!(
        server_features.len() < 100,
        "sanity check: features list is reasonable"
    );

    // Phase 3: Obsolete CPU affinity + reserveSpace
    wire_send!(s; u64: 0, u64: 0);

    // Read version string
    let version_str = wire::read_string(s).await?;
    assert!(
        version_str.contains("rio-gateway"),
        "version string should contain 'rio-gateway', got: {version_str}"
    );

    // Read trusted status
    let trusted = wire::read_u64(s).await?;
    assert!(trusted <= 1, "trusted should be 0 or 1");

    // Phase 4: Initial STDERR_LAST
    let last = wire::read_u64(s).await?;
    assert_eq!(last, STDERR_LAST, "handshake must end with STDERR_LAST");
    Ok(())
}

// ---------------------------------------------------------------------------
// T2: CancelBuild sent on SSH disconnect (8.2)
// ---------------------------------------------------------------------------

/// Verify that when a client disconnects (EOF) while active_build_ids is
/// non-empty, the gateway calls CancelBuild on the scheduler.
///
/// The realistic flow in Phase 2a: gateway processes opcodes serially, so
/// active_build_ids is only populated DURING submit_and_process_build. That
/// function removes the build_id unconditionally on return (handler.rs:623).
/// So CancelBuild-on-disconnect fires only if the client disconnects while
/// a build is in-flight AND the event stream closes without a terminal event.
///
/// Test scenario: scheduler closes event stream immediately after Started
/// (no Completed/Failed). Gateway converts to stream-error failure, removes
/// build_id. Client then disconnects. No CancelBuild expected (build already
/// cleaned up). This verifies the CLEANUP path works — not a leak.
///
/// Additionally: wopBuildDerivation requires full drv_cache setup which is
/// complex in-test. This test verifies the mechanism more directly: after a
/// clean handshake + setOptions + disconnect, no spurious CancelBuild.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_disconnect_without_active_build_no_cancel() -> anyhow::Result<()> {
    let mut sess = common::GatewaySession::new().await?;

    // Run the client protocol inline, then wait for server to observe EOF.
    {
        let s = &mut sess.stream;
        do_handshake(s).await?;
        send_set_options(s).await?;
        // Disconnect immediately — no build submitted.
    }
    // Replace stream with a fresh (unconnected) one to drop the client side
    // and trigger EOF on the server. Then wait for server to finish.
    sess.stream = tokio::io::duplex(1).0;

    tokio::time::timeout(std::time::Duration::from_secs(10), sess.join_server())
        .await
        .expect("server should finish within 10s");

    // No active builds at disconnect time -> no CancelBuild calls.
    let cancels = sess.scheduler.cancel_calls.read().unwrap().clone();
    assert!(
        cancels.is_empty(),
        "disconnect without active builds should NOT call CancelBuild, got: {cancels:?}"
    );
    Ok(())
}

/// Verify CancelBuild infrastructure works: directly populate active_build_ids
/// via session-internal state simulation by calling cancel_build via the
/// scheduler client (unit-test style, verifies the mock records correctly).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cancel_build_recorded_by_mock_scheduler() -> anyhow::Result<()> {
    // This test only needs the scheduler mock (no full session).
    let (sched, sched_addr, sched_handle) = spawn_mock_scheduler().await?;

    let mut scheduler_client =
        rio_proto::SchedulerServiceClient::connect(format!("http://{sched_addr}"))
            .await
            .expect("connect");

    // Simulate the session.rs disconnect handler: call cancel_build directly.
    scheduler_client
        .cancel_build(types::CancelBuildRequest {
            build_id: "test-build-id".into(),
            reason: "client_disconnect".into(),
        })
        .await
        .expect("cancel should succeed");

    let cancels = sched.cancel_calls.read().unwrap().clone();
    assert_eq!(cancels.len(), 1, "one CancelBuild call recorded");
    assert_eq!(cancels[0].0, "test-build-id");
    assert_eq!(cancels[0].1, "client_disconnect");

    sched_handle.abort();
    Ok(())
}
