//! The eval-store core: ingest, path computation + cross-checks, read-back.
//!
//! Every mutating op recomputes the store path with rio-nix and compares it
//! against the path nix computed for the same content (supplied by the C++
//! shim). A mismatch is a hard error carrying both paths — ADR-024's
//! "make the scariest failure mode loud" rule.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use thiserror::Error;

use rio_nix::derivation::{Derivation, DerivationError};
use rio_nix::hash::{HashAlgo, HashError, NixHash};
use rio_nix::nar::{self, NarEntry, NarError, NarNode};
use rio_nix::store_path::{self, StorePath, StorePathError, nixbase32};

use crate::cas::{Cas, DagNode, FingerprintRecord, PathInfo, stat_fingerprint};
use crate::stats::Stats;

/// Per-file content cap for in-memory ingest, mirroring rio-nix's NAR
/// parser bound. M0 ingests file contents through memory; larger sources
/// get a clean error instead of an OOM.
const MAX_FLAT_SIZE: u64 = 256 * 1024 * 1024;

/// Racy-fingerprint slack (ADR-024): the kernel files mtimes from a ~10ms
/// coarse clock, so a same-size in-place rewrite within the tick keeps the
/// full stat fingerprint and would silently serve stale content. Distrust
/// any record whose file mtime is within this window of the record's own
/// write time. 100ms = 10× the measured tick; P1 re-validates the constant
/// on developer-common filesystems before pinning the default.
const FINGERPRINT_SLACK_NS: i128 = 100_000_000;

#[derive(Debug, Error)]
pub enum EvalStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("NAR error: {0}")]
    Nar(#[from] NarError),

    #[error("store path error: {0}")]
    StorePath(#[from] StorePathError),

    #[error("derivation error: {0}")]
    Derivation(#[from] DerivationError),

    #[error("hash error: {0}")]
    Hash(#[from] HashError),

    #[error(
        "store path cross-check FAILED for {op} of '{name}': rio-nix computed {rust_path} but \
         nix computed {nix_path} — refusing to continue (one of the two hashing \
         implementations is wrong)"
    )]
    PathMismatch {
        op: &'static str,
        name: String,
        rust_path: String,
        nix_path: String,
    },

    #[error(
        "ATerm round-trip FAILED for derivation '{name}': rio-nix unparse(parse(aterm)) differs \
         from nix's original bytes — refusing to continue (parser/serializer divergence)"
    )]
    AtermRoundTrip { name: String },

    #[error(
        "NAR hash cross-check FAILED for '{path}': computed sha256:{computed} but caller \
         claimed sha256:{claimed}"
    )]
    NarHashMismatch {
        path: String,
        computed: String,
        claimed: String,
    },

    #[error(
        "path '{0}' is not in the client CAS — cluster-backed reads for foreign paths arrive \
         in ADR-024 M1; M0 serves only paths this store ingested"
    )]
    ForeignPath(String),

    #[error("no such entry '{rel}' in store object '{basename}'")]
    NoSuchEntry { basename: String, rel: String },

    #[error("unsupported in rio-evalstore M0: {0}")]
    Unsupported(String),

    #[error("client CAS is corrupt: {0}")]
    Corrupt(String),

    #[error("derivation is not valid UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

type Result<T> = std::result::Result<T, EvalStoreError>;

/// How the incoming dump stream is serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpMethod {
    Flat,
    NixArchive,
}

/// How the store path is content-addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaMethod {
    Flat,
    NixArchive,
    Text,
}

impl CaMethod {
    fn as_str(self) -> &'static str {
        match self {
            CaMethod::Flat => "flat",
            CaMethod::NixArchive => "nar",
            CaMethod::Text => "text",
        }
    }
}

/// Hashes computed during ingest, handed to the shim's path callback so it
/// can compute nix's view of the store path for the cross-check.
#[derive(Debug, serde::Serialize)]
pub struct AddHashes {
    /// SHA-256 of the NAR serialization, hex.
    pub nar_sha256: String,
    pub nar_size: u64,
    /// SHA-256 of the content per the CA method (== nar_sha256 for
    /// NixArchive; flat file-contents hash for Flat/Text), hex.
    pub content_sha256: String,
}

