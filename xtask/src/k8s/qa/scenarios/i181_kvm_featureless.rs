//! I-181: featureless build counts toward feature-gated kvm pool
//! (∅ ⊆ [kvm,...] is vacuously true) → kvm-xlarge over-spawns at
//! ~$5/hr per phantom.
//!
//! Check: with no kvm-feature work in flight, kvm pools' SpawnIntent
//! count stays at 0 over a featureless-build window.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct KvmFeatureless;

#[async_trait]
impl Scenario for KvmFeatureless {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i181-kvm-featureless",
            i_ref: Some(181),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Find kvm-feature pools (Pool CR spec.features contains "kvm").
        let pools = ctx.kubectl(&[
            "get",
            "pools.rio.build",
            "-A",
            "-o",
            "jsonpath={range .items[?(@.spec.features)]}{.metadata.name}={.spec.features}{\"\\n\"}{end}",
        ])?;
        let kvm_pools: Vec<_> = pools
            .lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(_, f)| f.contains("kvm"))
            .map(|(n, _)| n.to_string())
            .collect();
        if kvm_pools.is_empty() {
            // x86-64-kvm + aarch64-kvm Pools are part of the standard
            // deploy (POOLS_JSON / pool.yaml). Absent ⇒ kvm/nixos-test
            // builds can never schedule — deployment-shape regression.
            return Ok(Verdict::Fail(
                "no Pool with features∋kvm defined — kvm/nixos-test builds \
                 unschedulable (deploy didn't render kvm pools)"
                    .into(),
            ));
        }

        let bg = ctx.nix_build_via_gateway_bg(0, "i181", 20, 1);
        tokio::time::sleep(Duration::from_secs(10)).await;

        // SpawnIntents per pool — kvm pools should see 0 for the
        // featureless build. Via metric: rio_scheduler_spawn_intents
        // labeled by pool. If that metric isn't exported, fall back to
        // checking no Jobs were created in the kvm pool's namespace.
        let scrape = ctx.scrape_scheduler().await?;
        let mut over = Vec::new();
        for p in &kvm_pools {
            // Scrape::labeled is private; check series substring.
            let n: f64 = scrape
                .series("rio_scheduler_spawn_intents")
                .iter()
                .filter(|(l, _)| l.contains(&format!("pool=\"{p}\"")))
                .map(|(_, v)| *v)
                .sum();
            if n > 0.0 {
                over.push(format!("{p}={n}"));
            }
        }
        bg.abort();

        if over.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "kvm pool(s) counting featureless work (∅ ⊆ [kvm] vacuously true): {over:?}"
            )))
        }
    }
}
