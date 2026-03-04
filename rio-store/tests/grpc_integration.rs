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
    FindMissingPathsRequest, PathInfo, PutPathMetadata, PutPathRequest, QueryPathInfoRequest,
    put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;
use rio_store::backend::chunk::{ChunkBackend, MemoryChunkBackend};
use rio_store::grpc::StoreServiceImpl;
use rio_test_support::fixtures::{make_nar, make_path_info_for_nar};
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
pub async fn put_path_raw(
    client: &mut StoreServiceClient<Channel>,
    info: PathInfo,
    nar: Vec<u8>,
) -> Result<bool, tonic::Status> {
    let (tx, rx) = mpsc::channel(8);

    // Send metadata first
    // Fresh channel with buffer=8, receiver alive: these sends cannot fail.
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

    // Close the stream
    drop(tx);

    let outbound = ReceiverStream::new(rx);
    let response = client.put_path(outbound).await?;
    Ok(response.into_inner().created)
}

#[tokio::test]
async fn test_harness_smoke() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // QueryPathInfo on missing path should return NOT_FOUND
    let result = client
        .query_path_info(QueryPathInfoRequest {
            store_path: "/nix/store/00000000000000000000000000000000-does-not-exist".into(),
        })
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);

    server.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Group 9: Error handling
// ---------------------------------------------------------------------------

/// After a PutPath fails validation (hash mismatch), the placeholder rows
/// should be cleaned up so a retry with the correct data succeeds.
#[tokio::test]
async fn test_put_path_cleanup_on_hash_mismatch() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let store_path = "/nix/store/11111111111111111111111111111111-test-cleanup-path";
    let good_nar = make_nar(b"correct content").0;
    let bad_nar = make_nar(b"wrong content").0;

    // Declare the hash of good_nar but send bad_nar — should fail validation
    let info = make_path_info_for_nar(store_path, &good_nar);
    let result = put_path(&mut client, info.clone(), bad_nar).await;
    assert!(result.is_err(), "hash mismatch should be rejected");

    // Verify no stale rows remain (check via SQL: manifests with status='uploading')
    let stale: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM manifests WHERE status = 'uploading'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(stale, 0, "uploading placeholder should be cleaned up");

    // Retry with correct content should succeed (no unique constraint violation)
    let result = put_path(&mut client, info, good_nar).await;
    assert!(
        result.is_ok(),
        "retry after cleanup should succeed: {result:?}"
    );
    assert!(result?, "should be newly created");

    server.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Group 10: Remaining coverage
// ---------------------------------------------------------------------------

/// PutPath followed by GetPath should return the same NAR content.
#[tokio::test]
async fn test_put_get_roundtrip() -> TestResult {
    use rio_proto::types::{GetPathRequest, get_path_response};

    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let store_path = "/nix/store/22222222222222222222222222222222-test-roundtrip-path";
    let nar = make_nar(b"roundtrip test content!").0;
    let info = make_path_info_for_nar(store_path, &nar);

    // Put
    let created = put_path(&mut client, info.clone(), nar.clone())
        .await
        .context("put should succeed")?;
    assert!(created, "should be newly created");

    // Get
    let mut stream = client
        .get_path(GetPathRequest {
            store_path: store_path.into(),
        })
        .await
        .context("get should succeed")?
        .into_inner();

    let mut got_info = None;
    let mut got_nar = Vec::new();
    while let Some(msg) = stream.message().await? {
        match msg.msg {
            Some(get_path_response::Msg::Info(i)) => got_info = Some(i),
            Some(get_path_response::Msg::NarChunk(chunk)) => got_nar.extend_from_slice(&chunk),
            None => {}
        }
    }

    let got_info = got_info.expect("should receive PathInfo");
    assert_eq!(got_info.store_path, store_path);
    assert_eq!(got_info.nar_hash, info.nar_hash);
    assert_eq!(got_info.nar_size, info.nar_size);
    assert_eq!(got_nar, nar, "NAR content should roundtrip exactly");

    server.abort();
    Ok(())
}

/// GetPath on a path that was never uploaded should return NOT_FOUND.
#[tokio::test]
async fn test_get_path_nonexistent_returns_not_found() -> TestResult {
    use rio_proto::types::GetPathRequest;

    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let result = client
        .get_path(GetPathRequest {
            store_path: "/nix/store/99999999999999999999999999999999-never-uploaded".into(),
        })
        .await;

    assert!(result.is_err(), "nonexistent path should fail GetPath");
    assert_eq!(
        result.unwrap_err().code(),
        tonic::Code::NotFound,
        "should be NOT_FOUND"
    );

    server.abort();
    Ok(())
}

