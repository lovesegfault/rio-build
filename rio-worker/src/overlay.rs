//! Per-build overlayfs management.
//!
//! Each build gets its own overlayfs mount with:
//! - Lower layer: FUSE mount (presents store paths at its root)
//! - Upper layer: `{upper}/nix/store/` on local SSD (separate FS from FUSE)
//! - Work directory: required by overlayfs (same filesystem as upper)
//! - Merged: bind-mounted to `/nix/store` in the per-build daemon's mount
//!   namespace (see executor.rs `pre_exec`). Outputs written by nix-daemon
//!   land in `{upper}/nix/store/{hash}-{name}`; upload.rs scans that path.
//!
//! The overlay is cleaned up (unmounted + directories removed) on drop.
//! Worker must NOT drop `CAP_SYS_ADMIN` between overlay setup and Nix
//! invocation, as both operations require it.

use std::fs;
use std::path::{Path, PathBuf};

use nix::mount::{MntFlags, MsFlags};

/// An active overlayfs mount with FUSE lower layer and per-build upper layer.
///
/// The overlay is cleaned up (unmounted + directories removed) on drop.
///
/// Directory layout under `{base}/{build_id}/`:
///   - `upper/nix/store/`   — overlayfs upperdir (build outputs land here)
///   - `upper/nix/var/nix/db/` — synthetic SQLite DB (NOT part of overlay)
///   - `upper/etc/nix/`     — nix.conf (NOT part of overlay)
///   - `work/`              — overlayfs workdir
///   - `merged/`            — overlayfs mountpoint, bind-mounted to /nix/store
pub struct OverlayMount {
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
    mounted: bool,
}

impl OverlayMount {
    /// The upper root path under which outputs, synth DB, and nix.conf live.
    ///
    /// Note: the overlayfs `upperdir` is `{upper}/nix/store/`, not `{upper}/`.
    /// Callers use `upper_dir().join("nix/store")` to scan outputs,
    /// `upper_dir().join("nix/var/nix/db")` for the synth DB, and
    /// `upper_dir().join("etc/nix")` for nix.conf. The latter two are
    /// bind-mounted separately (not visible through the overlay).
    pub fn upper_dir(&self) -> &Path {
        &self.upper
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
                    "failed to teardown overlay in Drop (mount leaked)"
                );
                // Centralize the metric here so it fires regardless of exit
                // path (explicit teardown, ?-early-return, panic unwinding).
                // The explicit teardown_overlay() call sets mounted=false on
                // success, so this block only runs on Drop for error/panic paths.
                metrics::counter!("rio_worker_overlay_teardown_failures_total").increment(1);
            }
            self.mounted = false;
        }
    }
}

