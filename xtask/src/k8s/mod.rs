//! Unified k8s deploy. One command surface, provider flag selects
//! k3s (local) vs eks (AWS).

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};

use crate::config::XtaskConfig;
use crate::{helm, sh, ui};

mod chaos;
pub mod client;
mod eks;
mod k3s;
mod phases;
pub mod provider;
pub mod shared;
pub(crate) mod status;
mod stress;

use client as kube;

pub use eks::ami::AmiArch;
pub use phases::Phase;
use phases::{PhaseParams, run_up_phases};
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
    /// After deploy, block until Karpenter has replaced all Drifted
    /// NodeClaims (AMI rollout complete). Without this, builds started
    /// immediately after `up` may be disrupted by node drift eviction.
    /// EKS-only.
    #[arg(long)]
    wait_drift: bool,
    /// Allow this CIDR to reach the gateway NLB directly (internet-
    /// facing). Repeatable. When unset, the NLB stays `internal`
    /// (reachable only via the SSM bastion). Changing set↔unset
    /// recreates the NLB — new DNS name. EKS-only.
    #[arg(long = "public-cidr", value_name = "CIDR")]
    public_cidr: Vec<String>,

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
        req!(self.wait_drift, "--wait-drift", Phase::Deploy);
        req!(!self.public_cidr.is_empty(), "--public-cidr", Phase::Deploy);
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
    /// → push), then deploy. Phase flags select a subset.
    Up(UpOpts),
    /// EKS-unique e2e: NLB health, SSM tunnel, S3-chunked build (IRSA).
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
        /// Append a one-shot Prometheus scrape of scheduler-leader +
        /// store replicas: mailbox depth, queued/running, workers,
        /// mean actor-cmd latency. The "is the actor wedged?" gauges.
        /// <5s; no Prometheus install required.
        #[arg(long, short = 'm')]
        metrics: bool,
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
    /// Run rio-cli LOCALLY against a port-forwarded scheduler+store
    /// (plaintext gRPC; encryption is at the Cilium overlay so the
    /// port-forward needs no client cert). Prefer this over `kubectl
    /// exec deploy/rio-scheduler -- rio-cli …`: in-pod exec forces the
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
    /// Stress-build harness: N parallel builds through SSM tunnels.
    /// Foreground — Ctrl-C tears down tunnels + builds.
    #[command(subcommand)]
    Stress(stress::StressCmd),
    /// NixOS node AMI management (ADR-021). EKS-only — `up --ami`
    /// builds + registers; this is the maintenance side.
    #[command(subcommand)]
    Ami(AmiCmd),
    /// Grant build access to an SSH public key under its own tenant:
    /// creates the tenant, appends the key (with comment = tenant
    /// name) to the rio-gateway-ssh Secret, and rolls the gateway so
    /// the key takes effect. Idempotent — re-running for the same
    /// key/tenant pair is a no-op. Admin (rio-cli) access is NOT
    /// granted; the key only authenticates the ssh-ng build path.
    Grant {
        /// The user's OpenSSH public key — either inline
        /// (`'ssh-ed25519 AAAA... alice@host'`) or a path to a `.pub`
        /// file. Inline is tried first; if it doesn't parse as a key,
        /// the value is read as a file path.
        pubkey: String,
        /// Tenant name. Becomes the authorized_keys comment, which
        /// the gateway maps to `SubmitBuild.tenant_name`.
        #[arg(long)]
        tenant: String,
        /// Force a gateway rollout-restart so the key takes effect
        /// immediately. Without this, the gateway hot-reloads
        /// authorized_keys within ~70s (kubelet Secret refresh ~60s
        /// plus the gateway's 10s mtime poll) — no disruption to
        /// in-flight sessions.
        #[arg(long)]
        restart: bool,
    },
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
            metrics,
        } => status::run(&*p, kind, cfg, json, reap_stuck_nodes, metrics).await,
        K8sCmd::Grafana { port } => status::grafana(port).await,
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
        K8sCmd::Grant {
            pubkey,
            tenant,
            restart,
        } => shared::grant(&pubkey, &tenant, restart).await,
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
        deploy: provider::DeployOpts {
            log_level: o
                .deploy_log_level
                .clone()
                .unwrap_or_else(|| cfg.log_level.clone()),
            tenant: o.deploy_tenant.clone(),
            skip_preflight: o.deploy_skip_preflight,
            no_hooks: o.deploy_no_hooks,
            wait_drift: o.wait_drift,
            // CLI > env: any --public-cidr flag wins; otherwise fall
            // back to RIO_PUBLIC_CIDRS so a bare `up --deploy` keeps
            // the allowlist instead of reverting the NLB to internal.
            public_cidrs: if o.public_cidr.is_empty() {
                cfg.public_cidrs.clone()
            } else {
                o.public_cidr.clone()
            },
        },
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
        run_up_phases(p, cfg, &selected, pp, ami_branch).await?;
        if !explicit {
            info!("all phases done — run `cargo xtask k8s -p {kind} smoke` to verify");
        }
        Ok(())
    })
    .await
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
/// Port-forward scheduler:9001 + store:9002, set `RIO_SCHEDULER_ADDR`
/// / `RIO_STORE_ADDR` on a prepared shell, run `f`. Tunnels tear down
/// on return.
///
/// gRPC is plaintext (encryption is at the Cilium overlay), so no
/// client cert is fetched and no `:authority` validation is in play.
/// Enables running rio-cli LOCALLY instead of via `kubectl exec
/// deploy/rio-scheduler`, which in turn lets the scheduler image drop
/// rio-cli + its transitive deps (jq, column, …) — every extra binary
/// in a control-plane image is an execution primitive in a compromised
/// pod.
pub async fn with_cli_tunnel<F>(p: &dyn Provider, sched: u16, store: u16, f: F) -> Result<()>
where
    F: FnOnce(&xshell::Shell) -> Result<()>,
{
    let ((sched, _g1), (store, _g2)) =
        ui::step("tunnel scheduler+store", || p.tunnel_grpc(sched, store)).await?;

    let sh = sh::shell()?;
    let _e1 = sh.push_env("RIO_SCHEDULER_ADDR", format!("localhost:{sched}"));
    let _e2 = sh.push_env("RIO_STORE_ADDR", format!("localhost:{store}"));
    f(&sh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> UpOpts {
        UpOpts::default()
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
