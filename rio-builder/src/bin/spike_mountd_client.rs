//! VM-test client for the production `rio-mountd` daemon (P0567).
//!
//! Stands in for the builder side of the UDS protocol until P0559's
//! `castore_fuse/mount.rs` exists: connects to the daemon's
//! `SOCK_SEQPACKET` socket, speaks `mountd_proto`, and serves a minimal
//! FUSE filesystem on the handed-off `/dev/fuse` fd so `BACKING_OPEN`
//! has a passthrough-negotiated connection to register against. Each
//! subcommand is one `vm-mountd` subtest; results are printed as
//! `RESULT key=value` / `PERF key=value` lines the test driver greps.
//!
//! NOT production code. The real client is in-process in the builder.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, INodeNo, InitFlags, KernelConfig,
    ReplyAttr, ReplyDirectory, Request as FuseRequest, Session, SessionACL,
};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
use rio_builder::castore_fuse::mountd_proto::{self as proto, ErrKind, Reply, Req, Request, Resp};

// ─── Empty FUSE filesystem ─────────────────────────────────────────────
//
// The daemon's BACKING_OPEN ioctl is rejected with EPERM unless the FUSE
// connection negotiated FUSE_PASSTHROUGH at init (`fc->passthrough` is
// only set from the INIT reply). Serving an empty root directory is the
// minimum that (a) completes the handshake with the right flags and
// (b) lets the test driver `ls` the mountpoint to prove the handed-off
// fd is the mounted connection.

struct EmptyFs;

const TTL: Duration = Duration::from_secs(3600);

impl Filesystem for EmptyFs {
    fn init(&mut self, _req: &FuseRequest, config: &mut KernelConfig) -> io::Result<()> {
        config
            .add_capabilities(InitFlags::FUSE_PASSTHROUGH)
            .map_err(|unsup| io::Error::other(format!("kernel lacks {unsup:?}")))?;
        config
            .set_max_stack_depth(1)
            .map_err(|max| io::Error::other(format!("max_stack_depth>{max}")))?;
        Ok(())
    }

    fn getattr(&self, _req: &FuseRequest, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 != 1 {
            reply.error(Errno::ENOENT);
            return;
        }
        reply.attr(
            &TTL,
            &FileAttr {
                ino: INodeNo(1),
                size: 0,
                blocks: 0,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: FileType::Directory,
                perm: 0o555,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            },
        );
    }

    fn readdir(
        &self,
        _req: &FuseRequest,
        ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        reply: ReplyDirectory,
    ) {
        if ino.0 != 1 {
            reply.error(Errno::ENOTDIR);
            return;
        }
        reply.ok();
    }
}

// ─── Connection ────────────────────────────────────────────────────────

struct Conn {
    fd: OwnedFd,
    next_seq: u32,
}

impl Conn {
    fn connect(path: &Path) -> anyhow::Result<Self> {
        let fd = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .context("socket(AF_UNIX, SOCK_SEQPACKET)")?;
        let addr = UnixAddr::new(path).context("socket path")?;
        connect(fd.as_raw_fd(), &addr).with_context(|| format!("connect {}", path.display()))?;
        Ok(Self { fd, next_seq: 1 })
    }

    fn send(&mut self, req: Req, fds: &[RawFd]) -> anyhow::Result<u32> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let bytes = proto::encode(&Request { seq, req }).context("encode request")?;
        proto::send_frame(self.fd.as_raw_fd(), &bytes, fds).context("send request")?;
        Ok(seq)
    }

    fn recv(&mut self) -> anyhow::Result<(Reply, Vec<OwnedFd>)> {
        let frame = proto::recv_frame(self.fd.as_raw_fd()).context("recv reply")?;
        let reply: Reply = proto::decode(&frame.bytes).context("decode reply")?;
        Ok((reply, frame.fds))
    }

    /// Send one request and block for its reply. Only valid while no
    /// other request is in flight (the reply's `seq` must match).
    fn call(&mut self, req: Req, fds: &[RawFd]) -> anyhow::Result<(Resp, Vec<OwnedFd>)> {
        let seq = self.send(req, fds)?;
        let (reply, reply_fds) = self.recv()?;
        if reply.seq != seq {
            bail!("reply seq {} != request seq {seq}", reply.seq);
        }
        Ok((reply.resp, reply_fds))
    }
}

