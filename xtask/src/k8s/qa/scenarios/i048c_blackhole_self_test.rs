//! I-048c: assert the fault injector actually injects faults.
//!
//! Guards against the silent-no-op failure mode (I-056 class) where the
//! injector reports success but traffic is unaffected — the exact risk
//! that ruled out Chaos Mesh / Litmus on the v6-only cluster. This is
//! also the regression check for chaos.rs's own ip6tables path.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

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
        // chaos::run includes pod-create + wait-Running (~10-20s) before
        // the ip6tables rules go in. The previous deadline counted from
        // call time, so the rules might only be active for ~25s of a
        // 45s window — under h2 keepalive (30+10s). Instead: run the
        // blackhole for KEEPALIVE_WINDOW + 30s startup-slack and poll
        // the metric for the WHOLE duration; any increment is Pass.
        let chaos_dur = KEEPALIVE_WINDOW + Duration::from_secs(30);
        let chaos_fut = chaos::run(
            &dir,
            ChaosKind::Blackhole,
            ChaosTarget::SchedulerLeader,
            ChaosFrom::AllWorkers,
            chaos_dur,
        );
        tokio::pin!(chaos_fut);

        let mut incremented = false;
        let mut samples = vec![before];
        loop {
            tokio::select! {
                r = &mut chaos_fut => { r?; break; }
                _ = sleep(Duration::from_secs(5)) => {
                    if !incremented {
                        // Swallow scrape errors (port-forward can blip
                        // during chaos) instead of `?`-ing out and
                        // losing the chaos cleanup.
                        if let Ok(s) = ctx.scrape_scheduler().await {
                            let now = s.sum(METRIC);
                            samples.push(now);
                            if now > before {
                                incremented = true;
                            }
                        }
                    }
                }
            }
        }
        // The scheduler-side h2 keepalive is 30s interval + 20s timeout
        // = ~50s detect. If the worker reconnected AFTER the blackhole
        // lifted (it has client-side 30+10s) and the scheduler's old
        // stream is still half-open, the disconnect counter bumps when
        // the new stream replaces the old. Poll a final time post-chaos.
        if !incremented {
            sleep(Duration::from_secs(10)).await;
            if let Ok(s) = ctx.scrape_scheduler().await {
                let now = s.sum(METRIC);
                samples.push(now);
                incremented = now > before;
            }
        }

        bg.abort();
        if incremented {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{METRIC} did not increment during {chaos_dur:?} blackhole \
                 (samples: {samples:?}) — ip6tables likely a no-op \
                 (Cilium datapath bypass?) or scheduler-side keepalive \
                 not detecting"
            )))
        }
    }
}
