//! Per-build overlayfs management.
//!
//! Each build gets its own overlayfs mount with:
//! - Lower layer: FUSE mount (presents store paths at its root)
//! - Upper layer: `{upper}/nix/store/` on local SSD (separate FS from FUSE)
// r[impl builder.overlay.per-build]
// r[impl builder.overlay.stacked-lower]
// r[impl builder.overlay.upper-not-overlayfs]
//! - Work directory: required by overlayfs (same filesystem as upper)
//! - Merged: bind-mounted to `/nix/store` in the per-build daemon's mount
//!   namespace (see executor.rs `pre_exec`). Outputs written by nix-daemon
//!   land in `{upper}/nix/store/{hash}-{name}`; upload.rs scans that path.
//!
//! The overlay is cleaned up (unmounted + directories removed) on drop.
//! Worker must NOT drop `CAP_SYS_ADMIN` between overlay setup and Nix
//! invocation, as both operations require it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nix::mount::{MntFlags, MsFlags};

/// Errors from overlay setup and teardown.
///
/// Distinct from `anyhow::Error` so `ExecutorError::Overlay(#[from])` only
/// catches overlay failures — a bare `?` on an unrelated `anyhow::Result`
/// elsewhere in `execute_build` no longer silently becomes an "overlay
/// setup failed" message.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("invalid build_id {0:?}: must be non-empty and contain no '/' or NUL")]
    InvalidBuildId(String),

    #[error("failed to create directory {path}: {source}")]
    DirCreate {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("stat({path}) failed: {source}")]
    Stat {
        path: PathBuf,
        #[source]
        source: nix::errno::Errno,
    },

    #[error(
        "overlay upper ({upper}) and FUSE lower ({lower}) are on the same filesystem \
         (st_dev={st_dev}); the kernel will reject this mount. \
         Set --overlay-base-dir to a directory on a different mount \
         (e.g., a local SSD emptyDir volume, not the FUSE mount)."
    )]
    SameFilesystem {
        upper: PathBuf,
        lower: PathBuf,
        st_dev: u64,
    },

    #[error("mount overlay failed: {source} (mount_data: {mount_data})")]
    Mount {
        mount_data: String,
        #[source]
        source: nix::errno::Errno,
    },

    #[error("umount2 overlay failed: {source}")]
    Unmount {
        #[source]
        source: nix::errno::Errno,
    },
}

/// Shorthand: wrap `fs::create_dir_all` with path context.
fn mkdir_all(path: &Path) -> Result<(), OverlayError> {
    fs::create_dir_all(path).map_err(|source| OverlayError::DirCreate {
        path: path.to_path_buf(),
        source,
    })
}

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
    /// Shared worker-lifetime counter. Incremented in `Drop` when teardown
    /// fails (alongside the `rio_builder_overlay_teardown_failures_total`
    /// metric) so `execute_build` can refuse new work after N leaks.
    leak_counter: Arc<AtomicUsize>,
}

impl OverlayMount {
    /// The upper root path under which outputs, synth DB, and nix.conf live.
    ///
    /// Note: the overlayfs `upperdir` is `{upper}/nix/store/`, not `{upper}/`.
    /// See the three `upper_*` accessors below for the subpaths callers need.
    pub fn upper_dir(&self) -> &Path {
        &self.upper
    }

    /// `{upper}/nix/store` — the overlayfs upperdir itself. Build outputs
    /// materialize here. scan_new_outputs + upload_output read this;
    /// executor's FOD-whiteout (mknod) writes here.
    pub fn upper_store(&self) -> PathBuf {
        self.upper.join("nix/store")
    }

    /// `{upper}/nix/var/nix/db` — synth-db bind-mount target. Populated by
    /// synth_db before the daemon starts. NOT visible through the overlay
    /// (separate bind mount in spawn.rs).
    pub fn upper_synth_db(&self) -> PathBuf {
        self.upper.join("nix/var/nix/db")
    }

    /// `{upper}/etc/nix` — nix.conf bind-mount target. setup_nix_conf
    /// populates it from WORKER_NIX_CONF or the rio-nix-conf ConfigMap
    /// override. NOT visible through the overlay.
    pub fn upper_nix_conf(&self) -> PathBuf {
        self.upper.join("etc/nix")
    }

    /// The merged view path where the overlay is mounted.
    pub fn merged_dir(&self) -> &Path {
        &self.merged
    }

