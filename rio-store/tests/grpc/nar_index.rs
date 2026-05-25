//! `GetNarIndex`/`GetNarIndexBatch` round-trip + cache-hit + cascade.

use super::*;
use rio_nix::nar::{NarEntry, NarNode};
use rio_proto::types::{GetNarIndexBatchRequest, GetNarIndexRequest, NarEntryKind};
use sha2::{Digest, Sha256};

/// Build a 3-entry NAR: dir { a (regular), b (symlink), c (regular) }.
/// Returns `(nar_bytes, sha256, store_path)`.
fn make_dir_nar(name: &str, payload: &[u8]) -> (Vec<u8>, [u8; 32], String) {
    let node = NarNode::Directory {
        entries: vec![
            NarEntry {
                name: "a".into(),
                node: NarNode::Regular {
                    executable: false,
                    contents: payload.to_vec(),
                },
            },
            NarEntry {
                name: "b".into(),
                node: NarNode::Symlink { target: "a".into() },
            },
            NarEntry {
                name: "c".into(),
                node: NarNode::Regular {
                    executable: true,
                    contents: b"executable".to_vec(),
                },
            },
        ],
    };
    let mut buf = Vec::new();
    rio_nix::nar::serialize(&mut buf, &node).unwrap();
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    (buf, digest, test_store_path(name))
}

/// PutPath a multi-entry NAR → `GetNarIndex` returns all entries with
/// `file_digest` populated for regular files only. Second call is a
/// cache hit (no recompute) — verified structurally via the
/// `manifests.nar_indexed` flag flipping after the first call.
///
/// `nar_index_concurrency = 0` keeps PutPath's eager index (P0557)
/// from racing this test's "still unindexed" precondition — the point
/// here is the sync-on-miss recompute, not the eager path.
// r[verify store.index.rpc]
// r[verify store.index.sync-on-miss]
#[tokio::test]
async fn get_nar_index_sync_on_miss_then_cache_hit() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let service = StoreServiceImpl::new(db.pool.clone()).with_nar_index_concurrency(0);
    let (mut client, _server) = spawn_store_server(service).await?;
    let (nar, nar_hash, path) = make_dir_nar("nar-index-rt", b"hello nar index");
    let info = make_path_info(&path, &nar, nar_hash);
    put_path(&mut client, info, nar).await?;

    // Pre-condition: with 0 eager permits PutPath leaves the path
    // unindexed (the gate is `try_acquire`, never a wait).
    let indexed: bool = sqlx::query_scalar("SELECT nar_indexed FROM manifests LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    assert!(
        !indexed,
        "PutPath with nar_index_concurrency=0 must not eagerly index"
    );

    // Sync-on-miss: first call recomputes and write-throughs.
    let idx = client
        .get_nar_index(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        })
        .await?
        .into_inner();
    // root dir + 3 children.
    assert_eq!(idx.entries.len(), 4, "{:?}", idx.entries);
    assert_eq!(idx.entries[0].path, b"");
    assert_eq!(idx.entries[0].kind, NarEntryKind::Directory as i32);
    assert!(idx.entries[0].file_digest.is_empty());
    // P0572: directory entries carry `dir_digest`; root's matches `root_digest`.
    // r[verify store.index.dir-digest]
    assert_eq!(idx.entries[0].dir_digest.len(), 32);
    assert_eq!(idx.root_digest, idx.entries[0].dir_digest);
    let by_name: std::collections::HashMap<&[u8], _> =
        idx.entries.iter().map(|e| (e.path.as_slice(), e)).collect();
    let a = by_name[b"a".as_slice()];
    assert_eq!(a.kind, NarEntryKind::Regular as i32);
    assert_eq!(a.size, b"hello nar index".len() as u64);
    assert_eq!(a.file_digest, blake3::hash(b"hello nar index").as_bytes());
    assert!(!a.executable);
    assert!(a.nar_offset > 0);
    let b = by_name[b"b".as_slice()];
    assert_eq!(b.kind, NarEntryKind::Symlink as i32);
    assert_eq!(b.target, b"a");
    assert!(b.file_digest.is_empty());
    let c = by_name[b"c".as_slice()];
    assert!(c.executable);
    assert_eq!(c.file_digest, blake3::hash(b"executable").as_bytes());

    // Write-through: nar_index row exists, manifest flagged.
    let indexed: bool = sqlx::query_scalar("SELECT nar_indexed FROM manifests LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    assert!(
        indexed,
        "sync-on-miss should write-through and flip nar_indexed"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM nar_index")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(rows, 1);

    // Second call: PG hit, identical result.
    let idx2 = client
        .get_nar_index(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        })
        .await?
        .into_inner();
    assert_eq!(idx2.entries, idx.entries);

    Ok(())
}

