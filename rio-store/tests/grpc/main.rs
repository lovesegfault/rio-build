//! gRPC-level integration tests for StoreService.
//!
//! These tests spin up an in-process tonic server (inline storage or
//! [`MemoryChunkBackend`]) with an ephemeral PostgreSQL database
//! (bootstrapped by `rio-test-support`), then exercise the full gRPC
//! request/response path including streaming.

use sqlx::PgPool;
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

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Spawn an in-process store gRPC server (inline-only) and return a client.
///
/// Phase 2c: no chunk backend → all NARs go inline regardless of size.
/// Most tests use this; chunked-specific tests use `setup_store_chunked`.
pub async fn setup_store(
    pool: PgPool,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let service = StoreServiceImpl::new(pool);
    spawn_store_server(service).await
}

/// Spawn a store with a signing key. For B2's signing tests.
pub async fn setup_store_with_signer(
    pool: PgPool,
    signer: rio_store::signing::Signer,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let service = StoreServiceImpl::new(pool).with_signer(signer);
    spawn_store_server(service).await
}

/// Spawn an in-process store WITH chunk backend. NARs ≥ 256 KiB are chunked.
/// Returns the `Arc<MemoryChunkBackend>` so tests can inspect chunk counts.
pub async fn setup_store_chunked(
    pool: PgPool,
) -> anyhow::Result<(
    StoreServiceClient<Channel>,
    Arc<MemoryChunkBackend>,
    tokio::task::JoinHandle<()>,
)> {
    let backend = Arc::new(MemoryChunkBackend::new());
    let service =
        StoreServiceImpl::with_chunk_backend(pool, backend.clone() as Arc<dyn ChunkBackend>);
    let (client, server) = spawn_store_server(service).await?;
    Ok((client, backend, server))
}

/// Shared: spawn server + connect client. Extracted so both setup variants
/// use it.
async fn spawn_store_server(
    service: StoreServiceImpl,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    // Bind to a random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    // Fire-and-forget: aborted at test end, never joined.
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(StoreServiceServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .expect("in-process store server");
    });

    // Give the server a moment to start accepting
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let endpoint = format!("http://{addr}");
    let channel = Channel::from_shared(endpoint)?.connect().await?;
    let client = StoreServiceClient::new(channel);

    Ok((client, server))
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
mod reassembly;
mod signing;
mod trailer;
