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

/// `MAX_GC_EXTRA_ROOTS` (100k) is application-bounded at ~8 MB of path
/// strings. tonic's default `max_decoding_message_size` is 4 MiB —
/// without the explicit limit on `StoreAdminServiceServer` (main.rs),
/// transport rejects ~50k paths with `OutOfRange` before `check_bound`
/// ever runs. After: 80k paths reach the handler, which rejects on
/// `validate_store_path` (InvalidArgument naming the path).
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
