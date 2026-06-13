//! DrvBlobService (ADR-024): canonical drv blobs with server-side
//! identity verification, tenant-scoped presence/reads, write-through
//! idempotent puts.

use super::*;

use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier};
use rio_nix::derivation::Derivation as NixDerivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use rio_proto::DrvBlobServiceServer;
use rio_proto::derivation_util::{canonical_encode, derivation_digest, to_proto};
use rio_proto::store::drv_blob_service_client::DrvBlobServiceClient;
use rio_proto::types::{DrvBlob, GetDrvBlobRequest, HasDrvsRequest, PutDrvBlobsRequest};
use rio_store::grpc::DrvBlobServiceImpl;
use rio_store::test_helpers::seed_tenant;
use tonic::transport::Server;

const KEY: &[u8] = b"drv-blob-test-hmac-key-32-bytes!";

/// Builder-shaped HMAC assignment token carrying `tenant`.
fn token_for(tenant: uuid::Uuid) -> String {
    HmacSigner::from_key(KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "drv-blob-test".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: vec![],
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: Some(tenant.to_string()),
        input_closure_digest: String::new(),
    })
}

fn authed<T>(tenant: uuid::Uuid, msg: T) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    r.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        token_for(tenant).parse().expect("token is ASCII"),
    );
    r
}

/// A real canonical drv blob: ATerm → rio-nix parse → canonical proto
/// encode, with the drv path Nix would mint for that content
/// (`make_text(name, sha256(aterm), refs)`). Same construction as
/// rio-proto's golden codec tests, so the blob passes the full
/// server-side cross-check.
fn canonical_drv(tag: &str) -> (Vec<u8>, [u8; 32], String) {
    let aterm = format!(
        concat!(
            r#"Derive([("out","/nix/store/6123456789abcdfg0123456789abcdfg-{t}","","")],"#,
            r#"[],[],"x86_64-linux","/bin/sh",["-c","echo {t}"],[("name","{t}")])"#
        ),
        t = tag
    );
    let drv = NixDerivation::parse(&aterm).expect("fixture parses");
    let p = to_proto(&drv);
    let body = canonical_encode(&p);
    let digest = derivation_digest(&p);
    let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
    let drv_path = StorePath::make_text(&format!("{tag}.drv"), &h, &[])
        .expect("make_text")
        .to_string();
    (body, digest, drv_path)
}

fn blob(body: Vec<u8>, digest: [u8; 32], drv_path: String) -> DrvBlob {
    DrvBlob {
        digest: digest.to_vec(),
        drv_path,
        body,
    }
}

/// Harness: ephemeral PG + DrvBlobService behind the HMAC verifier.
struct DrvSession {
    db: TestDb,
    client: DrvBlobServiceClient<Channel>,
    server: tokio::task::JoinHandle<()>,
}

impl DrvSession {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = DrvBlobServiceImpl::new(
            db.pool.clone(),
            Some(Arc::new(HmacVerifier::from_key(KEY.to_vec()))),
        );
        let router = Server::builder().add_service(DrvBlobServiceServer::new(service));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        Ok(Self {
            db,
            client: DrvBlobServiceClient::new(channel),
            server,
        })
    }

    async fn stored_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM drv_blobs")
            .fetch_one(&self.db.pool)
            .await
            .expect("count drv_blobs")
    }
}

impl Drop for DrvSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

