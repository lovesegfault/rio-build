//! I-037: cordoned-but-never-drained nodes holding control-plane pods.
//!
//! Three AL2023 nodes were `SchedulingDisabled` for 6 days while
//! carrying BOTH karpenter replicas, BOTH coredns replicas,
//! cert-manager, and external-secrets. One node-loss away from a
//! cluster-wide DNS+autoscaling outage.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct CordonedControlPlane;

const CRITICAL_PREFIXES: &[&str] = &[
    "karpenter",
    "coredns",
    "cert-manager",
    "external-secrets",
    "cilium-operator",
];

#[async_trait]
impl Scenario for CordonedControlPlane {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i037-cordoned-control-plane",
            i_ref: Some(37),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let cordoned = ctx.kubectl(&[
            "get",
            "nodes",
            "-o",
            "jsonpath={.items[?(@.spec.unschedulable==true)].metadata.name}",
        ])?;
        let cordoned: Vec<_> = cordoned.split_whitespace().collect();
        if cordoned.is_empty() {
            return Ok(Verdict::Pass);
        }

        let mut at_risk = Vec::new();
        for node in &cordoned {
            let pods = ctx.kubectl(&[
                "get",
                "pods",
                "-A",
                "--field-selector",
                &format!("spec.nodeName={node}"),
                "-o",
                "jsonpath={.items[*].metadata.name}",
            ])?;
            for p in pods.split_whitespace() {
                if CRITICAL_PREFIXES.iter().any(|pre| p.starts_with(pre)) {
                    at_risk.push(format!("{node}:{p}"));
                }
            }
        }

        if at_risk.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "cordoned node(s) still carry control-plane workloads (drain them): {at_risk:?}"
            )))
        }
    }
}