/// GetPath on a corrupted blob (bitrot, disk failure) should stream chunks
/// then send DATA_LOSS at the end. This is the HashingReader integrity check
/// — the NAR's sha256 computed during streaming doesn't match the stored hash.
/// If this check is broken, corrupted NARs would be served silently, causing
/// silent build output corruption.
#[tokio::test]
async fn test_get_path_corrupted_blob_returns_data_loss() -> TestResult {
    use rio_proto::types::{GetPathRequest, get_path_response};

    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // 1. Upload a valid NAR.
    let store_path = "/nix/store/88888888888888888888888888888888-corruption-test";
    let good_nar = make_nar(b"valid content for corruption test").0;
    let info = make_path_info_for_nar(store_path, &good_nar);

    let created = put_path(&mut client, info, good_nar)
        .await
        .context("put should succeed")?;
    assert!(created);

    // 2. Corrupt manifests.inline_blob directly via SQL. Same length so
    // the size check passes; different content so the SHA-256 check fails.
    // This is the phase-2c equivalent of the old backend.corrupt_for_test():
    // simulates TOAST-storage bitrot or manual DB tampering.
    let corrupt_data = vec![0xAAu8; 200]; // garbage, wrong sha256
    sqlx::query(
        "UPDATE manifests SET inline_blob = $1 \
         WHERE store_path_hash = (SELECT store_path_hash FROM narinfo WHERE store_path = $2)",
    )
    .bind(&corrupt_data)
    .bind(store_path)
    .execute(&db.pool)
    .await
    .context("corrupt inline_blob")?;

    // 3. GetPath — stream should deliver chunks then DATA_LOSS at the end.
    let mut stream = client
        .get_path(GetPathRequest {
            store_path: store_path.into(),
        })
        .await
        .context("get_path call should succeed (error comes in stream)")?
        .into_inner();

    let mut got_data_loss = false;
    let mut got_chunks = false;
    loop {
        match stream.message().await {
            Ok(Some(msg)) => {
                if matches!(msg.msg, Some(get_path_response::Msg::NarChunk(_))) {
                    got_chunks = true;
                }
            }
            Ok(None) => break, // stream ended without error — bad if corrupt!
            Err(e) => {
                assert_eq!(
                    e.code(),
                    tonic::Code::DataLoss,
                    "corrupted blob should yield DATA_LOSS, got: {e:?}"
                );
                assert!(
                    e.message().contains("integrity"),
                    "error should mention integrity check: {}",
                    e.message()
                );
                got_data_loss = true;
                break;
            }
        }
    }

    assert!(
        got_chunks,
        "should have received at least one chunk before DATA_LOSS"
    );
    assert!(
        got_data_loss,
        "corrupted blob MUST yield DATA_LOSS at end of stream, not succeed silently"
    );

    server.abort();
    Ok(())
}

/// Second PutPath with same content should return created=false (idempotent).
#[tokio::test]
async fn test_idempotent_put_path() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let store_path = "/nix/store/33333333333333333333333333333333-test-idempotent-path";
    let nar = make_nar(b"idempotent test").0;
    let info = make_path_info_for_nar(store_path, &nar);

    // First put
    let created1 = put_path(&mut client, info.clone(), nar.clone())
        .await
        .context("first put should succeed")?;
    assert!(created1, "first put should create");

    // Second put with same content
    let created2 = put_path(&mut client, info, nar)
        .await
        .context("second put should succeed (idempotent)")?;
    assert!(!created2, "second put should return created=false");

    server.abort();
    Ok(())
}

/// Two concurrent PutPath requests for the same path: exactly one should
/// win (created=true); the other should either see created=false (if it
/// raced after the first completed) or Aborted (if it raced into the
/// in-progress window). Never: both created=true, or the loser's cleanup
/// deleting the winner's placeholder.
#[tokio::test]
async fn test_concurrent_putpath_same_path_one_wins() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client1, server) = setup_store(db.pool.clone()).await?;

    // Second client to the same server so we can send two concurrent streams.
    let mut client2 = client1.clone();

    let store_path = "/nix/store/55555555555555555555555555555555-concurrent-race";
    let nar = make_nar(b"concurrent race test data").0;
    let info = make_path_info_for_nar(store_path, &nar);

    // Launch both PutPath calls concurrently.
    let (r1, r2) = tokio::join!(
        put_path(&mut client1, info.clone(), nar.clone()),
        put_path(&mut client2, info.clone(), nar.clone()),
    );

    // Categorize outcomes.
    let outcomes: Vec<_> = [r1, r2]
        .into_iter()
        .map(|r| match r {
            Ok(true) => "created",
            Ok(false) => "exists",
            Err(e) if e.code() == tonic::Code::Aborted => "aborted",
            Err(e) => panic!("unexpected error: {e:?}"),
        })
        .collect();

    // Exactly one should have created; the other must be exists or aborted.
    let created_count = outcomes.iter().filter(|&&o| o == "created").count();
    assert_eq!(
        created_count, 1,
        "exactly one PutPath should create; got outcomes: {outcomes:?}"
    );

    // The path must be readable after the race settles (winner's data intact).
    let qpi = client1
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.into(),
        })
        .await
        .context("path should be queryable after concurrent uploads")?;
    assert_eq!(qpi.into_inner().nar_size, nar.len() as u64);

    server.abort();
    Ok(())
}

/// PutPath with chunks exceeding declared nar_size should be rejected.
#[tokio::test]
async fn test_put_path_rejects_oversized_nar() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // Declare nar_size=100 but send 100_000 bytes (well over + 4KB tolerance)
    let mut info = make_path_info_for_nar(
        "/nix/store/44444444444444444444444444444444-oversized-test",
        &[0u8; 100],
    );
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
    Ok(())
}

/// Oversized rejection must clean up the uploading placeholder so a retry
/// with correct data succeeds. Regression test for the placeholder leak at
/// the chunk-size-exceeded early return.
#[tokio::test]
async fn test_put_path_oversized_then_retry_succeeds() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let store_path = "/nix/store/66666666666666666666666666666666-oversized-retry";
    let real_nar = make_nar(b"retry test data").0;
    let real_info = make_path_info_for_nar(store_path, &real_nar);

    // First attempt: lie about size, send oversized data → rejected.
    let mut bad_info = real_info.clone();
    bad_info.nar_size = 100;
    let oversized = vec![0u8; 100_000];
    let r1 = put_path(&mut client, bad_info, oversized).await;
    assert!(r1.is_err(), "oversized must be rejected");
    assert_eq!(r1.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Second attempt: correct data. Must succeed (placeholder was cleaned up).
    let r2 = put_path(&mut client, real_info, real_nar.clone())
        .await
        .context("retry with correct data must succeed after oversized rejection")?;
    assert!(r2, "retry should create");

    // Verify the path is queryable.
    let qpi = client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.into(),
        })
        .await
        .context("path should be queryable")?;
    assert_eq!(qpi.into_inner().nar_size, real_nar.len() as u64);

    server.abort();
    Ok(())
}

