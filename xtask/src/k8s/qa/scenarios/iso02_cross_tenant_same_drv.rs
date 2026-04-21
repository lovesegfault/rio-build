//! Cross-tenant: B builds A's drv → separate attribution, shared
//! result.
//!
//! Policy (from `r[sched.tenant.authz+2]` + security.md): SubmitBuild
//! attributes to `claims.sub`. Derivations are content-addressed and
//! the store is shared, so B is ALLOWED to build the same drv. The
//! isolation guarantee is *attribution*, not drv-hash exclusivity:
//! B's submission creates a build under B's `tenant_id`, distinct from
//! A's. (The drv's outputs may already exist from A's run — that's
//! intended dedup; B's build cache-hits.)
//!
//! Assert: after both submit, `rio-cli builds` filtered by each
//! tenant's UUID shows ≥1 build for each. The same drv path appearing
//! under both tenants is the expected (and documented) behavior.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct CrossTenantSameDrv;

#[async_trait]
impl Scenario for CrossTenantSameDrv {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "iso02-cross-tenant-same-drv",
            i_ref: None,
            isolation: Isolation::Tenant { count: 2 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let a_uuid = ctx.tenant_uuid(0)?;
        let b_uuid = ctx.tenant_uuid(1)?;

        // smoke_expr embeds builtins.currentTime — fix it once so A and
        // B submit the IDENTICAL drv (same content hash).
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let expr = format!(
            r#"{BUSYBOX} builtins.derivation {{
              name = "rio-qa-iso02-{nonce}";
              system = "x86_64-linux";
              builder = "${{busybox}}";
              args = ["sh" "-c" "echo iso02 > $out"];
            }}"#,
            BUSYBOX = crate::k8s::eks::smoke::BUSYBOX_LET
        );

        // A builds. Then B builds the SAME expression.
        ctx.nix_build_expr_via_gateway(0, &expr).await?;
        // B's submission must not be PERMISSION_DENIED; the drv is
        // content-addressed, B is allowed to build it.
        ctx.nix_build_expr_via_gateway(1, &expr).await?;

        let count_for = |uuid: &str| -> Result<usize> {
            let resp: Value = ctx.cli_json(&["builds", "--tenant", uuid])?;
            Ok(resp
                .get("builds")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0))
        };
        let n_a = count_for(&a_uuid)?;
        let n_b = count_for(&b_uuid)?;

        if n_a == 0 || n_b == 0 {
            return Ok(Verdict::Fail(format!(
                "expected ≥1 build for each tenant after both submitted the \
                 same drv; got A={n_a} B={n_b} — submission may be \
                 mis-attributed or one was rejected"
            )));
        }
        Ok(Verdict::Pass)
    }
}
