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
// r[verify store.put.tenant-attribution]
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
// r[verify store.put.tenant-attribution]
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
// r[verify store.put.tenant-attribution]
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

/// Dedup re-push — the spec'd negative contract: a SECOND tenant pushing
/// an already-complete path takes the idempotent fast path (`created =
/// false`), gains NO `path_tenants` row (it committed nothing and proved
/// possession of no content), and its castore reads still get NotFound.
/// Attribution stays exactly where the original upload put it.
// r[verify store.put.tenant-attribution]
// r[verify store.castore.tenant-scope]
#[tokio::test]
async fn repush_of_complete_path_grants_no_attribution() -> TestResult {
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
        !put_path(&mut store_b, info_b, nar).await?,
        "already-complete path must take the idempotent fast path (created = false)"
    );

    // Attribution: A keeps its row; B gained nothing from the re-push.
    assert_eq!(attribution_count(&db.pool, &path, tenant_a).await, 1);
    assert_eq!(
        attribution_count(&db.pool, &path, tenant_b).await,
        0,
        "the already-complete fast path must NOT attribute the path to the re-pusher"
    );

    // Castore read surface: B still NotFound on the path's root digest;
    // A (the original pusher) still resolves it.
    let dirs = poll_scalar_until(&db.pool, "SELECT count(*) FROM directory_paths", 1i64).await;
    assert_eq!(dirs, 1, "eager index populated by the original push");
    let root_digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM directory_paths LIMIT 1")
        .fetch_one(&db.pool)
        .await?;

    let (mut dir_client, dir_server) = spawn_directory_service(db.pool.clone()).await;
    let _dir_guard = scopeguard::guard(dir_server, |h| h.abort());

    let err = dir_client
        .get_directory(with_assignment_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root_digest.clone())),
                recursive: false,
                digests: vec![],
            },
            &assignment_token(tenant_b),
        ))
        .await
        .expect_err("the re-pushing tenant must still not see the Directory body");
    assert_eq!(err.code(), tonic::Code::NotFound);

    let resp = dir_client
        .get_directory(with_assignment_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root_digest)),
                recursive: false,
                digests: vec![],
            },
            &assignment_token(tenant_a),
        ))
        .await?;
    let bodies: Vec<_> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(
        bodies.len(),
        1,
        "the original pusher still resolves the body"
    );

    Ok(())
}
