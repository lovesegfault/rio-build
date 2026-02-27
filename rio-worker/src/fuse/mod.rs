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

use self::cache::{Cache, FetchClaim};
use self::lookup::{ATTR_TTL, BLOCK_SIZE, stat_to_attr, synthetic_dir_attr};
use self::read::{io_error_to_errno, read_file_range};

/// Bidirectional inode-to-path map with kernel lookup refcounting, protected
/// by a single lock.
///
/// FUSE protocol: each successful lookup/create reply increments the kernel's
/// refcount for that inode by 1. The kernel sends `forget(ino, n)` when it
/// drops n references. When the count hits zero, the filesystem may free
/// resources associated with the inode.
struct InodeMap {
    inode_to_path: HashMap<u64, PathBuf>,
    path_to_inode: HashMap<PathBuf, u64>,
    /// Kernel lookup refcount per inode. Incremented on each reply.entry(),
    /// decremented (by n) on forget(ino, n). When it hits zero, the inode
    /// entry is removed from all maps (except ROOT, which is never forgotten).
    nlookup: HashMap<u64, u64>,
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
            nlookup: HashMap::new(),
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

    /// Read-only lookup: returns the inode if path is already tracked.
    fn get_existing(&self, path: &Path) -> Option<u64> {
        self.path_to_inode.get(path).copied()
    }

    /// Increment the kernel lookup refcount for an inode. Call this exactly
    /// once per successful reply.entry().
    fn increment_lookup(&mut self, ino: u64) {
        *self.nlookup.entry(ino).or_insert(0) += 1;
    }

    /// Decrement the kernel lookup refcount by `n`. If it reaches zero (and
    /// this is not ROOT), remove the inode from all maps. Returns true iff
    /// the inode was removed.
    fn forget(&mut self, ino: u64, n: u64) -> bool {
        if ino == INodeNo::ROOT.0 {
            return false;
        }
        let Some(count) = self.nlookup.get_mut(&ino) else {
            // Kernel sent forget for an inode we don't track (e.g., created
            // via readdir without a subsequent lookup). Nothing to do.
            return false;
        };
        *count = count.saturating_sub(n);
        if *count == 0 {
            self.nlookup.remove(&ino);
            if let Some(path) = self.inode_to_path.remove(&ino) {
                self.path_to_inode.remove(&path);
            }
            true
        } else {
            false
        }
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

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inode_to_path.len()
    }
}

/// High bit mask for ephemeral inodes. Persistent inodes start at 2 and
/// grow sequentially; setting bit 63 guarantees no collision (would need
/// 2^63 sequential allocations to overlap).
const EPHEMERAL_INODE_BIT: u64 = 1u64 << 63;

