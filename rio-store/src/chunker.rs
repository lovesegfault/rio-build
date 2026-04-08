//! Content-defined chunking for NAR deduplication.
//!
//! FastCDC picks chunk boundaries based on content, not position. A small
//! edit to one file in a big NAR only perturbs the chunk containing that
//! file — everything before and after still hashes the same. That's the
// r[impl store.cas.fastcdc]
//! whole point of chunked dedup: two NARs that are 70% identical should
//! share 70% of their chunks.
//!
//! # Size parameters (from `store.md:11`)
//!
//! - **min = 16 KiB**: FastCDC won't cut before this. Very small chunks
//!   would mean many S3 objects + large manifests for little dedup gain.
//! - **avg = 64 KiB**: the target. FastCDC treats this as the "normal"
//!   boundary-detection window.
//! - **max = 256 KiB**: hard cap. Without this, a long run of high-entropy
//!   data (encrypted blob, compressed archive) could yield one giant chunk
//!   — which has bad dedup properties (any byte change = whole chunk miss)
//!   and bad S3 characteristics (one slow GET blocks the whole reassembly).
//!
//! # BLAKE3 for chunk addressing
//!
//! Chunk hashes are BLAKE3, not SHA-256. The Nix protocol never sees these
//! — they're internal storage keys only. BLAKE3 is ~3× faster than SHA-256
//! for this workload and has the same collision resistance. The
//! `store.md:38` "hash domain separation" table makes this explicit: SHA-256
//! for everything Nix-facing, BLAKE3 for chunks only.

use fastcdc::v2020::FastCDC;

/// Minimum chunk size. FastCDC won't cut before this many bytes.
pub const CHUNK_MIN: u32 = 16 * 1024;

/// Average chunk size. The "expected" boundary spacing.
pub const CHUNK_AVG: u32 = 64 * 1024;

/// Maximum chunk size. Hard cap — FastCDC forces a cut at this length
/// even if no content-defined boundary is found.
pub const CHUNK_MAX: u32 = 256 * 1024;

/// A single chunk: its BLAKE3 hash and a borrowed slice into the source.
///
/// The slice borrows from the input `&[u8]` — no copy. Callers that need
/// owned data (S3 upload) convert to `Bytes::copy_from_slice()` at the
/// use site, which is one copy total (unavoidable: the input is a
/// contiguous Vec<u8>, S3 wants owned Bytes).
#[derive(Debug)]
pub struct Chunk<'a> {
    /// BLAKE3 hash of `data`. This is the chunk's storage key.
    pub hash: [u8; 32],
    /// Borrowed slice into the source NAR.
    pub data: &'a [u8],
}

/// Split a NAR into content-defined chunks.
///
/// Returns chunks in order (reassembly just concatenates). Every byte of
/// `nar` appears in exactly one chunk; concatenating all `chunk.data`
/// reconstructs the input bit-for-bit.
///
/// Empty input → empty output. A NAR small enough to be one chunk
/// (< CHUNK_MAX) → one chunk spanning the whole thing. In practice
/// callers gate on INLINE_THRESHOLD (256 KiB) before calling this, so
/// small NARs never reach here — but the function handles them correctly
/// anyway (useful for tests).
///
/// # Cost
///
/// One pass over the data for FastCDC boundaries (fast — it's a rolling
/// hash). One pass per chunk for BLAKE3 (also fast — ~1 GB/s). Total is
/// roughly 2× the input throughput, dominated by BLAKE3.
pub fn chunk_nar(nar: &[u8]) -> Vec<Chunk<'_>> {
    // FastCDC::new panics on empty input (reasonable — no boundaries in
    // nothing). Early-return instead.
    if nar.is_empty() {
        return Vec::new();
    }

    FastCDC::new(nar, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
        .map(|entry| {
            let data = &nar[entry.offset..entry.offset + entry.length];
            Chunk {
                hash: *blake3::hash(data).as_bytes(),
                data,
            }
        })
        .collect()
}

