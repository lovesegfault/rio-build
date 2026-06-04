//! Castore-FUSE lazy `/nix/store` (ADR-022 §2).
//!
//! A read-only view of one build's input closure. Metadata
//! (`lookup`/`getattr`/`readdir`/`readlink`) is answered from an
//! in-heap Directory DAG with infinite cache TTLs; `open()` brokers a
//! passthrough fd from the node-SSD backing cache so warm reads never
//! upcall. The client-side mount/serve sequence lives in [`session`];
//! the executor wires that session in front of each build's overlay as
//! its only lower (the P0560 cutover).

pub mod circuit;
pub mod mountd;
pub mod mountd_client;
pub mod mountd_proto;
pub mod open;
pub mod session;
mod stream;
mod sweep;
#[cfg(test)]
mod testing;
pub mod tree;

/// Re-export the bench/operator cache-reset entrypoint (`rio-mountd
/// evict-cache`); the sweep module itself stays private.
pub use self::sweep::evict_all;

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite,
    ReplyXattr, Request, TimeOrNow, WriteFlags,
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

/// How `listxattr` must answer on a filesystem that has no xattrs,
/// keyed by the kernel's two-phase probe protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XattrListReply {
    /// `size == 0` is the "how big a buffer do I need" probe — answer
    /// with `ReplyXattr::size(0)`.
    SizeProbe,
    /// `size > 0` wants the actual (empty) list — answer with
    /// `ReplyXattr::data(&[])`, i.e. zero payload bytes. Answering this
    /// call with `size(0)` instead emits an 8-byte `fuse_getxattr_out`
    /// struct that the kernel's `fuse_verify_xattr_list` rejects as a
    /// zero-length name → `EIO` to the caller; this broke
    /// `shutil.copy2` once already.
    EmptyData,
}

/// Pick the [`XattrListReply`] for a `listxattr` request asking for
/// `size` bytes of attribute names.
fn empty_xattr_list_reply(size: u32) -> XattrListReply {
    if size == 0 {
        XattrListReply::SizeProbe
    } else {
        XattrListReply::EmptyData
    }
}

/// The read-only gate for `open()` flags: the errno to reply when the
/// requested access mode (or a mutating flag) is incompatible with a
/// read-only filesystem, `None` for a plain read.
///
/// This check is a security boundary, not POSIX hygiene. On a
/// passthrough open the kernel opens the *backing* cache file with the
/// FUSE **caller's** flags under the BACKING_OPEN **broker's**
/// credentials — rio-mountd, root — and performs no DAC check of its
/// own (`fuse_passthrough_open` → `backing_file_open` → `vfs_open`,
/// fs/fuse/passthrough.c + fs/backing-file.c). The mount's
/// `default_permissions` stops build uids from write-opening the 0444
/// cache entries, but not a root process on the node
/// (CAP_DAC_OVERRIDE): without this rejection, root's `open(O_WRONLY)`
/// on a cache-hit file gets a passthrough fd whose `write(2)` lands in
/// `/var/rio/cache/<digest>` — the node-shared backing cache served to
/// every build on the node.
// r[impl builder.fs.open-read-only]
fn write_open_violation(flags: OpenFlags) -> Option<Errno> {
    use nix::libc::{O_ACCMODE, O_APPEND, O_RDONLY, O_TRUNC};
    if (flags.0 & O_ACCMODE) != O_RDONLY || (flags.0 & (O_TRUNC | O_APPEND)) != 0 {
        return Some(Errno::EROFS);
    }
    None
}

/// Deny one write-path FUSE operation: count the upcall and return the
/// errno to reply. Always `EROFS` — the errno POSIX prescribes for a
/// read-only filesystem — never fuser's defaults (`ENOSYS` for most
/// ops, `EPERM` for `symlink`/`link`): `ENOSYS` is not a legal errno
/// for `unlink(2)`/`mkdir(2)` and `EPERM` invites a privileged retry
/// that cannot succeed either.
fn deny_write_op(op: &'static str) -> Errno {
    upcall(op);
    tracing::debug!(op, "write operation denied on read-only castore FUSE");
    Errno::EROFS
}

