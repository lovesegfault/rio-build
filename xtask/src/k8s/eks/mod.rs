//! EKS provider: tofu-managed cluster, ECR images, Aurora PG, real S3.

use anyhow::Result;
use async_trait::async_trait;

use crate::config::XtaskConfig;
use crate::k8s::provider::{BuiltImages, Provider, StepCounts};
use crate::{sh, tofu, ui};

mod bootstrap;
mod deploy;
mod destroy;
mod push;
// pub so k3s/smoke.rs can reuse the provider-agnostic chaos helpers
// (tenant setup, ssh key, worker-kill, smoke_build).
pub mod smoke;

pub const TF_DIR: &str = "infra/eks";

pub struct Eks;

// Co-located step counts — bump when adding a ui::step to the method.
const PROVISION_STEPS: u64 = 3 + tofu::APPLY_STEPS; // backend+init+kubeconfig + plan+apply
const BUILD_STEPS: u64 = 1; // nix build multi-arch (nix copy is conditional on remote)
const DEPLOY_STEPS: u64 = 4 + ui::POLL_STEPS; // CRDs + wait-crd + ns+secret + chart-deps + helm

#[async_trait(?Send)]
impl Provider for Eks {
    fn step_counts(&self) -> StepCounts {
        use crate::k8s::shared::IMAGE_COUNT;
        StepCounts {
            provision: PROVISION_STEPS,
            build: BUILD_STEPS,
            // ECR login + skopeo per-image × 2 arches + manifest per-image
            push: 1 + IMAGE_COUNT * 2 + IMAGE_COUNT,
            deploy: DEPLOY_STEPS,
            // smoke::run calls its own ui::phase! — creates a nested
            // bar. From the outer phase's view, smoke is 0 inner
            // steps (the nested phase's steps belong to the nested
            // bar, not ours). The wrapping ui::step("smoke", ...) in
            // `up` is the 1 that counts.
            smoke: 0,
        }
    }

    async fn bootstrap(&self, cfg: &XtaskConfig) -> Result<()> {
        bootstrap::run(cfg).await
    }

    async fn provision(&self, cfg: &XtaskConfig, auto: bool) -> Result<()> {
        let backend = ui::step("resolve tfstate backend", || async {
            let aws = aws_config::load_from_env().await;
            Ok(tofu::Backend {
                bucket: tofu::state_bucket(cfg, &aws).await?,
                region: cfg.tfstate_region.clone(),
            })
        })
        .await?;
        ui::step("tofu init", || async { tofu::init(TF_DIR, &backend) }).await?;
        tofu::apply(TF_DIR, auto, &[]).await?;
        ui::step("kubeconfig", || async { kubeconfig() }).await
    }

    async fn kubeconfig(&self, _cfg: &XtaskConfig) -> Result<()> {
        kubeconfig()
    }

    async fn build(&self, cfg: &XtaskConfig) -> Result<BuiltImages> {
        push::build(cfg).await
    }

    async fn push(&self, images: &BuiltImages, cfg: &XtaskConfig) -> Result<()> {
        push::push(images, cfg).await
    }

    async fn deploy(&self, cfg: &XtaskConfig) -> Result<()> {
        deploy::run(cfg).await
    }

    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()> {
        smoke::run(cfg).await
    }

    async fn tunnel(&self, local_port: u16) -> Result<crate::k8s::shared::ProcessGuard> {
        smoke::ssm_tunnel(local_port).await
    }

    async fn destroy(&self, _cfg: &XtaskConfig) -> Result<()> {
        destroy::run().await
    }
}

fn kubeconfig() -> Result<()> {
    let path = sh::kubeconfig_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let path_s = path.to_str().unwrap();
    let region = tofu::output(TF_DIR, "region")?;
    let cluster = tofu::output(TF_DIR, "cluster_name")?;
    let shell = sh::shell()?;
    sh::run_sync(sh::cmd!(
        shell,
        "aws eks update-kubeconfig --region {region} --name {cluster} --kubeconfig {path_s}"
    ))
}