/// Result of a successful ingest.
#[derive(Debug, serde::Serialize)]
pub struct AddResult {
    /// Full `/nix/store/...` path.
    pub path: String,
    pub nar_sha256: String,
    pub nar_size: u64,
}

/// Path info as provided by nix for `addToStore(info, source)`.
#[derive(Debug, serde::Deserialize)]
pub struct ProvidedInfo {
    /// Full `/nix/store/...` path.
    pub path: String,
    /// SHA-256 of the NAR, hex.
    pub nar_hash: String,
    pub nar_size: u64,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub ca: Option<String>,
}

pub struct EvalStore {
    cas: Cas,
    stats: Stats,
}

impl EvalStore {
    /// Open (creating if needed) the client CAS. `cas_override` comes from
    /// the `?cas=` URI parameter; the default is `$XDG_CACHE_HOME/rio/cas`
    /// (falling back to `$HOME/.cache/rio/cas` per the XDG spec).
    pub fn open(cas_override: Option<&str>) -> Result<Self> {
        let root = match cas_override {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => default_cas_dir()?,
        };
        Ok(EvalStore {
            cas: Cas::open(root)?,
            stats: Stats::default(),
        })
    }

    // -- ingest -----------------------------------------------------------

    /// Ingest a dump stream (`addToStoreFromDump`). Single pass: the NAR
    /// bytes are hashed while being parsed. `nix_path_for` receives the
    /// computed hashes and must return nix's store path for the same
    /// content; any divergence from rio-nix's computation is a hard error.
    pub fn add_from_dump(
        &self,
        name: &str,
        dump_method: DumpMethod,
        ca_method: CaMethod,
        references: &[String],
        reader: &mut dyn Read,
        nix_path_for: &mut dyn FnMut(&AddHashes) -> Result<String>,
    ) -> Result<AddResult> {
        let refs = parse_refs(references)?;

        // Parse + hash in one pass.
        let (node, nar_sha256, nar_size) = match dump_method {
            DumpMethod::NixArchive => {
                let mut hr = HashingReader::new(reader);
                let node = nar::parse(&mut hr)?;
                let (hash, count) = hr.finish();
                (node, hash, count)
            }
            DumpMethod::Flat => {
                let mut contents = Vec::new();
                let mut limited = reader.take(MAX_FLAT_SIZE + 1);
                limited.read_to_end(&mut contents)?;
                if contents.len() as u64 > MAX_FLAT_SIZE {
                    return Err(EvalStoreError::Unsupported(format!(
                        "flat dump '{name}' exceeds the {MAX_FLAT_SIZE}-byte M0 in-memory bound"
                    )));
                }
                let node = NarNode::Regular {
                    executable: false,
                    contents,
                };
                let (hash, size) = nar_hash_of_node(&node)?;
                (node, hash, size)
            }
        };

        // Content hash per CA method.
        let content_sha256 = match ca_method {
            CaMethod::NixArchive => nar_sha256,
            CaMethod::Flat | CaMethod::Text => match &node {
                NarNode::Regular { contents, .. } => {
                    let h: [u8; 32] = Sha256::digest(contents).into();
                    h
                }
                _ => {
                    return Err(EvalStoreError::Unsupported(format!(
                        "{} content-addressing of a non-regular-file root for '{name}'",
                        ca_method.as_str()
                    )));
                }
            },
        };

        // rio-nix path computation.
        let rust_path = match ca_method {
            CaMethod::NixArchive => StorePath::make_fixed_output(
                name,
                &NixHash::new(HashAlgo::SHA256, content_sha256.to_vec())?,
                true,
                &refs,
            )?,
            CaMethod::Flat => StorePath::make_fixed_output(
                name,
                &NixHash::new(HashAlgo::SHA256, content_sha256.to_vec())?,
                false,
                &refs,
            )?,
            CaMethod::Text => StorePath::make_text(
                name,
                &NixHash::new(HashAlgo::SHA256, content_sha256.to_vec())?,
                &refs,
            )?,
        };

        // Cross-check against nix's own computation.
        let hashes = AddHashes {
            nar_sha256: hex::encode(nar_sha256),
            nar_size,
            content_sha256: hex::encode(content_sha256),
        };
        let nix_path = nix_path_for(&hashes)?;
        if nix_path != rust_path.as_str() {
            return Err(EvalStoreError::PathMismatch {
                op: "addToStoreFromDump",
                name: name.to_string(),
                rust_path: rust_path.to_string(),
                nix_path,
            });
        }

        let ca = Some(render_ca(ca_method, &content_sha256));
        let info = PathInfo {
            nar_hash: hashes.nar_sha256.clone(),
            nar_size,
            references: references.to_vec(),
            ca,
            drv_json_blob: None,
        };
        self.commit(rust_path.basename(), &node, info)?;
        self.stats.record("add_from_dump", nar_size);

        Ok(AddResult {
            path: rust_path.to_string(),
            nar_sha256: hashes.nar_sha256,
            nar_size,
        })
    }

