//! Unified k8s deploy. One command surface, provider flag selects
//! k3s (local) vs eks (AWS).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use tokio::task::JoinSet;

use crate::config::XtaskConfig;
use crate::{helm, kube, sh, ui};

mod chaos;
mod eks;
mod k3s;
mod metrics;
pub mod provider;
pub mod shared;
pub(crate) mod status;
mod stress;

pub use eks::ami::AmiArch;
use provider::{Provider, ProviderKind};
use tracing::{debug, info};

/// Control-plane namespace. Helm release anchors here; scheduler,
/// gateway, controller, and the SSH/postgres secrets live here. Code
/// that means "the rio namespace" uses this const — only multi-ns-aware
/// code iterates NAMESPACES.
pub const NS: &str = "rio-system";

/// Store namespace. ADR-019: store moved out of rio-system to run at
/// PSA baseline. Secrets the store reads (rio-postgres) need a copy
/// here.
pub const NS_STORE: &str = "rio-store";

/// Builder namespace. ADR-019: builders need CAP_SYS_ADMIN (FUSE),
/// which PSA baseline rejects. Builders are airgapped (NetworkPolicy).
pub const NS_BUILDERS: &str = "rio-builders";

/// Fetcher namespace. ADR-019: same FUSE story as builders, but with
/// open internet egress on 80/443 (FOD fetchurl/git).
pub const NS_FETCHERS: &str = "rio-fetchers";

/// All four rio namespaces and whether each needs the `privileged` PSA
/// label. Providers iterate this at deploy time so the namespaces exist
/// before `helm upgrade` (which renders cross-ns resources into them).
pub const NAMESPACES: &[(&str, bool)] = &[
    (NS, false),
    (NS_STORE, false),
    (NS_BUILDERS, true),
    (NS_FETCHERS, true),
];

/// Ensure all four rio namespaces exist with the right PSA label.
/// Called at the start of every provider's deploy() so cross-ns chart
/// resources (store.yaml → rio-store, builderpool.yaml → rio-builders,
/// etc.) have somewhere to land.
pub async fn ensure_namespaces(client: &kube::Client) -> anyhow::Result<()> {
    for &(ns, privileged) in NAMESPACES {
        kube::ensure_namespace(client, ns, privileged).await?;
    }
    Ok(())
}

/// Ordered bring-up phases for `k8s up`. `up` with no phase flags runs
/// the full sequence; flags select a subset (still in this order).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Phase {
    Bootstrap,
    Provision,
    Kubeconfig,
    Ami,
    Push,
    Deploy,
    Envoy,
}

impl Phase {
    /// Canonical order for selection/validation. Execution order is the
    /// DAG induced by [`Phase::deps`] — [`run_up_phases`] runs every
    /// phase whose dependencies are satisfied concurrently (ami ∥
    /// bootstrap→provision; kubeconfig ∥ push after provision; deploy
    /// waits on the join of all three).
    pub const ALL: [Phase; 7] = [
        Phase::Bootstrap,
        Phase::Provision,
        Phase::Kubeconfig,
        Phase::Ami,
        Phase::Push,
        Phase::Deploy,
        Phase::Envoy,
    ];

    fn name(self) -> &'static str {
        match self {
            Phase::Bootstrap => "bootstrap",
            Phase::Provision => "provision",
            Phase::Kubeconfig => "kubeconfig",
            Phase::Ami => "ami",
            Phase::Push => "push",
            Phase::Deploy => "deploy",
            Phase::Envoy => "envoy",
        }
    }

    /// Declarative dependency table for the [`run_up_phases`] DAG
    /// executor. A phase is ready when every dep listed here is done
    /// (or wasn't selected — unselected deps are treated as
    /// pre-satisfied so `up --push --deploy` works without
    /// `--provision`).
    ///
    /// Adding an edge: extend the match arm. The `phase_deps_acyclic`
    /// test catches cycles.
    pub const fn deps(self) -> &'static [Phase] {
        match self {
            Phase::Bootstrap | Phase::Ami => &[],
            Phase::Provision => &[Phase::Bootstrap],
            Phase::Kubeconfig => &[Phase::Provision],
            // ECR repo URL is a tofu output → push can't start before
            // provision. build (the nix-build half) is folded into the
            // push phase rather than threaded as a separate output —
            // it's fast and local, not worth a typed-output channel.
            Phase::Push => &[Phase::Provision],
            // deploy reads the AMI tag from EC2 (ami phase registers
            // + tags it) + image tag (push) + talks to the cluster
            // (kubeconfig).
            Phase::Deploy => &[Phase::Kubeconfig, Phase::Push, Phase::Ami],
            Phase::Envoy => &[Phase::Deploy],
        }
    }
}

#[derive(Args, Default)]
pub struct UpOpts {
    /// tofu state bucket (eks). No-op on k3s.
    #[arg(long)]
    bootstrap: bool,
    /// tofu apply (eks) | rook install (k3s).
    #[arg(long)]
    provision: bool,
    /// aws eks update-kubeconfig | k3s.yaml copy.
    #[arg(long)]
    kubeconfig: bool,
    /// Build + register the NixOS node AMI (ADR-021). EKS-only;
    /// silently skipped on k3s in the all-phases path, hard error
    /// when explicitly requested.
    #[arg(long)]
    ami: bool,
    /// Build + push docker images (ECR | ctr import).
    #[arg(long)]
    push: bool,
    /// helm upgrade rio chart.
    #[arg(long)]
    deploy: bool,
    /// Install Envoy Gateway operator (dashboard gRPC-Web).
    #[arg(long)]
    envoy: bool,

    // Namespaced per-phase opts. NO clap `requires` attribute —
    // validated at runtime by `validate_phase_opts()` so the
    // no-flags=all-phases path can still pass them.
    /// AMI architecture(s) to build. EKS-only.
    #[arg(long = "ami-arch", value_enum, default_value_t = AmiArch::All)]
    ami_arch: AmiArch,
    /// Tenant name for the authorized_keys comment. Overrides
    /// RIO_SSH_TENANT. When neither is set, preserves the existing
    /// Secret's comment (I-100); falls back to "default" only on
    /// first deploy.
    #[arg(long = "deploy-tenant")]
    deploy_tenant: Option<String>,
    /// RUST_LOG directive for deployed pods (e.g. "info,rio_scheduler=trace").
    /// Defaults to RIO_LOG_LEVEL env / `config::RIO_DEBUG`.
    #[arg(long = "deploy-log-level", value_name = "DIRECTIVE")]
    deploy_log_level: Option<String>,
    /// Skip the pre-deploy cluster health check (eks).
    #[arg(long = "deploy-skip-preflight")]
    deploy_skip_preflight: bool,
    /// Pass --no-hooks to helm upgrade — skips post-install/upgrade
    /// hooks (smoke tests). For AMI bring-up where the hook needs
    /// working nodes that don't exist yet.
    #[arg(long = "deploy-no-hooks")]
    deploy_no_hooks: bool,

