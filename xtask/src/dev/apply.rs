//! Full local bring-up: CRDs + rook + S3-bridge + helm install rio.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::config::XtaskConfig;
use crate::sh::{self, cmd, repo_root, shell};
use crate::{helm, kube, ssh, ui};

use super::rook;

pub async fn run(cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;

    ui::step("chart deps", || async { chart_deps() }).await?;
    ui::step("rook install", rook::install).await?;
    ui::step("s3 bridge", rook::s3_bridge).await?;

    ui::step("apply CRDs", || kube::apply_crds(&client)).await?;

    ui::step("ssh secret", || async {
        let authorized = ssh::authorized_keys(cfg)?;
        kube::apply_secret(
            &client,
            "rio-system",
            "rio-gateway-ssh",
            BTreeMap::from([("authorized_keys".into(), authorized)]),
        )
        .await
    })
    .await?;

    ui::step("helm install rio", || async {
        helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
            .values("infra/helm/rio-build/values/dev.yaml")
            .set("global.logLevel", &cfg.log_level)
            .run()
    })
    .await
}

pub fn chart_deps() -> Result<()> {
    let sh = shell()?;
    let charts = repo_root().join("infra/helm/rio-build/charts");
    std::fs::create_dir_all(&charts)?;
    let pg = sh::read(cmd!(
        sh,
        "nix build --no-link --print-out-paths .#helm-postgresql"
    ))?;
    let link = charts.join("postgresql");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(pg.trim(), &link)?;
    Ok(())
}
