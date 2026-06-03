//! `PutPathChunked` end-to-end tests (ADR-022 §6).
//!
//! Each test builds a `Begin` frame from a real on-disk tree the same
//! way the builder's fused walk would: `dump_path_streaming` →
//! `ParsedNar::parse` for the entries + Directory DAG, one chunk per
//! regular file (any chunk layout whose per-file run sums to
//! `FileEntry.size` is valid — single-chunk files keep the fixtures
//! readable), `novel` in global first-occurrence order.

use super::*;

use prost::Message as _;
use rio_nix::nar::{NarEntryKind, dump_path_streaming};
use rio_proto::types::{
    ChunkData, ChunkMeta, ChunkedOutput, GetPathRequest, PutPathChunkedBegin,
    PutPathChunkedRequest, get_path_response, put_path_chunked_request,
};
use rio_store::cas::ParsedNar;
use sha2::{Digest, Sha256};

/// Hash parts must be distinct across fixture paths — the refscan
/// candidate set is keyed by hash part, and `test_store_path` reuses
/// one hash for every name. Valid nixbase32 (no e/o/u/t).
const DEP_HASH: &str = "cccccccccccccccccccccccccccccccc";
const DERIVER: &str = "/nix/store/ffffffffffffffffffffffffffffffff-fixture.drv";

fn out_path(tag: &str) -> String {
    // A distinct 32-char nixbase32 hash part per tag keeps outputs
    // distinguishable in the refscan candidate set (which is keyed by
    // hash part). Tag characters must themselves be valid nixbase32.
    assert!(tag.len() < 32 && !tag.contains(['e', 'o', 'u', 't']));
    format!(
        "/nix/store/{tag}{}-chunked-{tag}",
        "b".repeat(32 - tag.len())
    )
}

fn dep_path() -> String {
    format!("/nix/store/{DEP_HASH}-some-dep")
}

/// One output's worth of `Begin` material derived from an on-disk tree.
struct TreeFixture {
    nar: Vec<u8>,
    output: ChunkedOutput,
    directories: Vec<rio_proto::castore::Directory>,
    /// `(digest, body)` for every chunk in `chunk_manifest` order.
    chunks: Vec<([u8; 32], Vec<u8>)>,
}

/// Derive everything `PutPathChunkedBegin` needs from a real tree:
/// NAR-dump it, index it, chunk each regular file as one whole-file
/// chunk, and lift the Directory DAG out of the index.
fn fixture_for_tree(dir: &std::path::Path, store_path: &str, refs: Vec<String>) -> TreeFixture {
    let mut nar = Vec::new();
    dump_path_streaming(dir, &mut nar).expect("fixture dump");
    let parsed = ParsedNar::parse(&nar).expect("fixture NAR parses");

    let mut chunks = Vec::new();
    let mut chunk_manifest = Vec::new();
    for e in &parsed.entries {
        if e.kind == NarEntryKind::Regular && e.size > 0 {
            let body = nar[e.nar_offset as usize..(e.nar_offset + e.size) as usize].to_vec();
            let digest = *blake3::hash(&body).as_bytes();
            chunk_manifest.push(ChunkMeta {
                digest: digest.to_vec(),
                size: e.size,
            });
            chunks.push((digest, body));
        }
    }

    let directories = parsed
        .dag
        .directories
        .iter()
        .map(|(_, body)| rio_proto::castore::Directory::decode(body.as_slice()).expect("decode"))
        .collect();
    let root_node =
        rio_proto::castore::RootNode::decode(parsed.dag.root_node.as_slice()).expect("decode");

    let nar_hash: [u8; 32] = Sha256::digest(&nar).into();
    TreeFixture {
        output: ChunkedOutput {
            store_path: store_path.to_string(),
            nar_hash: nar_hash.to_vec(),
            nar_size: nar.len() as u64,
            references: refs,
            root_node: Some(root_node),
            chunk_manifest,
        },
        directories,
        chunks,
        nar,
    }
}

/// Assemble a `Begin` + the `Chunk` frames for a set of outputs.
/// `novel` is every distinct chunk digest in global first-occurrence
/// order minus `already_durable`; the chunk frames follow that order.
fn assemble_begin(
    fixtures: &[&TreeFixture],
    input_closure: Vec<String>,
    already_durable: &std::collections::HashSet<[u8; 32]>,
) -> (PutPathChunkedBegin, Vec<ChunkData>) {
    let mut seen = std::collections::HashSet::new();
    let mut novel = Vec::new();
    let mut frames = Vec::new();
    let mut directories: Vec<rio_proto::castore::Directory> = Vec::new();
    let mut dir_seen = std::collections::HashSet::new();
    for f in fixtures {
        for (digest, body) in &f.chunks {
            if seen.insert(*digest) && !already_durable.contains(digest) {
                novel.push(digest.to_vec());
                frames.push(ChunkData {
                    digest: digest.to_vec(),
                    data: body.clone().into(),
                });
            }
        }
        for d in &f.directories {
            if dir_seen.insert(rio_proto::castore_util::directory_digest(d)) {
                directories.push(d.clone());
            }
        }
    }
    (
        PutPathChunkedBegin {
            deriver: DERIVER.to_string(),
            outputs: fixtures.iter().map(|f| f.output.clone()).collect(),
            directories,
            novel,
            input_closure,
        },
        frames,
    )
}

