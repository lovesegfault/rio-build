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
//! Mirrors `GatewaySession` at [`rio-gateway/tests/common/mod.rs`] for the
//! DuplexStream + `run_protocol` wiring, and `StoreSession` at
//! [`rio-store/tests/grpc/main.rs`] for the real-store spawn.

#![allow(dead_code)] // helpers used by sibling test modules; Cargo compiles each separately

use std::sync::Arc;

use rio_gateway::session;
use rio_proto::{StoreServiceClient, StoreServiceServer};
use rio_store::backend::chunk::{ChunkBackend, MemoryChunkBackend};
use rio_store::cas::ChunkCache;
use rio_store::grpc::StoreServiceImpl;
use rio_test_support::TestDb;
use rio_test_support::grpc::{MockScheduler, spawn_grpc_server, spawn_mock_scheduler};
use rio_test_support::wire::{do_handshake, send_set_options};
use tokio::io::DuplexStream;
use tonic::transport::{Channel, Server};

// rio-store's MIGRATOR is cfg(test) in lib.rs (the rio-store/fuzz/
// workspace's source filter excludes migrations/, and sqlx::migrate!
// reads files at compile time). Integration tests compile the lib
// without cfg(test), so we keep our own copy. Same migrations dir
// (workspace root), same embedded SQL. Mirrors rio-store/tests/grpc/main.rs:31.
// Path is relative to CARGO_MANIFEST_DIR (rio-gateway/).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Gateway wire protocol backed by REAL `rio-store` (ephemeral PG,
/// `StoreServiceImpl`) + `MockScheduler`. Functional-tier: catches bugs
/// `MockStore` hides (hash verify, NAR parse, reference integrity) without
/// the 2-5min VM cost.
///
/// Scheduler stays mocked: real scheduler needs leader election + worker
/// registration — too heavy for this tier. Tranche-2 scope.
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
    /// Chunk backend handle (when constructed via `new_chunked`). Tests can
    /// inspect chunk counts to prove NARs actually got chunked (not
    /// inline-blob shortcut).
    pub chunk_backend: Option<Arc<MemoryChunkBackend>>,
    store_handle: tokio::task::JoinHandle<()>,
    sched_handle: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
}

impl RioStack {
    /// Spawn real store (inline-only) + mock scheduler + `run_protocol`.
    /// Ready to send opcodes on `.stream`. Caller handshakes.
    ///
    /// Inline-only: all NARs go into `manifests.inline_blob` regardless
    /// of size. Most tranche-1 tests use this — chunking is a T4 concern.
    pub async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = StoreServiceImpl::new(db.pool.clone());
        Self::build(db, service, None).await
    }

    /// Like [`new`] but with a `MemoryChunkBackend`: NARs ≥ 256 KiB
    /// (`INLINE_THRESHOLD`) are FastCDC-chunked. Returns the backend so
    /// tests can assert chunk counts — proving NARs actually round-tripped
    /// through chunk+reassembly, not the inline-blob shortcut.
    ///
    /// T4 uses this: `wopAddMultipleToStore` → FastCDC chunk → PG manifest
    /// → `MemoryChunkBackend` → `wopNarFromPath` reassembly. MockStore's
    /// `HashMap::insert` → `HashMap::get` is byte-identical by construction;
    /// this is byte-identical by correctness.
    ///
    /// [`new`]: Self::new
    pub async fn new_chunked() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let service = StoreServiceImpl::with_chunk_cache(db.pool.clone(), cache);
        Self::build(db, service, Some(backend)).await
    }

    /// Handshake + `wopSetOptions` done. Ready for opcodes.
    pub async fn new_ready() -> anyhow::Result<Self> {
        let mut s = Self::new().await?;
        do_handshake(&mut s.stream).await?;
        send_set_options(&mut s.stream).await?;
        Ok(s)
    }

    /// Chunked variant + handshake + `wopSetOptions` done.
    pub async fn new_chunked_ready() -> anyhow::Result<Self> {
        let mut s = Self::new_chunked().await?;
        do_handshake(&mut s.stream).await?;
        send_set_options(&mut s.stream).await?;
        Ok(s)
    }

    /// Shared: spawn store server + scheduler mock + `run_protocol` task.
    async fn build(
        db: TestDb,
        service: StoreServiceImpl,
        chunk_backend: Option<Arc<MemoryChunkBackend>>,
    ) -> anyhow::Result<Self> {
        // Real store on ephemeral TCP (tonic has no in-process transport
        // for Server::builder — TcpListenerStream on 127.0.0.1:0 is the
        // standard pattern, same as spawn_mock_store).
        let router = Server::builder().add_service(StoreServiceServer::new(service));
        let (store_addr, store_handle) = spawn_grpc_server(router).await;

        let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await?;

        let store_client = rio_proto::client::connect_store(&store_addr.to_string()).await?;
        let sched_client = rio_proto::client::connect_scheduler(&sched_addr.to_string()).await?;

        let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);
        let mut sc = store_client.clone();
        let mut scc = sched_client.clone();
        // Fire-and-forget: aborted in Drop. Same EOF-is-clean /
        // error-is-logged-not-panicked discipline as GatewaySession.
        // Error-path opcode tests deliberately trigger STDERR_ERROR,
        // after which the handler returns Err — the test's real
        // assertion is the client-side drain_stderr_expecting_error.
        let server_task = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(server_stream);
            if let Err(e) =
                session::run_protocol(&mut r, &mut w, &mut sc, &mut scc, String::new()).await
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
            db,
            scheduler,
            store_client,
            chunk_backend,
            store_handle,
            sched_handle,
            server_task,
        })
    }

    /// Finish the session: drop the client stream (EOF), await server task.
    ///
    /// Consuming variant for the standard test teardown. `Drop` is a
    /// fallback (abort-only) for tests that return early via `?`.
    pub async fn finish(mut self) {
        // Drop the client stream to trigger EOF on the server side.
        // Struct has a Drop impl so can't `drop(self.stream)` directly;
        // replace with a dangling endpoint.
        self.stream = tokio::io::duplex(1).0;
        // Take the server_task out (Drop will abort a no-op dummy).
        let task = std::mem::replace(&mut self.server_task, tokio::spawn(async {}));
        task.await.expect("server task should not panic");
        // Drop runs here: aborts gRPC handles (TestDb's Drop drops the
        // PG database — happens after server_task joined, so no
        // store-query-on-dead-db race).
    }
}

impl Drop for RioStack {
    fn drop(&mut self) {
        self.store_handle.abort();
        self.sched_handle.abort();
        self.server_task.abort();
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

/// Build a NAR large enough to trigger FastCDC chunking (> 256 KiB
/// `INLINE_THRESHOLD`). Content is pseudo-random-enough for FastCDC
/// to find boundaries but deterministic for reproducible tests.
///
/// Same generator as `rio-store/tests/grpc/main.rs::make_large_nar` —
/// a 7919 (prime) multiplicative sequence mod 251. At 64 KiB average
/// chunk size, a 512 KiB NAR chunks into ~8 pieces.
pub fn make_large_nar(seed: u8, payload_size: usize) -> (Vec<u8>, [u8; 32]) {
    let payload: Vec<u8> = (0u64..payload_size as u64)
        .map(|i| (i.wrapping_mul(7919).wrapping_add(seed as u64) % 251) as u8)
        .collect();
    rio_test_support::fixtures::make_nar(&payload)
}
