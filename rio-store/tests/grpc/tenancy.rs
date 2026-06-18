//! `path_tenants` junction writes from the legacy upload RPCs
//! (`PutPath`, `PutPathBatch`) — `r[store.put.tenant-junction]`.
//!
//! The production failure these tests guard: the gateway uploads every
//! `.drv` via `PutPath`, which historically never wrote the junction.
//! The castore read surface (`ReadBlob` etc.) inner-joins
//! `path_tenants` on the caller's tenant, so the uploading tenant's own
//! builder could not read the `.drv` back: `NotFound` → castore-FUSE
//! `EIO` → every non-substituted build died as a spurious
//! infrastructure failure.

use super::*;

use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier};
use rio_proto::DirectoryServiceServer;
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::ReadBlobRequest;
use rio_store::grpc::DirectoryServiceImpl;
use rio_store::test_helpers::{path_hash, seed_tenant};
use rio_test_support::fixtures::test_drv_path;
use tokio_stream::StreamExt;

const KEY: &[u8] = b"tenancy-test-hmac-key-32-bytes!!";

/// HMAC assignment token authorizing `outputs` for `tenant` — the
/// builder/worker identity shape (no JWT anywhere).
fn token_for(tenant: uuid::Uuid, outputs: Vec<String>) -> String {
    HmacSigner::from_key(KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "tenancy-test".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: outputs,
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: Some(tenant.to_string()),
        input_closure_digest: String::new(),
    })
}

/// `path_tenants.tenant_id` rows for one store path, sorted.
async fn junction_tenants(pool: &sqlx::PgPool, path: &str) -> Vec<uuid::Uuid> {
    let mut rows: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(path_hash(path))
            .fetch_all(pool)
            .await
            .expect("path_tenants query");
    rows.sort();
    rows
}

/// Attach an assignment token to a request the way the builder does.
fn with_token<T>(req: T, tok: &str) -> tonic::Request<T> {
    let mut r = tonic::Request::new(req);
    r.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        tok.parse().expect("token is ASCII"),
    );
    r
}

/// The bug repro: a tenant uploads a `.drv` via legacy `PutPath` (the
/// gateway leg of every build), then its builder opens that `.drv`
/// through the castore read surface (`ReadBlob`, what castore-FUSE
/// issues). Both services share one PG pool — the production split.
/// Without the commit-time junction row the read side answers
/// `NotFound` and the build dies with EIO.
// r[verify store.put.tenant-junction]
#[tokio::test]
async fn put_path_then_read_blob_same_tenant() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "tenancy-rb").await;

    // Upload side: StoreService with HMAC enforcement.
    let store_svc = StoreServiceImpl::new(db.pool.clone())
        .with_hmac_verifier(Arc::new(HmacVerifier::from_key(KEY.to_vec())));
    let (mut store_client, store_server) = spawn_store_server(store_svc).await?;
    let _store_guard = scopeguard::guard(store_server, |h| h.abort());

    // Read side: DirectoryService on the SAME pool, same HMAC key.
    let dir_svc = DirectoryServiceImpl::new(
        db.pool.clone(),
        Some(Arc::new(HmacVerifier::from_key(KEY.to_vec()))),
        None,
        None,
    );
    let router = Server::builder().add_service(DirectoryServiceServer::new(dir_svc));
    let (addr, dir_server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let _dir_guard = scopeguard::guard(dir_server, |h| h.abort());
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut dir_client = DirectoryServiceClient::new(channel);

    // A single-file NAR shaped like a .drv — what the gateway forwards
    // via wopAddToStoreNar → PutPath.
    let drv_body: &[u8] = b"Derive([(\"out\",\"/nix/store/fake\",\"\",\"\")],[],[],\"x86_64\")";
    let (nar, _) = make_nar(drv_body);
    let path = test_drv_path("tenancy-drv");
    let info = make_path_info_for_nar(&path, &nar);

    let created = put_path_with_token(
        &mut store_client,
        info,
        nar,
        &token_for(tenant_a, vec![path.clone()]),
    )
    .await?;
    assert!(created, "fresh upload commits");

    // The builder's castore-FUSE open(): ReadBlob by blake3(file body)
    // with the SAME tenant's token.
    let digest = blake3::hash(drv_body);
    let resp = dir_client
        .read_blob(with_token(
            ReadBlobRequest {
                file_digest: digest.as_bytes().to_vec(),
            },
            &token_for(tenant_a, vec![]),
        ))
        .await
        .context("ReadBlob must see the path its own tenant just uploaded via PutPath")?;
    let mut body = Vec::new();
    let mut stream = resp.into_inner();
    while let Some(frame) = stream.next().await {
        body.extend_from_slice(&frame?.data);
    }
    assert_eq!(body, drv_body, "ReadBlob streams back the .drv contents");
    Ok(())
}