/// Drive the client stream: `Begin` then each `Chunk` frame in order.
async fn send_chunked(
    client: &mut StoreServiceClient<Channel>,
    begin: PutPathChunkedBegin,
    frames: Vec<ChunkData>,
    token: Option<&str>,
) -> Result<Vec<bool>, tonic::Status> {
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let _ = tx
            .send(PutPathChunkedRequest {
                msg: Some(put_path_chunked_request::Msg::Begin(begin)),
            })
            .await;
        for f in frames {
            let _ = tx
                .send(PutPathChunkedRequest {
                    msg: Some(put_path_chunked_request::Msg::Chunk(f)),
                })
                .await;
        }
    });
    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    if let Some(t) = token {
        req.metadata_mut().insert(
            rio_proto::ASSIGNMENT_TOKEN_HEADER,
            t.parse().expect("token is ASCII"),
        );
    }
    client
        .put_path_chunked(req)
        .await
        .map(|r| r.into_inner().created)
}

/// Stream a path back and concatenate the NarChunk messages.
async fn get_path_bytes(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> Result<Vec<u8>, tonic::Status> {
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

/// The standard multi-node fixture tree: a nested dir, an executable,
/// an empty file, a symlink, and a file whose content embeds the
/// dependency's store path (so the reference scan finds it).
fn write_fixture_tree(root: &std::path::Path, embed: &str) {
    std::fs::create_dir(root).unwrap();
    std::fs::write(root.join("data"), format!("points at {embed}\n")).unwrap();
    std::fs::write(root.join("empty"), b"").unwrap();
    std::fs::create_dir(root.join("bin")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(root.join("bin/app"), b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(root.join("bin/app"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    std::os::unix::fs::symlink("bin/app", root.join("run")).unwrap();
}

async fn count(pool: &sqlx::PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// r[verify store.put.chunked]
// r[verify store.chunk.self-verify]
/// Happy path: one output, every chunk novel. The manifest commits,
/// `GetPath` reproduces the original NAR byte-for-byte, every
/// referenced chunk is durable, and the castore index rows exist.
#[tokio::test]
async fn happy_path_commits_and_roundtrips() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());

    let path = out_path("hp");
    let fx = fixture_for_tree(&root, &path, vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());
    assert!(!frames.is_empty(), "fixture must have novel chunks");

    let created = send_chunked(&mut s.client, begin, frames, None).await?;
    assert_eq!(created, vec![true]);

    // Byte-for-byte NAR round-trip through the framing regenerator.
    let got = get_path_bytes(&mut s.client, &path).await?;
    assert_eq!(got, fx.nar, "GetPath must reproduce the original NAR");

    assert!(!backend.is_empty(), "novel chunks reached the backend");
    let status: String = sqlx::query_scalar("SELECT status FROM manifests")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(status, "complete");

    // Every referenced chunk is durable + uploaded — the durable flip
    // happens in the same transaction as the status flip.
    let not_durable = count(
        &s.db.pool,
        "SELECT COUNT(*) FROM chunks WHERE NOT durable OR uploaded_at IS NULL",
    )
    .await;
    assert_eq!(not_durable, 0, "all chunks durable + uploaded_at set");

    // Castore index rows written in the commit transaction.
    for (table, n) in [
        (
            "nar_index",
            count(&s.db.pool, "SELECT COUNT(*) FROM nar_index").await,
        ),
        (
            "directories",
            count(&s.db.pool, "SELECT COUNT(*) FROM directories").await,
        ),
        (
            "directory_paths",
            count(&s.db.pool, "SELECT COUNT(*) FROM directory_paths").await,
        ),
        (
            "file_blobs",
            count(&s.db.pool, "SELECT COUNT(*) FROM file_blobs").await,
        ),
    ] {
        assert!(
            n > 0,
            "{table} should have rows after a chunked commit, got {n}"
        );
    }
    Ok(())
}

// r[verify store.put.idempotent]
/// Re-uploading an already-complete output returns `created = [false]`
/// and does not double the directory refcounts.
#[tokio::test]
async fn idempotent_reupload_skips_and_keeps_refcounts() -> TestResult {
    let (mut s, _backend) = StoreSession::new_chunked().await?;
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());

    let path = out_path("id");
    let fx = fixture_for_tree(&root, &path, vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());

    let created = send_chunked(&mut s.client, begin.clone(), frames.clone(), None).await?;
    assert_eq!(created, vec![true]);
    let dir_refs: Vec<i64> = sqlx::query_scalar("SELECT refcount::bigint FROM directories")
        .fetch_all(&s.db.pool)
        .await?;
    let chunk_refs: i64 = count(&s.db.pool, "SELECT COALESCE(SUM(refcount),0) FROM chunks").await;

    let created = send_chunked(&mut s.client, begin, frames, None).await?;
    assert_eq!(created, vec![false], "second upload is an idempotent skip");

    let dir_refs_after: Vec<i64> = sqlx::query_scalar("SELECT refcount::bigint FROM directories")
        .fetch_all(&s.db.pool)
        .await?;
    assert_eq!(dir_refs, dir_refs_after, "directory refcounts unchanged");
    let chunk_refs_after: i64 =
        count(&s.db.pool, "SELECT COALESCE(SUM(refcount),0) FROM chunks").await;
    assert_eq!(chunk_refs, chunk_refs_after, "chunk refcounts unchanged");
    Ok(())
}

/// Counts backend `get` calls. The §6.3 receive walk must never fetch
/// chunks: the builder's claims are committed as claimed, so there is
/// nothing to recompute from already-durable bodies.
#[derive(Default)]
struct GetCountingBackend {
    inner: MemoryChunkBackend,
    gets: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ChunkBackend for GetCountingBackend {
    async fn put(&self, hash: &[u8; 32], data: bytes::Bytes) -> anyhow::Result<()> {
        self.inner.put(hash, data).await
    }
    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(hash).await
    }
    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        self.inner.exists_batch(hashes).await
    }
    fn key_for(&self, hash: &[u8; 32]) -> String {
        self.inner.key_for(hash)
    }
    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete_by_key(key).await
    }
    async fn put_blob(&self, key: &str, data: bytes::Bytes) -> anyhow::Result<()> {
        self.inner.put_blob(key, data).await
    }
    async fn get_blob(&self, key: &str) -> anyhow::Result<Option<bytes::Bytes>> {
        self.inner.get_blob(key).await
    }
    async fn delete_blob(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete_blob(key).await
    }
}

/// A fully-deduped upload (every chunk already durable, `novel` empty,
/// no bodies on the stream) commits without a single backend chunk
/// read. Pins the structural fix for the deduped-upload stall: the
/// old verify walk re-fetched every chunk serially to recompute the
/// NAR hash, which on a large fully-deduped output took longer than
/// the client's stream timeout.
// r[verify store.integrity.verify-on-put+2]
#[tokio::test]
async fn fully_deduped_upload_commits_with_zero_chunk_reads() -> TestResult {
    let backend = Arc::new(GetCountingBackend::default());
    let mut s =
        StoreSession::new_chunked_with_backend(Arc::clone(&backend) as Arc<dyn ChunkBackend>)
            .await?;

    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());

    // Path A uploads everything.
    let fx_a = fixture_for_tree(&root, &out_path("zra"), vec![dep_path()]);
    let (begin_a, frames_a) = assemble_begin(&[&fx_a], vec![dep_path()], &Default::default());
    assert_eq!(
        send_chunked(&mut s.client, begin_a, frames_a, None).await?,
        vec![true]
    );

    // Path B: the same tree at a different store path — every chunk
    // is already durable.
    let fx_b = fixture_for_tree(&root, &out_path("zrb"), vec![dep_path()]);
    let durable: std::collections::HashSet<[u8; 32]> =
        fx_b.chunks.iter().map(|(d, _)| *d).collect();
    let (begin_b, frames_b) = assemble_begin(&[&fx_b], vec![dep_path()], &durable);
    assert!(
        begin_b.novel.is_empty() && frames_b.is_empty(),
        "fixture must be fully deduped"
    );
    assert_eq!(
        send_chunked(&mut s.client, begin_b, frames_b, None).await?,
        vec![true]
    );

    assert_eq!(
        backend.gets.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "PutPathChunked must never read chunks from the backend"
    );
    let complete: i64 = count(
        &s.db.pool,
        "SELECT COUNT(*) FROM manifests WHERE status = 'complete'",
    )
    .await;
    assert_eq!(complete, 2, "both paths committed");
    Ok(())
}

// r[verify store.integrity.verify-on-put+2]
/// Wire-protocol violations: chunks out of `novel` order, a corrupt
/// chunk body, and an extra frame after `novel` is exhausted are all
/// INVALID_ARGUMENT; a truncated stream is FAILED_PRECONDITION
/// (incomplete).
#[tokio::test]
async fn protocol_violations_rejected() -> TestResult {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());
    let fx = fixture_for_tree(&root, &out_path("pv"), vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());
    assert!(
        frames.len() >= 2,
        "fixture needs at least two novel chunks to reorder"
    );

    // Out of order.
    {
        let (mut s, _b) = StoreSession::new_chunked().await?;
        let mut reordered = frames.clone();
        reordered.swap(0, 1);
        let err = send_chunked(&mut s.client, begin.clone(), reordered, None)
            .await
            .expect_err("out-of-order chunks");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    }
    // Corrupt body (digest no longer matches).
    {
        let (mut s, _b) = StoreSession::new_chunked().await?;
        let mut corrupt = frames.clone();
        let mut data = corrupt[0].data.to_vec();
        data[0] ^= 0xFF;
        corrupt[0].data = data.into();
        let err = send_chunked(&mut s.client, begin.clone(), corrupt, None)
            .await
            .expect_err("corrupt chunk body");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    }
    // Truncated: stream ends before the last novel chunk.
    {
        let (mut s, _b) = StoreSession::new_chunked().await?;
        let mut truncated = frames.clone();
        truncated.pop();
        let err = send_chunked(&mut s.client, begin.clone(), truncated, None)
            .await
            .expect_err("truncated stream");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
    }
    // Extra frame after novel is exhausted.
    {
        let (mut s, _b) = StoreSession::new_chunked().await?;
        let mut extra = frames.clone();
        extra.push(extra[0].clone());
        let err = send_chunked(&mut s.client, begin.clone(), extra, None)
            .await
            .expect_err("extra chunk frame");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    }
    // A digest field that is not 32 bytes is rejected on length before
    // it is compared or echoed into an error message.
    {
        let (mut s, _b) = StoreSession::new_chunked().await?;
        let mut bad_digest = frames.clone();
        bad_digest[0].digest = vec![0u8; 33];
        let err = send_chunked(&mut s.client, begin.clone(), bad_digest, None)
            .await
            .expect_err("oversized digest field");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
        assert!(err.message().contains("32 bytes"), "{err:?}");
    }
    Ok(())
}

