//! I-011: `rio-cli builds` shows `tenant=` (empty) for valid builds.
//!
//! Either a cli formatting bug or the builds row has a NULL tenant_id —
//! the latter is a SubmitBuild data-integrity issue (SSH-key-comment →
//! tenant resolution failed silently).

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
        ctx.nix_build_via_gateway(0, "i011", 5, 1).await?;

        // `rio-cli builds --json` outputs ListBuildsResponse:
        // {"builds": [...], "total_count": N, ...}. The proto has
        // `tenant_id` (UUID string), not `tenant_name` — the human
        // output resolves id→name via a separate ListTenants call.
        let resp: Value = ctx.cli_json(&["builds"])?;
        let builds = resp
            .get("builds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if builds.is_empty() {
            return Ok(Verdict::Skip("no builds returned".into()));
        }
        let empty: Vec<String> = builds
            .iter()
            .filter(|b| {
                b.get("tenant_id")
                    .and_then(Value::as_str)
                    .is_none_or(|s| s.trim().is_empty())
            })
            .filter_map(|b| b.get("build_id").and_then(Value::as_str).map(str::to_owned))
            .take(5)
            .collect();
        if empty.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{} build(s) with empty tenant_id (first 5): {empty:?} \
                 — SubmitBuild tenant resolution silently dropped",
                empty.len()
            )))
        }
    }
}
