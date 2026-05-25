//! `fuser::Filesystem` impl for the castore-FUSE (ADR-022 §2.4–§2.6).
//!
//! Mounted at `/var/rio/castore/{build_id}` (by rio-mountd; the builder
//! serves the handed-off `/dev/fuse` fd). The tree is immutable for the
//! mount's lifetime, so every reply carries `ttl = Duration::MAX` and
//! `init` advertises every cache-enable flag the kernel offers — the
//! dcache and icache absorb all repeat metadata traffic, and a cache
//! hit on `open()` replies passthrough so reads bypass FUSE entirely.
//!
//! The plan put this impl in `mod.rs`, but `castore_fuse/mod.rs`
//! already holds the submodule declarations for the mountd half of the
//! stack — the `Filesystem` impl lives here instead.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};

use super::open::{OpenCase, OpenPath};
use super::tree::{InoMap, Node};
use crate::IgnorePoison;

/// Infinite TTL: the tree is immutable for the mount's lifetime, so the
/// kernel never needs to revalidate a dentry or attr. The kernel
/// saturates the value via `timespec64_to_jiffies` → `MAX_SEC_IN_JIFFIES`
/// — no overflow.
const TTL: Duration = Duration::MAX;

/// Per-open-file-handle state, for `release()`.
struct OpenEntry {
    /// The `file_digest` whose shared backing registration this fh
    /// holds a reference on, if the open replied passthrough.
    /// `release()` decrements the [`BackingTable`] refcount and sends
    /// `BackingClose{id}` when it reaches zero.
    backing_digest: Option<[u8; 32]>,
    /// The opened backing-cache file, kept only when the open replied
    /// `FOPEN_KEEP_CACHE` (passthrough disabled or not negotiated) so
    /// `read()` can serve from it. Passthrough opens drop their fd —
    /// the kernel holds its own reference via the backing registration.
    file: Option<File>,
}

/// One live kernel backing registration per distinct file content,
/// refcounted across open file handles.
///
/// The kernel rejects a passthrough open of an inode that already has a
/// *different* `fuse_backing` attached (`fuse_inode_uncached_io_start`'s
/// `fi->fb != fb` check → `EBUSY` → user-visible `EIO`), and the same
/// content can be open many times concurrently (the loader and the
/// shell both open `busybox`; `make -jN` opens one header from N
/// compilers). Registering one backing id per `file_digest` and reusing
/// it for every open of either inode that resolves to that digest (the
/// exec and non-exec variants share a backing file) keeps the kernel's
/// per-inode backing pointer stable for as long as any open is live —
/// the I-061 lesson, restated by upstream fuser's `BackingId` docs
/// ("you must reuse backing IDs for the same inode for all open file
/// handles").
#[derive(Default)]
struct BackingTable {
    /// `file_digest` → (mountd-brokered backing id, open-handle count).
    by_digest: HashMap<[u8; 32], (u32, u32)>,
}

impl BackingTable {
    /// Reuse the existing backing id for `digest` (bumping its
    /// open-handle count) or mint a new one via `mint` and register it
    /// with a count of one. A failed mint leaves no entry, so the next
    /// open retries the registration instead of reusing a phantom id.
    ///
    /// The caller MUST hold the table's lock across this call —
    /// including the `mint` round-trip — so two racing first-opens of
    /// one digest serialize into mint-then-reuse instead of both
    /// registering (re-registering while the first registration is
    /// still attached to the inode is the kernel-EBUSY path).
    fn acquire<E>(
        &mut self,
        digest: [u8; 32],
        mint: impl FnOnce() -> Result<u32, E>,
    ) -> Result<u32, E> {
        match self.by_digest.get_mut(&digest) {
            Some((id, refcount)) => {
                *refcount += 1;
                Ok(*id)
            }
            None => {
                let id = mint()?;
                self.by_digest.insert(digest, (id, 1));
                Ok(id)
            }
        }
    }