/// Cross-upload dedup: path B shares a chunk with already-committed
/// path A. B's `novel` excludes the shared digest and its stream sends
/// only the truly-novel chunks; the shared chunk's refcount reaches 2.
#[tokio::test]
async fn dedup_shares_chunks_across_uploads() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;
    let shared_content = b"this exact blob appears in both outputs".to_vec();

    // Path A.
    let dir_a = tempfile::TempDir::new()?;
    let root_a = dir_a.path().join("root");
    std::fs::create_dir(&root_a)?;
    std::fs::write(root_a.join("shared"), &shared_content)?;
    std::fs::write(root_a.join("only-a"), b"unique to a")?;
    let fx_a = fixture_for_tree(&root_a, &out_path("da"), vec![]);
    let (begin_a, frames_a) = assemble_begin(&[&fx_a], vec![], &Default::default());
    assert_eq!(
        send_chunked(&mut s.client, begin_a, frames_a, None).await?,
        vec![true]
    );
    let backend_after_a = backend.len();

    // Path B shares the `shared` file's (single) chunk with A.
    let dir_b = tempfile::TempDir::new()?;
    let root_b = dir_b.path().join("root");
    std::fs::create_dir(&root_b)?;
    std::fs::write(root_b.join("shared"), &shared_content)?;
    std::fs::write(root_b.join("only-b"), b"unique to b")?;
    let fx_b = fixture_for_tree(&root_b, &out_path("db"), vec![]);
    let shared_digest = *blake3::hash(&shared_content).as_bytes();
    let durable: std::collections::HashSet<[u8; 32]> = [shared_digest].into();
    let (begin_b, frames_b) = assemble_begin(&[&fx_b], vec![], &durable);
    assert!(
        !begin_b.novel.iter().any(|d| d.as_slice() == shared_digest),
        "B's novel must exclude the already-durable shared chunk"
    );
    assert_eq!(
        send_chunked(&mut s.client, begin_b, frames_b, None).await?,
        vec![true]
    );

    assert_eq!(
        backend.len(),
        backend_after_a + 1,
        "B uploads only its truly-novel chunk"
    );
    let rc: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
        .bind(shared_digest.as_slice())
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(rc, 2, "shared chunk referenced by both manifests");
    Ok(())
}

