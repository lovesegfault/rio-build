//! Request dispatch for the castore-FUSE — every FUSE request the
//! kernel routes (the whole op set minus INTERRUPT/FORGET/
//! notifications) arrives here over the rings.
//!
//! Decision logic is shared with the rest of `castore_fuse` (`tree`,
//! `Opener::*`, `write_open_violation`, `empty_xattr_list_reply`,
//! `deny_write_op`, `negative_attr`); requests are parsed and replies
//! encoded by hand ([`super::abi`]) because the wire marshalling in
//! the `fuser` crate is private to it. The `vm-castore-fuse` scenario
//! runs the full read matrix over this path.
//!
//! # Tiers
//!
//! [`handle_fast`] runs on the queue thread and must never block on
//! the network: metadata ops are pure DAG reads, opens are attempted
//! through `Opener::open_warm` (local state only) and reads through
//! `Opener::read_fast`. Anything that would wait — a cold open's
//! fetch, a read inside a streaming fill window, `release`'s mountd
//! round-trip — returns [`FastOutcome::Punt`] and is replayed by a
//! slow-pool worker via [`handle_slow`], which runs the full blocking
//! ladder. Punting leaves the request buffers untouched, so the slow
//! tier re-parses the same bytes.
//!
//! Ops that never reach this dispatcher: INIT, FORGET/BATCH_FORGET,
//! INTERRUPT and notifications stay on `/dev/fuse` (kernel routes them
//! there even with an active ring; see
//! `Documentation/filesystems/fuse-io-uring.rst`), where the fuser
//! session keeps serving them.

use std::sync::Arc;

use fuser::{Errno, INodeNo};

use super::abi;
use crate::castore_fuse::open::{OpenOutcome, Opener};
use crate::castore_fuse::tree::{InoMap, Node, TTL};
use crate::castore_fuse::{
    XattrListReply, deny_write_op, empty_xattr_list_reply, negative_attr, upcall,
    write_open_violation,
};

/// The shared filesystem state the ring workers dispatch against —
/// the same `InoMap`/`Opener` pair the session assembly built.
pub(super) struct RingFs {
    pub tree: Arc<InoMap>,
    pub opener: Arc<Opener>,
}

/// A finished reply: `error` is 0 or a negated errno;
/// `len` is the number of payload bytes written (0 on error).
pub(super) struct Reply {
    pub error: i32,
    pub len: usize,
}

impl Reply {
    fn ok(len: usize) -> Self {
        Self { error: 0, len }
    }

    pub(super) fn err(errno: Errno) -> Self {
        Self {
            error: -errno.code(),
            len: 0,
        }
    }
}

/// What the fast tier did with a request.
pub(super) enum FastOutcome {
    /// Reply produced (written into the payload buffer where the op
    /// has one); the queue thread commits it inline.
    Done(Reply),
    /// The op needs the blocking ladder: hand the entry to the slow
    /// pool. The request buffers are untouched.
    Punt,
}

/// How an opcode is handled, factored out of [`handle_fast`] so the
/// write-path/ENOSYS classification is unit-testable without
/// constructing an `Opener`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Disposition {
    /// Read-path op with a real handler arm.
    Handled,
    /// Mutating op: denied with EROFS via `deny_write_op` — the errno
    /// POSIX prescribes for a read-only filesystem.
    DenyWrite(&'static str),
    /// Everything else: ENOSYS (the kernel remembers and falls back).
    NotImplemented,
}

