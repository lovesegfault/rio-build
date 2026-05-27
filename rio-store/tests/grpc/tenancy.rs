//! Gateway-push tenant attribution (`path_tenants`) and the castore
//! visibility it unlocks.
//!
//! The legacy buffered RPCs (`PutPath` / `PutPathBatch`) are the
//! gateway/admin upload path: the gateway authenticates with a service
//! token and forwards the per-session tenant JWT, whose `Claims.sub`
//! the store already uses for narinfo signing. These tests prove the
//! same tenant is attributed to the pushed path in `path_tenants` at
//! commit time — without that row a tenant's own pushed sources are
//! invisible to the tenant-scoped castore RPCs
//! (`r[store.castore.tenant-scope]`) and every build that mounts them
//! fails with "store returned no Directory body for digest".

use super::*;

use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier, TokenRole};
use rio_nix::nar::{NarEntry, NarNode};
use rio_proto::DirectoryServiceServer;
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::{GetDirectoryRequest, StatBlobRequest, get_directory_request};
use rio_store::cas::ChunkCache;
use rio_store::grpc::DirectoryServiceImpl;
use rio_store::test_helpers::{path_hash, seed_tenant};
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;

/// HMAC key for the DirectoryService spawned by these tests (the
/// builder-side castore surface authenticates with an assignment token
/// whose `tenant` claim names the build's tenant).
const DIR_KEY: &[u8] = b"tenancy-test-key-32-bytes-aaaaaa";

/// Assignment token whose `tenant` claim drives the castore tenant scope.
fn assignment_token(tenant: uuid::Uuid) -> String {
    HmacSigner::from_key(DIR_KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "tenancy-test".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: vec![],
        is_ca: false,
        expiry_unix: 9_999_999_999,
        tenant: Some(tenant.to_string()),
        role: TokenRole::Builder,
        input_closure_digest: String::new(),
    })
}

fn with_assignment_token<T>(req: T, tok: &str) -> tonic::Request<T> {
    let mut r = tonic::Request::new(req);
    r.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        tok.parse().expect("token is ASCII"),
    );
    r
}

/// Spawn a `DirectoryService` (HMAC tenant resolution) on `pool`.
async fn spawn_directory_service(
    pool: sqlx::PgPool,
) -> (DirectoryServiceClient<Channel>, tokio::task::JoinHandle<()>) {
    let cache = Arc::new(ChunkCache::new(mem_backend() as Arc<dyn ChunkBackend>));
    let svc = DirectoryServiceImpl::new(
        pool,
        Some(Arc::new(HmacVerifier::from_key(DIR_KEY.to_vec()))),
        cache,
    );
    let router = Server::builder().add_service(DirectoryServiceServer::new(svc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("connect to in-process directory service");
    (DirectoryServiceClient::new(channel), server)
}

/// Build a directory NAR — `dir { a (regular payload), b (symlink) }` —
/// so the eager indexer derives one `directories` body and one
/// `file_blobs` row. Returns `(nar_bytes, sha256, store_path)`.
fn make_source_nar(name: &str, payload: &[u8]) -> (Vec<u8>, [u8; 32], String) {
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
        ],
    };
    let mut buf = Vec::new();
    rio_nix::nar::serialize(&mut buf, &node).expect("serialize NAR");
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    (buf, digest, test_store_path(name))
}

/// Count of `path_tenants` rows for one (path, tenant) pair.
async fn attribution_count(pool: &sqlx::PgPool, path: &str, tenant: uuid::Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM path_tenants WHERE store_path_hash = $1 AND tenant_id = $2",
    )
    .bind(path_hash(path))
    .bind(tenant)
    .fetch_one(pool)
    .await
    .expect("path_tenants count query")
}

