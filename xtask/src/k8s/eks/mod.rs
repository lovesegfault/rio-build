//! EKS provider: tofu-managed cluster, ECR images, Aurora PG, real S3.

use anyhow::Result;
use async_trait::async_trait;

use crate::config::XtaskConfig;
use crate::k8s::provider::{BuiltImages, Provider};
use crate::{sh, tofu, ui};

pub mod ami;
mod bootstrap;
mod deploy;
mod destroy;
mod push;
// pub so k3s/smoke.rs can reuse the provider-agnostic chaos helpers
// (tenant setup, ssh key, worker-kill, smoke_build).
pub mod smoke;

pub const TF_DIR: &str = "infra/eks";

pub struct Eks;

#[async_trait]
impl Provider for Eks {
    fn context_matches(&self, ctx: &str) -> bool {
        // `aws eks update-kubeconfig` names the context
        // `arn:aws:eks:<region>:<account>:cluster/<name>`.
        ctx.starts_with("arn:aws:eks:")
    }

    async fn bootstrap(&self, cfg: &XtaskConfig) -> Result<()> {
        bootstrap::run(cfg).await
    }

    async fn provision(&self, cfg: &XtaskConfig, auto: bool, _nodes: u8) -> Result<()> {
        let backend = ui::step("resolve tfstate backend", || async {
            let aws = crate::aws::config(None).await;
            Ok(tofu::Backend {
                bucket: tofu::state_bucket(cfg, aws).await?,
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

    async fn deploy(
        &self,
        cfg: &XtaskConfig,
        log_level: &str,
        tenant: Option<&str>,
        skip_preflight: bool,
        no_hooks: bool,
    ) -> Result<()> {
        deploy::run(cfg, log_level, tenant, skip_preflight, no_hooks).await
    }

    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()> {
        smoke::run(cfg).await
    }

    async fn tunnel(&self, local_port: u16) -> Result<crate::k8s::shared::ProcessGuard> {
        smoke::ssm_tunnel(local_port).await
    }

    async fn tunnel_grpc(
        &self,
        sched_port: u16,
        store_port: u16,
    ) -> Result<(
        (u16, crate::k8s::shared::ProcessGuard),
        (u16, crate::k8s::shared::ProcessGuard),
    )> {
        // NOT SSM — scheduler/store aren't behind the NLB. kubectl
        // reaches them via the apiserver proxy, which `aws eks
        // update-kubeconfig` (provision step) already set up.
        crate::k8s::k3s::smoke::tunnel_grpc(sched_port, store_port).await
    }

    async fn destroy(&self, _cfg: &XtaskConfig) -> Result<()> {
        destroy::run().await
    }
}

fn kubeconfig() -> Result<()> {
    let path = sh::kubeconfig_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let path_s = path.to_str().unwrap();
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let cluster = tf.get("cluster_name")?;
    let shell = sh::shell()?;
    sh::run_sync(sh::cmd!(
        shell,
        "aws eks update-kubeconfig --region {region} --name {cluster} --kubeconfig {path_s}"
    ))
}
