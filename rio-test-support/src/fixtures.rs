//! NAR and PathInfo builders for tests.

use rio_nix::nar::NarNode;
use rio_proto::types::PathInfo;
use sha2::{Digest, Sha256};

/// Build a minimal NAR (single regular file) from raw contents.
/// Returns `(nar_bytes, sha256_digest)`.
pub fn make_nar(contents: &[u8]) -> (Vec<u8>, [u8; 32]) {
    let node = NarNode::Regular {
        executable: false,
        contents: contents.to_vec(),
    };
    let mut buf = Vec::new();
    rio_nix::nar::serialize(&mut buf, &node).unwrap();
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    (buf, digest)
}

/// Build a `PathInfo` for a test store path with the given NAR bytes and hash.
///
/// Use when you already have the digest (e.g., from [`make_nar`]).
pub fn make_path_info(store_path: &str, nar: &[u8], nar_hash: [u8; 32]) -> PathInfo {
    PathInfo {
        store_path: store_path.to_string(),
        store_path_hash: vec![],
        deriver: String::new(),
        nar_hash: nar_hash.to_vec(),
        nar_size: nar.len() as u64,
        references: vec![],
        registration_time: 0,
        ultimate: false,
        signatures: vec![],
        content_address: String::new(),
    }
}

/// Build a `PathInfo` computing the NAR hash from the NAR bytes.
///
/// Convenience wrapper over [`make_path_info`] for tests that don't need
/// the hash separately.
pub fn make_path_info_for_nar(store_path: &str, nar: &[u8]) -> PathInfo {
    let digest: [u8; 32] = Sha256::digest(nar).into();
    make_path_info(store_path, nar, digest)
}
