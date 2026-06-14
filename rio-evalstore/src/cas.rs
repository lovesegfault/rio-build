//! On-disk content-addressed store layout + flat-file index.
//!
//! Layout under the CAS root (default `$XDG_CACHE_HOME/rio/cas`):
//!
//! ```text
//! blobs/<aa>/<blake3-hex>        raw file contents, keyed by BLAKE3
//! paths/<store-basename>.json    Directory DAG (DagNode tree, blob refs)
//! index/<store-basename>.json    PathInfo (narHash, narSize, refs, ca, …)
//! fingerprints/<blake3(fs path)>.json   stat-fingerprint records
//! .lock                          advisory flock for cross-process writes
//! ```
//!
//! All writes are atomic (temp file + rename in the same directory), so
//! readers never observe torn files; the advisory lock only serializes
//! writers. This is the "simplest index that works" from the ADR-024 plan —
//! sqlite only if flat files get hairy.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use rio_nix::store_path::{STORE_DIR, StorePath};

/// Reject anything that is not a syntactically valid store-path basename
/// before it reaches a filesystem join. Basenames arrive over the FFI
/// boundary and are interpolated into `index/{basename}.json` paths — a
/// `..` or `/` must be a clean error, never a traversal.
pub(crate) fn validate_basename(basename: &str) -> io::Result<()> {
    match StorePath::parse(&format!("{STORE_DIR}/{basename}")) {
        Ok(_) => Ok(()),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad basename: {e}"),
        )),
    }
}

/// Same traversal concern for blob keys: they come from DAG files on disk,
/// which a corrupted CAS could populate with path-shaped garbage.
fn validate_blob_hex(hex: &str) -> io::Result<()> {
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad blob key {hex:?}: expected 64 lowercase hex chars"),
        ));
    }
    Ok(())
}

/// The CAS holds users' private source trees — keep everything
/// owner-only (dirs 0700, files 0600).
fn create_dir_private(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// One node of a stored Directory DAG. File contents live in the blob
/// store; the DAG holds BLAKE3 references only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DagNode {
    Regular {
        /// BLAKE3 hex of the file contents blob.
        blob: String,
        executable: bool,
        size: u64,
    },
    Symlink {
        target: String,
    },
    Directory {
        entries: BTreeMap<String, DagNode>,
    },
}

/// Index entry for one store path — everything `queryPathInfo` needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathInfo {
    /// SHA-256 of the path's NAR serialization, hex.
    pub nar_hash: String,
    pub nar_size: u64,
    /// Full `/nix/store/...` reference paths.
    pub references: Vec<String>,
    /// nix `ContentAddress` rendering (`text:sha256:<b32>`,
    /// `fixed:r:sha256:<b32>`, …) when content-addressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
    /// BLAKE3 hex of the derivation-JSON blob (drv paths only). The
    /// canonical stored form of a derivation per ADR-024.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drv_json_blob: Option<String>,
}

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
}

pub struct Cas {
    root: PathBuf,
}

