//! ChunkService RPCs (GetChunk).
//!
//! Chunking is server-side only — PutPath drives `cas::put_chunked`,
//! and GetPath streams whole NARs back. GetChunk is the sole
//! chunk-level RPC; it has no production caller today (the builder
//! uses whole-NAR `GetPath` streaming) and is exercised only here, as
//! the chunk-level retrieval surface for future out-of-process
//! reassembly.

use super::*;

// ===========================================================================
// ChunkService
// ===========================================================================

use rio_proto::ChunkServiceServer;
// ChunkServiceClient is not re-exported at crate root (no production
// callers). Tests reach it via the deep codegen path.
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::GetChunkRequest;
use rio_store::cas::ChunkCache;
use rio_store::grpc::ChunkServiceImpl;

/// HMAC key for the `HasChunks` caller-identity gate. The probe is
/// not tenant-scoped, but it must not be anonymous — see
/// `ChunkServiceImpl::require_caller_identity`.
const CHUNK_HMAC_KEY: &[u8] = b"chunk-service-test-key-32-bytes!";

/// A `HasChunksRequest` carrying a valid builder assignment token.
fn authed_has_chunks(digests: Vec<Vec<u8>>) -> tonic::Request<HasChunksRequest> {
    let tok = rio_auth::hmac::HmacSigner::from_key(CHUNK_HMAC_KEY.to_vec()).sign(
        &rio_auth::hmac::AssignmentClaims {
            executor_id: "test".into(),
            drv_hash: "00".repeat(32),
            expected_outputs: vec![],
            is_ca: false,
            expiry_unix: 9_999_999_999,
            tenant: Some(uuid::Uuid::nil().to_string()),
            input_closure_digest: String::new(),
        },
    );
    let mut r = tonic::Request::new(HasChunksRequest { digests });
    r.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        tok.parse().expect("token is ASCII"),
    );
    r
}

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
        let chunk_service = ChunkServiceImpl::new(
            db.pool.clone(),
            Some(cache),
            Some(Arc::new(rio_auth::hmac::HmacVerifier::from_key(
                CHUNK_HMAC_KEY.to_vec(),
            ))),
        );

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
    let db = TestDb::new(&MIGRATOR).await;
    let chunk_service = ChunkServiceImpl::new(db.pool.clone(), None, None);

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
        .get_path(GetPathRequest { store_path })
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

// ===========================================================================
// GetChunks (P0568)
// ===========================================================================

use std::collections::HashMap;

use rio_proto::types::GetChunksRequest;

/// Helper: open a GetChunks bidi stream, send `frames` (each a list of
/// digests), close the request side, and collect every `ChunkData`
/// keyed by digest. Errors from the stream propagate so callers can
/// assert on the abort code.
async fn collect_get_chunks(
    client: &mut ChunkServiceClient<Channel>,
    frames: Vec<Vec<Vec<u8>>>,
) -> Result<HashMap<Vec<u8>, Vec<u8>>, tonic::Status> {
    let (tx, rx) = mpsc::channel(8);
    for digests in frames {
        tx.send(GetChunksRequest { digests })
            .await
            .expect("fresh channel");
    }
    drop(tx);
    let mut stream = client
        .get_chunks(ReceiverStream::new(rx))
        .await?
        .into_inner();
    let mut got: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    while let Some(chunk) = stream.message().await? {
        let dup = got.insert(chunk.digest.clone(), chunk.data.to_vec());
        assert!(dup.is_none(), "server sent the same digest twice");
    }
    Ok(got)
}

