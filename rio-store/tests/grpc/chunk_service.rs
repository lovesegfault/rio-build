//! ChunkService RPCs (GetChunk, FindMissingChunks).

use super::*;

// ===========================================================================
// ChunkService
// ===========================================================================

use rio_proto::ChunkServiceClient;
use rio_proto::ChunkServiceServer;
use rio_proto::types::{
    FindMissingChunksRequest, GetChunkRequest, PutChunkMetadata, PutChunkRequest, put_chunk_request,
};
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

        let store_service = StoreServiceImpl::with_chunk_cache(db.pool.clone(), Arc::clone(&cache));
        let chunk_service = ChunkServiceImpl::new(db.pool.clone(), Some(cache));

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

    let msg = stream.message().await?.expect("one-message stream");
    assert!(!msg.data.is_empty());

    // BLAKE3 of the returned data should match the requested hash.
    // This is what ChunkCache::get_verified guarantees — but verify it
    // at the gRPC boundary too.
    let actual = blake3::hash(&msg.data);
    assert_eq!(actual.as_bytes().as_slice(), hash.as_slice());

    // Stream is one-message: next recv is None.
    assert!(stream.message().await?.is_none());
    Ok(())
}

/// GetChunk for unknown hash → NOT_FOUND.
#[tokio::test]
async fn test_getchunk_not_found() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let result = s
        .chunk
        .get_chunk(GetChunkRequest {
            digest: vec![0xFF; 32],
        })
        .await;
    assert_eq!(
        result.expect_err("should fail").code(),
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
            digest: vec![0x00; 16], // 16 bytes — not 32
        })
        .await;
    let status = result.expect_err("should fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("32 bytes"));
    Ok(())
}

/// FindMissingChunks: present chunk filtered out, missing chunk returned.
#[tokio::test]
async fn test_find_missing_chunks() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // Upload → some chunks exist.
    let (nar, info, _) = make_large_nar(61, 512 * 1024);
    put_path(&mut s.store, info, nar).await?;

    let present: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    let absent = vec![0xEE; 32];

    let resp = s
        .chunk
        .find_missing_chunks(FindMissingChunksRequest {
            digests: vec![present.clone(), absent.clone()],
        })
        .await?
        .into_inner();

    // Only `absent` should be in missing_digests.
    assert_eq!(resp.missing_digests, vec![absent]);
    assert!(!resp.missing_digests.contains(&present));
    Ok(())
}

/// FindMissingChunks validation: wrong-length digest fails the batch.
#[tokio::test]
async fn test_find_missing_chunks_bad_digest() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let result = s
        .chunk
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
    Ok(())
}

