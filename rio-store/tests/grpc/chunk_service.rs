//! ChunkService RPCs (GetChunk, FindMissingChunks, PutChunk).
//!
//! # Tenant-scope test interceptor
//!
//! FindMissingChunks and PutChunk are FAIL-CLOSED on missing `jwt::Claims`
//! (see `grpc/chunk.rs` — `require_tenant`). Production gets Claims from
//! `jwt_interceptor` verifying `x-rio-tenant-token`. Tests don't want the
//! ed25519 signing dance — instead, `ChunkSession` wires a server-side
//! interceptor that reads `x-test-tenant-id` (a bare UUID string) and
//! stamps a synthetic `Claims` into extensions. Same extensions slot the
//! handler reads, so the handler can't tell the difference.
//!
//! The alternative — calling handler methods directly, bypassing gRPC —
//! would lose the streaming/framing coverage these tests exist for.

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
use uuid::Uuid;

/// Metadata key for the test-only tenant header. Distinct from the
/// production `x-rio-tenant-token` so there's no risk of a test
/// accidentally exercising the real JWT verify path (which would
/// need a signing key) or of production code ever reading this.
const TEST_TENANT_HEADER: &str = "x-test-tenant-id";

/// Server-side test interceptor: `x-test-tenant-id` UUID → `jwt::Claims`
/// in extensions. Mirrors what `jwt_interceptor` does AFTER a successful
/// verify, but skips the crypto — tests control both ends of the channel,
/// so there's nothing for a signature to prove.
///
/// Absent header → pass-through with no Claims. That's intentional: the
/// FAIL-CLOSED tests below want to exercise the `None` branch, and
/// GetChunk (unscoped) doesn't need a tenant at all.
fn test_tenant_interceptor(
    mut req: tonic::Request<()>,
) -> Result<tonic::Request<()>, tonic::Status> {
    if let Some(raw) = req.metadata().get(TEST_TENANT_HEADER) {
        // Parse is a test-harness bug if it fails — panic-via-expect
        // surfaces that immediately instead of a confusing UNAUTHENTICATED
        // from the handler. Both `to_str` and `parse` are infallible for
        // the UUIDs this file mints; expect documents the invariant.
        let sub: Uuid = raw
            .to_str()
            .expect("test tenant header must be ASCII (UUID hyphenated form)")
            .parse()
            .expect("test tenant header must be a valid UUID — check the test's seed_tenant call");
        // iat/exp/jti are never read by ChunkService handlers (only
        // `sub` is — see `require_tenant` in grpc/chunk.rs). Dummy
        // values here; a handler that starts reading exp will make
        // these tests fail loudly (past expiry), which is the right
        // signal to update this interceptor.
        req.extensions_mut().insert(rio_common::jwt::Claims {
            sub,
            iat: 0,
            exp: 0,
            jti: String::new(),
        });
    }
    Ok(req)
}

/// Stamp the test tenant header on a request. For unary RPCs where
/// `T` is a message type (e.g., `FindMissingChunksRequest`) —
/// `IntoRequest<T>` wraps it in a `Request<T>`.
///
/// NOT usable for `Request<Stream>` (streaming RPCs): tonic's two
/// blanket impls (`impl IntoRequest<T> for T` and `impl IntoRequest<T>
/// for Request<T>`) both match when you already have a `Request`, and
/// inference can't pick. `put_chunk_req` below sets metadata directly
/// for the streaming case.
fn as_tenant<T>(tenant_id: Uuid, msg: T) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    stamp_tenant(&mut req, tenant_id);
    req
}

/// Set the test tenant header on an already-built request. Split out
/// from `as_tenant` so `put_chunk_req` (which needs `Request<Stream>`)
/// can reuse the header-setting without hitting the `IntoRequest`
/// inference ambiguity.
fn stamp_tenant<T>(req: &mut tonic::Request<T>, tenant_id: Uuid) {
    req.metadata_mut().insert(
        TEST_TENANT_HEADER,
        // UUID hyphenated form is pure ASCII hex + dashes — always a
        // valid metadata value. expect documents the impossibility.
        tenant_id
            .to_string()
            .parse()
            .expect("UUID hyphenated form is always valid ASCII metadata"),
    );
}