/// Gateway-style push (session JWT names the tenant) → the pushed path
/// is attributed to that tenant in `path_tenants`, and the pushing
/// tenant can read the path's Directory body and file blob back through
/// the tenant-scoped castore RPCs; a different tenant still gets
/// NotFound for the same digests (isolation preserved).
// r[verify store.put.tenant-attribution+2]
// r[verify store.castore.tenant-scope]
#[tokio::test]
async fn put_path_with_tenant_attributes_pushing_tenant() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "push-a").await;
    let tenant_b = seed_tenant(&db.pool, "push-b").await;

    // Store server with the gateway-forwarded session JWT for tenant A.
    let (mut store, store_server) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _store_guard = scopeguard::guard(store_server, |h| h.abort());

    let payload = b"client-pushed source file";
    let (nar, nar_hash, path) = make_source_nar("pushed-source", payload);
    let info = make_path_info(&path, &nar, nar_hash);
    assert!(put_path(&mut store, info, nar).await?, "fresh push creates");

    // Attribution row for the pushing tenant, written by the commit.
    assert_eq!(
        attribution_count(&db.pool, &path, tenant_a).await,
        1,
        "pushed path must be attributed to the pushing tenant"
    );
    assert_eq!(
        attribution_count(&db.pool, &path, tenant_b).await,
        0,
        "no attribution for a tenant that did not push"
    );

    // The eager nar_index pass (spawned by PutPath) populates the
    // castore junction tables; poll for its commit (same budget as the
    // other spawned-task assertions in this suite).
    let dirs = poll_scalar_until(&db.pool, "SELECT count(*) FROM directory_paths", 1i64).await;
    assert_eq!(dirs, 1, "eager index must populate directory_paths");
    let root_digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM directory_paths LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    let file_digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM file_blobs LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        file_digest,
        blake3::hash(payload).as_bytes().to_vec(),
        "file_blobs row is the pushed file's BLAKE3 digest"
    );

    // Castore read surface: the pushing tenant resolves the digests…
    let (mut dir_client, dir_server) = spawn_directory_service(db.pool.clone()).await;
    let _dir_guard = scopeguard::guard(dir_server, |h| h.abort());
    let tok_a = assignment_token(tenant_a);
    let tok_b = assignment_token(tenant_b);

    let resp = dir_client
        .get_directory(with_assignment_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root_digest.clone())),
                recursive: false,
                digests: vec![],
            },
            &tok_a,
        ))
        .await?;
    let bodies: Vec<_> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(bodies.len(), 1, "pushing tenant reads the Directory body");
    assert!(
        bodies[0].files.iter().any(|f| f.name.as_slice() == b"a"),
        "returned body lists the pushed file"
    );
    dir_client
        .stat_blob(with_assignment_token(
            StatBlobRequest {
                file_digest: file_digest.clone(),
                send_chunks: false,
            },
            &tok_a,
        ))
        .await
        .expect("pushing tenant can StatBlob the pushed file");

    // …and a different tenant cannot (NotFound, not a leak).
    let err = dir_client
        .get_directory(with_assignment_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root_digest)),
                recursive: false,
                digests: vec![],
            },
            &tok_b,
        ))
        .await
        .expect_err("other tenant must not see the pushed Directory");
    assert_eq!(err.code(), tonic::Code::NotFound);
    let err = dir_client
        .stat_blob(with_assignment_token(
            StatBlobRequest {
                file_digest,
                send_chunks: false,
            },
            &tok_b,
        ))
        .await
        .expect_err("other tenant must not see the pushed blob");
    assert_eq!(err.code(), tonic::Code::NotFound);

    Ok(())
}

/// Tenant-less push (dev mode / service-token-only with no forwarded
/// JWT): no attribution row is written — the store never invents a
/// default tenant. Behavior unchanged from before upload-time
/// attribution existed.
// r[verify store.put.tenant-attribution+2]
#[tokio::test]
async fn put_path_without_tenant_writes_no_attribution() -> TestResult {
    let mut s = StoreSession::new().await?;
    // Decoy tenant: proves "zero rows" is a decision, not an empty
    // tenants table.
    let _decoy = seed_tenant(&s.db.pool, "push-decoy").await;

    let (nar, nar_hash, path) = make_source_nar("anon-source", b"anonymous push");
    let info = make_path_info(&path, &nar, nar_hash);
    assert!(put_path(&mut s.client, info, nar).await?, "push creates");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM path_tenants")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(rows, 0, "tenant-less push must not write path_tenants");
    Ok(())
}

