//! I-056: executors stuck in `draining` long after their pod is gone.
//!
//! The original symptom was FODs stuck `[Ready]` with idle fetchers and
//! `fod_queue_depth=2`. Corrected root cause: executors marked draining
//! never transitioned to gone, leaving the dispatch filter rejecting on
//! a stale flag. Signature: `ListExecutors` rows with `status=draining`
//! (the draining status is not producible on the pull surface).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct StaleDraining;

#[async_trait]
impl Scenario for StaleDraining {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i056-stale-draining",
            i_ref: Some(56),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
            exercises: crate::exercises!(),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // `rio-cli workers --json` outputs the raw ListExecutorsResponse
        // proto: {"executors": [...], "leader_for_secs": N}. Index via
        // serde_json::Value so this scenario doesn't break every time
        // the proto grows a field.
        let resp: Value = ctx.cli_json(&["workers"])?;
        let executors = resp
            .get("executors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let stuck: Vec<String> = executors
            .iter()
            .filter(|e| {
                // The timestamp leg is gone: ExecutorInfo's Timestamp
                // field is #[serde(skip)] in the proto-JSON surface
                // (rio-proto/build.rs — prost Timestamps don't
                // Serialize), so the old `.get("last_heartbeat")`
                // conjunct never matched and the check was
                // draining-only in practice. Keep the honest form:
                // any draining row on this surface is stale (the
                // status is not producible post-stream-protocol).
                e.get("status").and_then(Value::as_str) == Some("draining")
            })
            .filter_map(|e| {
                e.get("executor_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();

        if stuck.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{} executor(s) stuck draining: {stuck:?}",
                stuck.len()
            )))
        }
    }
}
