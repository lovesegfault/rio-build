//! I-078: narinfo lookup missing index on `store_path` → Seq Scan.
//!
//! The store's QPI hit `narinfo` per path; without the index it was
//! ~400ms/query. Fixed by adding the index, but a future migration
//! could regress it. The robust check is `pg_stat_statements` mean
//! exec time, but that extension may not be enabled — fall back to a
//! direct EXPLAIN probe if so.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

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

    async fn run(&self, _ctx: &mut QaCtx) -> Result<Verdict> {
        // Needs PG access (psql via tunnel or rio-cli admin SQL passthrough).
        // Neither exists today; QaCtx doesn't expose a postgres handle.
        // The structurally-correct assert (EXPLAIN on the narinfo lookup,
        // reject if "Seq Scan on narinfo" appears) is documented here so
        // wiring is mechanical once a `ctx.psql(q)` helper lands.
        Ok(Verdict::Skip(
            "needs QaCtx::psql() — pg_stat_statements / EXPLAIN probe not wired".into(),
        ))
    }
}
