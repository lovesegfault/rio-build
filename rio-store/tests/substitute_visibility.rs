//! Cross-tenant sig-visibility gate — gRPC-level integration test.
//!
//! The unit test at `grpc/mod.rs::sig_visibility_gate_cross_tenant`
//! exercises `sig_visibility_gate()` directly. This test goes through
//! the full gRPC `QueryPathInfo` handler with a fake JWT interceptor
//! attaching `TenantClaims` — same path a real gateway→store call
//! would take. Proves the gate is actually WIRED into the handler
//! (not just correct in isolation).
//!
//! VM-level coverage of the same rule lives at
//! `nix/tests/scenarios/substitute.nix::substitute-cross-tenant-gate`.

use std::sync::Arc;

use tonic::transport::{Channel, Server};

use rio_proto::types::{
    AddSignaturesRequest, AddUpstreamRequest, BatchGetManifestRequest, BatchQueryPathInfoRequest,
    FindMissingPathsRequest, GetPathRequest, QueryPathFromHashPartRequest, QueryPathInfoRequest,
};
use rio_proto::{
    StoreAdminServiceClient, StoreAdminServiceServer, StoreServiceClient, StoreServiceServer,
};
use rio_store::grpc::{StoreAdminServiceImpl, StoreServiceImpl};
use rio_store::signing::{Signer, TenantSigner};
use rio_store::substitute::Substituter;
use rio_store::test_helpers::seed_tenant;
use rio_test_support::fixtures::{make_nar, test_store_path};
use rio_test_support::{TestDb, TestResult};

use rio_store::MIGRATOR;

/// Fake interceptor that sets `TenantClaims.sub` per-request from a
/// shared `Arc<RwLock>`. The test flips the lock between tenant IDs
/// to simulate different clients hitting the same server — cheaper
/// than spawning N servers, one per tenant.
///
/// The real interceptor (`rio_auth::jwt_interceptor`) verifies a
/// signed JWT. Handlers only read `extensions().get::<TenantClaims>()`
/// and don't care how Claims got there — so injecting directly is
/// equivalent for testing the handler's behavior.
///
/// `std::sync::RwLock` (not tokio's): tonic interceptors are sync
/// `FnMut` called inside the executor's poll loop. `tokio::RwLock::
/// blocking_read` panics there. The std lock is fine — reads are
/// nanoseconds, no contention (single-threaded test).
#[derive(Clone)]
struct SwitchableTenant(Arc<std::sync::RwLock<Option<uuid::Uuid>>>);

impl SwitchableTenant {
    fn new() -> Self {
        Self(Arc::new(std::sync::RwLock::new(None)))
    }
    fn set(&self, tid: Option<uuid::Uuid>) {
        *self.0.write().unwrap() = tid;
    }
    fn interceptor(&self) -> impl tonic::service::Interceptor + Clone + use<> {
        let state = Arc::clone(&self.0);
        move |mut req: tonic::Request<()>| {
            if let Some(tid) = *state.read().unwrap() {
                req.extensions_mut().insert(rio_auth::jwt::TenantClaims {
                    sub: tid,
                    iat: 1_700_000_000,
                    exp: 9_999_999_999,
                    jti: "sub-vis-test".into(),
                });
            }
            Ok(req)
        }
    }
}

