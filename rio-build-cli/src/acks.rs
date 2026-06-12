//! Persistent cluster-ack table (ADR-024 "the persistent asset": a
//! second invocation skips negotiation on ack hits).
//!
//! One JSON sidecar file in the CAS root (`cluster-acks.json`), the
//! same single-file + flock-merge-rename discipline as the fingerprint
//! table (`rio_evalstore::fingerprint`) — acks are mutable keyed state,
//! not content-addressed bytes, so they don't belong in the pack store.
//! Losing the file costs one re-`Has` per object, never wrong content.
//!
//! Records are scoped by `(cluster scope, kind, digest)` — switching
//! clusters or tenants must never replay another endpoint's acks —
//! and carry their own expiry (ADR-024: "client-side ack records carry
//! a TTL ≤ the cluster's minimum unpinned-blob lifetime"). Expired
//! records answer "not acked" and are dropped on the next persist.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Object kinds sharing the digest space (chunks, directories, drvs —
/// one shared blake3 space per ADR-024, but presence is negotiated per
/// kind, so acks are recorded per kind too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Chunk,
    Directory,
    Drv,
}

impl ObjectKind {
    fn tag(self) -> &'static str {
        match self {
            ObjectKind::Chunk => "chunk",
            ObjectKind::Directory => "directory",
            ObjectKind::Drv => "drv",
        }
    }
}

/// One acked digest. `expires_at_unix` is absolute wall-clock seconds
/// — records survive process restart (that's the table's point), so a
/// relative TTL would have nothing to be relative to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckRecord {
    pub scope: String,
    pub kind: ObjectKind,
    /// Hex-encoded 32-byte blake3.
    pub digest: String,
    pub expires_at_unix: u64,
    /// Wall-clock write time; merge keeps the newest record per key
    /// (same discipline as the fingerprint table).
    pub recorded_at_unix: u64,
}

/// The single-file ack table. All lookups are in-memory; every
/// `record`/`evict` persists via load + merge + rename under the
/// sibling flock so concurrent `rio build` invocations never clobber
/// each other's acks.
// r[impl bc.negotiate.ack-short-circuit]
pub struct ClusterAckTable {
    file: PathBuf,
    lock: PathBuf,
    /// Identifies the cluster + tenant identity these acks are valid
    /// for; baked into every record and filtered on lookup.
    scope: String,
    ttl: Duration,
    records: HashMap<String, AckRecord>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn key(scope: &str, kind: ObjectKind, digest: &[u8; 32]) -> String {
    format!("{scope}|{}|{}", kind.tag(), hex::encode(digest))
}

impl ClusterAckTable {
    /// Load (or start empty) under `cas_root`. `scope` should encode
    /// the cluster endpoint + tenant identity (see
    /// [`crate::config::Config::ack_scope`]).
    pub fn open(cas_root: &Path, scope: impl Into<String>, ttl: Duration) -> ClusterAckTable {
        let file = cas_root.join("cluster-acks.json");
        let records = load_records(&file);
        ClusterAckTable {
            file,
            lock: cas_root.join("cluster-acks.lock"),
            scope: scope.into(),
            ttl,
            records,
        }
    }

    /// Is `digest` acked for this scope and not expired?
    pub fn is_acked(&self, kind: ObjectKind, digest: &[u8; 32]) -> bool {
        self.records
            .get(&key(&self.scope, kind, digest))
            .is_some_and(|r| r.expires_at_unix > now_unix())
    }

    /// Record acks for `digests` and persist once. Empty input is a
    /// no-op (no file churn).
    pub fn record(&mut self, kind: ObjectKind, digests: &[[u8; 32]]) -> io::Result<()> {
        if digests.is_empty() {
            return Ok(());
        }
        let now = now_unix();
        for d in digests {
            self.records.insert(
                key(&self.scope, kind, d),
                AckRecord {
                    scope: self.scope.clone(),
                    kind,
                    digest: hex::encode(d),
                    expires_at_unix: now.saturating_add(self.ttl.as_secs()),
                    recorded_at_unix: now,
                },
            );
        }
        self.persist(&std::collections::HashSet::new())
    }

    /// Drop acks for `digests` (stale-ack recovery: the cluster
    /// rejected a submission naming these as missing) and persist.
    /// The evicted keys are tombstoned for THIS persist so the
    /// load-merge step can't resurrect them from disk; a concurrent
    /// writer that re-records the digest AFTER our evict legitimately
    /// wins on its own next persist (it re-uploaded; its ack is real).
    pub fn evict(&mut self, kind: ObjectKind, digests: &[[u8; 32]]) -> io::Result<()> {
        if digests.is_empty() {
            return Ok(());
        }
        let tombstones: std::collections::HashSet<String> =
            digests.iter().map(|d| key(&self.scope, kind, d)).collect();
        for k in &tombstones {
            self.records.remove(k);
        }
        self.persist(&tombstones)
    }

