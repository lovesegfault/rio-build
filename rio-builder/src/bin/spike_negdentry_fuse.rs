//! Spike: FUSE-layer negative-dentry caching via `ReplyEntry{ino=0, entry_valid=MAX}`.
//!
//! Settles ADR-022 §2.4's negative-reply row. The kernel's `fuse_lookup_name`
//! comments "Zero nodeid is same as -ENOENT, but with valid timeout" and
//! `fuse_dentry_revalidate`'s `if (!inode) goto invalid` is INSIDE the
//! timeout-expired branch. This binary proves it empirically: it counts
//! `lookup` upcalls per name, replying `ino=0, ttl=MAX` for "missing-ino0"
//! and `error(ENOENT)` for "missing-err". The VM test stats each 3× and
//! asserts ino0 → 1 upcall, err → 3 upcalls.
//!
//! NOT production code. M7 review-finding spike.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, MountOption,
    ReplyAttr, ReplyEntry, Request, SessionACL,
};

const TTL: Duration = Duration::MAX;
const INO_ROOT: u64 = 1;

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

// All-zero attr with ino=0 — kernel reads only nodeid + entry_valid for the
// negative-cache path (fuse_lookup_name: `if (err || !outarg->nodeid) goto out`).
fn neg_attr() -> FileAttr {
    let mut a = dir_attr(0);
    a.kind = FileType::RegularFile;
    a.perm = 0;
    a.nlink = 0;
    a
}

struct NegSpike {
    counts: Arc<Mutex<HashMap<String, u64>>>,
}

impl Filesystem for NegSpike {
    fn getattr(&self, _r: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 == INO_ROOT {
            reply.attr(&TTL, &dir_attr(INO_ROOT));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn lookup(&self, _r: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent.0 != INO_ROOT {
            reply.error(Errno::ENOENT);
            return;
        }
        let name = name.to_string_lossy().into_owned();
        *self.counts.lock().unwrap().entry(name.clone()).or_default() += 1;
        match name.as_str() {
            // Cached negative: nodeid=0 + entry_valid=MAX.
            "missing-ino0" => reply.entry(&TTL, &neg_attr(), Generation(0)),
            // Uncached negative: plain ENOENT (kernel sets entry_valid=0).
            _ => reply.error(Errno::ENOENT),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mount = args
        .next()
        .expect("usage: spike_negdentry_fuse <mount> <counter-file>");
    let counter_file = args
        .next()
        .expect("usage: spike_negdentry_fuse <mount> <counter-file>");

    let counts = Arc::new(Mutex::new(HashMap::new()));
    let counts_w = Arc::clone(&counts);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let snap: String = counts_w
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{k}={v}\n"))
                .collect();
            let _ = std::fs::write(&counter_file, snap);
        }
    });

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("spike-negdentry".into()),
        MountOption::AutoUnmount,
    ];
    config.acl = SessionACL::All;
    fuser::mount2(NegSpike { counts }, std::path::Path::new(&mount), &config).unwrap();
}