/// `Mount{build_id}` and unpack the success reply into the quota and the
/// handed-off `/dev/fuse` fd.
fn mount(conn: &mut Conn, build_id: &str) -> anyhow::Result<(u64, OwnedFd)> {
    let (resp, mut fds) = conn.call(
        Req::Mount {
            build_id: build_id.to_owned(),
        },
        &[],
    )?;
    match resp {
        Resp::Mounted {
            staging_quota_bytes,
        } => {
            let fd = fds.pop().context("Mounted reply carried no fd")?;
            Ok((staging_quota_bytes, fd))
        }
        other => bail!("Mount failed: {other:?}"),
    }
}

/// The variant name of an error reply, for `--expect` matching and
/// `RESULT` lines.
fn kind_name(kind: &ErrKind) -> &'static str {
    match kind {
        ErrKind::Retryable(_) => "Retryable",
        ErrKind::DigestMismatch => "DigestMismatch",
        ErrKind::NotRegular => "NotRegular",
        ErrKind::TooLarge => "TooLarge",
        ErrKind::RaceTimeout => "RaceTimeout",
        ErrKind::BadBuildId => "BadBuildId",
        ErrKind::AlreadyMounted => "AlreadyMounted",
        ErrKind::DuplicateBuildId => "DuplicateBuildId",
        ErrKind::BatchTooLarge => "BatchTooLarge",
    }
}

// ─── Staging helpers ───────────────────────────────────────────────────

