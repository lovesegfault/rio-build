//! I-207: stale `manifests.status='uploading'` row blocks PutPath.
//!
//! Full reproduction (PutPath retrigger for the SAME store_path)
//! requires a deterministic-output build helper this harness doesn't
//! have yet — `smoke_expr` includes `currentTime` in the name so
//! outputs never collide. This scenario asserts the LIVE invariant
//! instead: no manifest row may be `uploading` AND older than
//! 20 minutes (orphan-scanner STALE_THRESHOLD 15min + 5min slack).
//! That's the observable steady-state guarantee the I-207 fix
//! plus `r[store.put.stale-reclaim]` together provide.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct StaleUploading;

#[async_trait]
impl Scenario for StaleUploading {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i207-stale-uploading",
            i_ref: Some(207),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Postgres],
            },
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM manifests \
             WHERE status = 'uploading' AND updated_at < now() - interval '20 minutes'",
        )
        .fetch_one(ctx.pg())
        .await?;

        if n == 0 {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{n} manifest row(s) stuck status='uploading' > 20min — \
                 orphan-scanner / hot-path stale-reclaim not running"
            )))
        }
    }
}
