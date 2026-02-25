//! FUSE store daemon for rio-worker.
//!
//! Mounts `/nix/store` via `fuser` 0.17 and serves store paths from a local
//! SSD cache backed by remote `StoreService` gRPC. The FUSE mount is shared
//! across all concurrent builds as the lower layer of per-build overlayfs mounts.
//!
//! Key design points:
//! - `fuser` 0.17 data-path methods are `&self` (interior mutability via `RwLock`)
//! - Passthrough mode: `KernelConfig::set_max_stack_depth(1)` in `init()`
//! - Multi-threaded dispatch (`n_threads > 1`) to mitigate lookup/open bottleneck
//! - Async backing: `tokio::runtime::Handle::block_on` bridges sync FUSE callbacks

pub mod cache;
pub mod lookup;
pub mod read;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use fuser::{
    AccessFlags, BackingId, Config, Errno, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEntry, ReplyOpen, Request, SessionACL,
};
use tokio::runtime::Handle;
use tonic::transport::Channel;

use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types::{GetPathRequest, QueryPathInfoRequest};

use self::cache::Cache;
use self::lookup::{ATTR_TTL, BLOCK_SIZE, stat_to_attr, synthetic_dir_attr};
use self::read::{io_error_to_errno, read_file_range};

/// Bidirectional inode-to-path map, protected by a single lock.
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

/// FUSE filesystem that serves `/nix/store` from a local SSD cache
/// backed by remote `StoreService` gRPC.
pub struct NixStoreFs {
    /// Mount point path (e.g. `/nix/store`).
    #[allow(dead_code)]
    mount_point: PathBuf,
    /// Inode map (interior mutability for `&self` FUSE callbacks).
    inodes: RwLock<InodeMap>,
    /// Next file handle counter.
    next_fh: AtomicU64,
    /// Whether to use passthrough mode.
    passthrough: bool,
    /// Backing state for passthrough file handles.
    backing_state: RwLock<HashMap<u64, (File, BackingId)>>,
    /// Passthrough failure count.
    passthrough_failures: AtomicU64,
    /// LRU cache on local SSD.
    cache: Cache,
    /// gRPC client for remote store.
    store_client: StoreServiceClient<Channel>,
    /// Tokio runtime handle for async-in-sync bridging.
    runtime: Handle,
}

impl NixStoreFs {
    /// Create a new FUSE filesystem.
    pub fn new(
        mount_point: PathBuf,
        cache: Cache,
        store_client: StoreServiceClient<Channel>,
        runtime: Handle,
        passthrough: bool,
    ) -> Self {
        let inodes = InodeMap::new(mount_point.clone());
        Self {
            mount_point,
            inodes: RwLock::new(inodes),
            next_fh: AtomicU64::new(1),
            passthrough,
            backing_state: RwLock::new(HashMap::new()),
            passthrough_failures: AtomicU64::new(0),
            cache,
            store_client,
            runtime,
        }
    }

