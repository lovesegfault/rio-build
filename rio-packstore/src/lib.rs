//! Client-side pack store for the rio:// eval store (ADR-024 P1).
//!
//! A content-addressed store with NO loose objects: append-only
//! segment packs hold self-describing, resyncable records; one index
//! file caches `digest → (pack, offset, len, kind)` plus the root
//! table. The index is a cache of the packs, never the truth —
//! corrupt or missing, it is rebuilt by scanning packs.
//!
//! Concurrency model (all advisory flocks, no daemon, no threads):
//!
//! - Each writer process appends to its OWN segment under a shared
//!   flock held for the segment's lifetime.
//! - Every index rewrite is load + merge + rename under the exclusive
//!   `gc.lock` flock.
//! - GC (mark + repack, one mechanism) runs synchronously at open
//!   time when a cheap trigger fires, and only if the exclusive lock
//!   is free; it skips any segment whose writer flock it cannot take.
//!
//! Fork-safety: this type spawns no threads and starts no background
//! work, ever — it lives inside a nix eval process that forks workers.
//! It is also intentionally `!Sync` (single-threaded interior
//! mutability); each process/worker owns its own handle.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

mod gc;
mod index;
mod lock;
mod record;
mod segment;

pub use gc::GcStats;

use index::{BlobLoc, IndexView, RootEntry};
use segment::Segment;

/// blake3 of the payload bytes — 32 raw bytes, the one digest space
/// shared with rio-store's castore (chunks, directories, derivations).
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// Hash `bytes` the way the store does.
    pub fn of(bytes: &[u8]) -> Digest {
        Digest(*blake3::hash(bytes).as_bytes())
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Digest({self})")
    }
}

/// Record kind. A u8 newtype, not a closed enum, so callers add kinds
/// without touching this crate; values `0xF0..=0xFF` are reserved for
/// store-internal records (the root table lives in packs too — packs
/// alone must rebuild the index).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Kind(pub u8);

impl Kind {
    /// Per-directory castore-proto blob.
    pub const DIRECTORY: Kind = Kind(0);
    /// Per-file chunking metadata (FastCDC chunk list).
    pub const FILE_CHUNK_META: Kind = Kind(1);
    /// Fetched content: flake inputs, fetchTree/fetchurl results, IFD
    /// outputs — bytes with no other local home.
    pub const FETCHED: Kind = Kind(2);
}

/// First reserved kind value; [`PackStore::put`] rejects these.
pub const KIND_RESERVED_BASE: u8 = 0xF0;

/// Internal: a root-table snapshot record.
pub(crate) const KIND_ROOT: Kind = Kind(0xF0);

/// Record header bytes preceding each payload:
/// `magic(4) kind(1) flags(1) len(4 LE) digest(32)`.
pub const RECORD_HEADER_LEN: usize = 4 + 1 + 1 + 4 + 32;

/// Reserved record flag: payload is zstd-compressed.
/// TODO: compression is deferred to a follow-up — this flag reserves
/// the bit in the record format. Per ADR-024 it must be applied per
/// pack or per large record, never per blob (140B mean blob size).
pub const FLAG_ZSTD: u8 = 0b0000_0001;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt pack store: {0}")]
    Corrupt(String),
    #[error("kind {0:#04x} is reserved for store-internal records")]
    ReservedKind(u8),
    #[error("record payload too large: {0} bytes (max 4 GiB)")]
    TooLarge(u64),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Tuning knobs. The defaults are the ADR-024 values; tests shrink
/// them to force GC paths deterministically.
#[derive(Clone, Debug)]
pub struct Options {
    /// Evict LRU roots (then repack) when live bytes exceed this.
    pub size_cap_bytes: Option<u64>,
    /// GC trigger: more segment packs than this.
    pub max_segments: usize,
    /// GC trigger: approx dead bytes / total pack bytes above this.
    pub dead_ratio_trigger: f64,
    /// In-flight grace window: roots used more recently than this are
    /// never evicted, and packs younger than this are never repacked
    /// (their blobs may not be rooted yet).
    pub grace: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            size_cap_bytes: None,
            max_segments: 64,
            dead_ratio_trigger: 0.5,
            grace: Duration::from_secs(6 * 60 * 60),
        }
    }
}

