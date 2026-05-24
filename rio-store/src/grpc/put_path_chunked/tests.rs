//! Unit tests for the `PutPathChunked` validator and NAR reconstruction
//! walk. The gRPC integration tests (ephemeral PG + in-process server)
//! live in `rio-store/tests/grpc/put_path_chunked.rs`.

use std::collections::HashMap;

use prost::Message;

use rio_proto::castore::{Directory, RootNode, root_node};
use rio_proto::types::{ChunkRef, ChunkedOutputHeader, PutPathChunkedBegin};

use super::validate::{NarSegment, validate_begin};

/// Reassemble the full NAR byte stream from a validated output's
/// segments + the chunk bodies, exactly as the verify walk feeds it
/// into the SHA-256 accumulator. `chunks` maps digest → bytes.
fn reassemble(
    out: &super::validate::ValidatedOutput,
    chunks: &HashMap<[u8; 32], Vec<u8>>,
) -> Vec<u8> {
    let mut nar = Vec::new();
    let mut cursor = 0usize;
    for seg in &out.segments {
        match seg {
            NarSegment::Framing { bytes, digest } => {
                assert_eq!(
                    *digest,
                    *blake3::hash(bytes).as_bytes(),
                    "framing segment digest must match its bytes"
                );
                nar.extend_from_slice(bytes);
            }
            NarSegment::FileContents { n_chunks } => {
                for _ in 0..*n_chunks {
                    let (d, _) = out.chunk_manifest[cursor];
                    cursor += 1;
                    nar.extend_from_slice(&chunks[&d]);
                }
            }
        }
    }
    nar
}

/// Build a `Begin` for a single output from an on-disk tree: dump the
/// real NAR with `dump_path_streaming`, derive the Directory DAG with
/// `nar_ls` + `castore::build` (the same code the read path trusts),
/// and chunk each file's contents at `chunk_size` boundaries. Returns
/// the Begin, the chunk bodies, and the reference NAR bytes.
///
/// This IS a miniature of the builder's fused walk — using the
/// existing, separately-tested `nar_ls`/`castore::build` for the
/// fixture side means the server's reconstruction is tested against an
/// independent implementation of the same format.
pub(super) fn begin_for_tree(
    root: &std::path::Path,
    store_path: &str,
    chunk_size: usize,
) -> (PutPathChunkedBegin, HashMap<[u8; 32], Vec<u8>>, Vec<u8>) {
    let (header, dirs, chunks, nar) =
        crate::test_helpers::chunked_output_for_tree(root, store_path, chunk_size);
    let begin = crate::test_helpers::assemble_begin(vec![header], vec![dirs]);
    (begin, chunks, nar)
}

