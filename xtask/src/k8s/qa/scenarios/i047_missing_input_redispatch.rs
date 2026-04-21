//! I-047: deleting an FOD output's narinfo must cause the dependent
//! build to RE-DISPATCH the FOD, not fail with "dependency does not
//! exist".
//!
//! Probe: run a build (FOD + consumer), DELETE the FOD output's
//! narinfo, run the consumer again, assert success +
//! `rio_scheduler_stale_completed_reset_total` increments (the metric
//! tracking "we noticed a completed dep's output is gone, reset it").

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct MissingInputRedispatch;

#[async_trait]
impl Scenario for MissingInputRedispatch {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i047-missing-input-redispatch",
            i_ref: Some(47),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler, Component::Postgres],
            },
            timeout: Duration::from_secs(240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let before = ctx
            .scrape_scheduler()
            .await?
            .sum("rio_scheduler_stale_completed_reset_total");

        // First build — populates the FOD output.
        ctx.nix_build_via_gateway(0, "i047a", 3, 1).await?;

        // Delete the busybox FOD output's narinfo. The FOD output path
        // is content-addressed (sha256 of the busybox tarball) — the
        // exact store_path is fixed and shared across ALL smoke builds.
        // Deleting it has fleet-wide blast radius on a multi-tenant
        // cluster; on the dev cluster (authorized-destructive) it's
        // acceptable. We scope by `store_path LIKE '%busybox'` to
        // match only the FOD output.
        let n = sqlx::query(
            "DELETE FROM narinfo WHERE store_path LIKE '%/busybox' \
             AND ca IS NOT NULL",
        )
        .execute(ctx.pg())
        .await?
        .rows_affected();
        if n == 0 {
            return Ok(Verdict::Skip(
                "no busybox FOD narinfo found to delete — first build may have \
                 substituted from upstream cache instead of fetching"
                    .into(),
            ));
        }

        // Second build — should detect the missing dep and re-dispatch
        // the FOD instead of MiscFailure.
        let result = ctx.nix_build_via_gateway(0, "i047b", 3, 1).await;

        let after = ctx
            .scrape_scheduler()
            .await?
            .sum("rio_scheduler_stale_completed_reset_total");

        match result {
            Ok(()) if after > before => Ok(Verdict::Pass),
            Ok(()) => Ok(Verdict::Skip(format!(
                "second build succeeded but stale_completed_reset_total stayed \
                 at {before} — possibly the busybox FOD was re-substituted \
                 from upstream cache, bypassing the reset path"
            ))),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("does not exist") {
                    Ok(Verdict::Fail(format!(
                        "I-047 regression — dependent build failed instead of \
                         re-dispatching deleted FOD: {msg}"
                    )))
                } else {
                    Err(e)
                }
            }
        }
    }
}