/// Partial idempotent skip with a novel chunk whose only occurrence is
/// in the skipped output (the second-builder-races shape): the skipped
/// output's novel chunks are still consumed, PUT, and stamped
/// `uploaded_at` by the commit transaction, the non-skipped output
/// commits, and nothing is double-counted. This is the call shape where
/// `mark_chunks_uploaded_in_conn` touches digests outside the
/// non-skipped manifests' union — `lock_chunks_for_commit` must lock
/// those too (see `lock_chunks_for_commit_locks_uploaded_only_digests`
/// for the lock-level assertion).
#[tokio::test]
async fn partial_skip_with_skipped_only_novel_chunk_commits() -> TestResult {
    let (mut s, _backend) = StoreSession::new_chunked().await?;

    // Output 1, committed first (the "winner" builder).
    let dir_a = tempfile::TempDir::new()?;
    let root_a = dir_a.path().join("root");
    std::fs::create_dir(&root_a)?;
    std::fs::write(
        root_a.join("only-in-skipped"),
        b"content unique to output one",
    )?;
    let fx_a = fixture_for_tree(&root_a, &out_path("psa"), vec![]);
    let (begin_a, frames_a) = assemble_begin(&[&fx_a], vec![], &Default::default());
    assert_eq!(
        send_chunked(&mut s.client, begin_a, frames_a, None).await?,
        vec![true]
    );
    let skipped_only_digest = *blake3::hash(b"content unique to output one").as_bytes();
    let rc_before: i32 = sqlx::query_scalar("SELECT refcount FROM chunks WHERE blake3_hash = $1")
        .bind(skipped_only_digest.as_slice())
        .fetch_one(&s.db.pool)
        .await?;

    // The "loser" builder computed its Begin before the winner's commit:
    // both outputs, every chunk listed as novel (nothing marked
    // already-durable). Output 1 idempotent-skips at phase A; its novel
    // chunk is consumed off the stream and PUT anyway.
    let dir_b = tempfile::TempDir::new()?;
    let root_b = dir_b.path().join("root");
    std::fs::create_dir(&root_b)?;
    std::fs::write(root_b.join("only-b"), b"content unique to output two")?;
    let fx_b = fixture_for_tree(&root_b, &out_path("psb"), vec![]);
    let (begin, frames) = assemble_begin(&[&fx_a, &fx_b], vec![], &Default::default());
    let created = send_chunked(&mut s.client, begin, frames, None).await?;
    assert_eq!(
        created,
        vec![false, true],
        "output 1 idempotent-skips, output 2 commits"
    );

    // The skipped-only chunk keeps its single refcount (the skip must
    // not double-count) and stays S3-confirmed; output 2's chunks are
    // durable via its freshly-committed manifest.
    let (rc_after, uploaded, durable): (i32, bool, bool) = sqlx::query_as(
        "SELECT refcount, uploaded_at IS NOT NULL, durable FROM chunks WHERE blake3_hash = $1",
    )
    .bind(skipped_only_digest.as_slice())
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        rc_after, rc_before,
        "idempotent skip must not bump refcounts"
    );
    assert!(uploaded, "the skipped-only novel chunk is S3-confirmed");
    assert!(durable, "still referenced by output 1's complete manifest");
    let b_digest = *blake3::hash(b"content unique to output two").as_bytes();
    let (b_durable, b_uploaded): (bool, bool) = sqlx::query_as(
        "SELECT durable, uploaded_at IS NOT NULL FROM chunks WHERE blake3_hash = $1",
    )
    .bind(b_digest.as_slice())
    .fetch_one(&s.db.pool)
    .await?;
    assert!(b_durable && b_uploaded, "output 2's chunk commits normally");
    Ok(())
}