/// Inline-only store: ChunkService RPCs → FAILED_PRECONDITION.
#[tokio::test]
async fn test_chunkservice_no_cache_failed_precondition() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Construct with cache=None explicitly.
    let chunk_service = ChunkServiceImpl::new(db.pool.clone(), None);

    let router = Server::builder().add_service(ChunkServiceServer::new(chunk_service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
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

// ===========================================================================
// PutChunk
// ===========================================================================

/// Helper: build a well-formed PutChunk stream (Metadata → Data).
/// Splits `data` into two frames to exercise the multi-frame
/// accumulation path — a single-frame test would pass even if the
/// loop body ran exactly once and broke.
fn put_chunk_stream(digest: [u8; 32], data: Vec<u8>) -> Vec<PutChunkRequest> {
    let mid = data.len() / 2;
    vec![
        PutChunkRequest {
            msg: Some(put_chunk_request::Msg::Metadata(PutChunkMetadata {
                digest: digest.to_vec(),
                size: data.len() as u64,
            })),
        },
        PutChunkRequest {
            msg: Some(put_chunk_request::Msg::Data(data[..mid].to_vec())),
        },
        PutChunkRequest {
            msg: Some(put_chunk_request::Msg::Data(data[mid..].to_vec())),
        },
    ]
}

// r[verify store.chunk.put-standalone]
/// PutChunk round-trip: upload → verify in backend + PG → fetch
/// via GetChunk. The full loop proves the chunk is queryable via
/// the same shared state as GetChunk (same cache, same backend).
///
/// Also asserts the PG row lands at refcount=0 — that's the
/// grace-TTL anchor. If PutChunk wrote refcount=1, the orphan
/// sweep would never reap abandoned chunks (they'd look referenced).
#[tokio::test]
async fn test_putchunk_roundtrip_and_verify() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // 80 KiB of deterministic-but-nonrepeating data. Above
    // CHUNK_MIN (16 KiB) so it's a plausible real chunk; well
    // under CHUNK_MAX (256 KiB). The seed×wrapping pattern avoids
    // the "zero-filled buffer → BLAKE3 of zeroes" trap that would
    // accidentally collide with other tests' fixtures.
    let data: Vec<u8> = (0..80 * 1024)
        .map(|i| (i as u8).wrapping_mul(131))
        .collect();
    let digest = *blake3::hash(&data).as_bytes();

    // Precondition: FindMissingChunks says this digest is missing.
    // Without this, a prior test leaking state into the backend
    // would make the PutChunk a no-op (ON CONFLICT DO NOTHING) and
    // the test would "pass" without exercising the insert path.
    let pre = s
        .chunk
        .find_missing_chunks(FindMissingChunksRequest {
            digests: vec![digest.to_vec()],
        })
        .await?
        .into_inner();
    assert_eq!(
        pre.missing_digests,
        vec![digest.to_vec()],
        "precondition: chunk must be absent before PutChunk (test isolation)"
    );

    // PutChunk.
    let resp = s
        .chunk
        .put_chunk(tokio_stream::iter(put_chunk_stream(digest, data.clone())))
        .await?
        .into_inner();
    assert_eq!(resp.digest, digest.to_vec(), "response echoes the digest");

    // Backend has it. MemoryChunkBackend's get() is in-process —
    // this proves the backend.put() call in the handler fired.
    let stored = s
        .backend
        .get(&digest)
        .await?
        .context("chunk must be in backend after PutChunk")?;
    assert_eq!(&stored[..], &data[..], "backend bytes match uploaded bytes");

    // PG row at refcount=0. This is the grace-TTL invariant: the
    // chunk exists in PG (so sweep_orphan_chunks can find it) but
    // unreferenced (so the grace window applies).
    let (refcount, size, deleted): (i32, i64, bool) =
        sqlx::query_as("SELECT refcount, size, deleted FROM chunks WHERE blake3_hash = $1")
            .bind(digest.as_slice())
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        refcount, 0,
        "PutChunk inserts at refcount=0 (grace-TTL anchor)"
    );
    assert_eq!(size, data.len() as i64, "size recorded correctly");
    assert!(!deleted, "fresh chunk is not deleted");

    // GetChunk round-trip: fetch via the RPC we already trust.
    // If this fails but the backend check above passed, the
    // PutChunk handler uploaded but something about the chunk's
    // state (e.g. a missing PG row that GetChunk secretly reads)
    // is broken. GetChunk doesn't read PG — this is future-proofing.
    let fetched = collect_get_chunk(&mut s.chunk, digest.to_vec()).await?;
    assert_eq!(
        fetched, data,
        "GetChunk returns the bytes PutChunk uploaded"
    );

    // FindMissingChunks now says present.
    let post = s
        .chunk
        .find_missing_chunks(FindMissingChunksRequest {
            digests: vec![digest.to_vec()],
        })
        .await?
        .into_inner();
    assert!(
        post.missing_digests.is_empty(),
        "FindMissingChunks sees the chunk after PutChunk"
    );

    Ok(())
}

/// Hash mismatch: declared digest doesn't match the data's BLAKE3.
/// Must fail INVALID_ARGUMENT, and MUST NOT persist anything (the
/// worker-not-trusted invariant — a chunk the server couldn't
/// verify never enters the store).
#[tokio::test]
async fn test_putchunk_hash_mismatch_rejected() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let data: Vec<u8> = b"this is the real data, 28 b.".to_vec();
    let real_digest = *blake3::hash(&data).as_bytes();
    // Lie about the digest. Flip one bit — a gross mismatch
    // (all-zeros) could be accidentally caught by a length check
    // somewhere else; a near-miss proves it's the BLAKE3 compare
    // that fires.
    let mut fake_digest = real_digest;
    fake_digest[0] ^= 0x01;

    let result = s
        .chunk
        .put_chunk(tokio_stream::iter(put_chunk_stream(fake_digest, data)))
        .await;
    let status = result.expect_err("hash mismatch must fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("hash mismatch"),
        "error message identifies the failure mode: {}",
        status.message()
    );

    // Nothing persisted under EITHER digest. The fake one
    // obviously shouldn't be there; the real one shouldn't either
    // (we declared fake, the handler never computed real as a
    // valid key).
    let pg_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chunks WHERE blake3_hash = $1 OR blake3_hash = $2",
    )
    .bind(fake_digest.as_slice())
    .bind(real_digest.as_slice())
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(pg_count, 0, "nothing in PG after rejected PutChunk");

    assert!(
        s.backend.get(&fake_digest).await?.is_none(),
        "nothing in backend under fake digest"
    );
    assert!(
        s.backend.get(&real_digest).await?.is_none(),
        "nothing in backend under real digest either"
    );

    Ok(())
}

