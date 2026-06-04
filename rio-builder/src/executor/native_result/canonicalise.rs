//! Output canonicalisation: the filesystem-normalization pass nix-daemon
//! runs on every build output before registration, reimplemented for the
//! native executor.
//!
//! Canonicalisation makes the on-disk form of an output independent of
//! *how* the build produced it (umask, build order, tooling quirks):
//! deterministic mtimes, deterministic permission bits, no build-written
//! xattrs (kernel-owned ACL labels are left alone, matching CppNix's
//! `ignored-acls` default), no foreign owners, no special files.
//! Everything the NAR serialization
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
    #[error("failed to remove extended attribute `{attr}` from {path}: {errno}")]
    Xattr {
        path: String,
        attr: String,
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
    canonicalise_entry(root, &st, build_uid, inodes_seen, true)
}

/// Recursive worker. `st` is the already-fetched lstat of `path`.
/// `is_root` is true only for the output path itself — the
/// group/world-writability rejection applies there alone (CppNix
/// parity); inner entries are normalized instead.
fn canonicalise_entry(
    path: &Path,
    st: &nix::sys::stat::FileStat,
    build_uid: u32,
    inodes_seen: &mut HashSet<InodeId>,
    is_root: bool,
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

    // Inode identity is needed by the ownership check (hard-link second
    // names) as well as by the already-seen skip further down.
    let inode: InodeId = (st.st_dev, st.st_ino);

    // 2. Ownership: every inode must belong to the build user. This is
    //    also the hard-link defence — a link to a foreign-owned file
    //    (host /etc/shadow, another tenant's scratch) shares the inode
    //    and therefore the owner, and is rejected here regardless of
    //    which name we reached it by. One exception, mirroring CppNix's
    //    `canonicalisePathMetaData_`: a non-directory inode this build
    //    has already canonicalised. Production chowns processed inodes
    //    to root (step 8), so the second name of a legitimate hard link
    //    lstats as root-owned — it was verified builder-owned when first
    //    visited, so accept it and skip re-processing.
    if st.st_uid != build_uid {
        if is_dir || !inodes_seen.contains(&inode) {
            return Err(CanonicaliseError::WrongOwner {
                path: path.display().to_string(),
                found_uid: st.st_uid,
                expected_uid: build_uid,
            });
        }
        // CppNix invariant: an already-seen inode must look canonical —
        // its modes were normalized when its first name was processed.
        debug_assert!(
            is_symlink || matches!(st.st_mode & 0o7777, 0o444 | 0o555),
            "already-seen inode {} has non-canonical mode {:o}",
            path.display(),
            st.st_mode & 0o7777
        );
        return Ok(());
    }

    // 3. Permission gate: a group- or world-writable output ROOT is
    //    rejected (CppNix `registerOutputs` checks "suspicious ownership
    //    or permission" on the output path itself). Inner entries are
    //    NOT rejected — CppNix silently normalizes them during
    //    canonicalisation, and step 5 below does the same here.
    //    Symlink modes are meaningless on Linux (always 0777) — exempt.
    if is_root && !is_symlink && (st.st_mode & 0o022) != 0 {
        return Err(CanonicaliseError::Writable {
            path: path.display().to_string(),
            mode: st.st_mode & 0o7777,
        });
    }

    // Hard-link / already-seen handling: an inode reached through a
    // second name (inter-output hard link, or a link within one output)
    // has already been fully canonicalised — skip it.
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
    //    a directory or had the OWNER execute bit. Never on symlinks.
    //
    //    CppNix keys regular-file executability on `S_IXUSR` alone, both
    //    in `canonicalisePathMetaData_` (`mode & S_IXUSR ? 0555 : 0444`)
    //    and in the NAR dumper — a 0655 file (group/other-x, no owner-x)
    //    is non-executable there, so keying on any execute bit would
    //    silently diverge NAR bytes and CA store paths. Directories keep
    //    0555 unconditionally: a directory without owner-x is pathological
    //    (CppNix would chmod it 0444), has no NAR/path-observable effect
    //    (NARs carry no executable flag for directories), and 0444 would
    //    break the recursion below for unprivileged runs.
    if !is_symlink {
        let executable = is_dir || (st.st_mode & 0o100) != 0;
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
            canonicalise_entry(&child, &child_st, build_uid, inodes_seen, false)?;
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

/// Kernel- or filesystem-owned attributes that cannot be removed (often
/// not even by root) and are therefore skipped rather than stripped.
///
/// This mirrors CppNix's `ignored-acls` setting, whose default is
/// `security.csm security.selinux system.nfs4_acl`: SELinux relabels every
/// fresh inode and rejects label removal (`EACCES`) regardless of
/// privilege, and `system.nfs4_acl` is the filesystem's own ACL
/// representation, not data a build wrote. Treating them as fatal would
/// fail every build on a labeling host while stripping them is impossible
/// by design. Skipping them is safe for store semantics: they are not part
/// of NAR serialization, so they can never affect the uploaded bytes or
/// the content address — and the store, not the worker, remains the
/// authority on what gets registered.
fn is_ignored_xattr(name: &[u8]) -> bool {
    matches!(
        name,
        b"security.selinux" | b"system.nfs4_acl" | b"security.csm"
    )
}

/// The three xattr syscall arms of [`strip_xattrs`], each with its own
/// errno-tolerance table. A separate enum (rather than inline matches)
/// because the tables ARE the oracle-parity artifact and the
/// load-bearing answers cannot be produced by local filesystems in a
/// test: `ENODATA` from a *list* call is an SSHFS-class translation,
/// `ENOTSUP` from the *fill* call requires xattr support to vanish
/// between two adjacent syscalls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XattrArm {
    /// `llistxattr(path, NULL, 0)` — the size probe.
    ListProbe,
    /// `llistxattr(path, buf, len)` — the buffer fill.
    ListFill,
    /// `lremovexattr(path, name)` — the per-attribute removal.
    Remove,
}

/// Errno disposition per arm, exactly `canonicalisePathMetaData_`
/// (`posix-fs-canonicalise.cc:67-84`):
///
/// - `ListProbe`: `errno != ENOTSUP && errno != ENODATA` throws
///   (lines 69-71) — i.e. ENOTSUP (no xattr support) and ENODATA
///   (SSHFS-class filesystems answer the list probe with it; bug_101:
///   treating it as fatal failed every build on such filesystems
///   permanently where stock Nix succeeds) are success-empty.
/// - `ListFill`: any failure throws (lines 76-77) — no tolerance.
/// - `Remove`: the oracle throws on any failure (lines 81-83; it skips
///   `ignored-acls` names BEFORE attempting). rio's REGISTERED
///   divergence: ENOTSUP/ENODATA are tolerated here — the attribute is
///   already gone (removed concurrently, or support vanished mid-walk),
///   so failing the build would punish a benign race; an attribute that
///   REMAINS still fails the arm.
// r[impl builder.exec.canonicalise-xattr-errno]
fn xattr_errno_tolerated(arm: XattrArm, errno: nix::errno::Errno) -> bool {
    use nix::errno::Errno;
    match arm {
        XattrArm::ListProbe => matches!(errno, Errno::ENOTSUP | Errno::ENODATA),
        XattrArm::ListFill => false,
        XattrArm::Remove => matches!(errno, Errno::ENOTSUP | Errno::ENODATA),
    }
}

/// Remove every extended attribute on `path` (without following
/// symlinks — callers only invoke this for non-symlinks anyway), except
/// the kernel-owned ACL labels in [`is_ignored_xattr`], which are left in
/// place exactly as CppNix's `canonicalisePathMetaData_` does.
///
/// Errno handling per arm is [`xattr_errno_tolerated`] — the oracle's
/// tables verbatim plus one registered divergence on the remove arm.
/// Uses raw libc: the `nix` crate does not wrap the xattr family.
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
        return if xattr_errno_tolerated(XattrArm::ListProbe, errno) {
            Ok(())
        } else {
            Err(CanonicaliseError::io("llistxattr", path, errno))
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
        return if xattr_errno_tolerated(XattrArm::ListFill, errno) {
            Ok(())
        } else {
            Err(CanonicaliseError::io("llistxattr", path, errno))
        };
    }
    names.truncate(len as usize);

    // The buffer is a sequence of NUL-terminated attribute names.
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if is_ignored_xattr(name) {
            continue;
        }
        let c_name = match CString::new(name) {
            Ok(c) => c,
            Err(_) => continue, // embedded NUL is impossible by construction
        };
        // SAFETY: both pointers are valid NUL-terminated strings.
        let rc = unsafe { libc::lremovexattr(c_path.as_ptr(), c_name.as_ptr()) };
        if rc < 0 {
            let errno = nix::errno::Errno::last();
            if !xattr_errno_tolerated(XattrArm::Remove, errno) {
                return Err(CanonicaliseError::Xattr {
                    path: path.display().to_string(),
                    attr: name.escape_ascii().to_string(),
                    errno,
                });
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

    /// A group/world-writable output ROOT is "suspicious permissions"
    /// (CppNix `registerOutputs` checks the output path itself).
    #[test]
    fn rejects_group_writable_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o775)).unwrap();

        let err = canonicalise_output(&root, my_uid(), &mut HashSet::new()).unwrap_err();
        assert!(
            matches!(err, CanonicaliseError::Writable { .. }),
            "got {err}"
        );
    }

    /// Group/world-writable INNER entries are accepted and normalized to
    /// 0444/0555 — CppNix does not reject them, it canonicalises them
    /// (only the output root is subject to the rejection above).
    #[test]
    fn inner_writable_entries_are_normalized_not_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("loose"), b"x").unwrap();
        std::fs::set_permissions(root.join("loose"), std::fs::Permissions::from_mode(0o666))
            .unwrap();
        std::fs::write(root.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(root.join("tool"), std::fs::Permissions::from_mode(0o775))
            .unwrap();

        canonicalise_output(&root, my_uid(), &mut HashSet::new()).unwrap();
        assert_eq!(
            std::fs::metadata(root.join("loose")).unwrap().mode() & 0o7777,
            0o444
        );
        assert_eq!(
            std::fs::metadata(root.join("tool")).unwrap().mode() & 0o7777,
            0o555
        );
    }

    /// Executability is keyed on the OWNER execute bit only, like
    /// CppNix's `canonicalisePathMetaData_` and NAR dumper: a file with
    /// only group/other execute bits ends up 0444 (non-executable), not
    /// 0555 — keying on any execute bit would diverge NAR bytes and CA
    /// store paths from real Nix.
    #[test]
    fn group_or_other_exec_without_owner_exec_is_not_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        let cases = [
            ("group-x", 0o655, 0o444),
            ("other-x", 0o455, 0o444),
            ("owner-x", 0o744, 0o555),
        ];
        // Write everything first (canonicalisation makes the root 0555),
        // and only assert cases whose unusual mode the environment let us
        // set (mirrors strips_setuid_bits).
        let mut expectations = Vec::new();
        for (name, mode, expected) in cases {
            std::fs::write(root.join(name), b"x").unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
            if std::fs::metadata(root.join(name)).unwrap().mode() & 0o7777 != mode {
                eprintln!("skipping {name}: cannot set mode {mode:o} here");
                std::fs::remove_file(root.join(name)).unwrap();
                continue;
            }
            expectations.push((name, mode, expected));
        }

        canonicalise_output(&root, my_uid(), &mut HashSet::new()).unwrap();

        for (name, mode, expected) in expectations {
            assert_eq!(
                std::fs::metadata(root.join(name)).unwrap().mode() & 0o7777,
                expected,
                "{name}: mode {mode:o} must canonicalise to {expected:o}"
            );
        }
    }

    /// The second name of an inode this build already canonicalised is
    /// accepted even when it no longer lstats as builder-owned —
    /// production chowns processed inodes to root (step 8), so without
    /// this escape every hard-linked file would be rejected as
    /// foreign-owned via its second name (CppNix accepts already-seen
    /// inodes for exactly this reason). A foreign-owned inode that was
    /// NOT processed by this build stays rejected.
    #[test]
    fn already_seen_hard_link_passes_ownership_check() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("first-name");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        let second = tmp.path().join("second-name");
        std::fs::hard_link(&file, &second).unwrap();
        let st = lstat(&second).unwrap();
        let not_us = my_uid().wrapping_add(1);

        // Not seen before → the foreign-owner rejection applies.
        let mut seen = HashSet::new();
        let err = canonicalise_entry(&second, &st, not_us, &mut seen, false).unwrap_err();
        assert!(matches!(err, CanonicaliseError::WrongOwner { .. }));

        // Seen before (first name already canonicalised) → accepted.
        let mut seen = HashSet::new();
        seen.insert((st.st_dev, st.st_ino));
        canonicalise_entry(&second, &st, not_us, &mut seen, false).unwrap();
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
        // Some environments (notably Nix's own build sandbox, whose seccomp
        // filter EPERMs setuid/setgid mode bits — the very behavior this
        // module replicates) refuse to create the precondition; skip there
        // rather than fail on an unsatisfiable setup.
        if std::fs::set_permissions(root.join("tool"), std::fs::Permissions::from_mode(0o4555))
            .is_err()
            || std::fs::metadata(root.join("tool")).unwrap().mode() & 0o4000 == 0
        {
            eprintln!("skipping strips_setuid_bits: cannot create a setuid file here");
            return;
        }

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
            // The attribute we set must be gone. Asserting an empty list
            // would be wrong on hosts whose kernel labels every inode
            // (SELinux) or exposes ACLs as xattrs — those are ignored, not
            // stripped, and legitimately remain.
            // SAFETY: valid NUL-terminated strings; querying size only.
            let got = unsafe {
                libc::lgetxattr(
                    c_path.as_ptr(),
                    c"user.rio_test".as_ptr(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(got, -1, "user.rio_test must have been removed");
            assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::ENODATA);

            // Anything still listed must be a kernel-owned ignored attr.
            // SAFETY: same as above; size query then a sized read.
            let len = unsafe { libc::llistxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
            assert!(len >= 0);
            if len > 0 {
                let mut names = vec![0u8; len as usize];
                let len = unsafe {
                    libc::llistxattr(c_path.as_ptr(), names.as_mut_ptr().cast(), names.len())
                };
                assert!(len >= 0);
                names.truncate(len as usize);
                for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
                    assert!(
                        is_ignored_xattr(name),
                        "non-ignored xattr survived canonicalisation: {}",
                        name.escape_ascii()
                    );
                }
            }
        }
    }

    /// The ignored set is exactly CppNix's `ignored-acls` default; build-
    /// or user-written attributes are never ignored.
    // r[verify builder.exec.canonicalise-xattr-errno]
    #[test]
    fn ignored_xattr_set_matches_cppnix_default() {
        assert!(is_ignored_xattr(b"security.selinux"));
        assert!(is_ignored_xattr(b"system.nfs4_acl"));
        assert!(is_ignored_xattr(b"security.csm"));

        assert!(!is_ignored_xattr(b"user.rio_test"));
        assert!(!is_ignored_xattr(b"user.foo"));
        assert!(!is_ignored_xattr(b"trusted.overlay.opaque"));
        assert!(!is_ignored_xattr(b"security.capability"));
        assert!(!is_ignored_xattr(b"security.selinux.extra"));
    }

    /// The full errno-disposition table of the three xattr arms, vs
    /// `canonicalisePathMetaData_` (posix-fs-canonicalise.cc:67-84).
    /// Pinned as a table because the load-bearing rows are
    /// untriggerable on local filesystems: ENODATA from a LIST call is
    /// an SSHFS-class translation (bug_101 — every build on such a
    /// filesystem failed permanently as an output rejection where
    /// stock Nix succeeds), and ENOTSUP at the fill arm needs support
    /// to vanish between adjacent syscalls.
    // r[verify builder.exec.canonicalise-xattr-errno]
    #[test]
    fn xattr_errno_table_matches_oracle() {
        use nix::errno::Errno;

        // Probe arm: oracle line 69-71 — ENOTSUP and ENODATA are
        // success-empty; everything else fails the build.
        assert!(xattr_errno_tolerated(XattrArm::ListProbe, Errno::ENOTSUP));
        assert!(xattr_errno_tolerated(XattrArm::ListProbe, Errno::ENODATA));
        for fatal in [Errno::EACCES, Errno::EPERM, Errno::EIO, Errno::ERANGE] {
            assert!(
                !xattr_errno_tolerated(XattrArm::ListProbe, fatal),
                "{fatal} must fail the probe arm"
            );
        }

        // Fill arm: oracle line 76-77 — NOTHING is tolerated; rio's
        // former ENOTSUP tolerance here was an unregistered delta,
        // removed.
        for e in [
            Errno::ENOTSUP,
            Errno::ENODATA,
            Errno::EACCES,
            Errno::EIO,
            Errno::ERANGE,
        ] {
            assert!(
                !xattr_errno_tolerated(XattrArm::ListFill, e),
                "{e} must fail the fill arm (oracle tolerates nothing)"
            );
        }

        // Remove arm: the REGISTERED divergence — the oracle (line
        // 81-83) throws on any failure; rio tolerates exactly the
        // already-gone answers and nothing else.
        assert!(xattr_errno_tolerated(XattrArm::Remove, Errno::ENOTSUP));
        assert!(xattr_errno_tolerated(XattrArm::Remove, Errno::ENODATA));
        for fatal in [Errno::EACCES, Errno::EPERM, Errno::EIO] {
            assert!(
                !xattr_errno_tolerated(XattrArm::Remove, fatal),
                "{fatal} must fail the remove arm"
            );
        }
    }
}
