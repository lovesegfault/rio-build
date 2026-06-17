//! Shared types and assertion helpers for the check modules: the [`Ctx`]
//! every check receives, the [`Check`]/[`Outcome`] registry types, the
//! errno assertion helpers, and the [`PrivDrop`] privilege guard.
//!
//! Check logic lives in the sibling modules; this module holds only what
//! they share.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use nix::errno::Errno;
use nix::libc;
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

/// C path for raw libc syscalls. Fixture paths never carry an interior
/// NUL, so this is infallible in practice.
pub fn cpath(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("path has an interior NUL")
}

/// Raw `open(2)` with explicit flags (the probes need flag combinations
/// `std::fs` cannot express — O_DIRECT, O_PATH, O_TMPFILE, …); O_CLOEXEC
/// is always added. The errno is preserved in the `io::Error`.
pub fn open_raw(path: &Path, flags: libc::c_int) -> io::Result<OwnedFd> {
    let c = cpath(path);
    // SAFETY: valid C path; the fd is checked before being owned.
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

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

/// Recursive node count of a `find`-style walk (no symlink
/// following): the directory itself plus every descendant, compared
/// against `Manifest::expected_node_count`.
pub fn count_nodes(dir: &Path, count: &mut u64) -> anyhow::Result<()> {
    *count += 1;
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            count_nodes(&entry.path(), count)?;
        } else {
            *count += 1;
        }
    }
    Ok(())
}

/// A `libc` directory stream with RAII `closedir`, exposing what
/// `std::fs::read_dir` hides and the checks need: d_ino for every
/// entry INCLUDING the `.`/`..` dots (the dot-identity checks exist
/// for exactly those), and the glibc cookie primitives
/// (`telldir`/`seekdir`/`rewinddir`) the kernel readdir-resume path
/// depends on.
pub struct RawDir {
    dirp: *mut libc::DIR,
    label: String,
}

impl RawDir {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let c = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("path {} has an interior NUL", path.display()))?;
        // SAFETY: `c` is a valid NUL-terminated path for the duration
        // of the call.
        let dirp = unsafe { libc::opendir(c.as_ptr()) };
        ensure!(
            !dirp.is_null(),
            "opendir({}) failed: {}",
            path.display(),
            io::Error::last_os_error()
        );
        Ok(RawDir {
            dirp,
            label: path.display().to_string(),
        })
    }

    /// A stream over a dup of `fd` (the fd-relative walk keeps using
    /// its own fd for openat afterwards).
    pub fn from_fd(fd: BorrowedFd<'_>) -> anyhow::Result<Self> {
        let duped = nix::unistd::dup(fd).context("dup dirfd")?;
        let raw = duped.into_raw_fd();
        // SAFETY: `raw` is a freshly dup'd, owned fd; on success the
        // stream owns it (closedir closes it), on failure we reclaim
        // and drop it.
        let dirp = unsafe { libc::fdopendir(raw) };
        if dirp.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so `raw` is still ours to close.
            drop(unsafe { OwnedFd::from_raw_fd(raw) });
            bail!("fdopendir failed: {err}");
        }
        // A dup shares the file offset with the original; rewind so the
        // enumeration is complete even if the fd was read before.
        // SAFETY: live stream.
        unsafe { libc::rewinddir(dirp) };
        Ok(RawDir {
            dirp,
            label: format!("<dirfd {}>", fd.as_raw_fd()),
        })
    }

    /// One raw entry as (name, d_ino), dots included; `Ok(None)` at
    /// clean end-of-stream. A NULL return with a non-zero errno is a
    /// readdir error (e.g. EIO) and fails the check — distinguishing
    /// the two is the whole point of the garbage-offset probe.
    pub fn next_entry(&self) -> anyhow::Result<Option<(Vec<u8>, u64)>> {
        Errno::clear();
        // SAFETY: `self.dirp` is a live stream; the returned dirent is
        // owned by the stream and only read before the next call.
        let ent = unsafe { libc::readdir(self.dirp) };
        if ent.is_null() {
            let raw = Errno::last_raw();
            ensure!(
                raw == 0,
                "readdir({}) failed: {:?}",
                self.label,
                Errno::from_raw(raw)
            );
            return Ok(None);
        }
        // SAFETY: `ent` is a valid dirent from readdir.
        let (name, ino) = unsafe {
            (
                std::ffi::CStr::from_ptr((*ent).d_name.as_ptr())
                    .to_bytes()
                    .to_vec(),
                (*ent).d_ino,
            )
        };
        Ok(Some((name, ino)))
    }

    /// Every entry as (name, d_ino), dots included.
    pub fn entries(&self) -> anyhow::Result<Vec<(Vec<u8>, u64)>> {
        let mut out = Vec::new();
        while let Some(entry) = self.next_entry()? {
            out.push(entry);
        }
        Ok(out)
    }

    /// Every real entry name (dots filtered), order-independent.
    pub fn names(&self) -> anyhow::Result<BTreeSet<Vec<u8>>> {
        let mut out = BTreeSet::new();
        while let Some((name, _)) = self.next_entry()? {
            if name != b"." && name != b".." {
                out.insert(name);
            }
        }
        Ok(out)
    }

    pub fn tell(&self) -> libc::c_long {
        // SAFETY: live stream.
        unsafe { libc::telldir(self.dirp) }
    }

    pub fn seek(&self, loc: libc::c_long) {
        // SAFETY: live stream; any loc is accepted by the API, the FUSE
        // resume path is exactly what this exercises.
        unsafe { libc::seekdir(self.dirp, loc) }
    }

    pub fn rewind(&self) {
        // SAFETY: live stream.
        unsafe { libc::rewinddir(self.dirp) }
    }
}

impl Drop for RawDir {
    fn drop(&mut self) {
        // SAFETY: `dirp` came from opendir/fdopendir and is closed
        // exactly once.
        unsafe { libc::closedir(self.dirp) };
    }
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