/// Duplicate metadata mid-stream is a protocol violation and must be rejected
/// (not silently ignored).
#[tokio::test]
async fn test_put_path_rejects_duplicate_metadata() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let store_path = "/nix/store/77777777777777777777777777777777-dup-metadata";
    let nar = make_nar(b"dup metadata test").0;
    let info = make_path_info_for_nar(store_path, &nar);

    let (tx, rx) = mpsc::channel(8);
    // Metadata #1
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info.clone().into()),
        })),
    })
    .await?;
    // Metadata #2 (protocol violation)
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info.into()),
        })),
    })
    .await?;
    // Chunk (never read — server should reject before this)
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await?;
    drop(tx);

    let result = client.put_path(ReceiverStream::new(rx)).await;
    assert!(result.is_err(), "duplicate metadata must be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("duplicate metadata"),
        "error should mention duplicate metadata: {}",
        status.message()
    );

    server.abort();
    Ok(())
}

/// PutPath with absurdly large declared nar_size should be rejected BEFORE
/// allocation (prevents OOM from malicious clients).
#[tokio::test]
async fn test_put_path_rejects_absurd_nar_size() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let mut info = make_path_info_for_nar(
        "/nix/store/55555555555555555555555555555555-absurd-size-test",
        &[0u8; 10],
    );
    info.nar_size = u64::MAX; // Attempt to trigger huge Vec::with_capacity

    let result = put_path(&mut client, info, vec![0u8; 10]).await;

    // Must be rejected promptly — no hang, no crash.
    assert!(result.is_err(), "u64::MAX nar_size should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("exceeds maximum"),
        "error should mention size limit: {}",
        status.message()
    );

    server.abort();
    Ok(())
}

/// PutPath with more than MAX_REFERENCES entries should be rejected.
#[tokio::test]
async fn test_put_path_rejects_excessive_references() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // We're testing SERVER-SIDE rejection of too-many-references. Build raw
    // PathInfo directly since ValidatedPathInfo can't hold 10,001 unparsed
    // string references (client-side TryFrom would reject first).
    let nar = make_nar(b"refs-test").0;
    let base: PathInfo = make_path_info_for_nar(
        "/nix/store/66666666666666666666666666666666-too-many-refs",
        &nar,
    )
    .into();
    let info = PathInfo {
        // MAX_REFERENCES = 10_000; send 10_001 to trigger the check.
        // Each ref is a VALID store path (TryFrom would accept them); the
        // server's check_bound fires on COUNT, not on per-ref syntax.
        references: (0..10_001)
            .map(|i| format!("/nix/store/{:032}-ref-{i}", i % 10))
            .collect(),
        ..base
    };

    let result = put_path_raw(&mut client, info, nar).await;
    assert!(result.is_err(), "10,001 references should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("too many references"),
        "error should mention reference limit: {}",
        status.message()
    );

    server.abort();
    Ok(())
}

/// PutPath with a malformed reference path should be rejected.
#[tokio::test]
async fn test_put_path_rejects_malformed_reference() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // Testing SERVER-SIDE rejection: build raw PathInfo with a garbage ref.
    let nar = make_nar(b"refs-test").0;
    let base: PathInfo =
        make_path_info_for_nar("/nix/store/77777777777777777777777777777777-bad-ref", &nar).into();
    let info = PathInfo {
        references: vec!["not-a-valid-store-path".into()],
        ..base
    };

    let result = put_path_raw(&mut client, info, nar).await;
    assert!(result.is_err(), "malformed reference should be rejected");
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);

    server.abort();
    Ok(())
}

/// Malformed store paths (no 32-char hash prefix, traversal attempts) should
/// be rejected with INVALID_ARGUMENT at the RPC boundary.
#[tokio::test]
async fn test_rejects_malformed_store_paths() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let bad_paths = [
        "/nix/store/too-short",     // no 32-char hash
        "/nix/store/../etc/passwd", // traversal
        "not-a-store-path",         // no /nix/store/ prefix
        "",                         // empty
    ];

    for path in bad_paths {
        let result = client
            .query_path_info(QueryPathInfoRequest {
                store_path: path.into(),
            })
            .await;
        assert!(result.is_err(), "path {path:?} should be rejected");
        assert_eq!(
            result.unwrap_err().code(),
            tonic::Code::InvalidArgument,
            "path {path:?} should return INVALID_ARGUMENT"
        );
    }

    server.abort();
    Ok(())
}

/// FindMissingPaths with > MAX_BATCH_PATHS entries should be rejected.
#[tokio::test]
async fn test_find_missing_paths_rejects_oversized_batch() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // 10_001 paths (one over the limit).
    let paths: Vec<String> = (0..10_001)
        .map(|i| {
            format!(
                "/nix/store/{:032}-path-{}",
                i % 100_000_000_000_000_000_000_000_000_000_000u128,
                i
            )
        })
        .collect();

    let result = client
        .find_missing_paths(FindMissingPathsRequest { store_paths: paths })
        .await;

    assert!(result.is_err(), "oversized batch should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("too many paths"),
        "error should mention path limit: {}",
        status.message()
    );

    server.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Error-branch coverage: internal_error, blob-missing paths
// ---------------------------------------------------------------------------

/// A connection-level failure (PoolClosed) must surface as UNAVAILABLE
/// (retriable), NOT Internal. This is the key value-add of MetadataError:
/// before C4, a transient PG hiccup looked identical to a corrupt database.
///
/// Secondary: the message must not leak sqlx/Postgres internals. Server
/// logs the full error; client sees a generic summary.
#[tokio::test]
async fn test_connection_error_is_unavailable_and_hides_sqlx_details() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // Close the pool so the next DB query fails with a sqlx::Error::PoolClosed.
    db.pool.close().await;

    let result = client
        .query_path_info(QueryPathInfoRequest {
            store_path: "/nix/store/00000000000000000000000000000000-valid-name".into(),
        })
        .await;

    let status = result.expect_err("should fail on closed pool");
    // PoolClosed → MetadataError::Connection → UNAVAILABLE. Client should
    // retry with backoff. NOT Internal — that would look like corruption.
    assert_eq!(
        status.code(),
        tonic::Code::Unavailable,
        "connection-level failures must be retriable (was Internal before C4)"
    );
    // Belt-and-suspenders: no substring from common sqlx errors.
    assert!(!status.message().to_lowercase().contains("sqlx"));
    assert!(!status.message().to_lowercase().contains("postgres"));
    assert!(!status.message().to_lowercase().contains("pool"));

    server.abort();
    Ok(())
}

