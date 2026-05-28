//! Storage backends for replay archives: a plain directory or a DwarFS
//! image read in-process. Everything above this layer goes through
//! `read_file`/`list_dir`; NAR packing of embedded trees on the DwarFS
//! side walks those primitives recursively.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use dwarfs::AsChunks as _;
use rio_nix::nar::{NarEntry, NarNode};

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
    /// File size in bytes (0 for directories and symlinks).
    // NAR packing reads file contents directly; the size is kept for
    // debugging entry listings (Debug derive).
    #[allow(dead_code)]
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

    /// Read a file's full contents. `Ok(None)` if the path doesn't exist.
    pub(crate) fn read_file(&self, rel: &str) -> Result<Option<Vec<u8>>> {
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
                // A panic during a read can't corrupt the block cache;
                // recovering from poisoning keeps later reads working.
                let mut archive = dw.archive.lock().unwrap_or_else(|e| e.into_inner());
                let bytes = file
                    .read_to_vec(&mut *archive)
                    .with_context(|| format!("read {rel} from the DwarFS image"))?;
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
    pub(crate) fn nar_node(&self, rel: &str, entry: &WalkEntry) -> Result<NarNode> {
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
    use super::*;
    use crate::archive::writer::pack_with_mkdwarfs;
    use crate::archive::writer::test_support::tiny_archive;
    use crate::archive::{MANIFEST_MEMBER, STORE_DIR};

    #[test]
    fn dir_backend_reads_files_and_lists_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        tiny_archive(&root);

        let backend = Backend::open(&root).unwrap();
        assert!(backend.read_file(MANIFEST_MEMBER).unwrap().is_some());
        assert!(backend.read_file("nope.json").unwrap().is_none());

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
            dir_backend.read_file(MANIFEST_MEMBER).unwrap().unwrap(),
            image_backend.read_file(MANIFEST_MEMBER).unwrap().unwrap()
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
            let node = backend.nar_node(&rel, &entry).unwrap();
            let mut nar = Vec::new();
            rio_nix::nar::serialize(&mut nar, &node).unwrap();
            nar
        };
        assert_eq!(
            nar_of_embedded_tree(&dir_backend),
            nar_of_embedded_tree(&image_backend)
        );
    }
}
