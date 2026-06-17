//! Castore-FUSE lazy `/nix/store` (ADR-022 §2).
//!
//! A read-only view of one build's input closure. Metadata
//! (`lookup`/`getattr`/`readdir`/`readlink`) is answered from an
//! in-heap Directory DAG with infinite cache TTLs; `open()` brokers a
//! passthrough fd from the node-SSD backing cache so warm reads never
//! upcall. Requests are served over fuse-over-io_uring (the private
//! `uring` module — the only transport; the fuser session on `/dev/fuse`
//! handles INIT and the request classes the kernel never routes over
//! rings). The client-side mount/serve sequence lives in [`session`];
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
mod uring;

/// Re-export the bench/operator cache-reset entrypoint (`rio-mountd
/// evict-cache`); the sweep module itself stays private.
pub use self::sweep::evict_all;

use std::io;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use fuser::{
    Errno, FileAttr, FileType, Filesystem, INodeNo, InitFlags, KernelConfig, OpenFlags, Request,
};

use self::tree::InoMap;

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

/// One build's castore-FUSE. Constructed after the DAG prefetch and
/// the mountd `Mount{}` handshake; consumed by `Session::from_fd`.
/// Requests are dispatched by the fuse-over-io_uring engine (the
/// private `uring` module) against the `Arc`-shared tree/opener pair —
/// this type's `Filesystem` impl only carries the INIT negotiation.
pub struct CastoreFs {
    tree: Arc<InoMap>,
}

impl CastoreFs {
    pub fn new(tree: Arc<InoMap>) -> Self {
        Self { tree }
    }
}

/// The INIT-only `Filesystem` impl. Every request is served by the
/// ring dispatcher (`uring::dispatch`); the fuser session this impl
/// feeds exists for the handshake and the request classes the kernel
/// never routes over rings (INTERRUPT, FORGET, notifications — all
/// covered by fuser's defaults). The kernel parks regular requests
/// between the INIT reply and ring readiness, so no serving callback
/// can ever fire here.
impl Filesystem for CastoreFs {
    /// Negotiate the §2.4 cache capabilities, §2.9 passthrough
    /// stacking depth, and the mandatory fuse-over-io_uring transport.
    /// Failure to negotiate passthrough is fatal — a castore-FUSE that
    /// silently degrades to userspace `read()` for every input would
    /// still pass tests but ship a 10-100× data-path regression.
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
        // fuse-over-io_uring is the only transport: a kernel that does
        // not advertise the flag cannot serve this filesystem, so the
        // mount fails here, in the INIT handshake. Echoing the flag is
        // the kernel-side switch; mount_and_serve registers the rings
        // (created before INIT) right after the handshake completes.
        // r[impl builder.fs.io-uring-required]
        config
            .add_capabilities(InitFlags::FUSE_OVER_IO_URING)
            .map_err(|_| {
                io::Error::other(
                    "kernel lacks FUSE_OVER_IO_URING: the castore-FUSE serves exclusively over \
                 fuse-over-io_uring and requires Linux 6.14+ with fuse.enable_uring=1",
                )
            })?;
        // Read-only fs: shrinking max_write only bounds the payload
        // buffers the ring pre-registers per entry (see
        // uring::PAYLOAD_BUF_SZ for the sizing chain).
        let _ = config.set_max_write(uring::URING_MAX_WRITE);
        tracing::info!(
            inodes = self.tree.inode_count(),
            "castore-FUSE init (passthrough, max_stack_depth=1, fuse-over-io_uring)"
        );
        Ok(())
    }
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
    /// [`deny_write_op`] for why ENOSYS/EPERM would be wrong). The
    /// opcode→DenyWrite classification half of the contract lives in
    /// the ring dispatcher's `ring_disposition_covers_posix_write_ops`;
    /// this half pins the errno every classified op replies with.
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
        for &op in POSIX_WRITE_OPS {
            assert_eq!(
                deny_write_op(op).code(),
                Errno::EROFS.code(),
                "{op} must reply EROFS"
            );
        }
    }
}
