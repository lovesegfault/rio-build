//! I-167: drv name containing `?id=` breaks synth_db generation.
//!
//! nixpkgs has fetchurl derivations with URL-derived names like
//! `style.css?id=abc123`. The `?` and `=` survive into the .drv store
//! path's name segment; rio-store's synth_db keying choked on them.
//! Assert: a build with such a name completes without store-side
//! `invalid path component` errors.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct DrvNameQuery;

#[async_trait]
impl Scenario for DrvNameQuery {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i167-drv-name-query",
            i_ref: Some(167),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(90),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // The `?` and `=` are the I-167 trigger; `currentTime` keeps
        // the drv hash unique across runs.
        let expr = format!(
            r#"{BUSYBOX_LET} builtins.derivation {{
              name = "style.css?id=i167-${{toString builtins.currentTime}}";
              system = "x86_64-linux";
              builder = "${{busybox}}";
              args = ["sh" "-c" "echo i167 > $out"];
            }}"#
        );

        match ctx.nix_build_expr_via_gateway(0, &expr).await {
            Ok(()) => Ok(Verdict::Pass),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("invalid path component") || msg.contains("synth_db") {
                    Ok(Verdict::Fail(format!(
                        "I-167 regression — drv name with `?id=` rejected: {msg}"
                    )))
                } else {
                    Err(e)
                }
            }
        }
    }
}
