//! Walk ↔ store agreement tests (P0586).
//!
//! Drive the REAL rio-store `PutPathChunked` handler (ephemeral
//! PostgreSQL + in-memory chunk backend) with exactly what the
//! production client sends — `walk_output` → `build_begin` →
//! `send_chunked`, or the whole `upload_all_outputs` orchestration —
//! so the builder-side fused walk and the store-side
//! `validate_begin`/verify/commit pipeline are checked against each
//! other at the wire level. The MockStore-based unit tests in
//! `super::tests` cover client behavior; these cover the contract.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tonic::transport::Channel;

use rio_nix::nar;
use rio_nix::refscan::CandidateSet;
use rio_nix::store_path::StorePath;
use rio_proto::{ChunkServiceServer, StoreServiceClient, StoreServiceServer};
use rio_store::backend::ChunkBackend;
use rio_test_support::TestDb;

use crate::store_fetch::StoreClients;

use super::UploadError;
use super::chunked::{WalkedTarget, build_begin, chunked_stream_timeout, send_chunked};
use super::upload_all_outputs;
use super::walk::walk_output;

// Distinct, valid nixbase32 hash parts (alphabet excludes e/o/u/t).
const HASH_OUT: &str = "1111111111aaaaaaaaaabbbbbbbbbbcc";
const HASH_LIB: &str = "2222222222aaaaaaaaaabbbbbbbbbbcc";
const HASH_DEP: &str = "3333333333aaaaaaaaaabbbbbbbbbbcc";
const HASH_B: &str = "4444444444aaaaaaaaaabbbbbbbbbbcc";
const DERIVER: &str = "/nix/store/ffffffffffffffffffffffffffffffff-agreement.drv";

/// Real rio-store over an ephemeral PG with an in-memory chunk backend,
/// wrapped in the production `StoreClients` bundle. The `TestDb` must
/// stay alive for the duration of the test (its Drop deletes the DB).
///
/// Production auth posture (ADR-024 P2): the HMAC verifier is armed on
/// BOTH services and a tenant row is seeded. `HasChunks` presence is
/// tenant-scoped — answered from the `chunk_tenants` junction rows that
/// manifest completion writes for the uploader's resolved tenant — so a
/// verifier-less store would resolve tenant `None` at completion, write
/// no junction rows, and every later probe would miss (full re-upload,
/// no dedup). Uploads therefore authenticate exactly like production:
/// an assignment token carrying the tenant claim, minted per upload via
/// [`RealStore::token_for`] because the armed `StoreService` also
/// enforces the token's `expected_outputs` allowlist and the
/// deriver ↔ `drv_hash` binding. Read-backs (`GetPath`) stay anonymous:
/// callers without tenant context are unfiltered by design
/// (`r[store.tenant.narinfo-filter]`).
struct RealStore {
    db: TestDb,
    /// Seeded tenant UUID (string form) every token binds; the
    /// junction-row assertions query it directly.
    tenant: String,
    clients: StoreClients,
    store_client: StoreServiceClient<Channel>,
    backend: Arc<rio_store::backend::MemoryChunkBackend>,
    signer: rio_auth::hmac::HmacSigner,
    _server: tokio::task::JoinHandle<()>,
}

impl RealStore {
    /// Sign an assignment token the way the scheduler does at dispatch:
    /// bound to the fixture tenant, `expected_outputs` = exactly the
    /// paths the upload will claim (the armed store rejects any other),
    /// `drv_hash` = [`DERIVER`]'s hash part (the deriver ↔ token
    /// binding `validate_begin` enforces).
    fn token_for(&self, expected_outputs: &[&str]) -> String {
        self.signer.sign(&rio_auth::hmac::AssignmentClaims {
            executor_id: "agreement-tests".into(),
            drv_hash: StorePath::parse(DERIVER)
                .expect("DERIVER is a valid store path")
                .hash_part(),
            expected_outputs: expected_outputs.iter().map(|p| p.to_string()).collect(),
            is_ca: false,
            expiry_unix: u64::MAX,
            tenant: Some(self.tenant.clone()),
            input_closure_digest: String::new(),
        })
    }
}