/// A three-kind fixture tree: nested dirs, an executable, a symlink,
/// an empty file, and two byte-identical files (repeated chunk digest).
fn fixture_tree() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    std::fs::create_dir_all(p.join("bin")).unwrap();
    std::fs::create_dir_all(p.join("share/doc")).unwrap();
    std::fs::write(p.join("bin/tool"), b"#!/bin/sh\necho hello\n").unwrap();
    std::fs::set_permissions(p.join("bin/tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(p.join("share/doc/README"), b"read me please").unwrap();
    std::fs::write(p.join("share/doc/COPY"), b"read me please").unwrap();
    std::fs::write(p.join("empty"), b"").unwrap();
    std::os::unix::fs::symlink("bin/tool", p.join("default")).unwrap();
    tmp
}

/// THE golden test: the validator's segment walk + spliced chunk bodies
/// reproduces `dump_path_streaming`'s output byte-for-byte, for a tree
/// exercising every node kind, executables, empty files, and repeated
/// content. Any drift between the castore-DAG framing emitter and the
/// real NAR writer shows up here as a byte diff.
#[test]
fn reconstructed_nar_matches_dump_path_streaming() {
    let tmp = fixture_tree();
    let store_path = rio_test_support::fixtures::test_store_path("golden");
    for chunk_size in [3usize, 7, 64 * 1024] {
        let (begin, chunks, nar) = begin_for_tree(tmp.path(), &store_path, chunk_size);
        let validated = validate_begin(&begin, None)
            .unwrap_or_else(|e| panic!("chunk_size {chunk_size}: {e:?}"));
        let rebuilt = reassemble(&validated.outputs[0], &chunks);
        assert_eq!(
            rebuilt, nar,
            "chunk_size {chunk_size}: reconstructed NAR differs from dump_path_streaming"
        );
        // The recomputed nar_size matches too (checked inside
        // validate_begin against the declared value, which came from
        // the real NAR's length).
        assert_eq!(validated.outputs[0].info.nar_size, nar.len() as u64);
    }
}

/// The serialized `manifest_data.chunk_list` (framing chunks
/// interleaved with content chunks) concatenates to the exact NAR —
/// the invariant `GetPath`, `nar_index::reassemble`, and the GC sweep
/// rely on. The framing chunk bodies are reproduced from the segments
/// (which is where the verify walk uploads them from).
#[test]
fn chunk_list_concatenates_to_nar() {
    let tmp = fixture_tree();
    let store_path = rio_test_support::fixtures::test_store_path("chunklist");
    let (begin, chunks, nar) = begin_for_tree(tmp.path(), &store_path, 7);
    let validated = validate_begin(&begin, None).expect("valid");
    let out = &validated.outputs[0];

    // Collect every chunk body the verify walk would persist: the
    // builder-sent content chunks plus the server-generated framing
    // runs.
    let mut bodies: HashMap<[u8; 32], Vec<u8>> = chunks.clone();
    for seg in &out.segments {
        if let NarSegment::Framing { bytes, digest } = seg {
            bodies.insert(*digest, bytes.clone());
        }
    }

    let manifest = crate::manifest::Manifest::deserialize(&out.chunk_list_bytes)
        .expect("chunk_list deserializes");
    let mut rebuilt = Vec::new();
    for e in &manifest.entries {
        let body = &bodies[&e.hash];
        assert_eq!(
            body.len(),
            e.size as usize,
            "manifest entry size matches body"
        );
        rebuilt.extend_from_slice(body);
    }
    assert_eq!(rebuilt, nar, "chunk_list concatenation must equal the NAR");
    // Every unique_chunks entry has a persisted body (refcount
    // symmetry: what we count is what exists).
    for (h, s) in &out.unique_chunks {
        assert_eq!(bodies[h].len(), *s as usize);
    }
}

/// Single-regular-file root (no directories at all) round-trips.
#[test]
fn reconstructed_single_file_root() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("blob");
    std::fs::write(&f, b"just one file's contents here").unwrap();
    let store_path = rio_test_support::fixtures::test_store_path("single");
    let (begin, chunks, nar) = begin_for_tree(&f, &store_path, 5);
    let validated = validate_begin(&begin, None).expect("valid");
    assert_eq!(reassemble(&validated.outputs[0], &chunks), nar);
    assert!(validated.outputs[0].dir_digests.is_empty());
    assert_eq!(validated.outputs[0].file_blobs.len(), 1);
}

/// The nar_index entries produced by the walk match what `nar_ls` on
/// the real NAR produces (paths, kinds, sizes, offsets, digests) — the
/// commit txn persists these as the eager NAR index.
#[test]
fn walk_index_entries_match_nar_ls() {
    let tmp = fixture_tree();
    let store_path = rio_test_support::fixtures::test_store_path("index");
    let (begin, _chunks, nar) = begin_for_tree(tmp.path(), &store_path, 64 * 1024);
    let validated = validate_begin(&begin, None).expect("valid");

    let entries = rio_nix::nar::nar_ls(std::io::Cursor::new(&nar)).expect("nar_ls");
    let dag = crate::castore::build(&entries);
    let expected = crate::nar_index::encode_entries(&entries, &dag);
    assert_eq!(
        validated.outputs[0].nar_index_entries, expected,
        "walk-derived NarIndex differs from nar_ls-derived NarIndex"
    );
}

// ===========================================================================
// Rejection cases — every §6.2 bound.
// ===========================================================================

fn valid_begin() -> (PutPathChunkedBegin, tempfile::TempDir) {
    let tmp = fixture_tree();
    let store_path = rio_test_support::fixtures::test_store_path("rej");
    let (begin, _, _) = begin_for_tree(tmp.path(), &store_path, 64 * 1024);
    (begin, tmp)
}

fn assert_invalid(begin: &PutPathChunkedBegin, needle: &str) {
    let err = validate_begin(begin, None).expect_err("should be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    assert!(
        err.message().contains(needle),
        "expected {needle:?} in {:?}",
        err.message()
    );
}

#[test]
fn reject_empty_outputs() {
    let (mut begin, _t) = valid_begin();
    begin.outputs.clear();
    assert_invalid(&begin, "must not be empty");
}

