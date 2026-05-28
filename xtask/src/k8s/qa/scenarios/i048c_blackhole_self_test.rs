//! I-048c: assert the fault injector actually injects faults.
//!
//! Guards against the silent-no-op failure mode (I-056 class) where the
//! injector reports success but traffic is unaffected — the exact risk
//! that ruled out Chaos Mesh / Litmus on the v6-only cluster. This is
//! also the regression check for chaos.rs's own ip6tables path.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

use crate::k8s::chaos::{self, ChaosFrom, ChaosKind, ChaosTarget};
use crate::k8s::eks::smoke::CliCtx;
use crate::k8s::qa::{Component, Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct BlackholeSelfTest;

const METRIC: &str = "rio_scheduler_worker_disconnects_total";
const KEEPALIVE_WINDOW: Duration = Duration::from_secs(45);

#[async_trait]
impl Scenario for BlackholeSelfTest {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i048c-blackhole-self-test",
            i_ref: Some(48),
            isolation: Isolation::Exclusive {
                mutates: &[Component::Scheduler, Component::Builders],
            },
            timeout: Duration::from_secs(240),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // The chaos can only break a stream the scheduler actually
        // holds. The precondition must be IDENTITY-based, not gauge-
        // based: a nonzero fleet gauge alone was satisfied
        // 2.6s into a full-QA run by a leftover from the previous
        // Exclusive scenario (i046's mid-drain builder) — long before
        // the warmup builder could spawn — and that worker exited or
        // had already been reaped before the CCNP propagated, leaving
        // the keepalive nothing to time out on (observed 2026-05-13:
        // metric flat for 75s, FAIL).
        //
        // Mirror i033's pattern: snapshot `DebugListExecutors`
        // (the actor's in-memory executor map, `has_stream`-filtered),
        // submit a 90s warmup build, and gate on a *fresh* live
        // executor that wasn't in the snapshot. The fresh executor is
        // the warmup builder by construction — i048c is `Exclusive`,
        // nothing else dispatches concurrently. If dispatch is broken
        // (i033's failure mode) this fails LOUDLY with "no fresh
        // executor", which is the correct triage signal — better than
        // applying a no-op chaos to a phantom and reporting a
        // misleading "egressDeny not enforced".
        //
        // Fresh CliCtx, not `ctx.cli`: the shared handle is opened
        // before phase 2 and a prior leader-kill (i024) leaves it
        // forwarding to a dead pod or a standby (whose
        // `DebugListExecutors` is intentionally empty — see proto.typ).
        let cli = CliCtx::open(&ctx.kube, 0, 0).await?;
        let before_execs = super::common::live_executor_ids(&cli)?;

        let bg = ctx.nix_build_via_gateway_bg(0, "i048c-warmup", 90, 1);
        let connected =
            super::common::poll_until(Duration::from_secs(90), Duration::from_secs(3), || async {
                let now: HashSet<String> = super::common::live_executor_ids(&cli)?;
                let fresh = now.difference(&before_execs).count();
                Ok((fresh > 0).then_some(fresh))
            })
            .await?;
        if connected.is_none() {
            // Disambiguate "dispatch broken" from "warmup build never
            // submitted". The bg task is port-forward → SSH-banner poll
            // (≤80s) → nix-instantiate → nix copy → nix build; any leg
            // can fail or stall before the build reaches the gateway —
            // and phase 2 serializes on Scheduler, so nothing else is
            // dispatching while we poll: the *only* fresh executor that
            // can appear is the warmup builder. If the build was never
            // submitted, the poll has zero chance and the failure is a
            // host-side/setup problem, not a spawn-intent bug. Observed
            // on a 2026-05-19 fresh-deploy full-QA run: gateway logs had
            // no i048c-warmup SSH session, controller had no spawn
            // intent — the bg task died silently and `bg.abort()` ate
            // the evidence. Instrument-first (same pattern as
            // 166e6f5fe / i209): surface the bg task's state so the
            // next failure points at a leg.
            let bg_state = if bg.is_finished() {
                match bg.await {
                    Ok(Ok(())) => "completed Ok — build was submitted and \
                         finished but no fresh executor was observed (cache \
                         hit? cli forwarding to a standby with an empty \
                         executor map?)"
                        .to_string(),
                    Ok(Err(e)) => format!("errored: {e:#}"),
                    Err(je) if je.is_panic() => {
                        let p = je.into_panic();
                        let msg = p
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_else(|| "non-string panic payload".into());
                        format!("panicked: {msg}")
                    }
                    Err(je) => format!("join error: {je}"),
                }
            } else {
                bg.abort();
                "still running at poll deadline — port-forward / SSH-banner \
                 / nix-instantiate / nix-copy stuck or slow; warmup build \
                 likely never reached the gateway"
                    .to_string()
            };
            return Ok(Verdict::Fail(format!(
                "no fresh executor (has_stream=true, not in pre-warmup \
                 DebugListExecutors snapshot) within 90s of submitting a \
                 build. warmup bg task: {bg_state}. (only suspect the \
                 dispatch/spawn-intent path if the bg task completed Ok)"
            )));
        }

        let before = ctx.scrape_scheduler().await?.sum(METRIC);

        let dir = crate::sh::repo_root().join(".stress-test/chaos");
        std::fs::create_dir_all(&dir)?;
        if let Err(e) = chaos::remediate(&dir).await {
            tracing::warn!("stale-chaos remediation: {e:#}");
        }
        // chaos::run includes CCNP-create + wait-Valid + a ~3s
        // propagation grace before the deny is actually enforced on
        // every node (cilium-operator validates fast; per-agent
        // endpoint regen is what the grace covers). Run the blackhole
        // for KEEPALIVE_WINDOW + 30s startup-slack and poll the metric
        // for the WHOLE duration; any increment is Pass. Live spike
        // 2026-05-13 saw the increment at ~30s — the slack is for tail.
        let chaos_dur = KEEPALIVE_WINDOW + Duration::from_secs(30);
        let chaos_fut = chaos::run(
            &dir,
            ChaosKind::Blackhole,
            ChaosTarget::Scheduler,
            ChaosFrom::AllWorkers,
            chaos_dur,
        );
        tokio::pin!(chaos_fut);

        let mut incremented = false;
        let mut samples = vec![before];
        loop {
            tokio::select! {
                r = &mut chaos_fut => { r?; break; }
                _ = sleep(Duration::from_secs(5)) => {
                    if !incremented {
                        // Swallow scrape errors (port-forward can blip
                        // during chaos) instead of `?`-ing out and
                        // losing the chaos cleanup.
                        if let Ok(s) = ctx.scrape_scheduler().await {
                            let now = s.sum(METRIC);
                            samples.push(now);
                            if now > before {
                                incremented = true;
                            }
                        }
                    }
                }
            }
        }
        // The scheduler-side h2 keepalive is 30s interval + 20s timeout
        // = ~50s detect. If the worker reconnected AFTER the blackhole
        // lifted (it has client-side 30+10s) and the scheduler's old
        // stream is still half-open, the disconnect counter bumps when
        // the new stream replaces the old. Poll a final time post-chaos.
        if !incremented {
            sleep(Duration::from_secs(10)).await;
            if let Ok(s) = ctx.scrape_scheduler().await {
                let now = s.sum(METRIC);
                samples.push(now);
                incremented = now > before;
            }
        }

        bg.abort();
        if incremented {
            Ok(Verdict::Pass)
        } else {
            Ok(Verdict::Fail(format!(
                "{METRIC} did not increment during {chaos_dur:?} blackhole \
                 (samples: {samples:?}) — CCNP egressDeny not enforced \
                 (check `kubectl get ccnp rio-chaos-blackhole -o yaml` \
                 for Valid condition + that builder/fetcher endpoints \
                 are policy-enforced) or scheduler-side keepalive \
                 not detecting"
            )))
        }
    }
}
