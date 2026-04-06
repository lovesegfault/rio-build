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

    // Initially empty.
    let resp = svc.list_tenants(Request::new(())).await?.into_inner();
    assert!(resp.tenants.is_empty());

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
    assert_eq!(resp.tenants.len(), 1);
    assert_eq!(resp.tenants[0].tenant_name, "team-alpha");

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