#[test]
fn reject_duplicate_output_paths() {
    let (mut begin, _t) = valid_begin();
    let dup = begin.outputs[0].clone();
    begin.outputs.push(dup);
    assert_invalid(&begin, "duplicate output store_path");
}

#[test]
fn reject_oversize_nar_size() {
    let (mut begin, _t) = valid_begin();
    begin.outputs[0].nar_size = rio_common::limits::MAX_NAR_SIZE + 1;
    assert_invalid(&begin, "exceeds maximum");
}

#[test]
fn reject_wrong_nar_size() {
    let (mut begin, _t) = valid_begin();
    begin.outputs[0].nar_size += 1;
    assert_invalid(&begin, "serializes to");
}

#[test]
fn reject_ref_outside_closure() {
    let (mut begin, _t) = valid_begin();
    begin.outputs[0]
        .refs
        .push(rio_test_support::fixtures::test_store_path(
            "not-in-closure",
        ));
    assert_invalid(&begin, "not in input_closure");
}

#[test]
fn reject_missing_directory_body() {
    let (mut begin, _t) = valid_begin();
    begin.directories.remove(0);
    assert_invalid(&begin, "missing from");
}

#[test]
fn reject_tampered_directory_body() {
    let (mut begin, _t) = valid_begin();
    // Flip the executable bit on some file entry — the body still
    // validates structurally but its recomputed digest no longer
    // matches the parent's child reference, so the walk can't resolve
    // the original digest.
    let tampered = begin
        .directories
        .iter_mut()
        .find(|d| !d.files.is_empty())
        .expect("fixture has a dir with files");
    tampered.files[0].executable = !tampered.files[0].executable;
    assert_invalid(&begin, "missing from");
}

#[test]
fn reject_unsorted_directory_entries() {
    let (mut begin, _t) = valid_begin();
    let two_files = begin
        .directories
        .iter_mut()
        .find(|d| d.files.len() >= 2)
        .expect("fixture has a dir with two files");
    two_files.files.swap(0, 1);
    assert_invalid(&begin, "not strictly sorted");
}

