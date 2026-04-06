//! k3s provider: local cluster, Rook-Ceph S3, bitnami PG subchart.

use std::collections::BTreeMap;

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

use crate::config::XtaskConfig;
use crate::k8s::provider::{BuiltImages, Provider, StepCounts};
use crate::k8s::{NS, shared};
use crate::sh::{self, cmd, shell};
use crate::{helm, kube, ssh, ui};

mod push;
mod rook;
// pub(super) so kind can reuse the port-forward tunnel + chaos test.
pub(super) mod smoke;

pub struct K3s;

// Co-located step counts — bump when adding a ui::step to the method.
const PROVISION_STEPS: u64 = 3 + rook::INSTALL_STEPS + rook::S3_BRIDGE_STEPS;
const BUILD_STEPS: u64 = 1; // nix build (single arch)
const DEPLOY_STEPS: u64 = 5; // chart-deps + CRDs + ssh-secret + pg-secret + helm

#[async_trait(?Send)]
impl Provider for K3s {
    fn step_counts(&self) -> StepCounts {
        StepCounts {
            provision: PROVISION_STEPS,
            build: BUILD_STEPS,
            push: shared::IMAGE_COUNT, // ctr import per image
            deploy: DEPLOY_STEPS,
            smoke: 0, // nested phase — see eks comment
        }
    }

    fn context_matches(&self, ctx: &str) -> bool {
        // k3s.yaml's single context is named "default".
        ctx == "default"
    }

    async fn bootstrap(&self, _cfg: &XtaskConfig) -> Result<()> {
        info!("bootstrap: no-op for k3s (no terraform state)");
        Ok(())
    }

    async fn provision(&self, cfg: &XtaskConfig, _auto: bool, _nodes: u8) -> Result<()> {
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

    async fn deploy(&self, cfg: &XtaskConfig, log_level: &str) -> Result<()> {
        let client = kube::client().await?;

        ui::step("chart deps", || async { shared::chart_deps() }).await?;
        ui::step("apply CRDs", || kube::apply_crds(&client)).await?;

        ui::step("ssh secret", || async {
            kube::ensure_namespace(&client, NS, true).await?;
            let authorized = ssh::authorized_keys(cfg)?;
            kube::apply_secret(
                &client,
                NS,
                "rio-gateway-ssh",
                BTreeMap::from([("authorized_keys".into(), authorized)]),
            )
            .await
        })
        .await?;

        ui::step("postgres secret", || shared::ensure_pg_secrets(&client)).await?;

        let tag = std::fs::read_to_string(sh::repo_root().join(".rio-image-tag"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "latest".into());

        ui::step("helm install rio", || async {
            helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
                .namespace(NS)
                .values("infra/helm/rio-build/values/dev.yaml")
                .set("global.image.tag", &tag)
                .set("global.logLevel", log_level)
                .set("postgresql.auth.existingSecret", "rio-postgres-auth")
                .run()
        })
        .await
    }

    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()> {
        smoke::run(cfg).await
    }

    async fn tunnel(&self, local_port: u16) -> Result<shared::ProcessGuard> {
        smoke::tunnel(local_port).await
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
