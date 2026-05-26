//! DAG prefetch + content-addressed inode model (ADR-022 §2.2–§2.3).
//!
//! At mount time the builder holds the closure's input store paths and
//! each path's castore [`RootNode`] (from `WorkAssignment.input_roots`,
//! P0588). [`build_tree`] prefetches the full Directory DAG with one
//! multi-root `GetDirectory(recursive=true)` call and assembles an
//! [`InoMap`]: an immutable, content-addressed inode table that answers
//! every metadata op (`lookup`/`getattr`/`readdir`/`readlink`) from
//! heap for the rest of the mount's lifetime.
//!
//! Inode numbers are derived from content, not path: two files anywhere
//! in the closure with the same bytes and the same executable bit share
//! one FUSE inode (one icache entry, one `open()` upcall, one
//! page-cache); two directories with the same `dir_digest` share one
//! dcache subtree. The map is built once and never mutated — FUSE
//! callbacks read it through `&self` with no locking.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use prost::Message;

use rio_proto::castore::{Directory, RootNode, root_node};
use rio_proto::types::{GetDirectoryRequest, get_directory_request};

use crate::store_fetch::StoreClients;

/// Standard 512-byte block size reported in every [`FileAttr`].
const BLOCK_SIZE: u32 = 512;

/// Canonical Nix store-path mtime: one second past the Epoch. Matches
/// `mtimeStore` in Nix's `libstore/posix-fs-canonicalise.cc`. Not 0
/// (some tools treat it as "no timestamp") and never the wall-clock —
/// store paths are immutable and content-addressed.
const STORE_PATH_MTIME: Duration = Duration::from_secs(1);

/// One node in the castore tree, keyed by its content-derived inode.
///
/// Carries exactly the fields the metadata ops need: `getattr` reads
/// `size`/`executable`/`target.len()`, `readlink` reads `target`,
/// `open()` reads `file_digest`/`size`, and `lookup`/`readdir` resolve
/// `dir_digest` against the [`InoMap`]'s directory-body table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Regular file. `executable` is part of the inode key (VFS
    /// `i_mode` is per-inode), but the backing cache is keyed by
    /// `file_digest` alone so both variants share one fetch.
    File {
        file_digest: [u8; 32],
        size: u64,
        executable: bool,
    },
    /// Directory. Children live in the [`Directory`] body at
    /// `InoMap::dirs[dir_digest]`.
    Dir { dir_digest: [u8; 32] },
    /// Symlink. Raw NAR bytes (non-UTF8 legal).
    Symlink { target: Vec<u8> },
}

/// Errors from [`build_tree`].
///
/// Every variant is an infrastructure failure (the build never started;
/// re-queue to a fresh pod) except where noted — none of these can be
/// caused by the derivation under build.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// The `GetDirectory(recursive=true)` call did not complete within
    /// `dag_prefetch_timeout`. Infra-retry.
    #[error("DAG prefetch timed out after {0:?}")]
    PrefetchTimeout(Duration),
    /// The `GetDirectory` RPC failed.
    #[error("GetDirectory: {0}")]
    Rpc(#[from] tonic::Status),
    /// A `DirectoryEntry.digest` referenced by the prefetched DAG (or a
    /// root's `dir_digest`) has no corresponding `Directory` body in
    /// the stream. The store has not finished indexing this path.
    #[error(
        "store returned no Directory body for digest {digest} (referenced by {referrer}) — \
         has the NAR indexer (P0557) processed this path?"
    )]
    MissingDirectory { digest: String, referrer: String },
    /// A root entry in `WorkAssignment.input_roots` has no `node` set.
    #[error("input root {0} has an empty RootNode — scheduler sent an unindexed path")]
    EmptyRootNode(String),
    /// An input root's store path has no usable basename (empty, or
    /// ends in `/`) — there is no name to mount it under.
    #[error("input root {0:?} has no usable basename")]
    BadStorePath(String),
    /// Two input roots share a store-path basename. Store-path basenames
    /// are unique by construction (the hash prefix), so this means the
    /// scheduler sent a malformed closure; accepting it would let one
    /// root silently shadow the other under `lookup(ROOT, name)`.
    #[error("two input roots share the basename {0:?}")]
    DuplicateBasename(String),
    /// A digest field on the wire was not 32 bytes.
    #[error("malformed digest ({len} bytes, want 32) in {context}")]
    BadDigest { len: usize, context: String },
    /// A `Directory` body's child lists are not strictly name-sorted.
    /// `lookup` binary-searches them, so accepting the body would
    /// mis-resolve names (spurious ENOENT mid-build) instead of failing
    /// the mount; the store enforces the canonical sorted encoding at
    /// upload time, so this means a corrupt or non-conforming server.
    #[error("Directory body {digest} has unsorted child lists (non-canonical encoding)")]
    UnsortedDirectory { digest: String },
}

