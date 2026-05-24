//! `PutPathChunked` + `HasChunks` integration tests: ephemeral PG +
//! in-process gRPC server + `MemoryChunkBackend`, driven by a test
//! client that plays the builder's half of the protocol.
//!
//! The fixture builder (`chunked_output_for_tree`) derives the
//! Directory DAG from a real on-disk tree via `dump_path_streaming` +
//! `nar_ls` + `castore::build` — an independent implementation of the
//! NAR format the server's reconstruction is checked against.
// r[verify store.put.chunked]
// r[verify store.put.chunked-wire]
// r[verify store.put.chunked-bounds]
// r[verify store.chunk.self-verify]
// r[verify store.put.narhash-sync]
// r[verify store.put.refs-sync]
// r[verify store.put.chunked-ca]
// r[verify store.chunk.has-chunks-durable]
// r[verify store.atomic.multi-output]
// r[verify store.put.idempotent]

use sha2::Digest as _;

use super::*;
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::{GetPathRequest, HasChunksRequest, get_path_response};
use rio_proto::{ChunkServiceServer, DirectoryServiceServer};
use rio_store::cas::ChunkCache;
use rio_store::grpc::{ChunkServiceImpl, DirectoryServiceImpl};
use rio_store::test_helpers::{assemble_begin, chunked_output_for_tree, put_path_chunked};

/// Harness: StoreService + ChunkService + DirectoryService sharing one
/// chunk cache and one ephemeral PG.
struct ChunkedSession {
    db: TestDb,
    store: StoreServiceClient<Channel>,
    chunk: ChunkServiceClient<Channel>,
    backend: Arc<MemoryChunkBackend>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for ChunkedSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl ChunkedSession {
    async fn new() -> anyhow::Result<Self> {
        Self::build(|svc| svc).await
    }

    async fn new_with_hmac(key: Vec<u8>) -> anyhow::Result<Self> {
        Self::build(move |svc| {
            svc.with_hmac_verifier(Arc::new(rio_auth::hmac::HmacVerifier::from_key(key)))
        })
        .await
    }

    async fn build(
        customize: impl FnOnce(StoreServiceImpl) -> StoreServiceImpl,
    ) -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let backend = mem_backend();
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let store_service =
            customize(StoreServiceImpl::new(db.pool.clone()).with_chunk_cache(Arc::clone(&cache)));
        let chunk_service = ChunkServiceImpl::new(db.pool.clone(), Some(Arc::clone(&cache)));
        let directory_service = DirectoryServiceImpl::new(db.pool.clone(), None, Some(cache));

        let max = rio_common::grpc::max_message_size();
        let router = Server::builder()
            .add_service(
                StoreServiceServer::new(store_service)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .add_service(ChunkServiceServer::new(chunk_service))
            .add_service(DirectoryServiceServer::new(directory_service));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        Ok(Self {
            db,
            store: rio_proto::client::connect_single(&addr.to_string()).await?,
            chunk: ChunkServiceClient::new(channel),
            backend,
            server,
        })
    }

    /// `GetPath` the full NAR back.
    async fn get_nar(&mut self, store_path: &str) -> Result<Vec<u8>, tonic::Status> {
        let mut stream = self
            .store
            .get_path(GetPathRequest {
                store_path: store_path.to_owned(),
                manifest_hint: None,
            })
            .await?
            .into_inner();
        let mut nar = Vec::new();
        while let Some(msg) = stream.message().await? {
            if let Some(get_path_response::Msg::NarChunk(c)) = msg.msg {
                nar.extend_from_slice(&c);
            }
        }
        Ok(nar)
    }

    async fn manifest_status(&self, store_path: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT m.status::text FROM manifests m JOIN narinfo n USING (store_path_hash) \
             WHERE n.store_path = $1",
        )
        .bind(store_path)
        .fetch_optional(&self.db.pool)
        .await
        .unwrap()
    }
}

