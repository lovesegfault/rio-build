//! Spike: FUSE passthrough under overlay (P0578).
//!
//! A flat read-only FUSE filesystem that serves files from `<cache_dir>` via
//! `FOPEN_PASSTHROUGH`. Negotiates `max_stack_depth=1` at init so overlay can
//! stack on top (depth 2 = `FILESYSTEM_MAX_STACK_DEPTH`).
//!
//! On startup, before serving, runs an ioctl probe on a `dup()` of the
//! `/dev/fuse` fd and writes results to `<probe_out>`:
//!   - `root_backing_open`: ioctl on the dup as root → expect `Ok`
//!   - `unpriv_backing_open`: same ioctl after `setresuid(<uid>)` in a forked
//!     child → expect `EPERM` (kernel `capable(CAP_SYS_ADMIN)` check)
//!
//! NOT production code. See ADR-022 §2.5/§2.9 / P0578 spike rationale.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Context;
use fuser::{
    BackingId, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request, Session, SessionACL,
};
use nix::libc;
use nix::mount::{MsFlags, mount};

const TTL: Duration = Duration::from_secs(3600);
const INO_ROOT: u64 = 1;
const INO_FILE_BASE: u64 = 0x100;

struct Entry {
    name: String,
    path: PathBuf,
    size: u64,
}

struct PassthroughFs {
    entries: Vec<Entry>,
    /// One live BackingId per ino, refcounted across opens. Re-registering
    /// a *new* backing for an ino whose prior open hasn't finished releasing
    /// (deferred fput within a single syscall — overlay copy-up does several
    /// opens of the lower) trips the kernel's `fi->fb != fb` check
    /// (`-EBUSY` → user-visible `EIO`). The production castore-FUSE keys
    /// this on `file_digest`, not `ino` — same fix.
    open: Mutex<HashMap<u64, (BackingId, u32)>>,
}

