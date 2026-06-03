//! I-023: NodePool without arch requirement provisions wrong-arch nodes.
//!
//! Widened category-only requirements (`category In [t,m]`) provisioned
//! `t4g.medium` (Graviton) for an x86 pool. Karpenter family/category
//! filters don't constrain arch.
//!
//! NOT every NodePool needs arch: `rio-general` and `rio-nodeclaim-shim`
//! are intentionally arch-agnostic — control-plane images are multi-
//! arch manifest lists (ECR `{sha}` → `{sha}-{amd64,arm64}`), so
//! Graviton is a cost/availability optimization, not a correctness
//! risk. §13c/§13e: builder AND fetcher NodePools (incl. the static
//! metal and `rio-fetcher` pools) are gone — §13b NodeClaims carry
//! arch from the hwClass `requirements`, so the only static NodePools
//! left are the arch-agnostic ones above.

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
            exercises: crate::exercises!(),
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

        // Arch-agnostic by design (multi-arch images / never provisions).
        const ARCH_AGNOSTIC: &[&str] = &["rio-general", "rio-nodeclaim-shim"];

        let missing: Vec<_> = pools
            .lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(name, v)| v.trim().is_empty() && !ARCH_AGNOSTIC.contains(name))
            .map(|(name, _)| name.to_string())
            .collect();

        if missing.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "NodePool(s) missing kubernetes.io/arch requirement: {missing:?} \
                 — category/family filters don't constrain arch (Graviton risk). \
                 Intentionally arch-agnostic pools: {ARCH_AGNOSTIC:?}"
            )))
        }
    }
}
