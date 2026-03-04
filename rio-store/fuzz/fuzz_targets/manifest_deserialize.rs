#![no_main]

//! Fuzz `Manifest::deserialize`. The manifest format is length-prefixed
//! (version byte + N×36-byte entries) and read from PostgreSQL BYTEA —
//! untrusted in the sense that PG corruption or a hostile PG admin could
//! feed us arbitrary bytes. The parser has bounds checks (MAX_CHUNKS,
//! ENTRY_SIZE divisibility); fuzzing probes for gaps.

use libfuzzer_sys::fuzz_target;
use rio_store::manifest::Manifest;

fuzz_target!(|data: &[u8]| {
    // Any result (Ok or Err) is fine — we're only hunting panics.
    // If it DOES parse, re-serialize and verify roundtrip (same
    // "garbage-in = error, valid-in = roundtrip" property the proptest
    // at manifest.rs:308 checks, but with libFuzzer's coverage guidance).
    if let Ok(m) = Manifest::deserialize(data) {
        let reser = m.serialize();
        assert_eq!(reser, data, "deserialize→serialize must roundtrip");
    }
});
