//! Mount-time Directory-DAG prefetch and the content-addressed inode
//! table (ADR-022 §2.2–2.3).
//!
//! The castore-FUSE serves an immutable tree: the closure's store-path
//! basenames under a synthetic root, each expanding into the path's
//! castore Directory DAG. Everything `lookup`/`getattr`/`readdir`/
//! `readlink` need is held in heap, populated by one multi-root
//! `GetDirectory(recursive=true)` call before the mount is announced.
//! Chunk coordinates are NOT prefetched — `open()` resolves them
//! server-side via `ReadBlob`/`StatBlob` keyed on `file_digest` alone.

use std::collections::{BTreeMap, HashMap, HashSet};
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
    File {
        file_digest: [u8; 32],
        size: u64,
        executable: bool,
    },
    Dir {
        dir_digest: [u8; 32],
    },
    Symlink {
        target: Vec<u8>,
    },
}

/// One entry of a `readdir`/`readdirplus` enumeration.
pub struct DirEntry<'a> {
    pub ino: u64,
    /// Offset of the *next* entry — what the kernel passes back to
    /// resume after this one.
    pub next_offset: u64,
    pub kind: FileType,
    pub name: &'a [u8],
}

/// Errors from DAG prefetch / tree construction. All of them mean the
/// build never started — the caller classifies every variant as an
/// infrastructure failure and re-queues.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("GetDirectory: {0}")]
    Rpc(#[from] tonic::Status),
    #[error("GetDirectory(recursive) timed out after {0:?}")]
    Timeout(Duration),
    #[error("input root {store_path:?} has no root_node (path not yet NAR-indexed)")]
    MissingRootNode { store_path: String },
    /// `context` is the input root's store path, or the parent
    /// directory's hex digest when the bad entry is inside a streamed
    /// `Directory` body.
    #[error("castore digest under {context:?} is {got} bytes, want 32")]
    BadDigestLen { context: String, got: usize },
    #[error("store path {store_path:?} has no basename")]
    BadStorePath { store_path: String },
    #[error("duplicate store-path basename {basename:?} in input_roots")]
    DuplicateBasename { basename: String },
    #[error(
        "directory {digest} is referenced by the DAG but was not returned by GetDirectory \
         ({returned} bodies returned)"
    )]
    MissingDirectory { digest: String, returned: usize },
}

/// The castore-FUSE's entire metadata state: content-derived inode →
/// node, per-directory child indexes, and the synthetic root's
/// children. Built once at mount, never mutated.
pub struct InoMap {
    /// ino → node, for every node reachable from the roots. Two files
    /// with identical bytes and executable bit share one entry.
    inodes: HashMap<u64, Node>,
    /// dir ino → its serve-time child index. Precomputed at build so
    /// the metadata hot path (`lookup`/`readdir`, the bulk of a cold
    /// `find` over the closure) never re-derives a child ino (a blake3
    /// hash each) and never scans a child list — both showed up as
    /// ~11% of serve-thread CPU before this index existed.
    children: HashMap<u64, DirChildren>,
    /// `FUSE_ROOT_ID`'s children: store-path basename → child ino.
    /// `BTreeMap` so `readdir(ROOT)` enumerates in a stable order
    /// across calls (the kernel resumes by offset).
    roots: BTreeMap<Vec<u8>, u64>,
}

/// One directory's child index. The decoded `Directory` bodies are
/// dropped after construction — everything the serve path needs is
/// here and in [`InoMap::inodes`].
struct DirChildren {
    /// The `Directory` body's canonical order (directories, files,
    /// symlinks) with each child's derived ino — `readdir`'s stable
    /// enumeration, offset-resumable because the order never changes.
    entries: Vec<(Vec<u8>, u64, FileType)>,
    /// `lookup(parent, name)` probe: name → derived ino.
    by_name: HashMap<Vec<u8>, u64>,
}

// ── Inode derivation (ADR §2.3) ───────────────────────────────────────
//
// `h` = low 63 bits of blake3 with bit 63 set, so every derived ino is
// ≥ 2^63 and can never collide with `FUSE_ROOT_ID` (= 1). The three
// node kinds hash different-length inputs (33 / 32 / 1+n bytes), so a
// file digest can never alias a directory digest. A 63-bit collision
// between two distinct nodes is ~2^-63 per pair over a ~35k-node tree
// and would surface as one file's content served for another's path —
// caught by the build's own output-hash check.

