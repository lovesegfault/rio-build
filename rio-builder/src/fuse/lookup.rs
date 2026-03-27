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
}
