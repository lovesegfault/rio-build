//! I-086: a Pool reconcile failure must be LOUD — `WARN` log +
//! `rio_controller_reconcile_errors_total` increment, not silent.
//!
//! Original symptom: FetcherPool stuck `READY 0` for 22min with zero
//! controller log lines mentioning it. The error_policy on its
//! Controller builder swallowed the error.
//!
//! Probe: apply a deliberately-invalid Pool CR (bad spec), wait one
//! reconcile cycle, assert the metric and log both react. Then delete
//! the CR.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::common::{NS_SYSTEM, first_pod, logs_since_contain, poll_until};
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct ReconcileErrorLoud;

const BAD_POOL: &str = r#"
apiVersion: rio.build/v1alpha1
kind: Pool
metadata:
  name: qa-i086-bad
  namespace: rio-system
spec:
  kind: Builder
  systems: ["x86_64-linux"]
  features: []
  sizeClasses:
    - name: bad
      cpu: "not-a-quantity"
      memory: 1Gi
"#;

#[async_trait]
impl Scenario for ReconcileErrorLoud {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i086-reconcile-error-loud",
            i_ref: Some(86),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Controller],
            },
            timeout: Duration::from_secs(120),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let ctrl = first_pod(ctx, NS_SYSTEM, "rio-controller")?;
        let before = super::common::scrape_controller(ctx)
            .await?
            .sum("rio_controller_reconcile_errors_total");

        // Apply via kubectl --apply -f - (stdin). The CRD's CEL/
        // OpenAPI may reject `not-a-quantity` at admission — that's
        // ALSO a Pass: the protection is at a tighter layer than
        // I-086 needed. Only if admission ACCEPTS do we check the
        // controller's reconcile-error path.
        let apply = {
            let s = crate::sh::shell()?;
            crate::sh::try_read(crate::sh::cmd!(s, "kubectl apply -f -").stdin(BAD_POOL))
        };
        if let Err(e) = &apply {
            tracing::info!(
                "admission rejected bad Pool (CRD validation tighter than \
                 controller-side parse): {e:#}"
            );
            return Ok(Verdict::Pass);
        }

        let result = poll_until(Duration::from_secs(45), Duration::from_secs(5), || async {
            let now = super::common::scrape_controller(ctx)
                .await?
                .sum("rio_controller_reconcile_errors_total");
            Ok((now > before).then_some(()))
        })
        .await?;

        let log_hit = !logs_since_contain(ctx, NS_SYSTEM, &ctrl, 60, "qa-i086-bad")?.is_empty();

        // Cleanup.
        let _ = ctx.kubectl(&[
            "-n",
            NS_SYSTEM,
            "delete",
            "pool",
            "qa-i086-bad",
            "--ignore-not-found",
        ]);

        match (result.is_some(), log_hit) {
            (true, true) => Ok(Verdict::Pass),
            (true, false) => Ok(Verdict::Fail(
                "reconcile_errors_total incremented but no log line mentions \
                 qa-i086-bad — silent at log level"
                    .into(),
            )),
            (false, _) => Ok(Verdict::Fail(format!(
                "rio_controller_reconcile_errors_total stayed at {before} after \
                 applying invalid Pool — error_policy swallowing failures"
            ))),
        }
    }
}