/// A small tree with nested dirs, an executable, a symlink, and two
/// byte-identical files.
fn fixture_tree() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    std::fs::create_dir_all(p.join("bin")).unwrap();
    std::fs::create_dir_all(p.join("share/doc")).unwrap();
    std::fs::write(p.join("bin/tool"), b"#!/bin/sh\necho hello world\n").unwrap();
    std::fs::set_permissions(p.join("bin/tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(p.join("share/doc/README"), vec![0xA5u8; 4096]).unwrap();
    std::fs::write(p.join("share/doc/COPY"), vec![0xA5u8; 4096]).unwrap();
    std::os::unix::fs::symlink("bin/tool", p.join("default")).unwrap();
    tmp
}

/// Happy path: two outputs in one stream → both committed atomically,
/// GetPath round-trips both NARs byte-for-byte, the castore tables are
/// populated, the chunks are durable, and HasChunks reports them.
#[tokio::test]
async fn two_outputs_commit_and_roundtrip() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree_a = fixture_tree();
    let tree_b = tempfile::tempdir()?;
    std::fs::write(tree_b.path().join("data"), vec![0x5Au8; 10_000])?;

    let path_a = test_store_path("chunked-out-a");
    let path_b = test_store_path("chunked-out-b");
    let (out_a, dirs_a, chunks_a, nar_a) = chunked_output_for_tree(tree_a.path(), &path_a, 1024);
    let (out_b, dirs_b, chunks_b, nar_b) = chunked_output_for_tree(tree_b.path(), &path_b, 1024);
    let begin = assemble_begin(vec![out_a, out_b], vec![dirs_a, dirs_b]);
    let mut chunks = chunks_a;
    chunks.extend(chunks_b);

    let created = put_path_chunked(&mut s.store, begin.clone(), &chunks, None, |_, c| Some(c))
        .await
        .expect("upload should succeed");
    assert!(created, "first upload creates");

    // Both NARs round-trip byte-for-byte through GetPath (which
    // reassembles from manifest_data.chunk_list — framing chunks
    // interleaved with content chunks).
    assert_eq!(s.get_nar(&path_a).await?, nar_a, "output A round-trips");
    assert_eq!(s.get_nar(&path_b).await?, nar_b, "output B round-trips");

    // Narinfo carries the verified hash + size.
    let (nar_hash, nar_size): (Vec<u8>, i64) =
        sqlx::query_as("SELECT nar_hash, nar_size FROM narinfo WHERE store_path = $1")
            .bind(&path_a)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        nar_hash,
        sha2::Sha256::digest(&nar_a).to_vec(),
        "narinfo.nar_hash is the server-computed value"
    );
    assert_eq!(nar_size as u64, nar_a.len() as u64);

    // Castore tables: directories + directory_paths + file_blobs +
    // nar_index rows exist and the manifest is marked indexed.
    let n_dirs: i64 = sqlx::query_scalar("SELECT count(*) FROM directories")
        .fetch_one(&s.db.pool)
        .await?;
    assert!(n_dirs >= 3, "fixture tree has >= 3 distinct directories");
    let n_dp: i64 =
        sqlx::query_scalar("SELECT count(*) FROM directory_paths WHERE store_path_hash = $1")
            .bind(rio_store::test_helpers::path_hash(&path_a))
            .fetch_one(&s.db.pool)
            .await?;
    assert!(n_dp >= 3, "directory_paths links every reachable digest");
    let indexed: bool = sqlx::query_scalar(
        "SELECT m.nar_indexed FROM manifests m JOIN narinfo n USING (store_path_hash) \
         WHERE n.store_path = $1",
    )
    .bind(&path_a)
    .fetch_one(&s.db.pool)
    .await?;
    assert!(indexed, "eager nar_index marks the manifest indexed");
    // The two byte-identical files share one file_blobs digest.
    let n_blobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM file_blobs WHERE store_path_hash = $1")
            .bind(rio_store::test_helpers::path_hash(&path_a))
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        n_blobs, 2,
        "tool + (README == COPY) = 2 distinct file digests"
    );

    // Every chunk is durable AND NOT deleted; HasChunks reports all of
    // the content chunks present.
    let not_durable: i64 =
        sqlx::query_scalar("SELECT count(*) FROM chunks WHERE NOT durable OR deleted")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(not_durable, 0, "every committed chunk is durable");
    let digests: Vec<Vec<u8>> = begin
        .outputs
        .iter()
        .flat_map(|o| o.chunk_manifest.iter())
        .map(|c| c.hash.clone())
        .collect();
    let n = digests.len();
    let bitmap = s
        .chunk
        .has_chunks(HasChunksRequest { digests })
        .await?
        .into_inner()
        .bitmap;
    for i in 0..n {
        assert_ne!(
            bitmap[i / 8] & (1 << (i % 8)),
            0,
            "chunk {i} reported present"
        );
    }

    // r[verify store.put.idempotent]
    // Re-drive: every output already complete → created=false, nothing
    // double-counted.
    let rc_before: Vec<(Vec<u8>, i32)> =
        sqlx::query_as("SELECT blake3_hash, refcount FROM chunks ORDER BY blake3_hash")
            .fetch_all(&s.db.pool)
            .await?;
    let created = put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
        .await
        .expect("re-drive should succeed");
    assert!(!created, "re-drive reports created=false");
    let rc_after: Vec<(Vec<u8>, i32)> =
        sqlx::query_as("SELECT blake3_hash, refcount FROM chunks ORDER BY blake3_hash")
            .fetch_all(&s.db.pool)
            .await?;
    assert_eq!(rc_before, rc_after, "re-drive does not touch refcounts");
    Ok(())
}