/// Immutable content-addressed inode table for one castore mount.
///
/// Built once by [`build_tree`]; read concurrently by every fuser
/// thread without locking.
pub struct InoMap {
    /// Content-derived inode → node. `FUSE_ROOT_ID` (1) is NOT in this
    /// map — the root is synthetic (see [`InoMap::roots`]).
    inodes: HashMap<u64, Node>,
    /// `dir_digest` → `Directory` body, deduped by digest. Children of
    /// any Dir node are resolved here.
    dirs: HashMap<[u8; 32], Directory>,
    /// The synthetic root's children: store-path basename → child ino.
    /// `BTreeMap` so `readdir(ROOT)` enumerates in a stable (sorted)
    /// order across resumptions.
    roots: BTreeMap<Vec<u8>, u64>,
}

/// Counts only — a chromium-scale map holds ~35k nodes and ~5 MiB of
/// `Directory` bodies; dumping them in a panic message helps nobody.
impl std::fmt::Debug for InoMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InoMap")
            .field("inodes", &self.inodes.len())
            .field("dirs", &self.dirs.len())
            .field("roots", &self.roots.len())
            .finish()
    }
}

/// Derive a content-addressed inode: low 63 bits of `blake3(key)` with
/// bit 63 set (ADR-022 §2.3). Setting bit 63 guarantees `ino ≥ 2^63`,
/// which can never collide with `FUSE_ROOT_ID` (1). Collisions between
/// two distinct keys are 2⁻⁶³ per pair over a ~35k-node tree —
/// negligible, and a collision manifests as one file's content served
/// for another's path, caught by the build's own output-hash check.
// r[impl builder.fs.castore-inode-digest]
fn ino_of(key: &[u8]) -> u64 {
    let hash = blake3::hash(key);
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(first) | (1 << 63)
}

/// Inode for a regular file: `h(file_digest ‖ executable)`. The
/// executable bit is part of the key because `i_mode` is per-inode in
/// VFS — same-bytes/different-exec must be distinct inodes. Both still
/// resolve to the same `cache/ab/<file_digest>` backing file.
pub fn file_ino(file_digest: &[u8; 32], executable: bool) -> u64 {
    let mut key = [0u8; 33];
    key[..32].copy_from_slice(file_digest);
    key[32] = u8::from(executable);
    ino_of(&key)
}

/// Inode for a directory: `h(dir_digest)`.
pub fn dir_ino(dir_digest: &[u8; 32]) -> u64 {
    ino_of(dir_digest)
}

/// Inode for a symlink: `h("l" ‖ target)`. The `"l"` prefix domain-
/// separates a symlink whose target happens to be 32 bytes from a
/// directory with that `dir_digest`.
pub fn symlink_ino(target: &[u8]) -> u64 {
    let mut key = Vec::with_capacity(1 + target.len());
    key.push(b'l');
    key.extend_from_slice(target);
    ino_of(&key)
}

/// `&[u8] → [u8; 32]` with a typed error naming the field that was
/// malformed. Wire digests come from prost `bytes` fields, which carry
/// no length guarantee.
fn digest32(bytes: &[u8], context: &str) -> Result<[u8; 32], TreeError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| TreeError::BadDigest {
        len: bytes.len(),
        context: context.to_string(),
    })
}

/// Convert a wire [`RootNode`] (or a child entry) into a [`Node`] and
/// its content-derived inode.
fn node_of_root(store_path: &str, root: &RootNode) -> Result<(u64, Node), TreeError> {
    let inner = root
        .node
        .as_ref()
        .ok_or_else(|| TreeError::EmptyRootNode(store_path.to_string()))?;
    Ok(match inner {
        root_node::Node::DirDigest(d) => {
            let dir_digest = digest32(d, &format!("{store_path} root dir_digest"))?;
            (dir_ino(&dir_digest), Node::Dir { dir_digest })
        }
        root_node::Node::File(f) => {
            let file_digest = digest32(&f.digest, &format!("{store_path} root file digest"))?;
            (
                file_ino(&file_digest, f.executable),
                Node::File {
                    file_digest,
                    size: f.size,
                    executable: f.executable,
                },
            )
        }
        root_node::Node::Symlink(s) => (
            symlink_ino(&s.target),
            Node::Symlink {
                target: s.target.to_vec(),
            },
        ),
    })
}