    /// Drop one open's reference on `digest`'s backing registration.
    /// Returns `Some(id)` when the last reference is released — the
    /// caller must then close the id via mountd. Returns `None` while
    /// other opens still hold it, or if the digest was never
    /// registered (a release for an open whose registration failed).
    fn release(&mut self, digest: &[u8; 32]) -> Option<u32> {
        let (id, refcount) = self.by_digest.get_mut(digest)?;
        *refcount -= 1;
        if *refcount == 0 {
            let id = *id;
            self.by_digest.remove(digest);
            Some(id)
        } else {
            None
        }
    }
}

/// The castore FUSE filesystem: an immutable [`InoMap`] for metadata
/// plus an [`OpenPath`] for data.
pub struct CastoreFs {
    tree: InoMap,
    open_path: OpenPath,
    /// fh → open state. Bounded by `max_backing_ids`.
    opens: RwLock<HashMap<u64, OpenEntry>>,
    /// Live backing registrations. Held across the `BackingOpen`
    /// round-trip so two concurrent first-opens of one digest cannot
    /// both register (the round-trip is a sub-millisecond UDS ioctl
    /// broker, the same cost the old module paid for the in-process
    /// ioctl under its `backing_state` write lock).
    backings: Mutex<BackingTable>,
    next_fh: AtomicU64,
    /// Concurrent-open ceiling. Not an LRU: backing ids are released
    /// only on `release(fh)`, and the kernel holds its own reference on
    /// the backing file for the open's lifetime, so an eviction could
    /// not "fall back to FUSE read" anyway. On overflow `open()`
    /// returns `EMFILE` (build-fatal, surfaced in metrics).
    max_backing_ids: usize,
    /// `false` once `init()` fails to negotiate `FUSE_PASSTHROUGH`
    /// (kernel < 6.9) or when the `RIO_DISABLE_PASSTHROUGH` escape
    /// hatch is set. Plain bool: written only under `init`'s
    /// `&mut self`, which happens-before every other callback.
    passthrough: bool,
}

impl CastoreFs {
    pub fn new(
        tree: InoMap,
        open_path: OpenPath,
        max_backing_ids: usize,
        passthrough: bool,
    ) -> Self {
        Self {
            tree,
            open_path,
            opens: RwLock::new(HashMap::new()),
            backings: Mutex::new(BackingTable::default()),
            next_fh: AtomicU64::new(1),
            max_backing_ids,
            passthrough,
        }
    }

    /// The inode table, for assertions in tests and for the P0560
    /// mount sequence's "is the tree non-empty" check.
    pub fn tree(&self) -> &InoMap {
        &self.tree
    }

    fn count_upcall(op: &'static str) {
        metrics::counter!("rio_builder_castore_fuse_upcalls_total", "op" => op).increment(1);
    }
}

/// Raise `RLIMIT_NOFILE` to 65536 (or the hard limit, whichever is
/// lower). Every concurrently-open passthrough file holds one fd in
/// this process between `File::open` and the `BackingOpen` round-trip,
/// and the keep-cache fallback holds one for the open's whole lifetime
/// — the default soft limit of 1024 is not enough for a wide `make -j`.
/// Best-effort: failure is logged, not fatal (the `EMFILE` ceiling in
/// `open()` is the backstop).
pub fn raise_nofile_limit() {
    const WANT: u64 = 65536;
    match nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE) {
        Ok((soft, hard)) => {
            let target = WANT.min(hard);
            if soft >= target {
                return;
            }
            match nix::sys::resource::setrlimit(
                nix::sys::resource::Resource::RLIMIT_NOFILE,
                target,
                hard,
            ) {
                Ok(()) => tracing::debug!(soft = target, hard, "raised RLIMIT_NOFILE"),
                Err(e) => tracing::warn!(error = %e, "setrlimit(RLIMIT_NOFILE) failed"),
            }
        }
        Err(e) => tracing::warn!(error = %e, "getrlimit(RLIMIT_NOFILE) failed"),
    }
}

