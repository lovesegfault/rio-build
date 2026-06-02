//! `rio-mountd` — the privileged per-node broker for castore-FUSE.
//!
//! The unprivileged builder pod cannot (a) open `/dev/fuse`, (b) call
//! `FUSE_DEV_IOC_BACKING_OPEN`/`_CLOSE` (init-namespace `CAP_SYS_ADMIN`
//! checks in `fs/fuse/backing.c`), or (c) write the node-shared backing
//! cache (integrity boundary). One DaemonSet per node with
//! `CAP_SYS_ADMIN` brokers exactly those three operations and nothing
//! else — no overlay mount, no FUSE upcall relay; the builder serves
//! FUSE and assembles its own overlay.
//!
//! See [`super::mountd_proto`] for the wire protocol and ADR-022 §11
//! (the design overview) for the privilege analysis. `bin/rio-mountd.rs`
//! is a thin clap wrapper around [`run`].
//!
//! # Concurrency
//!
//! One tokio task per accepted connection reads frames and answers
//! `Mount`/`BackingOpen`/`BackingClose` inline (sub-millisecond).
//! `Promote`/`PromoteChunks` acquire a process-wide
//! `Semaphore(num_cpus)` permit and run their copy+hash loop on
//! `spawn_blocking`, replying out-of-order via the connection's writer
//! channel — a multi-second promote never blocks the same build's
//! `BackingOpen` traffic. A per-connection slot pool
//! (`MAX_INFLIGHT_PROMOTES_PER_CONN`) bounds how many promote batches
//! one build can have outstanding, so a frame-pipelining client cannot
//! queue unbounded privileged copy work.
// r[impl builder.mountd.concurrency]

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use nix::fcntl::{AtFlags, Flock, FlockArg, OFlag, openat};
use nix::libc;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, UnixAddr, accept4, bind, getsockopt, listen,
    socket, sockopt,
};
use nix::sys::stat::{Mode, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchownat, unlinkat};
use rio_common::limits::FASTCDC_MAX_BYTES;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::mountd_proto::{
    self as proto, ErrKind, FrameError, PROMOTE_CHUNKS_MAX, Reply, Req, Request, Resp,
};
use crate::IgnorePoison;
use crate::quota::apply_project_quota;

/// Maximum length of a `build_id`. Combined with the character class in
/// [`validate_build_id`] this bounds every per-build path component.
pub const BUILD_ID_MAX_LEN: usize = 64;

/// Default per-`Promote` size ceiling (4 GiB — matches the store's
/// `MAX_NAR_SIZE`; a single staged file larger than the largest
/// admissible NAR member is necessarily bogus).
pub const DEFAULT_MAX_PROMOTE_BYTES: u64 = 4 << 30;

/// How long a `Promote` waits for a concurrent promote of the same
/// digest (the `.promoting` placeholder) to finish before giving up
/// with [`ErrKind::RaceTimeout`].
const PROMOTE_RACE_WAIT: Duration = Duration::from_secs(2);

/// How long a uid-colliding connection may take to send its first
/// request before the typed rejection is abandoned and the connection
/// closed anyway. Bounds the doomed connection's lifetime so a hung or
/// malicious peer cannot hold a second-connection slot open — the
/// uid-bound invariant's "no standing second connection" property.
const UID_BUSY_REPLY_DEADLINE: Duration = Duration::from_secs(5);

/// Copy-loop buffer. 64 KiB amortizes syscall overhead without holding
/// a large allocation per concurrent promote.
const PROMOTE_BUF: usize = 64 * 1024;

/// Distinguishes concurrent promotes' temp destinations. Names only
/// need to be unique within one daemon: a single mountd owns the cache
/// directories, and leftovers from a crashed daemon are removed by the
/// next incarnation's startup orphan scan before it serves connections.
static PROMOTE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Daemon configuration. The binary populates this from clap; tests
/// populate it from a tempdir.
#[derive(Debug, Clone)]
pub struct MountdConfig {
    /// UDS listen path, e.g. `/run/rio-mountd.sock`. Created mode 0660
    /// owned `root:allowed_gid`.
    pub socket_path: PathBuf,
    /// Per-build FUSE mountpoints: `{castore_dir}/{build_id}`.
    pub castore_dir: PathBuf,
    /// Per-build staging: `{staging_dir}/{build_id}` (0700, chowned to
    /// the connection's peer uid).
    pub staging_dir: PathBuf,
    /// Shared backing cache: `{cache_dir}/{ab}/{file_digest_hex}`.
    pub cache_dir: PathBuf,
    /// Shared chunk cache: `{chunks_dir}/{ab}/{chunk_digest_hex}`.
    pub chunks_dir: PathBuf,
    /// XFS project-quota hard limit applied to each build's staging
    /// directory. `0` disables the kernel quota — only acceptable where
    /// staging is not on XFS-with-`prjquota` (unit/dev environments);
    /// the production node module asserts the mount option so helm
    /// always sets a non-zero value.
    pub staging_quota_bytes: u64,
    /// Per-`Promote` size ceiling ([`ErrKind::TooLarge`] above it).
    pub max_promote_bytes: u64,
    /// Group that owns the listening socket: `bind_socket` chowns the
    /// UDS to `root:allowed_gid` mode 0660. That socket-file DAC check
    /// at `connect(2)` is the entire access-control contract — mountd
    /// performs no peer-credential check after accept. Executor pods
    /// reach the socket only through the controller-mounted hostPath
    /// directory, and their `runAsGroup` matches this gid so the
    /// connect succeeds.
    pub allowed_gid: u32,
}

/// `^[A-Za-z0-9_-]{1,64}$` without pulling a regex into the hot path.
/// The character class excludes `/`, `.` and NUL, so a validated
/// `build_id` is always a single, non-traversing path component.
// r[impl builder.mountd.build-id-validated]
pub fn validate_build_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= BUILD_ID_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ─── FUSE_DEV_IOC_BACKING_{OPEN,CLOSE} ─────────────────────────────────

/// `<uapi/linux/fuse.h>` `struct fuse_backing_map`.
#[repr(C)]
struct FuseBackingMap {
    fd: u32,
    flags: u32,
    padding: u64,
}

/// `_IOW(type, nr, sizeof(T))` for x86_64/aarch64. The `nix` crate's
/// `ioctl` feature is off workspace-wide; computing the request code
/// inline matches `quota.rs`.
const fn iow<T>(ty: u32, nr: u32) -> libc::c_ulong {
    const IOC_WRITE: u32 = 1;
    ((IOC_WRITE << 30) | ((std::mem::size_of::<T>() as u32) << 16) | (ty << 8) | nr)
        as libc::c_ulong
}
const FUSE_DEV_IOC_MAGIC: u32 = 229;
const FUSE_DEV_IOC_BACKING_OPEN: libc::c_ulong = iow::<FuseBackingMap>(FUSE_DEV_IOC_MAGIC, 1);
const FUSE_DEV_IOC_BACKING_CLOSE: libc::c_ulong = iow::<u32>(FUSE_DEV_IOC_MAGIC, 2);

