//! Directory-DAG delta-sync: discover what changed, fetch only that.
//!
//! [`sync_closure`] copies a set of store paths from a remote rio-store
//! into the gateway's local rio-store without transferring the bytes
//! the local store already holds. Discovery is a BFS over the remote's
//! castore Directory DAG, pruned at every subtree whose `dir_digest`
//! the local store already has — RPC count and transfer volume scale
//! with the *change* between the two stores, not with the closure size
//! (ADR-022 §8).
//!
//! Per root path:
//!
//! 1. `GetNarIndex(nar_hash)` on the remote → `root_digest` + the flat
//!    per-file entry list (the NAR's structure).
//! 2. BFS from `root_digest`: `HasDirectories(frontier)` against the
//!    **local** store; present digests prune their whole subtree;
//!    absent ones are fetched from the remote (`GetDirectory`,
//!    non-recursive) and their children become the next frontier.
//!    File digests under *changed* directories are collected.
//! 3. `HasBlobs(collected)` against the local store; the absent ones
//!    are the only file contents fetched from the remote
//!    (`ReadBlob`).
//! 4. The NAR is reassembled from the `NarIndex` entries + file
//!    contents (remote-fetched for the changed files, local
//!    `ReadBlob` for everything else) and written to the local store
//!    via the existing `PutPath` machinery, which independently
//!    re-hashes the NAR against the remote's declared `nar_hash` —
//!    that check is the end-to-end integrity gate for the whole
//!    reassembly.
//!
//! The reassembly source is the `NarIndex` entry list rather than the
//! Directory bodies themselves: both describe the same tree, but the
//! entry list is per-path (no cross-path digest bookkeeping) and is
//! already fetched for the `root_digest`. The Directory DAG's job here
//! is *discovery* — partitioning file digests into "already local" and
//! "must fetch" in O(changed-subtrees) round trips.

use std::collections::{HashMap, HashSet};

use rio_nix::nar::{self, NarEntry, NarNode};
use rio_proto::types::{NarIndex, NarIndexEntry};

/// Counters accumulated over one [`sync_closure`] call. Emitted as
/// `rio_gateway_dagsync_*` by the caller; kept as a plain struct so
/// the walk is unit-testable without a metrics recorder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyncStats {
    /// Subtree roots skipped because the local store already had the
    /// `dir_digest`. Each one is a whole subtree the BFS never
    /// descended into.
    pub subtrees_pruned: u64,
    /// Directory bodies fetched from the remote (= changed
    /// directories discovered).
    pub dirs_fetched: u64,
    /// File contents fetched from the remote (`ReadBlob`).
    pub blobs_fetched: u64,
    /// Bytes of file content NOT transferred from the remote because
    /// the local store already held the blob (summed over the file
    /// entries of every synced NAR).
    pub bytes_saved: u64,
    /// Bytes of file content transferred from the remote.
    pub bytes_fetched: u64,
}

/// Errors from the delta-sync walk. Every variant aborts the sync for
/// the affected paths; the caller falls back to the whole-NAR client
/// push (the paths stay "missing" in the `wopQueryValidPaths` reply).
#[derive(Debug, thiserror::Error)]
pub(crate) enum DagSyncError {
    /// A castore RPC failed.
    #[error("{rpc}: {status}")]
    Rpc {
        rpc: &'static str,
        status: tonic::Status,
    },
    /// A digest field on the wire was not 32 bytes.
    #[error("malformed digest ({len} bytes, want 32) in {context}")]
    BadDigest { len: usize, context: String },
    /// A `HasBitmap` reply was shorter than the request demanded.
    #[error("{rpc} bitmap is {got} bytes, need {need} for {n} digests")]
    ShortBitmap {
        rpc: &'static str,
        got: usize,
        need: usize,
        n: usize,
    },
    /// The fetched blob's length disagrees with the NAR index entry.
    #[error("blob {digest} is {got} bytes, index says {want}")]
    BlobSizeMismatch { digest: String, got: u64, want: u64 },
    /// The `NarIndex` entry list does not describe a well-formed tree.
    #[error("malformed NAR index for {store_path}: {reason}")]
    BadIndex { store_path: String, reason: String },
    /// The BFS discovered more directories than the safety cap.
    #[error("directory walk exceeded {0} digests")]
    WalkTooLarge(usize),
}

