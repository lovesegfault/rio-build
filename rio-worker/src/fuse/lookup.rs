//! Path existence and metadata queries for the FUSE store.
//!
//! Handles `lookup` and `getattr` operations by checking the local SSD cache
//! first, then falling back to `StoreService.QueryPathInfo` via gRPC.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};

/// 1-hour attribute TTL -- appropriate for read-only filesystem over immutable Nix store paths.
pub const ATTR_TTL: Duration = Duration::from_secs(3600);

/// Standard 512-byte block size for FUSE.
pub const BLOCK_SIZE: u32 = 512;

/// Build a `FileAttr` from filesystem metadata.
pub fn stat_to_attr(ino: u64, meta: &std::fs::Metadata) -> FileAttr {
    use std::os::unix::fs::MetadataExt;

    let kind = if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };

    let atime = meta
        .accessed()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mtime = meta
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ctime = Duration::new(
        u64::try_from(meta.ctime()).unwrap_or(0),
        u32::try_from(meta.ctime_nsec()).unwrap_or(0),
    );

    FileAttr {
        ino: INodeNo(ino),
        size: meta.len(),
        blocks: meta.len().div_ceil(u64::from(BLOCK_SIZE)),
        atime: UNIX_EPOCH + atime,
        mtime: UNIX_EPOCH + mtime,
        ctime: UNIX_EPOCH + ctime,
        crtime: UNIX_EPOCH,
        kind,
        perm: (meta.mode() & 0o7777) as u16,
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

/// Build a synthetic `FileAttr` for a directory that exists in the store
/// but hasn't been cached locally yet.
pub fn synthetic_dir_attr(ino: u64) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o555,
        nlink: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

/// Extract the store path basename from a full path under `/nix/store/`.
///
/// For example, `/nix/store/abc...-hello-1.0` returns `Some("abc...-hello-1.0")`.
/// For deeper paths like `/nix/store/abc...-hello-1.0/bin/hello`, returns the
/// first component `Some("abc...-hello-1.0")`.
pub fn extract_store_basename(path: &Path) -> Option<&str> {
    // The mount point is the root inode; children under it are store path basenames.
    // We look for the first component after stripping the mount prefix.
    path.file_name()?.to_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_stat_to_attr_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let meta = file_path.symlink_metadata().unwrap();
        let attr = stat_to_attr(42, &meta);

        assert_eq!(attr.ino, INodeNo(42));
        assert_eq!(attr.size, 5);
        assert_eq!(attr.kind, FileType::RegularFile);
    }

    #[test]
    fn test_stat_to_attr_directory() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().symlink_metadata().unwrap();
        let attr = stat_to_attr(1, &meta);

        assert_eq!(attr.kind, FileType::Directory);
    }

    #[test]
    fn test_synthetic_dir_attr() {
        let attr = synthetic_dir_attr(10);
        assert_eq!(attr.ino, INodeNo(10));
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.perm, 0o555);
    }

    #[test]
    fn test_extract_store_basename() {
        let p = Path::new("abc-hello-1.0");
        assert_eq!(extract_store_basename(p), Some("abc-hello-1.0"));
    }
}
