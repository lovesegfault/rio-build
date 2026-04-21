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

use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct PrefetchFiltered;

const METRIC: &str = "rio_scheduler_prefetch_filtered_total";

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
              args = ["sh" "-c" "cat ${{multi.out}} > $out"];
            }}"#
        );

        let scrape = |s: crate::k8s::status::Scrape| {
            s.labeled(METRIC, "reason", "undeclared_output")
                .unwrap_or(0.0)
        };
        let before = scrape(ctx.scrape_scheduler().await?);
        ctx.nix_build_expr_via_gateway(0, &expr).await?;
        let after = scrape(ctx.scrape_scheduler().await?);

        if after > before {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{METRIC} did not increment ({before} → {after}) — \
                 prefetch hint may be sending undeclared outputs"
            )))
        }
    }
}
