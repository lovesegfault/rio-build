//! Unified k8s deploy. One command surface, provider flag selects
//! k3s (local) vs eks (AWS).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};

use crate::config::XtaskConfig;
use crate::{helm, kube, sh, ui};

mod eks;
mod k3s;
mod kind;
pub mod provider;
pub mod shared;
mod status;

use provider::{Provider, ProviderKind};
use tracing::info;

/// Log level for the deployed rio pods (sets RUST_LOG via helm
/// `global.logLevel`). Precedence: --trace > --debug > --log-level >
/// RIO_LOG_LEVEL env > "debug" default.
#[derive(Args, Default)]
#[group(multiple = false)]
pub struct LogLevelArgs {
    /// Set RUST_LOG=debug in deployed pods.
    #[arg(long)]
    debug: bool,
    /// Set RUST_LOG=trace in deployed pods.
    #[arg(long)]
    trace: bool,
    /// Arbitrary RUST_LOG directive (e.g. "info,rio_scheduler=trace").
    #[arg(long, value_name = "DIRECTIVE")]
    log_level: Option<String>,
}

impl LogLevelArgs {
    /// Resolve against the config fallback.
    fn resolve(&self, cfg: &XtaskConfig) -> String {
        if self.trace {
            "trace".into()
        } else if self.debug {
            "debug".into()
        } else {
            self.log_level
                .clone()
                .unwrap_or_else(|| cfg.log_level.clone())
        }
    }
}

pub const NS: &str = "rio-system";

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
    /// Provider-specific state setup (tofu bucket for eks, no-op for k3s).
    Bootstrap,
    /// Provision backing infra (tofu apply | rook install | kind create).
    Provision {
        /// Skip interactive confirmation prompts.
        #[arg(long)]
        auto: bool,
        /// Cluster node count (kind only: 1 control + N-1 workers).
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..))]
        nodes: u8,
    },
    /// Configure kubectl (aws eks update-kubeconfig | no-op).
    Kubeconfig,
    /// Build + push docker images (ECR | k3s ctr import).
    Push,
    /// helm upgrade rio chart.
    Deploy {
        #[command(flatten)]
        log: LogLevelArgs,
    },
    /// End-to-end build + worker-kill chaos test.
    Smoke,
    /// Install Envoy Gateway operator (dashboard gRPC-Web).
    Envoy,
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
    },
    /// (provision ∥ build) → push → deploy [→ envoy] [→ smoke].
    Up {
        #[arg(long)]
        auto: bool,
        /// Cluster node count (kind only: 1 control + N-1 workers).
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..))]
        nodes: u8,
        /// Also install envoy-gateway (dashboard).
        #[arg(long)]
        envoy: bool,
        /// Run smoke test after deploy.
        #[arg(long)]
        smoke: bool,
        #[command(flatten)]
        log: LogLevelArgs,
    },
    /// Tear down rio + backing infra.
    Destroy,
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
                    "provider required: pass -p {{kind,k3s,eks}} or set RIO_K8S_PROVIDER in .env.local"
                )
            })?
        }
    };
    let p = provider::get(kind);
    match args.cmd {
        K8sCmd::Bootstrap => p.bootstrap(cfg).await,
        K8sCmd::Provision { auto, nodes } => p.provision(cfg, auto, nodes).await,
        K8sCmd::Kubeconfig => p.kubeconfig(cfg).await,
        K8sCmd::Push => {
            let images = ui::step("build", || p.build(cfg)).await?;
            ui::step("push", || p.push(&images, cfg)).await
        }
        K8sCmd::Deploy { log } => p.deploy(cfg, &log.resolve(cfg)).await,
        K8sCmd::Smoke => p.smoke(cfg).await,
        K8sCmd::Envoy => envoy_install().await,
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
        K8sCmd::Status { json } => status::run(&*p, kind, json).await,
        K8sCmd::Up {
            auto,
            nodes,
            envoy,
            smoke,
            log,
        } => {
            let c = p.step_counts();
            let log_level = log.resolve(cfg);
            ui::phase! { "k8s up":
                // build (nix) and provision (tofu/rook) are
                // independent — neither reads the other's outputs.
                // try_join! overlaps the heavy Rust compile with
                // infra bring-up.
                join {
                    let _prov  = "provision" [+c.provision] => p.provision(cfg, auto, nodes);
                    let images = "build"     [+c.build]     => p.build(cfg);
                }
                // push needs tofu outputs (ecr_registry) — serialize.
                "push"             [+c.push]      => p.push(&images, cfg);
                if envoy: "envoy"  [+ENVOY_STEPS] => envoy_install();
                "deploy"           [+c.deploy]    => p.deploy(cfg, &log_level);
                if smoke: "smoke"  [+c.smoke]     => p.smoke(cfg);
            }
            .await
        }
        K8sCmd::Destroy => {
            let what = match kind {
                ProviderKind::Eks => crate::tofu::output(eks::TF_DIR, "cluster_name")
                    .map(|n| format!("EKS cluster '{n}'"))
                    .unwrap_or_else(|_| "the EKS cluster".into()),
                ProviderKind::K3s => "the local k3s rio deployment + rook".into(),
                ProviderKind::Kind => format!("kind cluster '{}'", kind::CLUSTER),
            };
            if !ui::confirm_destroy(&format!("This will DESTROY {what} and all data. Continue?"))? {
                bail!("destroy cancelled");
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
    let _guard = ui::step("establish tunnel", || p.tunnel(port)).await?;

    let store = format!("ssh-ng://rio@localhost:{port}?ssh-key={}", key.display());
    info!("store: {store}");

    let shell = sh::shell()?;
    let _env = shell.push_env(
        "NIX_SSHOPTS",
        "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null",
    );
    f(&shell, &store)
}

/// Envoy Gateway operator (dashboard gRPC-Web → gRPC+mTLS translation).
/// Provider-agnostic — same helm chart, same namespace.
const ENVOY_STEPS: u64 = ui::POLL_STEPS; // wait_rollout
async fn envoy_install() -> Result<()> {
    let shell = sh::shell()?;
    let client = kube::client().await?;

    let eg = sh::read(sh::cmd!(
        shell,
        "nix build --no-link --print-out-paths .#helm-envoy-gateway"
    ))?;

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
