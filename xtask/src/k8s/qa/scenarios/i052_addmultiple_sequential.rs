//! I-052: gateway's `wopAddMultipleToStore` processed entries
//! sequentially inside the stream — read entry → `PutPath().await` →
//! next. At 41ms mean × 45,766 paths = ~31min. The fix pipelines
//! PutPath as a bounded JoinSet while the next entry's wire bytes
//! are read.
//!
//! Regression check: realize 30 small independent derivations LOCALLY,
//! then `nix copy --to ssh-ng://...` the closure. Time ONLY the copy.
//! 30 × 41ms sequential = ~1.2s; pipelined ≈ a few hundred ms. We
//! assert < 30s — not a tight perf gate, just catches "regressed to
//! sequential at scale" if a future change re-serializes.

use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::sh::{self, cmd, shell};

pub struct AddMultipleSequential;

const N_PATHS: usize = 30;
const THRESHOLD: Duration = Duration::from_secs(30);

#[async_trait]
impl Scenario for AddMultipleSequential {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i052-addmultiple-sequential",
            i_ref: Some(52),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(180),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        // 30 INDEPENDENT derivations (no inter-deps) so the closure
        // push is one wopAddMultipleToStore with ~30+busybox entries.
        // Build LOCALLY first; the timed step is the copy. A list-of-
        // drvs expression realised via `nix build --expr`.
        let expr = format!(
            r#"{BUSYBOX_LET}
            builtins.genList
              (i: builtins.derivation {{
                name = "rio-qa-i052-{nonce}-${{toString i}}";
                system = builtins.currentSystem;
                builder = "${{busybox}}";
                args = ["sh" "-c" "echo i052-{nonce}-${{toString i}} > $out"];
              }})
              {N_PATHS}"#
        );

        // Realise locally; capture out paths.
        let outs = {
            let s = shell()?;
            sh::run_read(cmd!(
                s,
                "nix build --impure --no-link --print-out-paths --expr {expr}"
            ))
            .await?
        };
        let out_paths: Vec<&str> = outs.lines().filter(|l| !l.is_empty()).collect();
        if out_paths.len() < N_PATHS {
            return Ok(Verdict::Fail(format!(
                "local realise produced {} paths (expected {N_PATHS})",
                out_paths.len()
            )));
        }

        // Time the copy. NIX_SSHOPTS for IdentitiesOnly etc.
        let (store, _g) = ctx.gateway_tunnel(0).await?;
        let sshopts = crate::k8s::shared::NIX_SSHOPTS_BASE;
        let start = Instant::now();
        {
            let s = shell()?;
            sh::run(cmd!(s, "nix copy --to {store} {out_paths...}").env("NIX_SSHOPTS", sshopts))
                .await?;
        }
        let elapsed = start.elapsed();

        if elapsed < THRESHOLD {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "copy of {N_PATHS} small paths took {elapsed:?} (threshold \
                 {THRESHOLD:?}) — wopAddMultipleToStore sequential regression"
            )))
        }
    }
}
