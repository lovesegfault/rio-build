//! Bottom-up canonicalization of an in-memory source tree into
//! per-directory castore-proto blobs (ADR-024 P1, "Source-DAG
//! metadata").
//!
//! Each directory becomes its own `rio.castore.Directory` blob under
//! the EXISTING canonical-encode rule (`r[store.castore.canonical-
//! encoding]`): prost default field-order encode with the three entry
//! lists sorted byte-lex by name, digested with blake3. Reusing
//! rio-store's message and digest helper is the whole point — client
//! digests equal rio-store digests by construction, so upload
//! negotiation needs no conversion layer and no double hashing.
//!
//! Per-directory granularity (not one monolithic tree blob) is what
//! makes upload dedup work: a single file edit dirties only the chain
//! of directories above it, while every monolithic CDC layout re-dirties
//! 100% on one inserted entry (ADR-024 bench).
//!
//! The ingest pipeline constructs a [`BuiltDir`] (entries in any order,
//! file digests/sizes already computed by its chunker) and calls
//! [`BuiltDir::fold`]; the result feeds `DirStore::put_tree` in
//! [`crate::dircache`].

use std::collections::HashSet;

use bytes::Bytes;
use prost::Message;
use rio_nix::nar::MAX_NAR_DEPTH;
use rio_packstore::Digest;
use rio_proto::castore::{Directory, DirectoryEntry, FileEntry, SymlinkEntry};
use rio_proto::castore_util::{self, DirectoryError};

/// One child of a [`BuiltDir`], mirroring the castore proto entry
/// kinds. File digests/sizes are computed by the caller (the ingest
/// pipeline's chunker); this layer never reads file contents.
#[derive(Debug, Clone)]
pub enum BuiltEntry {
    /// A regular file: content (or chunk-list) digest + size, as the
    /// proto `FileEntry` carries them.
    File {
        digest: Digest,
        size: u64,
        executable: bool,
    },
    /// A subdirectory, folded recursively.
    Dir(BuiltDir),
    /// A symlink. Target is raw bytes (non-UTF-8 legal, like names).
    Symlink { target: Vec<u8> },
}

/// An in-memory directory tree under construction. Entry names are raw
/// bytes (non-UTF-8 legal); insertion order is irrelevant — [`fold`]
/// sorts byte-lex per the canonical-encode rule. Invalid names
/// (empty, `.`, `..`, `/` or NUL bytes, duplicates) are rejected by
/// `validate_directory` at fold time, never silently fixed up.
///
/// [`fold`]: BuiltDir::fold
#[derive(Debug, Clone, Default)]
pub struct BuiltDir {
    entries: Vec<(Vec<u8>, BuiltEntry)>,
}

/// The canonical blobs of one folded tree: children strictly before
/// parents (a consumer can stream-upload in order and every digest a
/// directory references is already sent), deduped by digest within the
/// tree (identical subtrees encode identically, so they fold to one
/// blob).
#[derive(Debug, Clone)]
pub struct FoldedTree {
    /// Digest of the root `Directory` blob.
    pub root_digest: Digest,
    /// `(digest, canonical bytes)` for every distinct directory in the
    /// tree, bottom-up.
    pub blobs: Vec<(Digest, Bytes)>,
}

