#![allow(dead_code)] // helpers used by integration tests; Cargo compiles each test binary separately

use rio_common::signal::Token as CancellationToken;
use rio_common::tenant::NormalizedName;
use rio_gateway::session;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use rio_test_support::grpc::{MockScheduler, MockStore, spawn_mock_scheduler, spawn_mock_store};
use rio_test_support::wire::{do_handshake, send_set_options};
use tokio::io::DuplexStream;
use tokio::task::JoinHandle;
use tonic::transport::Channel;

/// Spawn `run_protocol` on the server side of a fresh `DuplexStream`.
/// Returns the client side + the protocol task's join handle.
///
/// EOF on the client side is treated as clean shutdown. Non-EOF errors are
/// LOGGED, not panicked: error-path opcode tests deliberately trigger
/// `STDERR_ERROR`, after which the handler returns Err to close the
/// connection (per protocol spec). The test's real assertion is the
/// client-side `drain_stderr_expecting_error` check, not the server return.
///
/// Parametric over the store client — `GatewaySession` passes a
/// `MockStore`-backed client, `RioStack` (functional tier) passes a real
/// `StoreServiceImpl`-backed one.
pub fn spawn_session_task(
    mut store_client: StoreServiceClient<Channel>,
    mut sched_client: SchedulerServiceClient<Channel>,
    tenant: Option<NormalizedName>,
    shutdown: CancellationToken,
) -> (DuplexStream, JoinHandle<()>) {
    let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(server_stream);
        if let Err(e) = session::run_protocol(
            &mut r,
            &mut w,
            &mut store_client,
            &mut sched_client,
            tenant,
            None,
            rio_gateway::TenantLimiter::disabled(),
            rio_gateway::QuotaCache::new(),
            shutdown,
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
    (client_stream, server_task)
}

/// Join handles for the two gRPC mock servers + the `run_protocol` task.
/// `Drop` aborts all three (fallback for tests that return early via `?`);
/// [`join_server`] awaits the protocol task cleanly for the standard
/// end-of-test teardown.
///
/// Embedded by both `GatewaySession` (mock store) and `RioStack` (real
/// store) — the outer struct's own `Drop` is unnecessary since dropping
/// the field runs this `Drop`.
///
/// [`join_server`]: Self::join_server
pub struct SessionHandles {
    store_handle: JoinHandle<()>,
    sched_handle: JoinHandle<()>,
    server_task: JoinHandle<()>,
}

impl SessionHandles {
    pub fn new(
        store_handle: JoinHandle<()>,
        sched_handle: JoinHandle<()>,
        server_task: JoinHandle<()>,
    ) -> Self {
        Self {
            store_handle,
            sched_handle,
            server_task,
        }
    }

    /// Await the server task (after client stream is dropped/EOF).
    /// Aborting a finished handle in `Drop` afterwards is a no-op.
    pub async fn join_server(&mut self) {
        let task = std::mem::replace(&mut self.server_task, tokio::spawn(async {}));
        task.await.expect("server task should not panic");
    }
}

impl Drop for SessionHandles {
    fn drop(&mut self) {
        self.store_handle.abort();
        self.sched_handle.abort();
        self.server_task.abort();
    }
}

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
    handles: SessionHandles,
}

impl GatewaySession {
    /// Spawn mocks + run_protocol. Ready to send opcodes on `.stream`.
    /// Caller handshakes. For tests that start from a ready session,
    /// use [`new_with_handshake`] instead.
    ///
    /// [`new_with_handshake`]: Self::new_with_handshake
    pub async fn new() -> anyhow::Result<Self> {
        Self::new_with_tenant("").await
    }

    /// Like [`new`] but with an explicit `tenant_name`. Empty string =
    /// single-tenant mode (same as [`new`]). Non-empty enables the
    /// per-tenant rate-limit and quota checks in the build handlers —
    /// tests seed [`MockStore::tenant_quotas`] to drive the quota gate.
    ///
    /// [`new`]: Self::new
    pub async fn new_with_tenant(tenant_name: &str) -> anyhow::Result<Self> {
        // Idempotent (try_init); output captured per-test via
        // with_test_writer, shown only on failure. Without this,
        // tracing::debug! in run_protocol error-log paths is void.
        init_test_logging();
        let (store, store_addr, store_handle) = spawn_mock_store().await?;
        let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await?;

        let store_client = rio_proto::client::connect_store(&store_addr.to_string()).await?;
        let scheduler_client =
            rio_proto::client::connect_scheduler(&sched_addr.to_string()).await?;

        // Normalize at the test boundary — same path as production
        // (auth_publickey does the same from_maybe_empty on the
        // authorized_keys comment). Empty string → None (single-
        // tenant mode); non-empty → Some(NormalizedName).
        let tenant = NormalizedName::from_maybe_empty(tenant_name);
        let shutdown = CancellationToken::new();
        let (stream, server_task) = spawn_session_task(
            store_client.clone(),
            scheduler_client.clone(),
            tenant,
            shutdown.child_token(),
        );

        Ok(Self {
            stream,
            store,
            scheduler,
            store_client,
            scheduler_client,
            shutdown,
            handles: SessionHandles::new(store_handle, sched_handle, server_task),
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

    /// Handshake + wopSetOptions with an explicit `tenant_name`. Used
    /// by quota/rate-limit tests that need a non-empty tenant to
    /// trigger the per-tenant gates.
    pub async fn new_with_tenant_handshake(tenant_name: &str) -> anyhow::Result<Self> {
        let mut sess = Self::new_with_tenant(tenant_name).await?;
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
        self.handles.join_server().await;
    }

    /// Finish the session: drop the client stream (EOF), await server task.
    ///
    /// Consuming variant for the standard opcode-test teardown.
    /// [`SessionHandles`]'s `Drop` is the abort-only fallback for tests
    /// that return early via `?`.
    pub async fn finish(mut self) {
        // Drop the client stream to trigger EOF on the server side
        // (replace with a dangling endpoint — can't move out of a field).
        self.stream = tokio::io::duplex(1).0;
        self.handles.join_server().await;
    }
}

/// Initialize test logging (idempotent).
pub fn init_test_logging() {
    rio_test_support::init_test_logging("rio_gateway=debug,rio_nix=debug");
}
