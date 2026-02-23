use std::fs;
use std::path::{Path, PathBuf};

use nix::mount::{MntFlags, MsFlags};

/// An active overlayfs mount with FUSE lower layer and per-build upper layer.
///
/// The overlay is cleaned up (unmounted + directories removed) on drop.
pub struct OverlayMount {
    #[allow(dead_code)] // retained for diagnostics
    lower: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
    mounted: bool,
}

impl OverlayMount {
    /// The lower layer path (the FUSE mount point).
    #[allow(dead_code)] // public API for future use
    pub fn lower_dir(&self) -> &Path {
        &self.lower
    }

    /// The upper layer path where build outputs will appear.
    pub fn upper_dir(&self) -> &Path {
        &self.upper
    }

    /// The work directory used by overlayfs internally.
    #[allow(dead_code)] // public API for future use
    pub fn work_dir(&self) -> &Path {
        &self.work
    }

    /// The merged view path where the overlay is mounted.
    pub fn merged_dir(&self) -> &Path {
        &self.merged
    }
}

impl Drop for OverlayMount {
    fn drop(&mut self) {
        if self.mounted {
            if let Err(e) = teardown_overlay_inner(&self.merged, &self.upper, &self.work) {
                tracing::error!(
                    merged = %self.merged.display(),
                    error = %e,
                    "failed to teardown overlay in Drop"
                );
            }
            self.mounted = false;
        }
    }
}

/// Set up an overlayfs mount.
///
/// Creates upper, work, and merged directories under `base_dir/{build_id}/`,
/// then mounts overlayfs with `lower` as the lower layer.
///
/// Requires `CAP_SYS_ADMIN`.
pub fn setup_overlay(
    lower: &Path,
    base_dir: &Path,
    build_id: &str,
) -> anyhow::Result<OverlayMount> {
    anyhow::ensure!(
        !build_id.contains('/') && !build_id.contains('\0') && !build_id.is_empty(),
        "build_id must not contain path separators or be empty: {build_id:?}"
    );

    let build_dir = base_dir.join(build_id);
    let upper = build_dir.join("upper");
    let work = build_dir.join("work");
    let merged = build_dir.join("merged");

    fs::create_dir_all(&upper)?;
    fs::create_dir_all(&work)?;
    fs::create_dir_all(&merged)?;

    let mount_data = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );

    tracing::info!(
        build_id,
        lower = %lower.display(),
        upper = %upper.display(),
        merged = %merged.display(),
        mount_data = %mount_data,
        "mounting overlayfs"
    );

    nix::mount::mount(
        Some("overlay"),
        &merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(mount_data.as_str()),
    )
    .map_err(|e| anyhow::anyhow!("mount overlay failed: {e} (mount_data: {mount_data})"))?;

    Ok(OverlayMount {
        lower: lower.to_path_buf(),
        upper,
        work,
        merged,
        mounted: true,
    })
}

fn teardown_overlay_inner(merged: &Path, upper: &Path, work: &Path) -> anyhow::Result<()> {
    tracing::info!(merged = %merged.display(), "unmounting overlayfs");

    nix::mount::umount2(merged, MntFlags::MNT_DETACH)
        .map_err(|e| anyhow::anyhow!("umount2 overlay failed: {e}"))?;

    // Clean up directories (best-effort, but log failures)
    for (label, path) in [("upper", upper), ("work", work), ("merged", merged)] {
        if let Err(e) = fs::remove_dir_all(path) {
            tracing::warn!(path = %path.display(), layer = label, error = %e,
                "failed to remove overlay directory during cleanup");
        }
    }

    Ok(())
}

/// Explicitly tear down an overlay mount.
pub fn teardown_overlay(mut mount: OverlayMount) -> anyhow::Result<()> {
    let result = teardown_overlay_inner(&mount.merged, &mount.upper, &mount.work);
    mount.mounted = false;
    result
}

/// CLI entry point for the overlay-setup subcommand.
pub fn run_overlay_setup(lower: &Path, base_dir: &Path, build_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        lower.is_dir(),
        "lower directory does not exist: {}",
        lower.display()
    );

    let mount = setup_overlay(lower, base_dir, build_id)?;

    tracing::info!(
        merged = %mount.merged_dir().display(),
        upper = %mount.upper_dir().display(),
        "overlay mounted successfully — press Ctrl+C to unmount"
    );

    // Wait for signal
    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        if tx.send(()).is_err() {
            eprintln!("warning: signal channel receiver dropped");
        }
    })
    .map_err(|e| anyhow::anyhow!("failed to register signal handler: {e}"))?;

    // Intentionally ignore RecvError — if the signal handler fails, proceed with teardown
    let _ = rx.recv();

    teardown_overlay(mount)?;
    tracing::info!("overlay torn down");

    Ok(())
}

/// Create the Nix state directory structure in the overlay upper layer.
///
/// This creates `nix/var/nix/db/` under the upper directory so the
/// synthetic SQLite DB can be placed there.
pub fn prepare_nix_state_dirs(upper: &Path) -> anyhow::Result<PathBuf> {
    let db_dir = upper.join("nix/var/nix/db");
    fs::create_dir_all(&db_dir)?;
    Ok(db_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_nix_state_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = prepare_nix_state_dirs(dir.path()).unwrap();

        assert!(db_dir.exists());
        assert!(db_dir.is_dir());
        assert_eq!(db_dir, dir.path().join("nix/var/nix/db"));
    }

    #[test]
    fn test_overlay_mount_paths() {
        // Test path construction without actually mounting (no CAP_SYS_ADMIN)
        let base = PathBuf::from("/tmp/overlays");
        let build_id = "test-build-42";

        let expected_upper = base.join(build_id).join("upper");
        let expected_work = base.join(build_id).join("work");
        let expected_merged = base.join(build_id).join("merged");

        assert_eq!(
            expected_upper,
            PathBuf::from("/tmp/overlays/test-build-42/upper")
        );
        assert_eq!(
            expected_work,
            PathBuf::from("/tmp/overlays/test-build-42/work")
        );
        assert_eq!(
            expected_merged,
            PathBuf::from("/tmp/overlays/test-build-42/merged")
        );
    }
}
