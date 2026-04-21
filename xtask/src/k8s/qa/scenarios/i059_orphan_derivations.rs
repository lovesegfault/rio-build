//! I-059: PG orphan derivations (no `build_derivations` link) MUST NOT
//! be dispatched after recovery.
//!
//! Seed a derivation row with no interested build, restart the
//! scheduler, assert it does not appear in the actor's running set
//! (and isn't dispatched). The fix gates recovery's
//! compute_initial_states on `interested_builds` non-empty.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;

use super::common::{NS_SYSTEM, PgCleanup, kill_pod, wait_new_leader, wait_recovery_done};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct OrphanDerivations;

#[async_trait]
impl Scenario for OrphanDerivations {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i059-orphan-derivations",
            i_ref: Some(59),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler, Component::Postgres],
            },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Seed: one Ready derivation with NO build_derivations link.
        // Hash is synthetic so it can't collide with a real drv.
        // `system` must be valid or unroutable-ready gauge fires
        // (which is a different signal). Schema (001_scheduler.sql):
        // PK derivation_id UUID, drv_hash TEXT (no UNIQUE constraint
        // by itself, so no ON CONFLICT target — pre-delete instead).
        let drv_hash = "qai059orphan0000000000000000000000000000".to_string();
        sqlx::query("DELETE FROM derivations WHERE drv_hash = $1")
            .bind(&drv_hash)
            .execute(ctx.pg())
            .await?;
        let row = sqlx::query(
            "INSERT INTO derivations (drv_hash, drv_path, system, status) \
             VALUES ($1, $2, 'x86_64-linux', 'ready') \
             RETURNING derivation_id",
        )
        .bind(&drv_hash)
        .bind(format!("/nix/store/{drv_hash}-qa-orphan.drv"))
        .fetch_one(ctx.pg())
        .await?;
        let drv_id: sqlx::types::Uuid = row.try_get("derivation_id")?;
        let cleanup = PgCleanup::new(
            ctx.pg(),
            format!("DELETE FROM derivations WHERE derivation_id = '{drv_id}'"),
        );

        let old_leader = ctx.scheduler_leader().await?;
        kill_pod(ctx, NS_SYSTEM, &old_leader)?;
        wait_new_leader(ctx, &old_leader, Duration::from_secs(60)).await?;
        if !wait_recovery_done(ctx, -1.0, Duration::from_secs(60)).await? {
            cleanup.run().await?;
            return Ok(Verdict::Fail("new leader never completed recovery".into()));
        }

        // Give dispatch a couple of ticks.
        tokio::time::sleep(Duration::from_secs(10)).await;

        // The orphan must NOT have been assigned. Check the
        // assignments table (recovery would dispatch → insert here).
        let n_assignments: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM assignments WHERE derivation_id = $1")
                .bind(drv_id)
                .fetch_one(ctx.pg())
                .await?;

        cleanup.run().await?;

        if n_assignments == 0 {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "orphan derivation (no interested_builds) was dispatched \
                 ({n_assignments} assignment rows) — recovery didn't gate on \
                 build_derivations"
            )))
        }
    }
}
