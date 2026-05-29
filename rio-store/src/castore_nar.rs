//! Castore Directory DAG → NAR byte stream (the inverse of
//! [`crate::castore::build`]).
//!
//! ADR-022 §6 stores a chunked path's content as per-file chunks plus
//! a Directory DAG; the NAR framing is never persisted. Everything
//! that needs the actual NAR byte stream — `GetPath`, the
//! `PutPathChunked` verify task's SHA-256/refscan recompute, the
//! binary-cache compat writer — regenerates the framing from the DAG
//! and splices file contents in between.
//!
//! Two layers:
//!
//! - [`walk`] — canonical-NAR-order iteration over a `(RootNode,
//!   digest → Directory)` DAG, yielding one [`WalkEvent`] per node.
//!   Used directly by `validate_begin` (reachability + chunk-run
//!   alignment + blob-offset assignment) and by the framing adapter.
//! - [`nar_pieces`] — wraps [`walk`] and converts the event stream
//!   into alternating [`NarPiece::Framing`] / [`NarPiece::Contents`]
//!   pieces. Concatenating the framing bytes with each `Contents`
//!   placeholder replaced by that file's content bytes reproduces the
//!   NAR bit-for-bit. Consumers stay async-friendly: the iterator is
//!   sync, the caller awaits its chunk fetches between pieces.
//!
//! The framing byte sequences come from [`rio_nix::nar::frame`] — the
//! same primitives `dump_path_streaming` is tested against — so a DAG
//! built from a NAR re-encodes to that exact NAR
//! (`recode_roundtrips_dump_path` below).

use std::collections::HashMap;

use prost::Message as _;
use rio_nix::nar::frame;
use rio_proto::castore::{Directory, RootNode, root_node};

/// One node of a canonical-NAR-order walk over a Directory DAG.
///
/// `name` is the entry's basename (empty for the NAR root). A
/// directory's children arrive between its `EnterDir` and the matching
/// `LeaveDir`, in byte-lexicographic name order across all three entry
/// kinds.
#[derive(Debug, PartialEq, Eq)]
pub enum WalkEvent<'a> {
    EnterDir {
        name: &'a [u8],
    },
    LeaveDir,
    File {
        name: &'a [u8],
        digest: [u8; 32],
        size: u64,
        executable: bool,
    },
    Symlink {
        name: &'a [u8],
        target: &'a [u8],
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WalkError {
    /// A `DirectoryEntry.digest` reachable from the root has no body in
    /// the supplied map. For `PutPathChunked` this means the builder
    /// omitted a body from `Begin.directories`.
    #[error("directory {0} referenced but not supplied")]
    MissingDirectory(String),
    /// The walk visited more nodes than the caller's cap. A Directory
    /// *DAG* expands to a *tree* on walk — a deliberately
    /// deep-and-wide DAG can reference the same subtree exponentially
    /// many times, so the node count must be bounded independently of
    /// the (already-bounded) distinct-body count.
    #[error("walk exceeded {0} nodes")]
    TooManyNodes(usize),
    /// The walk reached a node nested deeper than
    /// [`rio_nix::nar::MAX_NAR_DEPTH`]. Every NAR reader (rio's own
    /// re-ingest and gateway restore, stock Nix `parseDump`) rejects
    /// such a tree, so it must never be committable — see
    /// `r[store.ingest.tree-bounds]`.
    #[error(
        "directory nesting depth {depth} exceeds maximum {max}",
        depth = .0,
        max = rio_nix::nar::MAX_NAR_DEPTH
    )]
    TooDeep(usize),
    /// The cumulative materialized index bytes (joined entry paths plus
    /// symlink targets) of the expanded tree exceed
    /// [`rio_nix::nar::MAX_NAR_INDEX_BYTES`].
    #[error(
        "cumulative entry path/target bytes {seen} exceed maximum {max}",
        seen = .0,
        max = rio_nix::nar::MAX_NAR_INDEX_BYTES
    )]
    IndexBytesTooLarge(u64),
    /// `RootNode.node` is unset or a digest field has the wrong length.
    #[error("malformed root node: {0}")]
    BadRoot(&'static str),
}