// r[verify store.drv.blob-kind]
/// Put → Has → Get round-trip: served bytes are byte-identical to the
/// received bytes, the digest answers present, and a double-put is an
/// idempotent no-op (`created=false`, still one row, bytes unchanged).
#[tokio::test]
async fn drv_put_get_has_roundtrip_byte_stable_and_idempotent() -> TestResult {
    let mut s = DrvSession::new().await?;
    let tenant = seed_tenant(&s.db.pool, "drv-roundtrip").await;
    let (body, digest, drv_path) = canonical_drv("roundtrip-0.1");

    let resp = s
        .client
        .put_drv_blobs(authed(
            tenant,
            PutDrvBlobsRequest {
                blobs: vec![blob(body.clone(), digest, drv_path.clone())],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true], "first put binds visibility");

    // Bulk Has: present.
    let has = s
        .client
        .has_drvs(authed(
            tenant,
            HasDrvsRequest {
                digests: vec![digest.to_vec(), vec![0xEE; 32]],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(has.bitmap, vec![0b0000_0001], "present + absent digest");

    // Byte-stable read-back.
    let got = s
        .client
        .get_drv_blob(authed(
            tenant,
            GetDrvBlobRequest {
                digest: digest.to_vec(),
            },
        ))
        .await?
        .into_inner();
    assert_eq!(got.body, body, "served bytes == received bytes");
    assert_eq!(got.drv_path, drv_path);
    assert_eq!(got.digest, digest.to_vec());

    // Idempotent double-put.
    let resp = s
        .client
        .put_drv_blobs(authed(
            tenant,
            PutDrvBlobsRequest {
                blobs: vec![blob(body.clone(), digest, drv_path)],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![false], "re-put is an idempotency hit");
    assert_eq!(s.stored_count().await, 1, "still exactly one row");
    let stored: Vec<u8> = sqlx::query_scalar("SELECT body FROM drv_blobs WHERE digest = $1")
        .bind(digest.as_slice())
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(stored, body, "stored bytes unchanged by the re-put");
    Ok(())
}

// r[verify store.drv.verify-on-put]
/// Each `verify_drv_blob` failure mode is a distinct named reject at
/// the service boundary, and NOTHING is stored on a reject — including
/// the valid sibling in the same batch (whole-batch atomicity).
#[tokio::test]
async fn drv_put_rejects_each_verify_failure_storing_nothing() -> TestResult {
    let mut s = DrvSession::new().await?;
    let tenant = seed_tenant(&s.db.pool, "drv-reject").await;
    let (body, digest, drv_path) = canonical_drv("reject-0.1");

    // 1. Digest mismatch: claimed digest is not blake3(body).
    let err = s
        .client
        .put_drv_blobs(authed(
            tenant,
            PutDrvBlobsRequest {
                blobs: vec![blob(body.clone(), [0xAB; 32], drv_path.clone())],
            },
        ))
        .await
        .expect_err("digest mismatch must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("digest mismatch"),
        "named error: {}",
        err.message()
    );

    // 2. Non-canonical bytes: an unknown trailing field decodes fine
    //    but the canonical re-encode drops it → byte-compare fails.
    //    The claimed digest matches the mutated bytes, so this reaches
    //    the canonicality check, not the digest check.
    let mut padded = body.clone();
    padded.extend_from_slice(&[0x7a, 0x00]); // field 15, wire type 2, len 0
    let padded_digest = *blake3::hash(&padded).as_bytes();
    let err = s
        .client
        .put_drv_blobs(authed(
            tenant,
            PutDrvBlobsRequest {
                blobs: vec![blob(padded, padded_digest, drv_path.clone())],
            },
        ))
        .await
        .expect_err("non-canonical bytes must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("not the canonical encoding"),
        "named error: {}",
        err.message()
    );

    // 3. Drv-path mismatch: valid body + digest under ANOTHER drv's
    //    claimed path — the recompute disagrees.
    let (_, _, other_path) = canonical_drv("other-0.1");
    let err = s
        .client
        .put_drv_blobs(authed(
            tenant,
            PutDrvBlobsRequest {
                blobs: vec![blob(body.clone(), digest, other_path)],
            },
        ))
        .await
        .expect_err("drv path mismatch must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("drv path mismatch"),
        "named error: {}",
        err.message()
    );

    // 4. Whole-batch atomicity: a valid blob next to a bad one stores
    //    nothing.
    let err = s
        .client
        .put_drv_blobs(authed(
            tenant,
            PutDrvBlobsRequest {
                blobs: vec![
                    blob(body.clone(), digest, drv_path),
                    blob(body, [0xCD; 32], "/nix/store/whatever-x.drv".into()),
                ],
            },
        ))
        .await
        .expect_err("batch with one bad blob must be rejected whole");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    assert_eq!(
        s.stored_count().await,
        0,
        "no reject may leave bytes behind — non-canonical content is never stored"
    );
    Ok(())
}

// r[verify store.drv.blob-kind]
/// Cross-tenant isolation: tenant A's drv blob is invisible to tenant
/// B's `HasDrvs` and `GetDrvBlob`; B's own put then binds B without
/// disclosing A (created reports B's binding as new).
#[tokio::test]
async fn drv_cross_tenant_isolation() -> TestResult {
    let mut s = DrvSession::new().await?;
    let tenant_a = seed_tenant(&s.db.pool, "drv-tenant-a").await;
    let tenant_b = seed_tenant(&s.db.pool, "drv-tenant-b").await;
    let (body, digest, drv_path) = canonical_drv("isolated-0.1");

    let resp = s
        .client
        .put_drv_blobs(authed(
            tenant_a,
            PutDrvBlobsRequest {
                blobs: vec![blob(body.clone(), digest, drv_path.clone())],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true]);

    // B: absent bitmap, NotFound read.
    let has = s
        .client
        .has_drvs(authed(
            tenant_b,
            HasDrvsRequest {
                digests: vec![digest.to_vec()],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(
        has.bitmap,
        vec![0b0000_0000],
        "tenant A's drv blob must be invisible to tenant B's HasDrvs"
    );
    let err = s
        .client
        .get_drv_blob(authed(
            tenant_b,
            GetDrvBlobRequest {
                digest: digest.to_vec(),
            },
        ))
        .await
        .expect_err("tenant B must not read tenant A's drv blob");
    assert_eq!(err.code(), tonic::Code::NotFound);

    // B's own write-through put binds B's visibility idempotently and
    // reports created=true (B's binding IS new — no cross-tenant
    // disclosure either way).
    let resp = s
        .client
        .put_drv_blobs(authed(
            tenant_b,
            PutDrvBlobsRequest {
                blobs: vec![blob(body, digest, drv_path)],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true], "B's binding is new for B");
    let has = s
        .client
        .has_drvs(authed(
            tenant_b,
            HasDrvsRequest {
                digests: vec![digest.to_vec()],
            },
        ))
        .await?
        .into_inner();
    assert_eq!(has.bitmap, vec![0b0000_0001], "B sees it after its put");
    assert_eq!(s.stored_count().await, 1, "dedup at rest: one blob row");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// GetPath drv-blob fallback (`r[store.drv.getpath-fallback]`): a `.drv`
// that exists ONLY as a drv blob (ADR-024 native submission — no
// narinfo row) is served by GetPath as a flat regular-file NAR with a
// synthesized PathInfo, under the same tenant scoping as GetDrvBlob.
// ─────────────────────────────────────────────────────────────────────

use rio_proto::StoreServiceClient;
use rio_proto::StoreServiceServer;
use rio_proto::types::{GetPathRequest, get_path_response};
use rio_store::grpc::StoreServiceImpl;
use sha2::{Digest as _, Sha256};

/// Builder-shaped HMAC assignment token WITHOUT a tenant claim
/// (orphaned/recovered node — `AssignmentClaims.tenant` docs).
fn token_tenantless() -> String {
    HmacSigner::from_key(KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "drv-blob-test".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: vec![],
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: None,
        input_closure_digest: String::new(),
    })
}

/// Like [`canonical_drv`] but with one `inputSrcs` entry, so the
/// fallback's synthesized `references` have something to carry.
/// Returns `(body, digest, drv_path, aterm, src_path)`.
fn canonical_drv_with_src(tag: &str) -> (Vec<u8>, [u8; 32], String, String, String) {
    let src = format!("/nix/store/7123456789abcdfg0123456789abcdfg-{tag}-src");
    let aterm = format!(
        concat!(
            r#"Derive([("out","/nix/store/6123456789abcdfg0123456789abcdfg-{t}","","")],"#,
            r#"[],["{s}"],"x86_64-linux","/bin/sh",["-c","echo {t}"],[("name","{t}")])"#
        ),
        t = tag,
        s = src
    );
    let drv = NixDerivation::parse(&aterm).expect("fixture parses");
    let p = to_proto(&drv);
    let body = canonical_encode(&p);
    let digest = derivation_digest(&p);
    let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
    let refs = vec![StorePath::parse(&src).expect("src parses")];
    let drv_path = StorePath::make_text(&format!("{tag}.drv"), &h, &refs)
        .expect("make_text")
        .to_string();
    (body, digest, drv_path, aterm, src)
}

/// Harness with BOTH services on one server over one PG: drv blobs go
/// in through the real `PutDrvBlobs`, come back out through the real
/// `GetPath` — exactly the native-build wire shape (client uploads,
/// builder fetches).
struct FallbackSession {
    db: TestDb,
    store: StoreServiceClient<Channel>,
    drv: DrvBlobServiceClient<Channel>,
    server: tokio::task::JoinHandle<()>,
}

impl FallbackSession {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let verifier = Arc::new(HmacVerifier::from_key(KEY.to_vec()));
        let store_svc =
            StoreServiceImpl::new(db.pool.clone()).with_hmac_verifier(Arc::clone(&verifier));
        let drv_svc = DrvBlobServiceImpl::new(db.pool.clone(), Some(verifier));
        let router = Server::builder()
            .add_service(StoreServiceServer::new(store_svc))
            .add_service(DrvBlobServiceServer::new(drv_svc));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        Ok(Self {
            db,
            store: StoreServiceClient::new(channel.clone()),
            drv: DrvBlobServiceClient::new(channel),
            server,
        })
    }

    /// Seed one drv blob through the real put path as `tenant`.
    async fn put_blob(
        &mut self,
        tenant: uuid::Uuid,
        body: Vec<u8>,
        digest: [u8; 32],
        drv_path: String,
    ) -> anyhow::Result<()> {
        let resp = self
            .drv
            .put_drv_blobs(authed(
                tenant,
                PutDrvBlobsRequest {
                    blobs: vec![blob(body, digest, drv_path)],
                },
            ))
            .await?
            .into_inner();
        anyhow::ensure!(resp.created == vec![true], "seed put must bind visibility");
        Ok(())
    }

    /// GetPath with an optional raw `x-rio-assignment-token` header,
    /// collecting the full stream into (PathInfo, NAR bytes).
    async fn get_path(
        &mut self,
        store_path: &str,
        token: Option<&str>,
    ) -> Result<(rio_proto::types::PathInfo, Vec<u8>), tonic::Status> {
        let mut req = tonic::Request::new(GetPathRequest {
            store_path: store_path.to_string(),
        });
        if let Some(t) = token {
            req.metadata_mut().insert(
                rio_proto::ASSIGNMENT_TOKEN_HEADER,
                t.parse().expect("token is ASCII"),
            );
        }
        let mut stream = self.store.get_path(req).await?.into_inner();
        let mut info = None;
        let mut nar = Vec::new();
        while let Some(msg) = stream.message().await? {
            match msg.msg {
                Some(get_path_response::Msg::Info(i)) => info = Some(i),
                Some(get_path_response::Msg::NarChunk(c)) => nar.extend_from_slice(&c),
                None => {}
            }
        }
        Ok((
            info.ok_or_else(|| tonic::Status::internal("stream had no PathInfo"))?,
            nar,
        ))
    }
}

impl Drop for FallbackSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

// r[verify store.drv.getpath-fallback]
/// The full native round-trip: PutDrvBlobs → GetPath of the `.drv`
/// path → NAR unwraps to the byte-exact ATerm the codec reconstructs,
/// and the synthesized narinfo is sane (hash/size over the served NAR,
/// references = the drv's input anchor set) and deterministic across
/// fetches.
#[tokio::test]
async fn drv_getpath_fallback_serves_aterm_nar() -> TestResult {
    let mut s = FallbackSession::new().await?;
    let tenant = seed_tenant(&s.db.pool, "drv-fallback").await;
    let (body, digest, drv_path, aterm, src) = canonical_drv_with_src("fallback-0.1");
    s.put_blob(tenant, body, digest, drv_path.clone()).await?;

    let token = token_for(tenant);
    let (info, nar) = s.get_path(&drv_path, Some(&token)).await?;

    // NAR shape: flat regular file whose contents are the ATerm,
    // byte-identical to the from_proto→to_aterm reconstruction (the
    // fixture aterm IS that reconstruction — DRVPROTO round-trip).
    let unwrapped = rio_nix::nar::extract_single_file(&nar)?;
    assert_eq!(
        String::from_utf8(unwrapped)?,
        aterm,
        "served NAR must unwrap to the byte-exact ATerm"
    );

    // Synthesized narinfo: deterministic in the served bytes.
    assert_eq!(info.store_path, drv_path);
    assert_eq!(
        info.store_path_hash,
        Sha256::digest(drv_path.as_bytes()).to_vec()
    );
    assert_eq!(info.nar_size, nar.len() as u64);
    assert_eq!(info.nar_hash, Sha256::digest(&nar).to_vec());
    assert_eq!(
        info.references,
        vec![src],
        "references = inputDrvs ∪ inputSrcs"
    );
    assert!(info.signatures.is_empty());
    assert!(info.deriver.is_empty());

    // What the builder actually does with the stream: parse it.
    let parsed = rio_nix::derivation::Derivation::parse_from_nar(&nar)?;
    assert_eq!(parsed.platform(), "x86_64-linux");

    // Deterministic / cacheable: a second fetch is byte-identical.
    let (info2, nar2) = s.get_path(&drv_path, Some(&token)).await?;
    assert_eq!(nar2, nar);
    assert_eq!(info2, info);
    Ok(())
}

// r[verify store.drv.getpath-fallback]
/// Miss behavior is unchanged: an absent `.drv` (and an absent
/// non-`.drv` path, token or not) still answers NotFound.
#[tokio::test]
async fn drv_getpath_fallback_absent_still_notfound() -> TestResult {
    let mut s = FallbackSession::new().await?;
    let tenant = seed_tenant(&s.db.pool, "drv-fallback-absent").await;
    let token = token_for(tenant);

    let absent_drv = "/nix/store/8123456789abcdfg0123456789abcdfg-absent-0.1.drv";
    let err = s
        .get_path(absent_drv, Some(&token))
        .await
        .expect_err("absent drv blob must stay NotFound");
    assert_eq!(err.code(), tonic::Code::NotFound);

    let absent_out = "/nix/store/8123456789abcdfg0123456789abcdfg-absent-0.1";
    let err = s
        .get_path(absent_out, Some(&token))
        .await
        .expect_err("non-.drv miss is untouched by the fallback");
    assert_eq!(err.code(), tonic::Code::NotFound);
    Ok(())
}

// r[verify store.drv.getpath-fallback]
/// Tenant/auth matrix — the GetDrvBlob rule, applied to the GetPath
/// shape: foreign tenant, anonymous, and tenant-less tokens all get
/// the same NotFound (no exists-but-not-yours oracle); a token that
/// fails verification is an auth error, not a hide.
#[tokio::test]
async fn drv_getpath_fallback_tenant_matrix() -> TestResult {
    let mut s = FallbackSession::new().await?;
    let tenant_a = seed_tenant(&s.db.pool, "drv-fallback-a").await;
    let tenant_b = seed_tenant(&s.db.pool, "drv-fallback-b").await;
    let (body, digest, drv_path, _aterm, _src) = canonical_drv_with_src("matrix-0.1");
    s.put_blob(tenant_a, body, digest, drv_path.clone()).await?;

    // Owner reads it.
    let (info, _) = s.get_path(&drv_path, Some(&token_for(tenant_a))).await?;
    assert_eq!(info.store_path, drv_path);

    // Foreign tenant: NotFound (binding is A's).
    let err = s
        .get_path(&drv_path, Some(&token_for(tenant_b)))
        .await
        .expect_err("tenant B must not read tenant A's drv via GetPath");
    assert_eq!(err.code(), tonic::Code::NotFound);

    // Anonymous: NotFound — exactly the pre-fallback answer.
    let err = s
        .get_path(&drv_path, None)
        .await
        .expect_err("anonymous GetPath of a drv-blob-only path stays NotFound");
    assert_eq!(err.code(), tonic::Code::NotFound);

    // Tenant-less (orphan/recovered) token: verified identity but
    // nothing to scope by → NotFound, mirroring GetDrvBlob's
    // same-answer discipline.
    let err = s
        .get_path(&drv_path, Some(&token_tenantless()))
        .await
        .expect_err("tenant-less token cannot resolve a binding");
    assert_eq!(err.code(), tonic::Code::NotFound);

    // Garbage token: an auth reject, not a hide — the caller presented
    // an identity and it failed verification.
    let err = s
        .get_path(&drv_path, Some("not-a-real-token"))
        .await
        .expect_err("unverifiable token is Unauthenticated");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    Ok(())
}

/// Anonymous callers are rejected on every RPC — the tenant ladder is
/// fail-closed.
#[tokio::test]
async fn drv_rpcs_reject_anonymous() -> TestResult {
    let mut s = DrvSession::new().await?;
    let (body, digest, drv_path) = canonical_drv("anon-0.1");

    let err = s
        .client
        .put_drv_blobs(PutDrvBlobsRequest {
            blobs: vec![blob(body, digest, drv_path)],
        })
        .await
        .expect_err("anonymous put rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let err = s
        .client
        .has_drvs(HasDrvsRequest {
            digests: vec![digest.to_vec()],
        })
        .await
        .expect_err("anonymous has rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let err = s
        .client
        .get_drv_blob(GetDrvBlobRequest {
            digest: digest.to_vec(),
        })
        .await
        .expect_err("anonymous get rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert_eq!(s.stored_count().await, 0);
    Ok(())
}
