//! Core PutPath/GetPath/FindMissing/QueryPathInfo tests.

use super::*;

#[tokio::test]
async fn test_harness_smoke() -> TestResult {
    let mut s = StoreSession::new().await?;

    // QueryPathInfo on missing path should return NOT_FOUND
    let result = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: test_store_path("does-not-exist"),
        })
        .await;
    assert_eq!(
        result.expect_err("nonexistent path should fail").code(),
        tonic::Code::NotFound
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Group 9: Error handling
// ---------------------------------------------------------------------------

/// After a PutPath fails validation (hash mismatch), the placeholder rows
/// should be cleaned up so a retry with the correct data succeeds.
#[tokio::test]
async fn test_put_path_cleanup_on_hash_mismatch() -> TestResult {
    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("test-cleanup-path");
    let good_nar = make_nar(b"correct content").0;
    let bad_nar = make_nar(b"wrong content").0;

    // Declare the hash of good_nar but send bad_nar — should fail validation
    let info = make_path_info_for_nar(&store_path, &good_nar);
    let result = put_path(&mut s.client, info.clone(), bad_nar).await;
    assert!(result.is_err(), "hash mismatch should be rejected");

    // Verify no stale rows remain (check via SQL: manifests with status='uploading')
    let stale: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM manifests WHERE status = 'uploading'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(stale, 0, "uploading placeholder should be cleaned up");

    // Retry with correct content should succeed (no unique constraint violation)
    let result = put_path(&mut s.client, info, good_nar).await;
    assert!(
        result.is_ok(),
        "retry after cleanup should succeed: {result:?}"
    );
    assert!(result?, "should be newly created");

    Ok(())
}

// ---------------------------------------------------------------------------
// Group 10: Remaining coverage
// ---------------------------------------------------------------------------

/// PutPath followed by GetPath should return the same NAR content.
/// Also exercises the transfer-volume metric emission sites:
/// `rio_store_put_path_bytes_total` at put_path.rs:575 and
/// `rio_store_get_path_bytes_total` at get_path.rs:86.
// r[verify obs.metric.transfer-volume]
#[tokio::test]
async fn test_put_get_roundtrip() -> TestResult {
    use rio_proto::types::{GetPathRequest, get_path_response};

    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("test-roundtrip-path");
    let nar = make_nar(b"roundtrip test content!").0;
    let info = make_path_info_for_nar(&store_path, &nar);

    // Put
    let created = put_path(&mut s.client, info.clone(), nar.clone())
        .await
        .context("put should succeed")?;
    assert!(created, "should be newly created");

    // Get
    let mut stream = s
        .client
        .get_path(GetPathRequest {
            store_path: store_path.clone(),
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

    Ok(())
}

/// GetPath on a path that was never uploaded should return NOT_FOUND.
#[tokio::test]
async fn test_get_path_nonexistent_returns_not_found() -> TestResult {
    use rio_proto::types::GetPathRequest;

    let mut s = StoreSession::new().await?;

    let result = s
        .client
        .get_path(GetPathRequest {
            store_path: test_store_path("never-uploaded"),
        })
        .await;

    assert_eq!(
        result
            .expect_err("nonexistent path should fail GetPath")
            .code(),
        tonic::Code::NotFound,
        "should be NOT_FOUND"
    );

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

    let mut s = StoreSession::new().await?;

    // 1. Upload a valid NAR.
    let store_path = test_store_path("corruption-test");
    let good_nar = make_nar(b"valid content for corruption test").0;
    let nar_len = good_nar.len();
    let info = make_path_info_for_nar(&store_path, &good_nar);

    let created = put_path(&mut s.client, info, good_nar)
        .await
        .context("put should succeed")?;
    assert!(created);

    // 2. Corrupt manifests.inline_blob directly via SQL. Same length so
    // the pre-flight size sanity-check passes; different content so the
    // post-stream SHA-256 check fails. This is the phase-2c equivalent
    // of the old backend.corrupt_for_test(): simulates TOAST-storage
    // bitrot or manual DB tampering.
    let corrupt_data = vec![0xAAu8; nar_len]; // same len, wrong sha256
    sqlx::query(
        "UPDATE manifests SET inline_blob = $1 \
         WHERE store_path_hash = (SELECT store_path_hash FROM narinfo WHERE store_path = $2)",
    )
    .bind(&corrupt_data)
    .bind(&store_path)
    .execute(&s.db.pool)
    .await
    .context("corrupt inline_blob")?;

    // 3. GetPath — stream should deliver chunks then DATA_LOSS at the end.
    let mut stream = s
        .client
        .get_path(GetPathRequest { store_path })
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

    Ok(())
}

/// Second PutPath with same content should return created=false (idempotent).
#[tokio::test]
async fn test_idempotent_put_path() -> TestResult {
    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("test-idempotent-path");
    let nar = make_nar(b"idempotent test").0;
    let info = make_path_info_for_nar(&store_path, &nar);

    // First put
    let created1 = put_path(&mut s.client, info.clone(), nar.clone())
        .await
        .context("first put should succeed")?;
    assert!(created1, "first put should create");

    // Second put with same content
    let created2 = put_path(&mut s.client, info, nar)
        .await
        .context("second put should succeed (idempotent)")?;
    assert!(!created2, "second put should return created=false");

    Ok(())
}

/// Two concurrent PutPath requests for the same path: exactly one should
/// win (created=true); the other should either see created=false (if it
/// raced after the first completed) or Aborted (if it raced into the
/// in-progress window). Never: both created=true, or the loser's cleanup
/// deleting the winner's placeholder.
// r[verify store.put.idempotent]
#[tokio::test]
async fn test_concurrent_putpath_same_path_one_wins() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Second client to the same server so we can send two concurrent streams.
    let mut client2 = s.client.clone();

    let store_path = test_store_path("concurrent-race");
    let nar = make_nar(b"concurrent race test data").0;
    let info = make_path_info_for_nar(&store_path, &nar);

    // Launch both PutPath calls concurrently.
    let (r1, r2) = tokio::join!(
        put_path(&mut s.client, info.clone(), nar.clone()),
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
    let qpi = s
        .client
        .query_path_info(QueryPathInfoRequest { store_path })
        .await
        .context("path should be queryable after concurrent uploads")?;
    assert_eq!(qpi.into_inner().nar_size, nar.len() as u64);

    Ok(())
}

/// Trailer nar_size disagreeing with actual NAR bytes → size mismatch.
///
/// Pre-trailer-mode this tested "chunks exceed declared nar_size". With
/// trailer mode there's no metadata size to exceed; the equivalent
/// protection is (a) MAX_NAR_SIZE bounds accumulation, and (b)
/// validate_nar_digest checks trailer.nar_size == actual bytes received.
/// This tests (b).
#[tokio::test]
async fn test_put_path_rejects_oversized_nar() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Declare nar_size=100 in trailer but send 100_000 bytes.
    let mut info = make_path_info_for_nar(&test_store_path("oversized-test"), &[0u8; 100]);
    info.nar_size = 100; // trailer will claim 100; NAR is 100_000 → mismatch

    let oversized_data = vec![0u8; 100_000];
    let result = put_path(&mut s.client, info, oversized_data).await;

    assert!(result.is_err(), "size mismatch should be rejected");
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::InvalidArgument,
        "should be INVALID_ARGUMENT, got: {status:?}"
    );
    assert!(
        status.message().contains("size mismatch"),
        "error message should mention size mismatch: {}",
        status.message()
    );

    Ok(())
}

/// Oversized rejection must clean up the uploading placeholder so a retry
/// with correct data succeeds. Regression test for the placeholder leak at
/// the chunk-size-exceeded early return.
#[tokio::test]
async fn test_put_path_oversized_then_retry_succeeds() -> TestResult {
    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("oversized-retry");
    let real_nar = make_nar(b"retry test data").0;
    let real_info = make_path_info_for_nar(&store_path, &real_nar);

    // First attempt: lie about size, send oversized data → rejected.
    let mut bad_info = real_info.clone();
    bad_info.nar_size = 100;
    let oversized = vec![0u8; 100_000];
    let r1 = put_path(&mut s.client, bad_info, oversized).await;
    assert!(r1.is_err(), "oversized must be rejected");
    assert_eq!(r1.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Second attempt: correct data. Must succeed (placeholder was cleaned up).
    let r2 = put_path(&mut s.client, real_info, real_nar.clone())
        .await
        .context("retry with correct data must succeed after oversized rejection")?;
    assert!(r2, "retry should create");

    // Verify the path is queryable.
    let qpi = s
        .client
        .query_path_info(QueryPathInfoRequest { store_path })
        .await
        .context("path should be queryable")?;
    assert_eq!(qpi.into_inner().nar_size, real_nar.len() as u64);

    Ok(())
}

/// Duplicate metadata mid-stream is a protocol violation and must be rejected
/// (not silently ignored).
#[tokio::test]
async fn test_put_path_rejects_duplicate_metadata() -> TestResult {
    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("dup-metadata");
    let nar = make_nar(b"dup metadata test").0;
    let info = make_path_info_for_nar(&store_path, &nar);

    // First metadata must have empty hash (else the new hash-upfront guard
    // fires before we reach the dup-metadata check).
    let mut raw: PathInfo = info.into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let (tx, rx) = mpsc::channel(8);
    // Metadata #1 (valid)
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw.clone()),
        })),
    })
    .await?;
    // Metadata #2 (protocol violation)
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    })
    .await?;
    // Chunk (never read — server should reject before this)
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await?;
    drop(tx);

    let result = s.client.put_path(ReceiverStream::new(rx)).await;
    assert!(result.is_err(), "duplicate metadata must be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("duplicate metadata"),
        "error should mention duplicate metadata: {}",
        status.message()
    );

    Ok(())
}