/// Stack frame: the children of one entered directory, pre-merged into
/// byte-lex name order, plus a cursor.
struct Frame<'a> {
    children: Vec<Child<'a>>,
    next: usize,
    /// Length of the `/`-joined path prefix (including the trailing
    /// `/`) this frame's children inherit. 0 for the root directory's
    /// frame — its children's paths are bare names.
    prefix_len: usize,
}

// The castore validation boundary and the NAR readers MUST agree on
// every tree-shape limit (`r[store.ingest.tree-bounds]`): a tree one
// side accepts but the other rejects is a path that commits 'complete'
// yet can never be re-served or re-ingested. `rio_nix::nar` is the
// single source of truth; the rio-common mirrors exist only because
// rio-proto's per-Directory validator predates the contract. Drift is
// a build error.
const _: () = {
    assert!(rio_common::limits::MAX_CASTORE_NAME_BYTES as u64 == rio_nix::nar::MAX_NAME_LEN);
    assert!(rio_common::limits::MAX_CASTORE_TARGET_BYTES as u64 == rio_nix::nar::MAX_TARGET_LEN);
    assert!(rio_common::limits::MAX_CASTORE_DIR_ENTRIES == rio_nix::nar::MAX_DIRECTORY_ENTRIES);
    assert!(rio_common::limits::MAX_DIR_NODES == rio_nix::nar::MAX_NAR_ENTRIES);
};

/// A reference into one of a `Directory`'s three kind lists. `Copy` so
/// `step()` can lift one out of the current stack frame before pushing
/// a new frame (which may reallocate the stack `Vec` and move the
/// frame the reference would otherwise point into).
#[derive(Clone, Copy)]
enum Child<'a> {
    Dir(&'a rio_proto::castore::DirectoryEntry),
    File(&'a rio_proto::castore::FileEntry),
    Symlink(&'a rio_proto::castore::SymlinkEntry),
}

impl Child<'_> {
    fn name(&self) -> &[u8] {
        match self {
            Child::Dir(e) => &e.name,
            Child::File(e) => &e.name,
            Child::Symlink(e) => &e.name,
        }
    }
}

/// Canonical-NAR-order iterator over a Directory DAG. See [`walk`].
pub struct TreeWalk<'a> {
    dirs: &'a HashMap<[u8; 32], Directory>,
    stack: Vec<Frame<'a>>,
    /// The root event, emitted on the first `next()` call. `None` once
    /// consumed.
    pending_root: Option<&'a RootNode>,
    /// Nodes emitted so far, checked against `max_nodes`.
    emitted: usize,
    max_nodes: usize,
    /// Cumulative materialized index bytes (joined entry paths plus
    /// symlink targets) of the expanded tree, checked against
    /// [`rio_nix::nar::MAX_NAR_INDEX_BYTES`]. The walk itself never
    /// allocates the paths, but its consumers (`tree_to_entries`, the
    /// NAR framing buffers) do — so the bound has to live here, where
    /// every consumer inherits it.
    index_bytes: u64,
    /// Set after an error or after the walk completes; all subsequent
    /// `next()` calls return `None`.
    done: bool,
}

/// Iterate `root`'s tree in canonical NAR walk order.
///
/// `max_nodes` bounds the number of emitted *nodes* (root + every
/// entry; `LeaveDir` events are not nodes and are not counted), so it
/// is directly comparable to `nar_ls`'s entry count and to
/// [`rio_nix::nar::MAX_NAR_ENTRIES`]. The walk is over the *expanded
/// tree*, not the deduped DAG — see [`WalkError::TooManyNodes`].
///
/// Independently of `max_nodes`, the walk enforces the NAR readers'
/// nesting-depth cap and the cumulative index-byte cap
/// (`r[store.ingest.tree-bounds]`) — a tree the readers would reject
/// must never walk to completion at the validation boundary.
pub fn walk<'a>(
    root: &'a RootNode,
    dirs: &'a HashMap<[u8; 32], Directory>,
    max_nodes: usize,
) -> TreeWalk<'a> {
    TreeWalk {
        dirs,
        stack: Vec::new(),
        pending_root: Some(root),
        emitted: 0,
        max_nodes,
        index_bytes: 0,
        done: false,
    }
}

