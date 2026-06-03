//! Storage backends for replay archives: a plain directory or a DwarFS
//! image read in-process. Everything above this layer goes through
//! `read_file`/`list_dir`; NAR packing of embedded trees on the DwarFS
//! side walks those primitives recursively.

use std::io::{ErrorKind, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail, ensure};
use dwarfs::AsChunks as _;
use rio_nix::nar::{MAX_DIRECTORY_ENTRIES, MAX_NAR_DEPTH, NarEntry, NarNode};

/// Where the archive bytes live. Everything above this enum goes through
/// [`Backend::read_file`] and [`Backend::list_dir`]; NAR packing of embedded
/// trees on the DwarFS side walks those same primitives recursively
/// ([`Backend::nar_node`]).
#[derive(Debug)]
pub(crate) enum Backend {
    /// A plain unpacked archive directory.
    Dir { root: PathBuf },
    /// A DwarFS image, read in-process. Boxed: the parsed index is much
    /// larger than a `PathBuf`.
    Dwarfs(Box<DwarfsBackend>),
}

/// DwarFS image state: `index` holds the parsed entry tree (cheap, immutable
/// lookups); `archive` owns the reader plus block cache and needs `&mut` for
/// reads, hence the mutex so callers' lookups stay `&self`.
#[derive(Debug)]
pub(crate) struct DwarfsBackend {
    index: dwarfs::ArchiveIndex,
    archive: Mutex<dwarfs::Archive<std::fs::File>>,
}

/// Entry kind yielded by [`Backend::list_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Regular,
    Directory,
    Symlink,
    /// Device/socket/fifo — never legitimate inside a store path.
    Other,
}

/// One directory entry from [`Backend::list_dir`].
#[derive(Debug, Clone)]
pub(crate) struct WalkEntry {
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
    /// File size in bytes (0 for directories and symlinks), as DECLARED
    /// by the backend's index (DwarFS chunk table / directory stat) —
    /// archive-controlled, never verified against content here. The NAR
    /// walk uses it as each file read's expected size (so a member whose
    /// content exceeds its own declaration errors instead of growing the
    /// read) and charges it against the walk's total byte budget.
    size: u64,
    /// Executable bit (regular files only).
    pub(crate) executable: bool,
    /// Symlink target, when `kind` is [`EntryKind::Symlink`].
    pub(crate) symlink_target: Option<String>,
}

impl Backend {
    /// Open `path` as a directory archive or, if it is a file, as a DwarFS
    /// image.
    pub(crate) fn open(path: &Path) -> Result<Self> {
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
            .with_context(|| format!("failed to open {} as a DwarFS image", path.display()))?;
        Ok(Backend::Dwarfs(Box::new(DwarfsBackend {
            index,
            archive: Mutex::new(archive),
        })))
    }

