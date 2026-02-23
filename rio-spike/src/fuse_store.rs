use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use std::{fs, io};

use fuser::{
    AccessFlags, BackingId, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEntry, ReplyOpen, Request,
};

// 1-hour attribute TTL — appropriate for read-only filesystem over immutable Nix store paths
const TTL: Duration = Duration::from_secs(3600);
// Standard 512-byte sector size for FUSE statfs and block count calculations
const BLOCK_SIZE: u32 = 512;

/// Bidirectional inode-to-path map, protected by a single lock to ensure consistency.
struct InodeMap {
    inode_to_path: HashMap<u64, PathBuf>,
    path_to_inode: HashMap<PathBuf, u64>,
    next_inode: u64,
}

impl InodeMap {
    fn new(root_path: PathBuf) -> Self {
        let mut inode_to_path = HashMap::new();
        let mut path_to_inode = HashMap::new();
        inode_to_path.insert(INodeNo::ROOT.0, root_path.clone());
        path_to_inode.insert(root_path, INodeNo::ROOT.0);
        Self {
            inode_to_path,
            path_to_inode,
            next_inode: 2,
        }
    }

    fn get_or_create(&mut self, path: PathBuf) -> u64 {
        if let Some(&ino) = self.path_to_inode.get(&path) {
            return ino;
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        self.inode_to_path.insert(ino, path.clone());
        self.path_to_inode.insert(path, ino);
        ino
    }

    fn real_path(&self, ino: u64) -> Option<PathBuf> {
        self.inode_to_path.get(&ino).cloned()
    }

    fn parent_inode(&self, dir_path: &Path) -> u64 {
        dir_path
            .parent()
            .and_then(|p| self.path_to_inode.get(p).copied())
            .unwrap_or(INodeNo::ROOT.0)
    }
}

/// A passthrough FUSE filesystem that serves files from a backing directory.
///
/// When `passthrough` is enabled, `read()` calls bypass userspace via kernel-level
/// file descriptor passthrough (Linux 6.9+). However, `lookup()` and `open()` calls
/// still traverse userspace, which dominates latency for workloads that open many
/// small files. See Phase 1a spike results in `docs/src/components/worker.md`.
pub struct PassthroughFs {
    #[allow(dead_code)]
    backing_dir: PathBuf,
    inodes: RwLock<InodeMap>,
    next_fh: AtomicU64,
    passthrough: bool,
    /// Backing file handles + BackingIds — must stay alive until release()
    backing_state: RwLock<HashMap<u64, (File, BackingId)>>,
    /// Count of passthrough open_backing failures (logged at power-of-two
    /// intervals during open, and summarized in destroy)
    passthrough_failures: AtomicU64,
}

impl PassthroughFs {
    pub fn new(backing_dir: PathBuf, passthrough: bool) -> Self {
        let inodes = InodeMap::new(backing_dir.clone());
        Self {
            backing_dir,
            inodes: RwLock::new(inodes),
            next_fh: AtomicU64::new(1),
            passthrough,
            backing_state: RwLock::new(HashMap::new()),
            passthrough_failures: AtomicU64::new(0),
        }
    }

    fn get_or_create_inode(&self, path: PathBuf) -> u64 {
        // Fast path: read lock for existing inodes
        {
            let map = self.inodes.read().unwrap_or_else(|e| {
                tracing::error!("inodes lock poisoned on read, recovering");
                e.into_inner()
            });
            if let Some(&ino) = map.path_to_inode.get(&path) {
                return ino;
            }
        }
        // Slow path: write lock to allocate
        let mut map = self.inodes.write().unwrap_or_else(|e| {
            tracing::error!("inodes lock poisoned on write, recovering");
            e.into_inner()
        });
        map.get_or_create(path)
    }

    fn real_path(&self, ino: u64) -> Option<PathBuf> {
        self.inodes
            .read()
            .unwrap_or_else(|e| {
                tracing::error!("inodes lock poisoned on read, recovering");
                e.into_inner()
            })
            .real_path(ino)
    }

    fn stat_to_attr(ino: u64, meta: &fs::Metadata) -> FileAttr {
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
}

fn io_error_to_errno(e: &io::Error) -> Errno {
    match e.kind() {
        io::ErrorKind::NotFound => Errno::ENOENT,
        io::ErrorKind::PermissionDenied => Errno::EACCES,
        _ => Errno::EIO,
    }
}

impl Filesystem for PassthroughFs {
    fn init(
        &mut self,
        _req: &Request,
        config: &mut fuser::KernelConfig,
    ) -> Result<(), std::io::Error> {
        if self.passthrough {
            // Enable passthrough: max_stack_depth=1 allows this FUSE mount to be
            // stacked under one overlayfs layer (the production layout).
            match config.set_max_stack_depth(1) {
                Ok(_) => tracing::info!("FUSE passthrough enabled (max_stack_depth=1)"),
                Err(max) => {
                    tracing::warn!(
                        max,
                        "kernel rejected max_stack_depth=1, disabling passthrough mode"
                    );
                    self.passthrough = false;
                }
            }
        }
        Ok(())
    }

