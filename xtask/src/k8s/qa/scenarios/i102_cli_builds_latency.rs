//! I-102: `rio-cli builds` slow with many builds in DB.
//!
//! Threshold is generous (2s — original target was 500ms) because the
//! cli runs through a port-forward; the assert catches order-of-
//! magnitude regressions (N+1 queries, missing index), not
//! micro-latency drift.

use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct CliBuildsLatency;

const THRESHOLD: Duration = Duration::from_secs(2);

#[async_trait]
impl Scenario for CliBuildsLatency {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i102-cli-builds-latency",
            i_ref: Some(102),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let start = Instant::now();
        let _: serde_json::Value = ctx.cli_json(&["builds"])?;
        let elapsed = start.elapsed();
        if elapsed < THRESHOLD {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "rio-cli builds took {elapsed:?} (threshold {THRESHOLD:?}) \
                 — N+1 query or missing index regression?"
            )))
        }
    }
}