// r[verify store.chunk.durable-flag]
/// A chunk the GC sweep claims between the builder's `HasChunks` probe
/// and the commit transaction must NOT be committed as `durable`: its
/// S3 object may already be drained away, and a manifest pointing at
/// it would make `HasChunks` lie to every future uploader. The commit
/// detects the claim on the row-locked chunk and aborts UNAVAILABLE so
/// the builder re-probes and re-streams.
#[tokio::test]
async fn gc_claimed_chunk_aborts_commit_unavailable() -> TestResult {
    let (mut s, _backend) = StoreSession::new_chunked().await?;
    let shared_content = b"chunk that gets GC-claimed between fetch and commit".to_vec();

    // Path A commits the chunk normally.
    let dir_a = tempfile::TempDir::new()?;
    let root_a = dir_a.path().join("root");
    std::fs::create_dir(&root_a)?;
    std::fs::write(root_a.join("shared"), &shared_content)?;
    let fx_a = fixture_for_tree(&root_a, &out_path("ga"), vec![]);
    let (begin_a, frames_a) = assemble_begin(&[&fx_a], vec![], &Default::default());
    assert_eq!(
        send_chunked(&mut s.client, begin_a, frames_a, None).await?,
        vec![true]
    );

    // Simulate the GC: A's manifest is collected and the sweep claims
    // the now-refcount-0 chunk for the drain. The S3 object is still
    // in the backend (the drain hasn't run yet) — exactly the window
    // where trusting the builder's HasChunks-era view would commit a
    // manifest whose object is about to disappear.
    sqlx::query("DELETE FROM narinfo")
        .execute(&s.db.pool)
        .await?;
    sqlx::query(
        "UPDATE chunks SET refcount = 0, deleted = TRUE, uploaded_at = NULL, durable = FALSE",
    )
    .execute(&s.db.pool)
    .await?;

    // Path B references the chunk as already-durable (its HasChunks
    // probe predates the sweep), so it is excluded from `novel` and
    // nothing in B's upload resets the GC claim.
    let dir_b = tempfile::TempDir::new()?;
    let root_b = dir_b.path().join("root");
    std::fs::create_dir(&root_b)?;
    std::fs::write(root_b.join("shared"), &shared_content)?;
    std::fs::write(root_b.join("only-b"), b"unique to b")?;
    let fx_b = fixture_for_tree(&root_b, &out_path("gb"), vec![]);
    let shared_digest = *blake3::hash(&shared_content).as_bytes();
    let durable: std::collections::HashSet<[u8; 32]> = [shared_digest].into();
    let (begin_b, frames_b) = assemble_begin(&[&fx_b], vec![], &durable);

    let err = send_chunked(&mut s.client, begin_b, frames_b, None)
        .await
        .expect_err("a GC-claimed chunk must not be committed as durable");
    assert_eq!(err.code(), tonic::Code::Unavailable, "{err:?}");

    // The placeholder is reaped; nothing claims the swept chunk.
    let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT COUNT(*) FROM manifests", 0).await;
    assert_eq!(n, 0);
    let still_deleted: bool =
        sqlx::query_scalar("SELECT deleted FROM chunks WHERE blake3_hash = $1")
            .bind(shared_digest.as_slice())
            .fetch_one(&s.db.pool)
            .await?;
    assert!(
        still_deleted,
        "the aborted commit must not resurrect the GC-claimed row"
    );
    Ok(())
}

// r[verify store.put.chunked-wire]
/// The HMAC claims path: a valid assignment token whose
/// `expected_outputs` covers the uploaded path commits; the same
/// stream without a token is PERMISSION_DENIED.
#[tokio::test]
async fn hmac_token_gates_the_upload() -> TestResult {
    let key = b"chunked-hmac-test-key-32-bytes!!".to_vec();
    let (mut s, _backend) = StoreSession::new_chunked_with_hmac(key.clone()).await?;
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());

    let path = out_path("hm");
    let fx = fixture_for_tree(&root, &path, vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());

    let err = send_chunked(&mut s.client, begin.clone(), frames.clone(), None)
        .await
        .expect_err("missing token must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");

    let token = rio_auth::hmac::HmacSigner::from_key(key).sign(&rio_auth::hmac::AssignmentClaims {
        executor_id: "builder-0".into(),
        drv_hash: DERIVER.into(),
        expected_outputs: vec![path.clone()],
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: None,
        input_closure_digest: rio_auth::hmac::AssignmentClaims::digest_input_closure(&[dep_path()]),
    });
    let created = send_chunked(&mut s.client, begin, frames, Some(&token)).await?;
    assert_eq!(created, vec![true]);
    Ok(())
}

