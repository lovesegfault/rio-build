//! NAR and PathInfo builders for tests, plus valid-store-path generators.

use rio_nix::nar::NarNode;
use rio_proto::dag::{DerivationEdge, DerivationNode};
use rio_proto::validated::ValidatedPathInfo;
use sha2::{Digest, Sha256};

/// 32×'a' — valid nixbase32. StorePath::parse validates the charset but not
/// hash semantics, so this passes. Paths with different names are distinct
/// under StorePath's Eq/Hash (which compare the FULL string, not just the
/// hash part — see rio-nix/src/store_path.rs:256-266).
///
/// Deterministic counterpart to [`rand_store_hash`]. `pub` so external
/// tests can construct matching paths (e.g., `format!("/nix/store/{TEST_HASH}-foo")`).
pub const TEST_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// nixbase32 alphabet (0-9, a-z minus e/o/u/t). `StorePath::parse`
/// validates against exactly this set — a random ASCII-alphanumeric
/// string won't pass (the chars 'e','o','u','t' are rejected).
///
/// Re-export of `rio_nix::store_path::nixbase32::CHARS` — single source
/// of truth. `pub` because rio-bench and property tests need it for
/// generating valid store paths on the fly.
pub use rio_nix::store_path::nixbase32::CHARS as NIXBASE32;

/// 32 random nixbase32 chars. A fresh valid store-path hash per call.
///
/// Use when tests need DISTINCT paths (criterion iterates;
/// scheduler DAG dedupes on `drv_hash`; collisions short-circuit the
/// merge path). Use [`TEST_HASH`] / [`test_store_path`] when
/// determinism matters (most unit tests).
pub fn rand_store_hash() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..32)
        .map(|_| NIXBASE32[rng.random_range(0..32)] as char)
        .collect()
}

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

/// Build a minimal `DerivationNode` for scheduler/DAG tests.
///
/// `drv_path` is auto-generated from `tag` via [`test_drv_path`], so
/// callers get a valid `/nix/store/{32-char-hash}-{tag}.drv` path for free.
/// `drv_hash` is set to `tag` (scheduler tests key on the hash string).
///
/// `pname` is hardcoded to `"test-pkg"` — no scheduler test asserts on it.
pub fn make_derivation_node(tag: &str, system: &str) -> DerivationNode {
    DerivationNode {
        drv_hash: tag.into(),
        drv_path: test_drv_path(tag),
        pname: "test-pkg".into(),
        system: system.into(),
        required_features: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        expected_output_paths: vec![],
        drv_content: Vec::new(),
        input_srcs_nar_size: 0,
        is_content_addressed: false,
        ca_modular_hash: Vec::new(),
        needs_resolve: false,
    }
}

/// Build a minimal `DerivationEdge` from tags.
///
/// Both `drv_path` fields are auto-generated via [`test_drv_path`],
/// matching [`make_derivation_node`]'s path generation.
pub fn make_edge(parent_tag: &str, child_tag: &str) -> DerivationEdge {
    DerivationEdge {
        parent_drv_path: test_drv_path(parent_tag),
        child_drv_path: test_drv_path(child_tag),
    }
}

/// Deterministic pseudo-random bytes: `(i * 7919 + seed) % 251`.
///
/// Varied enough that FastCDC finds chunk boundaries (zeros or a
/// repeating short pattern would not), but reproducible so test
/// failures replay. 7919 is prime; 251 is the largest prime under 256
/// so the sequence cycles slowly. `seed` shifts the sequence — two
/// blobs with different seeds share SOME chunks (the dedup property
/// tests rely on this) but not all.
///
/// `u64` index range: at `len = 1 MiB`, `i * 7919 ≈ 8e9` overflows
/// `i32` (the default for an untyped `0..N` literal). The explicit
/// `u64` cast and `wrapping_mul` make the overflow defined.
pub fn pseudo_random_bytes(seed: u64, len: usize) -> Vec<u8> {
    (0u64..len as u64)
        .map(|i| (i.wrapping_mul(7919).wrapping_add(seed) % 251) as u8)
        .collect()
}

/// Build a NAR of roughly `payload_size` bytes, large enough to
/// trigger FastCDC chunking (> 256 KiB `INLINE_THRESHOLD`).
///
/// Payload is [`pseudo_random_bytes`]`(seed, payload_size)` wrapped in
/// single-file NAR framing (~100 bytes overhead; payload dominates).
/// Returns `(nar, path_info, store_path)` — the rich superset; callers
/// that only need the NAR hash use `info.nar_hash`. `store_path` is
/// `test_store_path("large-nar-{seed}")` so distinct seeds get
/// distinct paths.
///
/// At 64 KiB average chunk size, a 512 KiB NAR chunks into ~8 pieces.
pub fn make_large_nar(seed: u8, payload_size: usize) -> (Vec<u8>, ValidatedPathInfo, String) {
    let payload = pseudo_random_bytes(seed as u64, payload_size);
    let (nar, _hash) = make_nar(&payload);
    let store_path = test_store_path(&format!("large-nar-{seed}"));
    let info = make_path_info_for_nar(&store_path, &nar);
    (nar, info, store_path)
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

/// Seed an output file at `{tmp}/nix/store/{basename}` with the given
/// content. Returns `(tmp_dir, store_dir)` — hold `tmp_dir` to keep the
/// tempdir alive across the test.
///
/// Shared between rio-builder's FOD verification tests and upload tests
/// (both need a file in a fake overlay-upper's nix/store).
pub fn seed_store_output(
    basename: &str,
    content: &[u8],
) -> std::io::Result<(tempfile::TempDir, std::path::PathBuf)> {
    let tmp = tempfile::tempdir()?;
    let store_dir = tmp.path().join("nix/store");
    std::fs::create_dir_all(&store_dir)?;
    std::fs::write(store_dir.join(basename), content)?;
    Ok((tmp, store_dir))
}