    /// Ingest a NAR for a path whose info nix already knows
    /// (`addToStore(info, source)`). The path is taken as given (it may be
    /// input-addressed and thus not recomputable from content); the NAR
    /// hash is cross-checked against the claimed one.
    pub fn add_nar(&self, info: &ProvidedInfo, reader: &mut dyn Read) -> Result<()> {
        let path = StorePath::parse(&info.path)?;
        let mut hr = HashingReader::new(reader);
        let node = nar::parse(&mut hr)?;
        let (hash, count) = hr.finish();
        let computed = hex::encode(hash);
        if computed != info.nar_hash {
            return Err(EvalStoreError::NarHashMismatch {
                path: info.path.clone(),
                computed,
                claimed: info.nar_hash.clone(),
            });
        }
        let stored = PathInfo {
            nar_hash: computed,
            nar_size: count,
            references: info.references.clone(),
            ca: info.ca.clone(),
            drv_json_blob: None,
        };
        self.commit(path.basename(), &node, stored)?;
        self.stats.record("add_nar", count);
        Ok(())
    }

    /// Capture a derivation (`writeDerivation`): ATerm bytes are the path
    /// content (hashing is always over the original ingested bytes), the
    /// derivation JSON is stored as the canonical blob per ADR-024.
    ///
    /// Two hard cross-checks: rio-nix's unparse(parse(aterm)) must
    /// reproduce nix's bytes, and rio-nix's text-path computation must
    /// match `nix_drv_path`.
    pub fn write_derivation(
        &self,
        name: &str,
        aterm: &[u8],
        drv_json: &[u8],
        nix_drv_path: &str,
    ) -> Result<String> {
        let text = std::str::from_utf8(aterm)?;
        let drv = Derivation::parse(text)?;
        if drv.to_aterm() != text {
            return Err(EvalStoreError::AtermRoundTrip {
                name: name.to_string(),
            });
        }

        let mut ref_strs: Vec<String> = drv.input_srcs().iter().cloned().collect();
        ref_strs.extend(drv.input_drvs().keys().cloned());
        let refs = parse_refs(&ref_strs)?;

        let content_sha256: [u8; 32] = Sha256::digest(aterm).into();
        let rust_path = StorePath::make_text(
            name,
            &NixHash::new(HashAlgo::SHA256, content_sha256.to_vec())?,
            &refs,
        )?;
        if nix_drv_path != rust_path.as_str() {
            return Err(EvalStoreError::PathMismatch {
                op: "writeDerivation",
                name: name.to_string(),
                rust_path: rust_path.to_string(),
                nix_path: nix_drv_path.to_string(),
            });
        }

        let node = NarNode::Regular {
            executable: false,
            contents: aterm.to_vec(),
        };
        let (nar_hash, nar_size) = nar_hash_of_node(&node)?;

        let (json_blob, json_new) = {
            // Scoped: commit() below re-acquires the same flock, and a
            // second flock on a fresh fd self-deadlocks within a process.
            let _lock = self.cas.lock_exclusive()?;
            self.cas.blob_put(drv_json)?
        };
        if json_new {
            self.stats.record("blob_write", drv_json.len() as u64);
        }

        let info = PathInfo {
            nar_hash: hex::encode(nar_hash),
            nar_size,
            references: ref_strs,
            ca: Some(render_ca(CaMethod::Text, &content_sha256)),
            drv_json_blob: Some(json_blob),
        };
        self.commit(rust_path.basename(), &node, info)?;
        self.stats.record("write_derivation", aterm.len() as u64);
        Ok(rust_path.to_string())
    }

