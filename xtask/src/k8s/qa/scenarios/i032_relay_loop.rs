//! I-032 (historical): the stream-era relay pump could silently drop
//! completion messages. The machinery is gone (pull-mode reports are
//! retried unaries), but the user-visible property is delivery-mode
//! independent and stays asserted: a build that echoes a known marker
//! surfaces that marker in scheduler-leader dispatch/completion traces.
//! The scenario also asserts the user-facing data-plane read-back: the
//! marker comes back from `rio-cli logs <drv>` via the store's TailLog.

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
            // 180 -> 240: the scenario gained a bounded 30s post-build
            // log read-back poll, and the qa scheduler hard-kills at
            // meta.timeout — budget for the tail, not the typical run.
            timeout: Duration::from_secs(240),
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
        let drv = ctx.nix_build_via_gateway(0, &tag, 3, 1).await?;

        // Control-plane evidence (FIRST, and unchanged: it greps
        // `--since=60s`, so the read-back poll below must not run before
        // it and push the dispatch lines out of that window). The tag is
        // embedded in the derivation NAME (`rio-smoke-<tag>-…`), so it
        // appears in the scheduler leader's dispatch/completion trace
        // lines for the build — the property I-032 actually protects:
        // the build's completion reaching the scheduler (pull-mode
        // ReportOutcome; historically the relay-pumped report, which is
        // what I-032 lost).
        let leader = ctx.scheduler_leader().await?;
        let logs = ctx.kubectl(&["-n", crate::k8s::NS, "logs", &leader, "--since=60s"])?;
        if !logs.contains(&tag) {
            // Fail-closed: if the tag isn't in scheduler logs, the
            // relay either dropped the completion or scheduler logging
            // changed. Distinguishable via the build itself succeeding
            // (it did, or `?` above would've propagated).
            return Ok(Verdict::Fail(format!(
                "build completed but tag '{tag}' absent from scheduler-leader \
                 logs — the completion/dispatch trace lines should carry \
                 the drv name"
            )));
        }

        // Data-plane evidence: the builder's log content goes builder →
        // rio-store AppendLog and is read back via LogService/TailLog.
        // Assert the user-facing read-back HERE, on the live cluster:
        // `rio-cli logs <drv>` (CliCtx::run exports RIO_STORE_ADDR to the
        // store tunnel; TailLog is the store's unauthenticated read-only
        // RPC; empty --exec-id resolves the latest execution). Poll: the
        // store returns NotFound until the drv_executions row is visible
        // (rio-cli then exits non-zero — treated as not-yet, since
        // poll_until propagates closure errors), and the final flush can
        // lag completion. The tag is the FIRST line the build echoes, so
        // it lands in the earliest chunk.
        let found =
            super::common::poll_until(Duration::from_secs(30), Duration::from_secs(3), || {
                let cli = ctx.cli.clone();
                let drv = drv.clone();
                let tag = tag.clone();
                async move {
                    match cli.run(&["logs", &drv]) {
                        Ok(out) if out.contains(&tag) => Ok(Some(())),
                        _ => Ok(None),
                    }
                }
            })
            .await?;

        if found.is_none() {
            // Self-diagnosing Fail: control-plane passed, data-plane did
            // not — name which leg broke and what the last read returned.
            let last = match ctx.cli.run(&["logs", &drv]) {
                Ok(out) => format!("logs returned {} bytes without the tag", out.len()),
                Err(e) => format!("rio-cli logs error: {e:#}"),
            };
            return Ok(Verdict::Fail(format!(
                "tag '{tag}' present in scheduler trace but absent from \
                 `rio-cli logs {drv}` after 30s — store log path \
                 (AppendLog→TailLog) lost the build output: {last}"
            )));
        }

        Ok(Verdict::Pass)
    }
}