/// An output whose NAR contains a reference to a closure member: when
/// the refs are declared the upload commits and narinfo carries them;
/// when the same hash is omitted from `refs` the server's rescan
/// catches the disagreement.
// r[verify store.put.refs-sync]
#[tokio::test]
async fn reference_scan_agreement_and_mismatch() -> TestResult {
    // The dependency needs a hash part DISTINCT from the output's
    // (`test_store_path` reuses one fixed hash for every name; the
    // refscan is keyed on hash parts, so a shared hash part would make
    // the dep and the output indistinguishable to the scanner).
    let dep = format!(
        "/nix/store/{}-chunked-dep",
        rio_test_support::fixtures::rand_store_hash()
    );

    // Tree containing the dependency's full path in a file body.
    let make_tree = || {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("conf"), format!("prefix={dep}\n")).unwrap();
        tmp
    };

    // Declared → commit, narinfo.references carries it.
    {
        let mut s = ChunkedSession::new().await?;
        let tree = make_tree();
        let path = test_store_path("chunked-refs-ok");
        let (mut out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
        out.refs = vec![dep.clone()];
        let mut begin = assemble_begin(vec![out], vec![dirs]);
        begin.input_closure = vec![dep.clone()];
        let created = put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
            .await
            .expect("declared refs should commit");
        assert!(created);
        let refs: Vec<String> =
            sqlx::query_scalar("SELECT unnest(\"references\") FROM narinfo WHERE store_path = $1")
                .bind(&path)
                .fetch_all(&s.db.pool)
                .await?;
        assert_eq!(refs, vec![dep.clone()]);
    }

    // Omitted → FAILED_PRECONDITION, nothing committed.
    {
        let mut s = ChunkedSession::new().await?;
        let tree = make_tree();
        let path = test_store_path("chunked-refs-bad");
        let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
        // refs stays empty but the closure candidate is present, so the
        // scanner finds the hash and the comparison fails.
        let mut begin = assemble_begin(vec![out], vec![dirs]);
        begin.input_closure = vec![dep.clone()];
        let err = put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
            .await
            .expect_err("undeclared scanned ref must be rejected");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("reference mismatch"), "{err:?}");
        // The PlaceholderGuard's reap is spawned on Drop — poll for it.
        let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT count(*) FROM manifests", 0).await;
        assert_eq!(n, 0, "placeholder reaped after a refs mismatch");
    }
    Ok(())
}

