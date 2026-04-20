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
        let backend = mem_backend();
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
/// the two would have DIFFERENT moka LRUs. The prior version of this
/// test warmed AND verified via `s.chunk` only — that proves
/// "ChunkService has a cache", not "StoreService and ChunkService
/// share one". With two private caches it still passed.
///
/// This test proves sharing: warm via **StoreService.GetPath** (which
/// reads chunks through the cache), CORRUPT the backend, then read via
/// **ChunkService.GetChunk**. If the cache is shared, ChunkService's
/// read comes from moka (good bytes). If StoreService had a private
/// cache, ChunkService misses → backend → corrupted bytes → BLAKE3
/// verify fail → gRPC error.
///
/// This mirrors what main.rs does: one Arc<ChunkCache> cloned into
/// all consumers.
#[tokio::test]
async fn test_shared_cache_warms_across_services() -> TestResult {
    use rio_proto::types::GetPathRequest;
    let mut s = ChunkSession::new().await?;

    // Upload something large enough to chunk.
    let (nar, info, _) = make_large_nar(60, 512 * 1024);
    let store_path = info.store_path.to_string();
    put_path(&mut s.store, info, nar).await?;

    // Grab one chunk's hash.
    let hash: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    let hash_arr: [u8; 32] = hash.as_slice().try_into().expect("32-byte hash");

    // Warm via StoreService.GetPath: reads every chunk through
    // cache.get_verified() (get_path.rs PREFETCH_K loop), populating
    // the SHARED moka. Drain the stream fully so all chunk reads run.
    let mut stream = s
        .store
        .get_path(GetPathRequest {
            store_path,
            manifest_hint: None,
        })
        .await?
        .into_inner();
    while stream.message().await?.is_some() {}

    // Corrupt the backend. If the cache is shared and populated, the
    // next read should NOT hit this. If setup had two caches (the old
    // bug), OR if sharing was broken, the next read goes to backend
    // and BLAKE3 verify fails → gRPC error.
    s.backend
        .corrupt_for_test(&hash_arr, bytes::Bytes::from_static(b"garbage"));

    // Verify via ChunkService.GetChunk: if cache is SHARED, this is a
    // moka hit (original good bytes). If StoreService had a private
    // cache, this misses → corrupted backend → verify fail.
    let got = collect_get_chunk(&mut s.chunk, hash).await?;
    assert!(
        !got.is_empty(),
        "ChunkService read came from SHARED moka cache warmed by \
         StoreService.GetPath. If this fails: cache is NOT shared."
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
