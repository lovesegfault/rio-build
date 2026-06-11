//! `GetNarIndex`/`GetNarIndexBatch` round-trip + cache-hit + cascade,
//! plus the caller-identity gate (no anonymous index reads).

use super::*;
use rio_nix::nar::{NarEntry, NarNode};
use rio_proto::types::{GetNarIndexBatchRequest, GetNarIndexRequest, NarEntryKind};
use sha2::{Digest, Sha256};

/// HMAC key for the NAR-index caller-identity gate. The index RPCs are
/// not tenant-scoped, but they must never be anonymous — see
/// `StoreServiceImpl::require_caller_identity`.
const NAR_HMAC_KEY: &[u8] = b"nar-index-service-test-key-32-b!";

/// A key the server does NOT hold — tokens signed with it must be
/// rejected exactly like anonymous requests.
const FORGED_HMAC_KEY: &[u8] = b"not-the-server-key-aaaaaaaaaaaaa";

/// Builder assignment token signed with `key`. Sign with
/// [`NAR_HMAC_KEY`] for a valid token, any other key for a forged one.
/// `expected_outputs` matters only for PutPath seeding (the index RPCs
/// ignore the claim body — identity is all they check).
fn assignment_token(key: &[u8], expected_outputs: Vec<String>) -> String {
    rio_auth::hmac::HmacSigner::from_key(key.to_vec()).sign(&rio_auth::hmac::AssignmentClaims {
        executor_id: "test".into(),
        drv_hash: "00".repeat(32),
        expected_outputs,
        is_ca: false,
        expiry_unix: 9_999_999_999,
        tenant: Some(uuid::Uuid::nil().to_string()),
        input_closure_digest: String::new(),
    })
}

/// Wrap `msg` in a request carrying a valid builder assignment token.
fn authed<T>(msg: T) -> tonic::Request<T> {
    with_token(msg, &assignment_token(NAR_HMAC_KEY, vec![]))
}

/// Wrap `msg` in a request carrying `token` verbatim.
fn with_token<T>(msg: T, token: &str) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    r.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        token.parse().expect("token is ASCII"),
    );
    r
}

/// HMAC-gated session + one seeded indexed path. The seed PutPath uses
/// a valid token with the path in `expected_outputs` (PutPath enforces
/// the claim body; the index RPCs only require an identity).
async fn seeded_hmac_session(
    name: &str,
    payload: &[u8],
) -> anyhow::Result<(StoreSession, [u8; 32])> {
    let mut s = StoreSession::new_with_hmac(NAR_HMAC_KEY.to_vec()).await?;
    let (nar, nar_hash, path) = make_dir_nar(name, payload);
    let info = make_path_info(&path, &nar, nar_hash);
    let token = assignment_token(NAR_HMAC_KEY, vec![path]);
    put_path_with_token(&mut s.client, info, nar, &token).await?;
    Ok((s, nar_hash))
}

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
/// `file_digest` populated for regular files only. The index is
/// written eagerly in the manifest-complete transaction (ADR-022 §6 /
/// P0557) — it cannot be recomputed from blob-aligned chunks later —
/// so the RPC is a pure PG read.
// r[verify store.index.rpc+1]
#[tokio::test]
async fn get_nar_index_returns_eagerly_written_index() -> TestResult {
    let (mut s, nar_hash) = seeded_hmac_session("nar-index-rt", b"hello nar index").await?;

    // First call: PG hit (no recompute path exists for indexed paths).
    // The eager-write DB-state assertions live in
    // `put_path_writes_castore_index_atomically`.
    let idx = s
        .client
        .get_nar_index(authed(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        }))
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

    // Second call: PG hit, identical result.
    let idx2 = s
        .client
        .get_nar_index(authed(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        }))
        .await?
        .into_inner();
    assert_eq!(idx2.entries, idx.entries);

    Ok(())
}