    /// Walk a NAR tree into the CAS (blobs + DAG + index) under the
    /// advisory write lock.
    fn commit(&self, basename: &str, node: &NarNode, info: PathInfo) -> Result<()> {
        let _lock = self.cas.lock_exclusive()?;
        let dag = self.ingest_node(node)?;
        self.cas.dag_put(basename, &dag)?;
        self.cas.index_put(basename, &info)?;
        Ok(())
    }

    fn ingest_node(&self, node: &NarNode) -> Result<DagNode> {
        Ok(match node {
            NarNode::Regular {
                executable,
                contents,
            } => {
                let (blob, new) = self.cas.blob_put(contents)?;
                if new {
                    self.stats.record("blob_write", contents.len() as u64);
                } else {
                    self.stats.record("blob_dedup", contents.len() as u64);
                }
                DagNode::Regular {
                    blob,
                    executable: *executable,
                    size: contents.len() as u64,
                }
            }
            NarNode::Symlink { target } => DagNode::Symlink {
                target: target.clone(),
            },
            NarNode::Directory { entries } => {
                let mut out = BTreeMap::new();
                for entry in entries {
                    out.insert(entry.name.clone(), self.ingest_node(&entry.node)?);
                }
                DagNode::Directory { entries: out }
            }
        })
    }

    // -- queries ------------------------------------------------------------

    pub fn is_valid_path(&self, basename: &str) -> bool {
        self.stats.record("is_valid_path", 0);
        let valid = self.cas.index_contains(basename);
        if valid {
            // LRU clock for the future GC sweep — see Cas::touch_entry.
            self.cas.touch_entry(basename);
        }
        valid
    }

    pub fn query_path_info(&self, basename: &str) -> Result<Option<PathInfo>> {
        self.stats.record("query_path_info", 0);
        if !is_store_basename(basename) {
            return Ok(None);
        }
        let info = self.cas.index_get(basename)?;
        if info.is_some() {
            self.cas.touch_entry(basename);
        }
        Ok(info)
    }

    pub fn query_path_from_hash_part(&self, hash_part: &str) -> Result<Option<String>> {
        self.stats.record("query_path_from_hash_part", 0);
        Ok(self
            .cas
            .index_find_by_hash_part(hash_part)?
            .map(|b| format!("{}/{b}", store_path::STORE_DIR)))
    }

    // -- read-back ----------------------------------------------------------

