//! I-064/I-175: gateway rollout-restart must NOT kill active SSH
//! sessions before `sessionDrainSecs`. The custom accept-loop
//! (r[gw.conn.session-drain]) decouples session lifetime from the
//! server-future drop.
//!
//! Probe: start a 60s build, rollout-restart the gateway, assert the
//! build's `nix build` process completes (its ssh-ng connection rode
//! the drain through to the new pod).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::NS_SYSTEM;
use crate::k8s::client as kube;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct GatewayDrain;

#[async_trait]
impl Scenario for GatewayDrain {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i064-gateway-drain",
            i_ref: Some(64),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Gateway],
            },
            timeout: Duration::from_secs(240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let bg = ctx.nix_build_via_gateway_bg(0, "i064", 60, 1);
        // Let the SSH session establish + DAG land.
        tokio::time::sleep(Duration::from_secs(8)).await;

        // rollout-restart (NOT grace-0 kill) — that's the deploy path
        // I-064 covers. With maxSurge:1/maxUnavailable:0 the new pod
        // is up before the old terminates; the OLD pod's SIGTERM
        // triggers session-drain.
        kube::rollout_restart(&ctx.kube, NS_SYSTEM, "rio-gateway").await?;

        match bg.await? {
            Ok(()) => Ok(Verdict::Pass),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("Nix daemon disconnected")
                    || msg.contains("Broken pipe")
                    || msg.contains("Connection reset")
                {
                    Ok(Verdict::Fail(format!(
                        "ssh-ng session died on gateway rollout-restart — \
                         session-drain not holding: {msg}"
                    )))
                } else {
                    Ok(Verdict::Fail(format!(
                        "build failed during gateway rollout (other cause): {msg}"
                    )))
                }
            }
        }
    }
}
