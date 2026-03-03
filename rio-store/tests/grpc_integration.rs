//! gRPC-level integration tests for StoreService.
//!
//! These tests spin up an in-process tonic server backed by [`MemoryBackend`]
//! and an ephemeral PostgreSQL database (bootstrapped by `rio-test-support`),
//! then exercise the full gRPC request/response path including streaming.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::store::store_service_server::StoreServiceServer;
use rio_proto::types::{
    FindMissingPathsRequest, PathInfo, PutPathMetadata, PutPathRequest, QueryPathInfoRequest,
    put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;
use rio_store::backend::memory::MemoryBackend;
use rio_store::grpc::StoreServiceImpl;
use rio_test_support::fixtures::{make_nar, make_path_info_for_nar};
use rio_test_support::{Context, TestDb, TestResult};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Spawn an in-process store gRPC server and return a connected client
/// plus the `Arc<MemoryBackend>` so tests can corrupt blobs directly
/// (for integrity-check tests).
///
/// Uses an ephemeral TCP port on 127.0.0.1. The returned `JoinHandle`
/// should be aborted at test end (or dropped) to shut down the server.
pub async fn setup_store(
    pool: PgPool,
) -> anyhow::Result<(
    StoreServiceClient<Channel>,
    Arc<MemoryBackend>,
    tokio::task::JoinHandle<()>,
)> {
    let backend = Arc::new(MemoryBackend::new());
    let service = StoreServiceImpl::new(backend.clone(), pool);

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

    Ok((client, backend, server))
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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

    let store_path = "/nix/store/11111111111111111111111111111111-test-cleanup-path";
    let good_nar = make_nar(b"correct content").0;
    let bad_nar = make_nar(b"wrong content").0;

    // Declare the hash of good_nar but send bad_nar — should fail validation
    let info = make_path_info_for_nar(store_path, &good_nar);
    let result = put_path(&mut client, info.clone(), bad_nar).await;
    assert!(result.is_err(), "hash mismatch should be rejected");

    // Verify no stale rows remain (check via SQL: nar_blobs with status='uploading')
    let stale: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM nar_blobs WHERE status = 'uploading'")
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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, backend, server) = setup_store(db.pool.clone()).await?;

    // 1. Upload a valid NAR.
    let store_path = "/nix/store/88888888888888888888888888888888-corruption-test";
    let good_nar = make_nar(b"valid content for corruption test").0;
    let info = make_path_info_for_nar(store_path, &good_nar);
    let sha256_hex = hex::encode(info.nar_hash);

    let created = put_path(&mut client, info, good_nar)
        .await
        .context("put should succeed")?;
    assert!(created);

    // 2. Corrupt the blob directly in the backend (same length so size check
    // passes, different content so hash check fails).
    let corrupt_data = vec![0xAAu8; 200]; // garbage, wrong sha256
    backend.corrupt_for_test(
        &format!("{sha256_hex}.nar"),
        bytes::Bytes::from(corrupt_data),
    );

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client1, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

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

/// internal_error() must NOT leak sqlx/Postgres details to the client.
/// Server logs the full error; client sees only "storage operation failed".
#[tokio::test]
async fn test_internal_error_hides_sqlx_details() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;

    // Close the pool so the next DB query fails with a sqlx::Error::PoolClosed.
    db.pool.close().await;

    let result = client
        .query_path_info(QueryPathInfoRequest {
            store_path: "/nix/store/00000000000000000000000000000000-valid-name".into(),
        })
        .await;

    let status = result.expect_err("should fail on closed pool");
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(
        status.message(),
        "storage operation failed",
        "message must be generic, not leak DB details"
    );
    // Belt-and-suspenders: no substring from common sqlx errors.
    assert!(!status.message().to_lowercase().contains("sqlx"));
    assert!(!status.message().to_lowercase().contains("postgres"));
    assert!(!status.message().to_lowercase().contains("pool"));

    server.abort();
    Ok(())
}

// Note: the "NAR blob not found for" branch (grpc.rs:369-372) is defense-
// in-depth for a race between query_path_info and get_blob_key (both do the
// same INNER JOIN narinfo/nar_blobs with status='complete'). Not normally
// reachable; no test.

/// GetPath with nar_blobs row present but blob missing from backend.
/// This is the data-loss scenario: metadata says it's there, backend lost it.
#[tokio::test]
async fn test_get_path_backend_blob_missing() -> TestResult {
    use rio_proto::types::GetPathRequest;

    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, backend, server) = setup_store(db.pool.clone()).await?;

    let store_path = "/nix/store/33333333333333333333333333333333-backend-missing";
    let nar = make_nar(b"content").0;
    let info = make_path_info_for_nar(store_path, &nar);
    put_path(&mut client, info, nar).await?;

    // Delete the blob from the backend so metadata says it's there but
    // backend.get() returns None. This is the data-loss scenario.
    use rio_store::backend::NarBackend;
    let blob_key: String = sqlx::query_scalar(
        "SELECT b.blob_key FROM nar_blobs b JOIN narinfo n \
         ON b.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(store_path)
    .fetch_one(&db.pool)
    .await?;
    backend
        .delete(&blob_key)
        .await
        .context("delete from backend")?;

    let result = client
        .get_path(GetPathRequest {
            store_path: store_path.into(),
        })
        .await;
    let status = result.expect_err("should be NotFound");
    assert_eq!(status.code(), tonic::Code::NotFound);
    assert!(
        status.message().contains("NAR blob missing from backend"),
        "got: {}",
        status.message()
    );

    server.abort();
    Ok(())
}

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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;
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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;
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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;
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
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM nar_blobs")
        .fetch_one(&db.pool)
        .await
        .context("count nar_blobs")?;
    assert_eq!(count.0, 0, "abort_upload should clean up placeholder");

    server.abort();
    Ok(())
}

/// Empty metadata hash + NO trailer → InvalidArgument.
#[tokio::test]
async fn test_trailer_mode_missing_trailer_rejected() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;
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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;
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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;
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
    let (mut client, _backend, server) = setup_store(db.pool.clone()).await?;
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