impl PassthroughFs {
    fn scan(cache_dir: &Path) -> anyhow::Result<Self> {
        let mut entries = Vec::new();
        for de in std::fs::read_dir(cache_dir)? {
            let de = de?;
            if !de.file_type()?.is_file() {
                continue;
            }
            let Some(name) = de.file_name().to_str().map(str::to_owned) else {
                anyhow::bail!("non-UTF-8 cache entry: {:?}", de.file_name());
            };
            let size = de.metadata()?.len();
            entries.push(Entry {
                name,
                path: de.path(),
                size,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        eprintln!(
            "spike_passthrough_fuse: {} entries from {cache_dir:?}",
            entries.len()
        );
        Ok(Self {
            entries,
            open: Mutex::new(HashMap::new()),
        })
    }

    fn dir_attr() -> FileAttr {
        FileAttr {
            ino: INodeNo(INO_ROOT),
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

    fn entry_for(&self, ino: u64) -> Option<&Entry> {
        self.entries.get((ino.checked_sub(INO_FILE_BASE)?) as usize)
    }
}

impl Filesystem for PassthroughFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // BOTH calls are required: `add_capabilities(FUSE_PASSTHROUGH)` puts
        // the flag in the FUSE_INIT reply (without it `fc->passthrough` stays
        // 0 → BACKING_OPEN is unconditionally EPERM); `set_max_stack_depth`
        // sets the depth field. fuser does not couple them — easy to miss.
        // Depth 1 → FUSE superblock has s_stack_depth=1 → overlay (depth 2)
        // can stack on top, and BACKING_OPEN accepts depth-0 backing files.
        config
            .add_capabilities(InitFlags::FUSE_PASSTHROUGH)
            .map_err(|unsup| io::Error::other(format!("kernel lacks {unsup:?}")))?;
        config
            .set_max_stack_depth(1)
            .map_err(|max| io::Error::other(format!("max_stack_depth>{max}")))?;
        eprintln!(
            "spike_passthrough_fuse: init (uid={}, max_stack_depth=1)",
            nix::unistd::geteuid()
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent.0 != INO_ROOT {
            reply.error(Errno::ENOENT);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.entries.iter().position(|e| e.name == name) {
            Some(i) => {
                let e = &self.entries[i];
                reply.entry(
                    &TTL,
                    &Self::file_attr(INO_FILE_BASE + i as u64, e.size),
                    Generation(0),
                );
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 == INO_ROOT {
            reply.attr(&TTL, &Self::dir_attr());
        } else if let Some(e) = self.entry_for(ino.0) {
            reply.attr(&TTL, &Self::file_attr(ino.0, e.size));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some(e) = self.entry_for(ino.0) else {
            reply.error(Errno::EISDIR);
            return;
        };
        // Lock held across open_backing() so two concurrent opens of the
        // same ino can't both register a fresh backing.
        let mut open = self.open.lock().unwrap();
        if let Some((bid, refcount)) = open.get_mut(&ino.0) {
            *refcount += 1;
            eprintln!(
                "spike_passthrough_fuse: open({}) → passthrough (reuse, rc={refcount})",
                e.name
            );
            // No FOPEN_KEEP_CACHE: the kernel's `FOPEN_PASSTHROUGH_MASK`
            // rejects any open flag outside {PASSTHROUGH, DIRECT_IO,
            // PARALLEL_DIRECT_WRITES, NOFLUSH} with user-visible EIO.
            reply.opened_passthrough(FileHandle(ino.0), FopenFlags::empty(), bid);
            return;
        }
        let f = match File::open(&e.path) {
            Ok(f) => f,
            Err(err) => {
                eprintln!("spike_passthrough_fuse: open({:?}) failed: {err}", e.path);
                reply.error(Errno::EIO);
                return;
            }
        };
        let bid = match reply.open_backing(&f) {
            Ok(b) => b,
            Err(err) => {
                eprintln!("spike_passthrough_fuse: open_backing failed: {err}");
                reply.error(Errno::EIO);
                return;
            }
        };
        eprintln!(
            "spike_passthrough_fuse: open({}) → passthrough (new)",
            e.name
        );
        reply.opened_passthrough(FileHandle(ino.0), FopenFlags::empty(), &bid);
        open.insert(ino.0, (bid, 1));
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
        let mut open = self.open.lock().unwrap();
        if let Some((_, refcount)) = open.get_mut(&fh.0) {
            *refcount -= 1;
            if *refcount == 0 {
                open.remove(&fh.0);
            }
        }
        reply.ok();
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
        // Passthrough means the kernel never upcalls read. This path is
        // unreachable in steady state; keep it so a regression (missing
        // FOPEN_PASSTHROUGH bit) surfaces as wrong content, not a hang.
        eprintln!(
            "spike_passthrough_fuse: UNEXPECTED read upcall for ino={}",
            ino.0
        );
        let Some(e) = self.entry_for(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let data = std::fs::read(&e.path).unwrap_or_default();
        let off = offset as usize;
        let end = (off + size as usize).min(data.len());
        reply.data(&data[off.min(data.len())..end]);
    }

    /// No fileattr/chattr support; the production castore-FUSE won't either
    /// (input paths are immutable). ENOTTY (not ENOSYS — fuser's default)
    /// is what the kernel maps to "no fileattr support" in `ovl_copy_fileattr`.
    fn ioctl(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: fuser::IoctlFlags,
        cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: fuser::ReplyIoctl,
    ) {
        eprintln!(
            "spike_passthrough_fuse: ioctl(ino={}, cmd={cmd:#x}) → ENOTTY",
            ino.0
        );
        reply.error(Errno::ENOTTY);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino.0 != INO_ROOT {
            reply.error(Errno::ENOTDIR);
            return;
        }
        for (i, e) in self.entries.iter().enumerate().skip(offset as usize) {
            if reply.add(
                INodeNo(INO_FILE_BASE + i as u64),
                (i + 1) as u64,
                FileType::RegularFile,
                &e.name,
            ) {
                break;
            }
        }
        reply.ok();
    }
}

// ── ioctl probe ────────────────────────────────────────────────────────────
//
// `fuse_backing_map` and the BACKING_OPEN ioctl are pub(crate) in fuser; the
// kernel ABI is stable so re-declare for the probe. This is the privileged
// surface rio-mountd brokers: the FUSE server cannot call it (assertion ii),
// so mountd does (assertion iii) on a `dup()` of the same /dev/fuse fd.

#[repr(C)]
struct FuseBackingMap {
    fd: u32,
    flags: u32,
    padding: u64,
}

/// `_IOW(type, nr, sizeof(T))` for x86_64/arm64. nix's `ioctl` feature is
/// off; computing the request code inline is fewer lines than turning it on.
const fn iow<T>(ty: u32, nr: u32) -> libc::c_ulong {
    const IOC_WRITE: u32 = 1;
    ((IOC_WRITE << 30) | ((std::mem::size_of::<T>() as u32) << 16) | (ty << 8) | nr)
        as libc::c_ulong
}
const FUSE_DEV_IOC_MAGIC: u32 = 229;
const FUSE_DEV_IOC_BACKING_OPEN: libc::c_ulong = iow::<FuseBackingMap>(FUSE_DEV_IOC_MAGIC, 1);
const FUSE_DEV_IOC_BACKING_CLOSE: libc::c_ulong = iow::<u32>(FUSE_DEV_IOC_MAGIC, 2);

/// SAFETY: caller passes a live /dev/fuse fd and a valid `FuseBackingMap`.
unsafe fn raw_backing_open(fd: i32, map: &FuseBackingMap) -> nix::Result<i32> {
    let r = unsafe { libc::ioctl(fd, FUSE_DEV_IOC_BACKING_OPEN, map as *const _) };
    nix::errno::Errno::result(r)
}

/// SAFETY: caller passes a live /dev/fuse fd and a backing_id from `_OPEN`.
unsafe fn raw_backing_close(fd: i32, id: &u32) -> nix::Result<i32> {
    let r = unsafe { libc::ioctl(fd, FUSE_DEV_IOC_BACKING_CLOSE, id as *const _) };
    nix::errno::Errno::result(r)
}

fn probe_backing_ioctl(dup_fd: &File, probe_file: &Path, drop_uid: u32) -> String {
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::{ForkResult, fork};

    let mut out = String::new();

    // Root path: ioctl on the dup'd /dev/fuse fd → expect a backing_id.
    let f = match File::open(probe_file) {
        Ok(f) => f,
        Err(e) => {
            return format!("probe_open_failed err={e}\n");
        }
    };
    let map = FuseBackingMap {
        fd: f.as_raw_fd() as u32,
        flags: 0,
        padding: 0,
    };
    match unsafe { raw_backing_open(dup_fd.as_raw_fd(), &map) } {
        Ok(id) => {
            out.push_str(&format!("root_backing_open=ok id={id}\n"));
            let id_u32 = id as u32;
            let _ = unsafe { raw_backing_close(dup_fd.as_raw_fd(), &id_u32) };
        }
        Err(e) => out.push_str(&format!("root_backing_open=err errno={e}\n")),
    }

    // Unpriv path: fork, drop to <drop_uid>, retry → expect EPERM.
    // Forked so the parent (the FUSE server) keeps root; the child can never
    // escalate back. exit(errno) is the report channel.
    // SAFETY: post-fork the child only calls async-signal-safe syscalls.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            let r = nix::unistd::setresuid(
                nix::unistd::Uid::from_raw(drop_uid),
                nix::unistd::Uid::from_raw(drop_uid),
                nix::unistd::Uid::from_raw(drop_uid),
            );
            let code = if r.is_err() {
                255
            } else {
                match unsafe { raw_backing_open(dup_fd.as_raw_fd(), &map) } {
                    Ok(_) => 0,
                    Err(e) => e as i32,
                }
            };
            // SAFETY: post-fork; immediate exit.
            unsafe { libc::_exit(code) };
        }
        Ok(ForkResult::Parent { child }) => match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => {
                let label = match code {
                    0 => "ok-UNEXPECTED".to_string(),
                    255 => "setuid-failed".to_string(),
                    n if n == libc::EPERM => "EPERM".to_string(),
                    n => format!("errno={n}"),
                };
                out.push_str(&format!("unpriv_backing_open={label}\n"));
            }
            other => out.push_str(&format!("unpriv_backing_open=waitpid {other:?}\n")),
        },
        Err(e) => out.push_str(&format!("unpriv_backing_open=fork-failed {e}\n")),
    }
    out
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mnt, cache_dir, probe_out, drop_uid] = args.as_slice() else {
        anyhow::bail!("usage: spike_passthrough_fuse <mount> <cache_dir> <probe_out> <drop_uid>");
    };
    let drop_uid: u32 = drop_uid.parse().context("drop_uid")?;
    let cache = Path::new(cache_dir);

    let fs = PassthroughFs::scan(cache)?;
    let probe_file = fs.entries.first().map(|e| e.path.clone());

    // Manual mount so we control the /dev/fuse fd and can dup it for the
    // probe (the dup models rio-mountd's kept fd).
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .context("open /dev/fuse")?;
    let dup = f.try_clone().context("dup /dev/fuse")?;
    let raw = f.as_raw_fd();
    let uid = nix::unistd::geteuid();
    let gid = nix::unistd::getegid();
    let data = format!(
        "fd={raw},rootmode=40000,user_id={uid},group_id={gid},allow_other,default_permissions,ro"
    );
    mount(
        Some("spike-passthrough"),
        mnt.as_str(),
        Some("fuse"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_RDONLY,
        Some(data.as_str()),
    )
    .with_context(|| format!("mount fuse at {mnt}"))?;
    eprintln!("spike_passthrough_fuse: mounted at {mnt}");

    // mount_options/acl on Config only apply to the mount() path; from_fd
    // takes acl positionally and never mounts.
    let mut config = Config::default();
    config.n_threads = Some(2);
    // from_fd handshakes synchronously: by the time it returns, the kernel
    // has FUSE_INIT'd with passthrough negotiated, so the probe ioctl below
    // doesn't race the init.
    let session = Session::from_fd(fs, OwnedFd::from(f), SessionACL::All, config)?;

    let probe = match probe_file {
        Some(p) => probe_backing_ioctl(&dup, &p, drop_uid),
        None => "probe=skipped (empty cache dir)\n".to_string(),
    };
    std::fs::write(probe_out, &probe)?;
    eprintln!("spike_passthrough_fuse: probe written to {probe_out}\n{probe}");

    // Sentinel for the test driver: probe results are durable, server is
    // about to enter its serve loop.
    std::fs::write(format!("{probe_out}.ready"), "ready\n")?;

    let bg = session.spawn()?;
    bg.join()?;
    Ok(())
}
