//! Decoded-directory cache over the pack store (ADR-024).
//!
//! [`DirStore`] persists per-directory castore blobs (built by
//! [`crate::dirblob`]) in a [`rio_packstore::PackStore`] and fronts
//! reads with a `digest → Arc<DecodedDir>` in-process map. The cache is
//! structural, not an optimization: from a parsed handle every lookup
//! is a binary search; from raw protobuf bytes nothing acceptable
//! exists — the warm-trace budget (2,561 ops in ≤92ms) only holds when
//! the hit path is a map lookup plus an `Arc` clone.
//!
//! Fork-safety mirrors the pack store: no threads, no background work,
//! no tokio. `RefCell` interior mutability keeps the handle `!Sync`,
//! matching `PackStore` — fork workers inherit the populated cache
//! read-only through fork's address-space copy, not through a shared
//! lock.
//!
//! Trust boundary: the pack store digest-verifies raw bytes on every
//! read, but that only proves the bytes match the digest — not that
//! they are a canonical `Directory` encode. The first decode therefore
//! re-validates structure AND recomputes the canonical digest; a
//! mismatch (unknown fields, non-minimal varints — bytes that decode
//! fine but would re-encode differently) is a hard error, never a
//! silently-cached lie.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use prost::Message;
use rio_packstore::{Digest, Kind, PackStore};
use rio_proto::castore::Directory;
use rio_proto::castore_util::{self, DirectoryError};

use crate::dirblob::{BuiltDir, DirBlobError};

#[derive(Debug, thiserror::Error)]
pub enum DirStoreError {
    #[error(transparent)]
    Pack(#[from] rio_packstore::Error),
    #[error(transparent)]
    Build(#[from] DirBlobError),
    #[error("directory {0} not in the pack store")]
    NotFound(Digest),
    #[error("directory {digest} is not a valid Directory message: {source}")]
    Decode {
        digest: Digest,
        source: prost::DecodeError,
    },
    #[error("directory {digest} failed structural validation: {source}")]
    Invalid {
        digest: Digest,
        source: DirectoryError,
    },
    #[error(
        "directory blob is not canonical: requested {requested}, re-encode digests to {recomputed}"
    )]
    DigestMismatch {
        requested: Digest,
        recomputed: Digest,
    },
}

pub type Result<T> = std::result::Result<T, DirStoreError>;

/// A child entry resolved by [`DecodedDir::child`]. Borrowed view —
/// field semantics mirror the castore proto exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryRef<'a> {
    /// `FileEntry`: content (or chunk-list) digest, byte size,
    /// executable bit.
    File {
        digest: Digest,
        size: u64,
        executable: bool,
    },
    /// `DirectoryEntry`: digest of the child `Directory` blob and its
    /// recursive descendant count (untrusted until a reachability walk
    /// verifies it, per the proto docs).
    Dir { digest: Digest, size: u64 },
    /// `SymlinkEntry`: raw target bytes.
    Symlink { target: &'a [u8] },
}

/// One decoded entry. Owned form of [`EntryRef`].
#[derive(Debug, Clone)]
enum Node {
    File {
        digest: Digest,
        size: u64,
        executable: bool,
    },
    Dir {
        digest: Digest,
        size: u64,
    },
    Symlink {
        target: Box<[u8]>,
    },
}

/// A parsed, lookup-optimized `Directory`: one flat entry list sorted
/// byte-lex by name (names are unique across the proto's three kind
/// lists, so a single sorted vec is a total order) — `child` is a
/// binary search, O(log n).
#[derive(Debug)]
pub struct DecodedDir {
    entries: Vec<(Box<[u8]>, Node)>,
}