/// Re-open the client-supplied backing fd read-only, so the file
/// description registered with the kernel can never carry write access
/// to the node-shared cache.
///
/// The fd arrives from the unprivileged builder over `SCM_RIGHTS`. The
/// honest client opens the cache entry `O_RDONLY`, but the broker must
/// not depend on that: it is the only party running under privileged
/// credentials, so it is the enforcement point. The kernel never does
/// I/O through the registered fd itself — `fuse_passthrough_open`
/// re-opens the backing *path* per FUSE open, with the FUSE caller's
/// flags, under *this daemon's* (root) credentials and no DAC check
/// (fs/fuse/passthrough.c → fs/backing-file.c `backing_file_open` →
/// `vfs_open`) — so pinning the registration read-only cannot break
/// passthrough reads; what it guarantees is that the privileged side
/// never registers a writable file description, whatever access mode
/// the client's fd carries.
///
/// `/proc/self/fd/<n>` resolves to the open file description's inode,
/// not its (re-traversed) path, so this also works for a cache entry
/// the LRU sweep unlinked between the client's open and this request.
/// `O_NONBLOCK` so a FIFO smuggled in cannot park this privileged
/// `open(2)` until a writer appears (same defense as `promote_one`'s
/// source open); the ioctl's `d_is_reg` check then rejects it.
// r[impl builder.mountd.backing-readonly]
fn reopen_backing_readonly(client_fd: BorrowedFd<'_>) -> std::io::Result<OwnedFd> {
    let proc_path = format!("/proc/self/fd/{}", client_fd.as_raw_fd());
    nix::fcntl::open(
        Path::new(&proc_path),
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
}

/// Register `backing` as a passthrough backing file on the FUSE
/// connection owned by `fuse_fd`. The kernel rejects nested/stacking
/// backing files (depth > 0) and scopes the returned id to the
/// connection; the only inspection the daemon does is the
/// [`reopen_backing_readonly`] pin its caller applies first.
// r[impl builder.mountd.backing-broker]
fn backing_open(fuse_fd: BorrowedFd<'_>, backing: BorrowedFd<'_>) -> std::io::Result<u32> {
    let map = FuseBackingMap {
        fd: backing.as_raw_fd() as u32,
        flags: 0,
        padding: 0,
    };
    // SAFETY: `fuse_fd` is a live /dev/fuse fd; `map` is a valid,
    // fully-initialized repr(C) struct matching the kernel ABI.
    let r = unsafe { libc::ioctl(fuse_fd.as_raw_fd(), FUSE_DEV_IOC_BACKING_OPEN, &map) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(r as u32)
}

fn backing_close(fuse_fd: BorrowedFd<'_>, id: u32) -> std::io::Result<()> {
    // SAFETY: `fuse_fd` is a live /dev/fuse fd; `id` outlives the call.
    let r = unsafe { libc::ioctl(fuse_fd.as_raw_fd(), FUSE_DEV_IOC_BACKING_CLOSE, &id) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ─── Promote ───────────────────────────────────────────────────────────

/// Unlinks `name` (relative to `dir`) on drop. A promote arms two of
/// these: one for the `.promoting` claim placeholder (always removed
/// when the promote ends — the published entry is a different inode)
/// and one for the unique temp destination (removed on every error and
/// panic path; on success the rename has already consumed the name and
/// the unlink is a harmless no-op).
struct UnlinkOnDrop<'a> {
    dir: BorrowedFd<'a>,
    name: &'a str,
}

impl Drop for UnlinkOnDrop<'_> {
    fn drop(&mut self) {
        let _ = unlinkat(self.dir, self.name, UnlinkatFlags::NoRemoveDir);
    }
}

/// Two-character shard prefix (`ab/` in `cache/ab/<hex>`): the first
/// byte of the digest, hex-encoded.
fn shard(digest: &[u8; 32]) -> String {
    hex::encode(&digest[..1])
}

/// Verify-copy one staged entry into a shared destination directory.
///
/// `staging` is the directory containing the source (named
/// `hex(digest)`); `dest_root` is the cache root under which the
/// `ab/<hex>` shard layout lives. Returns the number of bytes copied on
/// success so the caller can account `rio_mountd_promote_bytes_total`.
///
/// The copy re-hashes every byte and compares against `digest` before
/// the destination becomes visible — this is the integrity boundary
/// that keeps the node-shared cache trustworthy against a compromised
/// builder. The destination is a fresh mountd-owned temp file created
/// `0444` under a per-promote unique name; only that inode — the one
/// this call wrote and hashed — is `renameat`ed to the final name, and
/// only after the hash matches. The `.promoting` placeholder is purely
/// the claim that serializes concurrent promotes of one digest.
// r[impl builder.mountd.promote-verified]
// r[impl builder.mountd.promote-bounded-copy]
pub(crate) fn promote_one(
    staging: BorrowedFd<'_>,
    dest_root: BorrowedFd<'_>,
    digest: &[u8; 32],
    max_bytes: u64,
) -> Result<u64, ErrKind> {
    promote_one_hooked(staging, dest_root, digest, max_bytes, || {})
}

/// [`promote_one`] with a test seam: `before_publish` runs after the
/// verify-copy has been written and synced, immediately before
/// publication — the widest window in which a concurrent promote of the
/// same digest can interleave. Production passes a no-op.
fn promote_one_hooked(
    staging: BorrowedFd<'_>,
    dest_root: BorrowedFd<'_>,
    digest: &[u8; 32],
    max_bytes: u64,
    before_publish: impl FnOnce(),
) -> Result<u64, ErrKind> {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;

    let hex_name = hex::encode(digest);

    // ── Source: open O_NOFOLLOW so a symlink planted in staging cannot
    // redirect the privileged read, O_NONBLOCK so a FIFO planted there
    // cannot park the privileged open() until a writer appears (it has
    // no effect on the regular files the S_ISREG check then requires;
    // local regular-file reads never return EAGAIN).
    let src = match openat(
        staging,
        hex_name.as_str(),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(nix::errno::Errno::ELOOP) => return Err(ErrKind::NotRegular),
        Err(nix::errno::Errno::ENOENT) => {
            return Err(ErrKind::Retryable(format!("staging/{hex_name} not found")));
        }
        Err(e) => return Err(ErrKind::Retryable(format!("open staging/{hex_name}: {e}"))),
    };
    let st = match nix::sys::stat::fstat(&src) {
        Ok(st) => st,
        Err(e) => return Err(ErrKind::Retryable(format!("fstat staging/{hex_name}: {e}"))),
    };
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(ErrKind::NotRegular);
    }
    let claimed_size = st.st_size as u64;
    if claimed_size > max_bytes {
        return Err(ErrKind::TooLarge);
    }

    // ── Destination shard dir.
    let ab = shard(digest);
    match mkdirat(dest_root, ab.as_str(), Mode::from_bits_truncate(0o755)) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
        Err(e) => return Err(ErrKind::Retryable(format!("mkdir {ab}/: {e}"))),
    }
    let ab_dir = match openat(
        dest_root,
        ab.as_str(),
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => return Err(ErrKind::Retryable(format!("open {ab}/: {e}"))),
    };

    // ── Claim the digest with an O_EXCL `.promoting` placeholder, held
    // under an exclusive flock for the lifetime of this promote. The
    // placeholder is only a claim marker: the loser of a race waits for
    // the winner instead of doing a duplicate copy (content-addressed:
    // any winner writes the same bytes). Owner liveness is judged by the
    // lock state — see `wait_for_concurrent_promote`. The data itself
    // goes into a unique temp file below.
    let promoting = format!("{hex_name}.promoting");
    let _claim = loop {
        match openat(
            &ab_dir,
            promoting.as_str(),
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o444),
        ) {
            Ok(fd) => match Flock::lock(std::fs::File::from(fd), FlockArg::LockExclusiveNonblock) {
                Ok(lock) => break lock,
                // Either a concurrent promote probed the gap between our
                // create and our flock and is reclaiming the name, or
                // the flock itself failed (e.g. EINTR). Drop the claim
                // we just created before retrying so a non-theft failure
                // does not leave an orphan claim that costs the next
                // promoter of this digest a reclaim round.
                Err(_) => {
                    let _ = unlinkat(&ab_dir, promoting.as_str(), UnlinkatFlags::NoRemoveDir);
                    continue;
                }
            },
            Err(nix::errno::Errno::EEXIST) => {
                match wait_for_concurrent_promote(ab_dir.as_fd(), &hex_name, &promoting) {
                    RaceOutcome::AlreadyPromoted => return Ok(0),
                    RaceOutcome::ClaimReleased => continue,
                    RaceOutcome::TimedOut => return Err(ErrKind::RaceTimeout),
                }
            }
            Err(e) => return Err(ErrKind::Retryable(format!("create {promoting}: {e}"))),
        }
    };
    // The claim is removed when this promote ends — success or failure —
    // because the published entry is a different inode. If a racer
    // wrongly reclaimed this claim mid-promote, this unlink at worst
    // removes a successor's live claim — costing a duplicate verify-copy
    // of identical bytes, never corruption (publication only renames the
    // temp inode below).
    let _claim_cleanup = UnlinkOnDrop {
        dir: ab_dir.as_fd(),
        name: &promoting,
    };

    // ── Destination: a unique mountd-owned temp file. Publication below
    // renames only this inode — the one this promote writes and hashes —
    // so no concurrent promote of the same digest can get its bytes
    // published by us, no matter what happens to the shared `.promoting`
    // name in the meantime.
    let tmp_name = format!(
        "{hex_name}.{}.tmp",
        PROMOTE_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let dst = match openat(
        &ab_dir,
        tmp_name.as_str(),
        OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o444),
    ) {
        Ok(fd) => fd,
        Err(e) => return Err(ErrKind::Retryable(format!("create {tmp_name}: {e}"))),
    };
    let _tmp_cleanup = UnlinkOnDrop {
        dir: ab_dir.as_fd(),
        name: &tmp_name,
    };

    // ── Bounded verify-copy. The builder owns the source inode and can
    // append to it concurrently; copying "until EOF" would let it grow
    // the privileged write into the shared cache without bound. Stop at
    // the fstat-time size and treat any deviation — growth or
    // truncation — as a mismatch.
    let mut src_f = std::fs::File::from(src);
    let mut dst_f = std::fs::File::from(dst);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; PROMOTE_BUF];
    let mut copied: u64 = 0;
    loop {
        let want = std::cmp::min(PROMOTE_BUF as u64, claimed_size - copied) as usize;
        if want == 0 {
            break;
        }
        let n = match src_f.read(&mut buf[..want]) {
            Ok(0) => {
                // EOF before st_size bytes: concurrent truncation.
                return Err(ErrKind::DigestMismatch);
            }
            Ok(n) => n,
            Err(e) => return Err(ErrKind::Retryable(format!("read staging/{hex_name}: {e}"))),
        };
        hasher.update(&buf[..n]);
        if let Err(e) = dst_f.write_all(&buf[..n]) {
            return Err(ErrKind::Retryable(format!("write {ab}/{tmp_name}: {e}")));
        }
        copied += n as u64;
    }
    if *hasher.finalize().as_bytes() != *digest {
        return Err(ErrKind::DigestMismatch);
    }
    // The O_CREAT mode is filtered through the daemon's umask; a
    // hardened umask (0077) would leave the entry unreadable to the
    // builder uids that need to open it for BACKING_OPEN. Set the final
    // mode explicitly on the still-open fd.
    if let Err(e) = dst_f.set_permissions(std::fs::Permissions::from_mode(0o444)) {
        return Err(ErrKind::Retryable(format!("fchmod {ab}/{tmp_name}: {e}")));
    }
    if let Err(e) = dst_f.sync_all() {
        return Err(ErrKind::Retryable(format!("fsync {ab}/{tmp_name}: {e}")));
    }
    drop(dst_f);

    before_publish();

    // ── Publish the inode this promote wrote and verified.
    if let Err(e) = nix::fcntl::renameat(&ab_dir, tmp_name.as_str(), &ab_dir, hex_name.as_str()) {
        return Err(ErrKind::Retryable(format!("rename {tmp_name}: {e}")));
    }
    // Source is no longer needed; failure to unlink is not a promote
    // failure (the staging dir is removed wholesale at teardown).
    let _ = unlinkat(staging, hex_name.as_str(), UnlinkatFlags::NoRemoveDir);
    Ok(copied)
}

enum RaceOutcome {
    AlreadyPromoted,
    /// The `.promoting` claim is gone (its owner finished or failed and
    /// cleaned up) or was reclaimed from a provably dead owner. The
    /// caller retries the claim.
    ClaimReleased,
    TimedOut,
}