impl<'a> TreeWalk<'a> {
    /// Look up a directory body and push its merged child list.
    /// `prefix_len` is the joined-path prefix (incl. trailing `/`) the
    /// new frame's children inherit — 0 when entering the root.
    fn enter(&mut self, digest: [u8; 32], prefix_len: usize) -> Result<(), WalkError> {
        let dir = self
            .dirs
            .get(&digest)
            .ok_or_else(|| WalkError::MissingDirectory(hex::encode(digest)))?;
        let mut children: Vec<Child<'a>> = dir
            .directories
            .iter()
            .map(Child::Dir)
            .chain(dir.files.iter().map(Child::File))
            .chain(dir.symlinks.iter().map(Child::Symlink))
            .collect();
        // Each kind list is individually sorted (validate_directory);
        // NAR order is the byte-lex merge across all three.
        children.sort_unstable_by(|a, b| a.name().cmp(b.name()));
        self.stack.push(Frame {
            children,
            next: 0,
            prefix_len,
        });
        Ok(())
    }

    fn bump(&mut self) -> Result<(), WalkError> {
        self.emitted += 1;
        if self.emitted > self.max_nodes {
            return Err(WalkError::TooManyNodes(self.max_nodes));
        }
        Ok(())
    }

    /// Charge one node's materialized index bytes (its joined path
    /// length plus, for symlinks, its target length).
    // r[impl store.ingest.tree-bounds+2]
    fn charge_index_bytes(&mut self, n: usize) -> Result<(), WalkError> {
        self.index_bytes += n as u64;
        if self.index_bytes > rio_nix::nar::MAX_NAR_INDEX_BYTES {
            return Err(WalkError::IndexBytesTooLarge(self.index_bytes));
        }
        Ok(())
    }

    fn step(&mut self) -> Option<Result<WalkEvent<'a>, WalkError>> {
        // First call: emit the root node.
        if let Some(root) = self.pending_root.take() {
            if let Err(e) = self.bump() {
                return Some(Err(e));
            }
            return Some(match &root.node {
                Some(root_node::Node::DirDigest(d)) => {
                    let digest: [u8; 32] = match d.as_slice().try_into() {
                        Ok(d) => d,
                        Err(_) => return Some(Err(WalkError::BadRoot("dir_digest not 32 bytes"))),
                    };
                    self.enter(digest, 0)
                        .map(|()| WalkEvent::EnterDir { name: b"" })
                }
                Some(root_node::Node::File(f)) => match f.digest.as_slice().try_into() {
                    Ok(digest) => Ok(WalkEvent::File {
                        name: b"",
                        digest,
                        size: f.size,
                        executable: f.executable,
                    }),
                    Err(_) => Err(WalkError::BadRoot("file digest not 32 bytes")),
                },
                Some(root_node::Node::Symlink(s)) => {
                    self.charge_index_bytes(s.target.len())
                        .map(|()| WalkEvent::Symlink {
                            name: b"",
                            target: &s.target,
                        })
                }
                None => Err(WalkError::BadRoot("RootNode.node unset")),
            });
        }

        let frame = self.stack.last_mut()?;
        if frame.next >= frame.children.len() {
            self.stack.pop();
            // LeaveDir is not a node: it is neither counted against
            // `max_nodes` nor charged index bytes.
            return Some(Ok(WalkEvent::LeaveDir));
        }
        let child: Child<'a> = frame.children[frame.next];
        let prefix_len = frame.prefix_len;
        frame.next += 1;
        // This child's nesting depth is the number of entered ancestor
        // directories. The NAR readers (nar_ls, parse, restore) reject
        // anything deeper, so the walk must too — or the validation
        // boundary commits a tree whose regenerated NAR can never be
        // re-ingested.
        // r[impl store.ingest.tree-bounds+2]
        let depth = self.stack.len();
        if depth > rio_nix::nar::MAX_NAR_DEPTH {
            return Some(Err(WalkError::TooDeep(depth)));
        }
        if let Err(e) = self.bump() {
            return Some(Err(e));
        }
        let name_len = child.name().len();
        let target_len = match child {
            Child::Symlink(e) => e.target.len(),
            _ => 0,
        };
        if let Err(e) = self.charge_index_bytes(prefix_len + name_len + target_len) {
            return Some(Err(e));
        }
        Some(match child {
            Child::Dir(e) => {
                let name = e.name.as_slice();
                let digest: [u8; 32] = match e.digest.as_slice().try_into() {
                    Ok(d) => d,
                    // validate_directory rejects this before any walk;
                    // defense-in-depth for callers that skipped it.
                    Err(_) => {
                        return Some(Err(WalkError::BadRoot("child dir digest not 32 bytes")));
                    }
                };
                // The child directory's own joined-path length plus the
                // separator its children's paths will carry.
                self.enter(digest, prefix_len + name_len + 1)
                    .map(|()| WalkEvent::EnterDir { name })
            }
            Child::File(e) => match e.digest.as_slice().try_into() {
                Ok(digest) => Ok(WalkEvent::File {
                    name: &e.name,
                    digest,
                    size: e.size,
                    executable: e.executable,
                }),
                Err(_) => Err(WalkError::BadRoot("child file digest not 32 bytes")),
            },
            Child::Symlink(e) => Ok(WalkEvent::Symlink {
                name: &e.name,
                target: &e.target,
            }),
        })
    }
}

