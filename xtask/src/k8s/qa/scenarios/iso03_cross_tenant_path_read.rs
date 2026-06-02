//! Cross-tenant: B cannot read A's output via narinfo.
//!
//! Policy (`r[store.tenant.narinfo-filter]`, security.typ): the
//! store-side `sig_visibility_gate` filters `QueryPathInfo` by
//! `path_tenants.tenant_id = claims.sub`. The underlying chunks are
//! shared (security.typ — content-addressed, immutable), but a
//! tenant only sees narinfo for paths attributed to them via
//! `path_tenants`. So B's `nix copy --from ssh-ng://...key=B {A's
//! out}` should fail at the QueryPathInfo step ("path is not valid"
//! or similar).
//!
//! This is the documented policy. If the copy SUCCEEDS, either (a)
//! the path was attributed to B too (e.g. via shared input substitution
//! — `path_tenants` may add a row for B if B's build-graph references
//! it), or (b) the gate is broken. We control for (a) by using a
//! UNIQUE output that only A's build produces.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::sh::{self, cmd, shell};

pub struct CrossTenantPathRead;

#[async_trait]
impl Scenario for CrossTenantPathRead {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "iso03-cross-tenant-path-read",
            i_ref: None,
            isolation: Isolation::Tenant { count: 2 },
            timeout: Duration::from_secs(180),
            exercises: crate::exercises!(),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let expr = format!(
            r#"{BUSYBOX_LET} builtins.derivation {{
              name = "rio-qa-iso03-{nonce}";
              system = "x86_64-linux";
              builder = "${{busybox}}";
              args = ["sh" "-c" "echo iso03-{nonce} > $out"];
            }}"#
        );

        // Build as A and capture the output path. build_expr's `nix
        // build --print-out-paths` writes to stdout but the helper
        // discards it; instantiate locally to derive the out path.
        let drv = {
            let s = shell()?;
            sh::run_read(cmd!(s, "nix-instantiate --expr {expr}")).await?
        };
        ctx.nix_build_expr_via_gateway(0, &expr).await?;
        // `nix derivation show` keys the output spec differently across
        // CA-mode/nix-versions; `nix-store -q --outputs` is the
        // unambiguous "what paths does this drv produce" query.
        let target_out = {
            let s = shell()?;
            let outs = sh::run_read(cmd!(s, "nix-store -q --outputs {drv}")).await?;
            outs.lines()
                .next()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("nix-store -q --outputs {drv}: empty"))?
        };

        // B attempts to copy A's output. Capture stderr so the
        // assertion on the failure *reason* works regardless of
        // verbose mode — sh::run's `-v` path bails with the exit
        // status only (see sh::run_capture's doc).
        let (b_store, _g) = ctx.gateway_tunnel(1).await?;
        let sshopts = crate::k8s::shared::NIX_SSHOPTS_BASE;
        let s = shell()?;
        let (status, out) = sh::run_capture(
            cmd!(
                s,
                "timeout 30 nix copy --no-check-sigs --from {b_store} {target_out}"
            )
            .env("NIX_SSHOPTS", sshopts),
        )
        .await?;

        if status.success() {
            return Ok(Verdict::Fail(format!(
                "B successfully copied A's output {target_out} — \
                 r[store.tenant.narinfo-filter] not applied (or {target_out} was \
                 attributed to B via path_tenants)"
            )));
        }
        // Expected: narinfo-filter denies → "is not valid" /
        // "does not exist" / "path … not in store".
        if out.contains("not valid")
            || out.contains("does not exist")
            || out.contains("not in store")
            || out.contains("NotFound")
        {
            // Build-side half (`r[store.tenant.valid-paths-filter]`):
            // B builds the SAME expression A built — same .drv. B's
            // validity check must NOT count A's .drv as valid for B;
            // B's client re-uploads it, the idempotent skip writes B's
            // junction row, and the build runs. Before the fix this
            // died with `max_infra_retries=10 exhausted` on the
            // builder's castore-FUSE read of the .drv (the two-tenant
            // brick this scenario exists for).
            ctx.nix_build_expr_via_gateway(1, &expr)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "B building the expr A already built must succeed \
                         (valid-paths tenant scoping + junction self-heal): {e:#}"
                    )
                })?;
            Ok(Verdict::Pass)
        } else {
            // `timeout` exits 124 on timeout — distinguish a transport
            // hang (not a policy decision) from an actual error.
            let kind = if status.code() == Some(124) {
                "timed out (124) — transport hang, not a narinfo decision"
            } else {
                "failed but not with the narinfo-filter signature"
            };
            Ok(Verdict::Fail(format!(
                "B's copy of A's output {kind}: {out}"
            )))
        }
    }
}
