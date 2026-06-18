//! Mount-time Directory-DAG prefetch and the inode table (ADR-022
//! §2.2–2.3).
//!
//! The castore-FUSE serves an immutable tree: the closure's store-path
//! basenames under a synthetic root, each expanding into the path's
//! castore Directory DAG. Everything `lookup`/`getattr`/`readdir`/
//! `readlink` need is held in heap, populated by one multi-root
//! `GetDirectory(recursive=true)` call before the mount is announced.
//! Chunk coordinates are NOT prefetched — `open()` resolves them
//! server-side via `ReadBlob`/`StatBlob` keyed on `file_digest` alone.
//!
//! Inode numbers are small sequential values allocated during the tree
//! build (they must fit a non-LFS 32-bit payload's ino_t; see
//! `InoAlloc`), with identity split by node kind: files and symlinks
//! are content-deduped (identical bytes anywhere in the closure share
//! one inode — legal hardlink semantics, reported via `st_nlink`),
//! while directories are PATH-unique (each alias of a content-deduped
//! `Directory` body gets its own inode — POSIX forbids hardlinked
//! directories, and serving them desyncs tree walkers; see
//! `InoAlloc::dir`). The decoded `Directory` bodies, the backing
//! cache, and the chunk store all stay content-deduped — only the
//! runtime inode numbers multiply, so heap scales with the closure's
//! path count rather than its content count.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use prost::Message;

use rio_proto::castore::{Directory, FileEntry, RootNode, SymlinkEntry, root_node};
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::{GetDirectoryRequest, get_directory_request::ByWhat};

/// Every reply TTL in the castore-FUSE. The tree is immutable for the
/// mount's lifetime, so the kernel never needs to revalidate a dentry
/// or attr — `Duration::MAX` saturates to the kernel's
/// `MAX_SEC_IN_JIFFIES` without overflow.
pub const TTL: Duration = Duration::MAX;

/// Canonical store-path mtime (`1970-01-01 00:00:01`). Matches what
/// `nix-store --optimise` and the NAR format normalize to; a 0 mtime
/// breaks tools that treat it as "missing".
const STORE_MTIME: Duration = Duration::from_secs(1);

/// One node in the content-addressed tree. Each variant carries exactly
/// what its FUSE ops need: `File` resolves to a backing blob at
/// `open()`, `Dir` keys the [`InoMap`]'s directory bodies for
/// `lookup`/`readdir`, `Symlink` is its own `readlink` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A regular file. `open()` resolves `file_digest` to a backing
    /// blob (cache hit or fetch+promote).
    File {
        /// blake3 of the file body — the backing-cache key.
        file_digest: [u8; 32],
        /// `st_size`.
        size: u64,
        /// Whether `st_mode` carries the execute bits.
        executable: bool,
    },
    /// A directory. `lookup`/`readdir` index its body via the
    /// per-path child index in [`InoMap`].
    Dir {
        /// blake3 of the canonical `Directory` proto encoding.
        dir_digest: [u8; 32],
    },
    /// A symbolic link. `readlink` returns `target` verbatim.
    Symlink {
        /// The raw link target bytes (no UTF-8 requirement).
        target: Vec<u8>,
    },
}

/// One entry of a `readdir`/`readdirplus` enumeration.
pub struct DirEntry<'a> {
    /// The child's allocated ino.
    pub ino: u64,
    /// Offset of the *next* entry — what the kernel passes back to
    /// resume after this one.
    pub next_offset: u64,
    /// `d_type` for this entry.
    pub kind: FileType,
    /// The entry name (no NUL, no `/`); borrowed from the index.
    pub name: &'a [u8],
}

/// Errors from DAG prefetch / tree construction. All of them mean the
/// build never started — the caller classifies every variant as an
/// infrastructure failure and re-queues.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// A `GetDirectory` RPC failed.
    #[error("GetDirectory: {0}")]
    Rpc(#[from] tonic::Status),
    /// The recursive `GetDirectory` stream did not finish in time.
    #[error("GetDirectory(recursive) timed out after {0:?}")]
    Timeout(Duration),
    /// An input root's `root_node` is unset on the store side.
    #[error("input root {store_path:?} has no root_node (path not yet NAR-indexed)")]
    MissingRootNode {
        /// The closure store path whose castore root is missing.
        store_path: String,
    },
    /// `context` is the input root's store path, or the parent
    /// directory's hex digest when the bad entry is inside a streamed
    /// `Directory` body.
    #[error("castore digest under {context:?} is {got} bytes, want 32")]
    BadDigestLen {
        /// Where the bad digest was encountered (store path or parent
        /// dir hex digest).
        context: String,
        /// The malformed digest length.
        got: usize,
    },
    /// An input root store path has no path component after `/`.
    #[error("store path {store_path:?} has no basename")]
    BadStorePath {
        /// The offending closure store path.
        store_path: String,
    },
    /// Two input roots share a basename — the FUSE root would collide.
    #[error("duplicate store-path basename {basename:?} in input_roots")]
    DuplicateBasename {
        /// The colliding store-path basename.
        basename: String,
    },
    /// A directory digest is reachable from a root but the recursive
    /// `GetDirectory` stream never returned its body.
    #[error(
        "directory {digest} is referenced by the DAG but was not returned by GetDirectory \
         ({returned} bodies returned)"
    )]
    MissingDirectory {
        /// Hex blake3 of the missing directory body.
        digest: String,
        /// How many `Directory` bodies the stream did return.
        returned: usize,
    },
}

/// The castore-FUSE's entire metadata state: inode → node, per-path
/// directory child indexes, and the synthetic root's children. Built
/// once at mount, never mutated.
pub struct InoMap {
    /// ino → node, for every node reachable from the roots. Two files
    /// with identical bytes and executable bit share one entry; many
    /// per-path directory inos may map to one `Node::Dir` body digest.
    inodes: HashMap<u64, Node>,
    /// dir ino → its serve-time child index, one per directory PATH
    /// (aliases of a content-deduped body each get their own).
    /// Precomputed at build so the metadata hot path
    /// (`lookup`/`readdir`, the bulk of a cold `find` over the
    /// closure) never scans a child list — that scan (plus the child
    /// ino re-derivation the old blake3 scheme needed) showed up as
    /// ~11% of serve-thread CPU before this index existed.
    children: HashMap<u64, DirChildren>,
    /// `FUSE_ROOT_ID`'s children: store-path basename → child ino.
    /// `BTreeMap` so `readdir(ROOT)` enumerates in a stable order
    /// across calls (the kernel resumes by offset).
    roots: BTreeMap<Vec<u8>, u64>,
    /// file/symlink ino → number of paths that reach it (`st_nlink`).
    /// Deduped content with several aliases is exactly a hardlink, so
    /// the count must be honest for `du`/`tar` to dedup by
    /// (dev, ino, nlink). Directory nlink lives in [`DirChildren`].
    nlink: HashMap<u64, u32>,
    /// `FUSE_ROOT_ID`'s `st_nlink`: 2 + number of directory roots.
    root_nlink: u32,
}