/// Harness with both StoreService AND ChunkService sharing one cache.
/// Mirrors `StoreSession` (main.rs) — `Drop` aborts the server so
/// tests don't need `server.abort()` boilerplate.
///
/// Holds a default `tenant_id` seeded in PG during setup. Tests that
/// don't care about multi-tenancy just use `s.tenant_id` everywhere;
/// the isolation test seeds a second tenant via `seed_tenant()`.
struct ChunkSession {
    db: TestDb,
    store: StoreServiceClient<Channel>,
    chunk: ChunkServiceClient<Channel>,
    backend: Arc<MemoryChunkBackend>,
    /// Default tenant, seeded in PG. Not special in any way — the
    /// handler doesn't check it against the tenants table (the FK on
    /// chunk_tenants does that at INSERT time). It's here so single-
    /// tenant tests have a ready-made identity.
    tenant_id: Uuid,
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

        // InterceptorLayer wraps ALL services on this server — same
        // pattern as production (main.rs:495). StoreService ignores
        // the header (its handlers never read jwt::Claims from
        // extensions). ChunkService's tenant-scoped RPCs read it.
        //
        // Inlined server spawn: `rio_test_support::grpc::spawn_grpc_server`
        // takes `Router<Identity>` (default layer), but `.layer()`
        // changes the type to `Router<Stack<InterceptorLayer<_>, _>>`.
        // The helper isn't generic over the layer param (it's used by
        // ~20 other tests that don't need layers), so we inline the
        // same bind-ephemeral + serve_with_incoming pattern here.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let router = Server::builder()
            .layer(tonic::service::InterceptorLayer::new(
                test_tenant_interceptor,
            ))
            .add_service(StoreServiceServer::new(store_service))
            .add_service(ChunkServiceServer::new(chunk_service));
        let server = tokio::spawn(async move {
            router
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .expect("in-process gRPC server");
        });
        // yield_now: let the server task reach its accept loop before
        // we return and the caller tries to connect. Without this,
        // the first client connect can race the server's listener
        // being polled — intermittent "connection refused" flake.
        tokio::task::yield_now().await;

        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let store = StoreServiceClient::new(channel.clone());
        let chunk = ChunkServiceClient::new(channel);

        // Seed the default tenant. PutChunk's chunk_tenants insert
        // has an FK to tenants(tenant_id) — a random UUID that's not
        // in the table would fail the insert with a constraint error,
        // which would be a VERY confusing test failure. Seed first.
        let tenant_id = seed_tenant(&db.pool, "chunk-default").await;

        Ok(Self {
            db,
            store,
            chunk,
            backend,
            tenant_id,
            server,
        })
    }
}

/// Insert a tenant row, return its UUID. `tenant_name` is just for
/// human readability in PG if a test fails and you're poking the DB —
/// nothing reads it back.
async fn seed_tenant(pool: &sqlx::PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("tenant seed should never fail on a fresh TestDb")
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
///
/// Tenant-scoped since migration 017: "present" means attributed to
/// THIS tenant in `chunk_tenants`, not just in `chunks`. PutPath's
/// server-side chunker doesn't populate the junction (it dedups via
/// refcount RETURNING, not this RPC), so we attribute manually here.
#[tokio::test]
async fn test_find_missing_chunks() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // Upload via PutPath → chunks exist in `chunks` but NOT in
    // `chunk_tenants` (PutPath attributes at the path level, via
    // path_tenants — migration 012 — not at the chunk level).
    let (nar, info, _) = make_large_nar(61, 512 * 1024);
    put_path(&mut s.store, info, nar).await?;

    let present: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    let absent = vec![0xEE; 32];

    // Attribute `present` to our tenant. Without this, the scoped
    // query would report it missing — `chunks` row exists but
    // `chunk_tenants` row doesn't. The isolation test below proves
    // that's the RIGHT behavior for a foreign tenant's chunk; this
    // test proves it's also the behavior the attribution FIXES.
    sqlx::query("INSERT INTO chunk_tenants (blake3_hash, tenant_id) VALUES ($1, $2)")
        .bind(&present)
        .bind(s.tenant_id)
        .execute(&s.db.pool)
        .await?;

    let resp = s
        .chunk
        .find_missing_chunks(as_tenant(
            s.tenant_id,
            FindMissingChunksRequest {
                digests: vec![present.clone(), absent.clone()],
            },
        ))
        .await?
        .into_inner();

    // Only `absent` should be in missing_digests.
    assert_eq!(resp.missing_digests, vec![absent]);
    assert!(!resp.missing_digests.contains(&present));
    Ok(())
}

