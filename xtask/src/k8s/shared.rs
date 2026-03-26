//! Helpers both k3s and eks providers use.

use anyhow::Result;

use crate::sh::{self, cmd, repo_root, shell};

/// Number of docker images in `nix/docker.nix`'s dockerImages
/// linkFarm. Both providers push this many (eks: ×2 arches + manifest;
/// k3s: ×1 arch ctr import). Bump when adding/removing an image.
pub const IMAGE_COUNT: u64 = 9;

/// Symlink the postgresql subchart. Helm validates charts/ against
/// Chart.yaml BEFORE evaluating `condition: postgresql.enabled`, so
/// the dir must exist even when the subchart is disabled (eks uses
/// Aurora). Gitignored; nix-store symlink.
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

/// Guard that kills a child process on drop. Used for port-forward
/// and SSM tunnel processes in smoke tests.
pub struct ProcessGuard(pub tokio::process::Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}
