//! Cross-tenant: B cannot read A's output via narinfo.
//!
//! Policy (`r[store.tenant.narinfo-filter]`, security.md:141): the
//! store-side `sig_visibility_gate` filters `QueryPathInfo` by
//! `path_tenants.tenant_id = claims.sub`. The underlying chunks are
//! shared (security.md:52 — content-addressed, immutable), but a
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
        let out = {
            let s = shell()?;
            // nix derivation show {drv} → {drv: {outputs: {out: {path}}}}
            let json = sh::run_read(cmd!(s, "nix derivation show {drv}")).await?;
            let v: serde_json::Value = serde_json::from_str(&json)?;
            v.as_object()
                .and_then(|o| o.values().next())
                .and_then(|d| d.get("outputs"))
                .and_then(|o| o.get("out"))
                .and_then(|o| o.get("path"))
                .and_then(|p| p.as_str())
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("nix derivation show: no out path for {drv}"))?
        };

        // B attempts to copy A's output. NIX_SSHOPTS for BatchMode etc.
        let (b_store, _g) = ctx.gateway_tunnel(1).await?;
        let sshopts = crate::k8s::shared::NIX_SSHOPTS_BASE;
        let s = shell()?;
        let copy = sh::run(
            cmd!(
                s,
                "timeout 30 nix copy --no-check-sigs --from {b_store} {out}"
            )
            .env("NIX_SSHOPTS", sshopts),
        )
        .await;

        match copy {
            Err(e) => {
                let msg = format!("{e:#}");
                // Expected: narinfo-filter denies → "is not valid" /
                // "does not exist" / "path ... not in store".
                if msg.contains("not valid")
                    || msg.contains("does not exist")
                    || msg.contains("not in store")
                    || msg.contains("NotFound")
                {
                    Ok(Verdict::Pass)
                } else {
                    Ok(Verdict::Fail(format!(
                        "B's copy of A's output failed but not with the \
                         narinfo-filter signature: {msg}"
                    )))
                }
            }
            Ok(()) => Ok(Verdict::Fail(format!(
                "B successfully copied A's output {out} — \
                 r[store.tenant.narinfo-filter] not applied (or {out} was \
                 attributed to B via path_tenants)"
            ))),
        }
    }
}