/// Wrong claimed nar_hash → FAILED_PRECONDITION, zero manifests
/// committed, placeholders reaped, chunks left at refcount 0.
// r[verify store.put.narhash-sync]
// r[verify store.integrity.verify-on-put]
#[tokio::test]
async fn narhash_mismatch_rejected_and_nothing_committed() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree = fixture_tree();
    let path = test_store_path("chunked-badhash");
    let (mut out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
    out.nar_hash = vec![0xEE; 32];
    let begin = assemble_begin(vec![out], vec![dirs]);
    let err = put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
        .await
        .expect_err("wrong nar_hash must be rejected");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("NAR hash mismatch"), "{err:?}");

    // Placeholder reaped (async drop guard) → manifests row gone.
    let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT count(*) FROM manifests", 0).await;
    assert_eq!(n, 0, "no manifest row survives a hash mismatch");
    // No chunk ends up referenced.
    let max_rc: Option<i32> = sqlx::query_scalar("SELECT max(refcount) FROM chunks")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(
        max_rc.unwrap_or(0),
        0,
        "no refcount survives a failed upload"
    );
    Ok(())
}

/// Every S3 object a failed upload wrote has a refcount-0 `chunks` row,
/// and the orphan-chunk sweep can therefore find and collect it. ADR-022
/// §6.3: non-committed novel chunks "are refcount-0 orphans for the
/// grace-TTL sweep" — without the write-ahead row they would be
/// untracked S3 garbage forever.
// r[verify store.chunk.grace-ttl]
#[tokio::test]
async fn failed_upload_leaves_sweepable_orphan_chunk_rows() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree = fixture_tree();
    let path = test_store_path("chunked-orphan");
    let (mut out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
    // Force a verify failure AFTER every chunk has been received and
    // written to the backend: a wrong claimed nar_hash.
    out.nar_hash = vec![0xEE; 32];
    let begin = assemble_begin(vec![out], vec![dirs]);
    let n_novel = begin.novel.len();
    assert!(n_novel >= 2);
    put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
        .await
        .expect_err("wrong nar_hash must be rejected");

    // The backend holds objects (novel content + server framing)…
    let objects = s.backend.len();
    assert!(
        objects > n_novel,
        "novel chunks + framing runs were written"
    );
    // …and EVERY one of them has a chunks row at refcount 0,
    // deleted = false, durable = false — the exact shape
    // `sweep_orphan_chunks` scans for.
    let (rows, sweepable): (i64, i64) = sqlx::query_as(
        "SELECT count(*), \
                count(*) FILTER (WHERE refcount = 0 AND NOT deleted AND NOT durable) \
           FROM chunks",
    )
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        rows as usize, objects,
        "every S3 object written by the failed upload is tracked by a chunks row"
    );
    assert_eq!(
        rows, sweepable,
        "every row is in the orphan-sweepable state"
    );

    // Run the orphan sweep with zero grace: it must tombstone every row
    // and enqueue every S3 key for deletion.
    let backend_dyn: Arc<dyn ChunkBackend> = Arc::clone(&s.backend) as Arc<dyn ChunkBackend>;
    let (swept, _bytes) =
        rio_store::gc::sweep::sweep_orphan_chunks(&s.db.pool, Some(&backend_dyn), 0)
            .await
            .expect("orphan sweep");
    assert_eq!(
        swept as usize, objects,
        "the orphan sweep collects every chunk the failed upload wrote"
    );
    Ok(())
}

