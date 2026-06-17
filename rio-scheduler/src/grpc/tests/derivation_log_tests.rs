//! `GetDerivationLog` resolution, tail and tenancy tests
//! (`grpc/derivation_log.rs`).
//!
//! The trait wrapper's identity plumbing (`require_tenant`) is covered
//! by the existing submit/guard tests; these call the handler body
//! directly with an explicit caller tenant so the content-scoping rules
//! are exercised without minting JWTs.

use super::*;
use crate::db::{DerivationRow, SchedulerDb};
use crate::state::{BuildOptions, DerivationStatus, PriorityClass};
use rio_proto::scheduler::GetDerivationLogRequest;
use rio_proto::types::DerivationLogChunk;
use tokio_stream::StreamExt;
use uuid::Uuid;

/// Insert a minimal derivation row; returns `(derivation_id, drv_path)`.
async fn insert_drv(db: &SchedulerDb, hash: &str) -> anyhow::Result<(Uuid, String)> {
    let drv_path = rio_test_support::fixtures::test_drv_path(hash);
    let row = DerivationRow {
        drv_hash: hash.into(),
        drv_path: drv_path.clone(),
        pname: Some("test-pkg".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
    tx.commit().await?;
    Ok((ids.get(hash).expect("just inserted").0, drv_path))
}

async fn insert_tenant(pool: &sqlx::PgPool, name: &str) -> anyhow::Result<Uuid> {
    Ok(
        sqlx::query_scalar("INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id")
            .bind(name)
            .fetch_one(pool)
            .await?,
    )
}

async fn insert_build(db: &SchedulerDb, tenant: Option<Uuid>) -> anyhow::Result<Uuid> {
    let build_id = Uuid::now_v7();
    db.insert_build(
        build_id,
        tenant,
        PriorityClass::Scheduled,
        false,
        &BuildOptions::default(),
        None,
    )
    .await?;
    Ok(build_id)
}

async fn link_build_drv(
    pool: &sqlx::PgPool,
    build_id: Uuid,
    derivation_id: Uuid,
    exec: Option<Uuid>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO build_derivations (build_id, derivation_id, exec_id) VALUES ($1, $2, $3)",
    )
    .bind(build_id)
    .bind(derivation_id)
    .bind(exec)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stamp a ring-buffer entry for `drv_path` with `exec` and `n` lines
/// (`line-0` … `line-{n-1}`).
fn seed_ring(grpc: &SchedulerGrpc, drv_path: &str, exec: Uuid, n: u64) {
    let buffers = grpc.log_buffers();
    buffers.set_exec(drv_path, exec, "test-worker");
    let batch = rio_proto::types::BuildLogBatch {
        derivation_path: drv_path.to_string(),
        lines: (0..n).map(|i| format!("line-{i}").into_bytes()).collect(),
        first_line_number: 0,
        executor_id: "test-worker".into(),
    };
    assert!(buffers.push_for(drv_path, &batch, "test-worker"));
}

/// Unwrap the error arm of a `serve` result without requiring `Debug`
/// on the stream type.
fn expect_status<T>(res: Result<T, tonic::Status>, what: &str) -> tonic::Status {
    match res {
        Err(status) => status,
        Ok(_) => panic!("{what}: expected an error, got a stream"),
    }
}

/// Drain a chunk stream to completion; `Ok(chunks)` or the first error.
async fn collect_chunks(
    stream: tokio_stream::wrappers::ReceiverStream<Result<DerivationLogChunk, tonic::Status>>,
) -> Result<Vec<DerivationLogChunk>, tonic::Status> {
    let mut stream = stream;
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item?);
    }
    Ok(chunks)
}

fn log_req(drv_path: &str) -> GetDerivationLogRequest {
    GetDerivationLogRequest {
        build_id: String::new(),
        derivation_path: drv_path.to_string(),
        exec_id: String::new(),
        tail_lines: 0,
        since_line: 0,
    }
}

