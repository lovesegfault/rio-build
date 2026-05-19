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
// r[verify store.index.rpc]
// r[verify store.index.sync-on-miss]
#[tokio::test]
async fn get_nar_index_sync_on_miss_then_cache_hit() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, nar_hash, path) = make_dir_nar("nar-index-rt", b"hello nar index");
    let info = make_path_info(&path, &nar, nar_hash);
    put_path(&mut s.client, info, nar).await?;

    // Pre-condition: PutPath does NOT eagerly index (P0557 not landed).
    let indexed: bool = sqlx::query_scalar("SELECT nar_indexed FROM manifests LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    assert!(
        !indexed,
        "PutPath should leave the path unindexed (eager = P0557)"
    );

    // Sync-on-miss: first call recomputes and write-throughs.
    let idx = s
        .client
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
        .fetch_one(&s.db.pool)
        .await?;
    assert!(
        indexed,
        "sync-on-miss should write-through and flip nar_indexed"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM nar_index")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(rows, 1);

    // Second call: PG hit, identical result.
    let idx2 = s
        .client
        .get_nar_index(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        })
        .await?
        .into_inner();
    assert_eq!(idx2.entries, idx.entries);

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

    // 1 byte over the cap → OverSyncCap.
    let err = compute(&s.db.pool, None, &path, total - 1, None)
        .await
        .unwrap_err();
    assert!(err.is::<OverSyncCap>(), "{err}");

    // Exactly at the cap → allowed (`>`, not `>=`).
    let r = compute(&s.db.pool, None, &path, total, None).await;
    assert!(
        r.as_ref().err().is_none_or(|e| !e.is::<OverSyncCap>()),
        "{r:?}"
    );
    Ok(())
}