/// Another promote of the same digest holds the `.promoting` claim.
/// Poll for the final name to appear (the winner publishing); give up
/// with [`RaceOutcome::TimedOut`] after [`PROMOTE_RACE_WAIT`].
///
/// A claim whose owner no longer holds its exclusive flock has no live
/// owner (within a live daemon every promote unlinks its claim on every
/// exit path; an unheld claim is a leftover of a daemon that died
/// before its successor's orphan scan) and is reclaimed. Liveness is
/// judged ONLY by the lock state — never by the placeholder's age,
/// which says nothing about whether the owner is still running. The
/// flip side: there is no time-based reclaim of a live-but-wedged
/// owner — recovery from a promote that is truly stuck while still
/// holding its flock is a daemon restart, observable as
/// `rio_mountd_promote_inflight` staying high.
///
/// A poll loop replaces the planned inotify watch — at a 50 ms period
/// the added latency is bounded by one period and the code does not
/// need an inotify fd per in-flight race.
fn wait_for_concurrent_promote(
    ab_dir: BorrowedFd<'_>,
    final_name: &str,
    promoting: &str,
) -> RaceOutcome {
    let deadline = std::time::Instant::now() + PROMOTE_RACE_WAIT;
    loop {
        if fstatat(ab_dir, final_name, AtFlags::AT_SYMLINK_NOFOLLOW).is_ok() {
            return RaceOutcome::AlreadyPromoted;
        }
        match openat(
            ab_dir,
            promoting,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            // Claim gone without the final name appearing: the owner
            // failed and cleaned up. Retry the claim.
            Err(_) => return RaceOutcome::ClaimReleased,
            Ok(probe) => {
                if let Ok(lock) =
                    Flock::lock(std::fs::File::from(probe), FlockArg::LockExclusiveNonblock)
                {
                    // Nobody holds the claim. Reclaim it — but only if
                    // the name still refers to the inode we locked, so a
                    // claim a new owner created after our open is left
                    // alone.
                    let same_inode = match (
                        nix::sys::stat::fstat(&*lock),
                        fstatat(ab_dir, promoting, AtFlags::AT_SYMLINK_NOFOLLOW),
                    ) {
                        (Ok(ours), Ok(named)) => {
                            ours.st_dev == named.st_dev && ours.st_ino == named.st_ino
                        }
                        _ => false,
                    };
                    if same_inode {
                        let _ = unlinkat(ab_dir, promoting, UnlinkatFlags::NoRemoveDir);
                        return RaceOutcome::ClaimReleased;
                    }
                }
                // The lock is held (or the claim was just replaced): a
                // live promote owns it — keep waiting for its publish.
            }
        }
        if std::time::Instant::now() >= deadline {
            return RaceOutcome::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ─── Shared daemon state ───────────────────────────────────────────────

struct Shared {
    cfg: MountdConfig,
    castore_base: OwnedFd,
    staging_base: OwnedFd,
    cache_base: OwnedFd,
    chunks_base: OwnedFd,
    /// One live connection per `SO_PEERCRED.uid`. The value is the
    /// `build_id` the connection has mounted (empty until `Mount`
    /// succeeds) so a colliding connect's typed rejection can name the
    /// holder — usually the previous build of the same pod whose
    /// teardown is still in flight.
    live_uids: Mutex<HashMap<libc::uid_t, String>>,
    /// One live `Mount{build_id}` per process across all connections.
    /// `Arc` so the cache sweep can snapshot it without holding the
    /// whole `Shared`.
    live_build_ids: Arc<Mutex<HashSet<String>>>,
    /// Monotonic XFS project-id allocator. Starts at 1 after the
    /// startup orphan scan empties `staging/` — a reused projid whose
    /// previous owner's files are all gone accounts from zero, so
    /// persistence across restarts is unnecessary. Never derived from
    /// the adversary-chosen `build_id`.
    next_projid: AtomicU32,
    /// Bounds concurrent `Promote`/`PromoteChunks` copy loops.
    promote_sem: Arc<tokio::sync::Semaphore>,
}

/// Per-connection mutable state, owned by the connection task.
/// [`teardown`] consumes the registrations (`peer_uid`, `build_id`)
/// back out of [`Shared`] on every exit path.
struct ConnState {
    peer_uid: libc::uid_t,
    peer_gid: libc::gid_t,
    /// `dup()` of the build's `/dev/fuse` fd, held for the lifetime of
    /// the connection so `BACKING_OPEN` can be issued after the
    /// original was handed to the builder.
    kept: Option<OwnedFd>,
    build_id: Option<String>,
    staging_dirfd: Option<Arc<OwnedFd>>,
    staging_chunks_dirfd: Option<Arc<OwnedFd>>,
    projid: Option<u32>,
    /// Live `BACKING_OPEN` registrations (opened minus closed). Each
    /// one allocates a `struct fuse_backing` in the kernel charged to
    /// *mountd's* cgroup (the ioctl issuer), so an uncapped client
    /// looping on `BackingOpen` would OOM-kill the broker for every
    /// build on the node. See [`MAX_LIVE_BACKING_IDS`].
    live_backing_ids: u32,
    /// Permits for promote batches accepted from this connection but
    /// not yet answered; an exhausted pool rejects the request instead
    /// of queueing it. See [`MAX_INFLIGHT_PROMOTES_PER_CONN`].
    promote_slots: Arc<tokio::sync::Semaphore>,
}

/// Per-connection ceiling on live `BACKING_OPEN` registrations. The
/// legitimate count is one per distinct `file_digest` opened during the
/// build (the castore-FUSE keys backing ids per digest and reuses them
/// across opens), so this only needs to clear the largest plausible
/// input closure — 2^20 files is ~10× nixpkgs' biggest. At ~64 bytes of
/// kernel memory per registration the worst case is 64 MiB per
/// connection, which the DaemonSet's memory limit must budget for.
const MAX_LIVE_BACKING_IDS: u32 = 1 << 20;

/// Per-connection ceiling on `Promote`/`PromoteChunks` batches accepted
/// but not yet answered. The legitimate client awaits every promote it
/// issues, so its in-flight count is bounded by its own concurrency
/// (FUSE worker threads plus active streaming fills — single digits in
/// practice). Without a cap, a client can pipeline ~40-byte Promote
/// frames faster than the broker verify-copies them: each queued frame
/// is up to `max_promote_bytes` of privileged read+hash+write, so the
/// backlog starves co-located builds' promotes past their request
/// timeout and grows the broker's memory without bound — the same abuse
/// class [`MAX_LIVE_BACKING_IDS`] closes for `BackingOpen`.
const MAX_INFLIGHT_PROMOTES_PER_CONN: usize = 64;

// ─── Daemon entrypoint ─────────────────────────────────────────────────

/// Bind the socket, reap orphans from a previous incarnation, and serve
/// until cancelled.
pub async fn run(cfg: MountdConfig) -> anyhow::Result<()> {
    let open_base = |p: &Path| -> anyhow::Result<OwnedFd> {
        std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
        nix::fcntl::open(
            p,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("open {}", p.display()))
    };
    let castore_base = open_base(&cfg.castore_dir)?;
    let staging_base = open_base(&cfg.staging_dir)?;
    let cache_base = open_base(&cfg.cache_dir)?;
    let chunks_base = open_base(&cfg.chunks_dir)?;

    reap_orphans(&cfg, &castore_base);

    let shared = Arc::new(Shared {
        castore_base,
        staging_base,
        cache_base,
        chunks_base,
        live_uids: Mutex::new(HashMap::new()),
        live_build_ids: Arc::new(Mutex::new(HashSet::new())),
        next_projid: AtomicU32::new(0),
        promote_sem: Arc::new(tokio::sync::Semaphore::new(
            std::thread::available_parallelism().map_or(4, |n| n.get()),
        )),
        cfg,
    });

    let listener = bind_socket(&shared.cfg)?;
    info!(socket = %shared.cfg.socket_path.display(), "rio-mountd listening");
    let listener = AsyncFd::with_interest(listener, Interest::READABLE)?;

    // Disk-pressure LRU over the shared caches + dead-staging backstop.
    // Detached: it holds no fds and unlink is atomic, so there is
    // nothing to drain at shutdown.
    tokio::spawn(super::sweep::run(
        shared.cfg.cache_dir.clone(),
        shared.cfg.chunks_dir.clone(),
        shared.cfg.staging_dir.clone(),
        Arc::clone(&shared.live_build_ids),
    ));

    loop {
        let mut guard = listener.readable().await?;
        let conn = match guard.try_io(|l| {
            accept4(
                l.get_ref().as_raw_fd(),
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            )
            .map_err(std::io::Error::from)
        }) {
            Err(_would_block) => continue,
            Ok(Err(e)) => {
                warn!(error = %e, "accept failed");
                continue;
            }
            // SAFETY: accept4 just returned a fresh fd; we are its sole
            // owner.
            Ok(Ok(raw)) => unsafe { OwnedFd::from_raw_fd(raw) },
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(shared, conn).await {
                warn!(error = %e, "connection terminated");
            }
        });
    }
}

/// `SOCK_SEQPACKET` listener at `cfg.socket_path`, mode 0660, group
/// `cfg.allowed_gid`. The socket file's owner/mode is the daemon's only
/// access control: a peer that can `connect(2)` is in the allowed group
/// (or has DAC override) and is served. A stale socket from a previous
/// incarnation is unlinked first (the DaemonSet is the only writer of
/// this path).
fn bind_socket(cfg: &MountdConfig) -> anyhow::Result<OwnedFd> {
    let _ = std::fs::remove_file(&cfg.socket_path);
    let fd = socket(
        AddressFamily::Unix,
        SockType::SeqPacket,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )
    .context("socket(AF_UNIX, SOCK_SEQPACKET)")?;
    let addr = UnixAddr::new(&cfg.socket_path).context("socket path")?;
    bind(fd.as_raw_fd(), &addr).context("bind")?;
    // Tighten before listen() so there is no window where a foreign gid
    // can connect to a 0777 socket.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cfg.socket_path, std::fs::Permissions::from_mode(0o660))
        .context("chmod socket")?;
    fchownat(
        nix::fcntl::AT_FDCWD,
        &cfg.socket_path,
        None,
        Some(Gid::from_raw(cfg.allowed_gid)),
        AtFlags::empty(),
    )
    .context("chown socket")?;
    listen(&fd, Backlog::new(64).expect("valid backlog")).context("listen")?;
    Ok(fd)
}

/// Reap leftovers from a previous daemon incarnation. No connection can
/// be live before the listener exists, so everything found here is an
/// orphan: castore mountpoints (lazily unmounted then removed), staging
/// trees (removed), and `.promoting`/`.tmp` placeholders in the shared
/// caches (removed — their owning copy loop is gone).
/// The scan walks the configured *paths* rather than the pre-opened
/// base dirfds: it runs once at startup before any connection exists,
/// so there is no concurrent attacker to race — the openat-only
/// discipline matters for per-build paths derived from an
/// adversary-chosen `build_id`, not here.
// r[impl builder.mountd.orphan-scan]
fn reap_orphans(cfg: &MountdConfig, castore_base: &OwnedFd) {
    for name in list_dir(&cfg.castore_dir) {
        let mnt = cfg.castore_dir.join(&name);
        let _ = umount2(&mnt, MntFlags::MNT_DETACH);
        if let Err(e) = unlinkat(castore_base, name.as_str(), UnlinkatFlags::RemoveDir) {
            warn!(orphan = %mnt.display(), error = %e, "could not remove orphan mountpoint");
        } else {
            info!(orphan = %mnt.display(), "reaped orphan castore mountpoint");
        }
    }
    for name in list_dir(&cfg.staging_dir) {
        let path = cfg.staging_dir.join(&name);
        if let Err(e) = std::fs::remove_dir_all(&path) {
            warn!(orphan = %path.display(), error = %e, "could not remove orphan staging dir");
        } else {
            info!(orphan = %path.display(), "reaped orphan staging dir");
        }
    }
    for root in [&cfg.cache_dir, &cfg.chunks_dir] {
        for ab in list_dir(root) {
            for entry in list_dir(&root.join(&ab)) {
                if entry.ends_with(".promoting") || entry.ends_with(".tmp") {
                    let path = root.join(&ab).join(&entry);
                    let _ = std::fs::remove_file(&path);
                    info!(orphan = %path.display(), "reaped orphan placeholder");
                }
            }
        }
    }
}

/// Child names of a directory, excluding `.`/`..`. Errors and non-UTF-8
/// names are swallowed — every name the daemon itself creates (build
/// ids, hex digests, shard prefixes) is ASCII, so anything else is
/// foreign junk that neither the orphan scan nor the cache sweep has
/// any business touching.
pub(super) fn list_dir(path: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

// ─── Connection handling ───────────────────────────────────────────────

/// Replies waiting to be written to the peer. `Option<OwnedFd>` is the
/// single fd a `Mounted` reply carries; everything else sends `None`.
type ReplyTx = mpsc::UnboundedSender<(Reply, Option<OwnedFd>)>;

async fn handle_conn(shared: Arc<Shared>, fd: OwnedFd) -> anyhow::Result<()> {
    // Access control already happened at connect(2): the socket file is
    // root:allowed_gid mode 0660 (see `bind_socket`), and executor pods
    // reach it only through the controller-mounted hostPath directory.
    // No peer-credential check here — SO_PEERCRED is read for the
    // accepted-connection log line, the one-connection-per-uid binding
    // below, and the ownership operations (staging chown, fuse mount
    // `user_id`/`group_id`), which the kernel interprets as host-side
    // credentials.
    let creds = getsockopt(&fd, sockopt::PeerCredentials).context("SO_PEERCRED")?;
    // Every fallible setup step happens BEFORE the uid is registered:
    // an early `?` between registration and the teardown call at the
    // bottom would leave the uid in `live_uids` forever, permanently
    // locking that build's uid out of the broker.
    let async_fd = AsyncFd::new(fd)?;
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<(Reply, Option<OwnedFd>)>();

    // One live connection per peer uid. k8s user namespaces give each
    // pod a distinct host-uid range, so this binds one connection per
    // build; a sandbox-escaped build cannot open a second.
    // r[impl builder.mountd.uid-bound]
    {
        let holder = {
            let mut uids = shared.live_uids.lock().ignore_poison();
            match uids.entry(creds.uid()) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(String::new());
                    None
                }
                std::collections::hash_map::Entry::Occupied(o) => Some(o.get().clone()),
            }
        };
        if let Some(holder) = holder {
            // r[impl builder.mountd.uid-busy-typed]
            // Read exactly one frame so the rejection can carry the
            // request's seq (a reply sent before the client's first
            // request has no waiter and is dropped), then close. The
            // read deadline bounds the doomed connection's lifetime,
            // preserving uid-bound's no-standing-second-connection
            // property.
            metrics::counter!("rio_mountd_uid_busy_total").increment(1);
            warn!(
                uid = creds.uid(),
                holder_build_id = %holder,
                "rejecting connection: uid already connected; answering \
                 first request with a typed retryable rejection"
            );
            if let Ok(Ok(frame)) =
                tokio::time::timeout(UID_BUSY_REPLY_DEADLINE, read_frame(&async_fd)).await
                && let Ok(req) = proto::decode::<Request>(&frame.bytes)
            {
                let holder = if holder.is_empty() {
                    "an unmounted connection".to_owned()
                } else {
                    format!("build {holder}")
                };
                let reply = Reply {
                    seq: req.seq,
                    resp: Resp::Err(ErrKind::Retryable(format!(
                        "uid {} already has a live mountd connection (held by \
                         {holder}; previous teardown likely in flight) — retry",
                        creds.uid()
                    ))),
                };
                let _ = write_reply(&async_fd, &reply, None).await;
            }
            return Ok(());
        }
    }
    metrics::gauge!("rio_mountd_connections_current").increment(1.0);
    info!(uid = creds.uid(), gid = creds.gid(), "connection accepted");

    let mut state = ConnState {
        peer_uid: creds.uid(),
        peer_gid: creds.gid(),
        kept: None,
        build_id: None,
        staging_dirfd: None,
        staging_chunks_dirfd: None,
        projid: None,
        live_backing_ids: 0,
        promote_slots: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_PROMOTES_PER_CONN)),
    };

    let result = loop {
        tokio::select! {
            // Writer half: serialize replies (inline and spawn_blocking
            // alike) onto the socket one datagram at a time.
            Some((reply, fd_to_send)) = reply_rx.recv() => {
                if let Err(e) = write_reply(&async_fd, &reply, fd_to_send).await {
                    break Err(e);
                }
            }
            // Reader half: one datagram = one request.
            recv = read_frame(&async_fd) => {
                let frame = match recv {
                    Ok(f) => f,
                    Err(FrameError::Eof) => break Ok(()),
                    Err(e) => break Err(anyhow::anyhow!(e)),
                };
                handle_frame(&shared, &mut state, frame, &reply_tx).await;
            }
        }
    };

    teardown(&shared, &mut state);
    metrics::gauge!("rio_mountd_connections_current").decrement(1.0);
    result
}