// Note: the "manifest not found for" branch in GetPath is defense-in-depth
// for a race between query_path_info and get_manifest (both filter on
// manifests.status='complete'). Not normally reachable; no test.
//
// Phase 2c: the old test_get_path_backend_blob_missing is gone. That test
// exercised the scenario where nar_blobs had a complete row but the S3/fs
// backend had lost the blob (crash between backend.put and complete_upload).
// With inline storage, the NAR lives in the SAME transaction that flips
// status to complete — that race no longer exists. The chunked path (C3)
// reintroduces a similar race (PG manifest present, S3 chunk missing); a
// test for that lands with C3.

// ===========================================================================
// PutPathTrailer tests (phase2b C14 — hash-trailer upload mode)
// ===========================================================================

use rio_proto::types::PutPathTrailer;
use rio_test_support::fixtures::test_store_path;

/// Build a (nar, ValidatedPathInfo) pair for trailer tests.
fn trailer_fixture(name: &str) -> (Vec<u8>, ValidatedPathInfo) {
    let (nar, _hash) = make_nar(name.as_bytes());
    let store_path = test_store_path(name);
    let info = make_path_info_for_nar(&store_path, &nar);
    (nar, info)
}

/// Helper for trailer-mode uploads: send metadata with EMPTY nar_hash/size,
/// then chunks, then a PutPathTrailer with the real hash/size.
async fn put_path_trailer_mode(
    client: &mut StoreServiceClient<Channel>,
    info_without_hash: PathInfo,
    nar: Vec<u8>,
    trailer: PutPathTrailer,
) -> Result<bool, tonic::Status> {
    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info_without_hash),
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
    let resp = client.put_path(ReceiverStream::new(rx)).await?;
    Ok(resp.into_inner().created)
}

/// Hash-upfront mode (existing behavior) still works — backward-compat guard.
/// Trailer not sent; metadata has the real hash.
#[tokio::test]
async fn test_trailer_hash_upfront_mode_unchanged() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let (nar, info) = trailer_fixture("upfront-mode");

    let created = put_path(&mut client, info.clone(), nar.clone()).await?;
    assert!(created, "hash-upfront upload should succeed");

    let got = client
        .query_path_info(QueryPathInfoRequest {
            store_path: info.store_path.to_string(),
        })
        .await?
        .into_inner();
    assert_eq!(got.nar_hash, info.nar_hash.to_vec());

    server.abort();
    Ok(())
}

/// Hash-trailer mode: empty metadata hash + correct trailer → success.
/// Stored hash/size come from the trailer, not the (zero-filled) metadata.
#[tokio::test]
async fn test_trailer_mode_correct_hash_succeeds() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let (nar, info) = trailer_fixture("trailer-ok");

    // Metadata with EMPTY hash/size — triggers trailer mode.
    let mut raw: PathInfo = info.clone().into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let created = put_path_trailer_mode(
        &mut client,
        raw,
        nar.clone(),
        PutPathTrailer {
            nar_hash: info.nar_hash.to_vec(),
            nar_size: info.nar_size,
        },
    )
    .await?;
    assert!(created);

    // The STORED hash must be the trailer's (real) hash, not the zero
    // placeholder. This is the key assertion: server-side info.nar_hash
    // was overwritten correctly before complete_upload.
    let got = client
        .query_path_info(QueryPathInfoRequest {
            store_path: info.store_path.to_string(),
        })
        .await?
        .into_inner();
    assert_eq!(
        got.nar_hash,
        info.nar_hash.to_vec(),
        "stored hash should be the trailer's hash, not the zero placeholder"
    );
    assert_eq!(got.nar_size, info.nar_size);

    server.abort();
    Ok(())
}

/// Trailer with WRONG hash → InvalidArgument (hash mismatch).
#[tokio::test]
async fn test_trailer_mode_wrong_hash_rejected() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let (nar, info) = trailer_fixture("trailer-bad-hash");

    let mut raw: PathInfo = info.clone().into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let result = put_path_trailer_mode(
        &mut client,
        raw,
        nar.clone(),
        PutPathTrailer {
            nar_hash: vec![0xFFu8; 32], // WRONG
            nar_size: info.nar_size,
        },
    )
    .await;

    let status = result.expect_err("wrong trailer hash should fail validation");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("hash mismatch"),
        "got: {}",
        status.message()
    );

    // Placeholder cleaned up.
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM manifests")
        .fetch_one(&db.pool)
        .await
        .context("count manifests")?;
    assert_eq!(count.0, 0, "abort_upload should clean up placeholder");

    server.abort();
    Ok(())
}

/// Empty metadata hash + NO trailer → InvalidArgument.
#[tokio::test]
async fn test_trailer_mode_missing_trailer_rejected() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let (nar, info) = trailer_fixture("no-trailer");

    let mut raw: PathInfo = info.into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let result = put_path_raw(&mut client, raw, nar).await;
    let status = result.expect_err("missing trailer should fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("trailer"), "{}", status.message());

    server.abort();
    Ok(())
}

/// Trailer nar_hash ≠ 32 bytes → InvalidArgument.
#[tokio::test]
async fn test_trailer_mode_bad_hash_length_rejected() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let (nar, info) = trailer_fixture("bad-hash-len");

    let mut raw: PathInfo = info.into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let result = put_path_trailer_mode(
        &mut client,
        raw,
        nar.clone(),
        PutPathTrailer {
            nar_hash: vec![0u8; 20], // SHA-1 length, not SHA-256
            nar_size: nar.len() as u64,
        },
    )
    .await;

    let status = result.expect_err("20-byte hash should fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("32 bytes"),
        "{}",
        status.message()
    );

    server.abort();
    Ok(())
}

