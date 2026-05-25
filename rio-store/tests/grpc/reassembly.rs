//! Chunked GetPath reassembly (K=8 buffered prefetch).

use super::*;

// ===========================================================================
// Chunked GetPath reassembly
// ===========================================================================

/// THE roundtrip test: chunked PutPath → GetPath reassembles the exact
/// same bytes. If this passes, the whole chunker → manifest → backend →
/// cache → buffered prefetch → SHA-256 verify chain is correct.
#[tokio::test]
async fn test_chunked_roundtrip() -> TestResult {
    use rio_proto::types::{GetPathRequest, get_path_response};

    let (mut s, _backend) = StoreSession::new_chunked().await?;

    // 1 MiB NAR — chunks into ~16 pieces at FastCDC's 64 KiB average.
    let (nar, info, store_path) = make_large_nar(42, 1024 * 1024);
    let original = nar.clone();

    put_path(&mut s.client, info, nar).await?;

    // GetPath it back.
    let mut stream = s
        .client
        .get_path(GetPathRequest {
            store_path: store_path.clone(),
            manifest_hint: None,
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

    Ok(())
}

/// Chunked GetPath with a missing chunk: backend has the manifest but
/// one chunk is gone (simulating S3 losing an object). Should DATA_LOSS,
/// not hang or silently truncate.
#[tokio::test]
async fn test_chunked_getpath_missing_chunk_data_loss() -> TestResult {
    use rio_proto::types::GetPathRequest;

    let (mut s, backend) = StoreSession::new_chunked().await?;

    let (nar, info, store_path) = make_large_nar(43, 512 * 1024);
    put_path(&mut s.client, info, nar).await?;

    // Corrupt one chunk (delete would be cleaner but MemoryChunkBackend
    // doesn't expose delete; corrupt achieves the same thing — BLAKE3
    // verify fails, which is handled the same as NotFound: both produce
    // ChunkError → DATA_LOSS).
    //
    // Pick ANY chunk from the backend. corrupt_for_test needs the hash;
    // grab it from the chunks table.
    let one_hash: Vec<u8> = sqlx::query_scalar("SELECT blake3_hash FROM chunks LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    let hash_arr: [u8; 32] = one_hash.as_slice().try_into()?;
    backend.corrupt_for_test(&hash_arr, bytes::Bytes::from_static(b"CORRUPTED"));

    // GetPath: should produce DATA_LOSS mid-stream.
    let mut stream = s
        .client
        .get_path(GetPathRequest {
            store_path,
            manifest_hint: None,
        })
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

    Ok(())
}

// The "this replica's backend doesn't have the chunk" misconfiguration
// (e.g. two store replicas pointed at different buckets) surfaces as
// DATA_LOSS chunk-not-found mid-stream — covered by
// test_chunked_getpath_missing_chunk_data_loss above. The pre-P0583
// "store has no chunk backend at all" pre-flight FAILED_PRECONDITION is
// gone: a chunk backend is required config.
