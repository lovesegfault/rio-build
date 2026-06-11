//! Round-9 WO-S1-2 — ingestion-lane registration stamping (the signed
//! Q1 invariant's generality leg: *every byte-complete upload the
//! store accepted is registered evidence*).
//!
//! Pre-fix the PutPath lanes NEVER wrote `path_tenants` — 157,252 /
//! 168,317 paths (93.4% of the store) sat unstamped past grace, and an
//! uploaded-then-never-built-against path relied entirely on the 2h
//! grace window + signatures for visibility AND retention. The ingest
//! commit seam is the one place every upload passes: stamp the
//! AUTHENTICATED tenant there.

use super::*;
use tonic::transport::Server;

/// Spawn a store server with a fake interceptor that attaches
/// `jwt::TenantClaims { sub: tenant_id }` to every request — the same
/// shape the real `rio_auth` JWT interceptor produces on successful
/// verify (mirrors `signing.rs`'s harness; the handler reads
/// extensions, not headers).
async fn spawn_with_fake_jwt(
    service: StoreServiceImpl,
    tenant_id: uuid::Uuid,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let fake_interceptor = move |mut req: tonic::Request<()>| {
        req.extensions_mut().insert(rio_auth::jwt::TenantClaims {
            sub: tenant_id,
            iat: 1_700_000_000,
            exp: 9_999_999_999,
            jti: "test-session-registration".into(),
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

/// W9-D — the ingest stamp + the visibility it carries, end to end on
/// the production gRPC lanes: a tenant-authenticated UNSIGNED PutPath
/// (no signer wired — signature-based visibility cannot fire, so the
/// stamp is the only line) is, post-commit, (a) stamped with the
/// tenant's `path_tenants` row and (b) reported PRESENT (not missing)
/// by the tenant's own FindMissingPaths — the signature fallback
/// becomes defense-in-depth, not the only line.
///
/// Pre-fix red: (a) zero rows; (b) the path is pushed into
/// `missing_paths` by the visibility gate (present-but-invisible — the
/// I-217 own-tenant-Hidden channel for fresh uploads).
// r[verify store.registration.ingest-stamps]
#[tokio::test]
async fn putpath_stamps_tenant_and_fmp_sees_it() -> TestResult {
    use sha2::Digest;
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_id = rio_store::test_helpers::seed_tenant(&db.pool, "ingest-reg").await;

    // NO signer: visibility cannot ride signatures here.
    let service = StoreServiceImpl::new(db.pool.clone());
    let (mut client, _server) = spawn_with_fake_jwt(service, tenant_id).await?;

    let path = test_store_path("ingest-reg-out");
    let nar = make_nar(b"ingest registration content").0;
    let info = make_path_info_for_nar(&path, &nar);
    let created = put_path(&mut client, info, nar).await?;
    assert!(created, "fresh upload commits");

    // (a) the registration stamp exists for the authenticated tenant.
    let hash = sha2::Sha256::digest(path.as_bytes()).to_vec();
    let tenants: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        tenants,
        vec![tenant_id],
        "left: the PutPath ingest lane wrote no path_tenants row (the \
         93.4%-unstamped store) / right: the ingest commit seam stamps \
         the authenticated tenant"
    );

    // (b) the tenant's own FMP reports the path PRESENT without any
    // signature in play.
    let resp = client
        .find_missing_paths(FindMissingPathsRequest {
            store_paths: vec![path.clone()],
        })
        .await?
        .into_inner();
    assert!(
        !resp.missing_paths.contains(&path),
        "left: the fresh upload is reported MISSING to its own uploader \
         (present-but-invisible; the visibility gate pushes it into \
         missing_paths) / right: the stamp makes the tenant's own view \
         report it present — got missing_paths={:?}",
        resp.missing_paths
    );
    Ok(())
}

/// W9-D batch face: PutPathBatch stamps every CREATED output inside
/// the SAME atomic transaction that completes the manifests (the
/// ingest commit point — a torn stamp/commit pair is unrepresentable).
// r[verify store.registration.ingest-stamps]
#[tokio::test]
async fn putpathbatch_stamps_tenant_atomically() -> TestResult {
    use sha2::Digest;
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_id = rio_store::test_helpers::seed_tenant(&db.pool, "ingest-reg-batch").await;
    let service = StoreServiceImpl::new(db.pool.clone());
    let (mut client, _server) = spawn_with_fake_jwt(service, tenant_id).await?;

    let path_a = test_store_path("ingest-reg-batch-a");
    let path_b = test_store_path("ingest-reg-batch-b");
    let nar_a = make_nar(b"batch reg content a").0;
    let nar_b = make_nar(b"batch reg content b").0;

    // Stream construction mirrors chunked.rs's send_batch_output:
    // metadata → chunk → trailer per output_index, trailer-only mode.
    let (tx, rx) = mpsc::channel(16);
    for (idx, (path, nar)) in [(0u32, (&path_a, &nar_a)), (1u32, (&path_b, &nar_b))] {
        use rio_proto::types::PutPathBatchRequest;
        let mut info: PathInfo = make_path_info_for_nar(path, nar).into();
        let trailer = PutPathTrailer {
            nar_hash: std::mem::take(&mut info.nar_hash),
            nar_size: std::mem::take(&mut info.nar_size),
        };
        for msg in [
            put_path_request::Msg::Metadata(PutPathMetadata { info: Some(info) }),
            put_path_request::Msg::NarChunk(nar.clone()),
            put_path_request::Msg::Trailer(trailer),
        ] {
            tx.send(PutPathBatchRequest {
                output_index: idx,
                inner: Some(PutPathRequest { msg: Some(msg) }),
            })
            .await
            .expect("fresh channel");
        }
    }
    drop(tx);
    let created = client
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner()
        .created;
    assert_eq!(created, vec![true, true], "both outputs commit");

    for path in [&path_a, &path_b] {
        let hash = sha2::Sha256::digest(path.as_bytes()).to_vec();
        let tenants: Vec<uuid::Uuid> =
            sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_all(&db.pool)
                .await?;
        assert_eq!(
            tenants,
            vec![tenant_id],
            "batch output {path} carries the in-tx ingest stamp"
        );
    }
    Ok(())
}

/// The anonymous face: no tenant claims attached (dev mode / no JWT) —
/// no per-tenant ownership exists to stamp; the lane stays
/// stamp-free (visibility is unfiltered for anonymous callers).
// r[verify store.registration.ingest-stamps]
#[tokio::test]
async fn anonymous_putpath_writes_no_stamp() -> TestResult {
    use sha2::Digest;
    let mut s = StoreSession::new().await?;
    let path = test_store_path("ingest-anon");
    let nar = make_nar(b"anonymous content").0;
    let info = make_path_info_for_nar(&path, &nar);
    put_path(&mut s.client, info, nar).await?;

    let hash = sha2::Sha256::digest(path.as_bytes()).to_vec();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM path_tenants WHERE store_path_hash = $1")
        .bind(&hash)
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(n, 0, "anonymous uploads register no tenant ownership");
    Ok(())
}

/// W9-F's ingest-lane identity member (round-9 WO-S1-3): a declared
/// deriver persists at the ingest commit — the (path ↔ deriver)
/// linkage exists for client-declared uploads without any scheduler
/// involvement; an undeclared deriver stays typed-absent (NULL/empty,
/// the scheduler's registration fill backstops it at build
/// registration).
#[tokio::test]
async fn ingest_persists_declared_deriver() -> TestResult {
    use sha2::Digest;
    let mut s = StoreSession::new().await?;
    let path = test_store_path("ingest-deriver");
    let drv_path = test_store_path("ingest-deriver.drv");
    let nar = make_nar(b"deriver content").0;
    let mut info: PathInfo = make_path_info_for_nar(&path, &nar).into();
    info.deriver = drv_path.clone();
    put_path_raw(&mut s.client, info, nar).await?;

    let hash = sha2::Sha256::digest(path.as_bytes()).to_vec();
    let deriver: Option<String> =
        sqlx::query_scalar("SELECT deriver FROM narinfo WHERE store_path_hash = $1")
            .bind(&hash)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        deriver,
        Some(drv_path),
        "the ingest lane persists the declared deriver (identity at upload)"
    );
    Ok(())
}