    /// Construct an `OverlayMount` with `mounted=true` but no real mount.
    /// Test-only: used to drive the `Drop` path without `CAP_SYS_ADMIN`.
    #[cfg(test)]
    pub(crate) fn new_for_leak_test(leak_counter: Arc<AtomicUsize>) -> Self {
        Self {
            upper: PathBuf::from("/nonexistent-upper"),
            work: PathBuf::from("/nonexistent-work"),
            merged: PathBuf::from("/nonexistent-merged"),
            mounted: true,
            leak_counter,
        }
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
                // Centralize BOTH the metric and the leak counter here so
                // they fire regardless of exit path (explicit teardown,
                // ?-early-return, panic unwinding). teardown_overlay() sets
                // mounted=false on success, so this block only runs when
                // teardown actually failed. execute_build reads the counter
                // at entry to refuse new work after N leaks.
                metrics::counter!("rio_builder_overlay_teardown_failures_total").increment(1);
                self.leak_counter.fetch_add(1, Ordering::Relaxed);
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
    leak_counter: Arc<AtomicUsize>,
) -> Result<OverlayMount, OverlayError> {
    const HOST_STORE: &str = "/nix/store";
    if build_id.is_empty() || build_id.contains('/') || build_id.contains('\0') {
        return Err(OverlayError::InvalidBuildId(build_id.to_string()));
    }

    let build_dir = base_dir.join(build_id);
    let upper = build_dir.join("upper");
    // overlayfs upperdir: a dedicated subdirectory so (a) outputs land at
    // `{upper}/nix/store/{hash}-{name}` where upload.rs expects them, and
    // (b) the synth DB (`{upper}/nix/var/nix/db`) and nix.conf (`{upper}/etc/nix`)
    // remain OUTSIDE the overlay (they're bind-mounted separately).
    let store_upper = upper.join("nix/store");
    let work = build_dir.join("work");
    let merged = build_dir.join("merged");

    mkdir_all(&store_upper)?;
    mkdir_all(&work)?;
    mkdir_all(&merged)?;

    // The kernel rejects overlayfs mounts where upper and lower share a
    // filesystem when the lower is FUSE. Compare st_dev proactively so we
    // fail with a clear message rather than a cryptic EINVAL from mount(2).
    let lower_dev = nix::sys::stat::stat(lower)
        .map_err(|source| OverlayError::Stat {
            path: lower.to_path_buf(),
            source,
        })?
        .st_dev;
    let upper_dev = nix::sys::stat::stat(&store_upper)
        .map_err(|source| OverlayError::Stat {
            path: store_upper.clone(),
            source,
        })?
        .st_dev;
    if lower_dev == upper_dev {
        return Err(OverlayError::SameFilesystem {
            upper: store_upper,
            lower: lower.to_path_buf(),
            st_dev: lower_dev,
        });
    }

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
    .map_err(|source| OverlayError::Mount { mount_data, source })?;

    Ok(OverlayMount {
        upper,
        work,
        merged,
        mounted: true,
        leak_counter,
    })
}

fn teardown_overlay_inner(merged: &Path, upper: &Path, work: &Path) -> Result<(), OverlayError> {
    tracing::info!(merged = %merged.display(), "unmounting overlayfs");

    nix::mount::umount2(merged, MntFlags::MNT_DETACH)
        .map_err(|source| OverlayError::Unmount { source })?;

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

    // Remove the now-empty parent {base}/{build_id}/. Use remove_dir (not
    // remove_dir_all) — if it's non-empty, one of the child removals above
    // failed and we want that surfaced, not masked.
    if let Some(build_dir) = merged.parent()
        && let Err(e) = fs::remove_dir(build_dir)
    {
        tracing::warn!(
            path = %build_dir.display(),
            error = %e,
            "failed to remove overlay build_dir during cleanup"
        );
    }

    Ok(())
}

/// Explicitly tear down an overlay mount.
///
/// On success, sets `mounted=false` so `Drop` is a no-op.
/// On failure, leaves `mounted=true` so `Drop` will retry teardown and
/// increment `rio_builder_overlay_teardown_failures_total` (centralized there).
pub fn teardown_overlay(mut mount: OverlayMount) -> Result<(), OverlayError> {
    teardown_overlay_inner(&mount.merged, &mount.upper, &mount.work)?;
    mount.mounted = false;
    Ok(())
}

/// Create the Nix state directory structure in the overlay upper layer.
///
/// Takes `synth_db` directly (caller passes [`OverlayMount::upper_synth_db`])
/// — symmetric with how `setup_nix_conf` takes `upper_nix_conf()` instead of
/// re-deriving the path. Centralizes the `nix/var/nix/db` path construction
/// in one place (the `upper_synth_db` accessor).
pub fn prepare_nix_state_dirs(synth_db: &Path) -> Result<PathBuf, OverlayError> {
    mkdir_all(synth_db)?;
    Ok(synth_db.to_path_buf())
}

// r[verify builder.overlay.per-build]
// r[verify builder.overlay.upper-not-overlayfs]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_nix_state_dirs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let synth_db = dir.path().join("nix/var/nix/db");
        let db_dir = prepare_nix_state_dirs(&synth_db)?;

