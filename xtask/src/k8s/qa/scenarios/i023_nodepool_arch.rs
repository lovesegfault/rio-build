//! I-023: NodePool without arch requirement provisions wrong-arch nodes.
//!
//! Widened category-only requirements (`category In [t,m]`) provisioned
//! `t4g.medium` (Graviton) for an x86 pool. Karpenter family/category
//! filters don't constrain arch — every NodePool MUST carry an explicit
//! `kubernetes.io/arch` requirement.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct NodepoolArch;

#[async_trait]
impl Scenario for NodepoolArch {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i023-nodepool-arch",
            i_ref: Some(23),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let pools = match ctx.kubectl(&[
            "get",
            "nodepools.karpenter.sh",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name}={.spec.template.spec.requirements[?(@.key==\"kubernetes.io/arch\")].key}{\"\\n\"}{end}",
        ]) {
            Ok(o) => o,
            Err(_) => return Ok(Verdict::Skip("no Karpenter NodePool CRD (k3s?)".into())),
        };

        let missing: Vec<_> = pools
            .lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(_, v)| v.trim().is_empty())
            .map(|(name, _)| name.to_string())
            .collect();

        if missing.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "NodePool(s) missing kubernetes.io/arch requirement: {missing:?} \
                 — category/family filters don't constrain arch (Graviton risk)"
            )))
        }
    }
}
