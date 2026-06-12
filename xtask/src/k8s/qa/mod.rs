//! `xtask k8s qa` — live-cluster QA suite.
//!
//! Subsumes `smoke`, `stress run`, `stress chaos` into one staged command
//! mirroring the `up --<stage>` pattern. The `--scenarios` stage is the
//! net-new value: a regression registry seeded from `.stress-test/issues/`
//! (live-cluster escapes that VM tests structurally can't catch — Karpenter,
//! NLB, IRSA, real-timing races).
//!
//! Stages run in canonical order; no flags = full sequence:
//!   lint → health → scenarios → load → fault
//!
//! `--lint` is local-only (helm template + config-shape asserts) and is
//! the one stage that could move into `checks.*`. Everything else needs
//! a live cluster.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use async_trait::async_trait;
use clap::Args;
use tracing::info;

use crate::config::XtaskConfig;
use crate::k8s::provider::{Provider, ProviderKind};

pub mod ctx;
mod lint;
pub mod scenarios;
mod scheduler;

pub use ctx::QaCtx;

// ─── stage selection (mirrors UpOpts) ──────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Lint,
    Health,
    Scenarios,
    Load,
    Fault,
}

impl Stage {
    pub const ALL: [Stage; 5] = [
        Stage::Lint,
        Stage::Health,
        Stage::Scenarios,
        Stage::Load,
        Stage::Fault,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stage::Lint => "lint",
            Stage::Health => "health",
            Stage::Scenarios => "scenarios",
            Stage::Load => "load",
            Stage::Fault => "fault",
        }
    }
}

#[derive(Args, Default)]
pub struct QaOpts {
    /// Helm-template + config-shape asserts. Local; no cluster.
    #[arg(long)]
    lint: bool,
    /// NLB health, tunnel, one build, pools reconciled. Replaces `smoke`.
    #[arg(long)]
    health: bool,
    /// Regression registry (I-NNN-seeded). 3-tier scheduler.
    #[arg(long)]
    scenarios: bool,
    /// N parallel builds. Replaces `stress run`.
    #[arg(long)]
    load: bool,
    /// ip6tables blackhole. Replaces `stress chaos`.
    #[arg(long)]
    fault: bool,

    /// Run only scenarios whose id matches (substring). Implies --scenarios.
    #[arg(long = "only", value_name = "ID")]
    only: Vec<String>,
    /// Tenant pool size for Isolation::Tenant scenarios. Capped by the
    /// number of qa tenants the cluster has provisioned.
    #[arg(long, default_value_t = 8)]
    tenant_pool: usize,
    /// Parallel build count for --load.
    #[arg(long = "load-parallel", default_value_t = 8)]
    load_parallel: u16,
    /// Build target for --load (passed to nix-bench flake). Must be a
    /// `packages.x86_64-linux` attribute of the bench flake (default
    /// `~/src/nix-bench/main`); `hello-shallow` is the smallest. The
    /// pre-eval step warns-and-continues if the attribute doesn't
    /// resolve, but the actual `nix build` will fail every parallel
    /// build — so a wrong default makes `--load` deterministically red.
    #[arg(long = "load-target", default_value = "hello-shallow")]
    load_target: String,
    /// Spread --load client starts evenly over this window (e.g. 300s)
    /// instead of all-at-once. 512 simultaneous starts destabilized
    /// the EKS control plane (~5min apiserver blackout, NotReady node
    /// churn from the Karpenter thundering herd).
    #[arg(long = "load-stagger", value_parser = super::chaos::parse_duration_secs, default_value = "0s")]
    load_stagger: Duration,
    /// Blackhole target for --fault. Defaults to scheduler. Label-
    /// selector based — `scheduler` denies all `rio-scheduler` pods,
    /// not just the lease-holder (functionally equivalent: workers
    /// only hold a stream to the leader).
    #[arg(long = "fault-target", value_enum)]
    fault_target: Option<super::chaos::ChaosTarget>,
    /// Blackhole source endpoints for --fault. Defaults to all-workers.
    #[arg(long = "fault-from", value_enum)]
    fault_from: Option<super::chaos::ChaosFrom>,
    /// Blackhole duration for --fault.
    #[arg(long = "fault-duration", value_parser = super::chaos::parse_duration_secs, default_value = "60s")]
    fault_duration: Duration,
}

