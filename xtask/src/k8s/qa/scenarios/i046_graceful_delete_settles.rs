//! I-046/I-091 (re-pointed by bug_389): a GRACEFUL builder pod delete
//! must SETTLE the in-flight attempt — the scheduler closes the
//! killed attempt (assignment leaves `pending`) within the window,
//! via the pull-mode report path (`ReportOutcome`) or the
//! establishment sweep, and the build is re-dispatchable.
//!
//! History: this scenario asserted a grep for `not leader (standby
//! replica)` in the builder log after SIGTERM — the signature of the
//! stream-era admin drain hop. That RPC was deleted; the grep
//! could never match again, so the scenario was vacuous-pass forever
//! (or spurious-fail on an unrelated standby warn). The settlement
//! assert is the live property; builder logs are DIAGNOSTICS on
//! failure, never a verdict.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::poll_until;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct GracefulDeleteSettles;

#[async_trait]
impl Scenario for GracefulDeleteSettles {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i046-graceful-delete-settles",
            i_ref: Some(46),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Builders, Component::Scheduler],
            },
            timeout: Duration::from_secs(240),
            // The settle path is the pull-mode executor surface: the
            // dying builder reports (or the scheduler's establishment
            // sweep closes) and the re-dispatch is a fresh pull.
            exercises: crate::exercises!(
                rio_proto::ExecutorServiceClient<tonic::transport::Channel> =>
                    pull_assignment(rio_proto::types::PullAssignmentRequest),
                    report_outcome(rio_proto::types::ReportOutcomeRequest)
            ),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // Ensure ≥1 running builder working on a recognizable drv.
        let bg = ctx.nix_build_via_gateway_bg(0, "i046", 60, 1);
        let pod = poll_until(Duration::from_secs(60), Duration::from_secs(3), || async {
            Ok(ctx
                .running_pods(QaCtx::NS_BUILDERS, QaCtx::BUILDER_LABEL)?
                .into_iter()
                .next())
        })
        .await?;
        let Some(pod) = pod else {
            bg.abort();
            return Ok(Verdict::Fail(
                "no builder pod within 60s of submitting a build \
                 — dispatch/spawn-intent path broken"
                    .into(),
            ));
        };

        // Premise: an assignment row exists for the i046 drv before
        // the kill (otherwise the settlement assert is vacuous).
        let pg = ctx.pg().clone();
        let total_before: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             WHERE d.drv_path LIKE $1",
        )
        .bind("%i046%")
        .fetch_one(&pg)
        .await?;
        if total_before == 0 {
            bg.abort();
            return Ok(Verdict::Fail(
                "no assignment row for the i046 drv before the graceful \
                 delete — settlement assert would be vacuous"
                    .into(),
            ));
        }

        // Graceful delete (default grace period) — SIGTERM → the
        // builder's drain path reports / the scheduler sweep closes.
        ctx.kubectl(&[
            "-n",
            QaCtx::NS_BUILDERS,
            "delete",
            "pod",
            &pod,
            "--wait=false",
        ])?;

        // SETTLEMENT: within the window, the killed attempt must
        // leave `pending` (closed failed/completed; a re-dispatched
        // fresh attempt may add new rows — we assert at least one
        // settled row appears, i.e. closure happened, not which
        // verdict won the race).
        let settled = poll_until(Duration::from_secs(120), Duration::from_secs(3), || async {
            let pg = ctx.pg().clone();
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM assignments a \
                 JOIN derivations d ON d.derivation_id = a.derivation_id \
                 WHERE d.drv_path LIKE $1 AND a.status <> 'pending'",
            )
            .bind("%i046%")
            .fetch_one(&pg)
            .await?;
            Ok((n > 0).then_some(n))
        })
        .await?;
        bg.abort();

        match settled {
            Some(_) => Ok(Verdict::Pass),
            None => {
                // Diagnostics only (the INVERTED grep): standby-warn
                // lines in the builder log are acceptable when
                // settlement happened; here settlement did NOT happen,
                // so surface whatever the dying builder logged.
                let logs = ctx
                    .kubectl(&["-n", QaCtx::NS_BUILDERS, "logs", &pod, "--tail=20"])
                    .unwrap_or_else(|e| format!("(logs unavailable: {e})"));
                Ok(Verdict::Fail(format!(
                    "graceful delete of {pod} did not settle the i046 \
                     attempt within 120s (assignment still pending) — \
                     pull-mode report/establishment-sweep closure \
                     regression. Last builder log lines:\n{logs}"
                )))
            }
        }
    }
}
