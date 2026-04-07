//! k3s provider: local cluster, Rook-Ceph S3, bitnami PG subchart.

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

use crate::config::XtaskConfig;
use crate::k8s::provider::{BuiltImages, Provider};
use crate::k8s::{NS, ensure_namespaces, shared};
use crate::sh::{self, cmd, shell};
use crate::{helm, kube, ui};

mod push;
mod rook;
pub(super) mod smoke;

pub struct K3s;

#[async_trait]
impl Provider for K3s {
    fn context_matches(&self, ctx: &str) -> bool {
        // k3s.yaml's single context is named "default".
        ctx == "default"
    }

    async fn bootstrap(&self, _cfg: &XtaskConfig) -> Result<()> {
        info!("bootstrap: no-op for k3s (no terraform state)");
        Ok(())
    }

    async fn provision(&self, cfg: &XtaskConfig, _auto: bool) -> Result<()> {
        ui::step("kubeconfig", || self.kubeconfig(cfg)).await?;
        ui::step("rook install", rook::install).await?;
        ui::step("s3 bridge", rook::s3_bridge).await
    }

    async fn kubeconfig(&self, _cfg: &XtaskConfig) -> Result<()> {
        let src = std::path::Path::new("/etc/rancher/k3s/k3s.yaml");
        if !src.exists() {
            info!(
                "kubeconfig: {} not found — is k3s installed?",
                src.display()
            );
            return Ok(());
        }
        let dst = sh::kubeconfig_path();
        std::fs::create_dir_all(dst.parent().unwrap())?;
        // k3s.yaml is root-readable only. `sudo cat` then write as us
        // so downstream kube-rs/helm can read it.
        let shell = sh::shell()?;
        let body = sh::read(sh::cmd!(shell, "sudo cat /etc/rancher/k3s/k3s.yaml"))?;
        std::fs::write(&dst, body)?;
        info!("kubeconfig: copied {} → {}", src.display(), dst.display());
        Ok(())
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
        _skip_preflight: bool,
        _no_hooks: bool,
    ) -> Result<()> {
        let client = kube::client().await?;

        ui::step("chart deps", shared::chart_deps).await?;
        ui::step("apply CRDs", || kube::apply_crds(&client)).await?;

        ui::step("namespaces + ssh secret", || async {
            ensure_namespaces(&client).await?;
            shared::ensure_gateway_ssh_secret(&client, cfg, tenant).await
        })
        .await?;

        ui::step("postgres secret", || shared::ensure_pg_secrets(&client)).await?;

        let jwt = ui::step("jwt keypair", || shared::ensure_jwt_keypair(&client)).await?;

        // nix/docker.nix hardcodes tag="dev" in the tarballs. ctr import
        // uses the baked-in tag — no retag step. Git-SHA tags are
        // EKS-only (skopeo retags on push to ECR). Same tag → Deployment
        // spec unchanged → kube won't re-pull on its own; forced restart
        // below handles that.
        let was_installed = helm::release_status("rio", NS)?.is_some();
        ui::step("helm install rio", || async {
            helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
                .namespace(NS)
                .values("infra/helm/rio-build/values/dev.yaml")
                .set("global.image.tag", "dev")
                .set("global.logLevel", log_level)
                .set("postgresql.auth.existingSecret", "rio-postgres-auth")
                .set("jwt.enabled", "true")
                .set("jwt.signingSeed", &jwt.seed)
                .set("jwt.publicKey", &jwt.pubkey)
                .run()
        })
        .await?;

        if was_installed {
            ui::step("rollout restart (same-tag push)", || {
                shared::rollout_restart_rio(&client)
            })
            .await
        } else {
            ui::step_skip("rollout restart", "first install");
            Ok(())
        }
    }

    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()> {
        smoke::run(cfg).await
    }

    async fn tunnel(&self, local_port: u16) -> Result<shared::ProcessGuard> {
        smoke::tunnel(local_port).await
    }

    async fn tunnel_grpc(
        &self,
        sched_port: u16,
        store_port: u16,
    ) -> Result<((u16, shared::ProcessGuard), (u16, shared::ProcessGuard))> {
        smoke::tunnel_grpc(sched_port, store_port).await
    }

    async fn destroy(&self, _cfg: &XtaskConfig) -> Result<()> {
        ui::step("helm uninstall", || async {
            helm::uninstall("rio", NS)?;
            let client = kube::client().await?;
            kube::delete_secret(&client, NS, "rio-gateway-ssh").await?;
            kube::delete_secret(&client, NS, "rio-s3-creds").await?;
            kube::delete_secret(&client, NS, "rio-postgres").await?;
            kube::delete_secret(&client, NS, "rio-postgres-auth").await
        })
        .await?;

        ui::step("rook teardown", || async {
            let sh = shell()?;
            helm::uninstall("rook-ceph-cluster", "rook-ceph")?;
            let _ = sh::run_sync(cmd!(
                sh,
                "kubectl -n rook-ceph wait --for=delete cephcluster/rook-ceph --timeout=300s"
            ));
            helm::uninstall("rook-ceph", "rook-ceph")?;
            sh::run_sync(cmd!(sh, "kubectl delete ns rook-ceph --ignore-not-found"))
        })
        .await
    }
}