/// Tampered chunk body (bytes don't hash to the declared digest) →
/// INVALID_ARGUMENT.
#[tokio::test]
async fn tampered_chunk_body_rejected() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree = fixture_tree();
    let path = test_store_path("chunked-tamper");
    let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
    let begin = assemble_begin(vec![out], vec![dirs]);
    let err = put_path_chunked(&mut s.store, begin, &chunks, None, |i, mut c| {
        if i == 0 {
            c.data[0] ^= 0xFF;
        }
        Some(c)
    })
    .await
    .expect_err("tampered chunk must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("hash to"), "{err:?}");
    Ok(())
}

/// Chunk frames out of `novel` order → INVALID_ARGUMENT.
#[tokio::test]
async fn out_of_order_chunk_rejected() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree = fixture_tree();
    let path = test_store_path("chunked-order");
    let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
    let begin = assemble_begin(vec![out], vec![dirs]);
    assert!(
        begin.novel.len() >= 2,
        "fixture must yield >= 2 novel chunks"
    );
    // Swap the first two frames' digests+bodies (each frame is still
    // self-consistent, just in the wrong position).
    let novel = begin.novel.clone();
    let chunks2 = chunks.clone();
    let err = put_path_chunked(&mut s.store, begin, &chunks, None, move |i, c| {
        let swapped = match i {
            0 => &novel[1],
            1 => &novel[0],
            _ => return Some(c),
        };
        let key: [u8; 32] = swapped.as_slice().try_into().unwrap();
        Some(rio_proto::types::PutPathChunkedChunk {
            digest: swapped.clone(),
            data: chunks2[&key].clone(),
        })
    })
    .await
    .expect_err("out-of-order chunk must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("order"), "{err:?}");
    Ok(())
}

/// Builder dies after k of n novel chunks → FAILED_PRECONDITION
/// (incomplete), placeholder reaped.
#[tokio::test]
async fn incomplete_stream_rejected() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree = fixture_tree();
    let path = test_store_path("chunked-incomplete");
    let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
    let begin = assemble_begin(vec![out], vec![dirs]);
    let n_novel = begin.novel.len();
    assert!(n_novel >= 2);
    let err = put_path_chunked(&mut s.store, begin, &chunks, None, move |i, c| {
        (i < n_novel - 1).then_some(c)
    })
    .await
    .expect_err("truncated stream must be rejected");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("before all novel chunks"), "{err:?}");
    let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT count(*) FROM manifests", 0).await;
    assert_eq!(n, 0, "placeholder reaped after incomplete stream");
    Ok(())
}

/// High-dedup upload: a second store path with identical content sends
/// `novel = []` (every chunk already durable) and zero Chunk frames.
#[tokio::test]
async fn high_dedup_upload_with_no_novel_chunks() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree = fixture_tree();

    let path_a = test_store_path("chunked-dedup-a");
    let (out_a, dirs_a, chunks_a, _) = chunked_output_for_tree(tree.path(), &path_a, 1024);
    let begin_a = assemble_begin(vec![out_a], vec![dirs_a]);
    assert!(
        put_path_chunked(&mut s.store, begin_a, &chunks_a, None, |_, c| Some(c))
            .await
            .expect("first upload")
    );

    // Probe HasChunks like a real builder and send only the missing
    // set (which is empty).
    let path_b = test_store_path("chunked-dedup-b");
    let (out_b, dirs_b, chunks_b, nar_b) = chunked_output_for_tree(tree.path(), &path_b, 1024);
    let digests: Vec<Vec<u8>> = out_b
        .chunk_manifest
        .iter()
        .map(|c| c.hash.clone())
        .collect();
    let bitmap = s
        .chunk
        .has_chunks(HasChunksRequest {
            digests: digests.clone(),
        })
        .await?
        .into_inner()
        .bitmap;
    let mut begin_b = assemble_begin(vec![out_b], vec![dirs_b]);
    begin_b.novel = digests
        .iter()
        .enumerate()
        .filter(|(i, _)| bitmap[i / 8] & (1 << (i % 8)) == 0)
        .map(|(_, d)| d.clone())
        .collect();
    // Deduplicate while preserving first-occurrence order.
    let mut seen = std::collections::HashSet::new();
    begin_b.novel.retain(|d| seen.insert(d.clone()));
    assert!(
        begin_b.novel.is_empty(),
        "every chunk of an identical tree is already durable"
    );
    assert!(
        put_path_chunked(&mut s.store, begin_b, &chunks_b, None, |_, c| Some(c))
            .await
            .expect("dedup upload")
    );
    assert_eq!(s.get_nar(&path_b).await?, nar_b, "dedup output round-trips");
    Ok(())
}

