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
        // 9001 and call Health/Check via tonic-health directly (same
        // probe BalancedChannel does — the I-026 signature). grpcurl
        // would work but the server doesn't expose reflection.
        let (port, _guard) = shared::port_forward(NS, &format!("pod/{leader}"), 0, 9001).await?;
        let ch = tonic::transport::Channel::from_shared(format!("http://localhost:{port}"))?
            .connect()
            .await?;
        let status = tonic_health::pb::health_client::HealthClient::new(ch)
            .check(tonic_health::pb::HealthCheckRequest {
                service: SERVICE.into(),
            })
            .await?
            .into_inner()
            .status();
        if status == tonic_health::pb::health_check_response::ServingStatus::Serving {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "leader {leader} health for {SERVICE} = {status:?} \
                 — leader-acquire didn't flip the per-service reporter"
            )))
        }
    }
}
