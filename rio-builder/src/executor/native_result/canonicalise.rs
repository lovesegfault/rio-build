//! Output canonicalisation: the filesystem-normalization pass nix-daemon
//! runs on every build output before registration, reimplemented for the
//! native executor.
//!
//! Canonicalisation makes the on-disk form of an output independent of
//! *how* the build produced it (umask, build order, tooling quirks):
//! deterministic mtimes, deterministic permission bits, no xattrs, no
//! foreign owners, no special files. Everything the NAR serialization
//! cannot represent is either normalized away (timestamps, write bits,
//! xattrs) or rejected (FIFOs, sockets, device nodes) — otherwise two
//! builds of identical content could upload different NARs, or worse,
//! the uploaded NAR would silently differ from what is on disk.
//!
//! Mirrors CppNix's `canonicalisePathMetaData` (posix-fs-canonicalise.cc)
//! with the same ordering: type check → ownership/permission check →
//! xattr strip → chmod → recurse → mtime. The ownership check runs
//! unconditionally; the `lchown` to root only when the executor itself
//! runs as root (production pods do, unit tests do not — same euid gate
//! as `rio_exec`'s skeleton builder).

use std::collections::HashSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::sys::stat::{FchmodatFlags, Mode, SFlag, UtimensatFlags, fchmodat, lstat, utimensat};
use nix::sys::time::TimeSpec;

/// A `(st_dev, st_ino)` pair: the identity of an inode across the whole
/// build, used to let inter-output hard links survive canonicalisation
/// (each inode is processed once) without double-resetting timestamps.
pub(crate) type InodeId = (u64, u64);

/// Why an output failed canonicalisation. The message is operator- and
/// tenant-facing (it ends up in the BuildResult error), so every variant
/// names the offending path.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CanonicaliseError {
    #[error("failed to produce output path {path}")]
    Missing { path: String },
    #[error("output {path} contains a {kind}, which cannot be represented in a NAR archive")]
    SpecialFile { path: String, kind: &'static str },
    #[error(
        "suspicious ownership on {path}: owned by uid {found_uid}, expected the build user \
         (uid {expected_uid})"
    )]
    WrongOwner {
        path: String,
        found_uid: u32,
        expected_uid: u32,
    },
    #[error("suspicious permissions on {path}: mode {mode:o} is group- or world-writable")]
    Writable { path: String, mode: u32 },
    #[error("filesystem operation `{op}` failed on {path}: {errno}")]
    Io {
        op: &'static str,
        path: String,
        errno: nix::errno::Errno,
    },
}

impl CanonicaliseError {
    fn io(op: &'static str, path: &Path, errno: nix::errno::Errno) -> Self {
        Self::Io {
            op,
            path: path.display().to_string(),
            errno,
        }
    }
}

/// Canonicalise one output tree rooted at `root`.
///
/// `build_uid` is the uid the sandboxed build ran as — every inode in
/// the output must be owned by it (anything else means the build smuggled
/// in a foreign file, e.g. via a hard link to a path outside the sandbox
/// scratch area).
///
/// `inodes_seen` is shared across all outputs of one build: an inode
/// already canonicalised through another output (an inter-output hard
/// link) is skipped, which both avoids redundant work and is what allows
/// such links to exist at all — re-checking it would be harmless, but
/// re-setting timestamps through a second name is wasted I/O.
///
/// Blocking filesystem I/O — call from `spawn_blocking` in async code.
pub(crate) fn canonicalise_output(
    root: &Path,
    build_uid: u32,
    inodes_seen: &mut HashSet<InodeId>,
) -> Result<(), CanonicaliseError> {
    let st = match lstat(root) {
        Ok(st) => st,
        Err(nix::errno::Errno::ENOENT) => {
            return Err(CanonicaliseError::Missing {
                path: root.display().to_string(),
            });
        }
        Err(e) => return Err(CanonicaliseError::io("lstat", root, e)),
    };
    canonicalise_entry(root, &st, build_uid, inodes_seen)
}