    /// Skip interactive confirmation prompts (tofu apply diff).
    #[arg(long)]
    yes: bool,
}

impl UpOpts {
    fn has(&self, p: Phase) -> bool {
        match p {
            Phase::Bootstrap => self.bootstrap,
            Phase::Provision => self.provision,
            Phase::Kubeconfig => self.kubeconfig,
            Phase::Ami => self.ami,
            Phase::Push => self.push,
            Phase::Deploy => self.deploy,
            Phase::Envoy => self.envoy,
        }
    }

    /// No phase flags → full canonical sequence. Any phase flag →
    /// only the flagged ones, still in canonical order.
    fn phases(&self) -> Vec<Phase> {
        let any = Phase::ALL.iter().any(|&p| self.has(p));
        if !any {
            return Phase::ALL.to_vec();
        }
        Phase::ALL.into_iter().filter(|&p| self.has(p)).collect()
    }

    /// Namespaced-opt validation: only enforce "X requires --phase"
    /// when at least one phase flag is set. `up --deploy-tenant foo`
    /// (no phase flags) is fine — all phases run, deploy uses the
    /// tenant. `up --push --deploy-tenant foo` errors: phase flags are
    /// explicit and `--deploy` isn't among them.
    fn validate_phase_opts(&self, selected: &[Phase]) -> Result<()> {
        let explicit = selected.len() != Phase::ALL.len();
        if !explicit {
            return Ok(());
        }
        macro_rules! req {
            ($cond:expr, $opt:literal, $phase:expr) => {
                if $cond && !selected.contains(&$phase) {
                    bail!(
                        "{} requires --{} (or omit phase flags to run all)",
                        $opt,
                        $phase.name()
                    );
                }
            };
        }
        req!(
            self.deploy_tenant.is_some(),
            "--deploy-tenant",
            Phase::Deploy
        );
        req!(
            self.deploy_log_level.is_some(),
            "--deploy-log-level",
            Phase::Deploy
        );
        req!(
            self.deploy_skip_preflight,
            "--deploy-skip-preflight",
            Phase::Deploy
        );
        req!(self.deploy_no_hooks, "--deploy-no-hooks", Phase::Deploy);
        req!(
            !matches!(self.ami_arch, AmiArch::All),
            "--ami-arch",
            Phase::Ami
        );
        Ok(())
    }
}

#[derive(Args)]
// Allows `k8s up -p eks` (flag after subcommand) as well as
// `k8s -p eks up`. clap won't let a global arg be required, so the
// field is Option and validated in run().
#[command(args_conflicts_with_subcommands = false)]
pub struct K8sArgs {
    /// Target cluster provider. Reads RIO_K8S_PROVIDER if not given.
    #[arg(short, long, global = true, env = "RIO_K8S_PROVIDER")]
    provider: Option<ProviderKind>,

    #[command(subcommand)]
    cmd: K8sCmd,
}