/// Hard ceiling on distinct directory digests one walk may discover.
/// Mirrors the server's `GET_DIRECTORY_MAX_RESULTS` — a chromium-scale
/// closure is ~8k dirs, so 256k means a pathological DAG or a bug.
const WALK_MAX_DIRS: usize = 262_144;

/// `HasDirectories` / `HasBlobs` request batch cap. The server rejects
/// batches over 65 536; stay well under it so one frontier level never
/// has to be split server-side.
const HAS_BATCH: usize = 4096;

/// The castore presence/fetch surface the walk needs from a store.
///
/// Two implementations: `GrpcCastore` (over a `DirectoryServiceClient`)
/// in `substitute::mod`, and the in-memory fixtures in this module's
/// tests. `async fn` in a trait is fine here — both call sites are
/// statically dispatched (`L: CastoreView`, `R: CastoreView`), never
/// boxed.
#[allow(async_fn_in_trait)]
pub(crate) trait CastoreView {
    /// Presence bitmap over the `directories` table. `out[i]` ⇔
    /// `digests[i]` is present and tenant-visible.
    async fn has_directories(&mut self, digests: &[[u8; 32]]) -> Result<Vec<bool>, DagSyncError>;
    /// Presence bitmap over the `file_blobs` table.
    async fn has_blobs(&mut self, digests: &[[u8; 32]]) -> Result<Vec<bool>, DagSyncError>;
    /// One Directory body, non-recursive. The implementation returns
    /// the children's `(dir_digest)` list and the `(file_digest, size)`
    /// list — the walk never needs the body itself.
    async fn get_directory(&mut self, digest: [u8; 32]) -> Result<DirectoryChildren, DagSyncError>;
    /// A regular file's full contents by `file_digest`.
    async fn read_blob(&mut self, digest: [u8; 32]) -> Result<Vec<u8>, DagSyncError>;
}

/// The two child lists of one Directory body that the BFS consumes.
#[derive(Debug, Default, Clone)]
pub(crate) struct DirectoryChildren {
    pub dirs: Vec<[u8; 32]>,
    /// `(file_digest, size)` per regular-file child. The size feeds
    /// `bytes_saved` accounting without a second lookup.
    pub files: Vec<([u8; 32], u64)>,
}

/// Decode an LSB-first presence bitmap (`HasBitmap.bitmap`) into one
/// bool per requested digest. Bit `i` of the reply corresponds to
/// `digests[i]`; bit order within a byte is LSB-first (the server's
/// `build_bitmap`).
pub(crate) fn decode_bitmap(
    rpc: &'static str,
    bitmap: &[u8],
    n: usize,
) -> Result<Vec<bool>, DagSyncError> {
    let need = n.div_ceil(8);
    if bitmap.len() < need {
        return Err(DagSyncError::ShortBitmap {
            rpc,
            got: bitmap.len(),
            need,
            n,
        });
    }
    Ok((0..n)
        .map(|i| bitmap[i / 8] & (1 << (i % 8)) != 0)
        .collect())
}

/// `&[u8] → [u8; 32]` with a typed error naming the malformed field.
pub(crate) fn digest32(bytes: &[u8], context: &str) -> Result<[u8; 32], DagSyncError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| DagSyncError::BadDigest {
        len: bytes.len(),
        context: context.to_string(),
    })
}

/// Outcome of the discovery phase: which file digests live under
/// *changed* directories (candidates for a remote fetch).
#[derive(Debug, Default)]
pub(crate) struct WalkOutcome {
    /// `file_digest → size` for every regular file under a changed
    /// directory, deduped on digest.
    pub candidate_files: HashMap<[u8; 32], u64>,
}

