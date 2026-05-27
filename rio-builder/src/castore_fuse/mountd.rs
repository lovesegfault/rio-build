//! `rio-mountd` — the privileged per-node broker for castore-FUSE.
//!
//! The unprivileged builder pod cannot (a) call
//! `FUSE_DEV_IOC_BACKING_OPEN`/`_CLOSE` (init-namespace `CAP_SYS_ADMIN`
//! checks in `fs/fuse/backing.c`) or (b) write the node-shared backing
//! cache (integrity boundary). One DaemonSet per node with
//! `CAP_SYS_ADMIN` brokers exactly those operations and nothing else —
//! it opens no devices and mounts nothing. The builder opens
//! `/dev/fuse` itself, `mount(2)`s it inside its own mount namespace
//! (the kernel requires opener-userns == mounter-userns, and a
//! daemon-side mount could not propagate to builder pods anyway — the
//! P0560 option-(b) decision), serves FUSE on it, assembles its own
//! overlay, and hands the daemon a dup of the fd so the backing-open
//! broker has a same-connection target.
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
//! `BackingOpen` traffic.
// r[impl builder.mountd.concurrency]

use std::collections::HashSet;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::libc;
use nix::mount::{MntFlags, umount2};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, UnixAddr, accept4, bind, getsockopt, listen,
    socket, sockopt,
};
use nix::sys::stat::{Mode, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchownat, unlinkat};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::mountd_proto::{
    self as proto, ErrKind, FrameError, PROMOTE_CHUNKS_MAX, Reply, Req, Request, Resp,
};
use super::sweep;
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

/// A `.promoting` placeholder older than this is a leak from a crashed
/// promote (the owning task can no longer be running) and is reclaimed.
/// Sized as `DEFAULT_MAX_PROMOTE_BYTES / MIN_PROMOTE_THROUGHPUT
/// (50 MiB/s)` — any live promote finishes well inside it.
const PROMOTE_STALE_AFTER: Duration = Duration::from_secs(90);

/// How long a provisionally-admitted connection (token mode, peer gid
/// outside `allowed_gid`) may exist without a successfully
/// token-verified `Mount{}` before the daemon closes it. Sized to one
/// builder-side `mountd_request_timeout` — the legitimate client
/// connects and Mounts immediately, so anything still unauthenticated
/// after this is squatting an fd/task on the world-connectable socket.
const TOKEN_AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Copy-loop buffer. 64 KiB amortizes syscall overhead without holding
/// a large allocation per concurrent promote.
const PROMOTE_BUF: usize = 64 * 1024;