// r[verify store.castore.tenant-scope]
/// The builder authenticates with the HMAC assignment token only (no
/// gateway JWT), so the `path_tenants` junction MUST be written from
/// the token's `tenant` claim — both by the commit transaction and by
/// the all-outputs-skipped early return. Without it the just-uploaded
/// outputs are castore-invisible to (and unpinned for) their own
/// tenant until the scheduler's best-effort completion upsert.
#[tokio::test]
async fn hmac_tenant_writes_path_tenants_junction() -> TestResult {
    let key = b"chunked-hmac-test-key-32-bytes!!".to_vec();
    let (mut s, _backend) = StoreSession::new_chunked_with_hmac(key.clone()).await?;
    let tenant_a = rio_store::test_helpers::seed_tenant(&s.db.pool, "chunked-tenant-a").await;
    let tenant_b = rio_store::test_helpers::seed_tenant(&s.db.pool, "chunked-tenant-b").await;

    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());
    let path = out_path("pj");
    let fx = fixture_for_tree(&root, &path, vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());

    let token_for = |tenant: uuid::Uuid| {
        rio_auth::hmac::HmacSigner::from_key(key.clone()).sign(&rio_auth::hmac::AssignmentClaims {
            executor_id: "builder-0".into(),
            drv_hash: DERIVER.into(),
            expected_outputs: vec![path.clone()],
            is_ca: false,
            expiry_unix: u64::MAX,
            tenant: Some(tenant.to_string()),
            input_closure_digest: rio_auth::hmac::AssignmentClaims::digest_input_closure(&[
                dep_path(),
            ]),
        })
    };

    // Tenant A's builder commits the output: the commit transaction
    // itself must write A's junction row (no JWT anywhere in sight).
    let created = send_chunked(
        &mut s.client,
        begin.clone(),
        frames.clone(),
        Some(&token_for(tenant_a)),
    )
    .await?;
    assert_eq!(created, vec![true]);
    let store_path_hash = rio_store::test_helpers::path_hash(&path);
    let tenants_after_a: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&store_path_hash)
            .fetch_all(&s.db.pool)
            .await?;
    assert_eq!(
        tenants_after_a,
        vec![tenant_a],
        "an HMAC-only builder upload must write its tenant's junction row in the commit"
    );

    // Tenant B re-uploads the same output: idempotent skip
    // (created=[false]), but the early-return path must still write B's
    // junction so B can read the path's castore and pin it against A's
    // retention window lapsing.
    let created = send_chunked(&mut s.client, begin, frames, Some(&token_for(tenant_b))).await?;
    assert_eq!(created, vec![false]);
    let mut tenants_after_b: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&store_path_hash)
            .fetch_all(&s.db.pool)
            .await?;
    tenants_after_b.sort();
    let mut expected = vec![tenant_a, tenant_b];
    expected.sort();
    assert_eq!(
        tenants_after_b, expected,
        "the idempotent-skip early return must write the second tenant's junction row"
    );
    Ok(())
}

/// An assignment token can name a tenant that was deleted while the
/// build was in flight (`path_tenants.tenant_id` REFERENCES `tenants`
/// ON DELETE CASCADE, so the junction insert raises a foreign-key
/// violation). The fully-verified upload MUST still commit — a
/// junction row for a deleted tenant is meaningless, so the write is
/// skipped and the un-pinned path simply ages out via normal GC
/// retention — instead of failing with a misleading, non-retryable
/// ALREADY_EXISTS. Covers both the commit-transaction junction insert
/// (tenant A, fresh output) and the all-outputs-skipped early return
/// (tenant B, idempotent re-upload).
#[tokio::test]
async fn deleted_tenant_skips_junction_but_upload_commits() -> TestResult {
    let key = b"chunked-hmac-test-key-32-bytes!!".to_vec();
    let (mut s, _backend) = StoreSession::new_chunked_with_hmac(key.clone()).await?;
    let tenant_a = rio_store::test_helpers::seed_tenant(&s.db.pool, "chunked-gone-a").await;
    let tenant_b = rio_store::test_helpers::seed_tenant(&s.db.pool, "chunked-gone-b").await;

    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());
    let path = out_path("dl");
    let fx = fixture_for_tree(&root, &path, vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());

    let token_for = |tenant: uuid::Uuid| {
        rio_auth::hmac::HmacSigner::from_key(key.clone()).sign(&rio_auth::hmac::AssignmentClaims {
            executor_id: "builder-0".into(),
            drv_hash: DERIVER.into(),
            expected_outputs: vec![path.clone()],
            is_ca: false,
            expiry_unix: u64::MAX,
            tenant: Some(tenant.to_string()),
            input_closure_digest: rio_auth::hmac::AssignmentClaims::digest_input_closure(&[
                dep_path(),
            ]),
        })
    };

    // The token was minted before the tenant was deleted — the
    // in-flight-build shape. The commit transaction's junction insert
    // hits the FK violation; the upload must commit anyway.
    let token_a = token_for(tenant_a);
    sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
        .bind(tenant_a)
        .execute(&s.db.pool)
        .await?;
    let created = send_chunked(&mut s.client, begin.clone(), frames.clone(), Some(&token_a))
        .await
        .expect("a tenant deleted mid-build must not fail a fully-verified upload");
    assert_eq!(created, vec![true]);

    let store_path_hash = rio_store::test_helpers::path_hash(&path);
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM manifests WHERE store_path_hash = $1")
            .bind(&store_path_hash)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(status, "complete", "the output itself committed");
    let junction_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants WHERE store_path_hash = $1")
            .bind(&store_path_hash)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(junction_rows, 0, "no junction row for a deleted tenant");

    // Same shape through the all-outputs-skipped early return
    // (`insert_path_tenants_for_all`): tenant B's re-upload of the
    // already-complete output succeeds with no junction row either.
    let token_b = token_for(tenant_b);
    sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
        .bind(tenant_b)
        .execute(&s.db.pool)
        .await?;
    let created = send_chunked(&mut s.client, begin, frames, Some(&token_b))
        .await
        .expect("the idempotent-skip early return must tolerate a deleted tenant too");
    assert_eq!(created, vec![false]);
    let junction_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants WHERE store_path_hash = $1")
            .bind(&store_path_hash)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        junction_rows, 0,
        "still no junction row for deleted tenants"
    );
    Ok(())
}

