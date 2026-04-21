//! I-033/I-048b: SIGKILL scheduler-leader, assert no zombie executors
//! after recovery.
//!
//! The original failure mode: executors heartbeat-alive but their
//! BuildExecution stream was attached to the dead leader. They appear
//! in `cli workers` (PG `last_seen` fresh) but the actor has no stream
//! → never dispatched. Signature: `DebugListExecutors` shows
//! `has_stream=false` for entries that `ListExecutors` claims live.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{NS_SYSTEM, kill_pod, poll_until, wait_new_leader, wait_recovery_done};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct ZombieExecutors;

#[async_trait]
impl Scenario for ZombieExecutors {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i033-zombie-executors",
            i_ref: Some(33),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler],
            },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Precondition: at least one executor connected so the assert
        // is meaningful. Spawn a quick build to bring one up.
        let bg = ctx.nix_build_via_gateway_bg(0, "i033-warmup", 30, 1);
        let warm = poll_until(Duration::from_secs(60), Duration::from_secs(3), || async {
            let n = ctx
                .scrape_scheduler()
                .await?
                .sum("rio_scheduler_workers_active");
            Ok((n > 0.0).then_some(()))
        })
        .await?;
        if warm.is_none() {
            bg.abort();
            return Ok(Verdict::Fail(
                "no worker connected to scheduler-leader within 60s of \
                 submitting a build — dispatch/spawn-intent path broken"
                    .into(),
            ));
        }

        let old_leader = ctx.scheduler_leader().await?;
        let recovery_before = ctx
            .scrape_scheduler()
            .await?
            .labeled("rio_scheduler_recovery_total", "outcome", "success")
            .unwrap_or(0.0);

        kill_pod(ctx, NS_SYSTEM, &old_leader)?;
        let _ = wait_new_leader(ctx, &old_leader, Duration::from_secs(60)).await?;
        // recovery_before was scraped from the OLD leader; the new
        // leader's counter starts at 0, so "after > before" only works
        // if both are summed across replicas. They're not — but the new
        // leader's first success is >0 which is > stale-before only if
        // before==0. Safer: wait for `> 0` on the new leader directly.
        let _ = recovery_before;
        if !wait_recovery_done(ctx, -1.0, Duration::from_secs(60)).await? {
            bg.abort();
            return Ok(Verdict::Fail(
                "new leader never completed recovery within 60s".into(),
            ));
        }

        // Give workers ~45s to reconnect (h2 keepalive 30s + 10s + slack).
        tokio::time::sleep(Duration::from_secs(45)).await;
        bg.abort();

        // The DebugListExecutors RPC is exposed via `rio-cli workers
        // --actor` (not a separate subcommand). JSON shape is
        // DebugListExecutorsResponse: {"executors":[{executor_id,
        // has_stream, ...}]}. The original CliCtx's port-forward points
        // at the leader we just killed → transport error. Re-open.
        #[derive(serde::Deserialize)]
        struct DebugExecutor {
            executor_id: String,
            has_stream: bool,
        }
        #[derive(serde::Deserialize)]
        struct DebugList {
            executors: Vec<DebugExecutor>,
        }
        let cli2 = crate::k8s::eks::smoke::CliCtx::open(&ctx.kube, 0, 0).await?;
        let out = cli2.run(&["--json", "workers", "--actor"])?;
        let dl: DebugList = serde_json::from_str(&out)
            .map_err(|e| anyhow::anyhow!("workers --actor json: {e}: {out}"))?;
        let zombies: Vec<_> = dl
            .executors
            .iter()
            .filter(|e| !e.has_stream)
            .map(|e| e.executor_id.clone())
            .collect();

        if zombies.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{} zombie executors (has_stream=false) after leader SIGKILL+recovery: {zombies:?}",
                zombies.len()
            )))
        }
    }
}
