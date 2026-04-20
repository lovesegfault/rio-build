//! gRPC-transport-level tests for StoreAdminService.
//!
//! Distinct from the unit tests in `rio_store::grpc::admin::tests` —
//! those call the trait method directly on `StoreAdminServiceImpl`
//! and so cannot exercise the tonic transport layer (message-size
//! limits, status mapping through `storage_error`).

use super::*;
use rio_proto::types::GcRequest;
use rio_proto::{StoreAdminServiceClient, StoreAdminServiceServer};
use rio_store::grpc::StoreAdminServiceImpl;

/// Spawn a StoreAdminService over real gRPC transport with the same
/// `max_{de,en}coding_message_size` main.rs sets. Returns connected
/// client + server handle.
async fn spawn_admin(
    pool: sqlx::PgPool,
    backend: Option<Arc<dyn ChunkBackend>>,
) -> anyhow::Result<(
    StoreAdminServiceClient<Channel>,
    tokio::task::JoinHandle<()>,
)> {
    let max = rio_common::grpc::max_message_size();
    let svc = StoreAdminServiceImpl::new(pool, backend);
    let router = Server::builder().add_service(
        StoreAdminServiceServer::new(svc)
            .max_decoding_message_size(max)
            .max_encoding_message_size(max),
    );
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let client = rio_proto::client::connect_single(&addr.to_string()).await?;
    Ok((client, server))
}

/// `MAX_GC_EXTRA_ROOTS` (`MAX_DAG_NODES * 4` ≈ 4M) is application-
/// bounded at ~250 MB of path strings. tonic's default
/// `max_decoding_message_size` is 4 MiB — without the explicit limit on
/// `StoreAdminServiceServer` (main.rs), transport rejects ~50k paths
/// with `OutOfRange` before `check_bound` ever runs. After: 80k paths
/// reach the handler, which rejects on `validate_store_path`
/// (InvalidArgument naming the path).
///
/// Asserts `InvalidArgument` (handler reached), NOT `OutOfRange` /
/// `ResourceExhausted` (transport reject).
#[tokio::test]
async fn trigger_gc_80k_roots_reaches_handler() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut admin, server) = spawn_admin(db.pool.clone(), None).await?;

    // 80k × 70-byte garbage strings ≈ 5.6 MB > tonic's 4 MiB default.
    // Garbage (not store-path syntax) so the handler rejects EARLY at
    // validate_store_path — we don't want to actually run GC mark
    // over 80k seeds just to prove transport let it through.
    let roots: Vec<String> = (0..80_000)
        .map(|i| format!("not-a-store-path-but-seventy-bytes-long-padding-padding-{i:012}"))
        .collect();
    let payload_bytes: usize = roots.iter().map(|s| s.len()).sum();
    assert!(
        payload_bytes > 4 * 1024 * 1024,
        "payload must exceed tonic default 4 MiB to be meaningful; got {payload_bytes}"
    );

    let err = admin
        .trigger_gc(GcRequest {
            extra_roots: roots,
            dry_run: true,
            grace_period_hours: Some(24),
        })
        .await
        .unwrap_err();

    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "handler reached → validate_store_path rejects; got {} ({})",
        err.code(),
        err.message()
    );
    // Transport-level reject would say "message length too large".
    assert!(
        !err.message().to_lowercase().contains("too large"),
        "transport should not have rejected: {}",
        err.message()
    );

    server.abort();
    Ok(())
}

const SERVICE_KEY: &[u8] = b"admin-gate-service-key-32-bytes!!!";

/// Spawn a StoreAdminService WITH `service_verifier` so the
/// `r[store.admin.service-gate]` gate is armed (production parity).
async fn spawn_admin_gated(
    pool: sqlx::PgPool,
) -> anyhow::Result<(
    StoreAdminServiceClient<Channel>,
    tokio::task::JoinHandle<()>,
)> {
    let max = rio_common::grpc::max_message_size();
    let svc = StoreAdminServiceImpl::new(pool, None).with_service_verifier(Some(Arc::new(
        rio_auth::hmac::HmacVerifier::from_key(SERVICE_KEY.to_vec()),
    )));
    let router = Server::builder().add_service(
        StoreAdminServiceServer::new(svc)
            .max_decoding_message_size(max)
            .max_encoding_message_size(max),
    );
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let client = rio_proto::client::connect_single(&addr.to_string()).await?;
    Ok((client, server))
}

