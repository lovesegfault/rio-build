//! I-208: `derivations` resource floor must survive merge/recovery.
//!
//! The legacy `size_class_floor` column was DROPPED (migration 045);
//! ADR-023 replaced it with `floor_mem_bytes`/`floor_disk_bytes`/
//! `floor_deadline_secs` (migration 044). The original I-208 bug was
//! that merge didn't hydrate the floor from PG, so a learned floor
//! reset to default every run. This asserts the persistence direction:
//! write `floor_mem_bytes`, restart scheduler, read it back. If
//! recovery loads it, merge will too (same `from_row` codepath).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;

use super::common::{NS_SYSTEM, PgCleanup, kill_pod, wait_new_leader, wait_recovery_done};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct FloorHydrated;

const PROBE_FLOOR: i64 = 1_073_741_824; // 1 GiB — distinct from any default

#[async_trait]
impl Scenario for FloorHydrated {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i208-floor-hydrated",
            i_ref: Some(208),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler, Component::Postgres],
            },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Find one terminal derivation with default (0) floor.
        let row = sqlx::query(
            "SELECT derivation_id FROM derivations \
             WHERE floor_mem_bytes = 0 \
             AND status IN ('completed','poisoned') LIMIT 1",
        )
        .fetch_optional(ctx.pg())
        .await?;
        let Some(row) = row else {
            return Ok(Verdict::Skip(
                "no terminal derivation with floor_mem_bytes=0 to test against".into(),
            ));
        };
        let drv_id: sqlx::types::Uuid = row.try_get("derivation_id")?;

        sqlx::query("UPDATE derivations SET floor_mem_bytes = $2 WHERE derivation_id = $1")
            .bind(drv_id)
            .bind(PROBE_FLOOR)
            .execute(ctx.pg())
            .await?;
        let cleanup = PgCleanup::new(
            ctx.pg(),
            format!("UPDATE derivations SET floor_mem_bytes = 0 WHERE derivation_id = '{drv_id}'"),
        );

        let old_leader = ctx.scheduler_leader().await?;
        kill_pod(ctx, NS_SYSTEM, &old_leader)?;
        wait_new_leader(ctx, &old_leader, Duration::from_secs(60)).await?;
        if !wait_recovery_done(ctx, -1.0, Duration::from_secs(60)).await? {
            cleanup.run().await?;
            return Ok(Verdict::Fail("new leader never completed recovery".into()));
        }

        // Floor should still be PROBE_FLOOR. If recovery's hydration
        // wrote it back to 0 (the bug), this fails.
        let after: i64 =
            sqlx::query_scalar("SELECT floor_mem_bytes FROM derivations WHERE derivation_id = $1")
                .bind(drv_id)
                .fetch_one(ctx.pg())
                .await?;
        cleanup.run().await?;

        if after == PROBE_FLOOR {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "floor_mem_bytes not preserved across recovery: expected {PROBE_FLOOR}, got {after}"
            )))
        }
    }
}