/// Compute a deterministic ephemeral inode from a path.
///
/// Used for readdir entries that have not been lookup()'d. FUSE does not
/// require readdir inode numbers to match lookup inodes — they are
/// informational (ls -i). Applications doing hardlink detection use
/// lookup/getattr inodes. We don't implement readdirplus, so there's no
/// attribute caching from readdir.
///
/// If a readdir'd path is later lookup()'d, it gets a real persistent inode
/// via get_or_create_inode_for_lookup — the ephemeral one is simply forgotten.
fn ephemeral_inode(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish() | EPHEMERAL_INODE_BIT
}

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
            passthrough_failures: AtomicU64::new(0),
            cache,
            store_client,
            runtime,
        }
    }

    /// Get an existing persistent inode, or compute an ephemeral one.
    ///
    /// For readdir: we don't want to allocate persistent InodeMap entries
    /// for every directory entry (the kernel never forgets them — monotonic
    /// growth). If the path was already lookup()'d, reuse its real inode.
    /// Otherwise, compute a deterministic hash-based ephemeral inode.
    /// Never inserts into the InodeMap.
    fn get_or_ephemeral_inode(&self, path: &Path) -> u64 {
        let map = self.inodes.read().unwrap_or_else(|e| {
            tracing::error!("inodes lock poisoned on read, recovering");
            e.into_inner()
        });
        map.get_existing(path)
            .unwrap_or_else(|| ephemeral_inode(path))
    }

    /// Variant of get_or_create_inode that also increments the nlookup
    /// refcount. Call this exactly once per successful reply.entry() in the
    /// lookup path. readdir should use get_or_ephemeral_inode (no persistent
    /// allocation; the kernel does not forget readdir-returned inodes).
    fn get_or_create_inode_for_lookup(&self, path: PathBuf) -> u64 {
        let mut map = self.inodes.write().unwrap_or_else(|e| {
            tracing::error!("inodes lock poisoned on write, recovering");
            e.into_inner()
        });
        let ino = map.get_or_create(path);
        map.increment_lookup(ino);
        ino
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
    /// If another thread is already fetching, blocks on a condition variable
    /// until that fetch completes (or a 30s timeout, then returns EAGAIN).
    fn ensure_cached(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        match self.cache.get_path(store_basename) {
            Ok(Some(local_path)) => return Ok(local_path),
            Ok(None) => {} // not cached, fetch below
            Err(e) => {
                tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed");
                return Err(Errno::EIO);
            }
        }

        match self.cache.try_start_fetch(store_basename) {
            FetchClaim::Fetch(_guard) => {
                // We own the fetch. _guard notifies waiters on drop (success,
                // error, or panic) — no explicit cleanup needed.
                self.fetch_and_extract(store_basename)
            }
            FetchClaim::WaitFor(entry) => {
                // Another thread is fetching. Wait for it with a timeout as
                // belt-and-suspenders against a stuck fetcher (the guard's
                // Drop impl fires even on panic, so this timeout is defensive).
                const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                if !entry.wait(WAIT_TIMEOUT) {
                    tracing::warn!(
                        store_path = store_basename,
                        timeout_secs = WAIT_TIMEOUT.as_secs(),
                        "concurrent fetch did not complete within timeout, returning EAGAIN"
                    );
                    return Err(Errno::EAGAIN);
                }
                // Fetch completed — check cache again. Fetcher failure =>
                // Ok(None) => ENOENT so the FUSE caller can retry.
                // Index error => EIO (loud failure, not silent re-fetch).
                match self.cache.get_path(store_basename) {
                    Ok(Some(p)) => Ok(p),
                    Ok(None) => Err(Errno::ENOENT),
                    Err(e) => {
                        tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed after wait");
                        Err(Errno::EIO)
                    }
                }
            }
        }
    }

    /// Fetch a store path's NAR from remote store and extract to local cache.
    fn fetch_and_extract(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        // Increment on miss (entry to this function), not on fetch success:
        // failed fetches (store outage, NAR parse error) are still cache
        // misses. The metric should spike during store outages so dashboards
        // surface the problem; incrementing only on success hides it.
        metrics::counter!("rio_worker_fuse_cache_misses_total").increment(1);
        let store_path = format!("/nix/store/{store_basename}");
        let local_path = self.cache.cache_dir().join(store_basename);

        tracing::debug!(store_path = %store_path, "fetching from remote store");

        // Fetch NAR data via gRPC (async bridged to sync)
        let fetch_start = std::time::Instant::now();
        let nar_data = self.runtime.block_on(async {
            let mut client = self.store_client.clone();
            let request = GetPathRequest {
                store_path: store_path.clone(),
            };

            // Timeout the entire fetch (initial call + stream drain). A stalled
            // store would otherwise block this FUSE thread forever; a few
            // stalls exhaust the FUSE thread pool and freeze the whole mount.
            let fetch_fut = async {
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
                            let new_len =
                                (nar_bytes.len() as u64).saturating_add(chunk.len() as u64);
                            if new_len > rio_common::limits::MAX_NAR_SIZE {
                                tracing::error!(
                                    store_path = %store_path,
                                    size = new_len,
                                    limit = rio_common::limits::MAX_NAR_SIZE,
                                    "NAR exceeds MAX_NAR_SIZE"
                                );
                                return Err(Errno::EFBIG);
                            }
                            nar_bytes.extend_from_slice(&chunk);
                        }
                        Some(get_path_response::Msg::Info(_)) => {
                            // First message contains metadata; we already have it
                        }
                        None => {
                            tracing::warn!(
                                store_path = %store_path,
                                "empty GetPathResponse message (possible proto mismatch)"
                            );
                        }
                    }
                }

                Ok::<Vec<u8>, Errno>(nar_bytes)
            };

            tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, fetch_fut)
                .await
                .map_err(|_| {
                    tracing::error!(
                        store_path = %store_path,
                        timeout = ?rio_common::grpc::GRPC_STREAM_TIMEOUT,
                        "GetPath timed out; FUSE thread would block indefinitely without this"
                    );
                    Errno::EIO
                })?
        })?;
        metrics::histogram!("rio_worker_fuse_fetch_duration_seconds")
            .record(fetch_start.elapsed().as_secs_f64());

        // Parse and extract NAR to local disk
        let node = rio_nix::nar::parse(&mut io::Cursor::new(&nar_data)).map_err(|e| {
            tracing::warn!(store_path = %store_path, error = %e, "NAR parse failed");
            Errno::EIO
        })?;

        // Extract to a temp sibling dir, then atomically rename into place.
        // If extraction fails mid-way (disk full, etc.), the partial tree stays
        // in the tmp dir and is cleaned up on next cache init, rather than
        // being served as a broken store path by subsequent lookups.
        let tmp_path = local_path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
        rio_nix::nar::extract_to_path(&node, &tmp_path).map_err(|e| {
            tracing::warn!(
                store_path = %store_path,
                tmp_path = %tmp_path.display(),
                error = %e,
                "NAR extraction failed"
            );
            // Best-effort: remove the partial tmp tree.
            let _ = std::fs::remove_dir_all(&tmp_path);
            Errno::EIO
        })?;
        std::fs::rename(&tmp_path, &local_path).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_path);
            tracing::error!(
                store_path = %store_path,
                tmp_path = %tmp_path.display(),
                local_path = %local_path.display(),
                error = %e,
                "failed to rename extracted NAR into cache"
            );
            Errno::EIO
        })?;

        // Record in cache index. If this fails, the path is on disk but
        // invisible to contains() — every subsequent access would re-fetch
        // the NAR, creating an infinite re-fetch loop under DB failure.
        // Fail loudly (EIO) so the build surfaces the real problem instead
        // of silently amplifying network traffic.
        let size = dir_size(&local_path);
        if let Err(e) = self.cache.insert(store_basename, size) {
            tracing::error!(
                store_path = %store_basename,
                local_path = %local_path.display(),
                error = %e,
                "failed to record in cache index; path on disk but untracked"
            );
            return Err(Errno::EIO);
        }

        // Evict old entries if needed (best-effort)
        if let Err(e) = self.cache.evict_if_needed() {
            tracing::warn!(error = %e, "cache eviction failed");
        }

        Ok(local_path)
    }

    /// Check if a store path exists in the remote store.
    ///
    /// Returns `Ok(true)` if found, `Ok(false)` if NOT_FOUND, `Err(EIO)` on
    /// transport/server errors. Previously all errors mapped to `false`,
    /// which made transient store outages look like missing paths.
    fn query_path_exists(&self, store_basename: &str) -> Result<bool, Errno> {
        let store_path = format!("/nix/store/{store_basename}");
        self.runtime.block_on(async {
            let mut client = self.store_client.clone();
            let request = QueryPathInfoRequest {
                store_path: store_path.clone(),
            };
            // Timeout: QueryPathInfo is called from lookup() for every
            // top-level store path miss. A stalled store would block this
            // FUSE thread forever; lookup is the most frequent FUSE op.
            match tokio::time::timeout(
                rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                client.query_path_info(request),
            )
            .await
            {
                Ok(Ok(_)) => Ok(true),
                Ok(Err(status)) if status.code() == tonic::Code::NotFound => Ok(false),
                Ok(Err(status)) => {
                    tracing::warn!(
                        store_path = %store_path,
                        code = ?status.code(),
                        error = %status.message(),
                        "QueryPathInfo failed with non-NOT_FOUND error"
                    );
                    Err(Errno::EIO)
                }
                Err(_) => {
                    tracing::error!(
                        store_path = %store_path,
                        timeout = ?rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                        "QueryPathInfo timed out"
                    );
                    Err(Errno::EIO)
                }
            }
        })
    }
}

