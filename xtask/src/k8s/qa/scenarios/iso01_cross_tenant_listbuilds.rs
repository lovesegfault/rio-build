//! Cross-tenant: `ListBuilds` tenant filter is honored server-side.
//!
//! Policy (from docs): `AdminService.ListBuilds` is operator-auth
//! (service-token), so a tenant can't enumerate builds via that path
//! at all. The relevant guarantee is that the `tenant_filter`
//! parameter is applied SERVER-SIDE — `rio-cli builds --tenant <B>`
//! must not return A's build. If filtering were client-side only, a
//! malicious caller bypasses it; the server filter is the boundary.
//!
//! `r[sched.tenant.authz+2]` covers SchedulerService RPCs (Watch/
//! Cancel/QueryStatus reject `PERMISSION_DENIED` on tenant mismatch);
//! those have no nix-level entry point to probe directly here.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct CrossTenantListBuilds;

#[async_trait]
impl Scenario for CrossTenantListBuilds {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "iso01-cross-tenant-listbuilds",
            i_ref: None,
            isolation: Isolation::Tenant { count: 2 },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let a_uuid = ctx.tenant_uuid(0)?;
        let b_uuid = ctx.tenant_uuid(1)?;

        // A submits a build (5s, completes before we check).
        ctx.nix_build_via_gateway(0, "iso01-a", 5, 1).await?;

        // Filtered by B's UUID — A's build must not appear.
        let resp: Value = ctx.cli_json(&["builds", "--tenant", &b_uuid])?;
        let builds = resp
            .get("builds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let leaked: Vec<_> = builds
            .iter()
            .filter(|b| b.get("tenant_id").and_then(Value::as_str) == Some(a_uuid.as_str()))
            .filter_map(|b| b.get("build_id").and_then(Value::as_str).map(str::to_owned))
            .collect();

        if !leaked.is_empty() {
            return Ok(Verdict::Fail(format!(
                "ListBuilds(tenant_filter={b_uuid}) returned {} build(s) with \
                 tenant_id={a_uuid}: {leaked:?} — server-side filter not applied",
                leaked.len()
            )));
        }

        // Sanity: filter by A DOES include A's build (filter isn't
        // returning empty for everything).
        let resp_a: Value = ctx.cli_json(&["builds", "--tenant", &a_uuid])?;
        let a_builds = resp_a
            .get("builds")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        if a_builds == 0 {
            return Ok(Verdict::Fail(format!(
                "ListBuilds(tenant_filter={a_uuid}) returned 0 — A's own build \
                 not visible; filter may be over-rejecting"
            )));
        }

        Ok(Verdict::Pass)
    }
}