// r[impl builder.fs.write-ops-erofs]
pub(super) fn disposition(opcode: u32) -> Disposition {
    use abi::*;
    match opcode {
        FUSE_LOOKUP | FUSE_GETATTR | FUSE_READLINK | FUSE_OPEN | FUSE_READ | FUSE_RELEASE
        | FUSE_STATFS | FUSE_GETXATTR | FUSE_LISTXATTR | FUSE_OPENDIR | FUSE_READDIR
        | FUSE_RELEASEDIR | FUSE_READDIRPLUS | FUSE_IOCTL | FUSE_DESTROY => Disposition::Handled,
        // One row per POSIX write-path op (RENAME2 shares the rename
        // row, matching how the kernel routes it).
        FUSE_SETATTR => Disposition::DenyWrite("setattr"),
        FUSE_MKNOD => Disposition::DenyWrite("mknod"),
        FUSE_MKDIR => Disposition::DenyWrite("mkdir"),
        FUSE_UNLINK => Disposition::DenyWrite("unlink"),
        FUSE_RMDIR => Disposition::DenyWrite("rmdir"),
        FUSE_SYMLINK => Disposition::DenyWrite("symlink"),
        FUSE_RENAME | FUSE_RENAME2 => Disposition::DenyWrite("rename"),
        FUSE_LINK => Disposition::DenyWrite("link"),
        FUSE_CREATE => Disposition::DenyWrite("create"),
        FUSE_WRITE => Disposition::DenyWrite("write"),
        FUSE_SETXATTR => Disposition::DenyWrite("setxattr"),
        FUSE_REMOVEXATTR => Disposition::DenyWrite("removexattr"),
        FUSE_FALLOCATE => Disposition::DenyWrite("fallocate"),
        _ => Disposition::NotImplemented,
    }
}

/// Fast-tier dispatch, run on the queue thread. `op_in` is the fixed
/// per-op header region; `payload` is the entry's payload buffer,
/// holding `req_payload_len` request bytes on entry and receiving the
/// reply payload in place (only when the outcome is `Done`).
pub(super) fn handle_fast(
    fs: &RingFs,
    hdr: &abi::InHeader,
    op_in: &[u8],
    payload: &mut [u8],
    req_payload_len: usize,
) -> FastOutcome {
    metrics::counter!("rio_builder_castore_fuse_uring_requests_total").increment(1);
    match disposition(hdr.opcode) {
        Disposition::DenyWrite(op) => FastOutcome::Done(Reply::err(deny_write_op(op))),
        Disposition::NotImplemented => FastOutcome::Done(Reply::err(Errno::ENOSYS)),
        Disposition::Handled => handle_read_path_fast(fs, hdr, op_in, payload, req_payload_len),
    }
}

/// Slow-tier dispatch, run on a slow-pool worker for punted entries.
/// Only the opcodes [`handle_fast`] can punt arrive here; each runs
/// its full (possibly network-blocking) ladder. The metrics counter
/// already ticked in the fast tier.
pub(super) fn handle_slow(
    fs: &RingFs,
    hdr: &abi::InHeader,
    op_in: &[u8],
    payload: &mut [u8],
) -> Reply {
    use abi::*;
    let ino = hdr.nodeid;
    match hdr.opcode {
        FUSE_OPEN => match fs.tree.node(ino) {
            Some(Node::File {
                file_digest, size, ..
            }) => open_reply(payload, fs.opener.open_inner(*file_digest, *size)),
            // Unreachable: the fast tier only punts the File arm. Kept
            // total so a future punt of another node kind cannot
            // silently EIO.
            other => Reply::err(open_node_errno(ino, other)),
        },
        FUSE_READ => {
            let read_in = ReadIn::parse(op_in);
            read_reply(
                payload,
                fs.opener.read(read_in.fh, read_in.offset, read_in.size),
            )
        }
        FUSE_RELEASE => {
            let fh = parse_release_in_fh(op_in);
            if let Some(Node::File { file_digest, .. }) = fs.tree.node(ino) {
                fs.opener.release(file_digest, fh);
            }
            Reply::ok(0)
        }
        // The fast tier never punts anything else.
        opcode => {
            debug_assert!(false, "non-puntable opcode {opcode} reached the slow tier");
            Reply::err(Errno::EIO)
        }
    }
}

/// Marshal an open outcome (or errno) into a `fuse_open_out` reply —
/// shared by both tiers so warm and cold opens are byte-identical.
fn open_reply(payload: &mut [u8], outcome: Result<OpenOutcome, Errno>) -> Reply {
    use abi::*;
    match outcome {
        Ok(OpenOutcome::Passthrough { fh, backing_id }) => Reply::ok(write_open_out(
            payload,
            fh,
            FOPEN_PASSTHROUGH,
            backing_id as i32,
        )),
        Ok(OpenOutcome::KeepCache { fh }) => {
            Reply::ok(write_open_out(payload, fh, FOPEN_KEEP_CACHE, 0))
        }
        Err(errno) => {
            // The open-path EIO budget counter (every failed open
            // ticks it, whatever the cause).
            metrics::counter!("rio_builder_castore_fuse_eio_total").increment(1);
            Reply::err(errno)
        }
    }
}

