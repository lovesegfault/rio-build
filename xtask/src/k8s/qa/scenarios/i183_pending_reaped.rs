//! I-183: cancelling a build must reap Pending ephemeral Jobs.
//!
//! Submit a build → Jobs queued; cancel before they schedule →
//! controller's reconcile should delete excess Pending Jobs;
//! `rio_controller_ephemeral_jobs_reaped_total` increments.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{poll_until, scrape_controller};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct PendingReaped;

const METRIC: &str = "rio_controller_ephemeral_jobs_reaped_total";

#[async_trait]
impl Scenario for PendingReaped {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i183-pending-reaped",
            i_ref: Some(183),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Controller],
            },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let before = scrape_controller(ctx).await?.sum(METRIC);

        let bg = ctx.nix_build_via_gateway_bg(0, "i183", 120, 1);
        // Let the controller spawn at least one Job.
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Cancel: aborting the bg JoinHandle drops the ssh client
        // → cancel-on-disconnect (r[sched.cancel.on-disconnect]).
        bg.abort();

        // Observed 39s/59s/93s/106s across runs — variance is the
        // controller's reconcile-tick interval × concurrent phase-2
        // scheduler-kills delaying GetSpawnIntents. 150s budget.
        let reaped = poll_until(Duration::from_secs(150), Duration::from_secs(5), || async {
            let now = scrape_controller(ctx).await?.sum(METRIC);
            Ok((now > before).then_some(now))
        })
        .await?;

        match reaped {
            Some(n) => {
                tracing::info!("{METRIC} {before} → {n}");
                Ok(Verdict::Pass)
            }
            None => Ok(Verdict::Fail(format!(
                "{METRIC} did not increment within 150s of cancel — Pending \
                 Jobs not reaped (controller still trying to provision nodes)"
            ))),
        }
    }
}