#[derive(Subcommand)]
pub enum K8sCmd {
    /// Bring up the cluster: ami ∥ (bootstrap → provision → kubeconfig
    /// → push), then deploy → envoy. Phase flags select a subset.
    Up(UpOpts),
    /// End-to-end build + worker-kill chaos test.
    Smoke,
    /// helm rollback to REV (0 = previous).
    Rollback {
        #[arg(default_value_t = 0)]
        rev: u32,
    },
    /// helm release history.
    History,
    /// One-shot deployment health report.
    #[command(visible_alias = "st")]
    Status {
        /// Emit machine-readable JSON instead of the human report.
        #[arg(long)]
        json: bool,
        /// Delete NodeClaims stuck in Unknown >2min (Karpenter blocked
        /// on ResourceNotRegistered, InsufficientCapacity, etc.).
        /// Karpenter reprovisions healthy replacements.
        #[arg(long)]
        reap_stuck_nodes: bool,
    },
    /// One-shot Prometheus scrape of scheduler-leader + store
    /// replicas. Prints the gauges that answer "is the actor
    /// wedged?" — mailbox depth, queued/running, workers, mean
    /// actor-cmd latency. <5s; no Prometheus install required.
    #[command(visible_alias = "m")]
    Metrics {
        /// List what would be scraped instead of scraping.
        #[arg(long)]
        dry_run: bool,
    },
    /// Port-forward to Grafana (kube-prometheus-stack), print
    /// URL + credentials, hold until Ctrl-C. The 6 rio-* dashboards
    /// load via the Grafana sidecar (P0539b).
    #[command(visible_alias = "g")]
    Grafana {
        /// Local port to forward (0 = pick free).
        #[arg(long, default_value_t = 3000)]
        port: u16,
    },
    /// Tear down rio + backing infra.
    Destroy {
        /// Skip the interactive confirm. Destroy is irreversible —
        /// only use this from CI / scripted teardowns.
        #[arg(long)]
        yes: bool,
    },
    /// Open a tunnel to the gateway and run `nix build --store
    /// ssh-ng://rio@localhost:PORT <ARGS>`. Uses your SSH key (the
    /// pair of RIO_SSH_PUBKEY installed by `deploy`).
    #[command(visible_alias = "rsb")]
    RemoteStoreBuild {
        #[command(flatten)]
        remote: RemoteStoreArgs,
    },
    /// Open a tunnel to the gateway and run `nix copy --to
    /// ssh-ng://rio@localhost:PORT <ARGS>`. Uses your SSH key (the
    /// pair of RIO_SSH_PUBKEY installed by `deploy`).
    #[command(visible_alias = "cpt")]
    CopyTo {
        #[command(flatten)]
        remote: RemoteStoreArgs,
    },
    /// Run rio-cli LOCALLY against a port-forwarded scheduler+store.
    /// Fetches the mTLS client cert from the cluster (rio-scheduler-tls
    /// Secret — its `localhost` SAN makes a port-forwarded connection
    /// pass tonic's :authority check). Prefer this over `kubectl exec
    /// deploy/rio-scheduler -- rio-cli …`: in-pod exec forces the
    /// scheduler image to bundle rio-cli + jq + whatever pipes through.
    #[command(visible_alias = "cli")]
    CliTunnel {
        /// Local port for scheduler:9001 forward. 0 = ephemeral
        /// (I-101: fixed default raced concurrent invocations).
        #[arg(long, default_value_t = 0)]
        sched_port: u16,
        /// Local port for store:9002 forward. 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        store_port: u16,
        /// Passed through to rio-cli.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Detached stress-build harness. Fires N parallel builds through
    /// SSM tunnels, returns immediately, tracks PIDs on disk for
    /// SIGKILL-safe cleanup. Born from QA sessions where hand-rolled
    /// `setsid nohup` left zombie session-manager-plugin tunnels.
    #[command(subcommand)]
    Stress(stress::StressCmd),
    /// NixOS node AMI management (ADR-021). EKS-only — `up --ami`
    /// builds + registers; this is the maintenance side.
    #[command(subcommand)]
    Ami(AmiCmd),
}

#[derive(Subcommand)]
pub enum AmiCmd {
    /// Deregister stale rio AMIs + delete their snapshots. "Stale" =
    /// tagged `rio.build/ami`, NOT tagged `rio.build/ami-latest=true`,
    /// older than `--older-than-days`. Dry-run by default.
    Gc {
        /// Minimum age in days. AMIs newer than this are kept even if
        /// not latest (rollback window).
        #[arg(long, default_value_t = 7)]
        older_than_days: u64,
        /// Actually deregister + delete. Without this, prints the
        /// candidate set and exits.
        #[arg(long = "no-dry-run")]
        no_dry_run: bool,
    },
}

#[derive(Args)]
pub struct RemoteStoreArgs {
    /// Local port for the tunnel.
    #[arg(long, default_value_t = 2222)]
    port: u16,
    /// Passed through to the nix command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    args: Vec<String>,
}

pub async fn run(args: K8sArgs, cfg: &XtaskConfig) -> Result<()> {
    let kind = match args.provider {
        Some(k) => k,
        None => {
            ui::select("Provider?", ProviderKind::value_variants().to_vec())?.ok_or_else(|| {
                anyhow!(
                    "provider required: pass -p {{k3s,eks}} or set RIO_K8S_PROVIDER in .env.local"
                )
            })?
        }
    };
    let p = provider::get(kind);
    match args.cmd {
        K8sCmd::Up(opts) => run_up(p, kind, cfg, opts).await,
        K8sCmd::Smoke => p.smoke(cfg).await,
        K8sCmd::Rollback { rev } => {
            let rev = if rev > 0 {
                rev
            } else {
                let revs = helm::history_json("rio", NS)?;
                ui::select("Rollback to?", revs)?
                    .map(|r| r.revision)
                    .ok_or_else(|| anyhow!("specify a revision: cargo xtask k8s rollback <REV>"))?
            };
            helm::rollback("rio", NS, rev)
        }
        K8sCmd::History => helm::history("rio", NS),
        K8sCmd::Status {
            json,
            reap_stuck_nodes,
        } => status::run(&*p, kind, cfg, json, reap_stuck_nodes).await,
        K8sCmd::Metrics { dry_run } => metrics::run(dry_run).await,
        K8sCmd::Grafana { port } => metrics::grafana(port).await,
        K8sCmd::Destroy { yes } => {
            let what = match kind {
                ProviderKind::Eks => crate::tofu::output(eks::TF_DIR, "cluster_name")
                    .map(|n| format!("EKS cluster '{n}' (RDS, S3, ECR, VPC, IAM)"))
                    .unwrap_or_else(|_| "the EKS cluster (RDS, S3, ECR, VPC, IAM)".into()),
                ProviderKind::K3s => "the local k3s rio deployment + rook".into(),
            };
            // confirm_destroy returns false on non-TTY stdin — `--yes`
            // is the only way to run this from a script.
            if !yes
                && !ui::confirm_destroy(&format!(
                    "This will DESTROY {what} and all data. Continue?"
                ))?
            {
                bail!("destroy cancelled (use --yes to bypass)");
            }
            p.destroy(cfg).await
        }
        K8sCmd::RemoteStoreBuild { remote } => {
            with_remote_store(&*p, cfg, remote.port, |sh, store| {
                let args = &remote.args;
                sh::run_interactive(sh::cmd!(
                    sh,
                    "nix build --store {store} --eval-store auto {args...}"
                ))
            })
            .await
        }
        K8sCmd::CopyTo { remote } => {
            with_remote_store(&*p, cfg, remote.port, |sh, store| {
                let args = &remote.args;
                sh::run_interactive(sh::cmd!(sh, "nix copy --to {store} {args...}"))
            })
            .await
        }
        K8sCmd::CliTunnel {
            sched_port,
            store_port,
            args,
        } => {
            with_cli_tunnel(&*p, sched_port, store_port, |sh| {
                // Prefer an installed rio-cli (nix run / cargo install);
                // fall back to cargo run for dev iteration. The PATH probe
                // uses `command -v` via xshell — it's available in any
                // POSIX sh and cheaper than pulling the `which` crate.
                let on_path = sh::read(sh::cmd!(sh, "command -v rio-cli")).is_ok();
                if on_path {
                    sh::run_interactive(sh::cmd!(sh, "rio-cli {args...}"))
                } else {
                    sh::run_interactive(sh::cmd!(sh, "cargo run -q -p rio-cli -- {args...}"))
                }
            })
            .await
        }
        K8sCmd::Stress(cmd) => stress::run(cmd, &*p, kind, cfg).await,
        K8sCmd::Ami(cmd) => {
            if !matches!(kind, ProviderKind::Eks) {
                bail!("`ami` is EKS-only (NixOS node AMI, ADR-021); pass -p eks");
            }
            match cmd {
                AmiCmd::Gc {
                    older_than_days,
                    no_dry_run,
                } => eks::ami::gc(older_than_days, !no_dry_run).await,
            }
        }
    }
}

/// Dispatch the selected `up` phases.
///
/// `explicit` distinguishes `up --ami -p k3s` (hard error: the user
/// asked for an EKS-only phase on the wrong provider) from `up -p k3s`
/// (silent skip: ami is part of the canonical sequence but not
/// applicable here). Same distinction lets `validate_phase_opts`
/// reject `--push --deploy-tenant foo` while accepting
/// `--deploy-tenant foo` alone.
async fn run_up(
    p: Arc<dyn Provider>,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    o: UpOpts,
) -> Result<()> {
    let selected = o.phases();
    // Explicit = at least one phase flag set. Passing all 7 flags is
    // intentionally treated as implicit (≡ no flags) — semantically
    // "run everything"; the explicit/implicit distinction only governs
    // whether per-phase opt mismatches and provider-unsupported phases
    // are hard errors vs silent skips.
    let explicit = selected.len() != Phase::ALL.len();
    o.validate_phase_opts(&selected)?;
    // Provider-support validation BEFORE dispatch — same upfront-fail
    // discipline as validate_phase_opts. Without this,
    // `-p k3s up --bootstrap --provision --ami` would install rook
    // THEN error.
    if explicit && selected.contains(&Phase::Ami) && !matches!(kind, ProviderKind::Eks) {
        bail!("--ami is EKS-only (NixOS node AMI, ADR-021); pass -p eks");
    }

    let pp = PhaseParams {
        yes: o.yes,
        log_level: o
            .deploy_log_level
            .clone()
            .unwrap_or_else(|| cfg.log_level.clone()),
        tenant: o.deploy_tenant.clone(),
        skip_preflight: o.deploy_skip_preflight,
        no_hooks: o.deploy_no_hooks,
    };
    let cfg = Arc::new(cfg.clone());

    // ami_branch is built here (not inside run_up_phases) because the
    // EKS-only gate needs `kind`, which the provider-agnostic core
    // doesn't see. Tests inject their own ami future directly.
    let ami_selected = selected.contains(&Phase::Ami);
    let ami_arch = o.ami_arch;
    let ami_branch = async move {
        if !ami_selected {
            return Ok(());
        }
        match kind {
            ProviderKind::Eks => ui::step("ami", || eks::ami::run_phase(ami_arch)).await,
            // explicit + non-EKS already rejected above; reaching here
            // means implicit full-sequence on k3s — skip.
            _ => {
                debug!("ami: provider={kind}, skipping");
                Ok(())
            }
        }
    };

    ui::step("k8s up", || async move {
        run_up_phases(p, cfg, &selected, pp, ami_branch, envoy_install()).await?;
        if !explicit {
            info!("all phases done — run `cargo xtask k8s -p {kind} smoke` to verify");
        }
        Ok(())
    })
    .await
}

/// Per-phase knobs threaded through [`run_up_phases`]. Bundled so the
/// concurrent-dispatch core has a stable test surface independent of
/// `UpOpts` (which is clap-coupled). Owned + `Clone` so each spawned
/// phase task can hold its own copy (`'static` for `tokio::spawn`).
#[derive(Clone)]
struct PhaseParams {
    yes: bool,
    log_level: String,
    tenant: Option<String>,
    skip_preflight: bool,
    no_hooks: bool,
}

/// Concurrent core of `up`: a ready-set DAG executor over
/// [`Phase::deps`]. A phase spawns the moment all its (selected)
/// dependencies have completed; independent chains run concurrently
/// (ami ∥ bootstrap→provision; kubeconfig ∥ push after provision).
///
/// **No cancellation on error.** provision is `terraform apply` — real
/// cloud resources mid-creation. An AMI build failure (nix eval error,
/// S3 throttle) MUST NOT drop an in-flight provision on the floor;
/// that would abandon a half-applied tofu plan with state drift the
/// operator then has to untangle by hand. In-flight phases always run
/// to completion. A failed phase's *dependents* are never spawned
/// (their dep is never marked done); *siblings* on independent dep
/// chains continue. Errors are collected and surfaced together after
/// the graph drains.
///
/// `ami_branch` / `envoy_branch` are injected so tests can mock them
/// without an EKS account or a live cluster. They are only awaited if
/// their phase is in `selected`.
///
/// **I-198 — per-phase `tokio::spawn`.** Each phase runs on its own
/// tokio task (`JoinSet`), so a phase that blocks the calling thread
/// (sync `sh::read`, `std::thread::sleep`, an SDK call that turns out
/// to be blocking) parks ONE worker thread — siblings on other workers
/// keep running. The earlier `FuturesUnordered` design polled every
/// phase from a single task and depended on every phase being purely
/// cooperative; one stray sync call serialized the whole DAG.
/// `Provider: Send + Sync` and `Arc`-threaded `cfg`/`pp` give the
/// `'static` bound `tokio::spawn` needs. Phases should still prefer
/// `sh::run`/`run_read` (yield) over `sh::read`/`run_sync` (block) —
/// each blocked phase ties up a runtime worker — but a slip is now a
/// throughput cost, not a wall-clock-doubling stall.
///
/// `ui::step` is concurrency-aware (span-scoped depth, see ui.rs
/// `DepthLayer`); interleaved ✓/✗ lines from concurrent phases render
/// with correct indentation. The transient spinner is best-effort
/// (last-enter-wins) — acceptable, the persistent tree is the record.
async fn run_up_phases<A, E>(
    p: Arc<dyn Provider>,
    cfg: Arc<XtaskConfig>,
    selected: &[Phase],
    pp: PhaseParams,
    ami_branch: A,
    envoy_branch: E,
) -> Result<()>
where
    A: Future<Output = Result<()>> + Send + 'static,
    E: Future<Output = Result<()>> + Send + 'static,
{
    // Per-phase unsatisfied-dep set, restricted to `selected`: a dep
    // the user didn't ask for is treated as already satisfied (so
    // `up --push --deploy` works without `--provision`).
    let mut pending: HashMap<Phase, HashSet<Phase>> = selected
        .iter()
        .map(|&ph| {
            let deps = ph
                .deps()
                .iter()
                .copied()
                .filter(|d| selected.contains(d))
                .collect();
            (ph, deps)
        })
        .collect();
    let mut started: HashSet<Phase> = HashSet::new();
    let mut failed: Vec<(Phase, anyhow::Error)> = Vec::new();
    let mut running: JoinSet<(Phase, Result<()>)> = JoinSet::new();
    // Injected futures, taken exactly once when their phase dispatches.
    let mut ami = Some(ami_branch);
    let mut envoy = Some(envoy_branch);

    loop {
        // Spawn every selected phase whose pending-dep set is empty.
        // Each task owns clones of the Arcs/params it needs (`'static`
        // for `tokio::spawn`) — nothing borrows from this stack frame.
        for &ph in selected {
            if started.contains(&ph) || !pending[&ph].is_empty() {
                continue;
            }
            started.insert(ph);
            match ph {
                Phase::Bootstrap => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    running.spawn(async move {
                        (ph, ui::step("bootstrap", || p.bootstrap(&cfg)).await)
                    });
                }
                Phase::Provision => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    let yes = pp.yes;
                    running.spawn(async move {
                        (ph, ui::step("provision", || p.provision(&cfg, yes)).await)
                    });
                }
                Phase::Kubeconfig => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    running.spawn(async move {
                        (ph, ui::step("kubeconfig", || p.kubeconfig(&cfg)).await)
                    });
                }
                Phase::Ami => {
                    let a = ami.take().expect("ami dispatched once");
                    // Caller already wraps in ui::step("ami", ..).
                    running.spawn(async move { (ph, a.await) });
                }
                Phase::Push => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    running.spawn(async move {
                        // build+push kept as one phase: build is fast
                        // and local; not worth a typed-output channel
                        // between DAG nodes for one edge.
                        let r = async {
                            let images = ui::step("build", || p.build(&cfg)).await?;
                            ui::step("push", || p.push(&images, &cfg)).await
                        }
                        .await;
                        (ph, r)
                    });
                }
                Phase::Deploy => {
                    let (p, cfg, pp) = (p.clone(), cfg.clone(), pp.clone());
                    running.spawn(async move {
                        (
                            ph,
                            ui::step("deploy", || {
                                p.deploy(
                                    &cfg,
                                    &pp.log_level,
                                    pp.tenant.as_deref(),
                                    pp.skip_preflight,
                                    pp.no_hooks,
                                )
                            })
                            .await,
                        )
                    });
                }
                Phase::Envoy => {
                    let e = envoy.take().expect("envoy dispatched once");
                    running.spawn(async move { (ph, ui::step("envoy", || e).await) });
                }
            }
        }

        // Drain one. `join_next()` never aborts the rest — in-flight
        // phases run to completion regardless of what we do with the
        // result. JoinSet's Drop WOULD abort, but we only return after
        // the loop exits (set empty), so every spawned task has joined.
        let Some(joined) = running.join_next().await else {
            break; // nothing running, nothing ready ⇒ graph drained
        };
        // JoinError = panic or cancel. We never cancel; surface a panic
        // verbatim — the phase task is gone, no in-flight work to save.
        let (ph, r) = joined.expect("phase task panicked");
        match r {
            // Success: clear this phase from every dependent's pending
            // set; next spawn pass picks up newly-ready phases.
            Ok(()) => {
                for deps in pending.values_mut() {
                    deps.remove(&ph);
                }
            }
            // Failure: record but DON'T remove from dependents' pending
            // sets — they stay non-empty forever, so dependents never
            // spawn. Siblings (independent pending sets) are unaffected.
            Err(e) => failed.push((ph, e)),
        }
    }

    if failed.is_empty() {
        return Ok(());
    }
    let skipped: Vec<_> = selected
        .iter()
        .filter(|ph| !started.contains(ph))
        .map(|ph| ph.name())
        .collect();
    for ph in &skipped {
        ui::step_skip(ph, "dependency failed");
    }
    let skipped_ctx = if skipped.is_empty() {
        String::new()
    } else {
        format!(" (skipped: {})", skipped.join(", "))
    };
    match failed.len() {
        // Single failure: hoist the real anyhow::Error so the
        // backtrace/context chain survives.
        1 => {
            let (ph, e) = failed.pop().unwrap();
            Err(e.context(format!("{} failed{skipped_ctx}", ph.name())))
        }
        n => {
            let mut msg = format!("{n} phases failed{skipped_ctx}:");
            for (ph, e) in &failed {
                msg.push_str(&format!("\n  {}: {e:#}", ph.name()));
            }
            bail!(msg)
        }
    }
}