/// Daemon configuration. The binary populates this from clap; tests
/// populate it from a tempdir.
#[derive(Debug, Clone)]
pub struct MountdConfig {
    /// UDS listen path, e.g. `/run/rio-mountd/mountd.sock`. Created
    /// mode 0660 owned `root:allowed_gid`; the parent directory is
    /// created if missing (it lives on a tmpfs `/run` that is wiped
    /// every boot).
    pub socket_path: PathBuf,
    /// Legacy per-build FUSE mountpoint root (`/var/rio/castore`). The
    /// daemon no longer mounts anything here — the builder mounts the
    /// handed-off fd inside its own mount namespace — but the startup
    /// orphan scan still sweeps leftovers from pre-cutover daemons.
    /// The directory is OPTIONAL: it is neither created nor required to
    /// exist (post-cutover deployments don't provision it); the scan
    /// simply skips a missing path.
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
    /// `SO_PEERCRED.gid` allowed to connect. Without a token key
    /// (below) connections from any other gid are dropped before a
    /// single frame is read; with one, a non-matching gid is admitted
    /// only if its first frame is a `Mount{}` carrying a token that
    /// verifies (ADR-022 §P0559).
    pub allowed_gid: u32,
    /// HMAC key file for verifying scheduler-minted Mount-admission
    /// tokens (ADR-022 §P0559). `None` = token mode off: admission is
    /// the gid gate alone and the socket stays 0660 — exactly the
    /// pre-token behavior. `Some` = token mode on: the socket becomes
    /// world-connectable (0666) so `hostUsers: false` executor pods —
    /// whose remapped uids/gids cannot present the host gid — can
    /// reach it, and a connection is admitted EITHER by the gid match
    /// OR by a verifying token in its `Mount{}`.
    ///
    /// MUST be the dedicated mountd key (`RIO_MOUNTD_HMAC_KEY_PATH`),
    /// never the store-facing assignment key — this key lives on every
    /// builder node, and a node compromise must not yield a key the
    /// store trusts.
    pub token_key_path: Option<PathBuf>,
    /// How often the disk-pressure sweep ([`super::sweep`]) probes the
    /// cache/chunks/staging trees. `Duration::ZERO` disables it (unit
    /// and bring-up environments where the daemon does not own the
    /// disk).
    pub sweep_interval: Duration,
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

/// The fuse misc device's fixed numbers (`Documentation/admin-guide/
/// devices.txt`): char major 10 (misc), minor 229.
const FUSE_DEV_MAJOR: u64 = 10;
const FUSE_DEV_MINOR: u64 = 229;

/// Pure predicate behind [`validate_fuse_fd`]: is `(st_mode, st_rdev)`
/// the `/dev/fuse` character device (char major 10, minor 229)?
///
/// Split from the `fstat` so the ACCEPT path is unit-testable with
/// synthetic stat values — the environments the unit tests run in (nix
/// build sandbox, most CI runners) have no `/dev/fuse` to open, so an
/// fd-level test can only exercise rejections there.
fn is_fuse_chardev(st_mode: libc::mode_t, st_rdev: libc::dev_t) -> bool {
    let is_char = (st_mode & libc::S_IFMT) == libc::S_IFCHR;
    let (major, minor) = (libc::major(st_rdev), libc::minor(st_rdev));
    is_char && u64::from(major) == FUSE_DEV_MAJOR && u64::from(minor) == FUSE_DEV_MINOR
}

/// Gate the fd a `Mount{}` frame carries: it must be an open fd for the
/// `/dev/fuse` character device. The daemon holds this fd for the
/// connection's lifetime and issues `FUSE_DEV_IOC_BACKING_*` ioctls
/// against it; accepting an arbitrary fd would at best waste a kept fd
/// per connection and at worst hand a confused (or malicious) builder a
/// root-held reference to whatever it smuggled in. A wrong fd is a
/// builder bug, not a transient condition — the rejection is
/// build-fatal ([`ErrKind::BadFuseFd`]).
// r[impl builder.mountd.fuse-handoff+2]
fn validate_fuse_fd(fd: BorrowedFd<'_>) -> Result<(), ErrKind> {
    let st = nix::sys::stat::fstat(fd).map_err(|e| {
        warn!(error = %e, "Mount fd fstat failed");
        ErrKind::BadFuseFd
    })?;
    if !is_fuse_chardev(st.st_mode, st.st_rdev) {
        warn!(
            is_char = (st.st_mode & libc::S_IFMT) == libc::S_IFCHR,
            major = libc::major(st.st_rdev),
            minor = libc::minor(st.st_rdev),
            "Mount fd is not the /dev/fuse character device"
        );
        return Err(ErrKind::BadFuseFd);
    }
    Ok(())
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
/// inline matches `quota.rs` and `spike_passthrough_fuse.rs`.
const fn iow<T>(ty: u32, nr: u32) -> libc::c_ulong {
    const IOC_WRITE: u32 = 1;
    ((IOC_WRITE << 30) | ((std::mem::size_of::<T>() as u32) << 16) | (ty << 8) | nr)
        as libc::c_ulong
}
const FUSE_DEV_IOC_MAGIC: u32 = 229;
const FUSE_DEV_IOC_BACKING_OPEN: libc::c_ulong = iow::<FuseBackingMap>(FUSE_DEV_IOC_MAGIC, 1);
const FUSE_DEV_IOC_BACKING_CLOSE: libc::c_ulong = iow::<u32>(FUSE_DEV_IOC_MAGIC, 2);

/// Register `backing` as a passthrough backing file on the FUSE
/// connection owned by `fuse_fd`. The kernel rejects nested/stacking
/// backing files (depth > 0) and scopes the returned id to the
/// connection, so the daemon does not need to inspect the fd.
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

/// Bump the mtime of the shared-cache entry behind a successful
/// `BackingOpen` so the disk-pressure sweep — which evicts oldest-mtime
/// first because every production `/var/rio` mount is `noatime` — sees
/// it as recently used (a true LRU stamp, not promote-time FIFO).
///
/// Best-effort and narrowly gated: the fd came from the builder, so it
/// is touched only when it looks exactly like a published shared-cache
/// entry (regular file, owned by the daemon's own uid, mode 0444, on
/// the same filesystem as the cache/chunk roots). Anything else — the
/// builder's own staging files, castore-FUSE inodes, random host files
/// it can read — is left alone; the broker must not become a
/// touch-arbitrary-files oracle.
///
/// TODO: this re-fstats both base dirs and re-reads `geteuid()` on every
/// `BackingOpen` even though all three are fixed for the daemon's
/// lifetime — cache the two `st_dev`s and the euid in the connection's
/// shared state if BackingOpen latency ever matters (it is dwarfed by
/// the UDS round-trip today). The raw `libc::futimens` could also become
/// the safe `nix` wrapper once it grows a `UTIME_NOW` constructor.
fn touch_backing_entry(cache_base: &OwnedFd, chunks_base: &OwnedFd, backing: BorrowedFd<'_>) {
    let Ok(st) = nix::sys::stat::fstat(backing) else {
        return;
    };
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG
        || (st.st_mode & 0o7777) != 0o444
        || st.st_uid != nix::unistd::geteuid().as_raw()
    {
        return;
    }
    let on_shared_fs = [cache_base, chunks_base]
        .into_iter()
        .any(|base| nix::sys::stat::fstat(base).is_ok_and(|b| b.st_dev == st.st_dev));
    if !on_shared_fs {
        return;
    }
    let now = libc::timespec {
        tv_sec: 0,
        tv_nsec: libc::UTIME_NOW,
    };
    // SAFETY: `backing` is a live fd for the duration of the call; the
    // two-element timespec array outlives it.
    let _ = unsafe { libc::futimens(backing.as_raw_fd(), [now, now].as_ptr()) };
}

// ─── Promote ───────────────────────────────────────────────────────────

/// Unlinks the `.promoting` placeholder on drop unless defused. Every
/// error and panic path through a promote must leave no placeholder
/// behind — a leaked one makes every future promote of that digest wait
/// out [`PROMOTE_STALE_AFTER`] before reclaiming it.
struct PromotingGuard<'a> {
    dir: BorrowedFd<'a>,
    name: &'a str,
    defused: bool,
}

impl Drop for PromotingGuard<'_> {
    fn drop(&mut self) {
        if !self.defused {
            let _ = unlinkat(self.dir, self.name, UnlinkatFlags::NoRemoveDir);
        }
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
/// builder. The destination is created `0444` under a `.promoting`
/// name and `renameat`ed into place only after the hash matches.
// r[impl builder.mountd.promote-verified]
// r[impl builder.mountd.promote-bounded-copy]
pub(crate) fn promote_one(
    staging: BorrowedFd<'_>,
    dest_root: BorrowedFd<'_>,
    digest: &[u8; 32],
    max_bytes: u64,
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

    // ── Claim the digest with an O_EXCL `.promoting` placeholder.
    // Loser of the race waits for the winner, then reports Promoted
    // without copying (content-addressed: any winner wrote the same
    // bytes).
    let promoting = format!("{hex_name}.promoting");
    let dst = loop {
        match openat(
            &ab_dir,
            promoting.as_str(),
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o444),
        ) {
            Ok(fd) => break fd,
            Err(nix::errno::Errno::EEXIST) => {
                match wait_for_concurrent_promote(ab_dir.as_fd(), &hex_name, &promoting) {
                    RaceOutcome::AlreadyPromoted => return Ok(0),
                    RaceOutcome::StaleReclaimed => continue,
                    RaceOutcome::TimedOut => return Err(ErrKind::RaceTimeout),
                }
            }
            Err(e) => return Err(ErrKind::Retryable(format!("create {promoting}: {e}"))),
        }
    };
    let mut guard = PromotingGuard {
        dir: ab_dir.as_fd(),
        name: &promoting,
        defused: false,
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
            return Err(ErrKind::Retryable(format!("write {ab}/{promoting}: {e}")));
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
        return Err(ErrKind::Retryable(format!("fchmod {ab}/{promoting}: {e}")));
    }
    if let Err(e) = dst_f.sync_all() {
        return Err(ErrKind::Retryable(format!("fsync {ab}/{promoting}: {e}")));
    }
    drop(dst_f);

    // ── Publish.
    if let Err(e) = nix::fcntl::renameat(&ab_dir, promoting.as_str(), &ab_dir, hex_name.as_str()) {
        return Err(ErrKind::Retryable(format!("rename {promoting}: {e}")));
    }
    guard.defused = true;
    drop(guard);
    // Source is no longer needed; failure to unlink is not a promote
    // failure (the staging dir is removed wholesale at teardown).
    let _ = unlinkat(staging, hex_name.as_str(), UnlinkatFlags::NoRemoveDir);
    Ok(copied)
}

enum RaceOutcome {
    AlreadyPromoted,
    StaleReclaimed,
    TimedOut,
}

/// Another promote of the same digest holds the `.promoting`
/// placeholder. Poll for the final name to appear (the winner's
/// `renameat`); if the placeholder is older than
/// [`PROMOTE_STALE_AFTER`] it is a leak from a crashed daemon and is
/// reclaimed. A poll loop replaces the planned inotify watch — at a
/// 50 ms period the added latency is bounded by one period and the
/// code does not need an inotify fd per in-flight race.
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
        match fstatat(ab_dir, promoting, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Err(_) => {
                // Placeholder vanished without the final name appearing:
                // the winner failed and cleaned up. Retry the claim.
                return RaceOutcome::StaleReclaimed;
            }
            Ok(st) => {
                let age = std::time::SystemTime::now()
                    .duration_since(
                        std::time::UNIX_EPOCH + Duration::from_secs(st.st_mtime.max(0) as u64),
                    )
                    .unwrap_or_default();
                if age > PROMOTE_STALE_AFTER {
                    let _ = unlinkat(ab_dir, promoting, UnlinkatFlags::NoRemoveDir);
                    return RaceOutcome::StaleReclaimed;
                }
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
    /// Verifier for scheduler-minted Mount-admission tokens (ADR-022
    /// §P0559), loaded from [`MountdConfig::token_key_path`]. `Some` =
    /// token mode: peers outside `allowed_gid` may be admitted by a
    /// verifying token in their `Mount{}`. `None` = gid gate only.
    token_verifier: Option<rio_auth::hmac::HmacVerifier>,
    staging_base: OwnedFd,
    cache_base: OwnedFd,
    chunks_base: OwnedFd,
    /// One live connection per `SO_PEERCRED.uid`.
    live_uids: Mutex<HashSet<libc::uid_t>>,
    /// One live `Mount{build_id}` per process across all connections.
    live_build_ids: Mutex<HashSet<String>>,
    /// Monotonic XFS project-id allocator. Seeded at [`run`] time to
    /// one above the highest projid still assigned to a surviving
    /// staging dir (the startup orphan scan spares dirs whose builder
    /// holds the `.rio-live` flock), falling back to 1 on a fresh
    /// `staging/`. A reused projid only ever maps to a dir whose
    /// previous owner's files are all gone, so it accounts from zero
    /// and persistence across restarts is unnecessary. Never derived
    /// from the adversary-chosen `build_id`.
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
    /// Token mode is on and this peer's gid did NOT match
    /// `allowed_gid`: the connection is only provisionally admitted —
    /// its FIRST frame must be a `Mount{}` whose token verifies, and
    /// anything else (other request, undecodable frame, missing or
    /// invalid token) closes the connection. Always `false` when token
    /// mode is off (such peers are rejected at accept) or when the gid
    /// matched.
    needs_token: bool,
    /// The builder's `/dev/fuse` fd (received in `Mount{}`'s
    /// `SCM_RIGHTS`), held for the lifetime of the connection so
    /// `BACKING_OPEN`/`BACKING_CLOSE` can be issued against the
    /// connection the builder mounted from it.
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
}

/// Per-connection ceiling on live `BACKING_OPEN` registrations. The
/// legitimate count is one per distinct `file_digest` opened during the
/// build (the castore-FUSE keys backing ids per digest and reuses them
/// across opens), so this only needs to clear the largest plausible
/// input closure — 2^20 files is ~10× nixpkgs' biggest. At ~64 bytes of
/// kernel memory per registration the worst case is 64 MiB per
/// connection, which the DaemonSet's memory limit must budget for.
const MAX_LIVE_BACKING_IDS: u32 = 1 << 20;

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
    // The castore dir is deliberately NOT opened/created: post-cutover
    // the daemon never mounts anything, so the dir only exists on hosts
    // that ran a pre-cutover daemon — the orphan scan handles both
    // cases (a missing path is simply skipped).
    let staging_base = open_base(&cfg.staging_dir)?;
    let cache_base = open_base(&cfg.cache_dir)?;
    let chunks_base = open_base(&cfg.chunks_dir)?;

    // Mount-admission token verifier (ADR-022 §P0559). A configured but
    // unreadable/empty key file is a startup error, not a silent
    // fall-back to gid-only — the operator asked for token mode.
    let token_verifier = rio_auth::hmac::HmacVerifier::load(cfg.token_key_path.as_deref())
        .context("load mountd token key")?;
    if token_verifier.is_some() {
        info!(
            allowed_gid = cfg.allowed_gid,
            "Mount-admission token mode enabled: peers outside the allowed gid are admitted \
             by a verifying Mount token (socket becomes world-connectable)"
        );
    }

    reap_orphans(&cfg);

    // Staging dirs that survived the scan (their builder still holds
    // the live sentinel) keep the projid the PREVIOUS incarnation
    // assigned them. Start numbering above those so a fresh build's
    // quota bucket is never shared with a survivor's — sharing would
    // count the survivor's staged bytes against the new build's limit.
    let next_projid = list_dir(&cfg.staging_dir)
        .into_iter()
        .filter_map(|name| crate::quota::project_id(&cfg.staging_dir.join(name)))
        .max()
        .map_or(1, |max| max.saturating_add(1));

    let shared = Arc::new(Shared {
        token_verifier,
        staging_base,
        cache_base,
        chunks_base,
        live_uids: Mutex::new(HashSet::new()),
        live_build_ids: Mutex::new(HashSet::new()),
        next_projid: AtomicU32::new(next_projid),
        promote_sem: Arc::new(tokio::sync::Semaphore::new(
            std::thread::available_parallelism().map_or(4, |n| n.get()),
        )),
        cfg,
    });

    spawn_sweep(&shared);

    let listener = bind_socket(&shared.cfg, shared.token_verifier.is_some())?;
    info!(socket = %shared.cfg.socket_path.display(), "rio-mountd listening");
    let listener = AsyncFd::with_interest(listener, Interest::READABLE)?;

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

/// Spawn the periodic disk-pressure sweep (P0571). One pass per
/// [`MountdConfig::sweep_interval`]: `statvfs` the three trees, and
/// when the low-water mark trips, evict orphaned staging, then chunks,
/// then backing-cache entries (oldest mtime first) until the high-water
/// mark clears — see [`super::sweep`] for the policy. The pass does
/// blocking filesystem I/O, so it runs on the blocking pool; the
/// `live_build_ids` set keeps it from ever touching a live build's
/// staging dir.
fn spawn_sweep(shared: &Arc<Shared>) {
    if shared.cfg.sweep_interval.is_zero() {
        info!("disk-pressure sweep disabled (sweep_interval = 0)");
        return;
    }
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        let dirs = sweep::SweepDirs {
            cache: shared.cfg.cache_dir.clone(),
            chunks: shared.cfg.chunks_dir.clone(),
            staging: shared.cfg.staging_dir.clone(),
        };
        let mut ticker = tokio::time::interval(shared.cfg.sweep_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let shared = Arc::clone(&shared);
            let dirs = dirs.clone();
            let pass = tokio::task::spawn_blocking(move || {
                sweep::sweep_pass(&dirs, &shared.live_build_ids)
            })
            .await;
            if let Err(e) = pass {
                warn!(error = %e, "disk-pressure sweep pass panicked");
            }
        }
    });
}