/// The write-path deny table: one row per FUSE operation that would
/// mutate the filesystem, carrying the exact fuser callback signature
/// (minus `&self`/`_req`/`reply`). `write_path_deny_table!(expander)`
/// re-expands the rows through `expander!`, so the trait impl and the
/// unit test consume one source of truth: [`deny_with_erofs`] turns
/// each row into a handler replying `EROFS`, `deny_table_op_names`
/// (test-only) turns them into the op-name list checked against the
/// POSIX write-op set. A row whose name or signature does not match a
/// real `Filesystem` method is a compile error; a *deleted* row is
/// caught by the test.
macro_rules! write_path_deny_table {
    ($expander:ident) => {
        $expander! {
            setattr(
                _ino: INodeNo,
                _mode: Option<u32>,
                _uid: Option<u32>,
                _gid: Option<u32>,
                _size: Option<u64>,
                _atime: Option<TimeOrNow>,
                _mtime: Option<TimeOrNow>,
                _ctime: Option<SystemTime>,
                _fh: Option<FileHandle>,
                _crtime: Option<SystemTime>,
                _chgtime: Option<SystemTime>,
                _bkuptime: Option<SystemTime>,
                _flags: Option<BsdFileFlags>,
            ) -> ReplyAttr;
            mknod(_parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, _rdev: u32) -> ReplyEntry;
            mkdir(_parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32) -> ReplyEntry;
            unlink(_parent: INodeNo, _name: &OsStr) -> ReplyEmpty;
            rmdir(_parent: INodeNo, _name: &OsStr) -> ReplyEmpty;
            symlink(_parent: INodeNo, _link_name: &OsStr, _target: &Path) -> ReplyEntry;
            rename(
                _parent: INodeNo,
                _name: &OsStr,
                _newparent: INodeNo,
                _newname: &OsStr,
                _flags: RenameFlags,
            ) -> ReplyEmpty;
            link(_ino: INodeNo, _newparent: INodeNo, _newname: &OsStr) -> ReplyEntry;
            create(_parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, _flags: i32) -> ReplyCreate;
            write(
                _ino: INodeNo,
                _fh: FileHandle,
                _offset: u64,
                _data: &[u8],
                _write_flags: WriteFlags,
                _flags: OpenFlags,
                _lock_owner: Option<LockOwner>,
            ) -> ReplyWrite;
            setxattr(_ino: INodeNo, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32) -> ReplyEmpty;
            removexattr(_ino: INodeNo, _name: &OsStr) -> ReplyEmpty;
            fallocate(_ino: INodeNo, _fh: FileHandle, _offset: u64, _length: u64, _mode: i32) -> ReplyEmpty;
        }
    };
}

/// [`write_path_deny_table!`] expander: each row becomes a
/// [`Filesystem`] method replying `EROFS` via [`deny_write_op`].
macro_rules! deny_with_erofs {
    ($( $op:ident($($arg:ident: $ty:ty),* $(,)?) -> $reply:ty; )*) => {
        $(
            fn $op(&self, _req: &Request, $($arg: $ty,)* reply: $reply) {
                reply.error(deny_write_op(stringify!($op)));
            }
        )*
    };
}