// r[verify store.substitute.tenant-sig-visibility+2]
/// End-to-end through `QueryPathInfo` gRPC:
///   1. Tenant A substitutes path P (signed by K1) from a fake upstream.
///   2. Tenant B (also trusts K1) queries P → `Ok(Some)`.
///   3. Tenant C (trusts only K2) queries P → `NotFound`.
///   4. Add K1 to C's trusted_keys → re-query → `Ok(Some)`.
///
/// The path is substitution-only (zero `path_tenants` rows) so the
/// gate applies. A built path (nonzero rows) would bypass the gate —
/// covered by the unit test's final assertion.
#[tokio::test]
async fn query_path_info_gated_by_tenant_sig_trust() -> TestResult {
    use base64::Engine;

    let db = TestDb::new(&MIGRATOR).await;

    // ── Seed three tenants ──────────────────────────────────────────────
    let tid_a = seed_tenant(&db.pool, "subvis-a").await;
    let tid_b = seed_tenant(&db.pool, "subvis-b").await;
    let tid_c = seed_tenant(&db.pool, "subvis-c").await;

    // ── Test keys K1/K2 ─────────────────────────────────────────────────
    // K1 signs the upstream path; K2 is a valid-but-unrelated pubkey
    // for tenant C (AddUpstream validates pubkey length/format, so
    // "key-K2:aaaa" won't do). Same pattern as substitute.rs::
    // spawn_fake_upstream: derive `name:base64pubkey` from a seed.
    let seed_k1 = [0x33u8; 32];
    let pk_k1 = ed25519_dalek::SigningKey::from_bytes(&seed_k1).verifying_key();
    let trusted_k1 = format!(
        "key-K1:{}",
        base64::engine::general_purpose::STANDARD.encode(pk_k1.as_bytes())
    );
    let seed_k2 = [0x44u8; 32];
    let pk_k2 = ed25519_dalek::SigningKey::from_bytes(&seed_k2).verifying_key();
    let trusted_k2 = format!(
        "key-K2:{}",
        base64::engine::general_purpose::STANDARD.encode(pk_k2.as_bytes())
    );

    // ── Spawn StoreService + StoreAdminService with substituter ────────
    // Gate only fires when substituter is wired (`.is_none()` early-
    // return in sig_visibility_gate). The substituter itself won't hit
    // HTTP — the path is pre-seeded, not miss-then-fetch. But it must
    // be PRESENT.
    let sub = Arc::new(Substituter::new(db.pool.clone(), None));
    let store_svc = StoreServiceImpl::new(db.pool.clone()).with_substituter(sub);
    let admin_svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

    let switch = SwitchableTenant::new();
    let router = Server::builder()
        .layer(tonic::service::InterceptorLayer::new(switch.interceptor()))
        .add_service(StoreServiceServer::new(store_svc))
        .add_service(StoreAdminServiceServer::new(admin_svc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = StoreServiceClient::new(channel.clone());
    let mut admin = StoreAdminServiceClient::new(channel);

    // ── Configure upstreams via StoreAdminService (same path rio-cli
    //    uses). A trusts K1; B trusts K1; C trusts ONLY K2. ────────────
    for (tid, keys) in [
        (tid_a, vec![trusted_k1.clone()]),
        (tid_b, vec![trusted_k1.clone()]),
        (tid_c, vec![trusted_k2.clone()]),
    ] {
        admin
            .add_upstream(AddUpstreamRequest {
                tenant_id: tid.to_string(),
                url: format!("https://cache-{tid}.example"),
                priority: 50,
                trusted_keys: keys,
                sig_mode: "keep".into(),
            })
            .await?;
    }

    // ── Seed path P signed by K1 (simulates "A substituted this") ──────
    // Direct SQL because `metadata` is pub(crate). The schema is stable
    // (migration-frozen per CLAUDE.md § Migration files). Status must be
    // 'complete' or query_path_info filters it out.
    let path = test_store_path("subvis-p");
    let (nar, nar_hash) = make_nar(b"subvis-payload");
    let fp = rio_nix::narinfo::fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
    let signer_k1 = Signer::from_seed("key-K1", &seed_k1);
    let sig_k1 = signer_k1.sign(&fp);

    let path_hash = sha256(&path);
    // narinfo first: manifests.store_path_hash → narinfo FK.
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, signatures) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&path_hash[..])
    .bind(&path)
    .bind(&nar_hash[..])
    .bind(nar.len() as i64)
    .bind(&[sig_k1][..])
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO manifests (store_path_hash, status, inline_blob) \
         VALUES ($1, 'complete', $2)",
    )
    .bind(&path_hash[..])
    .bind(&nar[..])
    .execute(&db.pool)
    .await?;

    // Precondition: zero path_tenants rows → substitution-only path →
    // gate applies. If this fails, the test setup leaked a built-path
    // row and every assertion below is VACUOUS (gate bypassed).
    let pt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM path_tenants WHERE store_path_hash = $1")
            .bind(&path_hash[..])
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        pt_count, 0,
        "precondition: zero path_tenants rows (substitution-only)"
    );

    let req = || QueryPathInfoRequest {
        store_path: path.clone(),
    };

    // ── B trusts K1 → visible ──────────────────────────────────────────
    switch.set(Some(tid_b));
    let resp = client.query_path_info(req()).await?.into_inner();
    assert_eq!(
        resp.store_path, path,
        "tenant B trusts K1 → substituted path visible"
    );

    // ── C trusts only K2 → NotFound on EVERY tenant-facing read RPC ────
    // r[verify store.api.hash-part+2]
    // r[verify store.api.batch-query+2]
    // r[verify store.api.batch-manifest+2]
    // r[verify store.substitute.find-missing-gated]
    // Pre-fix: only QueryPathInfo was gated; the other five leaked.
    switch.set(Some(tid_c));
    let err = client
        .query_path_info(req())
        .await
        .expect_err("tenant C doesn't trust K1 → NotFound");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "expected NotFound, got {err:?}"
    );

    // GetPath: same gate as QueryPathInfo.
    let err = client
        .get_path(GetPathRequest {
            store_path: path.clone(),
            manifest_hint: None,
        })
        .await
        .expect_err("GetPath: C doesn't trust K1 → NotFound");
    assert_eq!(err.code(), tonic::Code::NotFound, "GetPath: {err:?}");

    // QueryPathFromHashPart: same gate, no substitute fallback.
    let hash_part = path
        .strip_prefix("/nix/store/")
        .unwrap()
        .split('-')
        .next()
        .unwrap()
        .to_string();
    let err = client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: hash_part.clone(),
        })
        .await
        .expect_err("HashPart: C doesn't trust K1 → NotFound");
    assert_eq!(err.code(), tonic::Code::NotFound, "HashPart: {err:?}");

    // FindMissingPaths: gate-failed paths reported as missing (NOT
    // present — would launder into path_tenants via scheduler).
    let resp = client
        .find_missing_paths(FindMissingPathsRequest {
            store_paths: vec![path.clone()],
        })
        .await?
        .into_inner();
    assert!(
        resp.missing_paths.contains(&path),
        "FindMissingPaths: gate-failed path must be in missing_paths, got {:?}",
        resp.missing_paths
    );

    // BatchQueryPathInfo / BatchGetManifest: builder-internal —
    // end-user tenant token rejected outright.
    let err = client
        .batch_query_path_info(BatchQueryPathInfoRequest {
            store_paths: vec![path.clone()],
        })
        .await
        .expect_err("BatchQueryPathInfo: end-user tenant token → PermissionDenied");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "BatchQueryPathInfo: {err:?}"
    );
    let err = client
        .batch_get_manifest(BatchGetManifestRequest {
            store_paths: vec![path.clone()],
        })
        .await
        .expect_err("BatchGetManifest: end-user tenant token → PermissionDenied");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "BatchGetManifest: {err:?}"
    );

    // r[verify store.api.add-signatures+2]
    // r[verify store.tenant.narinfo-filter]
    // AddSignatures: same gate. Gate-hidden → NotFound (NOT
    // PermissionDenied — indistinguishable from absent), and the write
    // does NOT happen. Pre-fix: C's call succeeded and appended,
    // letting any tenant fill another's path with junk sigs (DoS via
    // MAX_SIGNATURES post-dedup cap). Also: empty-list call must NOT
    // short-circuit Ok before the gate (existence-probe leak).
    let err = client
        .add_signatures(AddSignaturesRequest {
            store_path: path.clone(),
            signatures: vec!["junk-C:aaaa".into()],
        })
        .await
        .expect_err("AddSignatures: C doesn't trust K1 → NotFound");
    assert_eq!(err.code(), tonic::Code::NotFound, "AddSignatures: {err:?}");
    let err = client
        .add_signatures(AddSignaturesRequest {
            store_path: path.clone(),
            signatures: vec![],
        })
        .await
        .expect_err("AddSignatures(empty): C → NotFound (no existence-probe via no-op)");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "AddSignatures(empty): {err:?}"
    );
    let sigs: Vec<String> =
        sqlx::query_scalar("SELECT signatures FROM narinfo WHERE store_path_hash = $1")
            .bind(&path_hash[..])
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !sigs.iter().any(|s| s.starts_with("junk-C:")),
        "gate-failed AddSignatures must not write; got {sigs:?}"
    );

    // ── B trusts K1 → present via FindMissingPaths + HashPart ──────────
    // Positive control for the new gates (over-broad gate would block B).
    switch.set(Some(tid_b));
    let resp = client
        .find_missing_paths(FindMissingPathsRequest {
            store_paths: vec![path.clone()],
        })
        .await?
        .into_inner();
    assert!(
        !resp.missing_paths.contains(&path),
        "FindMissingPaths: B trusts K1 → path present (NOT missing)"
    );
    let resp = client
        .query_path_from_hash_part(QueryPathFromHashPartRequest { hash_part })
        .await?
        .into_inner();
    assert_eq!(resp.store_path, path, "HashPart: B trusts K1 → visible");
    // AddSignatures positive control: B trusts K1 → gate passes →
    // appended. Proves the gate isn't over-broad (rejecting everyone).
    client
        .add_signatures(AddSignaturesRequest {
            store_path: path.clone(),
            signatures: vec!["new-B:bbbb".into()],
        })
        .await?;
    let sigs: Vec<String> =
        sqlx::query_scalar("SELECT signatures FROM narinfo WHERE store_path_hash = $1")
            .bind(&path_hash[..])
            .fetch_one(&db.pool)
            .await?;
    assert!(
        sigs.iter().any(|s| s == "new-B:bbbb"),
        "B's AddSignatures must append; got {sigs:?}"
    );

    // ── Anonymous → batch RPCs pass through (builder, no token) ────────
    switch.set(None);
    let resp = client
        .batch_query_path_info(BatchQueryPathInfoRequest {
            store_paths: vec![path.clone()],
        })
        .await?
        .into_inner();
    assert_eq!(
        resp.entries.len(),
        1,
        "anonymous BatchQueryPathInfo unfiltered"
    );

    // ── Add K1 to C → re-query → now visible ───────────────────────────
    // Proves the gate re-reads tenant_trusted_keys per-request (no
    // stale in-process cache). Second upstream for C with a distinct
    // URL (AddUpstream is upsert-by-url; same URL would replace K2).
    switch.set(Some(tid_c));
    admin
        .add_upstream(AddUpstreamRequest {
            tenant_id: tid_c.to_string(),
            url: "https://cache-c-k1.example".into(),
            priority: 60,
            trusted_keys: vec![trusted_k1.clone()],
            sig_mode: "keep".into(),
        })
        .await?;
    let resp = client.query_path_info(req()).await?.into_inner();
    assert_eq!(
        resp.store_path, path,
        "after adding K1 to C's trusted_keys → visible"
    );

    // ── A (the substituting tenant) → visible ──────────────────────────
    // Sanity: A trusts K1 (it's their own upstream's key).
    switch.set(Some(tid_a));
    let resp = client.query_path_info(req()).await?.into_inner();
    assert_eq!(resp.store_path, path, "tenant A sees own path");

    server.abort();
    Ok(())
}

