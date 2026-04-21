//! `rio-cli create-tenant|list-tenants` — tenant CRUD via AdminService.

use crate::AdminClient;
use anyhow::anyhow;
use rio_proto::types::{CreateTenantRequest, DeleteTenantRequest, TenantInfo};

use crate::{emit, json, rpc};

#[derive(clap::Args, Clone)]
pub(crate) struct CreateArgs {
    /// Tenant name (unique, non-empty after trim).
    name: String,
    /// GC retention period in hours. Build artifacts for this tenant
    /// are eligible for sweep after this many hours without access.
    #[arg(long)]
    gc_retention_hours: Option<u32>,
    /// Storage cap in bytes. Soft limit — GC targets this tenant
    /// more aggressively when exceeded.
    #[arg(long)]
    gc_max_store_bytes: Option<u64>,
    /// Bearer token for binary-cache HTTP access. Unset = no cache
    /// access for this tenant.
    #[arg(long)]
    cache_token: Option<String>,
}

pub(crate) async fn run_create(
    as_json: bool,
    client: &mut AdminClient,
    a: CreateArgs,
) -> anyhow::Result<()> {
    let req = CreateTenantRequest {
        tenant_name: a.name,
        gc_retention_hours: a.gc_retention_hours,
        gc_max_store_bytes: a.gc_max_store_bytes,
        cache_token: a.cache_token,
    };
    let resp = rpc("CreateTenant", async || {
        client.create_tenant(req.clone()).await
    })
    .await?;
    let t = resp
        .tenant
        .ok_or_else(|| anyhow!("CreateTenant returned no TenantInfo"))?;
    if as_json {
        return json(&t);
    }
    print_tenant(&t);
    Ok(())
}

pub(crate) async fn run_delete(
    as_json: bool,
    client: &mut AdminClient,
    name: String,
) -> anyhow::Result<()> {
    let req = DeleteTenantRequest { tenant_name: name };
    let resp = rpc("DeleteTenant", async || {
        client.delete_tenant(req.clone()).await
    })
    .await?;
    if as_json {
        return json(&serde_json::json!({ "deleted": resp.deleted }));
    }
    println!("deleted");
    Ok(())
}

pub(crate) async fn run_list(as_json: bool, client: &mut AdminClient) -> anyhow::Result<()> {
    let resp = rpc("ListTenants", async || client.list_tenants(()).await).await?;
    emit(as_json, &resp.tenants, "(no tenants)", print_tenant)
}

/// Best-effort `tenant_id → tenant_name` lookup for human-readable
/// rendering in `builds`/`status`/etc. Returns an empty map on RPC
/// failure so callers fall back to printing the raw UUID rather than
/// failing the whole command on a display-only concern.
pub(crate) async fn name_map(
    client: &mut AdminClient,
) -> std::collections::HashMap<String, String> {
    match rpc("ListTenants", async || client.list_tenants(()).await).await {
        Ok(r) => r
            .tenants
            .into_iter()
            .map(|t| (t.tenant_id, t.tenant_name))
            .collect(),
        Err(_) => Default::default(),
    }
}

fn print_tenant(t: &TenantInfo) {
    println!(
        "tenant {} ({})  gc_retention={}h  max_store={}  cache_token={}",
        t.tenant_name,
        t.tenant_id,
        t.gc_retention_hours,
        t.gc_max_store_bytes
            .map(|b| format!("{b}B"))
            .unwrap_or_else(|| "unlimited".into()),
        if t.has_cache_token { "yes" } else { "no" }
    );
}