/// Trailer with nar_size > MAX_NAR_SIZE is rejected. Chunk accumulation
/// is independently bounded by MAX_NAR_SIZE (no pre-alloc from declared
/// size anymore), so this checks the trailer validation specifically.
#[tokio::test]
async fn test_put_path_rejects_absurd_nar_size() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut info = make_path_info_for_nar(&test_store_path("absurd-size-test"), &[0u8; 10]);
    info.nar_size = u64::MAX; // trailer.nar_size > MAX_NAR_SIZE → rejected

    // Must be rejected promptly — no hang, no crash.
    let status = put_path(&mut s.client, info, vec![0u8; 10])
        .await
        .expect_err("u64::MAX nar_size should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("exceeds maximum"),
        "error should mention size limit: {}",
        status.message()
    );

    Ok(())
}

/// PutPath with more than MAX_REFERENCES entries should be rejected.
#[tokio::test]
async fn test_put_path_rejects_excessive_references() -> TestResult {
    let mut s = StoreSession::new().await?;

    // We're testing SERVER-SIDE rejection of too-many-references. Build raw
    // PathInfo directly since ValidatedPathInfo can't hold 10,001 unparsed
    // string references (client-side TryFrom would reject first).
    let nar = make_nar(b"refs-test").0;
    let base: PathInfo = make_path_info_for_nar(&test_store_path("too-many-refs"), &nar).into();
    let info = PathInfo {
        // MAX_REFERENCES = 10_000; send 10_001 to trigger the check.
        // Each ref is a VALID store path (TryFrom would accept them); the
        // server's check_bound fires on COUNT, not on per-ref syntax.
        references: (0..10_001)
            .map(|i| test_store_path(&format!("ref-{i}")))
            .collect(),
        ..base
    };

    let status = put_path_raw(&mut s.client, info, nar)
        .await
        .expect_err("10,001 references should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("too many references"),
        "error should mention reference limit: {}",
        status.message()
    );

    Ok(())
}

