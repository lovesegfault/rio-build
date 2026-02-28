#![allow(dead_code)] // helpers used by integration tests; Cargo compiles each test binary separately

use rio_gateway::session;
use rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient;
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_test_support::grpc::{MockScheduler, MockStore, spawn_mock_scheduler, spawn_mock_store};
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
    store_handle: tokio::task::JoinHandle<()>,
    sched_handle: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
}

impl GatewaySession {
    /// Spawn mocks + run_protocol. Ready to send opcodes on `.stream`.
    pub async fn new() -> Self {
        let (store, store_addr, store_handle) = spawn_mock_store().await;
        let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await;

        let store_client = rio_proto::client::connect_store(&store_addr.to_string())
            .await
            .expect("connect to mock store");
        let scheduler_client = rio_proto::client::connect_scheduler(&sched_addr.to_string())
            .await
            .expect("connect to mock scheduler");

        let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);
        let mut sc = store_client.clone();
        let mut scc = scheduler_client.clone();
        let server_task = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(server_stream);
            // UnexpectedEof is normal when the test drops its client stream.
            if let Err(e) = session::run_protocol(&mut r, &mut w, &mut sc, &mut scc).await
                && !e.to_string().contains("UnexpectedEof")
            {
                panic!("run_protocol failed: {e}");
            }
        });

        Self {
            stream: client_stream,
            store,
            scheduler,
            store_client,
            scheduler_client,
            store_handle,
            sched_handle,
            server_task,
        }
    }

    /// Await the server task (after client stream is dropped/EOF).
    pub async fn join_server(&mut self) {
        // Take ownership of the server_task by replacing with a dummy.
        let task = std::mem::replace(&mut self.server_task, tokio::spawn(async {}));
        task.await.expect("server task should not panic");
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
