//! ed25519 narinfo signing at PutPath time.

use super::*;

// ===========================================================================
// narinfo signing
// ===========================================================================

use rio_store::signing::{Signer, TenantSigner};

/// Known seed → known keypair. The test verifies against the derived pubkey.
const TEST_SIGNING_SEED: [u8; 32] = [0x77; 32];

/// Cluster key seed for the tenant-signing test. Distinct from
/// `TENANT_SIGNING_SEED` below — if these matched, the mutation
/// check (tenant-sig must NOT verify under cluster pubkey) would
/// pass vacuously.
const CLUSTER_SIGNING_SEED: [u8; 32] = [0xCC; 32];
/// Tenant key seed. Distinct from cluster — see above.
const TENANT_SIGNING_SEED: [u8; 32] = [0xDD; 32];

fn make_test_signer() -> Signer {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(TEST_SIGNING_SEED);
    Signer::parse(&format!("test.cache.org-1:{b64}")).expect("valid key string")
}

/// PutPath with a signer: signature lands in narinfo.signatures and is
/// cryptographically valid. This is the end-to-end signing test — proves
/// maybe_sign() runs, the fingerprint is correct, and the sig verifies.
#[tokio::test]
async fn test_putpath_signs_narinfo() -> TestResult {
    let mut s = StoreSession::new_with_signer(make_test_signer()).await?;

    let store_path = test_store_path("signed-path");
    let nar = make_nar(b"content for signing").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    // Keep nar_hash + nar_size for fingerprint reconstruction below.
    let expected_hash = info.nar_hash;
    let expected_size = info.nar_size;

    put_path(&mut s.client, info, nar).await?;

    // Fetch back via QueryPathInfo — the sig should be in signatures.
    let fetched = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();

    assert_eq!(fetched.signatures.len(), 1, "should have exactly one sig");
    let sig_str = &fetched.signatures[0];
    assert!(
        sig_str.starts_with("test.cache.org-1:"),
        "sig should be prefixed with key name: {sig_str}"
    );

    // Verify the signature cryptographically. This is what a Nix client
    // does: reconstruct the fingerprint from narinfo fields, verify
    // against the pubkey from trusted-public-keys.
    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};

    let pubkey = SigningKey::from_bytes(&TEST_SIGNING_SEED).verifying_key();

    // Reconstruct the fingerprint. No refs in this test (make_path_info
    // produces empty references).
    let fp = rio_nix::narinfo::fingerprint(&store_path, &expected_hash, expected_size, &[]);

    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;

    pubkey
        .verify(fp.as_bytes(), &Signature::from_bytes(&sig_arr))
        .context("signature must verify against reconstructed fingerprint")?;

    Ok(())
}

/// Without a signer: PutPath still works, signatures just empty.
/// The baseline test — signing is optional.
#[tokio::test]
async fn test_putpath_no_signer_no_sig() -> TestResult {
    // Regular StoreSession (no signer).
    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("unsigned-path");
    let nar = make_nar(b"unsigned content").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar).await?;

    let fetched = s
        .client
        .query_path_info(QueryPathInfoRequest { store_path })
        .await?
        .into_inner();
    assert!(
        fetched.signatures.is_empty(),
        "no signer → no signature added"
    );

    Ok(())
}

/// Signature includes references in the fingerprint. A path with refs
/// produces a DIFFERENT signature than the same path without refs —
/// proves the fingerprint actually covers references (not just path+hash).
#[tokio::test]
async fn test_putpath_sig_covers_references() -> TestResult {
    let mut s = StoreSession::new_with_signer(make_test_signer()).await?;

    // Upload a dependency first (so the ref is a valid path).
    let dep_path = test_store_path("sig-dep");
    let dep_nar = make_nar(b"dependency").0;
    let dep_info = make_path_info_for_nar(&dep_path, &dep_nar);
    put_path(&mut s.client, dep_info, dep_nar).await?;

    // Upload the main path WITH a reference.
    let main_path = test_store_path("sig-main");
    let main_nar = make_nar(b"main content").0;
    let mut main_info = make_path_info_for_nar(&main_path, &main_nar);
    main_info.references =
        vec![rio_nix::store_path::StorePath::parse(&dep_path).context("parse dep")?];
    let main_hash = main_info.nar_hash;
    let main_size = main_info.nar_size;
    put_path(&mut s.client, main_info, main_nar).await?;

    // Fetch and verify. The fingerprint MUST include dep_path in refs.
    let fetched = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: main_path.clone(),
        })
        .await?
        .into_inner();
    let sig_str = &fetched.signatures[0];

    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};
    let pubkey = SigningKey::from_bytes(&TEST_SIGNING_SEED).verifying_key();

    // Fingerprint WITH the ref. slice::from_ref avoids a clone (clippy
    // is right — fingerprint only borrows).
    let fp_with_ref = rio_nix::narinfo::fingerprint(
        &main_path,
        &main_hash,
        main_size,
        std::slice::from_ref(&dep_path),
    );
    // Fingerprint WITHOUT the ref (wrong).
    let fp_no_ref = rio_nix::narinfo::fingerprint(&main_path, &main_hash, main_size, &[]);

    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;
    let sig = Signature::from_bytes(&sig_arr);

    // Must verify against fingerprint WITH ref.
    pubkey
        .verify(fp_with_ref.as_bytes(), &sig)
        .context("should verify against fingerprint WITH ref")?;
    // Must NOT verify against fingerprint WITHOUT ref. If this passed,
    // references wouldn't be covered by the signature — an attacker
    // could serve a path claiming different dependencies.
    assert!(
        pubkey.verify(fp_no_ref.as_bytes(), &sig).is_err(),
        "signature must NOT verify against fingerprint without ref — refs must be covered"
    );

    Ok(())
}

