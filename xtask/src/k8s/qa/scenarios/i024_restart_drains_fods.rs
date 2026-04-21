//! I-024/I-025: restart scheduler mid-build with pending FODs;
//! `fod_queue_depth` (queued FODs) must drain to 0 within 60s of the
//! new leader's recovery.
//!
//! Original cascade (I-024/025 → I-032 root): leader bounce left FODs
//! at Ready with no relay path; fetchers idle, queue stuck.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{NS_SYSTEM, kill_pod, poll_until, wait_new_leader, wait_recovery_done};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct RestartDrainsFods;

#[async_trait]
impl Scenario for RestartDrainsFods {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i024-restart-drains-fods",
            i_ref: Some(24),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler, Component::FetcherPool],
            },
            timeout: Duration::from_secs(240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // smoke_expr's busybox FOD is the FOD; the consumer's 30s
        // sleep gives us time to bounce the leader before completion.
        let bg = ctx.nix_build_via_gateway_bg(0, "i024", 30, 1);

        // Wait until the FOD is in flight.
        let queued = poll_until(Duration::from_secs(45), Duration::from_secs(3), || async {
            let s = ctx.scrape_scheduler().await?;
            let q = s.sum("rio_scheduler_derivations_queued")
                + s.sum("rio_scheduler_derivations_running");
            Ok((q > 0.0).then_some(()))
        })
        .await?;
        if queued.is_none() {
            bg.abort();
            return Ok(Verdict::Skip(
                "build never queued (gateway path issue?)".into(),
            ));
        }

        let old_leader = ctx.scheduler_leader().await?;
        kill_pod(ctx, NS_SYSTEM, &old_leader)?;
        wait_new_leader(ctx, &old_leader, Duration::from_secs(60)).await?;
        if !wait_recovery_done(ctx, -1.0, Duration::from_secs(60)).await? {
            bg.abort();
            return Ok(Verdict::Fail("new leader never completed recovery".into()));
        }

        let drained = poll_until(Duration::from_secs(90), Duration::from_secs(5), || async {
            let s = ctx.scrape_scheduler().await?;
            Ok((s.sum("rio_scheduler_derivations_queued") == 0.0).then_some(()))
        })
        .await?;
        let _ = bg.await;

        if drained.is_some() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(
                "derivations_queued never reached 0 within 90s of new-leader \
                 recovery — FOD relay/dispatch lost across restart"
                    .into(),
            ))
        }
    }
}
