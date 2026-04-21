//! I-013/I-036: SSH client disconnect → build cancels server-side
//! within 60s (not on next-dispatch).
//!
//! `cancel-on-disconnect` is the default per design. Aborting the
//! background-build task drops the port-forward guard → gateway sees
//! SSH EOF → should cancel.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::time::{Instant, sleep};

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct CancelOnDisconnect;

#[derive(Deserialize)]
struct Build {
    #[serde(default)]
    status: String,
}

#[async_trait]
impl Scenario for CancelOnDisconnect {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i013-cancel-on-disconnect",
            i_ref: Some(13),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Long enough that it can't complete before we abort.
        let bg = ctx.nix_build_via_gateway_bg(0, "i013", 120, 1);
        sleep(Duration::from_secs(15)).await;

        // Dropping the handle drops the port-forward → SSH EOF.
        bg.abort();

        // Within 60s, no build should be left Running. The just-
        // submitted build's id isn't directly observable (smoke_build
        // doesn't return it), so check the global invariant: zero
        // Running builds 60s after disconnect.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let builds: Vec<Build> = ctx.cli_json(&["builds"]).unwrap_or_default();
            let running = builds.iter().filter(|b| b.status == "Running").count();
            if running == 0 {
                return Ok(Verdict::Pass);
            }
            if Instant::now() >= deadline {
                return Ok(Verdict::Fail(format!(
                    "{running} build(s) still Running 60s after SSH disconnect \
                     — cancel-on-disconnect not firing"
                )));
            }
            sleep(Duration::from_secs(5)).await;
        }
    }
}
