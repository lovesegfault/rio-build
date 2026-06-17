//! Stat-fingerprint table: one file in the CAS root, not per-path
//! loose JSON (ADR-024 P1 — "no loose objects anywhere").
//!
//! `fingerprints.json` maps `blake3(method_key \0 fs_path)` → record.
//! Every rewrite is load + merge-by-newest-write-time + rename under a
//! sibling flock (`fingerprints.lock`): a plain write-new + rename
//! would silently discard a concurrent process's records. Lookups are
//! served from the in-memory copy loaded at open — a fingerprint is an
//! optimization cache, so missing a record another process wrote
//! mid-run costs one re-hash, never wrong content.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Stat fingerprint of a filesystem path at ingest time. Any mismatch on
/// lookup means re-hash — a stale entry costs a hash, never wrong content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i128,
    pub inode: u64,
    pub ctime_ns: i128,
}

/// A recorded `fingerprint → store path` mapping, scoped by the store-path
/// name, content-address method and references that produced the path (the
/// same file added under a different name, flat vs recursive, or with
/// different references yields a different store path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintRecord {
    pub fingerprint: Fingerprint,
    /// Discriminates name + CA method + refs: `"<name>|<method>|ref1|ref2"`.
    pub method_key: String,
    pub store_path: String,
    /// Wall-clock UNIX ns when this record was written. Lookup distrusts
    /// records whose file mtime falls within the coarse-clock slack of
    /// this time (racy-fingerprint rule, ADR-024). `serde(default)` makes
    /// pre-rule records (0) permanently distrusted; they re-record on the
    /// next ingest.
    #[serde(default)]
    pub recorded_at_ns: i128,
    /// For directory trees: hex blake3 over the sorted per-entry stat
    /// fingerprints of the whole tree ([`tree_stat_walk`]). `None` for
    /// regular files (the root `fingerprint` carries everything) and for
    /// records written before tree fingerprints existed — those never
    /// short-circuit a tree ingest, they just re-record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_digest: Option<String>,
}

/// Current stat fingerprint of a filesystem path (no symlink follow on the
/// final component is NOT needed here — sources are dumped through nix's
/// accessor, this is only the change-detection key).
pub fn stat_fingerprint(fs_path: &str) -> io::Result<Fingerprint> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::symlink_metadata(fs_path)?;
    Ok(Fingerprint {
        path: fs_path.to_string(),
        size: md.size(),
        mtime_ns: i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec()),
        inode: md.ino(),
        ctime_ns: i128::from(md.ctime()) * 1_000_000_000 + i128::from(md.ctime_nsec()),
    })
}

/// Aggregate of a sorted lstat-only walk over a directory tree — the
/// tree-level analog of [`stat_fingerprint`]. A warm re-eval of an
/// unchanged tree validates against this with stats alone: no file
/// reads, no content hashing, no NAR.
pub struct TreeStat {
    /// blake3 over every entry's (rel path, kind, size, mtime, inode,
    /// ctime), in sorted DFS order, root included. Any changed, added,
    /// or removed entry changes the digest — one mismatch invalidates
    /// the whole tree record (full re-ingest; no partial re-ingest).
    pub digest: [u8; 32],
    /// Newest REGULAR-FILE mtime in the tree, for the racy-fingerprint
    /// rule. Only files can mutate stat-invisibly (a same-size in-place
    /// rewrite within the mtime tick); symlinks cannot be retargeted in
    /// place (replacement changes inode/ctime) and directory content is
    /// the entry set, which the digest itself covers.
    pub latest_file_mtime_ns: i128,
}