/// Chunk AFTER trailer → protocol violation.
#[tokio::test]
async fn test_trailer_chunk_after_trailer_rejected() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let (nar, info) = trailer_fixture("late-chunk");

    let mut raw: PathInfo = info.clone().into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar.clone())),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(PutPathTrailer {
            nar_hash: info.nar_hash.to_vec(),
            nar_size: info.nar_size,
        })),
    })
    .await
    .expect("fresh channel");
    // The violation: more bytes AFTER the trailer.
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(b"sneaky".to_vec())),
    })
    .await
    .expect("fresh channel");
    drop(tx);

    let result = client.put_path(ReceiverStream::new(rx)).await;
    let status = result.expect_err("chunk after trailer should fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("trailer must be last"),
        "{}",
        status.message()
    );

    server.abort();
    Ok(())
}

/// Metadata has real hash AND trailer sent → trailer IGNORED.
/// If the garbage trailer were used, validation would fail. Success proves
/// it was ignored (backward-compat contract).
#[tokio::test]
async fn test_trailer_ignored_when_metadata_has_hash() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let (nar, info) = trailer_fixture("both-hashes");

    let raw: PathInfo = info.clone().into(); // REAL hash in metadata

    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar.clone())),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(PutPathTrailer {
            nar_hash: vec![0xDEu8; 32], // garbage — would fail if used
            nar_size: 999_999,
        })),
    })
    .await
    .expect("fresh channel");
    drop(tx);

    let resp = client.put_path(ReceiverStream::new(rx)).await?;
    assert!(
        resp.into_inner().created,
        "garbage trailer should be IGNORED when metadata has real hash"
    );

    server.abort();
    Ok(())
}

// ===========================================================================
// QueryPathFromHashPart + AddSignatures (phase2c E2)
// ===========================================================================

use rio_proto::types::{AddSignaturesRequest, QueryPathFromHashPartRequest};

/// QueryPathFromHashPart: put a path, look it up by its 32-char hash part.
#[tokio::test]
async fn test_query_path_from_hash_part_found() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // Store path with a known nixbase32 hash-part. All-'a' is valid nixbase32.
    let hash_part = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let store_path = format!("/nix/store/{hash_part}-hashpart-test");
    let nar = make_nar(b"hash part test").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut client, info, nar).await?;

    let resp = client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: hash_part.into(),
        })
        .await
        .context("should find by hash part")?;
    assert_eq!(resp.into_inner().store_path, store_path);

    server.abort();
    Ok(())
}

/// QueryPathFromHashPart: unknown hash → NOT_FOUND.
#[tokio::test]
async fn test_query_path_from_hash_part_not_found() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let result = client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: "00000000000000000000000000000000".into(),
        })
        .await;
    let status = result.expect_err("should be NOT_FOUND");
    assert_eq!(status.code(), tonic::Code::NotFound);

    server.abort();
    Ok(())
}

/// QueryPathFromHashPart: validation rejects wrong length and non-nixbase32.
/// These flow into a LIKE pattern — without validation, `%` would be an
/// injection. The test proves the validator runs BEFORE the query.
#[tokio::test]
async fn test_query_path_from_hash_part_validation() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // Wrong length (INVALID_ARGUMENT, not NOT_FOUND).
    let short = client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: "short".into(),
        })
        .await
        .expect_err("short should be invalid");
    assert_eq!(short.code(), tonic::Code::InvalidArgument);
    assert!(
        short.message().contains("32 chars"),
        "got: {}",
        short.message()
    );

    // LIKE-injection attempt: `%` is not nixbase32. If validation didn't
    // catch this, the LIKE `/nix/store/%%%-%` would match EVERYTHING.
    // Getting INVALID_ARGUMENT here (not a spurious match or NOT_FOUND)
    // proves the charset check fires.
    let inject = client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: "%".repeat(32),
        })
        .await
        .expect_err("% should be invalid");
    assert_eq!(inject.code(), tonic::Code::InvalidArgument);
    assert!(
        inject.message().contains("nixbase32"),
        "got: {}",
        inject.message()
    );

    server.abort();
    Ok(())
}

/// AddSignatures: append, re-query, verify sigs are persisted.
#[tokio::test]
async fn test_add_signatures_roundtrip() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let store_path = test_store_path("addsig-roundtrip");
    let nar = make_nar(b"addsig test").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut client, info, nar).await?;

    // Append two sigs.
    let sigs = vec![
        "cache.example.org-1:SIGNATURE_A".to_string(),
        "cache.example.org-1:SIGNATURE_B".to_string(),
    ];
    client
        .add_signatures(AddSignaturesRequest {
            store_path: store_path.clone(),
            signatures: sigs.clone(),
        })
        .await
        .context("add_signatures should succeed")?;

    // Re-query: sigs persisted.
    let info = client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(
        info.signatures, sigs,
        "signatures should be persisted and returned"
    );

    // Append a third — verifies `signatures || $2` concatenates, not replaces.
    client
        .add_signatures(AddSignaturesRequest {
            store_path: store_path.clone(),
            signatures: vec!["cache.example.org-1:SIGNATURE_C".to_string()],
        })
        .await?;
    let info = client
        .query_path_info(QueryPathInfoRequest { store_path })
        .await?
        .into_inner();
    assert_eq!(info.signatures.len(), 3, "third sig should be appended");
    assert_eq!(info.signatures[2], "cache.example.org-1:SIGNATURE_C");

    server.abort();
    Ok(())
}

/// AddSignatures on unknown path → NOT_FOUND.
#[tokio::test]
async fn test_add_signatures_not_found() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let result = client
        .add_signatures(AddSignaturesRequest {
            store_path: test_store_path("addsig-missing"),
            signatures: vec!["cache.example.org-1:SIG".to_string()],
        })
        .await;
    let status = result.expect_err("should be NOT_FOUND");
    assert_eq!(status.code(), tonic::Code::NotFound);

    server.abort();
    Ok(())
}