/// Recursive worker. `st` is the already-fetched lstat of `path`.
fn canonicalise_entry(
    path: &Path,
    st: &nix::sys::stat::FileStat,
    build_uid: u32,
    inodes_seen: &mut HashSet<InodeId>,
) -> Result<(), CanonicaliseError> {
    let kind = SFlag::from_bits_truncate(st.st_mode & SFlag::S_IFMT.bits());
    let is_symlink = kind == SFlag::S_IFLNK;
    let is_dir = kind == SFlag::S_IFDIR;
    let is_regular = kind == SFlag::S_IFREG;

    // 1. Type gate: only regular files, directories, and symlinks are
    //    representable in a NAR. Anything else is an output defect, not
    //    something to silently drop.
    if !(is_regular || is_dir || is_symlink) {
        let kind_name = match kind {
            SFlag::S_IFIFO => "FIFO",
            SFlag::S_IFSOCK => "socket",
            SFlag::S_IFBLK => "block device",
            SFlag::S_IFCHR => "character device",
            _ => "special file",
        };
        return Err(CanonicaliseError::SpecialFile {
            path: path.display().to_string(),
            kind: kind_name,
        });
    }

    // 2. Ownership: every inode must belong to the build user. This is
    //    also the hard-link defence — a link to a foreign-owned file
    //    (host /etc/shadow, another tenant's scratch) shares the inode
    //    and therefore the owner, and is rejected here regardless of
    //    which name we reached it by.
    if st.st_uid != build_uid {
        return Err(CanonicaliseError::WrongOwner {
            path: path.display().to_string(),
            found_uid: st.st_uid,
            expected_uid: build_uid,
        });
    }

    // 3. Permission gate: group- or world-writable non-symlink entries
    //    are rejected (CppNix: "suspicious ownership or permission").
    //    Symlink modes are meaningless on Linux (always 0777) — exempt.
    if !is_symlink && (st.st_mode & 0o022) != 0 {
        return Err(CanonicaliseError::Writable {
            path: path.display().to_string(),
            mode: st.st_mode & 0o7777,
        });
    }

    // Hard-link / already-seen handling: an inode reached through a
    // second name (inter-output hard link, or a link within one output)
    // has already been fully canonicalised — skip it.
    let inode: InodeId = (st.st_dev, st.st_ino);
    if st.st_nlink > 1 && !inodes_seen.insert(inode) {
        return Ok(());
    }

    // 4. Strip extended attributes (not representable in NARs; a build
    //    that sets them would round-trip differently through the store).
    //    Symlinks are skipped: l*xattr on symlinks is rejected by most
    //    filesystems anyway and CppNix skips them too.
    if !is_symlink {
        strip_xattrs(path)?;
    }

    // 5. Deterministic permission bits: 0444, plus 0111 iff the entry is
    //    a directory or had any execute bit. Never on symlinks.
    if !is_symlink {
        let executable = is_dir || (st.st_mode & 0o111) != 0;
        let new_mode = if executable { 0o555 } else { 0o444 };
        if st.st_mode & 0o7777 != new_mode {
            fchmodat(
                nix::fcntl::AT_FDCWD,
                path,
                Mode::from_bits_truncate(new_mode),
                FchmodatFlags::FollowSymlink,
            )
            .map_err(|e| CanonicaliseError::io("chmod", path, e))?;
        }
    }

    // 6. Recurse into directories before stamping the directory's own
    //    mtime (post-order), so child operations cannot disturb it.
    if is_dir {
        let entries = std::fs::read_dir(path).map_err(|e| CanonicaliseError::Io {
            op: "readdir",
            path: path.display().to_string(),
            errno: e
                .raw_os_error()
                .map(nix::errno::Errno::from_raw)
                .unwrap_or(nix::errno::Errno::EIO),
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| CanonicaliseError::Io {
                op: "readdir",
                path: path.display().to_string(),
                errno: e
                    .raw_os_error()
                    .map(nix::errno::Errno::from_raw)
                    .unwrap_or(nix::errno::Errno::EIO),
            })?;
            let child = entry.path();
            let child_st = lstat(&child).map_err(|e| CanonicaliseError::io("lstat", &child, e))?;
            canonicalise_entry(&child, &child_st, build_uid, inodes_seen)?;
        }
    }

    // 7. Deterministic mtime (the NAR doesn't carry it, but the on-disk
    //    tree feeds NAR dumps and FUSE serves it later — mtime=1 is the
    //    store-wide convention). atime is preserved (CppNix does the
    //    same); AT_SYMLINK_NOFOLLOW so symlinks stamp the link itself.
    if st.st_mtime != 1 {
        utimensat(
            nix::fcntl::AT_FDCWD,
            path,
            &TimeSpec::new(st.st_atime, st.st_atime_nsec),
            &TimeSpec::new(1, 0),
            UtimensatFlags::NoFollowSymlink,
        )
        .map_err(|e| CanonicaliseError::io("utimensat", path, e))?;
    }

    // 8. Ownership normalization to root:root — production only. The
    //    ownership *check* above already ran; handing the tree to root
    //    is what makes the store path immutable from the build user's
    //    perspective. Unit tests run unprivileged and skip it.
    if nix::unistd::geteuid().is_root() {
        nix::unistd::fchownat(
            nix::fcntl::AT_FDCWD,
            path,
            Some(nix::unistd::Uid::from_raw(0)),
            Some(nix::unistd::Gid::from_raw(0)),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .map_err(|e| CanonicaliseError::io("lchown", path, e))?;
    }

    Ok(())
}

/// Remove every extended attribute on `path` (without following
/// symlinks — callers only invoke this for non-symlinks anyway).
///
/// `ENOTSUP` (filesystem without xattr support) and `ENODATA` (attribute
/// vanished between list and remove) are not errors. Uses raw libc:
/// the `nix` crate does not wrap the xattr family.
fn strip_xattrs(path: &Path) -> Result<(), CanonicaliseError> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| CanonicaliseError::Io {
        op: "llistxattr",
        path: path.display().to_string(),
        errno: nix::errno::Errno::EINVAL,
    })?;

    // First call sizes the name buffer; an empty list is the common case.
    // SAFETY: c_path is a valid NUL-terminated string; a NULL buffer with
    // size 0 is the documented "query the size" form of llistxattr.
    let len = unsafe { libc::llistxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
    if len < 0 {
        let errno = nix::errno::Errno::last();
        return match errno {
            nix::errno::Errno::ENOTSUP => Ok(()),
            e => Err(CanonicaliseError::io("llistxattr", path, e)),
        };
    }
    if len == 0 {
        return Ok(());
    }

    let mut names = vec![0u8; len as usize];
    // SAFETY: `names` is a writable buffer of exactly the size the kernel
    // just reported; a concurrent change is handled by re-checking the
    // return value.
    let len = unsafe { libc::llistxattr(c_path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if len < 0 {
        let errno = nix::errno::Errno::last();
        return match errno {
            nix::errno::Errno::ENOTSUP => Ok(()),
            e => Err(CanonicaliseError::io("llistxattr", path, e)),
        };
    }
    names.truncate(len as usize);

    // The buffer is a sequence of NUL-terminated attribute names.
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let c_name = match CString::new(name) {
            Ok(c) => c,
            Err(_) => continue, // embedded NUL is impossible by construction
        };
        // SAFETY: both pointers are valid NUL-terminated strings.
        let rc = unsafe { libc::lremovexattr(c_path.as_ptr(), c_name.as_ptr()) };
        if rc < 0 {
            let errno = nix::errno::Errno::last();
            match errno {
                nix::errno::Errno::ENOTSUP | nix::errno::Errno::ENODATA => {}
                e => return Err(CanonicaliseError::io("lremovexattr", path, e)),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn my_uid() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    /// Happy path: a tree with a script, a data file, a subdirectory and
    /// a symlink comes out 0555/0444, mtime=1, with the symlink left
    /// mode-untouched.
    #[test]
    fn canonicalises_modes_and_mtimes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/tool"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            root.join("bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(root.join("data"), b"payload").unwrap();
        std::fs::set_permissions(root.join("data"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        symlink("bin/tool", root.join("link")).unwrap();

        let mut seen = HashSet::new();
        canonicalise_output(&root, my_uid(), &mut seen).unwrap();

        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().mode() & 0o7777;
        assert_eq!(mode(&root), 0o555, "directories become 0555");
        assert_eq!(mode(&root.join("bin/tool")), 0o555, "executables keep +x");
        assert_eq!(mode(&root.join("data")), 0o444, "plain files become 0444");
        for p in ["", "bin", "bin/tool", "data", "link"] {
            let meta = std::fs::symlink_metadata(root.join(p)).unwrap();
            assert_eq!(meta.mtime(), 1, "mtime of {p:?} must be 1");
        }
    }

    /// FIFOs cannot be represented in a NAR and must be rejected, not
    /// silently dropped.
    #[test]
    fn rejects_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        nix::unistd::mkfifo(
            &root.join("pipe"),
            nix::sys::stat::Mode::from_bits(0o644).unwrap(),
        )
        .unwrap();

        let err = canonicalise_output(&root, my_uid(), &mut HashSet::new()).unwrap_err();
        assert!(
            matches!(err, CanonicaliseError::SpecialFile { kind, .. } if kind == "FIFO"),
            "got {err}"
        );
    }

    /// Group/world-writable files are "suspicious permissions".
    #[test]
    fn rejects_world_writable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("loose"), b"x").unwrap();
        std::fs::set_permissions(root.join("loose"), std::fs::Permissions::from_mode(0o666))
            .unwrap();

        let err = canonicalise_output(&root, my_uid(), &mut HashSet::new()).unwrap_err();
        assert!(
            matches!(err, CanonicaliseError::Writable { .. }),
            "got {err}"
        );
    }

    /// A file owned by a different uid than the build user is rejected —
    /// this is also the hard-link-to-foreign-file defence.
    #[test]
    fn rejects_foreign_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();
        std::fs::set_permissions(root.join("file"), std::fs::Permissions::from_mode(0o644))
            .unwrap();

        // Claim the build ran as a uid that is NOT ours; our files then
        // look foreign-owned.
        let not_us = my_uid().wrapping_add(1);
        let err = canonicalise_output(&root, not_us, &mut HashSet::new()).unwrap_err();
        assert!(
            matches!(err, CanonicaliseError::WrongOwner { .. }),
            "got {err}"
        );
    }

    /// A missing output is its own distinct error ("failed to produce").
    #[test]
    fn missing_output_is_distinct() {
        let tmp = tempfile::tempdir().unwrap();
        let err = canonicalise_output(&tmp.path().join("nope"), my_uid(), &mut HashSet::new())
            .unwrap_err();
        assert!(
            matches!(err, CanonicaliseError::Missing { .. }),
            "got {err}"
        );
    }

    /// Hard links between two outputs of the same build survive: the
    /// second walk sees the inode in `inodes_seen` and skips it.
    #[test]
    fn inter_output_hard_links_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let out_a = tmp.path().join("a");
        let out_b = tmp.path().join("b");
        std::fs::create_dir_all(&out_a).unwrap();
        std::fs::create_dir_all(&out_b).unwrap();
        std::fs::write(out_a.join("shared"), b"both").unwrap();
        std::fs::set_permissions(out_a.join("shared"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::hard_link(out_a.join("shared"), out_b.join("shared")).unwrap();

        let mut seen = HashSet::new();
        canonicalise_output(&out_a, my_uid(), &mut seen).unwrap();
        canonicalise_output(&out_b, my_uid(), &mut seen).unwrap();
        assert_eq!(
            std::fs::metadata(out_b.join("shared")).unwrap().mode() & 0o7777,
            0o444
        );
    }

    /// Setuid/setgid bits are stripped by the 0444/0555 normalization
    /// (the seccomp filter prevents creating them in the sandbox, but a
    /// pre-staged file could arrive via an unpacked archive).
    #[test]
    fn strips_setuid_bits() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("tool"), b"x").unwrap();
        // 04555: setuid + r-xr-xr-x. Not group/world-writable, so it passes
        // the writability gate and must be cleaned by the chmod step.
        std::fs::set_permissions(root.join("tool"), std::fs::Permissions::from_mode(0o4555))
            .unwrap();

        canonicalise_output(&root, my_uid(), &mut HashSet::new()).unwrap();
        assert_eq!(
            std::fs::metadata(root.join("tool")).unwrap().mode() & 0o7777,
            0o555,
            "setuid bit must be gone"
        );
    }

    /// Xattrs are stripped when the filesystem supports them; on
    /// filesystems that don't, the strip is a no-op rather than an error.
    #[test]
    fn strips_xattrs_when_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        let f = root.join("file");
        std::fs::write(&f, b"x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();

        let c_path = CString::new(f.as_os_str().as_bytes()).unwrap();
        let val = b"v";
        // Best-effort: tmpdirs on tmpfs support user.* xattrs on recent
        // kernels; if setting fails (ENOTSUP/EPERM) the strip path for
        // "no xattrs present" is still exercised.
        // SAFETY: valid NUL-terminated strings + a real buffer.
        let set_rc = unsafe {
            libc::lsetxattr(
                c_path.as_ptr(),
                c"user.rio_test".as_ptr(),
                val.as_ptr().cast(),
                val.len(),
                0,
            )
        };

        canonicalise_output(&root, my_uid(), &mut HashSet::new()).unwrap();

        if set_rc == 0 {
            // SAFETY: same as above; querying size only.
            let len = unsafe { libc::llistxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
            assert_eq!(len, 0, "xattr must have been removed");
        }
    }
}
