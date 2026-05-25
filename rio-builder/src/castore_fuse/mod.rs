//! Castore-FUSE lazy `/nix/store` (ADR-022 §2).
//!
//! A read-only view of one build's input closure. Metadata
//! (`lookup`/`getattr`/`readdir`/`readlink`) is answered from an
//! in-heap Directory DAG with infinite cache TTLs; `open()` brokers a
//! passthrough fd from the node-SSD backing cache so warm reads never
//! upcall. Replaces the whole-store-path JIT FUSE in [`crate::fuse`]
//! at the P0560 cutover, which also owns the mount sequence that wires
//! a [`CastoreFs`] into a build's overlay.

pub mod circuit;
pub mod mountd;
pub mod mountd_client;
pub mod mountd_proto;
pub mod open;
mod sweep;
pub mod tree;

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};

use self::open::Opener;
use self::tree::{InoMap, Node, TTL};

/// Count one FUSE upcall. With the §2.4 cache configuration the kernel
/// answers repeats from dcache/icache, so a high rate here means the
/// caches are not absorbing (TTL regression, memory-pressure eviction).
fn upcall(op: &'static str) {
    metrics::counter!("rio_builder_castore_fuse_upcalls_total", "op" => op).increment(1);
}

/// The negative-lookup reply: `nodeid=0` with a valid timeout. The
/// kernel's `fuse_lookup_name` treats this as "same as -ENOENT, but
/// with valid timeout" — the miss is cached at the FUSE layer instead
/// of being re-asked on every probe (configure scripts stat the same
/// missing header hundreds of times).
fn negative_attr() -> FileAttr {
    FileAttr {
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
        blksize: 4096,
        flags: 0,
    }
}

/// One build's castore-FUSE. Constructed after the DAG prefetch and
/// the mountd `Mount{}` handshake; consumed by `Session::from_fd`.
pub struct CastoreFs {
    tree: InoMap,
    opener: Arc<Opener>,
}

impl CastoreFs {
    pub fn new(tree: InoMap, opener: Arc<Opener>) -> Self {
        Self { tree, opener }
    }

    /// The inode table, for callers that need to resolve digests
    /// outside a FUSE callback (e.g. the upload walk's input-reuse
    /// shortcut).
    pub fn tree(&self) -> &InoMap {
        &self.tree
    }
}

