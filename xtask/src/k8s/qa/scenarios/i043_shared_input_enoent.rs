//! I-043: input ENOENT just after dependency upload.
//!
//! Four-layer onion: overlayfs negative-dentry cache + FUSE bloom +
//! warm-before-mount ordering + store NotFound. Signature: a build
//! whose input was uploaded SECONDS ago fails `does not exist`.
//! Approximation here: two builds in quick succession sharing the same
//! freshly-uploaded output (the first build's output is the second's
//! input via the `tag` mechanism doesn't work — both are independent).
//! Honest scope: this checks for the ENOENT-class log signature in
//! builder logs over a 2-build window; full repro needs a derivation
//! that depends on a just-built path.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct SharedInputEnoent;

#[async_trait]
impl Scenario for SharedInputEnoent {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i043-shared-input-enoent",
            i_ref: Some(43),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Two trivial builds back-to-back. The shared-input dependency
        // structure isn't expressible via smoke_build's busybox helper —
        // this check is downgraded to "no ENOENT-class build failures
        // in the window", which catches the symptom if not the exact
        // race.
        ctx.nix_build_via_gateway(0, "i043a", 3, 1).await?;
        ctx.nix_build_via_gateway(0, "i043b", 3, 1).await?;

        let logs = ctx.kubectl(&[
            "-n",
            QaCtx::NS_BUILDERS,
            "logs",
            "-l",
            QaCtx::BUILDER_LABEL,
            "--since=2m",
            "--prefix",
        ])?;
        let enoent: Vec<_> = logs
            .lines()
            .filter(|l| l.contains("build input") && l.contains("does not exist"))
            .take(3)
            .collect();
        if enoent.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "builder log shows input-ENOENT (sample): {enoent:?} \
                 — overlayfs negative-dentry or FUSE warm race"
            )))
        }
    }
}
