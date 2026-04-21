//! I-048c: assert the fault injector actually injects faults.
//!
//! Guards against the silent-no-op failure mode (I-056 class) where the
//! injector reports success but traffic is unaffected — the exact risk
//! that ruled out Chaos Mesh / Litmus on the v6-only cluster. This is
//! also the regression check for chaos.rs's own ip6tables path.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::{Instant, sleep};

use crate::k8s::chaos::{self, ChaosFrom, ChaosKind, ChaosTarget};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BlackholeSelfTest;

const METRIC: &str = "rio_scheduler_worker_disconnects_total";
const KEEPALIVE_WINDOW: Duration = Duration::from_secs(45);

#[async_trait]
impl Scenario for BlackholeSelfTest {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i048c-blackhole-self-test",
            i_ref: Some(48),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler, Component::BuilderPool],
            },
            timeout: Duration::from_secs(240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Need ≥1 builder CONNECTED to the leader (not just running) so
        // there's something to disconnect. After a prior phase-2
        // leader-kill, builders may be mid-reconnect; submit a warmup
        // build to drive a fresh connection rather than Skip.
        let bg = ctx.nix_build_via_gateway_bg(0, "i048c-warmup", 90, 1);
        let connected =
            super::common::poll_until(Duration::from_secs(90), Duration::from_secs(3), || async {
                let n = ctx
                    .scrape_scheduler()
                    .await?
                    .sum("rio_scheduler_workers_active");
                Ok((n > 0.0).then_some(n))
            })
            .await?;
        if connected.is_none() {
            bg.abort();
            return Ok(Verdict::Fail(
                "no executor connected to scheduler-leader within 90s of \
                 submitting a build — dispatch/spawn-intent path broken"
                    .into(),
            ));
        }

        let before = ctx.scrape_scheduler().await?.sum(METRIC);

        let dir = crate::sh::repo_root().join(".stress-test/chaos");
        std::fs::create_dir_all(&dir)?;
        if let Err(e) = chaos::remediate(&dir).await {
            tracing::warn!("stale-chaos remediation: {e:#}");
        }
        // Spawn the blackhole; it blocks for `duration` then cleans up.
        // We poll the metric concurrently and abort early on increment.
        let chaos_fut = chaos::run(
            &dir,
            ChaosKind::Blackhole,
            ChaosTarget::SchedulerLeader,
            ChaosFrom::AllWorkers,
            KEEPALIVE_WINDOW + Duration::from_secs(15),
        );
        tokio::pin!(chaos_fut);

        let deadline = Instant::now() + KEEPALIVE_WINDOW;
        let mut incremented = false;
        loop {
            tokio::select! {
                r = &mut chaos_fut => { r?; break; }
                _ = sleep(Duration::from_secs(5)) => {
                    if !incremented && Instant::now() < deadline {
                        let now = ctx.scrape_scheduler().await?.sum(METRIC);
                        if now > before {
                            incremented = true;
                        }
                    }
                }
            }
        }

        bg.abort();
        if incremented {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{METRIC} did not increment within {KEEPALIVE_WINDOW:?} \
                 — ip6tables blackhole likely a no-op (Cilium datapath bypass?)"
            )))
        }
    }
}