/// Spawn a store server with a fake interceptor that attaches
/// `jwt::TenantClaims { sub: tenant_id }` to every request — exactly
/// how the production gateway's identity arrives at the store (P0259
/// interceptor verifies the JWT, the handler reads request
/// extensions). Mirrors `spawn_store_with_fake_jwt` in signing.rs.
async fn spawn_store_with_fake_jwt(
    service: StoreServiceImpl,
    tenant_id: uuid::Uuid,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let fake_interceptor = move |mut req: tonic::Request<()>| {
        req.extensions_mut().insert(rio_auth::jwt::TenantClaims {
            sub: tenant_id,
            iat: 1_700_000_000,
            exp: 9_999_999_999,
            jti: "tenancy-test-fake".into(),
        });
        Ok(req)
    };
    let router = Server::builder()
        .layer(tonic::service::InterceptorLayer::new(fake_interceptor))
        .add_service(StoreServiceServer::new(service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    Ok((StoreServiceClient::new(channel), server))
}

/// The gateway leg: caller identity is a JWT tenant (no HMAC token at
/// all — dev-mode/service-bypass store). The junction row must come
/// from `TenantClaims.sub`.
// r[verify store.put.tenant-junction]
#[tokio::test]
async fn put_path_jwt_tenant_writes_junction() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant = seed_tenant(&db.pool, "tenancy-jwt").await;

    let service = StoreServiceImpl::new(db.pool.clone());
    let (mut client, server) = spawn_store_with_fake_jwt(service, tenant).await?;
    let _guard = scopeguard::guard(server, |h| h.abort());

    let (nar, _) = make_nar(b"jwt tenant junction body");
    let path = test_drv_path("tenancy-jwt-drv");
    let info = make_path_info_for_nar(&path, &nar);
    assert!(put_path(&mut client, info, nar).await?);

    assert_eq!(
        junction_tenants(&db.pool, &path).await,
        vec![tenant],
        "PutPath with a gateway JWT must write the JWT tenant's junction row"
    );
    Ok(())
}

/// Content-addressed paths deduplicate across tenants: tenant B
/// re-uploading a path tenant A already committed gets an idempotent
/// skip (`created=false`), but B still needs castore read access and a
/// GC pin of its own — the skip arm must write B's junction row.
// r[verify store.put.tenant-junction]
#[tokio::test]
async fn put_path_idempotent_skip_writes_junction() -> TestResult {
    let mut s = StoreSession::new_with_hmac(KEY.to_vec()).await?;
    let tenant_a = seed_tenant(&s.db.pool, "tenancy-skip-a").await;
    let tenant_b = seed_tenant(&s.db.pool, "tenancy-skip-b").await;

    let (nar, _) = make_nar(b"idempotent skip junction body");
    let path = test_store_path("tenancy-skip");
    let info = make_path_info_for_nar(&path, &nar);

    // Tenant A: fresh commit → A's row.
    let created = put_path_with_token(
        &mut s.client,
        info.clone(),
        nar.clone(),
        &token_for(tenant_a, vec![path.clone()]),
    )
    .await?;
    assert!(created);
    assert_eq!(
        junction_tenants(&s.db.pool, &path).await,
        vec![tenant_a],
        "the commit transaction writes the uploader's junction row"
    );

    // Tenant B: same path → idempotent skip, but B's row must appear.
    let created = put_path_with_token(
        &mut s.client,
        info,
        nar,
        &token_for(tenant_b, vec![path.clone()]),
    )
    .await?;
    assert!(!created, "second upload is an idempotent skip");
    let mut expected = vec![tenant_a, tenant_b];
    expected.sort();
    assert_eq!(
        junction_tenants(&s.db.pool, &path).await,
        expected,
        "the idempotent-skip arm must write the second tenant's junction row"
    );
    Ok(())
}

/// Send one full output (metadata → chunk → trailer) on a batch
/// stream. Same shape as `send_batch_output` in chunked.rs, local so
/// the two test modules stay independent.
async fn send_batch_output(
    tx: &mpsc::Sender<rio_proto::types::PutPathBatchRequest>,
    output_index: u32,
    mut info: PathInfo,
    nar: Vec<u8>,
) {
    use rio_proto::types::PutPathBatchRequest;

    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };
    let frames = [
        put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
            declared_nar_size: 0,
        }),
        put_path_request::Msg::NarChunk(nar),
        put_path_request::Msg::Trailer(trailer),
    ];
    for msg in frames {
        tx.send(PutPathBatchRequest {
            output_index,
            inner: Some(PutPathRequest { msg: Some(msg) }),
        })
        .await
        .expect("fresh channel");
    }
}