// r[verify store.put.chunked-ca]
/// The CA gate's rejection half: under an `is_ca` claim (empty
/// `expected_outputs`, as at dispatch time — see the legacy-PutPath CA
/// tests in `hmac.rs`), a claimed output store path that does NOT match
/// the path derived from the claimed NAR hash and references is
/// PERMISSION_DENIED, and no placeholder is left behind. The accepting
/// half (matching path commits and reads back) is the floating-CA
/// subtest of the `vm-put-path-chunked` scenario.
#[tokio::test]
async fn ca_path_mismatch_rejects() -> TestResult {
    let key = b"chunked-hmac-test-key-32-bytes!!".to_vec();
    let (mut s, _backend) = StoreSession::new_chunked_with_hmac(key.clone()).await?;
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());

    // The claimed path is a fixed test path, not the fixed-output
    // derivation of the claimed hash, so the CA gate can never agree.
    let fx = fixture_for_tree(&root, &out_path("ca"), vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());

    let token = rio_auth::hmac::HmacSigner::from_key(key).sign(&rio_auth::hmac::AssignmentClaims {
        executor_id: "builder-0".into(),
        drv_hash: DERIVER.into(),
        expected_outputs: vec![],
        is_ca: true,
        expiry_unix: u64::MAX,
        tenant: None,
        input_closure_digest: rio_auth::hmac::AssignmentClaims::digest_input_closure(&[dep_path()]),
    });
    let err = send_chunked(&mut s.client, begin, frames, Some(&token))
        .await
        .expect_err("is_ca upload to a non-content-derived path must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");
    assert!(err.message().contains("content-derived CA path"), "{err:?}");

    // CA outputs claim no placeholder before the path check, so the
    // rejected upload must not leave a manifest row behind.
    let n = count(&s.db.pool, "SELECT COUNT(*) FROM manifests").await;
    assert_eq!(n, 0, "rejected CA upload must not leave a placeholder");
    Ok(())
}

// ── verify-pipeline tests (bounded-concurrent chunk PUTs) ──────────

/// Latency-injecting wrapper around the memory backend: tracks the PUT
/// in-flight high-water mark — the structural "uploads overlapped"
/// signal. The 15 ms sleep keeps each PUT alive long enough for the
/// walk to receive and submit later chunks while it runs.
#[derive(Default)]
struct LatencyHighWaterBackend {
    inner: MemoryChunkBackend,
    in_flight: std::sync::atomic::AtomicUsize,
    high_water: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ChunkBackend for LatencyHighWaterBackend {
    async fn put(&self, hash: &[u8; 32], data: bytes::Bytes) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.high_water.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let r = self.inner.put(hash, data).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        r
    }
    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
        self.inner.get(hash).await
    }
    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        self.inner.exists_batch(hashes).await
    }
    fn key_for(&self, hash: &[u8; 32]) -> String {
        self.inner.key_for(hash)
    }
    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete_by_key(key).await
    }
    async fn put_blob(&self, key: &str, data: bytes::Bytes) -> anyhow::Result<()> {
        self.inner.put_blob(key, data).await
    }
    async fn get_blob(&self, key: &str) -> anyhow::Result<Option<bytes::Bytes>> {
        self.inner.get_blob(key).await
    }
    async fn delete_blob(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete_blob(key).await
    }
}

/// Every PUT fails — the S3-outage-mid-stream case.
struct FailingPutBackend;

