#![no_main]

//! Differential-fuzz the FOD declared-hash decoder against the vendored
//! CppNix port (`rio_nix::hash_oracle`, line-by-line from the pinned
//! 2.34.7 `hash.cc`/`base-n.cc`/`base-nix-32.cc`).
//!
//! Properties (the same pair the 4096-case proptest
//! `oracle_containment_4096` in `rio-nix/src/hash.rs` pins — tracey
//! markers live on that proptest, never in fuzz files):
//!
//! 1. **Containment**: every input rio's
//!    `NixHash::parse_nonsri_unprefixed` accepts, the oracle accepts
//!    with the identical digest. rio may only ever be STRICTER (the
//!    registered divergence `nix.divergence.fod-base64-strict`), never
//!    accept something the oracle rejects and never disagree on bytes.
//! 2. **Length-discrimination identity**: rio reports a wrong-length
//!    rejection exactly when the oracle's `baseFromSize` does — the
//!    encoding arm SELECTION is the same function on both sides.

use libfuzzer_sys::fuzz_target;
use rio_nix::hash::{HashAlgo, HashError, NixHash};
use rio_nix::hash_oracle::{self, OracleReject};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else {
        return;
    };
    let algo = match sel % 3 {
        0 => HashAlgo::SHA1,
        1 => HashAlgo::SHA256,
        _ => HashAlgo::SHA512,
    };
    let Ok(s) = std::str::from_utf8(rest) else {
        return;
    };

    let rio = NixHash::parse_nonsri_unprefixed(algo, s);
    let oracle = hash_oracle::parse_nonsri_unprefixed_oracle(algo, s);

    match (&rio, &oracle) {
        (Ok(h), Ok(d)) => assert_eq!(
            h.digest(),
            &d[..],
            "digest disagreement on {s:?} ({algo:?})"
        ),
        (Ok(_), Err(rej)) => panic!(
            "containment violated: rio accepted {s:?} ({algo:?}) but the oracle rejected with {rej:?}"
        ),
        (Err(HashError::WrongEncodedLength { .. }), rej) => assert_eq!(
            rej,
            &Err(OracleReject::WrongLength),
            "length-arm disagreement on {s:?} ({algo:?})"
        ),
        (Err(_), rej) => assert!(
            !matches!(rej, Err(OracleReject::WrongLength)),
            "rio entered a codec the oracle length-rejects: {s:?} ({algo:?})"
        ),
    }
});
