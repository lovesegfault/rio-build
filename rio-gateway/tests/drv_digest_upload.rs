//! ADR-024 gateway half: `populate_digests_and_upload_drvs`
//! against a REAL `DrvBlobService` (PG-backed, JWT-verified).
//!
//! Proves the round-trip byte-stability contract end to end: the
//! bytes the gateway computes (canonical proto encode of the parsed
//! drv) are the bytes the store verifies, stores, and serves back
//! from `GetDrvBlob` — received bytes == stored bytes — and the
//! submission nodes come out digest-bearing with `input_drv_digests`
//! mirroring `inputDrvs`.

use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use tonic::transport::{Channel, Server};

use rio_gateway::translate;
use rio_nix::derivation::Derivation as NixDerivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use rio_proto::DrvBlobServiceServer;
use rio_proto::derivation_util::{canonical_encode, derivation_digest, to_proto};
use rio_proto::store::drv_blob_service_client::DrvBlobServiceClient;
use rio_proto::types::GetDrvBlobRequest;
use rio_store::grpc::DrvBlobServiceImpl;
use rio_store::test_helpers::seed_tenant;
use rio_test_support::TestDb;

pub use rio_migrations::MIGRATOR;

/// Leaf drv: ATerm → rio-nix parse, drv_path minted the way Nix would
/// (`make_text(name, sha256(aterm), refs)`), so the blob passes the
/// store's full server-side cross-check.
fn leaf_drv(tag: &str) -> (NixDerivation, String) {
    let aterm = format!(
        concat!(
            r#"Derive([("out","/nix/store/6123456789abcdfg0123456789abcdfg-{t}","","")],"#,
            r#"[],[],"x86_64-linux","/bin/sh",["-c","echo {t}"],[("name","{t}")])"#
        ),
        t = tag
    );
    let drv = NixDerivation::parse(&aterm).expect("leaf fixture parses");
    let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
    let drv_path = StorePath::make_text(&format!("{tag}.drv"), &h, &[])
        .expect("make_text")
        .to_string();
    (drv, drv_path)
}

/// Parent drv depending on `child_drv_path^out`. The child path is a
/// reference of the ATerm, so it feeds the parent's drv_path hash.
fn parent_drv(tag: &str, child_drv_path: &str) -> (NixDerivation, String) {
    let aterm = format!(
        concat!(
            r#"Derive([("out","/nix/store/7123456789abcdfg0123456789abcdfg-{t}","","")],"#,
            r#"[("{c}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo {t}"],[("name","{t}")])"#
        ),
        t = tag,
        c = child_drv_path
    );
    let drv = NixDerivation::parse(&aterm).expect("parent fixture parses");
    let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
    let refs = vec![StorePath::parse(child_drv_path).expect("child path")];
    let drv_path = StorePath::make_text(&format!("{tag}.drv"), &h, &refs)
        .expect("make_text")
        .to_string();
    (drv, drv_path)
}

struct Fixture {
    _db: TestDb,
    client: DrvBlobServiceClient<Channel>,
    _server: tokio::task::JoinHandle<()>,
    jwt: String,
    pool: sqlx::PgPool,
}

