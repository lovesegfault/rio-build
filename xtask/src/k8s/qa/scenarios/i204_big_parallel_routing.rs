//! I-204: `big-parallel`-only derivation routes exclusively to kvm
//! pool (the only pool advertising big-parallel) instead of general.
//!
//! Regression: pre-I-204, `{big-parallel}` was treated as a hard gate
//! and only matched the kvm NodePool — so firefox/chromium spawned
//! `.metal` while featureless builders sat idle. The fix strips
//! `soft_features` (default {big-parallel, benchmark}) at DAG-insert
//! time. Assert: a big-parallel-only build does NOT increment the kvm
//! pool's spawn-intent count (it should land on a general builder).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BigParallelRouting;

#[async_trait]
impl Scenario for BigParallelRouting {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i204-big-parallel-routing",
            i_ref: Some(204),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let expr = format!(
            r#"{BUSYBOX_LET} builtins.derivation {{
              name = "rio-qa-i204-${{toString builtins.currentTime}}";
              system = "x86_64-linux";
              requiredSystemFeatures = ["big-parallel"];
              builder = "${{busybox}}";
              args = ["sh" "-c" "echo i204 > $out"];
            }}"#
        );

        // r[sched.dispatch.soft-features] strips big-parallel before
        // GetSpawnIntents, so the kvm pool's filter (which only sees
        // STRIPPED features) shouldn't match — and the snapshot's
        // queued count for kvm shouldn't bump for this derivation.
        let kvm_before = ctx
            .scrape_scheduler()
            .await?
            .labeled("rio_scheduler_spawn_intents", "pool", "kvm")
            .unwrap_or(0.0);

        ctx.nix_build_expr_via_gateway(0, &expr).await?;

        let kvm_after = ctx
            .scrape_scheduler()
            .await?
            .labeled("rio_scheduler_spawn_intents", "pool", "kvm")
            .unwrap_or(0.0);

        if kvm_after > kvm_before {
            Ok(Verdict::Fail(format!(
                "kvm spawn_intents {kvm_before} → {kvm_after} for a \
                 big-parallel-only derivation — soft_features stripping not applied"
            )))
        } else {
            Ok(Verdict::Pass)
        }
    }
}
