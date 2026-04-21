//! I-114: liveness probe kills CPU-pegged builders mid-build.
//!
//! Builder probe had a 1s timeout; under full CPU load (the build
//! itself), kubelet timed out the probe → SIGTERM → EIO to the build.
//! The fix raised/removed the probe; this scenario asserts a long
//! CPU-bound build completes without builder pod restarts.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct LivenessKill;

const BUILD_SECS: u32 = 120;

#[async_trait]
impl Scenario for LivenessKill {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i114-liveness-kill",
            i_ref: Some(114),
            isolation: Isolation::Tenant { count: 1 },
            // build + cold-start headroom
            timeout: Duration::from_secs((BUILD_SECS as u64) + 240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Snapshot builder pod restarts before. SMOKE_EXPR's `read -t N
        // /dev/zero` is wall-clock, not CPU-bound — close enough for the
        // probe-timeout case (kubelet probe contends for the same node
        // CPU). A true stress-ng workload would be tighter.
        let before = builder_restarts(ctx)?;

        ctx.nix_build_via_gateway("i114", BUILD_SECS, 1).await?;

        let after = builder_restarts(ctx)?;
        if after > before {
            return Ok(Verdict::Fail(format!(
                "builder pod restarts {before}→{after} during {BUILD_SECS}s build \
                 — liveness/startup probe likely killing under load"
            )));
        }
        Ok(Verdict::Pass)
    }
}

fn builder_restarts(ctx: &QaCtx) -> Result<u32> {
    let out = ctx.kubectl(&[
        "-n",
        QaCtx::NS_BUILDERS,
        "get",
        "pods",
        "-l",
        QaCtx::BUILDER_LABEL,
        "-o",
        "jsonpath={range .items[*]}{.status.containerStatuses[0].restartCount} {end}",
    ])?;
    Ok(out
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .sum())
}