/// The pack store handle. One per process (or per fork worker); see
/// the module docs for the locking protocol it participates in.
pub struct PackStore {
    dir: PathBuf,
    /// `gc.lock` — the exclusive advisory lock for GC and every index
    /// rewrite. Held only for the duration of those operations.
    lock_file: File,
    /// Decoded on-disk index (plus post-GC state). A cache, like the
    /// file it mirrors: stale entries are healed by rebuild-and-retry
    /// in [`PackStore::get`].
    view: RefCell<IndexView>,
    /// Records this process appended to its own segment. Always valid
    /// — we hold the shared writer flock, so GC cannot unlink them.
    own: HashMap<Digest, BlobLoc>,
    own_roots: HashMap<String, RootEntry>,
    segment: Option<Segment>,
    /// Read fd cache. POSIX keeps reads through these safe even if a
    /// concurrent GC unlinks the pack; a digest mismatch or open
    /// failure falls back to a rebuild.
    read_fds: RefCell<HashMap<Arc<str>, File>>,
    last_gc: Option<GcStats>,
}

impl PackStore {
    /// Open (creating if needed) the store at `dir`, run the cheap GC
    /// trigger check, and run GC inline if a trigger fires and the
    /// exclusive lock is free. No background work survives this call.
    pub fn open(dir: impl AsRef<Path>, opts: Options) -> Result<PackStore> {
        let dir = dir.as_ref().to_path_buf();
        // Owner-only: the store holds fetched private source trees
        // (flake inputs). Modes apply at creation only — pre-existing
        // stores keep whatever the operator chose.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join(segment::PACKS_DIR))?;
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join("gc.lock"))?;

        let mut view = load_or_rebuild(&dir)?;
        let mut last_gc = None;
        if gc::triggers_fire(&dir, &opts, &view)? && lock::try_lock_exclusive(&lock_file)? {
            let result = (|| {
                // Re-load AND re-check under the lock: another process
                // may have flushed or completed a GC between our first
                // check and lock acquisition — running anyway would
                // repack an already-clean store.
                let fresh = load_or_rebuild(&dir)?;
                if !gc::triggers_fire(&dir, &opts, &fresh)? {
                    return Ok((fresh, None));
                }
                gc::run(&dir, &opts, unix_now(), fresh).map(|(v, s)| (v, Some(s)))
            })();
            lock::unlock(&lock_file)?;
            let (gc_view, stats) = result?;
            view = gc_view;
            last_gc = stats;
        }