    /// Resolve `rel` (slash-separated, possibly empty = root) within the
    /// store object `basename`. `Ok(None)` = object known, member missing.
    /// `Err(ForeignPath)` = object not in the CAS at all.
    fn resolve(&self, basename: &str, rel: &str) -> Result<Option<DagNode>> {
        if !is_store_basename(basename) {
            return Err(foreign(basename));
        }
        let Some(mut node) = self.cas.dag_get(basename)? else {
            return Err(foreign(basename));
        };
        // Accessor reads bump the entry's LRU clock (ADR-024 CAS GC).
        self.cas.touch_entry(basename);
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            match node {
                DagNode::Directory { mut entries } => match entries.remove(comp) {
                    Some(child) => node = child,
                    None => return Ok(None),
                },
                _ => return Ok(None),
            }
        }
        Ok(Some(node))
    }

    /// `Ok(None)` when the object or member doesn't exist — whole-store
    /// accessor semantics where unknown paths are merely absent.
    pub fn lstat(&self, basename: &str, rel: &str) -> Result<Option<DagNode>> {
        self.stats.record("lstat", 0);
        match self.resolve(basename, rel) {
            Err(EvalStoreError::ForeignPath(_)) => Ok(None),
            other => other,
        }
    }

    pub fn read_directory(&self, basename: &str, rel: &str) -> Result<BTreeMap<String, DagNode>> {
        self.stats.record("read_directory", 0);
        match self.resolve(basename, rel)? {
            Some(DagNode::Directory { entries }) => Ok(entries),
            Some(_) => Err(EvalStoreError::Corrupt(format!(
                "readDirectory on non-directory {basename}/{rel}"
            ))),
            None => Err(no_such_entry(basename, rel)),
        }
    }

    pub fn read_file(&self, basename: &str, rel: &str, sink: &mut dyn Write) -> Result<u64> {
        match self.resolve(basename, rel)? {
            Some(DagNode::Regular { blob, size, .. }) => {
                let data = self.cas.blob_get(&blob).map_err(|e| {
                    EvalStoreError::Corrupt(format!("blob for {basename}/{rel}: {e}"))
                })?;
                sink.write_all(&data)?;
                self.stats.record("read_file", size);
                Ok(size)
            }
            Some(_) => Err(EvalStoreError::Corrupt(format!(
                "readFile on non-regular {basename}/{rel}"
            ))),
            None => Err(no_such_entry(basename, rel)),
        }
    }

    pub fn read_link(&self, basename: &str, rel: &str) -> Result<String> {
        self.stats.record("read_link", 0);
        match self.resolve(basename, rel)? {
            Some(DagNode::Symlink { target }) => Ok(target),
            Some(_) => Err(EvalStoreError::Corrupt(format!(
                "readLink on non-symlink {basename}/{rel}"
            ))),
            None => Err(no_such_entry(basename, rel)),
        }
    }

    /// Regenerate the NAR framing from the DAG (castore pattern: framing
    /// is never persisted) and stream it into `sink`.
    pub fn nar_from_path(&self, basename: &str, sink: &mut dyn Write) -> Result<u64> {
        if !is_store_basename(basename) {
            return Err(foreign(basename));
        }
        let Some(dag) = self.cas.dag_get(basename)? else {
            return Err(foreign(basename));
        };
        self.cas.touch_entry(basename);
        let node = self.hydrate(&dag)?;
        let mut counter = CountingWriter::new(sink);
        nar::serialize(&mut counter, &node)?;
        let written = counter.count;
        self.stats.record("nar_from_path", written);
        Ok(written)
    }

    fn hydrate(&self, dag: &DagNode) -> Result<NarNode> {
        Ok(match dag {
            DagNode::Regular {
                blob, executable, ..
            } => NarNode::Regular {
                executable: *executable,
                contents: self
                    .cas
                    .blob_get(blob)
                    .map_err(|e| EvalStoreError::Corrupt(e.to_string()))?,
            },
            DagNode::Symlink { target } => NarNode::Symlink {
                target: target.clone(),
            },
            DagNode::Directory { entries } => NarNode::Directory {
                entries: entries
                    .iter()
                    .map(|(name, child)| {
                        Ok(NarEntry {
                            name: name.clone(),
                            node: self.hydrate(child)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            },
        })
    }

    /// Read the stored derivation JSON blob for a drv path, if captured.
    pub fn read_drv_json(&self, basename: &str) -> Result<Option<Vec<u8>>> {
        match self.cas.index_get(basename)? {
            Some(PathInfo {
                drv_json_blob: Some(blob),
                ..
            }) => Ok(Some(self.cas.blob_get(&blob).map_err(|e| {
                EvalStoreError::Corrupt(format!("drv json blob: {e}"))
            })?)),
            _ => Ok(None),
        }
    }

    // -- fingerprints --------------------------------------------------------

    /// Build the discriminator for fingerprint records. The store path
    /// depends on the name, the CA method and the reference set, so all
    /// three are part of the key — omitting any of them would let e.g.
    /// `builtins.path { path = ./x; name = "other"; }` hit a record made
    /// for plain `./x` and return a wrong store path. `|` cannot occur in
    /// store-path names or store paths, so the encoding is unambiguous.
    pub fn method_key(name: &str, ca_method: CaMethod, references: &[String]) -> String {
        let mut key = format!("{name}|{}", ca_method.as_str());
        let mut refs: Vec<&str> = references.iter().map(String::as_str).collect();
        refs.sort_unstable();
        for r in refs {
            key.push('|');
            key.push_str(r);
        }
        key
    }

    /// Fingerprint shortcut: if `fs_path`'s current stat matches a
    /// recorded ingest with the same method/refs AND the recorded store
    /// path is still in the index, return it without re-reading content.
    pub fn fingerprint_lookup(&self, fs_path: &str, method_key: &str) -> Result<Option<String>> {
        let Some(rec) = self.cas.fingerprint_get(fs_path, method_key)? else {
            self.stats.record("fingerprint_miss", 0);
            return Ok(None);
        };
        let now = match stat_fingerprint(fs_path) {
            Ok(fp) => fp,
            Err(_) => {
                self.stats.record("fingerprint_miss", 0);
                return Ok(None);
            }
        };
        let basename = match store_path::basename(&rec.store_path) {
            Some(b) => b,
            None => return Ok(None),
        };
        // Racy-fingerprint rule: a record whose file mtime lands within
        // the coarse-clock slack of the record's write time could have
        // been rewritten in-place without changing its fingerprint.
        let trusted = rec.fingerprint.mtime_ns < rec.recorded_at_ns - FINGERPRINT_SLACK_NS;
        if trusted && now == rec.fingerprint && self.cas.index_contains(basename) {
            self.stats.record("fingerprint_hit", 0);
            // A hit keeps the entry live — bump its LRU clock like any
            // other read.
            self.cas.touch_entry(basename);
            Ok(Some(rec.store_path))
        } else {
            self.stats.record("fingerprint_miss", 0);
            Ok(None)
        }
    }

    pub fn fingerprint_record(
        &self,
        fs_path: &str,
        method_key: &str,
        full_store_path: &str,
    ) -> Result<()> {
        let fingerprint = stat_fingerprint(fs_path)?;
        let recorded_at_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i128);
        let _lock = self.cas.lock_exclusive()?;
        self.cas.fingerprint_put(&FingerprintRecord {
            fingerprint,
            method_key: method_key.to_string(),
            store_path: full_store_path.to_string(),
            recorded_at_ns,
        })?;
        Ok(())
    }
}

impl Drop for EvalStore {
    fn drop(&mut self) {
        if Stats::enabled() {
            eprintln!("{}", self.stats.render());
        }
    }
}

fn default_cas_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("rio").join("cas"));
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => {
            Ok(PathBuf::from(home).join(".cache").join("rio").join("cas"))
        }
        _ => Err(EvalStoreError::Unsupported(
            "neither XDG_CACHE_HOME nor HOME is set and no ?cas= override given".to_string(),
        )),
    }
}

