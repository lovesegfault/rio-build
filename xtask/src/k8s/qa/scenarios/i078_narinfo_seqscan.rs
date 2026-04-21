//! I-078: narinfo lookup missing index → Seq Scan.
//!
//! The store hits `narinfo` per path; without the PK index it was
//! ~400ms/query. Fixed, but a future migration could regress it. The
//! structurally-correct check is the planner output: any plan that
//! shows `Seq Scan` on `narinfo` is the regression, regardless of
//! current row count.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct NarinfoSeqScan;

#[async_trait]
impl Scenario for NarinfoSeqScan {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i078-narinfo-seqscan",
            i_ref: Some(78),
            isolation: Isolation::Shared,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Runtime query (not `query!`) — the cluster's schema isn't
        // visible to xtask's offline cache. EXPLAIN doesn't bind $1,
        // so use a literal placeholder hash.
        let plan: serde_json::Value = sqlx::query(
            "EXPLAIN (FORMAT JSON) \
             SELECT * FROM narinfo WHERE store_path_hash = '\\x00'::bytea",
        )
        .fetch_one(ctx.pg())
        .await?
        .try_get(0)?;

        // EXPLAIN JSON is `[{"Plan": {...}}]`. Walk for any `"Seq Scan"`
        // node whose `"Relation Name"` is `narinfo`. A Seq Scan elsewhere
        // (e.g., a tiny lookup table) is fine.
        if seq_scan_on(&plan, "narinfo") {
            return Ok(Verdict::Fail(format!(
                "narinfo PK lookup uses Seq Scan — index missing/dropped:\n{plan}"
            )));
        }
        Ok(Verdict::Pass)
    }
}

fn seq_scan_on(v: &serde_json::Value, rel: &str) -> bool {
    match v {
        serde_json::Value::Object(m) => {
            let hit = m.get("Node Type").and_then(|n| n.as_str()) == Some("Seq Scan")
                && m.get("Relation Name").and_then(|n| n.as_str()) == Some(rel);
            hit || m.values().any(|c| seq_scan_on(c, rel))
        }
        serde_json::Value::Array(a) => a.iter().any(|c| seq_scan_on(c, rel)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_seq_scan() {
        let bad = serde_json::json!([{"Plan": {
            "Node Type": "Seq Scan", "Relation Name": "narinfo"
        }}]);
        assert!(seq_scan_on(&bad, "narinfo"));
        let good = serde_json::json!([{"Plan": {
            "Node Type": "Index Scan", "Relation Name": "narinfo",
            "Plans": [{"Node Type": "Seq Scan", "Relation Name": "other"}]
        }}]);
        assert!(!seq_scan_on(&good, "narinfo"));
    }
}