/// The negative-dentry reply: `nodeid = 0` with a valid (infinite)
/// timeout. The kernel's `fuse_lookup_name` treats this as "same as
/// `-ENOENT`, but with valid timeout" — the negative result is cached
/// at the FUSE layer and the kernel never re-asks for this name. A
/// plain `reply.error(ENOENT)` would be re-asked on every probe.
fn negative_entry(reply: ReplyEntry) {
    let zero = FileAttr {
        ino: INodeNo(0),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 0,
        flags: 0,
    };
    reply.entry(&TTL, &zero, Generation(0));
}

impl Filesystem for CastoreFs {
    // r[impl builder.fs.castore-cache-config]
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        raise_nofile_limit();

        // The tree is immutable: enable every metadata cache the kernel
        // offers. READDIRPLUS pre-populates the dcache so a stat of
        // every entry after `ls` is 0-upcall; PARALLEL_DIROPS removes
        // the per-inode mutex on concurrent lookups; CACHE_SYMLINKS
        // makes readlink once-ever per target. Degrade with a warning
        // if the kernel predates any of them — correctness is
        // unaffected, only upcall volume.
        if let Err(unsupported) = config.add_capabilities(
            InitFlags::FUSE_DO_READDIRPLUS
                | InitFlags::FUSE_READDIRPLUS_AUTO
                | InitFlags::FUSE_PARALLEL_DIROPS
                | InitFlags::FUSE_CACHE_SYMLINKS,
        ) {
            tracing::warn!(
                ?unsupported,
                "kernel lacks some castore-FUSE cache capabilities; metadata upcall volume \
                 will be higher"
            );
        }

