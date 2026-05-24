//! Functional-tier test: the REAL `upload_all_outputs` (fused walk +
//! `HasChunks` + `PutPathChunked` client-stream) against the REAL
//! `rio-store` server (`StoreServiceImpl` + ephemeral PostgreSQL +
//! `MemoryChunkBackend`).
//!
//! This is the highest-confidence pre-VM check for the builder half of
//! ADR-022 §6: the server independently reconstructs each output's NAR
//! from the Directory tree + chunk bodies and rejects on any
//! `nar_hash`/`refs`/framing/ordering disagreement, so a green run here
//! proves the fused walk's output is wire- and content-correct against
//! the production verifier — not against a mock that might share its
//! bugs.
//!
//! Mirrors `rio-store/tests/grpc/put_path_chunked.rs`'s `ChunkedSession`
//! on the server side and `rio-gateway/tests/functional/mod.rs`'s
//! `RioStack` for the dev-dependency pattern.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest as _;
use tonic::transport::{Channel, Server};

use rio_builder::upload::upload_all_outputs;
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::{GetPathRequest, get_path_response};
use rio_proto::{ChunkServiceServer, DirectoryServiceServer, StoreServiceClient};
use rio_store::MIGRATOR;
use rio_store::backend::{ChunkBackend, MemoryChunkBackend};
use rio_store::cas::ChunkCache;
use rio_store::grpc::{ChunkServiceImpl, DirectoryServiceImpl, StoreServiceImpl};
use rio_store::test_helpers::path_hash;
use rio_test_support::TestDb;

