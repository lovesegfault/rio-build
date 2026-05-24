//! Replay archive reader — the recorded build load consumed by `xtask k8s
//! replay`.
//!
//! An archive is either a DwarFS image (read in-process via the `dwarfs`
//! crate, no external tools) or a plain directory with the same layout:
//!
//! ```text
//! manifest.json        run metadata (window, substituter sources, counts)
//! requests.jsonl       one line per recorded client build request
//! builds.jsonl         one line per recorded build outcome (optional)
//! impure-env.json      drv path -> impureEnvVars names (optional)
//! narinfo/<hash>.narinfo   metadata sidecar for each embedded store path
//! nix/store/<hash>-<name>.drv      derivation ATerm text
//! nix/store/<hash>-<name>/...      embedded store paths, unpacked trees
//! ```
//!
//! See `docs/dev/2026-05-24-xtask-k8s-replay-design.md` ("Archive format")
//! for the v0 compatibility contract. Metadata (manifest, requests, builds,
//! impure-env, narinfo sidecars, the `nix/store/` entry index) is parsed
//! eagerly at [`ReplayArchive::open`]; derivation text and NAR payloads are
//! read on demand so large archives don't get slurped into memory up front.

use std::collections::{BTreeMap, HashMap};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail, ensure};
use dwarfs::AsChunks as _;
use rio_nix::nar::{NarEntry, NarNode};
use rio_nix::narinfo::NarInfo;

/// Run metadata from `manifest.json`.
///
/// Recorder-specific fields are intentionally not declared; serde ignores
/// unknown fields by default.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Manifest {
    /// Start of the recorded window.
    pub from: jiff::Timestamp,
    /// End of the recorded window.
    pub to: jiff::Timestamp,
    /// Substituters the recording host could reach (live relay sources).
    #[serde(default)]
    pub src_substituters: Vec<String>,
    /// Substituters the replay target is expected to reach on its own.
    #[serde(default)]
    pub target_substituters: Vec<String>,
    /// Whether the archive embeds everything it needs ("fat") instead of
    /// relying on substituters at replay time.
    #[serde(default)]
    pub fat: bool,
    /// When the archive was created.
    pub created_at: jiff::Timestamp,
    /// Number of recorded requests.
    #[serde(default)]
    pub requests: u64,
    /// Number of embedded derivations.
    #[serde(default)]
    pub drvs: u64,
    /// Number of embedded source store paths.
    #[serde(default)]
    pub embedded_srcs: u64,
}

/// One recorded client build request (a line of `requests.jsonl`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ReplayRequest {
    /// Client session id from the recording.
    pub ssh_session_id: i64,
    /// Seconds from the window start ([`Manifest::from`]). Negative recorded
    /// offsets are clamped to 0 at load time.
    pub offset_s: f64,
    /// `[drv_path, [output, ...]]` pairs; `["*"]` and `[]` both mean "all
    /// outputs".
    pub paths: Vec<(String, Vec<String>)>,
}

/// One recorded build outcome (a line of `builds.jsonl`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BuildRecord {
    /// Client session id the build belonged to.
    pub ssh_session_id: i64,
    /// Derivation that was built.
    pub drv_path: String,
    /// Status code — see [`prod_status`] for the known values; other
    /// non-zero codes are deterministic build failures.
    pub status: i32,
    /// Human-readable status message, when the recorder captured one.
    #[serde(default)]
    pub status_msg: Option<String>,
    /// Wall-clock build duration in seconds.
    #[serde(default)]
    pub duration_s: Option<f64>,
    /// Seconds from the window start at which the build stopped (used to
    /// time disconnect replay).
    #[serde(default)]
    pub stop_offset_s: Option<f64>,
    /// Recorded outputs by output name.
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputRecord>,
}

/// Recorded NAR identity of one build output.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OutputRecord {
    /// Lowercase hex SHA-256 of the uncompressed NAR.
    pub nar_hash_hex: String,
    /// Uncompressed NAR size in bytes.
    pub nar_size: u64,
}

