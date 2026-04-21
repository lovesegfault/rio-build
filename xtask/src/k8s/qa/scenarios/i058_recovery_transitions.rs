//! I-058: recovery never transitions Queued→Ready, recovered builds
//! freeze.
//!
//! Restart the scheduler with an active build that has SOME completed
//! deps and SOME still Queued. The new leader's recovery must call
//! `compute_initial_states` so the Queued nodes whose deps are all
//! terminal flip to Ready and dispatch. Signature for the bug: build
//! never completes after restart.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{NS_SYSTEM, kill_pod, poll_until, wait_new_leader, wait_recovery_done};
use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct RecoveryTransitions;

#[async_trait]
impl Scenario for RecoveryTransitions {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i058-recovery-transitions",
            i_ref: Some(58),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler],
            },
            timeout: Duration::from_secs(300),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // 3-node chain: a(5s) → b(5s) → c(5s). Kill the leader after
        // ~7s — `a` is done, `b` is mid-run or Queued, `c` is Queued.
        // The bug freezes c at Queued forever.
        let expr = format!(
            r#"{BUSYBOX_LET}
            let mk = name: dep: builtins.derivation {{
              inherit name;
              system = "x86_64-linux";
              builder = "${{busybox}}";
              args = ["sh" "-c" "${{if dep == null then "" else "cat ${{dep}} >/dev/null;"}} read -t 5 x </dev/zero||true; echo ${{name}} > $out"];
            }};
            a = mk "rio-qa-i058-a-${{toString builtins.currentTime}}" null;
            b = mk "rio-qa-i058-b-${{toString builtins.currentTime}}" a;
            in mk "rio-qa-i058-c-${{toString builtins.currentTime}}" b"#
        );

        let bg = ctx.nix_build_expr_via_gateway_bg(0, &expr);
        // Let `a` complete.
        tokio::time::sleep(Duration::from_secs(10)).await;

        let old_leader = ctx.scheduler_leader().await?;
        kill_pod(ctx, NS_SYSTEM, &old_leader)?;
        wait_new_leader(ctx, &old_leader, Duration::from_secs(60)).await?;
        if !wait_recovery_done(ctx, -1.0, Duration::from_secs(60)).await? {
            bg.abort();
            return Ok(Verdict::Fail("new leader never completed recovery".into()));
        }

        // The build's ssh-ng client (bg) is dead (gateway→scheduler
        // WatchBuild reconnects but the OUTER nix process likely
        // errored on the leader bounce). What we actually assert: the
        // SCHEDULER side completes — `derivations_running` drains to 0
        // AND `derivations_queued` drains to 0 within the chain's
        // remaining wall-clock + slack.
        let drained = poll_until(Duration::from_secs(120), Duration::from_secs(5), || async {
            let s = ctx.scrape_scheduler().await?;
            let q = s.sum("rio_scheduler_derivations_queued");
            let r = s.sum("rio_scheduler_derivations_running");
            Ok((q == 0.0 && r == 0.0).then_some(()))
        })
        .await?;
        let _ = bg.await;

        if drained.is_some() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(
                "derivations_queued/running never drained to 0 after leader \
                 restart — Queued→Ready transition not applied at recovery"
                    .into(),
            ))
        }
    }
}
