//! FUSE attribute helpers.
//!
//! `stat_to_attr` + TTL constants shared by `ops.rs`. The actual `lookup`/
//! `getattr` FUSE handlers live in `ops.rs` (the `Filesystem` trait impl).

use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};

/// 1-hour attribute TTL -- appropriate for read-only filesystem over immutable Nix store paths.
pub const ATTR_TTL: Duration = Duration::from_secs(3600);

/// Standard 512-byte block size for FUSE.
pub const BLOCK_SIZE: u32 = 512;

/// Canonical Nix store-path mtime: one second past the Epoch. Matches
/// `mtimeStore` in Nix's `libstore/posix-fs-canonicalise.cc`. Not 0
/// (some tools treat it as "no timestamp") and never the wall-clock —
/// store paths are immutable and content-addressed; their metadata is
/// fully determined by the NAR.
const STORE_PATH_MTIME: Duration = Duration::from_secs(1);

/// Build a `FileAttr` from filesystem metadata, presenting **canonical
/// Nix store-path metadata** rather than the cache file's on-disk state.
///
/// The on-disk file under the FUSE cache directory is an implementation
/// detail: `restore_path_streaming` writes it with the rio-builder
/// process `uid`/`gid`, an `umask`-derived mode, and (without
/// `builder.nar.canonical-mtime`) `mtime≈now`. The FUSE FS, however, *is*
/// the chroot store's lower layer — what `stat_to_attr` returns is the
/// metadata builds receive for their input store paths. Those MUST match
/// what Nix's reference daemon presents (`canonicalisePathMetaData`):
/// `mtime=1`, `0o444`/`0o555` perms, `root:root`. Leaking the cache
/// file's `mtime` is what made `set-source-date-epoch-to-latest.sh` set
/// `SOURCE_DATE_EPOCH` to the fetch wall-clock and broke `fetchPnpmDeps`.
// r[impl builder.fuse.canonical-metadata]
pub fn stat_to_attr(ino: u64, meta: &std::fs::Metadata) -> FileAttr {
    use std::os::unix::fs::MetadataExt;

    let kind = if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };

    // 0o444 only for non-executable regular files; everything else is
    // 0o555. NAR has no perm bits beyond a single per-file `executable`
    // flag, and Nix canonicalizes dirs/symlinks/exec-files identically.
    let executable = meta.mode() & 0o111 != 0;
    let perm: u16 = if kind == FileType::RegularFile && !executable {
        0o444
    } else {
        0o555
    };

    FileAttr {
        ino: INodeNo(ino),
        size: meta.len(),
        blocks: meta.len().div_ceil(u64::from(BLOCK_SIZE)),
        atime: UNIX_EPOCH + STORE_PATH_MTIME,
        mtime: UNIX_EPOCH + STORE_PATH_MTIME,
        ctime: UNIX_EPOCH + STORE_PATH_MTIME,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink: meta.nlink() as u32,
        uid: 0,
        gid: 0,
        rdev: meta.rdev() as u32,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_stat_to_attr_regular_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello")?;

        let meta = file_path.symlink_metadata()?;
        let attr = stat_to_attr(42, &meta);

        assert_eq!(attr.ino, INodeNo(42));
        assert_eq!(attr.size, 5);
        assert_eq!(attr.kind, FileType::RegularFile);
        Ok(())
    }

    #[test]
    fn test_stat_to_attr_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let meta = dir.path().symlink_metadata()?;
        let attr = stat_to_attr(1, &meta);

        assert_eq!(attr.kind, FileType::Directory);
        Ok(())
    }

    /// `stat_to_attr` MUST present canonical Nix store-path metadata,
    /// not the FUSE cache file's on-disk metadata. The cache directory
    /// is an implementation detail; the FUSE FS *is* the chroot store
    /// and store paths always have:
    ///   - mtime = atime = ctime = 1 second past Epoch
    ///   - perm  = 0o444 (regular non-exec) / 0o555 (exec, dir, symlink)
    ///   - uid   = gid = 0
    ///
    /// If `stat_to_attr` leaks the cache file's `meta.modified()` /
    /// `meta.uid()` / `meta.mode()`, every build sees `mtime≈now` on
    /// inputs (cache files are written at fetch time) and
    /// `set-source-date-epoch-to-latest.sh` derives a non-deterministic
    /// `SOURCE_DATE_EPOCH`, breaking tar-producing FODs (`fetchPnpmDeps`,
    /// `fetchYarnDeps`, …).
    // r[verify builder.fuse.canonical-metadata]
    #[test]
    fn stat_to_attr_canonicalizes_metadata() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, SystemTime};

        let canon = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let dir = tempfile::tempdir()?;

        // A regular, non-executable file. On disk it has mtime≈now,
        // mode 644 (umask-dependent), uid/gid of the test process —
        // none of which are canonical.
        let f = dir.path().join("file.txt");
        fs::write(&f, "x")?;
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644))?;
        let meta = f.symlink_metadata()?;
        // Sanity: on-disk mtime really is non-canonical, otherwise this
        // test would pass vacuously.
        assert_ne!(
            meta.modified()?,
            canon,
            "test setup: on-disk mtime must not already be canonical"
        );
        let attr = stat_to_attr(1, &meta);
        assert_eq!(attr.mtime, canon, "regular file mtime not canonical");
        assert_eq!(attr.atime, canon, "regular file atime not canonical");
        assert_eq!(attr.ctime, canon, "regular file ctime not canonical");
        assert_eq!(attr.perm, 0o444, "regular non-exec file perm not 0o444");
        assert_eq!(attr.uid, 0, "uid not canonical");
        assert_eq!(attr.gid, 0, "gid not canonical");

        // An executable file → 0o555.
        let x = dir.path().join("script.sh");
        fs::write(&x, "#!/bin/sh\n")?;
        fs::set_permissions(&x, fs::Permissions::from_mode(0o755))?;
        let attr = stat_to_attr(2, &x.symlink_metadata()?);
        assert_eq!(attr.mtime, canon, "exec file mtime not canonical");
        assert_eq!(attr.perm, 0o555, "exec file perm not 0o555");

        // A directory → 0o555.
        let d = dir.path().join("sub");
        fs::create_dir(&d)?;
        let attr = stat_to_attr(3, &d.symlink_metadata()?);
        assert_eq!(attr.mtime, canon, "directory mtime not canonical");
        assert_eq!(attr.perm, 0o555, "directory perm not 0o555");

        // A symlink → 0o555 (Linux `lstat()` reports 0o777, but Nix's
        // NAR has no perm bits for symlinks; 0o555 matches the
        // exec/dir convention and avoids advertising writeability).
        let l = dir.path().join("link");
        std::os::unix::fs::symlink("file.txt", &l)?;
        let attr = stat_to_attr(4, &l.symlink_metadata()?);
        assert_eq!(attr.kind, FileType::Symlink);
        assert_eq!(attr.mtime, canon, "symlink mtime not canonical");
        assert_eq!(attr.perm, 0o555, "symlink perm not 0o555");

        Ok(())
    }
}
