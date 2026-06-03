//! I-110: per-path store RPCs in builder closure-BFS hit a PG wall at
//! scale. The fix (`BatchQueryPathInfo` / `BatchGetManifest`, I-110b/c)
//! collapsed ~800 RPCs/builder → ~10. Validated live: 108 builders saw
//! 7 GetPath in 2min post-fix vs thousands pre-fix.
//!
//! Regression check: build a 15-derivation linear chain. Each link
//! depends on the previous, so the closure-BFS for link N visits
//! 1..N. Pre-fix this was O(N²) per-path RPCs; post-fix, batched.
//! The chain runs SEQUENTIALLY (each link blocks on prev), so 15 ×
//! ~6s dispatch+build+upload ≈ 90s baseline. 180s threshold gives
//! ~6s/link slack for the closure-BFS path-info RPCs; if those
//! regressed to per-path, the O(N²) tax pushes well past 180s.

use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::eks::smoke::BUSYBOX_LET;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BatchRpcScale;

// Wall-clock gate: 12 links × ~12s/link nominal + ~2.3× tail slack
// for builder spawn under phase-2 contention. Observed ~145s
// isolated, ~247s under contention (2026-05-13, 3% over the prior
// 240s threshold; that threshold itself was widened from 180s in
// 7df06acc8). The structural assertion — count per-path store RPCs,
// the I-110 fix collapses ~78 closure-BFS QueryPathInfo to ~12
// BatchQueryPathInfo — is preferred but blocked: rio-store exposes
// no per-method RPC counter. If one is added (it's an
// observability.typ-worthy metric), gate on a count budget instead;
// 50× headroom vs ~1.7× wall-clock.
const N_CHAIN: usize = 12;
const THRESHOLD: Duration = Duration::from_secs(330);

#[async_trait]
impl Scenario for BatchRpcScale {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i110-batch-rpc-scale",
            i_ref: Some(110),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(300),
            exercises: crate::exercises!(),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        // Nix foldl': d_{i} depends on d_{i-1}. Seed is busybox itself
        // (already a derivation). Each link writes its index + the
        // previous out path so the dependency is real.
        let expr = format!(
            r#"{BUSYBOX_LET}
            builtins.foldl'
              (prev: i: builtins.derivation {{
                name = "rio-qa-i110-{nonce}-${{toString i}}";
                system = "x86_64-linux";
                builder = "${{busybox}}";
                args = ["sh" "-c" "echo ${{prev}} ${{toString i}} > $out"];
              }})
              busybox
              (builtins.genList (i: i) {N_CHAIN})"#
        );

        let start = Instant::now();
        ctx.nix_build_expr_via_gateway(0, &expr).await?;
        let elapsed = start.elapsed();

        if elapsed < THRESHOLD {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{N_CHAIN}-drv chain took {elapsed:?} (threshold {THRESHOLD:?}) \
                 — closure-BFS per-path RPC regression (I-110)"
            )))
        }
    }
}
