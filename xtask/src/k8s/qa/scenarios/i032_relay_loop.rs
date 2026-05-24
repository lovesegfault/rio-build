//! I-032: relay pump loop didn't watch `target.changed()` → completion
//! messages lost. The user-visible symptom is `nix build --store
//! ssh-ng://…` hanging with no log output despite the builder having
//! produced lines. Assert: a build that echoes a known marker actually
//! surfaces that marker in scheduler-leader logs (the relay path).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct RelayLoop;

#[async_trait]
impl Scenario for RelayLoop {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i032-relay-loop",
            i_ref: Some(32),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // SMOKE_EXPR echoes the tag (`echo @TAG@`) before the sleep.
        // Unique tag so we don't match a prior scenario's output.
        let tag = format!(
            "i032-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros()
        );
        ctx.nix_build_via_gateway(0, &tag, 3, 1).await?;

        // The tag is embedded in the derivation NAME
        // (`rio-smoke-<tag>-…`), so it appears in the scheduler
        // leader's dispatch/completion trace lines for the build. That
        // is the control-plane evidence I-032 actually protects: the
        // relay pump delivering the WorkAssignmentAck/CompletionReport
        // (the messages that were lost when the pump didn't watch
        // `target.changed()`). The builder's log *content* no longer
        // transits the scheduler at all — it goes builder → rio-store
        // AppendLog and is read back via TailLog (asserted by the
        // vm-observability/log-service scenarios).
        // TODO: once QaCtx exposes the built .drv path, additionally
        // assert `rio-cli logs <drv>` (against RIO_STORE_ADDR) contains
        // the tag — the user-facing "my build's output is readable"
        // property for the post-cutover data plane.
        let leader = ctx.scheduler_leader().await?;
        let logs = ctx.kubectl(&["-n", crate::k8s::NS, "logs", &leader, "--since=60s"])?;
        if logs.contains(&tag) {
            Ok(Verdict::Pass)
        } else {
            // Fail-closed: if the tag isn't in scheduler logs, the
            // relay either dropped the completion or scheduler logging
            // changed. Distinguishable via the build itself succeeding
            // (it did, or `?` above would've propagated).
            Ok(Verdict::Fail(format!(
                "build completed but tag '{tag}' absent from scheduler-leader \
                 logs — the relay pump may not be forwarding builder \
                 control messages (the drv name appears in dispatch/\
                 completion traces)"
            )))
        }
    }
}
