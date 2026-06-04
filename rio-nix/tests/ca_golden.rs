//! Golden tests for floating-CA self-reference handling, pinned against
//! real Nix.
//!
//! # Fixture provenance
//!
//! Generated once with `nix (Nix) 2.34.7` (the dev-shell nix) on
//! 2026-05-25 by building the following expression into a throwaway
//! chroot store (`nix build --store /tmp/... --option sandbox true
//! --extra-experimental-features 'nix-command ca-derivations'`) and
//! capturing `nix path-info --json` plus `nix nar pack` for each
//! output:
//!
//! ```nix
//! rec {
//!   selfOnly = derivation {
//!     name = "rio-selfref-fixture";
//!     system = builtins.currentSystem;
//!     builder = "/bin/sh";
//!     args = [ "-c" "printf 'self lives at %s\\n' \"$out\" > $out" ];
//!     __contentAddressed = true;
//!     outputHashMode = "recursive";
//!     outputHashAlgo = "sha256";
//!   };
//!   selfAndRef = derivation {
//!     name = "rio-selfref-withdep-fixture";
//!     system = builtins.currentSystem;
//!     builder = "/bin/sh";
//!     args = [ "-c" "printf 'self %s dep %s\\n' \"$out\" \"$dep\" > $out" ];
//!     dep = selfOnly;
//!     __contentAddressed = true;
//!     outputHashMode = "recursive";
//!     outputHashAlgo = "sha256";
//!   };
//! }
//! ```
//!
//! `nix path-info` reported (hashes converted to base16 with
//! `nix hash convert --to base16`):
//!
//! | output | store path | `ca` (modulo hash) | `narHash` (final content) |
//! |---|---|---|---|
//! | selfOnly | `/nix/store/vfgpabz0zl9v0d8cdr0ydh9d0r9s43dk-rio-selfref-fixture` | `fixed:r:sha256:ac75c7…1413` | `5079b6…1b41` |
//! | selfAndRef | `/nix/store/1xhcmlv1nvca84x1898i57hyv7z8dxlq-rio-selfref-withdep-fixture` | `fixed:r:sha256:b88750…fd5c` | `1bc40e…1d0a` |
//!
//! selfOnly's references are `[itself]`; selfAndRef's are `[itself,
//! selfOnly]`. The committed NARs are the `nix nar pack` output of the
//! built store objects (192 and 264 bytes).
//!
//! What these pin:
//! - the modulo-hash semantics (occurrences of the path's own hash part
//!   are replaced with **NUL bytes** before hashing — an ASCII-`'0'`
//!   replacement fails these tests);
//! - the `:self` token's position in the fingerprint (after the sorted
//!   references);
//! - that *only* the output's own hash part is zeroed (the dep's hash
//!   part in selfAndRef's content is hashed as-is).

use std::io::Write;

use rio_nix::ca::HashModuloSink;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;

const SELF_ONLY_PATH: &str = "/nix/store/vfgpabz0zl9v0d8cdr0ydh9d0r9s43dk-rio-selfref-fixture";
const SELF_ONLY_NAME: &str = "rio-selfref-fixture";
const SELF_ONLY_CA_HEX: &str = "ac75c7a1abf0c30597b89c1aaf1dd706db621192b7655e9b17042f5725c61413";
const SELF_ONLY_NARHASH_HEX: &str =
    "5079b6645e73c4623874a09a1645e344b295c73c839af1aa295d5bf3048d1b41";
const SELF_ONLY_NAR: &[u8] = include_bytes!("fixtures/ca-selfref.nar");

const WITHDEP_PATH: &str =
    "/nix/store/1xhcmlv1nvca84x1898i57hyv7z8dxlq-rio-selfref-withdep-fixture";
const WITHDEP_NAME: &str = "rio-selfref-withdep-fixture";
const WITHDEP_CA_HEX: &str = "b88750ed974c8c182449dad73ef31c9b198591e492e02b50555219c23b5afd5c";
const WITHDEP_NARHASH_HEX: &str =
    "1bc40e5ce6267dafbe1b41b187dd78f7554c034fe8a7482a0af3f67741151d0a";
const WITHDEP_NAR: &[u8] = include_bytes!("fixtures/ca-selfref-withdep.nar");

fn hash_part(path: &str) -> String {
    StorePath::parse(path).unwrap().hash_part()
}

fn sha256_hex(hex: &str) -> NixHash {
    NixHash::new(HashAlgo::SHA256, hex::decode(hex).unwrap()).unwrap()
}