/// Unknown nar_hash → `NOT_FOUND`. Wrong-length → `INVALID_ARGUMENT`.
#[tokio::test]
async fn get_nar_index_not_found_and_bad_arg() -> TestResult {
    let mut s = StoreSession::new_with_hmac(NAR_HMAC_KEY.to_vec()).await?;
    let err = s
        .client
        .get_nar_index(authed(GetNarIndexRequest {
            nar_hash: vec![0xAB; 32],
        }))
        .await
        .expect_err("unknown nar_hash should fail");
    assert_eq!(err.code(), tonic::Code::NotFound);

    let err = s
        .client
        .get_nar_index(authed(GetNarIndexRequest {
            nar_hash: vec![0u8; 16],
        }))
        .await
        .expect_err("16-byte hash should fail");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    Ok(())
}

/// An anonymous (or forged-token) `GetNarIndex` is rejected with
/// `UNAUTHENTICATED` before any PG read. The index discloses a path's
/// full file listing, sizes, and per-file BLAKE3 digests for any
/// guessed or leaked nar_hash — an anonymous cross-tenant metadata
/// oracle (and, combined with the chunk RPCs, a content one). The
/// authed success on the same seeded hash proves the rejection is not
/// vacuous.
// r[verify store.index.rpc+1]
#[tokio::test]
async fn get_nar_index_rejects_anonymous_and_forged_callers() -> TestResult {
    let (mut s, nar_hash) = seeded_hmac_session("nar-index-anon", b"secret payload").await?;

    // No token at all.
    let err = s
        .client
        .get_nar_index(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        })
        .await
        .expect_err("anonymous GetNarIndex must be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // A token signed with the wrong key.
    let err = s
        .client
        .get_nar_index(with_token(
            GetNarIndexRequest {
                nar_hash: nar_hash.to_vec(),
            },
            &assignment_token(FORGED_HMAC_KEY, vec![]),
        ))
        .await
        .expect_err("forged-token GetNarIndex must be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // Vacuity sentinel: the same request with a valid token succeeds.
    let idx = s
        .client
        .get_nar_index(authed(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        }))
        .await?
        .into_inner();
    assert!(!idx.entries.is_empty());
    Ok(())
}

