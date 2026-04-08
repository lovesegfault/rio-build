//! `rio-cli create-tenant|list-tenants` — tenant CRUD via AdminService.
//!
//! Separate module (not inline in `main.rs`) — keep `main.rs` deltas to
//! enum variant + match arm + mod decl only.

use anyhow::anyhow;
use rio_proto::AdminServiceClient;
use rio_proto::types::{CreateTenantRequest, TenantInfo};
use tonic::transport::Channel;

use crate::{json, rpc};

pub(crate) async fn run_create(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    name: String,
    gc_retention_hours: Option<u32>,
    gc_max_store_bytes: Option<u64>,
    cache_token: Option<String>,
) -> anyhow::Result<()> {
    let req = CreateTenantRequest {
        tenant_name: name,
        gc_retention_hours,
        gc_max_store_bytes,
        cache_token,
    };
    let resp = rpc("CreateTenant", async || {
        client.create_tenant(req.clone()).await
    })
    .await?;
    let t = resp
        .tenant
        .ok_or_else(|| anyhow!("CreateTenant returned no TenantInfo"))?;
    if as_json {
        json(&t)?;
    } else {
        print_tenant(&t);
    }
    Ok(())
}

pub(crate) async fn run_list(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    let resp = rpc("ListTenants", async || client.list_tenants(()).await).await?;
    if as_json {
        json(&resp.tenants)?;
    } else if resp.tenants.is_empty() {
        println!("(no tenants)");
    } else {
        for t in &resp.tenants {
            print_tenant(t);
        }
    }
    Ok(())
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
