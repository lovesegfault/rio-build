//! I-201: stranded chunks — PG records a confirmed upload
//! (`uploaded_at` set, row not deleted) but the S3 object is absent.
//! That is the inconsistency that turns a dedup'd PutPath into data
//! loss: a later writer of the same content trusts the confirmed
//! presence, skips its own upload, and the chunk stays permanently
//! missing.
//!
//! Direct probe: sample confirmed-uploaded chunks from PG, S3
//! HeadObject each. Presence is keyed on `uploaded_at` — the same
//! predicate as the server-side VerifyChunks scan — never on the
//! chunk-liveness signal (liveness is not presence; the in-flight
//! window where a manifest already references the chunk but the PUT
//! has not finished is not a finding).
//! S3 key layout is `chunks/{aa}/{blake3-hex}` (rio-store/backend.rs).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;

use crate::k8s::eks::TF_DIR;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::sh::{self, cmd};

pub struct StrandedChunks;

const SAMPLE: i64 = 1000;

#[async_trait]
impl Scenario for StrandedChunks {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i201-stranded-chunks",
            i_ref: Some(201),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Store, Component::Postgres],
            },
            timeout: Duration::from_secs(180),
            exercises: crate::exercises!(),
        }
    }

    /// PG sample → S3 HeadObject probe — never submits a build, never
    /// touches the scheduler. Safe to run concurrently with a leader
    /// kill.
    fn reads(&self) -> &'static [Component] {
        &[]
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Bucket name from tofu output (EKS only — k3s uses an
        // in-cluster MinIO whose creds aren't on the operator's box).
        let Ok(bucket) = crate::tofu::output(TF_DIR, "chunk_bucket_name") else {
            return Ok(Verdict::Skip(
                "chunk_bucket_name tofu output unavailable (k3s?)".into(),
            ));
        };

        let rows = sqlx::query(
            "SELECT encode(blake3_hash, 'hex') AS h FROM chunks \
             WHERE uploaded_at IS NOT NULL AND NOT deleted \
             ORDER BY created_at DESC LIMIT $1",
        )
        .bind(SAMPLE)
        .fetch_all(ctx.pg())
        .await?;
        if rows.is_empty() {
            // No chunks ⇒ no stranded chunks. Vacuously the assertion
            // ("no PG-says-exists, S3-says-404 inconsistency") holds.
            return Ok(Verdict::Pass);
        }

        let s = sh::shell()?;
        for r in &rows {
            let hex: String = r.try_get("h")?;
            let key = format!("chunks/{}/{hex}", &hex[..2]);
            // `aws s3api head-object` exits non-zero with "Not Found"
            // (404) or "Forbidden" (403 — bucket policy treats missing
            // as 403 sometimes). Either way, stderr names the code.
            let res = sh::try_read(cmd!(
                s,
                "aws s3api head-object --bucket {bucket} --key {key}"
            ));
            if let Err(e) = res {
                let msg = format!("{e:#}");
                if msg.contains("Not Found") || msg.contains("NoSuchKey") || msg.contains("404") {
                    return Ok(Verdict::Fail(format!(
                        "stranded chunk: PG says uploaded (`uploaded_at` set, not deleted) for {hex} but s3://{bucket}/{key} → 404"
                    )));
                }
                // Other errors (auth, throttle) propagate.
                return Err(e.context(format!("head-object {key}")));
            }
        }
        Ok(Verdict::Pass)
    }
}
