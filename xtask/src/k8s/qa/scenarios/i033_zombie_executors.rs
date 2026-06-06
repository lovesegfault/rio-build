//! I-033/I-048b: SIGKILL scheduler-leader, assert no zombie executors
//! after recovery.
//!
//! The original failure mode: executors heartbeat-alive but their
//! stream-era BuildExecution session was attached to the dead leader. They appear
//! in `cli workers` (PG `last_seen` fresh) but the actor has no stream
//! → never dispatched. Signature: `DebugListExecutors` shows
//! `has_stream=false` for entries that `ListExecutors` claims live.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{DebugList, NS_SYSTEM, kill_pod, wait_new_leader, wait_recovery_done};
use crate::k8s::eks::smoke::CliCtx;
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
            // +90s budget for phase2_settle_after_kill
            timeout: Duration::from_secs(270),
            exercises: crate::exercises!(),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Snapshot the actor's live executor set BEFORE the warmup so
        // the precondition gates on a *fresh* worker — not a leftover
        // from the previous Exclusive scenario whose gauge hasn't
        // re-ticked yet (the i048c full-QA precondition race,
        // 2026-05-13). Fresh CliCtx: i033 *itself* kills the leader,
        // so ctx.cli is about to go stale anyway, and a prior i024
        // run already left it stale.
        let cli_pre = CliCtx::open(&ctx.kube, 0, 0).await?;
        let before_execs = super::common::live_executor_ids(&cli_pre)?;
        let bg = ctx.nix_build_via_gateway_bg(0, "i033-warmup", 30, 1);
        let warm = super::common::poll_until(
            Duration::from_secs(90), // bumped 60→90: same headroom as i048c
            Duration::from_secs(3),
            || async {
                let now = super::common::live_executor_ids(&cli_pre)?;
                let fresh = now.difference(&before_execs).count();
                Ok((fresh > 0).then_some(fresh))
            },
        )
        .await?;
        if warm.is_none() {
            bg.abort();
            return Ok(Verdict::Fail(
                "no fresh executor (has_stream=true, not in pre-warmup \
                 DebugListExecutors snapshot) within 90s of submitting a \
                 build — dispatch/spawn-intent path broken, or every \
                 connected worker pre-dates this scenario"
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
        if !wait_recovery_done(ctx, 0.0, Duration::from_secs(60)).await? {
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
        // has_stream, ...}]} — `super::common::DebugList`. The original
        // CliCtx's port-forward points at the leader we just killed →
        // transport error. Re-open.
        let cli2 = CliCtx::open(&ctx.kube, 0, 0).await?;
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
            // Don't poison the next phase-2 scenario with this one's
            // leader-kill blast radius — settle dispatch before returning.
            super::common::phase2_settle_after_kill(ctx).await?;
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{} zombie executors (has_stream=false) after leader SIGKILL+recovery: {zombies:?}",
                zombies.len()
            )))
        }
    }
}