/// Recursively compute the size of a directory tree.
///
/// Returns 0 and logs on I/O error. A silent 0 on error means the cache
/// index records size=0 for a large NAR — eviction never selects it (it
/// "takes no space"), and the cache can fill past its limit. The warn!
/// makes this visible.
fn dir_size(path: &Path) -> u64 {
    match dir_size_inner(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "dir_size failed; recording 0 (cache accounting will drift)"
            );
            0
        }
    }
}

fn dir_size_inner(path: &Path) -> io::Result<u64> {
    let meta = path.symlink_metadata()?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        // symlink, fifo, etc. — 0 contribution
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        total += dir_size_inner(&entry?.path())?;
    }
    Ok(total)
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

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let mut map = self.inodes.write().unwrap_or_else(|e| {
            tracing::error!("inodes lock poisoned on forget, recovering");
            e.into_inner()
        });
        if map.forget(ino.0, nlookup) {
            tracing::trace!(ino = ino.0, "forgot inode");
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.real_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let child_path = parent_path.join(name);

        // Check local cache first. Distinguish NotFound (fall through to
        // remote query) from other I/O errors (EACCES on corrupt perms,
        // EIO on disk failure): treating all errors as cache miss triggers
        // a re-fetch on every lookup, amplifying network + masking root cause.
        match child_path.symlink_metadata() {
            Ok(meta) => {
                let ino = self.get_or_create_inode_for_lookup(child_path);
                let attr = stat_to_attr(ino, &meta);
                reply.entry(&ATTR_TTL, &attr, Generation(0));
                metrics::counter!("rio_worker_fuse_cache_hits_total").increment(1);
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Fall through to remote query below.
            }
            Err(e) => {
                tracing::warn!(
                    path = %child_path.display(),
                    error = %e,
                    "lookup: local symlink_metadata failed with non-ENOENT error"
                );
                reply.error(io_error_to_errno(&e));
                return;
            }
        }

        // For top-level entries (direct children of mount point), check remote store.
        // Skip names that can't be valid store basenames: store paths are
        // `{32-char-nixbase32-hash}-{name}`, so anything shorter than 34 chars
        // or starting with `.` (like `.links`, Nix's hardlink-optimise dir) is
        // not a store path — don't gRPC-query it (would get InvalidArgument →
        // EIO, which cascades to callers like nix-daemon's mkdir .links).
        if parent.0 == INodeNo::ROOT.0 {
            let name_str = name.to_string_lossy();
            if name_str.len() < 34 || name_str.starts_with('.') {
                reply.error(Errno::ENOENT);
                return;
            }
            match self.query_path_exists(&name_str) {
                Ok(true) => {
                    // Path exists remotely; create a synthetic directory entry.
                    // The actual content will be fetched on open/read.
                    let ino = self.get_or_create_inode_for_lookup(child_path);
                    let attr = synthetic_dir_attr(ino);
                    reply.entry(&ATTR_TTL, &attr, Generation(0));
                    return;
                }
                Ok(false) => {
                    // Not found — fall through to ENOENT
                }
                Err(errno) => {
                    // Transport/server error — surface as EIO, don't mask as ENOENT
                    reply.error(errno);
                    return;
                }
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
                reply.error(io_error_to_errno(&e));
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

            let child_ino = self.get_or_ephemeral_inode(&child_path);
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
    fn test_inode_map_forget_removes_when_zero() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);
        let p = PathBuf::from("/nix/store/abc-hello");
        let ino = map.get_or_create(p.clone());
        map.increment_lookup(ino);
        map.increment_lookup(ino);
        // nlookup = 2; forget(1) -> nlookup = 1, not removed
        assert!(!map.forget(ino, 1));
        assert!(map.real_path(ino).is_some());
        // forget(1) -> nlookup = 0, removed
        assert!(map.forget(ino, 1));
        assert!(map.real_path(ino).is_none());
        assert_eq!(map.len(), 1, "only ROOT should remain");
    }

    #[test]
    fn test_inode_map_forget_keeps_when_nonzero() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);
        let ino = map.get_or_create(PathBuf::from("/nix/store/abc"));
        map.increment_lookup(ino);
        map.increment_lookup(ino);
        map.increment_lookup(ino);
        // nlookup = 3; forget(2) -> nlookup = 1, not removed
        assert!(!map.forget(ino, 2));
        assert!(map.real_path(ino).is_some());
        assert_eq!(map.len(), 2, "ROOT + abc");
    }

    #[test]
    fn test_inode_map_forget_never_removes_root() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root.clone());
        map.increment_lookup(INodeNo::ROOT.0);
        // Even if nlookup hits zero, ROOT is never removed.
        assert!(!map.forget(INodeNo::ROOT.0, 100));
        assert_eq!(map.real_path(INodeNo::ROOT.0), Some(root));
    }

    #[test]
    fn test_inode_map_forget_untracked_inode() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);
        // Forget for an inode we never tracked (e.g., an ephemeral readdir
        // inode, or a stale kernel call) must be a no-op, not a panic.
        let ephemeral = ephemeral_inode(Path::new("/nix/store/never-looked-up"));
        assert!(!map.forget(ephemeral, 1));
        assert!(map.real_path(ephemeral).is_none()); // never in the map
    }

    #[test]
    fn test_ephemeral_inode_high_bit_set() {
        let paths = [
            "/nix/store/abc-hello",
            "/nix/store/def-world",
            "/tmp/anything",
        ];
        for p in paths {
            let ino = ephemeral_inode(Path::new(p));
            assert!(
                ino & EPHEMERAL_INODE_BIT != 0,
                "ephemeral inode for {p} must have high bit set: {ino:#x}"
            );
            // And must not be 0 or ROOT (1).
            assert!(ino > 1);
        }
    }

    #[test]
    fn test_ephemeral_inode_deterministic() {
        let p = Path::new("/nix/store/same-path");
        assert_eq!(ephemeral_inode(p), ephemeral_inode(p));
        // Different paths -> (almost certainly) different inodes.
        let other = Path::new("/nix/store/other-path");
        assert_ne!(ephemeral_inode(p), ephemeral_inode(other));
    }

    #[test]
    fn test_inode_map_get_existing() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root.clone());
        // ROOT exists.
        assert_eq!(map.get_existing(&root), Some(INodeNo::ROOT.0));
        // Unknown path does not.
        assert_eq!(map.get_existing(Path::new("/nix/store/nope")), None);
        // After creating, it does.
        let ino = map.get_or_create(PathBuf::from("/nix/store/yes"));
        assert_eq!(map.get_existing(Path::new("/nix/store/yes")), Some(ino));
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
