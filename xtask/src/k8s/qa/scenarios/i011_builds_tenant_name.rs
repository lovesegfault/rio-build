//! I-011: builds submitted with a tenant SSH key end up with NULL
//! `tenant_id` — SubmitBuild's tenant resolution silently dropped.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BuildsTenantName;

#[async_trait]
impl Scenario for BuildsTenantName {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i011-builds-tenant-name",
            i_ref: Some(11),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // BuildInfo has no name/drv_path field — only build_id,
        // tenant_id, submitted_at, etc. So scope by tenant_id == this
        // tenant's UUID. Look that up via list-tenants (json is the raw
        // TenantInfo array). FK SET NULL after DeleteTenant means OTHER
        // tenants' orphaned builds correctly have empty tenant_id; we
        // only assert OUR submission resolved.
        let tenant_name = ctx.tenant(0).name.clone();
        let tenants: Vec<Value> = ctx.cli_json(&["list-tenants"])?;
        let Some(my_uuid) = tenants
            .iter()
            .find(|t| t.get("tenant_name").and_then(Value::as_str) == Some(&tenant_name))
            .and_then(|t| t.get("tenant_id").and_then(Value::as_str))
            .map(str::to_owned)
        else {
            return Ok(Verdict::Fail(format!(
                "tenant '{tenant_name}' not in list-tenants — alloc_tenants \
                 CreateTenant didn't persist?"
            )));
        };

        ctx.nix_build_via_gateway(0, "i011", 5, 1).await?;

        let resp: Value = ctx.cli_json(&["builds"])?;
        let builds = resp
            .get("builds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mine: Vec<_> = builds
            .iter()
            .filter(|b| b.get("tenant_id").and_then(Value::as_str) == Some(my_uuid.as_str()))
            .collect();

        if mine.is_empty() {
            // The build we submitted isn't attributed to our tenant
            // UUID — either it has empty/NULL tenant_id (the I-011
            // bug) or a DIFFERENT id (impossible — our key's comment
            // is our tenant name).
            Ok(Verdict::Fail(format!(
                "no build with tenant_id={my_uuid} ({tenant_name}) after \
                 submitting one — SubmitBuild tenant resolution dropped"
            )))
        } else {
            Ok(Verdict::Pass)
        }
    }
}
