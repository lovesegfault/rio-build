//! gRPC-level integration tests for StoreService.
//!
//! These tests spin up an in-process tonic server backed by [`MemoryBackend`]
//! and a real PostgreSQL database (via `#[sqlx::test]`), then exercise the
//! full gRPC request/response path including streaming.

use std::net::SocketAddr;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::store::store_service_server::StoreServiceServer;
use rio_proto::types::{
    PathInfo, PutPathMetadata, PutPathRequest, QueryPathInfoRequest, put_path_request,
};
use rio_store::backend::memory::MemoryBackend;
use rio_store::grpc::StoreServiceImpl;

/// Spawn an in-process store gRPC server and return a connected client.
///
/// Uses an ephemeral TCP port on 127.0.0.1. The returned `JoinHandle`
/// should be aborted at test end (or dropped) to shut down the server.
pub async fn setup_store(
    pool: PgPool,
) -> (
    StoreServiceClient<Channel>,
    SocketAddr,
    tokio::task::JoinHandle<()>,
) {
    let backend = Arc::new(MemoryBackend::new());
    let service = StoreServiceImpl::new(backend, pool);

    // Bind to a random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(StoreServiceServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // Give the server a moment to start accepting
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let endpoint = format!("http://{addr}");
    let channel = Channel::from_shared(endpoint)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let client = StoreServiceClient::new(channel);

    (client, addr, server)
}

/// Build a minimal valid NAR for a regular file with the given contents.
///
/// This matches the Nix NAR format for a single regular file.
pub fn make_nar(contents: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut nar = Vec::new();

    // NAR format: length-prefixed strings padded to 8 bytes
    fn write_str(out: &mut Vec<u8>, s: &[u8]) {
        let len = s.len() as u64;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(s);
        // Pad to 8-byte boundary
        let pad = (8 - (s.len() % 8)) % 8;
        out.extend_from_slice(&vec![0u8; pad]);
    }

    write_str(&mut nar, b"nix-archive-1");
    write_str(&mut nar, b"(");
    write_str(&mut nar, b"type");
    write_str(&mut nar, b"regular");
    write_str(&mut nar, b"contents");
    // File contents: length-prefixed + padded
    let len = contents.len() as u64;
    nar.extend_from_slice(&len.to_le_bytes());
    nar.write_all(contents).unwrap();
    let pad = (8 - (contents.len() % 8)) % 8;
    nar.extend_from_slice(&vec![0u8; pad]);
    write_str(&mut nar, b")");

    nar
}

/// Build a PathInfo for a store path with the given NAR content.
/// Computes the nar_hash automatically.
pub fn make_path_info(store_path: &str, nar: &[u8]) -> PathInfo {
    let nar_hash = Sha256::digest(nar).to_vec();
    PathInfo {
        store_path: store_path.to_string(),
        store_path_hash: Vec::new(),
        deriver: String::new(),
        nar_hash,
        nar_size: nar.len() as u64,
        references: vec![],
        registration_time: 0,
        ultimate: false,
        signatures: vec![],
        content_address: String::new(),
    }
}

/// Helper: upload a path via PutPath, sending metadata + one nar_chunk.
pub async fn put_path(
    client: &mut StoreServiceClient<Channel>,
    info: PathInfo,
    nar: Vec<u8>,
) -> Result<bool, tonic::Status> {
    let (tx, rx) = mpsc::channel(8);

    // Send metadata first
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
        })),
    })
    .await
    .unwrap();

    // Send NAR data as one chunk
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .unwrap();

    // Close the stream
    drop(tx);

    let outbound = ReceiverStream::new(rx);
    let response = client.put_path(outbound).await?;
    Ok(response.into_inner().created)
}

#[sqlx::test(migrations = "../migrations")]
async fn test_harness_smoke(pool: PgPool) {
    let (mut client, _addr, server) = setup_store(pool).await;

    // QueryPathInfo on missing path should return NOT_FOUND
    let result = client
        .query_path_info(QueryPathInfoRequest {
            store_path: "/nix/store/does-not-exist".into(),
        })
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);

    server.abort();
}
