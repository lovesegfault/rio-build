//! Unified k8s deploy. One command surface, provider flag selects
//! k3s (local) vs eks (AWS).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};

use crate::config::XtaskConfig;
use crate::{helm, kube, sh, ui};

mod eks;
mod k3s;
pub mod provider;
pub mod shared;

use provider::ProviderKind;

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
    /// Provision backing infra (tofu apply | rook install).
    Provision {
        /// Skip interactive confirmation prompts.
        #[arg(long)]
        auto: bool,
    },
    /// Configure kubectl (aws eks update-kubeconfig | no-op).
    Kubeconfig,
    /// Build + push docker images (ECR | k3s ctr import).
    Push,
    /// helm upgrade rio chart.
    Deploy,
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
    /// (provision ∥ build) → push → deploy [→ envoy] [→ smoke].
    Up {
        #[arg(long)]
        auto: bool,
        /// Also install envoy-gateway (dashboard).
        #[arg(long)]
        envoy: bool,
        /// Run smoke test after deploy.
        #[arg(long)]
        smoke: bool,
    },
    /// Tear down rio + backing infra.
    Destroy,
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
        K8sCmd::Bootstrap => p.bootstrap(cfg).await,
        K8sCmd::Provision { auto } => p.provision(cfg, auto).await,
        K8sCmd::Kubeconfig => p.kubeconfig(cfg).await,
        K8sCmd::Push => {
            let images = ui::step("build", || p.build(cfg)).await?;
            ui::step("push", || p.push(&images, cfg)).await
        }
        K8sCmd::Deploy => p.deploy(cfg).await,
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
        K8sCmd::Up { auto, envoy, smoke } => {
            // `up` has a variable step count (per-image skopeo copies
            // vary with N images; envoy/smoke are flag-conditional).
            // The hint here is eks-baseline; k3s is fewer (no NLB/SSM
            // path, ctr import instead of skopeo×N). Drift detection
            // prints the actual count at end — adjust after first run
            // if the warning fires.
            const UP_BASE: u64 = 42;
            let hint = UP_BASE + envoy as u64 + if smoke { 28 } else { 0 };
            ui::phase("k8s up", hint, || async {
                // build (nix) and provision (tofu/rook) are independent —
                // neither reads the other's outputs. Run them concurrently;
                // the heavy Rust compile overlaps with infra bring-up.
                let (prov, images) = tokio::join!(
                    ui::step("provision", || p.provision(cfg, auto)),
                    ui::step("build", || p.build(cfg)),
                );
                prov?;
                let images = images?;
                // push needs tofu outputs (ecr_registry) — serialize after.
                ui::step("push", || p.push(&images, cfg)).await?;
                if envoy {
                    ui::step("envoy", envoy_install).await?;
                }
                ui::step("deploy", || p.deploy(cfg)).await?;
                if smoke {
                    ui::step("smoke", || p.smoke(cfg)).await?;
                }
                Ok(())
            })
            .await
        }
        K8sCmd::Destroy => {
            let what = match kind {
                ProviderKind::Eks => crate::tofu::output(eks::TF_DIR, "cluster_name")
                    .map(|n| format!("EKS cluster '{n}'"))
                    .unwrap_or_else(|_| "the EKS cluster".into()),
                ProviderKind::K3s => "the local k3s rio deployment + rook".into(),
            };
            if !ui::confirm_destroy(&format!("This will DESTROY {what} and all data. Continue?"))? {
                bail!("destroy cancelled");
            }
            p.destroy(cfg).await
        }
    }
}

/// Envoy Gateway operator (dashboard gRPC-Web → gRPC+mTLS translation).
/// Provider-agnostic — same helm chart, same namespace.
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
