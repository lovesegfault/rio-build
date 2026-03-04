//! ChunkService RPCs (GetChunk, FindMissingChunks).

use super::*;

// ===========================================================================
// ChunkService (phase2c C6)
// ===========================================================================

use rio_proto::ChunkServiceClient;
use rio_proto::ChunkServiceServer;
use rio_proto::types::{FindMissingChunksRequest, GetChunkRequest};
use rio_store::cas::ChunkCache;
use rio_store::grpc::ChunkServiceImpl;

/// Spawn both StoreService AND ChunkService, sharing the same cache.
/// For ChunkService tests that need a chunk to be present (upload via
/// PutPath, then fetch via GetChunk).
async fn setup_chunk_service(
    pool: PgPool,
) -> anyhow::Result<(
    StoreServiceClient<Channel>,
    ChunkServiceClient<Channel>,
    Arc<MemoryChunkBackend>,
    tokio::task::JoinHandle<()>,
)> {
    let backend = Arc::new(MemoryChunkBackend::new());
    let cache = Arc::new(ChunkCache::new(backend.clone() as Arc<dyn ChunkBackend>));

    let store_service = StoreServiceImpl::with_chunk_backend(
        pool.clone(),
        backend.clone() as Arc<dyn ChunkBackend>,
    );
    // Same cache instance — a chunk warmed by StoreService's GetPath is
    // hot for ChunkService's GetChunk. This is the shared-state test.
    let chunk_service = ChunkServiceImpl::new(pool, Some(cache));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(StoreServiceServer::new(store_service))
            .add_service(ChunkServiceServer::new(chunk_service))
            .serve_with_incoming(incoming)
            .await
            .expect("in-process server");
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let store_client = StoreServiceClient::new(channel.clone());
    let chunk_client = ChunkServiceClient::new(channel);

    Ok((store_client, chunk_client, backend, server))
}

/// GetChunk for a chunk that exists (uploaded via PutPath): BLAKE3-verified
/// bytes come back. Proves StoreService and ChunkService share state.
#[tokio::test]
async fn test_getchunk_after_putpath() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut store_client, mut chunk_client, _backend, server) =
        setup_chunk_service(db.pool.clone()).await?;

    // Upload via PutPath (large, so it chunks).
    let (nar, info, _) = make_large_nar(60, 512 * 1024);
    put_path(&mut store_client, info, nar).await?;

    // Grab a chunk hash from PG.
    let hash: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&db.pool)
        .await?;

    // GetChunk it back. Should succeed + return non-empty data.
    let mut stream = chunk_client
        .get_chunk(GetChunkRequest {
            digest: hash.clone(),
        })
        .await?
        .into_inner();

    let msg = stream.message().await?.expect("one-message stream");
    assert!(!msg.data.is_empty());

    // BLAKE3 of the returned data should match the requested hash.
    // This is what ChunkCache::get_verified guarantees — but verify it
    // at the gRPC boundary too.
    let actual = blake3::hash(&msg.data);
    assert_eq!(actual.as_bytes().as_slice(), hash.as_slice());

    // Stream is one-message: next recv is None.
    assert!(stream.message().await?.is_none());

    server.abort();
    Ok(())
}

/// GetChunk for unknown hash → NOT_FOUND.
#[tokio::test]
async fn test_getchunk_not_found() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (_store_client, mut chunk_client, _backend, server) =
        setup_chunk_service(db.pool.clone()).await?;

    let result = chunk_client
        .get_chunk(GetChunkRequest {
            digest: vec![0xFF; 32],
        })
        .await;
    assert_eq!(
        result.expect_err("should fail").code(),
        tonic::Code::NotFound
    );

    server.abort();
    Ok(())
}

/// GetChunk with wrong-length digest → INVALID_ARGUMENT (not NOT_FOUND).
#[tokio::test]
async fn test_getchunk_bad_digest_length() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (_store_client, mut chunk_client, _backend, server) =
        setup_chunk_service(db.pool.clone()).await?;

    let result = chunk_client
        .get_chunk(GetChunkRequest {
            digest: vec![0x00; 16], // 16 bytes — not 32
        })
        .await;
    let status = result.expect_err("should fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("32 bytes"));

    server.abort();
    Ok(())
}

/// FindMissingChunks: present chunk filtered out, missing chunk returned.
#[tokio::test]
async fn test_find_missing_chunks() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut store_client, mut chunk_client, _backend, server) =
        setup_chunk_service(db.pool.clone()).await?;

    // Upload → some chunks exist.
    let (nar, info, _) = make_large_nar(61, 512 * 1024);
    put_path(&mut store_client, info, nar).await?;

    let present: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    let absent = vec![0xEE; 32];

    let resp = chunk_client
        .find_missing_chunks(FindMissingChunksRequest {
            digests: vec![present.clone(), absent.clone()],
        })
        .await?
        .into_inner();

    // Only `absent` should be in missing_digests.
    assert_eq!(resp.missing_digests, vec![absent]);
    assert!(!resp.missing_digests.contains(&present));

    server.abort();
    Ok(())
}

/// FindMissingChunks validation: wrong-length digest fails the batch.
#[tokio::test]
async fn test_find_missing_chunks_bad_digest() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (_store_client, mut chunk_client, _backend, server) =
        setup_chunk_service(db.pool.clone()).await?;

    let result = chunk_client
        .find_missing_chunks(FindMissingChunksRequest {
            digests: vec![vec![0x00; 32], vec![0x00; 5]], // second one bad
        })
        .await;
    let status = result.expect_err("bad digest should fail batch");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("digest[1]"),
        "error should identify WHICH digest is bad: {}",
        status.message()
    );

    server.abort();
    Ok(())
}

/// Inline-only store: ChunkService RPCs → FAILED_PRECONDITION.
#[tokio::test]
async fn test_chunkservice_no_cache_failed_precondition() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Construct with cache=None explicitly.
    let chunk_service = ChunkServiceImpl::new(db.pool.clone(), None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(ChunkServiceServer::new(chunk_service))
            .serve_with_incoming(incoming)
            .await
            .expect("in-process server");
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = ChunkServiceClient::new(channel);

    // Both RPCs should fail with FAILED_PRECONDITION.
    let get = client
        .get_chunk(GetChunkRequest {
            digest: vec![0; 32],
        })
        .await;
    assert_eq!(
        get.expect_err("should fail").code(),
        tonic::Code::FailedPrecondition
    );

    let find = client
        .find_missing_chunks(FindMissingChunksRequest {
            digests: vec![vec![0; 32]],
        })
        .await;
    assert_eq!(
        find.expect_err("should fail").code(),
        tonic::Code::FailedPrecondition
    );

    server.abort();
    Ok(())
}
