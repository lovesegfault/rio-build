//! I-049: 13.4s NotFound-retry sleep tax per build.
//!
//! `retry_notfound_once!()` slept 200ms × ~67 sandbox-temp probes per
//! build. Symptom: trivial build wall-clock >>15s. Generous threshold
//! (60s) covers cold-start ephemeral spawn; still catches the
//! order-of-magnitude regression.

use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct DispatchLatency;

const THRESHOLD: Duration = Duration::from_secs(60);

#[async_trait]
impl Scenario for DispatchLatency {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i049-dispatch-latency",
            i_ref: Some(49),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // The threshold is meaningless under phase-1 contention (10+
        // scenarios submitting concurrently → cold-start + queuing →
        // 95s observed on a 5s build). Warm a builder first so the
        // measured build is dispatch+execute only, not provision.
        ctx.nix_build_via_gateway(0, "i049-warmup", 3, 1).await?;
        let start = Instant::now();
        ctx.nix_build_via_gateway(0, "i049", 5, 1).await?;
        let elapsed = start.elapsed();
        if elapsed < THRESHOLD {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "trivial 5s build took {elapsed:?} (threshold {THRESHOLD:?}) \
                 — NotFound-retry sleep tax or dispatch stall"
            )))
        }
    }
}