/// P0557: PutPath with a free eager-index permit (the default) indexes
/// the path from the still-in-RAM NAR — `manifests.nar_indexed` flips
/// and the `nar_index` + castore junction rows land WITHOUT any
/// `GetNarIndex` call and without the `indexer_loop` running (the test
/// harness never spawns it). The poll is for the spawned task's commit,
/// not a wall-clock gate — there is no other writer in this process.
///
/// The castore-table assertions are the read-side contract: the rows
/// the eager pass writes are exactly what `GetDirectory`/`StatBlob`
/// resolve digests through, so an eager write that skipped them would
/// leave the path invisible to the castore-FUSE builder until GC.
// r[verify store.index.putpath-eager]
#[tokio::test]
async fn put_path_eagerly_indexes_without_get() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, nar_hash, path) = make_dir_nar("nar-index-eager", b"eager payload");
    let info = make_path_info(&path, &nar, nar_hash);
    put_path(&mut s.client, info, nar).await?;

    // The eager task is spawned (not awaited by the handler); poll for
    // its commit. 50×20ms is the same budget every other spawned-task
    // assertion in this suite uses.
    let indexed = poll_scalar_until(
        &s.db.pool,
        "SELECT nar_indexed FROM manifests LIMIT 1",
        true,
    )
    .await;
    assert!(
        indexed,
        "PutPath must eagerly index with a free permit (no GetNarIndex, no indexer_loop)"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM nar_index")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(rows, 1, "eager pass must write the nar_index row");
    // Castore junctions: 1 directory (the root), 2 file_blobs (a, c).
    let dirs: i64 = sqlx::query_scalar("SELECT count(*) FROM directory_paths")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(dirs, 1, "eager pass must populate directory_paths");
    let blobs: i64 = sqlx::query_scalar("SELECT count(*) FROM file_blobs")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(blobs, 2, "eager pass must populate file_blobs");
    Ok(())
}

/// The eager gate is per-output for `PutPathBatch` too: both outputs of
/// a 2-output batch index without a `GetNarIndex` call.
// r[verify store.index.putpath-eager]
#[tokio::test]
async fn put_path_batch_eagerly_indexes() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathMetadata, PutPathRequest, PutPathTrailer};

    let mut s = StoreSession::new().await?;
    let outputs = [
        make_dir_nar("batch-eager-0", b"first output"),
        make_dir_nar("batch-eager-1", b"second output"),
    ];

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    for (i, (nar, nar_hash, path)) in outputs.iter().enumerate() {
        let mut info: rio_proto::types::PathInfo = make_path_info(path, nar, *nar_hash).into();
        info.nar_hash = Vec::new();
        let trailer = PutPathTrailer {
            nar_hash: nar_hash.to_vec(),
            nar_size: std::mem::take(&mut info.nar_size),
        };
        for msg in [
            put_path_request::Msg::Metadata(PutPathMetadata { info: Some(info) }),
            put_path_request::Msg::NarChunk(nar.clone()),
            put_path_request::Msg::Trailer(trailer),
        ] {
            tx.send(PutPathBatchRequest {
                output_index: i as u32,
                inner: Some(PutPathRequest { msg: Some(msg) }),
            })
            .await
            .expect("fresh channel");
        }
    }
    drop(tx);
    let resp = s
        .client
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true, true]);

    let n = poll_scalar_until(
        &s.db.pool,
        "SELECT count(*) FROM manifests WHERE nar_indexed",
        2i64,
    )
    .await;
    assert_eq!(n, 2, "both batch outputs must be eagerly indexed");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM nar_index")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(rows, 2);
    Ok(())
}