/// `PutPathBatch` with a session JWT: every committed output is
/// attributed to the pushing tenant in the same transaction as the
/// batch commit.
// r[verify store.put.tenant-attribution+2]
#[tokio::test]
async fn put_path_batch_with_tenant_attributes_all_outputs() -> TestResult {
    use rio_proto::types::PutPathBatchRequest;

    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "batch-a").await;
    let (mut store, store_server) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _store_guard = scopeguard::guard(store_server, |h| h.abort());

    let outputs = [
        make_source_nar("batch-src-0", b"first source"),
        make_source_nar("batch-src-1", b"second source"),
    ];

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    for (i, (nar, nar_hash, path)) in outputs.iter().enumerate() {
        let mut info: PathInfo = make_path_info(path, nar, *nar_hash).into();
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
    let resp = store
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true, true]);

    for (_, _, path) in &outputs {
        assert_eq!(
            attribution_count(&db.pool, path, tenant_a).await,
            1,
            "batch output {path} must be attributed to the pushing tenant"
        );
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM path_tenants")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total, 2, "exactly one row per committed output");
    Ok(())
}

/// Content-verified re-upload — the amended dedup contract: a SECOND
/// tenant re-pushing the full, identical content of an already-complete
/// path is attributed to it (proof of possession earns the row), its
/// castore GetDirectory/StatBlob now succeed, and the success response
/// is indistinguishable from a fresh upload. The original pusher's
/// attribution and visibility are untouched, and once attributed the
/// re-pusher's next push takes the idempotent fast path again.
// r[verify store.put.tenant-attribution+2]
// r[verify store.put.idempotent+2]
// r[verify store.castore.tenant-scope]
#[tokio::test]
async fn repush_of_identical_content_grants_attribution() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "dedup-a").await;
    let tenant_b = seed_tenant(&db.pool, "dedup-b").await;

    // Tenant A pushes the path (store session forwarding A's JWT).
    let (mut store_a, server_a) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _guard_a = scopeguard::guard(server_a, |h| h.abort());
    let payload = b"shared source pushed twice";
    let (nar, nar_hash, path) = make_source_nar("dedup-source", payload);
    let info_a = make_path_info(&path, &nar, nar_hash);
    assert!(
        put_path(&mut store_a, info_a, nar.clone()).await?,
        "fresh push creates"
    );

    // Tenant B re-pushes the SAME path + content through its own session.
    let (mut store_b, server_b) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_b).await?;
    let _guard_b = scopeguard::guard(server_b, |h| h.abort());
    let info_b = make_path_info(&path, &nar, nar_hash);
    assert!(
        put_path(&mut store_b, info_b, nar.clone()).await?,
        "content-verified re-upload must be indistinguishable from a fresh upload (created = true)"
    );

    // Attribution: A keeps its row; B earned its own by proving possession.
    assert_eq!(attribution_count(&db.pool, &path, tenant_a).await, 1);
    assert_eq!(
        attribution_count(&db.pool, &path, tenant_b).await,
        1,
        "the content-verified re-upload must attribute the re-pushing tenant"
    );

    // Castore read surface: BOTH tenants resolve the root digest and the
    // file blob now (the rows were written by A's eager index; B's new
    // path_tenants row is what makes them reachable for B).
    let dirs = poll_scalar_until(&db.pool, "SELECT count(*) FROM directory_paths", 1i64).await;
    assert_eq!(dirs, 1, "eager index populated by the original push");
    let root_digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM directory_paths LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    let file_digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM file_blobs LIMIT 1")
        .fetch_one(&db.pool)
        .await?;

    let (mut dir_client, dir_server) = spawn_directory_service(db.pool.clone()).await;
    let _dir_guard = scopeguard::guard(dir_server, |h| h.abort());

    for (tid, who) in [(tenant_a, "original pusher"), (tenant_b, "re-pusher")] {
        let resp = dir_client
            .get_directory(with_assignment_token(
                GetDirectoryRequest {
                    by_what: Some(get_directory_request::ByWhat::Digest(root_digest.clone())),
                    recursive: false,
                    digests: vec![],
                },
                &assignment_token(tid),
            ))
            .await
            .unwrap_or_else(|e| panic!("{who} must resolve the Directory body: {e}"));
        let bodies: Vec<_> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
        assert_eq!(bodies.len(), 1, "{who} reads the Directory body");
        dir_client
            .stat_blob(with_assignment_token(
                StatBlobRequest {
                    file_digest: file_digest.clone(),
                    send_chunks: false,
                },
                &assignment_token(tid),
            ))
            .await
            .unwrap_or_else(|e| panic!("{who} must StatBlob the pushed file: {e}"));
    }

    // Once attributed, the re-pusher's next push short-circuits again.
    let info_b2 = make_path_info(&path, &nar, nar_hash);
    assert!(
        !put_path(&mut store_b, info_b2, nar).await?,
        "an already-attributed tenant's re-push keeps the idempotent fast path (created = false)"
    );

    Ok(())
}