/// Walk `fs_path` (lstat-only, byte-lex sorted DFS — the NAR entry
/// order) and fold every entry's stat fingerprint into one digest.
pub fn tree_stat_walk(fs_path: &str) -> io::Result<TreeStat> {
    use std::os::unix::fs::MetadataExt;

    fn entry_kind(md: &fs::Metadata) -> u8 {
        let ft = md.file_type();
        if ft.is_file() {
            1
        } else if ft.is_symlink() {
            2
        } else if ft.is_dir() {
            3
        } else {
            // Ingest rejects these; a distinct kind byte makes the
            // record miss so the re-ingest reports the real error.
            4
        }
    }

    fn walk(
        path: &Path,
        rel: &mut Vec<u8>,
        hasher: &mut blake3::Hasher,
        latest: &mut i128,
    ) -> io::Result<()> {
        let md = fs::symlink_metadata(path)?;
        let mtime_ns = i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec());
        let ctime_ns = i128::from(md.ctime()) * 1_000_000_000 + i128::from(md.ctime_nsec());
        hasher.update(rel);
        hasher.update(&[0, entry_kind(&md)]);
        hasher.update(&md.size().to_le_bytes());
        hasher.update(&mtime_ns.to_le_bytes());
        hasher.update(&md.ino().to_le_bytes());
        hasher.update(&ctime_ns.to_le_bytes());
        if md.file_type().is_file() {
            *latest = (*latest).max(mtime_ns);
        }
        if md.file_type().is_dir() {
            use std::os::unix::ffi::OsStrExt;
            let mut names: Vec<std::ffi::OsString> = Vec::new();
            for entry in fs::read_dir(path)? {
                names.push(entry?.file_name());
            }
            names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            for name in names {
                let saved = rel.len();
                if !rel.is_empty() {
                    rel.push(b'/');
                }
                rel.extend_from_slice(name.as_bytes());
                walk(&path.join(&name), rel, hasher, latest)?;
                rel.truncate(saved);
            }
        }
        Ok(())
    }

    let mut hasher = blake3::Hasher::new();
    let mut latest = i128::MIN;
    let mut rel = Vec::new();
    walk(Path::new(fs_path), &mut rel, &mut hasher, &mut latest)?;
    Ok(TreeStat {
        digest: *hasher.finalize().as_bytes(),
        latest_file_mtime_ns: latest,
    })
}

/// The single-file fingerprint table.
///
/// TODO: the table grows without bound (one record per (fs_path,
/// method_key) ever ingested) and every `put` rewrites the whole file.
/// Acceptable at P1 scale — records are ~300B and a working set is
/// thousands of paths, so the file stays in the low MBs and puts happen
/// once per *new/changed* source, not per eval. When the upload path
/// lands (P3), cap it with the same LRU discipline as the pack-store
/// root table and batch puts per process like the root touches.
pub struct FingerprintTable {
    file: PathBuf,
    lock: PathBuf,
    records: HashMap<String, FingerprintRecord>,
}

impl FingerprintTable {
    /// Load (or start empty) the table under `cas_root`.
    pub fn open(cas_root: &Path) -> FingerprintTable {
        let file = cas_root.join("fingerprints.json");
        let records = load_records(&file);
        FingerprintTable {
            file,
            lock: cas_root.join("fingerprints.lock"),
            records,
        }
    }

    /// Look up the record for `(fs_path, method_key)`, if any. Pure
    /// in-memory read.
    pub fn get(&self, fs_path: &str, method_key: &str) -> Option<&FingerprintRecord> {
        self.records.get(&record_key(fs_path, method_key))
    }

    /// Insert (or replace) a record and persist the table: under the
    /// flock, re-read the on-disk table, merge by newest
    /// `recorded_at_ns`, insert ours, atomic-rename the result.
    pub fn put(&mut self, rec: FingerprintRecord) -> io::Result<()> {
        let _guard = FlockGuard::acquire(&self.lock)?;
        for (key, theirs) in load_records(&self.file) {
            match self.records.entry(key) {
                Entry::Occupied(mut occupied) => {
                    if occupied.get().recorded_at_ns < theirs.recorded_at_ns {
                        occupied.insert(theirs);
                    }
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(theirs);
                }
            }
        }
        self.records
            .insert(record_key(&rec.fingerprint.path, &rec.method_key), rec);
        let data = serde_json::to_vec(&self.records).map_err(io::Error::other)?;
        atomic_write(&self.file, &data)
    }
}

