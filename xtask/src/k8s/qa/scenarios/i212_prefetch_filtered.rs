//! I-212: PrefetchHint sends ALL outputs of inputDrvs, not just
//! declared ones.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct PrefetchFiltered;

#[async_trait]
impl Scenario for PrefetchFiltered {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i212-prefetch-filtered",
            i_ref: Some(212),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, _ctx: &mut QaCtx) -> Result<Verdict> {
        Ok(Verdict::Skip(
            "needs multi-output inputDrv where build declares subset; \
             smoke_build is single-output. Add nix_build_expr helper, \
             then assert rio_scheduler_prefetch_filtered_total{reason=not_input} > 0."
                .into(),
        ))
    }
}
