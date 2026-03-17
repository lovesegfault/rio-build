#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::refscan::RefScanSink;
use std::io::Write;

// Fixed candidate set (fuzzer explores the input-byte space, not the
// candidate space). Two candidates to give the hash-map probe something
// to compare against when a valid-base32 window does appear.
const CAND_A: [u8; 32] = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
const CAND_B: [u8; 32] = *b"v5sv61sszx301i0x6xysaqzla09nksnd";

fuzz_target!(|input: &[u8]| {
    let mut sink = RefScanSink::new([CAND_A, CAND_B]);

    // Feed in irregular chunks to exercise seam logic. Chunk size is
    // derived from the first input byte (1..=60) so libFuzzer can
    // discover interesting chunk boundaries.
    let chunk_size = (input.first().copied().unwrap_or(7) as usize % 60) + 1;
    for chunk in input.chunks(chunk_size) {
        let _ = sink.write_all(chunk);
    }

    let found = sink.into_found();

    // Oracle: result ⊆ {CAND_A, CAND_B}. Anything else is a false
    // positive → panic. (The scanner only inserts from `remaining`,
    // which is initialized from the candidate set — this is a sanity
    // check on that invariant under fuzzing.)
    for h in &found {
        assert!(*h == CAND_A || *h == CAND_B, "false positive: {h:?}");
    }
});