#[async_trait::async_trait]
impl ChunkBackend for FailingPutBackend {
    async fn put(&self, _: &[u8; 32], _: bytes::Bytes) -> anyhow::Result<()> {
        anyhow::bail!("injected S3 outage")
    }
    async fn get(&self, _: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
        unimplemented!("PutPathChunked with all-novel chunks never GETs")
    }
    async fn exists_batch(&self, _: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        unimplemented!()
    }
    fn key_for(&self, hash: &[u8; 32]) -> String {
        hex::encode(hash)
    }
    async fn delete_by_key(&self, _: &str) -> anyhow::Result<()> {
        // The placeholder reaper may try to clean up after the abort.
        Ok(())
    }
    async fn put_blob(&self, _: &str, _: bytes::Bytes) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn get_blob(&self, _: &str) -> anyhow::Result<Option<bytes::Bytes>> {
        unimplemented!()
    }
    async fn delete_blob(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

// r[verify store.cas.upload-bounded]
/// The verify walk's chunk PUTs overlap (bounded by
/// `chunk_upload_max_concurrent`) and the pipelining changes nothing
/// about what gets stored: the committed path round-trips
/// byte-identically. Serial PUTs — the regression this guards against
/// — would hold the high-water mark at exactly 1.
#[tokio::test]
async fn verify_walk_pipelines_chunk_puts() -> TestResult {
    use std::sync::atomic::Ordering;

    let backend = Arc::new(LatencyHighWaterBackend::default());
    let mut s =
        StoreSession::new_chunked_with_backend(Arc::clone(&backend) as Arc<dyn ChunkBackend>)
            .await?;

    // 32 distinct-content files → 32 novel chunks through the walk.
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    std::fs::create_dir(&root)?;
    for i in 0..32 {
        std::fs::write(root.join(format!("f{i:02}")), format!("chunk-body-{i:02}"))?;
    }
    let path = out_path("plp");
    let fx = fixture_for_tree(&root, &path, vec![]);
    let (begin, frames) = assemble_begin(&[&fx], vec![], &Default::default());
    assert_eq!(
        send_chunked(&mut s.client, begin, frames, None).await?,
        vec![true]
    );
    assert_eq!(get_path_bytes(&mut s.client, &path).await?, fx.nar);

    let hw = backend.high_water.load(Ordering::SeqCst);
    assert!(
        hw > 1,
        "chunk PUTs never overlapped (high_water={hw}) — the serial-ingest regression"
    );
    assert!(
        hw <= rio_store::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY,
        "concurrency bound violated (high_water={hw})"
    );
    Ok(())
}

/// A PUT failure mid-stream surfaces as the same retryable Unavailable
/// the serial path produced, and nothing commits — `uploaded` must
/// only ever contain confirmed writes, so a manifest can never
/// reference a chunk that was not stored.
#[tokio::test]
async fn put_failure_mid_stream_is_unavailable_and_uncommitted() -> TestResult {
    let mut s = StoreSession::new_chunked_with_backend(Arc::new(FailingPutBackend)).await?;
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());
    let fx = fixture_for_tree(&root, &out_path("plf"), vec![dep_path()]);
    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());

    let err = send_chunked(&mut s.client, begin, frames, None)
        .await
        .expect_err("failed chunk PUTs must fail the upload");
    assert_eq!(err.code(), tonic::Code::Unavailable, "{err:?}");

    let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT COUNT(*) FROM manifests", 0).await;
    assert_eq!(n, 0, "no manifest may commit when chunk PUTs failed");
    Ok(())
}

// ── placeholder reap / stale-reclaim tests ─────────────────────────

/// An upload aborted mid-stream (Begin sent, stream closed before any
/// novel chunk) must not leave its placeholder behind: the
/// PlaceholderGuard drop-reap removes it, so the NEXT attempt starts
/// clean instead of hitting Aborted-concurrent until the stale
/// threshold. Run 5's silent variant of this is why the reap now logs
/// every outcome.
#[tokio::test]
async fn aborted_stream_reaps_placeholder() -> TestResult {
    let (mut s, _backend) = StoreSession::new_chunked().await?;
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());
    let fx = fixture_for_tree(&root, &out_path("rps"), vec![dep_path()]);
    let (begin, _frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());

    // Begin only — the sender closes with novel chunks outstanding,
    // the verify walk sees the stream end (Incomplete).
    let err = send_chunked(&mut s.client, begin, Vec::new(), None)
        .await
        .expect_err("incomplete stream must fail the upload");
    assert_ne!(err.code(), tonic::Code::Ok, "{err:?}");

    // The drop-reap is spawned fire-and-forget from the guard's Drop;
    // poll until it lands. A leftover row here is exactly the run-5
    // wedge.
    let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT COUNT(*) FROM manifests", 0).await;
    assert_eq!(n, 0, "aborted upload left its placeholder behind");
    Ok(())
}

// r[verify store.substitute.stale-reclaim+4]
/// The hot-path stale threshold is 90 s (3× the placeholder
/// heartbeat), not the old 5 minutes: a placeholder whose owner died
/// 2 minutes ago must be reclaimed by the next upload attempt instead
/// of aborting it as concurrent. (A LIVE owner heartbeats every 30 s,
/// so 120 s of silence means the owning future is gone; reap_one
/// re-checks the threshold inside its tx, so a racing fresh re-upload
/// is never collected.)
#[tokio::test]
async fn dead_placeholder_is_reclaimed_after_90s_not_300s() -> TestResult {
    let (mut s, _backend) = StoreSession::new_chunked().await?;
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    write_fixture_tree(&root, &dep_path());
    let path = out_path("src90");
    let fx = fixture_for_tree(&root, &path, vec![dep_path()]);

    // A dead attempt's leftovers: 'uploading', heartbeat silent for
    // 120 s — stale under the 90 s threshold, FRESH under the old
    // 300 s one (which would have wedged this test in
    // Aborted-concurrent).
    rio_store::test_helpers::StoreSeed::raw_path(path.as_str())
        .with_manifest_status("uploading")
        .seed(&s.db.pool)
        .await;
    sqlx::query(
        "UPDATE manifests SET updated_at = now() - interval '120 seconds', \
         claim_id = gen_random_uuid()",
    )
    .execute(&s.db.pool)
    .await?;

    let (begin, frames) = assemble_begin(&[&fx], vec![dep_path()], &Default::default());
    assert_eq!(
        send_chunked(&mut s.client, begin, frames, None).await?,
        vec![true],
        "the dead placeholder must be stale-reclaimed, not treated as live"
    );
    assert_eq!(get_path_bytes(&mut s.client, &path).await?, fx.nar);
    Ok(())
}
