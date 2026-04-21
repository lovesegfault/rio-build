//! I-201: stranded chunks — `chunks.refcount > 0` but the S3 object
//! is absent. Race: SIGKILL mid-upload leaves the PG row at refcount=1
//! while S3 PutObject never completed; a concurrent dedup sees the row,
//! skips its own upload, and the chunk is permanently missing.
//!
//! The full check (`SELECT blake3_hash FROM chunks WHERE refcount > 0`
//! sampled, then S3 HeadObject each) needs PG + S3 access from xtask.
//! Neither is wired in QaCtx yet.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct StrandedChunks;

#[async_trait]
impl Scenario for StrandedChunks {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i201-stranded-chunks",
            i_ref: Some(201),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Store, Component::Postgres],
            },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // The store exposes `rio_store_stranded_chunks` if the orphan
        // scanner found any on its last sweep — that's the cheap proxy.
        // If the metric is absent (older store, scanner disabled), Skip.
        let leader = ctx.kubectl(&[
            "-n",
            crate::k8s::NS_STORE,
            "get",
            "pods",
            "-l",
            "app.kubernetes.io/name=rio-store",
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ])?;
        if leader.is_empty() {
            return Ok(Verdict::Skip("no rio-store pod found".into()));
        }
        let body = crate::k8s::status::scrape_pod(
            &ctx.kube,
            crate::k8s::NS_STORE,
            &leader,
            crate::k8s::status::STORE_METRICS_PORT,
        )
        .await?;
        let scrape = crate::k8s::status::Scrape::parse(&body);

        match scrape.first("rio_store_stranded_chunks") {
            None => Ok(Verdict::Skip(
                "rio_store_stranded_chunks gauge not exported \
                 — needs QaCtx::psql() + S3 HeadObject for direct probe"
                    .into(),
            )),
            Some(n) if n > 0.0 => Ok(Verdict::Fail(format!(
                "{n} stranded chunks (refcount>0, S3 absent) per orphan-scanner"
            ))),
            Some(_) => Ok(Verdict::Pass),
        }
    }
}