/// Recorded build status codes (integer values in `builds.jsonl`).
///
/// Codes not listed here are treated as deterministic build failures.
pub mod prod_status {
    /// Build succeeded.
    pub const BUILT: i32 = 0;
    /// Build was cancelled before it finished.
    pub const CANCELLED: i32 = 6;
    /// The builder failed (infrastructure error, not the build itself).
    pub const BUILDER_ERROR: i32 = 10;
    /// The recording client disconnected before the build finished.
    pub const CLIENT_DISCONNECT: i32 = 13;
    /// The build was aborted for resource exhaustion.
    pub const RESOURCE_EXHAUSTED: i32 = 16;
}

/// An opened replay archive: backend handle plus eagerly parsed metadata.
#[derive(Debug)]
pub struct ReplayArchive {
    backend: Backend,
    manifest: Manifest,
    requests: Vec<ReplayRequest>,
    builds: HashMap<(i64, String), BuildRecord>,
    has_builds: bool,
    impure_env: BTreeMap<String, Vec<String>>,
    /// Store-path hash part → parsed narinfo sidecar.
    narinfos: HashMap<String, NarInfo>,
    /// Store-path hash part → `nix/store/` entry (basename, kind, exec bit).
    store_entries: HashMap<String, WalkEntry>,
}

impl ReplayArchive {
    /// Open a `.dwarfs` image or an archive directory.
    pub fn open(path: &Path) -> Result<Self> {
        let backend = Backend::open(path)?;

        let manifest_bytes = backend.read_file("manifest.json")?.ok_or_else(|| {
            anyhow!(
                "{}: no manifest.json — not a replay archive?",
                path.display()
            )
        })?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("{}: malformed manifest.json", path.display()))?;

        let requests_bytes = backend
            .read_file("requests.jsonl")?
            .ok_or_else(|| anyhow!("{}: no requests.jsonl", path.display()))?;
        let mut requests = parse_requests(&requests_bytes)?;
        // Recorded lines are not guaranteed globally ordered (per-session
        // buffers get flushed independently); the replay timeline wants
        // ascending offsets.
        requests.sort_by(|a, b| a.offset_s.total_cmp(&b.offset_s));

        let (builds, has_builds) = match backend.read_file("builds.jsonl")? {
            Some(bytes) => (parse_builds(&bytes)?, true),
            None => (HashMap::new(), false),
        };

        let impure_env = match backend.read_file("impure-env.json")? {
            Some(bytes) => serde_json::from_slice(&bytes).context("malformed impure-env.json")?,
            None => BTreeMap::new(),
        };

        // narinfo sidecars: a sidecar that fails to parse is skipped with a
        // warning — one bad sidecar shouldn't take down the whole replay; the
        // affected path simply won't be uploadable from the archive.
        let mut narinfos = HashMap::new();
        for entry in backend.list_dir("narinfo")?.unwrap_or_default() {
            if entry.kind != EntryKind::Regular {
                continue;
            }
            let Some(stem) = entry.name.strip_suffix(".narinfo") else {
                continue;
            };
            let rel = format!("narinfo/{}", entry.name);
            let bytes = backend
                .read_file(&rel)?
                .ok_or_else(|| anyhow!("{rel}: listed but unreadable"))?;
            let parsed = std::str::from_utf8(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|text| NarInfo::parse(text).map_err(anyhow::Error::from));
            match parsed {
                Ok(narinfo) => {
                    narinfos.insert(stem.to_string(), narinfo);
                }
                Err(err) => tracing::warn!("skipping unparseable {rel}: {err:#}"),
            }
        }

        // nix/store/ entry index: hash part → entry. Drives the store-path
        // keyed lookups; contents stay in the backend until asked for.
        let mut store_entries = HashMap::new();
        for entry in backend.list_dir("nix/store")?.unwrap_or_default() {
            store_entries.insert(hash_part(&entry.name).to_string(), entry);
        }