/// Deterministic, offset-unique content: every u64 lane is its own
/// offset xor a seed, so a copy that drops, duplicates, or reorders a
/// block changes the digest.
fn gen_content(seed: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    for (i, lane) in buf.chunks_exact_mut(8).enumerate() {
        lane.copy_from_slice(
            &((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed).to_le_bytes(),
        );
    }
    buf
}

/// Write `content` into the build's staging dir under `hex(claimed)`.
/// The staging dir is created by `Mount` (0700, owned by this uid).
fn stage(
    staging_root: &Path,
    build_id: &str,
    claimed: &[u8; 32],
    content: &[u8],
) -> anyhow::Result<PathBuf> {
    let path = staging_root.join(build_id).join(hex::encode(claimed));
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ─── CLI ───────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "spike_mountd_client", about = "VM-test client for rio-mountd")]
struct Args {
    /// rio-mountd UDS socket path.
    #[arg(long, default_value = "/run/rio-mountd.sock")]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount, serve an empty FUSE fs on the handed-off fd, write the
    /// ready file, hold the connection until killed.
    Serve {
        #[arg(long)]
        build_id: String,
        /// Written (with `quota=<bytes>`) once the FUSE handshake is
        /// done and the mountpoint is safe to touch.
        #[arg(long)]
        ready_file: PathBuf,
    },
    /// Mount and assert the daemon replies `Err(<expect>)`.
    ExpectMountErr {
        #[arg(long)]
        build_id: String,
        /// ErrKind variant name, e.g. `BadBuildId`.
        #[arg(long)]
        expect: String,
    },
    /// Assert the daemon drops the connection without answering the
    /// first request (gid gate, uid-bound rejection).
    ExpectRejected,
    /// Mount twice on one connection; assert the second is
    /// `AlreadyMounted`.
    DoubleMount {
        #[arg(long)]
        build_id: String,
    },
    /// Mount, stage `--size-mib` of generated content, `Promote` it.
    /// With `--corrupt`, stage it under a digest it does not hash to.
    Promote {
        #[arg(long)]
        build_id: String,
        #[arg(long)]
        staging_root: PathBuf,
        #[arg(long, default_value_t = 8)]
        size_mib: usize,
        #[arg(long)]
        corrupt: bool,
    },
    /// Mount, stage content, spawn a thread appending to the staged
    /// file, `Promote` while it grows. The daemon must publish exactly
    /// the fstat-time bytes or reject — never more.
    AppendPromote {
        #[arg(long)]
        build_id: String,
        #[arg(long)]
        staging_root: PathBuf,
        #[arg(long, default_value_t = 32)]
        size_mib: usize,
    },
    /// Mount + serve FUSE, then `--iters` × (open backing file,
    /// BackingOpen, BackingClose). Prints RTT percentiles.
    BackingBench {
        #[arg(long)]
        build_id: String,
        #[arg(long)]
        backing_file: PathBuf,
        #[arg(long, default_value_t = 1000)]
        iters: usize,
    },
    /// Mount + serve FUSE, fire one large `Promote`, then run
    /// BackingOpen/Close pairs while it is in flight. Asserts the
    /// promote does not serialize ahead of the inline ops.
    Concurrency {
        #[arg(long)]
        build_id: String,
        #[arg(long)]
        staging_root: PathBuf,
        #[arg(long)]
        backing_file: PathBuf,
        #[arg(long, default_value_t = 64)]
        promote_mib: usize,
        #[arg(long, default_value_t = 100)]
        iters: usize,
    },
    /// Mount, then write 1 MiB blocks into the staging dir until the
    /// kernel project quota returns ENOSPC (or `--give-up-mib` is
    /// reached, which is a failure).
    FillStaging {
        #[arg(long)]
        build_id: String,
        #[arg(long)]
        staging_root: PathBuf,
        #[arg(long, default_value_t = 256)]
        give_up_mib: usize,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Serve {
            build_id,
            ready_file,
        } => serve(&args.socket, &build_id, &ready_file),
        Cmd::ExpectMountErr { build_id, expect } => {
            expect_mount_err(&args.socket, &build_id, &expect)
        }
        Cmd::ExpectRejected => expect_rejected(&args.socket),
        Cmd::DoubleMount { build_id } => double_mount(&args.socket, &build_id),
        Cmd::Promote {
            build_id,
            staging_root,
            size_mib,
            corrupt,
        } => promote(&args.socket, &build_id, &staging_root, size_mib, corrupt),
        Cmd::AppendPromote {
            build_id,
            staging_root,
            size_mib,
        } => append_promote(&args.socket, &build_id, &staging_root, size_mib),
        Cmd::BackingBench {
            build_id,
            backing_file,
            iters,
        } => backing_bench(&args.socket, &build_id, &backing_file, iters),
        Cmd::Concurrency {
            build_id,
            staging_root,
            backing_file,
            promote_mib,
            iters,
        } => concurrency(
            &args.socket,
            &build_id,
            &staging_root,
            &backing_file,
            promote_mib,
            iters,
        ),
        Cmd::FillStaging {
            build_id,
            staging_root,
            give_up_mib,
        } => fill_staging(&args.socket, &build_id, &staging_root, give_up_mib),
    }
}

// ─── Subcommands ───────────────────────────────────────────────────────

/// Take over the handed-off `/dev/fuse` fd with the passthrough-capable
/// empty filesystem. `from_fd` completes the FUSE_INIT handshake before
/// returning, so once this returns the daemon's kept dup can issue
/// BACKING_OPEN and the mountpoint is safe to stat.
fn start_fuse(fuse_fd: OwnedFd) -> anyhow::Result<fuser::BackgroundSession> {
    let mut config = Config::default();
    config.n_threads = Some(1);
    let session = Session::from_fd(EmptyFs, fuse_fd, SessionACL::All, config)
        .context("FUSE session from handed-off fd")?;
    session.spawn().context("spawn FUSE session")
}