/// FFI-boundary check (Cas re-validates before every filesystem join): a
/// string that doesn't parse as a store-path basename can never be in the
/// CAS, so callers treat it as absent rather than risking a path
/// traversal via `index/{basename}.json`.
fn is_store_basename(basename: &str) -> bool {
    StorePath::parse(&format!("{}/{basename}", store_path::STORE_DIR)).is_ok()
}

fn foreign(basename: &str) -> EvalStoreError {
    EvalStoreError::ForeignPath(format!("{}/{basename}", store_path::STORE_DIR))
}

fn no_such_entry(basename: &str, rel: &str) -> EvalStoreError {
    EvalStoreError::NoSuchEntry {
        basename: basename.to_string(),
        rel: rel.to_string(),
    }
}

fn parse_refs(references: &[String]) -> Result<Vec<StorePath>> {
    references
        .iter()
        .map(|r| StorePath::parse(r).map_err(EvalStoreError::from))
        .collect()
}

/// Render a nix `ContentAddress` string for the index.
fn render_ca(method: CaMethod, sha256: &[u8; 32]) -> String {
    let b32 = nixbase32::encode(sha256);
    match method {
        CaMethod::NixArchive => format!("fixed:r:sha256:{b32}"),
        CaMethod::Flat => format!("fixed:sha256:{b32}"),
        CaMethod::Text => format!("text:sha256:{b32}"),
    }
}

/// SHA-256 + size of a node's NAR serialization (streamed, not buffered).
fn nar_hash_of_node(node: &NarNode) -> Result<([u8; 32], u64)> {
    let mut sink = HashCountWriter::default();
    nar::serialize(&mut sink, node)?;
    Ok((sink.hasher.finalize().into(), sink.count))
}

/// Reader adapter computing SHA-256 + byte count of everything read.
struct HashingReader<'a> {
    inner: &'a mut dyn Read,
    hasher: Sha256,
    count: u64,
}

impl<'a> HashingReader<'a> {
    fn new(inner: &'a mut dyn Read) -> Self {
        HashingReader {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    fn finish(self) -> ([u8; 32], u64) {
        (self.hasher.finalize().into(), self.count)
    }
}

impl Read for HashingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

#[derive(Default)]
struct HashCountWriter {
    hasher: Sha256,
    count: u64,
}

impl Write for HashCountWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        self.count += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    count: u64,
}

impl<'a> CountingWriter<'a> {
    fn new(inner: &'a mut dyn Write) -> Self {
        CountingWriter { inner, count: 0 }
    }
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
