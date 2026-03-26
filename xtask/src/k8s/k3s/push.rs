//! Build images for the host arch and import into k3s's containerd.
//!
//! Much simpler than EKS push: no ECR login, no skopeo, no manifest
//! lists. Single-arch (host), imported directly via `k3s ctr`.

use anyhow::{Context, Result, bail};

use crate::config::XtaskConfig;
use crate::sh::{self, cmd, repo_root, shell};
use crate::{git, ui};

pub async fn run(_cfg: &XtaskConfig) -> Result<()> {
    let sh = shell()?;
    let repo = git::open()?;
    let tag = git::image_tag(&repo)?;

    // Host arch only — k3s node IS this machine.
    let sys = match std::env::consts::ARCH {
        "x86_64" => "x86_64-linux",
        "aarch64" => "aarch64-linux",
        other => bail!("unsupported host arch: {other}"),
    };

    let out = tempfile::tempdir()?;
    let link = out.path().join("images");
    let link_s = link.to_str().unwrap();
    let attr = format!(".#packages.{sys}.dockerImages");

    ui::step(&format!("nix build {attr}"), || {
        sh::run(cmd!(sh, "nix build {attr} -L --out-link {link_s}"))
    })
    .await?;

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
        ui::step(&format!("ctr import {fname}"), || {
            sh::run(cmd!(sh, "sudo k3s ctr images import {path_s}"))
        })
        .await?;
        n += 1;
    }
    if n == 0 {
        bail!("no images in linkFarm — nix build produced nothing?");
    }

    std::fs::write(repo_root().join(".rio-image-tag"), format!("{tag}\n"))?;
    tracing::info!("imported {n} images, tag: {tag}");
    Ok(())
}
