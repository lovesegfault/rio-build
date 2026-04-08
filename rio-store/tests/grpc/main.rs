//! gRPC-level integration tests for StoreService.
//!
//! These tests spin up an in-process tonic server (inline storage or
//! [`MemoryChunkBackend`]) with an ephemeral PostgreSQL database
//! (bootstrapped by `rio-test-support`), then exercise the full gRPC
//! request/response path including streaming.

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

use anyhow::Context;
use rio_proto::StoreServiceClient;
use rio_proto::StoreServiceServer;
use rio_proto::types::{
    FindMissingPathsRequest, PathInfo, PutPathMetadata, PutPathRequest, PutPathTrailer,
    QueryPathInfoRequest, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;
use rio_store::MIGRATOR;
use rio_store::backend::chunk::{ChunkBackend, MemoryChunkBackend};
use rio_store::grpc::StoreServiceImpl;
// Re-export the shared helpers under their existing names so the test
// submodules' `use super::*;` keeps working unchanged.
pub use rio_store::test_helpers::{
    put_path, put_path_raw, spawn_store_service as spawn_store_server,
};
use rio_test_support::fixtures::{
    make_large_nar, make_nar, make_path_info_for_nar, pseudo_random_bytes, test_store_path,
};
use rio_test_support::{TestDb, TestResult};

use std::sync::Arc;

/// Test harness bundling the three things every gRPC integration test
/// needs: an ephemeral PG, a connected client, and a server handle.
///
/// `Drop` aborts the server — no more `server.abort()` boilerplate at
/// the end of every test, and no more "forgot to abort, server leaks
/// until process exit" footgun. The `db` field holds `TestDb` so its
/// Drop runs too (drops the PG database) — if we stored only
/// `db.pool.clone()`, `TestDb`'s Drop would fire immediately on return
/// from `new()` and the test's pool would point at a deleted DB.
///
/// Mirrors `GatewaySession` at rio-gateway/tests/common/mod.rs.
pub struct StoreSession {
    pub db: TestDb,
    pub client: StoreServiceClient<Channel>,
    server: tokio::task::JoinHandle<()>,
}

impl StoreSession {
    /// Inline-only store (no chunk backend). NARs of any size go into
    /// `manifests.inline_blob`. Most tests use this.
    pub async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = StoreServiceImpl::new(db.pool.clone());
        let (client, server) = spawn_store_server(service).await?;
        Ok(Self { db, client, server })
    }

    /// Store with a signing key. For narinfo signing tests.
    ///
    /// Takes a cluster `Signer` and wraps it in `TenantSigner` internally
    /// (same as main.rs). Callers testing cluster-key-only behavior
    /// don't notice the difference: `resolve_once(None)` routes
    /// to the cluster key without touching the pool. Callers testing
    /// per-tenant keys seed `tenant_keys` on `s.db.pool` first.
    pub async fn new_with_signer(signer: rio_store::signing::Signer) -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let ts = rio_store::signing::TenantSigner::new(signer, db.pool.clone());
        let service = StoreServiceImpl::new(db.pool.clone()).with_signer(ts);
        let (client, server) = spawn_store_server(service).await?;
        Ok(Self { db, client, server })
    }

    /// Store with HMAC verifier enabled. PutPath requires a valid
    /// `x-rio-assignment-token` header unless peer is mTLS-identified
    /// as rio-gateway. For testing the assignment-token enforcement.
    pub async fn new_with_hmac(key: Vec<u8>) -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let verifier = rio_common::hmac::HmacVerifier::from_key(key);
        let service = StoreServiceImpl::new(db.pool.clone()).with_hmac_verifier(verifier);
        let (client, server) = spawn_store_server(service).await?;
        Ok(Self { db, client, server })
    }

    /// Store WITH chunk backend. NARs ≥ 256 KiB are FastCDC-chunked.
    /// Returns the `MemoryChunkBackend` so tests can inspect chunk counts.
    pub async fn new_chunked() -> anyhow::Result<(Self, Arc<MemoryChunkBackend>)> {
        let db = TestDb::new(&MIGRATOR).await;
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(rio_store::cas::ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let service = StoreServiceImpl::new(db.pool.clone()).with_chunk_cache(cache);
        let (client, server) = spawn_store_server(service).await?;
        Ok((Self { db, client, server }, backend))
    }
}

impl Drop for StoreSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Like [`put_path`] but sets the `x-rio-assignment-token` metadata
/// header. For HMAC enforcement tests — `put_path_raw` passes the
/// stream directly (no `Request::new()` exposed to attach headers).
pub async fn put_path_with_token(
    client: &mut StoreServiceClient<Channel>,
    info: ValidatedPathInfo,
    nar: Vec<u8>,
    token: &str,
) -> Result<bool, tonic::Status> {
    let mut info: PathInfo = info.into();
    let (tx, rx) = mpsc::channel(8);
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
        })),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer)),
    })
    .await
    .expect("fresh channel");
    drop(tx);

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    req.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        token.parse().expect("token must be valid header value"),
    );
    let response = client.put_path(req).await?;
    Ok(response.into_inner().created)
}

// ===========================================================================
// Test submodules (each `use super::*;`)
// ===========================================================================

mod chunk_service;
mod chunked;
mod core;
mod hash_part;
mod hmac;
mod realisations;
mod reassembly;
mod signing;
mod trailer;
