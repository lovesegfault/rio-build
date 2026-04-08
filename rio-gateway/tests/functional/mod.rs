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
use rio_store::backend::chunk::{ChunkBackend, MemoryChunkBackend};
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
/// Construct via [`RioStackBuilder`] — scheduler is mocked. A real-
/// `SchedulerActor` axis (leader-election stub + worker-registration
/// mock) was scoped for tranche-2 but no plan materialized; re-add
/// the builder seam when that work is scheduled.
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

/// Builder for [`RioStack`]. Two orthogonal axes:
/// - **chunked:** inline-only (default) vs FastCDC + [`MemoryChunkBackend`]
/// - **ready:** bare stream vs handshake + `wopSetOptions` done
///
/// The chunk backend is returned from [`with_chunked()`], not stored on
/// `RioStack` — tests that don't chunk don't carry an `Option::None`.
///
/// A third axis (real `SchedulerActor` vs mock) was seam-cut in P0318
/// for a projected tranche-2 scenario suite that never materialized.
/// The dead `with_real_scheduler()` + `unimplemented!()` branch was
/// removed; re-add when a plan owns the work.
///
/// [`with_chunked()`]: Self::with_chunked
#[derive(Default)]
pub struct RioStackBuilder {
    // Some(cache) iff with_chunked() was called. The backend Arc is
    // already returned to the caller; we keep only the wrapped
    // ChunkCache for StoreServiceImpl::with_chunk_cache.
    chunk_cache: Option<Arc<ChunkCache>>,
}

impl RioStackBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use FastCDC chunking (NARs ≥ `INLINE_THRESHOLD` go through
    /// chunk+reassembly). Returns the backend so tests can assert
    /// chunk counts — proving NARs round-tripped through chunk→PG
    /// manifest→reassembly, not the inline-blob shortcut.
    ///
    /// Chainable: `let (builder, backend) = RioStackBuilder::new().with_chunked();`
    pub fn with_chunked(mut self) -> (Self, Arc<MemoryChunkBackend>) {
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        self.chunk_cache = Some(cache);
        (self, backend)
    }

    /// Spawn the stack. Stream is BARE — caller handshakes.
    pub async fn build(self) -> anyhow::Result<RioStack> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = match self.chunk_cache {
            Some(cache) => StoreServiceImpl::with_chunk_cache(db.pool.clone(), cache),
            None => StoreServiceImpl::new(db.pool.clone()),
        };
        RioStack::build_inner(db, service).await
    }

    /// Spawn + handshake + `wopSetOptions`. Ready for opcodes.
    pub async fn ready(self) -> anyhow::Result<RioStack> {
        let mut stack = self.build().await?;
        do_handshake(&mut stack.stream).await?;
        send_set_options(&mut stack.stream).await?;
        Ok(stack)
    }
}

impl RioStack {
    /// Shared: spawn store server + scheduler mock + `run_protocol` task.
    /// Called by [`RioStackBuilder::build`] — the builder owns axis state
    /// (chunked, scheduler); this owns spawn mechanics.
    async fn build_inner(db: TestDb, service: StoreServiceImpl) -> anyhow::Result<Self> {
        // Idempotent (try_init) — captured per-test via with_test_writer.
        // Without this, tracing::debug! in run_protocol error paths is void.
        let _ = tracing_subscriber::fmt()
            .with_env_filter("rio_gateway=debug,rio_nix=debug,rio_store=debug")
            .with_test_writer()
            .try_init();
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

        let sched_client = rio_proto::client::connect_scheduler(&sched_addr.to_string()).await?;

        // Functional tests don't exercise the shutdown-signal or tenant
        // paths — those are wire_opcodes concerns. Never-cancelled token,
        // single-tenant mode.
        let (stream, server_task) = spawn_session_task(
            store_client.clone(),
            sched_client,
            None,
            rio_common::signal::Token::new(),
        );

        Ok(Self {
            stream,
            db,
            scheduler,
            store_client,
            handles: SessionHandles::new(store_handle, sched_handle, server_task),
        })
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