/// Batch fetch of every chunk in a chunked NAR, split across two
/// request frames in an order unrelated to the manifest. Every chunk
/// comes back, every byte BLAKE3-verifies against its requested
/// digest, and the byte count matches the chunk total — proves the
/// server resolves by content address, not request position.
// r[verify proto.chunk.batch-bidi]
#[tokio::test]
async fn test_getchunks_batched_round_trip() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // Chunks carry file CONTENT bytes only (ADR-022 §6) — the sum of
    // chunk sizes is the payload length, not the NAR length (which
    // adds ~112 bytes of framing).
    let content_size = (768 * 1024) as i64;
    let (nar, info, _) = make_large_nar(70, 768 * 1024);
    put_path(&mut s.store, info, nar).await?;

    let hashes: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT blake3_hash FROM chunks ORDER BY blake3_hash")
            .fetch_all(&s.db.pool)
            .await?;
    assert!(hashes.len() >= 2, "NAR should have chunked into ≥2 pieces");

    // Split across two frames so the request shape exercises the
    // multi-frame path, and reverse the second so the server's
    // by-digest matching is doing real work.
    let mid = hashes.len() / 2;
    let mut frame_b: Vec<Vec<u8>> = hashes[mid..].to_vec();
    frame_b.reverse();
    let got = collect_get_chunks(&mut s.chunk, vec![hashes[..mid].to_vec(), frame_b]).await?;

    assert_eq!(got.len(), hashes.len(), "every requested digest answered");
    let mut total = 0i64;
    for h in &hashes {
        let data = got.get(h).expect("requested digest returned");
        assert_eq!(
            blake3::hash(data).as_bytes().as_slice(),
            h.as_slice(),
            "ChunkData content BLAKE3-verifies against its digest"
        );
        total += data.len() as i64;
    }
    assert_eq!(
        total, content_size,
        "sum of chunk sizes == file content size (framing is never chunked)"
    );
    Ok(())
}

/// Truncated digest in the middle of a batch → the stream aborts with
/// INVALID_ARGUMENT before any backend lookup for that digest.
/// Validation is front-loaded so a client bug surfaces as a usage
/// error, not a confusing backend miss.
// r[verify proto.chunk.batch-bidi]
#[tokio::test]
async fn test_getchunks_bad_digest_aborts() -> TestResult {
    let mut s = ChunkSession::new().await?;
    let err = collect_get_chunks(&mut s.chunk, vec![vec![vec![0xEE; 7]]])
        .await
        .expect_err("short digest should abort the stream");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    Ok(())
}

/// Unknown digest aborts with NOT_FOUND. The §6 fill task treats this
/// as an integrity error (the chunk should exist per the manifest) and
/// fails the build infrastructure-side — it MUST be distinguishable
/// from a transport error, which is retryable.
// r[verify proto.chunk.batch-bidi]
#[tokio::test]
async fn test_getchunks_not_found_aborts() -> TestResult {
    let mut s = ChunkSession::new().await?;
    let err = collect_get_chunks(&mut s.chunk, vec![vec![vec![0xEE; 32]]])
        .await
        .expect_err("unknown chunk should abort the stream");
    assert_eq!(err.code(), tonic::Code::NotFound);
    Ok(())
}

/// Inline-only store → FAILED_PRECONDITION, same as GetChunk. The
/// guard is shared so the two RPCs can't drift apart.
#[tokio::test]
async fn test_getchunks_no_cache_failed_precondition() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let chunk_service = ChunkServiceImpl::new(db.pool.clone(), None, None);
    let router = Server::builder().add_service(ChunkServiceServer::new(chunk_service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = ChunkServiceClient::new(channel);

    let err = collect_get_chunks(&mut client, vec![vec![vec![0; 32]]])
        .await
        .expect_err("no cache should fail");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    server.abort();
    Ok(())
}

// ===========================================================================
// HasChunks (P0586) — durable-presence probe
// ===========================================================================

use rio_proto::types::HasChunksRequest;

/// Insert a bare `chunks` row in a given (refcount, durable, deleted)
/// state. The WAL-window state (`refcount=1, durable=false`) is what a
/// SIGKILL between the refcount bump and the S3 PutObject leaves
/// behind — the I-201 hazard HasChunks must treat as absent.
async fn seed_chunk_row(
    pool: &sqlx::PgPool,
    hash: &[u8],
    durable: bool,
    deleted: bool,
) -> TestResult {
    sqlx::query(
        "INSERT INTO chunks (blake3_hash, refcount, size, durable, deleted) \
         VALUES ($1, 1, 1024, $2, $3)",
    )
    .bind(hash)
    .bind(durable)
    .bind(deleted)
    .execute(pool)
    .await?;
    Ok(())
}

// r[verify store.chunk.has-chunks-durable]
/// The I-201 regression: a refcount-1-but-not-durable chunk (the
/// SIGKILL-mid-upload state) MUST read as absent, alongside the
/// straightforward durable/deleted/absent cases.
#[tokio::test]
async fn test_has_chunks_durable_only_presence() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let durable = vec![0xD0u8; 32];
    let wal_window = vec![0xD1u8; 32]; // refcount 1, durable false
    let dead = vec![0xD2u8; 32]; // durable but deleted
    let absent = vec![0xD3u8; 32]; // no row at all
    seed_chunk_row(&s.db.pool, &durable, true, false).await?;
    seed_chunk_row(&s.db.pool, &wal_window, false, false).await?;
    seed_chunk_row(&s.db.pool, &dead, true, true).await?;

    let resp = s
        .chunk
        .has_chunks(authed_has_chunks(vec![
            durable.clone(),
            wal_window.clone(),
            dead.clone(),
            absent.clone(),
        ]))
        .await?
        .into_inner();
    // Only bit 0 (durable, not deleted) is set.
    assert_eq!(resp.bitmap, vec![0b0000_0001]);
    Ok(())
}