impl<'a> Iterator for TreeWalk<'a> {
    type Item = Result<WalkEvent<'a>, WalkError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.step();
        match &item {
            None | Some(Err(_)) => self.done = true,
            Some(Ok(_)) => {}
        }
        item
    }
}

/// One piece of a regenerated NAR byte stream.
#[derive(Debug, PartialEq, Eq)]
pub enum NarPiece {
    /// Literal NAR framing bytes — emit verbatim.
    Framing(Vec<u8>),
    /// Exactly `size` content bytes of the regular file whose whole-file
    /// blake3 is `digest` go here. The padding to the next 8-byte
    /// boundary is part of the *following* `Framing` piece.
    Contents { digest: [u8; 32], size: u64 },
}

/// Cap on the framing bytes [`NarPieces`] accumulates before yielding
/// a `Framing` piece even though no file content is due. Without this
/// a tree of only directories and symlinks — or a DAG whose shared
/// subtrees expand to millions of nodes before the walk's `max_nodes`
/// cap fires — accretes every node's framing (including its
/// attacker-length entry name) into one `Vec` until the walk ends:
/// `max_nodes × per-node framing` bytes from a kilobyte-sized DAG.
/// The peak buffer is this threshold plus one event's framing (≤ a few
/// hundred bytes once entry names are bounded by
/// `MAX_CASTORE_NAME_BYTES`).
const FRAMING_FLUSH_BYTES: usize = 256 * 1024;

/// Adapter from [`TreeWalk`] events to [`NarPiece`]s.
///
/// Yields alternating `Framing` / `Contents` pieces (starting and
/// ending with `Framing`). Long framing runs are split at
/// `FRAMING_FLUSH_BYTES`, so consecutive `Framing` pieces are
/// possible; a `Contents` piece is always preceded by the `Framing`
/// that opens its file node. Concatenating the pieces — substituting
/// each `Contents` with that file's bytes — reproduces the canonical
/// NAR.
pub struct NarPieces<'a> {
    walk: TreeWalk<'a>,
    /// Framing accumulated since the last yielded piece.
    buf: Vec<u8>,
    /// A `Contents` piece to yield after the `Framing` piece that was
    /// buffered before it.
    pending: Option<NarPiece>,
    /// Directory nesting depth. 0 = at the root (no `entry(...)`
    /// wrapper around the node).
    depth: usize,
    done: bool,
}

/// Regenerate the NAR byte stream for `root` as a lazy piece sequence.
/// See [`NarPieces`].
pub fn nar_pieces<'a>(
    root: &'a RootNode,
    dirs: &'a HashMap<[u8; 32], Directory>,
    max_nodes: usize,
) -> NarPieces<'a> {
    let mut buf = Vec::new();
    frame::magic(&mut buf).expect("Vec write is infallible");
    NarPieces {
        walk: walk(root, dirs, max_nodes),
        buf,
        pending: None,
        depth: 0,
        done: false,
    }
}