/// FindMissingChunks without a tenant header → UNAUTHENTICATED.
/// This is the FAIL-CLOSED branch. Not wrapping with `as_tenant()`
/// means the test interceptor passes through with no Claims.
#[tokio::test]
async fn test_find_missing_chunks_no_tenant_fail_closed() -> TestResult {
    let mut s = ChunkSession::new().await?;

    let result = s
        .chunk
        .find_missing_chunks(FindMissingChunksRequest {
            digests: vec![vec![0x00; 32]],
        })
        .await;
    let status = result.expect_err("no tenant → FAIL-CLOSED");
    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "FAIL-CLOSED: missing Claims must be UNAUTHENTICATED, not a silent \
         unscoped query (cross-tenant leak)"
    );
    assert!(
        status.message().contains("tenant-scoped"),
        "error identifies WHY auth is needed: {}",
        status.message()
    );
    Ok(())
}

/// FindMissingChunks validation: wrong-length digest fails the batch.
#[tokio::test]
async fn test_find_missing_chunks_bad_digest() -> TestResult {
    let mut s = ChunkSession::new().await?;

    // Tenant header set — this test targets the digest-length check,
    // not the auth gate. Without as_tenant() we'd get UNAUTHENTICATED
    // and never reach the validation being tested.
    let result = s
        .chunk
        .find_missing_chunks(as_tenant(
            s.tenant_id,
            FindMissingChunksRequest {
                digests: vec![vec![0x00; 32], vec![0x00; 5]], // second one bad
            },
        ))
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

/// Wrap a PutChunk frame vec into a tenant-stamped streaming Request.
///
/// Tonic's `put_chunk(stream)` auto-wraps the stream via `IntoStreamingRequest`,
/// but that path gives no hook for metadata. `Request::new(stream)` gives
/// us `metadata_mut()`. Same pattern as `put_path_with_token` in main.rs.
///
/// Calls `stamp_tenant` directly (not `as_tenant`) — see the note on
/// `as_tenant` about the `IntoRequest` inference ambiguity when the
/// input is already a `Request`.
fn put_chunk_req(
    tenant_id: Uuid,
    frames: Vec<PutChunkRequest>,
) -> tonic::Request<impl tokio_stream::Stream<Item = PutChunkRequest>> {
    let mut req = tonic::Request::new(tokio_stream::iter(frames));
    stamp_tenant(&mut req, tenant_id);
    req
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
        .find_missing_chunks(as_tenant(
            s.tenant_id,
            FindMissingChunksRequest {
                digests: vec![digest.to_vec()],
            },
        ))
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
        .put_chunk(put_chunk_req(
            s.tenant_id,
            put_chunk_stream(digest, data.clone()),
        ))
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

    // FindMissingChunks now says present. This is the full loop:
    // PutChunk wrote both the `chunks` row (refcount=0 above) AND
    // the `chunk_tenants` junction row. Without the junction row,
    // this would still report missing — the roundtrip closes only
    // if BOTH inserts landed.
    let post = s
        .chunk
        .find_missing_chunks(as_tenant(
            s.tenant_id,
            FindMissingChunksRequest {
                digests: vec![digest.to_vec()],
            },
        ))
        .await?
        .into_inner();
    assert!(
        post.missing_digests.is_empty(),
        "FindMissingChunks sees the chunk after PutChunk — BOTH chunks \
         row AND chunk_tenants junction row must have landed"
    );

    // Ground truth: junction row exists for this tenant.
    let junction: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chunk_tenants WHERE blake3_hash = $1 AND tenant_id = $2",
    )
    .bind(digest.as_slice())
    .bind(s.tenant_id)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(junction, 1, "PutChunk wrote exactly one junction row");

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
        .put_chunk(put_chunk_req(
            s.tenant_id,
            put_chunk_stream(fake_digest, data),
        ))
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
    // valid key). Also check chunk_tenants — the junction insert
    // happens AFTER the hash check, so a rejected upload must
    // leave zero junction rows too.
    let pg_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chunks WHERE blake3_hash = $1 OR blake3_hash = $2",
    )
    .bind(fake_digest.as_slice())
    .bind(real_digest.as_slice())
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(pg_count, 0, "nothing in chunks after rejected PutChunk");

    let junction_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunk_tenants")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(
        junction_count, 0,
        "nothing in chunk_tenants either — junction insert is AFTER hash verify"
    );

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

    let frames = vec![
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

    let result = s.chunk.put_chunk(put_chunk_req(s.tenant_id, frames)).await;
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
    let frames = vec![PutChunkRequest {
        msg: Some(put_chunk_request::Msg::Metadata(PutChunkMetadata {
            digest: vec![0u8; 32],
            size: 1024 * 1024, // 1 MiB — over CHUNK_MAX (256 KiB)
        })),
    }];

    let result = s.chunk.put_chunk(put_chunk_req(s.tenant_id, frames)).await;
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
        .put_chunk(put_chunk_req(
            s.tenant_id,
            put_chunk_stream(digest, data.clone()),
        ))
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

    // Second PutChunk — same bytes, same digest, SAME TENANT. The
    // chunk_tenants insert is ON CONFLICT DO NOTHING on the
    // (blake3_hash, tenant_id) PK, so same-tenant replay is a full
    // no-op (chunks stable, junction stable).
    s.chunk
        .put_chunk(put_chunk_req(s.tenant_id, put_chunk_stream(digest, data)))
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

    // Junction idempotence too: still exactly one row. ON CONFLICT
    // DO NOTHING on the composite PK means the second PutChunk
    // didn't write a duplicate.
    let junction_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chunk_tenants WHERE blake3_hash = $1 AND tenant_id = $2",
    )
    .bind(digest.as_slice())
    .bind(s.tenant_id)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        junction_count, 1,
        "second PutChunk (same tenant, same bytes) doesn't duplicate junction"
    );

    Ok(())
}