/// Unknown nar_hash → `NOT_FOUND`. Wrong-length → `INVALID_ARGUMENT`.
#[tokio::test]
async fn get_nar_index_not_found_and_bad_arg() -> TestResult {
    let mut s = StoreSession::new().await?;
    let err = s
        .client
        .get_nar_index(GetNarIndexRequest {
            nar_hash: vec![0xAB; 32],
        })
        .await
        .expect_err("unknown nar_hash should fail");
    assert_eq!(err.code(), tonic::Code::NotFound);

    let err = s
        .client
        .get_nar_index(GetNarIndexRequest {
            nar_hash: vec![0u8; 16],
        })
        .await
        .expect_err("16-byte hash should fail");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    Ok(())
}

/// `GetNarIndexBatch`: hit + miss in request order. PG-hit-only — no
/// sync-on-miss — so a path with no `nar_index` row returns
/// `index = None`, not a recompute.
// r[verify store.index.rpc]
#[tokio::test]
async fn get_nar_index_batch_order_and_misses() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, nar_hash, path) = make_dir_nar("nar-index-batch", b"batched");
    let info = make_path_info(&path, &nar, nar_hash);
    put_path(&mut s.client, info, nar).await?;

    // Warm one path via the unary RPC; the second hash is unknown.
    s.client
        .get_nar_index(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        })
        .await?;

    use tokio_stream::StreamExt;
    let mut stream = s
        .client
        .get_nar_index_batch(GetNarIndexBatchRequest {
            nar_hashes: vec![vec![0xCD; 32], nar_hash.to_vec()],
        })
        .await?
        .into_inner();
    let r0 = stream.next().await.unwrap()?;
    assert_eq!(r0.nar_hash, vec![0xCD; 32]);
    assert!(r0.index.is_none(), "unknown hash → index absent");
    let r1 = stream.next().await.unwrap()?;
    assert_eq!(r1.nar_hash, nar_hash.to_vec());
    assert_eq!(r1.index.unwrap().entries.len(), 4);
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn get_nar_index_batch_rejects_oversized() -> TestResult {
    const TEST_CAP: usize = 4;
    let db = TestDb::new(&MIGRATOR).await;
    let service = StoreServiceImpl::new(db.pool.clone()).with_max_batch_paths(TEST_CAP);
    let (mut client, _server) = spawn_store_server(service).await?;

    let err = client
        .get_nar_index_batch(GetNarIndexBatchRequest {
            nar_hashes: vec![vec![0u8; 32]; TEST_CAP + 1],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    use tokio_stream::StreamExt;
    let mut stream = client
        .get_nar_index_batch(GetNarIndexBatchRequest {
            nar_hashes: vec![vec![0u8; 32]; TEST_CAP],
        })
        .await?
        .into_inner();
    let mut n = 0;
    while stream.next().await.is_some() {
        n += 1;
    }
    assert_eq!(n, TEST_CAP);
    Ok(())
}

#[tokio::test]
async fn compute_rejects_over_sync_cap() -> TestResult {
    use rio_store::nar_index::{OverSyncCap, compute};

    let mut s = StoreSession::new().await?;
    let (nar, nar_hash, path) = make_dir_nar("nar-index-cap", b"x");
    let total = nar.len() as u64;
    let info = make_path_info(&path, &nar, nar_hash);
    put_path(&mut s.client, info, nar).await?;

    // The cap check runs against the manifest's summed size BEFORE any
    // chunk fetch, so an empty cache is fine for the over-cap arm; the
    // at-cap arm only needs to NOT fail with OverSyncCap.
    let cache = std::sync::Arc::new(rio_store::cas::ChunkCache::new(mem_backend()));

    // 1 byte over the cap → OverSyncCap.
    let err = compute(&s.db.pool, &cache, &path, total - 1, None)
        .await
        .unwrap_err();
    assert!(err.is::<OverSyncCap>(), "{err}");

    // Exactly at the cap → allowed (`>`, not `>=`).
    let r = compute(&s.db.pool, &cache, &path, total, None).await;
    assert!(
        r.as_ref().err().is_none_or(|e| !e.is::<OverSyncCap>()),
        "{r:?}"
    );
    Ok(())
}