async fn real_store() -> anyhow::Result<RealStore> {
    let db = TestDb::new(&rio_store::MIGRATOR).await;
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "agreement-tests")
        .await
        .to_string();
    let backend = rio_store::test_helpers::mem_backend();
    let cache = Arc::new(rio_store::cas::ChunkCache::new(
        Arc::clone(&backend) as Arc<dyn ChunkBackend>
    ));
    let hmac_key = b"agreement-tests-hmac-key".to_vec();
    let verifier = Arc::new(rio_auth::hmac::HmacVerifier::from_key(hmac_key.clone()));
    let service = rio_store::grpc::StoreServiceImpl::new(db.pool.clone())
        .with_chunk_cache(Arc::clone(&cache))
        .with_hmac_verifier(Arc::clone(&verifier));
    let chunk_service =
        rio_store::grpc::ChunkServiceImpl::new(db.pool.clone(), Some(cache), Some(verifier));
    let signer = rio_auth::hmac::HmacSigner::from_key(hmac_key);

    let max = rio_common::grpc::max_message_size();
    let router = tonic::transport::Server::builder()
        .add_service(
            StoreServiceServer::new(service)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        )
        .add_service(
            ChunkServiceServer::new(chunk_service)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        );
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let ch = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let clients = StoreClients::from_channel(ch);
    let store_client = clients.store.clone();
    Ok(RealStore {
        db,
        tenant,
        clients,
        store_client,
        backend,
        signer,
        _server: server,
    })
}