/// A re-push that claims an existing path but streams DIFFERENT content
/// is rejected loudly and grants nothing: the trailer is self-consistent
/// (declared hash matches the streamed bytes), so only the comparison
/// against the STORED manifest can — and must — fail. The rejection
/// names neither the stored hash nor the stored size, and the original
/// pusher's attribution is untouched.
// r[verify store.put.tenant-attribution+2]
#[tokio::test]
async fn repush_with_wrong_content_rejected_and_grants_nothing() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "mismatch-a").await;
    let tenant_b = seed_tenant(&db.pool, "mismatch-b").await;

    let (mut store_a, server_a) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _guard_a = scopeguard::guard(server_a, |h| h.abort());
    let (nar, nar_hash, path) = make_source_nar("mismatch-source", b"the real content");
    assert!(
        put_path(
            &mut store_a,
            make_path_info(&path, &nar, nar_hash),
            nar.clone()
        )
        .await?,
        "fresh push creates"
    );

    // Same store path (same name -> same test path), different payload.
    let (wrong_nar, wrong_hash, same_path) = make_source_nar("mismatch-source", b"something else");
    assert_eq!(same_path, path, "fixture: same path, different content");
    assert_ne!(wrong_hash, nar_hash, "fixture: contents actually differ");

    let (mut store_b, server_b) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_b).await?;
    let _guard_b = scopeguard::guard(server_b, |h| h.abort());
    let err = put_path(
        &mut store_b,
        make_path_info(&path, &wrong_nar, wrong_hash),
        wrong_nar,
    )
    .await
    .expect_err("re-push with mismatching content must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    assert!(
        err.message()
            .contains("does not match the already-stored path"),
        "rejection should be loud and specific: {err:?}"
    );
    assert!(
        !err.message().contains(&hex::encode(nar_hash)),
        "rejection must not echo the stored hash: {err:?}"
    );

    assert_eq!(
        attribution_count(&db.pool, &path, tenant_b).await,
        0,
        "a mismatching re-push must not attribute"
    );
    assert_eq!(attribution_count(&db.pool, &path, tenant_a).await, 1);
    Ok(())
}

