//! PutPathTrailer protocol tests (hash-trailer upload mode).

use super::*;

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

/// Hash-upfront was deleted: metadata with non-empty nar_hash is now
/// REJECTED (un-updated client, deploy error). Loud failure, not silent
/// ignore — otherwise an old gateway would get inexplicable "no trailer
/// received" errors deep into the stream.
#[tokio::test]
async fn test_metadata_with_hash_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = trailer_fixture("upfront-mode");

    // Build stream manually: metadata with REAL hash (what an old gateway
    // would send), no trailer.
    let raw: PathInfo = info.into();
    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .expect("fresh channel");
    drop(tx);

    let result = s.client.put_path(ReceiverStream::new(rx)).await;
    let status = result.expect_err("non-empty metadata hash should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("hash-upfront mode removed"),
        "got: {}",
        status.message()
    );

    Ok(())
}

/// Hash-trailer mode: empty metadata hash + correct trailer → success.
/// Stored hash/size come from the trailer, not the (zero-filled) metadata.
#[tokio::test]
async fn test_trailer_mode_correct_hash_succeeds() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = trailer_fixture("trailer-ok");

    // Metadata with EMPTY hash/size — triggers trailer mode.
    let mut raw: PathInfo = info.clone().into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let created = put_path_trailer_mode(
        &mut s.client,
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
    let got = s
        .client
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

    Ok(())
}

/// Trailer with WRONG hash → InvalidArgument (hash mismatch).
#[tokio::test]
async fn test_trailer_mode_wrong_hash_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = trailer_fixture("trailer-bad-hash");

    let mut raw: PathInfo = info.clone().into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let result = put_path_trailer_mode(
        &mut s.client,
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
        .fetch_one(&s.db.pool)
        .await
        .context("count manifests")?;
    assert_eq!(count.0, 0, "abort_upload should clean up placeholder");

    Ok(())
}

/// Stream closes without trailer → InvalidArgument. Trailer is mandatory.
/// `put_path_raw` now always sends a trailer, so this uses a manual stream.
#[tokio::test]
async fn test_trailer_mode_missing_trailer_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = trailer_fixture("no-trailer");

    let mut raw: PathInfo = info.into();
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
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .expect("fresh channel");
    // Stream closed — no trailer.
    drop(tx);

    let result = s.client.put_path(ReceiverStream::new(rx)).await;
    let status = result.expect_err("missing trailer should fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("no trailer received"),
        "{}",
        status.message()
    );

    Ok(())
}

/// Trailer nar_hash ≠ 32 bytes → InvalidArgument.
#[tokio::test]
async fn test_trailer_mode_bad_hash_length_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = trailer_fixture("bad-hash-len");

    let mut raw: PathInfo = info.into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;

    let result = put_path_trailer_mode(
        &mut s.client,
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

    Ok(())
}

/// Chunk AFTER trailer → protocol violation.
#[tokio::test]
async fn test_trailer_chunk_after_trailer_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;
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

    let result = s.client.put_path(ReceiverStream::new(rx)).await;
    let status = result.expect_err("chunk after trailer should fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("trailer must be last"),
        "{}",
        status.message()
    );

    Ok(())
}

/// Two trailer messages in the stream → reject. The server's state
/// machine should reject the second trailer as a protocol violation.
#[tokio::test]
async fn test_trailer_duplicate_trailer_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = trailer_fixture("dup-trailer");
    let mut raw: PathInfo = info.clone().into();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut raw.nar_hash),
        nar_size: std::mem::take(&mut raw.nar_size),
    };

    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    })
    .await
    .expect("fresh channel");
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .expect("fresh channel");
    // First trailer.
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer.clone())),
    })
    .await
    .expect("fresh channel");
    // VIOLATION: second trailer.
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer)),
    })
    .await
    .expect("fresh channel");
    drop(tx);

    let status = s
        .client
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("duplicate trailer → reject");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// Trailer with nar_size > MAX_NAR_SIZE → reject. The bound check
/// should fire before any chunk hashing/verification.
#[tokio::test]
async fn test_trailer_oversized_nar_size_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = trailer_fixture("oversized-trailer");
    let mut raw: PathInfo = info.into();
    raw.nar_hash.clear();
    raw.nar_size = 0;

    let status = put_path_trailer_mode(
        &mut s.client,
        raw,
        nar,
        PutPathTrailer {
            nar_hash: vec![0xAA; 32],
            // 4 GiB + 1 — bigger than MAX_NAR_SIZE.
            nar_size: rio_common::limits::MAX_NAR_SIZE + 1,
        },
    )
    .await
    .expect_err("oversized nar_size → reject");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// Stream starts with NarChunk (not Metadata) → reject. Metadata
/// must be first for the server to know what path is being uploaded.
#[tokio::test]
async fn test_first_message_chunk_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let (tx, rx) = mpsc::channel(4);
    // Violation: chunk first, no metadata.
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(b"no metadata".to_vec())),
    })
    .await
    .expect("fresh channel");
    drop(tx);

    let status = s
        .client
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("chunk-first → reject");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// Stream starts with Trailer (not Metadata) → reject.
#[tokio::test]
async fn test_first_message_trailer_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let (tx, rx) = mpsc::channel(4);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(PutPathTrailer {
            nar_hash: vec![0xAA; 32],
            nar_size: 100,
        })),
    })
    .await
    .expect("fresh channel");
    drop(tx);

    let status = s
        .client
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("trailer-first → reject");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    Ok(())
}
