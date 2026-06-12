//! The eval-store core: ingest, path computation + cross-checks, read-back.
//!
//! Storage (ADR-024 "client CAS v2"):
//! - **Per-directory castore blobs** in the append-pack store
//!   ([`crate::dirblob`] + [`crate::dircache`]) — never a monolithic
//!   DAG blob, never loose files.
//! - **A path-metadata record per store path** (`PathMeta`, JSON) in
//!   the same pack store; the pack root table maps the store-path
//!   basename to its pinned digest set, and readback locates the meta
//!   record among the pins by its kind.
//! - **File contents**: streamed ingests (NAR over FFI — no origin
//!   tree on disk) store whole-file `Kind::FETCHED` records keyed by
//!   content blake3. Local source trees ([`EvalStore::add_source_tree`])
//!   store NO content — the origin tree is the byte store (the ADR's
//!   not-a-mirror rule); reads re-read the origin and digest-verify.
//! - **Derivations are memory-only**: an in-process map of ATerm
//!   bytes. Drv blobs never reach the pack store.
//!
//! Every mutating op recomputes the store path with rio-nix and compares it
//! against the path nix computed for the same content (supplied by the C++
//! shim). A mismatch is a hard error carrying both paths — ADR-024's
//! "make the scariest failure mode loud" rule.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use rio_nix::derivation::{Derivation, DerivationError};
use rio_nix::hash::{HashAlgo, HashError, NixHash};
use rio_nix::nar::{self, NarError, NarNode, frame};
use rio_nix::store_path::{self, StorePath, StorePathError, nixbase32};
use rio_packstore::{Digest, Kind, Options, PackStore};

use crate::dirblob::{BuiltDir, BuiltEntry, DirBlobError};
use crate::dircache::{DecodedDir, DirStore, DirStoreError, EntryRef};
use crate::fingerprint::{FingerprintRecord, FingerprintTable, stat_fingerprint};
use crate::ingest::{self, IngestConfig, IngestError, IngestFile, IngestNode};
use crate::stats::Stats;

/// Pack-record kind for per-path metadata records ([`PathMeta`] JSON).
/// `Kind(0..=2)` are taken by rio-packstore's shared constants
/// (DIRECTORY / FILE_CHUNK_META / FETCHED).
const KIND_PATH_META: Kind = Kind(3);

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

    #[error("pack store error: {0}")]
    Pack(#[from] rio_packstore::Error),

    #[error("directory store error: {0}")]
    Dir(#[from] DirStoreError),

    #[error("directory blob error: {0}")]
    DirBlob(#[from] DirBlobError),

    #[error("source ingest failed: {0}")]
    Ingest(#[from] IngestError),

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

    /// TODO: ADR-024's at-most-twice escape hatch (re-ingest →
    /// re-negotiate the delta, snapshot into the CAS after two failed
    /// re-ingests) lands with the P3 upload path; P1 only detects and
    /// names the mutation.
    #[error(
        "origin file '{origin}' of store object '{basename}' changed since ingest (content \
         no longer matches the recorded digest) — the tree must be re-ingested"
    )]
    OriginChanged { basename: String, origin: String },

    #[error("origin file '{origin}' of store object '{basename}' is unreadable: {source}")]
    OriginUnreadable {
        basename: String,
        origin: String,
        #[source]
        source: io::Error,
    },

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
}

/// `lstat` result for one tree member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStat {
    Regular { size: u64, executable: bool },
    Symlink { target: Vec<u8> },
    Directory,
}

/// `readDirectory` entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Symlink,
    Directory,
}

/// Where a store path's file bytes live (ADR-024 origin tracking).
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Origin {
    /// Bytes arrived as a stream (NAR dump, toFile text, …) — no origin
    /// tree exists on disk, so contents are stored as `Kind::FETCHED`
    /// records keyed by content blake3.
    Streamed,
    /// Bytes were ingested from a local tree; the origin IS the byte
    /// store (not-a-mirror rule). Reads re-read and digest-verify.
    Local { fs_path: String },
}

/// The root node of a stored path. Directories point at a castore
/// Directory blob; single-file and symlink roots are inlined (castore
/// Directory blobs describe directories only).
#[derive(Debug, Clone, Serialize, Deserialize)]
enum MetaNode {
    Dir {
        digest: [u8; 32],
    },
    File {
        digest: [u8; 32],
        size: u64,
        executable: bool,
    },
    Symlink {
        target: Vec<u8>,
    },
}

