//! Spike: digest-addressed FUSE object store for composefs-style overlay tests.
//!
//! Serves files at `/<2-hex-prefix>/<64-hex-digest>` with synthetic content.
//! Counts every FUSE upcall; a 50ms ticker thread snapshots counters to a
//! sidecar file so the VM test can read them without per-upcall write overhead.
//!
//! Modes:
//!   spike_digest_fuse <mount> <counter-file> [<manifest>]
//!     Self-mount via fuser::mount2 (privileged path).
//!   spike_digest_fuse --recv-fd <socket> <counter-file> [<manifest>]
//!     Listen on <socket>, recv pre-opened /dev/fuse fd via SCM_RIGHTS, then
//!     serve via Session::from_fd. The mount itself is done by a separate
//!     privileged helper (spike_mountd) — this is the P0541 fd-handoff path.
//!
//! NOT production code. See ADR-022 §C / P0541 spike rationale.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, IoSliceMut};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen,
    Request, Session, SessionACL,
};
use nix::cmsg_space;
use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};

const TTL: Duration = Duration::from_secs(3600);
const INO_ROOT: u64 = 1;
const INO_DIR_BASE: u64 = 0x100;
const INO_FILE_BASE: u64 = 0x1000;

#[derive(Default)]
struct Counters {
    lookup: AtomicU64,
    getattr: AtomicU64,
    open: AtomicU64,
    read: AtomicU64,
    readdir: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> String {
        format!(
            "lookup={} getattr={} open={} read={} readdir={}\n",
            self.lookup.load(Ordering::Relaxed),
            self.getattr.load(Ordering::Relaxed),
            self.open.load(Ordering::Relaxed),
            self.read.load(Ordering::Relaxed),
            self.readdir.load(Ordering::Relaxed),
        )
    }
}

struct Entry {
    digest: String,
    size: u64,
    fill: u8,
}