// ===========================================================================
// Tenant isolation
// ===========================================================================

/// Tenant A cannot dedup against tenant B's chunks.
///
/// This is the security property the `chunk_tenants` junction exists for.
/// Unscoped dedup would leak one bit per probe: B asks "is hash X
/// present?" and learns whether ANYONE uploaded X. Probe for glibc's
/// known chunk hashes → learn A is building something glibc-linked.
/// Scoped dedup answers only "have I uploaded X?" — no signal about A.
///
/// Four-way assertion:
/// - A probes A's chunk → present (dedup works within a tenant)
/// - B probes A's chunk → MISSING (isolation — the whole point)
/// - B uploads same bytes → junction gets a second row, chunks no-ops
/// - B probes again → now present (B's OWN upload made it visible to B)
///
/// The fourth leg closes the loop: B isn't permanently locked out of
/// the hash, they just have to contribute the bytes themselves. After
/// that, BOTH tenants dedup against one physical `chunks` row via
/// their respective junction rows.
#[tokio::test]
async fn test_find_missing_scoped_by_tenant() -> TestResult {
    let mut s = ChunkSession::new().await?;
    let tenant_a = s.tenant_id;
    let tenant_b = seed_tenant(&s.db.pool, "chunk-tenant-b").await;

    // Precondition: A and B are distinct. Obvious, but if seed_tenant
    // had a bug that returned a fixed UUID, every assertion below
    // would degenerate into the single-tenant roundtrip case and pass
    // trivially. Assert the thing the test depends on.
    assert_ne!(
        tenant_a, tenant_b,
        "precondition: two DISTINCT tenants — test is vacuous otherwise"
    );

    // Deterministic bytes distinct from other tests' fixtures (seed
    // constant 97, not 131/7 used elsewhere in this file). 50 KiB is
    // plausible-real-chunk sized without being slow.
    let data: Vec<u8> = (0..50_000).map(|i| (i as u8).wrapping_mul(97)).collect();
    let digest = *blake3::hash(&data).as_bytes();

    // ── Tenant A uploads. ──
    s.chunk
        .put_chunk(put_chunk_req(
            tenant_a,
            put_chunk_stream(digest, data.clone()),
        ))
        .await?;

    // ── A probes → present. ──
    // Proves dedup works WITHIN a tenant. If this failed, the
    // isolation assertion below would be meaningless — "B sees
    // missing" means nothing if A also sees missing.
    let a_view = s
        .chunk
        .find_missing_chunks(as_tenant(
            tenant_a,
            FindMissingChunksRequest {
                digests: vec![digest.to_vec()],
            },
        ))
        .await?
        .into_inner();
    assert!(
        a_view.missing_digests.is_empty(),
        "tenant A sees its own chunk as present — without this, \
         the isolation assertion below is vacuous"
    );

    // ── B probes the same hash → MISSING. ──
    // THE KEY ASSERTION. Same server, same DB, same `chunks` row —
    // but B's probe returns "missing" because B has no junction row.
    // B learns nothing about what A has uploaded.
    let b_view = s
        .chunk
        .find_missing_chunks(as_tenant(
            tenant_b,
            FindMissingChunksRequest {
                digests: vec![digest.to_vec()],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(
        b_view.missing_digests,
        vec![digest.to_vec()],
        "ISOLATION: tenant B must see A's chunk as MISSING — scoped \
         dedup reveals only 'have I uploaded this', never 'has anyone'"
    );

    // ── Ground truth at this point: one chunks row, one junction row (A). ──
    let (chunks_rows, junction_rows): (i64, i64) = sqlx::query_as(
        "SELECT \
         (SELECT COUNT(*) FROM chunks WHERE blake3_hash = $1), \
         (SELECT COUNT(*) FROM chunk_tenants WHERE blake3_hash = $1)",
    )
    .bind(digest.as_slice())
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        chunks_rows, 1,
        "one physical chunks row (content-addressed)"
    );
    assert_eq!(junction_rows, 1, "one junction row (A's) before B uploads");

    // ── B uploads the same bytes. ──
    // `chunks` insert: ON CONFLICT DO NOTHING (row already exists).
    // `chunk_tenants` insert: fresh row — (digest, B) is a new PK.
    s.chunk
        .put_chunk(put_chunk_req(tenant_b, put_chunk_stream(digest, data)))
        .await?;

    // ── B probes again → now present. ──
    // B's own upload made the chunk visible to B. This is the
    // many-to-many in action: two junction rows, one chunks row.
    let b_view_after = s
        .chunk
        .find_missing_chunks(as_tenant(
            tenant_b,
            FindMissingChunksRequest {
                digests: vec![digest.to_vec()],
            },
        ))
        .await?
        .into_inner();
    assert!(
        b_view_after.missing_digests.is_empty(),
        "after B's own upload, B sees the chunk as present — B isn't \
         permanently locked out, they just had to contribute bytes"
    );

    // ── Final ground truth: still one chunks row, now two junction rows. ──
    // If the junction had been a single tenant_id COLUMN on chunks
    // (the wrong design the audit rejected), B's upload would have
    // either overwritten A's attribution OR been rejected. Two rows
    // here proves the many-to-many actually works.
    let junction_after: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM chunk_tenants WHERE blake3_hash = $1 ORDER BY tenant_id",
    )
    .bind(digest.as_slice())
    .fetch_all(&s.db.pool)
    .await?;
    let mut expected = vec![tenant_a, tenant_b];
    expected.sort();
    assert_eq!(
        junction_after, expected,
        "two junction rows (A + B) → one chunks row: many-to-many works"
    );

    Ok(())
}
