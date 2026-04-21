//! I-205: NodePool selector resolves to zero instance types.
//!
//! `mid-nvme-x86` had `gen-7 amd64 c/m/r ∩ nvme = ∅`. NodePool reports
//! `Ready=True` (lies); the only signal is the karpenter-controller log
//! line "filtered out all instance types". For each NodePool: assert
//! either ≥1 NodeClaim exists OR the controller log doesn't have the
//! filtered-out signature for that pool.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct NodepoolSchedulable;

#[async_trait]
impl Scenario for NodepoolSchedulable {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i205-nodepool-schedulable",
            i_ref: Some(205),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(60),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // EKS-only — k3s has no Karpenter. Skip rather than error.
        let pools_out = match ctx.kubectl(&[
            "get",
            "nodepools.karpenter.sh",
            "-o",
            "jsonpath={.items[*].metadata.name}",
        ]) {
            Ok(o) => o,
            Err(_) => return Ok(Verdict::Skip("no Karpenter NodePool CRD (k3s?)".into())),
        };
        let pools: Vec<_> = pools_out.split_whitespace().collect();
        if pools.is_empty() {
            // CRD exists (line above succeeded) but zero NodePools — on
            // EKS that means deploy didn't render karpenter.yaml, which
            // is a real misconfiguration (no builders can ever spawn).
            return Ok(Verdict::Fail(
                "Karpenter NodePool CRD present but zero NodePools defined \
                 — karpenter.yaml not rendered or all deleted"
                    .into(),
            ));
        }

        let controller_logs = ctx
            .kubectl(&[
                "-n",
                "kube-system",
                "logs",
                "deploy/karpenter",
                "--since=10m",
            ])
            .unwrap_or_default();

        let mut empty = Vec::new();
        for p in &pools {
            let claims = ctx.kubectl(&[
                "get",
                "nodeclaims",
                "-l",
                &format!("karpenter.sh/nodepool={p}"),
                "-o",
                "jsonpath={.items[*].metadata.name}",
            ])?;
            if !claims.trim().is_empty() {
                continue;
            }
            // Zero NodeClaims is fine if nothing is pending for this
            // pool. The bug signature is the controller explicitly
            // saying it filtered everything out.
            if controller_logs.contains("filtered out all instance types")
                && controller_logs.contains(p)
            {
                empty.push(p.to_string());
            }
        }

        if empty.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "NodePool(s) with empty instance-type intersection: {empty:?} \
                 — selector constraints resolve to zero SKUs"
            )))
        }
    }
}