async fn read_frame(fd: &AsyncFd<OwnedFd>) -> Result<proto::RecvFrame, FrameError> {
    loop {
        let mut guard = fd.readable().await.map_err(FrameError::Io)?;
        match guard.try_io(|inner| {
            proto::recv_frame(inner.get_ref().as_raw_fd()).map_err(|e| match e {
                FrameError::Io(io) => io,
                other => std::io::Error::other(other),
            })
        }) {
            Err(_would_block) => continue,
            Ok(Ok(frame)) => return Ok(frame),
            Ok(Err(e)) => {
                return Err(match e.downcast::<FrameError>() {
                    Ok(inner) => inner,
                    Err(io) => FrameError::Io(io),
                });
            }
        }
    }
}

/// Send one reply datagram. `fd_to_send` is dropped on return — once
/// `sendmsg` succeeds the kernel holds its own reference for the
/// in-flight `SCM_RIGHTS` message, so the daemon's copy is redundant.
async fn write_reply(
    fd: &AsyncFd<OwnedFd>,
    reply: &Reply,
    fd_to_send: Option<OwnedFd>,
) -> anyhow::Result<()> {
    let bytes = proto::encode(reply).context("encode reply")?;
    let fds: Vec<RawFd> = fd_to_send.iter().map(AsRawFd::as_raw_fd).collect();
    loop {
        let mut guard = fd.writable().await?;
        match guard.try_io(|inner| {
            proto::send_frame(inner.get_ref().as_raw_fd(), &bytes, &fds).map_err(|e| match e {
                FrameError::Io(io) => io,
                other => std::io::Error::other(other),
            })
        }) {
            Err(_would_block) => continue,
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(e)) => return Err(e.into()),
        }
    }
}

/// Dispatch one request. Replies are pushed onto `reply_tx` — inline
/// for the cheap ops, from a `spawn_blocking` task for promotes.
async fn handle_frame(
    shared: &Arc<Shared>,
    state: &mut ConnState,
    frame: proto::RecvFrame,
    reply_tx: &ReplyTx,
) {
    let req: Request = match proto::decode(&frame.bytes) {
        Ok(r) => r,
        Err(e) => {
            // Without a decoded seq there is nothing to correlate the
            // error to; seq 0 by convention.
            let _ = reply_tx.send((
                Reply {
                    seq: 0,
                    resp: Resp::Err(ErrKind::Retryable(format!("bad frame: {e}"))),
                },
                None,
            ));
            return;
        }
    };
    let seq = req.seq;
    let reply = |resp: Resp, fd: Option<OwnedFd>| {
        let _ = reply_tx.send((Reply { seq, resp }, fd));
    };

    // Every request other than Mount requires a completed Mount.
    if state.kept.is_none() && !matches!(req.req, Req::Mount { .. }) {
        reply(Resp::Err(ErrKind::Retryable("not mounted".into())), None);
        return;
    }

    // Request→reply latency. For the inline ops the recording happens
    // at the end of this function; for promotes the timer travels into
    // the spawned task and is recorded when the batch reply is sent —
    // recording at spawn time would measure tokio dispatch latency, not
    // the copy+hash the metric exists to observe.
    let timer = std::time::Instant::now();
    let op = match &req.req {
        Req::Mount { .. } => "mount",
        Req::BackingOpen => "backing_open",
        Req::BackingClose { .. } => "backing_close",
        Req::Promote { .. } => "promote",
        Req::PromoteChunks { .. } => "promote_chunks",
    };

    match req.req {
        Req::Mount { build_id } => {
            let (resp, fd) = handle_mount(shared, state, build_id);
            reply(resp, fd);
        }
        Req::BackingOpen => {
            let resp = if state.live_backing_ids >= MAX_LIVE_BACKING_IDS {
                Resp::Err(ErrKind::Retryable(format!(
                    "live backing-id cap reached ({MAX_LIVE_BACKING_IDS})"
                )))
            } else {
                match frame.fds.first() {
                    None => Resp::Err(ErrKind::Retryable("BackingOpen frame carried no fd".into())),
                    // Never register the client's fd directly — see
                    // `reopen_backing_readonly`.
                    Some(client_fd) => match reopen_backing_readonly(client_fd.as_fd())
                        .map_err(|e| format!("backing fd reopen: {e}"))
                        .and_then(|readonly| {
                            backing_open(
                                state.kept.as_ref().expect("checked above").as_fd(),
                                readonly.as_fd(),
                            )
                            .map_err(|e| format!("BACKING_OPEN: {e}"))
                        }) {
                        Ok(id) => {
                            state.live_backing_ids += 1;
                            Resp::BackingId(id)
                        }
                        Err(e) => Resp::Err(ErrKind::Retryable(e)),
                    },
                }
            };
            // `frame.fds` and the read-only re-open drop here — the
            // kernel holds its own reference to the backing file for
            // the registered id.
            reply(resp, None);
        }
        Req::BackingClose { backing_id } => {
            let resp = match backing_close(
                state.kept.as_ref().expect("checked above").as_fd(),
                backing_id,
            ) {
                Ok(()) => {
                    state.live_backing_ids = state.live_backing_ids.saturating_sub(1);
                    Resp::Ok
                }
                Err(e) => Resp::Err(ErrKind::Retryable(format!("BACKING_CLOSE: {e}"))),
            };
            reply(resp, None);
        }
        Req::Promote { digest } => {
            let Some(staging) = state.staging_dirfd.clone() else {
                reply(Resp::Err(ErrKind::Retryable("not mounted".into())), None);
                return;
            };
            let Some(slot) = claim_promote_slot(state, &reply) else {
                return;
            };
            spawn_promote(
                shared,
                reply_tx.clone(),
                PromoteJob {
                    seq,
                    staging,
                    dest: BaseDir::Cache,
                    digests: vec![digest],
                    timer,
                    slot,
                },
            );
            return;
        }
        Req::PromoteChunks { chunk_digests } => {
            if chunk_digests.len() > PROMOTE_CHUNKS_MAX {
                reply(Resp::Err(ErrKind::BatchTooLarge), None);
                return;
            }
            let Some(staging_chunks) = state.staging_chunks_dirfd.clone() else {
                reply(Resp::Err(ErrKind::Retryable("not mounted".into())), None);
                return;
            };
            let Some(slot) = claim_promote_slot(state, &reply) else {
                return;
            };
            spawn_promote(
                shared,
                reply_tx.clone(),
                PromoteJob {
                    seq,
                    staging: staging_chunks,
                    dest: BaseDir::Chunks,
                    digests: chunk_digests,
                    timer,
                    slot,
                },
            );
            return;
        }
    }
    metrics::histogram!("rio_mountd_request_seconds", "op" => op)
        .record(timer.elapsed().as_secs_f64());
}

/// Per-connection backpressure for promote work: take an in-flight slot
/// or reject the request. Returns `None` after queueing the rejection
/// reply — the caller just returns.
fn claim_promote_slot(
    state: &ConnState,
    reply: &impl Fn(Resp, Option<OwnedFd>),
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match Arc::clone(&state.promote_slots).try_acquire_owned() {
        Ok(slot) => Some(slot),
        Err(_) => {
            metrics::counter!("rio_mountd_promote_reject_total", "reason" => "backlog")
                .increment(1);
            reply(
                Resp::Err(ErrKind::Retryable(format!(
                    "promote backlog cap reached ({MAX_INFLIGHT_PROMOTES_PER_CONN} in flight)"
                ))),
                None,
            );
            None
        }
    }
}