/// `PutPathBatch` (the worker's multi-output upload): both fresh
/// commits and the `already_complete` skip arm must write the caller
/// tenant's junction rows for every output.
// r[verify store.put.tenant-junction]
#[tokio::test]
async fn put_path_batch_writes_junction_rows() -> TestResult {
    let s = StoreSession::new_with_hmac(KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let tenant_a = seed_tenant(&s.db.pool, "tenancy-batch-a").await;
    let tenant_b = seed_tenant(&s.db.pool, "tenancy-batch-b").await;

    let path0 = test_store_path("tenancy-batch0");
    let path1 = test_store_path("tenancy-batch1");
    let (nar0, _) = make_nar(b"batch output zero");
    let (nar1, _) = make_nar(b"batch output one");
    let info0 = make_path_info_for_nar(&path0, &nar0);
    let info1 = make_path_info_for_nar(&path1, &nar1);
    let outputs = vec![path0.clone(), path1.clone()];

    // Tenant A: both outputs fresh → both rows.
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, info0.clone().into(), nar0.clone()).await;
    send_batch_output(&tx, 1, info1.clone().into(), nar1.clone()).await;
    drop(tx);
    let resp = client
        .put_path_batch(with_token(
            ReceiverStream::new(rx),
            &token_for(tenant_a, outputs.clone()),
        ))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true, true]);
    assert_eq!(junction_tenants(&s.db.pool, &path0).await, vec![tenant_a]);
    assert_eq!(junction_tenants(&s.db.pool, &path1).await, vec![tenant_a]);

    // Tenant B: same batch → all idempotent skips, B's rows added.
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, info0.into(), nar0).await;
    send_batch_output(&tx, 1, info1.into(), nar1).await;
    drop(tx);
    let resp = client
        .put_path_batch(with_token(
            ReceiverStream::new(rx),
            &token_for(tenant_b, outputs),
        ))
        .await?
        .into_inner();
    assert_eq!(
        resp.created,
        vec![false, false],
        "second batch is all idempotent skips"
    );
    let mut expected = vec![tenant_a, tenant_b];
    expected.sort();
    for path in [&path0, &path1] {
        assert_eq!(
            junction_tenants(&s.db.pool, path).await,
            expected,
            "the already_complete arm must write the second tenant's junction rows"
        );
    }
    Ok(())
}

/// An assignment token can name a tenant that was deleted while the
/// upload was in flight (`path_tenants.tenant_id` → `tenants` FK). The
/// fully-verified upload must still commit — with no junction row —
/// instead of failing on the FK violation.
// r[verify store.put.tenant-junction]
#[tokio::test]
async fn put_path_deleted_tenant_skips_junction() -> TestResult {
    let mut s = StoreSession::new_with_hmac(KEY.to_vec()).await?;
    let tenant_a = seed_tenant(&s.db.pool, "tenancy-gone").await;

    let (nar, _) = make_nar(b"deleted tenant body");
    let path = test_store_path("tenancy-gone");
    let info = make_path_info_for_nar(&path, &nar);

    // Token minted before the tenant was deleted — the in-flight shape.
    let token = token_for(tenant_a, vec![path.clone()]);
    sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
        .bind(tenant_a)
        .execute(&s.db.pool)
        .await?;

    let created = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect("a tenant deleted mid-upload must not fail the upload");
    assert!(created, "the upload itself commits");
    assert_eq!(
        junction_tenants(&s.db.pool, &path).await,
        Vec::<uuid::Uuid>::new(),
        "no junction row for a deleted tenant"
    );
    Ok(())
}

