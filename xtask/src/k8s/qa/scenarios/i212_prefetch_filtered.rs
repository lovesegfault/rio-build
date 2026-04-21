//! I-212: PrefetchHint sends ALL outputs of inputDrvs, not just
//! declared ones.
//!
//! When a derivation declares `inputDrvs.<drv>.outputs = ["out"]` but
//! the input drv has `["out","doc"]`, the scheduler's prefetch hint
//! should only include `out`. Pre-I-212 it sent both, wasting builder
//! prefetch time on undeclared outputs. Assert: the
//! `rio_scheduler_prefetch_filtered_total{reason="undeclared_output"}`
//! counter increments — proves the filter ran and dropped the extra.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::scrape_builder;
use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct PrefetchFiltered;

// I-212's filter is BUILDER-side (warm-gate) — the metric is
// `rio_builder_...`, reason `not_input` (JIT allowlist armed and path
// not in declared inputs). See observability.md:229.
const METRIC: &str = "rio_builder_prefetch_filtered_total";

#[async_trait]
impl Scenario for PrefetchFiltered {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i212-prefetch-filtered",
            i_ref: Some(212),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // `multi` has out+doc; `consumer` declares only `multi.out`.
        // Nix's inputDrvs auto-derives the output set from the
        // expression body — referencing `${multi.out}` (not
        // `${multi.doc}`) is what makes the declared set `["out"]`.
        let expr = format!(
            r#"{BUSYBOX_LET}
            let multi = builtins.derivation {{
              name = "rio-qa-i212-multi-${{toString builtins.currentTime}}";
              system = "x86_64-linux";
              outputs = ["out" "doc"];
              builder = "${{busybox}}";
              args = ["sh" "-c" "echo o > $out; echo d > $doc"];
            }}; in builtins.derivation {{
              name = "rio-qa-i212-consumer-${{toString builtins.currentTime}}";
              system = "x86_64-linux";
              builder = "${{busybox}}";
              # busybox sh has no PATH — sh builtins only. Referencing
              # ${{multi.out}} (NOT ${{multi.doc}}) is what makes the
              # declared inputDrv outputs = ["out"]; the body just needs
              # to use it.
              args = ["sh" "-c" "echo ${{multi.out}} > $out"];
            }}"#
        );

        // The builder pod that handles the consumer drv is ephemeral
        // and unknown until it runs — submit, then scrape EVERY
        // running builder; the filter is per-prefetch-hint so any
        // increment anywhere proves it fired.
        ctx.nix_build_expr_via_gateway(0, &expr).await?;

        let pods = ctx.running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?;
        let mut total = 0.0;
        for p in &pods {
            if let Ok(s) = scrape_builder(ctx, p).await {
                total += s.labeled(METRIC, "reason", "not_input").unwrap_or(0.0);
            }
        }

        if total > 0.0 {
            Ok(Verdict::Pass)
        } else if pods.is_empty() {
            Ok(Verdict::Skip("no builder pods to scrape".into()))
        } else {
            Ok(Verdict::Fail(format!(
                "{METRIC}{{reason=not_input}} == 0 across {} builders — \
                 prefetch hint may be sending undeclared outputs",
                pods.len()
            )))
        }
    }
}
