//! I-209/I-210: failure paths must terminal the `assignments` row.
//!
//! Submit a build that PERMANENTLY fails (non-zero exit), then assert
//! its assignment row is NOT `status='pending'`. Pre-fix, only the
//! success path called `update_assignment_status`; failure/cascade/
//! cancel left rows pending forever (12,626 on the live cluster).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct AssignmentTerminal;

#[async_trait]
impl Scenario for AssignmentTerminal {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i209-assignment-terminal",
            i_ref: Some(209),
            isolation: Isolation::Exclusive {
                // Mutates Scheduler dispatch state via a build, and
                // we read PG. Declared coarse on Scheduler so it
                // doesn't overlap leader-kill scenarios.
                mutates: &[Component::Scheduler, Component::Postgres],
            },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // `exit 1` → PermanentFailure (deterministic, not infra).
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = format!("rio-qa-i209-fail-{nonce}");
        let expr = format!(
            r#"{BUSYBOX_LET} builtins.derivation {{
              name = "{name}";
              system = "x86_64-linux";
              builder = "${{busybox}}";
              args = ["sh" "-c" "echo i209; exit 1"];
            }}"#
        );

        // Expected to fail — swallow the Err (it's the success case).
        let _ = ctx.nix_build_expr_via_gateway(0, &expr).await;

        // Find the derivation by name fragment, then its assignments.
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             WHERE d.drv_path LIKE $1 AND a.status = 'pending'",
        )
        .bind(format!("%{name}%"))
        .fetch_one(ctx.pg())
        .await?;

        let terminal: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             WHERE d.drv_path LIKE $1 AND a.status <> 'pending'",
        )
        .bind(format!("%{name}%"))
        .fetch_one(ctx.pg())
        .await?;

        if terminal == 0 && pending == 0 {
            return Ok(Verdict::Skip(
                "no assignment row found for failing drv — dispatch path \
                 didn't reach assignment (FOD-only? check name match)"
                    .into(),
            ));
        }
        if pending == 0 {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{pending} assignment row(s) left status='pending' for a \
                 PermanentFailure derivation — failure path not terminal-ing"
            )))
        }
    }
}