/// HasChunks durable-presence semantics: a chunk at refcount >= 1 whose
/// manifest is still 'uploading' reports ABSENT; it flips to present
/// only when a complete manifest references it; the GC sweep flips it
/// back to absent.
// r[verify store.chunk.has-chunks-durable]
#[tokio::test]
async fn has_chunks_reports_durable_presence_only() -> TestResult {
    let mut s = ChunkedSession::new().await?;

    // Seed a chunk row at refcount=1 (as an in-flight legacy upload
    // would) WITHOUT a complete manifest: durable defaults to FALSE.
    let digest = [0x42u8; 32];
    sqlx::query("INSERT INTO chunks (blake3_hash, refcount, size) VALUES ($1, 1, 64)")
        .bind(digest.as_slice())
        .execute(&s.db.pool)
        .await?;
    let probe = |client: &mut ChunkServiceClient<Channel>| {
        let mut c = client.clone();
        async move {
            c.has_chunks(HasChunksRequest {
                digests: vec![digest.to_vec()],
            })
            .await
            .map(|r| r.into_inner().bitmap[0] & 1 != 0)
        }
    };
    assert!(
        !probe(&mut s.chunk).await?,
        "refcount >= 1 but no complete manifest → absent (I-201)"
    );

    sqlx::query("UPDATE chunks SET durable = TRUE WHERE blake3_hash = $1")
        .bind(digest.as_slice())
        .execute(&s.db.pool)
        .await?;
    assert!(probe(&mut s.chunk).await?, "durable → present");

    // The GC tombstone clears durable presence.
    sqlx::query("UPDATE chunks SET deleted = TRUE WHERE blake3_hash = $1")
        .bind(digest.as_slice())
        .execute(&s.db.pool)
        .await?;
    assert!(!probe(&mut s.chunk).await?, "deleted tombstone → absent");
    Ok(())
}

/// HMAC enforcement: no token → PERMISSION_DENIED before anything is
/// read; a valid token whose `expected_outputs` covers the path →
/// committed.
#[tokio::test]
async fn hmac_token_gates_the_upload() -> TestResult {
    let key = b"chunked-hmac-test-key".to_vec();
    let mut s = ChunkedSession::new_with_hmac(key.clone()).await?;
    let tree = fixture_tree();
    let path = test_store_path("chunked-hmac");
    let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);
    let begin = assemble_begin(vec![out], vec![dirs]);

    let err = put_path_chunked(&mut s.store, begin.clone(), &chunks, None, |_, c| Some(c))
        .await
        .expect_err("missing token must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    let signer = rio_auth::hmac::HmacSigner::from_key(key);
    let token = signer.sign(&rio_auth::hmac::AssignmentClaims {
        executor_id: "builder-0".into(),
        drv_hash: "node0".into(),
        expected_outputs: vec![path.clone()],
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: None,
        role: rio_auth::hmac::TokenRole::Builder,
        input_closure_digest: String::new(),
    });
    assert!(
        put_path_chunked(&mut s.store, begin, &chunks, Some(&token), |_, c| Some(c))
            .await
            .expect("valid token should commit")
    );
    assert_eq!(s.manifest_status(&path).await.as_deref(), Some("complete"));
    Ok(())
}

