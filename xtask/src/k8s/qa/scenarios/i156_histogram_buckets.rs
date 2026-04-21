//! I-156: histogram metrics missing `_bucket` series → p99 panels
//! show "No data".
//!
//! `Scrape` doesn't expose its name set, so this checks a fixed list
//! of histograms that have bitten before. If a new histogram is added
//! without a bucket map, this won't catch it — the per-crate
//! `metrics_registered` test (referenced at observability.rs:254)
//! is the structural guard; this is the live-cluster sanity check.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::scrape_all_stores;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct HistogramBuckets;

/// (component, base_name). `_bucket`/`_count`/`_sum` are appended.
const HISTOGRAMS: &[(&str, &str)] = &[
    ("scheduler", "rio_scheduler_dispatch_latency_seconds"),
    ("scheduler", "rio_scheduler_actor_cmd_latency_seconds"),
    ("store", "rio_store_put_path_duration_seconds"),
    ("store", "rio_store_get_path_duration_seconds"),
];

#[async_trait]
impl Scenario for HistogramBuckets {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i156-histogram-buckets",
            i_ref: Some(156),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let sched = ctx.scrape_scheduler().await?;
        let stores = scrape_all_stores(ctx).await?;

        let mut missing = Vec::new();
        for &(comp, base) in HISTOGRAMS {
            let scrape = match comp {
                "scheduler" => Some(&sched),
                "store" => stores.first().map(|(_, s)| s),
                _ => unreachable!(),
            };
            let Some(scrape) = scrape else {
                continue;
            };
            // _count present but _bucket absent = the I-156 signature.
            // _count absent = metric not emitted yet (no traffic) — skip.
            let has_count = scrape.first(&format!("{base}_count")).is_some();
            let has_bucket = !scrape.series(&format!("{base}_bucket")).is_empty();
            if has_count && !has_bucket {
                missing.push(base);
            }
        }

        if missing.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "histogram(s) with _count but no _bucket series: {missing:?} \
                 — exporter in summary mode or bucket map missing"
            )))
        }
    }
}
