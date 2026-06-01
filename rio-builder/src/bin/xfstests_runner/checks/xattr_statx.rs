//! Extended-attribute read legs and statx field correctness.
//!
//! castore-FUSE is read-only and content-addressed: it stores no user
//! xattrs and exposes the canonical store-path metadata. These checks
//! pin the two read-side contracts overlayfs depends on when it stacks
//! a build's per-pod upperdir over a castore lowerdir.
//!
//! * **xattr (generic/020 + generic/062 + generic/097):** the upstream
//!   tests round-trip getfattr/setfattr. castore cannot set, so the
//!   ported assertion is the read leg overlay actually exercises:
//!   `getxattr` of a missing attribute returns ENODATA (never EIO,
//!   ENOSYS, or EOPNOTSUPP) and `listxattr` returns an empty list
//!   cleanly. `ovl_copy_xattr` probes `trusted.overlay.*`/`user.*` on
//!   every lower inode during copy-up; a mount-wide ENOSYS or a stray
//!   EIO there aborts the copy-up and fails the build.
//!
//! * **statx (generic/423 + generic/532):** statx() must report fields
//!   that agree with stat()/lstat() across every node kind, with the
//!   requested STATX_BASIC_STATS bits all present in the result mask,
//!   and (generic/532's regression) the reported `stx_attributes` must
//!   be a subset of `stx_attributes_mask` — a flag claimed outside the
//!   mask is the XFS bug 532 was written for.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use nix::errno::Errno;
use nix::libc;

use super::{Ctx, Outcome, readable_plain_file};

/// One representative path of each node kind, drawn from the manifest:
/// a plain regular file, a directory, and a symlink.
fn sample_nodes(
    ctx: &Ctx,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
    let file = ctx.on_mount(&readable_plain_file(ctx)?.path);
    let dir = ctx
        .manifest
        .dirs
        .first()
        .map(|d| ctx.on_mount(d))
        .context("manifest has no directory")?;
    let symlink = ctx
        .manifest
        .symlinks
        .first()
        .map(|s| ctx.on_mount(&s.path))
        .context("manifest has no symlink")?;
    Ok((file, dir, symlink))
}

/// NUL-terminated copy of a mount path for the raw xattr syscalls.
fn cpath(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("mount path has no NUL")
}

/// NUL-terminated attribute name.
fn cname(name: &str) -> CString {
    CString::new(name).expect("attr name has no NUL")
}

/// `lgetxattr(path, name)` with a zero-length buffer: probes whether the
/// attribute exists without reading it. Returns `Ok(len)` if present,
/// `Err(errno)` otherwise — exactly the syscall overlay's copy-up runs.
fn lgetxattr_probe(path: &Path, name: &str) -> Result<isize, Errno> {
    let (cpath, cname) = (cpath(path), cname(name));
    // SAFETY: valid C strings, null data pointer with size 0 is the
    // documented "query size" form, errno read immediately on -1.
    let rc = unsafe { libc::lgetxattr(cpath.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0) };
    if rc < 0 { Err(Errno::last()) } else { Ok(rc) }
}

/// `llistxattr(path, buf)` with `buf.len()` bytes of room. With a
/// non-empty buffer the kernel takes the data path of the FUSE
/// `listxattr` handler (not the size probe), so this exercises the
/// `XattrListReply::EmptyData` branch — the one whose EIO trap (a size
/// struct returned where data was asked for) the rule below guards.
/// Returns the number of name bytes written (0 for no xattrs).
fn llistxattr_buffered(path: &Path, cap: usize) -> Result<isize, Errno> {
    let cpath = cpath(path);
    let mut buf = vec![0u8; cap];
    // SAFETY: valid C string; buf is valid for cap bytes; errno read
    // immediately on -1.
    let rc = unsafe { libc::llistxattr(cpath.as_ptr(), buf.as_mut_ptr().cast(), cap) };
    if rc < 0 { Err(Errno::last()) } else { Ok(rc) }
}