    fn destroy(&mut self) {
        let failures = self.passthrough_failures.load(Ordering::Relaxed);
        if failures > 0 {
            tracing::warn!(
                count = failures,
                "passthrough open_backing failed for some files (backing fs may be overlay)"
            );
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.real_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let child_path = parent_path.join(name);

        match child_path.symlink_metadata() {
            Ok(meta) => {
                let ino = self.get_or_create_inode(child_path);
                let attr = Self::stat_to_attr(ino, &meta);
                reply.entry(&TTL, &attr, Generation(0));
            }
            Err(e) => {
                // ENOENT is normal — the kernel probes for files that may not exist
                // (e.g., dynamic linker searching for glibc-hwcaps, shared libs).
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(parent = parent.0, name = ?name, error = %e, "lookup failed");
                }
                reply.error(io_error_to_errno(&e));
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match path.symlink_metadata() {
            Ok(meta) => {
                let attr = Self::stat_to_attr(ino.0, &meta);
                reply.attr(&TTL, &attr);
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(ino = ino.0, path = %path.display(), error = %e, "getattr failed");
                }
                reply.error(io_error_to_errno(&e));
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match fs::read_link(&path) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(e) => {
                tracing::warn!(ino = ino.0, path = %path.display(), error = %e, "readlink failed");
                reply.error(Errno::EINVAL);
            }
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);

        if self.passthrough {
            match File::open(&path) {
                Ok(file) => match reply.open_backing(&file) {
                    Ok(backing_id) => {
                        // Send the passthrough reply, then keep state alive until release()
                        reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &backing_id);
                        self.backing_state
                            .write()
                            .unwrap_or_else(|e| {
                                tracing::error!("backing_state lock poisoned, recovering");
                                e.into_inner()
                            })
                            .insert(fh, (file, backing_id));
                    }
                    Err(e) => {
                        let count = self.passthrough_failures.fetch_add(1, Ordering::Relaxed);
                        if count == 0 {
                            tracing::warn!(
                                ino = ino.0,
                                error = %e,
                                "passthrough open_backing failed, falling back to standard FUSE read. \
                                 Likely cause: kernel does not support FUSE passthrough (requires \
                                 Linux 6.9+) or backing filesystem is stacked (overlayfs)."
                            );
                        }
                        reply.opened(FileHandle(fh), FopenFlags::empty());
                    }
                },
                Err(e) => {
                    tracing::warn!(ino = ino.0, path = %path.display(), error = %e, "open failed");
                    reply.error(Errno::EIO);
                }
            }
        } else {
            reply.opened(FileHandle(fh), FopenFlags::empty());
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match read_file_range(&path, offset, size as usize) {
            Ok(data) => reply.data(&data),
            Err(e) => {
                tracing::warn!(ino = ino.0, path = %path.display(), offset, size, error = %e, "read failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.backing_state
            .write()
            .unwrap_or_else(|e| {
                tracing::error!("backing_state lock poisoned, recovering");
                e.into_inner()
            })
            .remove(&fh.0);
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(dir_path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let entries = match fs::read_dir(&dir_path) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(ino = ino.0, path = %dir_path.display(), error = %e, "readdir failed");
                reply.error(Errno::EIO);
                return;
            }
        };

        let mut all_entries: Vec<(u64, FileType, String)> = Vec::new();
        all_entries.push((ino.0, FileType::Directory, ".".to_string()));
        // Look up the real parent inode; fall back to ROOT if not found
        let parent_ino = self
            .inodes
            .read()
            .unwrap_or_else(|e| {
                tracing::error!("inodes lock poisoned on read, recovering");
                e.into_inner()
            })
            .parent_inode(&dir_path);
        all_entries.push((parent_ino, FileType::Directory, "..".to_string()));

        for result in entries {
            let entry = match result {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(ino = ino.0, path = %dir_path.display(), error = %e, "skipping unreadable dir entry");
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_path = dir_path.join(&name);

            let kind = match entry.file_type() {
                Ok(ft) if ft.is_dir() => FileType::Directory,
                Ok(ft) if ft.is_symlink() => FileType::Symlink,
                Ok(_) => FileType::RegularFile,
                Err(e) => {
                    tracing::warn!(path = %child_path.display(), error = %e, "skipping entry with unknown file type");
                    continue;
                }
            };

            let child_ino = self.get_or_create_inode(child_path);
            all_entries.push((child_ino, kind, name));
        }

        for (i, (entry_ino, kind, name)) in all_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*entry_ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: fuser::ReplyEmpty) {
        if self.real_path(ino.0).is_some() {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, BLOCK_SIZE, 255, 0);
    }
}

fn read_file_range(path: &Path, offset: u64, size: usize) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let meta = file.metadata()?;
    let file_size = meta.len();

    if offset >= file_size {
        return Ok(Vec::new());
    }

    let read_size = size.min((file_size - offset) as usize);
    let mut buf = vec![0u8; read_size];

    file.seek(SeekFrom::Start(offset))?;
    let n = file.read(&mut buf)?;
    buf.truncate(n);

    Ok(buf)
}

fn make_fuse_config() -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("rio-spike".to_string()),
        MountOption::AutoUnmount,
    ];
    config.acl = fuser::SessionACL::All; // allow_other
    config
}

/// Mount the passthrough FUSE filesystem (blocks until unmounted).
pub fn mount_fuse(backing_dir: &Path, mount_point: &Path, passthrough: bool) -> anyhow::Result<()> {
    let fs = PassthroughFs::new(backing_dir.to_path_buf(), passthrough);

    tracing::info!(
        backing_dir = %backing_dir.display(),
        mount_point = %mount_point.display(),
        passthrough,
        "mounting passthrough FUSE filesystem"
    );

    fuser::mount2(fs, mount_point, &make_fuse_config())?;
    Ok(())
}

/// CLI entry point for the fuse-mount subcommand.
pub fn run_fuse_mount(
    backing_dir: &Path,
    mount_point: &Path,
    passthrough: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        backing_dir.is_dir(),
        "backing directory does not exist: {}",
        backing_dir.display()
    );

    if !mount_point.exists() {
        fs::create_dir_all(mount_point)?;
    }

    tracing::info!("press Ctrl+C to unmount and exit");
    mount_fuse(backing_dir, mount_point, passthrough)
}

/// Mount FUSE in a background thread, returning a handle to unmount later.
pub fn mount_fuse_background(
    backing_dir: &Path,
    mount_point: &Path,
    passthrough: bool,
) -> anyhow::Result<fuser::BackgroundSession> {
    let fs = PassthroughFs::new(backing_dir.to_path_buf(), passthrough);

    let session = fuser::spawn_mount2(fs, mount_point, &make_fuse_config())?;

    tracing::info!(
        backing_dir = %backing_dir.display(),
        mount_point = %mount_point.display(),
        passthrough,
        "FUSE filesystem mounted in background"
    );

    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough_fs_init() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), false);