/// In-tenant resolution without a build pin: the latest execution among
/// the caller's builds is served, and `tail_lines` rebases the cursor
/// server-side so only the last N lines come back.
#[tokio::test]
async fn test_derivation_log_tail_for_own_build() -> anyhow::Result<()> {
    let (db_guard, grpc, _handle, _task) = setup_grpc_with_pool().await;
    let db = SchedulerDb::new(db_guard.pool.clone());

    let tenant = insert_tenant(&db_guard.pool, "log-owner").await?;
    let (drv_id, drv_path) = insert_drv(&db, "log-own-drv").await?;
    let build = insert_build(&db, Some(tenant)).await?;
    let exec = Uuid::now_v7();
    link_build_drv(&db_guard.pool, build, drv_id, Some(exec)).await?;
    seed_ring(&grpc, &drv_path, exec, 30);

    let req = GetDerivationLogRequest {
        tail_lines: 20,
        ..log_req(&drv_path)
    };
    let chunks = collect_chunks(derivation_log::serve(&grpc, Some(tenant), req).await?).await?;
    let lines: Vec<&[u8]> = chunks
        .iter()
        .flat_map(|c| &c.lines)
        .map(Vec::as_slice)
        .collect();
    assert_eq!(lines.len(), 20, "server-side tail must serve 20 lines");
    assert_eq!(
        lines[0], b"line-10",
        "tail must start 20 lines from the end"
    );
    assert_eq!(chunks[0].first_line_number, 10);
    assert_eq!(chunks[0].exec_id, exec.to_string());

    // Pinned-build form resolves the same execution.
    let req = GetDerivationLogRequest {
        build_id: build.to_string(),
        ..log_req(&drv_path)
    };
    let chunks = collect_chunks(derivation_log::serve(&grpc, Some(tenant), req).await?).await?;
    let total: usize = chunks.iter().map(|c| c.lines.len()).sum();
    assert_eq!(total, 30, "tail_lines=0 serves the full log");
    Ok(())
}

/// Cross-tenant content gate: a pinned execution that ran for another
/// tenant's build yields an empty stream (no content, no error) even
/// when the caller's own build legitimately contains the derivation;
/// the owning tenant still gets the content; an unpinned request for a
/// derivation with no execution under the caller's builds is NOT_FOUND.
// r[verify sched.log.tenant-scoped]
#[tokio::test]
async fn test_derivation_log_tenant_scoped() -> anyhow::Result<()> {
    let (db_guard, grpc, _handle, _task) = setup_grpc_with_pool().await;
    let db = SchedulerDb::new(db_guard.pool.clone());

    let tenant_a = insert_tenant(&db_guard.pool, "log-ten-a").await?;
    let tenant_b = insert_tenant(&db_guard.pool, "log-ten-b").await?;
    let (drv_id, drv_path) = insert_drv(&db, "log-shared-drv").await?;

    // Tenant B originally built (and failed) the derivation: its build
    // records the execution, and the ring buffer holds that execution's
    // log lines.
    let build_b = insert_build(&db, Some(tenant_b)).await?;
    let exec_b = Uuid::now_v7();
    link_build_drv(&db_guard.pool, build_b, drv_id, Some(exec_b)).await?;
    seed_ring(&grpc, &drv_path, exec_b, 5);

    // Tenant A's later build fail-fasted on the poisoned node: the drv is
    // part of A's build but no execution ever ran for it.
    let build_a = insert_build(&db, Some(tenant_a)).await?;
    link_build_drv(&db_guard.pool, build_a, drv_id, None).await?;

    // A pins its own build + the culprit execution it learned from
    // BuildFailed: the execution is B's, so no content is served.
    let req = GetDerivationLogRequest {
        build_id: build_a.to_string(),
        exec_id: exec_b.to_string(),
        tail_lines: 20,
        ..log_req(&drv_path)
    };
    let chunks = collect_chunks(derivation_log::serve(&grpc, Some(tenant_a), req).await?).await?;
    assert!(
        chunks.iter().all(|c| c.lines.is_empty()),
        "another tenant's execution log must not be served: {chunks:?}"
    );

    // A without any pin: no execution of this drv under A's builds →
    // NOT_FOUND (same answer as for a drv A never touched).
    let err = expect_status(
        derivation_log::serve(&grpc, Some(tenant_a), log_req(&drv_path)).await,
        "unpinned cross-tenant resolution must not serve content",
    );
    assert_eq!(err.code(), tonic::Code::NotFound);

    // A pinning B's build outright is a permission error.
    let req = GetDerivationLogRequest {
        build_id: build_b.to_string(),
        ..log_req(&drv_path)
    };
    let err = expect_status(
        derivation_log::serve(&grpc, Some(tenant_a), req).await,
        "foreign build pin must be rejected",
    );
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // The owner gets the content.
    let chunks =
        collect_chunks(derivation_log::serve(&grpc, Some(tenant_b), log_req(&drv_path)).await?)
            .await?;
    let total: usize = chunks.iter().map(|c| c.lines.len()).sum();
    assert_eq!(total, 5, "the owning tenant reads its own execution's log");

    // A drv nobody recorded at all is NOT_FOUND too — indistinguishable
    // from the cross-tenant case above.
    let err = expect_status(
        derivation_log::serve(
            &grpc,
            Some(tenant_a),
            log_req(&rio_test_support::fixtures::test_drv_path("log-unknown")),
        )
        .await,
        "unknown drv must be NOT_FOUND",
    );
    assert_eq!(err.code(), tonic::Code::NotFound);
    Ok(())
}