impl InoMap {
    /// Assemble the inode table from prefetched parts. Pure — no I/O.
    /// [`build_tree`] is the production caller; tests drive this
    /// directly with fixture `Directory` values.
    ///
    /// Validates that every `dir_digest` reachable from the roots has a
    /// body in `dirs` — a missing body would otherwise surface as a
    /// confusing ENOENT mid-build instead of a mount-time error.
    pub fn assemble(
        roots: &[(String, RootNode)],
        dirs: HashMap<[u8; 32], Directory>,
    ) -> Result<Self, TreeError> {
        let mut inodes: HashMap<u64, Node> = HashMap::new();
        let mut root_children = BTreeMap::new();

        for (store_path, root) in roots {
            let (ino, node) = node_of_root(store_path, root)?;
            if let Node::Dir { dir_digest } = &node
                && !dirs.contains_key(dir_digest)
            {
                return Err(TreeError::MissingDirectory {
                    digest: hex::encode(dir_digest),
                    referrer: store_path.clone(),
                });
            }
            inodes.insert(ino, node);
            // The synthetic root's children are keyed by store-path
            // BASENAME — that is the name a build resolves under
            // /nix/store/. Accept either a full /nix/store/<base> path
            // or a bare basename so callers don't have to care. An
            // empty basename has nothing to mount under; a duplicate
            // would make lookup(ROOT, name) ambiguous (one root silently
            // shadowing another) — both are typed mount-time errors.
            let basename = store_path
                .rsplit('/')
                .next()
                .filter(|b| !b.is_empty())
                .ok_or_else(|| TreeError::BadStorePath(store_path.clone()))?;
            if root_children
                .insert(basename.as_bytes().to_vec(), ino)
                .is_some()
            {
                return Err(TreeError::DuplicateBasename(basename.to_string()));
            }
        }

        // Walk every Directory body and register each child's inode.
        // Iteration order doesn't matter: inserting the same
        // content-derived ino twice writes the same Node both times.
        for (digest, dir) in &dirs {
            // `lookup` binary-searches the three child lists, trusting
            // the canonical-encoding sort order the store enforces at
            // upload time. Verify it once per body at mount time so a
            // corrupt or non-conforming server fails the mount loudly
            // instead of mis-resolving names mid-build.
            let sorted = dir.directories.is_sorted_by(|a, b| a.name < b.name)
                && dir.files.is_sorted_by(|a, b| a.name < b.name)
                && dir.symlinks.is_sorted_by(|a, b| a.name < b.name);
            if !sorted {
                return Err(TreeError::UnsortedDirectory {
                    digest: hex::encode(digest),
                });
            }
            for d in &dir.directories {
                let child = digest32(&d.digest, "DirectoryEntry.digest")?;
                if !dirs.contains_key(&child) {
                    return Err(TreeError::MissingDirectory {
                        digest: hex::encode(child),
                        referrer: hex::encode(digest),
                    });
                }
                inodes.insert(dir_ino(&child), Node::Dir { dir_digest: child });
            }
            for f in &dir.files {
                let file_digest = digest32(&f.digest, "FileEntry.digest")?;
                inodes.insert(
                    file_ino(&file_digest, f.executable),
                    Node::File {
                        file_digest,
                        size: f.size,
                        executable: f.executable,
                    },
                );
            }
            for s in &dir.symlinks {
                inodes.insert(
                    symlink_ino(&s.target),
                    Node::Symlink {
                        target: s.target.to_vec(),
                    },
                );
            }
        }

        Ok(Self {
            inodes,
            dirs,
            roots: root_children,
        })
    }

    /// Resolve an inode to its node. `None` for unknown inodes and for
    /// the synthetic root (which has no [`Node`] — use [`Self::attr`]).
    pub fn node(&self, ino: u64) -> Option<&Node> {
        self.inodes.get(&ino)
    }

    /// Number of distinct (deduplicated) inodes, excluding the root.
    pub fn len(&self) -> usize {
        self.inodes.len()
    }

    /// `true` if the closure is empty (no input roots).
    ///
    /// Deliberately keyed on `roots`, not `len() == 0`: [`Self::assemble`]
    /// accepts `Directory` bodies that no root reaches (their children
    /// still get inodes, so `len() > 0`), but with no roots the mount
    /// presents an empty tree — which is what callers gating on
    /// "anything to mount?" care about.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Look up `name` under `parent_ino`. Reads the parent's
    /// `Directory` body (already in heap), finds the child by name,
    /// returns its content-derived inode and attributes. `None` means
    /// ENOENT — the name is outside the prefetched DAG (the
    /// declared-input allowlist).
    // r[impl builder.fs.castore-dag-source]
    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Option<(u64, FileAttr)> {
        if parent_ino == INodeNo::ROOT.0 {
            let ino = *self.roots.get(name)?;
            return Some((ino, self.attr(ino)?));
        }
        let Node::Dir { dir_digest } = self.inodes.get(&parent_ino)? else {
            return None;
        };
        let dir = self.dirs.get(dir_digest)?;

        // The three child lists are each sorted byte-lex by name
        // (`r[store.castore.canonical-encoding]`), so binary search
        // each. With ttl=MAX the kernel asks for each (parent, name)
        // exactly once per mount — this path is cold by design.
        if let Ok(i) = dir
            .directories
            .binary_search_by(|e| e.name.as_slice().cmp(name))
        {
            let child = digest32(&dir.directories[i].digest, "DirectoryEntry.digest").ok()?;
            let ino = dir_ino(&child);
            return Some((ino, self.attr(ino)?));
        }
        if let Ok(i) = dir.files.binary_search_by(|e| e.name.as_slice().cmp(name)) {
            let f = &dir.files[i];
            let file_digest = digest32(&f.digest, "FileEntry.digest").ok()?;
            let ino = file_ino(&file_digest, f.executable);
            return Some((ino, self.attr(ino)?));
        }
        if let Ok(i) = dir
            .symlinks
            .binary_search_by(|e| e.name.as_slice().cmp(name))
        {
            let ino = symlink_ino(&dir.symlinks[i].target);
            return Some((ino, self.attr(ino)?));
        }
        None
    }

