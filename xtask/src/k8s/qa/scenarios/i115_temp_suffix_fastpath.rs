//! I-115: nix-daemon's sandbox temp probes (`.chroot`/`.lock`/`.check`)
//! reach store GetPath instead of a FUSE-side ENOENT fast-path.
//!
//! The original fix added a suffix denylist, later replaced by the
//! old-FUSE JIT allowlist, and since the castore cutover the property
//! is structural: the per-build castore mount serves `lookup()` from a
//! tree prefetched at mount time, so a name outside the closure gets a
//! cached negative dentry and NO lookup ever contacts the store. The
//! observable signal is `rio_builder_castore_fuse_upcalls_total{op="lookup"}`
//! ticking on a busy builder while the build proceeds.
//!
//! Regression check: submit a build, scrape its builder pod, assert
//! lookup upcalls > 0 (the castore lookup path is live and answering
//! from heap). If the castore mount were bypassed or broken, the build
//! would fail outright or the counter would stay 0.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

use super::common::scrape_builder;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct TempSuffixFastpath;

const METRIC: &str = "rio_builder_castore_fuse_upcalls_total";

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
        // `op="lookup"` across them. With ephemeral one-build-per-
        // pod workers we can't tie a pod to OUR build, but the property
        // is cluster-level: if the castore lookup path is broken, EVERY
        // builder's lookup count is 0.
        // Scrape EVERY builder on EVERY tick for the full 60s window —
        // don't break at first non-empty (a freshly-started pod from a
        // concurrent scenario may have lookup=0 because its castore
        // mount only just appeared; OUR build's pod might be later in
        // the window). The property is cluster-level: path broken ⇒
        // EVERY scrape across the window shows 0.
        let mut lookup_sum = 0.0;
        let mut scraped = 0usize;
        for _ in 0..12 {
            sleep(Duration::from_secs(5)).await;
            for p in &ctx.running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)? {
                if let Ok(s) = scrape_builder(ctx, p).await {
                    lookup_sum += s.labeled(METRIC, "op", "lookup").unwrap_or(0.0);
                    scraped += 1;
                }
            }
            if lookup_sum > 0.0 {
                break; // proven; no need to keep scraping
            }
        }

        let _ = bg.await;

        if scraped == 0 {
            return Ok(Verdict::Fail(
                "no builder pod became scrapeable within 60s of submitting a \
                 build — dispatch/spawn-intent path broken"
                    .into(),
            ));
        }
        if lookup_sum > 0.0 {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{METRIC}{{op=\"lookup\"}} == 0 across {scraped} builder pod(s) — \
                 the castore-FUSE lookup path is not serving; builds are either not \
                 mounting the castore lower or not reaching their inputs"
            )))
        }
    }
}