impl Filesystem for CastoreFs {
    /// Negotiate the §2.4 cache capabilities and §2.9 passthrough
    /// stacking depth. Failure to negotiate passthrough is fatal — a
    /// castore-FUSE that silently degrades to userspace `read()` for
    /// every input would still pass tests but ship a 10-100× data-path
    /// regression.
    // r[impl builder.fs.castore-cache-config]
    // r[impl builder.fs.passthrough-stack-depth]
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), io::Error> {
        // Best-effort: each of these is a strict improvement when
        // available and correct (just slower) when not.
        if let Err(unsupported) = config.add_capabilities(
            InitFlags::FUSE_DO_READDIRPLUS
                | InitFlags::FUSE_READDIRPLUS_AUTO
                | InitFlags::FUSE_PARALLEL_DIROPS
                | InitFlags::FUSE_CACHE_SYMLINKS,
        ) {
            tracing::warn!(?unsupported, "kernel lacks some FUSE cache capabilities");
        }
        // Required pair: add_capabilities puts FUSE_PASSTHROUGH in the
        // INIT reply (without it `fc->passthrough` stays 0 and every
        // BACKING_OPEN is EPERM); set_max_stack_depth(1) lets overlay
        // stack on top (depth 2 == FILESYSTEM_MAX_STACK_DEPTH) and
        // restricts backing files to non-stacking filesystems.
        config
            .add_capabilities(InitFlags::FUSE_PASSTHROUGH)
            .map_err(|unsup| io::Error::other(format!("kernel lacks {unsup:?}")))?;
        config.set_max_stack_depth(1).map_err(|max| {
            io::Error::other(format!("kernel rejected max_stack_depth=1 (max {max})"))
        })?;
        tracing::info!(
            inodes = self.tree.inode_count(),
            "castore-FUSE init (passthrough, max_stack_depth=1)"
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        upcall("lookup");
        match self.tree.lookup(parent.0, name.as_bytes()) {
            Some((_, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            // Names outside the prefetched DAG are a legitimate ENOENT
            // (the closure is the allowlist), cached forever.
            None => reply.entry(&TTL, &negative_attr(), Generation(0)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        upcall("getattr");
        match self.tree.attr(ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        upcall("readlink");
        match self.tree.node(ino.0) {
            Some(Node::Symlink { target }) => reply.data(target),
            Some(_) => reply.error(Errno::EINVAL),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if ino.0 != INodeNo::ROOT.0 && !matches!(self.tree.node(ino.0), Some(Node::Dir { .. })) {
            reply.error(Errno::ENOTDIR);
            return;
        }
        // FOPEN_CACHE_DIR: the kernel caches the dirent pages, so the
        // second readdir of the same directory is 0-upcall.
        reply.opened(
            FileHandle(0),
            FopenFlags::FOPEN_CACHE_DIR | FopenFlags::FOPEN_KEEP_CACHE,
        );
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        upcall("readdir");
        let Some(entries) = self.tree.readdir(ino.0, offset) else {
            reply.error(Errno::ENOTDIR);
            return;
        };
        for e in entries {
            if reply.add(
                INodeNo(e.ino),
                e.next_offset,
                e.kind,
                OsStr::from_bytes(e.name),
            ) {
                break;
            }
        }
        reply.ok();
    }

    /// `readdirplus` pre-populates the dcache: every entry carries its
    /// attrs with an infinite TTL, so the `stat()` storm that follows
    /// a directory listing (ls -l, find, globbing) is 0-upcall.
    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        upcall("readdir");
        let Some(entries) = self.tree.readdir(ino.0, offset) else {
            reply.error(Errno::ENOTDIR);
            return;
        };
        for e in entries {
            let attr = self.tree.attr(e.ino).unwrap_or_else(negative_attr);
            if reply.add(
                INodeNo(e.ino),
                e.next_offset,
                OsStr::from_bytes(e.name),
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
        upcall("open");
        match self.tree.node(ino.0) {
            Some(Node::File {
                file_digest, size, ..
            }) => self.opener.open(*file_digest, *size, reply),
            Some(Node::Dir { .. }) => reply.error(Errno::EISDIR),
            // The kernel resolves symlinks before open(); reaching here
            // with one means O_NOFOLLOW|O_PATH trickery. Refuse.
            Some(Node::Symlink { .. }) => reply.error(Errno::ELOOP),
            None => {
                if ino.0 == INodeNo::ROOT.0 {
                    reply.error(Errno::EISDIR);
                } else {
                    reply.error(Errno::ENOENT);
                }
            }
        }
    }

    /// Reachable only when passthrough is disabled (escape hatch) or a
    /// backing registration failed — warm reads otherwise go kernel →
    /// backing file with no upcall.
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        upcall("read");
        match self.opener.read(fh.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(Node::File { file_digest, .. }) = self.tree.node(ino.0) {
            self.opener.release(file_digest, fh.0);
        }
        reply.ok();
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        // No xattrs on store paths, ever. ENODATA (not ENOSYS) so the
        // kernel keeps the per-inode "has no xattrs" state instead of
        // disabling xattr support for the whole mount — overlayfs
        // probes `user.overlay.*` on every lower inode and treats a
        // mount-wide ENOSYS as an error.
        reply.error(Errno::ENODATA);
    }

    // r[impl builder.fuse.listxattr-empty]
    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        // size==0 is the "how big a buffer do I need" probe → 0.
        // size>0 wants the actual (empty) list → zero bytes of data.
        // Replying size(0) to a size>0 call emits an 8-byte
        // fuse_getxattr_out struct that fuse_verify_xattr_list rejects
        // → EIO; this broke shutil.copy2 once already.
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    /// `ENOTTY`, not fuser's default `ENOSYS`: overlay copy-up probes
    /// `FS_IOC_GETFLAGS` via `ovl_copy_fileattr` and only `ENOTTY`
    /// means "no fileattr support" there.
    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: fuser::IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: fuser::ReplyIoctl,
    ) {
        reply.error(Errno::ENOTTY);
    }
}