/// BFS the Directory DAG from `roots`, pruning at every subtree the
/// local store already holds.
///
/// `local` answers presence; `remote` serves the bodies of the
/// directories the local store lacks. Returns the set of file digests
/// that *might* need fetching (those under changed directories — a
/// file under a pruned subtree is guaranteed locally present because
/// the local store indexes a path's directories and file blobs in one
/// transaction).
// r[impl gw.substitute.dag-delta-sync]
pub(crate) async fn walk_dag<L: CastoreView, R: CastoreView>(
    local: &mut L,
    remote: &mut R,
    roots: &[[u8; 32]],
    stats: &mut SyncStats,
) -> Result<WalkOutcome, DagSyncError> {
    let mut seen: HashSet<[u8; 32]> = roots.iter().copied().collect();
    let mut frontier: Vec<[u8; 32]> = seen.iter().copied().collect();
    let mut out = WalkOutcome::default();

    while !frontier.is_empty() {
        if seen.len() > WALK_MAX_DIRS {
            return Err(DagSyncError::WalkTooLarge(WALK_MAX_DIRS));
        }
        let mut next: Vec<[u8; 32]> = Vec::new();
        for batch in frontier.chunks(HAS_BATCH) {
            let present = local.has_directories(batch).await?;
            for (digest, present) in batch.iter().zip(present) {
                if present {
                    // The local store already holds this directory —
                    // and therefore (single-transaction indexing) the
                    // whole subtree under it plus its file blobs.
                    stats.subtrees_pruned += 1;
                    continue;
                }
                let children = remote.get_directory(*digest).await?;
                stats.dirs_fetched += 1;
                for child in children.dirs {
                    if seen.insert(child) {
                        next.push(child);
                    }
                }
                for (file, size) in children.files {
                    out.candidate_files.entry(file).or_insert(size);
                }
            }
        }
        frontier = next;
    }
    Ok(out)
}

/// Partition `candidates` into locally-present and absent, fetch the
/// absent ones from the remote, and return the fetched contents keyed
/// by digest. Locally-present candidates are simply skipped here —
/// the per-NAR reassembly reads them from the local store and credits
/// `bytes_saved` at that point (one accounting site for every
/// locally-sourced byte, whether the file was a candidate or lives
/// under a pruned subtree).
pub(crate) async fn fetch_missing_blobs<L: CastoreView, R: CastoreView>(
    local: &mut L,
    remote: &mut R,
    candidates: &HashMap<[u8; 32], u64>,
    stats: &mut SyncStats,
) -> Result<HashMap<[u8; 32], Vec<u8>>, DagSyncError> {
    let mut fetched: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
    if candidates.is_empty() {
        return Ok(fetched);
    }
    // Deterministic order for the batch boundaries (HashMap iteration
    // order is arbitrary; the RPC result is order-coupled to the
    // request).
    let mut digests: Vec<[u8; 32]> = candidates.keys().copied().collect();
    digests.sort_unstable();
    for batch in digests.chunks(HAS_BATCH) {
        let present = local.has_blobs(batch).await?;
        for (digest, present) in batch.iter().zip(present) {
            let want = candidates[digest];
            if present {
                continue;
            }
            let body = remote.read_blob(*digest).await?;
            if body.len() as u64 != want {
                return Err(DagSyncError::BlobSizeMismatch {
                    digest: hex::encode(digest),
                    got: body.len() as u64,
                    want,
                });
            }
            stats.blobs_fetched += 1;
            stats.bytes_fetched += body.len() as u64;
            fetched.insert(*digest, body);
        }
    }
    Ok(fetched)
}

/// One parsed `NarIndexEntry` with the fields reassembly needs and the
/// path split out for tree-building.
struct IndexEntry {
    /// '/'-separated components of `NarIndexEntry.path`. Empty for the
    /// root.
    components: Vec<String>,
    kind: i32,
    size: u64,
    executable: bool,
    target: Vec<u8>,
    file_digest: Option<[u8; 32]>,
}

/// The distinct `file_digest → size` set of one NAR's regular files.
/// The caller resolves every digest to its content (remote-fetched
/// map, or a local `ReadBlob`) before calling [`assemble_nar`].
pub(crate) fn file_digests_of(
    store_path: &str,
    index: &NarIndex,
) -> Result<HashMap<[u8; 32], u64>, DagSyncError> {
    let mut out = HashMap::new();
    for e in &index.entries {
        if e.kind == rio_proto::types::NarEntryKind::Regular as i32 {
            let d = digest32(
                &e.file_digest,
                &format!("file_digest of {:?} in {store_path}", e.path.escape_ascii()),
            )?;
            out.entry(d).or_insert(e.size);
        }
    }
    Ok(out)
}

