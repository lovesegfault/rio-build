//! I-006: IRSA trust-policy namespace mismatch → S3-chunked PutPath
//! fails `AssumeRoleWithWebIdentity` denied.
//!
//! ADR-019 moved store to `rio-store` namespace; tofu IRSA policy still
//! trusted `rio-system:rio-store`. Signature: build with >256KiB output
//! (chunked S3 path) → store log shows `AccessDenied` /
//! `AssumeRoleWithWebIdentity`.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::NS_STORE;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct IrsaDenied;

#[async_trait]
impl Scenario for IrsaDenied {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i006-irsa-denied",
            i_ref: Some(6),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // 512 KiB output forces the chunked-S3 path (threshold 256 KiB).
        ctx.nix_build_via_gateway(0, "i006", 5, 512).await?;

        // Last 5min of store logs across all replicas.
        let logs = ctx.kubectl(&[
            "-n",
            NS_STORE,
            "logs",
            "-l",
            "app.kubernetes.io/name=rio-store",
            "--since=5m",
            "--prefix",
        ])?;
        let denied: Vec<_> = logs
            .lines()
            .filter(|l| {
                l.contains("AssumeRoleWithWebIdentity")
                    || (l.contains("AccessDenied") && l.contains("s3"))
            })
            .take(3)
            .collect();
        if denied.is_empty() {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "store log shows IRSA/S3 access denied (sample): {denied:?} \
                 — IRSA trust policy namespace mismatch?"
            )))
        }
    }
}
