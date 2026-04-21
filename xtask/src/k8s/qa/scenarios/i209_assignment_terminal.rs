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

        // The gateway returns the failure as soon as the builder
        // reports it; the scheduler's actor commits the assignment
        // status update on its own tick. Poll until the row appears.
        let pat = format!("%{name}%");
        let q = |pred: &'static str| {
            let pat = pat.clone();
            let pg = ctx.pg().clone();
            async move {
                sqlx::query_scalar::<_, i64>(&format!(
                    "SELECT COUNT(*) FROM assignments a \
                     JOIN derivations d ON d.derivation_id = a.derivation_id \
                     WHERE d.drv_path LIKE $1 AND a.status {pred}"
                ))
                .bind(pat)
                .fetch_one(&pg)
                .await
            }
        };
        let row =
            super::common::poll_until(Duration::from_secs(30), Duration::from_secs(2), || async {
                let pending = q("= 'pending'").await?;
                let terminal = q("<> 'pending'").await?;
                Ok((pending + terminal > 0).then_some((pending, terminal)))
            })
            .await?;

        match row {
            None => Ok(Verdict::Fail(format!(
                "no assignment row for {name} within 30s of build failure — \
                 dispatch never wrote one (check derivations.drv_path match)"
            ))),
            Some((0, _)) => Ok(Verdict::Pass),
            Some((pending, _)) => Ok(Verdict::Fail(format!(
                "{pending} assignment row(s) still status='pending' for a \
                 PermanentFailure derivation — failure path not terminal-ing"
            ))),
        }
    }
}
