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

        // I-011 is about SubmitBuild dropping tenant resolution — the
        // probe is "did the build I JUST submitted get my tenant_id?".
        // builds.tenant_id IS NULL is also a valid state for orphans
        // (FK ON DELETE SET NULL after DeleteTenant — every prior qa
        // run's cleanup orphans its builds), so checking ALL builds
        // false-positives. Find THIS build by matching the unique
        // smoke-tag in its name.
        let tag = "i011";
        let resp: Value = ctx.cli_json(&["builds"])?;
        let builds = resp
            .get("builds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mine: Vec<_> = builds
            .iter()
            .filter(|b| {
                b.get("name")
                    .or_else(|| b.get("display_name"))
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.contains(tag))
            })
            .collect();
        if mine.is_empty() {
            return Ok(Verdict::Skip(format!(
                "no build with tag '{tag}' in `rio-cli builds` output — \
                 name field missing or build not listed"
            )));
        }
        let empty: Vec<String> = mine
            .iter()
            .filter(|b| {
                b.get("tenant_id")
                    .and_then(Value::as_str)
                    .is_none_or(|s| s.trim().is_empty())
            })
            .filter_map(|b| b.get("build_id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        if empty.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "build(s) {empty:?} submitted as tenant '{}' have empty \
                 tenant_id — SubmitBuild resolution silently dropped",
                ctx.tenant(0).name
            )))
        }
    }
}
