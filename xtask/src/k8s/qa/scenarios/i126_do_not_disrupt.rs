//! I-126: ephemeral builder/fetcher pods missing
//! `karpenter.sh/do-not-disrupt` → consolidation evicts mid-build.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::k8s::{NS_BUILDERS, NS_FETCHERS};

pub struct DoNotDisrupt;

#[async_trait]
impl Scenario for DoNotDisrupt {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i126-do-not-disrupt",
            i_ref: Some(126),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Background build so a builder pod exists for the duration.
        let bg = ctx.nix_build_via_gateway_bg(0, "i126", 30, 1);
        // Give the controller a beat to spawn.
        tokio::time::sleep(Duration::from_secs(10)).await;

        let mut missing = Vec::new();
        for (ns, label) in [
            (NS_BUILDERS, QaCtx::BUILDER_LABEL),
            (NS_FETCHERS, "app.kubernetes.io/name=rio-fetcher"),
        ] {
            let out = ctx.kubectl(&[
                "-n",
                ns,
                "get",
                "pods",
                "-l",
                label,
                "-o",
                "jsonpath={range .items[*]}{.metadata.name}={.metadata.annotations.karpenter\\.sh/do-not-disrupt}{\"\\n\"}{end}",
            ])?;
            for line in out.lines() {
                let Some((name, ann)) = line.split_once('=') else {
                    continue;
                };
                if ann.trim() != "true" {
                    missing.push(format!("{ns}/{name}"));
                }
            }
        }

        bg.abort();

        if missing.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "worker pod(s) missing karpenter.sh/do-not-disrupt=true: {missing:?}"
            )))
        }
    }
}