        Ok(Self {
            backend,
            manifest,
            requests,
            builds,
            has_builds,
            impure_env,
            narinfos,
            store_entries,
        })
    }

    /// Run metadata from `manifest.json`.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Recorded requests, sorted by `offset_s` ascending (input order is not
    /// guaranteed); negative offsets clamped to 0.0.
    pub fn requests(&self) -> &[ReplayRequest] {
        &self.requests
    }

    /// Recorded build outcomes keyed by `(ssh_session_id, drv_path)`;
    /// duplicate keys: last line wins. Empty if `builds.jsonl` is absent.
    pub fn builds(&self) -> &HashMap<(i64, String), BuildRecord> {
        &self.builds
    }

    /// Whether the archive carries a `builds.jsonl` at all.
    pub fn has_builds(&self) -> bool {
        self.has_builds
    }

    /// drv path → `impureEnvVars` names. Empty if the file is absent.
    pub fn impure_env(&self) -> &BTreeMap<String, Vec<String>> {
        &self.impure_env
    }

    /// Narinfo sidecar for an embedded path, by store-path hash part (a full
    /// path or basename works too).
    pub fn narinfo(&self, hash_part_or_path: &str) -> Option<&NarInfo> {
        self.narinfos.get(hash_part(hash_part_or_path))
    }

    /// True if the archive embeds this store path's contents (an unpacked
    /// tree under `nix/store/`, not a `.drv`).
    pub fn has_embedded(&self, store_path: &str) -> bool {
        self.store_entries
            .get(hash_part(store_path))
            .is_some_and(|entry| !entry.name.ends_with(".drv"))
    }

    /// Read a `.drv` file's ATerm text from the archive. Accepts a full
    /// store path or a basename.
    pub fn read_drv(&self, drv_path: &str) -> Result<String> {
        let entry = self
            .store_entries
            .get(hash_part(drv_path))
            .ok_or_else(|| anyhow!("derivation {drv_path} is not present in the archive"))?;
        ensure!(
            entry.name.ends_with(".drv"),
            "{drv_path} resolves to {} in the archive, which is not a .drv",
            entry.name
        );
        let rel = format!("nix/store/{}", entry.name);
        let bytes = self
            .backend
            .read_file(&rel)?
            .ok_or_else(|| anyhow!("{rel}: listed in the archive index but unreadable"))?;
        String::from_utf8(bytes).with_context(|| format!("{rel}: derivation text is not UTF-8"))
    }

    /// NAR-serialize an embedded store path (used as upload payload).
    /// Accepts a full store path or a basename.
    pub fn dump_nar(&self, store_path: &str) -> Result<Vec<u8>> {
        let entry = self
            .store_entries
            .get(hash_part(store_path))
            .ok_or_else(|| anyhow!("store path {store_path} is not embedded in the archive"))?;
        let rel = format!("nix/store/{}", entry.name);
        match &self.backend {
            Backend::Dir { root } => {
                let mut nar = Vec::new();
                rio_nix::nar::dump_path_streaming(&root.join(&rel), &mut nar)
                    .with_context(|| format!("NAR-serialize {rel}"))?;
                Ok(nar)
            }
            Backend::Dwarfs(_) => {
                let node = self
                    .backend
                    .nar_node(&rel, entry)
                    .with_context(|| format!("NAR-serialize {rel} from the DwarFS image"))?;
                let mut nar = Vec::new();
                rio_nix::nar::serialize(&mut nar, &node)
                    .with_context(|| format!("NAR-serialize {rel} from the DwarFS image"))?;
                Ok(nar)
            }
        }
    }
}

/// Parse `requests.jsonl`, skipping blank lines and clamping negative
/// offsets to 0 (clock skew at capture time can produce slightly negative
/// values; rejecting the whole archive for that would be overkill).
fn parse_requests(bytes: &[u8]) -> Result<Vec<ReplayRequest>> {
    let text = std::str::from_utf8(bytes).context("requests.jsonl is not valid UTF-8")?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut request: ReplayRequest = serde_json::from_str(line)
            .with_context(|| format!("requests.jsonl line {}: malformed record", idx + 1))?;
        request.offset_s = request.offset_s.max(0.0);
        out.push(request);
    }
    Ok(out)
}