impl NarPieces<'_> {
    /// Append one event's framing to `self.buf`; stash a `Contents`
    /// piece in `self.pending` for `File` events.
    fn push_event(&mut self, ev: &WalkEvent<'_>) {
        let w = &mut self.buf;
        // Vec<u8> writes cannot fail.
        match ev {
            WalkEvent::EnterDir { name } => {
                if self.depth > 0 {
                    frame::entry_open(w, name).expect("vec write");
                }
                frame::node_open(w).expect("vec write");
                frame::directory_open(w).expect("vec write");
                self.depth += 1;
            }
            WalkEvent::LeaveDir => {
                frame::node_close(w).expect("vec write");
                self.depth -= 1;
                if self.depth > 0 {
                    frame::entry_close(w).expect("vec write");
                }
            }
            WalkEvent::Symlink { name, target } => {
                if self.depth > 0 {
                    frame::entry_open(w, name).expect("vec write");
                }
                frame::node_open(w).expect("vec write");
                frame::symlink(w, target).expect("vec write");
                frame::node_close(w).expect("vec write");
                if self.depth > 0 {
                    frame::entry_close(w).expect("vec write");
                }
            }
            WalkEvent::File {
                name,
                digest,
                size,
                executable,
            } => {
                if self.depth > 0 {
                    frame::entry_open(w, name).expect("vec write");
                }
                frame::node_open(w).expect("vec write");
                frame::regular_header(w, *executable, *size).expect("vec write");
                self.pending = Some(NarPiece::Contents {
                    digest: *digest,
                    size: *size,
                });
            }
        }
    }

    /// The framing that follows a file's content bytes: padding +
    /// node/entry close. Called after `push_event` stashed the
    /// `Contents` piece and the buffer was flushed.
    fn close_file(&mut self, size: u64) {
        let w = &mut self.buf;
        frame::contents_padding(w, size).expect("vec write");
        frame::node_close(w).expect("vec write");
        if self.depth > 0 {
            frame::entry_close(w).expect("vec write");
        }
    }
}

impl Iterator for NarPieces<'_> {
    type Item = Result<NarPiece, WalkError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // A Contents piece stashed by the previous call: yield it and
        // queue the post-content framing.
        if let Some(piece) = self.pending.take() {
            if let NarPiece::Contents { size, .. } = piece {
                self.close_file(size);
            }
            return Some(Ok(piece));
        }
        loop {
            match self.walk.next() {
                Some(Ok(ev)) => {
                    self.push_event(&ev);
                    if self.pending.is_some() || self.buf.len() >= FRAMING_FLUSH_BYTES {
                        // Flush the framing accumulated up to this
                        // file's content position — or unconditionally
                        // once the buffer crosses the flush threshold,
                        // so a content-free subtree cannot grow it
                        // without bound (see [`FRAMING_FLUSH_BYTES`]).
                        return Some(Ok(NarPiece::Framing(std::mem::take(&mut self.buf))));
                    }
                }
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(e));
                }
                None => {
                    self.done = true;
                    if self.buf.is_empty() {
                        return None;
                    }
                    return Some(Ok(NarPiece::Framing(std::mem::take(&mut self.buf))));
                }
            }
        }
    }
}