/// `llistxattr(path)` size probe (null buffer, size 0). Returns the byte
/// length of the packed name list (0 for no xattrs) or the errno.
fn llistxattr_probe(path: &Path) -> Result<isize, Errno> {
    let cpath = cpath(path);
    // SAFETY: valid C string, null buffer with size 0 is the size-probe
    // form, errno read immediately on -1.
    let rc = unsafe { libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) };
    if rc < 0 { Err(Errno::last()) } else { Ok(rc) }
}

/// `lsetxattr(path, name, value)`: the set leg of generic/097. On a
/// read-only castore mount this reaches the write-path deny table.
fn lsetxattr(path: &Path, name: &str, value: &[u8]) -> Result<(), Errno> {
    let (cpath, cname) = (cpath(path), cname(name));
    // SAFETY: valid C strings; value is valid for value.len(); errno
    // read immediately on -1.
    let rc = unsafe {
        libc::lsetxattr(
            cpath.as_ptr(),
            cname.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc < 0 { Err(Errno::last()) } else { Ok(()) }
}

/// `lremovexattr(path, name)`: the remove leg of generic/097.
fn lremovexattr(path: &Path, name: &str) -> Result<(), Errno> {
    let (cpath, cname) = (cpath(path), cname(name));
    // SAFETY: valid C strings; errno read immediately on -1.
    let rc = unsafe { libc::lremovexattr(cpath.as_ptr(), cname.as_ptr()) };
    if rc < 0 { Err(Errno::last()) } else { Ok(()) }
}

/// generic/020 + generic/062 + generic/097 (read legs): `getxattr` of
/// any name must fail with ENODATA — not EOPNOTSUPP (marks the whole
/// mount xattr-less to overlayfs), not ENOSYS (not a legal getxattr
/// errno), never EIO — and `listxattr` must return an empty list
/// cleanly, on every node kind (see the module doc for why copy-up
/// depends on these exact errnos). The buffered `listxattr` leg pins
/// the data-path branch of the FUSE handler (an empty DATA reply, never
/// the size struct — the documented EIO trap); the set/remove legs
/// confirm writes are denied at the read-only boundary.
// r[verify builder.fuse.listxattr-empty]
pub fn generic_020_062_097_xattr_read_legs(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let (file, dir, symlink) = sample_nodes(ctx)?;
    // The names overlay copy-up and common tooling actually probe, one
    // per namespace the kernel will route to the filesystem as root.
    let probe_names = ["user.test", "trusted.overlay.opaque", "user.overlay.impure"];

    for (kind, path) in [("file", &file), ("dir", &dir), ("symlink", &symlink)] {
        for name in probe_names {
            match lgetxattr_probe(path, name) {
                Ok(len) => bail!(
                    "getxattr({name}) on the {kind} {} unexpectedly succeeded (len {len}); \
                     castore stores no xattrs",
                    path.display()
                ),
                Err(errno) => ensure!(
                    errno == Errno::ENODATA,
                    "getxattr({name}) on the {kind} {} returned {errno:?}, expected ENODATA \
                     (EOPNOTSUPP/ENOSYS/EIO here would abort overlay copy-up)",
                    path.display()
                ),
            }
        }

        // Size probe (null buffer): the kernel asks "how many bytes?".
        let listed = llistxattr_probe(path).map_err(|e| {
            anyhow::anyhow!(
                "listxattr size probe on the {kind} {} failed with {e:?}, expected an empty list",
                path.display()
            )
        })?;
        ensure!(
            listed == 0,
            "listxattr size probe on the {kind} {} reported {listed} bytes of names, expected 0",
            path.display()
        );
        // Buffered read (non-empty buffer): the data-path branch. Must
        // return 0 bytes; a size struct delivered here is the EIO trap.
        let buffered = llistxattr_buffered(path, 4096).map_err(|e| {
            anyhow::anyhow!(
                "buffered listxattr on the {kind} {} failed with {e:?}, expected an empty list",
                path.display()
            )
        })?;
        ensure!(
            buffered == 0,
            "buffered listxattr on the {kind} {} returned {buffered} bytes, expected 0",
            path.display()
        );

        // Set/remove (generic/097 write legs): denied at the read-only
        // boundary. For files and dirs the call reaches the FUSE
        // write-path deny table, which answers EROFS. For symlinks the
        // VFS rejects first: xattr_permission forbids `user.*` xattrs on
        // anything but regular files and directories, returning EPERM
        // for writes (and ENODATA for reads) before any FS or mount
        // write check runs. removexattr of a never-present name may also
        // legitimately surface ENODATA.
        let denied: &[Errno] = if kind == "symlink" {
            &[Errno::EPERM]
        } else {
            &[Errno::EROFS]
        };
        match lsetxattr(path, "user.rio-probe", b"x") {
            Ok(()) => bail!(
                "setxattr on the {kind} {} unexpectedly succeeded on a read-only mount",
                path.display()
            ),
            Err(e) => ensure!(
                denied.contains(&e),
                "setxattr on the {kind} {} returned {e:?}, expected one of {denied:?}",
                path.display()
            ),
        }
        match lremovexattr(path, "user.rio-probe") {
            Ok(()) => bail!(
                "removexattr on the {kind} {} unexpectedly succeeded on a read-only mount",
                path.display()
            ),
            Err(e) => ensure!(
                denied.contains(&e) || e == Errno::ENODATA,
                "removexattr on the {kind} {} returned {e:?}, expected one of {denied:?} or ENODATA",
                path.display()
            ),
        }
    }
    Ok(Outcome::Pass)
}

/// A `statx` wrapper requesting STATX_BASIC_STATS, syncing as stat does.
fn statx_basic(path: &Path, follow: bool) -> anyhow::Result<libc::statx> {
    let cpath = CString::new(path.as_os_str().as_bytes()).context("mount path has no NUL")?;
    // SAFETY: zeroed statx is a valid initial buffer; the struct is
    // filled by the kernel on success.
    let mut buf: libc::statx = unsafe { std::mem::zeroed() };
    let mut flags = libc::AT_STATX_SYNC_AS_STAT;
    if !follow {
        flags |= libc::AT_SYMLINK_NOFOLLOW;
    }
    // SAFETY: valid C string and out-pointer; rc checked before reading.
    let rc = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            cpath.as_ptr(),
            flags,
            libc::STATX_BASIC_STATS,
            &mut buf,
        )
    };
    ensure!(
        rc == 0,
        "statx({}) failed: {:?}",
        path.display(),
        Errno::last()
    );
    Ok(buf)
}