    /// Read a file's full contents, refusing above `max_bytes`. `Ok(None)`
    /// if the path doesn't exist.
    ///
    /// `max_bytes` is the caller's expected-size bound for the member
    /// class being read — REQUIRED by signature (the same treatment as
    /// `ArtifactStore::get_to_file`'s `max_bytes`), because archive bytes
    /// are archive-controlled and decompression-amplified: the 5 GiB
    /// publish cap bounds the COMPRESSED image, while DwarFS chunks alias
    /// zstd/LZMA blocks, so an under-cap image legally declares a member
    /// decompressing to hundreds of GiB. Without a per-read bound, `open`
    /// OOM-aborts (and the k8s Job retry-loops) instead of refusing with
    /// an error naming the member — the abort-vs-error gap the sibling
    /// depth/entry caps on [`Backend::nar_node`] were added to close.
    ///
    /// Both arms enforce the bound the same way: refuse when the
    /// backend's DECLARED size (stat / chunk table) exceeds `max_bytes`,
    /// then read through a `take(max_bytes + 1)` reader with a post-read
    /// length check — so a declaration that lies SMALL (a corrupt chunk
    /// table, a file growing mid-read) is bounded either way, mirroring
    /// `substituter::fetch_nar`'s anti-bomb discipline on the network
    /// sibling axis.
    pub(crate) fn read_file(&self, rel: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        match self {
            Backend::Dir { root } => {
                let path = root.join(rel);
                let declared = match std::fs::metadata(&path) {
                    Ok(meta) => meta.len(),
                    Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
                    Err(err) => {
                        return Err(err).with_context(|| format!("stat {}", path.display()));
                    }
                };
                ensure!(
                    declared <= max_bytes,
                    "{rel}: {declared} bytes exceeds the {max_bytes}-byte cap for this archive \
                     member class — refusing to buffer it"
                );
                let file = match std::fs::File::open(&path) {
                    Ok(file) => file,
                    // Deleted between stat and open: same answer as a
                    // missing path observed at stat time.
                    Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
                    Err(err) => {
                        return Err(err).with_context(|| format!("open {}", path.display()));
                    }
                };
                // Pre-size from the (archive-controlled) declared size,
                // capped so a declares-big-delivers-small member can't make
                // us reserve gigabytes up front (fetch_nar's discipline).
                let mut bytes = Vec::with_capacity(reserve_for(declared));
                std::io::Read::take(file, max_bytes.saturating_add(1))
                    // bounded-io: size-capped by the stat pre-check and the
                    // take(max_bytes + 1) belt above
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("read {}", path.display()))?;
                ensure!(
                    bytes.len() as u64 <= max_bytes,
                    "{rel}: grew past its {declared}-byte stat size while being read, beyond \
                     the {max_bytes}-byte cap for this archive member class"
                );
                Ok(Some(bytes))
            }
            Backend::Dwarfs(dw) => {
                let Some(inode) = dw.index.get_path(rel.split('/')) else {
                    return Ok(None);
                };
                let file = inode
                    .as_file()
                    .ok_or_else(|| anyhow!("{rel}: not a regular file in the DwarFS image"))?;
                let declared = file.as_chunks().total_size();
                ensure!(
                    declared <= max_bytes,
                    "{rel}: the image's chunk table declares {declared} bytes, exceeding the \
                     {max_bytes}-byte cap for this archive member class — refusing to buffer it"
                );
                // A panic during a read can't corrupt the block cache;
                // recovering from poisoning keeps later reads working.
                let mut archive = dw.archive.lock().unwrap_or_else(|e| e.into_inner());
                // Capped pre-size, same rationale as the directory arm.
                let mut bytes = Vec::with_capacity(reserve_for(declared));
                // `take` rather than the crate's `read_to_vec`: the belt
                // bounds a chunk table that lies SMALL, and `Take`'s
                // generic `read_to_end` sidesteps the crate reader's
                // exact-size assertion (an abort path) on such a lie.
                std::io::Read::take(file.as_reader(&mut *archive), max_bytes.saturating_add(1))
                    // bounded-io: size-capped by the chunk-table pre-check
                    // and the take(max_bytes + 1) belt above
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("read {rel} from the DwarFS image"))?;
                ensure!(
                    bytes.len() as u64 <= max_bytes,
                    "{rel}: the DwarFS image yielded more than its declared {declared} bytes, \
                     beyond the {max_bytes}-byte cap for this archive member class"
                );
                Ok(Some(bytes))
            }
        }
    }

    /// List a directory. `Ok(None)` if the path doesn't exist.
    pub(crate) fn list_dir(&self, rel: &str) -> Result<Option<Vec<WalkEntry>>> {
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
    /// the backend. Used for the DwarFS side of NAR dumping; the directory
    /// backend streams via [`rio_nix::nar::dump_path_streaming`] instead.
    ///
    /// Enforces rio-nix's [`MAX_NAR_DEPTH`] / [`MAX_DIRECTORY_ENTRIES`]
    /// caps, with the same depth accounting as `dump_path_streaming` (the
    /// walked root is depth 0), so the two backends agree on which trees
    /// are representable. Archive store trees are walked BEFORE any digest
    /// verification, the dwarfs format itself imposes no depth bound, and
    /// this walk recurses per directory level on a default 2 MiB blocking
    /// thread — without the cap, a deep tree in a hostile or foreign image
    /// stack-overflows (an abort, not an unwind) instead of erroring.
    ///
    /// The third hostile resource axis — member BYTE SIZE, sibling of the
    /// depth and fan-out caps above — is bounded by `byte_budget`: the
    /// whole tree is buffered in memory at once (every file's contents
    /// live in the returned [`NarNode`]), so each regular file's
    /// index-declared size is charged against the budget before its read,
    /// and the read itself is capped at exactly that declared size
    /// (`read_file`'s belt then catches a declaration that lies small).
    /// Callers pass [`super::MAX_EMBEDDED_NAR_BYTES`] in production.
    pub(crate) fn nar_node(
        &self,
        rel: &str,
        entry: &WalkEntry,
        byte_budget: u64,
    ) -> Result<NarNode> {
        let mut remaining = byte_budget;
        self.nar_node_at(rel, entry, 0, &mut remaining, byte_budget)
    }

    /// [`nar_node`](Self::nar_node) with the depth and the remaining byte
    /// budget threaded through the recursion (`byte_budget` rides along
    /// only so refusals can name the full cap, not the remainder).
    fn nar_node_at(
        &self,
        rel: &str,
        entry: &WalkEntry,
        depth: usize,
        remaining: &mut u64,
        byte_budget: u64,
    ) -> Result<NarNode> {
        if depth > MAX_NAR_DEPTH {
            bail!(
                "{rel}: directory nesting depth {depth} exceeds the NAR walker limit \
                 {MAX_NAR_DEPTH}"
            );
        }
        match entry.kind {
            EntryKind::Regular => {
                ensure!(
                    entry.size <= *remaining,
                    "{rel}: {} declared bytes exceed the remaining in-memory NAR budget \
                     ({remaining} of {byte_budget} bytes left) — refusing to buffer this tree",
                    entry.size
                );
                *remaining -= entry.size;
                let contents = self
                    .read_file(rel, entry.size)?
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
                // `>=`, matching every rio-nix walker's comparison exactly,
                // so the backends stay boundary-identical on the entry cap.
                if children.len() >= MAX_DIRECTORY_ENTRIES {
                    bail!(
                        "{rel}: {} directory entries exceed the NAR walker limit \
                         {MAX_DIRECTORY_ENTRIES}",
                        children.len()
                    );
                }
                // NAR requires directory entries sorted by name (byte order);
                // rio-nix's writer serializes in the order given.
                children.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
                let mut entries = Vec::with_capacity(children.len());
                for child in &children {
                    let child_rel = format!("{rel}/{}", child.name);
                    entries.push(NarEntry {
                        name: child.name.clone(),
                        node: self.nar_node_at(
                            &child_rel,
                            child,
                            depth + 1,
                            remaining,
                            byte_budget,
                        )?,
                    });
                }
                Ok(NarNode::Directory { entries })
            }
            EntryKind::Other => bail!("{rel}: unsupported file type for NAR serialization"),
        }
    }
}