        assert_eq!(fs.real_path(INodeNo::ROOT.0).as_deref(), Some(dir.path()));
        assert!(fs.real_path(2).is_none());
    }

    #[test]
    fn test_inode_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path().to_path_buf(), false);

        let path1 = dir.path().join("a");
        let path2 = dir.path().join("b");

        let ino1 = fs.get_or_create_inode(path1.clone());
        let ino2 = fs.get_or_create_inode(path2);

        assert_ne!(ino1, ino2);
        assert_eq!(fs.get_or_create_inode(path1), ino1);
    }

    #[test]
    fn test_stat_to_attr_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let meta = file_path.symlink_metadata().unwrap();
        let attr = PassthroughFs::stat_to_attr(42, &meta);

        assert_eq!(attr.ino, INodeNo(42));
        assert_eq!(attr.size, 5);
        assert_eq!(attr.kind, FileType::RegularFile);
    }

    #[test]
    fn test_stat_to_attr_directory() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().symlink_metadata().unwrap();
        let attr = PassthroughFs::stat_to_attr(1, &meta);

        assert_eq!(attr.kind, FileType::Directory);
    }

    #[test]
    fn test_read_file_range() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("data.bin");
        fs::write(&file_path, b"hello world").unwrap();

        let data = read_file_range(&file_path, 0, 5).unwrap();
        assert_eq!(&data, b"hello");

        let data = read_file_range(&file_path, 6, 5).unwrap();
        assert_eq!(&data, b"world");

        let data = read_file_range(&file_path, 100, 5).unwrap();
        assert!(data.is_empty());

        let data = read_file_range(&file_path, 0, 100).unwrap();
        assert_eq!(&data, b"hello world");
    }
}