// ===========================================================================
// Per-tenant signing — store.tenant.sign-key (store.md:188)
// ===========================================================================

/// Spawn a store server with a fake interceptor that ALWAYS attaches
/// `jwt::Claims { sub: tenant_id }` to every request's extensions.
///
/// The real P0259 interceptor (`rio_common::jwt_interceptor`) verifies a
/// signed JWT from the `x-rio-tenant-token` metadata header. Here we skip
/// all that and inject Claims directly — the handler reads `request
/// .extensions().get::<jwt::Claims>()`, it doesn't care HOW Claims got
/// there. This tests the put_path → maybe_sign chain, not the
/// interceptor's cryptography (that's covered by the interceptor's own
/// unit tests in rio-common).
async fn spawn_store_with_fake_jwt(
    service: StoreServiceImpl,
    tenant_id: uuid::Uuid,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let fake_interceptor = move |mut req: tonic::Request<()>| {
        // Attach Claims exactly as the real interceptor would on
        // successful verify. Only `sub` is read by put_path — iat/exp/jti
        // are for audit/expiry/revocation, all scheduler-side concerns.
        req.extensions_mut().insert(rio_common::jwt::TenantClaims {
            sub: tenant_id,
            iat: 1_700_000_000,
            exp: 9_999_999_999,
            jti: "test-session-fake".into(),
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

// r[verify store.tenant.sign-key]
/// End-to-end: PutPath with JWT Claims.sub=TENANT_ID, where TENANT_ID
/// has an active `tenant_keys` row, produces a signature verifiable by
/// the TENANT's pubkey and NOT by the cluster's.
///
/// This is the end-to-end proof that the full chain works:
///   P0259 interceptor attaches Claims → put_path extracts sub →
///   maybe_sign(Some(tid)) → resolve_once → tenant_keys lookup →
///   tenant-key signature lands in narinfo.signatures.
///
/// Mutation check: if maybe_sign were called with `None` instead of
/// the extracted `tenant_id` (i.e., the wiring broke), the signature
/// would verify under the CLUSTER key. The final `cluster_pk.verify
/// .is_err()` assert catches exactly that — a `None`-path cluster
/// signature passes the TYPE (it's a valid signature) but fails the
/// KEY check (wrong key signed it).
#[tokio::test]
async fn put_path_with_tenant_jwt_signs_with_tenant_key() -> TestResult {
    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};

    // Self-precondition: if the seeds match, the verify-NOT-cluster
    // assert below is vacuous. Runtime assert (not const assert) so a
    // refactor collapsing the consts fails the test loudly, not
    // silently at compile. Same "proves nothing" guard as
    // signing.rs:505 and rio-scheduler/src/grpc/tests.rs:1346.
    assert_ne!(
        CLUSTER_SIGNING_SEED, TENANT_SIGNING_SEED,
        "test precondition: cluster and tenant seeds must differ"
    );

    let db = TestDb::new(&MIGRATOR).await;

    // --- 1. Seed tenant + active key ----------------------------------
    let tenant_id = rio_test_support::seed_tenant(&db.pool, "tsign-e2e").await;
    sqlx::query("INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) VALUES ($1, $2, $3)")
        .bind(tenant_id)
        .bind("tenant-e2e-1")
        .bind(TENANT_SIGNING_SEED.as_slice())
        .execute(&db.pool)
        .await?;

    // --- 2. Build service with TenantSigner(cluster, pool) ------------
    let cluster_b64 = base64::engine::general_purpose::STANDARD.encode(CLUSTER_SIGNING_SEED);
    let cluster = Signer::parse(&format!("cluster-e2e-1:{cluster_b64}"))?;
    let ts = TenantSigner::new(cluster, db.pool.clone());
    let service = StoreServiceImpl::new(db.pool.clone()).with_signer(ts);

    // --- 3. PutPath with Claims{sub: tenant_id} attached --------------
    // The fake interceptor does the attach; put_path reads it from
    // extensions and passes to maybe_sign.
    let (mut client, server) = spawn_store_with_fake_jwt(service, tenant_id).await?;
    // Drop on scope exit aborts the server (same idiom as StoreSession,
    // just manual here because we inlined the spawn).
    let _guard = scopeguard::guard(server, |h| h.abort());

    let store_path = test_store_path("tenant-signed-path");
    let nar = make_nar(b"content signed by tenant key").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    let expected_hash = info.nar_hash;
    let expected_size = info.nar_size;

    put_path(&mut client, info, nar).await?;

    // --- 4. Query back, extract signature -----------------------------
    let fetched = client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();

    assert_eq!(fetched.signatures.len(), 1, "exactly one sig expected");
    let sig_str = &fetched.signatures[0];

    // Quick sanity on the key NAME before the crypto check. If this
    // fails, the key lookup took the wrong branch — easier to debug
    // than an ed25519 verify failure.
    assert!(
        sig_str.starts_with("tenant-e2e-1:"),
        "signature key name should be the tenant's (tenant-e2e-1:), got {sig_str}"
    );

    // --- 5+6. Crypto verify: tenant pubkey YES, cluster pubkey NO -----
    let tenant_pk = SigningKey::from_bytes(&TENANT_SIGNING_SEED).verifying_key();
    let cluster_pk = SigningKey::from_bytes(&CLUSTER_SIGNING_SEED).verifying_key();

    let fp = rio_nix::narinfo::fingerprint(&store_path, &expected_hash, expected_size, &[]);

    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;
    let sig = Signature::from_bytes(&sig_arr);

    tenant_pk
        .verify(fp.as_bytes(), &sig)
        .context("tenant-signed narinfo MUST verify under tenant pubkey")?;

    // THE mutation-resistance check. If maybe_sign passed None (wiring
    // bug), the signature would be cluster-key and THIS assert fails.
    assert!(
        cluster_pk.verify(fp.as_bytes(), &sig).is_err(),
        "tenant-signed narinfo MUST NOT verify under cluster pubkey — \
         independent trust chain per tenant"
    );

    Ok(())
}

// r[verify store.tenant.sign-key]
/// No JWT Claims in extensions (what happens when the interceptor isn't
/// wired, or is wired but no `x-rio-tenant-token` header was sent) →
/// tenant_id=None → cluster key. The complement of the test above —
/// together they prove the branch is LIVE: one path fires one key,
/// the other path fires the other.
///
/// Uses `StoreSession::new_with_signer` (no interceptor) so Claims are
/// never attached. Seeds a tenant_keys row anyway to prove we're not
/// accidentally hitting it — without a seeded row, an empty table would
/// ALSO give cluster-key and the test proves less.
#[tokio::test]
async fn put_path_without_jwt_claims_falls_back_to_cluster_key() -> TestResult {
    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};

    let cluster_b64 = base64::engine::general_purpose::STANDARD.encode(CLUSTER_SIGNING_SEED);
    let cluster = Signer::parse(&format!("cluster-e2e-1:{cluster_b64}"))?;
    // new_with_signer wraps in TenantSigner internally.
    let mut s = StoreSession::new_with_signer(cluster).await?;

    // Decoy: seed a tenant WITH a key. If put_path mistakenly queried
    // tenant_keys with some garbage UUID, it'd probably miss this
    // anyway — but a seeded row rules out "empty table gave us the
    // right answer for the wrong reason".
    let decoy_tid = rio_test_support::seed_tenant(&s.db.pool, "tsign-decoy").await;
    sqlx::query("INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) VALUES ($1, $2, $3)")
        .bind(decoy_tid)
        .bind("decoy-never-used-1")
        .bind(TENANT_SIGNING_SEED.as_slice())
        .execute(&s.db.pool)
        .await?;

    let store_path = test_store_path("cluster-fallback-path");
    let nar = make_nar(b"no jwt means cluster key").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    let expected_hash = info.nar_hash;
    let expected_size = info.nar_size;

    put_path(&mut s.client, info, nar).await?;

    let fetched = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();

    assert_eq!(fetched.signatures.len(), 1);
    let sig_str = &fetched.signatures[0];
    assert!(
        sig_str.starts_with("cluster-e2e-1:"),
        "no JWT → cluster key name in sig, got {sig_str}"
    );

    // Crypto: cluster pubkey YES, tenant pubkey NO.
    let cluster_pk = SigningKey::from_bytes(&CLUSTER_SIGNING_SEED).verifying_key();
    let tenant_pk = SigningKey::from_bytes(&TENANT_SIGNING_SEED).verifying_key();

    let fp = rio_nix::narinfo::fingerprint(&store_path, &expected_hash, expected_size, &[]);
    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;
    let sig = Signature::from_bytes(&sig_arr);

    cluster_pk
        .verify(fp.as_bytes(), &sig)
        .context("cluster-fallback sig MUST verify under cluster pubkey")?;
    assert!(
        tenant_pk.verify(fp.as_bytes(), &sig).is_err(),
        "cluster-fallback sig MUST NOT verify under tenant (decoy) pubkey"
    );

    Ok(())
}
