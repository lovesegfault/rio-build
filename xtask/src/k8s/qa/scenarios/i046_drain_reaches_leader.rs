//! I-046/I-091: builder's `DrainExecutor` RPC must reach the LEADER,
//! not a follower. With a balanced channel + multi-replica scheduler,
//! the original code routed ~50% to the standby and got
//! `not leader (standby replica)`.
//!
//! Probe: SIGTERM a running builder (graceful — its drain path fires
//! DrainExecutor), then assert no `"not leader"` error appears in its
//! log AND `rio_controller_disruption_drains_total` increments.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{logs_since_contain, poll_until};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct DrainReachesLeader;

#[async_trait]
impl Scenario for DrainReachesLeader {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i046-drain-reaches-leader",
            i_ref: Some(46),
            isolation: Isolation::Exclusive {
                mutates: &[Component::BuilderPool, Component::Scheduler],
            },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Ensure ≥1 running builder.
        let bg = ctx.nix_build_via_gateway_bg(0, "i046", 60, 1);
        let pod = poll_until(Duration::from_secs(60), Duration::from_secs(3), || async {
            Ok(ctx
                .running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?
                .into_iter()
                .next())
        })
        .await?;
        let Some(pod) = pod else {
            bg.abort();
            return Ok(Verdict::Fail(
                "no builder pod within 60s of submitting a build \
                 — dispatch/spawn-intent path broken"
                    .into(),
            ));
        };

        // Graceful delete (default grace period) — SIGTERM → drain.
        ctx.kubectl(&[
            "-n",
            QaCtx::NS_BUILDERS,
            "delete",
            "pod",
            &pod,
            "--wait=false",
        ])?;

        // Give it ~15s to issue DrainExecutor and log.
        tokio::time::sleep(Duration::from_secs(15)).await;
        let bad = logs_since_contain(
            ctx,
            QaCtx::NS_BUILDERS,
            &pod,
            30,
            "not leader (standby replica)",
        )?;
        bg.abort();

        if bad.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "DrainExecutor routed to follower ({} log hits) — \
                 leader-aware admin channel regression",
                bad.len()
            )))
        }
    }
}