fn h(input: &[u8]) -> u64 {
    let d = *blake3::hash(input).as_bytes();
    let lo = u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    lo | (1 << 63)
}

/// `ino(FileEntry) = h(file_digest ‖ executable)`. The executable bit
/// is part of the key because `st_mode` is per-inode in VFS — two
/// paths sharing an inode share their mode. Both inodes still resolve
/// to the same backing-cache file (keyed by `file_digest` alone), so
/// the split costs one extra `struct inode`, not a second fetch.
// r[impl builder.fs.castore-inode-digest]
pub fn file_ino(file_digest: &[u8; 32], executable: bool) -> u64 {
    let mut buf = [0u8; 33];
    buf[..32].copy_from_slice(file_digest);
    buf[32] = u8::from(executable);
    h(&buf)
}

/// `ino(DirectoryEntry) = h(dir_digest)`. Identical subtrees share one
/// inode and one dcache subtree regardless of which store path led
/// there.
pub fn dir_ino(dir_digest: &[u8; 32]) -> u64 {
    h(dir_digest)
}

/// `ino(SymlinkEntry) = h("l" ‖ target)`. The `"l"` prefix separates
/// the symlink domain from a hypothetical 32-byte target that could
/// otherwise alias a dir digest.
pub fn symlink_ino(target: &[u8]) -> u64 {
    let mut buf = Vec::with_capacity(1 + target.len());
    buf.push(b'l');
    buf.extend_from_slice(target);
    h(&buf)
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
        let mut root_children: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        // Directories whose children still need inodes. Explicit stack:
        // the DAG is content-deduped but can be deep.
        let mut pending: Vec<[u8; 32]> = Vec::new();
        let mut visited: HashSet<[u8; 32]> = HashSet::new();

        let insert_dir = |digest: [u8; 32],
                          inodes: &mut HashMap<u64, Node>,
                          pending: &mut Vec<[u8; 32]>,
                          visited: &mut HashSet<[u8; 32]>|
         -> u64 {
            let ino = dir_ino(&digest);
            inodes.insert(ino, Node::Dir { dir_digest: digest });
            if visited.insert(digest) {
                pending.push(digest);
            }
            ino
        };

        // `ctx` is the error context for a bad digest length: the input
        // root's store path, or the parent directory's hex digest.
        let insert_file =
            |f: &FileEntry, ctx: &str, inodes: &mut HashMap<u64, Node>| -> Result<u64, TreeError> {
                let digest = digest32(&f.digest, ctx)?;
                let ino = file_ino(&digest, f.executable);
                inodes.insert(
                    ino,
                    Node::File {
                        file_digest: digest,
                        size: f.size,
                        executable: f.executable,
                    },
                );
                Ok(ino)
            };

        let insert_symlink = |s: &SymlinkEntry, inodes: &mut HashMap<u64, Node>| -> u64 {
            let ino = symlink_ino(&s.target);
            inodes.insert(
                ino,
                Node::Symlink {
                    target: s.target.clone(),
                },
            );
            ino
        };

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
                    insert_dir(digest, &mut inodes, &mut pending, &mut visited)
                }
                Some(root_node::Node::File(f)) => insert_file(f, store_path, &mut inodes)?,
                Some(root_node::Node::Symlink(s)) => insert_symlink(s, &mut inodes),
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

        let mut children: HashMap<u64, DirChildren> = HashMap::with_capacity(dirs.len());
        while let Some(digest) = pending.pop() {
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
                let child = digest32(&d.digest, &ctx)?;
                let ino = insert_dir(child, &mut inodes, &mut pending, &mut visited);
                entries.push((d.name.clone(), ino, FileType::Directory));
            }
            for f in &dir.files {
                let ino = insert_file(f, &ctx, &mut inodes)?;
                entries.push((f.name.clone(), ino, FileType::RegularFile));
            }
            for s in &dir.symlinks {
                let ino = insert_symlink(s, &mut inodes);
                entries.push((s.name.clone(), ino, FileType::Symlink));
            }
            let by_name = entries
                .iter()
                .map(|(n, ino, _)| (n.clone(), *ino))
                .collect();
            children.insert(dir_ino(&digest), DirChildren { entries, by_name });
        }

        Ok(Self {
            inodes,
            children,
            roots: root_children,
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
        // Two hash probes, no digest re-derivation: the child index was
        // built alongside the inode table, so a hit here is exactly the
        // ino the derivation functions produce (and a non-directory
        // parent has no index → None, same as before).
        let ino = *self.children.get(&parent_ino)?.by_name.get(name)?;
        Some((ino, self.attr(ino)?))
    }

    /// Canonical store-path attributes for `ino`. Everything is owned
    /// by root, timestamped at epoch+1s, and read-only — the same
    /// normalization the NAR format applies.
    // r[impl builder.fuse.canonical-metadata+2]
    pub fn attr(&self, ino: u64) -> Option<FileAttr> {
        if ino == INodeNo::ROOT.0 {
            return Some(make_attr(ino, FileType::Directory, 0, 0o555));
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
        Some(make_attr(ino, kind, size, perm))
    }

    /// The node behind `ino`, for `open()`'s `ino → file_digest`
    /// resolution and `readlink`.
    pub fn node(&self, ino: u64) -> Option<&Node> {
        self.inodes.get(&ino)
    }

    /// Enumerate `ino`'s children starting from `offset` (the kernel's
    /// resume point; 0 on the first call). `None` if `ino` is not a
    /// directory. Emits `.` and `..` at offsets 1 and 2 — a
    /// content-addressed dir has no unique parent, so both point at
    /// the dir itself; the kernel resolves `..` through the dcache and
    /// never through these inos.
    pub fn readdir(&self, ino: u64, offset: u64) -> Option<impl Iterator<Item = DirEntry<'_>>> {
        let dir = if ino == INodeNo::ROOT.0 {
            None
        } else {
            // Only directories have a child index; files/symlinks (and
            // unknown inos) fall out here as "not a directory".
            Some(self.children.get(&ino)?)
        };

        let dots = [
            (ino, FileType::Directory, b".".as_slice()),
            (ino, FileType::Directory, b"..".as_slice()),
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

fn make_attr(ino: u64, kind: FileType, size: u64, perm: u16) -> FileAttr {
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
        nlink: 1,
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

    /// Content-addressed inode identity (ADR §2.3): same bytes anywhere
    /// in the closure → same inode (one icache entry, one fetch, one
    /// page cache); same bytes but different executable bit → distinct
    /// inodes (st_mode is per-inode in VFS); identical directories
    /// reached from different store paths → one shared subtree.
    // r[verify builder.fs.castore-inode-digest]
    #[test]
    fn inode_identity_is_content_derived() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        // aaa-pkg/lib and bbb-dup/lib are the same Directory body →
        // same ino → the dcache subtree is shared.
        let (a, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        let (b, _) = lookup_path(&map, &[b"bbb-dup", b"lib"]).unwrap();
        assert_eq!(a, b, "identical dirs share one inode");

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

        // Every derived ino has bit 63 set → can never collide with
        // FUSE_ROOT_ID.
        for ino in [a, f1, tool] {
            assert!(ino & (1 << 63) != 0, "derived ino {ino:#x} missing bit 63");
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

    /// The serve path reads the build-time child index; the derivation
    /// functions (`file_ino`/`dir_ino`/`symlink_ino`) are the spec
    /// (ADR §2.3). They must agree — a drifted index would let lookup
    /// advertise one inode while `open()`/`getattr` (which go through
    /// the node table) serve another.
    // r[verify builder.fs.castore-inode-digest]
    #[test]
    fn child_index_agrees_with_ino_derivation() {
        let (roots, dirs) = sample();
        let map = InoMap::from_parts(&roots, dirs).expect("build tree");

        let (lib_ino, _) = lookup_path(&map, &[b"aaa-pkg", b"lib"]).unwrap();
        let (bin_ino, _) = lookup_path(&map, &[b"aaa-pkg", b"bin"]).unwrap();
        assert_eq!(
            map.lookup(lib_ino, b"libfoo.so").unwrap().0,
            file_ino(&[1u8; 32], false)
        );
        assert_eq!(
            map.lookup(bin_ino, b"tool").unwrap().0,
            file_ino(&[1u8; 32], true)
        );
        assert_eq!(
            map.lookup(lib_ino, b"alias").unwrap().0,
            symlink_ino(b"libfoo.so")
        );

        // readdir's precomputed inos match lookup's for every entry
        // (offset 2 skips the dot entries, which have no lookup).
        for dir in [lib_ino, bin_ino] {
            for e in map.readdir(dir, 2).expect("is a dir") {
                let (ino, _) = map.lookup(dir, e.name).expect("readdir name resolves");
                assert_eq!(ino, e.ino, "readdir/lookup ino mismatch for {:?}", e.name);
            }
        }
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
