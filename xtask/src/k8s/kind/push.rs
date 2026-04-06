//! Load host-arch images into kind's containerd.
//!
//! `kind load image-archive` imports a docker save tarball directly
//! into the cluster nodes' containerd — no registry, no sudo.

use anyhow::{Context, Result, bail};

use crate::config::XtaskConfig;
use crate::k8s::provider::BuiltImages;
use crate::sh::{self, cmd, shell};
use crate::ui;

use super::CLUSTER;

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
        let load = {
            let sh = shell()?;
            sh::run(cmd!(
                sh,
                "kind load image-archive {path_s} --name {CLUSTER}"
            ))
        };
        ui::step(&format!("kind load {fname}"), || load).await?;
        n += 1;
    }
    if n == 0 {
        bail!("no images in linkFarm — nix build produced nothing?");
    }

    tracing::info!("loaded {n} images (tag :dev baked in by nix/docker.nix)");
    Ok(())
}