// r[verify store.substitute.tenant-sig-visibility+2]
/// PutPath→scheduler timing-window regression: a freshly-built,
/// rio-signed path with zero `path_tenants` rows must be visible to
/// its owning tenant via the cluster-key union.
///
/// `path_tenants` is populated by the scheduler at build-completion
/// (`upsert_path_tenants` in rio-scheduler), NOT by PutPath. During
/// the intervening window the gate sees count=0 and fires. Without
/// the cluster key in the trusted set, the rio signature fails to
/// verify against the tenant's upstream-only `trusted_keys` and the
/// path returns `NotFound` to its own tenant.
///
/// Sequence:
///   1. Seed a path signed by the CLUSTER key (not any upstream key).
///   2. Do NOT populate path_tenants (scheduler not yet caught up).
///   3. Tenant has upstream trusted_keys that do NOT include the
///      cluster key (normal config — tenants don't list the cluster
///      key in their upstream config).
///   4. QueryPathInfo from that tenant → path visible (cluster sig
///      verifies against the unioned cluster key).
///
/// Before the fix, step 4 returns NotFound. After, it returns the
/// path.
#[tokio::test]
async fn sig_visibility_gate_cluster_key_timing_window() -> TestResult {
    use base64::Engine;

    let db = TestDb::new(&MIGRATOR).await;
    let tid = seed_tenant(&db.pool, "timing-window").await;

    // ── Cluster key (what rio signs freshly-built paths with) ──────────
    let cluster_seed = [0x55u8; 32];
    let cluster_signer = Signer::from_seed("rio-cluster", &cluster_seed);
    let cluster_trusted_entry = cluster_signer.trusted_key_entry();

    // ── Unrelated upstream key (tenant trusts this, NOT cluster) ───────
    // The bug: gate checked ONLY upstream keys. A cluster-signed path
    // with no upstream-key match → NotFound.
    let upstream_seed = [0x66u8; 32];
    let pk_upstream = ed25519_dalek::SigningKey::from_bytes(&upstream_seed).verifying_key();
    let trusted_upstream = format!(
        "key-upstream:{}",
        base64::engine::general_purpose::STANDARD.encode(pk_upstream.as_bytes())
    );

    // ── Service WITH signer (cluster key available for the union) ──────
    // Substituter must be present or the gate short-circuits early.
    let sub = Arc::new(Substituter::new(db.pool.clone(), None));
    let ts = TenantSigner::new(cluster_signer.clone(), db.pool.clone());
    let store_svc = StoreServiceImpl::new(db.pool.clone())
        .with_substituter(sub)
        .with_signer(ts);
    let admin_svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

    let switch = SwitchableTenant::new();
    let router = Server::builder()
        .layer(tonic::service::InterceptorLayer::new(switch.interceptor()))
        .add_service(StoreServiceServer::new(store_svc))
        .add_service(StoreAdminServiceServer::new(admin_svc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = StoreServiceClient::new(channel.clone());
    let mut admin = StoreAdminServiceClient::new(channel);

    // ── Tenant trusts ONLY the upstream key, NOT cluster ───────────────
    // This is the normal config: tenants configure upstream cache
    // pubkeys. The cluster key is rio-internal; tenants never list it.
    admin
        .add_upstream(AddUpstreamRequest {
            tenant_id: tid.to_string(),
            url: "https://cache-timing.example".into(),
            priority: 50,
            trusted_keys: vec![trusted_upstream],
            sig_mode: "keep".into(),
        })
        .await?;

    // Confirm cluster key is NOT in the DB-side trusted set. The gate's
    // union adds it at query-time, not here.
    let db_trusted: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT unnest(trusted_keys) FROM tenant_upstreams WHERE tenant_id = $1",
    )
    .bind(tid)
    .fetch_all(&db.pool)
    .await?;
    assert!(
        !db_trusted.contains(&cluster_trusted_entry),
        "precondition: tenant's DB-side trusted_keys must NOT include cluster key \
         (union happens at query-time in the gate)"
    );

    // ── Seed path signed by CLUSTER key ────────────────────────────────
    // Simulates: PutPath completed (rio-signed the path), scheduler
    // hasn't yet run upsert_path_tenants.
    let path = test_store_path("timing-window-p");
    let (nar, nar_hash) = make_nar(b"timing-window-payload");
    let fp = rio_nix::narinfo::fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
    let sig_cluster = cluster_signer.sign(&fp);

    let path_hash = sha256(&path);
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, signatures) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&path_hash[..])
    .bind(&path)
    .bind(&nar_hash[..])
    .bind(nar.len() as i64)
    .bind(&[sig_cluster][..])
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO manifests (store_path_hash, status, inline_blob) \
         VALUES ($1, 'complete', $2)",
    )
    .bind(&path_hash[..])
    .bind(&nar[..])
    .execute(&db.pool)
    .await?;

    // ── Precondition: path_tenants count = 0 ───────────────────────────
    // Proves we're testing the timing window, not the post-scheduler
    // path (where count ≥ 1 bypasses the gate entirely). If this fails,
    // every assertion below is vacuous.
    let pt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM path_tenants WHERE store_path_hash = $1")
            .bind(&path_hash[..])
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        pt_count, 0,
        "precondition: zero path_tenants rows (scheduler not yet caught up)"
    );

    // ── THE assertion: cluster-signed path visible during the window ───
    // Before the fix: gate checks only upstream keys → cluster sig
    // fails → NotFound. After: cluster key is in the trusted set →
    // cluster sig verifies → path returned.
    switch.set(Some(tid));
    let resp = client
        .query_path_info(QueryPathInfoRequest {
            store_path: path.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(
        resp.store_path, path,
        "cluster-signed path must be visible during PutPath→scheduler window \
         (cluster key unioned into trusted set)"
    );

    server.abort();
    Ok(())
}

// r[verify store.tenant.sign-key]
// r[verify store.substitute.tenant-sig-visibility+2]
/// Clone of `sig_visibility_gate_cluster_key_timing_window` but with a
/// `tenant_keys` row seeded — `maybe_sign` then signs with the TENANT
/// key (not cluster). Pre-fix: trusted set = upstream ∪ cluster only →
/// the tenant's own sig fails to verify → `NotFound` to its own tenant.
#[tokio::test]
async fn sig_visibility_gate_tenant_key_timing_window() -> TestResult {
    use base64::Engine;

    let db = TestDb::new(&MIGRATOR).await;
    let tid = seed_tenant(&db.pool, "tenant-key-window").await;

    // Tenant has its own signing key (per-tenant sign-key spec).
    let tenant_seed = [0x99u8; 32];
    sqlx::query(
        "INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) \
         VALUES ($1, 'tenant-own-1', $2)",
    )
    .bind(tid)
    .bind(&tenant_seed[..])
    .execute(&db.pool)
    .await?;
    let tenant_signer = Signer::from_seed("tenant-own-1", &tenant_seed);

    // Cluster signer DIFFERENT from tenant key (proves it's the
    // tenant_keys union doing the work, not cluster).
    let cluster = Signer::from_seed("rio-cluster", &[0xCCu8; 32]);
    let sub = Arc::new(Substituter::new(db.pool.clone(), None));
    let ts = TenantSigner::new(cluster, db.pool.clone());
    let store_svc = StoreServiceImpl::new(db.pool.clone())
        .with_substituter(sub)
        .with_signer(ts);
    let admin_svc = StoreAdminServiceImpl::new(db.pool.clone(), None);

    let switch = SwitchableTenant::new();
    let router = Server::builder()
        .layer(tonic::service::InterceptorLayer::new(switch.interceptor()))
        .add_service(StoreServiceServer::new(store_svc))
        .add_service(StoreAdminServiceServer::new(admin_svc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = StoreServiceClient::new(channel.clone());
    let mut admin = StoreAdminServiceClient::new(channel);

    // Tenant trusts ONLY an unrelated upstream key.
    let upstream_seed = [0x66u8; 32];
    let pk = ed25519_dalek::SigningKey::from_bytes(&upstream_seed).verifying_key();
    let trusted_upstream = format!(
        "key-upstream:{}",
        base64::engine::general_purpose::STANDARD.encode(pk.as_bytes())
    );
    admin
        .add_upstream(AddUpstreamRequest {
            tenant_id: tid.to_string(),
            url: "https://cache-tk.example".into(),
            priority: 50,
            trusted_keys: vec![trusted_upstream],
            sig_mode: "keep".into(),
        })
        .await?;

    // Seed path signed ONLY by the tenant key. Zero path_tenants rows.
    let path = test_store_path("tenant-key-window-p");
    let (nar, nar_hash) = make_nar(b"tenant-key-payload");
    let fp = rio_nix::narinfo::fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
    let sig_tenant = tenant_signer.sign(&fp);
    let path_hash = sha256(&path);
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, signatures) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&path_hash[..])
    .bind(&path)
    .bind(&nar_hash[..])
    .bind(nar.len() as i64)
    .bind(&[sig_tenant][..])
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO manifests (store_path_hash, status, inline_blob) VALUES ($1, 'complete', $2)",
    )
    .bind(&path_hash[..])
    .bind(&nar[..])
    .execute(&db.pool)
    .await?;

    // THE assertion: tenant sees its own tenant-key-signed path.
    switch.set(Some(tid));
    let resp = client
        .query_path_info(QueryPathInfoRequest {
            store_path: path.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(
        resp.store_path, path,
        "tenant-key-signed path must be visible to its own tenant during \
         path_tenants count=0 window (tenant_keys unioned into trusted set)"
    );

    server.abort();
    Ok(())
}

/// SHA-256 of the store-path string (the `store_path_hash` column
/// convention). Mirrors `ValidatedPathInfo::sha256_digest` without
/// constructing a full `ValidatedPathInfo`.
fn sha256(s: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).into()
}