/// AddSignatures with empty list is a no-op (not an error). Happens when
/// `nix store sign` runs with no configured signing keys.
#[tokio::test]
async fn test_add_signatures_empty_is_noop() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    // Path doesn't exist, but empty-sigs short-circuits before the DB
    // query — so NOT_FOUND is not returned. This is intentional: the
    // no-op should be cheap, not a PG roundtrip just to tell the client
    // "yes, you successfully did nothing to a path that doesn't exist".
    client
        .add_signatures(AddSignaturesRequest {
            store_path: test_store_path("addsig-empty"),
            signatures: vec![],
        })
        .await
        .context("empty sigs should be OK")?;

    server.abort();
    Ok(())
}

// ===========================================================================
// Chunked CAS (phase2c C3)
// ===========================================================================

/// Build a NAR of roughly `size` bytes. Content is pseudo-random enough
/// for FastCDC to find boundaries but deterministic for reproducible tests.
///
/// NOT using make_nar (that wraps a tiny payload in NAR framing). We want
/// a big payload. The NAR framing adds ~100 bytes; payload dominates.
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

/// Small NAR + chunked backend: should STILL go inline (under threshold).
#[tokio::test]
async fn test_chunked_small_nar_stays_inline() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, backend, server) = setup_store_chunked(db.pool.clone()).await?;

    let store_path = test_store_path("chunked-small");
    let nar = make_nar(b"tiny").0;
    let info = make_path_info_for_nar(&store_path, &nar);

    let created = put_path(&mut client, info, nar).await?;
    assert!(created);

    // Chunk backend should be empty — went inline.
    assert!(
        backend.is_empty(),
        "small NAR should not reach chunk backend"
    );

    // Manifest should be Inline (has inline_blob).
    let inline_blob: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&db.pool)
    .await?;
    assert!(inline_blob.is_some(), "small NAR should have inline_blob");

    server.abort();
    Ok(())
}

/// Large NAR: chunked path activates. Backend gets chunks, inline_blob NULL,
/// manifest_data populated.
#[tokio::test]
async fn test_chunked_large_nar_chunks() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, backend, server) = setup_store_chunked(db.pool.clone()).await?;

    // 1 MiB — well over INLINE_THRESHOLD (256 KiB).
    let (nar, info, store_path) = make_large_nar(1, 1024 * 1024);

    let created = put_path(&mut client, info, nar).await?;
    assert!(created);

    // Chunk backend should have chunks (1 MiB / 64 KiB avg ≈ 16).
    let chunk_count = backend.len();
    assert!(
        chunk_count > 0,
        "large NAR should reach chunk backend, got {chunk_count} chunks"
    );
    assert!(
        chunk_count > 4,
        "1 MiB at 64 KiB avg should be >4 chunks, got {chunk_count}"
    );

    // inline_blob should be NULL (chunked marker).
    let inline_blob: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&db.pool)
    .await?;
    assert!(
        inline_blob.is_none(),
        "chunked NAR should have NULL inline_blob"
    );

    // manifest_data should exist.
    let md_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifest_data md JOIN narinfo n \
         ON md.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(md_count, 1, "manifest_data row should exist");

    // chunks table refcounts all == 1 (first upload).
    let refcounts: Vec<(i32,)> = sqlx::query_as("SELECT refcount FROM chunks")
        .fetch_all(&db.pool)
        .await?;
    assert_eq!(refcounts.len(), chunk_count);
    for (rc,) in &refcounts {
        assert_eq!(*rc, 1, "first upload: all refcounts should be 1");
    }

    server.abort();
    Ok(())
}

/// The dedup test: upload two large NARs that share most of their content.
/// The second upload should skip most chunks (backend chunk count should
/// NOT double).
#[tokio::test]
async fn test_chunked_dedup_across_uploads() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, backend, server) = setup_store_chunked(db.pool.clone()).await?;

    // Two NARs with IDENTICAL payloads (seed=5 both times). Different
    // store paths (so they're different PutPath calls) but same content
    // → same chunks → 100% dedup on the second upload.
    //
    // In practice two store paths with the same NAR content would be a
    // weird nixpkgs thing (two fetchurl of the same file), but it DOES
    // happen, and it's the clearest dedup test.
    let (nar_a, info_a, _) = make_large_nar(5, 1024 * 1024);
    put_path(&mut client, info_a, nar_a).await?;
    let chunks_after_a = backend.len();
    assert!(chunks_after_a > 4);

    // Second NAR: same seed = same payload = same chunks.
    // Different store_path (different name arg) so it's a fresh PutPath.
    let payload: Vec<u8> = (0u64..1024 * 1024)
        .map(|i| (i.wrapping_mul(7919).wrapping_add(5) % 251) as u8)
        .collect();
    let (nar_b, _) = make_nar(&payload);
    let path_b = test_store_path("large-nar-5-dup");
    let info_b = make_path_info_for_nar(&path_b, &nar_b);

    put_path(&mut client, info_b, nar_b).await?;
    let chunks_after_b = backend.len();

    // THE dedup assertion: chunk count should NOT have doubled.
    // Identical payloads → identical chunks → zero new uploads.
    assert_eq!(
        chunks_after_b, chunks_after_a,
        "identical content should dedup 100%: {chunks_after_a} chunks after A, \
         {chunks_after_b} after B (should be equal)"
    );

    // Refcounts should all be 2 (both manifests reference every chunk).
    let refcounts: Vec<(i32,)> = sqlx::query_as("SELECT refcount FROM chunks")
        .fetch_all(&db.pool)
        .await?;
    for (rc,) in &refcounts {
        assert_eq!(*rc, 2, "two uploads of same content: refcount should be 2");
    }

    server.abort();
    Ok(())
}

/// Idempotent PutPath for chunked: second upload of same store path returns
/// created=false, doesn't touch chunks.
#[tokio::test]
async fn test_chunked_idempotent() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, backend, server) = setup_store_chunked(db.pool.clone()).await?;

    let (nar, info, _) = make_large_nar(7, 512 * 1024);

    let first = put_path(&mut client, info.clone(), nar.clone()).await?;
    assert!(first);
    let chunks_first = backend.len();

    // Same path again: idempotency short-circuits at check_manifest_complete,
    // before any chunking happens.
    let second = put_path(&mut client, info, nar).await?;
    assert!(!second, "second PutPath should return created=false");
    assert_eq!(
        backend.len(),
        chunks_first,
        "idempotent PutPath should not touch chunks"
    );

    server.abort();
    Ok(())
}

