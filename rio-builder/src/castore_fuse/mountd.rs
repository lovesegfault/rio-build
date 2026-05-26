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
use nix::mount::{MntFlags, MsFlags, mount, umount2};
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
    /// `SO_PEERCRED.gid` allowed to connect. Connections from any other
    /// gid are dropped before a single frame is read.
    pub allowed_gid: u32,
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
    castore_base: OwnedFd,
    staging_base: OwnedFd,
    cache_base: OwnedFd,
    chunks_base: OwnedFd,
    /// One live connection per `SO_PEERCRED.uid`.
    live_uids: Mutex<HashSet<libc::uid_t>>,
    /// One live `Mount{build_id}` per process across all connections.
    live_build_ids: Mutex<HashSet<String>>,
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
        live_uids: Mutex::new(HashSet::new()),
        live_build_ids: Mutex::new(HashSet::new()),
        next_projid: AtomicU32::new(1),
        promote_sem: Arc::new(tokio::sync::Semaphore::new(
            std::thread::available_parallelism().map_or(4, |n| n.get()),
        )),
        cfg,
    });

    spawn_sweep(&shared);

    let listener = bind_socket(&shared.cfg)?;
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

/// `SOCK_SEQPACKET` listener at `cfg.socket_path`, mode 0660, group
/// `cfg.allowed_gid`. A stale socket from a previous incarnation is
/// unlinked first (the DaemonSet is the only writer of this path).
fn bind_socket(cfg: &MountdConfig) -> anyhow::Result<OwnedFd> {
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

/// Replies waiting to be written to the peer. `Option<OwnedFd>` is the
/// single fd a `Mounted` reply carries; everything else sends `None`.
type ReplyTx = mpsc::UnboundedSender<(Reply, Option<OwnedFd>)>;

async fn handle_conn(shared: Arc<Shared>, fd: OwnedFd) -> anyhow::Result<()> {
    // ── Peer-credential gate, before any frame is read.
    let creds = getsockopt(&fd, sockopt::PeerCredentials).context("SO_PEERCRED")?;
    if creds.gid() != shared.cfg.allowed_gid {
        warn!(
            uid = creds.uid(),
            gid = creds.gid(),
            "rejecting connection: wrong gid"
        );
        return Ok(());
    }
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
                return;
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
            return;
        }
    }
    metrics::histogram!("rio_mountd_request_seconds", "op" => op)
        .record(timer.elapsed().as_secs_f64());
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
        let projid = shared.next_projid.fetch_add(1, Ordering::Relaxed);
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
/// tree. Used by both the Mount error path and connection teardown.
fn cleanup_build_dirs(shared: &Arc<Shared>, build_id: &str) {
    let mnt = shared.cfg.castore_dir.join(build_id);
    let _ = umount2(&mnt, MntFlags::MNT_DETACH);
    let _ = unlinkat(&shared.castore_base, build_id, UnlinkatFlags::RemoveDir);
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
