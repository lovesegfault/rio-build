//! EKS deploy. Replaces `infra/eks.just` + push-images.sh + smoke-test.sh.

use anyhow::Result;
use clap::Subcommand;

use crate::config::XtaskConfig;

mod bootstrap;
mod deploy;
mod destroy;
mod push;
mod smoke;

pub const TF_DIR: &str = "infra/eks";
pub const NS: &str = "rio-system";

#[derive(Subcommand)]
pub enum EksCmd {
    /// Create/update the S3 tofu state bucket (handles first-time setup).
    Bootstrap,
    /// tofu init + apply.
    Apply {
        #[arg(long)]
        auto: bool,
    },
    /// Configure kubectl for the EKS cluster.
    Kubeconfig,
    /// Build docker images (multi-arch) + push to ECR.
    Push,
    /// kubectl apply CRDs + helm upgrade (reads tofu outputs).
    Deploy,
    /// End-to-end smoke test (SSM tunnel, nix build, worker-kill).
    Smoke,
    /// helm rollback to REV (0 = previous).
    Rollback {
        #[arg(default_value_t = 0)]
        rev: u32,
    },
    /// helm release history.
    History,
    /// apply → kubeconfig → push → deploy.
    Up {
        #[arg(long)]
        auto: bool,
    },
    /// Delete WorkerPools + NodeClaims, then tofu destroy.
    Destroy,
}

pub async fn run(cmd: EksCmd, cfg: &XtaskConfig) -> Result<()> {
    match cmd {
        EksCmd::Bootstrap => bootstrap::run(cfg).await,
        EksCmd::Apply { auto } => apply(cfg, auto).await,
        EksCmd::Kubeconfig => kubeconfig(),
        EksCmd::Push => push::run(cfg).await,
        EksCmd::Deploy => deploy::run(cfg).await,
        EksCmd::Smoke => smoke::run(cfg).await,
        EksCmd::Rollback { rev } => crate::helm::rollback("rio", NS, rev),
        EksCmd::History => crate::helm::history("rio", NS),
        EksCmd::Up { auto } => {
            apply(cfg, auto).await?;
            kubeconfig()?;
            push::run(cfg).await?;
            deploy::run(cfg).await
        }
        EksCmd::Destroy => destroy::run().await,
    }
}

async fn apply(cfg: &XtaskConfig, auto: bool) -> Result<()> {
    let aws = aws_config::load_from_env().await;
    let backend = crate::tofu::Backend {
        bucket: crate::tofu::state_bucket(cfg, &aws).await?,
        region: cfg.tfstate_region.clone(),
    };
    crate::tofu::init(TF_DIR, &backend)?;
    crate::tofu::apply(TF_DIR, auto, &[])?;
    Ok(())
}

fn kubeconfig() -> Result<()> {
    let cmd = crate::tofu::output(TF_DIR, "kubeconfig_command")?;
    let sh = crate::sh::shell()?;
    // kubeconfig_command is the full aws-cli invocation; run via bash.
    crate::sh::cmd!(sh, "bash -c {cmd}").run()?;
    Ok(())
}