/// Hash mismatch rollback: send a large NAR declaring the WRONG hash.
/// Validation fails → abort_upload. Verify: no manifest_data, no chunks,
/// refcounts untouched.
///
/// This exercises the OLD abort path (pre-chunking) — the validation
/// failure happens at step 5, BEFORE put_chunked is called. So this is
/// really testing that the inline abort path still works for large NARs.
#[tokio::test]
async fn test_chunked_hash_mismatch_no_leaked_state() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, backend, server) = setup_store_chunked(db.pool.clone()).await?;

    let (_good_nar, good_info, _) = make_large_nar(9, 512 * 1024);
    let (bad_nar, _, _) = make_large_nar(10, 512 * 1024);

    // Declare good_nar's hash, send bad_nar → validation fails.
    let result = put_path(&mut client, good_info, bad_nar).await;
    assert!(result.is_err(), "hash mismatch should be rejected");

    // No leaked state: chunks empty, no manifest rows.
    assert!(backend.is_empty());
    let mf_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM manifests")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(mf_count, 0);
    let chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(chunk_count, 0);

    // Retry with correct NAR succeeds.
    let (good_nar, info, _) = make_large_nar(9, 512 * 1024);
    let retry = put_path(&mut client, info, good_nar).await?;
    assert!(retry);

    server.abort();
    Ok(())
}

// ===========================================================================
// Chunked GetPath reassembly (phase2c C5)
// ===========================================================================

/// THE roundtrip test: chunked PutPath → GetPath reassembles the exact
/// same bytes. If this passes, the whole chunker → manifest → backend →
/// cache → buffered prefetch → SHA-256 verify chain is correct.
#[tokio::test]
async fn test_chunked_roundtrip() -> TestResult {
    use rio_proto::types::{GetPathRequest, get_path_response};

    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, _backend, server) = setup_store_chunked(db.pool.clone()).await?;

    // 1 MiB NAR — well over INLINE_THRESHOLD, chunks into ~16 pieces.
    let (nar, info, store_path) = make_large_nar(42, 1024 * 1024);
    let original = nar.clone();

    put_path(&mut client, info, nar).await?;

    // GetPath it back.
    let mut stream = client
        .get_path(GetPathRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();

    let mut reassembled = Vec::with_capacity(original.len());
    let mut got_info = false;
    while let Some(msg) = stream.message().await? {
        match msg.msg {
            Some(get_path_response::Msg::Info(_)) => {
                got_info = true;
            }
            Some(get_path_response::Msg::NarChunk(chunk)) => {
                reassembled.extend_from_slice(&chunk);
            }
            None => {}
        }
    }

    assert!(got_info, "first message should be PathInfo");
    // Byte-for-byte: if buffered() was buffer_unordered(), chunks would
    // arrive scrambled and this would fail (different bytes, same length).
    assert_eq!(
        reassembled, original,
        "reassembled NAR must match original byte-for-byte"
    );

    server.abort();
    Ok(())
}

/// Chunked GetPath with a missing chunk: backend has the manifest but
/// one chunk is gone (simulating S3 losing an object). Should DATA_LOSS,
/// not hang or silently truncate.
#[tokio::test]
async fn test_chunked_getpath_missing_chunk_data_loss() -> TestResult {
    use rio_proto::types::GetPathRequest;

    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, backend, server) = setup_store_chunked(db.pool.clone()).await?;

    let (nar, info, store_path) = make_large_nar(43, 512 * 1024);
    put_path(&mut client, info, nar).await?;

    // Corrupt one chunk (delete would be cleaner but MemoryChunkBackend
    // doesn't expose delete; corrupt achieves the same thing — BLAKE3
    // verify fails, which is handled the same as NotFound: both produce
    // ChunkError → DATA_LOSS).
    //
    // Pick ANY chunk from the backend. corrupt_for_test needs the hash;
    // grab it from the chunks table.
    let one_hash: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    let hash_arr: [u8; 32] = one_hash.as_slice().try_into()?;
    backend.corrupt_for_test(&hash_arr, bytes::Bytes::from_static(b"CORRUPTED"));

    // GetPath: should produce DATA_LOSS mid-stream.
    let mut stream = client
        .get_path(GetPathRequest { store_path })
        .await?
        .into_inner();

    let mut got_data_loss = false;
    loop {
        match stream.message().await {
            Ok(Some(_)) => {}  // PathInfo or some chunks before the corrupt one
            Ok(None) => break, // stream ended clean — bad!
            Err(e) => {
                assert_eq!(
                    e.code(),
                    tonic::Code::DataLoss,
                    "corrupt chunk should yield DATA_LOSS, got: {e:?}"
                );
                got_data_loss = true;
                break;
            }
        }
    }
    assert!(
        got_data_loss,
        "missing/corrupt chunk MUST yield DATA_LOSS, not clean stream end"
    );

    server.abort();
    Ok(())
}

/// Inline-only store + chunked manifest: fail clearly at pre-flight,
/// not deep in the spawned task with no context.
#[tokio::test]
async fn test_chunked_manifest_no_cache_preflight_fails() -> TestResult {
    use rio_proto::types::GetPathRequest;

    let db = TestDb::new(&MIGRATOR).await;

    // First: use a CHUNKED store to write a chunked path.
    {
        let (mut client, _backend, server) = setup_store_chunked(db.pool.clone()).await?;
        let (nar, info, _) = make_large_nar(44, 512 * 1024);
        put_path(&mut client, info, nar).await?;
        server.abort();
    }

    // Second: use an INLINE-ONLY store (same PG) to try reading it.
    // Simulates a misconfigured deployment where one instance wrote
    // chunked and another can't read it.
    let (mut client, server) = setup_store(db.pool.clone()).await?;
    let store_path = test_store_path("large-nar-44");

    let result = client.get_path(GetPathRequest { store_path }).await;
    let status = result.expect_err("should fail at pre-flight");
    assert_eq!(
        status.code(),
        tonic::Code::FailedPrecondition,
        "should be FAILED_PRECONDITION (clear config error), got: {status:?}"
    );
    assert!(
        status.message().contains("chunk backend"),
        "message should explain the config issue: {}",
        status.message()
    );

    server.abort();
    Ok(())
}

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