/// Decode `nar_index.root_node` + `directories.body` rows into the
/// `(RootNode, digest → Directory)` pair the walkers consume. The
/// digest key is recomputed from the body (not taken from the row key)
/// so a corrupt `directories` row surfaces as `MissingDirectory` at
/// walk time instead of serving a tree that doesn't match its digest.
pub fn decode_dag(
    root_node: &[u8],
    bodies: impl IntoIterator<Item = Vec<u8>>,
) -> Result<(RootNode, HashMap<[u8; 32], Directory>), prost::DecodeError> {
    let root = RootNode::decode(root_node)?;
    let mut dirs = HashMap::new();
    for body in bodies {
        let dir = Directory::decode(body.as_slice())?;
        dirs.insert(rio_proto::castore_util::directory_digest(&dir), dir);
    }
    Ok((root, dirs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::nar::{dump_path_streaming, nar_ls};

    /// `dump_path` without the `test-oracle` feature dance.
    fn dump_path(p: &std::path::Path) -> anyhow::Result<Vec<u8>> {
        let mut v = Vec::new();
        dump_path_streaming(p, &mut v)?;
        Ok(v)
    }

    /// Build the `(RootNode, dirs)` pair for a NAR via the production
    /// `nar_ls` → `castore::build` pipeline.
    fn dag_of(nar: &[u8]) -> (RootNode, HashMap<[u8; 32], Directory>) {
        let entries = nar_ls(std::io::Cursor::new(nar)).expect("fixture NAR parses");
        let dag = crate::castore::build(&entries);
        decode_dag(&dag.root_node, dag.directories.into_iter().map(|(_, b)| b))
            .expect("round-trip decode")
    }

    /// Substitute each `Contents` piece with the file's bytes pulled
    /// from the original NAR (located by digest via `nar_ls`).
    fn reassemble(nar: &[u8], root: &RootNode, dirs: &HashMap<[u8; 32], Directory>) -> Vec<u8> {
        let entries = nar_ls(std::io::Cursor::new(nar)).expect("parse");
        let by_digest: HashMap<[u8; 32], (u64, u64)> = entries
            .iter()
            .filter(|e| e.kind == rio_nix::nar::NarEntryKind::Regular)
            .map(|e| (e.file_digest, (e.nar_offset, e.size)))
            .collect();
        let mut out = Vec::new();
        for piece in nar_pieces(root, dirs, 1 << 20) {
            match piece.expect("walk fixture") {
                NarPiece::Framing(b) => out.extend_from_slice(&b),
                NarPiece::Contents { digest, size } => {
                    let &(off, sz) = by_digest.get(&digest).expect("digest known");
                    assert_eq!(sz, size);
                    out.extend_from_slice(&nar[off as usize..(off + sz) as usize]);
                }
            }
        }
        out
    }

    /// THE invariant: `nar → castore::build → nar_pieces → bytes` is
    /// the identity. If this breaks, every `PutPathChunked` upload
    /// fails its SHA-256 recompute and every `GetPath` serves a
    /// corrupt NAR.
    #[test]
    fn recode_roundtrips_dump_path() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let root = dir.path().join("root");
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("a"), b"alpha")?; // 5 bytes → padded
        std::fs::write(root.join("empty"), b"")?; // zero-byte file
        std::fs::create_dir(root.join("sub"))?;
        std::fs::create_dir(root.join("sub/deeper"))?; // empty dir
        std::fs::write(root.join("sub/b"), b"12345678")?; // exactly 8 → no pad
        std::os::unix::fs::symlink("../a", root.join("sub/link"))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(root.join("x"), b"#!/bin/sh\n")?;
            std::fs::set_permissions(root.join("x"), std::fs::Permissions::from_mode(0o755))?;
        }

        let nar = dump_path(&root)?;
        let (rn, dirs) = dag_of(&nar);
        assert_eq!(reassemble(&nar, &rn, &dirs), nar);
        Ok(())
    }

    /// Single-file and single-symlink roots have no enclosing
    /// directory — the `depth == 0` branches.
    #[test]
    fn recode_non_directory_roots() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let f = dir.path().join("just-a-file");
        std::fs::write(&f, b"contents here")?;
        let nar = dump_path(&f)?;
        let (rn, dirs) = dag_of(&nar);
        assert_eq!(reassemble(&nar, &rn, &dirs), nar);

        let l = dir.path().join("just-a-link");
        std::os::unix::fs::symlink("/nowhere", &l)?;
        let nar = dump_path(&l)?;
        let (rn, dirs) = dag_of(&nar);
        assert_eq!(reassemble(&nar, &rn, &dirs), nar);
        Ok(())
    }

    /// A shared subtree (same dir body referenced twice) is expanded
    /// at every occurrence — the walk is over the tree, not the DAG.
    #[test]
    fn walk_expands_shared_subtrees() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a"))?;
        std::fs::create_dir_all(root.join("b"))?;
        std::fs::write(root.join("a/same"), b"identical")?;
        std::fs::write(root.join("b/same"), b"identical")?;
        let nar = dump_path(&root)?;
        let (rn, dirs) = dag_of(&nar);
        // a and b dedup to ONE Directory body…
        assert_eq!(dirs.len(), 2, "root + the shared a/b body");
        // …but the walk visits it twice and the recode still matches.
        assert_eq!(reassemble(&nar, &rn, &dirs), nar);
        let files = walk(&rn, &dirs, 1 << 20)
            .filter(|e| matches!(e, Ok(WalkEvent::File { .. })))
            .count();
        assert_eq!(files, 2);
        Ok(())
    }

    /// A reachable digest with no supplied body fails the walk.
    #[test]
    fn walk_rejects_missing_directory_body() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("sub"))?;
        std::fs::write(root.join("sub/f"), b"x")?;
        let nar = dump_path(&root)?;
        let (rn, mut dirs) = dag_of(&nar);
        // Drop the non-root body.
        let root_digest: [u8; 32] = match &rn.node {
            Some(root_node::Node::DirDigest(d)) => d.as_slice().try_into().unwrap(),
            _ => unreachable!(),
        };
        dirs.retain(|k, _| *k == root_digest);
        let err = walk(&rn, &dirs, 1 << 20)
            .find_map(Result::err)
            .expect("must fail");
        assert!(matches!(err, WalkError::MissingDirectory(_)));
        Ok(())
    }

    /// Directory nesting deeper than the NAR readers' `MAX_NAR_DEPTH`
    /// is rejected by the walk (bug_006): a committed deeper tree would
    /// regenerate a NAR that every consumer (re-ingest, substitution,
    /// gateway restore, stock Nix import) rejects with NestingTooDeep —
    /// the path would be 'complete' but permanently unservable.
    /// Exactly at the limit must still walk to completion, mirroring
    /// `nar_ls_accepts_at_depth_limit`.
    // r[verify store.ingest.tree-bounds+2]
    #[test]
    fn walk_rejects_nesting_deeper_than_nar_readers() {
        use rio_nix::nar::MAX_NAR_DEPTH;
        use rio_proto::castore::DirectoryEntry;
        use rio_proto::castore_util::directory_digest;

        // Chain of `levels` directories: the outermost is the NAR root
        // (depth 0), so the innermost sits at depth `levels - 1`.
        let chain = |levels: usize| {
            let mut dirs = HashMap::new();
            let mut child: Option<[u8; 32]> = None;
            for _ in 0..levels {
                let d = match child {
                    None => Directory::default(),
                    Some(c) => Directory {
                        directories: vec![DirectoryEntry {
                            name: b"d".to_vec(),
                            digest: c.to_vec(),
                            size: 0,
                        }],
                        ..Default::default()
                    },
                };
                let dg = directory_digest(&d);
                dirs.insert(dg, d);
                child = Some(dg);
            }
            let root = RootNode {
                node: Some(root_node::Node::DirDigest(child.unwrap().to_vec())),
            };
            (root, dirs)
        };

        let (root, dirs) = chain(MAX_NAR_DEPTH + 2); // deepest at depth 257
        let err = walk(&root, &dirs, usize::MAX)
            .find_map(Result::err)
            .expect("a tree deeper than the NAR readers accept must not walk");
        assert!(matches!(err, WalkError::TooDeep(_)), "got {err:?}");

        let (root, dirs) = chain(MAX_NAR_DEPTH + 1); // deepest at depth 256
        assert!(
            walk(&root, &dirs, usize::MAX).all(|e| e.is_ok()),
            "a tree at exactly the NAR readers' depth limit must walk"
        );
    }

    /// A kilobyte-sized DAG whose shared subtrees expand to more
    /// materialized path bytes than `MAX_NAR_INDEX_BYTES` is rejected by
    /// the walk (bug_012): `tree_to_entries` (the chunked-commit index
    /// materialization) joins every entry's full path, so without the
    /// cap a small `Begin` frame expands to tens of GB of heap.
    // r[verify store.ingest.tree-bounds+2]
    #[test]
    fn walk_rejects_cumulative_index_bytes_over_max() {
        use rio_proto::castore::DirectoryEntry;
        use rio_proto::castore_util::directory_digest;

        // 17 levels of doubling references with 250-byte entry names:
        // 2^17 leaf expansions × ~4.3 KiB of joined path ≈ 560 MB of
        // index from a 19-body DAG.
        let leaf = Directory::default();
        let mut dirs = HashMap::new();
        let mut child = directory_digest(&leaf);
        dirs.insert(child, leaf);
        for _ in 0..17 {
            let d = Directory {
                directories: vec![
                    DirectoryEntry {
                        name: vec![b'a'; 250],
                        digest: child.to_vec(),
                        size: 0,
                    },
                    DirectoryEntry {
                        name: vec![b'b'; 250],
                        digest: child.to_vec(),
                        size: 0,
                    },
                ],
                ..Default::default()
            };
            child = directory_digest(&d);
            dirs.insert(child, d);
        }
        let root = RootNode {
            node: Some(root_node::Node::DirDigest(child.to_vec())),
        };

        let err = walk(&root, &dirs, usize::MAX)
            .find_map(Result::err)
            .expect("an expansion past MAX_NAR_INDEX_BYTES must not walk");
        assert!(
            matches!(err, WalkError::IndexBytesTooLarge(_)),
            "got {err:?}"
        );
    }

    /// The node cap fires on the *expanded* tree size.
    #[test]
    fn walk_enforces_node_cap() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let root = dir.path().join("root");
        std::fs::create_dir(&root)?;
        for i in 0..10 {
            std::fs::write(root.join(format!("f{i}")), b"x")?;
        }
        let nar = dump_path(&root)?;
        let (rn, dirs) = dag_of(&nar);
        let err = walk(&rn, &dirs, 5).find_map(Result::err).expect("capped");
        assert_eq!(err, WalkError::TooManyNodes(5));
        Ok(())
    }

    /// A content-free DAG (only directories and symlinks) whose shared
    /// subtrees expand to far more framing than [`FRAMING_FLUSH_BYTES`]
    /// must be yielded as bounded pieces, not one buffer that grows
    /// with the *expanded* tree size. Without the flush threshold a
    /// kilobyte-sized DAG of doubling directory references holds
    /// `max_nodes × per-node-framing` bytes in `NarPieces.buf` before
    /// the first (and only) `Framing` piece is yielded.
    #[test]
    fn framing_pieces_are_bounded_for_content_free_trees() {
        use rio_proto::castore::{DirectoryEntry, SymlinkEntry};
        use rio_proto::castore_util::directory_digest;

        // Leaf: one symlink. Level i: two entries referencing level
        // i-1. 13 levels → 2^13 = 8192 leaf expansions, each ~100+
        // bytes of framing → well past one flush threshold, from a
        // 14-body DAG.
        let leaf = Directory {
            symlinks: vec![SymlinkEntry {
                name: b"l".to_vec(),
                target: b"target".to_vec(),
            }],
            ..Default::default()
        };
        let mut dirs = HashMap::new();
        let mut child_digest = directory_digest(&leaf);
        dirs.insert(child_digest, leaf);
        for _ in 0..13 {
            let d = Directory {
                directories: vec![
                    DirectoryEntry {
                        name: b"a".to_vec(),
                        digest: child_digest.to_vec(),
                        size: 0,
                    },
                    DirectoryEntry {
                        name: b"b".to_vec(),
                        digest: child_digest.to_vec(),
                        size: 0,
                    },
                ],
                ..Default::default()
            };
            child_digest = directory_digest(&d);
            dirs.insert(child_digest, d);
        }
        let root = RootNode {
            node: Some(root_node::Node::DirDigest(child_digest.to_vec())),
        };

        let mut pieces = 0usize;
        let mut total = 0usize;
        let mut max_piece = 0usize;
        for piece in nar_pieces(&root, &dirs, 1 << 20) {
            match piece.expect("walk succeeds") {
                NarPiece::Framing(b) => {
                    pieces += 1;
                    total += b.len();
                    max_piece = max_piece.max(b.len());
                }
                NarPiece::Contents { .. } => panic!("content-free tree"),
            }
        }
        assert!(
            total > 4 * FRAMING_FLUSH_BYTES,
            "fixture must actually exceed the flush threshold several times over \
             (got {total} bytes of framing)"
        );
        assert!(pieces > 1, "the flush must have fired mid-walk");
        // One event's framing past the threshold is the allowed
        // overshoot (the check runs after the event is appended).
        assert!(
            max_piece < FRAMING_FLUSH_BYTES + 1024,
            "no Framing piece may materially exceed the flush threshold, got {max_piece}"
        );
    }
}
