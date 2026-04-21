//! I-050: scheduler `/metrics` port-forward target wrong → silent
//! "error sending request" on every poll.
//!
//! `scrape_scheduler` succeeding (and returning a non-empty body) IS
//! the assertion — i050 was the symptom of the metrics port not being
//! exposed where xtask expected it. This scenario also checks the body
//! contains at least one `rio_scheduler_` series so a 200-with-empty-
//! body doesn't pass.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct MetricsReachable;

#[async_trait]
impl Scenario for MetricsReachable {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i050-metrics-reachable",
            i_ref: Some(50),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let scrape = ctx.scrape_scheduler().await?;
        if scrape.any_with_prefix("rio_scheduler_") {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(
                "scheduler /metrics reachable but no rio_scheduler_* series \
                 — exporter started with empty registry?"
                    .into(),
            ))
        }
    }
}
