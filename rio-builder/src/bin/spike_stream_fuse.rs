//! Spike: streaming-open for the composefs digest-FUSE (P0575 / ADR-022 §C.7).
//!
//! Tests whether `open()` can return after the first simulated-network chunk
//! lands while a background task fills the rest, with `read()` blocking on
//! unfilled ranges and `FOPEN_KEEP_CACHE` giving zero-upcall warm reads
//! once the page cache is populated. One synthetic 256 MiB file at
//! `/<prefix>/<digest>`; sequential 1 MiB chunks at 10 ms/chunk (~25 Gbps line
//! rate would be ~0.3 ms; 10 ms keeps wall-clock observable).
//!
//! Usage: `spike_stream_fuse <mount> <counter-file>`
//! Env: `STREAM_CHUNK_MS` (default 10), `STREAM_KEEP_CACHE` (1=on default, 0=off).
//!
//! NOT production code.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen,
    Request, SessionACL,
};

const TTL: Duration = Duration::from_secs(3600);
const INO_ROOT: u64 = 1;
const INO_PREFIX: u64 = 0x1ee;
const INO_FILE: u64 = 0x1000;

const MIB: u64 = 1024 * 1024;
const FILE_SIZE: u64 = 256 * MIB;
const CHUNK: u64 = MIB;
const FILL_BYTE: u8 = 0xee;
const PREFIX: &str = "ee";
const DIGEST: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

#[derive(Default)]
struct Counters {
    lookup: AtomicU64,
    getattr: AtomicU64,
    open: AtomicU64,
    read: AtomicU64,
    readdir: AtomicU64,
    /// Total microseconds spent inside `open()` (sum across calls).
    open_us: AtomicU64,
    /// Total microseconds `read()` spent waiting on the fill condvar.
    read_wait_us: AtomicU64,
}

struct Fill {
    /// Contiguous bytes available from offset 0. Sequential fill model.
    hwm: Mutex<u64>,
    cv: Condvar,
    started: AtomicBool,
    chunk_ms: u64,
}

impl Fill {
    fn wait_for(&self, want: u64) -> Duration {
        let t0 = Instant::now();
        let mut hwm = self.hwm.lock().unwrap();
        while *hwm < want {
            hwm = self.cv.wait(hwm).unwrap();
        }
        t0.elapsed()
    }

    fn spawn(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            let step = Duration::from_millis(this.chunk_ms);
            let mut filled = 0u64;
            while filled < FILE_SIZE {
                std::thread::sleep(step);
                filled = (filled + CHUNK).min(FILE_SIZE);
                *this.hwm.lock().unwrap() = filled;
                this.cv.notify_all();
            }
            eprintln!("spike_stream_fuse: fill complete ({} bytes)", FILE_SIZE);
        });
    }
}

struct StreamFs {
    counters: Arc<Counters>,
    fill: Arc<Fill>,
    keep_cache: bool,
}

impl StreamFs {
    fn snapshot(c: &Counters, fill: &Fill) -> String {
        let hwm = *fill.hwm.lock().unwrap();
        format!(
            "lookup={} getattr={} open={} read={} readdir={} open_us={} read_wait_us={} hwm={}\n",
            c.lookup.load(Ordering::Relaxed),
            c.getattr.load(Ordering::Relaxed),
            c.open.load(Ordering::Relaxed),
            c.read.load(Ordering::Relaxed),
            c.readdir.load(Ordering::Relaxed),
            c.open_us.load(Ordering::Relaxed),
            c.read_wait_us.load(Ordering::Relaxed),
            hwm,
        )
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

    fn file_attr() -> FileAttr {
        FileAttr {
            ino: INodeNo(INO_FILE),
            size: FILE_SIZE,
            blocks: FILE_SIZE.div_ceil(512),
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

impl Filesystem for StreamFs {
    fn init(&mut self, _req: &Request, _config: &mut fuser::KernelConfig) -> Result<(), io::Error> {
        eprintln!(
            "spike_stream_fuse: init ({} MiB file, {} ms/chunk, keep_cache={})",
            FILE_SIZE / MIB,
            self.fill.chunk_ms,
            self.keep_cache,
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.counters.lookup.fetch_add(1, Ordering::Relaxed);
        match (parent.0, name.to_str()) {
            (INO_ROOT, Some(n)) if n == PREFIX => {
                reply.entry(&TTL, &Self::dir_attr(INO_PREFIX), Generation(0));
            }
            (INO_PREFIX, Some(n)) if n == DIGEST => {
                reply.entry(&TTL, &Self::file_attr(), Generation(0));
            }
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.counters.getattr.fetch_add(1, Ordering::Relaxed);
        match ino.0 {
            INO_ROOT => reply.attr(&TTL, &Self::dir_attr(INO_ROOT)),
            INO_PREFIX => reply.attr(&TTL, &Self::dir_attr(INO_PREFIX)),
            INO_FILE => reply.attr(&TTL, &Self::file_attr()),
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.counters.open.fetch_add(1, Ordering::Relaxed);
        if ino.0 != INO_FILE {
            reply.error(Errno::EISDIR);
            return;
        }
        let t0 = Instant::now();
        // Mitigation (i): kick the fill, return after first chunk lands.
        self.fill.spawn();
        self.fill.wait_for(CHUNK);
        let us = t0.elapsed().as_micros() as u64;
        self.counters.open_us.fetch_add(us, Ordering::Relaxed);
        let flags = if self.keep_cache {
            FopenFlags::FOPEN_KEEP_CACHE
        } else {
            FopenFlags::empty()
        };
        reply.opened(FileHandle(ino.0), flags);
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
        if ino.0 != INO_FILE {
            reply.error(Errno::ENOENT);
            return;
        }
        let end = (offset + u64::from(size)).min(FILE_SIZE);
        let len = end.saturating_sub(offset) as usize;
        let waited = self.fill.wait_for(end);
        self.counters
            .read_wait_us
            .fetch_add(waited.as_micros() as u64, Ordering::Relaxed);
        reply.data(&vec![FILL_BYTE; len]);
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
        if ino.0 == INO_ROOT && offset == 0 {
            let _ = reply.add(INodeNo(INO_PREFIX), 1, FileType::Directory, PREFIX);
        }
        reply.ok();
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mount_point = args
        .next()
        .expect("usage: spike_stream_fuse <mount> <counter-file>");
    let counter_path: std::path::PathBuf = args
        .next()
        .expect("usage: spike_stream_fuse <mount> <counter-file>")
        .into();

    let chunk_ms: u64 = std::env::var("STREAM_CHUNK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let keep_cache = std::env::var("STREAM_KEEP_CACHE")
        .map(|v| v != "0")
        .unwrap_or(true);

    let counters = Arc::new(Counters::default());
    let fill = Arc::new(Fill {
        hwm: Mutex::new(0),
        cv: Condvar::new(),
        started: AtomicBool::new(false),
        chunk_ms,
    });

    std::fs::write(&counter_path, StreamFs::snapshot(&counters, &fill))?;
    {
        let c = Arc::clone(&counters);
        let f = Arc::clone(&fill);
        let p = counter_path.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(50));
                let _ = std::fs::write(&p, StreamFs::snapshot(&c, &f));
            }
        });
    }

    let fs = StreamFs {
        counters,
        fill,
        keep_cache,
    };

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("spike-stream".into()),
        MountOption::AutoUnmount,
    ];
    config.acl = SessionACL::All;
    config.n_threads = Some(4);

    eprintln!("spike_stream_fuse: mounting at {mount_point}");
    fuser::mount2(fs, Path::new(&mount_point), &config)?;
    Ok(())
}