/// Reassemble a NAR from its index entries and a content source.
///
/// `content(digest, size)` returns the file body for one
/// `file_digest` — the caller backs it with the remote-fetched map
/// first and the local store second (pre-resolved via
/// [`file_digests_of`], so the lookup here is synchronous). Entries
/// arrive in DFS pre-order with children already byte-lex sorted (the
/// indexer validates this on the source NAR), so the rebuilt tree
/// serializes to the same canonical bytes the remote holds;
/// `PutPath`'s independent re-hash is the proof.
pub(crate) fn assemble_nar<F>(
    store_path: &str,
    index: &NarIndex,
    mut content: F,
) -> Result<Vec<u8>, DagSyncError>
where
    F: FnMut([u8; 32], u64) -> Result<Vec<u8>, DagSyncError>,
{
    let bad = |reason: String| DagSyncError::BadIndex {
        store_path: store_path.to_string(),
        reason,
    };
    if index.entries.is_empty() {
        return Err(bad("empty entry list".into()));
    }
    let mut parsed: Vec<IndexEntry> = Vec::with_capacity(index.entries.len());
    for e in &index.entries {
        parsed.push(parse_entry(store_path, e)?);
    }
    if !parsed[0].components.is_empty() {
        return Err(bad(format!(
            "first entry is not the root (path {:?})",
            index.entries[0].path.escape_ascii()
        )));
    }

    // Build the tree from the DFS pre-order list: a stack of open
    // directories; each entry attaches to the deepest open directory
    // that is its parent (depth = components.len() - 1).
    let root = build_tree(store_path, &parsed, &mut content)?;
    let mut buf = Vec::new();
    nar::serialize(&mut buf, &root).map_err(|e| bad(format!("NAR serialize: {e}")))?;
    Ok(buf)
}

/// Convert one wire entry into the parsed form, validating digests and
/// UTF-8 up front so the tree builder can't half-construct a NAR.
fn parse_entry(store_path: &str, e: &NarIndexEntry) -> Result<IndexEntry, DagSyncError> {
    let bad = |reason: String| DagSyncError::BadIndex {
        store_path: store_path.to_string(),
        reason,
    };
    // NAR entry names are raw bytes on the wire; `NarNode` (and the
    // local store's own NAR parser) require UTF-8. A non-UTF-8 name is
    // exotic enough that falling back to the whole-NAR path is the
    // right answer rather than lossy-converting it.
    let path = String::from_utf8(e.path.clone())
        .map_err(|_| bad(format!("non-UTF-8 entry path {:?}", e.path)))?;
    let components: Vec<String> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').map(str::to_owned).collect()
    };
    if components.iter().any(String::is_empty) {
        return Err(bad(format!("entry path {path:?} has an empty component")));
    }
    let kind = e.kind;
    let file_digest = match rio_proto::types::NarEntryKind::try_from(kind) {
        Ok(rio_proto::types::NarEntryKind::Regular) => Some(digest32(
            &e.file_digest,
            &format!("file_digest of {path:?}"),
        )?),
        Ok(rio_proto::types::NarEntryKind::Directory | rio_proto::types::NarEntryKind::Symlink) => {
            None
        }
        _ => return Err(bad(format!("entry {path:?} has unknown kind {kind}"))),
    };
    Ok(IndexEntry {
        components,
        kind,
        size: e.size,
        executable: e.executable,
        target: e.target.clone(),
        file_digest,
    })
}

