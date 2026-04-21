//! I-048c: assert the fault injector actually injects faults.
//!
//! Guards against the silent-no-op failure mode (I-056 class) where the
//! injector reports success but traffic is unaffected — the exact risk
//! that ruled out Chaos Mesh / Litmus on the v6-only cluster. This is
//! also the regression check for chaos.rs's own ip6tables path.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BlackholeSelfTest;

#[async_trait]
impl Scenario for BlackholeSelfTest {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i048c-blackhole-self-test",
            i_ref: Some(48),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler, Component::BuilderPool],
            },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, _ctx: &mut QaCtx) -> Result<Verdict> {
        // 1. snapshot rio_scheduler_worker_disconnects_total
        // 2. chaos::run(Blackhole, SchedulerLeader, AllWorkers, 60s)
        // 3. assert metric incremented within 45s (h2 keepalive 30s
        //    + 10s timeout + slack)
        // 4. assert metric STOPS incrementing after chaos teardown
        //    (remediation worked)
        Ok(Verdict::Skip(
            "stub — wired after scheduler lifetime pass".into(),
        ))
    }
}