fn serve(socket: &Path, build_id: &str, ready_file: &Path) -> anyhow::Result<()> {
    let mut conn = Conn::connect(socket)?;
    let (quota, fuse_fd) = mount(&mut conn, build_id)?;
    let bg = start_fuse(fuse_fd)?;
    println!("RESULT mount=ok quota={quota}");
    std::fs::write(ready_file, format!("quota={quota}\n")).context("write ready file")?;
    // Hold the UDS connection (teardown fires when it closes) and the
    // FUSE session until the test driver kills us.
    bg.join()?;
    Ok(())
}

fn expect_mount_err(socket: &Path, build_id: &str, expect: &str) -> anyhow::Result<()> {
    let mut conn = Conn::connect(socket)?;
    let (resp, _) = conn.call(
        Req::Mount {
            build_id: build_id.to_owned(),
        },
        &[],
    )?;
    match resp {
        Resp::Err(kind) if kind_name(&kind) == expect => {
            println!("RESULT mount=err kind={expect}");
            Ok(())
        }
        other => bail!("expected Err({expect}), got {other:?}"),
    }
}

fn expect_rejected(socket: &Path) -> anyhow::Result<()> {
    let mut conn = Conn::connect(socket)?;
    // The daemon closes rejected connections before reading a frame, so
    // the send may race the close (EPIPE) or land in the kernel buffer
    // and never be answered (EOF on the reply read). Both prove the
    // rejection; a Reply of any kind disproves it.
    let sent = conn.send(
        Req::Mount {
            build_id: "rejected".to_owned(),
        },
        &[],
    );
    if let Err(e) = sent {
        println!("RESULT rejected=at-send err={e:#}");
        return Ok(());
    }
    match conn.recv() {
        Err(e) => {
            println!("RESULT rejected=at-recv err={e:#}");
            Ok(())
        }
        Ok((reply, _)) => bail!("expected the daemon to drop the connection, got {reply:?}"),
    }
}

fn double_mount(socket: &Path, build_id: &str) -> anyhow::Result<()> {
    let mut conn = Conn::connect(socket)?;
    let (_, _fuse_fd) = mount(&mut conn, build_id)?;
    let second = format!("{build_id}-second");
    let (resp, _) = conn.call(Req::Mount { build_id: second }, &[])?;
    match resp {
        Resp::Err(ErrKind::AlreadyMounted) => {
            println!("RESULT second_mount=AlreadyMounted");
            Ok(())
        }
        other => bail!("expected AlreadyMounted, got {other:?}"),
    }
}

fn promote(
    socket: &Path,
    build_id: &str,
    staging_root: &Path,
    size_mib: usize,
    corrupt: bool,
) -> anyhow::Result<()> {
    let mut conn = Conn::connect(socket)?;
    let (_, _fuse_fd) = mount(&mut conn, build_id)?;
    let content = gen_content(0, size_mib << 20);
    // A corrupted stage claims a digest the content does not hash to:
    // the digest of the content with its first byte flipped.
    let claimed = if corrupt {
        let mut other = content.clone();
        other[0] ^= 0xFF;
        *blake3::hash(&other).as_bytes()
    } else {
        *blake3::hash(&content).as_bytes()
    };
    stage(staging_root, build_id, &claimed, &content)?;
    let start = Instant::now();
    let (resp, _) = conn.call(Req::Promote { digest: claimed }, &[])?;
    let elapsed = start.elapsed();
    match resp {
        Resp::Ok => {
            println!(
                "RESULT promote=ok digest={} bytes={} elapsed_ms={}",
                hex::encode(claimed),
                content.len(),
                elapsed.as_millis()
            );
            let mib_s = (content.len() as f64 / (1 << 20) as f64) / elapsed.as_secs_f64();
            println!(
                "PERF promote_throughput mib_s={mib_s:.1} bytes={}",
                content.len()
            );
        }
        Resp::Err(kind) => {
            println!(
                "RESULT promote=err kind={} digest={}",
                kind_name(&kind),
                hex::encode(claimed)
            );
            if !corrupt {
                bail!("promote of well-formed content failed: {kind}");
            }
        }
        other => bail!("unexpected promote reply: {other:?}"),
    }
    Ok(())
}