/// One directory's child index. The decoded `Directory` bodies are
/// dropped after construction — everything the serve path needs is
/// here and in [`InoMap::inodes`].
struct DirChildren {
    /// The directory's parent ino — `readdir`'s `..` entry. Unique
    /// because inos are per-path: every directory is reached by
    /// exactly one (parent, name) edge.
    parent_ino: u64,
    /// The `Directory` body's canonical order (directories, files,
    /// symlinks) with each child's allocated ino — `readdir`'s stable
    /// enumeration, offset-resumable because the order never changes.
    entries: Vec<(Vec<u8>, u64, FileType)>,
    /// `lookup(parent, name)` probe: name → allocated ino.
    by_name: HashMap<Vec<u8>, u64>,
    /// `st_nlink`: 2 + subdirectory count (`.`, the parent's entry,
    /// and one `..` per subdirectory) — the convention fts' leaf-count
    /// optimization relies on.
    nlink: u32,
}

// ── Inode allocation (ADR §2.3) ──────────────────────────────────────
//
// Inos are small sequential numbers handed out during the one-shot
// `from_parts` walk — counter from `FUSE_ROOT_ID + 1` — NOT digest
// hashes. The production pools advertise 32-bit build payloads
// (i686-linux), and a non-LFS 32-bit glibc binary gets EOVERFLOW from
// stat()/readdir() for any st_ino above 2^32; the previous scheme (low
// 63 bits of blake3 with bit 63 forced) put every ino at ≥ 2^63. The
// dedup classes are preserved by keying the allocator: same class →
// same ino, so icache/nlink/page-cache semantics are unchanged. No ino
// is ever persisted outside FUSE replies, so the numbering being
// per-mount (allocation-order-dependent) is unobservable.

/// Sequential ino allocator for one tree build. Dedup classes:
/// files key on (content digest, executable bit), symlinks on target
/// bytes, directories are one fresh ino per PATH (never deduped —
/// content-shared directory inos are hardlinked-directory semantics,
/// which POSIX forbids and which desync fts-based tree walkers; see
/// `InoAlloc::dir`).
// r[impl builder.fs.castore-inode-digest+2]
struct InoAlloc {
    next: u64,
    /// (file_digest, executable) → ino. The executable bit is part of
    /// the key because `st_mode` is per-inode in VFS — two paths
    /// sharing an inode share their mode. Both inodes still resolve to
    /// the same backing-cache file (keyed by `file_digest` alone), so
    /// the split costs one extra `struct inode`, not a second fetch.
    files: HashMap<([u8; 32], bool), u64>,
    /// symlink target bytes → ino.
    symlinks: HashMap<Vec<u8>, u64>,
}

impl InoAlloc {
    fn new() -> Self {
        Self {
            next: INodeNo::ROOT.0 + 1,
            files: HashMap::new(),
            symlinks: HashMap::new(),
        }
    }

    fn fresh(next: &mut u64) -> u64 {
        let ino = *next;
        *next += 1;
        ino
    }

    /// Same (digest, exec) anywhere in the closure → same ino: the
    /// kernel's icache holds one `struct inode`, one `open()` upcall
    /// fetches the bytes once, one `BACKING_OPEN` binds them to one
    /// page-cache.
    fn file(&mut self, file_digest: [u8; 32], executable: bool) -> u64 {
        *self
            .files
            .entry((file_digest, executable))
            .or_insert_with(|| Self::fresh(&mut self.next))
    }

    /// Same target bytes → same ino (an honest hardlink, like files).
    fn symlink(&mut self, target: &[u8]) -> u64 {
        if let Some(&ino) = self.symlinks.get(target) {
            return ino;
        }
        let ino = Self::fresh(&mut self.next);
        self.symlinks.insert(target.to_vec(), ino);
        ino
    }

    /// One fresh ino per directory PATH. Content-deduping directory
    /// inos (the original `h(dir_digest)` scheme) gave one inode to
    /// every alias of an identical subtree — hardlinked-directory
    /// semantics, which POSIX/Linux forbid (`link(2)` on a directory
    /// is EPERM) precisely because multiple parents break tree
    /// walkers: the kernel holds ONE dentry for the shared inode and
    /// re-parents it whenever a concurrent walk takes a different
    /// alias, so GNU find's fts — ascending via `openat(fd, "..")` and
    /// comparing (dev, ino) against the remembered parent — sees a
    /// mismatch, fabricates ENOENT, and aborts with zero failing
    /// syscalls. Per-path inos keep the single-parent invariant; the
    /// decoded body, backing cache, and chunks stay deduped by digest.
    fn dir(&mut self) -> u64 {
        Self::fresh(&mut self.next)
    }
}

fn digest32(bytes: &[u8], context: &str) -> Result<[u8; 32], TreeError> {
    bytes.try_into().map_err(|_| TreeError::BadDigestLen {
        context: context.to_owned(),
        got: bytes.len(),
    })
}

impl InoMap {
    /// Prefetch the closure's Directory DAG and build the inode table.
    ///
    /// One `GetDirectory(recursive=true)` seeded with every root's
    /// `dir_digest` (multi-root; one RPC for the whole closure instead
    /// of one per root — the I-110 PG-wall lesson). Wrapped in
    /// `timeout` so a hung store surfaces as an infra-retry before the
    /// build starts, not as a wedged mount.
    ///
    /// `assignment_token`: store RPC auth; empty = no header — see
    /// `mount_and_serve`'s `assignment_token` parameter.
    // r[impl builder.fs.castore-dag-source]
    pub async fn prefetch(
        client: &mut DirectoryServiceClient<tonic::transport::Channel>,
        input_roots: &[rio_proto::types::InputRoot],
        timeout: Duration,
        assignment_token: &str,
    ) -> Result<Self, TreeError> {
        let started = std::time::Instant::now();
        let roots: Vec<(String, RootNode)> = input_roots
            .iter()
            .map(|r| {
                let node = r
                    .root_node
                    .clone()
                    .ok_or_else(|| TreeError::MissingRootNode {
                        store_path: r.store_path.clone(),
                    })?;
                Ok((r.store_path.clone(), node))
            })
            .collect::<Result<_, TreeError>>()?;

        // Seed the BFS frontier with every directory root. File and
        // symlink roots have no DAG to fetch.
        let mut seeds: Vec<Vec<u8>> = Vec::new();
        for (path, node) in &roots {
            if let Some(root_node::Node::DirDigest(d)) = &node.node {
                digest32(d, path)?;
                seeds.push(d.clone());
            }
        }

        let directories = if seeds.is_empty() {
            Vec::new()
        } else {
            let mut req = tonic::Request::new(GetDirectoryRequest {
                by_what: Some(ByWhat::Digest(seeds[0].clone())),
                recursive: true,
                digests: seeds[1..].to_vec(),
            });
            crate::upload::common::attach_assignment_token(&mut req, assignment_token)?;
            let fetch = async {
                let mut stream = client.get_directory(req).await?.into_inner();
                let mut out = Vec::new();
                while let Some(dir) = stream.message().await? {
                    out.push(dir);
                }
                Ok::<_, tonic::Status>(out)
            };
            tokio::time::timeout(timeout, fetch)
                .await
                .map_err(|_| TreeError::Timeout(timeout))??
        };

        metrics::histogram!("rio_builder_castore_dag_prefetch_seconds")
            .record(started.elapsed().as_secs_f64());
        Self::from_parts(&roots, directories)
    }

