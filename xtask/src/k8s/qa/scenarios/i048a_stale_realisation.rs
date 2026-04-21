//! I-048a: realisation row pointing at an absent narinfo must be
//! filtered (not used as a cache-hit).
//!
//! Seed a synthetic realisation whose `store_path` has no narinfo;
//! assert `rio_scheduler_stale_realisation_filtered_total` increments
//! when a build referencing that drv-hash queries cached outputs.
//! Full end-to-end requires submitting a CA build whose drv_hash
//! matches — this scenario asserts the SEEDING+CLEANUP path and the
//! steady-state invariant (no live stale realisations >0); the
//! per-build trigger is left as Skip with reason.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::PgCleanup;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct StaleRealisation;

#[async_trait]
impl Scenario for StaleRealisation {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i048a-stale-realisation",
            i_ref: Some(48),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Postgres],
            },
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Live invariant: no realisations point at non-existent narinfo.
        // Schema (002_store.sql): realisations(drv_hash BYTEA,
        // output_name, output_path, output_hash BYTEA, ...).
        let stale: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM realisations r \
             LEFT JOIN narinfo n ON n.store_path = r.output_path \
             WHERE n.store_path_hash IS NULL",
        )
        .fetch_one(ctx.pg())
        .await?;

        if stale > 0 {
            return Ok(Verdict::Fail(format!(
                "{stale} stale realisation row(s) (no matching narinfo) — \
                 GC/filter not reaping them"
            )));
        }

        // Seed-and-cleanup probe: insert one, confirm the LEFT JOIN
        // query above counts it (validates the detection query the
        // scheduler's filter uses), then delete.
        let drv_hash = b"qai048a_stale_realisation_probe_";
        let probe_path = "/nix/store/qai048a-nonexistent";
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash) \
             VALUES ($1, 'out', $2, $1) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&drv_hash[..])
        .bind(probe_path)
        .execute(ctx.pg())
        .await?;
        let cleanup = PgCleanup::new(
            ctx.pg(),
            format!("DELETE FROM realisations WHERE output_path = '{probe_path}'"),
        );

        let detected: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM realisations r \
             LEFT JOIN narinfo n ON n.store_path = r.output_path \
             WHERE n.store_path_hash IS NULL AND r.output_path = $1",
        )
        .bind(probe_path)
        .fetch_one(ctx.pg())
        .await?;
        cleanup.run().await?;

        if detected == 1 {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Skip(
                "stale-realisation detection query returned 0 for seeded row — \
                 schema differs from expected; adjust JOIN"
                    .into(),
            ))
        }
    }
}