/// A metadata-only "probe" of an existing path (stream closed without
/// chunks or trailer) still grants nothing — the spirit of the original
/// negative contract survives: only proof of possession of the full,
/// matching content earns attribution.
// r[verify store.put.tenant-attribution+2]
#[tokio::test]
async fn metadata_only_probe_grants_no_attribution() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "probe-a").await;
    let tenant_b = seed_tenant(&db.pool, "probe-b").await;

    let (mut store_a, server_a) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _guard_a = scopeguard::guard(server_a, |h| h.abort());
    let (nar, nar_hash, path) = make_source_nar("probe-source", b"probed content");
    assert!(
        put_path(&mut store_a, make_path_info(&path, &nar, nar_hash), nar).await?,
        "fresh push creates"
    );

    // B sends ONLY the metadata message and closes the stream — the
    // shape of an existence probe that never proves possession.
    let (mut store_b, server_b) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_b).await?;
    let _guard_b = scopeguard::guard(server_b, |h| h.abort());
    let mut probe_info: PathInfo = make_path_info(&path, b"", [0u8; 32]).into();
    probe_info.nar_hash = Vec::new();
    probe_info.nar_size = 0;
    let (tx, rx) = mpsc::channel(4);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(probe_info),
        })),
    })
    .await
    .expect("fresh channel");
    drop(tx);
    let err = store_b
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("a probe that never streams the NAR must fail");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    assert_eq!(
        attribution_count(&db.pool, &path, tenant_b).await,
        0,
        "a metadata-only probe must not attribute"
    );
    Ok(())
}

/// `PutPathBatch` flavor of the content-verified re-upload: a batch from
/// tenant B that re-streams an output already pushed by tenant A gets
/// that output attributed to B inside the batch transaction (response
/// indistinguishable from a fresh upload), while a batch claiming the
/// path with different content is rejected whole and attributes nothing.
// r[verify store.put.tenant-attribution+2]
#[tokio::test]
async fn put_path_batch_repush_attributes_verified_output() -> TestResult {
    use rio_proto::types::PutPathBatchRequest;

    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "batch-repush-a").await;
    let tenant_b = seed_tenant(&db.pool, "batch-repush-b").await;

    // A pushes the source via plain PutPath.
    let (mut store_a, server_a) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _guard_a = scopeguard::guard(server_a, |h| h.abort());
    let (nar, nar_hash, path) = make_source_nar("batch-repush-src", b"batch shared source");
    assert!(
        put_path(
            &mut store_a,
            make_path_info(&path, &nar, nar_hash),
            nar.clone()
        )
        .await?,
        "fresh push creates"
    );

    // Helper: send a single-output PutPathBatch for (path, nar, hash).
    async fn send_batch(
        store: &mut StoreServiceClient<Channel>,
        path: &str,
        nar: &[u8],
        nar_hash: [u8; 32],
    ) -> Result<rio_proto::types::PutPathBatchResponse, tonic::Status> {
        let mut info: PathInfo = make_path_info(path, nar, nar_hash).into();
        info.nar_hash = Vec::new();
        let trailer = PutPathTrailer {
            nar_hash: nar_hash.to_vec(),
            nar_size: std::mem::take(&mut info.nar_size),
        };
        let (tx, rx) = mpsc::channel(8);
        for msg in [
            put_path_request::Msg::Metadata(PutPathMetadata { info: Some(info) }),
            put_path_request::Msg::NarChunk(nar.to_vec()),
            put_path_request::Msg::Trailer(trailer),
        ] {
            tx.send(PutPathBatchRequest {
                output_index: 0,
                inner: Some(PutPathRequest { msg: Some(msg) }),
            })
            .await
            .expect("fresh channel");
        }
        drop(tx);
        store
            .put_path_batch(ReceiverStream::new(rx))
            .await
            .map(|r| r.into_inner())
    }

    // B re-streams the identical content in a batch -> attributed.
    let (mut store_b, server_b) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_b).await?;
    let _guard_b = scopeguard::guard(server_b, |h| h.abort());
    let resp = send_batch(&mut store_b, &path, &nar, nar_hash).await?;
    assert_eq!(
        resp.created,
        vec![true],
        "verified batch re-upload must look like a fresh upload"
    );
    assert_eq!(
        attribution_count(&db.pool, &path, tenant_b).await,
        1,
        "batch content-verified re-upload must attribute the re-pushing tenant"
    );
    assert_eq!(attribution_count(&db.pool, &path, tenant_a).await, 1);

    // A third tenant claiming the path with different content: the whole
    // batch is rejected, nothing attributed.
    let tenant_c = seed_tenant(&db.pool, "batch-repush-c").await;
    let (mut store_c, server_c) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_c).await?;
    let _guard_c = scopeguard::guard(server_c, |h| h.abort());
    let (wrong_nar, wrong_hash, same_path) =
        make_source_nar("batch-repush-src", b"divergent content");
    assert_eq!(same_path, path);
    let err = send_batch(&mut store_c, &path, &wrong_nar, wrong_hash)
        .await
        .expect_err("mismatching batch re-push must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    assert_eq!(
        attribution_count(&db.pool, &path, tenant_c).await,
        0,
        "a rejected batch must not attribute"
    );
    Ok(())
}

