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

mod fetch;
mod inode;
mod ops;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;

use fuser::{BackingId, Config, INodeNo, MountOption, SessionACL};
use tokio::runtime::Handle;
use tonic::transport::Channel;

use rio_proto::store::store_service_client::StoreServiceClient;

use self::cache::Cache;
use self::inode::{InodeMap, ephemeral_inode};

/// FUSE filesystem that serves `/nix/store` from a local SSD cache
/// backed by remote `StoreService` gRPC.
pub struct NixStoreFs {
    /// Inode map (interior mutability for `&self` FUSE callbacks).
    /// Rooted at `cache.cache_dir()`, NOT the FUSE mount point — see `new()`.
    inodes: RwLock<InodeMap>,
    /// Next file handle counter.
    next_fh: AtomicU64,
    /// Whether to use passthrough mode.
    passthrough: bool,
    /// Backing state for passthrough file handles.
    backing_state: RwLock<HashMap<u64, (File, BackingId)>>,
    /// Open file handles for non-passthrough read(). Keyed by fh.
    /// Lets read() use pread on a cached handle instead of open+seek+read
    /// per chunk. Removed in release().
    open_files: RwLock<HashMap<u64, File>>,
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
    /// Create a new FUSE filesystem backed by the given local cache.
    ///
    /// `InodeMap` is rooted at `cache.cache_dir()`, NOT the FUSE mount point:
    /// all `real_path(ino)` lookups resolve to paths inside the local cache
    /// directory. FUSE callbacks (`getattr`, `lookup`, `readdir`) then do
    /// `stat()`/`read_dir()` on those cache paths — normal filesystem ops.
    ///
    /// Previously the InodeMap was rooted at the mount point, so `getattr(ROOT)`
    /// did `stat(mount_point)` — which re-enters FUSE via `getattr(ROOT)`,
    /// recursing until all FUSE threads deadlocked. This bug was latent (unit
    /// tests never actually mounted) until the VM milestone test called
    /// `overlay::setup_overlay`, which stats the FUSE mount as the overlay lower.
    pub fn new(
        cache: Cache,
        store_client: StoreServiceClient<Channel>,
        runtime: Handle,
        passthrough: bool,
    ) -> Self {
        let inodes = InodeMap::new(cache.cache_dir().to_path_buf());
        Self {
            inodes: RwLock::new(inodes),
            next_fh: AtomicU64::new(1),
            passthrough,
            backing_state: RwLock::new(HashMap::new()),
            open_files: RwLock::new(HashMap::new()),
            passthrough_failures: AtomicU64::new(0),
            cache,
            store_client,
            runtime,
        }
    }

    // --- Lock-poison recovery helpers -----------------------------------
    // All FUSE callbacks take &self and need interior mutability. Poison
    // recovery (via into_inner) is safe here: the only state mutated under
    // these locks is pure data (inode maps, open file handles). A poisoned
    // lock means a panic mid-mutation left a partial entry — worst case is
    // a stale inode or a leaked fd until the next forget/release.

    fn inodes_read(&self) -> std::sync::RwLockReadGuard<'_, InodeMap> {
        self.inodes.read().unwrap_or_else(|e| {
            tracing::error!("inodes lock poisoned (read), recovering");
            e.into_inner()
        })
    }

    fn inodes_write(&self) -> std::sync::RwLockWriteGuard<'_, InodeMap> {
        self.inodes.write().unwrap_or_else(|e| {
            tracing::error!("inodes lock poisoned (write), recovering");
            e.into_inner()
        })
    }

    fn open_files_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<u64, File>> {
        self.open_files.read().unwrap_or_else(|e| e.into_inner())
    }

    fn open_files_write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<u64, File>> {
        self.open_files.write().unwrap_or_else(|e| e.into_inner())
    }

    fn backing_state_write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<u64, (File, BackingId)>> {
        self.backing_state.write().unwrap_or_else(|e| {
            tracing::error!("backing_state lock poisoned, recovering");
            e.into_inner()
        })
    }

    /// Get an existing persistent inode, or compute an ephemeral one.
    ///
    /// For readdir: we don't want to allocate persistent InodeMap entries
    /// for every directory entry (the kernel never forgets them — monotonic
    /// growth). If the path was already lookup()'d, reuse its real inode.
    /// Otherwise, compute a deterministic hash-based ephemeral inode.
    /// Never inserts into the InodeMap.
    fn get_or_ephemeral_inode(&self, path: &Path) -> u64 {
        let map = self.inodes_read();
        map.get_existing(path)
            .unwrap_or_else(|| ephemeral_inode(path))
    }

    /// Variant of get_or_create_inode that also increments the nlookup
    /// refcount. Call this exactly once per successful reply.entry() in the
    /// lookup path. readdir should use get_or_ephemeral_inode (no persistent
    /// allocation; the kernel does not forget readdir-returned inodes).
    fn get_or_create_inode_for_lookup(&self, path: PathBuf) -> u64 {
        let mut map = self.inodes_write();
        let ino = map.get_or_create(path);
        map.increment_lookup(ino);
        ino
    }

    fn real_path(&self, ino: u64) -> Option<PathBuf> {
        self.inodes_read().real_path(ino)
    }

    /// Extract the store path basename from an inode.
    ///
    /// For a path like `/nix/store/abc-hello`, the first component after
    /// the mount point is the store path basename.
    fn store_basename_for_inode(&self, ino: u64) -> Option<String> {
        let path = self.real_path(ino)?;
        let mount = self.inodes_read().real_path(INodeNo::ROOT.0)?;

        let relative = path.strip_prefix(&mount).ok()?;
        let first_component = relative.components().next()?;
        Some(first_component.as_os_str().to_string_lossy().into_owned())
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
    let fs = NixStoreFs::new(cache, store_client, runtime, passthrough);

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