/// Feed `data` to a `HashModuloSink` in `chunk`-sized writes.
fn modulo_hash(data: &[u8], modulus: &str, chunk: usize) -> (NixHash, u64) {
    let mut sink = HashModuloSink::new(HashAlgo::SHA256, modulus);
    for c in data.chunks(chunk) {
        sink.write_all(c).unwrap();
    }
    sink.finish()
}

/// The committed NAR fixtures are intact: their plain SHA-256 equals
/// the `narHash` Nix reported for the built outputs.
#[test]
fn fixture_nars_match_recorded_nar_hashes() {
    for (nar, want) in [
        (SELF_ONLY_NAR, SELF_ONLY_NARHASH_HEX),
        (WITHDEP_NAR, WITHDEP_NARHASH_HEX),
    ] {
        let got = NixHash::compute(HashAlgo::SHA256, nar);
        assert_eq!(hex::encode(got.digest()), want);
    }
}

/// HashModuloSink over the final NAR with the path's own hash part as
/// modulus reproduces the `ca` hash Nix recorded — across every chunk
/// split, including 1-byte writes (boundary-straddling occurrences).
#[test]
fn hash_modulo_reproduces_nix_ca_hash_self_only() {
    let modulus = hash_part(SELF_ONLY_PATH);
    for chunk in [1, 7, 32, 64, SELF_ONLY_NAR.len()] {
        let (got, occurrences) = modulo_hash(SELF_ONLY_NAR, &modulus, chunk);
        assert_eq!(
            hex::encode(got.digest()),
            SELF_ONLY_CA_HEX,
            "chunk size {chunk}"
        );
        assert_eq!(occurrences, 1, "the content embeds $out exactly once");
    }
}

/// Same as above for the two-reference fixture: only the output's OWN
/// hash part is zeroed; the dependency's hash part is hashed verbatim.
#[test]
fn hash_modulo_reproduces_nix_ca_hash_with_dep() {
    let modulus = hash_part(WITHDEP_PATH);
    for chunk in [1, 13, 64, WITHDEP_NAR.len()] {
        let (got, occurrences) = modulo_hash(WITHDEP_NAR, &modulus, chunk);
        assert_eq!(
            hex::encode(got.digest()),
            WITHDEP_CA_HEX,
            "chunk size {chunk}"
        );
        assert_eq!(occurrences, 1);
    }
}

/// `make_fixed_output_with_self` turns the modulo hash into the exact
/// store path Nix computed — self-reference only.
#[test]
fn self_reference_ca_path_matches_nix() {
    let p = StorePath::make_fixed_output_with_self(
        SELF_ONLY_NAME,
        &sha256_hex(SELF_ONLY_CA_HEX),
        true,
        &[],
        true,
    )
    .unwrap();
    assert_eq!(p.as_str(), SELF_ONLY_PATH);
}

/// …and with a reference to another path alongside the self-reference
/// (pins the `source:ref:…:self` ordering).
#[test]
fn self_reference_plus_ref_ca_path_matches_nix() {
    let dep = StorePath::parse(SELF_ONLY_PATH).unwrap();
    let p = StorePath::make_fixed_output_with_self(
        WITHDEP_NAME,
        &sha256_hex(WITHDEP_CA_HEX),
        true,
        &[dep],
        true,
    )
    .unwrap();
    assert_eq!(p.as_str(), WITHDEP_PATH);

    // Without the self flag the path comes out different — the flag is
    // load-bearing, not decorative.
    let without_self = StorePath::make_fixed_output_with_self(
        WITHDEP_NAME,
        &sha256_hex(WITHDEP_CA_HEX),
        true,
        &[StorePath::parse(SELF_ONLY_PATH).unwrap()],
        false,
    )
    .unwrap();
    assert_ne!(without_self.as_str(), WITHDEP_PATH);
}

/// End-to-end self-consistency: zeroing with the WRONG modulus (the
/// dep's hash part instead of the output's own) must NOT reproduce the
/// recorded ca hash — guards against a sink that zeroes every
/// candidate it sees rather than the designated one.
#[test]
fn wrong_modulus_does_not_reproduce_ca_hash() {
    let dep_modulus = hash_part(SELF_ONLY_PATH);
    let (got, _) = modulo_hash(WITHDEP_NAR, &dep_modulus, 64);
    assert_ne!(hex::encode(got.digest()), WITHDEP_CA_HEX);
}
