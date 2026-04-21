//! I-039: store-pod restart mid-build must NOT kill the build with
//! EIO. FUSE retries the gRPC channel + store has PDB-backed
//! `replicas≥2`, so a build should ride through one pod loss.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{first_pod, kill_pod};
use crate::k8s::NS_STORE;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct StoreKillSurvives;

#[async_trait]
impl Scenario for StoreKillSurvives {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i039-store-kill-survives",
            i_ref: Some(39),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Store],
            },
            timeout: Duration::from_secs(240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let stores = ctx.running_pods(NS_STORE, "app.kubernetes.io/name=rio-store")?;
        if stores.len() < 2 {
            // Chart deploys store.replicas≥2 on EKS for HA. <2 is a
            // real misconfiguration (any store kill = total outage),
            // not a precondition to skip.
            return Ok(Verdict::Fail(format!(
                "only {} store replica(s) Running — store HA broken \
                 (chart sets replicas≥2; deploy or readiness issue)",
                stores.len()
            )));
        }

        // 60s build — long enough to kill mid-way, short enough for
        // the scenario timeout.
        let bg = ctx.nix_build_via_gateway_bg(0, "i039", 60, 1);
        tokio::time::sleep(Duration::from_secs(10)).await;

        let victim = first_pod(ctx, NS_STORE, "rio-store")?;
        kill_pod(ctx, NS_STORE, &victim)?;

        match bg.await? {
            Ok(()) => Ok(Verdict::Pass),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("Input/output error") || msg.to_lowercase().contains("eio") {
                    Ok(Verdict::Fail(format!(
                        "build died with EIO after store-pod kill — FUSE retry / PDB regression: {msg}"
                    )))
                } else {
                    Ok(Verdict::Fail(format!(
                        "build failed after store-pod kill (not EIO, but still a regression): {msg}"
                    )))
                }
            }
        }
    }
}