/// Which shared destination a promote writes into.
enum BaseDir {
    Cache,
    Chunks,
}

impl BaseDir {
    /// The `op` label of the request that targets this destination,
    /// for `rio_mountd_request_seconds`.
    fn op(&self) -> &'static str {
        match self {
            BaseDir::Cache => "promote",
            BaseDir::Chunks => "promote_chunks",
        }
    }
}

/// One accepted promote batch travelling from [`handle_frame`] to the
/// spawned worker task.
struct PromoteJob {
    seq: u32,
    /// Per-build staging directory holding the sources (files or
    /// chunks, matching `dest`).
    staging: Arc<OwnedFd>,
    dest: BaseDir,
    digests: Vec<[u8; 32]>,
    /// Started when the request frame was decoded; recorded when the
    /// batch reply is queued.
    timer: std::time::Instant,
    /// Held until the reply is queued so the per-connection cap counts
    /// queued and running work alike.
    slot: tokio::sync::OwnedSemaphorePermit,
}

/// Run a promote batch on the blocking pool, bounded by the process-wide
/// semaphore, and push the single batch reply when it finishes.
fn spawn_promote(shared: &Arc<Shared>, reply_tx: ReplyTx, job: PromoteJob) {
    let shared = Arc::clone(shared);
    let PromoteJob {
        seq,
        staging,
        dest,
        digests,
        timer,
        slot,
    } = job;
    let op = dest.op();
    // Whole files are bounded by the configured promote ceiling. Chunks
    // are bounded much tighter, at the chunker's hard cap: every
    // legitimate chunk is a FastCDC output, so anything above
    // `FASTCDC_MAX_BYTES` is bogus — and the tighter bound keeps the
    // privileged copy work one accepted `PromoteChunks` batch can demand
    // at `PROMOTE_CHUNKS_MAX × FASTCDC_MAX_BYTES` (16 MiB) instead of
    // `PROMOTE_CHUNKS_MAX × max_promote_bytes` (256 GiB at the default
    // ceiling).
    let max_bytes = match dest {
        BaseDir::Cache => shared.cfg.max_promote_bytes,
        BaseDir::Chunks => shared.cfg.max_promote_bytes.min(FASTCDC_MAX_BYTES as u64),
    };
    tokio::spawn(async move {
        // The connection's in-flight slot is released only when this
        // task ends, i.e. after the reply has been queued.
        let _slot = slot;
        let _permit = shared
            .promote_sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        metrics::gauge!("rio_mountd_promote_inflight").increment(1.0);
        let result = tokio::task::spawn_blocking(move || {
            let dest_root = match dest {
                BaseDir::Cache => &shared.cache_base,
                BaseDir::Chunks => &shared.chunks_base,
            };
            let mut total = 0u64;
            for digest in &digests {
                match promote_one(staging.as_fd(), dest_root.as_fd(), digest, max_bytes) {
                    Ok(n) => total += n,
                    Err(e) => return Err(e),
                }
            }
            Ok(total)
        })
        .await;
        metrics::gauge!("rio_mountd_promote_inflight").decrement(1.0);
        let resp = match result {
            Ok(Ok(bytes)) => {
                metrics::counter!("rio_mountd_promote_bytes_total").increment(bytes);
                Resp::Ok
            }
            Ok(Err(kind)) => {
                metrics::counter!(
                    "rio_mountd_promote_reject_total",
                    "reason" => reject_reason(&kind)
                )
                .increment(1);
                Resp::Err(kind)
            }
            Err(join) => Resp::Err(ErrKind::Retryable(format!("promote task panicked: {join}"))),
        };
        metrics::histogram!("rio_mountd_request_seconds", "op" => op)
            .record(timer.elapsed().as_secs_f64());
        let _ = reply_tx.send((Reply { seq, resp }, None));
    });
}

fn reject_reason(kind: &ErrKind) -> &'static str {
    match kind {
        ErrKind::DigestMismatch => "mismatch",
        ErrKind::NotRegular => "not-regular",
        ErrKind::TooLarge => "too-large",
        ErrKind::RaceTimeout => "race-timeout",
        _ => "other",
    }
}

/// `Mount{build_id}`: claim the id, fuse-mount the per-build castore
/// mountpoint, set up staging with a kernel-enforced quota, and hand
/// the `/dev/fuse` fd back.
// r[impl builder.mountd.fuse-handoff]
// r[impl builder.mountd.one-mount]
// r[impl builder.mountd.build-id-unique]
// r[impl builder.mountd.staging-quota]
fn handle_mount(
    shared: &Arc<Shared>,
    state: &mut ConnState,
    build_id: String,
) -> (Resp, Option<OwnedFd>) {
    if state.kept.is_some() {
        return (Resp::Err(ErrKind::AlreadyMounted), None);
    }
    if !validate_build_id(&build_id) {
        return (Resp::Err(ErrKind::BadBuildId), None);
    }
    // Claim the build_id process-wide before touching the filesystem so
    // two connections racing on the same id cannot both mkdir.
    if !shared
        .live_build_ids
        .lock()
        .ignore_poison()
        .insert(build_id.clone())
    {
        return (Resp::Err(ErrKind::DuplicateBuildId), None);
    }
    // From here on, every failure must release the claim.
    match mount_build(shared, state, &build_id) {
        Ok((fuse_fd, quota)) => {
            // Record the holder so a later uid-colliding connect can
            // name it in its rejection (see `live_uids` doc).
            if let Some(slot) = shared
                .live_uids
                .lock()
                .ignore_poison()
                .get_mut(&state.peer_uid)
            {
                slot.clone_from(&build_id);
            }
            state.build_id = Some(build_id);
            (
                Resp::Mounted {
                    staging_quota_bytes: quota,
                },
                Some(fuse_fd),
            )
        }
        Err(e) => {
            shared
                .live_build_ids
                .lock()
                .ignore_poison()
                .remove(&build_id);
            // Best-effort cleanup of whatever mount_build got through.
            cleanup_build_dirs(shared, &build_id);
            warn!(build_id, error = %e, "Mount failed");
            (Resp::Err(ErrKind::Retryable(format!("mount: {e}"))), None)
        }
    }
}

/// The fallible body of `Mount`. Returns the fd to send and the applied
/// quota. The caller owns claim-release and directory cleanup on error.
fn mount_build(
    shared: &Arc<Shared>,
    state: &mut ConnState,
    build_id: &str,
) -> anyhow::Result<(OwnedFd, u64)> {
    let cfg = &shared.cfg;

    // ── /dev/fuse + mountpoint.
    let fuse_fd: OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .context("open /dev/fuse")?
        .into();
    let kept = fuse_fd.try_clone().context("dup /dev/fuse")?;
    match mkdirat(
        &shared.castore_base,
        build_id,
        Mode::from_bits_truncate(0o755),
    ) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
        Err(e) => return Err(e).context("mkdir castore mountpoint"),
    }
    let mnt = cfg.castore_dir.join(build_id);
    // rootmode=40555: directory, world-readable. allow_other +
    // default_permissions so processes in the build's user namespace
    // (not just the FUSE server's uid) can traverse it and the kernel
    // enforces the mode bits the server reports.
    let data = format!(
        "fd={},rootmode=40555,user_id={},group_id={},allow_other,default_permissions",
        fuse_fd.as_raw_fd(),
        state.peer_uid,
        state.peer_gid,
    );
    mount(
        Some("rio-castore"),
        &mnt,
        Some("fuse.rio-castore"),
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
        Some(data.as_str()),
    )
    .with_context(|| format!("mount fuse at {}", mnt.display()))?;

    // ── Staging: 0700, owned by the build's uid, kernel-quota'd.
    match mkdirat(
        &shared.staging_base,
        build_id,
        Mode::from_bits_truncate(0o700),
    ) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
        Err(e) => return Err(e).context("mkdir staging"),
    }
    let staging_dirfd = openat(
        &shared.staging_base,
        build_id,
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .context("open staging dir")?;
    fchownat(
        &staging_dirfd,
        ".",
        Some(Uid::from_raw(state.peer_uid)),
        Some(Gid::from_raw(state.peer_gid)),
        AtFlags::empty(),
    )
    .context("chown staging dir")?;
    mkdirat(&staging_dirfd, "chunks", Mode::from_bits_truncate(0o700))
        .context("mkdir staging/chunks")?;
    fchownat(
        &staging_dirfd,
        "chunks",
        Some(Uid::from_raw(state.peer_uid)),
        Some(Gid::from_raw(state.peer_gid)),
        AtFlags::empty(),
    )
    .context("chown staging/chunks")?;
    let staging_chunks_dirfd = openat(
        &staging_dirfd,
        "chunks",
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .context("open staging/chunks")?;

    // ── Kernel-enforced staging quota. The projid is mountd-assigned
    // and monotonic — never derived from the adversary-chosen build_id,
    // so a build cannot collide into a victim's quota bucket.
    let mut applied_quota = 0;
    if cfg.staging_quota_bytes > 0 {
        let projid = crate::quota::MOUNTD_PROJID_BASE
            + (shared.next_projid.fetch_add(1, Ordering::Relaxed)
                % (crate::quota::MOUNTD_PROJID_CEILING - crate::quota::MOUNTD_PROJID_BASE));
        apply_project_quota(&staging_dirfd, projid, cfg.staging_quota_bytes)
            .context("apply staging project quota")?;
        state.projid = Some(projid);
        applied_quota = cfg.staging_quota_bytes;
    }

    state.kept = Some(kept);
    state.staging_dirfd = Some(Arc::new(staging_dirfd));
    state.staging_chunks_dirfd = Some(Arc::new(staging_chunks_dirfd));
    info!(
        build_id,
        uid = state.peer_uid,
        quota = applied_quota,
        "mounted castore FUSE, fd handed off"
    );
    Ok((fuse_fd, applied_quota))
}

/// Best-effort removal of the per-build castore mountpoint and staging
/// tree. Used by the Mount error path, where the staging tree is empty
/// or nearly so — connection teardown instead quarantine-renames the
/// tree and deletes it off the claim-release path (see [`teardown`]).
fn cleanup_build_dirs(shared: &Arc<Shared>, build_id: &str) {
    let mnt = shared.cfg.castore_dir.join(build_id);
    let _ = umount2(&mnt, MntFlags::MNT_DETACH);
    let _ = unlinkat(&shared.castore_base, build_id, UnlinkatFlags::RemoveDir);
    let _ = std::fs::remove_dir_all(shared.cfg.staging_dir.join(build_id));
}

/// Connection teardown: undo everything `Mount` set up. Runs on every
/// connection exit path (orderly close, error, daemon-side rejection
/// after a successful mount).
///
/// The `build_id` and uid claims are released after metadata-only
/// syscalls only — never across the O(staging size) recursive delete,
/// which the next build's Mount on this pod would otherwise collide
/// with. The deep delete runs detached; if it fails or the daemon dies
/// first, the sweep's `REAP_PREFIX` arm retries it.
// r[impl builder.mountd.teardown-fast-release]
fn teardown(shared: &Arc<Shared>, state: &mut ConnState) {
    if let Some(build_id) = state.build_id.take() {
        let mnt = shared.cfg.castore_dir.join(&build_id);
        let _ = umount2(&mnt, MntFlags::MNT_DETACH);
        let _ = unlinkat(
            &shared.castore_base,
            build_id.as_str(),
            UnlinkatFlags::RemoveDir,
        );
        if let (Some(projid), Some(staging)) = (state.projid, state.staging_dirfd.as_ref()) {
            // Release the quota record so the projid slot doesn't
            // accumulate dead accounting. The tree is about to be
            // quarantined and deleted, so the recorded usage is
            // irrelevant either way.
            let _ = apply_project_quota(staging.as_ref(), projid, 0);
        }
        // Quarantine BEFORE releasing the build_id claim: once the
        // claim is gone a re-`Mount` of the same build_id may mkdir a
        // fresh staging dir at the original name, and a later rename
        // would steal the new build's tree (same ordering argument as
        // `sweep::reap_dead_staging`, where the rename runs under the
        // liveness lock).
        let quarantined = super::sweep::quarantine_staging(&shared.cfg.staging_dir, &build_id);
        shared
            .live_build_ids
            .lock()
            .ignore_poison()
            .remove(&build_id);
        info!(build_id, "connection closed, build torn down");
        if let Some(doomed) = quarantined {
            let deep_delete = move || {
                let _ = std::fs::remove_dir_all(&doomed);
            };
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn_blocking(deep_delete);
                }
                // No runtime (unit tests calling teardown directly):
                // delete inline.
                Err(_) => deep_delete(),
            }
        }
    }
    shared
        .live_uids
        .lock()
        .ignore_poison()
        .remove(&state.peer_uid);
    // Dropping `kept` closes our last reference to the build's
    // /dev/fuse; if the builder's copy is also gone the kernel aborts
    // the FUSE connection and the (already lazily-detached) mount
    // becomes fully dead.
    state.kept = None;
    state.staging_dirfd = None;
    state.staging_chunks_dirfd = None;
}