/// Real DrvBlobService behind the production JWT interceptor; the
/// returned `jwt` authenticates as a seeded tenant — the same header
/// the gateway session attaches.
async fn fixture() -> anyhow::Result<Fixture> {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant = seed_tenant(&db.pool, "drv-digest-gw").await;

    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let pubkey = Arc::new(std::sync::RwLock::new(signing.verifying_key()));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let jwt = rio_auth::jwt::sign(
        &rio_auth::jwt::TenantClaims {
            sub: tenant,
            iat: now,
            exp: now + 3600,
            jti: uuid::Uuid::new_v4().to_string(),
        },
        &signing,
    )?;

    let service = DrvBlobServiceImpl::new(db.pool.clone(), None);
    let router = Server::builder()
        .layer(tonic::service::InterceptorLayer::new(
            rio_auth::jwt_interceptor::jwt_interceptor(Some(pubkey)),
        ))
        .add_service(DrvBlobServiceServer::new(service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let client = DrvBlobServiceClient::connect(format!("http://{addr}")).await?;
    let pool = db.pool.clone();
    Ok(Fixture {
        _db: db,
        client,
        _server: server,
        jwt,
        pool,
    })
}

// r[verify gw.submit.digest-populate]
// r[verify store.drv.verify-on-put]
/// The full gateway-side flow: build nodes from parsed drvs, populate
/// digests, upload missing blobs, and verify byte-stability via
/// `GetDrvBlob` (served bytes == the gateway's canonical bytes).
#[tokio::test]
async fn ssh_ng_submission_becomes_digest_bearing_and_blobs_are_byte_stable() -> anyhow::Result<()>
{
    let mut f = fixture().await?;

    let (child, child_path) = leaf_drv("gw-child");
    let (parent, parent_path) = parent_drv("gw-parent", &child_path);

    let mut drv_cache: HashMap<StorePath, NixDerivation> = HashMap::new();
    drv_cache.insert(StorePath::parse(&child_path)?, child.clone());
    drv_cache.insert(StorePath::parse(&parent_path)?, parent.clone());

    let mut nodes = vec![
        translate::build_node(&child_path, &child),
        translate::build_node(&parent_path, &parent),
    ];

    translate::populate_digests_and_upload_drvs(
        &mut nodes,
        &drv_cache,
        Some(&mut f.client),
        Some(&f.jwt),
    )
    .await;

    // Digest fields populated: own digests match the canonical-proto
    // digest, parent's input references == [child digest].
    let child_digest = derivation_digest(&to_proto(&child));
    let parent_digest = derivation_digest(&to_proto(&parent));
    assert_eq!(nodes[0].drv_digest, child_digest.to_vec());
    assert_eq!(nodes[1].drv_digest, parent_digest.to_vec());
    assert!(nodes[0].input_drv_digests.is_empty(), "leaf has no inputs");
    assert_eq!(
        nodes[1].input_drv_digests,
        vec![child_digest.to_vec()],
        "parent's input_drv_digests must mirror its inputDrvs"
    );

    // Byte-stability: GetDrvBlob serves exactly the canonical bytes
    // the gateway computed and uploaded (received == stored).
    for (drv, path, digest) in [
        (&child, &child_path, child_digest),
        (&parent, &parent_path, parent_digest),
    ] {
        let mut req = tonic::Request::new(GetDrvBlobRequest {
            digest: digest.to_vec(),
        });
        req.metadata_mut().insert(
            rio_proto::TENANT_TOKEN_HEADER,
            f.jwt.parse().expect("jwt is ASCII"),
        );
        let served = f.client.get_drv_blob(req).await?.into_inner();
        let sent = canonical_encode(&to_proto(drv));
        assert_eq!(
            served.body, sent,
            "stored bytes must equal the gateway's uploaded bytes for {path}"
        );
        assert_eq!(&served.drv_path, path);
    }

    // Idempotent re-run: everything already present (HasDrvs all-set),
    // no re-upload, fields stay populated.
    let blobs_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM drv_blobs")
        .fetch_one(&f.pool)
        .await?;
    translate::populate_digests_and_upload_drvs(
        &mut nodes,
        &drv_cache,
        Some(&mut f.client),
        Some(&f.jwt),
    )
    .await;
    assert_eq!(nodes[1].drv_digest, parent_digest.to_vec());
    let blobs_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM drv_blobs")
        .fetch_one(&f.pool)
        .await?;
    assert_eq!(blobs_before, blobs_after, "re-run uploads nothing new");
    Ok(())
}

// r[verify gw.submit.digest-populate]
/// Degrade paths leave every node digest-less (no mixed submissions):
/// no JWT, no client, or a node whose drv is missing from the cache.
#[tokio::test]
async fn digest_population_degrades_to_legacy_atomically() -> anyhow::Result<()> {
    let mut f = fixture().await?;
    let (child, child_path) = leaf_drv("gw-deg-child");
    let (parent, parent_path) = parent_drv("gw-deg-parent", &child_path);
    let full_cache: HashMap<StorePath, NixDerivation> = HashMap::from([
        (StorePath::parse(&child_path)?, child.clone()),
        (StorePath::parse(&parent_path)?, parent.clone()),
    ]);
    let make_nodes = || {
        vec![
            translate::build_node(&child_path, &child),
            translate::build_node(&parent_path, &parent),
        ]
    };

    // No JWT → legacy.
    let mut nodes = make_nodes();
    translate::populate_digests_and_upload_drvs(&mut nodes, &full_cache, Some(&mut f.client), None)
        .await;
    assert!(nodes.iter().all(|n| n.drv_digest.is_empty()));

    // No DrvBlobService client → legacy.
    let mut nodes = make_nodes();
    translate::populate_digests_and_upload_drvs(&mut nodes, &full_cache, None, Some(&f.jwt)).await;
    assert!(nodes.iter().all(|n| n.drv_digest.is_empty()));

    // A node without a parsed drv (the BasicDerivation fallback
    // shape) → EVERY node stays digest-less, not just that one.
    let partial_cache: HashMap<StorePath, NixDerivation> =
        HashMap::from([(StorePath::parse(&child_path)?, child.clone())]);
    let mut nodes = make_nodes();
    translate::populate_digests_and_upload_drvs(
        &mut nodes,
        &partial_cache,
        Some(&mut f.client),
        Some(&f.jwt),
    )
    .await;
    assert!(
        nodes.iter().all(|n| n.drv_digest.is_empty()),
        "partial cache must degrade the WHOLE submission to legacy"
    );
    let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM drv_blobs")
        .fetch_one(&f.pool)
        .await?;
    assert_eq!(blobs, 0, "degraded runs upload nothing");
    Ok(())
}