impl DecodedDir {
    /// Caller must have run `validate_directory` (digest lengths and
    /// cross-list name uniqueness are assumed here).
    fn from_proto(d: &Directory) -> DecodedDir {
        fn digest32(bytes: &[u8]) -> Digest {
            // validate_directory rejected any non-32-byte digest.
            Digest(bytes.try_into().expect("validated 32-byte digest"))
        }
        let mut entries =
            Vec::with_capacity(d.directories.len() + d.files.len() + d.symlinks.len());
        for e in &d.directories {
            entries.push((
                e.name.clone().into_boxed_slice(),
                Node::Dir {
                    digest: digest32(&e.digest),
                    size: e.size,
                },
            ));
        }
        for e in &d.files {
            entries.push((
                e.name.clone().into_boxed_slice(),
                Node::File {
                    digest: digest32(&e.digest),
                    size: e.size,
                    executable: e.executable,
                },
            ));
        }
        for e in &d.symlinks {
            entries.push((
                e.name.clone().into_boxed_slice(),
                Node::Symlink {
                    target: e.target.clone().into_boxed_slice(),
                },
            ));
        }
        // The three kind lists are each sorted, but the flat list
        // interleaves them — one sort restores the total order the
        // binary search needs.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        DecodedDir { entries }
    }