/// Malformed digest → INVALID_ARGUMENT; empty request → empty bitmap.
#[tokio::test]
async fn test_has_chunks_validation() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let err = s
        .chunk
        .has_chunks(authed_has_chunks(vec![vec![0xEE; 7]]))
        .await
        .expect_err("short digest must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let resp = s
        .chunk
        .has_chunks(authed_has_chunks(Vec::new()))
        .await?
        .into_inner();
    assert!(resp.bitmap.is_empty(), "empty probe → empty bitmap");
    Ok(())
}

/// An anonymous (or forged-token) HasChunks probe is rejected before
/// the digest list is even parsed. Presence is one bit the caller
/// cannot compute themselves; for a file under FASTCDC_MIN_BYTES the
/// whole file is one chunk, so an unauthenticated probe would be a
/// content-existence oracle over offline candidate guesses ("has
/// anyone built a config containing exactly this secret?"). The
/// retrieval RPCs stay anonymous — their response discloses nothing
/// the digest didn't already prove.
// r[verify store.chunk.has-chunks-authenticated]
#[tokio::test]
async fn test_has_chunks_rejects_anonymous_and_forged_callers() -> TestResult {
    let mut s = ChunkSession::new().await?;
    let durable = vec![0xD0u8; 32];
    seed_chunk_row(&s.db.pool, &durable, true, false).await?;

    // No token at all.
    let err = s
        .chunk
        .has_chunks(HasChunksRequest {
            digests: vec![durable.clone()],
        })
        .await
        .expect_err("anonymous probe must be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // A token signed with the wrong key.
    let forged = rio_auth::hmac::HmacSigner::from_key(b"not-the-server-key-aaaaaaaaaaaaa".to_vec())
        .sign(&rio_auth::hmac::AssignmentClaims {
            executor_id: "evil".into(),
            drv_hash: "00".repeat(32),
            expected_outputs: vec![],
            is_ca: false,
            expiry_unix: 9_999_999_999,
            tenant: Some(uuid::Uuid::nil().to_string()),
            input_closure_digest: String::new(),
        });
    let mut req = tonic::Request::new(HasChunksRequest {
        digests: vec![durable.clone()],
    });
    req.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        forged.parse().expect("token is ASCII"),
    );
    let err = s
        .chunk
        .has_chunks(req)
        .await
        .expect_err("forged token must be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // Vacuity sentinel: the same digest IS reported present to an
    // authenticated caller, so the rejections above are the auth gate
    // firing, not the chunk being absent.
    let resp = s
        .chunk
        .has_chunks(authed_has_chunks(vec![durable]))
        .await?
        .into_inner();
    assert_eq!(resp.bitmap, vec![0b0000_0001]);
    Ok(())
}

// r[verify store.chunk.durable-flag]
/// End-to-end: a chunked PutPath flips its chunks to durable when the
/// manifest completes, and HasChunks then reports them present. Before
/// the upload completes nothing is durable (the WAL window).
#[tokio::test]
async fn test_has_chunks_after_putpath_complete() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let (nar, info, _) = make_large_nar(60, 512 * 1024);
    put_path(&mut s.store, info, nar).await?;

    let hashes: Vec<Vec<u8>> = sqlx::query_scalar("SELECT blake3_hash FROM chunks")
        .fetch_all(&s.db.pool)
        .await?;
    assert!(hashes.len() >= 2, "512 KiB NAR must chunk into >1 chunk");

    let resp = s
        .chunk
        .has_chunks(authed_has_chunks(hashes.clone()))
        .await?
        .into_inner();
    // Every chunk of the completed manifest is durable → every bit set.
    for (i, h) in hashes.iter().enumerate() {
        assert_ne!(
            resp.bitmap[i / 8] & (1 << (i % 8)),
            0,
            "chunk {} of a completed manifest must be durable",
            hex::encode(h)
        );
    }
    Ok(())
}
