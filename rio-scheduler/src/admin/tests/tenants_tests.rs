//! `ListTenants`/`CreateTenant` RPC tests.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/tenants.rs` submodule seam introduced by P0383.

use super::*;

// r[verify sched.admin.list-tenants]
// r[verify sched.admin.create-tenant]
#[tokio::test]
async fn test_create_and_list_tenants() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;

    // Initially empty — modulo the harness default tenant every
    // actor-test setup seeds (merged_bug_003: builds must be tenanted
    // for the substitution lane to exist at all).
    let resp = svc.list_tenants(Request::new(())).await?.into_inner();
    let non_harness = |ts: &[rio_proto::types::TenantInfo]| {
        ts.iter()
            .filter(|t| t.tenant_name != "harness-default-tenant")
            .count()
    };
    assert_eq!(non_harness(&resp.tenants), 0);

    // Create a tenant.
    let created = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-alpha".into(),
            gc_retention_hours: Some(72),
            gc_max_store_bytes: Some(100 * 1024 * 1024 * 1024),
            cache_token: Some("secret-token".into()),
        }))
        .await?
        .into_inner();
    let t = created.tenant.expect("tenant should be set");
    assert_eq!(t.tenant_name, "team-alpha");
    assert_eq!(t.gc_retention_hours, 72);
    assert_eq!(t.gc_max_store_bytes, Some(100 * 1024 * 1024 * 1024));
    assert!(t.has_cache_token, "cache_token was set");
    assert!(!t.tenant_id.is_empty(), "UUID should be populated");
    assert!(
        t.created_at.is_some_and(|ts| ts.seconds > 0),
        "created_at should be populated (epoch seconds via EXTRACT)"
    );

    // List shows it.
    let resp = svc.list_tenants(Request::new(())).await?.into_inner();
    assert_eq!(non_harness(&resp.tenants), 1);
    assert!(
        resp.tenants.iter().any(|t| t.tenant_name == "team-alpha"),
        "created tenant must appear in the list"
    );

    // Duplicate name → AlreadyExists.
    let dup = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-alpha".into(),
            ..Default::default()
        }))
        .await;
    assert_eq!(dup.unwrap_err().code(), tonic::Code::AlreadyExists);

    // Empty name → InvalidArgument.
    let empty = svc
        .create_tenant(Request::new(
            rio_proto::types::CreateTenantRequest::default(),
        ))
        .await;
    assert_eq!(empty.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Whitespace-only name → InvalidArgument (same as empty).
    let ws_name = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "   ".into(),
            ..Default::default()
        }))
        .await;
    assert_eq!(ws_name.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Interior whitespace ("team a") → InvalidArgument. Almost
    // certainly a misconfigured authorized_keys comment (space where a
    // dash was intended). If this stored successfully, no read path
    // would ever find it — they all look up the dashed form.
    let interior_ws = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team a".into(),
            ..Default::default()
        }))
        .await;
    let err = interior_ws.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("interior whitespace"),
        "error should name the InteriorWhitespace reason so the \
         operator knows it's a space-vs-dash typo, not an empty name: {}",
        err.message()
    );

    // Empty cache_token → InvalidArgument (round-3 fix; this test was
    // missing — the validation existed but was never exercised).
    let empty_tok = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-gamma".into(),
            cache_token: Some("".into()),
            ..Default::default()
        }))
        .await;
    assert_eq!(empty_tok.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Whitespace-only cache_token → InvalidArgument (round-4 fix;
    // same bypass class as empty-token, just with "   " instead of "").
    let ws_tok = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-delta".into(),
            cache_token: Some("   ".into()),
            ..Default::default()
        }))
        .await;
    assert_eq!(ws_tok.unwrap_err().code(), tonic::Code::InvalidArgument);

    // Surrounding whitespace is TRIMMED before storage. Read paths
    // trim (gateway comment().trim(), cache auth str::trim), so an
    // untrimmed PG row makes WHERE tenant_name = 'team-trim' never
    // match — invisible-whitespace 'unknown tenant' bug.
    let trimmed = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "  team-trim  ".into(),
            cache_token: Some("  trim-secret  ".into()),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(
        trimmed.tenant.as_ref().unwrap().tenant_name,
        "team-trim",
        "stored tenant_name must be trimmed"
    );
    // cache_token isn't returned (has_cache_token bool only) — verify PG directly.
    let stored_tok: Option<String> =
        sqlx::query_scalar("SELECT cache_token FROM tenants WHERE tenant_name = 'team-trim'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(stored_tok.as_deref(), Some("trim-secret"));

    // gc_retention_hours > i32::MAX → InvalidArgument (was silently
    // wrapping to negative via `as i32` and storing in PG INTEGER).
    let oor = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-oor".into(),
            gc_retention_hours: Some(u32::MAX),
            ..Default::default()
        }))
        .await;
    let err = oor.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("out of range"));

    // Tenant with defaults (no optionals).
    let defaults = svc
        .create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
            tenant_name: "team-beta".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    let t = defaults.tenant.expect("tenant should be set");
    assert_eq!(t.gc_retention_hours, 168, "default 7 days");
    assert_eq!(t.gc_max_store_bytes, None);
    assert!(!t.has_cache_token);

    Ok(())
}

