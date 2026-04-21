//! I-165: FUSE self-deadlock leaves processes in D-state.
//!
//! When the rio-mountd process serves and consumes its own mount, a
//! shutdown race can leave readers in uninterruptible sleep (D). The
//! fusectl-abort fix landed but I-165c showed it can still recur under
//! OOMKill (Drop never runs). Signature: any process in any builder pod
//! with `State: D` for more than a transient blip.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::poll_until;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct FuseDState;

#[async_trait]
impl Scenario for FuseDState {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i165-fuse-dstate",
            i_ref: Some(165),
            // The probe is read-only (`kubectl exec cat /proc/*/status`);
            // moved to Tenant so it can submit a warmup build to ensure
            // ≥1 builder is running rather than Skip on idle cluster.
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Self-setup: ensure ≥1 running builder pod by submitting a 30s
        // bg build, then poll until it appears.
        let bg = ctx.nix_build_via_gateway_bg(0, "i165-warmup", 30, 1);
        let builders = poll_until(Duration::from_secs(60), Duration::from_secs(3), || async {
            let p = ctx.running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?;
            Ok((!p.is_empty()).then_some(p))
        })
        .await?;
        let Some(builders) = builders else {
            bg.abort();
            return Ok(Verdict::Fail(
                "no builder pod came up within 60s of submitting a warmup build".into(),
            ));
        };

        let mut stuck = Vec::new();
        for pod in &builders {
            // Two samples 5s apart — D-state is normal for sub-second
            // I/O waits; the bug is *persistent* D.
            let first = d_state_pids(ctx, pod)?;
            if first.is_empty() {
                continue;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            let second = d_state_pids(ctx, pod)?;
            let persistent: Vec<_> = first.iter().filter(|p| second.contains(p)).collect();
            if !persistent.is_empty() {
                stuck.push(format!("{pod}: pids {persistent:?}"));
            }
        }
        bg.abort();

        if stuck.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "persistent D-state processes (FUSE self-deadlock?): {stuck:?}"
            )))
        }
    }
}

fn d_state_pids(ctx: &QaCtx, pod: &str) -> Result<Vec<String>> {
    // Builder image is minimal; rely on shell + /proc only.
    let out = ctx
        .kubectl(&[
            "-n",
            QaCtx::NS_BUILDERS,
            "exec",
            pod,
            "--",
            "sh",
            "-c",
            "for p in /proc/[0-9]*; do \
               s=$(awk '/^State:/{print $2}' $p/status 2>/dev/null); \
               [ \"$s\" = D ] && echo ${p#/proc/}; \
             done; true",
        ])
        .unwrap_or_default();
    Ok(out.lines().map(String::from).collect())
}