    /// Pure tree construction from already-fetched parts. Split from
    /// [`InoMap::prefetch`] so unit tests can build a tree without a
    /// store.
    ///
    /// Fails if any directory referenced by the DAG is missing from
    /// `directories` — a partial prefetch would otherwise turn a
    /// declared input subtree into `ENOENT`, which the JIT-fetch
    /// imperative forbids (ENOENT is reserved for names *outside* the
    /// closure).
    pub fn from_parts(
        roots: &[(String, RootNode)],
        directories: Vec<Directory>,
    ) -> Result<Self, TreeError> {
        // Key every received body by its recomputed canonical digest.
        // The stream has no digest field; trusting a server-claimed
        // digest would let a buggy store alias one subtree as another.
        let mut dirs: HashMap<[u8; 32], Directory> = HashMap::with_capacity(directories.len());
        let returned = directories.len();
        for dir in directories {
            let digest = *blake3::hash(&dir.encode_to_vec()).as_bytes();
            dirs.insert(digest, dir);
        }

        let mut inodes: HashMap<u64, Node> = HashMap::new();
        let mut nlink: HashMap<u64, u32> = HashMap::new();
        let mut root_children: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        // Directory PATHS whose children still need inodes:
        // (own ino, body digest, parent ino). Per-path expansion — a
        // content-deduped body is revisited once per alias, so the walk
        // is bounded by the closure's path count (~572k on a
        // whole-store mount), not its body count. Explicit stack: the
        // tree can be deep.
        let mut pending: Vec<(u64, [u8; 32], u64)> = Vec::new();

        let mut alloc = InoAlloc::new();

        // `ctx` is the error context for a bad digest length: the input
        // root's store path, or the parent directory's hex digest.
        let insert_file = |f: &FileEntry,
                           ctx: &str,
                           alloc: &mut InoAlloc,
                           inodes: &mut HashMap<u64, Node>,
                           nlink: &mut HashMap<u64, u32>|
         -> Result<u64, TreeError> {
            let digest = digest32(&f.digest, ctx)?;
            let ino = alloc.file(digest, f.executable);
            inodes.insert(
                ino,
                Node::File {
                    file_digest: digest,
                    size: f.size,
                    executable: f.executable,
                },
            );
            *nlink.entry(ino).or_insert(0) += 1;
            Ok(ino)
        };

        let insert_symlink = |s: &SymlinkEntry,
                              alloc: &mut InoAlloc,
                              inodes: &mut HashMap<u64, Node>,
                              nlink: &mut HashMap<u64, u32>|
         -> u64 {
            let ino = alloc.symlink(&s.target);
            inodes.insert(
                ino,
                Node::Symlink {
                    target: s.target.clone(),
                },
            );
            *nlink.entry(ino).or_insert(0) += 1;
            ino
        };

        let mut root_nlink: u32 = 2;
        for (store_path, root) in roots {
            let basename = store_path
                .rsplit('/')
                .next()
                .filter(|b| !b.is_empty())
                .ok_or_else(|| TreeError::BadStorePath {
                    store_path: store_path.clone(),
                })?;
            let ino = match &root.node {
                Some(root_node::Node::DirDigest(d)) => {
                    let digest = digest32(d, store_path)?;
                    let ino = alloc.dir();
                    inodes.insert(ino, Node::Dir { dir_digest: digest });
                    pending.push((ino, digest, INodeNo::ROOT.0));
                    root_nlink += 1;
                    ino
                }
                Some(root_node::Node::File(f)) => {
                    insert_file(f, store_path, &mut alloc, &mut inodes, &mut nlink)?
                }
                Some(root_node::Node::Symlink(s)) => {
                    insert_symlink(s, &mut alloc, &mut inodes, &mut nlink)
                }
                None => {
                    return Err(TreeError::MissingRootNode {
                        store_path: store_path.clone(),
                    });
                }
            };
            if root_children
                .insert(basename.as_bytes().to_vec(), ino)
                .is_some()
            {
                // Two input roots with the same basename would make
                // lookup(ROOT, name) ambiguous. Store-path basenames
                // are unique by construction (hash prefix); a duplicate
                // means the scheduler sent a malformed closure.
                return Err(TreeError::DuplicateBasename {
                    basename: basename.to_owned(),
                });
            }
        }

        let mut children: HashMap<u64, DirChildren> = HashMap::with_capacity(pending.len());
        while let Some((self_ino, digest, parent_ino)) = pending.pop() {
            let Some(dir) = dirs.get(&digest) else {
                return Err(TreeError::MissingDirectory {
                    digest: hex::encode(digest),
                    returned,
                });
            };
            // Error context only; hoisted so the per-child calls below
            // don't allocate a hex string each on the happy path.
            let ctx = hex::encode(digest);
            let mut entries: Vec<(Vec<u8>, u64, FileType)> =
                Vec::with_capacity(dir.directories.len() + dir.files.len() + dir.symlinks.len());
            for d in &dir.directories {
                let child_digest = digest32(&d.digest, &ctx)?;
                let ino = alloc.dir();
                inodes.insert(
                    ino,
                    Node::Dir {
                        dir_digest: child_digest,
                    },
                );
                pending.push((ino, child_digest, self_ino));
                entries.push((d.name.clone(), ino, FileType::Directory));
            }
            for f in &dir.files {
                let ino = insert_file(f, &ctx, &mut alloc, &mut inodes, &mut nlink)?;
                entries.push((f.name.clone(), ino, FileType::RegularFile));
            }
            for s in &dir.symlinks {
                let ino = insert_symlink(s, &mut alloc, &mut inodes, &mut nlink);
                entries.push((s.name.clone(), ino, FileType::Symlink));
            }
            let by_name = entries
                .iter()
                .map(|(n, ino, _)| (n.clone(), *ino))
                .collect();
            children.insert(
                self_ino,
                DirChildren {
                    parent_ino,
                    nlink: 2 + dir.directories.len() as u32,
                    entries,
                    by_name,
                },
            );
        }

        Ok(Self {
            inodes,
            children,
            roots: root_children,
            nlink,
            root_nlink,
        })
    }