    /// Look up a child by name. O(log n).
    pub fn child(&self, name: &[u8]) -> Option<EntryRef<'_>> {
        let i = self
            .entries
            .binary_search_by(|(n, _)| n.as_ref().cmp(name))
            .ok()?;
        Some(self.entries[i].1.as_ref())
    }

    /// All entries, sorted byte-lex by name.
    pub fn entries(&self) -> impl Iterator<Item = (&[u8], EntryRef<'_>)> {
        self.entries.iter().map(|(n, e)| (n.as_ref(), e.as_ref()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Node {
    fn as_ref(&self) -> EntryRef<'_> {
        match self {
            Node::File {
                digest,
                size,
                executable,
            } => EntryRef::File {
                digest: *digest,
                size: *size,
                executable: *executable,
            },
            Node::Dir { digest, size } => EntryRef::Dir {
                digest: *digest,
                size: *size,
            },
            Node::Symlink { target } => EntryRef::Symlink { target },
        }
    }
}

/// Per-directory blob store: pack-store persistence + decoded cache.
pub struct DirStore {
    pack: PackStore,
    cache: RefCell<HashMap<Digest, Arc<DecodedDir>>>,
}

impl DirStore {
    pub fn new(pack: PackStore) -> DirStore {
        DirStore {
            pack,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Fold `root` bottom-up and persist every directory blob the pack
    /// store does not already hold (`Kind::DIRECTORY`). Returns the
    /// root digest. Re-putting an identical tree writes zero records.
    pub fn put_tree(&mut self, root: &BuiltDir) -> Result<Digest> {
        let folded = root.fold()?;
        for (digest, bytes) in &folded.blobs {
            // `PackStore::put` dedups on digest internally, so an
            // identical re-put writes nothing. Its returned digest is
            // blake3 over these exact bytes — the fold digest by
            // construction, hence a debug assert, not an error path.
            let written = self.pack.put(Kind::DIRECTORY, bytes)?;
            debug_assert_eq!(written, *digest);
        }
        Ok(folded.root_digest)
    }

    /// Fetch a decoded directory. Hit path: one map lookup + one `Arc`
    /// clone. Miss path: pack-store read (digest-verified), decode,
    /// structural validation, canonical-digest recompute, insert.
    pub fn get(&self, digest: &Digest) -> Result<Arc<DecodedDir>> {
        Ok(self.get_tracked(digest)?.0)
    }

    /// [`DirStore::get`] that also reports whether this call decoded
    /// the blob (cache miss) — `false` is the pure hit path. The
    /// eval-store stats use this to prove the warm path does zero
    /// decodes beyond first touch (the ADR-024 92× pathology gate).
    pub fn get_tracked(&self, digest: &Digest) -> Result<(Arc<DecodedDir>, bool)> {
        if let Some(hit) = self.cache.borrow().get(digest) {
            return Ok((Arc::clone(hit), false));
        }
        let Some(bytes) = self.pack.get(digest)? else {
            return Err(DirStoreError::NotFound(*digest));
        };
        let body = Directory::decode(bytes.as_ref()).map_err(|source| DirStoreError::Decode {
            digest: *digest,
            source,
        })?;
        castore_util::validate_directory(&body).map_err(|source| DirStoreError::Invalid {
            digest: *digest,
            source,
        })?;
        // The pack store verified blake3(bytes) == digest, which does
        // not prove the bytes are a canonical encode (unknown fields
        // and non-minimal varints survive decode). Recompute once at
        // insert; cached entries are canonical by construction.
        let recomputed = Digest(castore_util::directory_digest(&body));
        if recomputed != *digest {
            return Err(DirStoreError::DigestMismatch {
                requested: *digest,
                recomputed,
            });
        }
        let decoded = Arc::new(DecodedDir::from_proto(&body));
        self.cache
            .borrow_mut()
            .insert(*digest, Arc::clone(&decoded));
        Ok((decoded, true))
    }

    /// The underlying pack store — the ingest pipeline shares this
    /// handle for file-chunk metadata and fetched-content records (one
    /// writer segment per process).
    pub fn pack(&self) -> &PackStore {
        &self.pack
    }

    pub fn pack_mut(&mut self) -> &mut PackStore {
        &mut self.pack
    }

    /// Durability point — see [`PackStore::flush`].
    pub fn flush(&mut self) -> Result<()> {
        Ok(self.pack.flush()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirblob::BuiltEntry;
    use rio_proto::castore::FileEntry;

    fn open_store(dir: &std::path::Path) -> DirStore {
        DirStore::new(PackStore::open(dir, rio_packstore::Options::default()).unwrap())
    }

    fn file(digest: [u8; 32], size: u64, executable: bool) -> BuiltEntry {
        BuiltEntry::File {
            digest: Digest(digest),
            size,
            executable,
        }
    }

    /// A realistic tree: nested dirs, a symlink, an executable, and a
    /// non-UTF-8 entry name (names are raw NAR bytes).
    fn realistic_tree() -> BuiltDir {
        let mut bin = BuiltDir::new();
        bin.push(&b"tool"[..], file([3; 32], 1024, true));
        let mut src = BuiltDir::new();
        src.push(b"caf\xc3\xa9 \xff".to_vec(), file([4; 32], 7, false));
        src.push(&b"lib.rs"[..], file([5; 32], 2048, false));
        let mut root = BuiltDir::new();
        root.push(&b"bin"[..], BuiltEntry::Dir(bin));
        root.push(&b"src"[..], BuiltEntry::Dir(src));
        root.push(&b"README"[..], file([6; 32], 100, false));
        root.push(
            &b"latest"[..],
            BuiltEntry::Symlink {
                target: b"bin/tool".to_vec(),
            },
        );
        root
    }

    #[test]
    fn put_tree_get_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let root_digest = store.put_tree(&realistic_tree()).unwrap();

        let root = store.get(&root_digest).unwrap();
        assert_eq!(root.len(), 4);
        assert_eq!(
            root.child(b"README"),
            Some(EntryRef::File {
                digest: Digest([6; 32]),
                size: 100,
                executable: false,
            })
        );
        assert_eq!(
            root.child(b"latest"),
            Some(EntryRef::Symlink {
                target: b"bin/tool",
            })
        );
        assert_eq!(root.child(b"nope"), None);

        let Some(EntryRef::Dir {
            digest: bin_digest,
            size: 1,
        }) = root.child(b"bin")
        else {
            panic!("bin must be a Dir with one descendant");
        };
        let bin = store.get(&bin_digest).unwrap();
        assert_eq!(
            bin.child(b"tool"),
            Some(EntryRef::File {
                digest: Digest([3; 32]),
                size: 1024,
                executable: true,
            })
        );

        let Some(EntryRef::Dir {
            digest: src_digest, ..
        }) = root.child(b"src")
        else {
            panic!("src must be a Dir");
        };
        let src = store.get(&src_digest).unwrap();
        assert_eq!(
            src.child(b"caf\xc3\xa9 \xff"),
            Some(EntryRef::File {
                digest: Digest([4; 32]),
                size: 7,
                executable: false,
            })
        );
        // Entries iterate sorted byte-lex regardless of kind.
        let names: Vec<&[u8]> = root.entries().map(|(n, _)| n).collect();
        assert_eq!(names, vec![&b"README"[..], b"bin", b"latest", b"src"]);
    }

    /// Re-putting an identical tree writes zero new records: every
    /// blob digest is already `contains`-known before the second put,
    /// and the pack files do not grow.
    #[test]
    fn dedup_re_put_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let tree = realistic_tree();
        let first = store.put_tree(&tree).unwrap();
        store.flush().unwrap();

        let folded = tree.fold().unwrap();
        for digest in folded.digests() {
            assert!(
                store.pack().contains(&digest),
                "{digest} must be present before the re-put"
            );
        }
        let pack_bytes_before = total_pack_bytes(tmp.path());
        let second = store.put_tree(&tree).unwrap();
        store.flush().unwrap();
        assert_eq!(first, second);
        assert_eq!(
            total_pack_bytes(tmp.path()),
            pack_bytes_before,
            "identical re-put must not grow the packs"
        );
    }

    fn total_pack_bytes(dir: &std::path::Path) -> u64 {
        let mut total = 0;
        for entry in std::fs::read_dir(dir.join("packs")).unwrap() {
            total += entry.unwrap().metadata().unwrap().len();
        }
        total
    }

    /// `get_tracked` reports a decode exactly once per digest; every
    /// later fetch is a cache hit.
    #[test]
    fn get_tracked_reports_decode_only_on_first_touch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let root_digest = store.put_tree(&realistic_tree()).unwrap();
        let (a, decoded_a) = store.get_tracked(&root_digest).unwrap();
        assert!(decoded_a, "first touch must decode");
        let (b, decoded_b) = store.get_tracked(&root_digest).unwrap();
        assert!(!decoded_b, "second touch must be a cache hit");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn cache_hit_returns_the_same_arc() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let root_digest = store.put_tree(&realistic_tree()).unwrap();
        let a = store.get(&root_digest).unwrap();
        let b = store.get(&root_digest).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "hit path must be lookup + Arc clone");
    }

    #[test]
    fn missing_digest_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        assert!(matches!(
            store.get(&Digest([0xAB; 32])),
            Err(DirStoreError::NotFound(_))
        ));
    }

    /// Bytes that are not a Directory message → named decode error.
    #[test]
    fn garbage_blob_is_a_decode_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        // Wire-invalid: tag 1 declares LEN 200 with 3 payload bytes.
        let garbage = [0x0a, 0xc8, 0x01, 0xff, 0xff];
        let digest = store.pack_mut().put(Kind::DIRECTORY, &garbage).unwrap();
        assert!(matches!(
            store.get(&digest),
            Err(DirStoreError::Decode { .. })
        ));
    }

    /// A decodable body that violates the structural invariants
    /// (unsorted entries) → named validation error.
    #[test]
    fn unsorted_blob_is_an_invalid_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let unsorted = Directory {
            files: vec![
                FileEntry {
                    name: b"b".to_vec(),
                    digest: vec![1; 32],
                    size: 1,
                    executable: false,
                },
                FileEntry {
                    name: b"a".to_vec(),
                    digest: vec![1; 32],
                    size: 1,
                    executable: false,
                },
            ],
            ..Default::default()
        };
        let digest = store
            .pack_mut()
            .put(Kind::DIRECTORY, &unsorted.encode_to_vec())
            .unwrap();
        assert!(matches!(
            store.get(&digest),
            Err(DirStoreError::Invalid { .. })
        ));
    }

    /// A body that decodes and validates but is not a canonical encode
    /// (unknown trailing field survives decode, vanishes on re-encode)
    /// → digest-mismatch hard error, never a cached lie.
    #[test]
    fn non_canonical_blob_is_a_digest_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let body = Directory {
            files: vec![FileEntry {
                name: b"a".to_vec(),
                digest: vec![1; 32],
                size: 1,
                executable: false,
            }],
            ..Default::default()
        };
        let mut bytes = body.encode_to_vec();
        // Unknown field 7, varint 1 — prost decodes and discards it.
        bytes.extend_from_slice(&[0x38, 0x01]);
        let digest = store.pack_mut().put(Kind::DIRECTORY, &bytes).unwrap();
        assert!(matches!(
            store.get(&digest),
            Err(DirStoreError::DigestMismatch { .. })
        ));
    }

    /// ~10k distinct directories plus one 10k-entry directory: a smoke
    /// test that the walk is decode-once + binary-search lookups, not
    /// an accidental O(n²). No wall-clock assert (builder variance) —
    /// an O(n²) regression shows up as this test visibly hanging.
    #[test]
    fn ten_k_dir_tree_lookup_sanity() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());

        let mut root = BuiltDir::new();
        for i in 0..100u8 {
            let mut mid = BuiltDir::new();
            for j in 0..100u8 {
                let mut leaf = BuiltDir::new();
                // Distinct file digests → distinct leaf bodies; without
                // this the whole tree dedups to 3 blobs.
                let mut d = [0u8; 32];
                d[0] = i;
                d[1] = j;
                leaf.push(&b"data"[..], file(d, 1, false));
                mid.push(format!("leaf-{j:03}").into_bytes(), BuiltEntry::Dir(leaf));
            }
            root.push(format!("mid-{i:03}").into_bytes(), BuiltEntry::Dir(mid));
        }
        let mut wide = BuiltDir::new();
        for k in 0..10_000u16 {
            let mut d = [0u8; 32];
            d[..2].copy_from_slice(&k.to_le_bytes());
            wide.push(format!("file-{k:05}").into_bytes(), file(d, 1, false));
        }
        root.push(&b"wide"[..], BuiltEntry::Dir(wide));

        let root_digest = store.put_tree(&root).unwrap();

        // Full warm walk: every mid, every leaf, every wide entry.
        let root_dir = store.get(&root_digest).unwrap();
        for i in 0..100u8 {
            let Some(EntryRef::Dir { digest, size }) =
                root_dir.child(format!("mid-{i:03}").as_bytes())
            else {
                panic!("mid-{i:03} missing");
            };
            // 100 leaf dirs + 100 files = recursive descendant count.
            assert_eq!(size, 200);
            let mid = store.get(&digest).unwrap();
            for j in 0..100u8 {
                let Some(EntryRef::Dir { digest, .. }) =
                    mid.child(format!("leaf-{j:03}").as_bytes())
                else {
                    panic!("leaf-{j:03} missing");
                };
                let leaf = store.get(&digest).unwrap();
                assert!(matches!(
                    leaf.child(b"data"),
                    Some(EntryRef::File { size: 1, .. })
                ));
            }
        }
        let Some(EntryRef::Dir { digest, size }) = root_dir.child(b"wide") else {
            panic!("wide missing");
        };
        assert_eq!(size, 10_000);
        let wide_dir = store.get(&digest).unwrap();
        assert_eq!(wide_dir.len(), 10_000);
        for k in (0..10_000u16).step_by(7) {
            assert!(wide_dir.child(format!("file-{k:05}").as_bytes()).is_some());
        }
        // Second full pass must be pure cache hits — same Arcs.
        let again = store.get(&root_digest).unwrap();
        assert!(Arc::ptr_eq(&root_dir, &again));
    }
}
