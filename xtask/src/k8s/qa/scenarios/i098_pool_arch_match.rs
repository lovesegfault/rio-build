//! I-098: builder advertises spec.systems but lands on wrong-arch node.
//!
//! Live-cluster complement to the unit test
//! `job_pod_derives_arch_node_selector_from_systems` — that proves the
//! controller code is correct; this proves the LIVE Jobs have it (no
//! stale CR override or chart-side selector clobbering it).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::k8s::{NS_BUILDERS, NS_FETCHERS};

pub struct PoolArchMatch;

#[async_trait]
impl Scenario for PoolArchMatch {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i098-pool-arch-match",
            i_ref: Some(98),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let mut missing = Vec::new();
        for ns in [NS_BUILDERS, NS_FETCHERS] {
            let out = ctx.kubectl(&[
                "-n",
                ns,
                "get",
                "jobs",
                "-o",
                "jsonpath={range .items[*]}{.metadata.name}={.spec.template.spec.nodeSelector.kubernetes\\.io/arch}{\"\\n\"}{end}",
            ])?;
            for line in out.lines() {
                let Some((name, arch)) = line.split_once('=') else {
                    continue;
                };
                if arch.trim().is_empty() {
                    missing.push(format!("{ns}/{name}"));
                }
            }
        }
        if missing.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "worker Job(s) missing kubernetes.io/arch nodeSelector: {missing:?} \
                 — controller arch-derivation not applied (Graviton risk)"
            )))
        }
    }
}
