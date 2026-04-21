//! Cross-tenant: SubmitBuild attribution goes to the submitter.
//!
//! `rio-cli builds` doesn't expose `--tenant` (the proto's
//! `ListBuildsRequest.tenant_filter` exists but isn't wired to the
//! CLI), so the server-side-filter probe is deferred. What we CAN
//! assert: A submits → that build's `tenant_id` is A's UUID (not B's,
//! not empty). And B has zero builds (B didn't submit). If the gateway
//! mis-attributed (wrong key→tenant mapping, JWT sub mix-up), one of
//! those would fail.
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

        // A submits a build (5s, completes before we check). B does NOT.
        ctx.nix_build_via_gateway(0, "iso01-a", 5, 1).await?;

        // No `--tenant` on rio-cli; fetch all and filter client-side.
        let resp: Value = ctx.cli_json(&["builds"])?;
        let builds = resp
            .get("builds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let count = |uuid: &str| -> usize {
            builds
                .iter()
                .filter(|b| b.get("tenant_id").and_then(Value::as_str) == Some(uuid))
                .count()
        };
        let n_a = count(&a_uuid);
        let n_b = count(&b_uuid);

        if n_a == 0 {
            return Ok(Verdict::Fail(format!(
                "no build with tenant_id={a_uuid} after A submitted — \
                 SubmitBuild attribution dropped or mis-attributed"
            )));
        }
        if n_b != 0 {
            return Ok(Verdict::Fail(format!(
                "{n_b} build(s) with tenant_id={b_uuid} but B never submitted — \
                 cross-tenant attribution leak (A's key mapped to B?)"
            )));
        }
        Ok(Verdict::Pass)
    }
}