        Ok(PackStore {
            dir,
            lock_file,
            view: RefCell::new(view),
            own: HashMap::new(),
            own_roots: HashMap::new(),
            segment: None,
            read_fds: RefCell::new(HashMap::new()),
            last_gc,
        })
    }

    /// Idempotent content-addressed write. Re-putting bytes the store
    /// already holds writes nothing.
    pub fn put(&mut self, kind: Kind, bytes: &[u8]) -> Result<Digest> {
        if kind.0 >= KIND_RESERVED_BASE {
            return Err(Error::ReservedKind(kind.0));
        }
        let digest = Digest::of(bytes);
        if self.contains(&digest) {
            return Ok(digest);
        }
        let seg = self.segment()?;
        let rec_offset = seg.append_record(kind, bytes, &digest)?;
        let pack = seg.name.clone();
        self.own.insert(
            digest,
            BlobLoc {
                pack,
                offset: rec_offset + RECORD_HEADER_LEN as u64,
                len: bytes.len() as u32,
                kind,
            },
        );
        Ok(digest)
    }

    /// Fetch a blob. `None` means the store has no record of the
    /// digest. Every read is digest-verified; a verification failure
    /// (stale index after a concurrent repack) heals by rebuilding the
    /// in-memory view from the packs and retrying once.
    pub fn get(&self, digest: &Digest) -> Result<Option<Bytes>> {
        if let Some(loc) = self.own.get(digest) {
            // Own-segment records cannot go stale: the shared writer
            // flock keeps GC away. A failure here is a real error.
            return self.read_verified(loc, digest).map(Some);
        }
        let loc = self.view.borrow().blobs.get(digest).cloned();
        let Some(loc) = loc else {
            return Ok(None);
        };
        if let Ok(bytes) = self.read_verified(&loc, digest) {
            return Ok(Some(bytes));
        }
        // Stale view: the pack was repacked/unlinked since we loaded
        // the index. The packs are the truth — rescan them.
        *self.view.borrow_mut() = rebuild_from_packs(&self.dir)?;
        self.read_fds.borrow_mut().clear();
        let loc = self.view.borrow().blobs.get(digest).cloned();
        match loc {
            Some(loc) => self.read_verified(&loc, digest).map(Some),
            None => Ok(None),
        }
    }

    /// Presence check against this process's view (own writes plus the
    /// index loaded at open / last heal). Never touches the disk.
    pub fn contains(&self, digest: &Digest) -> bool {
        self.own.contains_key(digest) || self.view.borrow().blobs.contains_key(digest)
    }

    /// The record kind this process's view has for `digest`, if any.
    /// A digest's kind never changes (records are content-addressed and
    /// immutable), so a stale view still answers correctly for any
    /// digest it knows. Never touches the disk.
    pub fn kind_of(&self, digest: &Digest) -> Option<Kind> {
        if let Some(loc) = self.own.get(digest) {
            return Some(loc.kind);
        }
        self.view.borrow().blobs.get(digest).map(|loc| loc.kind)
    }

    /// Record (or replace) a root: a store path and the digests it
    /// pins. Roots are the GC mark sources and the LRU eviction unit.
    pub fn add_root(&mut self, store_path: &str, digests: &[Digest]) -> Result<()> {
        self.add_root_at(store_path, digests, unix_now())
    }

    /// [`PackStore::add_root`] with a caller-supplied last-use clock
    /// (UNIX seconds). The clock is an input, not ambient state, so
    /// replay and LRU tests stay deterministic.
    pub fn add_root_at(
        &mut self,
        store_path: &str,
        digests: &[Digest],
        last_use_unix: u64,
    ) -> Result<()> {
        let entry = RootEntry {
            last_use: last_use_unix,
            digests: digests.to_vec(),
        };
        self.append_root_record(store_path, &entry)?;
        self.own_roots.insert(store_path.to_string(), entry);
        Ok(())
    }

    /// Bump a root's last-use timestamp (the LRU clock). Returns false
    /// if the root is unknown.
    pub fn touch_root(&mut self, store_path: &str) -> Result<bool> {
        let existing = self
            .own_roots
            .get(store_path)
            .cloned()
            .or_else(|| self.view.borrow().roots.get(store_path).cloned());
        let Some(mut entry) = existing else {
            return Ok(false);
        };
        entry.last_use = unix_now();
        self.append_root_record(store_path, &entry)?;
        self.own_roots.insert(store_path.to_string(), entry);
        Ok(true)
    }

    /// All root names this process can see (its own roots plus the
    /// index view), deduplicated. Order is unspecified.
    pub fn root_names(&self) -> Vec<String> {
        let view = self.view.borrow();
        let mut names: Vec<String> = view
            .roots
            .keys()
            .chain(self.own_roots.keys())
            .cloned()
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// A root's last-use clock (UNIX seconds), if the root exists. Own
    /// (this-process) entries win over the loaded index view.
    pub fn root_last_use(&self, store_path: &str) -> Option<u64> {
        if let Some(entry) = self.own_roots.get(store_path) {
            return Some(entry.last_use);
        }
        self.view.borrow().roots.get(store_path).map(|e| e.last_use)
    }

    /// Whether a root exists, without cloning its digest list (the
    /// hot-path presence probe — `is_valid_path` runs once per eval
    /// store-path mention).
    pub fn has_root(&self, store_path: &str) -> bool {
        self.own_roots.contains_key(store_path) || self.view.borrow().roots.contains_key(store_path)
    }

    /// The digests a root pins, if the root exists.
    pub fn root_digests(&self, store_path: &str) -> Option<Vec<Digest>> {
        if let Some(entry) = self.own_roots.get(store_path) {
            return Some(entry.digests.clone());
        }
        self.view
            .borrow()
            .roots
            .get(store_path)
            .map(|e| e.digests.clone())
    }

    /// Durability point: fsync the segment, then merge this process's
    /// entries into the on-disk index (load + merge + rename under the
    /// exclusive flock — see the index module docs for why a plain
    /// write-new + rename is data loss).
    pub fn flush(&mut self) -> Result<()> {
        if self.own.is_empty() && self.own_roots.is_empty() {
            // Nothing local to persist — don't take the exclusive lock
            // and rewrite the whole index for a read-only handle.
            return Ok(());
        }
        if let Some(seg) = &self.segment {
            seg.sync()?;
        }
        lock::lock_exclusive(&self.lock_file)?;
        let result = self.merge_index();
        // Unlock even when the merge failed — a stuck exclusive lock
        // would block every other writer's flush forever.
        lock::unlock(&self.lock_file)?;
        result
    }

    /// Drop the writer segment without flushing. Fork-worker hygiene:
    /// a child inherits the parent's segment, whose O_APPEND file
    /// description is shared with the parent and every sibling —
    /// appending through it interleaves records and desyncs this
    /// handle's offset bookkeeping. After this call the next `put`
    /// allocates a fresh per-pid segment, restoring the one-writer-
    /// per-segment invariant. Closing the inherited fd does not
    /// release the parent's shared flock (same file description stays
    /// open in the parent), so inherited `own` records remain
    /// GC-protected for the parent's lifetime.
    pub fn forget_segment(&mut self) {
        self.segment = None;
    }

    /// Stats from the GC pass run by [`PackStore::open`], if one ran.
    pub fn last_gc_stats(&self) -> Option<&GcStats> {
        self.last_gc.as_ref()
    }

    fn merge_index(&mut self) -> Result<()> {
        // Re-read on disk under the lock; on-disk is the base, our own
        // entries overlay it. Entries we loaded at open are NOT merged
        // back: a concurrent GC may have repacked them, and re-adding
        // their old locations would resurrect unlinked packs.
        let mut merged = load_or_rebuild(&self.dir)?;
        for (digest, loc) in &self.own {
            merged.blobs.insert(*digest, loc.clone());
        }
        for (name, entry) in &self.own_roots {
            match merged.roots.entry(name.clone()) {
                Entry::Occupied(mut occupied) => {
                    // Newer last-use wins — ours or a concurrent
                    // toucher's; both reference the same content.
                    if occupied.get().last_use <= entry.last_use {
                        occupied.insert(entry.clone());
                    }
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(entry.clone());
                }
            }
        }
        index::write(&self.dir, &merged)?;
        *self.view.borrow_mut() = merged;
        Ok(())
    }

    fn append_root_record(&mut self, store_path: &str, entry: &RootEntry) -> Result<()> {
        let payload = index::encode_root_payload(store_path, entry)?;
        let digest = Digest::of(&payload);
        self.segment()?
            .append_record(KIND_ROOT, &payload, &digest)?;
        Ok(())
    }

    /// The writer segment, created lazily on first write — open()
    /// stays cheap and read-only stores never leave empty segments.
    fn segment(&mut self) -> Result<&mut Segment> {
        if self.segment.is_none() {
            self.segment = Some(Segment::create(&self.dir.join(segment::PACKS_DIR))?);
        }
        Ok(self.segment.as_mut().expect("just created"))
    }

    fn read_verified(&self, loc: &BlobLoc, digest: &Digest) -> Result<Bytes> {
        let mut fds = self.read_fds.borrow_mut();
        let file = match fds.entry(loc.pack.clone()) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => {
                vacant.insert(File::open(segment::pack_path(&self.dir, &loc.pack))?)
            }
        };
        let mut buf = vec![0u8; loc.len as usize];
        file.read_exact_at(&mut buf, loc.offset)?;
        if blake3::hash(&buf).as_bytes() != &digest.0 {
            return Err(Error::Corrupt(format!(
                "digest mismatch reading {digest} from {}",
                loc.pack
            )));
        }
        Ok(Bytes::from(buf))
    }
}