/// Open a tunnel to the gateway, resolve the ssh-ng:// store URL,
/// and run `f` with a prepared shell. The store URL's `ssh-key=`
/// points at the private half of RIO_SSH_PUBKEY (what `deploy` put
/// in authorized_keys). Gateway host key is ephemeral, so
/// NIX_SSHOPTS sets StrictHostKeyChecking=no. The tunnel tears down
/// when this function returns (ProcessGuard drop).
async fn with_remote_store<F>(p: &dyn Provider, cfg: &XtaskConfig, port: u16, f: F) -> Result<()>
where
    F: FnOnce(&xshell::Shell, &str) -> Result<()>,
{
    let key = crate::ssh::privkey_path(cfg)?;
    // P0539e: a prior `rsb`/`stress` may have left a session-manager-
    // plugin or kubectl port-forward bound to this port (ProcessGuard
    // only fires if xtask exited cleanly; SIGKILL/panic leaks it).
    // The new tunnel would then bind fine but route to the OLD NLB —
    // surfacing as the "type 80" SSH error 30s later. Reap first.
    shared::kill_port_listeners(port);
    let _guard = ui::step("establish tunnel", || p.tunnel(port)).await?;

    let store = format!(
        "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
        key.display()
    );
    info!("store: {store}");

    let shell = sh::shell()?;
    // I-149/I-161: ServerAliveInterval — see `shared::NIX_SSHOPTS_BASE`.
    let _env = shell.push_env("NIX_SSHOPTS", shared::NIX_SSHOPTS_BASE);
    f(&shell, &store)
}