/// Data-before-metadata: stream shape violation. Same rejection
/// shape as PutPath (put_path.rs:256). The size-cap-before-buffer
/// property depends on this ordering.
#[tokio::test]
async fn test_putchunk_data_before_metadata_rejected() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let stream = vec![
        PutChunkRequest {
            msg: Some(put_chunk_request::Msg::Data(vec![0xAB; 100])),
        },
        PutChunkRequest {
            msg: Some(put_chunk_request::Msg::Metadata(PutChunkMetadata {
                digest: vec![0u8; 32],
                size: 100,
            })),
        },
    ];

    let result = s.chunk.put_chunk(tokio_stream::iter(stream)).await;
    let status = result.expect_err("data-before-metadata must fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("first") && status.message().contains("Metadata"),
        "error identifies the ordering violation: {}",
        status.message()
    );

    Ok(())
}

/// Declared size exceeds CHUNK_MAX: rejected before reading ANY
/// data bytes. This is the DoS guard — a client can't make us
/// allocate a 32 MiB buffer by lying in the metadata frame.
#[tokio::test]
async fn test_putchunk_oversize_rejected_early() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // Just the metadata frame — if the handler reads past it before
    // rejecting, this stream ends early and we'd get a DIFFERENT
    // error ("declared size X, received 0"). Getting the CHUNK_MAX
    // error proves the reject fired on the metadata frame alone.
    let stream = vec![PutChunkRequest {
        msg: Some(put_chunk_request::Msg::Metadata(PutChunkMetadata {
            digest: vec![0u8; 32],
            size: 1024 * 1024, // 1 MiB — over CHUNK_MAX (256 KiB)
        })),
    }];

    let result = s.chunk.put_chunk(tokio_stream::iter(stream)).await;
    let status = result.expect_err("oversize must fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("CHUNK_MAX"),
        "error identifies the size cap: {}",
        status.message()
    );

    Ok(())
}

/// Idempotence: PutChunk the same chunk twice. Second call
/// succeeds (ON CONFLICT DO NOTHING), refcount stays 0, created_at
/// is NOT reset (the grace clock doesn't restart — a slow-retrying
/// client can't keep a dead chunk alive indefinitely).
#[tokio::test]
async fn test_putchunk_idempotent_created_at_stable() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let data: Vec<u8> = (0..40_000).map(|i| (i as u8).wrapping_add(7)).collect();
    let digest = *blake3::hash(&data).as_bytes();

    // First PutChunk.
    s.chunk
        .put_chunk(tokio_stream::iter(put_chunk_stream(digest, data.clone())))
        .await?;

    // Backdate created_at — simulate time passing. If the second
    // PutChunk resets created_at to now(), this backdating is
    // clobbered and the assert below catches it.
    sqlx::query(
        "UPDATE chunks SET created_at = now() - interval '1 hour' \
         WHERE blake3_hash = $1",
    )
    .bind(digest.as_slice())
    .execute(&s.db.pool)
    .await?;

    // EXTRACT(EPOCH ...)::bigint — rio-store has no chrono dep
    // (see grpc/admin.rs:479), so we compare timestamps as integer
    // epoch seconds. Second-granularity is plenty: the backdate
    // above is 1 hour, a second PutChunk resetting to now() would
    // differ by ~3600.
    let created_before: i64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM created_at)::bigint FROM chunks WHERE blake3_hash = $1",
    )
    .bind(digest.as_slice())
    .fetch_one(&s.db.pool)
    .await?;

    // Second PutChunk — same bytes, same digest.
    s.chunk
        .put_chunk(tokio_stream::iter(put_chunk_stream(digest, data)))
        .await?;

    let (refcount, created_after): (i32, i64) = sqlx::query_as(
        "SELECT refcount, EXTRACT(EPOCH FROM created_at)::bigint \
         FROM chunks WHERE blake3_hash = $1",
    )
    .bind(digest.as_slice())
    .fetch_one(&s.db.pool)
    .await?;

    assert_eq!(refcount, 0, "second PutChunk doesn't bump refcount");
    assert_eq!(
        created_after, created_before,
        "ON CONFLICT DO NOTHING: created_at stable — grace clock NOT reset"
    );

    Ok(())
}