/// Load the index, or rebuild the view by scanning packs when it is
/// missing or corrupt (the index is a cache; packs are the truth).
fn load_or_rebuild(dir: &Path) -> Result<IndexView> {
    match index::load(dir)? {
        Some(view) => Ok(view),
        None => rebuild_from_packs(dir),
    }
}

/// Scan every pack and reconstruct the full view: blob locations from
/// data records, the root table from ROOT records (newest snapshot per
/// name wins). Read-only — torn tails are skipped here, never
/// truncated (that needs the GC + segment locks, see the record docs).
fn rebuild_from_packs(dir: &Path) -> Result<IndexView> {
    let packs_dir = dir.join(segment::PACKS_DIR);
    let mut view = IndexView::default();
    for pack in segment::list_packs(&packs_dir)? {
        let data = match fs::read(segment::pack_path(dir, &pack.name)) {
            Ok(d) => d,
            // Raced with a concurrent GC unlink — its records were
            // copied to the consolidated pack, which this scan covers.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let pack_name: Arc<str> = Arc::from(pack.name.as_str());
        for rec in record::scan(&data).records {
            if rec.kind == KIND_ROOT {
                let payload = &data[rec.payload_offset as usize..][..rec.payload_len as usize];
                let Some((name, entry)) = index::parse_root_payload(payload) else {
                    continue;
                };
                match view.roots.entry(name) {
                    Entry::Occupied(mut occupied) => {
                        if occupied.get().last_use <= entry.last_use {
                            occupied.insert(entry);
                        }
                    }
                    Entry::Vacant(vacant) => {
                        vacant.insert(entry);
                    }
                }
            } else {
                view.blobs.entry(rec.digest).or_insert(BlobLoc {
                    pack: pack_name.clone(),
                    offset: rec.payload_offset,
                    len: rec.payload_len,
                    kind: rec.kind,
                });
            }
        }
    }
    Ok(view)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
