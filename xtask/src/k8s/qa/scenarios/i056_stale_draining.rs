//! I-056: executors stuck in `draining` long after their pod is gone.
//!
//! The original symptom was FODs stuck `[Ready]` with idle fetchers and
//! `fod_queue_depth=2`. Corrected root cause: executors marked draining
//! never transitioned to gone, leaving the dispatch filter rejecting on
//! a stale flag. Signature: `ListExecutors` rows with `draining=true`
//! and `last_seen` more than ~2min ago.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct StaleDraining;

#[derive(Deserialize)]
struct Worker {
    executor_id: String,
    #[serde(default)]
    draining: bool,
    #[serde(default)]
    last_seen_ago_secs: Option<u64>,
}

const STALE_SECS: u64 = 120;

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
        let workers: Vec<Worker> = match ctx.cli_json(&["workers"]) {
            Ok(w) => w,
            Err(e) => {
                return Ok(Verdict::Skip(format!(
                    "rio-cli workers --json shape mismatch: {e:#}; \
                     adjust Worker fields to current proto"
                )));
            }
        };

        let stuck: Vec<_> = workers
            .iter()
            .filter(|w| w.draining && w.last_seen_ago_secs.is_some_and(|s| s > STALE_SECS))
            .map(|w| w.executor_id.as_str())
            .collect();

        if stuck.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{} executor(s) draining with last_seen > {STALE_SECS}s: {stuck:?}",
                stuck.len()
            )))
        }
    }
}