fn append_promote(
    socket: &Path,
    build_id: &str,
    staging_root: &Path,
    size_mib: usize,
) -> anyhow::Result<()> {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut conn = Conn::connect(socket)?;
    let (_, _fuse_fd) = mount(&mut conn, build_id)?;
    let content = gen_content(0xA99E4D, size_mib << 20);
    let digest = *blake3::hash(&content).as_bytes();
    let path = stage(staging_root, build_id, &digest, &content)?;

    let stop = Arc::new(AtomicBool::new(false));
    let appender = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || -> u64 {
            let mut appended = 0u64;
            let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) else {
                return 0;
            };
            while !stop.load(Ordering::Relaxed) {
                if f.write_all(&[0xEE; 64 * 1024]).is_err() {
                    break;
                }
                appended += 64 * 1024;
            }
            appended
        })
    };
    let result = conn.call(Req::Promote { digest }, &[]);
    stop.store(true, Ordering::Relaxed);
    let appended = appender.join().unwrap_or(0);
    match result?.0 {
        // Copy finished before the first append landed: published entry
        // is exactly `content` (the driver re-hashes it).
        Resp::Ok => println!(
            "RESULT append_promote=ok digest={} bytes={} appended={appended}",
            hex::encode(digest),
            content.len()
        ),
        // Bounded copy read appended bytes inside its st_size window:
        // rejected, nothing published.
        Resp::Err(ErrKind::DigestMismatch) => {
            println!(
                "RESULT append_promote=mismatch digest={} appended={appended}",
                hex::encode(digest)
            );
        }
        other => bail!("unexpected append-promote reply: {other:?}"),
    }
    Ok(())
}

fn backing_bench(
    socket: &Path,
    build_id: &str,
    backing_file: &Path,
    iters: usize,
) -> anyhow::Result<()> {
    let mut conn = Conn::connect(socket)?;
    let (_, fuse_fd) = mount(&mut conn, build_id)?;
    let _bg = start_fuse(fuse_fd)?;

    let mut rtts = Vec::with_capacity(iters);
    for _ in 0..iters {
        let f = std::fs::File::open(backing_file)
            .with_context(|| format!("open {}", backing_file.display()))?;
        let start = Instant::now();
        let (resp, _) = conn.call(Req::BackingOpen, &[f.as_raw_fd()])?;
        let id = match resp {
            Resp::BackingId(id) => id,
            other => bail!("BackingOpen failed: {other:?}"),
        };
        rtts.push(start.elapsed());
        let (resp, _) = conn.call(Req::BackingClose { backing_id: id }, &[])?;
        if !matches!(resp, Resp::Ok) {
            bail!("BackingClose failed: {resp:?}");
        }
    }
    rtts.sort_unstable();
    println!(
        "PERF backing_open_rtt_us p50={} p99={} max={} iters={iters}",
        percentile(&rtts, 0.5).as_micros(),
        percentile(&rtts, 0.99).as_micros(),
        rtts.last().copied().unwrap_or_default().as_micros(),
    );
    println!("RESULT backing_bench=ok iters={iters}");
    Ok(())
}

