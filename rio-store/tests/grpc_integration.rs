//! gRPC-level integration tests for StoreService.
//!
//! These tests spin up an in-process tonic server backed by [`MemoryBackend`]
//! and a real PostgreSQL database, then exercise the full gRPC request/response
//! path including streaming.
//!
//! Tests skip gracefully if `DATABASE_URL` is not set.

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

/// Test-database harness. Creates an isolated database per-test and drops on Drop.
pub struct TestDb {
    pub pool: PgPool,
    db_name: String,
    admin_url: String,
}

impl TestDb {
    /// Set up an isolated test database. Returns `None` if `DATABASE_URL` not set.
    pub async fn new() -> Option<Self> {
        let admin_url = std::env::var("DATABASE_URL").ok()?;

        let db_name = format!(
            "rio_store_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let admin_pool = PgPool::connect(&admin_url)
            .await
            .expect("failed to connect to admin DATABASE_URL");
        sqlx::query(&format!(r#"CREATE DATABASE "{db_name}""#))
            .execute(&admin_pool)
            .await
            .expect("failed to create test database");
        admin_pool.close().await;

        let test_url = if let Some(idx) = admin_url.rfind('/') {
            format!("{}/{}", &admin_url[..idx], db_name)
        } else {
            format!("{admin_url}/{db_name}")
        };
        let pool = PgPool::connect(&test_url)
            .await
            .expect("failed to connect to test database");

        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrations failed");

        Some(Self {
            pool,
            db_name,
            admin_url,
        })
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let db_name = self.db_name.clone();
        let admin_url = self.admin_url.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let admin_pool = match PgPool::connect(&admin_url).await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let _ = sqlx::query(&format!(
                    r#"SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                       WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"#
                ))
                .execute(&admin_pool)
                .await;
                let _ = sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{db_name}""#))
                    .execute(&admin_pool)
                    .await;
            });
        })
        .join()
        .ok();
    }
}

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

#[tokio::test]
async fn test_harness_smoke() {
    let Some(db) = TestDb::new().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (mut client, _addr, server) = setup_store(db.pool.clone()).await;

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

// ---------------------------------------------------------------------------
// Group 5: Protocol safety bounds
// ---------------------------------------------------------------------------

/// PutPath with chunks exceeding declared nar_size should be rejected.
#[tokio::test]
async fn test_put_path_rejects_oversized_nar() {
    let Some(db) = TestDb::new().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (mut client, _addr, server) = setup_store(db.pool.clone()).await;

    // Declare nar_size=100 but send 100_000 bytes (well over + 4KB tolerance)
    let mut info = make_path_info("/nix/store/oversized-test", &[0u8; 100]);
    info.nar_size = 100; // Lie about the size

    let oversized_data = vec![0u8; 100_000];
    let result = put_path(&mut client, info, oversized_data).await;

    assert!(result.is_err(), "oversized NAR should be rejected");
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::InvalidArgument,
        "should be INVALID_ARGUMENT, got: {status:?}"
    );
    assert!(
        status.message().contains("exceed"),
        "error message should mention size exceeded: {}",
        status.message()
    );

    server.abort();
}
