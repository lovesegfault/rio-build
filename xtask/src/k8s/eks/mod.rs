//! EKS provider: tofu-managed cluster, ECR images, Aurora PG, real S3.

use anyhow::Result;
use async_trait::async_trait;

use crate::config::XtaskConfig;
use crate::k8s::provider::{BuiltImages, DeployOpts, Provider};
use crate::{sh, tofu, ui};

pub mod ami;
mod bootstrap;
pub(super) mod deploy;
pub(in crate::k8s) mod destroy;
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

    async fn provision(&self, cfg: &XtaskConfig, auto: bool) -> Result<()> {
        init_backend(cfg).await?;

        // gateway_dns + cloudflare_api_token sourced from RIO_DNS_* /
        // RIO_CLOUDFLARE_TOKEN in .env.local so the domain string and
        // secret never land in the repo. Non-secret vars go via -var
        // (visible in plan output for debugging); the token goes via
        // process env so it stays out of argv.
        let gw_dns_json = cfg.dns_provider.as_deref().map(|p| {
            serde_json::json!({
                "provider": p,
                "zone": cfg.dns_zone.as_deref().unwrap_or_default(),
                "prefix": cfg.dns_prefix.as_deref().unwrap_or_default(),
            })
            .to_string()
        });
        // hubble_ui_enabled defaults false (variables.tf); xtask-driven
        // dev/QA clusters get the web UI for flow debugging.
        let mut vars: Vec<(&str, &str)> = vec![("hubble_ui_enabled", "true")];
        if let Some(j) = gw_dns_json.as_deref() {
            vars.push(("gateway_dns", j));
        }
        let mut envs: Vec<(&str, &str)> = vec![];
        if let Some(t) = cfg.cloudflare_token.as_deref() {
            envs.push(("TF_VAR_cloudflare_api_token", t));
        }

        tofu::apply(TF_DIR, auto, &vars, &envs).await?;
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

    async fn deploy(&self, cfg: &XtaskConfig, opts: &DeployOpts) -> Result<()> {
        deploy::run(cfg, opts).await
    }

    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()> {
        smoke::run(cfg).await
    }

    async fn tunnel(&self, local_port: u16) -> Result<crate::k8s::shared::ProcessGuard> {
        smoke::gateway_port_forward(local_port).await
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
        crate::k8s::shared::tunnel_grpc(sched_port, store_port).await
    }

    async fn destroy(&self, cfg: &XtaskConfig) -> Result<()> {
        destroy::run(cfg).await
    }
}

/// Resolve the S3 tfstate backend (bucket from STS account-id, region
/// from config) and `tofu init -reconfigure` against it. Shared by
/// `provision` and `destroy` so a stale `.terraform/` — init'd against
/// a different account's bucket — can't leak into the next tofu run.
pub(super) async fn init_backend(cfg: &XtaskConfig) -> Result<()> {
    let backend = ui::step("resolve tfstate backend", || async {
        let aws = crate::aws::config(None).await;
        Ok(tofu::Backend {
            bucket: tofu::state_bucket(cfg, aws).await?,
            region: cfg.tfstate_region.clone(),
        })
    })
    .await?;
    ui::step("tofu init", || async { tofu::init(TF_DIR, &backend) }).await
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
