#![no_main]

//! Fuzz the untrusted-input path of the canonical `rio.drv.v1.Derivation`
//! codec (ADR-024): protobuf decode + structural validation, then — for
//! inputs that survive — the differential identity invariant the gateway
//! relies on: a validated message converts to rio-nix form, its ATerm
//! reparses, and converting back re-encodes to the message's own
//! canonical bytes with a stable digest.

use libfuzzer_sys::fuzz_target;
use prost::Message;
use rio_proto::derivation_util::{derivation_digest, from_proto, to_proto, validate_derivation};
use rio_proto::drv;

fuzz_target!(|data: &[u8]| {
    let Ok(msg) = drv::Derivation::decode(data) else {
        return;
    };
    if validate_derivation(&msg).is_err() {
        return;
    }
    // Validated ⇒ the message is in canonical form; its encode is THE
    // canonical bytes (not necessarily equal to `data` — unknown
    // fields / non-minimal varints are the NonCanonical reject in
    // verify_drv_blob, not a panic here).
    let canonical = msg.encode_to_vec();
    let digest = derivation_digest(&msg);

    // Conversion is fallible only on non-UTF-8 / Nix invariants.
    let Ok(nix_drv) = from_proto(&msg) else {
        return;
    };
    let aterm = nix_drv.to_aterm();
    let reparsed = rio_nix::derivation::Derivation::parse(&aterm)
        .expect("ATerm written by rio-nix must reparse");
    assert_eq!(reparsed, nix_drv, "ATerm round-trip must be lossless");
    let reproto = to_proto(&reparsed);
    assert_eq!(
        reproto.encode_to_vec(),
        canonical,
        "validated message must survive proto -> rio-nix -> proto bit-exactly"
    );
    assert_eq!(derivation_digest(&reproto), digest);
});
