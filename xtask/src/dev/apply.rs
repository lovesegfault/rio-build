//! Full local bring-up: CRDs + rook + S3-bridge + helm install rio.

use std::collections::BTreeMap;

use anyhow::Result;
use tracing::info;

use crate::config::XtaskConfig;
use crate::sh::{cmd, repo_root, shell};
use crate::{helm, kube, ssh};

use super::rook;

pub async fn run(cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;

    // Subchart symlink: helm validates charts/ against Chart.yaml
    // BEFORE evaluating condition: postgresql.enabled. Gitignored.
    chart_deps()?;

    rook::install().await?;
    rook::s3_bridge().await?;

    info!("applying CRDs");
    kube::apply_crds(&client).await?;

    info!("applying ssh secret");
    let authorized = ssh::authorized_keys(cfg)?;
    kube::apply_secret(
        &client,
        "rio-system",
        "rio-gateway-ssh",
        BTreeMap::from([("authorized_keys".into(), authorized)]),
    )
    .await?;

    info!("helm install rio");
    helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
        .values("infra/helm/rio-build/values/dev.yaml")
        .set("global.logLevel", &cfg.log_level)
        .run()?;
    Ok(())
}

pub fn chart_deps() -> Result<()> {
    let sh = shell()?;
    let charts = repo_root().join("infra/helm/rio-build/charts");
    std::fs::create_dir_all(&charts)?;
    let pg = cmd!(
        sh,
        "nix build --no-link --print-out-paths .#helm-postgresql"
    )
    .read()?;
    let link = charts.join("postgresql");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(pg.trim(), &link)?;
    Ok(())
}
