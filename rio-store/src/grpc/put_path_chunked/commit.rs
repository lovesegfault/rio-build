//! Commit-side assembly for `PutPathChunked` (ADR-022 §6.2).
//!
//! The legacy upload path derives everything the commit transaction
//! needs (`nar_index.entries`, the Directory DAG, `file_blobs` offsets)
//! from the buffered NAR via [`cas::ParsedNar::parse`]. `PutPathChunked`
//! never materializes the NAR, so the same artifacts must be derived
//! from the validated Directory tree instead. [`tree_to_entries`]
//! produces a [`NarLsEntry`] list **byte-for-byte identical** to what
//! `nar_ls` would emit over the regenerated NAR — the agreement test in
//! this module is what keeps a chunked commit's persisted index
//! indistinguishable from a legacy one.

use std::collections::HashMap;
use std::io::{self, Write};

use tonic::Status;

use rio_nix::nar::{NarEntryKind, NarLsEntry, frame};
use rio_proto::castore::{Directory, RootNode};

use crate::cas::ParsedNar;
use crate::castore_nar::{self, WalkError, WalkEvent};
use crate::manifest::{Manifest, ManifestEntry};

use super::validate::ValidatedOutput;

/// `io::Write` sink that only counts bytes. Drives the same
/// `frame::*` call sequence [`castore_nar::NarPieces`] emits so the
/// running total IS the NAR byte offset — without allocating any
/// framing.
struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Walk a validated Directory tree and produce the [`NarLsEntry`] list
/// `nar_ls` would produce over the regenerated NAR.
///
/// Each entry carries the `/`-joined path from the NAR root (root =
/// empty path) and, for regular files, the byte offset of its content
/// within the canonical NAR encoding — computed by feeding the exact
/// `frame::*` sequence `NarPieces::push_event` writes into a counting
/// sink. The `file_digest` is taken from the Directory body's
/// `FileEntry.digest` — server-verified against the run's content by
/// the verify walk / deferred file-digest proof before this runs
/// (`r[store.integrity.verify-on-put+3]`).
pub(super) fn tree_to_entries(
    root: &RootNode,
    dirs: &HashMap<[u8; 32], Directory>,
    max_nodes: usize,
) -> Result<Vec<NarLsEntry>, WalkError> {
    let mut counter = Counter(0);
    frame::magic(&mut counter).expect("counter write is infallible");

    let mut out = Vec::new();
    // Component names of every entered non-root directory, in nesting
    // order. The root directory contributes no component (its NAR path
    // is the empty string), so the stack depth lags `depth` by one.
    let mut components: Vec<Vec<u8>> = Vec::new();
    let mut depth = 0usize;

    // Joined path for an entry named `name` at the current nesting.
    // The root node itself (depth 0) has the empty path.
    let join = |components: &[Vec<u8>], name: &[u8], depth: usize| -> Vec<u8> {
        if depth == 0 {
            return Vec::new();
        }
        let mut p = Vec::new();
        for c in components {
            p.extend_from_slice(c);
            p.push(b'/');
        }
        p.extend_from_slice(name);
        p
    };

    for ev in castore_nar::walk(root, dirs, max_nodes) {
        match ev? {
            WalkEvent::EnterDir { name } => {
                let path = join(&components, name, depth);
                if depth > 0 {
                    frame::entry_open(&mut counter, name).expect("counter write");
                    components.push(name.to_vec());
                }
                frame::node_open(&mut counter).expect("counter write");
                frame::directory_open(&mut counter).expect("counter write");
                depth += 1;
                out.push(NarLsEntry {
                    path,
                    kind: NarEntryKind::Directory,
                    size: 0,
                    executable: false,
                    nar_offset: 0,
                    target: Vec::new(),
                    file_digest: [0u8; 32],
                });
            }
            WalkEvent::LeaveDir => {
                frame::node_close(&mut counter).expect("counter write");
                depth -= 1;
                if depth > 0 {
                    frame::entry_close(&mut counter).expect("counter write");
                    components.pop();
                }
            }
            WalkEvent::File {
                name,
                digest,
                size,
                executable,
            } => {
                let path = join(&components, name, depth);
                if depth > 0 {
                    frame::entry_open(&mut counter, name).expect("counter write");
                }
                frame::node_open(&mut counter).expect("counter write");
                frame::regular_header(&mut counter, executable, size).expect("counter write");
                // The content bytes begin immediately after the
                // length-prefix u64 `regular_header` ends with — the
                // same position `nar_ls` records.
                let nar_offset = counter.0;
                counter.0 += size;
                frame::contents_padding(&mut counter, size).expect("counter write");
                frame::node_close(&mut counter).expect("counter write");
                if depth > 0 {
                    frame::entry_close(&mut counter).expect("counter write");
                }
                out.push(NarLsEntry {
                    path,
                    kind: NarEntryKind::Regular,
                    size,
                    executable,
                    nar_offset,
                    target: Vec::new(),
                    file_digest: digest,
                });
            }
            WalkEvent::Symlink { name, target } => {
                let path = join(&components, name, depth);
                if depth > 0 {
                    frame::entry_open(&mut counter, name).expect("counter write");
                }
                frame::node_open(&mut counter).expect("counter write");
                frame::symlink(&mut counter, target).expect("counter write");
                frame::node_close(&mut counter).expect("counter write");
                if depth > 0 {
                    frame::entry_close(&mut counter).expect("counter write");
                }
                out.push(NarLsEntry {
                    path,
                    kind: NarEntryKind::Symlink,
                    size: 0,
                    executable: false,
                    nar_offset: 0,
                    target: target.to_vec(),
                    file_digest: [0u8; 32],
                });
            }
        }
    }
    Ok(out)
}

