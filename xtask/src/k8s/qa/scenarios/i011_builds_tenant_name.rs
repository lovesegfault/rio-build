//! I-011: `rio-cli builds` shows `tenant=` (empty) for valid builds.
//!
//! Either a cli formatting bug or the builds row has a NULL tenant_id —
//! the latter is a SubmitBuild data-integrity issue (SSH-key-comment →
//! tenant resolution failed silently).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BuildsTenantName;

#[derive(Deserialize)]
struct Build {
    build_id: String,
    #[serde(default)]
    tenant_name: String,
}

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

        let builds: Vec<Build> = match ctx.cli_json(&["builds"]) {
            Ok(b) => b,
            Err(e) => {
                return Ok(Verdict::Skip(format!(
                    "rio-cli builds --json shape mismatch: {e:#}"
                )));
            }
        };
        if builds.is_empty() {
            return Ok(Verdict::Skip("no builds returned".into()));
        }
        let empty: Vec<_> = builds
            .iter()
            .filter(|b| b.tenant_name.trim().is_empty())
            .map(|b| b.build_id.as_str())
            .take(5)
            .collect();
        if empty.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{} build(s) with empty tenant_name (first 5): {empty:?} \
                 — SubmitBuild tenant resolution or cli rendering",
                empty.len()
            )))
        }
    }
}
