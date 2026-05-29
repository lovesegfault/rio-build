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

    // 1 MiB NAR — well over INLINE_THRESHOLD, chunks into ~16 pieces.
    let (nar, info, store_path) = make_large_nar(42, 1024 * 1024);
    let original = nar.clone();

    put_path(&mut s.client, info, nar).await?;

    // GetPath it back.
    let mut stream = s
        .client
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

    Ok(())
}

/// Inline-only store + chunked manifest: fail clearly at pre-flight,
/// not deep in the spawned task with no context.
#[tokio::test]
async fn test_chunked_manifest_no_cache_preflight_fails() -> TestResult {
    use rio_proto::types::GetPathRequest;

    // Can't use StoreSession here — both servers need to share ONE PG
    // (StoreSession::new() creates a fresh DB each time). Build manually
    // via spawn_store_server.
    let db = TestDb::new(&MIGRATOR).await;

    // First: use a CHUNKED store to write a chunked path.
    {
        let backend = mem_backend();
        let cache = Arc::new(rio_store::cas::ChunkCache::new(
            backend as Arc<dyn ChunkBackend>,
        ));
        let service = StoreServiceImpl::new(db.pool.clone()).with_chunk_cache(cache);
        let (mut cli, server) = spawn_store_server(service).await?;
        let (nar, info, _) = make_large_nar(44, 512 * 1024);
        put_path(&mut cli, info, nar).await?;
        server.abort();
    }

    // Second: use an INLINE-ONLY store (same PG) to try reading it.
    // Simulates a misconfigured deployment where one instance wrote
    // chunked and another can't read it.
    let service = StoreServiceImpl::new(db.pool.clone());
    let (mut cli, server) = spawn_store_server(service).await?;
    let store_path = test_store_path("large-nar-44");

    let result = cli.get_path(GetPathRequest { store_path }).await;
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
// ADR-022 §6: framing regeneration round-trips
// ===========================================================================

/// A directory NAR exercising every node kind the framing regenerator
/// must emit: a regular file, an EMPTY regular file, an executable, a
/// symlink, a nested directory, and an EMPTY nested directory. `pad`
/// inflates the two non-empty files so the same fixture can be pushed
/// over `INLINE_THRESHOLD` for the chunked variant.
fn make_tree_nar(name: &str, pad: usize) -> (Vec<u8>, String) {
    use rio_nix::nar::{NarEntry, NarNode};
    let mut big_a = b"alpha-contents-".to_vec();
    big_a.extend(pseudo_random_bytes(11, pad));
    let mut big_b = b"beta-contents-".to_vec();
    big_b.extend(pseudo_random_bytes(12, pad));
    let node = NarNode::Directory {
        entries: vec![
            NarEntry {
                name: "a-file".into(),
                node: NarNode::Regular {
                    executable: false,
                    contents: big_a,
                },
            },
            NarEntry {
                name: "b-empty".into(),
                node: NarNode::Regular {
                    executable: false,
                    contents: Vec::new(),
                },
            },
            NarEntry {
                name: "c-sub".into(),
                node: NarNode::Directory {
                    entries: vec![
                        NarEntry {
                            name: "deeper-empty".into(),
                            node: NarNode::Directory {
                                entries: Vec::new(),
                            },
                        },
                        NarEntry {
                            name: "exe".into(),
                            node: NarNode::Regular {
                                executable: true,
                                contents: big_b,
                            },
                        },
                    ],
                },
            },
            NarEntry {
                name: "d-link".into(),
                node: NarNode::Symlink {
                    target: "a-file".into(),
                },
            },
        ],
    };
    let mut buf = Vec::new();
    rio_nix::nar::serialize(&mut buf, &node).expect("NAR serialize to Vec");
    (buf, test_store_path(name))
}

/// Stream a path back and concatenate the NarChunk messages.
async fn get_path_bytes(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> Result<Vec<u8>, tonic::Status> {
    use rio_proto::types::{GetPathRequest, get_path_response};
    let mut stream = client
        .get_path(GetPathRequest {
            store_path: store_path.to_string(),
        })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(msg) = stream.message().await? {
        if let Some(get_path_response::Msg::NarChunk(c)) = msg.msg {
            out.extend_from_slice(&c);
        }
    }
    Ok(out)
}

/// Inline multi-entry round-trip: the NAR framing is regenerated from
/// the Directory DAG and the contents come from the inline blob
/// stream, and the result is byte-identical to the uploaded NAR. The
/// fixture covers empty files, empty directories, executables, and
/// symlinks — every framing arm the regenerator has.
// r[verify store.nar.reassembly]
#[tokio::test]
async fn tree_nar_roundtrips_inline() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, store_path) = make_tree_nar("tree-inline", 64);
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar.clone()).await?;

    // Stored form is the blob stream, NOT the NAR: strictly smaller
    // (framing stripped) and not byte-equal to any NAR prefix.
    let blob: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n USING (store_path_hash) \
         WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&s.db.pool)
    .await?;
    let blob = blob.expect("inline path has a blob");
    assert!(
        blob.len() < nar.len(),
        "inline_blob is the blob stream (no framing): {} vs NAR {}",
        blob.len(),
        nar.len()
    );

    let got = get_path_bytes(&mut s.client, &store_path).await?;
    assert_eq!(got, nar, "regenerated NAR is byte-identical");
    Ok(())
}