/// Per-path metadata record (`KIND_PATH_META`), stored in the pack
/// store and pinned in the path's root digest list — the root table
/// resolves a basename to this record (located by kind, see
/// [`EvalStore::path_meta`]), and this record resolves everything else.
///
/// Encoded as JSON. The encoding only has to be self-consistent, never
/// byte-stable: the record is content-addressed and local-only (never
/// negotiated, never digest-compared across machines), so a serde
/// field-order change merely re-keys it — one dedup miss on the next
/// ingest, never a correctness problem.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PathMeta {
    /// Store-path basename (`<hash>-<name>`); cross-checked against the
    /// root-table key on load.
    path: String,
    #[serde(flatten)]
    info: PathInfo,
    root: MetaNode,
    origin: Origin,
}

/// A captured derivation: memory-only (ADR-024 "Derivations are
/// memory-only client-side"), gone at process exit.
struct DrvEntry {
    aterm: Vec<u8>,
    info: PathInfo,
}

/// Everything mutable, behind one mutex: nix may call store ops from
/// more than one thread, and the pack store / decoded-dir cache are
/// deliberately single-threaded types.
struct Inner {
    dirs: DirStore,
    fingerprints: FingerprintTable,
    drvs: HashMap<String, DrvEntry>,
    /// Decoded path-metadata cache (same role as the decoded-dir cache:
    /// a per-op JSON parse on the hot lstat path would re-create the
    /// 92× pathology one level up).
    metas: HashMap<String, std::sync::Arc<PathMeta>>,
    /// Root LRU touches accumulated this process, flushed as batched
    /// root records on [`EvalStore::flush`] — not one pack append per
    /// read (ADR-024: "the LRU clock moves from per-file utimensat into
    /// batched index records"). Deliberately close-only, no periodic or
    /// size-bound flush: a crash loses at most one process's touches,
    /// which can only make roots look OLDER than they are — worst case
    /// an early LRU eviction and a re-ingest, never wrong content. The
    /// 6h GC grace window already covers the crashed-eval case.
    touched: HashSet<String>,
}