/// Real store + real PG + in-memory chunk backend, plus a scratch
/// overlay-upper directory for the builder's walk to read from.
struct Session {
    db: TestDb,
    store: StoreServiceClient<Channel>,
    chunk: ChunkServiceClient<Channel>,
    backend: Arc<MemoryChunkBackend>,
    server: tokio::task::JoinHandle<()>,
    /// Tempdir holding `nix/store/<basename>` output trees.
    _tmp: tempfile::TempDir,
    upper_store: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Session {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let store_service =
            StoreServiceImpl::new(db.pool.clone()).with_chunk_cache(Arc::clone(&cache));
        let chunk_service = ChunkServiceImpl::new(db.pool.clone(), Some(Arc::clone(&cache)));
        let directory_service = DirectoryServiceImpl::new(db.pool.clone(), None, Some(cache));

        let max = rio_common::grpc::max_message_size();
        let router = Server::builder()
            .add_service(
                rio_proto::StoreServiceServer::new(store_service)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .add_service(ChunkServiceServer::new(chunk_service))
            .add_service(DirectoryServiceServer::new(directory_service));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
        let ch = rio_proto::client::connect_channel(&addr.to_string()).await?;

        let tmp = tempfile::tempdir()?;
        let upper_store = tmp.path().join("nix/store");
        std::fs::create_dir_all(&upper_store)?;
        Ok(Self {
            db,
            store: StoreServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            chunk: ChunkServiceClient::new(ch),
            backend,
            server,
            _tmp: tmp,
            upper_store,
        })
    }

    /// Drive the real production upload path over everything in
    /// `upper_store`.
    async fn upload(
        &self,
        deriver: &str,
        ref_candidates: &[String],
        input_closure: &[String],
    ) -> Result<Vec<rio_proto::validated::ValidatedPathInfo>, rio_builder::upload::UploadError>
    {
        upload_all_outputs(
            &self.store,
            &self.chunk,
            &self.upper_store,
            "",
            deriver,
            ref_candidates,
            input_closure,
        )
        .await
    }

    /// `GetPath` the full NAR back from the store.
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

/// A multi-kind output tree: nested dirs, an executable, a symlink, two
/// byte-identical files, an empty file, and a multi-chunk blob.
fn complex_tree(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("share/doc")).unwrap();
    std::fs::write(root.join("bin/tool"), b"#!/bin/sh\necho hello\n").unwrap();
    std::fs::set_permissions(
        root.join("bin/tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(root.join("share/doc/README"), vec![0xA5u8; 4096]).unwrap();
    std::fs::write(root.join("share/doc/COPY"), vec![0xA5u8; 4096]).unwrap();
    std::fs::write(root.join("empty"), b"").unwrap();
    std::fs::write(
        root.join("blob"),
        rio_test_support::fixtures::pseudo_random_bytes(7, 600 * 1024),
    )
    .unwrap();
    std::os::unix::fs::symlink("bin/tool", root.join("default")).unwrap();
}

/// Two outputs in one derivation → one `PutPathChunked` stream → both
/// committed atomically, both `GetPath` round-trip byte-for-byte
/// against the `dump_path` oracle, the castore tables are populated,
/// and every chunk is durable.
// r[verify builder.upload.fused-walk]
// r[verify builder.upload.chunked-manifest]
// r[verify builder.upload.batch+2]
#[tokio::test]
async fn real_store_two_output_roundtrip() -> anyhow::Result<()> {
    let mut s = Session::new().await?;
    let b_a = rio_test_support::fixtures::test_store_basename("chunked-real-a");
    let b_b = format!(
        "{}-chunked-real-b",
        rio_test_support::fixtures::rand_store_hash()
    );
    complex_tree(&s.upper_store.join(&b_a));
    std::fs::create_dir_all(s.upper_store.join(&b_b))?;
    std::fs::write(s.upper_store.join(&b_b).join("data"), vec![0x5Au8; 10_000])?;
    let oracle_a = rio_nix::nar::dump_path(&s.upper_store.join(&b_a))?;
    let oracle_b = rio_nix::nar::dump_path(&s.upper_store.join(&b_b))?;

    let results = s.upload("", &[], &[]).await.expect("upload succeeds");
    assert_eq!(results.len(), 2);

    let path_a = format!("/nix/store/{b_a}");
    let path_b = format!("/nix/store/{b_b}");
    // Both NARs round-trip byte-for-byte through the store's own
    // reassembly (manifest_data.chunk_list → framing + content chunks).
    assert_eq!(s.get_nar(&path_a).await?, oracle_a, "output A round-trips");
    assert_eq!(s.get_nar(&path_b).await?, oracle_b, "output B round-trips");

    // narinfo carries the server-verified hash (== the oracle's).
    let (nar_hash, nar_size): (Vec<u8>, i64) =
        sqlx::query_as("SELECT nar_hash, nar_size FROM narinfo WHERE store_path = $1")
            .bind(&path_a)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(nar_hash, sha2::Sha256::digest(&oracle_a).to_vec());
    assert_eq!(nar_size as u64, oracle_a.len() as u64);

    // Castore tables populated at commit (servable via GetDirectory /
    // ReadBlob without a separate indexer pass).
    let n_dp: i64 =
        sqlx::query_scalar("SELECT count(*) FROM directory_paths WHERE store_path_hash = $1")
            .bind(path_hash(&path_a))
            .fetch_one(&s.db.pool)
            .await?;
    assert!(n_dp >= 3, "directory_paths links every reachable digest");
    let n_blobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM file_blobs WHERE store_path_hash = $1")
            .bind(path_hash(&path_a))
            .fetch_one(&s.db.pool)
            .await?;
    // tool + (README == COPY) + empty + blob = 4 distinct file digests.
    assert_eq!(n_blobs, 4, "distinct file digests in file_blobs");

    // Every committed chunk is durable; the backend holds the objects.
    let not_durable: i64 =
        sqlx::query_scalar("SELECT count(*) FROM chunks WHERE NOT durable OR deleted")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(not_durable, 0, "every committed chunk is durable");
    assert!(!s.backend.is_empty(), "chunk bodies reached the backend");
    Ok(())
}

/// Idempotent re-drive: a second `upload_all_outputs` over the same
/// outputs is skipped by the `FindMissingPaths` pre-check (zero new
/// gRPC streams, zero refcount changes) and still reports the store's
/// metadata.
#[tokio::test]
async fn real_store_redrive_is_idempotent() -> anyhow::Result<()> {
    let s = Session::new().await?;
    let b = rio_test_support::fixtures::test_store_basename("chunked-redrive");
    complex_tree(&s.upper_store.join(&b));

    let first = s.upload("", &[], &[]).await.expect("first upload");
    assert_eq!(first.len(), 1);
    let rc_before: Vec<(Vec<u8>, i32)> =
        sqlx::query_as("SELECT blake3_hash, refcount FROM chunks ORDER BY blake3_hash")
            .fetch_all(&s.db.pool)
            .await?;

    let second = s.upload("", &[], &[]).await.expect("re-drive");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].nar_hash, first[0].nar_hash);
    let rc_after: Vec<(Vec<u8>, i32)> =
        sqlx::query_as("SELECT blake3_hash, refcount FROM chunks ORDER BY blake3_hash")
            .fetch_all(&s.db.pool)
            .await?;
    assert_eq!(rc_before, rc_after, "re-drive does not touch refcounts");
    Ok(())
}

/// High-dedup: a second store path with identical content sends zero
/// novel chunks (every digest already durable from the first upload)
/// and still commits + round-trips.
#[tokio::test]
async fn real_store_identical_content_dedups_to_zero_novel() -> anyhow::Result<()> {
    let mut s = Session::new().await?;
    let payload = rio_test_support::fixtures::pseudo_random_bytes(11, 500 * 1024);

    let b_a = rio_test_support::fixtures::test_store_basename("dedup-a");
    std::fs::create_dir_all(s.upper_store.join(&b_a))?;
    std::fs::write(s.upper_store.join(&b_a).join("data"), &payload)?;
    s.upload("", &[], &[]).await.expect("first upload");
    let objects_after_first = s.backend.len();
    assert!(objects_after_first > 0);

    // Replace the upper with a second, identically-shaped output at a
    // different store path.
    std::fs::remove_dir_all(s.upper_store.join(&b_a))?;
    let b_b = format!("{}-dedup-b", rio_test_support::fixtures::rand_store_hash());
    std::fs::create_dir_all(s.upper_store.join(&b_b))?;
    std::fs::write(s.upper_store.join(&b_b).join("data"), &payload)?;
    s.upload("", &[], &[]).await.expect("second upload");

    // The content chunks are shared; only the second output's FRAMING
    // chunks (server-generated, different store path ⇒ same framing
    // bytes actually — the NAR framing of an identical tree is
    // identical) are new. So the backend object count must not grow by
    // the content chunk count again.
    let oracle = rio_nix::nar::dump_path(&s.upper_store.join(&b_b))?;
    assert_eq!(
        s.get_nar(&format!("/nix/store/{b_b}")).await?,
        oracle,
        "dedup output round-trips"
    );
    assert_eq!(
        s.backend.len(),
        objects_after_first,
        "identical content + identical framing ⇒ zero new backend objects"
    );
    Ok(())
}

/// References found by the fused walk are declared in `Begin`, the
/// server's independent rescan agrees, and `narinfo.references`
/// carries them after commit.
// r[verify builder.upload.references-scanned+2]
#[tokio::test]
async fn real_store_references_scanned_and_verified() -> anyhow::Result<()> {
    let s = Session::new().await?;
    let dep = format!(
        "/nix/store/{}-chunked-dep",
        rio_test_support::fixtures::rand_store_hash()
    );
    let b = rio_test_support::fixtures::test_store_basename("chunked-refs");
    let path = format!("/nix/store/{b}");
    std::fs::create_dir_all(s.upper_store.join(&b))?;
    std::fs::write(
        s.upper_store.join(&b).join("conf"),
        format!("prefix={dep}\n"),
    )?;

    // Candidate set = input closure ∪ output paths; Begin.input_closure
    // must contain the dep for the server's refs ⊆ closure check.
    let results = s
        .upload("", &[dep.clone(), path.clone()], std::slice::from_ref(&dep))
        .await
        .expect("upload succeeds");
    assert_eq!(
        results[0]
            .references
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>(),
        vec![dep.clone()]
    );
    let refs: Vec<String> =
        sqlx::query_scalar("SELECT unnest(\"references\") FROM narinfo WHERE store_path = $1")
            .bind(&path)
            .fetch_one(&s.db.pool)
            .await
            .map(|r: String| vec![r])
            .unwrap_or_default();
    assert_eq!(refs, vec![dep]);
    assert_eq!(s.manifest_status(&path).await.as_deref(), Some("complete"));
    Ok(())
}

/// An inline-only store (no chunk backend) rejects `PutPathChunked`;
/// the builder falls back to the legacy `PutPath` path and the build
/// still succeeds end-to-end against the real server.
#[tokio::test]
async fn real_store_inline_only_falls_back_to_legacy() -> anyhow::Result<()> {
    // A store WITHOUT a chunk cache: PutPathChunked → FailedPrecondition.
    let db = TestDb::new(&MIGRATOR).await;
    let service = StoreServiceImpl::new(db.pool.clone());
    let max = rio_common::grpc::max_message_size();
    let router = Server::builder()
        .add_service(
            rio_proto::StoreServiceServer::new(service)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        )
        .add_service(ChunkServiceServer::new(ChunkServiceImpl::new(
            db.pool.clone(),
            None,
        )));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let ch = rio_proto::client::connect_channel(&addr.to_string()).await?;
    let store = StoreServiceClient::new(ch.clone())
        .max_decoding_message_size(max)
        .max_encoding_message_size(max);
    let chunk = ChunkServiceClient::new(ch);

    let tmp = tempfile::tempdir()?;
    let upper = tmp.path().join("nix/store");
    std::fs::create_dir_all(&upper)?;
    let b = rio_test_support::fixtures::test_store_basename("inline-fallback");
    std::fs::create_dir_all(upper.join(&b))?;
    std::fs::write(upper.join(&b).join("data"), b"small inline output")?;

    let results = upload_all_outputs(&store, &chunk, &upper, "", "", &[], &[])
        .await
        .expect("legacy fallback succeeds");
    assert_eq!(results.len(), 1);
    let status: Option<String> = sqlx::query_scalar(
        "SELECT m.status::text FROM manifests m JOIN narinfo n USING (store_path_hash) \
         WHERE n.store_path = $1",
    )
    .bind(format!("/nix/store/{b}"))
    .fetch_optional(&db.pool)
    .await?;
    assert_eq!(status.as_deref(), Some("complete"));
    server.abort();
    Ok(())
}
