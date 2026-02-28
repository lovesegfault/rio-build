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

mod build;
mod misc;
mod opcodes_read;
mod opcodes_write;
