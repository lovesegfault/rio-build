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
use crate::k8s::shared;

pub struct SchedulerHealth;

const SERVICE: &str = "rio.scheduler.SchedulerService";

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
        // The scheduler image is intentionally minimal
        // (r[sec.image.control-plane-minimal]) — no `grpc-health-probe`
        // inside. Probe from OUTSIDE: port-forward to the leader pod's
        // 9001 and call grpc.health.v1.Health/Check via grpcurl (in the
        // dev shell). This is the exact probe BalancedChannel does, so
        // it's the I-026 signature directly.
        let (port, _guard) = shared::port_forward(NS, &format!("pod/{leader}"), 0, 9001).await?;
        let s = crate::sh::shell()?;
        let body = format!(r#"{{"service":"{SERVICE}"}}"#);
        let addr = format!("localhost:{port}");
        let out = crate::sh::try_read(crate::sh::cmd!(
            s,
            "grpcurl -plaintext -d {body} {addr} grpc.health.v1.Health/Check"
        ));
        match out {
            Ok(o) if o.contains("SERVING") => Ok(Verdict::Pass),
            Ok(o) => Ok(Verdict::Fail(format!(
                "leader {leader} health for {SERVICE} != SERVING: {o:?} \
                 — leader-acquire didn't flip the per-service reporter"
            ))),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("NOT_SERVING") || msg.to_lowercase().contains("not serving") {
                    Ok(Verdict::Fail(format!(
                        "leader {leader} health for {SERVICE} = NOT_SERVING \
                         — leader-acquire didn't flip the per-service reporter"
                    )))
                } else {
                    // grpcurl invocation itself failed (binary missing,
                    // port-forward race) — treat as scenario error.
                    Err(e)
                }
            }
        }
    }
}