/// The cross-tenant brick: tenant A uploads a `.drv`; tenant B's
/// validity check (`FindMissingPaths`, what `wopQueryValidPaths`
/// inverts) must NOT report it valid, because B cannot read it through
/// the castore surface. A `.drv` exemption here means B's nix client
/// skips the upload, B's builder gets `NotFound` → EIO from
/// castore-FUSE, and the build dies after `max_infra_retries` —
/// reproduced live on a 2-tenant cluster (`qa --only iso03`).
///
/// The healing half: B re-uploads (idempotent skip), the skip arm
/// writes B's junction row (`r[store.put.tenant-junction]`), and the
/// same path becomes both valid and readable for B.
// r[verify store.tenant.valid-paths-filter]
#[tokio::test]
async fn find_missing_paths_drv_tenant_scoped() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "tenancy-fmp-a").await;
    let tenant_b = seed_tenant(&db.pool, "tenancy-fmp-b").await;

    // One store per caller identity (the fake-JWT interceptor pins a
    // single tenant), both on the same pool — the production split
    // where one PG backs every gateway session.
    let (mut client_a, server_a) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_a).await?;
    let _guard_a = scopeguard::guard(server_a, |h| h.abort());
    let (mut client_b, server_b) =
        spawn_store_with_fake_jwt(StoreServiceImpl::new(db.pool.clone()), tenant_b).await?;
    let _guard_b = scopeguard::guard(server_b, |h| h.abort());

    // Castore read side, HMAC-authenticated like a real builder.
    let dir_svc = DirectoryServiceImpl::new(
        db.pool.clone(),
        Some(Arc::new(HmacVerifier::from_key(KEY.to_vec()))),
        None,
        None,
    );
    let router = Server::builder().add_service(DirectoryServiceServer::new(dir_svc));
    let (addr, dir_server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let _dir_guard = scopeguard::guard(dir_server, |h| h.abort());
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut dir_client = DirectoryServiceClient::new(channel);

    // Tenant A uploads the .drv (the gateway leg of A's build).
    let drv_body: &[u8] = b"Derive([(\"out\",\"/nix/store/fmp\",\"\",\"\")],[],[],\"x86_64\")";
    let (nar, _) = make_nar(drv_body);
    let path = test_drv_path("tenancy-fmp");
    let info = make_path_info_for_nar(&path, &nar);
    assert!(put_path(&mut client_a, info.clone(), nar.clone()).await?);
    assert_eq!(junction_tenants(&db.pool, &path).await, vec![tenant_a]);

    // A's own validity check: present.
    let missing_for_a = client_a
        .find_missing_paths(FindMissingPathsRequest {
            store_paths: vec![path.clone()],
        })
        .await?
        .into_inner()
        .missing_paths;
    assert!(
        missing_for_a.is_empty(),
        "the uploader's own validity check must report the .drv present"
    );

    // B's validity check: MUST be missing — B cannot read it, so
    // reporting it valid would brick B's build.
    let missing_for_b = client_b
        .find_missing_paths(FindMissingPathsRequest {
            store_paths: vec![path.clone()],
        })
        .await?
        .into_inner()
        .missing_paths;
    assert_eq!(
        missing_for_b,
        vec![path.clone()],
        ".drv built by another tenant must be reported missing (valid-but-unreadable is forbidden)"
    );

    // Single-path agreement: wopIsValidPath goes through QueryPathInfo.
    let qpi_b = client_b
        .query_path_info(QueryPathInfoRequest {
            store_path: path.clone(),
        })
        .await;
    assert_eq!(
        qpi_b
            .expect_err("B's QueryPathInfo must not see A's .drv")
            .code(),
        tonic::Code::NotFound,
        "single-path lookup must agree with the batch answer"
    );

    // B re-uploads → idempotent skip writes B's junction row → the
    // path is now valid AND readable for B (self-healing ownership).
    assert!(!put_path(&mut client_b, info, nar).await?);
    let mut expected = vec![tenant_a, tenant_b];
    expected.sort();
    assert_eq!(junction_tenants(&db.pool, &path).await, expected);

    let missing_for_b = client_b
        .find_missing_paths(FindMissingPathsRequest {
            store_paths: vec![path.clone()],
        })
        .await?
        .into_inner()
        .missing_paths;
    assert!(
        missing_for_b.is_empty(),
        "after B's idempotent-skip upload the .drv is valid for B"
    );

    let digest = blake3::hash(drv_body);
    let resp = dir_client
        .read_blob(with_token(
            ReadBlobRequest {
                file_digest: digest.as_bytes().to_vec(),
            },
            &token_for(tenant_b, vec![]),
        ))
        .await
        .context("B's castore ReadBlob must succeed once B owns a junction row")?;
    let mut body = Vec::new();
    let mut stream = resp.into_inner();
    while let Some(frame) = stream.next().await {
        body.extend_from_slice(&frame?.data);
    }
    assert_eq!(body, drv_body, "valid-for-B now implies readable-by-B");
    Ok(())
}