/// Assemble the [`ParsedNar`] the commit transaction's
/// `set_nar_index_in_conn` consumes, from the validated tree instead
/// of a buffered NAR. CPU-bound (a prost encode per directory plus the
/// entry walk) — call under [`crate::cas::cpu_bound`].
pub(super) fn build_parsed(
    out: &ValidatedOutput,
    dirs: &HashMap<[u8; 32], Directory>,
    max_nodes: usize,
) -> Result<ParsedNar, Status> {
    // validate_begin already proved the walk succeeds (reachability +
    // node cap); a failure here is a server bug, not client input.
    let entries = tree_to_entries(&out.root_node, dirs, max_nodes).map_err(|e| {
        Status::internal(format!(
            "PutPathChunked: tree walk failed after validation: {e}"
        ))
    })?;
    let dag = crate::castore::build(&entries);
    let encoded_entries = crate::nar_index::encode_entries(&entries, &dag);
    Ok(ParsedNar {
        entries,
        dag,
        encoded_entries,
    })
}

/// Serialize one output's claimed chunk list into the
/// `manifest_data.chunk_list` format `GetPath` reassembles from.
pub(super) fn build_manifest(out: &ValidatedOutput) -> Vec<u8> {
    Manifest {
        entries: out
            .chunk_manifest
            .iter()
            .map(|(hash, size)| ManifestEntry {
                hash: *hash,
                size: *size,
            })
            .collect(),
    }
    .serialize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::nar::{dump_path_streaming, nar_ls};

    fn dump_path(p: &std::path::Path) -> Vec<u8> {
        let mut v = Vec::new();
        dump_path_streaming(p, &mut v).expect("fixture dump");
        v
    }

    /// Build `(RootNode, dirs)` for a NAR via the production
    /// `nar_ls` → `castore::build` pipeline (same as
    /// `castore_nar::tests::dag_of`).
    fn dag_of(nar: &[u8]) -> (RootNode, HashMap<[u8; 32], Directory>) {
        let entries = nar_ls(std::io::Cursor::new(nar)).expect("fixture NAR parses");
        let dag = crate::castore::build(&entries);
        castore_nar::decode_dag(&dag.root_node, dag.directories.into_iter().map(|(_, b)| b))
            .expect("round-trip decode")
    }

    fn assert_agrees_with_nar_ls(path: &std::path::Path) {
        let nar = dump_path(path);
        let expected = nar_ls(std::io::Cursor::new(nar.as_slice())).expect("nar_ls");
        let (root, dirs) = dag_of(&nar);
        let got = tree_to_entries(&root, &dirs, 1 << 20).expect("walk");
        assert_eq!(
            got, expected,
            "tree_to_entries must reproduce nar_ls byte-for-byte for {path:?}"
        );
    }

    /// THE agreement test: the entry list derived from the Directory
    /// tree is identical to the one `nar_ls` derives from the NAR byte
    /// stream. If this breaks, a chunked commit persists a `nar_index`
    /// whose offsets/paths disagree with what `GetPath`'s framing
    /// regeneration serves.
    #[test]
    fn tree_to_entries_agrees_with_nar_ls() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a"), b"alpha").unwrap(); // 5 bytes → padded
        std::fs::write(root.join("empty"), b"").unwrap(); // zero-byte file
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::create_dir(root.join("sub/deeper")).unwrap(); // empty dir
        std::fs::write(root.join("sub/b"), b"12345678").unwrap(); // exactly 8 → no pad
        std::os::unix::fs::symlink("../a", root.join("sub/link")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(root.join("x"), b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(root.join("x"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        assert_agrees_with_nar_ls(&root);
    }

    /// Single-file and single-symlink roots have no enclosing directory
    /// — the `depth == 0` (empty path, no entry framing) branches.
    #[test]
    fn tree_to_entries_non_directory_roots() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let f = dir.path().join("just-a-file");
        std::fs::write(&f, b"contents here").unwrap();
        assert_agrees_with_nar_ls(&f);

        let l = dir.path().join("just-a-link");
        std::os::unix::fs::symlink("/nowhere", &l).unwrap();
        assert_agrees_with_nar_ls(&l);
    }

    /// `build_manifest` round-trips through `Manifest::deserialize` and
    /// its `total_size()` is the sum of the regular files' sizes (the
    /// blob-stream length, not the NAR length).
    #[test]
    fn build_manifest_roundtrips() {
        let out = ValidatedOutput {
            store_path: rio_nix::store_path::StorePath::parse(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-m",
            )
            .unwrap(),
            nar_hash: [0u8; 32],
            nar_size: 1 << 20,
            references: Vec::new(),
            root_node: RootNode { node: None },
            chunk_manifest: vec![([0x11; 32], 100), ([0x22; 32], 200), ([0x11; 32], 100)],
            is_ca: false,
            file_runs: Vec::new(),
        };
        let bytes = build_manifest(&out);
        let parsed = Manifest::deserialize(&bytes).expect("round-trip");
        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(parsed.total_size(), 400);
        assert_eq!(parsed.entries[0].hash, [0x11; 32]);
        assert_eq!(parsed.entries[1].size, 200);
    }
}