/// `SOCK_SEQPACKET` listener at `cfg.socket_path`, group
/// `cfg.allowed_gid`. A stale socket from a previous incarnation is
/// unlinked first (the DaemonSet is the only writer of this path).
///
/// Mode is derived from the admission mode rather than configured
/// separately, so the permissive DAC can never be enabled without the
/// token gate that justifies it: 0660 (gid-only admission — the
/// pre-token posture, unchanged) or 0666 when `token_mode` is on
/// (`hostUsers: false` executor pods present arbitrary remapped
/// uids/gids, so the inode must be connectable by anyone; admission
/// then happens in-protocol via the Mount token or the gid check).
fn bind_socket(cfg: &MountdConfig, token_mode: bool) -> anyhow::Result<OwnedFd> {
    // The default socket lives in a dedicated /run/rio-mountd/
    // directory (so the DaemonSet hostPath-mounts one directory, not
    // all of /run) and /run is a tmpfs wiped every boot — own the
    // parent's existence here rather than requiring every deployment
    // (k8s DirectoryOrCreate, VM tests, bare `cargo run`) to pre-create
    // it. The mode is set explicitly rather than inherited from
    // `create_dir_all`'s `0777 & ~umask`: under a hardened umask
    // (0027/0077 — exactly what a systemd `UMask=` drop-in sets) the
    // dir comes out 0750/0700 root:root and builder uids EACCES on
    // connect() before any auth check runs. 0755 because builder uids
    // only need search permission to reach the socket; the 0660 socket
    // inode below is the access gate. The explicit chmod also repairs
    // a dir left behind by a previous wrong-umask incarnation.
    if let Some(parent) = cfg.socket_path.parent() {
        std::fs::create_dir_all(parent).context("create socket parent dir")?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))
            .context("chmod socket parent dir")?;
    }
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
    // can connect to a 0777 socket. (In token mode the final mode is
    // permissive anyway, but setting it before listen keeps the two
    // postures on the same code path.)
    use std::os::unix::fs::PermissionsExt;
    let mode = if token_mode { 0o666 } else { 0o660 };
    std::fs::set_permissions(&cfg.socket_path, std::fs::Permissions::from_mode(mode))
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