/// Materialize the [`NarNode`] tree from the DFS pre-order entry list.
///
/// Iterative-with-explicit-stack rather than recursive so a 256-deep
/// NAR (the parser's `MAX_NAR_DEPTH`) costs heap, not call-stack.
fn build_tree<F>(
    store_path: &str,
    entries: &[IndexEntry],
    content: &mut F,
) -> Result<NarNode, DagSyncError>
where
    F: FnMut([u8; 32], u64) -> Result<Vec<u8>, DagSyncError>,
{
    let bad = |reason: String| DagSyncError::BadIndex {
        store_path: store_path.to_string(),
        reason,
    };

    // Leaf-node constructor shared by the root-is-a-leaf case and the
    // directory-child case.
    fn leaf<F>(store_path: &str, e: &IndexEntry, content: &mut F) -> Result<NarNode, DagSyncError>
    where
        F: FnMut([u8; 32], u64) -> Result<Vec<u8>, DagSyncError>,
    {
        match rio_proto::types::NarEntryKind::try_from(e.kind) {
            Ok(rio_proto::types::NarEntryKind::Regular) => {
                // parse_entry guarantees Some for Regular.
                let digest = e.file_digest.expect("regular entry has a file_digest");
                let body = content(digest, e.size)?;
                if body.len() as u64 != e.size {
                    return Err(DagSyncError::BlobSizeMismatch {
                        digest: hex::encode(digest),
                        got: body.len() as u64,
                        want: e.size,
                    });
                }
                Ok(NarNode::Regular {
                    executable: e.executable,
                    contents: body,
                })
            }
            Ok(rio_proto::types::NarEntryKind::Symlink) => {
                let target =
                    String::from_utf8(e.target.clone()).map_err(|_| DagSyncError::BadIndex {
                        store_path: store_path.to_string(),
                        reason: format!("non-UTF-8 symlink target {:?}", e.target),
                    })?;
                Ok(NarNode::Symlink { target })
            }
            _ => Err(DagSyncError::BadIndex {
                store_path: store_path.to_string(),
                reason: "leaf() called on a directory entry".into(),
            }),
        }
    }

    // Root is a single file or symlink: one entry, no tree.
    if entries[0].kind != rio_proto::types::NarEntryKind::Directory as i32 {
        if entries.len() != 1 {
            return Err(bad(format!(
                "non-directory root followed by {} more entries",
                entries.len() - 1
            )));
        }
        return leaf(store_path, &entries[0], content);
    }

    // Stack of directories currently being filled. `depth` of an entry
    // is `components.len()`; the root directory is depth 0 and its
    // children are depth 1, so an entry at depth d is a child of
    // stack[d-1]. Pre-order guarantees the parent was pushed before
    // any of its descendants appear.
    let mut stack: Vec<Vec<NarEntry>> = vec![Vec::new()];
    for (i, e) in entries.iter().enumerate().skip(1) {
        let depth = e.components.len();
        if depth == 0 {
            return Err(bad("second root entry in the index".into()));
        }
        // Close finished directories until the stack is exactly
        // `depth` deep (the parent of this entry is the top).
        while stack.len() > depth {
            let done = stack.pop().expect("len > depth >= 1");
            let parent = stack.last_mut().expect("root never popped here");
            let last = parent.last_mut().ok_or_else(|| {
                bad(format!(
                    "entry {i} closes a directory that has no open parent slot"
                ))
            })?;
            last.node = NarNode::Directory { entries: done };
        }
        if stack.len() < depth {
            return Err(bad(format!(
                "entry {:?} skips a level (stack depth {}, entry depth {depth})",
                e.components.join("/"),
                stack.len()
            )));
        }
        let name = e
            .components
            .last()
            .expect("depth >= 1 implies a last component")
            .clone();
        if e.kind == rio_proto::types::NarEntryKind::Directory as i32 {
            // Placeholder; replaced when the subtree closes.
            stack
                .last_mut()
                .expect("stack is never empty")
                .push(NarEntry {
                    name,
                    node: NarNode::Directory {
                        entries: Vec::new(),
                    },
                });
            stack.push(Vec::new());
        } else {
            let node = leaf(store_path, e, content)?;
            stack
                .last_mut()
                .expect("stack is never empty")
                .push(NarEntry { name, node });
        }
    }
    // Close everything still open down to the root.
    while stack.len() > 1 {
        let done = stack.pop().expect("len > 1");
        let parent = stack.last_mut().expect("len >= 1 after pop");
        let last = parent
            .last_mut()
            .ok_or_else(|| bad("trailing directory close has no parent slot".into()))?;
        last.node = NarNode::Directory { entries: done };
    }
    let root_entries = stack.pop().expect("root frame");
    Ok(NarNode::Directory {
        entries: root_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::NarEntryKind;

    /// In-memory castore for the walk tests: a set of present
    /// directory digests, a set of present blob digests, a body map,
    /// and a call log so tests can assert *which* RPCs happened.
    #[derive(Default)]
    struct MemCastore {
        dirs_present: HashSet<[u8; 32]>,
        blobs_present: HashSet<[u8; 32]>,
        bodies: HashMap<[u8; 32], DirectoryChildren>,
        blobs: HashMap<[u8; 32], Vec<u8>>,
        get_directory_calls: Vec<[u8; 32]>,
        read_blob_calls: Vec<[u8; 32]>,
    }

    impl CastoreView for MemCastore {
        async fn has_directories(
            &mut self,
            digests: &[[u8; 32]],
        ) -> Result<Vec<bool>, DagSyncError> {
            Ok(digests
                .iter()
                .map(|d| self.dirs_present.contains(d))
                .collect())
        }
        async fn has_blobs(&mut self, digests: &[[u8; 32]]) -> Result<Vec<bool>, DagSyncError> {
            Ok(digests
                .iter()
                .map(|d| self.blobs_present.contains(d))
                .collect())
        }
        async fn get_directory(
            &mut self,
            digest: [u8; 32],
        ) -> Result<DirectoryChildren, DagSyncError> {
            self.get_directory_calls.push(digest);
            self.bodies.get(&digest).cloned().ok_or(DagSyncError::Rpc {
                rpc: "GetDirectory",
                status: tonic::Status::not_found("no such directory"),
            })
        }
        async fn read_blob(&mut self, digest: [u8; 32]) -> Result<Vec<u8>, DagSyncError> {
            self.read_blob_calls.push(digest);
            self.blobs.get(&digest).cloned().ok_or(DagSyncError::Rpc {
                rpc: "ReadBlob",
                status: tonic::Status::not_found("no such blob"),
            })
        }
    }

    fn d(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// LSB-first decoding, length validation, and the empty case.
    #[test]
    fn bitmap_decoding_is_lsb_first() {
        // 10 digests → 2 bytes. Bits 0, 3, 9 set.
        let bits = decode_bitmap("t", &[0b0000_1001, 0b0000_0010], 10).unwrap();
        let expected: Vec<bool> = (0..10).map(|i| i == 0 || i == 3 || i == 9).collect();
        assert_eq!(bits, expected);
        assert!(decode_bitmap("t", &[], 0).unwrap().is_empty());
        // Short reply is an error, not a silent all-absent.
        let err = decode_bitmap("t", &[0xFF], 9).unwrap_err();
        assert!(matches!(err, DagSyncError::ShortBitmap { need: 2, .. }));
    }

    /// The BFS never descends into a subtree the local store already
    /// has: the pruned subtree's children are not enumerated against
    /// the remote and its files are not blob-fetch candidates.
    // r[verify gw.substitute.dag-delta-sync]
    #[tokio::test]
    async fn walk_prunes_locally_present_subtrees() {
        // Remote tree:        root(1)
        //                    /       \
        //         changed(2)          unchanged(3)
        //         file A              file B, child dir(4) { file C }
        let mut remote = MemCastore::default();
        remote.bodies.insert(
            d(1),
            DirectoryChildren {
                dirs: vec![d(2), d(3)],
                files: vec![],
            },
        );
        remote.bodies.insert(
            d(2),
            DirectoryChildren {
                dirs: vec![],
                files: vec![(d(0xA), 5)],
            },
        );
        remote.bodies.insert(
            d(3),
            DirectoryChildren {
                dirs: vec![d(4)],
                files: vec![(d(0xB), 7)],
            },
        );
        // Local store has subtree 3 (and everything under it).
        let mut local = MemCastore::default();
        local.dirs_present.insert(d(3));

        let mut stats = SyncStats::default();
        let out = walk_dag(&mut local, &mut remote, &[d(1)], &mut stats)
            .await
            .unwrap();

        assert_eq!(stats.subtrees_pruned, 1, "subtree 3 pruned once");
        assert_eq!(stats.dirs_fetched, 2, "root + changed dir fetched");
        assert_eq!(
            remote.get_directory_calls,
            vec![d(1), d(2)],
            "the pruned subtree (3) and its child (4) are never fetched from the remote"
        );
        assert_eq!(
            out.candidate_files,
            HashMap::from([(d(0xA), 5)]),
            "only files under CHANGED directories are fetch candidates"
        );
    }

    /// A digest reachable through two parents is visited once (the DAG
    /// is a DAG, not a tree).
    #[tokio::test]
    async fn walk_dedups_shared_subtrees() {
        let mut remote = MemCastore::default();
        remote.bodies.insert(
            d(1),
            DirectoryChildren {
                dirs: vec![d(2), d(3)],
                files: vec![],
            },
        );
        // Both 2 and 3 contain the same child 4.
        for parent in [2u8, 3] {
            remote.bodies.insert(
                d(parent),
                DirectoryChildren {
                    dirs: vec![d(4)],
                    files: vec![],
                },
            );
        }
        remote.bodies.insert(d(4), DirectoryChildren::default());
        let mut local = MemCastore::default();
        let mut stats = SyncStats::default();
        walk_dag(&mut local, &mut remote, &[d(1)], &mut stats)
            .await
            .unwrap();
        assert_eq!(stats.dirs_fetched, 4, "1, 2, 3, 4 — 4 exactly once");
        assert_eq!(
            remote
                .get_directory_calls
                .iter()
                .filter(|x| **x == d(4))
                .count(),
            1
        );
    }

    /// Blob partitioning: locally-present candidates are never
    /// fetched; absent ones are fetched and length-checked.
    // r[verify gw.substitute.dag-delta-sync]
    #[tokio::test]
    async fn fetch_missing_blobs_partitions_local_and_remote() {
        let mut local = MemCastore::default();
        local.blobs_present.insert(d(0xA));
        let mut remote = MemCastore::default();
        remote.blobs.insert(d(0xB), b"seven b".to_vec());
        let candidates = HashMap::from([(d(0xA), 5u64), (d(0xB), 7u64)]);
        let mut stats = SyncStats::default();
        let fetched = fetch_missing_blobs(&mut local, &mut remote, &candidates, &mut stats)
            .await
            .unwrap();
        assert_eq!(stats.blobs_fetched, 1);
        assert_eq!(
            stats.bytes_saved, 0,
            "bytes_saved is credited at reassembly time (when the local read happens), not here"
        );
        assert_eq!(stats.bytes_fetched, 7);
        assert_eq!(fetched, HashMap::from([(d(0xB), b"seven b".to_vec())]));
        assert_eq!(remote.read_blob_calls, vec![d(0xB)]);

        // A blob whose length disagrees with the index is rejected —
        // feeding it into the NAR would produce a hash mismatch at
        // PutPath anyway, but failing here names the digest.
        let mut remote = MemCastore::default();
        remote.blobs.insert(d(0xB), b"short".to_vec());
        let err = fetch_missing_blobs(
            &mut MemCastore::default(),
            &mut remote,
            &HashMap::from([(d(0xB), 7u64)]),
            &mut SyncStats::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DagSyncError::BlobSizeMismatch { .. }));
    }

    /// Helper: a NarIndexEntry literal.
    fn entry(path: &str, kind: NarEntryKind, size: u64, exec: bool, digest: u8) -> NarIndexEntry {
        NarIndexEntry {
            path: path.as_bytes().to_vec(),
            kind: kind as i32,
            size,
            executable: exec,
            nar_offset: 0,
            target: Vec::new(),
            file_digest: if kind == NarEntryKind::Regular {
                vec![digest; 32]
            } else {
                Vec::new()
            },
            dir_digest: Vec::new(),
        }
    }

    /// Digest tags for the reassembly fixtures: the `tool` blob and
    /// the `data` blob.
    const TOOL: u8 = 0x11;
    const DATA: u8 = 0x22;

    /// The reassembled NAR is byte-identical to serializing the same
    /// tree directly — the round-trip proof that the index-driven
    /// rebuild produces the canonical encoding `PutPath` will re-hash.
    // r[verify gw.substitute.dag-delta-sync]
    #[test]
    fn assemble_nar_matches_direct_serialization() {
        // tree: root/ { bin/ { tool* }, data, link -> bin/tool, sub/ { } }
        let mut sym = entry("link", NarEntryKind::Symlink, 0, false, 0);
        sym.target = b"bin/tool".to_vec();
        let index = NarIndex {
            entries: vec![
                entry("", NarEntryKind::Directory, 0, false, 0),
                entry("bin", NarEntryKind::Directory, 0, false, 0),
                entry("bin/tool", NarEntryKind::Regular, 4, true, TOOL),
                entry("data", NarEntryKind::Regular, 5, false, DATA),
                sym,
                entry("sub", NarEntryKind::Directory, 0, false, 0),
            ],
            root_digest: vec![0xEE; 32],
        };
        assert_eq!(
            file_digests_of("/nix/store/x-test", &index).unwrap(),
            HashMap::from([(d(TOOL), 4u64), (d(DATA), 5u64)]),
            "distinct regular-file digests with their sizes"
        );
        let blobs: HashMap<[u8; 32], Vec<u8>> =
            HashMap::from([(d(TOOL), b"exec".to_vec()), (d(DATA), b"hello".to_vec())]);
        let got = assemble_nar("/nix/store/x-test", &index, |digest, _| {
            blobs.get(&digest).cloned().ok_or(DagSyncError::Rpc {
                rpc: "ReadBlob",
                status: tonic::Status::not_found("missing"),
            })
        })
        .unwrap();

        let want_tree = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "bin".into(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "tool".into(),
                            node: NarNode::Regular {
                                executable: true,
                                contents: b"exec".to_vec(),
                            },
                        }],
                    },
                },
                NarEntry {
                    name: "data".into(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"hello".to_vec(),
                    },
                },
                NarEntry {
                    name: "link".into(),
                    node: NarNode::Symlink {
                        target: "bin/tool".into(),
                    },
                },
                NarEntry {
                    name: "sub".into(),
                    node: NarNode::Directory { entries: vec![] },
                },
            ],
        };
        let mut want = Vec::new();
        nar::serialize(&mut want, &want_tree).unwrap();
        assert_eq!(got, want, "index-driven reassembly must be canonical");
    }

    /// Single-file and single-symlink roots (no directory tree).
    #[test]
    fn assemble_nar_leaf_roots() {
        let index = NarIndex {
            entries: vec![entry("", NarEntryKind::Regular, 3, true, 0xF)],
            root_digest: Vec::new(),
        };
        let got = assemble_nar("/nix/store/x-file", &index, |_, _| Ok(b"abc".to_vec())).unwrap();
        let mut want = Vec::new();
        nar::serialize(
            &mut want,
            &NarNode::Regular {
                executable: true,
                contents: b"abc".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(got, want);
    }

    /// Malformed indexes are rejected with a named reason instead of
    /// producing a garbage NAR that fails the PutPath hash check.
    #[test]
    fn assemble_nar_rejects_malformed_indexes() {
        let no_content = |_: [u8; 32], _: u64| Ok(Vec::new());
        let idx = |entries| NarIndex {
            entries,
            root_digest: vec![],
        };
        // Empty.
        let err = assemble_nar("/nix/store/x", &idx(vec![]), no_content).unwrap_err();
        assert!(matches!(err, DagSyncError::BadIndex { .. }), "{err}");
        // First entry is not the root.
        let err = assemble_nar(
            "/nix/store/x",
            &idx(vec![entry("a", NarEntryKind::Directory, 0, false, 0)]),
            no_content,
        )
        .unwrap_err();
        assert!(matches!(err, DagSyncError::BadIndex { .. }), "{err}");
        // An entry that skips a level (no parent directory entry).
        let err = assemble_nar(
            "/nix/store/x",
            &idx(vec![
                entry("", NarEntryKind::Directory, 0, false, 0),
                entry("a/b", NarEntryKind::Regular, 0, false, 1),
            ]),
            no_content,
        )
        .unwrap_err();
        assert!(matches!(err, DagSyncError::BadIndex { .. }), "{err}");
        // A non-directory root followed by more entries.
        let err = assemble_nar(
            "/nix/store/x",
            &idx(vec![
                entry("", NarEntryKind::Regular, 0, false, 1),
                entry("a", NarEntryKind::Regular, 0, false, 1),
            ]),
            no_content,
        )
        .unwrap_err();
        assert!(matches!(err, DagSyncError::BadIndex { .. }), "{err}");
        // A regular entry whose file_digest is not 32 bytes.
        let mut short = entry("a", NarEntryKind::Regular, 0, false, 1);
        short.file_digest = vec![1, 2, 3];
        let err = assemble_nar(
            "/nix/store/x",
            &idx(vec![entry("", NarEntryKind::Directory, 0, false, 0), short]),
            no_content,
        )
        .unwrap_err();
        assert!(
            matches!(err, DagSyncError::BadDigest { len: 3, .. }),
            "{err}"
        );
    }
}