    /// Resolve `name` under `parent_ino`. `None` = the name is not in
    /// the prefetched DAG → the caller replies a cached negative entry
    /// (the declared-input allowlist: anything outside the closure is
    /// a legitimate ENOENT, never a fetch trigger).
    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Option<(u64, FileAttr)> {
        if parent_ino == INodeNo::ROOT.0 {
            let ino = *self.roots.get(name)?;
            return Some((ino, self.attr(ino)?));
        }
        // Two hash probes: the child index was built alongside the
        // inode table, so a hit here is exactly the ino the allocator
        // assigned during the build walk (and a non-directory parent
        // has no index → None).
        let ino = *self.children.get(&parent_ino)?.by_name.get(name)?;
        Some((ino, self.attr(ino)?))
    }

    /// Canonical store-path attributes for `ino`. Everything is owned
    /// by root, timestamped at epoch+1s, and read-only — the same
    /// normalization the NAR format applies. `st_nlink` is honest:
    /// alias count for files/symlinks, 2 + subdirectories for
    /// directories.
    // r[impl builder.fuse.canonical-metadata+2]
    // r[impl builder.fs.castore-nlink]
    pub fn attr(&self, ino: u64) -> Option<FileAttr> {
        if ino == INodeNo::ROOT.0 {
            return Some(make_attr(
                ino,
                FileType::Directory,
                0,
                0o555,
                self.root_nlink,
            ));
        }
        let (kind, size, perm) = match self.inodes.get(&ino)? {
            Node::Dir { .. } => (FileType::Directory, 0, 0o555),
            Node::File {
                size, executable, ..
            } => (
                FileType::RegularFile,
                *size,
                if *executable { 0o555 } else { 0o444 },
            ),
            Node::Symlink { target } => (FileType::Symlink, target.len() as u64, 0o777),
        };
        let nlink = match kind {
            FileType::Directory => self.children.get(&ino).map_or(2, |c| c.nlink),
            _ => self.nlink.get(&ino).copied().unwrap_or(1),
        };
        Some(make_attr(ino, kind, size, perm, nlink))
    }

    /// The node behind `ino`, for `open()`'s `ino → file_digest`
    /// resolution and `readlink`.
    pub fn node(&self, ino: u64) -> Option<&Node> {
        self.inodes.get(&ino)
    }

    /// Enumerate `ino`'s children starting from `offset` (the kernel's
    /// resume point; 0 on the first call). `None` if `ino` is not a
    /// directory. Emits `.` and `..` at offsets 1 and 2; `..` carries
    /// the parent's ino (per-path inos give every directory exactly
    /// one parent) because fts-based walkers compare it against the
    /// directory they descended from. The mount root's `..` is itself,
    /// the FUSE convention — the kernel resolves the mount boundary.
    pub fn readdir(&self, ino: u64, offset: u64) -> Option<impl Iterator<Item = DirEntry<'_>>> {
        let dir = if ino == INodeNo::ROOT.0 {
            None
        } else {
            // Only directories have a child index; files/symlinks (and
            // unknown inos) fall out here as "not a directory".
            Some(self.children.get(&ino)?)
        };

        let parent_ino = dir.map_or(INodeNo::ROOT.0, |c| c.parent_ino);
        let dots = [
            (ino, FileType::Directory, b".".as_slice()),
            (parent_ino, FileType::Directory, b"..".as_slice()),
        ];

        // Root: enumerate the closure's basenames. Non-root: the
        // precomputed child index. Both are stable across calls, which
        // is all the offset-resume contract needs.
        let root_iter = if ino == INodeNo::ROOT.0 {
            Some(self.roots.iter().map(|(name, &child)| {
                let kind = match self.inodes.get(&child) {
                    Some(Node::Dir { .. }) => FileType::Directory,
                    Some(Node::Symlink { .. }) => FileType::Symlink,
                    _ => FileType::RegularFile,
                };
                (child, kind, name.as_slice())
            }))
        } else {
            None
        };
        let dir_iter = dir.map(|c| {
            c.entries
                .iter()
                .map(|(name, ino, kind)| (*ino, *kind, name.as_slice()))
        });

        Some(
            dots.into_iter()
                .chain(root_iter.into_iter().flatten())
                .chain(dir_iter.into_iter().flatten())
                .enumerate()
                .map(|(i, (ino, kind, name))| DirEntry {
                    ino,
                    next_offset: i as u64 + 1,
                    kind,
                    name,
                })
                .skip(offset as usize),
        )
    }

    /// Number of distinct inodes (excluding the synthetic root).
    pub fn inode_count(&self) -> usize {
        self.inodes.len()
    }
}

