//! NAR and PathInfo builders for tests, plus valid-store-path generators.

use rio_nix::nar::NarNode;
use rio_proto::validated::ValidatedPathInfo;
use sha2::{Digest, Sha256};

/// 32×'a' — valid nixbase32. StorePath::parse validates the charset but not
/// hash semantics, so this passes. Paths with different names are distinct
/// under StorePath's Eq/Hash (which compare the FULL string, not just the
/// hash part — see rio-nix/src/store_path.rs:256-266).
const TEST_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Generate a valid store-path basename (`{32-char-hash}-{name}`).
///
/// Store-path names allow ASCII alphanumeric + `+-._?=` (uppercase OK).
pub fn test_store_basename(name: &str) -> String {
    format!("{TEST_HASH}-{name}")
}

/// Generate a valid full store path (`/nix/store/{32-char-hash}-{name}`).
pub fn test_store_path(name: &str) -> String {
    format!("/nix/store/{TEST_HASH}-{name}")
}

/// Generate a valid full .drv store path (`/nix/store/{hash}-{name}.drv`).
pub fn test_drv_path(name: &str) -> String {
    test_store_path(&format!("{name}.drv"))
}

/// Build a minimal NAR (single regular file) from raw contents.
/// Returns `(nar_bytes, sha256_digest)`.
pub fn make_nar(contents: &[u8]) -> (Vec<u8>, [u8; 32]) {
    let node = NarNode::Regular {
        executable: false,
        contents: contents.to_vec(),
    };
    let mut buf = Vec::new();
    // Vec<u8> impl Write never fails; the Result is for real I/O sinks.
    rio_nix::nar::serialize(&mut buf, &node).expect("NAR serialize to Vec is infallible");
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    (buf, digest)
}

/// Build a `ValidatedPathInfo` for a test store path with the given NAR bytes
/// and hash.
///
/// `store_path` must be a valid `/nix/store/{32-char-hash}-{name}` string;
/// use [`test_store_path`] to generate one. Panics on invalid input —
/// test fixtures should never be malformed.
pub fn make_path_info(store_path: &str, nar: &[u8], nar_hash: [u8; 32]) -> ValidatedPathInfo {
    ValidatedPathInfo {
        store_path: rio_nix::store_path::StorePath::parse(store_path)
            .unwrap_or_else(|e| panic!("test fixture store_path {store_path:?} is invalid: {e}")),
        store_path_hash: vec![],
        deriver: None,
        nar_hash,
        nar_size: nar.len() as u64,
        references: vec![],
        registration_time: 0,
        ultimate: false,
        signatures: vec![],
        content_address: None,
    }
}

/// Build a `ValidatedPathInfo` computing the NAR hash from the NAR bytes.
pub fn make_path_info_for_nar(store_path: &str, nar: &[u8]) -> ValidatedPathInfo {
    let digest: [u8; 32] = Sha256::digest(nar).into();
    make_path_info(store_path, nar, digest)
}
