//! Streaming NAR reference scanner.
//!
//! Detects which store-path hash parts appear anywhere in a byte stream,
//! using a Boyer-Moore-style skip-scan over the restricted nixbase32
//! alphabet. Algorithm matches Nix C++ `src/libstore/references.cc`.
//!
//! Store-path references in build outputs are always embedded as their
//! 32-character nixbase32 hash prefix (the `abc…xyz` in
//! `/nix/store/abc…xyz-name`). The scanner looks for exactly those 32-byte
//! windows in the NAR stream and reports which members of a *candidate set*
//! appeared. The candidate set is bounded (input closure + outputs of the
//! derivation being built), so the hash-map probe is cheap.
//!
//! **Not** Aho-Corasick: the nixbase32 alphabet is only 32 of 256 byte
//! values (~12.5%), so scanning each 32-byte window *backwards from the
//! end* lets us skip `j+1` bytes on the first non-base32 byte at offset
//! `j`. Binary NAR sections (ELF headers, compressed blobs) traverse at
//! ~O(n/32). Text sections degrade toward O(n) but never worse than naïve.

use std::collections::{HashMap, HashSet};
use std::io;

use crate::store_path::{HASH_CHARS, StorePath};

/// Lookup table: `IS_BASE32[b] == true` iff byte `b` is in the nixbase32
/// alphabet (`0123456789abcdfghijklmnpqrsvwxyz` — note: no `e`/`o`/`t`/`u`).
/// Initialized at compile time — the hot path is a single array index.
const IS_BASE32: [bool; 256] = {
    let mut t = [false; 256];
    let alphabet = b"0123456789abcdfghijklmnpqrsvwxyz";
    let mut i = 0;
    while i < alphabet.len() {
        t[alphabet[i] as usize] = true;
        i += 1;
    }
    t
};

/// Streaming reference scanner. Feed NAR bytes via [`io::Write`];
/// [`into_found`](Self::into_found) returns the subset of candidate hash
/// parts that appeared anywhere in the stream.
///
/// Chunk-boundary safe: a hash straddling two `write()` calls is detected
/// via a rolling tail buffer (≤ `HASH_CHARS - 1` bytes).
///
/// Algorithm matches Nix's `RefScanSink` (`src/libstore/references.cc`) —
/// a Boyer-Moore-style skip-scan over the restricted nixbase32 alphabet.
#[derive(Debug)]
pub struct RefScanSink {
    /// Hash prefixes not yet found. Moved to `found` on first match
    /// (so we stop probing the hash map for already-found refs).
    remaining: HashSet<[u8; HASH_CHARS]>,
    found: HashSet<[u8; HASH_CHARS]>,
    /// Last ≤ `HASH_CHARS - 1` bytes of the previous chunk, for seam
    /// searching. Never exceeds `HASH_CHARS - 1` between calls.
    tail: Vec<u8>,
}

impl RefScanSink {
    /// Construct a scanner for the given candidate hash prefixes.
    ///
    /// `candidates` are the 32-byte nixbase32 **hash parts** of store paths
    /// (i.e., the `abc…xyz` in `/nix/store/abc…xyz-name`), as ASCII bytes.
    /// Callers typically build this from [`CandidateSet::hashes`] or
    /// directly via `StorePath::hash_part().as_bytes().try_into()`.
    pub fn new(candidates: impl IntoIterator<Item = [u8; HASH_CHARS]>) -> Self {
        Self {
            remaining: candidates.into_iter().collect(),
            found: HashSet::new(),
            tail: Vec::with_capacity(HASH_CHARS),
        }
    }

    /// Consume the sink and return the set of candidate hashes that
    /// appeared in the stream. Result is always a subset of the
    /// constructor's candidate set — no false positives by construction.
    #[must_use]
    pub fn into_found(self) -> HashSet<[u8; HASH_CHARS]> {
        self.found
    }

