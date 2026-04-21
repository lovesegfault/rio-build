//! I-208: `derivations.size_class_floor` must survive merge.
//!
//! Seed a floor value on an existing derivation, submit a build that
//! re-merges it, assert the PG floor was NOT reset to NULL/tiny.
//! Live trigger needs the same .drv hash across runs — `smoke_expr`
//! defeats that with `currentTime`. So this asserts the persistence
//! direction: write a floor, restart scheduler, read it back. If
//! recovery loads it, merge will too (same `from_row` codepath).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;

use super::common::{NS_SYSTEM, PgCleanup, kill_pod, wait_new_leader, wait_recovery_done};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct FloorHydrated;

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
        // Find one terminal derivation with NULL floor and a non-null
        // drv_path (so it was a real build, not synthetic).
        let row = sqlx::query(
            "SELECT derivation_id, derivation_hash FROM derivations \
             WHERE size_class_floor IS NULL AND drv_path IS NOT NULL \
             AND status IN ('completed','poisoned') LIMIT 1",
        )
        .fetch_optional(ctx.pg())
        .await?;
        let Some(row) = row else {
            return Ok(Verdict::Skip(
                "no terminal derivation with NULL floor to test against".into(),
            ));
        };
        let drv_id: sqlx::types::Uuid = row.try_get("derivation_id")?;

        sqlx::query("UPDATE derivations SET size_class_floor = 'small' WHERE derivation_id = $1")
            .bind(drv_id)
            .execute(ctx.pg())
            .await?;
        let cleanup = PgCleanup::new(
            ctx.pg(),
            format!(
                "UPDATE derivations SET size_class_floor = NULL WHERE derivation_id = '{drv_id}'"
            ),
        );

        let old_leader = ctx.scheduler_leader().await?;
        kill_pod(ctx, NS_SYSTEM, &old_leader)?;
        wait_new_leader(ctx, &old_leader, Duration::from_secs(60)).await?;
        if !wait_recovery_done(ctx, -1.0, Duration::from_secs(60)).await? {
            cleanup.run().await?;
            return Ok(Verdict::Fail("new leader never completed recovery".into()));
        }

        // Floor should still be 'small'. If recovery's hydration path
        // wrote it back to NULL (the bug), this fails.
        let after: Option<String> =
            sqlx::query_scalar("SELECT size_class_floor FROM derivations WHERE derivation_id = $1")
                .bind(drv_id)
                .fetch_one(ctx.pg())
                .await?;
        cleanup.run().await?;

        match after.as_deref() {
            Some("small") => Ok(Verdict::Pass),
            other => Ok(Verdict::Fail(format!(
                "size_class_floor not preserved across recovery: expected 'small', got {other:?}"
            ))),
        }
    }
}