/// Chunked multi-entry round-trip + the per-file chunk alignment
/// property: every manifest entry belongs to exactly one file (file
/// boundaries are chunk boundaries), so a 2-file NAR whose files are
/// each under CHUNK_MAX produces exactly 2 chunks whose sizes are the
/// two file sizes — not a CHUNK_MAX-aligned split of the whole NAR.
// r[verify store.nar.reassembly]
#[tokio::test]
async fn tree_nar_roundtrips_chunked_with_per_file_chunks() -> TestResult {
    let (mut s, _backend) = StoreSession::new_chunked().await?;
    // 200 KiB per file → 400 KiB NAR → over INLINE_THRESHOLD, but each
    // file is under CHUNK_MAX so each is exactly one chunk.
    let pad = 200 * 1024;
    let (nar, store_path) = make_tree_nar("tree-chunked", pad);
    assert!(nar.len() > rio_store::cas::INLINE_THRESHOLD);
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar.clone()).await?;

    // The manifest has exactly one entry per non-empty file, sized to
    // that file's content length. A whole-NAR chunker would emit
    // 256 KiB-aligned chunks here instead.
    let chunk_list: Vec<u8> = sqlx::query_scalar(
        "SELECT md.chunk_list FROM manifest_data md JOIN narinfo n USING (store_path_hash) \
         WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&s.db.pool)
    .await?;
    // [version][32-byte hash + 4-byte LE size]* — 2 entries.
    assert_eq!(chunk_list.len(), 1 + 2 * 36, "exactly 2 chunks");
    let size0 = u32::from_le_bytes(chunk_list[33..37].try_into().unwrap());
    let size1 = u32::from_le_bytes(chunk_list[69..73].try_into().unwrap());
    assert_eq!(
        size0 as usize,
        pad + "alpha-contents-".len(),
        "chunk 0 is exactly a-file's contents"
    );
    assert_eq!(
        size1 as usize,
        pad + "beta-contents-".len(),
        "chunk 1 is exactly c-sub/exe's contents"
    );

    let got = get_path_bytes(&mut s.client, &store_path).await?;
    assert_eq!(got, nar, "regenerated NAR is byte-identical");
    Ok(())
}