struct DigestFs {
    counters: Arc<Counters>,
    entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

impl DigestFs {
    fn from_manifest(path: Option<&str>, counters: Arc<Counters>) -> anyhow::Result<Self> {
        let mut entries = Vec::new();
        match path {
            Some(p) => {
                for line in std::fs::read_to_string(p)?.lines() {
                    let mut it = line.split_ascii_whitespace();
                    let (Some(d), Some(s)) = (it.next(), it.next()) else {
                        continue;
                    };
                    let size: u64 = s.parse()?;
                    let fill = u8::from_str_radix(&d[..2], 16)?;
                    entries.push(Entry {
                        digest: d.to_string(),
                        size,
                        fill,
                    });
                }
            }
            None => {
                for b in [0xaa_u8, 0xbb, 0xcc] {
                    let hex = format!("{b:02x}").repeat(32);
                    entries.push(Entry {
                        digest: hex,
                        size: 1024 * 1024,
                        fill: b,
                    });
                }
            }
        }
        let by_name = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.digest.clone(), i))
            .collect();
        eprintln!("spike_digest_fuse: {} entries loaded", entries.len());
        Ok(Self {
            counters,
            entries,
            by_name,
        })
    }

    fn dir_attr(ino: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
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
        }
    }

    fn file_attr(ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

impl Filesystem for DigestFs {
    fn init(&mut self, _req: &Request, _config: &mut fuser::KernelConfig) -> Result<(), io::Error> {
        eprintln!("spike_digest_fuse: init (uid={})", nix::unistd::geteuid());
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.counters.lookup.fetch_add(1, Ordering::Relaxed);
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        if parent.0 == INO_ROOT {
            if name.len() == 2
                && let Ok(b) = u8::from_str_radix(name, 16)
            {
                reply.entry(
                    &TTL,
                    &Self::dir_attr(INO_DIR_BASE | u64::from(b)),
                    Generation(0),
                );
                return;
            }
        } else if (INO_DIR_BASE..INO_DIR_BASE + 256).contains(&parent.0)
            && let Some(&idx) = self.by_name.get(name)
        {
            let e = &self.entries[idx];
            reply.entry(
                &TTL,
                &Self::file_attr(INO_FILE_BASE + idx as u64, e.size),
                Generation(0),
            );
            return;
        }
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.counters.getattr.fetch_add(1, Ordering::Relaxed);
        match ino.0 {
            INO_ROOT => reply.attr(&TTL, &Self::dir_attr(INO_ROOT)),
            i if (INO_DIR_BASE..INO_DIR_BASE + 256).contains(&i) => {
                reply.attr(&TTL, &Self::dir_attr(i));
            }
            i if i >= INO_FILE_BASE && ((i - INO_FILE_BASE) as usize) < self.entries.len() => {
                let e = &self.entries[(i - INO_FILE_BASE) as usize];
                reply.attr(&TTL, &Self::file_attr(i, e.size));
            }
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.counters.open.fetch_add(1, Ordering::Relaxed);
        if ino.0 >= INO_FILE_BASE && ((ino.0 - INO_FILE_BASE) as usize) < self.entries.len() {
            reply.opened(FileHandle(ino.0), FopenFlags::FOPEN_KEEP_CACHE);
        } else {
            reply.error(Errno::EISDIR);
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
        self.counters.read.fetch_add(1, Ordering::Relaxed);
        let idx = (ino.0 - INO_FILE_BASE) as usize;
        let Some(e) = self.entries.get(idx) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let end = (offset + u64::from(size)).min(e.size);
        let len = end.saturating_sub(offset) as usize;
        reply.data(&vec![e.fill; len]);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        self.counters.readdir.fetch_add(1, Ordering::Relaxed);
        if ino.0 == INO_ROOT {
            for b in (offset as u16)..256 {
                let name = format!("{b:02x}");
                if reply.add(
                    INodeNo(INO_DIR_BASE | u64::from(b)),
                    u64::from(b) + 1,
                    FileType::Directory,
                    &name,
                ) {
                    break;
                }
            }
            reply.ok();
        } else if (INO_DIR_BASE..INO_DIR_BASE + 256).contains(&ino.0) {
            reply.ok();
        } else {
            reply.error(Errno::ENOTDIR);
        }
    }
}

fn recv_fd(socket_path: &str) -> anyhow::Result<OwnedFd> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    eprintln!("spike_digest_fuse: listening for fd on {socket_path}");
    let (stream, _) = listener.accept()?;
    let mut cmsg = cmsg_space!([RawFd; 1]);
    let mut buf = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::empty(),
    )?;
    for c in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = c
            && let Some(fd) = fds.into_iter().next()
        {
            // SAFETY: fd was just delivered by the kernel via SCM_RIGHTS;
            // we are its sole owner.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
    anyhow::bail!("no SCM_RIGHTS control message received")
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let counters = Arc::new(Counters::default());

    let spawn_ticker = |counter_path: std::path::PathBuf| {
        std::fs::write(&counter_path, counters.snapshot()).ok();
        let c = Arc::clone(&counters);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(50));
                let _ = std::fs::write(&counter_path, c.snapshot());
            }
        });
    };

    if args.first().map(String::as_str) == Some("--recv-fd") {
        let socket = args
            .get(1)
            .expect("--recv-fd <socket> <counter> [manifest]");
        let counter_path: std::path::PathBuf = args
            .get(2)
            .expect("--recv-fd <socket> <counter> [manifest]")
            .into();
        let manifest = args.get(3).map(String::as_str);
        spawn_ticker(counter_path);
        let fs = DigestFs::from_manifest(manifest, Arc::clone(&counters))?;
        let fd = recv_fd(socket)?;
        eprintln!(
            "spike_digest_fuse: received fd={}, serving (euid={})",
            fd.as_raw_fd(),
            nix::unistd::geteuid()
        );
        let mut config = Config::default();
        config.n_threads = Some(2);
        let bg = Session::from_fd(fs, fd, SessionACL::All, config)?.spawn()?;
        bg.join()?;
        return Ok(());
    }

    let mount_point = args
        .first()
        .expect("usage: spike_digest_fuse <mount> <counter-file> [<manifest>]");
    let counter_path: std::path::PathBuf = args
        .get(1)
        .expect("usage: spike_digest_fuse <mount> <counter-file> [<manifest>]")
        .into();
    let manifest = args.get(2).map(String::as_str);
    spawn_ticker(counter_path);
    let fs = DigestFs::from_manifest(manifest, Arc::clone(&counters))?;
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("spike-digest".into()),
        MountOption::AutoUnmount,
    ];
    config.acl = SessionACL::All;
    config.n_threads = Some(2);
    eprintln!("spike_digest_fuse: mounting at {mount_point}");
    fuser::mount2(fs, Path::new(mount_point), &config)?;
    Ok(())
}