/// generic/423: statx() must agree with lstat() on every node kind.
/// The kernel fills statx from the same FUSE getattr reply stat() uses,
/// so a divergence means the FUSE answered inconsistently for the two
/// paths (or the kernel could not populate a basic field). Builds that
/// statx their inputs — coreutils, rustc's file cache, ninja — must see
/// the canonical store metadata: root-owned, nlink 1, epoch+1s times,
/// 512-byte block accounting.
// r[verify builder.fuse.canonical-metadata+2]
pub fn generic_423_statx_field_correctness(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let (file, dir, symlink) = sample_nodes(ctx)?;

    for (kind, path) in [("file", &file), ("dir", &dir), ("symlink", &symlink)] {
        let sx = statx_basic(path, false)?;
        let st = fs::symlink_metadata(path).with_context(|| format!("lstat {}", path.display()))?;

        // Every STATX_BASIC_STATS field we keyed on must actually be in
        // the returned mask — a cleared bit means "not filled", and then
        // the field below is meaningless.
        for (bit, label) in [
            (libc::STATX_MODE, "mode"),
            (libc::STATX_SIZE, "size"),
            (libc::STATX_INO, "ino"),
            (libc::STATX_NLINK, "nlink"),
            (libc::STATX_BLOCKS, "blocks"),
            (libc::STATX_MTIME, "mtime"),
        ] {
            ensure!(
                sx.stx_mask & bit != 0,
                "statx of the {kind} {} did not fill {label} (mask {:#x})",
                path.display(),
                sx.stx_mask
            );
        }

        ensure!(
            u32::from(sx.stx_mode) == st.mode(),
            "statx/lstat mode disagree for the {kind} {}: {:o} vs {:o}",
            path.display(),
            sx.stx_mode,
            st.mode()
        );
        ensure!(
            sx.stx_size == st.size(),
            "statx/lstat size disagree for the {kind} {}: {} vs {}",
            path.display(),
            sx.stx_size,
            st.size()
        );
        ensure!(
            sx.stx_ino == st.ino(),
            "statx/lstat ino disagree for the {kind} {}: {} vs {}",
            path.display(),
            sx.stx_ino,
            st.ino()
        );
        ensure!(
            u64::from(sx.stx_nlink) == st.nlink(),
            "statx/lstat nlink disagree for the {kind} {}: {} vs {}",
            path.display(),
            sx.stx_nlink,
            st.nlink()
        );
        ensure!(
            u64::from(sx.stx_nlink) == 1,
            "statx nlink of the {kind} {} is {}, expected the castore choice of 1",
            path.display(),
            sx.stx_nlink
        );
        ensure!(
            sx.stx_uid == 0 && sx.stx_gid == 0,
            "statx of the {kind} {} not root-owned ({}:{})",
            path.display(),
            sx.stx_uid,
            sx.stx_gid
        );
        // st_blocks is in 512-byte units, the same accounting make_attr
        // derives (size.div_ceil(512)); statx must match lstat.
        ensure!(
            sx.stx_blocks == st.blocks(),
            "statx/lstat st_blocks disagree for the {kind} {}: {} vs {}",
            path.display(),
            sx.stx_blocks,
            st.blocks()
        );
        ensure!(
            sx.stx_blksize > 0,
            "statx of the {kind} {} reported a zero blksize",
            path.display()
        );
        // Canonical store timestamp: epoch + 1s, and ctime == mtime
        // (there is no separate change time on an immutable store path).
        ensure!(
            sx.stx_mtime.tv_sec == 1,
            "statx mtime of the {kind} {} is {}s, expected the canonical store mtime of 1s",
            path.display(),
            sx.stx_mtime.tv_sec
        );
        ensure!(
            sx.stx_ctime.tv_sec == sx.stx_mtime.tv_sec,
            "statx ctime != mtime for the {kind} {} ({}s vs {}s)",
            path.display(),
            sx.stx_ctime.tv_sec,
            sx.stx_mtime.tv_sec
        );
    }
    Ok(Outcome::Pass)
}

/// generic/532: a filesystem must never set a `stx_attributes` flag it
/// does not also advertise in `stx_attributes_mask` (the XFS regression
/// 532 was written for). castore fills neither, but pinning the
/// invariant keeps a future flag-reporting change honest — and a
/// nonzero mask with zero attributes is the correct shape for a store
/// path that has no immutable/append/encrypted bits set.
pub fn generic_532_statx_attributes_mask_sanity(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let (file, dir, symlink) = sample_nodes(ctx)?;
    for (kind, path) in [("file", &file), ("dir", &dir), ("symlink", &symlink)] {
        let sx = statx_basic(path, false)?;
        ensure!(
            sx.stx_attributes & !sx.stx_attributes_mask == 0,
            "statx of the {kind} {} sets attributes {:#x} outside the supported mask {:#x}",
            path.display(),
            sx.stx_attributes,
            sx.stx_attributes_mask
        );
    }
    Ok(Outcome::Pass)
}