// r[verify sched.admin.delete-tenant]
#[tokio::test]
async fn test_delete_tenant() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;

    svc.create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
        tenant_name: "ephemeral".into(),
        ..Default::default()
    }))
    .await?;

    // Seed an upstream row so we exercise FK CASCADE.
    let tenant_id: uuid::Uuid =
        sqlx::query_scalar("SELECT tenant_id FROM tenants WHERE tenant_name = 'ephemeral'")
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO tenant_upstreams (tenant_id, url, priority) VALUES ($1, 'https://x', 0)",
    )
    .bind(tenant_id)
    .execute(&db.pool)
    .await?;

    let resp = svc
        .delete_tenant(Request::new(rio_proto::types::DeleteTenantRequest {
            tenant_name: "ephemeral".into(),
        }))
        .await?
        .into_inner();
    assert!(resp.deleted);

    // Gone from tenants AND cascaded.
    let n_tenants: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenants WHERE tenant_name = 'ephemeral'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(n_tenants, 0);
    let n_upstreams: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_upstreams WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(n_upstreams, 0, "tenant_upstreams should ON DELETE CASCADE");

    // Unknown name → NotFound.
    let nf = svc
        .delete_tenant(Request::new(rio_proto::types::DeleteTenantRequest {
            tenant_name: "never-existed".into(),
        }))
        .await;
    assert_eq!(nf.unwrap_err().code(), tonic::Code::NotFound);

    // Empty name → InvalidArgument.
    let empty = svc
        .delete_tenant(Request::new(
            rio_proto::types::DeleteTenantRequest::default(),
        ))
        .await;
    assert_eq!(empty.unwrap_err().code(), tonic::Code::InvalidArgument);

    Ok(())
}

// r[verify store.gc.hold+2]
/// W10-G (bug_095, migration 104): tenant offboarding cannot erase
/// hold evidence. Pre-104 the gc_holds FK was ON DELETE CASCADE — a
/// tenant delete silently erased the tenant's hold audit history and
/// any ACTIVE litigation-class hold with no release record (the red:
/// deleted=true, hold row GONE). Post-104 + the typed disposition:
/// the delete REFUSES (FailedPrecondition naming the heal edge) while
/// an active hold exists; releasing the hold flips the refusal to the
/// ARCHIVAL face (hold history pins the tenant anchor — released
/// rows are audit evidence and their tenant_id must keep resolving);
/// the audit row survives every attempt.
#[tokio::test]
async fn delete_tenant_dispositions_gc_holds() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;

    svc.create_tenant(Request::new(rio_proto::types::CreateTenantRequest {
        tenant_name: "held-tenant".into(),
        ..Default::default()
    }))
    .await?;
    let tenant_id: uuid::Uuid =
        sqlx::query_scalar("SELECT tenant_id FROM tenants WHERE tenant_name = 'held-tenant'")
            .fetch_one(&db.pool)
            .await?;

    // An ACTIVE tenant-scope hold (litigation class).
    let hold_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO gc_holds (scope, tenant_id, reason, created_by) \
         VALUES ('tenant', $1, 'litigation hold', 'w10-g') RETURNING hold_id",
    )
    .bind(tenant_id)
    .fetch_one(&db.pool)
    .await?;

    // Active hold ⇒ the delete REFUSES typed and the hold survives.
    let refused = svc
        .delete_tenant(Request::new(rio_proto::types::DeleteTenantRequest {
            tenant_name: "held-tenant".into(),
        }))
        .await;
    let err = refused.expect_err(
        "W10-G RED: tenant delete succeeded with an ACTIVE hold — \
         the cascade erased litigation evidence",
    );
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "the refusal is typed, got {err:?}"
    );
    assert!(
        err.message().contains("active"),
        "the refusal names the heal edge (release first), got: {}",
        err.message()
    );
    let holds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM gc_holds WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(holds, 1, "W10-G RED: the active hold row was erased");

    // Release the hold (the heal edge): the delete now refuses on the
    // ARCHIVAL face — released holds are audit evidence and pin their
    // tenant anchor.
    sqlx::query("UPDATE gc_holds SET released_at = now() WHERE hold_id = $1")
        .bind(hold_id)
        .execute(&db.pool)
        .await?;
    let archival = svc
        .delete_tenant(Request::new(rio_proto::types::DeleteTenantRequest {
            tenant_name: "held-tenant".into(),
        }))
        .await;
    let err = archival.expect_err("hold HISTORY must also refuse (archival doctrine)");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("archival") || err.message().contains("history"),
        "the refusal names the doctrine, got: {}",
        err.message()
    );

    // The release record survives every offboarding attempt.
    let released: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM gc_holds WHERE tenant_id = $1 AND released_at IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        released, 1,
        "the release record is permanent audit evidence"
    );
    Ok(())
}