/// The errno gate applied to a lookup name BEFORE the InoMap probe.
/// A component longer than the advertised NAME_MAX must be
/// ENAMETOOLONG (`_POSIX_NO_TRUNC`), never ENOENT: ENOENT asserts the
/// name was validly looked up and is absent, and the infinite-TTL
/// negative entry caches that lie. `None` = the name is legal, proceed
/// to the DAG probe.
fn lookup_name_errno(name: &[u8]) -> Option<Errno> {
    (name.len() > abi::NAME_MAX).then_some(Errno::ENAMETOOLONG)
}

/// The errno for an `open()` of a non-file node (or a missing one) —
/// shared by both tiers.
fn open_node_errno(ino: u64, node: Option<&Node>) -> Errno {
    match node {
        Some(Node::Dir { .. }) => Errno::EISDIR,
        Some(Node::Symlink { .. }) => Errno::ELOOP,
        Some(Node::File { .. }) => unreachable!("file nodes are handled by the caller"),
        None if ino == INodeNo::ROOT.0 => Errno::EISDIR,
        None => Errno::ENOENT,
    }
}

/// Marshal a userspace-read result into the payload buffer.
fn read_reply(payload: &mut [u8], data: Result<Vec<u8>, Errno>) -> Reply {
    match data {
        Ok(data) => {
            let len = data.len().min(payload.len());
            payload[..len].copy_from_slice(&data[..len]);
            Reply::ok(len)
        }
        Err(errno) => Reply::err(errno),
    }
}