fn make_attr(ino: u64, kind: FileType, size: u64, perm: u16, nlink: u32) -> FileAttr {
    let t = UNIX_EPOCH + STORE_MTIME;
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(512),
        atime: t,
        mtime: t,
        ctime: t,
        crtime: t,
        kind,
        perm,
        nlink,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::castore::{DirectoryEntry, FileEntry, SymlinkEntry};

    fn dir_digest_of(d: &Directory) -> [u8; 32] {
        *blake3::hash(&d.encode_to_vec()).as_bytes()
    }

    fn file_entry(name: &[u8], digest: [u8; 32], size: u64, executable: bool) -> FileEntry {
        FileEntry {
            name: name.to_vec(),
            digest: digest.to_vec(),
            size,
            executable,
        }
    }

    fn dir_root(store_path: &str, digest: [u8; 32]) -> (String, RootNode) {
        (
            store_path.to_owned(),
            RootNode {
                node: Some(root_node::Node::DirDigest(digest.to_vec())),
            },
        )
    }

    /// Two store paths sharing one leaf directory, plus a file root and
    /// a symlink root:
    ///
    ///   /nix/store/aaa-pkg/        bin/tool (exec, [1;32])
    ///                              lib/     -> shared leaf
    ///   /nix/store/bbb-dup/        lib/     -> same shared leaf
    ///   /nix/store/ccc-one.patch   regular file root [4;32]
    ///   /nix/store/ddd-link        symlink root -> "aaa-pkg"
    ///
    /// The shared leaf contains the same file digest as bin/tool but
    /// non-executable, so the exec-bit inode split is observable.
    fn sample() -> (Vec<(String, RootNode)>, Vec<Directory>) {
        let leaf = Directory {
            directories: vec![],
            files: vec![
                file_entry(b"libfoo.so", [1u8; 32], 7, false),
                file_entry(b"data", [2u8; 32], 3, false),
            ],
            symlinks: vec![SymlinkEntry {
                name: b"alias".to_vec(),
                target: b"libfoo.so".to_vec(),
            }],
        };
        let leaf_digest = dir_digest_of(&leaf);

        let bin = Directory {
            directories: vec![],
            files: vec![file_entry(b"tool", [1u8; 32], 7, true)],
            symlinks: vec![],
        };
        let bin_digest = dir_digest_of(&bin);

        let pkg = Directory {
            directories: vec![
                DirectoryEntry {
                    name: b"bin".to_vec(),
                    digest: bin_digest.to_vec(),
                    size: 1,
                },
                DirectoryEntry {
                    name: b"lib".to_vec(),
                    digest: leaf_digest.to_vec(),
                    size: 3,
                },
            ],
            files: vec![],
            symlinks: vec![],
        };
        let dup = Directory {
            directories: vec![DirectoryEntry {
                name: b"lib".to_vec(),
                digest: leaf_digest.to_vec(),
                size: 3,
            }],
            files: vec![],
            symlinks: vec![],
        };

        let roots = vec![
            dir_root("/nix/store/aaa-pkg", dir_digest_of(&pkg)),
            dir_root("/nix/store/bbb-dup", dir_digest_of(&dup)),
            (
                "/nix/store/ccc-one.patch".to_owned(),
                RootNode {
                    node: Some(root_node::Node::File(file_entry(b"", [4u8; 32], 11, false))),
                },
            ),
            (
                "/nix/store/ddd-link".to_owned(),
                RootNode {
                    node: Some(root_node::Node::Symlink(SymlinkEntry {
                        name: vec![],
                        target: b"aaa-pkg".to_vec(),
                    })),
                },
            ),
        ];
        (roots, vec![leaf, bin, pkg, dup])
    }

    /// Every (ino, path) reachable from the synthetic root, via the
    /// public readdir/lookup surface (what a tree walker sees).
    fn walk_all_inos(map: &InoMap) -> Vec<(u64, String)> {
        let mut out = vec![(INodeNo::ROOT.0, "/".to_owned())];
        let mut stack = vec![(INodeNo::ROOT.0, String::new())];
        while let Some((dir, prefix)) = stack.pop() {
            // Offset 2 skips `.`/`..` — their inos are already covered
            // as the directory's own entry and its parent's.
            for e in map.readdir(dir, 2).expect("is a dir") {
                let path = format!("{prefix}/{}", e.name.escape_ascii());
                out.push((e.ino, path.clone()));
                if e.kind == FileType::Directory {
                    stack.push((e.ino, path));
                }
            }
        }
        out
    }

    /// THE 32-bit payload guard: every inode number must fit in 32
    /// bits. The production pools advertise i686-linux builds
    /// (xtask eks deploy `systems`), and a non-LFS 32-bit glibc binary
    /// gets EOVERFLOW from stat()/readdir() for any st_ino that does
    /// not fit its 32-bit ino_t. The old digest-derived scheme forced
    /// bit 63, so EVERY ino overflowed.
    // r[verify builder.fs.castore-inode-digest+2]
    #[test]
    fn inos_fit_in_32_bits_for_non_lfs_payloads() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");
        for (ino, path) in walk_all_inos(&map) {
            assert!(
                ino < 1u64 << 32,
                "ino {ino:#x} at {path:?} overflows a 32-bit ino_t"
            );
        }
    }

    /// Allocation is sequential and dense: distinct inos are exactly
    /// FUSE_ROOT_ID+1 ..= FUSE_ROOT_ID+inode_count, in walk order. A
    /// gap or a restart would mean two nodes raced one counter slot or
    /// a dedup class leaked an extra allocation.
    // r[verify builder.fs.castore-inode-digest+2]
    #[test]
    fn allocation_is_sequential_and_dense() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");
        let mut inos: Vec<u64> = walk_all_inos(&map)
            .into_iter()
            .map(|(ino, _)| ino)
            .filter(|&ino| ino != INodeNo::ROOT.0)
            .collect();
        inos.sort_unstable();
        inos.dedup();
        assert_eq!(inos.len(), map.inode_count(), "walk reaches every inode");
        assert_eq!(
            inos.first().copied(),
            Some(INodeNo::ROOT.0 + 1),
            "allocation starts at FUSE_ROOT_ID + 1"
        );
        assert_eq!(
            inos.last().copied(),
            Some(INodeNo::ROOT.0 + map.inode_count() as u64),
            "allocation is dense — the counter never skips"
        );
    }

    /// Symlink inode identity: same target bytes anywhere in the
    /// closure → same inode (with an honest alias count in st_nlink);
    /// different target → distinct inode.
    // r[verify builder.fs.castore-inode-digest+2]
    // r[verify builder.fs.castore-nlink]
    #[test]
    fn symlink_inodes_dedup_by_target_bytes() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        // `alias -> libfoo.so` appears under both lib aliases: one ino.
        let (s1, a1) = lookup_path(&map, &[b"aaa-pkg", b"lib", b"alias"]).unwrap();
        let (s2, a2) = lookup_path(&map, &[b"bbb-dup", b"lib", b"alias"]).unwrap();
        assert_eq!(s1, s2, "identical symlink targets share one inode");
        assert_eq!(a1.nlink, 2, "st_nlink counts both path aliases");
        assert_eq!(a2.nlink, 2);

        // The root symlink has a different target → distinct inode.
        let (root_link, _) = map.lookup(INodeNo::ROOT.0, b"ddd-link").unwrap();
        assert_ne!(s1, root_link, "different targets must not share an inode");
    }

    fn lookup_path(map: &InoMap, path: &[&[u8]]) -> Option<(u64, FileAttr)> {
        let mut cur = INodeNo::ROOT.0;
        let mut last = None;
        for seg in path {
            let hit = map.lookup(cur, seg)?;
            cur = hit.0;
            last = Some(hit);
        }
        last
    }

    /// `lookup(ROOT, basename)` resolves every closure root, descends
    /// into nested directories, and returns the canonical store-path
    /// attrs. A name outside the closure is None (the declared-input
    /// allowlist), and the §2.4 TTL every reply carries is infinite.
    // r[verify builder.fs.castore-dag-source]
    // r[verify builder.fs.castore-cache-config]
    // r[verify builder.fuse.canonical-metadata+2]
    #[test]
    fn lookup_round_trip() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        // Every root basename resolves from ROOT with the right kind.
        let (_, pkg) = map.lookup(INodeNo::ROOT.0, b"aaa-pkg").expect("dir root");
        assert_eq!(pkg.kind, FileType::Directory);
        assert_eq!(pkg.perm, 0o555);
        let (_, patch) = map
            .lookup(INodeNo::ROOT.0, b"ccc-one.patch")
            .expect("file root");
        assert_eq!(patch.kind, FileType::RegularFile);
        assert_eq!(patch.size, 11);
        assert_eq!(patch.perm, 0o444);
        let (link_ino, link) = map
            .lookup(INodeNo::ROOT.0, b"ddd-link")
            .expect("symlink root");
        assert_eq!(link.kind, FileType::Symlink);
        assert_eq!(
            link.perm, 0o777,
            "symlink mode is the Linux-immutable 0o777"
        );
        assert_eq!(
            map.node(link_ino),
            Some(&Node::Symlink {
                target: b"aaa-pkg".to_vec()
            })
        );

        // Descent through nested directories reaches the leaf file with
        // the executable bit reflected in the mode.
        let (_, tool) = lookup_path(&map, &[b"aaa-pkg", b"bin", b"tool"]).expect("nested file");
        assert_eq!(tool.perm, 0o555, "executable file mode");
        assert_eq!(tool.size, 7);
        let (_, lib) = lookup_path(&map, &[b"aaa-pkg", b"lib", b"libfoo.so"]).expect("leaf file");
        assert_eq!(lib.perm, 0o444, "non-executable file mode");

        // Canonical store-path metadata: epoch+1 mtime, root-owned.
        assert_eq!(lib.mtime, UNIX_EPOCH + Duration::from_secs(1));
        assert_eq!(lib.uid, 0);
        assert_eq!(lib.gid, 0);

        // Outside the closure → None → cached negative entry. The
        // closure IS the allowlist; a typo'd dependency must not
        // trigger a fetch.
        assert!(map.lookup(INodeNo::ROOT.0, b"zzz-not-an-input").is_none());
        let (pkg_ino, _) = map.lookup(INodeNo::ROOT.0, b"aaa-pkg").unwrap();
        assert!(map.lookup(pkg_ino, b"share").is_none());

        // The §2.4 contract: every reply TTL is infinite so the kernel
        // never revalidates. A finite TTL here silently reintroduces
        // one upcall per entry per timeout — everything still works,
        // just 100x slower.
        assert_eq!(TTL, Duration::MAX);
    }

    /// File/symlink inode identity (ADR §2.3): same bytes anywhere in
    /// the closure → same inode (one icache entry, one fetch, one page
    /// cache); same bytes but different executable bit → distinct
    /// inodes (st_mode is per-inode in VFS). Directory identity is
    /// per-path — covered by `dir_alias_inodes_are_per_path`.
    // r[verify builder.fs.castore-inode-digest+2]
    #[test]
    fn inode_identity_is_content_deduped() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        let (a, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();

        // Same file digest, same exec bit, different paths → same ino.
        let (f1, _) = lookup_path(&map, &[b"aaa-pkg", b"lib", b"libfoo.so"]).unwrap();
        let (f2, _) = lookup_path(&map, &[b"bbb-dup", b"lib", b"libfoo.so"]).unwrap();
        assert_eq!(f1, f2, "identical files share one inode");

        // Same file digest ([1;32]) but executable → distinct ino.
        let (tool, _) = lookup_path(&map, &[b"aaa-pkg", b"bin", b"tool"]).unwrap();
        assert_ne!(
            f1, tool,
            "same bytes with a different exec bit must be a distinct inode"
        );
        assert_eq!(
            map.node(tool),
            Some(&Node::File {
                file_digest: [1u8; 32],
                size: 7,
                executable: true
            })
        );

        // Allocation starts above FUSE_ROOT_ID → no collision with the
        // synthetic root.
        for ino in [a, f1, tool] {
            assert!(
                ino > INodeNo::ROOT.0,
                "allocated ino {ino:#x} collides with ROOT"
            );
        }
    }

    /// `readdir` enumerates `.`/`..` then the children in a stable
    /// order, resumes from a mid-stream offset without skipping or
    /// duplicating, and reports each entry's kind. `readdirplus` is
    /// this plus `attr()` per entry, which the lookup test covers.
    #[test]
    fn readdir_enumerates_and_resumes() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        let names = |ino, off| -> Vec<(Vec<u8>, FileType, u64)> {
            map.readdir(ino, off)
                .expect("is a dir")
                .map(|e| (e.name.to_vec(), e.kind, e.next_offset))
                .collect()
        };

        // ROOT lists the closure's basenames after the dot entries.
        let root = names(INodeNo::ROOT.0, 0);
        let just_names: Vec<&[u8]> = root.iter().map(|(n, _, _)| n.as_slice()).collect();
        assert_eq!(
            just_names,
            vec![
                b".".as_slice(),
                b"..",
                b"aaa-pkg",
                b"bbb-dup",
                b"ccc-one.patch",
                b"ddd-link"
            ]
        );
        assert_eq!(root[3].1, FileType::Directory);
        assert_eq!(root[4].1, FileType::RegularFile);
        assert_eq!(root[5].1, FileType::Symlink);

        // Resuming from entry N's next_offset yields exactly the
        // entries after N — the kernel re-calls readdir with the last
        // offset it consumed when its buffer fills mid-listing.
        let resume = names(INodeNo::ROOT.0, root[2].2);
        assert_eq!(
            resume
                .iter()
                .map(|(n, _, _)| n.as_slice())
                .collect::<Vec<_>>(),
            vec![b"bbb-dup".as_slice(), b"ccc-one.patch", b"ddd-link"]
        );

        // A nested directory enumerates its Directory body's three
        // lists; a file is not enumerable.
        let (lib_ino, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        let lib = names(lib_ino, 0);
        assert_eq!(
            lib.iter().map(|(n, _, _)| n.as_slice()).collect::<Vec<_>>(),
            vec![b".".as_slice(), b"..", b"libfoo.so", b"data", b"alias"]
        );
        let (file_ino, _) = lookup_path(&map, &[b"aaa-pkg", b"lib", b"data"]).unwrap();
        assert!(map.readdir(file_ino, 0).is_none());
    }

    /// The serve path reads the build-time child index; the node table
    /// (`node()`/`attr()`) is what `open()`/`getattr` serve from. They
    /// must agree for every entry every walker can reach — a drifted
    /// index would let lookup advertise one inode while the node table
    /// serves another (or nothing).
    // r[verify builder.fs.castore-inode-digest+2]
    #[test]
    fn child_index_agrees_with_node_table() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        for (ino, path) in walk_all_inos(&map) {
            // Every advertised ino resolves in the attr/node tables
            // with the kind readdir claimed.
            let attr = map.attr(ino).unwrap_or_else(|| {
                panic!("readdir/lookup advertised {ino:#x} at {path:?} but attr() has no entry")
            });
            if ino == INodeNo::ROOT.0 {
                continue;
            }
            let node = map.node(ino).expect("non-root ino is in the node table");
            let node_kind = match node {
                Node::Dir { .. } => FileType::Directory,
                Node::File { .. } => FileType::RegularFile,
                Node::Symlink { .. } => FileType::Symlink,
            };
            assert_eq!(attr.kind, node_kind, "attr/node kind mismatch at {path:?}");
        }

        // readdir's precomputed inos match lookup's for every entry
        // (offset 2 skips the dot entries, which have no lookup).
        let (lib_ino, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        let (bin_ino, _) = lookup_path(&map, &[b"aaa-pkg", b"bin"]).unwrap();
        for dir in [lib_ino, bin_ino] {
            for e in map.readdir(dir, 2).expect("is a dir") {
                let (ino, _) = map.lookup(dir, e.name).expect("readdir name resolves");
                assert_eq!(ino, e.ino, "readdir/lookup ino mismatch for {:?}", e.name);
            }
        }
    }

    /// Directories must get one inode PER PATH, not per content:
    /// `aaa-pkg/lib` and `bbb-dup/lib` decode to the byte-identical
    /// `Directory` body, but sharing an inode is hardlinked-directory
    /// semantics, which POSIX forbids (link(2) on a directory is
    /// EPERM). With a shared inode the kernel re-parents the one
    /// dentry alias whenever a concurrent path walk takes the other
    /// route, and GNU find's fts — which ascends via openat(fd, "..")
    /// and compares (dev, ino) against the remembered parent —
    /// fabricates ENOENT and aborts with zero failing syscalls.
    /// Files and symlinks keep content-derived dedup (legal hardlink
    /// semantics) with an honest st_nlink alias count so du/tar dedup
    /// by (dev, ino, nlink) keeps working.
    // r[verify builder.fs.castore-inode-digest+2]
    // r[verify builder.fs.castore-nlink]
    #[test]
    fn dir_alias_inodes_are_per_path() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        // The two aliases of the deduped leaf directory: distinct inos.
        let (lib_a, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        let (lib_b, _) = lookup_path(&map, &[b"bbb-dup", b"lib"]).unwrap();
        assert_ne!(
            lib_a, lib_b,
            "two paths to the same Directory body must be distinct inodes \
             (hardlinked directories are illegal)"
        );

        // readdir of each parent advertises that parent's alias ino.
        let readdir_ino = |dir: u64, name: &[u8]| -> u64 {
            map.readdir(dir, 0)
                .expect("is a dir")
                .find(|e| e.name == name)
                .expect("entry listed")
                .ino
        };
        let (aaa, _) = map.lookup(INodeNo::ROOT.0, b"aaa-pkg").unwrap();
        let (bbb, _) = map.lookup(INodeNo::ROOT.0, b"bbb-dup").unwrap();
        assert_eq!(readdir_ino(aaa, b"lib"), lib_a);
        assert_eq!(readdir_ino(bbb, b"lib"), lib_b);

        // A byte-identical FILE under both parents stays ONE inode
        // (legitimate hardlink dedup) and reports both aliases in
        // st_nlink.
        let (f1, a1) = lookup_path(&map, &[b"aaa-pkg", b"lib", b"libfoo.so"]).unwrap();
        let (f2, a2) = lookup_path(&map, &[b"bbb-dup", b"lib", b"libfoo.so"]).unwrap();
        assert_eq!(f1, f2, "identical files share one inode");
        assert_eq!(a1.nlink, 2, "st_nlink counts both path aliases");
        assert_eq!(a2.nlink, 2);

        // A file with a single path keeps nlink 1.
        let (_, tool) = lookup_path(&map, &[b"aaa-pkg", b"bin", b"tool"]).unwrap();
        assert_eq!(tool.nlink, 1, "unaliased file has nlink 1");
    }

    /// readdir's `..` entry must carry the PARENT's inode (and `.` the
    /// directory's own): fts/find compare the d_ino of `..` against
    /// the parent they descended from, so a self-referential `..`
    /// d_ino is disinformation. The per-path inode scheme gives every
    /// directory exactly one parent, so the true parent ino is always
    /// known. The mount root keeps the FUSE convention (its `..` is
    /// itself — the kernel resolves the mount boundary).
    #[test]
    fn readdir_dotdot_reports_parent_ino() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        let dot_inos = |dir: u64| -> (u64, u64) {
            let mut it = map.readdir(dir, 0).expect("is a dir");
            let dot = it.next().expect("`.` first");
            assert_eq!(dot.name, b".");
            let dotdot = it.next().expect("`..` second");
            assert_eq!(dotdot.name, b"..");
            (dot.ino, dotdot.ino)
        };

        // Mount root: both dots are the root itself.
        assert_eq!(
            dot_inos(INodeNo::ROOT.0),
            (INodeNo::ROOT.0, INodeNo::ROOT.0)
        );

        // Store-path root dirs hang under FUSE_ROOT_ID.
        let (aaa, _) = map.lookup(INodeNo::ROOT.0, b"aaa-pkg").unwrap();
        let (bbb, _) = map.lookup(INodeNo::ROOT.0, b"bbb-dup").unwrap();
        assert_eq!(dot_inos(aaa), (aaa, INodeNo::ROOT.0));

        // Each alias of the deduped leaf reports ITS OWN parent.
        let (lib_a, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        let (lib_b, _) = lookup_path(&map, &[b"bbb-dup", b"lib"]).unwrap();
        assert_eq!(dot_inos(lib_a), (lib_a, aaa));
        assert_eq!(dot_inos(lib_b), (lib_b, bbb));
    }

    /// Every readdir entry of every directory alias must resolve via
    /// `lookup(alias_ino, name)` to the SAME ino readdir advertised —
    /// i.e. each alias's enumeration belongs to that alias's subtree.
    #[test]
    fn readdir_alias_children_match_lookup_through_alias() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        let (lib_a, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        let (lib_b, _) = lookup_path(&map, &[b"bbb-dup", b"lib"]).unwrap();
        let (bin, _) = lookup_path(&map, &[b"aaa-pkg", b"bin"]).unwrap();
        let (aaa, _) = map.lookup(INodeNo::ROOT.0, b"aaa-pkg").unwrap();
        let (bbb, _) = map.lookup(INodeNo::ROOT.0, b"bbb-dup").unwrap();
        for dir in [lib_a, lib_b, bin, aaa, bbb] {
            for e in map.readdir(dir, 2).expect("is a dir") {
                let (ino, _) = map.lookup(dir, e.name).expect("readdir name resolves");
                assert_eq!(
                    ino, e.ino,
                    "readdir/lookup ino mismatch under {dir:#x} for {:?}",
                    e.name
                );
            }
        }
    }

    /// Directory st_nlink follows the 2-plus-subdirectories convention
    /// (`.` + parent's entry + one `..` per subdirectory), so fts'
    /// leaf-count optimization sees honest numbers.
    // r[verify builder.fs.castore-nlink]
    #[test]
    fn dir_nlink_is_two_plus_subdir_count() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        // ROOT: two of the four closure roots are directories.
        assert_eq!(map.attr(INodeNo::ROOT.0).unwrap().nlink, 4);
        // aaa-pkg has two subdirectories (bin, lib).
        let (_, aaa) = map.lookup(INodeNo::ROOT.0, b"aaa-pkg").unwrap();
        assert_eq!(aaa.nlink, 4);
        // lib is a leaf directory.
        let (_, lib) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        assert_eq!(lib.nlink, 2);
    }

    /// A DAG that references a directory the server never streamed must
    /// fail tree construction, not silently serve ENOENT for a declared
    /// input subtree (ENOENT is reserved for names outside the
    /// closure).
    #[test]
    fn missing_directory_body_fails_construction() {
        let (roots, mut dirs) = sample();
        // Drop the shared leaf: both aaa-pkg/lib and bbb-dup/lib now
        // dangle.
        dirs.remove(0);
        let Err(err) = InoMap::from_parts(&roots, dirs) else {
            panic!("partial DAG must not build");
        };
        assert!(
            matches!(err, TreeError::MissingDirectory { .. }),
            "got {err:?}"
        );
    }

    /// Two input roots with the same basename would make
    /// `lookup(ROOT, name)` ambiguous — reject the assignment instead
    /// of silently shadowing one with the other.
    #[test]
    fn duplicate_basename_rejected() {
        let (mut roots, dirs) = sample();
        let dup = roots[0].clone();
        roots.push(dup);
        let Err(err) = InoMap::from_parts(&roots, dirs) else {
            panic!("duplicate basename must be rejected");
        };
        assert!(
            matches!(err, TreeError::DuplicateBasename { .. }),
            "got {err:?}"
        );
    }

    /// `DirectoryService` that mirrors the real store's tenant gate
    /// (anonymous callers are refused) and records the request it
    /// served, so the test can assert on the exact wire shape.
    #[derive(Clone)]
    struct RecordingDirectory {
        bodies: Vec<Directory>,
        seen: std::sync::Arc<std::sync::Mutex<Option<GetDirectoryRequest>>>,
    }

    type BoxStream<T> =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<T, tonic::Status>> + Send>>;

    #[tonic::async_trait]
    impl rio_proto::DirectoryService for RecordingDirectory {
        type GetDirectoryStream = BoxStream<Directory>;
        type ReadBlobStream = BoxStream<rio_proto::types::BlobChunk>;

        async fn get_directory(
            &self,
            request: tonic::Request<GetDirectoryRequest>,
        ) -> Result<tonic::Response<Self::GetDirectoryStream>, tonic::Status> {
            if request
                .metadata()
                .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
                .is_none()
            {
                return Err(tonic::Status::unauthenticated("no assignment token"));
            }
            *self.seen.lock().unwrap() = Some(request.into_inner());
            let frames: Vec<Result<Directory, tonic::Status>> =
                self.bodies.iter().cloned().map(Ok).collect();
            Ok(tonic::Response::new(Box::pin(tokio_stream::iter(frames))))
        }

        async fn has_directories(
            &self,
            _: tonic::Request<rio_proto::types::HasDirectoriesRequest>,
        ) -> Result<tonic::Response<rio_proto::types::HasBitmap>, tonic::Status> {
            unimplemented!("not part of the prefetch path")
        }

        async fn has_blobs(
            &self,
            _: tonic::Request<rio_proto::types::HasBlobsRequest>,
        ) -> Result<tonic::Response<rio_proto::types::HasBitmap>, tonic::Status> {
            unimplemented!("not part of the prefetch path")
        }

        async fn read_blob(
            &self,
            _: tonic::Request<rio_proto::types::ReadBlobRequest>,
        ) -> Result<tonic::Response<Self::ReadBlobStream>, tonic::Status> {
            unimplemented!("not part of the prefetch path")
        }

        async fn stat_blob(
            &self,
            _: tonic::Request<rio_proto::types::StatBlobRequest>,
        ) -> Result<tonic::Response<rio_proto::types::StatBlobResponse>, tonic::Status> {
            unimplemented!("not part of the prefetch path")
        }
    }

    /// The mount-time prefetch is exactly one recursive `GetDirectory`
    /// carrying the assignment token and every directory root's digest
    /// (first in `by_what`, rest in `digests`). An anonymous prefetch
    /// must surface the store's UNAUTHENTICATED instead of building a
    /// silently empty tree, and a missing seed would turn a declared
    /// input subtree into ENOENT — both only show up (expensively) in
    /// the VM test otherwise.
    // r[verify builder.fs.castore-dag-source]
    #[tokio::test]
    async fn prefetch_sends_one_authed_multi_root_request() {
        let (roots, dirs) = sample();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let svc = RecordingDirectory {
            bodies: dirs,
            seen: std::sync::Arc::clone(&seen),
        };
        let router = tonic::transport::Server::builder()
            .add_service(rio_proto::DirectoryServiceServer::new(svc));
        let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect_lazy();
        let mut client = DirectoryServiceClient::new(channel);

        let input_roots: Vec<rio_proto::types::InputRoot> = roots
            .iter()
            .map(|(path, node)| rio_proto::types::InputRoot {
                store_path: path.clone(),
                root_node: Some(node.clone()),
            })
            .collect();
        let timeout = Duration::from_secs(5);

        let denied = InoMap::prefetch(&mut client, &input_roots, timeout, "")
            .await
            .err();
        assert!(
            matches!(&denied, Some(TreeError::Rpc(s)) if s.code() == tonic::Code::Unauthenticated),
            "anonymous prefetch must fail with the store's UNAUTHENTICATED, got {denied:?}"
        );

        let map = InoMap::prefetch(&mut client, &input_roots, timeout, "test-token")
            .await
            .expect("authed prefetch");
        // A nested path resolves — the streamed bodies (not just the
        // roots) made it into the tree.
        assert!(lookup_path(&map, &[b"aaa-pkg", b"bin", b"tool"]).is_some());

        let req = seen.lock().unwrap().take().expect("server saw the request");
        assert!(req.recursive, "prefetch must be one recursive walk");
        let mut sent: Vec<Vec<u8>> = req.digests;
        match req.by_what {
            Some(ByWhat::Digest(d)) => sent.push(d),
            None => panic!("by_what must carry the first seed"),
        }
        sent.sort();
        let mut want: Vec<Vec<u8>> = roots
            .iter()
            .filter_map(|(_, node)| match &node.node {
                Some(root_node::Node::DirDigest(d)) => Some(d.clone()),
                _ => None,
            })
            .collect();
        want.sort();
        assert_eq!(
            sent, want,
            "every dir root must be seeded, file/symlink roots must not"
        );
    }
}