impl QaOpts {
    fn has(&self, s: Stage) -> bool {
        match s {
            Stage::Lint => self.lint,
            Stage::Health => self.health,
            Stage::Scenarios => self.scenarios || !self.only.is_empty(),
            Stage::Load => self.load,
            Stage::Fault => self.fault,
        }
    }

    /// No stage flags → canonical full sequence. Any flag → flagged subset
    /// in canonical order. Same semantics as `UpOpts::phases`.
    fn stages(&self) -> Vec<Stage> {
        let any = Stage::ALL.iter().any(|&s| self.has(s));
        if !any {
            return Stage::ALL.to_vec();
        }
        Stage::ALL.into_iter().filter(|&s| self.has(s)).collect()
    }

    /// Stage-specific detail appended to the `[N/M] {stage}` banner —
    /// the knobs that explain why one run takes longer than another.
    fn banner_detail(&self, s: Stage) -> String {
        match s {
            Stage::Lint | Stage::Health => String::new(),
            Stage::Scenarios => {
                let n = scenarios::ALL
                    .iter()
                    .filter(|s| {
                        self.only.is_empty() || self.only.iter().any(|f| s.meta().id.contains(f))
                    })
                    .count();
                format!(" — {n} registered, {} tenants", self.tenant_pool)
            }
            Stage::Load => {
                let stagger = if self.load_stagger.is_zero() {
                    String::new()
                } else {
                    format!(", stagger {:?}", self.load_stagger)
                };
                format!(" — {}× {}{stagger}", self.load_parallel, self.load_target)
            }
            Stage::Fault => format!(
                " — blackhole {:?}, {:?}→{:?}",
                self.fault_duration,
                self.fault_from
                    .unwrap_or(super::chaos::ChaosFrom::AllWorkers),
                self.fault_target
                    .unwrap_or(super::chaos::ChaosTarget::Scheduler),
            ),
        }
    }

    fn validate_stage_opts(&self, selected: &[Stage]) -> Result<()> {
        let explicit = selected.len() != Stage::ALL.len();
        if !explicit {
            return Ok(());
        }
        if self.load_parallel != 8 && !selected.contains(&Stage::Load) {
            bail!("--load-parallel requires --load (or omit stage flags to run all)");
        }
        if !self.load_stagger.is_zero() && !selected.contains(&Stage::Load) {
            bail!("--load-stagger requires --load (or omit stage flags to run all)");
        }
        if self.fault_target.is_some() && !selected.contains(&Stage::Fault) {
            bail!("--fault-target requires --fault (or omit stage flags to run all)");
        }
        Ok(())
    }
}

// ─── scenario contract ─────────────────────────────────────────────────

/// What a scenario touches. Coarse — refine to per-arch / per-replica
/// only when the Exclusive set's serial wall-clock proves it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)] // unused variants are API for the remaining 108 candidates
pub enum Component {
    Scheduler,
    Store,
    Gateway,
    Controller,
    Builders,
    Fetchers,
    Postgres,
    S3,
}

#[derive(Debug, Clone)]
pub enum Isolation {
    /// Read-only. Phase 1, unbounded.
    Shared,
    /// Owns `count` ephemeral tenants. Phase 1, bounded by tenant-pool
    /// semaphore. Most scenarios use `count: 1`; cross-tenant isolation
    /// checks (e.g., "tenant A can't read tenant B's outputs") use 2+.
    Tenant { count: usize },
    /// Mutates cluster. Phase 2; concurrent with other Exclusives iff
    /// the combined read/write sets don't conflict — see
    /// [`Scenario::reads`] for the reader/writer model.
    Exclusive { mutates: &'static [Component] },
}

#[derive(Debug, Clone)]
pub struct ScenarioMeta {
    pub id: &'static str,
    /// Links back to `.stress-test/issues/` entry. None for scenarios
    /// that aren't regression-seeded.
    #[allow(dead_code)] // surfaced in report() once that grows --json
    pub i_ref: Option<u16>,
    pub isolation: Isolation,
    pub timeout: Duration,
}

#[async_trait]
pub trait Scenario: Send + Sync {
    fn meta(&self) -> ScenarioMeta;

