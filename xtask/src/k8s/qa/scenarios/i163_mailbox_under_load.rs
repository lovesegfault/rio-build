//! I-163: ClusterStatus times out under load → actor mailbox overload.
//!
//! Root cause was Heartbeat at 165ms × 290 workers; the actor command
//! queue grew unbounded. The mailbox-depth gauge is the canonical "is
//! the actor wedged?" signal: it should drain to ~0 after work
//! quiesces. Persistent depth > 0 with no inbound load = head-of-line
//! block somewhere.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::{Instant, sleep};

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct MailboxUnderLoad;

const METRIC: &str = "rio_scheduler_actor_mailbox_depth";

#[async_trait]
impl Scenario for MailboxUnderLoad {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i163-mailbox-under-load",
            i_ref: Some(163),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // One trivial build to nudge the actor; the assert is post-build.
        ctx.nix_build_via_gateway("i163", 5, 1).await?;

        // Mailbox should drain to 0 within 30s after the build finishes.
        // Tolerate transient spikes — fail only if EVERY sample over the
        // window stays high (the I-163 signature was sustained growth).
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut max_seen = 0.0_f64;
        while Instant::now() < deadline {
            let d = ctx.scrape_scheduler().await?.first(METRIC).unwrap_or(0.0);
            max_seen = max_seen.max(d);
            if d == 0.0 {
                return Ok(Verdict::Pass);
            }
            sleep(Duration::from_secs(3)).await;
        }
        Ok(Verdict::Fail(format!(
            "{METRIC} never drained to 0 in 30s (max seen: {max_seen}) \
             — actor head-of-line block or sustained command flood"
        )))
    }
}