    /// Canonical store-path attributes for `ino` (mode
    /// `0o40555`/`0o100555`/`0o100444`/`0o120777`, `mtime=1`,
    /// `uid=gid=0`). `None` for unknown inodes.
    pub fn attr(&self, ino: u64) -> Option<FileAttr> {
        if ino == INodeNo::ROOT.0 {
            return Some(make_attr(ino, FileType::Directory, 0, 0o555));
        }
        Some(match self.inodes.get(&ino)? {
            Node::Dir { .. } => make_attr(ino, FileType::Directory, 0, 0o555),
            Node::File {
                size, executable, ..
            } => make_attr(
                ino,
                FileType::RegularFile,
                *size,
                if *executable { 0o555 } else { 0o444 },
            ),
            Node::Symlink { target } => {
                make_attr(ino, FileType::Symlink, target.len() as u64, 0o777)
            }
        })
    }

    /// Enumerate the children of `ino` for `readdir`/`readdirplus`.
    ///
    /// Returns `None` if `ino` is not a directory. The enumeration
    /// order is stable across calls (root: basename-sorted; others:
    /// the `Directory` body's list order — directories, then files,
    /// then symlinks, each name-sorted) so the kernel's resume offset
    /// always identifies the same position. `.`/`..` are NOT included
    /// — the caller emits them at offsets 1 and 2.
    pub fn children(&self, ino: u64) -> Option<Vec<(u64, FileType, &[u8])>> {
        if ino == INodeNo::ROOT.0 {
            return Some(
                self.roots
                    .iter()
                    .map(|(name, &child)| {
                        let kind = match self.inodes.get(&child) {
                            Some(Node::Dir { .. }) => FileType::Directory,
                            Some(Node::Symlink { .. }) => FileType::Symlink,
                            // Unknown is unreachable (assemble() inserts
                            // every root); RegularFile is a safe default.
                            _ => FileType::RegularFile,
                        };
                        (child, kind, name.as_slice())
                    })
                    .collect(),
            );
        }
        let Node::Dir { dir_digest } = self.inodes.get(&ino)? else {
            return None;
        };
        let dir = self.dirs.get(dir_digest)?;
        let mut out =
            Vec::with_capacity(dir.directories.len() + dir.files.len() + dir.symlinks.len());
        for d in &dir.directories {
            // assemble() validated every digest; a malformed one here
            // would already have failed the mount.
            let Ok(child) = digest32(&d.digest, "DirectoryEntry.digest") else {
                continue;
            };
            out.push((dir_ino(&child), FileType::Directory, d.name.as_slice()));
        }
        for f in &dir.files {
            let Ok(child) = digest32(&f.digest, "FileEntry.digest") else {
                continue;
            };
            out.push((
                file_ino(&child, f.executable),
                FileType::RegularFile,
                f.name.as_slice(),
            ));
        }
        for s in &dir.symlinks {
            out.push((symlink_ino(&s.target), FileType::Symlink, s.name.as_slice()));
        }
        Some(out)
    }

    /// Symlink target for `readlink`. `None` if `ino` is not a symlink.
    pub fn readlink(&self, ino: u64) -> Option<&[u8]> {
        match self.inodes.get(&ino)? {
            Node::Symlink { target } => Some(target.as_slice()),
            _ => None,
        }
    }
}

/// Build a canonical store-path [`FileAttr`].
fn make_attr(ino: u64, kind: FileType, size: u64, perm: u16) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(u64::from(BLOCK_SIZE)),
        atime: UNIX_EPOCH + STORE_PATH_MTIME,
        mtime: UNIX_EPOCH + STORE_PATH_MTIME,
        ctime: UNIX_EPOCH + STORE_PATH_MTIME,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