impl Cas {
    pub fn open(root: PathBuf) -> io::Result<Self> {
        for sub in ["blobs", "paths", "index", "fingerprints"] {
            create_dir_private(&root.join(sub))?;
        }
        Ok(Cas { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Take the cross-process advisory write lock. Held for the duration
    /// of a commit (multi-file write); dropped on guard drop.
    ///
    /// This exclusive mode is also what the future GC sweep takes for its
    /// whole run (ADR-024, client-CAS GC: sweep at store open under the
    /// exclusive advisory lock with a grace window) — per-op writers and
    /// the sweep serialize on the same lock file.
    // TODO: implement the LRU sweep itself (ADR-024 client-CAS GC
    // paragraph): at store open, under lock_exclusive, evict path entries
    // (index + DAG files) whose mtime is older than the watermark allows,
    // then mark-sweep blobs by reachability from the surviving DAGs.
    // M0 ships the layout + LRU clock (touch_entry) only.
    pub fn lock_exclusive(&self) -> io::Result<FlockGuard> {
        FlockGuard::acquire(&self.root.join(".lock"))
    }

    /// Bump the LRU clock of a store-path entry. The eviction unit of the
    /// future GC sweep is the path entry, and its index file's mtime is
    /// the clock — explicitly set via utimensat on every access because
    /// kernel atime cannot be relied on (noatime/relatime mounts).
    /// Best-effort: a failed touch must never fail the read it decorates.
    ///
    /// Blobs deliberately carry no timestamps: the sweep decides them by
    /// mark-sweep reachability from surviving path entries.
    pub fn touch_entry(&self, basename: &str) {
        use std::os::unix::ffi::OsStrExt;
        if validate_basename(basename).is_err() {
            return;
        }
        let path = self.root.join("index").join(format!("{basename}.json"));
        let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: valid NUL-terminated path; null times = set both
        // atime/mtime to now.
        unsafe {
            libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), std::ptr::null(), 0);
        }
    }

    // -- blobs ------------------------------------------------------------

    fn blob_path(&self, hex: &str) -> PathBuf {
        self.root.join("blobs").join(&hex[..2]).join(hex)
    }

    /// Store a blob by BLAKE3. Returns `(hex digest, newly_written)` —
    /// existing blobs are not rewritten (content-addressed dedup).
    pub fn blob_put(&self, data: &[u8]) -> io::Result<(String, bool)> {
        let hex = blake3::hash(data).to_hex().to_string();
        let path = self.blob_path(&hex);
        if path.exists() {
            return Ok((hex, false));
        }
        create_dir_private(path.parent().expect("blob path has parent"))?;
        atomic_write(&path, data)?;
        Ok((hex, true))
    }

    /// Read a blob. Integrity is enforced at fetch boundaries, not per
    /// read: ingest derives the key by hashing the bytes, and any future
    /// network fill (M1 cluster fetch) must verify on arrival. Local reads
    /// trust the disk — the CAS sits in the same trust domain as the source
    /// trees it was hashed from, and downstream importers (e.g. nix's local
    /// store on IFD copy) re-verify narHash on their side.
    pub fn blob_get(&self, hex: &str) -> io::Result<Vec<u8>> {
        validate_blob_hex(hex)?;
        fs::read(self.blob_path(hex))
            .map_err(|e| io::Error::new(e.kind(), format!("blob {hex}: {e}")))
    }

    // -- DAGs (per store path) ---------------------------------------------

    pub fn dag_put(&self, basename: &str, dag: &DagNode) -> io::Result<()> {
        validate_basename(basename)?;
        let data = serde_json::to_vec(dag).map_err(io::Error::other)?;
        atomic_write(
            &self.root.join("paths").join(format!("{basename}.json")),
            &data,
        )
    }

    pub fn dag_get(&self, basename: &str) -> io::Result<Option<DagNode>> {
        validate_basename(basename)?;
        read_json(&self.root.join("paths").join(format!("{basename}.json")))
    }

    // -- index ---------------------------------------------------------------

    pub fn index_put(&self, basename: &str, info: &PathInfo) -> io::Result<()> {
        validate_basename(basename)?;
        let data = serde_json::to_vec(info).map_err(io::Error::other)?;
        atomic_write(
            &self.root.join("index").join(format!("{basename}.json")),
            &data,
        )
    }

    pub fn index_get(&self, basename: &str) -> io::Result<Option<PathInfo>> {
        validate_basename(basename)?;
        read_json(&self.root.join("index").join(format!("{basename}.json")))
    }

    pub fn index_contains(&self, basename: &str) -> bool {
        if validate_basename(basename).is_err() {
            return false;
        }
        self.root
            .join("index")
            .join(format!("{basename}.json"))
            .exists()
    }

    /// Linear scan for a basename starting with `hash_part`. Fine for the
    /// flat-file index at client scale; revisit with the index format.
    pub fn index_find_by_hash_part(&self, hash_part: &str) -> io::Result<Option<String>> {
        for entry in fs::read_dir(self.root.join("index"))? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(basename) = name.strip_suffix(".json") else {
                continue;
            };
            if basename.starts_with(hash_part) {
                return Ok(Some(basename.to_string()));
            }
        }
        Ok(None)
    }

    // -- fingerprints ----------------------------------------------------

    fn fingerprint_file(&self, fs_path: &str, method_key: &str) -> PathBuf {
        let key = blake3::hash(format!("{method_key}\0{fs_path}").as_bytes())
            .to_hex()
            .to_string();
        self.root.join("fingerprints").join(format!("{key}.json"))
    }

    pub fn fingerprint_put(&self, rec: &FingerprintRecord) -> io::Result<()> {
        let data = serde_json::to_vec(rec).map_err(io::Error::other)?;
        atomic_write(
            &self.fingerprint_file(&rec.fingerprint.path, &rec.method_key),
            &data,
        )
    }

