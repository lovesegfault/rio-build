//! The check registry and shared helpers.
//!
//! Every check is a Rust function asserting kernel-visible behavior of
//! the live castore-FUSE mount through direct syscalls — deterministic
//! errnos, no shell, no commands that can prompt. Check names carry
//! their xfstests origin (`generic_NNN_*`); the doc comment on each
//! function states what failure mode of this FUSE it guards.
//!
//! Checks run in registration order and the order is load-bearing for
//! the read-path checks: `generic_075_091_read_integrity` must perform
//! the FIRST read of the big blob (the cold/streaming path) before the
//! concurrency and write-through checks touch the same file warm.
//! `--filter` exists for debugging a single check; a filtered run
//! cannot distinguish cold from warm reads.

pub mod dir_locks;
pub mod errno_battery;
pub mod io_paths;
pub mod meta;
pub mod read;
pub mod write_attack;
pub mod xattr_statx;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use nix::errno::Errno;
use nix::unistd::{Gid, Uid, setegid, seteuid};

use crate::manifest::{FileSpec, Manifest};

/// Everything a check needs: the mount under test, the oracle, and the
/// probe identities.
pub struct Ctx {
    /// The live castore-FUSE mountpoint (`/var/rio/castore/<build-id>`).
    pub mount: PathBuf,
    /// `<mount>/<dep store-path basename>` — the fixture tree root.
    pub dep_root: PathBuf,
    /// The expected-content oracle.
    pub manifest: Manifest,
    /// rio-mountd's shared backing cache; enables the warm-read wait
    /// and the write-through probe's repair step.
    pub cache_dir: Option<PathBuf>,
    /// Output of the overlay-leg consumer build (3 lines), if the
    /// harness dispatched one.
    pub consumer_output: Option<PathBuf>,
    /// Unprivileged uid/gid for the permission-enforcement probes (the
    /// uid class a real build's processes map to on the host side).
    pub probe_uid: u32,
    pub probe_gid: u32,
    /// A SECOND unprivileged uid/gid, distinct from `probe_uid`, for the
    /// generic/088 DAC check — proves `default_permissions` enforcement
    /// is not special-cased to the mount-owner uid.
    pub second_uid: u32,
    pub second_gid: u32,
}

impl Ctx {
    /// Absolute path of a manifest-relative path on the mount.
    pub fn on_mount(&self, rel: &str) -> PathBuf {
        self.dep_root.join(rel)
    }
}