/// Parse `builds.jsonl` into a `(session, drv)`-keyed map; duplicate keys
/// keep the last line (a re-recorded outcome supersedes the earlier one).
fn parse_builds(bytes: &[u8]) -> Result<HashMap<(i64, String), BuildRecord>> {
    let text = std::str::from_utf8(bytes).context("builds.jsonl is not valid UTF-8")?;
    let mut out = HashMap::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: BuildRecord = serde_json::from_str(line)
            .with_context(|| format!("builds.jsonl line {}: malformed record", idx + 1))?;
        out.insert((record.ssh_session_id, record.drv_path.clone()), record);
    }
    Ok(out)
}

/// The hash part of a store path: the basename characters before the first
/// `-`. Accepts a full `/nix/store/...` path, a basename, or a bare hash.
fn hash_part(path_or_name: &str) -> &str {
    let base = path_or_name.rsplit('/').next().unwrap_or(path_or_name);
    base.split('-').next().unwrap_or(base)
}

/// Where the archive bytes live. Everything above this enum goes through
/// [`Backend::read_file`] and [`Backend::list_dir`]; NAR packing of embedded
/// trees on the DwarFS side walks those same primitives recursively
/// ([`Backend::nar_node`]).
#[derive(Debug)]
enum Backend {
    /// A plain unpacked archive directory.
    Dir { root: PathBuf },
    /// A DwarFS image, read in-process. Boxed: the parsed index is much
    /// larger than a `PathBuf`.
    Dwarfs(Box<DwarfsBackend>),
}

/// DwarFS image state: `index` holds the parsed entry tree (cheap, immutable
/// lookups); `archive` owns the reader plus block cache and needs `&mut` for
/// reads, hence the mutex so [`ReplayArchive`] lookups stay `&self`.
#[derive(Debug)]
struct DwarfsBackend {
    index: dwarfs::ArchiveIndex,
    archive: Mutex<dwarfs::Archive<std::fs::File>>,
}

/// Entry kind yielded by [`Backend::list_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Regular,
    Directory,
    Symlink,
    /// Device/socket/fifo — never legitimate inside a store path.
    Other,
}

/// One directory entry from [`Backend::list_dir`].
#[derive(Debug, Clone)]
struct WalkEntry {
    name: String,
    kind: EntryKind,
    /// File size in bytes (0 for directories and symlinks).
    size: u64,
    /// Executable bit (regular files only).
    executable: bool,
    /// Symlink target, when `kind` is [`EntryKind::Symlink`].
    symlink_target: Option<String>,
}

impl Backend {
    /// Open `path` as a directory archive or, if it is a file, as a DwarFS
    /// image.
    fn open(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat replay archive {}", path.display()))?;
        if meta.is_dir() {
            return Ok(Backend::Dir {
                root: path.to_path_buf(),
            });
        }
        let file = std::fs::File::open(path)
            .with_context(|| format!("open replay archive {}", path.display()))?;
        let (index, archive) = dwarfs::Archive::new(file)
            .with_context(|| format!("{}: not a readable DwarFS image", path.display()))?;
        Ok(Backend::Dwarfs(Box::new(DwarfsBackend {
            index,
            archive: Mutex::new(archive),
        })))
    }

