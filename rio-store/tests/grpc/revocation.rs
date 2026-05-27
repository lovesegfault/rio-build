//! Terminal-build revocation of assignment tokens on the castore read
//! surface (`r[store.castore.terminal-revocation]`).
//!
//! A token whose `drv_hash` references a terminal derivation must be
//! rejected with `PERMISSION_DENIED` (non-leaky message) on the
//! tenant-scoped castore RPCs; a token for a live build must pass the
//! gate and reach the data plane (NotFound for digests that don't
//! exist — proof the request got past auth). Cache semantics (negative
//! caching, TTL-bounded refresh after a status flip) are unit-tested in
//! `rio-store/src/revocation.rs` against the same Postgres schema.

use super::*;

use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier, TokenRole};
use rio_proto::DirectoryServiceServer;
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::{GetDirectoryRequest, StatBlobRequest, get_directory_request};
use rio_store::cas::ChunkCache;
use rio_store::grpc::DirectoryServiceImpl;
use rio_store::revocation::BuildTerminalProbe;
use rio_store::test_helpers::seed_tenant;
use rio_test_support::metrics::CountingRecorder;

const REV_KEY: &[u8] = b"revocation-test-key-32-bytes-aaa";

/// Assignment token bound to `drv_hash` for `tenant` (the same shape
/// the scheduler mints at dispatch).
fn assignment_token(tenant: uuid::Uuid, drv_hash: &str) -> String {
    HmacSigner::from_key(REV_KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "revocation-test".into(),
        drv_hash: drv_hash.into(),
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

/// Spawn a `DirectoryService` with HMAC tenant resolution and (when
/// `revoke`) the terminal-build revocation probe wired, mirroring the
/// main.rs default config.
async fn spawn_directory_service(
    pool: sqlx::PgPool,
    revoke: bool,
) -> (DirectoryServiceClient<Channel>, tokio::task::JoinHandle<()>) {
    let cache = Arc::new(ChunkCache::new(mem_backend() as Arc<dyn ChunkBackend>));
    let mut svc = DirectoryServiceImpl::new(
        pool,
        Some(Arc::new(HmacVerifier::from_key(REV_KEY.to_vec()))),
        cache,
    );
    if revoke {
        svc = svc.with_revocation(BuildTerminalProbe::new(std::time::Duration::from_secs(10)));
    }
    let router = Server::builder().add_service(DirectoryServiceServer::new(svc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("connect to in-process directory service");
    (DirectoryServiceClient::new(channel), server)
}

/// Seed a `derivations` row the way the scheduler does (the columns it
/// always writes), with the given status.
async fn seed_derivation(pool: &sqlx::PgPool, drv_hash: &str, status: &str) {
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ($1, $2, 'x86_64-linux', $3)",
    )
    .bind(drv_hash)
    .bind(format!("/nix/store/{drv_hash}-revocation.drv"))
    .bind(status)
    .execute(pool)
    .await
    .expect("seed derivations row");
}

fn stat_req(tok: &str) -> tonic::Request<StatBlobRequest> {
    with_assignment_token(
        StatBlobRequest {
            file_digest: vec![0u8; 32],
            send_chunks: false,
        },
        tok,
    )
}

fn get_dir_req(tok: &str) -> tonic::Request<GetDirectoryRequest> {
    with_assignment_token(
        GetDirectoryRequest {
            by_what: Some(get_directory_request::ByWhat::Digest(vec![0u8; 32])),
            recursive: false,
            digests: vec![],
        },
        tok,
    )
}

/// A token for a live (non-terminal) build passes the gate (the RPC
/// reaches the data plane and answers NotFound for an unknown digest);
/// the same RPCs with a token for a completed build are rejected with
/// PERMISSION_DENIED, the message leaks nothing about why, and the
/// rejection counter increments once per rejected call.
// r[verify store.castore.terminal-revocation]
#[tokio::test]
async fn terminal_build_token_rejected_on_castore_reads() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    let tenant = seed_tenant(&db.pool, "revocation-tenant").await;
    seed_derivation(&db.pool, "rev-grpc-live", "running").await;
    seed_derivation(&db.pool, "rev-grpc-done", "completed").await;

    let (mut client, server) = spawn_directory_service(db.pool.clone(), true).await;
    let _guard_srv = scopeguard::guard(server, |h| h.abort());

    let live_tok = assignment_token(tenant, "rev-grpc-live");
    let done_tok = assignment_token(tenant, "rev-grpc-done");

    // Live build: past the revocation + tenant gates, into the data
    // plane — the unknown digest answers NotFound, not an auth error.
    let err = client
        .stat_blob(stat_req(&live_tok))
        .await
        .expect_err("unknown digest should be NotFound for a live build's token");
    assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");
    let err = client
        .get_directory(get_dir_req(&live_tok))
        .await
        .expect_err("unknown digest should be NotFound for a live build's token");
    assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");

    // Terminal build: rejected before any data-plane lookup, with the
    // same non-leaky phrasing as every other token rejection.
    for (rpc, err) in [
        (
            "StatBlob",
            client.stat_blob(stat_req(&done_tok)).await.err(),
        ),
        (
            "GetDirectory",
            client.get_directory(get_dir_req(&done_tok)).await.err(),
        ),
    ] {
        let err = err.unwrap_or_else(|| panic!("{rpc} with a terminal build's token must fail"));
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{rpc}: {err:?}");
        assert_eq!(
            err.message(),
            "assignment token rejected",
            "{rpc}: rejection must not say why (terminal-vs-forged is an oracle): {err:?}"
        );
    }

    assert_eq!(
        recorder.get("rio_store_castore_terminal_rejected_total{}"),
        2,
        "one rejection per denied RPC; saw counters: {:?}",
        recorder.all_keys()
    );

    Ok(())
}

/// The config knob is real: with revocation NOT wired (i.e.
/// `assignment_revocation.enabled = false`), a terminal build's token
/// still reaches the data plane — pre-revocation behavior preserved.
#[tokio::test]
async fn revocation_disabled_keeps_terminal_token_readable() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant = seed_tenant(&db.pool, "revocation-off-tenant").await;
    seed_derivation(&db.pool, "rev-off-done", "completed").await;

    let (mut client, server) = spawn_directory_service(db.pool.clone(), false).await;
    let _guard_srv = scopeguard::guard(server, |h| h.abort());

    let done_tok = assignment_token(tenant, "rev-off-done");
    let err = client
        .stat_blob(stat_req(&done_tok))
        .await
        .expect_err("unknown digest is still NotFound when revocation is disabled");
    assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");
    Ok(())
}