/// Prefetch the closure's Directory DAG and assemble the inode table.
///
/// One `GetDirectory(recursive=true)` call seeded with ALL `Dir` roots'
/// digests in field 3 (the multi-root extension) — one RPC for the
/// whole closure instead of one per root (the I-110 PG-wall lesson).
/// The whole call is wrapped in `timeout(dag_prefetch_timeout)`; expiry
/// is an infrastructure retry, not a build failure (the build never
/// started, no partial state).
///
/// Streamed `Directory` bodies are keyed by
/// `blake3(canonical_encode(body))` — the server sends canonical
/// encodings (`r[store.castore.canonical-encoding]`), so re-encoding
/// the decoded message reproduces the digest. Duplicate bodies (the
/// server already dedupes, but a malicious/buggy server might not) are
/// idempotent inserts.
// r[impl builder.fs.castore-dag-source]
pub async fn build_tree(
    store: &StoreClients,
    roots: &[(String, RootNode)],
    dag_prefetch_timeout: Duration,
    assignment_token: &str,
) -> Result<InoMap, TreeError> {
    let started = std::time::Instant::now();

    // Collect every Dir root's digest. File/symlink roots have no
    // Directory body to fetch.
    let mut dir_roots: Vec<Vec<u8>> = Vec::new();
    for (store_path, root) in roots {
        if let Some(root_node::Node::DirDigest(d)) = &root.node {
            // Validate eagerly so a malformed digest fails the mount
            // with a named path instead of a server-side decode error.
            digest32(d, &format!("{store_path} root dir_digest"))?;
            dir_roots.push(d.clone());
        }
    }

    let mut dirs: HashMap<[u8; 32], Directory> = HashMap::new();
    if let Some((first, rest)) = dir_roots.split_first() {
        // The assignment token is how rio-store's tenant gate
        // authenticates this build's castore reads — without it the
        // call is rejected before any Directory body is streamed.
        let req = crate::store_fetch::authed_request(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(first.clone())),
                recursive: true,
                digests: rest.to_vec(),
            },
            assignment_token,
        )?;
        let mut client = store.directory.clone();
        let fetch = async {
            let mut stream = client.get_directory(req).await?.into_inner();
            while let Some(body) = stream.message().await? {
                let digest = *blake3::hash(&body.encode_to_vec()).as_bytes();
                dirs.insert(digest, body);
            }
            Ok::<_, tonic::Status>(())
        };
        tokio::time::timeout(dag_prefetch_timeout, fetch)
            .await
            .map_err(|_| TreeError::PrefetchTimeout(dag_prefetch_timeout))??;
    }

    let map = InoMap::assemble(roots, dirs)?;
    metrics::histogram!("rio_builder_castore_dag_prefetch_seconds")
        .record(started.elapsed().as_secs_f64());
    tracing::info!(
        roots = roots.len(),
        directories = map.dirs.len(),
        inodes = map.len(),
        elapsed = ?started.elapsed(),
        "castore DAG prefetched"
    );
    Ok(map)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use rio_proto::castore::{DirectoryEntry, FileEntry, SymlinkEntry};

    /// Canonical digest of a Directory body — the same computation
    /// `build_tree` applies to each streamed message and the upload
    /// path applies when minting `dir_digest`s.
    pub(in crate::castore_fuse) fn dir_digest_of(dir: &Directory) -> [u8; 32] {
        *blake3::hash(&dir.encode_to_vec()).as_bytes()
    }

    /// A fixture DAG exercising every node kind plus content dedup:
    ///
    /// ```text
    /// aaaa-hello/              dir  (digest HELLO_ROOT)
    ///   bin/                   dir  (digest HELLO_BIN)
    ///     hello                file (HELLO_BLOB, exec)
    ///   share/                 dir  (digest HELLO_SHARE)
    ///     copy.txt             file (DOC_BLOB)      ── same bytes,
    ///     doc.txt              file (DOC_BLOB)      ── same inode
    ///     link                 symlink → ../bin/hello
    /// bbbb-script              file root (SCRIPT_BLOB, exec)
    /// cccc-link                symlink root → aaaa-hello/bin/hello
    /// ```
    pub(in crate::castore_fuse) struct Fixture {
        pub roots: Vec<(String, RootNode)>,
        pub dirs: HashMap<[u8; 32], Directory>,
        pub hello_root: [u8; 32],
        pub hello_bin: [u8; 32],
        pub hello_share: [u8; 32],
        pub hello_blob: [u8; 32],
        pub doc_blob: [u8; 32],
        pub script_blob: [u8; 32],
    }

    pub(in crate::castore_fuse) fn fixture() -> Fixture {
        let hello_blob = *blake3::hash(b"#!/bin/sh\necho hello\n").as_bytes();
        let doc_blob = *blake3::hash(b"documentation\n").as_bytes();
        let script_blob = *blake3::hash(b"standalone script").as_bytes();

        let bin = Directory {
            directories: vec![],
            files: vec![FileEntry {
                name: b"hello".to_vec(),
                digest: hello_blob.to_vec(),
                size: 21,
                executable: true,
            }],
            symlinks: vec![],
        };
        let hello_bin = dir_digest_of(&bin);

        // Lists must be sorted byte-lex by name (the canonical
        // encoding rule) — lookup binary-searches them.
        let share = Directory {
            directories: vec![],
            files: vec![
                FileEntry {
                    name: b"copy.txt".to_vec(),
                    digest: doc_blob.to_vec(),
                    size: 14,
                    executable: false,
                },
                FileEntry {
                    name: b"doc.txt".to_vec(),
                    digest: doc_blob.to_vec(),
                    size: 14,
                    executable: false,
                },
            ],
            symlinks: vec![SymlinkEntry {
                name: b"link".to_vec(),
                target: b"../bin/hello".to_vec(),
            }],
        };
        let hello_share = dir_digest_of(&share);

        let root = Directory {
            directories: vec![
                DirectoryEntry {
                    name: b"bin".to_vec(),
                    digest: hello_bin.to_vec(),
                    size: 1,
                },
                DirectoryEntry {
                    name: b"share".to_vec(),
                    digest: hello_share.to_vec(),
                    size: 3,
                },
            ],
            files: vec![],
            symlinks: vec![],
        };
        let hello_root = dir_digest_of(&root);

        let dirs = HashMap::from([(hello_root, root), (hello_bin, bin), (hello_share, share)]);
        let roots = vec![
            (
                "/nix/store/aaaa-hello".to_string(),
                RootNode {
                    node: Some(root_node::Node::DirDigest(hello_root.to_vec())),
                },
            ),
            (
                "/nix/store/bbbb-script".to_string(),
                RootNode {
                    node: Some(root_node::Node::File(FileEntry {
                        name: vec![],
                        digest: script_blob.to_vec(),
                        size: 17,
                        executable: true,
                    })),
                },
            ),
            (
                "/nix/store/cccc-link".to_string(),
                RootNode {
                    node: Some(root_node::Node::Symlink(SymlinkEntry {
                        name: vec![],
                        target: b"aaaa-hello/bin/hello".to_vec(),
                    })),
                },
            ),
        ];
        Fixture {
            roots,
            dirs,
            hello_root,
            hello_bin,
            hello_share,
            hello_blob,
            doc_blob,
            script_blob,
        }
    }

    pub(in crate::castore_fuse) fn assembled() -> (Fixture, InoMap) {
        let fx = fixture();
        let map = InoMap::assemble(&fx.roots, fx.dirs.clone()).expect("assemble fixture");
        (fx, map)
    }

    /// `lookup(ROOT, basename)` resolves every root kind to its
    /// content-derived inode and canonical attrs, and an unknown name
    /// is `None` (→ a cached negative dentry).
    // r[verify builder.fs.castore-dag-source]
    #[test]
    fn lookup_root_basename_roundtrip() {
        let (fx, map) = assembled();

        let (dir_inode, dir_attr) = map
            .lookup(INodeNo::ROOT.0, b"aaaa-hello")
            .expect("dir root");
        assert_eq!(dir_inode, dir_ino(&fx.hello_root));
        assert_eq!(dir_attr.kind, FileType::Directory);
        assert_eq!(dir_attr.perm, 0o555);

        let (file_inode, file_attr) = map
            .lookup(INodeNo::ROOT.0, b"bbbb-script")
            .expect("file root");
        assert_eq!(file_inode, file_ino(&fx.script_blob, true));
        assert_eq!(file_attr.kind, FileType::RegularFile);
        assert_eq!(file_attr.perm, 0o555, "executable file");
        assert_eq!(file_attr.size, 17);

        let (link_inode, link_attr) = map
            .lookup(INodeNo::ROOT.0, b"cccc-link")
            .expect("symlink root");
        assert_eq!(link_inode, symlink_ino(b"aaaa-hello/bin/hello"));
        assert_eq!(link_attr.kind, FileType::Symlink);
        assert_eq!(link_attr.perm, 0o777);
        assert_eq!(
            map.readlink(link_inode).expect("readlink"),
            b"aaaa-hello/bin/hello"
        );

        assert!(
            map.lookup(INodeNo::ROOT.0, b"dddd-not-an-input").is_none(),
            "names outside the closure are ENOENT (the declared-input allowlist)"
        );
    }

    /// Descending the DAG: each lookup reads the parent's Directory
    /// body and returns the child's content-derived inode.
    // r[verify builder.fs.castore-dag-source]
    #[test]
    fn lookup_descends_the_dag() {
        let (fx, map) = assembled();
        let (root_ino, _) = map.lookup(INodeNo::ROOT.0, b"aaaa-hello").unwrap();
        let (bin_inode, _) = map.lookup(root_ino, b"bin").expect("bin");
        assert_eq!(bin_inode, dir_ino(&fx.hello_bin));
        let (hello_inode, hello_attr) = map.lookup(bin_inode, b"hello").expect("hello");
        assert_eq!(hello_inode, file_ino(&fx.hello_blob, true));
        assert_eq!(hello_attr.size, 21);
        assert_eq!(hello_attr.perm, 0o555);
        assert!(
            map.lookup(bin_inode, b"nonexistent").is_none(),
            "unknown child of a real directory is None"
        );
        assert!(
            map.lookup(hello_inode, b"anything").is_none(),
            "lookup under a non-directory is None"
        );
    }

    /// Two paths with identical content (same `file_digest`, same
    /// executable bit) share one FUSE inode: one icache entry, one
    /// `open()` upcall, one page-cache. Same bytes with a different
    /// executable bit must NOT share (st_mode is per-inode).
    // r[verify builder.fs.castore-inode-digest]
    #[test]
    fn identical_content_shares_an_inode() {
        let (fx, map) = assembled();
        let share_ino = dir_ino(&fx.hello_share);
        let (a, _) = map.lookup(share_ino, b"doc.txt").expect("doc.txt");
        let (b, _) = map.lookup(share_ino, b"copy.txt").expect("copy.txt");
        assert_eq!(a, b, "same content + same exec bit → same inode");
        assert_eq!(a, file_ino(&fx.doc_blob, false));

        assert_ne!(
            file_ino(&fx.doc_blob, false),
            file_ino(&fx.doc_blob, true),
            "same content + different exec bit → different inode"
        );
    }

    /// The inode derivation contract: bit 63 always set (so no
    /// collision with FUSE_ROOT_ID=1), deterministic, and
    /// domain-separated between node kinds.
    // r[verify builder.fs.castore-inode-digest]
    #[test]
    fn ino_derivation_is_content_addressed() {
        let d = [0x42u8; 32];
        for ino in [
            file_ino(&d, false),
            file_ino(&d, true),
            dir_ino(&d),
            symlink_ino(&d),
        ] {
            assert!(ino & (1 << 63) != 0, "bit 63 set: {ino:#x}");
            assert!(ino > 1, "never collides with FUSE_ROOT_ID");
        }
        assert_eq!(dir_ino(&d), dir_ino(&d), "deterministic");
        assert_ne!(
            dir_ino(&d),
            symlink_ino(&d),
            "a symlink whose target is these 32 bytes is not the directory with this digest"
        );
        assert_ne!(dir_ino(&d), file_ino(&d, false));
    }

    /// `children()` (the readdir/readdirplus source) enumerates every
    /// entry exactly once in a stable order, with the kind and
    /// content-derived inode the dcache will be populated with.
    #[test]
    fn children_enumeration_is_stable_and_complete() {
        let (fx, map) = assembled();

        let root_children = map.children(INodeNo::ROOT.0).expect("root children");
        assert_eq!(
            root_children
                .iter()
                .map(|(_, _, name)| *name)
                .collect::<Vec<_>>(),
            vec![&b"aaaa-hello"[..], b"bbbb-script", b"cccc-link"],
            "root enumerates the closure by basename, sorted"
        );

        let share = map
            .children(dir_ino(&fx.hello_share))
            .expect("share children");
        assert_eq!(share.len(), 3);
        assert_eq!(
            share
                .iter()
                .map(|(_, kind, name)| (*kind, *name))
                .collect::<Vec<_>>(),
            vec![
                (FileType::RegularFile, &b"copy.txt"[..]),
                (FileType::RegularFile, b"doc.txt"),
                (FileType::Symlink, b"link"),
            ]
        );
        // Stable across calls (the kernel resumes readdir by offset).
        assert_eq!(map.children(dir_ino(&fx.hello_share)).unwrap(), share);

        assert!(
            map.children(file_ino(&fx.hello_blob, true)).is_none(),
            "children() of a file is None → ENOTDIR"
        );
    }

    /// Canonical store-path metadata: mode
    /// 0o40555/0o100555/0o100444/0o120777, mtime=1, uid/gid=0. The
    /// FUSE FS *is* the chroot store's lower layer — leaking anything
    /// else (wall-clock mtimes, the builder's uid) breaks
    /// reproducibility downstream.
    #[test]
    fn attr_modes_match_canonical_store_metadata() {
        let (fx, map) = assembled();
        let canon = UNIX_EPOCH + Duration::from_secs(1);

        let root_attr = map.attr(INodeNo::ROOT.0).expect("synthetic root attr");
        assert_eq!(root_attr.kind, FileType::Directory);
        assert_eq!(root_attr.perm, 0o555);

        for (ino, kind, perm) in [
            (dir_ino(&fx.hello_bin), FileType::Directory, 0o555),
            (file_ino(&fx.hello_blob, true), FileType::RegularFile, 0o555),
            (file_ino(&fx.doc_blob, false), FileType::RegularFile, 0o444),
            (symlink_ino(b"../bin/hello"), FileType::Symlink, 0o777),
        ] {
            let attr = map.attr(ino).unwrap_or_else(|| panic!("attr({ino:#x})"));
            assert_eq!(attr.kind, kind);
            assert_eq!(attr.perm, perm);
            assert_eq!(attr.mtime, canon, "mtime is 1s past the epoch");
            assert_eq!(attr.uid, 0);
            assert_eq!(attr.gid, 0);
        }
        assert_eq!(
            map.attr(symlink_ino(b"../bin/hello")).unwrap().size,
            b"../bin/hello".len() as u64,
            "symlink size is the target length"
        );
        assert!(map.attr(0xDEAD).is_none(), "unknown ino has no attr");
    }

    /// A DAG that references a Directory body the stream never
    /// delivered must fail the mount, not surface as a confusing
    /// ENOENT mid-build.
    #[test]
    fn assemble_rejects_a_missing_directory_body() {
        let fx = fixture();
        let mut dirs = fx.dirs.clone();
        dirs.remove(&fx.hello_bin);
        let err = InoMap::assemble(&fx.roots, dirs).expect_err("missing body");
        assert!(
            matches!(err, TreeError::MissingDirectory { .. }),
            "got {err:?}"
        );

        // A root pointing at a digest with no body at all.
        let err = InoMap::assemble(&fx.roots, HashMap::new()).expect_err("no bodies");
        assert!(matches!(err, TreeError::MissingDirectory { .. }));
    }

    /// A Directory body whose child list is not byte-lex name-sorted is
    /// rejected at mount time: `lookup` binary-searches the lists, so
    /// accepting a non-canonical body would mis-resolve names instead
    /// of failing loudly.
    #[test]
    fn assemble_rejects_unsorted_directory_body() {
        let fx = fixture();
        let mut dirs = fx.dirs.clone();
        dirs.get_mut(&fx.hello_share)
            .expect("fixture has the share body")
            .files
            .reverse(); // copy.txt / doc.txt now out of order
        let err = InoMap::assemble(&fx.roots, dirs).expect_err("unsorted body");
        assert!(
            matches!(err, TreeError::UnsortedDirectory { .. }),
            "got {err:?}"
        );
    }

    /// Two input roots with the same store-path basename would make
    /// `lookup(ROOT, name)` ambiguous — one root would silently shadow
    /// the other. Reject the closure at mount time instead.
    #[test]
    fn assemble_rejects_duplicate_root_basenames() {
        let fx = fixture();
        let mut roots = fx.roots.clone();
        // The same basename as an existing file root, under a different
        // path prefix (so only the basename collides).
        roots.push((
            "/other/prefix/bbbb-script".to_string(),
            RootNode {
                node: Some(root_node::Node::Symlink(SymlinkEntry {
                    name: vec![],
                    target: b"elsewhere".to_vec(),
                })),
            },
        ));
        let err = InoMap::assemble(&roots, fx.dirs.clone()).expect_err("duplicate basename");
        assert!(
            matches!(err, TreeError::DuplicateBasename(ref b) if b == "bbbb-script"),
            "got {err:?}"
        );
    }

    /// A store path with no usable basename (empty, or ending in `/`)
    /// has nothing to mount the root under — a typed error, not a
    /// silently empty name in the root directory.
    #[test]
    fn assemble_rejects_an_empty_root_basename() {
        for bad in ["", "/nix/store/"] {
            let err = InoMap::assemble(
                &[(
                    bad.to_string(),
                    RootNode {
                        node: Some(root_node::Node::Symlink(SymlinkEntry {
                            name: vec![],
                            target: b"x".to_vec(),
                        })),
                    },
                )],
                HashMap::new(),
            )
            .expect_err("empty basename");
            assert!(
                matches!(err, TreeError::BadStorePath(ref p) if p == bad),
                "got {err:?} for {bad:?}"
            );
        }
    }

    /// An input root with no `node` set (the scheduler dispatched an
    /// unindexed path) is a named error, not a panic or a silent skip.
    #[test]
    fn assemble_rejects_an_empty_root_node() {
        let err = InoMap::assemble(
            &[("/nix/store/eeee-empty".to_string(), RootNode { node: None })],
            HashMap::new(),
        )
        .expect_err("empty root node");
        assert!(matches!(err, TreeError::EmptyRootNode(p) if p.contains("eeee-empty")));
    }

    /// Wire digests that are not 32 bytes are a named error pointing
    /// at the malformed field.
    #[test]
    fn assemble_rejects_malformed_digests() {
        let err = InoMap::assemble(
            &[(
                "/nix/store/ffff-bad".to_string(),
                RootNode {
                    node: Some(root_node::Node::DirDigest(vec![1, 2, 3])),
                },
            )],
            HashMap::new(),
        )
        .expect_err("short digest");
        assert!(matches!(err, TreeError::BadDigest { len: 3, .. }));
    }
}