/// Two byte-identical files under different names in one path: the
/// blob stream carries the contents twice (the walk visits both), the
/// manifest carries the chunk run twice, but `file_blobs` has ONE row
/// per distinct digest at the FIRST occurrence's blob offset — and the
/// regenerated NAR is still byte-identical.
#[tokio::test]
async fn duplicate_file_contents_roundtrip_and_single_file_blobs_row() -> TestResult {
    use rio_nix::nar::{NarEntry, NarNode};
    let mut s = StoreSession::new().await?;

    let payload = b"identical contents in two places".to_vec();
    let node = NarNode::Directory {
        entries: vec![
            NarEntry {
                name: "first".into(),
                node: NarNode::Regular {
                    executable: false,
                    contents: payload.clone(),
                },
            },
            NarEntry {
                name: "second".into(),
                node: NarNode::Regular {
                    executable: false,
                    contents: payload.clone(),
                },
            },
        ],
    };
    let mut nar = Vec::new();
    rio_nix::nar::serialize(&mut nar, &node)?;
    let store_path = test_store_path("dup-contents");
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar.clone()).await?;

    // One file_blobs row (digest-keyed), at blob offset 0 (the first
    // occurrence; the second occurrence at offset payload.len() loses
    // the or_insert).
    let rows: Vec<(Vec<u8>, i64, i64)> =
        sqlx::query_as("SELECT digest, nar_offset, size FROM file_blobs")
            .fetch_all(&s.db.pool)
            .await?;
    assert_eq!(rows.len(), 1, "one row per distinct file digest");
    assert_eq!(rows[0].0, blake3::hash(&payload).as_bytes().to_vec());
    assert_eq!(rows[0].1, 0, "first occurrence's blob offset wins");
    assert_eq!(rows[0].2 as usize, payload.len());

    // The blob stream carries the contents twice.
    let blob: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n USING (store_path_hash) \
         WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        blob.expect("inline").len(),
        payload.len() * 2,
        "the walk visits both occurrences"
    );

    let got = get_path_bytes(&mut s.client, &store_path).await?;
    assert_eq!(got, nar);
    Ok(())
}

/// A complete path whose castore index is missing is unservable: the
/// NAR framing only exists in the Directory DAG, so GetPath must
/// return DATA_LOSS (the path exists but its content is unreachable),
/// not NOT_FOUND and not a garbage NAR.
#[tokio::test]
async fn get_path_without_castore_index_is_data_loss() -> TestResult {
    use rio_proto::types::GetPathRequest;
    let mut s = StoreSession::new().await?;
    let (nar, store_path) = make_tree_nar("no-index", 64);
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar).await?;

    // Simulate the corruption: drop the index row out from under a
    // complete manifest.
    sqlx::query(
        "DELETE FROM nar_index WHERE store_path_hash = \
         (SELECT store_path_hash FROM narinfo WHERE store_path = $1)",
    )
    .bind(&store_path)
    .execute(&s.db.pool)
    .await?;

    let err = s
        .client
        .get_path(GetPathRequest { store_path })
        .await
        .expect_err("missing index must not serve a garbage NAR");
    assert_eq!(err.code(), tonic::Code::DataLoss, "{err:?}");
    assert!(
        err.message().contains("castore index"),
        "error names the missing piece: {}",
        err.message()
    );
    Ok(())
}

/// The other half of GetPath's NOT_FOUND-vs-DATA_LOSS contract (the
/// DATA_LOSS half is `get_path_without_castore_index_is_data_loss`
/// above): a narinfo row whose manifest row is gone is exactly what a
/// concurrent GC sweep leaves behind mid-collection, so the handler
/// must answer NOT_FOUND — never DATA_LOSS, which is reserved for a
/// complete manifest whose castore index is missing in the same
/// snapshot (a state GC cannot produce).
#[tokio::test]
async fn get_path_without_manifest_row_is_not_found() -> TestResult {
    use rio_proto::types::GetPathRequest;
    let mut s = StoreSession::new().await?;
    let (nar, store_path) = make_tree_nar("no-manifest", 64);
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar).await?;

    // The mid-collection state: the manifest row (and via CASCADE the
    // manifest_data / nar_index rows) is gone, the narinfo row is not.
    sqlx::query(
        "DELETE FROM manifests WHERE store_path_hash = \
         (SELECT store_path_hash FROM narinfo WHERE store_path = $1)",
    )
    .bind(&store_path)
    .execute(&s.db.pool)
    .await?;

    let err = s
        .client
        .get_path(GetPathRequest { store_path })
        .await
        .expect_err("a path without a manifest row is not servable");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "a missing manifest row is the concurrent-GC shape and must be NOT_FOUND, \
         not DATA_LOSS: {err:?}"
    );
    Ok(())
}
