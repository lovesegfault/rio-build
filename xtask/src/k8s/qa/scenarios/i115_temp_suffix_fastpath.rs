//! I-115: nix-daemon's sandbox temp probes (`.chroot`/`.lock`/`.check`)
//! reach store GetPath instead of FUSE-side ENOENT fast-path.
//!
//! The original fix added a suffix denylist; that was later REPLACED by
//! the JIT allowlist (`r[builder.fuse.jit-lookup]`), which is strictly
//! stronger: only basenames in the per-build registered-input set
//! trigger a fetch; everything else (the ~67 daemon probes per build)
//! gets fast ENOENT. The metric is
//! `rio_builder_fuse_jit_lookup_total{outcome="reject"}`.
//!
//! Regression check: submit a build, scrape its builder pod, assert
//! `reject` > 0. If the allowlist gate were broken (e.g. `jit_classify`
//! short-circuited to `KnownInput`), every probe would fall through to
//! the store and `reject` would stay 0.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

use super::common::scrape_builder;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct TempSuffixFastpath;

const METRIC: &str = "rio_builder_fuse_jit_lookup_total";

#[async_trait]
impl Scenario for TempSuffixFastpath {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i115-temp-suffix-fastpath",
            i_ref: Some(115),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // 30s build — long enough that we can find+scrape the builder
        // pod mid-run (ephemeral pods exit ~120s after completion, but
        // scraping during the build avoids racing pod teardown).
        let bg = ctx.nix_build_via_gateway_bg(0, "i115", 30, 1);

        // Poll for any running builder, then scrape every one and sum
        // `outcome="reject"` across them. With ephemeral one-build-per-
        // pod workers we can't tie a pod to OUR build, but the property
        // is cluster-level: if the JIT gate is broken, EVERY builder's
        // reject count is 0.
        let mut reject_sum = 0.0;
        let mut scraped = 0usize;
        for _ in 0..12 {
            sleep(Duration::from_secs(5)).await;
            let pods = ctx.running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?;
            if pods.is_empty() {
                continue;
            }
            for p in &pods {
                // A pod that just started may not have its metrics
                // server up yet — skip transient scrape errors.
                if let Ok(s) = scrape_builder(ctx, p).await {
                    reject_sum += s.labeled(METRIC, "outcome", "reject").unwrap_or(0.0);
                    scraped += 1;
                }
            }
            if scraped > 0 {
                break;
            }
        }

        bg.await??;

        if scraped == 0 {
            return Ok(Verdict::Fail(
                "no builder pod became scrapeable within 60s of submitting a \
                 build — dispatch/spawn-intent path broken"
                    .into(),
            ));
        }
        if reject_sum > 0.0 {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{METRIC}{{outcome=\"reject\"}} == 0 across {scraped} builder pod(s) — \
                 JIT-lookup fast-path not firing; daemon temp probes likely hitting GetPath"
            )))
        }
    }
}
