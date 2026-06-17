//! Directory merkle layer (ADR-022 §8): bottom-up `dir_digest` pass
//! over `nar_ls` output. Lives here, not in rio-nix, because the
//! canonical encoding is a prost encode of [`Directory`] and rio-nix
//! cannot depend on rio-proto.

use std::collections::HashMap;

use prost::Message;
use rio_nix::nar::{NarEntryKind, NarLsEntry};
use rio_proto::castore::{Directory, DirectoryEntry, FileEntry, RootNode, SymlinkEntry, root_node};

/// Output of [`build`]: directory DAG state for `set_nar_index_in_conn`.
#[derive(Default)]
pub struct DirectoryDag {
    /// Distinct `(dir_digest, encoded Directory body)`, sorted so the
    /// UPSERT and GC decrement take row locks in the same order
    /// (`r[store.chunk.lock-order]`).
    pub directories: Vec<([u8; 32], Vec<u8>)>,
    /// Distinct `(file_digest, blob_offset, size)`, digest-sorted.
    /// First occurrence's offset wins; size is content-derived so
    /// duplicates always agree.
    ///
    /// `blob_offset` is the file's offset in the **blob stream** — the
    /// concatenation of every regular file's contents in NAR walk
    /// order — NOT its offset in the NAR byte stream. The blob stream
    /// is what a version-2 `manifest_data.chunk_list` reassembles to
    /// (per-file chunks, no framing), so the `StatBlob`/`ReadBlob`
    /// cumsum over chunk sizes maps these offsets to exact chunk
    /// windows. Persisted in the `file_blobs.nar_offset` column (the
    /// column name predates the ADR-022 §6 format change).
    pub file_blobs: Vec<([u8; 32], u64, u64)>,
    /// Encoded [`RootNode`] for `nar_index.root_node`.
    pub root_node: Vec<u8>,
    /// Root `dir_digest`, or empty when the root is not a directory.
    pub root_digest: Vec<u8>,
    /// Per-input-entry `dir_digest`, index-aligned; `[0; 32]` for
    /// non-directories.
    pub dir_digests: Vec<[u8; 32]>,
}

/// Bottom-up Directory pass over a `nar_ls` entry list (DFS pre-order,
/// root at 0). Panics on empty input — the NAR wire format always has
/// a root node.
// r[impl store.index.dir-digest]
// r[impl store.castore.canonical-encoding]
pub fn build(entries: &[NarLsEntry]) -> DirectoryDag {
    assert!(!entries.is_empty(), "nar_ls always emits a root node");

    // Per-entry children indices. `nar_ls` validates byte-lex basename
    // order within each dir, so children come off the slice sorted.
    let path_to_idx: HashMap<&[u8], usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.path.as_slice(), i))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (i, e) in entries.iter().enumerate().skip(1) {
        let parent_path = match e.path.rsplit_once_byte(b'/') {
            Some((parent, _)) => parent,
            None => &[],
        };
        let parent_idx = path_to_idx[parent_path];
        children[parent_idx].push(i);
    }

    // Reverse iteration over pre-order = children before parents.
    let mut dir_digests = vec![[0u8; 32]; entries.len()];
    let mut dir_sizes = vec![0u64; entries.len()];
    let mut bodies: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
    for i in (0..entries.len()).rev() {
        if entries[i].kind != NarEntryKind::Directory {
            continue;
        }
        let mut dir = Directory::default();
        let mut size: u64 = 0;
        for &c in &children[i] {
            let child = &entries[c];
            let name = basename(&child.path).to_vec();
            size += 1;
            match child.kind {
                NarEntryKind::Regular => dir.files.push(FileEntry {
                    name,
                    digest: child.file_digest.to_vec(),
                    size: child.size,
                    executable: child.executable,
                }),
                NarEntryKind::Directory => {
                    size += dir_sizes[c];
                    dir.directories.push(DirectoryEntry {
                        name,
                        digest: dir_digests[c].to_vec(),
                        size: dir_sizes[c],
                    });
                }
                NarEntryKind::Symlink => dir.symlinks.push(SymlinkEntry {
                    name,
                    target: child.target.clone(),
                }),
            }
        }
        // Canonical encoding = default prost field-order encode.
        let body = dir.encode_to_vec();
        let digest = *blake3::hash(&body).as_bytes();
        dir_digests[i] = digest;
        dir_sizes[i] = size;
        bodies.entry(digest).or_insert(body);
    }

    // Root node: NAR root is at index 0.
    let root = &entries[0];
    let root_node = RootNode {
        node: Some(match root.kind {
            NarEntryKind::Directory => root_node::Node::DirDigest(dir_digests[0].to_vec()),
            NarEntryKind::Regular => root_node::Node::File(FileEntry {
                name: Vec::new(),
                digest: root.file_digest.to_vec(),
                size: root.size,
                executable: root.executable,
            }),
            NarEntryKind::Symlink => root_node::Node::Symlink(SymlinkEntry {
                name: Vec::new(),
                target: root.target.clone(),
            }),
        }),
    }
    .encode_to_vec();
    let root_digest = if root.kind == NarEntryKind::Directory {
        dir_digests[0].to_vec()
    } else {
        Vec::new()
    };

    // Distinct file_digest → (blob_offset, size), first occurrence
    // wins. The blob offset is the running total of every preceding
    // regular file's size in walk order — entries are DFS pre-order,
    // which is exactly the order the per-file chunk runs appear in a
    // version-2 manifest.
    let mut file_blobs_map: HashMap<[u8; 32], (u64, u64)> = HashMap::new();
    let mut blob_offset: u64 = 0;
    for e in entries {
        if e.kind == NarEntryKind::Regular {
            file_blobs_map
                .entry(e.file_digest)
                .or_insert((blob_offset, e.size));
            blob_offset += e.size;
        }
    }

    let mut directories: Vec<([u8; 32], Vec<u8>)> = bodies.into_iter().collect();
    directories.sort_unstable_by_key(|(d, _)| *d);
    let mut file_blobs: Vec<([u8; 32], u64, u64)> = file_blobs_map
        .into_iter()
        .map(|(d, (o, s))| (d, o, s))
        .collect();
    file_blobs.sort_unstable_by_key(|(d, _, _)| *d);

    DirectoryDag {
        directories,
        file_blobs,
        root_node,
        root_digest,
        dir_digests,
    }
}