/// Reap leftovers from a previous daemon incarnation: castore
/// mountpoints (lazily unmounted then removed — only pre-cutover
/// daemons created these; the current protocol never mounts host-side,
/// and the dir is optional — a missing path is skipped via `list_dir`'s
/// empty result), staging trees (removed), and `.promoting`/`.tmp`
/// placeholders in the shared caches (removed — their owning copy loop
/// is gone).
///
/// "No connection can be live before the listener exists" does NOT mean
/// every staging tree is an orphan: a daemon restart (force-delete,
/// upgrade, crash) kills the *connections* but the builds themselves
/// keep running on the node — the ADR's resilience claim — and their
/// staging dirs are still in active use by the builder's fill path.
/// Those dirs hold the builder's [`sweep::LIVE_SENTINEL`] flock, and
/// the scan skips them (P0560 round 3b finding (c): the restarted
/// daemon's scan deleted an in-flight build's staging). They are reaped
/// later, once the holding build exits, by the disk-pressure sweep or
/// the next restart's scan.
///
/// The scan walks the configured *paths* rather than pre-opened base
/// dirfds: it runs once at startup before any connection exists, so
/// there is no concurrent attacker to race — the openat-only discipline
/// matters for per-build paths derived from an adversary-chosen
/// `build_id`, not here.
// r[impl builder.mountd.orphan-scan]
fn reap_orphans(cfg: &MountdConfig) {
    for name in list_dir(&cfg.castore_dir) {
        let mnt = cfg.castore_dir.join(&name);
        let _ = umount2(&mnt, MntFlags::MNT_DETACH);
        if let Err(e) = std::fs::remove_dir(&mnt) {
            warn!(orphan = %mnt.display(), error = %e, "could not remove orphan mountpoint");
        } else {
            info!(orphan = %mnt.display(), "reaped orphan castore mountpoint");
        }
    }
    for name in list_dir(&cfg.staging_dir) {
        let path = cfg.staging_dir.join(&name);
        if sweep::staging_dir_is_live(&path) {
            info!(
                staging = %path.display(),
                "startup scan: staging dir belongs to a still-running build (live \
                 sentinel held); leaving it alone"
            );
            continue;
        }
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
/// foreign junk that the orphan scan has no business touching.
fn list_dir(path: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

// ─── Connection handling ───────────────────────────────────────────────

/// Replies waiting to be written to the peer. No current reply carries
/// an fd (fds only flow builder → daemon since the P0560 option-(b)
/// change); the `Option<OwnedFd>` slot is kept so the writer half does
/// not need reshaping if a future reply ever does.
type ReplyTx = mpsc::UnboundedSender<(Reply, Option<OwnedFd>)>;

async fn handle_conn(shared: Arc<Shared>, fd: OwnedFd) -> anyhow::Result<()> {
    // ── Peer-credential gate, before any frame is read.
    //
    // Two admission modes (ADR-022 §P0559):
    //   - token mode OFF (no key configured): the gid must match, full
    //     stop — exactly the pre-token behavior.
    //   - token mode ON: a matching gid still admits unconditionally
    //     (the standalone/systemd path), and a non-matching gid is
    //     admitted PROVISIONALLY — its first frame must be a Mount{}
    //     whose token verifies, otherwise the connection is closed.
    // r[impl builder.mountd.token-admission]
    let creds = getsockopt(&fd, sockopt::PeerCredentials).context("SO_PEERCRED")?;
    let gid_ok = creds.gid() == shared.cfg.allowed_gid;
    let token_mode = shared.token_verifier.is_some();
    if !gid_ok && !token_mode {
        warn!(
            uid = creds.uid(),
            gid = creds.gid(),
            "rejecting connection: wrong gid"
        );
        return Ok(());
    }
    let needs_token = !gid_ok;
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
        let mut uids = shared.live_uids.lock().ignore_poison();
        if !uids.insert(creds.uid()) {
            warn!(
                uid = creds.uid(),
                "rejecting connection: uid already connected"
            );
            return Ok(());
        }
    }
    metrics::gauge!("rio_mountd_connections_current").increment(1.0);
    info!(
        uid = creds.uid(),
        gid = creds.gid(),
        needs_token,
        "connection accepted"
    );

    let mut state = ConnState {
        peer_uid: creds.uid(),
        peer_gid: creds.gid(),
        needs_token,
        kept: None,
        build_id: None,
        staging_dirfd: None,
        staging_chunks_dirfd: None,
        projid: None,
        live_backing_ids: 0,
    };

    // Provisionally-admitted peers (token mode, non-matching gid) get a
    // bounded window to authenticate. Without it, any local process
    // could hold a connection slot (an fd, a task, a uid-slot of its
    // own uid) open indefinitely on the now world-connectable socket
    // without ever sending a frame.
    let auth_deadline = tokio::time::Instant::now() + TOKEN_AUTH_TIMEOUT;

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
                if handle_frame(&shared, &mut state, frame, &reply_tx).await.is_break() {
                    // Flush whatever the handler queued (the typed
                    // rejection) so the peer sees it before the close.
                    while let Ok((reply, fd_to_send)) = reply_rx.try_recv() {
                        let _ = write_reply(&async_fd, &reply, fd_to_send).await;
                    }
                    break Ok(());
                }
            }
            // Pre-auth deadline: only armed while a token is still owed.
            () = tokio::time::sleep_until(auth_deadline), if state.needs_token && state.kept.is_none() => {
                warn!(
                    uid = creds.uid(),
                    gid = creds.gid(),
                    "closing connection: no authenticated Mount within the pre-auth window"
                );
                break Ok(());
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
///
/// Returns [`ControlFlow::Break`] when the connection must be closed:
/// a provisionally-admitted peer (token mode, gid outside
/// `allowed_gid`) sent anything other than a successfully
/// token-verified `Mount{}` as its pre-auth traffic. The caller flushes
/// the queued rejection reply and drops the connection, keeping the
/// pre-auth parsing surface at exactly one frame.
async fn handle_frame(
    shared: &Arc<Shared>,
    state: &mut ConnState,
    frame: proto::RecvFrame,
    reply_tx: &ReplyTx,
) -> std::ops::ControlFlow<()> {
    use std::ops::ControlFlow;

    // True until this connection has a completed Mount, for peers that
    // still owe a token: only a Mount may be processed, and any failure
    // closes the connection.
    let pre_auth = state.needs_token && state.kept.is_none();

    // Compat decode: tolerate the pre-token Mount{build_id} shape from
    // a not-yet-upgraded client (see mountd_proto::decode_request_compat).
    let req: Request = match proto::decode_request_compat(&frame.bytes) {
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
            return if pre_auth {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            };
        }
    };
    let seq = req.seq;
    let reply = |resp: Resp, fd: Option<OwnedFd>| {
        let _ = reply_tx.send((Reply { seq, resp }, fd));
    };

    // Every request other than Mount requires a completed Mount; on a
    // connection that still owes a token it is also an admission
    // violation (the first frame must be the authenticating Mount).
    if state.kept.is_none() && !matches!(req.req, Req::Mount { .. }) {
        if pre_auth {
            warn!(
                uid = state.peer_uid,
                gid = state.peer_gid,
                "closing connection: pre-auth request other than Mount"
            );
            metrics::counter!("rio_mountd_mount_rejected_total", "reason" => "token-missing")
                .increment(1);
            reply(Resp::Err(ErrKind::Unauthorized), None);
            return ControlFlow::Break(());
        }
        reply(Resp::Err(ErrKind::Retryable("not mounted".into())), None);
        return ControlFlow::Continue(());
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
        Req::Mount { build_id, token } => {
            let resp = handle_mount(shared, state, build_id, token.as_deref(), &frame.fds);
            let unauthorized = matches!(resp, Resp::Err(ErrKind::Unauthorized));
            reply(resp, None);
            if unauthorized {
                // The peer had no business Mounting; nothing it sends
                // afterwards can change that — drop the connection.
                return ControlFlow::Break(());
            }
        }
        Req::BackingOpen => {
            let resp = if state.live_backing_ids >= MAX_LIVE_BACKING_IDS {
                Resp::Err(ErrKind::Retryable(format!(
                    "live backing-id cap reached ({MAX_LIVE_BACKING_IDS})"
                )))
            } else {
                match frame.fds.first() {
                    None => Resp::Err(ErrKind::Retryable("BackingOpen frame carried no fd".into())),
                    Some(backing) => match backing_open(
                        state.kept.as_ref().expect("checked above").as_fd(),
                        backing.as_fd(),
                    ) {
                        Ok(id) => {
                            state.live_backing_ids += 1;
                            // A successful BackingOpen of a shared-cache
                            // entry is the "this content is in use"
                            // signal the LRU sweep orders evictions by.
                            touch_backing_entry(
                                &shared.cache_base,
                                &shared.chunks_base,
                                backing.as_fd(),
                            );
                            Resp::BackingId(id)
                        }
                        Err(e) => Resp::Err(ErrKind::Retryable(format!("BACKING_OPEN: {e}"))),
                    },
                }
            };
            // `frame.fds` drops here — the kernel holds its own
            // reference to the backing file for the registered id.
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
                return ControlFlow::Continue(());
            };
            spawn_promote(
                shared,
                reply_tx.clone(),
                seq,
                staging,
                BaseDir::Cache,
                vec![digest],
                shared.cfg.max_promote_bytes,
                timer,
                op,
            );
            return ControlFlow::Continue(());
        }
        Req::PromoteChunks { chunk_digests } => {
            if chunk_digests.len() > PROMOTE_CHUNKS_MAX {
                reply(Resp::Err(ErrKind::BatchTooLarge), None);
                return ControlFlow::Continue(());
            }
            let Some(staging_chunks) = state.staging_chunks_dirfd.clone() else {
                reply(Resp::Err(ErrKind::Retryable("not mounted".into())), None);
                return ControlFlow::Continue(());
            };
            spawn_promote(
                shared,
                reply_tx.clone(),
                seq,
                staging_chunks,
                BaseDir::Chunks,
                chunk_digests,
                // Chunks are FastCDC outputs; anything larger than the
                // chunker's max is bogus. Reuse the configured promote
                // ceiling rather than importing the chunker constant —
                // the store re-verifies chunk sizes on upload anyway.
                shared.cfg.max_promote_bytes,
                timer,
                op,
            );
            return ControlFlow::Continue(());
        }
    }
    metrics::histogram!("rio_mountd_request_seconds", "op" => op)
        .record(timer.elapsed().as_secs_f64());
    ControlFlow::Continue(())
}

/// Which shared destination a promote writes into.
enum BaseDir {
    Cache,
    Chunks,
}

/// Run a promote batch on the blocking pool, bounded by the process-wide
/// semaphore, and push the single batch reply when it finishes.
#[allow(clippy::too_many_arguments)]
fn spawn_promote(
    shared: &Arc<Shared>,
    reply_tx: ReplyTx,
    seq: u32,
    staging: Arc<OwnedFd>,
    dest: BaseDir,
    digests: Vec<[u8; 32]>,
    max_bytes: u64,
    timer: std::time::Instant,
    op: &'static str,
) {
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
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

/// `Mount{build_id, token}`: authorize the connection (token mode),
/// claim the id, set up staging with a kernel-enforced quota, and keep
/// the builder's `/dev/fuse` fd (sent in the request's `SCM_RIGHTS`)
/// for later `BackingOpen` brokering.
///
/// The daemon neither opens `/dev/fuse` nor creates/mounts any castore
/// mountpoint (P0560 option (b)): the kernel requires a fuse mount to
/// happen from the same user namespace that opened the device
/// (`fs/fuse/inode.c` "Require mount to happen from the same user
/// namespace which opened /dev/fuse"), and a daemon-side mount could
/// never propagate into builder pods anyway — so the builder opens the
/// device itself, `mount(2)`s it inside its own mount namespace, and
/// hands the daemon a dup so the privileged `FUSE_DEV_IOC_BACKING_*`
/// ioctls have a same-connection fd to go through.
// r[impl builder.mountd.fuse-handoff+2]
// r[impl builder.mountd.one-mount]
// r[impl builder.mountd.build-id-unique]
// r[impl builder.mountd.staging-quota]
fn handle_mount(
    shared: &Arc<Shared>,
    state: &mut ConnState,
    build_id: String,
    token: Option<&str>,
    fds: &[OwnedFd],
) -> Resp {
    if state.kept.is_some() {
        return Resp::Err(ErrKind::AlreadyMounted);
    }
    if !validate_build_id(&build_id) {
        return Resp::Err(ErrKind::BadBuildId);
    }
    // ── Admission (ADR-022 §P0559). Peers whose gid matched
    // `allowed_gid` were admitted at accept time and skip this; a
    // provisionally-admitted peer must present a token that verifies
    // (signature with the mountd key, unexpired, audience "rio-mountd")
    // for EXACTLY the build_id it is claiming. The reply never says
    // which check failed — the specifics go to the daemon log and the
    // reject counter only.
    // r[impl builder.mountd.token-admission]
    if state.needs_token {
        let verifier = shared
            .token_verifier
            .as_ref()
            .expect("needs_token is only ever set when a token verifier is configured");
        let outcome = match token {
            None => Err("token-missing"),
            Some(tok) => match rio_auth::hmac::MountdClaims::verify(verifier, tok, &build_id) {
                Ok(claims) => Ok(claims),
                Err(rio_auth::hmac::MountdTokenError::BuildIdMismatch) => Err("build-id-mismatch"),
                Err(rio_auth::hmac::MountdTokenError::Audience) => Err("audience-mismatch"),
                Err(rio_auth::hmac::MountdTokenError::Token(_)) => Err("token-invalid"),
            },
        };
        match outcome {
            Ok(claims) => {
                info!(
                    build_id,
                    uid = state.peer_uid,
                    tenant = claims.tenant.as_deref().unwrap_or(""),
                    "Mount admitted by token"
                );
                metrics::counter!("rio_mountd_mount_admission_total", "method" => "token")
                    .increment(1);
            }
            Err(reason) => {
                warn!(
                    build_id,
                    uid = state.peer_uid,
                    gid = state.peer_gid,
                    reason,
                    "rejecting Mount: peer gid is not the allowed gid and the presented \
                     token does not authorize this build"
                );
                metrics::counter!("rio_mountd_mount_rejected_total", "reason" => reason)
                    .increment(1);
                return Resp::Err(ErrKind::Unauthorized);
            }
        }
    } else {
        metrics::counter!("rio_mountd_mount_admission_total", "method" => "gid").increment(1);
    }
    // The builder's /dev/fuse fd: required, and gated to actually BE the
    // fuse character device before the daemon keeps a root-held
    // reference to it (the only thing it is ever used for is
    // BACKING_OPEN/CLOSE ioctls). Missing or wrong → build-fatal: the
    // same client would send the same thing again.
    let Some(sent) = fds.first() else {
        warn!(build_id, "Mount frame carried no fd");
        return Resp::Err(ErrKind::BadFuseFd);
    };
    if let Err(kind) = validate_fuse_fd(sent.as_fd()) {
        return Resp::Err(kind);
    }
    let Ok(kept) = sent.try_clone() else {
        return Resp::Err(ErrKind::Retryable("dup of the Mount fd failed".into()));
    };
    // Claim the build_id process-wide before touching the filesystem so
    // two connections racing on the same id cannot both mkdir.
    if !shared
        .live_build_ids
        .lock()
        .ignore_poison()
        .insert(build_id.clone())
    {
        return Resp::Err(ErrKind::DuplicateBuildId);
    }
    // From here on, every failure must release the claim.
    match mount_build(shared, state, &build_id) {
        Ok(quota) => {
            state.kept = Some(kept);
            state.build_id = Some(build_id);
            Resp::Mounted {
                staging_quota_bytes: quota,
            }
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
            Resp::Err(ErrKind::Retryable(format!("mount: {e}")))
        }
    }
}

/// The fallible body of `Mount`: staging dir + chunks subdir + project
/// quota. Returns the applied quota. The caller owns claim-release and
/// directory cleanup on error.
fn mount_build(shared: &Arc<Shared>, state: &mut ConnState, build_id: &str) -> anyhow::Result<u64> {
    let cfg = &shared.cfg;

    // ── Staging: 0700, owned by the build's uid, kernel-quota'd.
    //
    // EEXIST is tolerated deliberately: after a daemon restart the
    // surviving build's staging dir (spared by the startup orphan scan
    // via its flock'd `.rio-live` sentinel) is re-adopted by the
    // builder's re-`Mount{build_id}` on its new connection — existing
    // staged content is left untouched, the dir is re-chowned to the
    // same peer uid, and the staging quota is re-applied to the projid
    // the dir already carries (see [`staging_projid`]).
    // r[impl builder.fs.mountd-reconnect]
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
    // EEXIST tolerated for the same re-adoption reason as the parent
    // dir above: a surviving build's staging already has its chunks
    // subdir from the original Mount.
    match mkdirat(&staging_dirfd, "chunks", Mode::from_bits_truncate(0o700)) {
        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
        Err(e) => return Err(e).context("mkdir staging/chunks"),
    }
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
    // so a build cannot collide into a victim's quota bucket. A
    // re-adopted (surviving) staging dir keeps the projid the previous
    // incarnation tagged it with — see [`staging_projid`].
    let mut applied_quota = 0;
    if cfg.staging_quota_bytes > 0 {
        let projid = staging_projid(
            crate::quota::project_id_of(&staging_dirfd),
            &shared.next_projid,
        );
        apply_project_quota(&staging_dirfd, projid, cfg.staging_quota_bytes)
            .context("apply staging project quota")?;
        state.projid = Some(projid);
        applied_quota = cfg.staging_quota_bytes;
    }

    state.staging_dirfd = Some(Arc::new(staging_dirfd));
    state.staging_chunks_dirfd = Some(Arc::new(staging_chunks_dirfd));
    info!(
        build_id,
        uid = state.peer_uid,
        quota = applied_quota,
        "staging ready, builder's /dev/fuse fd kept for backing brokering"
    );
    Ok(applied_quota)
}

/// Which XFS project id a staging dir gets at `Mount` time.
///
/// A surviving (re-adopted) dir keeps the id the previous daemon
/// incarnation tagged it with (`existing`): the files staged before the
/// restart already carry that id, so re-tagging the directory with a
/// fresh one would hand the build a second, empty quota bucket (≈ +1×
/// `staging_quota_bytes` per restart), leave the old id's hard-limit
/// record dangling past teardown (teardown only clears the id in
/// `ConnState`), and hide the old id from the next incarnation's
/// seed-above-survivors scan (which reads the directory's projid, not
/// its files'). Only a genuinely untagged dir draws a fresh id from the
/// monotonic counter — still never derived from the adversary-chosen
/// `build_id`.
fn staging_projid(existing: Option<u32>, next_projid: &AtomicU32) -> u32 {
    match existing {
        Some(id) => id,
        None => next_projid.fetch_add(1, Ordering::Relaxed),
    }
}

/// Best-effort removal of the per-build staging tree. Used by both the
/// Mount error path and connection teardown. (The daemon creates no
/// castore mountpoint since the P0560 option-(b) protocol change — the
/// builder's mount dies with the builder's own mount namespace.)
fn cleanup_build_dirs(shared: &Arc<Shared>, build_id: &str) {
    let _ = std::fs::remove_dir_all(shared.cfg.staging_dir.join(build_id));
}

/// Connection teardown: undo everything `Mount` set up. Runs on every
/// connection exit path (orderly close, error, daemon-side rejection
/// after a successful mount).
fn teardown(shared: &Arc<Shared>, state: &mut ConnState) {
    if let Some(build_id) = state.build_id.take() {
        cleanup_build_dirs(shared, &build_id);
        if let (Some(projid), Some(staging)) = (state.projid, state.staging_dirfd.as_ref()) {
            // Release the quota record so the projid slot doesn't
            // accumulate dead accounting. The staging tree is already
            // gone so the live usage is zero either way.
            let _ = apply_project_quota(staging.as_ref(), projid, 0);
        }
        shared
            .live_build_ids
            .lock()
            .ignore_poison()
            .remove(&build_id);
        info!(build_id, "connection closed, build torn down");
    }
    shared
        .live_uids
        .lock()
        .ignore_poison()
        .remove(&state.peer_uid);
    // Dropping `kept` releases the daemon's only reference to the
    // builder's /dev/fuse; once the builder's own fd and mount are gone
    // too, the kernel aborts the FUSE connection.
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
        "Rejected promotes (labeled by reason: mismatch/not-regular/too-large/race-timeout/other)"
    );
    describe_gauge!(
        "rio_mountd_promote_inflight",
        "Promote copy tasks currently running"
    );
    describe_gauge!(
        "rio_mountd_connections_current",
        "Live UDS connections (== builds being served on this node)"
    );
    describe_counter!(
        "rio_mountd_mount_admission_total",
        "Successful Mount admissions (labeled by method: gid = SO_PEERCRED gid matched \
         allowedGid, token = scheduler-minted Mount token verified)"
    );
    describe_counter!(
        "rio_mountd_mount_rejected_total",
        "Mount admission rejections in token mode (labeled by reason: token-missing/\
         token-invalid/build-id-mismatch/audience-mismatch)"
    );
    describe_gauge!(
        "rio_mountd_cache_free_bytes",
        "Free bytes (statvfs f_bavail × f_frsize) on the filesystem hosting the shared \
         backing cache, sampled at the disk-pressure sweep interval"
    );
    describe_counter!(
        "rio_mountd_sweep_low_space_total",
        "Disk-pressure sweep activations: min(free%) across cache/chunks/staging dropped \
         below the low-water mark (10%)"
    );
    describe_counter!(
        "rio_mountd_sweep_removed_total",
        "Entries removed by the disk-pressure sweep, labeled by tier \
         (staging = orphaned per-build staging trees, chunks, cache)"
    );
    describe_counter!(
        "rio_mountd_sweep_bytes_freed_total",
        "Bytes reclaimed by the disk-pressure sweep, labeled by tier (staging/chunks/cache)"
    );
    describe_histogram!(
        "rio_mountd_sweep_seconds",
        "Wall-clock duration of one triggered disk-pressure sweep pass (eviction until \
         the high-water mark clears or candidates run out)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    /// Build a minimal [`Shared`] rooted in a tempdir so connection-level
    /// handlers (`handle_mount`) can run without a daemon, a socket, or
    /// CAP_SYS_ADMIN. Quota is disabled (0) so no XFS is needed. Token
    /// mode off; [`test_shared_with_token_key`] turns it on.
    fn test_shared(tmp: &Path) -> Arc<Shared> {
        test_shared_inner(tmp, None)
    }

    /// [`test_shared`] with the Mount-admission token verifier
    /// configured from `key` (token mode ON).
    fn test_shared_with_token_key(tmp: &Path, key: &[u8]) -> Arc<Shared> {
        test_shared_inner(
            tmp,
            Some(rio_auth::hmac::HmacVerifier::from_key(key.to_vec())),
        )
    }

    fn test_shared_inner(
        tmp: &Path,
        token_verifier: Option<rio_auth::hmac::HmacVerifier>,
    ) -> Arc<Shared> {
        let sub = |name: &str| {
            let p = tmp.join(name);
            std::fs::create_dir_all(&p).unwrap();
            nix::fcntl::open(&p, OFlag::O_DIRECTORY | OFlag::O_CLOEXEC, Mode::empty()).unwrap()
        };
        Arc::new(Shared {
            cfg: MountdConfig {
                socket_path: tmp.join("mountd.sock"),
                castore_dir: tmp.join("castore"),
                staging_dir: tmp.join("staging"),
                cache_dir: tmp.join("cache"),
                chunks_dir: tmp.join("chunks"),
                staging_quota_bytes: 0,
                max_promote_bytes: DEFAULT_MAX_PROMOTE_BYTES,
                allowed_gid: nix::unistd::getegid().as_raw(),
                token_key_path: None,
                sweep_interval: Duration::ZERO,
            },
            token_verifier,
            staging_base: sub("staging"),
            cache_base: sub("cache"),
            chunks_base: sub("chunks"),
            live_uids: Mutex::new(HashSet::new()),
            live_build_ids: Mutex::new(HashSet::new()),
            next_projid: AtomicU32::new(1),
            promote_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    fn test_conn_state() -> ConnState {
        ConnState {
            peer_uid: nix::unistd::geteuid().as_raw(),
            peer_gid: nix::unistd::getegid().as_raw(),
            needs_token: false,
            kept: None,
            build_id: None,
            staging_dirfd: None,
            staging_chunks_dirfd: None,
            projid: None,
            live_backing_ids: 0,
        }
    }

    /// [`test_conn_state`] for a peer that was only provisionally
    /// admitted (token mode, gid outside `allowed_gid`).
    fn test_conn_state_needs_token() -> ConnState {
        ConnState {
            needs_token: true,
            ..test_conn_state()
        }
    }

    /// The startup orphan scan reaps staging trees and cache
    /// placeholders left by a previous incarnation, but a staging dir
    /// whose builder still holds the [`sweep::LIVE_SENTINEL`] flock
    /// belongs to a build that survived the daemon restart and MUST be
    /// left alone (P0560 round 3b finding (c)). Once the holder exits
    /// (lock released) a later scan reaps it like any other orphan.
    // r[verify builder.mountd.orphan-scan]
    #[test]
    fn reap_orphans_spares_flock_held_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared(tmp.path());
        let cfg = &shared.cfg;

        // A build that outlived the previous daemon: sentinel flock held.
        let survivor = cfg.staging_dir.join("restart-survivor_drv");
        std::fs::create_dir_all(&survivor).unwrap();
        std::fs::write(survivor.join("deadbeef"), b"staged bytes").unwrap();
        let sentinel = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(survivor.join(sweep::LIVE_SENTINEL))
            .unwrap();
        let held = nix::fcntl::Flock::lock(sentinel, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| e)
            .unwrap();

        // A genuine orphan (crashed build, no flock holder) and a stale
        // promote placeholder.
        let orphan = cfg.staging_dir.join("crashed-build_drv");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("leftover"), b"junk").unwrap();
        let shard = cfg.cache_dir.join("ab");
        std::fs::create_dir_all(&shard).unwrap();
        let placeholder = shard.join("cafe.promoting");
        std::fs::write(&placeholder, b"half a promote").unwrap();

        reap_orphans(cfg);
        assert!(
            survivor.join("deadbeef").exists(),
            "live build's staging must survive the startup scan"
        );
        assert!(!orphan.exists(), "unheld staging dir is reaped");
        assert!(!placeholder.exists(), "stale .promoting is reaped");

        // Builder exits → flock released → the next scan reaps it.
        drop(held);
        reap_orphans(cfg);
        assert!(
            !survivor.exists(),
            "released staging dir is reaped by the next scan"
        );
    }

    /// The pure (mode, rdev) predicate behind the Mount fd gate, with
    /// SYNTHETIC stat values: the real /dev/fuse numbers pass; a wrong
    /// major, wrong minor, the wrong device entirely, or a
    /// non-character file format are all rejected. This is the
    /// accept-path coverage the fd-level test below cannot provide in
    /// environments without /dev/fuse (the nix build sandbox).
    #[test]
    fn is_fuse_chardev_accepts_only_the_fuse_device() {
        let fuse_rdev = libc::makedev(
            FUSE_DEV_MAJOR as libc::c_uint,
            FUSE_DEV_MINOR as libc::c_uint,
        );

        // Accept: the fuse char device, with or without permission bits.
        assert!(is_fuse_chardev(libc::S_IFCHR, fuse_rdev));
        assert!(is_fuse_chardev(libc::S_IFCHR | 0o666, fuse_rdev));

        // Reject: right format, wrong device (/dev/null is char 1:3).
        assert!(!is_fuse_chardev(libc::S_IFCHR, libc::makedev(1, 3)));
        // Reject: one of major/minor off by one.
        assert!(!is_fuse_chardev(libc::S_IFCHR, libc::makedev(11, 229)));
        assert!(!is_fuse_chardev(libc::S_IFCHR, libc::makedev(10, 228)));
        // Reject: right device numbers but not a character device.
        for mode in [libc::S_IFREG, libc::S_IFDIR, libc::S_IFIFO, libc::S_IFBLK] {
            assert!(
                !is_fuse_chardev(mode, fuse_rdev),
                "format {mode:o} must not pass the fuse chardev gate"
            );
        }
    }

    /// The Mount fd gate: only the /dev/fuse character device passes.
    /// Regular files, directories, and pipes are exactly the things a
    /// confused client would send — all rejected as the build-fatal
    /// [`ErrKind::BadFuseFd`].
    #[test]
    fn validate_fuse_fd_rejects_non_fuse_fds() {
        let tmp = tempfile::tempdir().unwrap();

        // Regular file.
        let f = std::fs::File::create(tmp.path().join("plain")).unwrap();
        assert_eq!(validate_fuse_fd(f.as_fd()), Err(ErrKind::BadFuseFd));

        // Directory.
        let d = std::fs::File::open(tmp.path()).unwrap();
        assert_eq!(validate_fuse_fd(d.as_fd()), Err(ErrKind::BadFuseFd));

        // Pipe (not even stat-able as a device).
        let (r, _w) = nix::unistd::pipe().unwrap();
        assert_eq!(validate_fuse_fd(r.as_fd()), Err(ErrKind::BadFuseFd));

        // /dev/null is a character device — but not the fuse one.
        if let Ok(null) = std::fs::File::open("/dev/null") {
            assert_eq!(validate_fuse_fd(null.as_fd()), Err(ErrKind::BadFuseFd));
        }

        // The real thing passes, where the environment provides it
        // (not in the nix build sandbox). The accept path is still
        // covered everywhere via the synthetic-stat predicate test
        // above; this is the end-to-end fstat confirmation.
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
        {
            Ok(fuse) => assert_eq!(validate_fuse_fd(fuse.as_fd()), Ok(())),
            Err(e) => eprintln!(
                "skipping the /dev/fuse accept check (cannot open /dev/fuse: {e}); \
                 the synthetic-stat predicate test still covers the accept path"
            ),
        }
    }

    /// A Mount frame without an fd (or with a non-fuse fd) is rejected
    /// build-fatally and leaves no state behind: the build_id is not
    /// claimed, no staging dir is created, and the same id is still
    /// claimable by a well-formed Mount afterwards.
    #[test]
    fn handle_mount_rejects_missing_or_bogus_fd() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared(tmp.path());

        // No fd at all.
        let mut state = test_conn_state();
        let resp = handle_mount(&shared, &mut state, "b-nofd".into(), None, &[]);
        assert_eq!(resp, Resp::Err(ErrKind::BadFuseFd));
        assert!(state.kept.is_none() && state.build_id.is_none());
        assert!(
            !shared.live_build_ids.lock().unwrap().contains("b-nofd"),
            "a rejected Mount must not leave the build_id claimed"
        );
        assert!(!tmp.path().join("staging/b-nofd").exists());

        // A regular file masquerading as the fuse fd.
        let bogus: OwnedFd = std::fs::File::create(tmp.path().join("bogus"))
            .unwrap()
            .into();
        let mut state = test_conn_state();
        let resp = handle_mount(&shared, &mut state, "b-bogus".into(), None, &[bogus]);
        assert_eq!(resp, Resp::Err(ErrKind::BadFuseFd));
        assert!(state.kept.is_none() && state.build_id.is_none());
        assert!(!shared.live_build_ids.lock().unwrap().contains("b-bogus"));
        assert!(!tmp.path().join("staging/b-bogus").exists());

        // The id rejected above is still claimable once a real fuse fd
        // arrives (where the environment provides /dev/fuse).
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
        {
            Ok(fuse) => {
                let mut state = test_conn_state();
                let resp = handle_mount(&shared, &mut state, "b-nofd".into(), None, &[fuse.into()]);
                assert!(
                    matches!(resp, Resp::Mounted { .. }),
                    "well-formed Mount after a rejected one must succeed, got {resp:?}"
                );
                assert!(state.kept.is_some());
                assert!(tmp.path().join("staging/b-nofd").exists());
            }
            Err(e) => eprintln!(
                "skipping the rejected-id-reclaim-with-real-fd check \
                 (cannot open /dev/fuse: {e})"
            ),
        }
    }

    const TEST_TOKEN_KEY: &[u8] = b"mountd-test-token-key-32-bytes!!";

    /// Sign a [`rio_auth::hmac::MountdClaims`] token the way the
    /// scheduler does, with knobs for the negative cases.
    fn mint_token(key: &[u8], build_id: &str, aud: &str, expiry_offset_secs: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        rio_auth::hmac::HmacSigner::from_key(key.to_vec()).sign(&rio_auth::hmac::MountdClaims {
            aud: aud.into(),
            build_id: build_id.into(),
            tenant: None,
            issued_unix: now,
            expiry_unix: (now as i64 + expiry_offset_secs).max(0) as u64,
        })
    }

    /// The §P0559 admission matrix at the `handle_mount` level. The
    /// fuse-fd gate sits AFTER admission, so in environments without
    /// /dev/fuse an admitted Mount surfaces as `BadFuseFd` — every
    /// assertion therefore distinguishes only "admitted past the token
    /// gate" (anything but `Unauthorized`) from "rejected by it"
    /// (`Unauthorized`), plus the no-state-left-behind invariant.
    // r[verify builder.mountd.token-admission]
    // r[verify builder.mountd.token-key-separate]
    #[test]
    fn mount_admission_matrix() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared_with_token_key(tmp.path(), TEST_TOKEN_KEY);
        let admitted = |resp: &Resp| !matches!(resp, Resp::Err(ErrKind::Unauthorized));

        // gid-admitted connection (needs_token = false): no token
        // required even though token mode is on — the standalone /
        // systemd path keeps working unchanged.
        let resp = handle_mount(&shared, &mut test_conn_state(), "b-gid".into(), None, &[]);
        assert!(admitted(&resp), "gid-admitted Mount must not need a token");

        // Token-admitted connection: a valid token for the requested
        // build_id passes the gate.
        let tok = mint_token(
            TEST_TOKEN_KEY,
            "b-tok",
            rio_auth::hmac::MOUNTD_TOKEN_AUDIENCE,
            3600,
        );
        let resp = handle_mount(
            &shared,
            &mut test_conn_state_needs_token(),
            "b-tok".into(),
            Some(&tok),
            &[],
        );
        assert!(admitted(&resp), "valid token must admit, got {resp:?}");

        // Every rejection in one sweep: missing, garbage, wrong key,
        // expired, wrong audience, and a token minted for a DIFFERENT
        // build_id. None may claim the id or create staging.
        let wrong_key = mint_token(
            b"some-other-key-32-bytes-long-!!!",
            "b-rej",
            rio_auth::hmac::MOUNTD_TOKEN_AUDIENCE,
            3600,
        );
        let expired = mint_token(
            TEST_TOKEN_KEY,
            "b-rej",
            rio_auth::hmac::MOUNTD_TOKEN_AUDIENCE,
            -120,
        );
        let wrong_aud = mint_token(TEST_TOKEN_KEY, "b-rej", "rio-store", 3600);
        let other_build = mint_token(
            TEST_TOKEN_KEY,
            "b-someone-else",
            rio_auth::hmac::MOUNTD_TOKEN_AUDIENCE,
            3600,
        );
        for (what, token) in [
            ("missing", None),
            ("garbage", Some("not-a-token".to_string())),
            ("wrong-key", Some(wrong_key)),
            ("expired", Some(expired)),
            ("wrong-audience", Some(wrong_aud)),
            ("other-build", Some(other_build)),
        ] {
            let mut state = test_conn_state_needs_token();
            let resp = handle_mount(&shared, &mut state, "b-rej".into(), token.as_deref(), &[]);
            assert_eq!(
                resp,
                Resp::Err(ErrKind::Unauthorized),
                "{what}: token-required Mount must be rejected"
            );
            assert!(state.kept.is_none() && state.build_id.is_none());
            assert!(
                !shared.live_build_ids.lock().unwrap().contains("b-rej"),
                "{what}: a rejected Mount must not leave the build_id claimed"
            );
            assert!(
                !tmp.path().join("staging/b-rej").exists(),
                "{what}: a rejected Mount must not create staging"
            );
        }

        // Key separation: a token with the ASSIGNMENT claims shape,
        // even when signed with the mountd key, is not a Mount
        // credential.
        let assignment_shaped = rio_auth::hmac::HmacSigner::from_key(TEST_TOKEN_KEY.to_vec()).sign(
            &rio_auth::hmac::AssignmentClaims {
                executor_id: "w".into(),
                drv_hash: "b-rej".into(),
                expected_outputs: vec![],
                is_ca: false,
                expiry_unix: u64::MAX,
                tenant: None,
                role: rio_auth::hmac::TokenRole::Builder,
                input_closure_digest: String::new(),
            },
        );
        let resp = handle_mount(
            &shared,
            &mut test_conn_state_needs_token(),
            "b-rej".into(),
            Some(&assignment_shaped),
            &[],
        );
        assert_eq!(resp, Resp::Err(ErrKind::Unauthorized));

        // Token mode OFF (no verifier): gid-admitted connections behave
        // exactly as before — and a token, if one is sent anyway, is
        // simply ignored.
        let off = test_shared(tmp.path());
        let tok = mint_token(
            TEST_TOKEN_KEY,
            "b-off",
            rio_auth::hmac::MOUNTD_TOKEN_AUDIENCE,
            3600,
        );
        let resp = handle_mount(
            &off,
            &mut test_conn_state(),
            "b-off".into(),
            Some(&tok),
            &[],
        );
        assert!(admitted(&resp), "token mode off must not consult tokens");
    }

    /// Full happy path for a token-admitted Mount where the environment
    /// provides /dev/fuse: staging is created and owned by the peer uid
    /// exactly like a gid-admitted Mount — token admission changes who
    /// may Mount, never what a Mount sets up.
    // r[verify builder.mountd.token-admission]
    #[test]
    fn token_admitted_mount_sets_up_staging() {
        let Ok(fuse) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
        else {
            eprintln!("skipping: /dev/fuse unavailable in this environment");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared_with_token_key(tmp.path(), TEST_TOKEN_KEY);
        let mut state = test_conn_state_needs_token();
        let tok = mint_token(
            TEST_TOKEN_KEY,
            "b-tok-happy",
            rio_auth::hmac::MOUNTD_TOKEN_AUDIENCE,
            3600,
        );
        let resp = handle_mount(
            &shared,
            &mut state,
            "b-tok-happy".into(),
            Some(&tok),
            &[fuse.into()],
        );
        assert!(matches!(resp, Resp::Mounted { .. }), "got {resp:?}");
        assert!(state.kept.is_some());
        assert!(tmp.path().join("staging/b-tok-happy").exists());
    }

    /// Connection-close semantics for provisionally-admitted peers: any
    /// pre-auth frame other than a token-verified Mount (a non-Mount
    /// request, an undecodable frame, a Mount with a bad token) replies
    /// and then closes the connection; a token-verified Mount keeps it
    /// open. Post-auth (or gid-admitted) connections keep the existing
    /// keep-open behavior for protocol errors.
    // r[verify builder.mountd.token-admission]
    #[tokio::test]
    async fn pre_auth_frames_close_the_connection() {
        use std::ops::ControlFlow;

        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared_with_token_key(tmp.path(), TEST_TOKEN_KEY);
        let frame = |req: Req| proto::RecvFrame {
            bytes: proto::encode(&Request { seq: 1, req }).unwrap(),
            fds: Vec::new(),
        };
        let recv_resp = |rx: &mut mpsc::UnboundedReceiver<(Reply, Option<OwnedFd>)>| {
            let (reply, _) = rx.try_recv().expect("a reply must have been queued");
            reply.resp
        };

        // Pre-auth non-Mount request → Unauthorized + close.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = test_conn_state_needs_token();
        let flow = handle_frame(&shared, &mut state, frame(Req::BackingOpen), &tx).await;
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(recv_resp(&mut rx), Resp::Err(ErrKind::Unauthorized));

        // Pre-auth undecodable frame → close (the queued reply is
        // Retryable so even a confused peer sees a typed error first).
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut state = test_conn_state_needs_token();
        let bad = proto::RecvFrame {
            bytes: vec![0xFF; 16],
            fds: Vec::new(),
        };
        assert_eq!(
            handle_frame(&shared, &mut state, bad, &tx).await,
            ControlFlow::Break(())
        );

        // Pre-auth Mount with a bad token → Unauthorized + close.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = test_conn_state_needs_token();
        let flow = handle_frame(
            &shared,
            &mut state,
            frame(Req::Mount {
                build_id: "b-close".into(),
                token: Some("garbage".into()),
            }),
            &tx,
        )
        .await;
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(recv_resp(&mut rx), Resp::Err(ErrKind::Unauthorized));

        // Pre-auth Mount with a VALID token → connection stays open
        // (the Mount itself may still fail on the fuse-fd gate in this
        // environment, which is not an admission failure).
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut state = test_conn_state_needs_token();
        let tok = mint_token(
            TEST_TOKEN_KEY,
            "b-open",
            rio_auth::hmac::MOUNTD_TOKEN_AUDIENCE,
            3600,
        );
        let flow = handle_frame(
            &shared,
            &mut state,
            frame(Req::Mount {
                build_id: "b-open".into(),
                token: Some(tok),
            }),
            &tx,
        )
        .await;
        assert_eq!(flow, ControlFlow::Continue(()));

        // Gid-admitted connection: a pre-Mount non-Mount request keeps
        // the pre-token behavior (Retryable reply, connection open).
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = test_conn_state();
        let flow = handle_frame(&shared, &mut state, frame(Req::BackingOpen), &tx).await;
        assert_eq!(flow, ControlFlow::Continue(()));
        assert!(matches!(
            recv_resp(&mut rx),
            Resp::Err(ErrKind::Retryable(_))
        ));
    }

    /// A restarted daemon accepting a re-`Mount{build_id}` for a build
    /// that survived it: the staging dir already exists (spared by the
    /// startup scan — the builder still holds the `.rio-live` flock)
    /// and still holds a staged-but-not-yet-promoted file. The Mount
    /// must succeed, must NOT wipe the staged content, and a Promote on
    /// the new connection must publish that surviving file. This is the
    /// daemon half of the client's reconnect-after-restart contract.
    // r[verify builder.fs.mountd-reconnect]
    #[test]
    fn mount_build_readopts_a_surviving_staging_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = test_shared(tmp.path());
        let build_id = "restart-survivor_drv";

        // What the previous incarnation (plus the builder) left behind:
        // the staging dir, its chunks subdir, a verified staged file
        // awaiting promote, and the flock-held live sentinel.
        let staging = tmp.path().join("staging").join(build_id);
        std::fs::create_dir_all(staging.join("chunks")).unwrap();
        let content = b"staged before the daemon restarted".to_vec();
        let digest = *blake3::hash(&content).as_bytes();
        std::fs::write(staging.join(hex::encode(digest)), &content).unwrap();
        let sentinel = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(staging.join(sweep::LIVE_SENTINEL))
            .unwrap();
        let _held = nix::fcntl::Flock::lock(sentinel, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| e)
            .unwrap();

        // The re-Mount on the restarted daemon (fresh Shared, empty
        // claims) must re-adopt the dir rather than fail or wipe it.
        let mut state = test_conn_state();
        let quota = mount_build(&shared, &mut state, build_id)
            .expect("re-Mount over a surviving staging dir must succeed");
        assert_eq!(quota, 0, "quota disabled in the test fixture");
        assert_eq!(
            std::fs::read(staging.join(hex::encode(digest))).unwrap(),
            content,
            "re-adoption must not touch the surviving staged content"
        );
        assert!(
            staging.join(sweep::LIVE_SENTINEL).exists(),
            "the live sentinel survives the re-Mount"
        );

        // And the surviving staged file is promotable through the new
        // connection's staging dirfd.
        let staging_dirfd = state.staging_dirfd.clone().expect("staging dirfd set");
        let n = promote_one(
            staging_dirfd.as_fd(),
            shared.cache_base.as_fd(),
            &digest,
            DEFAULT_MAX_PROMOTE_BYTES,
        )
        .expect("promote of the surviving staged file");
        assert_eq!(n, content.len() as u64);
        assert_eq!(
            std::fs::read(
                tmp.path()
                    .join("cache")
                    .join(shard(&digest))
                    .join(hex::encode(digest))
            )
            .unwrap(),
            content,
            "the pre-restart staged bytes are what lands in the shared cache"
        );
    }

    /// Survivor re-adoption with quota ENABLED: the staging dir's
    /// existing project id is reused (the limit is re-applied to it)
    /// and the monotonic counter is untouched; only an untagged dir
    /// draws a fresh id. The XFS ioctls themselves cannot run on the
    /// test tmpdir (no prjquota — `quota.rs`'s
    /// `apply_fails_loudly_without_prjquota` covers that side), so this
    /// pins the decision logic those ioctls are fed: a second bucket
    /// per restart, a dangling limit record, or a hidden survivor id
    /// would all start from `staging_projid` picking a fresh id here.
    /// Teardown then clears whichever id `ConnState` carries — the
    /// reused one — so the record that exists is the record cleared.
    // r[verify builder.fs.mountd-reconnect]
    // r[verify builder.mountd.staging-quota]
    #[test]
    fn staging_projid_reuses_a_survivors_id() {
        let next = AtomicU32::new(7);

        // Re-adopted dir: keep its id, allocate nothing.
        assert_eq!(staging_projid(Some(3), &next), 3);
        assert_eq!(
            next.load(Ordering::Relaxed),
            7,
            "reusing a survivor's projid must not consume a fresh one"
        );

        // Untagged (fresh) dir: allocate from the counter.
        assert_eq!(staging_projid(None, &next), 7);
        assert_eq!(staging_projid(None, &next), 8);
        assert_eq!(next.load(Ordering::Relaxed), 9);

        // The probe feeding `existing` reports None on a filesystem
        // without project quotas (the unit-test tmpdir), so the fresh
        // path is what mount_build exercises here; the reuse arm above
        // is what a real XFS staging root takes after a daemon restart.
        let tmp = tempfile::tempdir().unwrap();
        let dirfd = dirfd(tmp.path());
        assert_eq!(crate::quota::project_id_of(&dirfd), None);
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

    /// `BackingOpen` refreshes the LRU stamp of a published cache entry
    /// (mode 0444, daemon-owned, on the cache filesystem) — and only of
    /// such files: anything that does not look like a published entry
    /// keeps its mtime.
    #[test]
    fn touch_backing_entry_bumps_only_published_entries() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let chunks = tmp.path().join("chunks");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&chunks).unwrap();
        let cache_base = dirfd(&cache);
        let chunks_base = dirfd(&chunks);

        let old = libc::timespec {
            tv_sec: 1_000_000,
            tv_nsec: 0,
        };
        let make = |path: &Path, mode: u32| {
            std::fs::write(path, b"x").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            let f = std::fs::File::open(path).unwrap();
            // SAFETY: `f` is live for the call; the array outlives it.
            assert_eq!(
                unsafe { libc::futimens(f.as_raw_fd(), [old, old].as_ptr()) },
                0
            );
            f
        };

        // Published entry: 0444, our uid, on the cache fs → touched.
        let published = cache.join("ab-entry");
        let f = make(&published, 0o444);
        touch_backing_entry(&cache_base, &chunks_base, f.as_fd());
        let mtime = std::fs::metadata(&published).unwrap().modified().unwrap();
        assert!(
            mtime > std::time::UNIX_EPOCH + Duration::from_secs(2_000_000),
            "published entry's mtime must be refreshed"
        );

        // Builder-writable file (not 0444): left alone.
        let foreign = cache.join("not-an-entry");
        let f = make(&foreign, 0o644);
        touch_backing_entry(&cache_base, &chunks_base, f.as_fd());
        let mtime = std::fs::metadata(&foreign).unwrap().modified().unwrap();
        assert_eq!(
            mtime,
            std::time::UNIX_EPOCH + Duration::from_secs(1_000_000),
            "non-0444 file must keep its mtime"
        );
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

    /// A `.promoting` placeholder with no live owner and no published
    /// result times the loser out rather than hanging forever.
    #[test]
    fn promote_race_timeout_on_abandoned_placeholder() {
        let fx = Fx::new();
        let digest = fx.stage(b"contended");
        // Fresh placeholder (mtime = now) → not stale → the racer waits
        // the full window and times out.
        std::fs::create_dir_all(fx.placeholder(&digest).parent().unwrap()).unwrap();
        std::fs::write(fx.placeholder(&digest), b"").unwrap();

        let start = std::time::Instant::now();
        let r = fx.promote(&digest);
        assert!(matches!(r, Err(ErrKind::RaceTimeout)), "got {r:?}");
        assert!(start.elapsed() >= PROMOTE_RACE_WAIT);
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

    /// An error mid-promote must not leak the `.promoting` placeholder
    /// (a leak blocks every future promote of that digest for
    /// PROMOTE_STALE_AFTER).
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
}