/// Set up an overlayfs mount for a single build.
///
/// Creates upper, work, and merged directories under `base_dir/{build_id}/`,
/// then mounts overlayfs with `lower` as the lower layer.
///
/// Requires `CAP_SYS_ADMIN`.
///
/// # Stacked lower layers
///
/// The overlay uses TWO lower layers (colon-separated, left-to-right priority):
///   1. `/nix/store` (host) — so nix-daemon + glibc + all its deps stay
///      reachable through the overlay. Without this, exec() after the child's
///      `/nix/store` bind-mount would get ENOENT resolving nix-daemon's path
///      or its dynamic library deps (all living in `/nix/store/...`).
///   2. `lower` (FUSE mount) — lazy-fetch store paths from rio-store.
///
/// Host-store paths take priority on collision (unlikely: rio-store paths
/// have distinct hashes). Build outputs go to upperdir via copy-up.
///
/// # Important
///
/// The upper and work directories MUST be on a different filesystem than the
/// FUSE lower layer. The kernel rejects overlay mounts where upper and lower
/// are on the same filesystem when the lower is a FUSE mount.
pub fn setup_overlay(
    lower: &Path,
    base_dir: &Path,
    build_id: &str,
) -> anyhow::Result<OverlayMount> {
    const HOST_STORE: &str = "/nix/store";
    anyhow::ensure!(
        !build_id.contains('/') && !build_id.contains('\0') && !build_id.is_empty(),
        "build_id must not contain path separators or be empty: {build_id:?}"
    );

    let build_dir = base_dir.join(build_id);
    let upper = build_dir.join("upper");
    // overlayfs upperdir: a dedicated subdirectory so (a) outputs land at
    // `{upper}/nix/store/{hash}-{name}` where upload.rs expects them, and
    // (b) the synth DB (`{upper}/nix/var/nix/db`) and nix.conf (`{upper}/etc/nix`)
    // remain OUTSIDE the overlay (they're bind-mounted separately).
    let store_upper = upper.join("nix/store");
    let work = build_dir.join("work");
    let merged = build_dir.join("merged");

    fs::create_dir_all(&store_upper)?;
    fs::create_dir_all(&work)?;
    fs::create_dir_all(&merged)?;

    // The kernel rejects overlayfs mounts where upper and lower share a
    // filesystem when the lower is FUSE. Compare st_dev proactively so we
    // fail with a clear message rather than a cryptic EINVAL from mount(2).
    let lower_dev = nix::sys::stat::stat(lower)
        .map_err(|e| anyhow::anyhow!("stat({}) failed: {e}", lower.display()))?
        .st_dev;
    let upper_dev = nix::sys::stat::stat(&store_upper)
        .map_err(|e| anyhow::anyhow!("stat({}) failed: {e}", store_upper.display()))?
        .st_dev;
    anyhow::ensure!(
        lower_dev != upper_dev,
        "overlay upper ({}) and FUSE lower ({}) are on the same filesystem \
         (st_dev={lower_dev}); the kernel will reject this mount. \
         Set --overlay-base-dir to a directory on a different mount \
         (e.g., a local SSD emptyDir volume, not the FUSE mount).",
        store_upper.display(),
        lower.display(),
    );

    // Stacked lowers: host /nix/store first (for nix-daemon + deps), FUSE second.
    // Colon-separated, left-to-right lookup priority.
    let mount_data = format!(
        "lowerdir={}:{},upperdir={},workdir={}",
        HOST_STORE,
        lower.display(),
        store_upper.display(),
        work.display()
    );

    tracing::info!(
        build_id,
        lower = %lower.display(),
        upper = %upper.display(),
        merged = %merged.display(),
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
            tracing::warn!(
                path = %path.display(),
                layer = label,
                error = %e,
                "failed to remove overlay directory during cleanup"
            );
        }
    }

    Ok(())
}

/// Explicitly tear down an overlay mount.
///
/// On success, sets `mounted=false` so `Drop` is a no-op.
/// On failure, leaves `mounted=true` so `Drop` will retry teardown and
/// increment `rio_worker_overlay_teardown_failures_total` (centralized there).
pub fn teardown_overlay(mut mount: OverlayMount) -> anyhow::Result<()> {
    teardown_overlay_inner(&mount.merged, &mount.upper, &mount.work)?;
    mount.mounted = false;
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

    #[test]
    fn test_setup_overlay_rejects_bad_build_id() {
        let dir = tempfile::tempdir().unwrap();
        let lower = dir.path();

        assert!(setup_overlay(lower, dir.path(), "").is_err());
        assert!(setup_overlay(lower, dir.path(), "foo/bar").is_err());
        assert!(setup_overlay(lower, dir.path(), "foo\0bar").is_err());
    }

    #[test]
    fn test_setup_overlay_rejects_same_filesystem() {
        // Both lower and base_dir under the same tempdir → same st_dev.
        // The check fires before the mount syscall, so this runs without CAP_SYS_ADMIN.
        let dir = tempfile::tempdir().unwrap();
        let lower = dir.path().join("lower");
        let base = dir.path().join("base");
        std::fs::create_dir_all(&lower).unwrap();

        let Err(e) = setup_overlay(&lower, &base, "test-build") else {
            panic!("expected setup_overlay to fail when upper and lower share a filesystem");
        };
        let msg = e.to_string();
        assert!(msg.contains("same filesystem"), "got: {msg}");
    }

    /// Verify that setup_overlay creates `{upper}/nix/store/` as the overlayfs
    /// upperdir (so build outputs land where upload.rs expects them).
    /// Uses the same-filesystem rejection path to test dir creation without
    /// needing CAP_SYS_ADMIN for the actual mount.
    #[test]
    fn test_setup_overlay_creates_store_upper() {
        let dir = tempfile::tempdir().unwrap();
        let lower = dir.path().join("lower");
        let base = dir.path().join("base");
        std::fs::create_dir_all(&lower).unwrap();

        // Fails at st_dev check (post-mkdir, pre-mount).
        let _ = setup_overlay(&lower, &base, "test-build");

        // Overlayfs upperdir should have been created at upper/nix/store/,
        // NOT just upper/. This is critical for upload.rs which scans
        // `upper_dir().join("nix/store")` for build outputs.
        let store_upper = base.join("test-build/upper/nix/store");
        assert!(
            store_upper.exists(),
            "overlayfs upperdir {store_upper:?} should be created"
        );
        assert!(store_upper.is_dir());
    }
}
