//! I-056: executors stuck in `draining` long after their pod is gone.
//!
//! The original symptom was FODs stuck `[Ready]` with idle fetchers and
//! `fod_queue_depth=2`. Corrected root cause: executors marked draining
//! never transitioned to gone, leaving the dispatch filter rejecting on
//! a stale flag. Signature: `ListExecutors` rows with `status=draining`
//! and `last_heartbeat` more than ~2min ago.

use std::time::{Duration, SystemTime};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct StaleDraining;

const STALE_SECS: i64 = 120;

#[async_trait]
impl Scenario for StaleDraining {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i056-stale-draining",
            i_ref: Some(56),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
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

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("post-1970")
            .as_secs() as i64;

        let stuck: Vec<String> = executors
            .iter()
            .filter(|e| {
                let draining = e.get("status").and_then(Value::as_str) == Some("draining");
                let hb_secs = e
                    .get("last_heartbeat")
                    .and_then(|t| t.get("seconds"))
                    .and_then(Value::as_i64);
                draining && hb_secs.is_some_and(|s| now - s > STALE_SECS)
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
                "{} executor(s) draining with last_heartbeat > {STALE_SECS}s: {stuck:?}",
                stuck.len()
            )))
        }
    }
}