    /// Fast path: all candidates already found → subsequent writes are
    /// no-ops. The upload hot loop can check this to skip the scan
    /// entirely once the (typically small) candidate set is exhausted.
    #[inline]
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.remaining.is_empty()
    }

    /// Core search: Boyer-Moore-style skip-scan. Mutates
    /// `remaining`/`found` in place. Separate from `write()` so both seam
    /// and full-chunk paths can call it.
    fn search(&mut self, data: &[u8]) {
        if data.len() < HASH_CHARS || self.remaining.is_empty() {
            return;
        }
        let mut i = 0;
        'outer: while i + HASH_CHARS <= data.len() {
            // Check from the END of the window backwards. On first
            // non-base32 byte at offset j, no window starting in i..=i+j
            // can possibly be a valid hash — skip all of them.
            let mut j = HASH_CHARS - 1;
            loop {
                if !IS_BASE32[data[i + j] as usize] {
                    i += j + 1;
                    continue 'outer;
                }
                if j == 0 {
                    break;
                }
                j -= 1;
            }
            // All 32 bytes are valid nixbase32. Probe the candidate set.
            // Slice is exactly HASH_CHARS bytes by the loop invariant.
            let window: [u8; HASH_CHARS] = data[i..i + HASH_CHARS].try_into().unwrap();
            if self.remaining.remove(&window) {
                self.found.insert(window);
            }
            i += 1;
        }
    }
}

impl io::Write for RefScanSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // Early out: all refs found, nothing left to scan for.
        // Still return Ok(data.len()) — the tee keeps writing through us.
        if self.remaining.is_empty() {
            return Ok(data.len());
        }

        // Seam search: a hash may start in the tail and end in `data`.
        // tail is ≤31 bytes; we append up to 32 bytes from `data` so the
        // seam buffer is ≤63 bytes — always stack-cheap.
        let head_len = data.len().min(HASH_CHARS);
        if !self.tail.is_empty() {
            // `take` + truncate-restore: zero-alloc (Vec capacity
            // reused), avoids the `&mut self` vs `&seam` borrow conflict.
            // Crucially: the tail-update below MUST see the ORIGINAL
            // tail, not the extended seam (Nix C++ `auto s = tail;` is
            // a copy — `tail` is unchanged by the seam search).
            let mut seam = std::mem::take(&mut self.tail);
            let orig_len = seam.len();
            seam.extend_from_slice(&data[..head_len]);
            self.search(&seam);
            seam.truncate(orig_len);
            self.tail = seam;
        }

        // Full-chunk search. Overlaps the seam's tail region by ≤31
        // bytes — harmless (hash lookups are idempotent, and found hashes
        // are already removed from `remaining` so they won't re-match).
        self.search(data);

        // Update tail: keep the last (HASH_CHARS - 1) bytes of the stream
        // seen so far. Matches Nix's `references.cc:49-52` tail-shift.
        //
        // If `data` is ≥ HASH_CHARS bytes, the old tail is irrelevant
        // (any hash ending in `data` starts in `data`); otherwise keep
        // `rest = HASH_CHARS - head_len` bytes of the old tail.
        let rest = HASH_CHARS - head_len;
        if rest < self.tail.len() {
            self.tail.drain(..self.tail.len() - rest);
        }
        self.tail.extend_from_slice(&data[data.len() - head_len..]);
        // Cap at HASH_CHARS - 1 (we never need more overlap than that).
        if self.tail.len() >= HASH_CHARS {
            self.tail.drain(..self.tail.len() - (HASH_CHARS - 1));
        }

        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Compile-time check: scanner is usable in a tee alongside async hashing.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RefScanSink>();
    assert_send_sync::<CandidateSet>();
};

/// Extract the 32-byte nixbase32 hash part from a full store path string
/// (`/nix/store/abc…xyz-name`) for use as a [`RefScanSink`] candidate.
///
/// Returns `None` if the path doesn't parse — caller decides whether that's
/// fatal (it shouldn't be for paths from a parsed `Derivation`, but may be
/// for user-supplied input).
#[must_use]
pub fn hash_part_of(path: &str) -> Option<[u8; HASH_CHARS]> {
    let sp = StorePath::parse(path).ok()?;
    // hash_part() returns a fresh String (nixbase32::encode). Always 32
    // ASCII bytes by construction — try_into can't fail, but .ok() keeps
    // this function total.
    sp.hash_part().as_bytes().try_into().ok()
}