/// Missing or corrupt table → empty: the table is a stat-shortcut
/// cache; the worst cost of losing it is one re-hash per source.
fn load_records(file: &Path) -> HashMap<String, FingerprintRecord> {
    fs::read(file)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn record_key(fs_path: &str, method_key: &str) -> String {
    blake3::hash(format!("{method_key}\0{fs_path}").as_bytes())
        .to_hex()
        .to_string()
}

/// RAII advisory file lock (flock). Unlocked on drop via close.
pub struct FlockGuard {
    _file: fs::File,
}

impl FlockGuard {
    fn acquire(path: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)?;
        // SAFETY: valid open fd; LOCK_EX blocks until acquired.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FlockGuard { _file: file })
    }
}

/// Write `data` to `path` atomically: temp file in the same directory,
/// fsync, rename. Readers see either the old file or the complete new
/// one. 0600 — fingerprints carry private source-tree paths.
fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().expect("atomic_write target has parent");
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        blake3::hash(data).to_hex()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, method_key: &str, store_path: &str, at: i128) -> FingerprintRecord {
        FingerprintRecord {
            fingerprint: Fingerprint {
                path: path.to_string(),
                size: 1,
                mtime_ns: 2,
                inode: 3,
                ctime_ns: 4,
            },
            method_key: method_key.to_string(),
            store_path: store_path.to_string(),
            recorded_at_ns: at,
            tree_digest: None,
        }
    }

    #[test]
    fn roundtrip_keyed_by_method_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = FingerprintTable::open(dir.path());
        table
            .put(record("/src/a", "nar", "/nix/store/x-a", 1))
            .unwrap();
        assert_eq!(
            table.get("/src/a", "nar").unwrap().store_path,
            "/nix/store/x-a"
        );
        // Different method key → independent slot.
        assert!(table.get("/src/a", "flat").is_none());
        assert!(table.get("/src/b", "nar").is_none());

        // A fresh open reads the persisted table.
        let reopened = FingerprintTable::open(dir.path());
        assert_eq!(
            reopened.get("/src/a", "nar").unwrap().store_path,
            "/nix/store/x-a"
        );
    }

    /// Two tables over the same file: a put must merge the other
    /// writer's records instead of clobbering them (the 1000/1000-lost
    /// failure mode of write-new + rename).
    #[test]
    fn put_merges_concurrent_writers_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = FingerprintTable::open(dir.path());
        let mut b = FingerprintTable::open(dir.path());
        a.put(record("/src/a", "nar", "/nix/store/x-a", 1)).unwrap();
        b.put(record("/src/b", "nar", "/nix/store/x-b", 1)).unwrap();

        let merged = FingerprintTable::open(dir.path());
        assert!(
            merged.get("/src/a", "nar").is_some(),
            "writer A's record lost"
        );
        assert!(
            merged.get("/src/b", "nar").is_some(),
            "writer B's record lost"
        );
    }

    /// Same key written by two processes: newest recorded_at_ns wins.
    #[test]
    fn newest_record_wins_on_merge() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = FingerprintTable::open(dir.path());
        let mut b = FingerprintTable::open(dir.path());
        // B (newer) persists first; A's older in-memory copy must not
        // overwrite it during A's later put of an unrelated key.
        b.put(record("/src/x", "nar", "/nix/store/new-x", 200))
            .unwrap();
        a.put(record("/src/x", "nar", "/nix/store/old-x", 100))
            .unwrap();
        // A's put inserted its own record unconditionally (it IS the
        // newest write for that key from A's view at put time) — but a
        // reload after B re-puts shows last-writer-by-clock semantics.
        b.put(record("/src/x", "nar", "/nix/store/new-x", 300))
            .unwrap();
        let merged = FingerprintTable::open(dir.path());
        assert_eq!(
            merged.get("/src/x", "nar").unwrap().store_path,
            "/nix/store/new-x"
        );
    }

    #[test]
    fn corrupt_table_starts_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("fingerprints.json"), b"{not json").unwrap();
        let mut table = FingerprintTable::open(dir.path());
        assert!(table.get("/src/a", "nar").is_none());
        // And a put heals the file.
        table
            .put(record("/src/a", "nar", "/nix/store/x-a", 1))
            .unwrap();
        assert!(
            FingerprintTable::open(dir.path())
                .get("/src/a", "nar")
                .is_some()
        );
    }
}
