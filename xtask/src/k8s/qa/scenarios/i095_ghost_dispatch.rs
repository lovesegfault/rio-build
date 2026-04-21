//! I-095/097: dispatch hot-loops on dead ephemeral executors.
//!
//! When an ephemeral builder/fetcher exited, its assignment channel
//! closed but the executor record stayed dispatchable until heartbeat
//! timeout. Every dispatch tick re-picked the dead executor for every
//! Ready drv → "channel closed" log spam, mailbox saturation, 99.25%
//! wasted assignments. Fix landed at c89ace66 (mark undispatchable
//! immediately on send failure).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct GhostDispatch;

#[async_trait]
impl Scenario for GhostDispatch {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i095-ghost-dispatch",
            i_ref: Some(95),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Churn ephemeral builders by submitting a fan of short builds:
        // each completes in ~5s, builder exits, scheduler must NOT
        // re-pick it. Five staggered submissions to maximize the
        // "completed-builder still in dispatch table" window.
        let mut bg = Vec::new();
        for i in 0..5 {
            bg.push(ctx.nix_build_via_gateway_bg(0, &format!("i095-{i}"), 5, 1));
            sleep(Duration::from_secs(2)).await;
        }
        for h in bg {
            h.await??;
        }

        // The I-095 signature: leader log floods with "failed to send
        // assignment to worker" / "channel closed". Check the last 90s.
        let leader = ctx.scheduler_leader().await?;
        let logs = ctx.kubectl(&["-n", crate::k8s::NS, "logs", &leader, "--since=90s"])?;
        let hits = logs
            .lines()
            .filter(|l| l.contains("failed to send assignment") && l.contains("channel closed"))
            .count();

        // The fix doesn't make this zero (one closed-channel per dead
        // executor is expected and harmless); the bug was hot-loop spam.
        if hits > 20 {
            return Ok(Verdict::Fail(format!(
                "{hits} 'channel closed' assignment failures in 90s \
                 (>20 indicates dispatch hot-loop on dead executors)"
            )));
        }
        Ok(Verdict::Pass)
    }
}
