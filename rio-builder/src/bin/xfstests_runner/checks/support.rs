//! Shared types and assertion helpers for the check modules: the [`Ctx`]
//! every check receives, the [`Check`]/[`Outcome`] registry types, the
//! errno assertion helpers, and the [`PrivDrop`] privilege guard.
//!
//! Check logic lives in the sibling modules; this module holds only what
//! they share.

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