/// [`write_path_deny_table!`] expander for the unit test: the rows' op
/// names, in declaration order.
#[cfg(test)]
macro_rules! deny_table_op_names {
    ($( $op:ident($($arg:ident: $ty:ty),* $(,)?) -> $reply:ty; )*) => {
        &[$(stringify!($op)),*]
    };
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

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        upcall("open");
        // Write-mode opens must never reach the Opener: a passthrough
        // reply to one would let the kernel open the node-shared
        // backing cache file for writing under rio-mountd's root
        // credentials. See [`write_open_violation`].
        // r[impl builder.fs.open-read-only]
        if let Some(errno) = write_open_violation(flags) {
            reply.error(errno);
            return;
        }
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

    /// Reachable only when an open degraded to a userspace read or
    /// during a streaming open's fill window (see [`Opener::read`]).
    /// Warm passthrough reads go kernel → backing file with no upcall.
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
        // Probe-vs-data branch (and the EIO trap it avoids) documented
        // on XattrListReply.
        match empty_xattr_list_reply(size) {
            XattrListReply::SizeProbe => reply.size(0),
            XattrListReply::EmptyData => reply.data(&[]),
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

    // ─── Write path: read-only filesystem, everything is EROFS ─────────
    // The tree is immutable for the mount's lifetime and the backing
    // cache is node-shared; every mutating operation is denied with the
    // errno POSIX prescribes for a read-only filesystem. One macro row
    // per op — see [`write_path_deny_table!`] and [`deny_write_op`].
    // r[impl builder.fs.write-ops-erofs]
    write_path_deny_table!(deny_with_erofs);
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify builder.fuse.listxattr-empty]
    /// A `listxattr` query with a non-zero buffer must get an empty
    /// DATA payload, never the size struct — see [`XattrListReply`] for
    /// the kernel check (and the `shutil.copy2` regression) this
    /// guards. Pins the pure decision function: fuser's reply types
    /// cannot be constructed outside that crate, and a real mount needs
    /// privileges the test sandbox lacks.
    #[test]
    fn listxattr_empty_list_never_uses_size_struct_for_data_requests() {
        assert_eq!(empty_xattr_list_reply(0), XattrListReply::SizeProbe);
        for size in [1, 8, 4096, u32::MAX] {
            assert_eq!(
                empty_xattr_list_reply(size),
                XattrListReply::EmptyData,
                "size={size}: a non-zero buffer must get the empty data reply"
            );
        }
    }

    // r[verify builder.fs.open-read-only]
    /// Any open whose access mode is not `O_RDONLY` (or that carries a
    /// mutating flag) must be denied with EROFS — see
    /// [`write_open_violation`] for the cache-poisoning hole this
    /// closes. Like the listxattr test above, this pins the pure
    /// decision function that `open()` maps 1:1 onto its reply.
    #[test]
    fn open_rejects_write_access_modes_with_erofs() {
        use nix::libc::{O_APPEND, O_CLOEXEC, O_NOFOLLOW, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
        for flags in [
            O_WRONLY,
            O_RDWR,
            O_WRONLY | O_TRUNC,
            O_RDWR | O_TRUNC,
            O_RDONLY | O_TRUNC,
            O_RDONLY | O_APPEND,
            O_WRONLY | O_APPEND,
        ] {
            let verdict = write_open_violation(OpenFlags(flags));
            assert_eq!(
                verdict
                    .unwrap_or_else(|| panic!("flags {flags:#o}: write-mode open must be denied"))
                    .code(),
                Errno::EROFS.code(),
                "flags {flags:#o}: the read-only-filesystem errno is EROFS"
            );
        }
        // Plain reads — including ones with non-mutating extra flags —
        // still work.
        for flags in [O_RDONLY, O_RDONLY | O_NOFOLLOW, O_RDONLY | O_CLOEXEC] {
            assert!(
                write_open_violation(OpenFlags(flags)).is_none(),
                "flags {flags:#o}: a read-only open must be allowed"
            );
        }
    }

    // r[verify builder.fs.write-ops-erofs]
    /// Every write-path FUSE operation must answer EROFS (see
    /// [`deny_write_op`] for why fuser's ENOSYS/EPERM defaults are
    /// wrong). Comparing the deny table against the POSIX write-op set
    /// catches a deleted row, which would silently fall back to those
    /// defaults.
    #[test]
    fn write_path_ops_deny_with_erofs() {
        const POSIX_WRITE_OPS: &[&str] = &[
            "setattr",
            "mknod",
            "mkdir",
            "unlink",
            "rmdir",
            "symlink",
            "rename",
            "link",
            "create",
            "write",
            "setxattr",
            "removexattr",
            "fallocate",
        ];
        let table: &[&str] = write_path_deny_table!(deny_table_op_names);
        assert_eq!(
            table, POSIX_WRITE_OPS,
            "the deny table must cover exactly the write-path ops"
        );
        for &op in POSIX_WRITE_OPS {
            assert_eq!(
                deny_write_op(op).code(),
                Errno::EROFS.code(),
                "{op} must reply EROFS"
            );
        }
    }
}