    /// Read a file's full contents. `Ok(None)` if the path doesn't exist.
    fn read_file(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Backend::Dir { root } => {
                let path = root.join(rel);
                match std::fs::read(&path) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
                }
            }
            Backend::Dwarfs(dw) => {
                let Some(inode) = dw.index.get_path(rel.split('/')) else {
                    return Ok(None);
                };
                let file = inode
                    .as_file()
                    .ok_or_else(|| anyhow!("{rel}: not a regular file in the DwarFS image"))?;
                let mut archive = dw.archive.lock().expect("dwarfs reader lock poisoned");
                let bytes = file
                    .read_to_vec(&mut *archive)
                    .with_context(|| format!("read {rel} from the DwarFS image"))?;
                Ok(Some(bytes))
            }
        }
    }

    /// List a directory. `Ok(None)` if the path doesn't exist.
    fn list_dir(&self, rel: &str) -> Result<Option<Vec<WalkEntry>>> {
        match self {
            Backend::Dir { root } => {
                let path = root.join(rel);
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => bail!("{}: expected a directory", path.display()),
                    Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
                    Err(err) => {
                        return Err(err).with_context(|| format!("stat {}", path.display()));
                    }
                }
                let mut out = Vec::new();
                for dirent in
                    std::fs::read_dir(&path).with_context(|| format!("list {}", path.display()))?
                {
                    let dirent = dirent.with_context(|| format!("list {}", path.display()))?;
                    let name = dirent.file_name().into_string().map_err(|raw| {
                        anyhow!("{}: non-UTF-8 entry name {raw:?}", path.display())
                    })?;
                    out.push(walk_entry_from_fs(&path.join(&name), name)?);
                }
                Ok(Some(out))
            }
            Backend::Dwarfs(dw) => {
                let Some(inode) = dw.index.get_path(rel.split('/')) else {
                    return Ok(None);
                };
                let dir = inode
                    .as_dir()
                    .ok_or_else(|| anyhow!("{rel}: not a directory in the DwarFS image"))?;
                Ok(Some(
                    dir.entries().map(|e| walk_entry_from_dwarfs(&e)).collect(),
                ))
            }
        }
    }

    /// Build the [`NarNode`] tree for `rel` (described by `entry`) by walking
    /// the backend. Used for the DwarFS side of [`ReplayArchive::dump_nar`];
    /// the directory backend streams via
    /// [`rio_nix::nar::dump_path_streaming`] instead.
    fn nar_node(&self, rel: &str, entry: &WalkEntry) -> Result<NarNode> {
        match entry.kind {
            EntryKind::Regular => {
                let contents = self
                    .read_file(rel)?
                    .ok_or_else(|| anyhow!("{rel}: listed but unreadable"))?;
                Ok(NarNode::Regular {
                    executable: entry.executable,
                    contents,
                })
            }
            EntryKind::Symlink => {
                let target = entry
                    .symlink_target
                    .clone()
                    .ok_or_else(|| anyhow!("{rel}: symlink without a target"))?;
                Ok(NarNode::Symlink { target })
            }
            EntryKind::Directory => {
                let mut children = self
                    .list_dir(rel)?
                    .ok_or_else(|| anyhow!("{rel}: listed but unreadable"))?;
                // NAR requires directory entries sorted by name (byte order);
                // rio-nix's writer serializes in the order given.
                children.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
                let mut entries = Vec::with_capacity(children.len());
                for child in &children {
                    let child_rel = format!("{rel}/{}", child.name);
                    entries.push(NarEntry {
                        name: child.name.clone(),
                        node: self.nar_node(&child_rel, child)?,
                    });
                }
                Ok(NarNode::Directory { entries })
            }
            EntryKind::Other => bail!("{rel}: unsupported file type for NAR serialization"),
        }
    }
}

/// Build a [`WalkEntry`] from a filesystem path (directory backend).
fn walk_entry_from_fs(path: &Path, name: String) -> Result<WalkEntry> {
    use std::os::unix::fs::PermissionsExt as _;

    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let file_type = meta.file_type();
    let (kind, symlink_target) = if file_type.is_dir() {
        (EntryKind::Directory, None)
    } else if file_type.is_file() {
        (EntryKind::Regular, None)
    } else if file_type.is_symlink() {
        let target =
            std::fs::read_link(path).with_context(|| format!("readlink {}", path.display()))?;
        let target = target
            .to_str()
            .ok_or_else(|| anyhow!("{}: non-UTF-8 symlink target", path.display()))?
            .to_string();
        (EntryKind::Symlink, Some(target))
    } else {
        (EntryKind::Other, None)
    };
    Ok(WalkEntry {
        name,
        kind,
        size: if kind == EntryKind::Regular {
            meta.len()
        } else {
            0
        },
        executable: kind == EntryKind::Regular && meta.permissions().mode() & 0o100 != 0,
        symlink_target,
    })
}