/// Last path component, or the whole path if there is no `/`.
fn basename(path: &[u8]) -> &[u8] {
    match path.rsplit_once_byte(b'/') {
        Some((_, base)) => base,
        None => path,
    }
}

/// `[u8]::rsplit_once` (nightly `slice_split_once`) for one byte.
trait RsplitOnceByte {
    fn rsplit_once_byte(&self, b: u8) -> Option<(&[u8], &[u8])>;
}
impl RsplitOnceByte for [u8] {
    fn rsplit_once_byte(&self, b: u8) -> Option<(&[u8], &[u8])> {
        let i = self.iter().rposition(|&x| x == b)?;
        Some((&self[..i], &self[i + 1..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &[u8]) -> NarLsEntry {
        NarLsEntry {
            path: path.to_vec(),
            kind: NarEntryKind::Directory,
            size: 0,
            executable: false,
            nar_offset: 0,
            target: Vec::new(),
            file_digest: [0; 32],
        }
    }
    fn file(path: &[u8], digest: [u8; 32], size: u64, exec: bool, off: u64) -> NarLsEntry {
        NarLsEntry {
            path: path.to_vec(),
            kind: NarEntryKind::Regular,
            size,
            executable: exec,
            nar_offset: off,
            target: Vec::new(),
            file_digest: digest,
        }
    }
    fn sym(path: &[u8], target: &[u8]) -> NarLsEntry {
        NarLsEntry {
            path: path.to_vec(),
            kind: NarEntryKind::Symlink,
            size: 0,
            executable: false,
            nar_offset: 0,
            target: target.to_vec(),
            file_digest: [0; 32],
        }
    }

    /// Pin the canonical `Directory` encode bytes. prost field-order
    /// encoding isn't formally guaranteed (snix #111); a prost upgrade
    /// that changes the encoder must be a deliberate decision, not a
    /// silent dedup-namespace split.
    ///
    /// TODO: cross-check `root_digest` against `tvix-store import` of
    /// an equivalent on-disk fixture (the snix-interop golden).
    // r[verify store.castore.canonical-encoding]
    #[test]
    fn golden_directory_encoding() {
        let entries = vec![
            dir(b""),
            file(b"a", [1u8; 32], 3, false, 100),
            dir(b"b"),
            file(b"b/d", [2u8; 32], 5, true, 200),
            sym(b"c", b"a"),
        ];
        let dag = build(&entries);

        // Inner directory `b` has one file `d`.
        let inner = Directory {
            directories: vec![],
            files: vec![FileEntry {
                name: b"d".to_vec(),
                digest: vec![2u8; 32],
                size: 5,
                executable: true,
            }],
            symlinks: vec![],
        };
        let inner_bytes = inner.encode_to_vec();
        let inner_digest = *blake3::hash(&inner_bytes).as_bytes();
        // The shared digest helper (what the builder-side walk and the
        // PutPathChunked validator both call) MUST agree with the
        // bottom-up pass's inline computation — a split here is a
        // silent dedup-namespace split between builder-uploaded and
        // store-indexed paths.
        assert_eq!(
            rio_proto::castore_util::directory_digest(&inner),
            inner_digest
        );

        // Outer (root) has dir `b`, file `a`, symlink `c` — in that
        // proto-field order, names sorted within each list.
        let outer = Directory {
            directories: vec![DirectoryEntry {
                name: b"b".to_vec(),
                digest: inner_digest.to_vec(),
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
        let outer_bytes = outer.encode_to_vec();
        let outer_digest = *blake3::hash(&outer_bytes).as_bytes();

        // Self-consistency: the bottom-up pass produces the same bodies
        // the explicit construction above would.
        assert_eq!(dag.dir_digests[0], outer_digest);
        assert_eq!(dag.dir_digests[2], inner_digest);
        assert_eq!(dag.root_digest, outer_digest.to_vec());
        assert_eq!(dag.directories.len(), 2);
        assert!(
            dag.directories
                .iter()
                .any(|(d, b)| d == &inner_digest && b == &inner_bytes)
        );
        assert!(
            dag.directories
                .iter()
                .any(|(d, b)| d == &outer_digest && b == &outer_bytes)
        );

        // Pinning bytes pins the digest (it's pure blake3(bytes)).
        #[rustfmt::skip]
        let pinned: &[u8] = &[
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
        assert_eq!(inner_bytes, pinned, "prost encode-order drift");
    }

    /// Shared subtree → one `directories` entry, distinct file content
    /// → one `file_blobs` entry per digest.
    #[test]
    fn dedup_shared_subtree_and_blobs() {
        // root/{a,b}/x — both `a` and `b` contain a file `x` with
        // identical content → identical Directory bodies → one digest.
        let entries = vec![
            dir(b""),
            dir(b"a"),
            file(b"a/x", [9u8; 32], 4, false, 100),
            dir(b"b"),
            file(b"b/x", [9u8; 32], 4, false, 300),
        ];
        let dag = build(&entries);
        assert_eq!(dag.dir_digests[1], dag.dir_digests[3], "shared subtree");
        // 2 distinct directories: root + (a == b).
        assert_eq!(dag.directories.len(), 2);
        // 1 distinct file content.
        assert_eq!(dag.file_blobs.len(), 1);
        assert_eq!(dag.file_blobs[0].0, [9u8; 32]);
        // First occurrence's BLOB offset retained (a/x is the first
        // regular file in walk order → offset 0), size carried through.
        // b/x's occurrence (blob offset 4) loses the or_insert race.
        assert_eq!(dag.file_blobs[0].1, 0);
        assert_eq!(dag.file_blobs[0].2, 4);
    }

    /// Single-file root → no directories, root_digest empty, root_node
    /// carries the FileEntry inline.
    #[test]
    fn single_file_root() {
        let entries = vec![file(b"", [3u8; 32], 7, true, 96)];
        let dag = build(&entries);
        assert!(dag.directories.is_empty());
        assert!(dag.root_digest.is_empty());
        // Blob offset 0 — the root file is the only (and first) regular
        // file regardless of where its contents sat in the NAR.
        assert_eq!(dag.file_blobs, vec![([3u8; 32], 0, 7)]);
        let rn = RootNode::decode(dag.root_node.as_slice()).unwrap();
        match rn.node.unwrap() {
            root_node::Node::File(f) => {
                assert_eq!(f.digest, vec![3u8; 32]);
                assert_eq!(f.size, 7);
                assert!(f.executable);
                assert!(f.name.is_empty());
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    /// Directory `size` is the recursive descendant count.
    #[test]
    fn directory_size_recursive() {
        // root/ a/ b/ c   = 3 nested dirs, 1 file at the bottom.
        let entries = vec![
            dir(b""),
            dir(b"a"),
            dir(b"a/b"),
            file(b"a/b/c", [1u8; 32], 1, false, 100),
        ];
        let dag = build(&entries);
        let root_body = &dag
            .directories
            .iter()
            .find(|(d, _)| d == &dag.dir_digests[0])
            .unwrap()
            .1;
        let root = Directory::decode(root_body.as_slice()).unwrap();
        // root has one child `a`; a has one child `b`; b has one file
        // `c`. size(b) = 1, size(a) = 1 + 1 = 2, size(root.a-entry) = 2.
        assert_eq!(root.directories[0].size, 2);
    }
}
