#![allow(dead_code)] // helpers used by integration tests; Cargo compiles each test binary separately

use rio_common::signal::Token as CancellationToken;
use rio_gateway::session;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use rio_test_support::grpc::{MockScheduler, MockStore, spawn_mock_scheduler, spawn_mock_store};
use rio_test_support::wire::{do_handshake, send_set_options};
use tokio::io::DuplexStream;
use tonic::transport::Channel;

/// All-in-one gateway test session: spawns mock gRPC servers, connects
/// clients, creates a DuplexStream, and runs `run_protocol` on the server
/// side. EOF on client side is treated as clean shutdown.
pub struct GatewaySession {
    /// Client-side stream (write requests, read responses).
    pub stream: DuplexStream,
    /// Mock store (pre-seed paths, inspect put_calls).
    pub store: MockStore,
    /// Mock scheduler (set outcome, inspect submit_calls).
    pub scheduler: MockScheduler,
    /// Store gRPC client (for direct queries bypassing the protocol).
    pub store_client: StoreServiceClient<Channel>,
    /// Scheduler gRPC client.
    pub scheduler_client: SchedulerServiceClient<Channel>,
    /// Graceful-shutdown token passed to `run_protocol`. Tests fire
    /// this to exercise the `channel_close → Drop` path without needing
    /// a real russh handler stack. Compare: dropping `.stream` exercises
    /// the mpsc-EOF path; firing `.shutdown` exercises the token path.
    pub shutdown: CancellationToken,
    store_handle: tokio::task::JoinHandle<()>,
    sched_handle: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
}

impl GatewaySession {
    /// Spawn mocks + run_protocol. Ready to send opcodes on `.stream`.
    /// Caller handshakes. For tests that start from a ready session,
    /// use [`new_with_handshake`] instead.
    ///
    /// [`new_with_handshake`]: Self::new_with_handshake
    pub async fn new() -> anyhow::Result<Self> {
        let (store, store_addr, store_handle) = spawn_mock_store().await?;
        let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await?;

        let store_client = rio_proto::client::connect_store(&store_addr.to_string()).await?;
        let scheduler_client =
            rio_proto::client::connect_scheduler(&sched_addr.to_string()).await?;

        let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);
        let mut sc = store_client.clone();
        let mut scc = scheduler_client.clone();
        let shutdown = CancellationToken::new();
        let shutdown_child = shutdown.child_token();
        // Fire-and-forget: aborted in Drop or awaited in finish()/join_server().
        //
        // IMPORTANT: non-EOF errors are LOGGED, not panicked. Error-path
        // opcode tests deliberately trigger STDERR_ERROR, after which the
        // handler returns Err to close the connection (per protocol spec).
        // The test's real assertion is the client-side
        // `drain_stderr_expecting_error` check, not the server return.
        let server_task = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(server_stream);
            if let Err(e) = session::run_protocol(
                &mut r,
                &mut w,
                &mut sc,
                &mut scc,
                String::new(),
                None,
                rio_gateway::TenantLimiter::disabled(),
                shutdown_child,
            )
            .await
            {
                let is_eof = e
                    .downcast_ref::<rio_nix::protocol::wire::WireError>()
                    .is_some_and(|we| {
                        matches!(we, rio_nix::protocol::wire::WireError::Io(io)
                            if io.kind() == std::io::ErrorKind::UnexpectedEof)
                    });
                if !is_eof {
                    tracing::debug!(error = %e, "run_protocol returned error (expected for error-path tests)");
                }
            }
        });

        Ok(Self {
            stream: client_stream,
            store,
            scheduler,
            store_client,
            scheduler_client,
            shutdown,
            store_handle,
            sched_handle,
            server_task,
        })
    }

    /// Like [`new`] but also performs handshake + sends wopSetOptions.
    /// Use this for opcode tests that start from a ready session.
    ///
    /// [`new`]: Self::new
    pub async fn new_with_handshake() -> anyhow::Result<Self> {
        let mut sess = Self::new().await?;
        do_handshake(&mut sess.stream).await?;
        send_set_options(&mut sess.stream).await?;
        Ok(sess)
    }

    /// Await the server task (after client stream is dropped/EOF).
    /// Borrowing variant — leaves `self` usable for post-join assertions.
    /// For the common "end of test" pattern prefer [`finish`].
    ///
    /// [`finish`]: Self::finish
    pub async fn join_server(&mut self) {
        // Take ownership of the server_task by replacing with a dummy.
        let task = std::mem::replace(&mut self.server_task, tokio::spawn(async {}));
        task.await.expect("server task should not panic");
    }

    /// Finish the session: drop the client stream (EOF), await server task.
    ///
    /// Consuming variant for the standard opcode-test teardown. The `Drop`
    /// impl is a fallback (abort-only) for tests that return early via `?`.
    pub async fn finish(mut self) {
        // Drop the client stream to trigger EOF on the server side. We
        // can't `drop(self.stream)` directly (struct has a Drop impl),
        // so replace it with a dangling endpoint.
        self.stream = tokio::io::duplex(1).0;
        self.join_server().await;
        // Drop runs here and aborts the gRPC handles (server_task already
        // joined — abort on a finished handle is a no-op).
    }
}

impl Drop for GatewaySession {
    fn drop(&mut self) {
        self.store_handle.abort();
        self.sched_handle.abort();
        self.server_task.abort();
    }
}

/// Initialize test logging (idempotent).
pub fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rio_gateway=debug,rio_nix=debug")
        .with_test_writer()
        .try_init();
}
