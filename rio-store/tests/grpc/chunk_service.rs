//! ChunkService RPCs (GetChunk).
//!
//! Chunking is server-side only — PutPath drives `cas::put_chunked`.
//! GetChunk is the sole chunk-level RPC; the builder fans it out to
//! reassemble NARs from their manifests.

use super::*;

// ===========================================================================
// ChunkService
// ===========================================================================

use rio_proto::ChunkServiceServer;
// ChunkServiceClient is not re-exported at crate root (no production
// callers outside the builder). Tests reach it via the deep codegen path.
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::GetChunkRequest;
use rio_store::cas::ChunkCache;
use rio_store::grpc::ChunkServiceImpl;

/// Harness with both StoreService AND ChunkService sharing one cache.
/// Mirrors `StoreSession` (main.rs) — `Drop` aborts the server so
/// tests don't need `server.abort()` boilerplate.
struct ChunkSession {
    db: TestDb,
    store: StoreServiceClient<Channel>,
    chunk: ChunkServiceClient<Channel>,
    backend: Arc<MemoryChunkBackend>,
    server: tokio::task::JoinHandle<()>,
}

impl ChunkSession {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let backend = Arc::new(MemoryChunkBackend::new());
        // ONE cache, shared across StoreService and ChunkService.
        // A previous convenience constructor (since removed) created
        // a private cache per service — two caches that both missed
        // → both hit the same backend → correct data but no cross-
        // service warming. with_chunk_cache takes an Arc so callers
        // MUST decide sharing explicitly. test_shared_cache_warms_
        // across_services proves it works.
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));

        let store_service =
            StoreServiceImpl::new(db.pool.clone()).with_chunk_cache(Arc::clone(&cache));
        let chunk_service = ChunkServiceImpl::new(Some(cache));

        let router = Server::builder()
            .add_service(StoreServiceServer::new(store_service))
            .add_service(ChunkServiceServer::new(chunk_service));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;

        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let store = StoreServiceClient::new(channel.clone());
        let chunk = ChunkServiceClient::new(channel);

        Ok(Self {
            db,
            store,
            chunk,
            backend,
            server,
        })
    }
}

impl Drop for ChunkSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// GetChunk for a chunk that exists (uploaded via PutPath): BLAKE3-verified
/// bytes come back. Proves StoreService and ChunkService share state.
#[tokio::test]
async fn test_getchunk_after_putpath() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // Upload via PutPath (large, so it chunks).
    let (nar, info, _) = make_large_nar(60, 512 * 1024);
    put_path(&mut s.store, info, nar).await?;

    // Grab a chunk hash from PG.
    let hash: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;

    // GetChunk it back. Should succeed + return non-empty data.
    let mut stream = s
        .chunk
        .get_chunk(GetChunkRequest {
            digest: hash.clone(),
        })
        .await?
        .into_inner();
    let mut got = Vec::new();
    while let Some(resp) = stream.message().await? {
        got.extend_from_slice(&resp.data);
    }
    assert!(!got.is_empty(), "chunk has content");

    // Verify the digest matches what we asked for.
    let computed = *blake3::hash(&got).as_bytes();
    assert_eq!(
        computed.as_slice(),
        hash.as_slice(),
        "GetChunk content BLAKE3-verifies against requested digest"
    );
    Ok(())
}

/// GetChunk for unknown hash → NOT_FOUND.
#[tokio::test]
async fn test_getchunk_not_found() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let result = s
        .chunk
        .get_chunk(GetChunkRequest {
            digest: vec![0xEE; 32],
        })
        .await;
    assert_eq!(
        result.expect_err("unknown chunk should fail").code(),
        tonic::Code::NotFound
    );
    Ok(())
}

/// GetChunk with wrong-length digest → INVALID_ARGUMENT (not NOT_FOUND).
#[tokio::test]
async fn test_getchunk_bad_digest_length() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let result = s
        .chunk
        .get_chunk(GetChunkRequest {
            digest: vec![0xEE; 7],
        })
        .await;
    assert_eq!(
        result.expect_err("short digest should fail").code(),
        tonic::Code::InvalidArgument
    );
    Ok(())
}

/// Inline-only store: ChunkService RPCs → FAILED_PRECONDITION.
#[tokio::test]
async fn test_chunkservice_no_cache_failed_precondition() -> TestResult {
    // Construct with cache=None explicitly.
    let chunk_service = ChunkServiceImpl::new(None);

    let router = Server::builder().add_service(ChunkServiceServer::new(chunk_service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = ChunkServiceClient::new(channel);

    let get = client
        .get_chunk(GetChunkRequest {
            digest: vec![0; 32],
        })
        .await;
    assert_eq!(
        get.expect_err("should fail").code(),
        tonic::Code::FailedPrecondition
    );

    server.abort();
    Ok(())
}

/// Prove StoreService and ChunkService ACTUALLY share one cache.
///
/// If StoreService and ChunkService each had their own ChunkCache,
/// the two would have DIFFERENT moka LRUs. That would pass incidentally: both
/// miss → both hit the shared MemoryChunkBackend → same data. But
/// "warmed by GetPath is hot for GetChunk" wasn't really tested.
///
/// This test proves sharing: GetChunk populates moka, then CORRUPT
/// the backend, then GetChunk again. If the cache is real, the second
/// read comes from moka (good bytes, BLAKE3 verify passes). If there's
/// no cache sharing (or no cache at all), the second read goes to
/// backend (corrupted bytes, verify fails).
///
/// This mirrors what main.rs does: one Arc<ChunkCache> cloned into
/// all consumers.
#[tokio::test]
async fn test_shared_cache_warms_across_services() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // Upload something large enough to chunk.
    let (nar, info, _) = make_large_nar(60, 512 * 1024);
    put_path(&mut s.store, info, nar).await?;

    // Grab one chunk's hash.
    let hash: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    let hash_arr: [u8; 32] = hash.as_slice().try_into().expect("32-byte hash");

    // First GetChunk: cold → backend → moka insert.
    let first = collect_get_chunk(&mut s.chunk, hash.clone()).await?;
    assert!(!first.is_empty(), "chunk has content");

    // Corrupt the backend. If the cache is shared and populated, the
    // next read should NOT hit this. If setup had two caches (the old
    // bug), OR if sharing was broken, the next read goes to backend
    // and BLAKE3 verify fails → gRPC error.
    s.backend
        .corrupt_for_test(&hash_arr, bytes::Bytes::from_static(b"garbage"));

    // Second GetChunk: if cache is real, this is a moka hit (original
    // good bytes). If not, it reads the corrupted backend → verify
    // fail → gRPC Internal error.
    let second = collect_get_chunk(&mut s.chunk, hash).await?;
    assert_eq!(
        second, first,
        "second read came from SHARED moka cache (same bytes), not \
         corrupted backend. If this fails: cache is NOT shared."
    );
    Ok(())
}

/// Helper: GetChunk stream → flatten to bytes.
async fn collect_get_chunk(
    client: &mut ChunkServiceClient<Channel>,
    digest: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    let mut stream = client
        .get_chunk(GetChunkRequest { digest })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(resp) = stream.message().await? {
        out.extend_from_slice(&resp.data);
    }
    Ok(out)
}