    /// Load + merge-by-newest + insert ours + atomic rename, under the
    /// flock — the fingerprint-table discipline (a plain write-new +
    /// rename silently discards a concurrent process's records).
    /// Expired records are dropped here so the file doesn't grow
    /// without bound.
    fn persist(&mut self, tombstones: &std::collections::HashSet<String>) -> io::Result<()> {
        let _guard = FlockGuard::acquire(&self.lock)?;
        let now = now_unix();
        for (k, theirs) in load_records(&self.file) {
            if tombstones.contains(&k) {
                continue;
            }
            match self.records.entry(k) {
                Entry::Occupied(mut occupied) => {
                    if occupied.get().recorded_at_unix < theirs.recorded_at_unix {
                        occupied.insert(theirs);
                    }
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(theirs);
                }
            }
        }
        self.records.retain(|_, r| r.expires_at_unix > now);
        let data = serde_json::to_vec(&self.records).map_err(io::Error::other)?;
        atomic_write(&self.file, &data)
    }
}

/// Missing or corrupt table → empty (it's a negotiation cache; the
/// worst cost is one re-`Has` per object).
fn load_records(file: &Path) -> HashMap<String, AckRecord> {
    std::fs::read(file)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

/// RAII advisory flock, same as the fingerprint table's.
struct FlockGuard {
    _file: std::fs::File,
}

impl FlockGuard {
    fn acquire(path: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
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

/// Atomic write: temp file in the same directory, fsync, rename. 0600
/// — ack records reveal what a tenant has built.
fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().expect("ack table path has a parent");
    let tmp = dir.join(format!(
        ".tmp-acks-{}-{}",
        std::process::id(),
        blake3::hash(data).to_hex()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D1: [u8; 32] = [1; 32];
    const D2: [u8; 32] = [2; 32];

    // r[verify bc.negotiate.ack-short-circuit]
    #[test]
    fn record_survives_reopen_and_scopes_isolate() {
        let dir = tempfile::tempdir().unwrap();
        let ttl = Duration::from_secs(3600);
        let mut t = ClusterAckTable::open(dir.path(), "cluster-a|tenant-x", ttl);
        t.record(ObjectKind::Drv, &[D1]).unwrap();
        assert!(t.is_acked(ObjectKind::Drv, &D1));
        // Kind is part of the key.
        assert!(!t.is_acked(ObjectKind::Chunk, &D1));

        // Survives process restart — that's the table's point.
        let reopened = ClusterAckTable::open(dir.path(), "cluster-a|tenant-x", ttl);
        assert!(reopened.is_acked(ObjectKind::Drv, &D1));

        // A different cluster/tenant scope must not see the ack.
        let other = ClusterAckTable::open(dir.path(), "cluster-b|tenant-x", ttl);
        assert!(!other.is_acked(ObjectKind::Drv, &D1));
    }

    #[test]
    fn expired_records_answer_not_acked_and_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = ClusterAckTable::open(dir.path(), "s", Duration::ZERO);
        t.record(ObjectKind::Drv, &[D1]).unwrap();
        // TTL zero ⇒ expired immediately.
        assert!(!t.is_acked(ObjectKind::Drv, &D1));
        // The next persist prunes it from disk.
        t.record(ObjectKind::Chunk, &[D2]).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("cluster-acks.json")).unwrap();
        assert!(!raw.contains(&hex::encode(D1)), "expired record persisted");
    }

    #[test]
    fn evict_removes_and_concurrent_writers_merge() {
        let dir = tempfile::tempdir().unwrap();
        let ttl = Duration::from_secs(3600);
        let mut a = ClusterAckTable::open(dir.path(), "s", ttl);
        let mut b = ClusterAckTable::open(dir.path(), "s", ttl);
        a.record(ObjectKind::Drv, &[D1]).unwrap();
        b.record(ObjectKind::Drv, &[D2]).unwrap();
        // Writer b never saw D1 in memory but must not clobber it on
        // disk (the fingerprint-table merge rule).
        let merged = ClusterAckTable::open(dir.path(), "s", ttl);
        assert!(merged.is_acked(ObjectKind::Drv, &D1));
        assert!(merged.is_acked(ObjectKind::Drv, &D2));

        a.evict(ObjectKind::Drv, &[D1]).unwrap();
        assert!(!a.is_acked(ObjectKind::Drv, &D1));
        let reopened = ClusterAckTable::open(dir.path(), "s", ttl);
        assert!(!reopened.is_acked(ObjectKind::Drv, &D1), "evict persisted");
        assert!(reopened.is_acked(ObjectKind::Drv, &D2));
    }
}