// ===========================================================================
// narinfo signing (phase2c B2)
// ===========================================================================

use rio_store::signing::Signer;

/// Known seed → known keypair. The test verifies against the derived pubkey.
const TEST_SIGNING_SEED: [u8; 32] = [0x77; 32];

fn make_test_signer() -> Signer {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(TEST_SIGNING_SEED);
    Signer::parse(&format!("test.cache.org-1:{b64}")).expect("valid key string")
}

/// PutPath with a signer: signature lands in narinfo.signatures and is
/// cryptographically valid. This is the end-to-end signing test — proves
/// maybe_sign() runs, the fingerprint is correct, and the sig verifies.
#[tokio::test]
async fn test_putpath_signs_narinfo() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store_with_signer(db.pool.clone(), make_test_signer()).await?;

    let store_path = test_store_path("signed-path");
    let nar = make_nar(b"content for signing").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    // Keep nar_hash + nar_size for fingerprint reconstruction below.
    let expected_hash = info.nar_hash;
    let expected_size = info.nar_size;

    put_path(&mut client, info, nar).await?;

    // Fetch back via QueryPathInfo — the sig should be in signatures.
    let fetched = client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();

    assert_eq!(fetched.signatures.len(), 1, "should have exactly one sig");
    let sig_str = &fetched.signatures[0];
    assert!(
        sig_str.starts_with("test.cache.org-1:"),
        "sig should be prefixed with key name: {sig_str}"
    );

    // Verify the signature cryptographically. This is what a Nix client
    // does: reconstruct the fingerprint from narinfo fields, verify
    // against the pubkey from trusted-public-keys.
    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};

    let pubkey = SigningKey::from_bytes(&TEST_SIGNING_SEED).verifying_key();

    // Reconstruct the fingerprint. No refs in this test (make_path_info
    // produces empty references).
    let fp = rio_nix::narinfo::fingerprint(&store_path, &expected_hash, expected_size, &[]);

    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;

    pubkey
        .verify(fp.as_bytes(), &Signature::from_bytes(&sig_arr))
        .context("signature must verify against reconstructed fingerprint")?;

    server.abort();
    Ok(())
}

/// Without a signer: PutPath still works, signatures just empty.
/// The baseline test — signing is optional.
#[tokio::test]
async fn test_putpath_no_signer_no_sig() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Regular setup_store (no signer).
    let (mut client, server) = setup_store(db.pool.clone()).await?;

    let store_path = test_store_path("unsigned-path");
    let nar = make_nar(b"unsigned content").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut client, info, nar).await?;

    let fetched = client
        .query_path_info(QueryPathInfoRequest { store_path })
        .await?
        .into_inner();
    assert!(
        fetched.signatures.is_empty(),
        "no signer → no signature added"
    );

    server.abort();
    Ok(())
}

/// Signature includes references in the fingerprint. A path with refs
/// produces a DIFFERENT signature than the same path without refs —
/// proves the fingerprint actually covers references (not just path+hash).
#[tokio::test]
async fn test_putpath_sig_covers_references() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, server) = setup_store_with_signer(db.pool.clone(), make_test_signer()).await?;

    // Upload a dependency first (so the ref is a valid path).
    let dep_path = test_store_path("sig-dep");
    let dep_nar = make_nar(b"dependency").0;
    let dep_info = make_path_info_for_nar(&dep_path, &dep_nar);
    put_path(&mut client, dep_info, dep_nar).await?;

    // Upload the main path WITH a reference.
    let main_path = test_store_path("sig-main");
    let main_nar = make_nar(b"main content").0;
    let mut main_info = make_path_info_for_nar(&main_path, &main_nar);
    main_info.references =
        vec![rio_nix::store_path::StorePath::parse(&dep_path).context("parse dep")?];
    let main_hash = main_info.nar_hash;
    let main_size = main_info.nar_size;
    put_path(&mut client, main_info, main_nar).await?;

    // Fetch and verify. The fingerprint MUST include dep_path in refs.
    let fetched = client
        .query_path_info(QueryPathInfoRequest {
            store_path: main_path.clone(),
        })
        .await?
        .into_inner();
    let sig_str = &fetched.signatures[0];

    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};
    let pubkey = SigningKey::from_bytes(&TEST_SIGNING_SEED).verifying_key();

    // Fingerprint WITH the ref. slice::from_ref avoids a clone (clippy
    // is right — fingerprint only borrows).
    let fp_with_ref = rio_nix::narinfo::fingerprint(
        &main_path,
        &main_hash,
        main_size,
        std::slice::from_ref(&dep_path),
    );
    // Fingerprint WITHOUT the ref (wrong).
    let fp_no_ref = rio_nix::narinfo::fingerprint(&main_path, &main_hash, main_size, &[]);

    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;
    let sig = Signature::from_bytes(&sig_arr);

    // Must verify against fingerprint WITH ref.
    pubkey
        .verify(fp_with_ref.as_bytes(), &sig)
        .context("should verify against fingerprint WITH ref")?;
    // Must NOT verify against fingerprint WITHOUT ref. If this passed,
    // references wouldn't be covered by the signature — an attacker
    // could serve a path claiming different dependencies.
    assert!(
        pubkey.verify(fp_no_ref.as_bytes(), &sig).is_err(),
        "signature must NOT verify against fingerprint without ref — refs must be covered"
    );

    server.abort();
    Ok(())
}