// r[impl sec.image.control-plane-minimal]
/// Port-forward scheduler:9001 + store:9002, fetch the mTLS client
/// cert into a tempdir, set `RIO_SCHEDULER_ADDR`/`RIO_STORE_ADDR`/
/// `RIO_TLS__*` on a prepared shell, run `f`. Tunnels + tempdir tear
/// down on return.
///
/// The `rio-scheduler-tls` Secret already carries a `localhost` SAN
/// (cert-manager.yaml — originally for in-pod rio-cli). Port-forward
/// preserves that `:authority`, so the fetched cert validates cleanly
/// with no new Certificate resource. Enables running rio-cli LOCALLY
/// instead of via `kubectl exec deploy/rio-scheduler`, which in turn
/// lets the scheduler image drop rio-cli + its transitive deps (jq,
/// column, …) — every extra binary in a control-plane image is an
/// execution primitive in a compromised pod.
pub async fn with_cli_tunnel<F>(p: &dyn Provider, sched: u16, store: u16, f: F) -> Result<()>
where
    F: FnOnce(&xshell::Shell) -> Result<()>,
{
    let client = kube::client().await?;
    let ((sched, _g1), (store, _g2)) =
        ui::step("tunnel scheduler+store", || p.tunnel_grpc(sched, store)).await?;
    let (_dir, cert, key, ca) = ui::step("fetch mTLS cert", || {
        kube::fetch_tls_to_tempdir(&client, NS, "rio-scheduler-tls")
    })
    .await?;

    let sh = sh::shell()?;
    let _e1 = sh.push_env("RIO_SCHEDULER_ADDR", format!("localhost:{sched}"));
    let _e2 = sh.push_env("RIO_STORE_ADDR", format!("localhost:{store}"));
    let _e3 = sh.push_env("RIO_TLS__CERT_PATH", &cert);
    let _e4 = sh.push_env("RIO_TLS__KEY_PATH", &key);
    let _e5 = sh.push_env("RIO_TLS__CA_PATH", &ca);
    f(&sh)
}

