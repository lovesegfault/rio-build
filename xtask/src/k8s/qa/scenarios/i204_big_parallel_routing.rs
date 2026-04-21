//! I-204: `big-parallel`-only derivation routes exclusively to kvm
//! pool (the only pool advertising big-parallel) instead of general.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BigParallelRouting;

#[async_trait]
impl Scenario for BigParallelRouting {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i204-big-parallel-routing",
            i_ref: Some(204),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, _ctx: &mut QaCtx) -> Result<Verdict> {
        Ok(Verdict::Skip(
            "needs derivation with requiredSystemFeatures=[big-parallel]; \
             smoke_build emits featureless busybox. Add nix_build_expr helper."
                .into(),
        ))
    }
}