        if self.passthrough {
            // BOTH calls are required: `add_capabilities` puts
            // FUSE_PASSTHROUGH in the INIT reply flags (without it
            // `fc->passthrough` stays 0 and BACKING_OPEN is
            // unconditionally EPERM); `set_max_stack_depth` populates
            // the reply's depth field, which the kernel only honors
            // when the flag is also set. Depth 1 → the FUSE superblock
            // gets s_stack_depth=1 → overlayfs (depth 2) can stack on
            // top, and BACKING_OPEN accepts depth-0 backing files.
            if let Err(unsupported) = config.add_capabilities(InitFlags::FUSE_PASSTHROUGH) {
                tracing::warn!(
                    ?unsupported,
                    "kernel lacks FUSE_PASSTHROUGH (< 6.9?); falling back to userspace reads"
                );
                self.passthrough = false;
            } else if let Err(max) = config.set_max_stack_depth(1) {
                tracing::warn!(
                    max,
                    "kernel rejected max_stack_depth=1; falling back to userspace reads"
                );
                self.passthrough = false;
            }
        }
        tracing::info!(
            passthrough = self.passthrough,
            inodes = self.tree.len(),
            "castore-FUSE initialized"
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        Self::count_upcall("lookup");
        match self.tree.lookup(parent.0, name.as_bytes()) {
            Some((_ino, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            // Outside the prefetched DAG → negative dentry with
            // infinite TTL. This is the declared-input allowlist: a
            // build cannot read store paths outside its closure, and
            // the daemon's `.lock`/`.chroot` probes get a cached
            // negative instead of one upcall each per probe.
            None => negative_entry(reply),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        Self::count_upcall("getattr");
        match self.tree.attr(ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        Self::count_upcall("readlink");
        match self.tree.readlink(ino.0) {
            Some(target) => reply.data(target),
            None => reply.error(Errno::EINVAL),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // No per-handle state for directories (readdir reads the
        // immutable tree), so fh=0. CACHE_DIR + KEEP_CACHE let the
        // kernel cache the dirent pages: the second readdir of the same
        // directory is 0-upcall.
        if ino.0 == INodeNo::ROOT.0 || matches!(self.tree.node(ino.0), Some(Node::Dir { .. })) {
            reply.opened(
                FileHandle(0),
                FopenFlags::FOPEN_CACHE_DIR | FopenFlags::FOPEN_KEEP_CACHE,
            );
        } else {
            reply.error(Errno::ENOTDIR);
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        Self::count_upcall("readdir");
        let Some(children) = self.tree.children(ino.0) else {
            reply.error(Errno::ENOTDIR);
            return;
        };
        // Offsets are 1-based positions: `.`=1, `..`=2, children start
        // at 3. The kernel resumes with the offset of the last entry it
        // accepted, so each entry is added with its own position.
        if offset < 1 && reply.add(ino, 1, FileType::Directory, ".") {
            reply.ok();
            return;
        }
        // `..` of a content-addressed directory is ambiguous (the same
        // dir_digest can appear under many parents); the kernel
        // resolves `..` from the dcache and never asks FUSE, so the
        // inode here is display-only (`ls -ai`). ROOT is a safe
        // placeholder.
        if offset < 2 && reply.add(INodeNo::ROOT, 2, FileType::Directory, "..") {
            reply.ok();
            return;
        }
        for (i, (child_ino, kind, name)) in children.iter().enumerate() {
            let pos = i as u64 + 3;
            if pos <= offset {
                continue;
            }
            if reply.add(INodeNo(*child_ino), pos, *kind, OsStr::from_bytes(name)) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        Self::count_upcall("readdir");
        let Some(children) = self.tree.children(ino.0) else {
            reply.error(Errno::ENOTDIR);
            return;
        };
        let Some(self_attr) = self.tree.attr(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        // The kernel skips dentry instantiation for `.`/`..` in a
        // READDIRPLUS reply (fuse_direntplus_link ignores dot names),
        // so their attrs are display-only.
        if offset < 1 && reply.add(ino, 1, ".", &TTL, &self_attr, Generation(0)) {
            reply.ok();
            return;
        }
        if offset < 2 {
            let root_attr = self
                .tree
                .attr(INodeNo::ROOT.0)
                .expect("root attr is always synthesizable");
            if reply.add(INodeNo::ROOT, 2, "..", &TTL, &root_attr, Generation(0)) {
                reply.ok();
                return;
            }
        }
        for (i, (child_ino, _kind, name)) in children.iter().enumerate() {
            let pos = i as u64 + 3;
            if pos <= offset {
                continue;
            }
            let Some(attr) = self.tree.attr(*child_ino) else {
                continue;
            };
            // Each entry carries its full attr with an infinite TTL —
            // this is what pre-populates the dcache so the find/stat
            // pass after `ls` costs zero further upcalls.
            if reply.add(
                INodeNo(*child_ino),
                pos,
                OsStr::from_bytes(name),
                &TTL,
                &attr,
                Generation(0),
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        Self::count_upcall("open");
        let started = std::time::Instant::now();

        let Some(node) = self.tree.node(ino.0) else {
            let errno = if ino.0 == INodeNo::ROOT.0 {
                Errno::EISDIR
            } else {
                Errno::ENOENT
            };
            reply.error(errno);
            return;
        };
        let (file_digest, size) = match node {
            Node::File {
                file_digest, size, ..
            } => (*file_digest, *size),
            Node::Dir { .. } => {
                reply.error(Errno::EISDIR);
                return;
            }
            // The kernel resolves symlinks in the VFS and opens the
            // target; an open() on a symlink inode only reaches the
            // filesystem with O_PATH|O_NOFOLLOW, which needs no data.
            Node::Symlink { .. } => {
                reply.error(Errno::ELOOP);
                return;
            }
        };

        // Per-build concurrent-open ceiling. Checked before the fetch
        // so a build leaking fds fails fast instead of after paying a
        // fetch per leaked open.
        if self.opens.read().ignore_poison().len() >= self.max_backing_ids {
            tracing::error!(
                max = self.max_backing_ids,
                "castore-fuse: concurrent-open ceiling reached"
            );
            metrics::counter!("rio_builder_castore_fuse_eio_total").increment(1);
            reply.error(Errno::EMFILE);
            return;
        }

        // Ensure the backing-cache entry exists (fetch + promote on
        // miss), then open it read-only.
        let case = match self.open_path.ensure_backing(&file_digest, size) {
            Ok(case) => case,
            Err(errno) => {
                metrics::counter!("rio_builder_castore_fuse_eio_total").increment(1);
                reply.error(errno);
                return;
            }
        };
        let backing = self.open_path.cache_path(&file_digest);
        let file = match File::open(&backing) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(
                    path = %backing.display(),
                    error = %e,
                    "castore-fuse: backing cache entry vanished after ensure_backing"
                );
                metrics::counter!("rio_builder_castore_fuse_eio_total").increment(1);
                reply.error(Errno::EIO);
                return;
            }
        };

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let mode = if self.passthrough {
            // One backing registration per file_digest, shared across
            // every concurrent open of that content. The lock is held
            // across the BackingOpen round-trip so two first-opens of
            // one digest cannot both register — re-registering while
            // the first registration is still attached to the inode is
            // the kernel-EBUSY path (I-061). The round-trip is a
            // sub-millisecond UDS-brokered ioctl, the same cost the old
            // module paid for the in-process ioctl under its
            // backing_state write lock.
            let registered = self
                .backings
                .lock()
                .ignore_poison()
                .acquire(file_digest, || {
                    self.open_path
                        .mountd()
                        .backing_open(file.as_raw_fd(), self.open_path.mountd_timeout())
                });
            match registered {
                Ok(backing_id) => {
                    // Register the open BEFORE replying: the kernel may
                    // send release(fh) the instant the reply lands, and
                    // a release that finds no entry would leak the
                    // backing refcount.
                    self.opens.write().ignore_poison().insert(
                        fh,
                        OpenEntry {
                            backing_digest: Some(file_digest),
                            file: None,
                        },
                    );
                    // SAFETY: `backing_id` was minted by rio-mountd's
                    // FUSE_DEV_IOC_BACKING_OPEN on its kept dup of this
                    // session's /dev/fuse fd — same fuse_conn, so the
                    // id is valid for this connection and stays
                    // registered until our release() sends
                    // BackingClose. into_raw() immediately defuses the
                    // wrapper's Drop so fuser never issues the
                    // (EPERM-for-this-process) close ioctl — mountd
                    // owns the id's lifetime.
                    let wrapped = unsafe { reply.wrap_backing(backing_id) };
                    reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &wrapped);
                    let _ = wrapped.into_raw();
                    // The kernel took its own reference on the backing
                    // file at BACKING_OPEN time; our fd is no longer
                    // needed.
                    drop(file);
                    "passthrough"
                }
                Err(e) => {
                    // Passthrough brokering failed (mountd restarting,
                    // backing-id cap). Degrade this one open to
                    // userspace reads rather than failing the build.
                    tracing::warn!(
                        error = %e,
                        "castore-fuse: BackingOpen failed; serving this open without passthrough"
                    );
                    self.opens.write().ignore_poison().insert(
                        fh,
                        OpenEntry {
                            backing_digest: None,
                            file: Some(file),
                        },
                    );
                    reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
                    "keep_cache"
                }
            }
        } else {
            self.opens.write().ignore_poison().insert(
                fh,
                OpenEntry {
                    backing_digest: None,
                    file: Some(file),
                },
            );
            reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
            "keep_cache"
        };

        metrics::counter!("rio_builder_castore_fuse_open_mode_total", "mode" => mode).increment(1);
        metrics::counter!("rio_builder_castore_fuse_open_case_total", "case" => case.label())
            .increment(1);
        metrics::histogram!(
            "rio_builder_castore_fuse_open_seconds",
            "hit" => if case == OpenCase::Hit { "node_ssd" } else { "remote" },
        )
        .record(started.elapsed().as_secs_f64());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        Self::count_upcall("read");
        // Reachable only when an open replied FOPEN_KEEP_CACHE: the
        // RIO_DISABLE_PASSTHROUGH escape hatch, a kernel without
        // FUSE_PASSTHROUGH, or a per-open BackingOpen failure.
        // Passthrough opens never upcall read. P0575's streaming window
        // adds the serve-from-partial path here.
        let files = self.opens.read().ignore_poison();
        let Some(file) = files.get(&fh.0).and_then(|e| e.file.as_ref()) else {
            drop(files);
            tracing::error!(
                ino = ino.0,
                fh = fh.0,
                "castore-fuse: read upcall for a passthrough or unknown fh — \
                 passthrough regression?"
            );
            metrics::counter!("rio_builder_castore_fuse_eio_total").increment(1);
            reply.error(Errno::EIO);
            return;
        };
        let mut buf = vec![0u8; size as usize];
        match read_at_full(file, &mut buf, offset) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(e) => {
                tracing::warn!(ino = ino.0, offset, size, error = %e, "castore-fuse: read failed");
                metrics::counter!("rio_builder_castore_fuse_eio_total").increment(1);
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
        reply: ReplyEmpty,
    ) {
        let entry = self.opens.write().ignore_poison().remove(&fh.0);
        if let Some(OpenEntry {
            backing_digest: Some(digest),
            ..
        }) = entry
        {
            // Drop this fh's reference on the shared backing
            // registration; close it via mountd only when the last
            // open of this content releases. By the time this callback
            // runs the kernel has already detached `fi->fb` for this
            // open, so a future re-registration cannot EBUSY.
            let last = self.backings.lock().ignore_poison().release(&digest);
            if let Some(id) = last {
                // Best-effort: a failed BackingClose leaks one IDR slot
                // in the kernel until the connection dies. The
                // connection is per-build, so the leak is bounded by
                // the build's lifetime.
                if let Err(e) = self
                    .open_path
                    .mountd()
                    .backing_close(id, self.open_path.mountd_timeout())
                {
                    tracing::warn!(
                        backing_id = id,
                        error = %e,
                        "castore-fuse: BackingClose failed"
                    );
                }
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // Read-only content-addressed tree: there is no meaningful
        // block count. 255 = NAME_MAX, 512 = block size.
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }

    // ── xattr stubs ───────────────────────────────────────────────────
    // NAR-derived content has no xattrs and the FS is read-only — these
    // are the truthful answers, not placeholders. overlayfs probes
    // `user.overlay.*`/`trusted.overlay.*` on every lower inode; the
    // explicit ENODATA/empty-list replies keep that off the WARN log
    // and out of the ENOSYS path.

    fn getxattr(&self, _: &Request, _: INodeNo, _: &OsStr, _: u32, reply: ReplyXattr) {
        reply.error(Errno::ENODATA);
    }

    // r[impl builder.fs.listxattr-size-branch]
    fn listxattr(&self, _: &Request, _: INodeNo, size: u32, reply: ReplyXattr) {
        // The two branches are NOT interchangeable. size==0 is the
        // size-probe: the caller wants the buffer length, and
        // `reply.size(0)` answers "no xattrs". size>0 wants the actual
        // (empty) name list: `reply.size(0)` there serializes an
        // 8-byte `fuse_getxattr_out` struct as the *data*, which the
        // kernel's `fuse_verify_xattr_list` reads as a corrupt
        // zero-length name and rejects with EIO — this broke
        // `shutil.copy2` from store paths once already.
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}

/// `pread` exactly `buf.len()` bytes at `offset`, stopping early at
/// EOF. Returns the number of bytes read. Stateless (no seek), so
/// concurrent reads on the same fh are safe.
fn read_at_full(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read_at(&mut buf[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree is immutable for the mount's lifetime, so every
    /// `reply.entry`/`reply.attr`/`readdirplus` reply MUST carry an
    /// infinite TTL — that is what lets the dcache absorb every repeat
    /// metadata op. The other half of the cache configuration (the
    /// `init()` capability flags) needs a real kernel handshake and is
    /// asserted by P0560§B's `stat-dcache-absorbed` VM subtest.
    // r[verify builder.fs.castore-cache-config]
    #[test]
    fn every_reply_carries_an_infinite_ttl() {
        assert_eq!(TTL, Duration::MAX);
    }

    /// `read()` (the keep-cache fallback path) serves exact ranges and
    /// truncates at EOF instead of zero-padding.
    #[test]
    fn read_at_full_handles_offsets_and_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"0123456789").unwrap();
        let f = File::open(&path).unwrap();

        let mut buf = [0u8; 4];
        assert_eq!(read_at_full(&f, &mut buf, 0).unwrap(), 4);
        assert_eq!(&buf, b"0123");
        assert_eq!(read_at_full(&f, &mut buf, 6).unwrap(), 4);
        assert_eq!(&buf, b"6789");
        // Past-EOF range: short read, not an error and not padding.
        assert_eq!(read_at_full(&f, &mut buf, 8).unwrap(), 2);
        assert_eq!(&buf[..2], b"89");
        assert_eq!(read_at_full(&f, &mut buf, 100).unwrap(), 0);
    }

    /// Raising the fd limit is idempotent and never panics — it runs
    /// inside `init()` where an error would fail the whole mount.
    #[test]
    fn raise_nofile_limit_is_best_effort() {
        raise_nofile_limit();
        raise_nofile_limit();
    }

    // ── BackingTable: the I-061 EBUSY-avoidance refcounting ──────────
    //
    // The kernel rejects a passthrough open of an inode that already
    // has a *different* fuse_backing attached, so the table must mint
    // exactly one backing id per live file_digest and only return it
    // for closing once the last open releases. The caller holds the
    // table's lock across acquire (mint included), so "two concurrent
    // first-opens" serialize into mint-then-reuse — which is exactly
    // what these sequential calls model.

    /// Two opens of the same content mint exactly one backing id; the
    /// second reuses the first's registration.
    #[test]
    fn backing_table_mints_once_and_reuses_for_concurrent_opens() {
        let mut table = BackingTable::default();
        let digest = [0xAB; 32];
        let mut mints = 0u32;

        let first = table
            .acquire::<()>(digest, || {
                mints += 1;
                Ok(7)
            })
            .expect("first open mints");
        let second = table
            .acquire::<()>(digest, || {
                mints += 1;
                Ok(999)
            })
            .expect("second open reuses");
        assert_eq!(first, 7);
        assert_eq!(second, 7, "the second open must reuse the first's id");
        assert_eq!(mints, 1, "exactly one BackingOpen for two opens");

        // A different digest is an independent registration.
        let other = table.acquire::<()>([0xCD; 32], || Ok(8)).unwrap();
        assert_eq!(other, 8);
    }

    /// N acquires need N releases before the id is returned for
    /// closing; closing early would yank the backing out from under a
    /// still-open file handle.
    #[test]
    fn backing_table_closes_only_on_the_last_release() {
        let mut table = BackingTable::default();
        let digest = [0x11; 32];
        for _ in 0..3 {
            table.acquire::<()>(digest, || Ok(42)).unwrap();
        }
        assert_eq!(table.release(&digest), None, "2 opens still live");
        assert_eq!(table.release(&digest), None, "1 open still live");
        assert_eq!(
            table.release(&digest),
            Some(42),
            "the last release returns the id to close"
        );
        // The registration is gone: the next open mints a fresh id
        // (safe — the kernel detached fi->fb when the last open
        // released) and a spurious extra release is a no-op, not an
        // underflow.
        assert_eq!(table.release(&digest), None);
        let mut minted = false;
        let next = table
            .acquire::<()>(digest, || {
                minted = true;
                Ok(43)
            })
            .unwrap();
        assert_eq!(next, 43);
        assert!(minted, "a re-open after the last release mints fresh");
    }

    /// A failed mint (mountd unreachable, backing-id cap) leaves no
    /// entry — the next open retries the registration instead of
    /// reusing an id that was never issued.
    #[test]
    fn backing_table_failed_mint_leaves_no_entry() {
        let mut table = BackingTable::default();
        let digest = [0x22; 32];
        let err = table
            .acquire(digest, || Err::<u32, &str>("mountd is down"))
            .expect_err("mint failure propagates");
        assert_eq!(err, "mountd is down");
        assert!(
            table.by_digest.is_empty(),
            "a failed mint must not leave a phantom registration"
        );
        assert_eq!(
            table.release(&digest),
            None,
            "releasing a never-registered digest is a no-op"
        );

        // The retry mints for real.
        let id = table.acquire::<()>(digest, || Ok(5)).unwrap();
        assert_eq!(id, 5);
    }
}
