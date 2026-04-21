//! I-116: idle ephemeral builders never exit (waited until
//! activeDeadlineSeconds=3600).
//!
//! Fix added a 120s idle-timeout select! arm. Check: spawn a short
//! build (forces at least one builder), record its pod, wait
//! 120s+slack with no further work, assert that pod is gone.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

use super::common::any_builder;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct IdleExit;

const IDLE_WINDOW: Duration = Duration::from_secs(150);

#[async_trait]
impl Scenario for IdleExit {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i116-idle-exit",
            i_ref: Some(116),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(300),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        ctx.nix_build_via_gateway(0, "i116", 3, 1).await?;
        let Some(pod) = any_builder(ctx)? else {
            return Ok(Verdict::Skip(
                "no running builder after build — already exited".into(),
            ));
        };

        sleep(IDLE_WINDOW).await;

        let still = ctx.running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?;
        if still.contains(&pod) {
            Ok(Verdict::Fail(format!(
                "builder {pod} still Running after {IDLE_WINDOW:?} idle \
                 — idle-timeout select! arm not firing"
            )))
        } else {
            Ok(Verdict::Pass)
        }
    }
}