/// Fetch a path's NAR bytes back from the store via the shared client
/// helper (GetPath stream collection lives in `rio_proto::client`).
async fn get_path_bytes(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<Vec<u8>> {
    let (_info, nar) = rio_proto::client::get_path_nar(
        client,
        store_path,
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        rio_common::limits::MAX_NAR_SIZE,
        &[],
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("{store_path} not found in store"))?;
    Ok(nar)
}

/// A multi-node output tree exercising every castore node kind the walk
/// produces: nested directories, an executable, an empty file, an empty
/// directory, a symlink, a multi-chunk file, and embedded references.
fn write_rich_tree(root: &Path, embed: &[&str]) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir(root).unwrap();
    let mut data = String::from("references:\n");
    for p in embed {
        data.push_str(&format!("  {p}\n"));
    }
    fs::write(root.join("data"), data).unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    fs::create_dir(root.join("emptydir")).unwrap();
    fs::create_dir_all(root.join("share/doc")).unwrap();
    fs::write(root.join("share/doc/readme"), b"docs").unwrap();
    fs::create_dir(root.join("bin")).unwrap();
    fs::write(root.join("bin/app"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("bin/app"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("bin/app", root.join("run")).unwrap();
    // Multi-chunk regular file (> FASTCDC_MAX_BYTES).
    fs::write(
        root.join("blob"),
        rio_test_support::fixtures::pseudo_random_bytes(
            42,
            3 * rio_common::limits::FASTCDC_MAX_BYTES + 12_345,
        ),
    )
    .unwrap();
}

/// The full production upload path against the real store: every claim
/// the fused walk produces (nar_hash, references, Directory DAG,
/// chunk-run alignment, novel ordering) must satisfy `validate_begin`
/// and the verify task, the commit must reproduce each output's NAR
/// byte-for-byte via `GetPath`, and a re-dispatch must take the
/// idempotent pre-check path.
#[tokio::test]
async fn agreement_upload_all_outputs_against_real_store() -> anyhow::Result<()> {
    let s = real_store().await?;
    let tmp = tempfile::tempdir()?;
    let upper_store = tmp.path().join("nix/store");
    fs::create_dir_all(&upper_store)?;

    let out_path = format!("/nix/store/{HASH_OUT}-agreement-pkg");
    let lib_path = format!("/nix/store/{HASH_LIB}-agreement-pkg-lib");
    let dep_path = format!("/nix/store/{HASH_DEP}-agreement-dep");

    // Output `out` references the closure dep AND its sibling output;
    // output `lib` references nothing. Both outputs share the identical
    // bin/app + share/doc subtrees so cross-output Directory dedup and
    // shared chunks are exercised.
    write_rich_tree(
        &upper_store.join(format!("{HASH_OUT}-agreement-pkg")),
        &[dep_path.as_str(), lib_path.as_str()],
    );
    write_rich_tree(
        &upper_store.join(format!("{HASH_LIB}-agreement-pkg-lib")),
        &[],
    );

    let closure = vec![dep_path.clone()];
    let token = s.token_for(&[&out_path, &lib_path]);
    let results = upload_all_outputs(&s.clients, &upper_store, &token, DERIVER, &closure)
        .await
        .expect("real store accepts the production Begin + chunk stream");

    assert_eq!(results.len(), 2);
    assert!(
        !s.backend.is_empty(),
        "novel chunks reached the chunk backend"
    );

    // Byte-for-byte NAR round-trips through the store's framing
    // regenerator — the strongest walk↔castore agreement check.
    let mut client = s.store_client.clone();
    for basename in [
        format!("{HASH_OUT}-agreement-pkg"),
        format!("{HASH_LIB}-agreement-pkg-lib"),
    ] {
        let store_path = format!("/nix/store/{basename}");
        let local_nar = nar::dump_path(&upper_store.join(&basename))?;
        let remote_nar = get_path_bytes(&mut client, &store_path).await?;
        assert_eq!(
            remote_nar, local_nar,
            "{store_path}: GetPath must reproduce the original NAR byte-for-byte"
        );
        let r = results
            .iter()
            .find(|r| r.store_path.as_str() == store_path)
            .expect("result entry");
        assert_eq!(r.nar_hash, <[u8; 32]>::from(Sha256::digest(&local_nar)));
        assert_eq!(r.nar_size, local_nar.len() as u64);
    }
    // The scanned references are committed as claimed and recorded on
    // the narinfo.
    let out_refs: Vec<String> = results
        .iter()
        .find(|r| r.store_path.as_str() == out_path)
        .unwrap()
        .references
        .iter()
        .map(|p| p.to_string())
        .collect();
    // Sorted: the lib output's hash part (2222…) precedes the dep's (3333…).
    assert_eq!(out_refs, vec![lib_path.clone(), dep_path.clone()]);

    // Re-dispatch of the same derivation: the idempotent pre-check sees
    // both outputs present and skips the upload; the store's recorded
    // nar_hash matches what we computed locally.
    let chunks_before = s.backend.len();
    let again = upload_all_outputs(&s.clients, &upper_store, &token, DERIVER, &closure).await?;
    assert_eq!(again.len(), 2);
    assert_eq!(
        s.backend.len(),
        chunks_before,
        "second identical upload streams zero chunks"
    );
    for r in &again {
        let local_nar = nar::dump_path(
            &upper_store.join(r.store_path.as_str().trim_start_matches("/nix/store/")),
        )?;
        assert_eq!(r.nar_hash, <[u8; 32]>::from(Sha256::digest(&local_nar)));
    }
    Ok(())
}

/// Chunk-level dedup agreement: an upload whose `novel` (and chunk
/// stream) omits a chunk that is already durable from an earlier upload
/// must still commit — the store splices the deduped chunk from its own
/// CAS — and `GetPath` must reproduce the new output byte-for-byte.
/// This is the `build_begin` durable-set path the HasChunks probe drives
/// in production, exercised against the real verify task.
#[tokio::test]
async fn agreement_durable_chunk_omitted_from_stream() -> anyhow::Result<()> {
    let s = real_store().await?;
    let tmp = tempfile::tempdir()?;
    let upper_store = tmp.path().join("nix/store");
    fs::create_dir_all(&upper_store)?;

    let shared = b"this exact blob appears in both store paths".to_vec();

    // Path A: uploaded normally (everything novel).
    let basename_a = format!("{HASH_OUT}-dedup-a");
    let store_path_a = format!("/nix/store/{basename_a}");
    let root_a = upper_store.join(&basename_a);
    fs::create_dir(&root_a)?;
    fs::write(root_a.join("shared"), &shared)?;
    fs::write(root_a.join("only-a"), b"unique to a")?;
    let token_a = s.token_for(&[&store_path_a]);
    upload_all_outputs(&s.clients, &upper_store, &token_a, DERIVER, &[])
        .await
        .expect("first upload succeeds");
    let chunks_after_a = s.backend.len();

    // Path B shares the `shared` chunk with A. Build its Begin with the
    // production assembler, telling it the shared chunk is durable —
    // exactly what an authenticated HasChunks probe would report.
    let basename_b = format!("{HASH_B}-dedup-b");
    let store_path_b = format!("/nix/store/{basename_b}");
    let upper_b = tmp.path().join("second/nix/store");
    fs::create_dir_all(&upper_b)?;
    let root_b = upper_b.join(&basename_b);
    fs::create_dir(&root_b)?;
    fs::write(root_b.join("shared"), &shared)?;
    fs::write(root_b.join("only-b"), b"unique to b")?;

    let candidates = Arc::new(CandidateSet::from_paths([store_path_b.as_str()]));
    let walked = walk_output(&root_b, &candidates).await.expect("walk B");
    let target = WalkedTarget {
        basename: basename_b.clone(),
        store_path: store_path_b.clone(),
        parsed: StorePath::parse(&store_path_b).expect("valid store path"),
        walked,
    };
    let durable: HashSet<[u8; 32]> = [*blake3::hash(&shared).as_bytes()].into();
    let (begin, plan) = build_begin(&[target], DERIVER, &[], &durable);
    assert!(
        !begin
            .novel
            .iter()
            .any(|d| d.as_slice() == blake3::hash(&shared).as_bytes()),
        "B's novel must exclude the already-durable shared chunk"
    );

    let token_b = s.token_for(&[&store_path_b]);
    let (created, _bytes) = send_chunked(
        &s.store_client,
        begin,
        plan,
        &upper_b,
        &token_b,
        chunked_stream_timeout(1),
    )
    .await
    .expect("store accepts an upload that omits durable chunk bodies");
    assert_eq!(created, vec![true]);
    assert_eq!(
        s.backend.len(),
        chunks_after_a + 1,
        "B uploaded only its truly-novel chunk"
    );

    // The store reassembles B from its own copy of the shared chunk.
    let mut client = s.store_client.clone();
    let remote_nar = get_path_bytes(&mut client, &store_path_b).await?;
    assert_eq!(remote_nar, nar::dump_path(&root_b)?);
    Ok(())
}

/// Deployment-shape contract: a store WITHOUT a chunk backend (the
/// `ChunkBackendKind::Inline` default — "fine for dev/CI") cannot
/// accept builder uploads at all. The real handler rejects the
/// production `Begin` with `FAILED_PRECONDITION` and the builder
/// surfaces that status verbatim instead of looping on retries. This
/// is the off-VM reproduction of the VM-test failure mode where every
/// build-performing scenario went red because the deployed store had
/// no `[chunk_backend]` configured: any deployment that receives
/// builder uploads MUST configure one (ADR-022 §6).
///
/// Unlike [`real_store`], this fixture is deliberately verifier-less
/// (dev mode): the missing-chunk-backend `FAILED_PRECONDITION` must
/// surface for an unauthenticated dev-mode caller too, not only behind
/// the production HMAC gate.
#[tokio::test]
async fn agreement_inline_only_store_rejects_chunked_upload() -> anyhow::Result<()> {
    let db = TestDb::new(&rio_store::MIGRATOR).await;
    // No `.with_chunk_cache(..)`: the inline-only deployment shape.
    let service = rio_store::grpc::StoreServiceImpl::new(db.pool.clone());
    let chunk_service = rio_store::grpc::ChunkServiceImpl::new(db.pool.clone(), None, None);

    let max = rio_common::grpc::max_message_size();
    let router = tonic::transport::Server::builder()
        .add_service(
            StoreServiceServer::new(service)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        )
        .add_service(
            ChunkServiceServer::new(chunk_service)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        );
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let clients = StoreClients::from_channel(
        Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?,
    );

    let tmp = tempfile::tempdir()?;
    let upper_store = tmp.path().join("nix/store");
    fs::create_dir_all(&upper_store)?;
    fs::write(
        upper_store.join(format!("{HASH_OUT}-inline-only")),
        b"some output bytes",
    )?;

    let err = upload_all_outputs(&clients, &upper_store, "", DERIVER, &[])
        .await
        .expect_err("an inline-only store must reject the chunked upload");
    let UploadError::UploadRejected { source, .. } = err else {
        panic!("expected UploadRejected, got {err:?}");
    };
    assert_eq!(source.code(), tonic::Code::FailedPrecondition);
    assert!(
        source.message().contains("chunk backend"),
        "the store's configuration error must reach the builder verbatim: {source}"
    );
    Ok(())
}

/// End-to-end dedup through the REAL `HasChunks` probe: a chunk made
/// durable by an earlier upload of the SAME tenant is excluded from
/// `novel` on a later upload of a different path — observed via the
/// upload-bytes counter, which counts only the streamed novel chunk
/// bytes — and the deduped output still round-trips byte-for-byte via
/// `GetPath`. Unlike `agreement_durable_chunk_omitted_from_stream`,
/// nothing is hand-assembled here: the probe itself (authenticated
/// with a tenant-bound assignment token) discovers the durable chunk.
/// Presence is tenant-scoped (ADR-024 P2), so the test also asserts
/// the mechanism directly: the first upload's completion must have
/// written the `chunk_tenants` junction row the probe answers from.
#[tokio::test]
async fn agreement_real_probe_excludes_durable_chunks() -> anyhow::Result<()> {
    let s = real_store().await?;

    let shared = b"this exact blob is shared between the two uploads".to_vec();
    let unique = b"only in the second upload".to_vec();

    // First upload: path A carries the shared chunk → durable at commit.
    let tmp_a = tempfile::tempdir()?;
    let upper_a = tmp_a.path().join("nix/store");
    fs::create_dir_all(&upper_a)?;
    let basename_a = format!("{HASH_OUT}-probe-dedup-a");
    let store_path_a = format!("/nix/store/{basename_a}");
    let root_a = upper_a.join(&basename_a);
    fs::create_dir(&root_a)?;
    fs::write(root_a.join("shared"), &shared)?;
    let token_a = s.token_for(&[&store_path_a]);
    upload_all_outputs(&s.clients, &upper_a, &token_a, DERIVER, &[])
        .await
        .expect("first upload succeeds");
    let chunks_after_a = s.backend.len();

    // The mechanism under test, asserted at the source: manifest
    // completion bound the shared chunk to the uploader's tenant. This
    // junction row is what the tenant-scoped HasChunks probe answers
    // from — without it the probe would (correctly) answer absent and
    // the second upload would re-stream the shared chunk.
    let shared_digest = blake3::hash(&shared);
    let junction_bound: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
           SELECT 1 FROM chunk_tenants \
            WHERE blake3_hash = $1 AND tenant_id = $2::uuid)",
    )
    .bind(shared_digest.as_bytes().as_slice())
    .bind(&s.tenant)
    .fetch_one(&s.db.pool)
    .await?;
    assert!(
        junction_bound,
        "manifest completion must write the chunk_tenants junction row \
         for the uploader's tenant"
    );

    // Second upload: path B = the shared chunk plus one novel chunk.
    let tmp_b = tempfile::tempdir()?;
    let upper_b = tmp_b.path().join("nix/store");
    fs::create_dir_all(&upper_b)?;
    let basename_b = format!("{HASH_B}-probe-dedup-b");
    let store_path_b = format!("/nix/store/{basename_b}");
    let root_b = upper_b.join(&basename_b);
    fs::create_dir(&root_b)?;
    fs::write(root_b.join("shared"), &shared)?;
    fs::write(root_b.join("unique"), &unique)?;

    // Recorder scoped to the second upload only, so the bytes counter
    // reflects exactly what that upload streamed.
    let token_b = s.token_for(&[&store_path_b]);
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let result = {
        let _guard = metrics::set_default_local_recorder(&recorder);
        upload_all_outputs(&s.clients, &upper_b, &token_b, DERIVER, &[]).await
    };
    result.expect("second upload succeeds");

    assert_eq!(
        recorder.get("rio_builder_upload_bytes_total{}"),
        unique.len() as u64,
        "the real HasChunks probe excluded the durable shared chunk from novel; \
         only the truly-novel chunk's bytes went on the wire"
    );
    assert_eq!(
        s.backend.len(),
        chunks_after_a + 1,
        "the chunk backend gained exactly the one novel chunk"
    );

    // The store splices the deduped chunk from its own CAS on read-back.
    let mut client = s.store_client.clone();
    let remote_nar = get_path_bytes(&mut client, &store_path_b).await?;
    assert_eq!(remote_nar, nar::dump_path(&root_b)?);
    Ok(())
}