    fn get_or_create_inode(&self, path: PathBuf) -> u64 {
        // Fast path: read lock
        {
            let map = self.inodes.read().unwrap_or_else(|e| {
                tracing::error!("inodes lock poisoned on read, recovering");
                e.into_inner()
            });
            if let Some(&ino) = map.path_to_inode.get(&path) {
                return ino;
            }
        }
        // Slow path: write lock
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

    /// Extract the store path basename from an inode.
    ///
    /// For a path like `/nix/store/abc-hello`, the first component after
    /// the mount point is the store path basename.
    fn store_basename_for_inode(&self, ino: u64) -> Option<String> {
        let path = self.real_path(ino)?;
        let mount = self
            .inodes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .real_path(INodeNo::ROOT.0)?;

        let relative = path.strip_prefix(&mount).ok()?;
        let first_component = relative.components().next()?;
        Some(first_component.as_os_str().to_string_lossy().into_owned())
    }

    /// Ensure a store path is cached locally, fetching from remote if needed.
    ///
    /// Returns the local filesystem path to the materialized store path.
    fn ensure_cached(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        if let Some(local_path) = self.cache.get_path(store_basename) {
            return Ok(local_path);
        }

        // Check if another thread is already fetching
        if !self.cache.try_start_fetch(store_basename) {
            // Wait briefly then check again (simple spin; production would use condvar)
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Some(local_path) = self.cache.get_path(store_basename) {
                return Ok(local_path);
            }
            return Err(Errno::EAGAIN);
        }

        let result = self.fetch_and_extract(store_basename);
        self.cache.finish_fetch(store_basename);
        result
    }

    /// Fetch a store path's NAR from remote store and extract to local cache.
    fn fetch_and_extract(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        let store_path = format!("/nix/store/{store_basename}");
        let local_path = self.cache.cache_dir().join(store_basename);

        tracing::debug!(store_path = %store_path, "fetching from remote store");

        // Fetch NAR data via gRPC (async bridged to sync)
        let nar_data = self.runtime.block_on(async {
            let mut client = self.store_client.clone();
            let request = GetPathRequest {
                store_path: store_path.clone(),
            };

            let response = client.get_path(request).await.map_err(|e| {
                tracing::warn!(store_path = %store_path, error = %e, "GetPath failed");
                Errno::EIO
            })?;

            let mut stream = response.into_inner();
            let mut nar_bytes = Vec::new();

            while let Some(msg) = stream.message().await.map_err(|e| {
                tracing::warn!(
                    store_path = %store_path,
                    error = %e,
                    "GetPath stream error"
                );
                Errno::EIO
            })? {
                match msg.msg {
                    Some(get_path_response::Msg::NarChunk(chunk)) => {
                        nar_bytes.extend_from_slice(&chunk);
                    }
                    Some(get_path_response::Msg::Info(_)) => {
                        // First message contains metadata; we already have it
                    }
                    None => {}
                }
            }

            Ok::<Vec<u8>, Errno>(nar_bytes)
        })?;

        // Parse and extract NAR to local disk
        let node = rio_nix::nar::parse(&mut io::Cursor::new(&nar_data)).map_err(|e| {
            tracing::warn!(store_path = %store_path, error = %e, "NAR parse failed");
            Errno::EIO
        })?;

        rio_nix::nar::extract_to_path(&node, &local_path).map_err(|e| {
            tracing::warn!(
                store_path = %store_path,
                local_path = %local_path.display(),
                error = %e,
                "NAR extraction failed"
            );
            Errno::EIO
        })?;

        // Record in cache index
        let size = dir_size(&local_path);
        if let Err(e) = self.cache.insert(store_basename, size) {
            tracing::warn!(
                store_path = %store_basename,
                error = %e,
                "failed to record in cache index"
            );
        }

        // Evict old entries if needed (best-effort)
        if let Err(e) = self.cache.evict_if_needed() {
            tracing::warn!(error = %e, "cache eviction failed");
        }

        metrics::counter!("rio_worker_fuse_cache_misses_total").increment(1);
        Ok(local_path)
    }

    /// Check if a store path exists in the remote store.
    fn query_path_exists(&self, store_basename: &str) -> bool {
        let store_path = format!("/nix/store/{store_basename}");
        self.runtime.block_on(async {
            let mut client = self.store_client.clone();
            let request = QueryPathInfoRequest {
                store_path: store_path.clone(),
            };
            client.query_path_info(request).await.is_ok()
        })
    }
}

/// Recursively compute the size of a directory tree.
fn dir_size(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else {
                total += p.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

// We need this import for the match arms on GetPathResponse::Msg.
use rio_proto::types::get_path_response;

impl Filesystem for NixStoreFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> Result<(), io::Error> {
        if self.passthrough {
            match config.set_max_stack_depth(1) {
                Ok(_depth) => tracing::info!("FUSE passthrough enabled (max_stack_depth=1)"),
                Err(max) => {
                    tracing::warn!(
                        max,
                        "kernel rejected max_stack_depth=1, disabling passthrough"
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
                "passthrough open_backing failed for some files"
            );
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.real_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let child_path = parent_path.join(name);

        // Check local cache first
        if let Ok(meta) = child_path.symlink_metadata() {
            let ino = self.get_or_create_inode(child_path);
            let attr = stat_to_attr(ino, &meta);
            reply.entry(&ATTR_TTL, &attr, Generation(0));
            metrics::counter!("rio_worker_fuse_cache_hits_total").increment(1);
            return;
        }

        // For top-level entries (direct children of mount point), check remote store
        if parent.0 == INodeNo::ROOT.0 {
            let name_str = name.to_string_lossy();
            if self.query_path_exists(&name_str) {
                // Path exists remotely; create a synthetic directory entry.
                // The actual content will be fetched on open/read.
                let ino = self.get_or_create_inode(child_path);
                let attr = synthetic_dir_attr(ino);
                reply.entry(&ATTR_TTL, &attr, Generation(0));
                return;
            }
        }

        // ENOENT is normal for probing
        if name.to_string_lossy() != ".Trash" && name.to_string_lossy() != ".Trash-0" {
            tracing::trace!(parent = parent.0, name = ?name, "lookup: not found");
        }
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match path.symlink_metadata() {
            Ok(meta) => {
                let attr = stat_to_attr(ino.0, &meta);
                reply.attr(&ATTR_TTL, &attr);
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(
                        ino = ino.0,
                        path = %path.display(),
                        error = %e,
                        "getattr failed"
                    );
                }
                // If it's a known store path, try ensuring it's cached
                if let Some(basename) = self.store_basename_for_inode(ino.0) {
                    match self.ensure_cached(&basename) {
                        Ok(_) => {
                            if let Ok(meta) = path.symlink_metadata() {
                                let attr = stat_to_attr(ino.0, &meta);
                                reply.attr(&ATTR_TTL, &attr);
                                return;
                            }
                        }
                        Err(errno) => {
                            reply.error(errno);
                            return;
                        }
                    }
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

        // Ensure the containing store path is cached
        if let Some(basename) = self.store_basename_for_inode(ino.0)
            && let Err(errno) = self.ensure_cached(&basename)
        {
            reply.error(errno);
            return;
        }

        match fs::read_link(&path) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(e) => {
                tracing::warn!(
                    ino = ino.0,
                    path = %path.display(),
                    error = %e,
                    "readlink failed"
                );
                reply.error(Errno::EINVAL);
            }
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Ensure the containing store path is cached
        if let Some(basename) = self.store_basename_for_inode(ino.0)
            && let Err(errno) = self.ensure_cached(&basename)
        {
            reply.error(errno);
            return;
        }

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);

        if self.passthrough {
            match File::open(&path) {
                Ok(file) => match reply.open_backing(&file) {
                    Ok(backing_id) => {
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
                                "passthrough open_backing failed, falling back to standard read"
                            );
                        }
                        reply.opened(FileHandle(fh), FopenFlags::empty());
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        ino = ino.0,
                        path = %path.display(),
                        error = %e,
                        "open failed"
                    );
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
                tracing::warn!(
                    ino = ino.0,
                    path = %path.display(),
                    offset,
                    size,
                    error = %e,
                    "read failed"
                );
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
        // Remove BackingId on release to avoid leaking file handles
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

        // Ensure the containing store path is cached
        if ino.0 != INodeNo::ROOT.0
            && let Some(basename) = self.store_basename_for_inode(ino.0)
            && let Err(errno) = self.ensure_cached(&basename)
        {
            reply.error(errno);
            return;
        }

        let entries = match fs::read_dir(&dir_path) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    ino = ino.0,
                    path = %dir_path.display(),
                    error = %e,
                    "readdir failed"
                );
                reply.error(Errno::EIO);
                return;
            }
        };

        let mut all_entries: Vec<(u64, FileType, String)> = Vec::new();
        all_entries.push((ino.0, FileType::Directory, ".".to_string()));
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
                    tracing::warn!(
                        ino = ino.0,
                        error = %e,
                        "skipping unreadable dir entry"
                    );
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
                    tracing::warn!(
                        path = %child_path.display(),
                        error = %e,
                        "skipping entry with unknown file type"
                    );
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

/// Build the FUSE mount configuration.
pub fn make_fuse_config(n_threads: u32) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("rio-worker".to_string()),
        MountOption::AutoUnmount,
    ];
    config.acl = SessionACL::All; // allow_other
    config.n_threads = Some(n_threads as usize);
    config
}

/// Mount the FUSE filesystem in a background thread.
///
/// Returns the `BackgroundSession` handle. Dropping it unmounts the filesystem.
pub fn mount_fuse_background(
    mount_point: &Path,
    cache: Cache,
    store_client: StoreServiceClient<Channel>,
    runtime: Handle,
    passthrough: bool,
    n_threads: u32,
) -> anyhow::Result<fuser::BackgroundSession> {
    let fs = NixStoreFs::new(
        mount_point.to_path_buf(),
        cache,
        store_client,
        runtime,
        passthrough,
    );

    let config = make_fuse_config(n_threads);
    let session = fuser::spawn_mount2(fs, mount_point, &config)?;

    tracing::info!(
        mount_point = %mount_point.display(),
        passthrough,
        n_threads,
        "FUSE store mounted in background"
    );

    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inode_map_new() {
        let root = PathBuf::from("/nix/store");
        let map = InodeMap::new(root.clone());

        assert_eq!(map.real_path(INodeNo::ROOT.0), Some(root));
        assert!(map.real_path(2).is_none());
    }

    #[test]
    fn test_inode_map_allocation() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);

        let p1 = PathBuf::from("/nix/store/abc-hello");
        let p2 = PathBuf::from("/nix/store/def-world");

        let ino1 = map.get_or_create(p1.clone());
        let ino2 = map.get_or_create(p2);

        assert_ne!(ino1, ino2);
        assert_eq!(map.get_or_create(p1), ino1);
    }

    #[test]
    fn test_inode_map_parent() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);

        let child = PathBuf::from("/nix/store/abc-hello");
        let _ino = map.get_or_create(child.clone());

        assert_eq!(map.parent_inode(&child), INodeNo::ROOT.0);
    }

    #[test]
    fn test_dir_size() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("b.txt"), "world!").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c.txt"), "nested").unwrap();

        let size = dir_size(dir.path());
        // 5 + 6 + 6 = 17 bytes of file content
        assert_eq!(size, 17);
    }
}
