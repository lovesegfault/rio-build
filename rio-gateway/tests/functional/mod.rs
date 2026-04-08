//! `RioStack` — functional-tier test fixture.
//!
//! Gateway wire protocol backed by REAL `rio-store` (`StoreServiceImpl` +
//! ephemeral PostgreSQL) instead of `MockStore`. Catches bugs `MockStore`
//! hides — hash verification, NAR parse, reference integrity — without the
//! 2-5min VM cost.
//!
//! Lives here (not in `rio-test-support`) to avoid a dep cycle:
//! `rio-store` dev-depends on `rio-test-support`, so
//! `rio-test-support → rio-store` would cycle.
//!
//! Shares `DuplexStream` + `run_protocol` wiring with `GatewaySession` via
//! [`super::common::spawn_session_task`] / [`super::common::SessionHandles`];
//! mirrors `StoreSession` at [`rio-store/tests/grpc/main.rs`] for the
//! real-store spawn.

#![allow(dead_code)] // helpers used by sibling test modules; Cargo compiles each separately

use std::sync::Arc;

use rio_proto::StoreServiceClient;
use rio_store::backend::{ChunkBackend, MemoryChunkBackend};
use rio_store::cas::ChunkCache;
use rio_store::grpc::StoreServiceImpl;
use rio_store::test_helpers::spawn_store_service;
use rio_test_support::TestDb;
use rio_test_support::grpc::{MockScheduler, spawn_mock_scheduler};
use rio_test_support::wire::{do_handshake, send_set_options};
use tokio::io::DuplexStream;
use tonic::transport::Channel;

use super::common::{SessionHandles, spawn_session_task};

pub use rio_store::MIGRATOR;

/// Gateway wire protocol backed by REAL `rio-store` (ephemeral PG,
/// `StoreServiceImpl`) + `MockScheduler`. Functional-tier: catches bugs
/// `MockStore` hides (hash verify, NAR parse, reference integrity) without
/// the 2-5min VM cost.
///
/// Construct via [`RioStack::ready()`] or [`RioStack::ready_chunked()`].
/// Scheduler is always mocked.
pub struct RioStack {
    /// Client-side wire stream. Write opcodes, read responses.
    pub stream: DuplexStream,
    /// Direct DB access for white-box assertions (check `narinfo` table,
    /// verify references column — things the wire protocol doesn't expose).
    pub db: TestDb,
    /// Scheduler mock — set build outcomes, inspect `SubmitBuild` calls.
    pub scheduler: MockScheduler,
    /// Direct store gRPC client — bypass the wire protocol for setup
    /// (seed paths) or for assertions gateway doesn't expose.
    pub store_client: StoreServiceClient<Channel>,
    handles: SessionHandles,
}

impl RioStack {
    /// Spawn + handshake + `wopSetOptions`. Inline-only store (no chunking).
    /// Ready for opcodes.
    pub async fn ready() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = StoreServiceImpl::new(db.pool.clone());
        Self::build_inner(db, service).await
    }

    /// Like [`ready()`](Self::ready) but with FastCDC chunking — NARs ≥
    /// `INLINE_THRESHOLD` go through chunk+reassembly. Returns the backend
    /// so tests can assert chunk counts, proving NARs round-tripped through
    /// chunk→PG manifest→reassembly, not the inline-blob shortcut.
    pub async fn ready_chunked() -> anyhow::Result<(Self, Arc<MemoryChunkBackend>)> {
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let db = TestDb::new(&MIGRATOR).await;
        let service = StoreServiceImpl::new(db.pool.clone()).with_chunk_cache(cache);
        Ok((Self::build_inner(db, service).await?, backend))
    }

    /// Shared spawn mechanics: store server + scheduler mock +
    /// `run_protocol` task, then handshake + `wopSetOptions`.
    async fn build_inner(db: TestDb, service: StoreServiceImpl) -> anyhow::Result<Self> {
        // Idempotent — captured per-test via with_test_writer.
        // Without this, tracing::debug! in run_protocol error paths is void.
        rio_test_support::init_test_logging("rio_gateway=debug,rio_nix=debug,rio_store=debug");
        // Real store on ephemeral TCP (tonic has no in-process transport
        // for Server::builder — TcpListenerStream on 127.0.0.1:0 is the
        // standard pattern, same as spawn_mock_store).
        //
        // `?` on the next 3 lines detaches store_handle (JoinHandle::drop
        // doesn't abort). Test-only + connect-to-127.0.0.1-just-spawned
        // rarely fails + process-exit reaps. If mass-connect-failures ever
        // exhaust ephemeral ports: scopeguard::guard or AbortOnDrop wrapper.
        // Same pattern at rio-test-support/src/grpc.rs spawn_mock_store_with_client.
        let (store_client, store_handle) = spawn_store_service(service).await?;
        let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await?;

        let sched_client = rio_proto::client::connect_single(&sched_addr.to_string()).await?;

        // Functional tests don't exercise the shutdown-signal or tenant
        // paths — those are wire_opcodes concerns. Never-cancelled token,
        // single-tenant mode.
        let (stream, server_task) = spawn_session_task(
            store_client.clone(),
            sched_client,
            None,
            rio_common::signal::Token::new(),
        );

        let mut stack = Self {
            stream,
            db,
            scheduler,
            store_client,
            handles: SessionHandles::new(store_handle, sched_handle, server_task),
        };
        do_handshake(&mut stack.stream).await?;
        send_set_options(&mut stack.stream).await?;
        Ok(stack)
    }

    /// Finish the session: drop the client stream (EOF), await server task.
    ///
    /// Consuming variant for the standard test teardown.
    /// [`SessionHandles`]'s `Drop` is the abort-only fallback for tests
    /// that return early via `?`. `TestDb`'s `Drop` drops the PG database
    /// after `server_task` has joined — no store-query-on-dead-db race.
    pub async fn finish(mut self) {
        // Drop the client stream to trigger EOF on the server side
        // (replace with a dangling endpoint — can't move out of a field).
        self.stream = tokio::io::duplex(1).0;
        self.handles.join_server().await;
    }
}

// ===========================================================================
// Shared helpers for functional test modules
// ===========================================================================

use rio_nix::protocol::wire;
use rio_test_support::wire::drain_stderr_until_last;
use rio_test_support::wire_send;

/// Send `wopAddToStoreNar` (39) for one path. Returns after `STDERR_LAST`.
///
/// `references` is the non-empty-capable variant — every `wire_opcodes`
/// test sends `NO_STRINGS` here; T5 sends real chains.
pub async fn add_to_store_nar(
    s: &mut DuplexStream,
    path: &str,
    nar: &[u8],
    nar_hash: [u8; 32],
    references: &[&str],
) -> anyhow::Result<()> {
    wire_send!(s;
        u64: 39,                           // wopAddToStoreNar
        string: path,
        string: "",                        // deriver
        string: &hex::encode(nar_hash),    // narHash — real store VERIFIES
        strings: references,
        u64: 0,                            // registrationTime
        u64: nar.len() as u64,             // narSize
        bool: false,                       // ultimate
        strings: wire::NO_STRINGS,         // sigs
        string: "",                        // ca
        bool: false, bool: true,           // repair, dontCheckSigs
        framed: nar,
    );
    drain_stderr_until_last(s).await?;
    Ok(())
}

pub use rio_test_support::fixtures::make_large_nar;