/// A `hash_part → full_path` map. Bridges the scanner's hash-domain output
/// back to the full `/nix/store/…` paths needed for `PathInfo.references`.
///
/// Typical flow:
/// 1. [`CandidateSet::from_paths`] with the derivation's input closure + outputs
/// 2. [`CandidateSet::hashes`] → feed into [`RefScanSink::new`]
/// 3. Scan the NAR stream via `Write`
/// 4. [`RefScanSink::into_found`] → [`CandidateSet::resolve`] → sorted `Vec<String>`
#[derive(Debug, Clone)]
pub struct CandidateSet {
    map: HashMap<[u8; HASH_CHARS], String>,
}

impl CandidateSet {
    /// Build from an iterator of full store path strings. Paths that don't
    /// parse are silently skipped (they can't possibly match — a malformed
    /// path by definition has no valid hash part to embed in a NAR).
    pub fn from_paths<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let map = paths
            .into_iter()
            .filter_map(|p| {
                let p = p.as_ref();
                hash_part_of(p).map(|h| (h, p.to_string()))
            })
            .collect();
        Self { map }
    }

    /// Number of valid candidates (post-parse).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True if no valid candidates remain.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Hash parts for feeding into [`RefScanSink::new`].
    pub fn hashes(&self) -> impl Iterator<Item = [u8; HASH_CHARS]> + '_ {
        self.map.keys().copied()
    }

    /// Map found hash parts back to full store paths.
    ///
    /// Output is **sorted** for determinism: `PathInfo.references` order
    /// affects the narinfo signature fingerprint — MUST be stable across
    /// runs. Nix itself uses a `BTreeSet` (`StorePathSet`) for the same
    /// reason.
    #[must_use]
    pub fn resolve(&self, found: &HashSet<[u8; HASH_CHARS]>) -> Vec<String> {
        let mut refs: Vec<String> = found
            .iter()
            .filter_map(|h| self.map.get(h).cloned())
            .collect();
        refs.sort();
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A valid 32-char nixbase32 hash (from a real store path).
    const HASH_A: [u8; HASH_CHARS] = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    /// Another valid hash.
    const HASH_B: [u8; HASH_CHARS] = *b"v5sv61sszx301i0x6xysaqzla09nksnd";
    /// Third valid hash (never appears in test data — negative case).
    const HASH_C: [u8; HASH_CHARS] = *b"0000000000000000000000000000000z";

    // ─── Basic match placement ──────────────────────────────────────────

    #[test]
    fn empty_candidate_set_yields_empty_result() {
        let mut sink = RefScanSink::new(std::iter::empty());
        assert!(sink.is_exhausted());
        sink.write_all(&HASH_A).unwrap();
        sink.write_all(b"arbitrary bytes including /nix/store/ paths")
            .unwrap();
        assert!(sink.into_found().is_empty());
    }

    #[test]
    fn finds_hash_at_chunk_start() {
        let mut sink = RefScanSink::new([HASH_A]);
        let mut data = HASH_A.to_vec();
        data.extend_from_slice(b" trailing");
        sink.write_all(&data).unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    #[test]
    fn finds_hash_at_chunk_end() {
        let mut sink = RefScanSink::new([HASH_A]);
        let mut data = b"leading ".to_vec();
        data.extend_from_slice(&HASH_A);
        sink.write_all(&data).unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    #[test]
    fn finds_hash_in_middle_of_chunk() {
        let mut sink = RefScanSink::new([HASH_A]);
        sink.write_all(b"prefix /nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-hello-2.12 suffix")
            .unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    // ─── False-positive guards ───────────────────────────────────────────

    #[test]
    fn candidate_not_in_stream_is_not_found() {
        // HASH_C never written. Must not appear in result.
        let mut sink = RefScanSink::new([HASH_A, HASH_C]);
        sink.write_all(&HASH_A).unwrap();
        let found = sink.into_found();
        assert!(found.contains(&HASH_A));
        assert!(!found.contains(&HASH_C));
    }

    #[test]
    fn no_false_positive_for_valid_base32_not_in_candidates() {
        // 32 valid base32 chars, but NOT in the candidate set.
        let decoy = *b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // all valid base32
        let mut sink = RefScanSink::new([HASH_A]);
        sink.write_all(&decoy).unwrap();
        assert!(sink.into_found().is_empty());
    }

    #[test]
    fn no_false_positive_for_near_miss() {
        // 31 matching chars + 1 off. Must NOT match.
        let mut sink = RefScanSink::new([HASH_A]);
        // last char differs: 'a' → 'b'
        sink.write_all(b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3b").unwrap();
        assert!(sink.into_found().is_empty());
    }

    #[test]
    fn no_false_positive_in_binary_garbage() {
        // All-0xFF is not nixbase32 — fast-skip the whole thing, find nothing.
        let mut sink = RefScanSink::new([HASH_A]);
        sink.write_all(&[0xFFu8; 10_000]).unwrap();
        assert!(sink.into_found().is_empty());
    }

    // ─── Boyer-Moore skip behaviour ─────────────────────────────────────

    #[test]
    fn boyer_moore_skip_over_binary_then_find() {
        // Binary garbage (0xFF is not base32) followed by a hash. The skip
        // should land us directly at the hash without scanning every byte.
        // Correctness test only — perf is measured elsewhere.
        let mut data = vec![0xFFu8; 10_000];
        data.extend_from_slice(&HASH_A);
        let mut sink = RefScanSink::new([HASH_A]);
        sink.write_all(&data).unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    // ─── Seam / chunk-boundary ──────────────────────────────────────────

    #[test]
    fn finds_hash_straddling_chunk_boundary() {
        // THE seam test. Split a hash across two write() calls at every
        // possible offset — each split point must be detected.
        for split in 1..HASH_CHARS {
            let mut sink = RefScanSink::new([HASH_A]);
            let mut chunk1 = b"leading garbage ".to_vec();
            chunk1.extend_from_slice(&HASH_A[..split]);
            let mut chunk2 = HASH_A[split..].to_vec();
            chunk2.extend_from_slice(b" trailing");
            sink.write_all(&chunk1).unwrap();
            sink.write_all(&chunk2).unwrap();
            assert_eq!(
                sink.into_found(),
                HashSet::from([HASH_A]),
                "failed at split={split}"
            );
        }
    }

    #[test]
    fn finds_hash_straddling_many_tiny_chunks() {
        // Pathological: every write() is 1 byte. Seam logic must still work.
        let mut sink = RefScanSink::new([HASH_A]);
        for &b in b"xx".iter().chain(HASH_A.iter()).chain(b"yy".iter()) {
            sink.write_all(&[b]).unwrap();
        }
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    #[test]
    fn finds_hash_spread_across_three_chunks() {
        // Hash split across three writes. Exercises tail buffer
        // accumulation when each chunk is < HASH_CHARS.
        let mut sink = RefScanSink::new([HASH_A]);
        sink.write_all(&HASH_A[..10]).unwrap();
        sink.write_all(&HASH_A[10..20]).unwrap();
        sink.write_all(&HASH_A[20..]).unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    // ─── Multiple candidates ────────────────────────────────────────────

    #[test]
    fn multiple_candidates_subset_found() {
        // Three candidates; stream contains two of them. Verify exact
        // subset semantics.
        let mut sink = RefScanSink::new([HASH_A, HASH_B, HASH_C]);
        let mut data = b"foo ".to_vec();
        data.extend_from_slice(&HASH_B);
        data.extend_from_slice(b" bar ");
        data.extend_from_slice(&HASH_A);
        sink.write_all(&data).unwrap();
        let found = sink.into_found();
        assert_eq!(found, HashSet::from([HASH_A, HASH_B]));
        assert!(!found.contains(&HASH_C));
    }

    #[test]
    fn self_reference_detected() {
        // An output can contain its own path (shebangs, rpaths). The
        // candidate set includes the output itself — scanner must find it.
        let mut sink = RefScanSink::new([HASH_B]);
        sink.write_all(b"#!/nix/store/v5sv61sszx301i0x6xysaqzla09nksnd-foo/bin/sh\n")
            .unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_B]));
    }

    // ─── Exhaustion short-circuit ───────────────────────────────────────

    #[test]
    fn is_exhausted_short_circuits() {
        let mut sink = RefScanSink::new([HASH_A]);
        assert!(!sink.is_exhausted());
        sink.write_all(&HASH_A).unwrap();
        assert!(sink.is_exhausted());
        // Subsequent writes are no-ops (can't observe directly, but at
        // least verify no panic / no state corruption).
        sink.write_all(&[0u8; 1024]).unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    #[test]
    fn tail_buffer_bounded_after_exhaustion() {
        // After exhaustion, the early-out must not let the tail grow
        // unbounded. Write many small chunks; tail stays ≤ HASH_CHARS.
        let mut sink = RefScanSink::new([HASH_A]);
        sink.write_all(&HASH_A).unwrap();
        assert!(sink.is_exhausted());
        for _ in 0..1000 {
            sink.write_all(b"x").unwrap();
        }
        assert!(sink.tail.len() < HASH_CHARS);
    }

    // ─── End-to-end NAR roundtrip ───────────────────────────────────────

    #[test]
    fn nar_roundtrip_scan() {
        // End-to-end: write a file containing a store path, dump it as a
        // NAR, feed the NAR through the scanner. This is the shape of the
        // real worker upload flow.
        use crate::nar;
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("output");
        std::fs::write(
            &out,
            b"RPATH=/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-glibc-2.38/lib\n",
        )
        .unwrap();

        let mut sink = RefScanSink::new([HASH_A]);
        nar::dump_path_streaming(&out, &mut sink).unwrap();
        assert_eq!(sink.into_found(), HashSet::from([HASH_A]));
    }

    // ─── CandidateSet helpers ───────────────────────────────────────────

    #[test]
    fn candidate_set_resolve_sorted() {
        let cs = CandidateSet::from_paths([
            "/nix/store/v5sv61sszx301i0x6xysaqzla09nksnd-foo",
            "/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-hello-2.12",
        ]);
        assert_eq!(cs.len(), 2);
        // resolve() must sort: the 7rjj... path lexically precedes v5sv...
        let found = HashSet::from([HASH_A, HASH_B]);
        assert_eq!(
            cs.resolve(&found),
            vec![
                "/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-hello-2.12".to_string(),
                "/nix/store/v5sv61sszx301i0x6xysaqzla09nksnd-foo".to_string(),
            ]
        );
    }

    #[test]
    fn candidate_set_skips_unparseable() {
        let cs = CandidateSet::from_paths([
            "/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-valid",
            "not-a-store-path",
            "/nix/store/SHORT-invalid",
        ]);
        assert_eq!(cs.len(), 1);
        assert!(!cs.is_empty());
    }

    #[test]
    fn hash_part_of_roundtrip() {
        let h = hash_part_of("/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-hello").unwrap();
        assert_eq!(h, HASH_A);
        assert!(hash_part_of("garbage").is_none());
    }

    // ─── Proptest ───────────────────────────────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Property: a candidate hash embedded at a random offset in
            /// random bytes is always found, regardless of how the stream
            /// is chunked.
            #[test]
            fn prop_finds_hash_at_random_offset(
                prefix in proptest::collection::vec(any::<u8>(), 0..500),
                suffix in proptest::collection::vec(any::<u8>(), 0..500),
                chunk_size in 1usize..200,
            ) {
                let mut data = prefix;
                data.extend_from_slice(&HASH_A);
                data.extend_from_slice(&suffix);

                let mut sink = RefScanSink::new([HASH_A]);
                for chunk in data.chunks(chunk_size) {
                    sink.write_all(chunk).unwrap();
                }
                prop_assert!(sink.into_found().contains(&HASH_A));
            }

            /// Property: result is always a subset of the candidate set,
            /// for arbitrary input bytes and arbitrary chunking.
            /// Verifies no false positives can be manufactured.
            #[test]
            fn prop_result_subset_of_candidates(
                data in proptest::collection::vec(any::<u8>(), 0..1000),
                chunk_size in 1usize..128,
            ) {
                let candidates = HashSet::from([HASH_A, HASH_B]);
                let mut sink = RefScanSink::new(candidates.iter().copied());
                for chunk in data.chunks(chunk_size.max(1)) {
                    sink.write_all(chunk).unwrap();
                }
                let found = sink.into_found();
                prop_assert!(found.is_subset(&candidates));
            }
        }
    }
}
