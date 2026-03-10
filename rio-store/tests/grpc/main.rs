//! gRPC-level integration tests for StoreService.
//!
//! These tests spin up an in-process tonic server (inline storage or
//! [`MemoryChunkBackend`]) with an ephemeral PostgreSQL database
//! (bootstrapped by `rio-test-support`), then exercise the full gRPC
//! request/response path including streaming.

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

use rio_proto::StoreServiceClient;
use rio_proto::StoreServiceServer;
use rio_proto::types::{
    FindMissingPathsRequest, PathInfo, PutPathMetadata, PutPathRequest, PutPathTrailer,
    QueryPathInfoRequest, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;
use rio_store::backend::chunk::{ChunkBackend, MemoryChunkBackend};
use rio_store::grpc::StoreServiceImpl;
use rio_test_support::fixtures::{make_nar, make_path_info_for_nar, test_store_path};
use rio_test_support::{Context, TestDb, TestResult};

use std::sync::Arc;

// Can't use rio_store::MIGRATOR — it's cfg(test) in lib.rs (the
// rio-store/fuzz/ workspace's source filter excludes migrations/,
// and sqlx::migrate! reads files at compile time). Integration tests
// compile the lib without cfg(test), so we keep our own copy. Same
// migrations dir, same embedded SQL.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

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
    pub async fn new_with_signer(signer: rio_store::signing::Signer) -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = StoreServiceImpl::new(db.pool.clone()).with_signer(signer);
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
            backend.clone() as Arc<dyn ChunkBackend>
        ));
        let service = StoreServiceImpl::with_chunk_cache(db.pool.clone(), cache);
        let (client, server) = spawn_store_server(service).await?;
        Ok((Self { db, client, server }, backend))
    }
}

impl Drop for StoreSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Shared: spawn server + connect client.
async fn spawn_store_server(
    service: StoreServiceImpl,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let router = Server::builder().add_service(StoreServiceServer::new(service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;

    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    Ok((StoreServiceClient::new(channel), server))
}

/// Helper: upload a path via PutPath, sending metadata + one nar_chunk.
///
/// Takes `ValidatedPathInfo` (the common case from `make_path_info_for_nar`)
/// and converts to raw `PathInfo` internally. For tests that need to send
/// DELIBERATELY INVALID data (bad references, etc.) to exercise server-side
/// validation, use [`put_path_raw`] instead.
pub async fn put_path(
    client: &mut StoreServiceClient<Channel>,
    info: ValidatedPathInfo,
    nar: Vec<u8>,
) -> Result<bool, tonic::Status> {
    put_path_raw(client, info.into(), nar).await
}

/// Raw variant: takes unvalidated `PathInfo` directly. Use this to test
/// server-side rejection of malformed input (e.g., bad reference strings).
///
/// Trailer-mode: extracts `nar_hash`/`nar_size` from `info`, zeroes them
/// in the metadata, and sends them in a `PutPathTrailer`. (Hash-upfront
/// was deleted — store rejects non-empty metadata nar_hash.)
pub async fn put_path_raw(
    client: &mut StoreServiceClient<Channel>,
    mut info: PathInfo,
    nar: Vec<u8>,
) -> Result<bool, tonic::Status> {
    let (tx, rx) = mpsc::channel(8);

    // Extract hash/size for trailer, zero them in metadata.
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };

    // Send metadata first. Fresh channel, buffer=8, receiver alive:
    // these sends cannot fail.
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
        })),
    })
    .await
    .expect("fresh channel");

    // Send NAR data as one chunk
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .expect("fresh channel");

    // Send trailer
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer)),
    })
    .await
    .expect("fresh channel");

    // Close the stream
    drop(tx);

    let outbound = ReceiverStream::new(rx);
    let response = client.put_path(outbound).await?;
    Ok(response.into_inner().created)
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
        "x-rio-assignment-token",
        token.parse().expect("token must be valid header value"),
    );
    let response = client.put_path(req).await?;
    Ok(response.into_inner().created)
}

/// Build a NAR of roughly `payload_size` bytes. Content is pseudo-random
/// enough for FastCDC to find boundaries but deterministic for
/// reproducible tests.
///
/// Used by `chunked`, `reassembly`, and `chunk_service` — hence shared
/// in main.rs. NOT using make_nar (that wraps a tiny payload in NAR
/// framing). We want a big payload. The NAR framing adds ~100 bytes;
/// payload dominates.
fn make_large_nar(seed: u8, payload_size: usize) -> (Vec<u8>, ValidatedPathInfo, String) {
    // Deterministic pseudo-random payload. seed varies per test so two
    // NARs with different seeds share SOME chunks (dedup test) but not all.
    let payload: Vec<u8> = (0u64..payload_size as u64)
        .map(|i| (i.wrapping_mul(7919).wrapping_add(seed as u64) % 251) as u8)
        .collect();

    // Wrap in a NAR via the fixture helper (single-file NAR).
    let (nar, _hash) = make_nar(&payload);
    let store_path = test_store_path(&format!("large-nar-{seed}"));
    let info = make_path_info_for_nar(&store_path, &nar);
    (nar, info, store_path)
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
