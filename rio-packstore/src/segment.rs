//! Writer-owned segment packs.
//!
//! INVARIANT (ADR-024): each writer process appends to its OWN
//! segment, never anyone else's, and holds a SHARED flock on it for
//! the segment's lifetime. GC/repack take-exclusive-or-skip on that
//! flock, so a live writer's segment is never repacked or unlinked
//! out from under it. Nothing is ever mutated in place — segments are
//! append-only, and readers holding old fds stay POSIX-safe across
//! repack.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::lock;

pub(crate) const PACKS_DIR: &str = "packs";
pub(crate) const PACK_SUFFIX: &str = ".pack";

pub(crate) struct Segment {
    file: File,
    pub name: String,
    pub len: u64,
}

impl Segment {
    /// Create a fresh uniquely-named segment and take the shared
    /// writer flock before returning — there is no window in which the
    /// segment exists unlocked (GC lists the directory and could
    /// otherwise unlink a freshly created, still-empty segment).
    pub(crate) fn create(packs_dir: &Path) -> io::Result<Segment> {
        let pid = std::process::id();
        for attempt in 0u32..1024 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let name = format!("seg-{nanos:x}-{pid}-{attempt}{PACK_SUFFIX}");
            // 0600: segments hold fetched private source trees; tight
            // even if the 0700 store directory is later loosened.
            match OpenOptions::new()
                .create_new(true)
                .append(true)
                .read(true)
                .mode(0o600)
                .open(packs_dir.join(&name))
            {
                Ok(file) => {
                    lock::lock_shared(&file)?;
                    // Make the new pack's dirent durable before any
                    // index rewrite can reference the pack: sync()
                    // covers the record bytes and index::write covers
                    // the index, but without this directory fsync a
                    // crash can erase the whole pack file while the
                    // durable index points at it — after a repack
                    // (sources already unlinked) that is data loss,
                    // not a stale cache entry.
                    File::open(packs_dir)?.sync_data()?;
                    return Ok(Segment { file, name, len: 0 });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("could not allocate a unique segment name"))
    }

    /// Append one encoded record; returns the record's start offset.
    /// Unbuffered on purpose: records are written as one `write_all`
    /// of a pre-assembled buffer, so `get` on freshly written data
    /// reads coherently through the page cache without a flush step.
    pub(crate) fn append(&mut self, encoded: &[u8]) -> io::Result<u64> {
        let offset = self.len;
        self.file.write_all(encoded)?;
        self.len += encoded.len() as u64;
        Ok(offset)
    }

    /// One fsync per flush, not per record — per-blob fsync measured
    /// 113.7s for a cold nixpkgs ingest vs 49ms append-pack (ADR-024).
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// A pack file as seen on disk by GC and the rebuild scanner.
pub(crate) struct PackFile {
    pub name: String,
    pub size: u64,
    pub mtime_unix: u64,
}

pub(crate) fn list_packs(packs_dir: &Path) -> io::Result<Vec<PackFile>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(packs_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !name.ends_with(PACK_SUFFIX) {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime_unix = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(PackFile {
            name,
            size: meta.len(),
            mtime_unix,
        });
    }
    // Deterministic processing order for GC and rebuild.
    out.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub(crate) fn total_pack_bytes(packs_dir: &Path) -> io::Result<u64> {
    Ok(list_packs(packs_dir)?.iter().map(|p| p.size).sum())
}

pub(crate) fn pack_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(PACKS_DIR).join(name)
}
