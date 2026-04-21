//! I-085: controller-generated headless Services hardcoded
//! `ipFamilies: [IPv6, IPv4]` → invalid on single-stack cluster.
//!
//! Fix dropped the field. Regression check: no Service in any rio
//! namespace carries `ipFamilies` longer than the cluster supports
//! (single-stack v6 → length 1). The chart hardcodes `[IPv6]` per
//! `e939b058`; controller-emitted Services should either omit it or
//! match.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::NAMESPACES;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct ControllerIpFamilies;

#[async_trait]
impl Scenario for ControllerIpFamilies {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i085-controller-ipfamilies",
            i_ref: Some(85),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let mut bad = Vec::new();
        for &(ns, _) in NAMESPACES {
            // name=count pairs; count>1 on a v6-only cluster is the bug.
            let out = ctx.kubectl(&[
                "-n",
                ns,
                "get",
                "svc",
                "-o",
                "jsonpath={range .items[*]}{.metadata.name}={.spec.ipFamilies}{\"\\n\"}{end}",
            ])?;
            for line in out.lines() {
                let Some((name, fams)) = line.split_once('=') else {
                    continue;
                };
                // jsonpath renders the array as `["IPv6","IPv4"]`. Count
                // IPv occurrences as a cheap length probe.
                let n = fams.matches("IPv").count();
                if n > 1 {
                    bad.push(format!("{ns}/{name} ({fams})"));
                }
            }
        }
        if bad.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "Service(s) with multi-family ipFamilies on single-stack cluster: {bad:?}"
            )))
        }
    }
}
