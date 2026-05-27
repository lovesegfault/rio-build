#![no_main]

//! Fuzz `MountdVerifier::verify_for_build` — the Mount-admission token
//! parser. rio-mountd runs this on the token string a peer supplies in
//! `Mount{}` over the world-connectable (token-mode) UDS, i.e. on
//! attacker-controlled bytes from a hostile builder pod, so the parser
//! must never panic and must never run serde on unauthenticated bytes
//! (signature first; see the verification-order unit tests).
//!
//! The verifier under test is the `Dual` shape so both arms parse: the
//! `rmt2.` Ed25519 envelope (split → base64 → signature → claims JSON)
//! and the legacy two-segment HMAC envelope. Keys are fixed so the seed
//! corpus (tokens signed by the same fixed keys — see
//! `corpus/mountd_token_verify/`) exercises the post-signature claim
//! checks, not just the early rejects.

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use rio_auth::hmac::HmacKey;
use rio_auth::mountd_token::{MountdSigningKey, MountdTrustRoots, MountdVerifier};

/// Fixed Ed25519 seed: corpus seeds prefixed `seed-rmt2-` are signed by
/// this key (and `seed-legacy-` by [`HMAC_KEY`]) so they verify all the
/// way to the claim checks.
const ED25519_SEED: [u8; 32] = [0x41; 32];
/// Fixed HMAC key for the legacy arm.
const HMAC_KEY: &[u8] = b"mountd-fuzz-hmac-key-32-bytes-ok";

static VERIFIER: LazyLock<MountdVerifier> = LazyLock::new(|| {
    let signer =
        MountdSigningKey::from_seed("rio-mountd-fuzz", &ED25519_SEED).expect("fixed seed is valid");
    let roots = MountdTrustRoots::parse(&signer.trust_root_entry()).expect("own trust root parses");
    MountdVerifier::from_parts(Some(HmacKey::from_key(HMAC_KEY.to_vec())), Some(roots))
        .expect("both arms configured")
});

fuzz_target!(|data: &[u8]| {
    // The wire carries the token as a UTF-8 string field; non-UTF-8
    // inputs cannot reach the verifier.
    let Ok(token) = std::str::from_utf8(data) else {
        return;
    };
    // Any Ok/Err outcome is fine — we are hunting panics (slice index,
    // integer overflow, allocation blow-ups) in the envelope split,
    // base64 decode, signature handling, and claims deserialization.
    // Run both verifier postures so the node-check arm is covered too.
    let _ = VERIFIER.verify_for_build(token, "b-fuzz_drv", Some("node-a"));
    let _ = VERIFIER.verify_for_build(token, "b-fuzz_drv", None);
});