    pub fn fingerprint_get(
        &self,
        fs_path: &str,
        method_key: &str,
    ) -> io::Result<Option<FingerprintRecord>> {
        read_json(&self.fingerprint_file(fs_path, method_key))
    }
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
/// fsync, rename. Readers see either the old file or the complete new one.
fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().expect("atomic_write target has parent");
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        blake3::hash(data).to_hex()
    ));
    {
        // 0600: see create_dir_private.
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

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_and_dedup() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cas = Cas::open(dir.path().to_path_buf())?;
        let (hex, new) = cas.blob_put(b"hello world")?;
        assert!(new);
        let (hex2, new2) = cas.blob_put(b"hello world")?;
        assert_eq!(hex, hex2);
        assert!(!new2, "second put of identical content must dedup");
        assert_eq!(cas.blob_get(&hex)?, b"hello world");
        Ok(())
    }

    #[test]
    fn dag_and_index_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cas = Cas::open(dir.path().to_path_buf())?;
        let dag = DagNode::Directory {
            entries: BTreeMap::from([(
                "f".to_string(),
                DagNode::Regular {
                    blob: "00".repeat(32),
                    executable: true,
                    size: 3,
                },
            )]),
        };
        let base = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x";
        cas.dag_put(base, &dag)?;
        assert_eq!(cas.dag_get(base)?, Some(dag));
        assert_eq!(
            cas.dag_get("cccccccccccccccccccccccccccccccc-missing")?,
            None
        );

        let info = PathInfo {
            nar_hash: "aa".repeat(32),
            nar_size: 120,
            references: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-r".into()],
            ca: None,
            drv_json_blob: None,
        };
        cas.index_put(base, &info)?;
        assert!(cas.index_contains(base));
        let got = cas.index_get(base)?.expect("present");
        assert_eq!(got.nar_size, 120);
        assert_eq!(cas.index_find_by_hash_part("bbb")?, Some(base.to_string()));
        assert_eq!(cas.index_find_by_hash_part("zzz")?, None);
        Ok(())
    }

    #[test]
    fn fingerprint_roundtrip_keyed_by_method() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cas = Cas::open(dir.path().to_path_buf())?;
        let f = dir.path().join("src.txt");
        fs::write(&f, "content")?;
        let fp = stat_fingerprint(f.to_str().unwrap())?;
        let rec = FingerprintRecord {
            fingerprint: fp.clone(),
            method_key: "nar".into(),
            store_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src.txt".into(),
            recorded_at_ns: 1,
        };
        cas.fingerprint_put(&rec)?;
        let got = cas
            .fingerprint_get(f.to_str().unwrap(), "nar")?
            .expect("present");
        assert_eq!(got.fingerprint, fp);
        // Different method key → independent slot.
        assert!(cas.fingerprint_get(f.to_str().unwrap(), "flat")?.is_none());
        Ok(())
    }

    #[test]
    fn traversal_basenames_are_rejected_before_any_path_join() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cas = Cas::open(dir.path().join("cas"))?;
        let victim = dir.path().join("victim.json");
        for bad in [
            "../../victim",
            "a/../../victim",
            "/etc/passwd",
            "..",
            "not-a-store-basename",
        ] {
            assert_eq!(
                cas.dag_get(bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "dag_get({bad:?})"
            );
            assert_eq!(
                cas.index_get(bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "index_get({bad:?})"
            );
            let dag = DagNode::Symlink { target: "x".into() };
            assert_eq!(
                cas.dag_put(bad, &dag).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "dag_put({bad:?})"
            );
            assert!(!cas.index_contains(bad));
            cas.touch_entry(bad); // must be a no-op, not a join
        }
        assert!(!victim.exists(), "traversal escaped the CAS root");

        // Blob keys from a (possibly corrupted) DAG get the same treatment.
        assert_eq!(
            cas.blob_get("../escape").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        Ok(())
    }

    #[test]
    fn cas_contents_are_owner_only() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let cas = Cas::open(dir.path().join("cas"))?;
        let (hex, _) = cas.blob_put(b"private source")?;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cas.blob_path(&hex)), 0o600);
        assert_eq!(mode(cas.blob_path(&hex).parent().unwrap()), 0o700);
        assert_eq!(mode(&cas.root().join("index")), 0o700);
        Ok(())
    }

    #[test]
    fn flock_guard_acquires() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cas = Cas::open(dir.path().to_path_buf())?;
        let _g = cas.lock_exclusive()?;
        Ok(())
    }
}