    /// Components this scenario *depends on* without mutating. The
    /// `mutates` set on `Isolation::Exclusive` is the WRITE set; this is
    /// the READ set. The phase-2 greedy scheduler must not run a
    /// scenario whose read set intersects another in-flight scenario's
    /// write set (and vice versa) — reader/writer semantics. Read-read
    /// overlap is fine.
    ///
    /// Defaults to `[Scheduler]`: almost every scenario submits a build
    /// (or scrapes scheduler metrics), and a build submission during a
    /// concurrent leader kill (e.g. i024) yields a false-positive
    /// "scheduler actor is unavailable" error — the actor exited
    /// gracefully during drain, it didn't panic. Override to `&[]` for
    /// scenarios that genuinely never touch the scheduler (PG-only
    /// invariant checks, gateway key reloads, controller CR probes).
    ///
    /// Only consulted for `Exclusive` scenarios (phase 2); phase-1
    /// `Shared`/`Tenant` scenarios run before any Exclusive starts.
    fn reads(&self) -> &'static [Component] {
        &[Component::Scheduler]
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict>;
}

#[derive(Debug)]
pub enum Verdict {
    Pass,
    /// Precondition not met (e.g., no aarch64 nodes). Reported but
    /// doesn't fail the run.
    Skip(String),
    Fail(String),
}

// ─── entrypoint ────────────────────────────────────────────────────────

pub async fn run(
    opts: QaOpts,
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
) -> Result<()> {
    let selected = opts.stages();
    opts.validate_stage_opts(&selected)?;

    let total = selected.len();
    let run_start = Instant::now();
    for (i, stage) in selected.iter().enumerate() {
        info!(
            "[{}/{total}] {}{}",
            i + 1,
            stage.name(),
            opts.banner_detail(*stage)
        );
        let stage_start = Instant::now();
        let result: Result<()> = async {
            match stage {
                Stage::Lint => lint::run()?,
                Stage::Health => p.smoke(cfg).await?,
                Stage::Scenarios => {
                    scheduler::run(scenarios::ALL, &opts.only, opts.tenant_pool, kind, cfg).await?
                }
                Stage::Load => {
                    let load = super::stress::LoadOpts {
                        target: opts.load_target.clone(),
                        parallel: opts.load_parallel,
                        // base_port 0: each parallel tunnel binds its own
                        // ephemeral local port — no bind races.
                        base_port: 0,
                        bench_flake: None,
                        watch: false,
                        stagger: opts.load_stagger,
                    };
                    super::stress::cmd_run(p, kind, cfg, &load).await?
                }
                Stage::Fault => {
                    use super::chaos;
                    let dir = crate::sh::repo_root().join(".stress-test/chaos");
                    std::fs::create_dir_all(&dir)?;
                    if let Err(e) = chaos::remediate(&dir).await {
                        tracing::warn!("stale-chaos remediation: {e:#}");
                    }
                    chaos::run(
                        &dir,
                        chaos::ChaosKind::Blackhole,
                        opts.fault_target.unwrap_or(chaos::ChaosTarget::Scheduler),
                        opts.fault_from.unwrap_or(chaos::ChaosFrom::AllWorkers),
                        opts.fault_duration,
                    )
                    .await?
                }
            }
            Ok(())
        }
        .await;
        let elapsed = stage_start.elapsed().as_secs_f64();
        match result {
            Ok(()) => info!("✓ {:36} {elapsed:>6.1}s", stage.name()),
            Err(e) => {
                tracing::error!("✗ {} — {e:#}", stage.name());
                tracing::error!(
                    "qa FAILED at stage [{}/{total}] {} — {:.0}s",
                    i + 1,
                    stage.name(),
                    run_start.elapsed().as_secs_f64()
                );
                return Err(e);
            }
        }
    }
    info!(
        "qa PASSED — {total}/{total} stages, {:.0}s",
        run_start.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_flags_is_full_sequence() {
        assert_eq!(QaOpts::default().stages(), Stage::ALL.to_vec());
    }

    #[test]
    fn only_implies_scenarios() {
        let o = QaOpts {
            only: vec!["i095".into()],
            ..Default::default()
        };
        assert_eq!(o.stages(), vec![Stage::Scenarios]);
    }

    #[test]
    fn flags_select_subset_in_canonical_order() {
        let o = QaOpts {
            fault: true,
            health: true,
            ..Default::default()
        };
        assert_eq!(o.stages(), vec![Stage::Health, Stage::Fault]);
    }
}
