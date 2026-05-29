//! Derived per-path NAR file/dir/symlink listing (`nar_index` table).
//!
//! Encode/decode helpers for the `nar_index.entries` BYTEA. The index
//! is written exactly once, inside the manifest-complete transaction
//! (`metadata::set_nar_index_in_conn`, fed by `cas::ParsedNar`), and
//! is authoritative from that moment: the NAR byte stream is not
//! persisted (ADR-022 §6), so the index cannot be recomputed from the
//! stored per-file chunks after the fact. A `'complete'` path with no
//! index row is data loss, not a cache miss.

use prost::Message;

use rio_nix::nar::{NarEntryKind, NarLsEntry};
use rio_proto::types::{NarEntryKind as ProtoNarEntryKind, NarIndex, NarIndexEntry};

use crate::castore::DirectoryDag;

/// Worst-case encoded bytes of one [`NarIndexEntry`] *excluding* the
/// `path` and `target` content bytes (those are bounded across the
/// whole index by `MAX_NAR_INDEX_BYTES`). Every field number is ≤ 15,
/// so each tag is 1 byte; per field, tag + worst-case payload as
/// [`to_proto_entry`] encodes it:
///
/// - repeated-`entries` wrapper: 1 + 5 (message-length varint)
/// - `path`: 1 + 5 (length varint; content charged to the index cap)
/// - `kind`: 1 + 1
/// - `size`: 1 + 10 (u64 varint)
/// - `executable`: 1 + 1
/// - `nar_offset`: 1 + 10
/// - `target`: 1 + 2 (length varint ≤ 4096; content charged to the cap)
/// - `file_digest`: 1 + 1 + 32
/// - `dir_digest`: 1 + 1 + 32 (mutually exclusive with `file_digest`
///   in [`to_proto_entry`], but counted anyway so the bound does not
///   depend on that invariant)
const MAX_ENTRY_FIXED_OVERHEAD: usize = 6 + 6 + 2 + 11 + 2 + 11 + 3 + 34 + 34;

// The ingest caps are sized so the worst-case accepted NAR's encoded
// `nar_index` still fits one `GetNarIndex` response at the default gRPC
// message ceiling: fixed per-entry overhead × the entry cap, plus the
// path/target bytes the index-byte cap bounds, plus the one-off
// `root_digest` field (1 tag + 1 length + 32 bytes). Operators can lower
// `RIO_GRPC_MAX_MESSAGE_SIZE` below the default at runtime; staying
// servable under such a reduced ceiling is their concern, not this
// invariant's.
const _: () = assert!(
    MAX_ENTRY_FIXED_OVERHEAD * rio_nix::nar::MAX_NAR_ENTRIES
        + rio_nix::nar::MAX_NAR_INDEX_BYTES as usize
        + 34
        <= rio_common::grpc::DEFAULT_MAX_MESSAGE_SIZE,
    "a maximal accepted NAR's encoded nar_index must fit one GetNarIndex response"
);

/// Encode a `nar_ls` entry list as the `nar_index.entries` BYTEA, with
/// per-entry `dir_digest` from `dag`.
pub fn encode_entries(entries: &[NarLsEntry], dag: &DirectoryDag) -> Vec<u8> {
    NarIndex {
        entries: entries
            .iter()
            .zip(&dag.dir_digests)
            .map(|(e, d)| to_proto_entry(e, d))
            .collect(),
        root_digest: dag.root_digest.clone(),
    }
    .encode_to_vec()
}

/// Decode the `nar_index.entries` BYTEA back to a proto `NarIndex`.
pub fn decode_entries(bytes: &[u8]) -> anyhow::Result<NarIndex> {
    Ok(NarIndex::decode(bytes)?)
}

/// Distinct, sorted `dir_digest`s from `nar_index.entries` — the
/// per-path contribution to `directories.refcount` is one per unique
/// digest. The GC sweep reads this before the CASCADE removes the row.
// r[impl store.castore.gc]
pub fn digests_from_index(bytes: &[u8]) -> anyhow::Result<Vec<[u8; 32]>> {
    let idx = decode_entries(bytes)?;
    let mut dirs: Vec<[u8; 32]> = Vec::new();
    for e in &idx.entries {
        if e.dir_digest.len() == 32 {
            dirs.push(e.dir_digest.as_slice().try_into().expect("len checked"));
        }
    }
    dirs.sort_unstable();
    dirs.dedup();
    Ok(dirs)
}