/// One `PutPathBatch` mixing the three shapes a pushing tenant can hit
/// per output: already complete AND already attributed to it, already
/// complete but attributed only to another tenant, and absent. The
/// per-output outcomes must match the single-output semantics exactly —
/// idempotent fast path (`created=false`, no new row) for the
/// attributed one, content-verified re-upload (`created=true`, new
/// `path_tenants` row) for the unattributed one, fresh upload
/// (`created=true`, new row) for the absent one. This is the shape that
/// exercises the batched `path_tenants` probe (one query for the whole
/// already-complete set) replacing the per-output PK probe.
// r[verify store.put.tenant-attribution+2]
// r[verify store.put.idempotent+2]
#[tokio::test]
async fn put_path_batch_mixed_attribution_outcomes() -> TestResult {
    use rio_proto::types::PutPathBatchRequest;

    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "mixed-batch-a").await;
    let tenant_b = seed_tenant(&db.pool, "mixed-batch-b").await;

    // outputs[0]: complete + attributed to B (B re-pushes it below).
    // outputs[1]: complete, attributed to A only.
    // outputs[2]: absent until the batch.
    let outputs = [
        make_source_nar("mixed-attributed", b"already mine"),
        make_source_nar("mixed-unattributed", b"someone else's source"),
        make_source_nar("mixed-absent", b"brand new source"),
    ];

    let (mut store_a, server_a) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _guard_a = scopeguard::guard(server_a, |h| h.abort());
    let (mut store_b, server_b) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_b).await?;
    let _guard_b = scopeguard::guard(server_b, |h| h.abort());

    // A seeds the two already-complete outputs.
    for (nar, nar_hash, path) in &outputs[..2] {
        assert!(
            put_path(
                &mut store_a,
                make_path_info(path, nar, *nar_hash),
                nar.clone()
            )
            .await?,
            "seed push of {path} creates"
        );
    }
    // B's verified re-upload of outputs[0] earns its attribution row
    // BEFORE the batch, so the batch sees one attributed and one
    // unattributed already-complete output.
    let (nar0, hash0, path0) = &outputs[0];
    assert!(
        put_path(
            &mut store_b,
            make_path_info(path0, nar0, *hash0),
            nar0.clone()
        )
        .await?,
        "B's pre-batch re-upload attributes outputs[0]"
    );
    assert_eq!(attribution_count(&db.pool, path0, tenant_b).await, 1);
    assert_eq!(
        attribution_count(&db.pool, &outputs[1].2, tenant_b).await,
        0
    );

    // B sends ONE batch carrying all three outputs.
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    for (i, (nar, nar_hash, path)) in outputs.iter().enumerate() {
        let mut info: PathInfo = make_path_info(path, nar, *nar_hash).into();
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
    let resp = store_b
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(
        resp.created,
        vec![false, true, true],
        "attributed → idempotent fast path; unattributed → verified re-upload; absent → fresh"
    );

    // B holds exactly one row per output it can now read; A's rows are
    // untouched and A gains nothing from B's batch.
    for (_, _, path) in &outputs {
        assert_eq!(
            attribution_count(&db.pool, path, tenant_b).await,
            1,
            "B must hold exactly one path_tenants row for {path}"
        );
    }
    assert_eq!(attribution_count(&db.pool, path0, tenant_a).await, 1);
    assert_eq!(
        attribution_count(&db.pool, &outputs[1].2, tenant_a).await,
        1
    );
    assert_eq!(
        attribution_count(&db.pool, &outputs[2].2, tenant_a).await,
        0
    );
    Ok(())
}