/// Capped `Vec` pre-allocation for a member read: the declared size, but
/// never more than 64 MiB up front — a member declaring big and
/// delivering small must not lever a transient multi-GiB reserve out of
/// the allocator (the same capped-reserve discipline as
/// `substituter::fetch_nar`); larger honest members grow the buffer
/// incrementally under the caller's cap.
fn reserve_for(declared: u64) -> usize {
    usize::try_from(declared.min(64 * 1024 * 1024)).unwrap_or(0)
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
        // The shared NAR predicate, NOT an open-coded mask: this entry's
        // bit feeds `nar_node`, whose bytes must agree with rio-nix's
        // streaming dump (the sidecar NarHashes are recorded over that
        // dump) — see `is_nar_executable` for the single-source rule.
        executable: kind == EntryKind::Regular
            && rio_nix::nar::is_nar_executable(meta.permissions().mode()),
        symlink_target,
    })
}

/// Build a [`WalkEntry`] from a DwarFS directory entry.
fn walk_entry_from_dwarfs(entry: &dwarfs::DirEntry<'_>) -> WalkEntry {
    let inode = entry.inode();
    // Same shared predicate as `walk_entry_from_fs` — the two backends
    // and rio-nix's walkers must mark the same files executable or the
    // directory form and the image form of one archive NAR-serialize
    // differently (mkdwarfs preserves modes verbatim, so non-canonical
    // foreign/v0 modes reach this decision unchanged).
    let executable =
        rio_nix::nar::is_nar_executable(inode.metadata().file_type_mode().permission_bits());
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
    use super::*;
    use crate::archive::writer::pack_with_mkdwarfs;
    use crate::archive::writer::test_support::tiny_archive;
    use crate::archive::{MANIFEST_MEMBER, STORE_DIR};

    /// Generous member cap for tests that aren't about the bound itself.
    const TEST_MEMBER_CAP: u64 = 1024 * 1024;

    #[test]
    fn dir_backend_reads_files_and_lists_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        tiny_archive(&root);

        let backend = Backend::open(&root).unwrap();
        assert!(
            backend
                .read_file(MANIFEST_MEMBER, TEST_MEMBER_CAP)
                .unwrap()
                .is_some()
        );
        assert!(
            backend
                .read_file("nope.json", TEST_MEMBER_CAP)
                .unwrap()
                .is_none()
        );

        let entries = backend.list_dir(STORE_DIR).unwrap().unwrap();
        assert_eq!(entries.len(), 3, "got: {entries:?}");
        let drvs = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Regular && entry.name.ends_with(".drv"))
            .count();
        let dirs = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .count();
        assert_eq!(drvs, 2, "got: {entries:?}");
        assert_eq!(dirs, 1, "got: {entries:?}");

        assert!(backend.list_dir("missing").unwrap().is_none());
    }

    #[test]
    fn dwarfs_backend_matches_dir_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        tiny_archive(&root);
        let image = dir.path().join("archive.dwarfs");
        pack_with_mkdwarfs(&root, &image).unwrap();

        let dir_backend = Backend::open(&root).unwrap();
        let image_backend = Backend::open(&image).unwrap();

        // Identical member bytes through both backends.
        assert_eq!(
            dir_backend
                .read_file(MANIFEST_MEMBER, TEST_MEMBER_CAP)
                .unwrap()
                .unwrap(),
            image_backend
                .read_file(MANIFEST_MEMBER, TEST_MEMBER_CAP)
                .unwrap()
                .unwrap()
        );

        // Identical store listings (sorted; backends promise no entry order).
        let sorted_names = |backend: &Backend| {
            let mut names: Vec<String> = backend
                .list_dir(STORE_DIR)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            names.sort();
            names
        };
        assert_eq!(sorted_names(&dir_backend), sorted_names(&image_backend));

        // The embedded source tree (regular file + executable + symlink)
        // NAR-serializes byte-identically from either backend.
        let nar_of_embedded_tree = |backend: &Backend| {
            let entry = backend
                .list_dir(STORE_DIR)
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|entry| entry.kind == EntryKind::Directory)
                .expect("the tiny archive embeds one store-path tree");
            let rel = format!("{STORE_DIR}/{}", entry.name);
            let node = backend
                .nar_node(&rel, &entry, crate::archive::MAX_EMBEDDED_NAR_BYTES)
                .unwrap();
            let mut nar = Vec::new();
            rio_nix::nar::serialize(&mut nar, &node).unwrap();
            nar
        };
        assert_eq!(
            nar_of_embedded_tree(&dir_backend),
            nar_of_embedded_tree(&image_backend)
        );
    }

    /// Append a chain of `nested_dirs` single-child directories under
    /// `root`, with one file in the deepest one. With the chain root at
    /// NAR depth 0, the file sits at depth `nested_dirs + 1`.
    fn deep_tree(root: &std::path::Path, nested_dirs: usize) {
        let mut path = root.to_path_buf();
        for _ in 0..nested_dirs {
            path.push("d");
        }
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("f"), b"leaf").unwrap();
    }

    /// Resource-limit parity at the depth boundary: a tree whose deepest
    /// node sits at exactly rio-nix's `MAX_NAR_DEPTH` walks through BOTH
    /// backends (must-admit, byte-identically — and identically to the
    /// directory backend's production streaming dump), while one level
    /// deeper is REFUSED by both backends and by the streaming dump
    /// (must-block). The expectation universe is rio-nix's exported caps —
    /// the same constants every guarded NAR walker enforces; before the
    /// DwarFS walk shared them, it recursed unguarded and a deep tree in an
    /// untrusted image stack-overflowed (aborted) the engine while the
    /// directory backend errored cleanly.
    #[test]
    fn backends_agree_on_the_nar_depth_limit() {
        use rio_nix::nar::MAX_NAR_DEPTH;

        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        let store = root.join(STORE_DIR);
        // File at depth MAX_NAR_DEPTH (the limit): tree root depth 0 +
        // (MAX-1) nested dirs + the file.
        deep_tree(&store.join("tree-at-limit"), MAX_NAR_DEPTH - 1);
        // File at depth MAX_NAR_DEPTH + 1: one past the limit.
        deep_tree(&store.join("tree-too-deep"), MAX_NAR_DEPTH);
        let image = dir.path().join("archive.dwarfs");
        pack_with_mkdwarfs(&root, &image).unwrap();

        let dir_backend = Backend::open(&root).unwrap();
        let image_backend = Backend::open(&image).unwrap();
        let entry_named = |backend: &Backend, name: &str| {
            backend
                .list_dir(STORE_DIR)
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("fixture tree {name} missing"))
        };

        // Must-admit: at the limit, both backends produce the same bytes,
        // and the directory backend's production path (the guarded
        // streaming dump) agrees byte-for-byte — three walkers, one cap.
        let rel = format!("{STORE_DIR}/tree-at-limit");
        let nar_via = |backend: &Backend| {
            let entry = entry_named(backend, "tree-at-limit");
            let node = backend
                .nar_node(&rel, &entry, crate::archive::MAX_EMBEDDED_NAR_BYTES)
                .unwrap();
            let mut nar = Vec::new();
            rio_nix::nar::serialize(&mut nar, &node).unwrap();
            nar
        };
        let from_dir = nar_via(&dir_backend);
        let from_image = nar_via(&image_backend);
        assert_eq!(from_dir, from_image);
        let mut streamed = Vec::new();
        rio_nix::nar::dump_path_streaming(&store.join("tree-at-limit"), &mut streamed).unwrap();
        assert_eq!(from_dir, streamed);

        // Must-block: one level past the limit, every walker refuses —
        // an error naming the depth, never an abort.
        let rel = format!("{STORE_DIR}/tree-too-deep");
        for backend in [&dir_backend, &image_backend] {
            let entry = entry_named(backend, "tree-too-deep");
            let err = format!(
                "{:#}",
                backend
                    .nar_node(&rel, &entry, crate::archive::MAX_EMBEDDED_NAR_BYTES)
                    .unwrap_err()
            );
            assert!(
                err.contains(&MAX_NAR_DEPTH.to_string()),
                "the refusal names the shared cap: {err}"
            );
        }
        let streamed_err =
            rio_nix::nar::dump_path_streaming(&store.join("tree-too-deep"), &mut Vec::new())
                .unwrap_err();
        assert!(
            matches!(streamed_err, rio_nix::nar::NarError::NestingTooDeep(_)),
            "the streaming dump refuses the same tree: {streamed_err:?}"
        );
    }

    /// Resource-limit parity at the member-size boundary — the third
    /// hostile resource axis of the same walk the depth/entry caps guard
    /// (`backends_agree_on_the_nar_depth_limit` is the sibling test).
    /// `read_file`'s bound is REQUIRED by signature; the rows here cross
    /// it in both directions through BOTH backends: a member exactly at
    /// the cap is admitted byte-identically (must-admit), one byte over
    /// is refused with an error naming the member and the cap — a
    /// refusal, never an OOM abort (must-block). The image's bytes are
    /// archive-controlled (a hostile image's chunk table declares
    /// whatever it likes), which is why the caller's cap, not the
    /// member's declaration, decides.
    #[test]
    fn backends_agree_on_the_member_size_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        tiny_archive(&root);
        let image = dir.path().join("archive.dwarfs");
        pack_with_mkdwarfs(&root, &image).unwrap();

        let dir_backend = Backend::open(&root).unwrap();
        let image_backend = Backend::open(&image).unwrap();

        let manifest_len = std::fs::metadata(root.join(MANIFEST_MEMBER)).unwrap().len();

        // Must-admit: a cap of exactly the member's size reads the full
        // bytes through both backends, byte-identically.
        let at_cap_dir = dir_backend
            .read_file(MANIFEST_MEMBER, manifest_len)
            .unwrap()
            .unwrap();
        let at_cap_image = image_backend
            .read_file(MANIFEST_MEMBER, manifest_len)
            .unwrap()
            .unwrap();
        assert_eq!(at_cap_dir, at_cap_image);
        assert_eq!(at_cap_dir.len() as u64, manifest_len);

        // Must-block: one byte under the member's size, both backends
        // refuse — an error naming the member and the cap, never an
        // abort, and never a silent truncation.
        let cap = manifest_len - 1;
        for backend in [&dir_backend, &image_backend] {
            let err = format!("{:#}", backend.read_file(MANIFEST_MEMBER, cap).unwrap_err());
            assert!(
                err.contains(MANIFEST_MEMBER) && err.contains(&cap.to_string()),
                "the refusal names the member and the cap: {err}"
            );
        }

        // Missing members still answer None under any cap (the bound
        // gates sizes, not existence).
        for backend in [&dir_backend, &image_backend] {
            assert!(backend.read_file("nope.json", 1).unwrap().is_none());
        }
    }

    /// The in-memory NAR walk's total byte budget — the whole-tree
    /// sibling of the per-member cap above, because `nar_node` holds
    /// every file's contents simultaneously: a tree whose summed
    /// DECLARED sizes fit the budget walks through both backends
    /// byte-identically (must-admit, exactly at the boundary), one
    /// declared byte over is refused with an error naming the budget
    /// (must-block) — so a hostile index declaring many small files
    /// cannot multiply per-file allowances into an unbounded buffer.
    #[test]
    fn backends_agree_on_the_nar_byte_budget() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        let store = root.join(STORE_DIR);
        let tree = store.join("tree-sized");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("a"), b"12345678").unwrap(); // 8 bytes
        std::fs::write(tree.join("b"), b"87654321").unwrap(); // 8 bytes
        let image = dir.path().join("archive.dwarfs");
        pack_with_mkdwarfs(&root, &image).unwrap();

        let dir_backend = Backend::open(&root).unwrap();
        let image_backend = Backend::open(&image).unwrap();
        let entry_of = |backend: &Backend| {
            backend
                .list_dir(STORE_DIR)
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|entry| entry.name == "tree-sized")
                .expect("fixture tree missing")
        };
        let rel = format!("{STORE_DIR}/tree-sized");

        // Must-admit at the exact boundary: 16 declared bytes, 16-byte
        // budget — both backends, identical bytes.
        let nar_via = |backend: &Backend| {
            let node = backend.nar_node(&rel, &entry_of(backend), 16).unwrap();
            let mut nar = Vec::new();
            rio_nix::nar::serialize(&mut nar, &node).unwrap();
            nar
        };
        assert_eq!(nar_via(&dir_backend), nar_via(&image_backend));

        // Must-block one byte short: the second file's declared size no
        // longer fits; the refusal names the budget, never aborts.
        for backend in [&dir_backend, &image_backend] {
            let err = format!(
                "{:#}",
                backend.nar_node(&rel, &entry_of(backend), 15).unwrap_err()
            );
            assert!(
                err.contains("NAR budget") && err.contains("15"),
                "the refusal names the byte budget: {err}"
            );
        }
    }
}