fn concurrency(
    socket: &Path,
    build_id: &str,
    staging_root: &Path,
    backing_file: &Path,
    promote_mib: usize,
    iters: usize,
) -> anyhow::Result<()> {
    let mut conn = Conn::connect(socket)?;
    let (_, fuse_fd) = mount(&mut conn, build_id)?;
    let _bg = start_fuse(fuse_fd)?;

    let content = gen_content(0xC04C44, promote_mib << 20);
    let digest = *blake3::hash(&content).as_bytes();
    stage(staging_root, build_id, &digest, &content)?;
    let promote_seq = conn.send(Req::Promote { digest }, &[])?;

    // BackingOpen/Close pairs while the promote copies. Replies are
    // seq-correlated because the promote's reply can land between any
    // two backing replies.
    let mut rtts = Vec::with_capacity(iters);
    let mut promote_resp: Option<Resp> = None;
    let mut backing_done_before_promote = 0usize;
    fn recv_until(
        conn: &mut Conn,
        want_seq: u32,
        promote_seq: u32,
        promote_resp: &mut Option<Resp>,
    ) -> anyhow::Result<Resp> {
        loop {
            let (reply, _) = conn.recv()?;
            if reply.seq == promote_seq {
                *promote_resp = Some(reply.resp);
                continue;
            }
            if reply.seq == want_seq {
                return Ok(reply.resp);
            }
            bail!(
                "unexpected reply seq {} (waiting for {want_seq})",
                reply.seq
            );
        }
    }
    for _ in 0..iters {
        let f = std::fs::File::open(backing_file)
            .with_context(|| format!("open {}", backing_file.display()))?;
        let start = Instant::now();
        let seq = conn.send(Req::BackingOpen, &[f.as_raw_fd()])?;
        let id = match recv_until(&mut conn, seq, promote_seq, &mut promote_resp)? {
            Resp::BackingId(id) => id,
            other => bail!("BackingOpen failed: {other:?}"),
        };
        rtts.push(start.elapsed());
        if promote_resp.is_none() {
            backing_done_before_promote += 1;
        }
        let seq = conn.send(Req::BackingClose { backing_id: id }, &[])?;
        if !matches!(
            recv_until(&mut conn, seq, promote_seq, &mut promote_resp)?,
            Resp::Ok
        ) {
            bail!("BackingClose failed");
        }
    }
    // Drain the promote reply if it has not arrived yet.
    while promote_resp.is_none() {
        let (reply, _) = conn.recv()?;
        if reply.seq == promote_seq {
            promote_resp = Some(reply.resp);
        }
    }
    if !matches!(promote_resp, Some(Resp::Ok)) {
        bail!("concurrent promote failed: {promote_resp:?}");
    }
    rtts.sort_unstable();
    println!(
        "PERF concurrent_backing_rtt_us p50={} p99={} iters={iters} promote_mib={promote_mib}",
        percentile(&rtts, 0.5).as_micros(),
        percentile(&rtts, 0.99).as_micros(),
    );
    println!(
        "RESULT concurrency=ok backing_before_promote={backing_done_before_promote} iters={iters}"
    );
    Ok(())
}

fn fill_staging(
    socket: &Path,
    build_id: &str,
    staging_root: &Path,
    give_up_mib: usize,
) -> anyhow::Result<()> {
    use std::io::Write;

    let mut conn = Conn::connect(socket)?;
    let (quota, _fuse_fd) = mount(&mut conn, build_id)?;
    let path = staging_root.join(build_id).join("fill");
    let mut f =
        std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let block = vec![0x55u8; 1 << 20];
    let mut written = 0u64;
    loop {
        match f.write_all(&block).and_then(|()| f.sync_all()) {
            Ok(()) => {
                written += block.len() as u64;
                if written >= (give_up_mib as u64) << 20 {
                    bail!(
                        "wrote {written} bytes without ENOSPC (quota={quota}) — project quota not enforced"
                    );
                }
            }
            // XFS reports project-quota exhaustion as ENOSPC (a
            // deliberate kernel special case for directory-tree
            // quotas); ext4-with-prjquota reports EDQUOT. Both mean
            // the kernel stopped the write at the limit.
            Err(e)
                if e.raw_os_error() == Some(nix::libc::ENOSPC)
                    || e.raw_os_error() == Some(nix::libc::EDQUOT) =>
            {
                println!("RESULT fill_staging=enospc written={written} quota={quota}");
                return Ok(());
            }
            Err(e) => return Err(e).context("write to staging"),
        }
    }
}
