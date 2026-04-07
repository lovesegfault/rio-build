//! Import host-arch images into k3s's containerd.
//!
//! Much simpler than EKS push: no ECR login, no skopeo, no manifest
//! lists.

use anyhow::{Context, Result, bail};

use crate::config::XtaskConfig;
use crate::k8s::provider::BuiltImages;
use crate::sh::{self, cmd, shell};
use crate::ui;

pub use crate::k8s::shared::build_host_arch as build;

pub async fn push(images: &BuiltImages, _cfg: &XtaskConfig) -> Result<()> {
    let link = images.dir.path().join("images");

    let mut n = 0;
    for entry in std::fs::read_dir(&link)? {
        let path = entry?.path();
        let Some(fname) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !fname.ends_with(".tar.zst") {
            continue;
        }
        let path_s = path.to_str().context("non-utf8 path")?;
        let import = {
            let sh = shell()?;
            sh::run(cmd!(sh, "sudo k3s ctr images import {path_s}"))
        };
        ui::step(&format!("ctr import {fname}"), || import).await?;
        n += 1;
    }
    if n == 0 {
        bail!("no images in linkFarm — nix build produced nothing?");
    }

    tracing::info!("imported {n} images (tag :dev baked in by nix/docker.nix)");
    Ok(())
}