/// Tenant-scoped source visibility on FindMissingPaths
/// (`require_tenant_attribution`): an unattributed tenant is told the
/// path is missing (so `nix copy` re-pushes it and the verified
/// re-upload can attribute it), the attributed tenant and global-truth
/// (anonymous / service) callers keep seeing it as present, and `.drv`
/// paths stay exempt.
// r[verify store.tenant.find-missing-attribution]
#[tokio::test]
async fn find_missing_paths_attribution_scope() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "fmp-a").await;
    let tenant_b = seed_tenant(&db.pool, "fmp-b").await;

    // A pushes a source (attributed to A only) and a .drv.
    let (mut store_a, server_a) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _guard_a = scopeguard::guard(server_a, |h| h.abort());
    let (nar, nar_hash, path) = make_source_nar("fmp-source", b"fmp payload");
    assert!(
        put_path(&mut store_a, make_path_info(&path, &nar, nar_hash), nar).await?,
        "source push creates"
    );
    let (drv_nar, drv_hash, drv_path) = make_source_nar("fmp-thing.drv", b"Derive([...])");
    assert!(
        drv_path.ends_with(".drv"),
        "fixture: {drv_path} must be a .drv path"
    );
    assert!(
        put_path(
            &mut store_a,
            make_path_info(&drv_path, &drv_nar, drv_hash),
            drv_nar
        )
        .await?,
        ".drv push creates"
    );

    let fmp = |paths: Vec<String>, flag: bool| FindMissingPathsRequest {
        store_paths: paths,
        require_tenant_attribution: flag,
    };

    // Attributed tenant + flag -> present.
    let resp = store_a
        .find_missing_paths(fmp(vec![path.clone()], true))
        .await?
        .into_inner();
    assert!(
        resp.missing_paths.is_empty(),
        "attributed tenant must see its source as present, got {:?}",
        resp.missing_paths
    );

    // Unattributed tenant + flag -> missing (the re-push trigger), but
    // the .drv stays exempt.
    let (mut store_b, server_b) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_b).await?;
    let _guard_b = scopeguard::guard(server_b, |h| h.abort());
    let resp = store_b
        .find_missing_paths(fmp(vec![path.clone(), drv_path.clone()], true))
        .await?
        .into_inner();
    assert_eq!(
        resp.missing_paths,
        vec![path.clone()],
        "unattributed tenant must see the source as missing and the .drv as present"
    );

    // Anonymous / service-identity caller (no tenant): global truth,
    // with or without the flag (no tenant identity -> flag is inert).
    let (mut store_anon, server_anon) =
        spawn_store_server(StoreServiceImpl::new(db.pool.clone())).await?;
    let _guard_anon = scopeguard::guard(server_anon, |h| h.abort());
    for flag in [false, true] {
        let resp = store_anon
            .find_missing_paths(fmp(vec![path.clone(), drv_path.clone()], flag))
            .await?
            .into_inner();
        assert!(
            resp.missing_paths.is_empty(),
            "tenant-less caller must keep global-truth presence (flag={flag}), got {:?}",
            resp.missing_paths
        );
    }
    Ok(())
}