/// Build a [`WalkEntry`] from a DwarFS directory entry.
fn walk_entry_from_dwarfs(entry: &dwarfs::DirEntry<'_>) -> WalkEntry {
    let inode = entry.inode();
    let executable = inode.metadata().file_type_mode().permission_bits() & 0o100 != 0;
    let (kind, size, symlink_target) = match inode.classify() {
        dwarfs::InodeKind::Directory(_) => (EntryKind::Directory, 0, None),
        dwarfs::InodeKind::File(file) => (EntryKind::Regular, file.as_chunks().total_size(), None),
        dwarfs::InodeKind::Symlink(link) => {
            (EntryKind::Symlink, 0, Some(link.target().to_string()))
        }
        _ => (EntryKind::Other, 0, None),
    };
    WalkEntry {
        name: entry.name().to_string(),
        kind,
        size,
        executable: kind == EntryKind::Regular && executable,
        symlink_target,
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay/basic")
    }

    /// Recursive copy of the fixture into `dst` so tests can delete or edit
    /// individual files without touching the committed copy.
    fn copy_fixture_to(dst: &Path) {
        fn copy_tree(src: &Path, dst: &Path) {
            std::fs::create_dir_all(dst).unwrap();
            for entry in std::fs::read_dir(src).unwrap() {
                let entry = entry.unwrap();
                let to = dst.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy_tree(&entry.path(), &to);
                } else {
                    std::fs::copy(entry.path(), &to).unwrap();
                }
            }
        }
        copy_tree(&fixture(), dst);
    }

    #[test]
    fn opens_dir_archive_and_parses_metadata() {
        let a = ReplayArchive::open(&fixture()).unwrap();
        assert_eq!(
            a.manifest().src_substituters,
            vec!["https://cache.example.org"]
        );
        assert!(!a.manifest().fat);
        assert_eq!(a.manifest().requests, 4);
        // Sorted by offset regardless of file order.
        let offs: Vec<f64> = a.requests().iter().map(|r| r.offset_s).collect();
        assert_eq!(offs, vec![0.25, 2.0, 5.5, 9.0]);
        assert_eq!(a.requests()[0].ssh_session_id, 10);
        assert_eq!(a.requests()[1].ssh_session_id, 13);
        // The "*" and [] output forms survive parsing.
        assert_eq!(a.requests()[2].paths[0].1, vec!["*"]);
        assert!(a.requests()[3].paths[1].1.is_empty());
        assert!(a.has_builds());
        let dep = a
            .builds()
            .get(&(
                10,
                "/nix/store/a1111111111111111111111111111111-dep.drv".to_string(),
            ))
            .unwrap();
        assert_eq!(dep.status, prod_status::BUILT);
        assert_eq!(dep.outputs["out"].nar_size, 120);
        // The cached drv was a cache hit at record time: no build record.
        assert!(!a.builds().keys().any(|(_, d)| d.contains("cached")));
        assert_eq!(a.impure_env().len(), 1);
    }

    #[test]
    fn reads_drvs_and_embedded_paths() {
        let a = ReplayArchive::open(&fixture()).unwrap();
        let drv = a
            .read_drv("/nix/store/a1111111111111111111111111111111-dep.drv")
            .unwrap();
        let parsed = rio_nix::derivation::Derivation::parse(&drv).unwrap();
        assert_eq!(parsed.outputs().len(), 1);
        // Basename form resolves too.
        let cached = a
            .read_drv("a4444444444444444444444444444444-cached.drv")
            .unwrap();
        let parsed_cached = rio_nix::derivation::Derivation::parse(&cached).unwrap();
        assert_eq!(parsed_cached.outputs().len(), 2);

        assert!(a.has_embedded("/nix/store/b1111111111111111111111111111111-src.txt"));
        assert!(!a.has_embedded("/nix/store/c1111111111111111111111111111111-dep"));
        // .drv files are derivations, not embedded source paths.
        assert!(!a.has_embedded("/nix/store/a1111111111111111111111111111111-dep.drv"));

        let ni = a.narinfo("b1111111111111111111111111111111").unwrap();
        let nar = a
            .dump_nar("/nix/store/b1111111111111111111111111111111-src.txt")
            .unwrap();
        assert_eq!(nar.len() as u64, ni.nar_size);
        // The dumped NAR's sha256 must match the sidecar's NarHash
        // (`sha256:<nixbase32>`).
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        let nar_hash = format!("sha256:{}", rio_nix::store_path::nixbase32::encode(&digest));
        assert_eq!(nar_hash, ni.nar_hash);
    }

    /// Smoke test for the DwarFS backend against a committed image of the
    /// same fixture (`basic.dwarfs`, built from `basic/` with `mkdwarfs`).
    #[test]
    fn opens_dwarfs_archive() {
        let image = fixture().parent().unwrap().join("basic.dwarfs");
        let a = ReplayArchive::open(&image).unwrap();
        assert_eq!(a.requests().len(), 4);
        assert!(a.has_embedded("/nix/store/b1111111111111111111111111111111-src.txt"));
        // The DwarFS NAR packer must reproduce the same NAR the sidecar (and
        // the directory backend) describe.
        let nar = a
            .dump_nar("b1111111111111111111111111111111-src.txt")
            .unwrap();
        let ni = a.narinfo("b1111111111111111111111111111111").unwrap();
        assert_eq!(nar.len() as u64, ni.nar_size);
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        assert_eq!(
            format!("sha256:{}", rio_nix::store_path::nixbase32::encode(&digest)),
            ni.nar_hash
        );
    }

    #[test]
    fn missing_optional_files_are_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("archive");
        copy_fixture_to(&dir);
        std::fs::remove_file(dir.join("builds.jsonl")).unwrap();
        std::fs::remove_file(dir.join("impure-env.json")).unwrap();

        let a = ReplayArchive::open(&dir).unwrap();
        assert!(!a.has_builds());
        assert!(a.builds().is_empty());
        assert!(a.impure_env().is_empty());
        assert_eq!(a.requests().len(), 4);
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("archive");
        copy_fixture_to(&dir);
        std::fs::remove_file(dir.join("manifest.json")).unwrap();

        let err = ReplayArchive::open(&dir).unwrap_err();
        assert!(
            format!("{err:#}").contains("manifest.json"),
            "error should name the missing manifest: {err:#}"
        );
    }

    #[test]
    fn malformed_requests_line_reports_file_and_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("archive");
        copy_fixture_to(&dir);
        let path = dir.join("requests.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"ssh_session_id\":99,\n");
        std::fs::write(&path, text).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&dir).unwrap_err());
        assert!(err.contains("requests.jsonl"), "{err}");
        assert!(err.contains("line 5"), "{err}");
    }

    #[test]
    fn negative_offsets_clamp_to_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("archive");
        copy_fixture_to(&dir);
        let path = dir.join("requests.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            "{\"ssh_session_id\":14,\"offset_s\":-3.5,\"paths\":[[\"/nix/store/a1111111111111111111111111111111-dep.drv\",[\"out\"]]]}\n",
        );
        std::fs::write(&path, text).unwrap();

        let a = ReplayArchive::open(&dir).unwrap();
        assert_eq!(a.requests().len(), 5);
        // Clamped to 0.0 it sorts before the 0.25 request.
        assert_eq!(a.requests()[0].ssh_session_id, 14);
        assert_eq!(a.requests()[0].offset_s, 0.0);
    }
}