fn handle_read_path_fast(
    fs: &RingFs,
    hdr: &abi::InHeader,
    op_in: &[u8],
    payload: &mut [u8],
    req_payload_len: usize,
) -> FastOutcome {
    use FastOutcome::{Done, Punt};
    use abi::*;
    let ino = hdr.nodeid;
    Done(match hdr.opcode {
        FUSE_LOOKUP => {
            upcall("lookup");
            // The request payload is the NUL-terminated name; copy it
            // out before the reply overwrites the buffer.
            let raw = &payload[..req_payload_len.min(payload.len())];
            let name = raw.split(|&b| b == 0).next().unwrap_or(&[]).to_vec();
            // Oversized components are ENAMETOOLONG before any DAG
            // probe — the kernel passes long names through, and the
            // advertised NAME_MAX makes ENOENT here a POSIX lie.
            if let Some(errno) = lookup_name_errno(&name) {
                return Done(Reply::err(errno));
            }
            // Names outside the prefetched DAG are a legitimate ENOENT
            // (the closure is the allowlist), cached forever via the
            // nodeid=0 negative entry.
            let attr = match fs.tree.lookup(ino, &name) {
                Some((_, attr)) => attr,
                None => negative_attr(),
            };
            Reply::ok(write_entry_out(payload, 0, &TTL, &attr, 0))
        }
        FUSE_GETATTR => {
            upcall("getattr");
            match fs.tree.attr(ino) {
                Some(attr) => Reply::ok(write_attr_out(payload, &TTL, &attr)),
                None => Reply::err(Errno::ENOENT),
            }
        }
        FUSE_READLINK => {
            upcall("readlink");
            match fs.tree.node(ino) {
                Some(Node::Symlink { target }) => {
                    let len = target.len().min(payload.len());
                    payload[..len].copy_from_slice(&target[..len]);
                    Reply::ok(len)
                }
                Some(_) => Reply::err(Errno::EINVAL),
                None => Reply::err(Errno::ENOENT),
            }
        }
        FUSE_OPENDIR => {
            if ino != INodeNo::ROOT.0 && !matches!(fs.tree.node(ino), Some(Node::Dir { .. })) {
                return Done(Reply::err(Errno::ENOTDIR));
            }
            // FOPEN_CACHE_DIR: the kernel caches the dirent pages, so
            // the second readdir of the same directory is 0-upcall.
            Reply::ok(write_open_out(
                payload,
                0,
                FOPEN_CACHE_DIR | FOPEN_KEEP_CACHE,
                0,
            ))
        }
        FUSE_READDIR => {
            upcall("readdir");
            let read_in = ReadIn::parse(op_in);
            let Some(entries) = fs.tree.readdir(ino, read_in.offset) else {
                return Done(Reply::err(Errno::ENOTDIR));
            };
            // Collected (small list) because the packer needs the
            // payload mutably while the iterator borrows the tree.
            let entries: Vec<_> = entries
                .map(|e| (e.ino, e.next_offset, e.kind, e.name.to_vec()))
                .collect();
            let mut buf = DirentBuf::new(payload, read_in.size as usize);
            for (e_ino, next_offset, kind, name) in entries {
                if !buf.push(e_ino, next_offset, kind, &name) {
                    break;
                }
            }
            Reply::ok(buf.len())
        }
        FUSE_READDIRPLUS => {
            // Same "readdir" upcall label for both readdir flavors.
            upcall("readdir");
            let read_in = ReadIn::parse(op_in);
            let Some(entries) = fs.tree.readdir(ino, read_in.offset) else {
                return Done(Reply::err(Errno::ENOTDIR));
            };
            let entries: Vec<_> = entries
                .map(|e| (e.ino, e.next_offset, e.name.to_vec()))
                .collect();
            let mut buf = DirentBuf::new(payload, read_in.size as usize);
            for (e_ino, next_offset, name) in entries {
                let attr = fs.tree.attr(e_ino).unwrap_or_else(negative_attr);
                if !buf.push_plus(next_offset, &name, &TTL, &attr, 0) {
                    break;
                }
            }
            Reply::ok(buf.len())
        }
        FUSE_OPEN => {
            upcall("open");
            // Write-mode opens must never reach the Opener — the
            // EROFS gate is what keeps root from write-opening the
            // node-shared backing cache through passthrough.
            // r[impl builder.fs.open-read-only] — shared decision fn;
            // this is the ring-transport call site (the violation is
            // denied here, before a punt, so it never reaches the
            // slow tier either).
            let flags = fuser::OpenFlags(parse_open_in_flags(op_in));
            if let Some(errno) = write_open_violation(flags) {
                return Done(Reply::err(errno));
            }
            match fs.tree.node(ino) {
                Some(Node::File { file_digest, .. }) => {
                    // Warm (local-state) opens answer inline; a cache
                    // miss means a network fetch — slow pool.
                    match fs.opener.open_warm(*file_digest) {
                        Ok(Some(outcome)) => open_reply(payload, Ok(outcome)),
                        Ok(None) => return Punt,
                        Err(errno) => open_reply(payload, Err(errno)),
                    }
                }
                other => Reply::err(open_node_errno(ino, other)),
            }
        }
        FUSE_READ => {
            upcall("read");
            let read_in = ReadIn::parse(op_in);
            // Reads from an already-open local handle answer inline; a
            // read inside a streaming fill window can block on the
            // fill's high-water mark — slow pool.
            match fs
                .opener
                .read_fast(read_in.fh, read_in.offset, read_in.size)
            {
                Some(result) => read_reply(payload, result),
                None => return Punt,
            }
        }
        // `release` can send a mountd BackingClose (UDS round-trip
        // with a timeout) — never on the queue thread.
        FUSE_RELEASE => return Punt,
        FUSE_RELEASEDIR | FUSE_DESTROY => Reply::ok(0),
        FUSE_STATFS => Reply::ok(write_statfs_out(payload)),
        // No xattrs on store paths, ever — ENODATA, not ENOSYS, so the
        // kernel keeps the per-inode "has no xattrs" state instead of
        // disabling xattr support for the whole mount (overlayfs
        // probes `user.overlay.*` on every lower inode and treats a
        // mount-wide ENOSYS as an error).
        FUSE_GETXATTR => Reply::err(Errno::ENODATA),
        // r[impl builder.fuse.listxattr-empty]
        FUSE_LISTXATTR => {
            let size = parse_getxattr_in_size(op_in);
            match empty_xattr_list_reply(size) {
                XattrListReply::SizeProbe => Reply::ok(write_getxattr_out(payload, 0)),
                XattrListReply::EmptyData => Reply::ok(0),
            }
        }
        // ENOTTY, not ENOSYS: overlay copy-up probes FS_IOC_GETFLAGS
        // and only ENOTTY means "no fileattr support" there.
        FUSE_IOCTL => Reply::err(Errno::ENOTTY),
        _ => unreachable!("disposition() gates the opcode set"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuser::FileType;

    /// Map a [`FileType`] used by readdir to the `d_type` the kernel
    /// expects (`mode >> 12`).
    fn dirent_type(kind: FileType) -> u32 {
        use nix::libc::{DT_DIR, DT_LNK, DT_REG};
        match kind {
            FileType::Directory => DT_DIR as u32,
            FileType::RegularFile => DT_REG as u32,
            FileType::Symlink => DT_LNK as u32,
            _ => 0,
        }
    }

    // r[verify builder.fs.write-ops-erofs]
    /// Every POSIX write-path opcode must classify as DenyWrite — a
    /// missed opcode would fall to ENOSYS, which is not a legal errno
    /// for unlink(2)/mkdir(2) and invites retries that cannot succeed.
    #[test]
    fn ring_disposition_covers_posix_write_ops() {
        use abi::*;
        let write_ops = [
            (FUSE_SETATTR, "setattr"),
            (FUSE_MKNOD, "mknod"),
            (FUSE_MKDIR, "mkdir"),
            (FUSE_UNLINK, "unlink"),
            (FUSE_RMDIR, "rmdir"),
            (FUSE_SYMLINK, "symlink"),
            (FUSE_RENAME, "rename"),
            (FUSE_RENAME2, "rename"),
            (FUSE_LINK, "link"),
            (FUSE_CREATE, "create"),
            (FUSE_WRITE, "write"),
            (FUSE_SETXATTR, "setxattr"),
            (FUSE_REMOVEXATTR, "removexattr"),
            (FUSE_FALLOCATE, "fallocate"),
        ];
        for (opcode, op) in write_ops {
            assert_eq!(
                disposition(opcode),
                Disposition::DenyWrite(op),
                "opcode {opcode} must deny as {op}"
            );
        }
        // The read matrix is handled, not denied.
        for opcode in [
            FUSE_LOOKUP,
            FUSE_GETATTR,
            FUSE_READLINK,
            FUSE_OPEN,
            FUSE_READ,
            FUSE_READDIR,
            FUSE_READDIRPLUS,
            FUSE_STATFS,
        ] {
            assert_eq!(disposition(opcode), Disposition::Handled);
        }
        // Anything unknown (FUSE_LSEEK=46, FUSE_POLL=40, future ops)
        // gets fuser-parity ENOSYS.
        for opcode in [20, 25, 31, 34, 40, 46, 47, 52, 999] {
            assert_eq!(disposition(opcode), Disposition::NotImplemented);
        }
    }

    /// POSIX `_POSIX_NO_TRUNC`: a lookup of a name component longer
    /// than the advertised NAME_MAX (statfs `f_namemax` = 255, see
    /// `write_statfs_out`) must fail with ENAMETOOLONG, not ENOENT —
    /// ENOENT claims the name was validly looked up and is absent,
    /// and the infinite-TTL negative entry makes that lie permanent.
    /// The kernel sends each path component as its own FUSE_LOOKUP, so
    /// one gate covers final and mid-path components alike.
    #[test]
    fn lookup_rejects_oversized_name_components_with_enametoolong() {
        // At or under the limit: no gate — the name proceeds to the
        // DAG probe.
        assert!(lookup_name_errno(&[b'a'; 255]).is_none());
        assert!(lookup_name_errno(b"normal-name").is_none());
        // Past the limit: ENAMETOOLONG.
        for len in [256usize, 300, 4000] {
            let verdict = lookup_name_errno(&vec![b'x'; len]);
            assert_eq!(
                verdict.map(|e| e.code()),
                Some(Errno::ENAMETOOLONG.code()),
                "{len}-byte component must be ENAMETOOLONG"
            );
        }
    }

    /// `d_type` derivation parity with fuser (`mode >> 12`).
    #[test]
    fn dirent_type_matches_mode_shift() {
        let mut buf = vec![0u8; 256];
        for kind in [
            FileType::Directory,
            FileType::RegularFile,
            FileType::Symlink,
        ] {
            let mut d = abi::DirentBuf::new(&mut buf, 256);
            assert!(d.push(1, 1, kind, b"n"));
            let typ = u32::from_ne_bytes(buf[20..24].try_into().unwrap());
            assert_eq!(typ, dirent_type(kind), "{kind:?}");
        }
    }
}