#[test]
fn reject_chunk_run_sum_mismatch() {
    let (mut begin, _t) = valid_begin();
    // Drop the last chunk: some file's run no longer sums to its size.
    begin.outputs[0].chunk_manifest.pop();
    let err = validate_begin(&begin, None).expect_err("should be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[test]
fn reject_trailing_chunk_manifest_entries() {
    let (mut begin, _t) = valid_begin();
    begin.outputs[0].chunk_manifest.push(ChunkRef {
        hash: [0xAB; 32].to_vec(),
        size: 17,
    });
    // The extra entry either breaks a file's run sum or trails past the
    // last file; both are INVALID_ARGUMENT.
    let err = validate_begin(&begin, None).expect_err("should be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[test]
fn reject_chunk_size_zero_and_oversize() {
    let (mut begin, _t) = valid_begin();
    begin.outputs[0].chunk_manifest[0].size = 0;
    assert_invalid(&begin, "out of range");

    let (mut begin, _t) = valid_begin();
    begin.outputs[0].chunk_manifest[0].size = (crate::chunker::CHUNK_MAX + 1) as u32;
    assert_invalid(&begin, "out of range");
}

#[test]
fn reject_conflicting_chunk_lengths() {
    let (mut begin, _t) = valid_begin();
    // Two outputs sharing a digest but disagreeing on its length.
    let mut second = begin.outputs[0].clone();
    second.store_path = rio_test_support::fixtures::test_store_path("rej2");
    second.chunk_manifest[0].size += 1;
    begin.outputs.push(second);
    assert_invalid(&begin, "declared with size");
}

#[test]
fn reject_novel_not_in_any_manifest() {
    let (mut begin, _t) = valid_begin();
    begin.novel.push([0xCD; 32].to_vec());
    assert_invalid(&begin, "does not appear in any output's chunk_manifest");
}

#[test]
fn reject_novel_duplicate() {
    let (mut begin, _t) = valid_begin();
    let first = begin.novel[0].clone();
    begin.novel.push(first);
    assert_invalid(&begin, "duplicate");
}

#[test]
fn reject_novel_out_of_order() {
    let (mut begin, _t) = valid_begin();
    assert!(
        begin.novel.len() >= 2,
        "fixture must produce at least two novel chunks"
    );
    begin.novel.swap(0, 1);
    assert_invalid(&begin, "first-occurrence order");
}

#[test]
fn reject_too_many_outputs() {
    let (mut begin, _t) = valid_begin();
    let template = begin.outputs[0].clone();
    for i in 0..rio_common::limits::MAX_BATCH_OUTPUTS {
        let mut o = template.clone();
        o.store_path = rio_test_support::fixtures::test_store_path(&format!("many-{i}"));
        begin.outputs.push(o);
    }
    assert_invalid(&begin, "MAX_BATCH_OUTPUTS");
}

#[test]
fn reject_input_closure_digest_mismatch() {
    let (begin, _t) = valid_begin();
    let claims = rio_auth::hmac::AssignmentClaims {
        executor_id: "builder-0".into(),
        drv_hash: "whatever".into(),
        expected_outputs: vec![begin.outputs[0].store_path.clone()],
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: None,
        role: rio_auth::hmac::TokenRole::Builder,
        input_closure_digest: "deadbeef".into(),
    };
    let err = validate_begin(&begin, Some(&claims)).expect_err("should be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("input_closure_digest"));
}

#[test]
fn reject_path_not_in_expected_outputs() {
    let (begin, _t) = valid_begin();
    let claims = rio_auth::hmac::AssignmentClaims {
        executor_id: "builder-0".into(),
        drv_hash: "whatever".into(),
        expected_outputs: vec![rio_test_support::fixtures::test_store_path("other")],
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: None,
        role: rio_auth::hmac::TokenRole::Builder,
        input_closure_digest: String::new(),
    };
    let err = validate_begin(&begin, Some(&claims)).expect_err("should be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
}

/// Accepts a Begin whose claims match (the happy path under HMAC).
#[test]
fn accept_with_matching_claims() {
    let (begin, _t) = valid_begin();
    let mut closure = begin.input_closure.clone();
    closure.sort_unstable();
    let claims = rio_auth::hmac::AssignmentClaims {
        executor_id: "builder-0".into(),
        drv_hash: "node0".into(),
        expected_outputs: vec![begin.outputs[0].store_path.clone()],
        is_ca: false,
        expiry_unix: u64::MAX,
        tenant: None,
        role: rio_auth::hmac::TokenRole::Builder,
        input_closure_digest: rio_auth::hmac::AssignmentClaims::digest_input_closure(&closure),
    };
    validate_begin(&begin, Some(&claims)).expect("valid begin with matching claims");
}

/// A directory-tree "zip bomb" — k levels each fanning out to the same
/// child body — is rejected by the walk-entry bound, not by an OOM.
#[test]
fn reject_exponential_tree() {
    // Level 0 is a leaf dir with one symlink (no chunk consumption, so
    // the walk can't bail early on the chunk cursor); level i has 64
    // entries all pointing at level i-1. 64^4 = 16.7M entries — the
    // walk must trip the entry-count or framing-byte bound, not OOM.
    let mut bodies: Vec<Directory> = Vec::new();
    let leaf = Directory {
        directories: vec![],
        files: vec![],
        symlinks: vec![rio_proto::castore::SymlinkEntry {
            name: b"l".to_vec(),
            target: b"target".to_vec(),
        }],
    };
    let mut prev_digest = *blake3::hash(&leaf.encode_to_vec()).as_bytes();
    bodies.push(leaf);
    for _level in 0..4 {
        let dir = Directory {
            directories: (0..64u8)
                .map(|i| rio_proto::castore::DirectoryEntry {
                    name: format!("d{i:02}").into_bytes(),
                    digest: prev_digest.to_vec(),
                    size: 1,
                })
                .collect(),
            files: vec![],
            symlinks: vec![],
        };
        prev_digest = *blake3::hash(&dir.encode_to_vec()).as_bytes();
        bodies.push(dir);
    }
    let begin = PutPathChunkedBegin {
        deriver: String::new(),
        outputs: vec![ChunkedOutputHeader {
            store_path: rio_test_support::fixtures::test_store_path("bomb"),
            nar_hash: vec![0; 32],
            nar_size: 1,
            refs: vec![],
            root_node: Some(RootNode {
                node: Some(root_node::Node::DirDigest(prev_digest.to_vec())),
            }),
            chunk_manifest: vec![],
        }],
        directories: bodies,
        novel: vec![],
        input_closure: vec![],
    };
    let err = validate_begin(&begin, None).expect_err("zip bomb must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("entries") || err.message().contains("framing"),
        "got {:?}",
        err.message()
    );
}