/// Floating-CA: the store recomputes the CA path from the verified NAR
/// hash. A correct path commits (with no placeholder claimed before the
/// content is proven); a wrong path is PERMISSION_DENIED.
// r[verify store.put.chunked-ca]
// r[verify sec.authz.ca-path-derived+2]
#[tokio::test]
async fn ca_path_recompute_gates_commit() -> TestResult {
    let key = b"chunked-ca-test-key".to_vec();
    let signer = rio_auth::hmac::HmacSigner::from_key(key.clone());
    let ca_claims = |outputs: Vec<String>| rio_auth::hmac::AssignmentClaims {
        executor_id: "builder-0".into(),
        drv_hash: "ca-node".into(),
        expected_outputs: outputs,
        is_ca: true,
        expiry_unix: u64::MAX,
        tenant: None,
        role: rio_auth::hmac::TokenRole::Builder,
        input_closure_digest: String::new(),
    };

    // Build the tree, then derive the CORRECT CA path from the real
    // NAR hash so the store's recompute agrees.
    let tree = tempfile::tempdir()?;
    std::fs::write(tree.path().join("blob"), b"content-addressed bytes")?;
    let mut nar = Vec::new();
    rio_nix::nar::dump_path_streaming(tree.path(), &mut nar)?;
    let nar_hash = rio_nix::hash::NixHash::new(
        rio_nix::hash::HashAlgo::SHA256,
        sha2::Sha256::digest(&nar).to_vec(),
    )?;
    let ca_path =
        rio_nix::store_path::StorePath::make_fixed_output("ca-fixture", &nar_hash, true, &[])?;

    // Correct CA path → committed.
    {
        let mut s = ChunkedSession::new_with_hmac(key.clone()).await?;
        let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), ca_path.as_str(), 1024);
        let begin = assemble_begin(vec![out], vec![dirs]);
        let token = signer.sign(&ca_claims(vec![String::new()]));
        assert!(
            put_path_chunked(&mut s.store, begin, &chunks, Some(&token), |_, c| Some(c))
                .await
                .expect("correct CA path should commit")
        );
        assert_eq!(
            s.manifest_status(ca_path.as_str()).await.as_deref(),
            Some("complete")
        );
    }

    // Wrong store path under an is_ca token → PERMISSION_DENIED and no
    // placeholder row is ever claimed for the squatted path.
    {
        let mut s = ChunkedSession::new_with_hmac(key).await?;
        let wrong = test_store_path("ca-squat");
        let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &wrong, 1024);
        let begin = assemble_begin(vec![out], vec![dirs]);
        let token = signer.sign(&ca_claims(vec![String::new()]));
        let err = put_path_chunked(&mut s.store, begin, &chunks, Some(&token), |_, c| Some(c))
            .await
            .expect_err("wrong CA path must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM manifests")
            .fetch_one(&s.db.pool)
            .await?;
        assert_eq!(n, 0, "no placeholder is ever claimed for a wrong CA path");
    }
    Ok(())
}

/// Bounds violations are rejected before any placeholder or S3 write.
// r[verify store.put.chunked-bounds]
#[tokio::test]
async fn bounds_violations_rejected_before_any_write() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    let tree = fixture_tree();
    let path = test_store_path("chunked-bounds");
    let (out, dirs, chunks, _) = chunked_output_for_tree(tree.path(), &path, 1024);

    // Oversize nar_size.
    let mut o = out.clone();
    o.nar_size = rio_common::limits::MAX_NAR_SIZE + 1;
    let begin = assemble_begin(vec![o], vec![dirs.clone()]);
    let err = put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
        .await
        .expect_err("oversize nar_size");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // Missing Directory body.
    let mut short_dirs = dirs.clone();
    short_dirs.pop();
    let begin = assemble_begin(vec![out.clone()], vec![short_dirs]);
    let err = put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
        .await
        .expect_err("missing directory body");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // Nothing reached PG or the backend.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM manifests")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(n, 0, "no placeholder before validation passes");
    assert!(s.backend.is_empty(), "no S3 write before validation passes");
    Ok(())
}

