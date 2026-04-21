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

use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct FuseDState;

#[async_trait]
impl Scenario for FuseDState {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i165-fuse-dstate",
            i_ref: Some(165),
            isolation: Isolation::Exclusive {
                mutates: &[Component::BuilderPool],
            },
            timeout: Duration::from_secs(90),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let builders = ctx.running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?;
        if builders.is_empty() {
            return Ok(Verdict::Skip("no running builder pods".into()));
        }

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