/// Envoy Gateway operator (dashboard gRPC-Web → gRPC+mTLS translation).
/// Provider-agnostic — same helm chart, same namespace.
async fn envoy_install() -> Result<()> {
    let client = kube::client().await?;

    // I-198: was sync sh::read — `nix build` can be multi-second and
    // this runs as a DAG phase. run_read yields. Shell scoped tight
    // (xshell::Shell is !Sync) so it isn't held across await.
    let eg = {
        let shell = sh::shell()?;
        sh::run_read(sh::cmd!(
            shell,
            "nix build --no-link --print-out-paths .#helm-envoy-gateway"
        ))
    }
    .await?;

    helm::Helm::upgrade_install("envoy-gateway", &eg)
        .namespace("envoy-gateway-system")
        .create_namespace()
        .run()?;
    kube::wait_rollout(
        &client,
        "envoy-gateway-system",
        "envoy-gateway",
        Duration::from_secs(120),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use async_trait::async_trait;
    use provider::BuiltImages;

    use super::*;

    fn opts() -> UpOpts {
        UpOpts::default()
    }

    // -- mock provider for run_up_phases concurrency tests ------------

    type Log = Arc<Mutex<Vec<&'static str>>>;

    /// Records call order; per-method delays make ready-set
    /// interleaving observable. `provision_done` is the witness for
    /// the "ami error must not cancel provision" test.
    struct MockProvider {
        log: Log,
        provision_delay: Duration,
        kubeconfig_delay: Duration,
        push_delay: Duration,
        push_err: bool,
        provision_done: Arc<AtomicBool>,
    }

    impl MockProvider {
        fn new(log: Log) -> Self {
            Self {
                log,
                provision_delay: Duration::ZERO,
                kubeconfig_delay: Duration::ZERO,
                push_delay: Duration::ZERO,
                push_err: false,
                provision_done: Arc::new(AtomicBool::new(false)),
            }
        }
        fn record(&self, what: &'static str) {
            self.log.lock().unwrap().push(what);
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn context_matches(&self, _: &str) -> bool {
            unimplemented!()
        }
        async fn bootstrap(&self, _: &XtaskConfig) -> Result<()> {
            self.record("bootstrap");
            Ok(())
        }
        async fn provision(&self, _: &XtaskConfig, _: bool) -> Result<()> {
            tokio::time::sleep(self.provision_delay).await;
            self.record("provision");
            self.provision_done.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn kubeconfig(&self, _: &XtaskConfig) -> Result<()> {
            self.record("kubeconfig:start");
            tokio::time::sleep(self.kubeconfig_delay).await;
            self.record("kubeconfig");
            Ok(())
        }
        async fn build(&self, _: &XtaskConfig) -> Result<BuiltImages> {
            self.record("build");
            Ok(BuiltImages {
                dir: tempfile::TempDir::new()?,
                tag: "test".into(),
            })
        }
        async fn push(&self, _: &BuiltImages, _: &XtaskConfig) -> Result<()> {
            tokio::time::sleep(self.push_delay).await;
            if self.push_err {
                bail!("mock push failed");
            }
            self.record("push");
            Ok(())
        }
        async fn deploy(
            &self,
            _: &XtaskConfig,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: bool,
        ) -> Result<()> {
            self.record("deploy");
            Ok(())
        }
        async fn smoke(&self, _: &XtaskConfig) -> Result<()> {
            unimplemented!()
        }
        async fn tunnel(&self, _: u16) -> Result<shared::ProcessGuard> {
            unimplemented!()
        }
        async fn tunnel_grpc(
            &self,
            _: u16,
            _: u16,
        ) -> Result<((u16, shared::ProcessGuard), (u16, shared::ProcessGuard))> {
            unimplemented!()
        }
        async fn destroy(&self, _: &XtaskConfig) -> Result<()> {
            unimplemented!()
        }
    }

    fn pp() -> PhaseParams {
        PhaseParams {
            yes: true,
            log_level: "info".into(),
            tenant: None,
            skip_preflight: true,
            no_hooks: false,
        }
    }

    fn cfg() -> Arc<XtaskConfig> {
        Arc::new(XtaskConfig::default())
    }

    fn pos(log: &[&str], what: &str) -> usize {
        log.iter().position(|&x| x == what).unwrap()
    }

    /// All phases selected: ami runs concurrently with the infra
    /// chain, and deploy only starts after BOTH have produced a
    /// result. The ami branch sleeps longer than the whole infra
    /// chain so "ami after push, deploy after ami" is observable.
    #[tokio::test]
    async fn up_phases_concurrent_ordering() {
        let log: Log = Arc::default();
        let p: Arc<dyn Provider> = Arc::new(MockProvider::new(log.clone()));

        let ami_log = log.clone();
        let ami = async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            ami_log.lock().unwrap().push("ami");
            Ok(())
        };
        let envoy_log = log.clone();
        let envoy = async move {
            envoy_log.lock().unwrap().push("envoy");
            Ok(())
        };

        run_up_phases(p, cfg(), &Phase::ALL, pp(), ami, envoy)
            .await
            .unwrap();

        let log = log.lock().unwrap();
        // infra chain stays internally ordered
        assert!(pos(&log, "bootstrap") < pos(&log, "provision"));
        assert!(pos(&log, "provision") < pos(&log, "push"));
        // ami overlapped infra: it finished AFTER push (30ms sleep vs
        // synchronous infra chain) — proves the branches actually ran
        // concurrently, not sequentially
        assert!(pos(&log, "push") < pos(&log, "ami"), "log: {log:?}");
        // deploy waited for the join: it's after BOTH ami and push
        assert!(pos(&log, "ami") < pos(&log, "deploy"), "log: {log:?}");
        assert!(pos(&log, "push") < pos(&log, "deploy"), "log: {log:?}");
        // envoy is last
        assert!(pos(&log, "deploy") < pos(&log, "envoy"));
    }

    /// The critical safety property: an AMI build failure does NOT
    /// cancel an in-flight provision. `try_join!` would drop the
    /// infra future the moment ami errors; `join!` lets provision
    /// run to completion (sets its flag) and surfaces the error
    /// afterward.
    #[tokio::test]
    async fn up_ami_error_does_not_cancel_provision() {
        let log: Log = Arc::default();
        let mut p = MockProvider::new(log.clone());
        p.provision_delay = Duration::from_millis(50);
        let provision_done = p.provision_done.clone();
        let p: Arc<dyn Provider> = Arc::new(p);

        // ami fails immediately — well before provision's 50ms sleep
        // completes. With try_join!, provision would be dropped here.
        let ami = async { Err(anyhow!("ami build failed")) };
        let envoy = async { Ok(()) };

        let r = run_up_phases(
            p,
            cfg(),
            &[Phase::Bootstrap, Phase::Provision, Phase::Ami],
            pp(),
            ami,
            envoy,
        )
        .await;

        // provision ran to completion despite ami erroring first
        assert!(
            provision_done.load(Ordering::SeqCst),
            "provision was cancelled — join! semantics broken"
        );
        assert!(log.lock().unwrap().contains(&"provision"));
        // overall result is still Err, with the ami context
        let e = r.unwrap_err().to_string();
        assert!(e.contains("ami failed"), "err: {e}");
    }

    /// Phase selection still gates: `up --push --deploy` runs only
    /// push+deploy; ami_branch is the caller's responsibility (here a
    /// no-op) and bootstrap/provision/kubeconfig are skipped.
    #[tokio::test]
    async fn up_phases_selection_subset() {
        let log: Log = Arc::default();
        let p: Arc<dyn Provider> = Arc::new(MockProvider::new(log.clone()));

        run_up_phases(
            p,
            cfg(),
            &[Phase::Push, Phase::Deploy],
            pp(),
            async { Ok(()) },
            async { Ok(()) },
        )
        .await
        .unwrap();

        assert_eq!(&*log.lock().unwrap(), &["build", "push", "deploy"]);
    }

    /// True ready-set (not layer-barrier, not serial chain): kubeconfig
    /// and push both depend ONLY on provision, so once provision
    /// finishes both must spawn in the same tick. Each one's start
    /// marker must land before the OTHER's finish marker — proves
    /// overlap regardless of which finishes first.
    #[tokio::test]
    async fn up_dag_kubeconfig_push_concurrent() {
        let log: Log = Arc::default();
        let mut p = MockProvider::new(log.clone());
        p.provision_delay = Duration::from_millis(5);
        p.kubeconfig_delay = Duration::from_millis(30);
        p.push_delay = Duration::from_millis(30);
        let p: Arc<dyn Provider> = Arc::new(p);

        run_up_phases(
            p,
            cfg(),
            &[
                Phase::Bootstrap,
                Phase::Provision,
                Phase::Kubeconfig,
                Phase::Push,
            ],
            pp(),
            async { Ok(()) },
            async { Ok(()) },
        )
        .await
        .unwrap();

        let log = log.lock().unwrap();
        // kubeconfig started before push finished
        assert!(
            pos(&log, "kubeconfig:start") < pos(&log, "push"),
            "log: {log:?}"
        );
        // push started (build is its first sub-step) before kubeconfig
        // finished
        assert!(pos(&log, "build") < pos(&log, "kubeconfig"), "log: {log:?}");
        // and both honored the provision dep
        assert!(pos(&log, "provision") < pos(&log, "kubeconfig:start"));
        assert!(pos(&log, "provision") < pos(&log, "build"));
    }

    /// I-198 regression. Each phase runs on its own tokio task
    /// (`JoinSet::spawn`), so a phase that BLOCKS THE THREAD — raw
    /// `std::thread::sleep`, sync `sh::read`, an SDK call that turns
    /// out to be blocking — parks one runtime worker; the sibling on
    /// another worker keeps running. ami parks for 200ms with NO
    /// `spawn_blocking` / no yield; bootstrap (sibling, no deps,
    /// instant) must still complete in <100ms.
    ///
    /// Under the old single-task `FuturesUnordered` executor this
    /// would serialize: cooperative concurrency only — one parked
    /// thread = no sibling polled. Option A (a6a1de7a) papered over it
    /// by mandating `spawn_blocking` everywhere; this test only passes
    /// with the structural fix (per-phase spawn).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn up_dag_raw_blocking_does_not_stall_sibling() {
        let log: Log = Arc::default();
        let bootstrap_at = Arc::new(Mutex::new(None::<Duration>));

        let ami_log = log.clone();
        let ami = async move {
            ami_log.lock().unwrap().push("ami:start");
            // Raw thread park — NO spawn_blocking, NO .await. Under
            // FuturesUnordered this stalls every sibling.
            std::thread::sleep(Duration::from_millis(200));
            ami_log.lock().unwrap().push("ami");
            Ok(())
        };

        let t0 = std::time::Instant::now();
        // Wrap bootstrap to timestamp its completion. Ami is first in
        // `selected` so it dispatches (and parks its worker) before
        // bootstrap is spawned — if both ran on one task, bootstrap_at
        // would be ≥200ms.
        let p: Arc<dyn Provider> = Arc::new(TimestampBootstrap {
            inner: MockProvider::new(log.clone()),
            t0,
            at: bootstrap_at.clone(),
        });
        run_up_phases(
            p,
            cfg(),
            &[Phase::Ami, Phase::Bootstrap],
            pp(),
            ami,
            async { Ok(()) },
        )
        .await
        .unwrap();

        let log = log.lock().unwrap();
        let b_at = bootstrap_at.lock().unwrap().expect("bootstrap ran");
        // Bootstrap completed while ami's worker was parked. 100ms
        // slack for CI jitter; failure mode is ≥200ms.
        assert!(
            b_at < Duration::from_millis(100),
            "bootstrap serialized behind ami's blocking work: {b_at:?} (log: {log:?})"
        );
        // bootstrap finished before ami's 200ms sleep returned —
        // holds regardless of which task the runtime schedules first.
        assert!(pos(&log, "bootstrap") < pos(&log, "ami"), "log: {log:?}");
        assert!(log.contains(&"ami:start"), "log: {log:?}");
    }

    /// Wraps a MockProvider to timestamp bootstrap completion.
    struct TimestampBootstrap {
        inner: MockProvider,
        t0: std::time::Instant,
        at: Arc<Mutex<Option<Duration>>>,
    }

    #[async_trait]
    impl Provider for TimestampBootstrap {
        fn context_matches(&self, c: &str) -> bool {
            self.inner.context_matches(c)
        }
        async fn bootstrap(&self, c: &XtaskConfig) -> Result<()> {
            self.inner.bootstrap(c).await?;
            *self.at.lock().unwrap() = Some(self.t0.elapsed());
            Ok(())
        }
        async fn provision(&self, c: &XtaskConfig, a: bool) -> Result<()> {
            self.inner.provision(c, a).await
        }
        async fn kubeconfig(&self, c: &XtaskConfig) -> Result<()> {
            self.inner.kubeconfig(c).await
        }
        async fn build(&self, c: &XtaskConfig) -> Result<provider::BuiltImages> {
            self.inner.build(c).await
        }
        async fn push(&self, i: &provider::BuiltImages, c: &XtaskConfig) -> Result<()> {
            self.inner.push(i, c).await
        }
        async fn deploy(
            &self,
            c: &XtaskConfig,
            l: &str,
            t: Option<&str>,
            s: bool,
            h: bool,
        ) -> Result<()> {
            self.inner.deploy(c, l, t, s, h).await
        }
        async fn smoke(&self, _: &XtaskConfig) -> Result<()> {
            unimplemented!()
        }
        async fn tunnel(&self, _: u16) -> Result<shared::ProcessGuard> {
            unimplemented!()
        }
        async fn tunnel_grpc(
            &self,
            _: u16,
            _: u16,
        ) -> Result<((u16, shared::ProcessGuard), (u16, shared::ProcessGuard))> {
            unimplemented!()
        }
        async fn destroy(&self, _: &XtaskConfig) -> Result<()> {
            unimplemented!()
        }
    }

    /// A failed phase poisons its dependents but NOT its siblings.
    /// push errors immediately; kubeconfig (sibling — same dep,
    /// independent of push) is mid-flight and runs to completion;
    /// deploy (depends on push) is never spawned.
    #[tokio::test]
    async fn up_dag_failed_dep_skips_dependents_not_siblings() {
        let log: Log = Arc::default();
        let mut p = MockProvider::new(log.clone());
        p.kubeconfig_delay = Duration::from_millis(20);
        p.push_err = true;
        let p: Arc<dyn Provider> = Arc::new(p);

        let r = run_up_phases(
            p,
            cfg(),
            // Ami unselected → pre-satisfied, so deploy's only blockers
            // are kubeconfig+push.
            &[
                Phase::Provision,
                Phase::Kubeconfig,
                Phase::Push,
                Phase::Deploy,
            ],
            pp(),
            async { Ok(()) },
            async { Ok(()) },
        )
        .await;

        let log = log.lock().unwrap();
        // sibling completed despite push having already failed
        assert!(log.contains(&"kubeconfig"), "log: {log:?}");
        // dependent never spawned
        assert!(!log.contains(&"deploy"), "log: {log:?}");
        // error names the failed phase and the skipped dependent
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains("push failed"), "err: {e}");
        assert!(e.contains("skipped: deploy"), "err: {e}");
    }

    /// Kahn's-algorithm cycle check over [`Phase::deps`]. Runs as a
    /// test (not `debug_assert!` in `deps()`) because `deps()` is
    /// `const fn` and the table is static — one check at test time is
    /// enough to catch a bad edge added in review.
    #[test]
    fn phase_deps_acyclic() {
        let mut indeg: HashMap<Phase, usize> =
            Phase::ALL.iter().map(|&p| (p, p.deps().len())).collect();
        // self-edges would otherwise pass Kahn (indeg never hits 0 →
        // caught as "cycle"), but flag them explicitly for the message.
        for &p in &Phase::ALL {
            assert!(!p.deps().contains(&p), "{p:?} depends on itself");
        }
        let mut ready: Vec<Phase> = indeg
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&p, _)| p)
            .collect();
        let mut visited = 0;
        while let Some(p) = ready.pop() {
            visited += 1;
            for &succ in &Phase::ALL {
                if succ.deps().contains(&p) {
                    let d = indeg.get_mut(&succ).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        ready.push(succ);
                    }
                }
            }
        }
        assert_eq!(
            visited,
            Phase::ALL.len(),
            "Phase::deps() has a cycle: unresolved = {:?}",
            indeg
                .iter()
                .filter(|&(_, &d)| d > 0)
                .map(|(p, _)| p)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_flags_is_full_sequence() {
        let o = opts();
        assert_eq!(o.phases(), Phase::ALL.to_vec());
        // and namespaced opts are accepted in the all-phases path
        let mut o = opts();
        o.deploy_tenant = Some("t".into());
        o.ami_arch = AmiArch::X86_64;
        assert!(o.validate_phase_opts(&o.phases()).is_ok());
    }

    #[test]
    fn flags_select_subset_in_canonical_order() {
        // Flag order doesn't matter; canonical order does.
        let mut o = opts();
        o.deploy = true;
        o.push = true;
        assert_eq!(o.phases(), vec![Phase::Push, Phase::Deploy]);
    }

    #[test]
    fn namespaced_opt_requires_its_phase_when_explicit() {
        let mut o = opts();
        o.push = true;
        o.deploy_tenant = Some("t".into());
        let e = o.validate_phase_opts(&o.phases()).unwrap_err().to_string();
        assert!(e.contains("--deploy-tenant requires --deploy"), "{e}");

        // OK once --deploy is added.
        o.deploy = true;
        assert!(o.validate_phase_opts(&o.phases()).is_ok());
    }

    #[test]
    fn ami_arch_default_doesnt_trip_validation() {
        // --ami-arch defaults to All; validation should only fire on
        // a non-default value with explicit phases.
        let mut o = opts();
        o.push = true;
        assert!(o.validate_phase_opts(&o.phases()).is_ok());
        o.ami_arch = AmiArch::Aarch64;
        assert!(o.validate_phase_opts(&o.phases()).is_err());
    }
}
