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
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Precondition: at least one builder connected, otherwise the
        // disconnect counter has nothing to count and we'd false-Pass.
        let builders = ctx.running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?;
        if builders.is_empty() {
            return Ok(Verdict::Skip(
                "no running builder pods — nothing to disconnect".into(),
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