impl FoldedTree {
    /// Every directory digest in the tree (root included). This is the
    /// set a pack-store root must pin: GC mark is a flat union of root
    /// digest lists, with no transitive walk through blob contents.
    pub fn digests(&self) -> Vec<Digest> {
        self.blobs.iter().map(|(d, _)| *d).collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DirBlobError {
    /// A directory body failed structural validation (bad name,
    /// duplicate, oversized entry, …). Hard error: an invalid body
    /// must never be encoded, digested, or persisted.
    #[error("invalid directory body: {0}")]
    Invalid(#[from] DirectoryError),
    /// Nesting deeper than the NAR reader's [`MAX_NAR_DEPTH`]. The same
    /// bound for the same reason: a deeper tree can never be regenerated
    /// as a NAR the reader accepts, so committing it would create an
    /// unservable path. It also keeps the fold recursion bounded —
    /// stack depth is capped regardless of caller-supplied input.
    #[error("directory nesting depth {0} exceeds maximum {MAX_NAR_DEPTH}")]
    TooDeep(usize),
}

impl BuiltDir {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a child. Order is irrelevant; duplicate names surface as a
    /// hard error at [`BuiltDir::fold`] time.
    pub fn push(&mut self, name: impl Into<Vec<u8>>, entry: BuiltEntry) {
        self.entries.push((name.into(), entry));
    }

    /// Bottom-up canonicalization: encode every directory canonical
    /// (sorted entry lists, prost default encode), digest each with
    /// the shared [`castore_util::directory_digest`] helper, and return
    /// the deduped blob list with the root digest.
    ///
    /// Every body is `validate_directory`-checked before it is digested
    /// — the digest of an invalid body is meaningless and persisting it
    /// would commit a tree the store side rejects on upload.
    pub fn fold(&self) -> Result<FoldedTree, DirBlobError> {
        let mut blobs = Vec::new();
        let mut seen = HashSet::new();
        let (root_digest, _size) = fold_dir(self, 0, &mut blobs, &mut seen)?;
        Ok(FoldedTree { root_digest, blobs })
    }
}

/// Returns the directory's digest and its proto `size` (immediate
/// children plus the sum of every child directory's `size` — the
/// recursive descendant count `DirectoryEntry.size` carries).
///
/// `depth` mirrors the NAR reader's accounting exactly (root at 0,
/// reject `> MAX_NAR_DEPTH`) so the fold accepts precisely the trees
/// the reader can re-parse.
fn fold_dir(
    dir: &BuiltDir,
    depth: usize,
    blobs: &mut Vec<(Digest, Bytes)>,
    seen: &mut HashSet<Digest>,
) -> Result<(Digest, u64), DirBlobError> {
    if depth > MAX_NAR_DEPTH {
        return Err(DirBlobError::TooDeep(depth));
    }
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut symlinks = Vec::new();
    let mut descendants = dir.entries.len() as u64;

    for (name, entry) in &dir.entries {
        match entry {
            BuiltEntry::Dir(child) => {
                let (digest, size) = fold_dir(child, depth + 1, blobs, seen)?;
                descendants += size;
                directories.push(DirectoryEntry {
                    name: name.clone(),
                    digest: digest.0.to_vec(),
                    size,
                });
            }
            BuiltEntry::File {
                digest,
                size,
                executable,
            } => files.push(FileEntry {
                name: name.clone(),
                digest: digest.0.to_vec(),
                size: *size,
                executable: *executable,
            }),
            BuiltEntry::Symlink { target } => symlinks.push(SymlinkEntry {
                name: name.clone(),
                target: target.clone(),
            }),
        }
    }

    // The canonical-encode rule puts the sort on the producer; a
    // duplicate name shows up post-sort as a strict-< violation and
    // validate rejects it.
    directories.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    symlinks.sort_by(|a, b| a.name.cmp(&b.name));

    let body = Directory {
        directories,
        files,
        symlinks,
    };
    castore_util::validate_directory(&body)?;
    let digest = Digest(castore_util::directory_digest(&body));
    if seen.insert(digest) {
        blobs.push((digest, Bytes::from(body.encode_to_vec())));
    }
    Ok((digest, descendants))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(digest_byte: u8, size: u64, executable: bool) -> BuiltEntry {
        BuiltEntry::File {
            digest: Digest([digest_byte; 32]),
            size,
            executable,
        }
    }

    /// Digest parity with rio-store: the same tree as
    /// `rio_store::castore::tests::golden_directory_encoding`, with the
    /// inner directory's canonical bytes pinned to the identical byte
    /// sequence that test pins. If either side drifts (a prost upgrade
    /// changing encode order, a divergent local encoding), this fails
    /// loudly instead of silently splitting the dedup namespace between
    /// client-ingested and store-indexed trees.
    #[test]
    fn golden_digest_parity_with_rio_store() {
        // b/ contains executable file d (digest [2;32], size 5).
        let mut inner = BuiltDir::new();
        inner.push(&b"d"[..], file(2, 5, true));
        // root contains dir b, file a (digest [1;32], size 3), symlink c -> a.
        let mut root = BuiltDir::new();
        root.push(&b"b"[..], BuiltEntry::Dir(inner));
        root.push(&b"a"[..], file(1, 3, false));
        root.push(
            &b"c"[..],
            BuiltEntry::Symlink {
                target: b"a".to_vec(),
            },
        );

        let folded = root.fold().unwrap();
        assert_eq!(folded.blobs.len(), 2, "inner + root");

        // Bytes pinned by rio-store's golden_directory_encoding.
        #[rustfmt::skip]
        let pinned_inner: &[u8] = &[
            // field 2 (files), wire-type 2 (LEN), len=41
            0x12, 0x29,
            //   FileEntry:
            //   field 1 (name), len=1, b"d"
            0x0a, 0x01, b'd',
            //   field 2 (digest), len=32, [2;32]
            0x12, 0x20,
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
            //   field 3 (size), varint 5
            0x18, 0x05,
            //   field 4 (executable), varint 1
            0x20, 0x01,
        ];
        // Bottom-up: blobs[0] is the inner dir, blobs[1] the root.
        let (inner_digest, inner_bytes) = &folded.blobs[0];
        assert_eq!(inner_bytes.as_ref(), pinned_inner, "canonical bytes drift");
        assert_eq!(inner_digest.0, *blake3::hash(pinned_inner).as_bytes());

        // The root body references the inner digest with size=1 (one
        // descendant) and carries the entries in proto field order.
        let expected_root = Directory {
            directories: vec![DirectoryEntry {
                name: b"b".to_vec(),
                digest: inner_digest.0.to_vec(),
                size: 1,
            }],
            files: vec![FileEntry {
                name: b"a".to_vec(),
                digest: vec![1u8; 32],
                size: 3,
                executable: false,
            }],
            symlinks: vec![SymlinkEntry {
                name: b"c".to_vec(),
                target: b"a".to_vec(),
            }],
        };
        assert_eq!(
            folded.root_digest.0,
            castore_util::directory_digest(&expected_root)
        );
        assert_eq!(folded.blobs[1].1.as_ref(), expected_root.encode_to_vec());
    }

    /// Identical subtrees encode identically → one blob, one digest.
    #[test]
    fn fold_dedups_identical_subtrees() {
        let mut leaf = BuiltDir::new();
        leaf.push(&b"x"[..], file(7, 11, false));
        let mut root = BuiltDir::new();
        root.push(&b"a"[..], BuiltEntry::Dir(leaf.clone()));
        root.push(&b"b"[..], BuiltEntry::Dir(leaf));

        let folded = root.fold().unwrap();
        // leaf (shared) + root, not leaf + leaf + root.
        assert_eq!(folded.blobs.len(), 2);
        assert_eq!(folded.digests().len(), 2);
    }

    /// `DirectoryEntry.size` parity with rio-store: the exact tree of
    /// `rio_store::castore::tests::directory_size_recursive`
    /// (root/a/b/c — three nested dirs, one file at the bottom). That
    /// test pins size(b) = 1 and size(a) = 2: the recursive descendant
    /// count, not blob bytes and not a self-inclusive count. A
    /// divergence here digests identically (size lives in the PARENT's
    /// encoded entry) but fails the store's reachability walk on
    /// upload.
    #[test]
    fn directory_entry_size_matches_rio_store() {
        let mut b = BuiltDir::new();
        b.push(&b"c"[..], file(1, 1, false));
        let mut a = BuiltDir::new();
        a.push(&b"b"[..], BuiltEntry::Dir(b));
        let mut root = BuiltDir::new();
        root.push(&b"a"[..], BuiltEntry::Dir(a));

        let folded = root.fold().unwrap();
        // Bottom-up emission: blobs = [b, a, root].
        let a_body = Directory::decode(folded.blobs[1].1.as_ref()).unwrap();
        assert_eq!(a_body.directories[0].size, 1, "size(b)");
        let root_body = Directory::decode(folded.blobs[2].1.as_ref()).unwrap();
        assert_eq!(root_body.directories[0].size, 2, "size(a)");
    }

    /// The fold accepts exactly the nesting the NAR reader accepts
    /// (root at depth 0, reject > MAX_NAR_DEPTH) and therefore cannot
    /// recurse deeper than that bound on any input.
    #[test]
    fn fold_depth_limit_matches_nar_reader() {
        fn chain(depth: usize) -> BuiltDir {
            let mut cur = BuiltDir::new();
            cur.push(&b"f"[..], file(1, 1, false));
            for _ in 0..depth {
                let mut parent = BuiltDir::new();
                parent.push(&b"d"[..], BuiltEntry::Dir(cur));
                cur = parent;
            }
            cur
        }
        assert!(chain(MAX_NAR_DEPTH).fold().is_ok());
        assert!(matches!(
            chain(MAX_NAR_DEPTH + 1).fold(),
            Err(DirBlobError::TooDeep(_))
        ));
    }

    #[test]
    fn fold_rejects_duplicate_and_invalid_names() {
        let mut dup = BuiltDir::new();
        dup.push(&b"a"[..], file(1, 1, false));
        dup.push(
            &b"a"[..],
            BuiltEntry::Symlink {
                target: b"t".to_vec(),
            },
        );
        assert!(matches!(dup.fold(), Err(DirBlobError::Invalid(_))));

        let mut bad = BuiltDir::new();
        bad.push(&b"a/b"[..], file(1, 1, false));
        assert!(matches!(bad.fold(), Err(DirBlobError::Invalid(_))));
    }
}