/// A check's non-failure result.
pub enum Outcome {
    Pass,
    /// The check could not run (missing optional input); reason given.
    Skip(&'static str),
}

/// One registered check.
pub struct Check {
    pub name: &'static str,
    /// The xfstests test(s) this re-expresses.
    pub origin: &'static str,
    pub run: fn(&Ctx) -> anyhow::Result<Outcome>,
}

/// All checks, in execution order (cold-read sequencing, then
/// privilege-dropping batteries, then the root-probe batteries, with
/// the state-restoring write-through and dirty-pipe page-cache probes
/// last — they need the big blob warm in the passthrough page cache).
pub fn registry() -> Vec<Check> {
    vec![
        Check {
            name: "mount_castore_fstype",
            origin: "harness (xfstests _require_scratch)",
            run: meta::mount_castore_fstype,
        },
        Check {
            name: "overlay_readdir_consumer",
            origin: "xfstests generic/257+453 (overlay-lowerdir leg)",
            run: meta::overlay_readdir_consumer,
        },
        Check {
            name: "generic_257_readdir_multibatch",
            origin: "xfstests generic/257",
            run: meta::generic_257_readdir_multibatch,
        },
        Check {
            name: "inode_identity_content_addressed",
            origin: "castore-specific (content-addressed identity)",
            run: meta::inode_identity_content_addressed,
        },
        Check {
            name: "generic_401_file_kinds",
            origin: "xfstests generic/401",
            run: meta::generic_401_file_kinds,
        },
        Check {
            name: "generic_002_nlink_walk",
            origin: "xfstests generic/002 (inverted: castore has no hardlinks, asserts nlink==1 — F-B)",
            run: meta::generic_002_nlink_walk,
        },
        Check {
            name: "generic_005_symlink_errnos",
            origin: "xfstests generic/005",
            run: meta::generic_005_symlink_errnos,
        },
        Check {
            name: "generic_360_symlink_targets",
            origin: "xfstests generic/360",
            run: meta::generic_360_symlink_targets,
        },
        Check {
            name: "generic_453_byte_exact_names",
            origin: "xfstests generic/453",
            run: meta::generic_453_byte_exact_names,
        },
        Check {
            name: "generic_075_091_read_integrity",
            origin: "xfstests generic/075 + generic/091",
            run: read::generic_075_091_read_integrity,
        },
        Check {
            name: "generic_095_113_310_concurrency",
            origin: "xfstests generic/095 + generic/310 + generic/113 (sync open/close legs only; no AIO)",
            run: read::generic_095_113_310_concurrency,
        },
        // mmap/splice/copy_file_range run after the big blob is warm
        // (promoted to passthrough by the read-integrity check) so they
        // exercise the passthrough backing fd, the production read path.
        Check {
            name: "generic_074_127_mmap_reads",
            origin: "xfstests generic/074 + generic/127",
            run: io_paths::generic_074_127_mmap_reads,
        },
        Check {
            name: "generic_249_splice_read",
            origin: "xfstests generic/249",
            run: io_paths::generic_249_splice_read,
        },
        Check {
            name: "generic_430_553_copy_file_range",
            origin: "xfstests generic/430 + generic/553",
            run: io_paths::generic_430_553_copy_file_range,
        },
        Check {
            name: "generic_285_448_706_seek_hole_data",
            origin: "xfstests generic/285 + generic/448 + generic/706",
            run: io_paths::generic_285_448_706_seek_hole_data,
        },
        Check {
            name: "generic_020_062_097_xattr_read_legs",
            origin: "xfstests generic/020 + generic/062 + generic/097 (xattr read legs)",
            run: xattr_statx::generic_020_062_097_xattr_read_legs,
        },
        Check {
            name: "generic_423_statx_field_correctness",
            origin: "xfstests generic/423",
            run: xattr_statx::generic_423_statx_field_correctness,
        },
        Check {
            name: "generic_532_statx_attributes_mask_sanity",
            origin: "xfstests generic/532",
            run: xattr_statx::generic_532_statx_attributes_mask_sanity,
        },
        Check {
            name: "generic_471_rewinddir",
            origin: "xfstests generic/471",
            run: dir_locks::generic_471_rewinddir,
        },
        Check {
            name: "generic_676_seekdir",
            origin: "xfstests generic/676",
            run: dir_locks::generic_676_seekdir,
        },
        Check {
            name: "generic_088_second_uid_dac",
            origin: "xfstests generic/088",
            run: dir_locks::generic_088_second_uid_dac,
        },
        Check {
            name: "generic_131_read_locks",
            origin: "xfstests generic/131",
            run: dir_locks::generic_131_read_locks,
        },
        Check {
            name: "generic_126_exec_access",
            origin: "xfstests generic/126",
            run: errno_battery::generic_126_exec_access,
        },
        Check {
            name: "generic_050_write_protection_unprivileged",
            origin: "xfstests generic/050 + generic/123 (adapted: unprivileged overwrite/append/delete/move all denied)",
            run: errno_battery::generic_050_write_protection_unprivileged,
        },
        Check {
            name: "generic_294_eexist_unprivileged",
            origin: "xfstests generic/294",
            run: errno_battery::generic_294_eexist_unprivileged,
        },
        Check {
            name: "generic_294_erofs_battery_root",
            origin: "xfstests generic/294 + generic/050 (root leg)",
            run: errno_battery::generic_294_erofs_battery_root,
        },
        Check {
            name: "generic_007_enoent_never_eio",
            origin: "xfstests generic/007",
            run: errno_battery::generic_007_enoent_never_eio,
        },
        Check {
            name: "statfs_zero_totals",
            origin: "castore-specific (statfs F-A pin; no upstream statfs-totals analogue)",
            run: errno_battery::statfs_zero_totals,
        },
        Check {
            name: "generic_680_dirty_pipe",
            origin: "xfstests generic/680 (CVE-2022-0847, Dirty Pipe)",
            run: write_attack::generic_680_dirty_pipe,
        },
        Check {
            name: "write_through_passthrough_root",
            origin: "xfstests generic/050 (root write leg)",
            run: errno_battery::write_through_passthrough_root,
        },
    ]
}

/// Find the dep root under the mount: the single directory entry whose
/// name ends with the manifest's `root_suffix`.
pub fn resolve_dep_root(mount: &Path, manifest: &Manifest) -> anyhow::Result<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(mount)
        .with_context(|| format!("read_dir({})", mount.display()))?
        .collect::<Result<_, _>>()
        .context("enumerate mount root")?;
    entries
        .iter()
        .map(|e| e.file_name())
        .find(|name| {
            use std::os::unix::ffi::OsStrExt;
            name.as_bytes().ends_with(manifest.root_suffix.as_bytes())
        })
        .map(|name| mount.join(name))
        .with_context(|| {
            format!(
                "no entry matching *{} under {} (found: {:?})",
                manifest.root_suffix,
                mount.display(),
                entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
            )
        })
}