/// Mirror of excessive_references but for signatures (MAX_SIGNATURES=100).
#[tokio::test]
async fn test_put_path_rejects_excessive_signatures() -> TestResult {
    let mut s = StoreSession::new().await?;

    let nar = make_nar(b"sigs-test").0;
    let base: PathInfo = make_path_info_for_nar(&test_store_path("too-many-sigs"), &nar).into();
    let info = PathInfo {
        signatures: (0..rio_common::limits::MAX_SIGNATURES + 1)
            .map(|i| format!("cache-{i}:sig{i}"))
            .collect(),
        ..base
    };

    let status = put_path_raw(&mut s.client, info, nar)
        .await
        .expect_err("excessive signatures → reject");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("signatures"),
        "msg: {}",
        status.message()
    );

    Ok(())
}

/// PutPath with a malformed reference path should be rejected.
#[tokio::test]
async fn test_put_path_rejects_malformed_reference() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Testing SERVER-SIDE rejection: build raw PathInfo with a garbage ref.
    let nar = make_nar(b"refs-test").0;
    let base: PathInfo = make_path_info_for_nar(&test_store_path("bad-ref"), &nar).into();
    let info = PathInfo {
        references: vec!["not-a-valid-store-path".into()],
        ..base
    };

    let result = put_path_raw(&mut s.client, info, nar).await;
    assert!(result.is_err(), "malformed reference should be rejected");
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// Malformed store paths (no 32-char hash prefix, traversal attempts) should
/// be rejected with INVALID_ARGUMENT at the RPC boundary.
#[tokio::test]
async fn test_rejects_malformed_store_paths() -> TestResult {
    let mut s = StoreSession::new().await?;

    let bad_paths = [
        "/nix/store/too-short",     // no 32-char hash
        "/nix/store/../etc/passwd", // traversal
        "not-a-store-path",         // no /nix/store/ prefix
        "",                         // empty
    ];

    for path in bad_paths {
        let result = s
            .client
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

    Ok(())
}

/// FindMissingPaths with > max_batch_paths entries should be rejected.
/// Uses a low cap (.with_max_batch_paths(100)) so the test exercises the
/// rejection path without allocating DEFAULT_MAX_BATCH_PATHS+1 = 100k+1
/// path strings.
#[tokio::test]
async fn test_find_missing_paths_rejects_oversized_batch() -> TestResult {
    const TEST_CAP: usize = 100;
    let db = TestDb::new(&MIGRATOR).await;
    let service = StoreServiceImpl::new(db.pool.clone()).with_max_batch_paths(TEST_CAP);
    let (mut client, _server) = spawn_store_server(service).await?;

    let paths: Vec<String> = (0..TEST_CAP + 1)
        .map(|i| test_store_path(&format!("path-{i}")))
        .collect();

    let result = client
        .find_missing_paths(FindMissingPathsRequest { store_paths: paths })
        .await;

    assert!(result.is_err(), "oversized batch should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let msg = status.message();
    assert!(
        msg.contains("too many paths"),
        "error should mention path limit: {msg}"
    );
    assert!(
        msg.contains("RIO_MAX_BATCH_PATHS"),
        "error should name the env var to raise: {msg}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Error-branch coverage: internal_error, blob-missing paths
// ---------------------------------------------------------------------------

/// A connection-level failure (PoolClosed) must surface as UNAVAILABLE
/// (retriable), NOT Internal. This is the key value-add of MetadataError:
/// before typed errors, a transient PG hiccup looked identical to a corrupt database.
///
/// Secondary: the message must not leak sqlx/Postgres internals. Server
/// logs the full error; client sees a generic summary.
#[tokio::test]
async fn test_connection_error_is_unavailable_and_hides_sqlx_details() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Close the pool so the next DB query fails with a sqlx::Error::PoolClosed.
    s.db.pool.close().await;

    let result = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: test_store_path("valid-name"),
        })
        .await;

    let status = result.expect_err("should fail on closed pool");
    // PoolClosed → MetadataError::Connection → UNAVAILABLE. Client should
    // retry with backoff. NOT Internal — that would look like corruption.
    assert_eq!(
        status.code(),
        tonic::Code::Unavailable,
        "connection-level failures must be retriable (UNAVAILABLE, not Internal)"
    );
    // Belt-and-suspenders: no substring from common sqlx errors.
    assert!(!status.message().to_lowercase().contains("sqlx"));
    assert!(!status.message().to_lowercase().contains("postgres"));
    assert!(!status.message().to_lowercase().contains("pool"));

    Ok(())
}

/// GetPath on a path where narinfo.nar_size disagrees with the manifest's
/// summed size should fail fast with DATA_LOSS — before streaming any
/// bytes. Catches manifest/narinfo drift (PutPath bug, manual DB surgery).
// r[verify store.get.size-sanity-check]
#[tokio::test]
async fn test_get_path_size_mismatch_returns_data_loss() -> TestResult {
    use rio_proto::types::GetPathRequest;

    let mut s = StoreSession::new().await?;

    // 1. Upload a valid NAR (inline storage — no chunk backend).
    let store_path = test_store_path("size-mismatch-test");
    let nar = make_nar(b"content for size mismatch test").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    let real_size = info.nar_size;

    let created = put_path(&mut s.client, info, nar)
        .await
        .context("put should succeed")?;
    assert!(created);

    // 2. Corrupt narinfo.nar_size to disagree with manifests.inline_blob
    // length. The manifest's total_size() = blob.len() = real_size;
    // narinfo now says real_size + 1 → mismatch.
    sqlx::query("UPDATE narinfo SET nar_size = $1 WHERE store_path = $2")
        .bind((real_size + 1) as i64)
        .bind(&store_path)
        .execute(&s.db.pool)
        .await
        .context("corrupt narinfo.nar_size")?;

    // 3. GetPath — should return DATA_LOSS synchronously (pre-flight
    // check, before the streaming task spawns). No chunks, no PathInfo.
    let result = s.client.get_path(GetPathRequest { store_path }).await;

    let status = result.expect_err("size mismatch should fail GetPath synchronously");
    assert_eq!(
        status.code(),
        tonic::Code::DataLoss,
        "manifest/narinfo drift must be DATA_LOSS: {status:?}"
    );
    assert!(
        status.message().contains("size mismatch"),
        "error should name the check: {}",
        status.message()
    );
    assert!(
        status.message().contains(&real_size.to_string())
            && status.message().contains(&(real_size + 1).to_string()),
        "error should include both sizes for debugging: {}",
        status.message()
    );

    Ok(())
}

// Note: the "manifest not found for" branch in GetPath is defense-in-depth
// for a race between query_path_info and get_manifest (both filter on
// manifests.status='complete'). Not normally reachable; no test.
//
// The inline-storage path has no "blob-missing" race: the NAR lives in
// the SAME transaction that flips status to complete. The chunked-path
// equivalent (PG manifest present, S3 chunk missing) is covered by
// tests/grpc/reassembly.rs::test_chunked_getpath_missing_chunk_data_loss.