/// Same gate on the batch variant: anonymous or forged-token
/// `GetNarIndexBatch` fails at the RPC boundary, before any per-hash
/// PG work.
// r[verify store.index.rpc+1]
#[tokio::test]
async fn get_nar_index_batch_rejects_anonymous_and_forged_callers() -> TestResult {
    let (mut s, nar_hash) = seeded_hmac_session("nar-index-anon-batch", b"secret batch").await?;

    // No token at all.
    let err = s
        .client
        .get_nar_index_batch(GetNarIndexBatchRequest {
            nar_hashes: vec![nar_hash.to_vec()],
        })
        .await
        .expect_err("anonymous GetNarIndexBatch must be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // A token signed with the wrong key.
    let err = s
        .client
        .get_nar_index_batch(with_token(
            GetNarIndexBatchRequest {
                nar_hashes: vec![nar_hash.to_vec()],
            },
            &assignment_token(FORGED_HMAC_KEY, vec![]),
        ))
        .await
        .expect_err("forged-token GetNarIndexBatch must be rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // Vacuity sentinel: the same batch with a valid token streams the
    // seeded index back.
    use tokio_stream::StreamExt;
    let mut stream = s
        .client
        .get_nar_index_batch(authed(GetNarIndexBatchRequest {
            nar_hashes: vec![nar_hash.to_vec()],
        }))
        .await?
        .into_inner();
    let r0 = stream.next().await.unwrap()?;
    assert!(r0.index.is_some());
    Ok(())
}

/// `GetNarIndexBatch`: hit + miss in request order. A `nar_hash` the
/// store has no complete path for returns `index = None` in its slot;
/// the hit is unaffected by the neighboring miss.
// r[verify store.index.rpc+1]
#[tokio::test]
async fn get_nar_index_batch_order_and_misses() -> TestResult {
    let (mut s, nar_hash) = seeded_hmac_session("nar-index-batch", b"batched").await?;

    // Warm one path via the unary RPC; the second hash is unknown.
    s.client
        .get_nar_index(authed(GetNarIndexRequest {
            nar_hash: nar_hash.to_vec(),
        }))
        .await?;

    use tokio_stream::StreamExt;
    let mut stream = s
        .client
        .get_nar_index_batch(authed(GetNarIndexBatchRequest {
            nar_hashes: vec![vec![0xCD; 32], nar_hash.to_vec()],
        }))
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
    let service = StoreServiceImpl::new(db.pool.clone())
        .with_max_batch_paths(TEST_CAP)
        .with_hmac_verifier(Arc::new(rio_auth::hmac::HmacVerifier::from_key(
            NAR_HMAC_KEY.to_vec(),
        )));
    let (mut client, _server) = spawn_store_server(service).await?;

    let err = client
        .get_nar_index_batch(authed(GetNarIndexBatchRequest {
            nar_hashes: vec![vec![0u8; 32]; TEST_CAP + 1],
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    use tokio_stream::StreamExt;
    let mut stream = client
        .get_nar_index_batch(authed(GetNarIndexBatchRequest {
            nar_hashes: vec![vec![0u8; 32]; TEST_CAP],
        }))
        .await?
        .into_inner();
    let mut n = 0;
    while stream.next().await.is_some() {
        n += 1;
    }
    assert_eq!(n, TEST_CAP);
    Ok(())
}

/// The complete transaction writes the whole castore index — nar_index
/// + directories + directory_paths + file_blobs — atomically with the
/// status flip, and file_blobs carries BLOB-stream offsets (the file's
/// position in the concatenation of regular-file contents in walk
/// order), not NAR offsets. A `'complete'` path therefore always has
/// the rows GetPath's framing regeneration and ReadBlob/StatBlob's
/// window math depend on.
// r[verify store.index.authoritative]
#[tokio::test]
async fn put_path_writes_castore_index_atomically() -> TestResult {
    let mut s = StoreSession::new().await?;
    // dir { a (15-byte regular), b (symlink), c (10-byte regular) }.
    let (nar, nar_hash, path) = make_dir_nar("nar-index-eager", b"hello nar index");
    let info = make_path_info(&path, &nar, nar_hash);
    put_path(&mut s.client, info, nar).await?;

    let status: String = sqlx::query_scalar("SELECT status::text FROM manifests LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(status, "complete");

    let nar_index_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM nar_index")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(nar_index_rows, 1, "nar_index written in the complete tx");
    let root_node: Option<Vec<u8>> = sqlx::query_scalar("SELECT root_node FROM nar_index LIMIT 1")
        .fetch_one(&s.db.pool)
        .await?;
    assert!(
        root_node.is_some_and(|r| !r.is_empty()),
        "root_node populated"
    );

    // One distinct directory body (the root dir), refcount 1, junction
    // row present.
    let dirs: i64 = sqlx::query_scalar("SELECT count(*) FROM directories")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(dirs, 1, "one distinct Directory body");
    let junctions: i64 = sqlx::query_scalar("SELECT count(*) FROM directory_paths")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(junctions, 1);

    // file_blobs: two regular files at BLOB offsets 0 and 15 (a's
    // contents are 15 bytes; c starts where a ends — no NAR framing in
    // between).
    let blobs: Vec<(Vec<u8>, i64, i64)> =
        sqlx::query_as("SELECT digest, nar_offset, size FROM file_blobs ORDER BY nar_offset")
            .fetch_all(&s.db.pool)
            .await?;
    assert_eq!(blobs.len(), 2, "two distinct regular files");
    assert_eq!(
        (blobs[0].1, blobs[0].2),
        (0, 15),
        "first file at blob offset 0"
    );
    assert_eq!(
        (blobs[1].1, blobs[1].2),
        (15, 10),
        "second file immediately after the first in the blob stream"
    );
    assert_eq!(
        blobs[0].0,
        blake3::hash(b"hello nar index").as_bytes().to_vec()
    );
    assert_eq!(blobs[1].0, blake3::hash(b"executable").as_bytes().to_vec());
    Ok(())
}