        assert!(db_dir.exists());
        assert!(db_dir.is_dir());
        assert_eq!(db_dir, synth_db);
        Ok(())
    }

    /// Verify the leak counter increments exactly when teardown fails in Drop.
    /// We construct with `mounted=true` and paths that aren't real mounts;
    /// `umount2` will fail → Drop's error branch fires → counter increments.
    #[test]
    fn test_leak_counter_increments_on_drop_failure() {
        let counter = Arc::new(AtomicUsize::new(0));

        // Scope so Drop runs at the closing brace.
        {
            let _mount = OverlayMount::new_for_leak_test(Arc::clone(&counter));
            assert_eq!(
                counter.load(Ordering::Relaxed),
                0,
                "counter should not increment until Drop"
            );
        }

        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "counter should increment exactly once when Drop's teardown fails"
        );
    }

    /// Verify the counter does NOT increment when `mounted=false` (i.e., after
    /// a successful explicit teardown_overlay() or if setup never completed).
    #[test]
    fn test_leak_counter_no_increment_when_unmounted() {
        let counter = Arc::new(AtomicUsize::new(0));

        {
            let mut mount = OverlayMount::new_for_leak_test(Arc::clone(&counter));
            // Simulate successful explicit teardown.
            mount.mounted = false;
        }

        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "counter should not increment when mounted=false at Drop"
        );
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

    fn dummy_counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    #[test]
    fn test_setup_overlay_rejects_bad_build_id() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let lower = dir.path();

        for bad in ["", "foo/bar", "foo\0bar"] {
            let Err(err) = setup_overlay(lower, dir.path(), bad, dummy_counter()) else {
                panic!("expected InvalidBuildId for {bad:?}");
            };
            assert!(
                matches!(err, OverlayError::InvalidBuildId(_)),
                "expected InvalidBuildId for {bad:?}, got {err:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_setup_overlay_rejects_same_filesystem() -> anyhow::Result<()> {
        // Both lower and base_dir under the same tempdir → same st_dev.
        // The check fires before the mount syscall, so this runs without CAP_SYS_ADMIN.
        let dir = tempfile::tempdir()?;
        let lower = dir.path().join("lower");
        let base = dir.path().join("base");
        std::fs::create_dir_all(&lower)?;

        let Err(e) = setup_overlay(&lower, &base, "test-build", dummy_counter()) else {
            panic!("expected setup_overlay to fail when upper and lower share a filesystem");
        };
        assert!(
            matches!(e, OverlayError::SameFilesystem { .. }),
            "expected SameFilesystem, got {e:?}"
        );
        // The Display message must mention it for log readability.
        let msg = e.to_string();
        assert!(msg.contains("same filesystem"), "got: {msg}");
        Ok(())
    }

    /// Verify that setup_overlay creates `{upper}/nix/store/` as the overlayfs
    /// upperdir (so build outputs land where upload.rs expects them).
    /// Uses the same-filesystem rejection path to test dir creation without
    /// needing CAP_SYS_ADMIN for the actual mount.
    #[test]
    fn test_setup_overlay_creates_store_upper() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let lower = dir.path().join("lower");
        let base = dir.path().join("base");
        std::fs::create_dir_all(&lower)?;

        // Fails at st_dev check (post-mkdir, pre-mount).
        let _ = setup_overlay(&lower, &base, "test-build", dummy_counter());

        // Overlayfs upperdir should have been created at upper/nix/store/,
        // NOT just upper/. This is critical for upload.rs which scans
        // `upper_store()` for build outputs.
        let store_upper = base.join("test-build/upper/nix/store");
        assert!(
            store_upper.exists(),
            "overlayfs upperdir {store_upper:?} should be created"
        );
        assert!(store_upper.is_dir());
        Ok(())
    }
}