fn sign_service(caller: &str) -> String {
    rio_auth::hmac::HmacSigner::from_key(SERVICE_KEY.to_vec()).sign(
        &rio_auth::hmac::ServiceClaims {
            caller: caller.into(),
            expiry_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 60,
        },
    )
}

/// bug_064: `StoreAdminService` shares port 9002 with builders behind
/// permissive-on-absent JWT; without `x-rio-service-token`, a
/// compromised builder could call `AddUpstream{tenant_id: <victim>,
/// trusted_keys: [attacker_key]}` and poison the victim's substitution
/// path. With the gate armed, no token → PermissionDenied; nothing
/// inserted.
// r[verify store.admin.service-gate]
// r[verify sec.authz.service-token]
#[tokio::test]
async fn add_upstream_without_service_token_rejected() -> TestResult {
    use rio_proto::types::AddUpstreamRequest;
    let db = TestDb::new(&MIGRATOR).await;
    let tid = rio_store::test_helpers::seed_tenant(&db.pool, "gate-victim").await;
    let (mut admin, server) = spawn_admin_gated(db.pool.clone()).await?;

    // No header → reject.
    let err = admin
        .add_upstream(AddUpstreamRequest {
            tenant_id: tid.to_string(),
            url: "https://attacker.example".into(),
            priority: 1,
            trusted_keys: vec![],
            sig_mode: "replace".into(),
        })
        .await
        .expect_err("no service token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // Non-allowlisted caller (e.g. forged "rio-builder") → reject.
    let mut req = tonic::Request::new(AddUpstreamRequest {
        tenant_id: tid.to_string(),
        url: "https://attacker.example".into(),
        priority: 1,
        trusted_keys: vec![],
        sig_mode: "replace".into(),
    });
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        sign_service("rio-builder").parse().unwrap(),
    );
    let err = admin
        .add_upstream(req)
        .await
        .expect_err("non-allowlisted caller → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("not in allowlist"),
        "msg: {}",
        err.message()
    );

    // No INSERT happened.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_upstreams WHERE tenant_id = $1")
        .bind(tid)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(n, 0, "rejected calls must not write tenant_upstreams");

    // Allowlisted caller (rio-cli) → accept.
    let mut req = tonic::Request::new(AddUpstreamRequest {
        tenant_id: tid.to_string(),
        url: "https://cache.example".into(),
        priority: 50,
        trusted_keys: vec![],
        sig_mode: "keep".into(),
    });
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        sign_service("rio-cli").parse().unwrap(),
    );
    admin.add_upstream(req).await?;

    server.abort();
    Ok(())
}

/// `TriggerGC` allowlist is `["rio-scheduler", "rio-controller",
/// "rio-cli"]`. An assignment-token (what a builder holds) is the
/// WRONG key + WRONG claims shape → service gate rejects.
// r[verify store.admin.service-gate]
#[tokio::test]
async fn trigger_gc_without_service_token_rejected() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (mut admin, server) = spawn_admin_gated(db.pool.clone()).await?;

    let err = admin
        .trigger_gc(GcRequest {
            extra_roots: vec![],
            dry_run: true,
            grace_period_hours: Some(24),
        })
        .await
        .expect_err("no service token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // rio-scheduler IS allowlisted → accept (gets a stream).
    let mut req = tonic::Request::new(GcRequest {
        extra_roots: vec![],
        dry_run: true,
        grace_period_hours: Some(24),
    });
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        sign_service("rio-scheduler").parse().unwrap(),
    );
    admin.trigger_gc(req).await?;

    server.abort();
    Ok(())
}
