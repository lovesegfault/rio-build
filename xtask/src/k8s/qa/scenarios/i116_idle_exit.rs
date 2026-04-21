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

use super::common::{any_builder, poll_until};
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
        // bg build that outlives the poll so the builder pod is
        // observable WHILE running (the original blocking 3s build
        // could finish + idle-exit before we sample).
        let bg = ctx.nix_build_via_gateway_bg(0, "i116", 15, 1);
        let pod = poll_until(Duration::from_secs(90), Duration::from_secs(3), || async {
            any_builder(ctx)
        })
        .await?;
        let Some(pod) = pod else {
            bg.abort();
            return Ok(Verdict::Fail(
                "no running builder pod within 90s of submitting a build \
                 — dispatch/spawn-intent path broken"
                    .into(),
            ));
        };
        // Let the build complete so the idle clock starts.
        let _ = bg.await;

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
