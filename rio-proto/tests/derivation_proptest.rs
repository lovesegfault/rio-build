//! Property tests for the canonical `rio.drv.v1.Derivation` codec:
//! random valid derivations round-trip exactly through proto and the
//! full `verify_drv_blob` pipeline, and any single-byte mutation of a
//! canonical blob fails verification.

use proptest::prelude::*;

use rio_nix::derivation::{Derivation as NixDerivation, DerivationOutput};
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use rio_proto::derivation_util::{
    canonical_encode, derivation_digest, from_proto, to_proto, validate_derivation, verify_drv_blob,
};

/// nixbase32 alphabet (no e/o/t/u) — hash parts must parse as store
/// paths.
fn hash_part() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        proptest::sample::select(
            "0123456789abcdfghijklmnpqrsvwxyz"
                .chars()
                .collect::<Vec<_>>(),
        ),
        32,
    )
    .prop_map(|v| v.into_iter().collect())
}

/// A store-path name component (subset of Nix's legal name chars).
fn path_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9+._?=-]{0,12}"
}

fn store_path(suffix: &'static str) -> impl Strategy<Value = String> {
    (hash_part(), path_name()).prop_map(move |(h, n)| format!("/nix/store/{h}-{n}{suffix}"))
}

/// Output names: distinct, lowercase. Paths concrete-IA shaped.
fn outputs() -> impl Strategy<Value = Vec<DerivationOutput>> {
    proptest::collection::btree_set("[a-z][a-z0-9]{0,6}", 1..4).prop_flat_map(|names| {
        let names: Vec<String> = names.into_iter().collect();
        proptest::collection::vec(store_path(""), names.len()).prop_map(move |paths| {
            names
                .iter()
                .zip(paths)
                .map(|(n, p)| DerivationOutput::new(n.clone(), p, "", "").unwrap())
                .collect()
        })
    })
}

/// Env/arg strings: arbitrary unicode including the ATerm escape set.
fn free_text() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[ -~λ→✓日本語\\\\\"\n\r\t]{0,24}").unwrap()
}

fn nix_derivation() -> impl Strategy<Value = NixDerivation> {
    (
        outputs(),
        proptest::collection::btree_map(
            store_path(".drv"),
            proptest::collection::btree_set("[a-z][a-z0-9]{0,6}", 1..3),
            0..3,
        ),
        proptest::collection::btree_set(store_path(""), 0..3),
        "[a-z0-9_]{1,16}",
        "/bin/[a-z]{1,8}",
        proptest::collection::vec(free_text(), 0..4),
        proptest::collection::btree_map("[a-zA-Z_][a-zA-Z0-9_]{0,10}", free_text(), 0..5),
    )
        .prop_map(
            |(outs, input_drvs, input_srcs, platform, builder, args, env)| {
                NixDerivation::new(outs, input_drvs, input_srcs, platform, builder, args, env)
                    .expect("outputs non-empty by construction")
            },
        )
}

proptest! {
    /// Round trip: rio-nix → proto (canonical by construction) →
    /// encode → full verify pipeline → identical rio-nix value and
    /// identical ATerm bytes.
    #[test]
    fn random_valid_drvs_round_trip(drv in nix_derivation()) {
        let p = to_proto(&drv);
        prop_assert!(validate_derivation(&p).is_ok());

        let enc = canonical_encode(&p);
        let digest = derivation_digest(&p);

        // Determinism: re-converting and re-encoding is bit-stable.
        prop_assert_eq!(&canonical_encode(&to_proto(&drv)), &enc);

        // Pure converter inverse.
        let back = from_proto(&p).unwrap();
        prop_assert_eq!(&back, &drv);
        prop_assert_eq!(back.to_aterm(), drv.to_aterm());

        // Full gateway pipeline with an honestly-minted drv path.
        let aterm = drv.to_aterm();
        let refs: Vec<StorePath> = drv
            .input_drvs()
            .keys()
            .chain(drv.input_srcs().iter())
            .map(|r| StorePath::parse(r).unwrap())
            .collect();
        let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
        let drv_path = StorePath::make_text("prop-fixture.drv", &h, &refs).unwrap();
        let v = verify_drv_blob(&enc, &digest, drv_path.as_str()).unwrap();
        prop_assert_eq!(v.aterm, aterm);
        prop_assert_eq!(v.derivation, drv);
    }

    /// Tamper: flip one byte anywhere in a canonical blob, recompute
    /// its (honest) blake3, and verification must still fail — the
    /// blob can no longer be the canonical encoding of a message whose
    /// ATerm hashes to the claimed drv path. (Same-message different
    /// bytes ⇒ NonCanonical; different message ⇒ different ATerm ⇒
    /// drv-path hash mismatch; everything else dies in decode/
    /// validate/UTF-8.)
    #[test]
    fn mutated_canonical_bytes_fail_verify(
        drv in nix_derivation(),
        pos_seed in any::<prop::sample::Index>(),
        bit in 0u8..8,
    ) {
        let p = to_proto(&drv);
        let enc = canonical_encode(&p);
        prop_assume!(!enc.is_empty());

        let aterm = drv.to_aterm();
        let refs: Vec<StorePath> = drv
            .input_drvs()
            .keys()
            .chain(drv.input_srcs().iter())
            .map(|r| StorePath::parse(r).unwrap())
            .collect();
        let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
        let drv_path = StorePath::make_text("prop-fixture.drv", &h, &refs).unwrap();

        let mut mutated = enc.clone();
        let pos = pos_seed.index(mutated.len());
        mutated[pos] ^= 1 << bit;
        let mutated_digest = *blake3::hash(&mutated).as_bytes();

        prop_assert!(verify_drv_blob(&mutated, &mutated_digest, drv_path.as_str()).is_err());

        // And the unmutated blob with the now-wrong digest fails on
        // the digest check alone.
        prop_assert!(verify_drv_blob(&enc, &mutated_digest, drv_path.as_str()).is_err());
    }
}
