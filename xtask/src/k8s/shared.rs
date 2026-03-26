//! Helpers all three providers (kind/k3s/eks) use.

use anyhow::{Result, bail};

use crate::config::XtaskConfig;
use crate::k8s::provider::BuiltImages;
use crate::sh::{self, cmd, repo_root, shell};
use crate::{git, ui};

/// Number of docker images in `nix/docker.nix`'s dockerImages
/// linkFarm. All providers push this many (eks: ×2 arches + manifest;
/// k3s/kind: ×1 arch import/load). Bump when adding/removing an image.
pub const IMAGE_COUNT: u64 = 9;

/// Subcharts listed in Chart.yaml's `dependencies:`. Helm validates
/// charts/ against Chart.yaml BEFORE evaluating `condition: *.enabled`,
/// so every entry must be symlinked even when disabled for a given
/// provider (eks uses Aurora+S3, k3s uses Rook).
const SUBCHARTS: &[&str] = &["postgresql", "rustfs"];

/// Symlink all subcharts from their nix-store derivations into
/// `infra/helm/rio-build/charts/`. Gitignored.
pub fn chart_deps() -> Result<()> {
    let sh = shell()?;
    let charts = repo_root().join("infra/helm/rio-build/charts");
    std::fs::create_dir_all(&charts)?;
    for name in SUBCHARTS {
        let attr = format!(".#helm-{name}");
        let path = sh::read(cmd!(sh, "nix build --no-link --print-out-paths {attr}"))?;
        let link = charts.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(path.trim(), &link)?;
    }
    Ok(())
}

/// Build the dockerImages linkFarm for the host arch only.
/// Shared by k3s (`ctr import`) and kind (`kind load image-archive`) —
/// both run on the local machine so only need the host arch.
pub async fn build_host_arch(_cfg: &XtaskConfig) -> Result<BuiltImages> {
    let sh = shell()?;
    let repo = git::open()?;
    let tag = git::image_tag(&repo)?;

    let sys = match std::env::consts::ARCH {
        "x86_64" => "x86_64-linux",
        "aarch64" => "aarch64-linux",
        other => bail!("unsupported host arch: {other}"),
    };

    let dir = tempfile::tempdir()?;
    let link = dir.path().join("images");
    let link_s = link.to_str().unwrap();
    let attr = format!(".#packages.{sys}.dockerImages");

    ui::step(&format!("nix build {attr}"), || {
        sh::run(cmd!(sh, "nix build {attr} -L --out-link {link_s}"))
    })
    .await?;
    Ok(BuiltImages { dir, tag })
}

/// Guard that kills a child process on drop. Used for port-forward
/// and SSM tunnel processes in smoke tests.
pub struct ProcessGuard(pub tokio::process::Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}