/// `NarLsEntry` → wire `NarIndexEntry`. The in-memory `[0; 32]`
/// sentinel maps to the proto's empty-bytes sentinel.
fn to_proto_entry(e: &NarLsEntry, dir_digest: &[u8; 32]) -> NarIndexEntry {
    NarIndexEntry {
        path: e.path.clone(),
        kind: match e.kind {
            NarEntryKind::Regular => ProtoNarEntryKind::Regular,
            NarEntryKind::Directory => ProtoNarEntryKind::Directory,
            NarEntryKind::Symlink => ProtoNarEntryKind::Symlink,
        }
        .into(),
        size: e.size,
        executable: e.executable,
        nar_offset: e.nar_offset,
        target: e.target.clone(),
        file_digest: if e.kind == NarEntryKind::Regular {
            e.file_digest.to_vec()
        } else {
            Vec::new()
        },
        // r[impl store.index.dir-digest]
        dir_digest: if e.kind == NarEntryKind::Directory {
            dir_digest.to_vec()
        } else {
            Vec::new()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::castore;
    use rio_nix::nar::NarEntryKind;

    /// Encode/decode round-trip preserves order, content, and the
    /// per-entry `dir_digest` + `root_digest`. Also pins the proto
    /// contract: dirs/symlinks carry an EMPTY `file_digest` (not 32
    /// zeros), files/symlinks carry an EMPTY `dir_digest`, and the
    /// `kind` enum survives as its i32 wire value.
    #[test]
    fn encode_decode_roundtrip() {
        let entries = vec![
            NarLsEntry {
                path: b"".to_vec(),
                kind: NarEntryKind::Directory,
                size: 0,
                executable: false,
                nar_offset: 0,
                target: Vec::new(),
                file_digest: [0u8; 32],
            },
            NarLsEntry {
                path: b"a".to_vec(),
                kind: NarEntryKind::Regular,
                size: 3,
                executable: true,
                nar_offset: 100,
                target: Vec::new(),
                file_digest: [1u8; 32],
            },
            NarLsEntry {
                path: b"b".to_vec(),
                kind: NarEntryKind::Symlink,
                size: 0,
                executable: false,
                nar_offset: 0,
                target: b"a".to_vec(),
                file_digest: [0u8; 32],
            },
        ];
        let dag = castore::build(&entries);
        let bytes = encode_entries(&entries, &dag);
        let decoded = decode_entries(&bytes).unwrap();
        assert_eq!(decoded.entries.len(), 3);
        assert_eq!(decoded.entries[0].path, b"");
        assert_eq!(decoded.entries[0].kind, ProtoNarEntryKind::Directory as i32);
        assert_eq!(decoded.entries[0].dir_digest, dag.dir_digests[0].to_vec());
        assert_eq!(decoded.entries[1].kind, ProtoNarEntryKind::Regular as i32);
        assert_eq!(decoded.entries[1].file_digest, vec![1u8; 32]);
        assert_eq!(decoded.entries[1].nar_offset, 100);
        assert!(decoded.entries[1].executable);
        assert_eq!(decoded.entries[2].kind, ProtoNarEntryKind::Symlink as i32);
        assert_eq!(decoded.entries[2].target, b"a");
        assert!(decoded.entries[0].file_digest.is_empty());
        assert!(decoded.entries[1].dir_digest.is_empty());
        assert!(decoded.entries[2].file_digest.is_empty());
        assert!(decoded.entries[2].dir_digest.is_empty());
        assert_eq!(decoded.root_digest, dag.root_digest);

        // r[verify store.castore.gc]
        let dirs = digests_from_index(&bytes).unwrap();
        assert_eq!(dirs, vec![dag.dir_digests[0]]);
    }
}