/// Partial idempotency: one output is already `'complete'` (uploaded
/// via the LEGACY whole-NAR path, so its per-file chunk digests do not
/// exist in the CAS) and a second, new output rides in the same Begin.
/// The skipped output must not poison the new one's verify — the walk
/// must not try to fetch the skipped output's nonexistent chunks.
// r[verify store.put.idempotent]
#[tokio::test]
async fn partial_skip_does_not_block_new_outputs() -> TestResult {
    let mut s = ChunkedSession::new().await?;

    // Output A: committed via the legacy inline path (small NAR, no
    // chunk rows at all — the most hostile case for a re-drive that
    // assumes A's chunks exist).
    let tree_a = tempfile::tempdir()?;
    std::fs::write(tree_a.path().join("old"), b"legacy-uploaded contents")?;
    let path_a = test_store_path("chunked-partial-a");
    let (out_a, dirs_a, chunks_a, nar_a) = chunked_output_for_tree(tree_a.path(), &path_a, 7);
    let info_a = rio_test_support::fixtures::make_path_info_for_nar(&path_a, &nar_a);
    assert!(put_path(&mut s.store, info_a, nar_a).await?);

    // Output B: genuinely new.
    let tree_b = tempfile::tempdir()?;
    std::fs::write(tree_b.path().join("new"), vec![0x3Cu8; 5000])?;
    let path_b = test_store_path("chunked-partial-b");
    let (out_b, dirs_b, chunks_b, nar_b) = chunked_output_for_tree(tree_b.path(), &path_b, 1024);

    // The builder re-drives both outputs in one Begin (it does not
    // know A is already complete) and — the hostile half — declares
    // A's chunks NOT novel, as a HasChunks probe that raced another
    // uploader would. Those digests exist nowhere in the CAS (A went
    // through the inline path); the walk must not try to fetch them
    // for an output it is skipping anyway.
    let mut begin = assemble_begin(vec![out_a, out_b], vec![dirs_a, dirs_b]);
    let a_digests: std::collections::HashSet<Vec<u8>> =
        chunks_a.keys().map(|k| k.to_vec()).collect();
    begin.novel.retain(|d| !a_digests.contains(d));
    let chunks = chunks_b;
    let created = put_path_chunked(&mut s.store, begin, &chunks, None, |_, c| Some(c))
        .await
        .expect("partial-skip upload must succeed");
    assert!(created, "output B is newly created");
    assert_eq!(
        s.manifest_status(&path_b).await.as_deref(),
        Some("complete"),
        "the new output commits even though its sibling was skipped"
    );
    assert_eq!(s.get_nar(&path_b).await?, nar_b, "output B round-trips");
    Ok(())
}

/// The legacy whole-NAR PutPath also marks its chunks durable in its
/// completion transaction, so HasChunks covers content uploaded either
/// way.
#[tokio::test]
async fn legacy_put_path_marks_chunks_durable() -> TestResult {
    let mut s = ChunkedSession::new().await?;
    // 1 MiB NAR → forced through the legacy chunked path.
    let (nar, info, _) = rio_test_support::fixtures::make_large_nar(11, 1024 * 1024);
    assert!(put_path(&mut s.store, info, nar).await?);
    let (total, durable): (i64, i64) =
        sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE durable) FROM chunks")
            .fetch_one(&s.db.pool)
            .await?;
    assert!(total > 4, "1 MiB NAR FastCDC-chunks into several pieces");
    assert_eq!(
        total, durable,
        "legacy complete txn marks every chunk durable"
    );
    Ok(())
}
