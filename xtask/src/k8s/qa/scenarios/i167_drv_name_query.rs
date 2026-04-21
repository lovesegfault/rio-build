//! I-167: drv name containing `?id=` breaks synth_db generation.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct DrvNameQuery;

#[async_trait]
impl Scenario for DrvNameQuery {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i167-drv-name-query",
            i_ref: Some(167),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, _ctx: &mut QaCtx) -> Result<Verdict> {
        Ok(Verdict::Skip(
            "needs custom-expr build (smoke_build's tag is sanitized into the \
             busybox derivation name); add nix_build_expr_via_gateway helper"
                .into(),
        ))
    }
}