// ─── Shared assertion helpers ──────────────────────────────────────────

/// The errno of an `io::Error`, or `UnknownErrno` for non-OS errors.
pub fn errno_of(e: &std::io::Error) -> Errno {
    Errno::from_raw(e.raw_os_error().unwrap_or(0))
}

/// First byte offset at which `actual` differs from `expected` (or the
/// length difference if one is a prefix of the other).
pub fn first_divergence(expected: &[u8], actual: &[u8]) -> Option<usize> {
    let prefix_mismatch = expected.iter().zip(actual.iter()).position(|(a, b)| a != b);
    let len_mismatch = (expected.len() != actual.len()).then_some(expected.len().min(actual.len()));
    prefix_mismatch.or(len_mismatch)
}

/// A non-empty, non-executable input file (served 0444): the default
/// probe target for read, lock, and xattr checks.
pub fn readable_plain_file(ctx: &Ctx) -> anyhow::Result<&FileSpec> {
    ctx.manifest
        .files
        .iter()
        .find(|f| !f.executable && !f.content.is_empty())
        .context("manifest has no non-empty plain file")
}

/// Assert `res` failed with one of `accepted`'s errnos; return which.
pub fn expect_errno<T>(
    what: &str,
    res: std::io::Result<T>,
    accepted: &[Errno],
) -> anyhow::Result<Errno> {
    match res {
        Ok(_) => anyhow::bail!("{what}: unexpectedly succeeded (expected {accepted:?})"),
        Err(e) => {
            let actual = errno_of(&e);
            anyhow::ensure!(
                accepted.contains(&actual),
                "{what}: got {actual:?}, expected one of {accepted:?}"
            );
            Ok(actual)
        }
    }
}

/// Poll `cond` until it is true or `timeout` elapses.
pub fn wait_for(
    what: &str,
    timeout: Duration,
    mut cond: impl FnMut() -> bool,
) -> anyhow::Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("timed out after {timeout:?} waiting for {what}")
}

/// Temporarily drop the effective uid/gid to the probe identity. The
/// kernel evaluates `default_permissions` against effective ids, so
/// this is what makes the unprivileged batteries see EACCES instead of
/// CAP_DAC_OVERRIDE bypass. Restores root on drop (panic-safe); the
/// runner is single-threaded while a guard is alive.
pub struct PrivDrop;

impl PrivDrop {
    pub fn to(uid: u32, gid: u32) -> anyhow::Result<Self> {
        // gid first: once euid is unprivileged, setegid needs CAP_SETGID
        // we no longer have.
        setegid(Gid::from_raw(gid)).context("setegid(probe gid)")?;
        if let Err(e) = seteuid(Uid::from_raw(uid)) {
            let _ = setegid(Gid::from_raw(0));
            return Err(e).context("seteuid(probe uid)");
        }
        Ok(PrivDrop)
    }
}

impl Drop for PrivDrop {
    fn drop(&mut self) {
        // Restore order is the reverse: regain euid 0 first, which also
        // re-raises the capabilities setegid needs.
        seteuid(Uid::from_raw(0)).expect("restore euid 0");
        setegid(Gid::from_raw(0)).expect("restore egid 0");
    }
}