pub struct EvalStore {
    inner: Mutex<Inner>,
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
        let pack = PackStore::open(&root, Options::default())?;
        let fingerprints = FingerprintTable::open(&root);
        Ok(EvalStore {
            inner: Mutex::new(Inner {
                dirs: DirStore::new(pack),
                fingerprints,
                drvs: HashMap::new(),
                metas: HashMap::new(),
                touched: HashSet::new(),
            }),
            stats: Stats::default(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic mid-op is caught at the FFI boundary; the store must
        // stay usable afterwards, so poisoning is ignored.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Per-op counters (tests + the ADR-024 measurement plan).
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Persist this process's pack records and batched LRU touches.
    /// Called from `Drop`; exposed for tests and deliberate sync points.
    pub fn flush(&self) -> Result<()> {
        let mut inner = self.lock();
        let touched: Vec<String> = inner.touched.drain().collect();
        for basename in touched {
            // Unknown roots (e.g. drv paths, which have none) are a no-op.
            inner.dirs.pack_mut().touch_root(&basename)?;
        }
        inner.dirs.flush()?;
        Ok(())
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

        let info = PathInfo {
            nar_hash: hashes.nar_sha256.clone(),
            nar_size,
            references: references.to_vec(),
            ca: Some(render_ca(ca_method, &content_sha256)),
        };
        let mut inner = self.lock();
        self.commit_streamed(&mut inner, rust_path.basename(), &node, info)?;
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
        };
        let mut inner = self.lock();
        self.commit_streamed(&mut inner, path.basename(), &node, stored)?;
        self.stats.record("add_nar", count);
        Ok(())
    }

    /// Ingest a local source tree (`addToStore(SourcePath)`-shaped): one
    /// walk produces the NAR sha256, per-file blake3 + FastCDC chunk
    /// lists, and the per-directory blobs together
    /// ([`crate::ingest::ingest_tree`]). File CONTENT is NOT copied into
    /// the CAS — the origin tree is the byte store (not-a-mirror rule);
    /// chunk lists are persisted as `FILE_CHUNK_META` records for the P3
    /// upload negotiation.
    ///
    /// Only recursive (NixArchive) content addressing is supported: that
    /// is the only method nix uses for tree sources, and the flat methods
    /// would need a second content hash the single-read pipeline does not
    /// produce.
    pub fn add_source_tree(
        &self,
        fs_path: &str,
        name: &str,
        references: &[String],
        nix_path_for: &mut dyn FnMut(&AddHashes) -> Result<String>,
    ) -> Result<AddResult> {
        let refs = parse_refs(references)?;
        let result = ingest::ingest_tree(std::path::Path::new(fs_path), &IngestConfig::default())?;
        let nar_sha256 = result.nar_sha256;
        let nar_size = result.nar_size;

        let rust_path = StorePath::make_fixed_output(
            name,
            &NixHash::new(HashAlgo::SHA256, nar_sha256.to_vec())?,
            true,
            &refs,
        )?;
        let hashes = AddHashes {
            nar_sha256: hex::encode(nar_sha256),
            nar_size,
            content_sha256: hex::encode(nar_sha256),
        };
        let nix_path = nix_path_for(&hashes)?;
        if nix_path != rust_path.as_str() {
            return Err(EvalStoreError::PathMismatch {
                op: "addToStore",
                name: name.to_string(),
                rust_path: rust_path.to_string(),
                nix_path,
            });
        }

        // Tree → BuiltDir + per-file chunk-meta payloads (unique by file
        // digest; identical files share one record).
        let mut chunk_metas: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        let root = match &result.root {
            IngestNode::Dir(dir) => {
                let built = built_from_ingest(dir, &mut chunk_metas);
                MetaRootBuild::Dir(built)
            }
            IngestNode::File(f) => {
                chunk_metas.insert(f.digest, chunk_meta_payload(f));
                MetaRootBuild::Node(MetaNode::File {
                    digest: f.digest,
                    size: f.size,
                    executable: f.executable,
                })
            }
            IngestNode::Symlink(s) => MetaRootBuild::Node(MetaNode::Symlink {
                target: s.target.clone(),
            }),
        };

        let info = PathInfo {
            nar_hash: hashes.nar_sha256.clone(),
            nar_size,
            references: references.to_vec(),
            ca: Some(render_ca(CaMethod::NixArchive, &nar_sha256)),
        };

        let mut inner = self.lock();
        let mut extra_pins = Vec::new();
        for payload in chunk_metas.values() {
            let digest = Digest::of(payload);
            if !inner.dirs.pack().contains(&digest) {
                inner.dirs.pack_mut().put(Kind::FILE_CHUNK_META, payload)?;
                self.stats.record("chunkmeta_write", payload.len() as u64);
            }
            extra_pins.push(digest);
        }
        self.commit(
            &mut inner,
            rust_path.basename(),
            root,
            info,
            Origin::Local {
                fs_path: fs_path.to_string(),
            },
            extra_pins,
        )?;
        self.stats.record("add_source_tree", nar_size);

        Ok(AddResult {
            path: rust_path.to_string(),
            nar_sha256: hashes.nar_sha256,
            nar_size,
        })
    }

    /// Capture a derivation (`writeDerivation`) into the in-process
    /// memory-only map (ADR-024: drvs are a few KB, recomputed
    /// deterministically by every eval, and all consumers are
    /// in-process — they never reach the pack store).
    ///
    /// Two hard cross-checks: rio-nix's unparse(parse(aterm)) must
    /// reproduce nix's bytes, and rio-nix's text-path computation must
    /// match `nix_drv_path`.
    pub fn write_derivation(&self, name: &str, aterm: &[u8], nix_drv_path: &str) -> Result<String> {
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

        let entry = DrvEntry {
            aterm: aterm.to_vec(),
            info: PathInfo {
                nar_hash: hex::encode(nar_hash),
                nar_size,
                references: ref_strs,
                ca: Some(render_ca(CaMethod::Text, &content_sha256)),
            },
        };
        self.lock()
            .drvs
            .insert(rust_path.basename().to_string(), entry);
        self.stats.record("write_derivation", aterm.len() as u64);
        Ok(rust_path.to_string())
    }

    /// Commit a streamed (NAR / flat dump) tree: file bytes land as
    /// `Kind::FETCHED` whole-file records (no origin exists on disk),
    /// directories as castore blobs.
    fn commit_streamed(
        &self,
        inner: &mut Inner,
        basename: &str,
        node: &NarNode,
        info: PathInfo,
    ) -> Result<()> {
        // Unique file contents by blake3 — identical files store once.
        let mut contents: HashMap<[u8; 32], &[u8]> = HashMap::new();
        let root = match node {
            NarNode::Directory { .. } => MetaRootBuild::Dir(built_from_nar(node, &mut contents)?),
            NarNode::Regular {
                executable,
                contents: bytes,
            } => {
                let digest = *blake3::hash(bytes).as_bytes();
                contents.insert(digest, bytes);
                MetaRootBuild::Node(MetaNode::File {
                    digest,
                    size: bytes.len() as u64,
                    executable: *executable,
                })
            }
            NarNode::Symlink { target } => MetaRootBuild::Node(MetaNode::Symlink {
                target: target.clone().into_bytes(),
            }),
        };

        let mut extra_pins = Vec::new();
        for (digest, bytes) in &contents {
            let digest = Digest(*digest);
            if inner.dirs.pack().contains(&digest) {
                self.stats.record("fetched_dedup", bytes.len() as u64);
            } else {
                inner.dirs.pack_mut().put(Kind::FETCHED, bytes)?;
                self.stats.record("fetched_write", bytes.len() as u64);
            }
            extra_pins.push(digest);
        }
        self.commit(inner, basename, root, info, Origin::Streamed, extra_pins)
    }

    /// Shared commit tail: persist directory blobs, the path-meta
    /// record, and the root entry pinning ALL of it (the pack store's
    /// GC mark is a flat union of root digest lists — anything not
    /// pinned here is repacked away).
    fn commit(
        &self,
        inner: &mut Inner,
        basename: &str,
        root: MetaRootBuild,
        info: PathInfo,
        origin: Origin,
        extra_pins: Vec<Digest>,
    ) -> Result<()> {
        let mut pins = Vec::new();
        let root = match root {
            MetaRootBuild::Dir(built) => {
                let folded = built.fold()?;
                for (digest, bytes) in &folded.blobs {
                    if inner.dirs.pack().contains(digest) {
                        self.stats.record("dirblob_dedup", bytes.len() as u64);
                    } else {
                        inner.dirs.pack_mut().put(Kind::DIRECTORY, bytes)?;
                        self.stats.record("dirblob_write", bytes.len() as u64);
                    }
                }
                pins.extend(folded.digests());
                MetaNode::Dir {
                    digest: folded.root_digest.0,
                }
            }
            MetaRootBuild::Node(node) => node,
        };
        pins.extend(extra_pins);

        let meta = PathMeta {
            path: basename.to_string(),
            info,
            root,
            origin,
        };
        let meta_bytes = serde_json::to_vec(&meta)
            .map_err(|e| EvalStoreError::Corrupt(format!("path meta encode failed: {e}")))?;
        let meta_digest = Digest::of(&meta_bytes);
        if !inner.dirs.pack().contains(&meta_digest) {
            inner.dirs.pack_mut().put(KIND_PATH_META, &meta_bytes)?;
            self.stats.record("meta_write", meta_bytes.len() as u64);
        }
        // Write-side convention only: meta first keeps the record easy
        // to eyeball in pack dumps. Readback finds it by kind
        // (`path_meta`), so root-list order is not load-bearing.
        pins.insert(0, meta_digest);
        inner.dirs.pack_mut().add_root(basename, &pins)?;
        inner
            .metas
            .insert(basename.to_string(), std::sync::Arc::new(meta));
        Ok(())
    }

    // -- queries ------------------------------------------------------------

    pub fn is_valid_path(&self, basename: &str) -> bool {
        self.stats.record("is_valid_path", 0);
        if !is_store_basename(basename) {
            return false;
        }
        let mut inner = self.lock();
        let valid = inner.drvs.contains_key(basename) || inner.dirs.pack().has_root(basename);
        if valid {
            // LRU clock — batched, persisted by flush().
            inner.touched.insert(basename.to_string());
        }
        valid
    }

    pub fn query_path_info(&self, basename: &str) -> Result<Option<PathInfo>> {
        self.stats.record("query_path_info", 0);
        if !is_store_basename(basename) {
            return Ok(None);
        }
        let mut inner = self.lock();
        if let Some(drv) = inner.drvs.get(basename) {
            return Ok(Some(drv.info.clone()));
        }
        let Some(meta) = self.path_meta(&mut inner, basename)? else {
            return Ok(None);
        };
        inner.touched.insert(basename.to_string());
        Ok(Some(meta.info.clone()))
    }

    pub fn query_path_from_hash_part(&self, hash_part: &str) -> Result<Option<String>> {
        self.stats.record("query_path_from_hash_part", 0);
        let inner = self.lock();
        let mut candidates: Vec<String> = inner
            .dirs
            .pack()
            .root_names()
            .into_iter()
            .chain(inner.drvs.keys().cloned())
            .filter(|b| b.starts_with(hash_part))
            .collect();
        candidates.sort_unstable();
        Ok(candidates
            .into_iter()
            .next()
            .map(|b| format!("{}/{b}", store_path::STORE_DIR)))
    }

    // -- read-back ----------------------------------------------------------

    /// Decode (or serve from cache) the path-meta record for `basename`.
    /// `Ok(None)` = no such path in the CAS.
    fn path_meta(
        &self,
        inner: &mut Inner,
        basename: &str,
    ) -> Result<Option<std::sync::Arc<PathMeta>>> {
        if let Some(meta) = inner.metas.get(basename) {
            return Ok(Some(std::sync::Arc::clone(meta)));
        }
        let Some(digests) = inner.dirs.pack().root_digests(basename) else {
            return Ok(None);
        };
        // Found by KIND, not list position: the digest list is a Vec the
        // pack store re-encodes on every GC/touch, so "meta is first" is
        // a write-side locality convention, never a readback contract —
        // a future reorder/dedup of root digest lists must not corrupt
        // readback.
        let Some(meta_digest) = digests
            .iter()
            .find(|d| inner.dirs.pack().kind_of(d) == Some(KIND_PATH_META))
        else {
            return Err(EvalStoreError::Corrupt(format!(
                "root entry for {basename} pins no path-meta record"
            )));
        };
        let Some(bytes) = inner.dirs.pack().get(meta_digest)? else {
            return Err(EvalStoreError::Corrupt(format!(
                "path meta record {meta_digest} for {basename} missing from the pack store"
            )));
        };
        let meta: PathMeta = serde_json::from_slice(&bytes).map_err(|e| {
            EvalStoreError::Corrupt(format!("path meta record for {basename} undecodable: {e}"))
        })?;
        if meta.path != basename {
            return Err(EvalStoreError::Corrupt(format!(
                "path meta record for {basename} names {}",
                meta.path
            )));
        }
        let meta = std::sync::Arc::new(meta);
        inner
            .metas
            .insert(basename.to_string(), std::sync::Arc::clone(&meta));
        Ok(Some(meta))
    }

    /// Fetch a decoded directory, recording hit/decode stats — the
    /// counters the warm-path regression test asserts on.
    fn dir_get(&self, inner: &Inner, digest: &Digest) -> Result<std::sync::Arc<DecodedDir>> {
        let (dir, decoded) = inner.dirs.get_tracked(digest)?;
        self.stats.record(
            if decoded {
                "dir_decode"
            } else {
                "dir_cache_hit"
            },
            0,
        );
        Ok(dir)
    }

    /// Resolve `rel` (slash-separated, possibly empty = root) within the
    /// store object `basename`. `Ok(None)` = object known, member missing.
    /// `Err(ForeignPath)` = object not in the CAS at all.
    fn resolve(
        &self,
        inner: &mut Inner,
        basename: &str,
        rel: &str,
    ) -> Result<Option<(std::sync::Arc<PathMeta>, Resolved)>> {
        if !is_store_basename(basename) {
            return Err(foreign(basename));
        }
        let Some(meta) = self.path_meta(inner, basename)? else {
            return Err(foreign(basename));
        };
        inner.touched.insert(basename.to_string());
        let mut node = match &meta.root {
            MetaNode::Dir { digest } => Resolved::Dir {
                digest: Digest(*digest),
            },
            MetaNode::File {
                digest,
                size,
                executable,
            } => Resolved::File {
                digest: Digest(*digest),
                size: *size,
                executable: *executable,
            },
            MetaNode::Symlink { target } => Resolved::Symlink {
                target: target.clone(),
            },
        };
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            let Resolved::Dir { digest } = node else {
                return Ok(None);
            };
            let dir = self.dir_get(inner, &digest)?;
            match dir.child(comp.as_bytes()) {
                Some(EntryRef::Dir { digest, .. }) => node = Resolved::Dir { digest },
                Some(EntryRef::File {
                    digest,
                    size,
                    executable,
                }) => {
                    node = Resolved::File {
                        digest,
                        size,
                        executable,
                    }
                }
                Some(EntryRef::Symlink { target }) => {
                    node = Resolved::Symlink {
                        target: target.to_vec(),
                    }
                }
                None => return Ok(None),
            }
        }
        Ok(Some((meta, node)))
    }

    /// `Ok(None)` when the object or member doesn't exist — whole-store
    /// accessor semantics where unknown paths are merely absent.
    pub fn lstat(&self, basename: &str, rel: &str) -> Result<Option<PathStat>> {
        self.stats.record("lstat", 0);
        let mut inner = self.lock();
        if let Some(drv) = inner.drvs.get(basename) {
            return Ok(if rel.is_empty() {
                Some(PathStat::Regular {
                    size: drv.aterm.len() as u64,
                    executable: false,
                })
            } else {
                None
            });
        }
        match self.resolve(&mut inner, basename, rel) {
            Err(EvalStoreError::ForeignPath(_)) => Ok(None),
            Err(e) => Err(e),
            Ok(None) => Ok(None),
            Ok(Some((_, node))) => Ok(Some(match node {
                Resolved::File {
                    size, executable, ..
                } => PathStat::Regular { size, executable },
                Resolved::Symlink { target } => PathStat::Symlink { target },
                Resolved::Dir { .. } => PathStat::Directory,
            })),
        }
    }

    /// Entries of a directory member, sorted byte-lex by name.
    pub fn read_directory(&self, basename: &str, rel: &str) -> Result<Vec<(Vec<u8>, EntryKind)>> {
        self.stats.record("read_directory", 0);
        let mut inner = self.lock();
        if inner.drvs.contains_key(basename) {
            return Err(EvalStoreError::Corrupt(format!(
                "readDirectory on non-directory {basename}/{rel}"
            )));
        }
        match self.resolve(&mut inner, basename, rel)? {
            Some((_, Resolved::Dir { digest })) => {
                let dir = self.dir_get(&inner, &digest)?;
                Ok(dir
                    .entries()
                    .map(|(name, entry)| {
                        let kind = match entry {
                            EntryRef::File { .. } => EntryKind::Regular,
                            EntryRef::Symlink { .. } => EntryKind::Symlink,
                            EntryRef::Dir { .. } => EntryKind::Directory,
                        };
                        (name.to_vec(), kind)
                    })
                    .collect())
            }
            Some(_) => Err(EvalStoreError::Corrupt(format!(
                "readDirectory on non-directory {basename}/{rel}"
            ))),
            None => Err(no_such_entry(basename, rel)),
        }
    }

    pub fn read_file(&self, basename: &str, rel: &str, sink: &mut dyn Write) -> Result<u64> {
        let mut inner = self.lock();
        if let Some(drv) = inner.drvs.get(basename) {
            if !rel.is_empty() {
                return Err(no_such_entry(basename, rel));
            }
            sink.write_all(&drv.aterm)?;
            let size = drv.aterm.len() as u64;
            self.stats.record("read_file", size);
            return Ok(size);
        }
        match self.resolve(&mut inner, basename, rel)? {
            Some((meta, Resolved::File { digest, size, .. })) => {
                let data = self.file_bytes(&inner, &meta, rel.as_bytes(), &digest, size)?;
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

    pub fn read_link(&self, basename: &str, rel: &str) -> Result<Vec<u8>> {
        self.stats.record("read_link", 0);
        let mut inner = self.lock();
        if inner.drvs.contains_key(basename) {
            return Err(EvalStoreError::Corrupt(format!(
                "readLink on non-symlink {basename}/{rel}"
            )));
        }
        match self.resolve(&mut inner, basename, rel)? {
            Some((_, Resolved::Symlink { target })) => Ok(target),
            Some(_) => Err(EvalStoreError::Corrupt(format!(
                "readLink on non-symlink {basename}/{rel}"
            ))),
            None => Err(no_such_entry(basename, rel)),
        }
    }

    /// Fetch one file's bytes per the path's origin (ADR-024):
    /// streamed content from its `FETCHED` record; local content by
    /// re-reading the origin tree and digest-verifying (the
    /// not-a-mirror rule's read side).
    fn file_bytes(
        &self,
        inner: &Inner,
        meta: &PathMeta,
        rel: &[u8],
        digest: &Digest,
        size: u64,
    ) -> Result<Bytes> {
        let data = match &meta.origin {
            Origin::Streamed => inner.dirs.pack().get(digest)?.ok_or_else(|| {
                EvalStoreError::Corrupt(format!(
                    "content record {digest} for {}/{} missing from the pack store",
                    meta.path,
                    rel.escape_ascii()
                ))
            })?,
            Origin::Local { fs_path } => {
                use std::os::unix::ffi::OsStrExt;
                let mut path = PathBuf::from(fs_path);
                if !rel.is_empty() {
                    // Every component matched a validated tree entry name
                    // during resolve, so the join cannot traverse.
                    path.push(std::ffi::OsStr::from_bytes(rel));
                }
                let data =
                    std::fs::read(&path).map_err(|source| EvalStoreError::OriginUnreadable {
                        basename: meta.path.clone(),
                        origin: path.display().to_string(),
                        source,
                    })?;
                if Digest::of(&data) != *digest {
                    return Err(EvalStoreError::OriginChanged {
                        basename: meta.path.clone(),
                        origin: path.display().to_string(),
                    });
                }
                Bytes::from(data)
            }
        };
        if data.len() as u64 != size {
            return Err(EvalStoreError::Corrupt(format!(
                "content for {}/{} is {} bytes, tree records {size}",
                meta.path,
                rel.escape_ascii(),
                data.len()
            )));
        }
        Ok(data)
    }

    /// Regenerate the NAR framing from the decoded-dir walk (castore
    /// pattern: framing is never persisted) and stream it into `sink`.
    pub fn nar_from_path(&self, basename: &str, sink: &mut dyn Write) -> Result<u64> {
        if !is_store_basename(basename) {
            return Err(foreign(basename));
        }
        let mut inner = self.lock();
        let mut counter = CountingWriter::new(sink);
        if let Some(drv) = inner.drvs.get(basename) {
            frame::magic(&mut counter)?;
            frame::node_open(&mut counter)?;
            frame::regular_header(&mut counter, false, drv.aterm.len() as u64)?;
            counter.write_all(&drv.aterm)?;
            frame::contents_padding(&mut counter, drv.aterm.len() as u64)?;
            frame::node_close(&mut counter)?;
            let written = counter.count;
            self.stats.record("nar_from_path", written);
            return Ok(written);
        }
        let Some(meta) = self.path_meta(&mut inner, basename)? else {
            return Err(foreign(basename));
        };
        inner.touched.insert(basename.to_string());
        let root = match &meta.root {
            MetaNode::Dir { digest } => Resolved::Dir {
                digest: Digest(*digest),
            },
            MetaNode::File {
                digest,
                size,
                executable,
            } => Resolved::File {
                digest: Digest(*digest),
                size: *size,
                executable: *executable,
            },
            MetaNode::Symlink { target } => Resolved::Symlink {
                target: target.clone(),
            },
        };
        frame::magic(&mut counter)?;
        let mut rel = Vec::new();
        self.emit_nar_node(&inner, &meta, &root, &mut rel, 0, &mut counter)?;
        let written = counter.count;
        self.stats.record("nar_from_path", written);
        Ok(written)
    }

    /// Emit one node (and, for directories, its subtree) in canonical
    /// NAR token order via [`rio_nix::nar::frame`]. `rel` is the
    /// slash-joined path below the store root (origin re-reads need it).
    fn emit_nar_node<W: Write>(
        &self,
        inner: &Inner,
        meta: &PathMeta,
        node: &Resolved,
        rel: &mut Vec<u8>,
        depth: usize,
        w: &mut W,
    ) -> Result<()> {
        frame::node_open(w)?;
        match node {
            Resolved::File {
                digest,
                size,
                executable,
            } => {
                frame::regular_header(w, *executable, *size)?;
                let data = self.file_bytes(inner, meta, rel, digest, *size)?;
                w.write_all(&data)?;
                frame::contents_padding(w, *size)?;
            }
            Resolved::Symlink { target } => frame::symlink(w, target)?,
            Resolved::Dir { digest } => {
                // Hash-chained digests make reference cycles impossible;
                // the depth check is defense against a hand-corrupted
                // store and keeps the recursion bounded like the readers.
                if depth > nar::MAX_NAR_DEPTH {
                    return Err(EvalStoreError::Corrupt(format!(
                        "directory nesting in {} exceeds {}",
                        meta.path,
                        nar::MAX_NAR_DEPTH
                    )));
                }
                frame::directory_open(w)?;
                let dir = self.dir_get(inner, digest)?;
                for (name, entry) in dir.entries() {
                    let child = match entry {
                        EntryRef::File {
                            digest,
                            size,
                            executable,
                        } => Resolved::File {
                            digest,
                            size,
                            executable,
                        },
                        EntryRef::Symlink { target } => Resolved::Symlink {
                            target: target.to_vec(),
                        },
                        EntryRef::Dir { digest, .. } => Resolved::Dir { digest },
                    };
                    frame::entry_open(w, name)?;
                    let saved = rel.len();
                    if !rel.is_empty() {
                        rel.push(b'/');
                    }
                    rel.extend_from_slice(name);
                    self.emit_nar_node(inner, meta, &child, rel, depth + 1, w)?;
                    rel.truncate(saved);
                    frame::entry_close(w)?;
                }
            }
        }
        frame::node_close(w)?;
        Ok(())
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
    /// path is still in the CAS, return it without re-reading content.
    pub fn fingerprint_lookup(&self, fs_path: &str, method_key: &str) -> Result<Option<String>> {
        let mut inner = self.lock();
        let Some(rec) = inner.fingerprints.get(fs_path, method_key).cloned() else {
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
        if trusted && now == rec.fingerprint && inner.dirs.pack().has_root(basename) {
            self.stats.record("fingerprint_hit", 0);
            // A hit keeps the entry live — bump its LRU clock like any
            // other read.
            inner.touched.insert(basename.to_string());
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
        self.lock().fingerprints.put(FingerprintRecord {
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
        if let Err(e) = self.flush() {
            eprintln!("rio-evalstore: flush on close failed: {e}");
        }
        if Stats::enabled() {
            eprintln!("{}", self.stats.render());
        }
    }
}

/// A resolved tree member (post `resolve` walk).
#[derive(Debug, Clone)]
enum Resolved {
    File {
        digest: Digest,
        size: u64,
        executable: bool,
    },
    Symlink {
        target: Vec<u8>,
    },
    Dir {
        digest: Digest,
    },
}

/// Root shape handed to `commit` before the dir tree is folded.
enum MetaRootBuild {
    Dir(BuiltDir),
    Node(MetaNode),
}

/// Convert a parsed NAR directory into a [`BuiltDir`], collecting every
/// regular file's contents (keyed by blake3) for `FETCHED` storage.
fn built_from_nar<'a>(
    node: &'a NarNode,
    contents: &mut HashMap<[u8; 32], &'a [u8]>,
) -> Result<BuiltDir> {
    let NarNode::Directory { entries } = node else {
        return Err(EvalStoreError::Corrupt(
            "built_from_nar called on a non-directory".into(),
        ));
    };
    let mut dir = BuiltDir::new();
    for entry in entries {
        let built = match &entry.node {
            NarNode::Regular {
                executable,
                contents: bytes,
            } => {
                let digest = *blake3::hash(bytes).as_bytes();
                contents.insert(digest, bytes);
                BuiltEntry::File {
                    digest: Digest(digest),
                    size: bytes.len() as u64,
                    executable: *executable,
                }
            }
            NarNode::Symlink { target } => BuiltEntry::Symlink {
                target: target.clone().into_bytes(),
            },
            NarNode::Directory { .. } => BuiltEntry::Dir(built_from_nar(&entry.node, contents)?),
        };
        dir.push(entry.name.as_bytes(), built);
    }
    Ok(dir)
}

/// Convert an ingested directory into a [`BuiltDir`], collecting one
/// chunk-meta payload per distinct file digest.
fn built_from_ingest(
    dir: &crate::ingest::IngestDir,
    chunk_metas: &mut HashMap<[u8; 32], Vec<u8>>,
) -> BuiltDir {
    let mut out = BuiltDir::new();
    for entry in &dir.entries {
        let built = match &entry.node {
            IngestNode::File(f) => {
                chunk_metas
                    .entry(f.digest)
                    .or_insert_with(|| chunk_meta_payload(f));
                BuiltEntry::File {
                    digest: Digest(f.digest),
                    size: f.size,
                    executable: f.executable,
                }
            }
            IngestNode::Symlink(s) => BuiltEntry::Symlink {
                target: s.target.clone(),
            },
            IngestNode::Dir(d) => BuiltEntry::Dir(built_from_ingest(d, chunk_metas)),
        };
        out.push(entry.name.clone(), built);
    }
    out
}

/// `FILE_CHUNK_META` payload: `file_blake3(32)` then one
/// `chunk_blake3(32) ‖ offset(8 LE) ‖ len(4 LE)` run per chunk, in file
/// order. Self-describing (the prefix names the file) so the P3 upload
/// negotiation can associate records with tree file digests.
fn chunk_meta_payload(file: &IngestFile) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + file.chunks.len() * 44);
    out.extend_from_slice(&file.digest);
    for chunk in &file.chunks {
        out.extend_from_slice(&chunk.digest);
        out.extend_from_slice(&chunk.offset.to_le_bytes());
        out.extend_from_slice(&chunk.len.to_le_bytes());
    }
    out
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

/// FFI-boundary check: a string that doesn't parse as a store-path
/// basename can never be in the CAS, so callers treat it as absent.
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