// r[verify store.cas.fastcdc]
#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::fixtures::pseudo_random_bytes;

    #[test]
    fn empty_input_empty_output() {
        assert!(chunk_nar(&[]).is_empty());
    }

    #[test]
    fn small_input_one_chunk() {
        // Under CHUNK_MIN → FastCDC emits it as a single chunk (there's
        // no boundary to find). 100 bytes = plausible tiny NAR.
        let data = vec![0x42u8; 100];
        let chunks = chunk_nar(&data);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, data.as_slice());
        assert_eq!(chunks[0].hash, *blake3::hash(&data).as_bytes());
    }

    /// The roundtrip property: concatenating all chunks reconstructs the
    /// input. This is THE invariant — if this doesn't hold, GetPath
    /// serves corrupt NARs.
    #[test]
    fn chunks_concatenate_to_input() {
        // 1 MiB of pseudo-random-ish data. Not truly random (determinism
        // makes test failures reproducible) but varied enough that FastCDC
        // finds multiple boundaries.
        let data = pseudo_random_bytes(0, 1024 * 1024);
        let chunks = chunk_nar(&data);

        // Multiple chunks expected (1 MiB / 64 KiB avg ≈ 16).
        assert!(
            chunks.len() > 1,
            "1 MiB should chunk into >1 pieces, got {}",
            chunks.len()
        );

        // Concatenate and compare.
        let reassembled: Vec<u8> = chunks.iter().flat_map(|c| c.data.iter().copied()).collect();
        assert_eq!(reassembled, data, "concatenation must reconstruct input");
    }

    /// No chunk exceeds CHUNK_MAX. Without the max, high-entropy data
    /// (where FastCDC's rolling hash never matches the boundary pattern)
    /// would emit one giant chunk. The max is a hard cap.
    #[test]
    fn chunk_size_bounds() {
        // 5 MiB. Zero bytes are low-entropy — FastCDC finds boundaries
        // easily. But we also want to test the max, so mix in a pattern.
        let data: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
        let chunks = chunk_nar(&data);

        for (i, chunk) in chunks.iter().enumerate() {
            // The LAST chunk can be smaller than CHUNK_MIN — there's
            // nothing to enforce it (can't cut past EOF).
            if i < chunks.len() - 1 {
                assert!(
                    chunk.data.len() >= CHUNK_MIN as usize,
                    "chunk {i} is {} bytes, below CHUNK_MIN {}",
                    chunk.data.len(),
                    CHUNK_MIN
                );
            }
            assert!(
                chunk.data.len() <= CHUNK_MAX as usize,
                "chunk {i} is {} bytes, above CHUNK_MAX {}",
                chunk.data.len(),
                CHUNK_MAX
            );
        }
    }

    /// Stability: same input → same chunks. FastCDC is deterministic
    /// (no randomness, no state between calls). If this fails, dedup is
    /// broken — the same NAR uploaded twice would chunk differently.
    #[test]
    fn chunking_is_deterministic() {
        let data: Vec<u8> = (0u64..512 * 1024).map(|i| (i * 13 % 256) as u8).collect();
        let chunks_a = chunk_nar(&data);
        let chunks_b = chunk_nar(&data);

        assert_eq!(chunks_a.len(), chunks_b.len());
        for (a, b) in chunks_a.iter().zip(chunks_b.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.data, b.data);
        }
    }

    /// The dedup property: two inputs that share a large prefix should
    /// share most of their early chunks. A small change at byte N should
    /// only perturb the chunk containing N (plus maybe one neighbor —
    /// FastCDC boundaries can shift locally).
    ///
    /// This is what makes chunked storage worthwhile over whole-NAR.
    #[test]
    fn shared_prefix_shares_chunks() {
        // Two 1 MiB blobs that differ only in their last 1 KiB.
        let base = pseudo_random_bytes(0, 1024 * 1024);
        let mut modified = base.clone();
        let tail_start = base.len() - 1024;
        for b in &mut modified[tail_start..] {
            *b = b.wrapping_add(1);
        }

        let chunks_a = chunk_nar(&base);
        let chunks_b = chunk_nar(&modified);

        // Find the first differing chunk. All chunks before that should
        // be identical (hash + offset).
        let first_diff = chunks_a
            .iter()
            .zip(chunks_b.iter())
            .position(|(a, b)| a.hash != b.hash)
            .unwrap_or_else(|| chunks_a.len().min(chunks_b.len()));

        // The change was in the last 1 KiB. At avg 64 KiB chunks, we have
        // ~16 chunks; only the last 1-2 should differ. "Most" is conservative:
        // at least 50% shared proves the property without being fragile to
        // FastCDC's exact boundary placement.
        let shared_fraction = first_diff as f64 / chunks_a.len() as f64;
        assert!(
            shared_fraction > 0.5,
            "only {shared_fraction:.0}% of chunks shared; \
             content-defined chunking should localize small changes \
             (first diff at chunk {first_diff}/{}, total chunks)",
            chunks_a.len()
        );
    }
}