/// Registers prometheus metric descriptions for the mountd binary. The
/// help strings here are the source for `docs/gen/metrics.json` — see
/// `xtask/src/regen/docs_data.rs::metrics()`.
// r[impl obs.metric.mountd]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};
    describe_histogram!(
        "rio_mountd_request_seconds",
        "UDS request latency (labeled by op: mount/backing_open/backing_close/promote/promote_chunks)"
    );
    describe_counter!(
        "rio_mountd_promote_bytes_total",
        "Bytes verify-copied into the shared caches by Promote/PromoteChunks"
    );
    describe_counter!(
        "rio_mountd_promote_reject_total",
        "Rejected promotes (labeled by reason: mismatch/not-regular/too-large/race-timeout/backlog/other)"
    );
    describe_counter!(
        "rio_mountd_uid_busy_total",
        "Connections rejected because their peer uid already had a live connection (a builder racing the previous build's teardown)"
    );
    describe_gauge!(
        "rio_mountd_promote_inflight",
        "Promote copy tasks currently running"
    );
    describe_gauge!(
        "rio_mountd_connections_current",
        "Live UDS connections (== builds being served on this node)"
    );
    describe_gauge!(
        "rio_mountd_cache_free_bytes",
        "statvfs free bytes on the shared cache filesystem, sampled at the LRU-sweep interval"
    );
    describe_counter!(
        "rio_mountd_cache_evicted_bytes_total",
        "Bytes unlinked by the disk-pressure LRU sweep (labeled by dir: cache/chunks)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    // ─── BackingOpen pins the registration to read-only ────────────────

    // r[verify builder.mountd.backing-readonly]
    /// The fd registered with `FUSE_DEV_IOC_BACKING_OPEN` must be a
    /// fresh O_RDONLY description of the client's file — never the
    /// client's own (possibly writable) description. A compromised
    /// builder sending an O_RDWR fd must not get a writable file
    /// description registered under the broker's root credentials.
    #[test]
    fn backing_fd_is_reopened_readonly() {
        use std::os::unix::fs::FileExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache-entry");
        std::fs::write(&path, b"shared cache bytes").unwrap();
        // What a compromised client would send (the honest one sends
        // O_RDONLY).
        let writable = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        let pinned = reopen_backing_readonly(writable.as_fd()).expect("reopen");

        // F_GETFL: the access mode is O_RDONLY regardless of what the
        // client supplied.
        let fl = nix::fcntl::fcntl(&pinned, nix::fcntl::FcntlArg::F_GETFL).unwrap();
        assert_eq!(
            fl & libc::O_ACCMODE,
            libc::O_RDONLY,
            "registered backing fd must be read-only, F_GETFL = {fl:#o}"
        );

        // Same inode — the pin re-opens the client's file, not some
        // other path.
        let orig = nix::sys::stat::fstat(&writable).unwrap();
        let pin = nix::sys::stat::fstat(&pinned).unwrap();
        assert_eq!((orig.st_dev, orig.st_ino), (pin.st_dev, pin.st_ino));

        // The pinned description still serves reads (this is what the
        // kernel registration needs)...
        let pinned_file = std::fs::File::from(pinned);
        let mut buf = vec![0u8; b"shared cache bytes".len()];
        pinned_file.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"shared cache bytes");

        // ...but cannot write.
        let denied = pinned_file.write_at(b"poison", 0);
        assert!(
            denied.is_err(),
            "write through the pinned fd must fail, got {denied:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"shared cache bytes",
            "the cache entry must be untouched"
        );
    }

    // r[verify builder.mountd.backing-readonly]
    /// The LRU sweep can unlink a cache entry between the client's open
    /// and its BackingOpen request. The read-only re-open must still
    /// work on the unlinked-but-open file — `/proc/self/fd` resolves to
    /// the inode, not the (gone) path — or every open racing the sweep
    /// would degrade to a userspace read for no reason.
    #[test]
    fn backing_fd_reopen_survives_concurrent_unlink() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache-entry");
        std::fs::write(&path, b"swept").unwrap();
        let client = std::fs::File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let pinned = reopen_backing_readonly(client.as_fd())
            .expect("an unlinked-but-open cache entry must still be registrable");
        let st = nix::sys::stat::fstat(&pinned).unwrap();
        assert_eq!(st.st_size, b"swept".len() as i64);
    }

    // r[verify builder.mountd.backing-readonly]
    /// A FIFO smuggled in as the backing fd must not park the
    /// privileged re-open until a writer appears (the same DoS
    /// `promote_one` defends against with O_NONBLOCK). The kernel's
    /// `d_is_reg` check rejects the registration downstream.
    #[test]
    fn backing_fd_reopen_does_not_block_on_fifos() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fifo");
        nix::unistd::mkfifo(&path, Mode::from_bits_truncate(0o600)).unwrap();
        let client = nix::fcntl::open(
            &path,
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .unwrap();

        // Must return, not hang.
        reopen_backing_readonly(client.as_fd()).expect("re-open of a FIFO must not block");
    }

    // r[verify builder.mountd.build-id-validated]
    #[test]
    fn build_id_validation() {
        assert!(validate_build_id("b-123_ABC"));
        assert!(validate_build_id(&"a".repeat(64)));
        assert!(!validate_build_id(""));
        assert!(!validate_build_id(&"a".repeat(65)));
        assert!(!validate_build_id("../escape"));
        assert!(!validate_build_id("a/b"));
        assert!(!validate_build_id("a.b"));
        assert!(!validate_build_id("a b"));
        assert!(!validate_build_id("a\0b"));
        assert!(!validate_build_id("ünïcode"));
    }

    /// A staging dir and a cache root in a tempdir, plus the path
    /// arithmetic every promote test repeats.
    struct Fx {
        _tmp: tempfile::TempDir,
        staging: PathBuf,
        cache: PathBuf,
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let staging = tmp.path().join("staging");
            let cache = tmp.path().join("cache");
            std::fs::create_dir_all(&staging).unwrap();
            std::fs::create_dir_all(&cache).unwrap();
            Self {
                _tmp: tmp,
                staging,
                cache,
            }
        }

        /// Write `content` under its own digest name in staging.
        fn stage(&self, content: &[u8]) -> [u8; 32] {
            let digest = *blake3::hash(content).as_bytes();
            std::fs::write(self.staged(&digest), content).unwrap();
            digest
        }

        fn promote(&self, digest: &[u8; 32]) -> Result<u64, ErrKind> {
            self.promote_max(digest, DEFAULT_MAX_PROMOTE_BYTES)
        }

        fn promote_max(&self, digest: &[u8; 32], max: u64) -> Result<u64, ErrKind> {
            let staging = dirfd(&self.staging);
            let cache = dirfd(&self.cache);
            promote_one(staging.as_fd(), cache.as_fd(), digest, max)
        }

        fn promote_hooked(
            &self,
            digest: &[u8; 32],
            before_publish: impl FnOnce(),
        ) -> Result<u64, ErrKind> {
            let staging = dirfd(&self.staging);
            let cache = dirfd(&self.cache);
            promote_one_hooked(
                staging.as_fd(),
                cache.as_fd(),
                digest,
                DEFAULT_MAX_PROMOTE_BYTES,
                before_publish,
            )
        }

        fn staged(&self, digest: &[u8; 32]) -> PathBuf {
            self.staging.join(hex::encode(digest))
        }

        fn published(&self, digest: &[u8; 32]) -> PathBuf {
            self.cache.join(shard(digest)).join(hex::encode(digest))
        }

        fn placeholder(&self, digest: &[u8; 32]) -> PathBuf {
            self.cache
                .join(shard(digest))
                .join(format!("{}.promoting", hex::encode(digest)))
        }
    }

    fn dirfd(p: &Path) -> OwnedFd {
        nix::fcntl::open(p, OFlag::O_DIRECTORY | OFlag::O_CLOEXEC, Mode::empty()).unwrap()
    }

    /// Happy path: staged bytes land at `cache/ab/<hex>` and the
    /// staging entry is consumed.
    // r[verify builder.mountd.promote-verified]
    #[test]
    fn promote_happy_path() {
        let fx = Fx::new();
        let content = b"hello castore".repeat(1000);
        let digest = fx.stage(&content);

        let n = fx.promote(&digest).expect("promote");

        assert_eq!(n, content.len() as u64);
        assert_eq!(std::fs::read(fx.published(&digest)).unwrap(), content);
        assert!(!fx.staged(&digest).exists(), "staging entry consumed");
        assert!(!fx.placeholder(&digest).exists(), "no leaked placeholder");
    }

    /// The published cache entry must be world-readable even under a
    /// hardened umask — builder uids open it O_RDONLY for BACKING_OPEN.
    /// Safe to mutate the process umask here: nextest runs each test in
    /// its own process.
    #[test]
    fn promote_mode_is_0444_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        nix::sys::stat::umask(Mode::from_bits_truncate(0o077));
        let fx = Fx::new();
        let digest = fx.stage(b"readable by all");

        fx.promote(&digest).unwrap();

        let mode = std::fs::metadata(fx.published(&digest))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o444, "got {mode:o}");
    }

    /// Content that does not hash to the claimed digest is rejected and
    /// nothing becomes visible in the cache.
    // r[verify builder.mountd.promote-verified]
    #[test]
    fn promote_rejects_digest_mismatch() {
        let fx = Fx::new();
        let claimed = *blake3::hash(b"what the builder promised").as_bytes();
        std::fs::write(fx.staged(&claimed), b"what it actually staged").unwrap();

        let r = fx.promote(&claimed);

        assert!(matches!(r, Err(ErrKind::DigestMismatch)), "got {r:?}");
        assert!(!fx.published(&claimed).exists());
        assert!(
            !fx.placeholder(&claimed).exists(),
            "placeholder must be cleaned up on mismatch"
        );
    }

    /// A symlink staged under the digest name must not be followed by
    /// the privileged copy — O_NOFOLLOW fails on the link itself, the
    /// target is never resolved.
    #[test]
    fn promote_rejects_symlink() {
        let fx = Fx::new();
        let digest = *blake3::hash(b"irrelevant").as_bytes();
        symlink("/etc/shadow", fx.staged(&digest)).unwrap();

        let r = fx.promote(&digest);
        assert!(matches!(r, Err(ErrKind::NotRegular)), "got {r:?}");
    }

    /// A FIFO staged under the digest name must neither park the
    /// privileged `open(2)` until a writer appears nor reach the copy
    /// loop.
    #[test]
    fn promote_rejects_fifo() {
        let fx = Fx::new();
        let digest = *blake3::hash(b"fifo").as_bytes();
        nix::unistd::mkfifo(&fx.staged(&digest), Mode::from_bits_truncate(0o600)).unwrap();

        let r = fx.promote(&digest);
        assert!(matches!(r, Err(ErrKind::NotRegular)), "got {r:?}");
    }

    /// st_size above the ceiling is rejected before a single byte is
    /// read.
    #[test]
    fn promote_rejects_too_large() {
        let fx = Fx::new();
        let content = vec![7u8; 4096];
        let digest = fx.stage(&content);

        let r = fx.promote_max(&digest, content.len() as u64 - 1);
        assert!(matches!(r, Err(ErrKind::TooLarge)), "got {r:?}");
    }

    /// The cache entry is exactly the bytes that were hashed: a source
    /// whose size changes between `fstat` and the copy loop is a
    /// mismatch, not a short-but-"published" entry. Extending the file
    /// with a sparse tail after staging makes `st_size` disagree with
    /// the digest the same way a concurrent shrink would.
    // r[verify builder.mountd.promote-bounded-copy]
    #[test]
    fn promote_rejects_size_change_after_stage() {
        let fx = Fx::new();
        let content = vec![3u8; 256 * 1024];
        let digest = fx.stage(&content);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(fx.staged(&digest))
            .unwrap();
        f.set_len(content.len() as u64 + 4096).unwrap();
        drop(f);

        let r = fx.promote(&digest);
        assert!(matches!(r, Err(ErrKind::DigestMismatch)), "got {r:?}");
    }

    /// Losing the `.promoting` race against a promote that already
    /// published returns success without copying.
    #[test]
    fn promote_race_already_promoted() {
        let fx = Fx::new();
        let content = b"already there";
        let digest = fx.stage(content);
        // Simulate the winner: final name exists AND a racer is still
        // mid-flight on the placeholder.
        std::fs::create_dir_all(fx.published(&digest).parent().unwrap()).unwrap();
        std::fs::write(fx.published(&digest), content).unwrap();
        std::fs::write(fx.placeholder(&digest), b"").unwrap();

        let r = fx.promote(&digest);
        assert!(matches!(r, Ok(0)), "got {r:?}");
    }

    /// A `.promoting` claim nobody holds a flock on has no live owner
    /// (a leftover of a crashed daemon): it is reclaimed so promotes of
    /// that digest do not stay blocked until the next daemon restart.
    #[test]
    fn promote_reclaims_claim_with_no_live_owner() {
        let fx = Fx::new();
        let content = b"contended";
        let digest = fx.stage(content);
        std::fs::create_dir_all(fx.placeholder(&digest).parent().unwrap()).unwrap();
        std::fs::write(fx.placeholder(&digest), b"").unwrap();

        let r = fx.promote(&digest);

        assert!(matches!(r, Ok(n) if n == content.len() as u64), "got {r:?}");
        assert_eq!(std::fs::read(fx.published(&digest)).unwrap(), content);
        assert!(
            !fx.placeholder(&digest).exists(),
            "reclaimed claim must not leak"
        );
    }

    /// Whatever happens to the shared `.promoting` claim while a
    /// promote is mid-flight (a concurrent promote wrongly deciding it
    /// is stale, reclaiming it, and writing its own in-progress bytes
    /// under that name), the entry published under the final cache name
    /// must be exactly the bytes this promote verified — never someone
    /// else's unverified file.
    // r[verify builder.mountd.promote-verified]
    #[test]
    fn promote_publishes_only_the_inode_it_verified() {
        let fx = Fx::new();
        let content = b"verified payload".repeat(1024);
        let digest = fx.stage(&content);

        let placeholder = fx.placeholder(&digest);
        let r = fx.promote_hooked(&digest, || {
            // A racer that decided our claim was stale: it unlinks the
            // placeholder and re-creates it as its own (unverified,
            // partially written) destination.
            std::fs::remove_file(&placeholder).unwrap();
            std::fs::write(&placeholder, b"racer's unverified partial bytes").unwrap();
        });

        assert!(matches!(r, Ok(n) if n == content.len() as u64), "got {r:?}");
        assert_eq!(
            std::fs::read(fx.published(&digest)).unwrap(),
            content,
            "published entry must be the verified bytes, not the racer's"
        );
    }

    /// A live promote may spend arbitrarily long inside a single read,
    /// write or fsync (contended disk, cgroup IO throttling) without
    /// ever touching its `.promoting` claim. A concurrent promote of
    /// the same digest must keep waiting (and eventually time out) — it
    /// must never reclaim the claim of an owner that is still alive,
    /// however old the placeholder looks.
    // r[verify builder.mountd.promote-verified]
    #[test]
    fn promote_waits_for_live_owner_regardless_of_claim_age() {
        let fx = Fx::new();
        let digest = fx.stage(b"contended");
        std::fs::create_dir_all(fx.placeholder(&digest).parent().unwrap()).unwrap();
        std::fs::write(fx.placeholder(&digest), b"").unwrap();
        // Simulate the live-but-stalled owner: an exclusive flock held
        // on the claim for the whole test. Make the claim look ancient
        // so any age-based heuristic would (wrongly) reclaim it.
        let owner = std::fs::File::open(fx.placeholder(&digest)).unwrap();
        let _owner_lock =
            nix::fcntl::Flock::lock(owner, nix::fcntl::FlockArg::LockExclusive).unwrap();
        age_path(&fx.placeholder(&digest), 600);

        let start = std::time::Instant::now();
        let r = fx.promote(&digest);

        assert!(matches!(r, Err(ErrKind::RaceTimeout)), "got {r:?}");
        assert!(
            start.elapsed() >= PROMOTE_RACE_WAIT,
            "the loser must wait the full race window for the live owner"
        );
        assert!(
            fx.placeholder(&digest).exists(),
            "a live owner's claim must not be reclaimed"
        );
        assert!(!fx.published(&digest).exists());
    }

    /// Everything needed to drive [`handle_frame`] directly: a tempdir
    ///-backed [`Shared`], a mounted-looking [`ConnState`], and the
    /// reply channel the connection task would own. `promote_permits`
    /// sizes the process-wide copy semaphore — `0` parks every accepted
    /// promote before it does any work, a non-zero count lets promotes
    /// run to completion.
    struct ConnFx {
        _tmp: tempfile::TempDir,
        shared: Arc<Shared>,
        state: ConnState,
        reply_tx: ReplyTx,
        reply_rx: mpsc::UnboundedReceiver<(Reply, Option<OwnedFd>)>,
    }

    impl ConnFx {
        fn new(promote_permits: usize) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let sub = |name: &str| {
                let p = tmp.path().join(name);
                std::fs::create_dir_all(&p).unwrap();
                p
            };
            let staging_dir = sub("staging");
            std::fs::create_dir_all(staging_dir.join("chunks")).unwrap();
            let cfg = MountdConfig {
                socket_path: tmp.path().join("mountd.sock"),
                castore_dir: sub("castore"),
                staging_dir,
                cache_dir: sub("cache"),
                chunks_dir: sub("chunks"),
                staging_quota_bytes: 0,
                max_promote_bytes: DEFAULT_MAX_PROMOTE_BYTES,
                allowed_gid: 0,
            };
            let shared = Arc::new(Shared {
                castore_base: dirfd(&cfg.castore_dir),
                staging_base: dirfd(&cfg.staging_dir),
                cache_base: dirfd(&cfg.cache_dir),
                chunks_base: dirfd(&cfg.chunks_dir),
                live_uids: Mutex::new(HashMap::new()),
                live_build_ids: Arc::new(Mutex::new(HashSet::new())),
                next_projid: AtomicU32::new(1),
                promote_sem: Arc::new(tokio::sync::Semaphore::new(promote_permits)),
                cfg,
            });
            let state = ConnState {
                peer_uid: 0,
                peer_gid: 0,
                // Any fd satisfies the mounted-connection check; Promote
                // never touches it.
                kept: Some(dirfd(tmp.path())),
                build_id: None,
                staging_dirfd: Some(Arc::new(dirfd(&shared.cfg.staging_dir))),
                staging_chunks_dirfd: Some(Arc::new(dirfd(&shared.cfg.staging_dir.join("chunks")))),
                projid: None,
                live_backing_ids: 0,
                promote_slots: Arc::new(tokio::sync::Semaphore::new(
                    MAX_INFLIGHT_PROMOTES_PER_CONN,
                )),
            };
            let (reply_tx, reply_rx) = mpsc::unbounded_channel();
            Self {
                _tmp: tmp,
                shared,
                state,
                reply_tx,
                reply_rx,
            }
        }
    }

    fn promote_frame(seq: u32, digest: [u8; 32]) -> proto::RecvFrame {
        proto::RecvFrame {
            bytes: proto::encode(&Request {
                seq,
                req: Req::Promote { digest },
            })
            .unwrap(),
            fds: Vec::new(),
        }
    }

    fn promote_chunks_frame(seq: u32, chunk_digests: Vec<[u8; 32]>) -> proto::RecvFrame {
        proto::RecvFrame {
            bytes: proto::encode(&Request {
                seq,
                req: Req::PromoteChunks { chunk_digests },
            })
            .unwrap(),
            fds: Vec::new(),
        }
    }

    /// Requests beyond the per-connection in-flight cap must be
    /// rejected immediately instead of queued — see
    /// [`MAX_INFLIGHT_PROMOTES_PER_CONN`] for the abuse class this
    /// closes.
    #[tokio::test]
    async fn promote_backlog_cap_rejects_pipelined_floods() {
        // Zero permits: every accepted promote parks before doing any
        // work, holding the connection's backlog at its maximum
        // deterministically (no timing involved).
        let mut fx = ConnFx::new(0);

        for seq in 0..MAX_INFLIGHT_PROMOTES_PER_CONN as u32 {
            handle_frame(
                &fx.shared,
                &mut fx.state,
                promote_frame(seq, [seq as u8; 32]),
                &fx.reply_tx,
            )
            .await;
        }
        assert!(
            fx.reply_rx.try_recv().is_err(),
            "requests inside the cap must be accepted, not answered inline"
        );

        handle_frame(
            &fx.shared,
            &mut fx.state,
            promote_frame(9999, [0xAA; 32]),
            &fx.reply_tx,
        )
        .await;
        let (reply, _fd) = fx
            .reply_rx
            .try_recv()
            .expect("the request beyond the cap must be rejected immediately");
        assert_eq!(reply.seq, 9999);
        assert!(
            matches!(reply.resp, Resp::Err(ErrKind::Retryable(_))),
            "got {:?}",
            reply.resp
        );
    }

    /// The per-connection cap bounds *in-flight* work, not lifetime
    /// work: a slot taken when a promote is accepted must be released
    /// once its reply is queued. With slots leaking, a well-behaved
    /// connection that promotes more than the cap over its lifetime
    /// would start seeing phantom backlog rejections.
    #[tokio::test]
    async fn promote_slot_released_when_reply_is_queued() {
        // A real permit so every accepted promote runs to completion.
        let mut fx = ConnFx::new(1);

        for seq in 0..(MAX_INFLIGHT_PROMOTES_PER_CONN + 1) as u32 {
            // A fresh source per request: each promote is real work
            // that succeeds, so any non-Ok reply means a slot leaked
            // and the cap misfired as a backlog rejection.
            let content = seq.to_le_bytes();
            let digest = *blake3::hash(&content).as_bytes();
            std::fs::write(fx.shared.cfg.staging_dir.join(hex::encode(digest)), content).unwrap();

            handle_frame(
                &fx.shared,
                &mut fx.state,
                promote_frame(seq, digest),
                &fx.reply_tx,
            )
            .await;
            let (reply, _fd) = fx.reply_rx.recv().await.expect("promote reply");
            assert_eq!(reply.seq, seq);
            assert!(
                matches!(reply.resp, Resp::Ok),
                "promote {seq} got {:?}",
                reply.resp
            );
        }
    }

    /// A staged chunk above `FASTCDC_MAX_BYTES` must be rejected even
    /// though it is far below the whole-file ceiling — see the chunk
    /// bound rationale in [`spawn_promote`].
    #[tokio::test]
    async fn promote_chunks_rejects_chunk_above_chunker_max() {
        let mut fx = ConnFx::new(1);
        let content = vec![0xCD; FASTCDC_MAX_BYTES + 1];
        let digest = *blake3::hash(&content).as_bytes();
        std::fs::write(
            fx.shared
                .cfg
                .staging_dir
                .join("chunks")
                .join(hex::encode(digest)),
            &content,
        )
        .unwrap();

        handle_frame(
            &fx.shared,
            &mut fx.state,
            promote_chunks_frame(7, vec![digest]),
            &fx.reply_tx,
        )
        .await;
        let (reply, _fd) = fx.reply_rx.recv().await.expect("promote_chunks reply");
        assert_eq!(reply.seq, 7);
        assert!(
            matches!(reply.resp, Resp::Err(ErrKind::TooLarge)),
            "got {:?}",
            reply.resp
        );
    }

    /// Set a path's mtime/atime `age` seconds into the past.
    fn age_path(path: &Path, age: u64) {
        let t = nix::sys::time::TimeSpec::from_duration(
            (std::time::SystemTime::now() - Duration::from_secs(age))
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap(),
        );
        nix::sys::stat::utimensat(
            nix::fcntl::AT_FDCWD,
            path,
            &t,
            &t,
            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
    }

    /// Re-promoting an already-published digest succeeds: the fresh
    /// `.promoting` claim overwrites the final name with identical
    /// bytes. Content-addressing makes the overwrite a no-op.
    #[test]
    fn promote_idempotent() {
        let fx = Fx::new();
        let content = b"twice";
        let digest = fx.stage(content);
        fx.promote(&digest).unwrap();

        fx.stage(content);
        fx.promote(&digest).unwrap();

        assert_eq!(std::fs::read(fx.published(&digest)).unwrap(), content);
    }

    /// An error mid-promote must leak neither the `.promoting` claim
    /// nor the temp destination (a leaked claim makes every future
    /// promote of that digest spend the race window before reclaiming
    /// it; a leaked temp wastes cache disk until the next restart).
    #[test]
    fn promote_failure_leaves_no_placeholder() {
        let fx = Fx::new();
        let claimed = *blake3::hash(b"promised").as_bytes();
        std::fs::write(fx.staged(&claimed), b"delivered").unwrap();

        let _ = fx.promote(&claimed);

        let leftovers: Vec<_> = std::fs::read_dir(fx.cache.join(shard(&claimed)))
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    /// Promote racing a writer that keeps appending to the staged file:
    /// the cache must end up with either exactly the hashed content or
    /// nothing — never a successful publish of more (or other) bytes
    /// than were hashed. This is the closest a unit test gets to the VM
    /// test's concurrent-appender subtest without a second process.
    // r[verify builder.mountd.promote-bounded-copy]
    #[test]
    fn promote_bounded_under_concurrent_append() {
        let fx = Fx::new();
        // Large enough that the copy loop runs many iterations.
        let content = vec![9u8; 8 * 1024 * 1024];
        let digest = fx.stage(&content);

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let appender = {
            let path = fx.staged(&digest);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
                while !stop.load(Ordering::Relaxed) {
                    let _ = f.write_all(&[0xEE; 4096]);
                }
            })
        };
        let r = fx.promote(&digest);
        stop.store(true, Ordering::Relaxed);
        appender.join().unwrap();

        match r {
            // The copy finished before the appender's first write
            // landed: the published entry is exactly `content`.
            Ok(n) => {
                assert_eq!(n, content.len() as u64);
                assert_eq!(std::fs::read(fx.published(&digest)).unwrap(), content);
            }
            // The bounded copy read appended bytes inside its st_size
            // window: rejected, nothing published.
            Err(ErrKind::DigestMismatch) => {
                assert!(!fx.published(&digest).exists());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ─── uid collision: typed rejection, not a silent close ────────────

    /// Send one frame on a raw SEQPACKET fd and read one reply,
    /// polling through `EWOULDBLOCK` (the daemon end shares the
    /// socketpair's NONBLOCK flag).
    fn roundtrip_raw(fd: &OwnedFd, req: &Request) -> Option<Reply> {
        let bytes = proto::encode(req).unwrap();
        proto::send_frame(fd.as_raw_fd(), &bytes, &[]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match proto::recv_frame(fd.as_raw_fd()) {
                Ok(frame) => return Some(proto::decode::<Reply>(&frame.bytes).unwrap()),
                Err(FrameError::Eof) => return None,
                Err(FrameError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "no reply within 2s");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("recv: {e:?}"),
            }
        }
    }

    // r[verify builder.mountd.uid-busy-typed]
    /// A connection whose uid already has a live one must get a typed
    /// retryable rejection to its first request — naming the holding
    /// build — instead of the bare close that is indistinguishable
    /// from a daemon crash on the builder side.
    #[tokio::test(flavor = "multi_thread")]
    async fn uid_collision_gets_typed_retryable_reply() {
        let fx = ConnFx::new(0);
        let shared = Arc::clone(&fx.shared);
        // Simulate the holder: this process's uid (what SO_PEERCRED on
        // a socketpair reports) already owns a mounted connection.
        shared
            .live_uids
            .lock()
            .ignore_poison()
            .insert(nix::unistd::getuid().as_raw(), "holder-build".into());

        let (daemon_end, client_end) = nix::sys::socket::socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let conn = tokio::spawn(handle_conn(Arc::clone(&shared), daemon_end));

        let reply = tokio::task::spawn_blocking(move || {
            roundtrip_raw(
                &client_end,
                &Request {
                    seq: 7,
                    req: Req::Mount {
                        build_id: "colliding-build".into(),
                    },
                },
            )
        })
        .await
        .unwrap()
        .expect("a typed rejection, not EOF");

        assert_eq!(reply.seq, 7, "rejection must correlate to the request");
        match reply.resp {
            Resp::Err(ErrKind::Retryable(msg)) => {
                assert!(msg.contains("holder-build"), "names the holder: {msg}");
            }
            other => panic!("expected Retryable rejection, got {other:?}"),
        }
        conn.await.unwrap().unwrap();
        // The doomed connection must not have evicted the holder.
        assert_eq!(
            shared
                .live_uids
                .lock()
                .ignore_poison()
                .get(&nix::unistd::getuid().as_raw())
                .map(String::as_str),
            Some("holder-build")
        );
    }

    // r[verify builder.mountd.teardown-fast-release]
    /// Teardown must release the uid and build_id claims without
    /// waiting for the recursive staging delete, with the original
    /// staging path already quarantine-renamed at release time so a
    /// same-build_id remount cannot lose its fresh tree to the pending
    /// delete. Pins the mechanism's invariants; the defect it guards —
    /// claim-hold DURATION across an O(staging size) delete — is a
    /// timing property a unit test cannot observe, so this test alone
    /// does not discriminate the pre-fix shape.
    #[tokio::test(flavor = "multi_thread")]
    async fn teardown_releases_claims_before_staging_reap() {
        let mut fx = ConnFx::new(0);
        let build_id = "teardown-fast";
        let staging = fx.shared.cfg.staging_dir.join(build_id);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("artifact"), vec![0u8; 1 << 16]).unwrap();
        fx.shared
            .live_build_ids
            .lock()
            .ignore_poison()
            .insert(build_id.to_owned());
        fx.shared
            .live_uids
            .lock()
            .ignore_poison()
            .insert(fx.state.peer_uid, build_id.to_owned());
        fx.state.build_id = Some(build_id.to_owned());
        fx.state.projid = None;

        teardown(&fx.shared, &mut fx.state);

        // Claims released and original path free as soon as teardown
        // returns — NOT after the deep delete completes.
        assert!(
            !fx.shared
                .live_uids
                .lock()
                .ignore_poison()
                .contains_key(&fx.state.peer_uid),
            "uid claim must be released synchronously"
        );
        assert!(
            !fx.shared
                .live_build_ids
                .lock()
                .ignore_poison()
                .contains(build_id),
            "build_id claim must be released synchronously"
        );
        assert!(
            !staging.exists(),
            "staging tree must be quarantine-renamed off the original path"
        );

        // The detached delete (or, if it lost a race, the sweep's
        // REAP_PREFIX arm) must actually remove the quarantined tree.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let leftovers: Vec<_> = std::fs::read_dir(&fx.shared.cfg.staging_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with(".reap."))
                })
                .collect();
            if leftovers.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "quarantined staging tree not deleted: {leftovers:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
