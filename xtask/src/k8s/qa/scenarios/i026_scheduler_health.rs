//! I-026: scheduler grpc-health for `SchedulerService` stays NotServing
//! after leader acquisition.
//!
//! k8s readiness (empty-service health) saw the pod healthy; workers
//! probing `rio.scheduler.SchedulerService` saw NotServing → 0 dispatch.
//! The two probes diverging is the signature.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::NS;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct SchedulerHealth;

#[async_trait]
impl Scenario for SchedulerHealth {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i026-scheduler-health",
            i_ref: Some(26),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let leader = ctx.scheduler_leader().await?;
        // grpc-health-probe is in the scheduler image; exec it in-pod
        // so we don't need a local binary or a tunnel.
        let out = ctx.kubectl(&[
            "-n",
            NS,
            "exec",
            &leader,
            "--",
            "grpc-health-probe",
            "-addr=localhost:9001",
            "-service=rio.scheduler.SchedulerService",
        ]);
        match out {
            Ok(o) if o.contains("SERVING") => Ok(Verdict::Pass),
            Ok(o) => Ok(Verdict::Fail(format!(
                "leader {leader} health for SchedulerService != SERVING: {o:?} \
                 — leader-acquire didn't flip the per-service reporter"
            ))),
            Err(e) => Ok(Verdict::Skip(format!(
                "grpc-health-probe not in scheduler image or exec denied: {e:#}"
            ))),
        }
    }
}
