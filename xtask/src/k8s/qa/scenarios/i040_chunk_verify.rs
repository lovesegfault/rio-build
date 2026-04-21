//! I-040: chunk lost from S3 while manifest claims complete —
//! `rio-cli verify-chunks` must detect it.
//!
//! Destructive: deletes one S3 chunk object, runs verify-chunks,
//! asserts the missing hex appears in the report. Restoring the chunk
//! is non-trivial (would need a re-upload of the owning path), so this
//! mutates real state — declared Exclusive(S3, Postgres) and runs LAST
//! in qa's canonical order. Dev cluster is authorized-destructive.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;

use crate::k8s::eks::TF_DIR;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::sh::{self, cmd};

pub struct ChunkVerify;

#[async_trait]
impl Scenario for ChunkVerify {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i040-chunk-verify",
            i_ref: Some(40),
            isolation: Isolation::Exclusive {
                mutates: &[Component::S3, Component::Postgres],
            },
            timeout: Duration::from_secs(300),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let Ok(bucket) = crate::tofu::output(TF_DIR, "chunk_bucket_name") else {
            return Ok(Verdict::Skip(
                "chunk_bucket_name tofu output unavailable (k3s?)".into(),
            ));
        };

        // Pick a chunk that's referenced AND not deleted. Prefer one
        // with refcount==1 so we minimize blast radius (only one
        // manifest depends on it). On a dev cluster the I-201 scenario
        // already covers the inverse direction (PG row, S3 absent —
        // pre-existing); here we CAUSE one and assert detection.
        let row = sqlx::query(
            "SELECT encode(blake3_hash, 'hex') AS h FROM chunks \
             WHERE refcount = 1 AND NOT deleted ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(ctx.pg())
        .await?;
        let Some(row) = row else {
            return Ok(Verdict::Skip("no refcount=1 chunk to test against".into()));
        };
        let hex: String = row.try_get("h")?;
        let key = format!("chunks/{}/{hex}", &hex[..2]);

        let s = sh::shell()?;
        sh::try_read(cmd!(
            s,
            "aws s3api delete-object --bucket {bucket} --key {key}"
        ))?;

        // verify-chunks streams missing hex hashes to stdout.
        // CliCtx::run captures stdout; --store-addr is set by CliCtx.
        let out = match ctx.cli.run(&["verify-chunks", "--limit", "0"]) {
            Ok(o) => o,
            Err(e) => {
                // Some deployments need the limit flag named
                // differently or don't support it — fall back.
                let msg = format!("{e:#}");
                if msg.contains("unexpected argument") {
                    ctx.cli.run(&["verify-chunks"])?
                } else {
                    return Err(e);
                }
            }
        };

        if out.contains(&hex) {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "verify-chunks did not report deleted chunk {hex}. Output (first 500B): {}",
                &out.chars().take(500).collect::<String>()
            )))
        }
    }
}
