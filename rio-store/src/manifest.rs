//! Chunk manifest serialization.
//!
//! A manifest is the ordered list of (BLAKE3 hash, size) pairs that
//! describes how to reassemble a chunked NAR. It's stored in PG
//! (`manifest_data.chunk_list` BYTEA) and read on every GetPath.
// r[impl store.manifest.format]
//!
//! # Format
//!
//! ```text
//! [version: u8 = 1] [entry: 36 bytes]*
//!   where entry = [blake3: 32 bytes] [size: u32 LE]
//! ```
//!
//! Compact, self-describing (version byte), and trivially seekable (fixed
//! stride). Not protobuf because:
//! - It's stored in PG, not sent over gRPC — no wire-compat concerns
//! - Protobuf varints would complicate the fixed-stride property
//! - A 10 GB NAR at 64 KB chunks is ~160k entries × 36 bytes = ~5.6 MB of
//!   manifest; the format overhead is negligible either way
//!
//! # Why `u32` for size
//!
//! CHUNK_MAX is 256 KiB = fits in u32 with 16k× headroom. u64 would double
//! the per-entry size (36 → 40 bytes) for no reason. If a future CHUNK_MAX
//! exceeded 4 GiB (it won't — that's "whole NAR" territory), bump the
//! version byte and widen.

use thiserror::Error;

/// Format version. Bump for incompatible changes; add a new `deserialize`
/// branch that handles the old version for migration.
const VERSION: u8 = 1;

/// Bytes per entry: 32 (BLAKE3) + 4 (u32 size).
const ENTRY_SIZE: usize = 36;

/// Upper bound on chunk count. 200k chunks at 64 KiB avg ≈ 12.2 GiB NAR —
/// above MAX_NAR_SIZE (4 GiB). Above 10 GiB / CHUNK_MIN too. If a manifest
/// claims more, it's corrupted or hostile; don't preallocate 200k × 36 B
/// = 7 MB just because the length field says so.
///
/// Not a `const { assert! }` tripwire against MAX_NAR_SIZE because that
/// constant lives in rio-common and pulling it here would be a dep-cycle
/// concern. The margin is wide enough that manual drift-checking suffices.
pub const MAX_CHUNKS: usize = 200_000;

/// A single manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestEntry {
    /// BLAKE3 hash of the chunk content.
    pub hash: [u8; 32],
    /// Chunk size in bytes. Always ≤ CHUNK_MAX (256 KiB), so u32 is plenty.
    pub size: u32,
}

/// The chunk manifest: ordered list of entries.
///
/// Reassembly = fetch each chunk by hash, concatenate in order, verify
/// BLAKE3 per chunk (and SHA-256 over the whole result as belt-and-
/// suspenders — that's GetPath's job, not ours).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("manifest is empty (no version byte)")]
    Empty,

    #[error("unknown manifest version {0} (expected {VERSION})")]
    UnknownVersion(u8),

    #[error("manifest body length {0} is not a multiple of {ENTRY_SIZE} (truncated or corrupt)")]
    BadLength(usize),

    #[error("manifest has {0} entries, exceeds MAX_CHUNKS {MAX_CHUNKS}")]
    TooManyChunks(usize),
}

impl Manifest {
    /// Serialize to the on-disk format.
    ///
    /// Preallocates exactly the right size — no reallocation. Hot path
    /// for PutPath.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.entries.len() * ENTRY_SIZE);
        out.push(VERSION);
        for entry in &self.entries {
            out.extend_from_slice(&entry.hash);
            out.extend_from_slice(&entry.size.to_le_bytes());
        }
        out
    }

    /// Parse the on-disk format.
    ///
    /// Validates: version byte, length divisibility, MAX_CHUNKS bound.
    /// Does NOT validate that chunk hashes exist in S3 or that sizes
    /// match — that's the reassembly layer's job (per-chunk BLAKE3 verify).
    /// This is just "is the byte layout well-formed".
    pub fn deserialize(data: &[u8]) -> Result<Self, ManifestError> {
        let (&version, body) = data.split_first().ok_or(ManifestError::Empty)?;

        if version != VERSION {
            return Err(ManifestError::UnknownVersion(version));
        }

        if body.len() % ENTRY_SIZE != 0 {
            return Err(ManifestError::BadLength(body.len()));
        }

        let count = body.len() / ENTRY_SIZE;
        if count > MAX_CHUNKS {
            // Check BEFORE Vec::with_capacity — don't let a corrupt length
            // field make us preallocate 7 MB (or worse, if a future format
            // change widens entries).
            return Err(ManifestError::TooManyChunks(count));
        }

        let mut entries = Vec::with_capacity(count);
        // chunks_exact: no remainder (we checked divisibility above), and
        // the compiler can prove it, so no bounds-check per iteration.
        for chunk in body.chunks_exact(ENTRY_SIZE) {
            // These try_into()s can't fail — chunks_exact guarantees exactly
            // ENTRY_SIZE bytes, and 32+4=36. expect() documents the invariant
            // instead of silently unwrap()ing.
            let hash: [u8; 32] = chunk[..32]
                .try_into()
                .expect("chunks_exact guarantees 36 bytes");
            let size_bytes: [u8; 4] = chunk[32..36]
                .try_into()
                .expect("chunks_exact guarantees 36 bytes");
            entries.push(ManifestEntry {
                hash,
                size: u32::from_le_bytes(size_bytes),
            });
        }

        Ok(Self { entries })
    }

    /// Total size in bytes when all chunks are concatenated.
    ///
    /// u64 because even though each chunk is u32-sized, 200k × 256 KiB
    /// = 50 GiB > u32::MAX. Used by GetPath to sanity-check against
    /// `narinfo.nar_size` before reassembly — manifest/narinfo drift
    /// (PutPath wrote a manifest whose summed sizes don't match the
    /// declared nar_size) surfaces as a clear DATA_LOSS error rather
    /// than streaming garbage.
    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size as u64).sum()
    }
}

// r[verify store.manifest.format]
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample() -> Manifest {
        Manifest {
            entries: vec![
                ManifestEntry {
                    hash: [0xAA; 32],
                    size: 65536,
                },
                ManifestEntry {
                    hash: [0xBB; 32],
                    size: 12345,
                },
            ],
        }
    }

    #[test]
    fn roundtrip_basic() {
        let m = sample();
        let bytes = m.serialize();
        let parsed = Manifest::deserialize(&bytes).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn serialize_format() {
        // Exact byte layout — locks in the format so any accidental change
        // shows up here, not as a mysterious GetPath integrity failure.
        let m = Manifest {
            entries: vec![ManifestEntry {
                hash: [0x11; 32],
                size: 0x12345678,
            }],
        };
        let bytes = m.serialize();
        assert_eq!(bytes.len(), 1 + 36);
        assert_eq!(bytes[0], VERSION);
        assert_eq!(&bytes[1..33], &[0x11; 32]);
        // u32 LE: 0x12345678 → [0x78, 0x56, 0x34, 0x12]
        assert_eq!(&bytes[33..37], &[0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn empty_manifest_roundtrips() {
        // An empty manifest (no chunks) is valid — it's what you'd get for
        // an empty NAR. Unusual (caller should use inline_blob for tiny
        // NARs) but not an error.
        let m = Manifest { entries: vec![] };
        let bytes = m.serialize();
        assert_eq!(bytes, vec![VERSION]); // just the version byte
        let parsed = Manifest::deserialize(&bytes).unwrap();
        assert_eq!(parsed.entries.len(), 0);
    }

    #[test]
    fn deserialize_rejects_empty() {
        assert!(matches!(
            Manifest::deserialize(&[]),
            Err(ManifestError::Empty)
        ));
    }

    #[test]
    fn deserialize_rejects_unknown_version() {
        assert!(matches!(
            Manifest::deserialize(&[99]),
            Err(ManifestError::UnknownVersion(99))
        ));
    }

    #[test]
    fn deserialize_rejects_truncated() {
        // Version byte + 35 bytes = one byte short of a full entry.
        let truncated = vec![VERSION; 1 + 35];
        assert!(matches!(
            Manifest::deserialize(&truncated),
            Err(ManifestError::BadLength(35))
        ));
    }

    /// Boundary arithmetic: the `body.len() % ENTRY_SIZE` divisibility
    /// check, `/ ENTRY_SIZE` count computation, and `> MAX_CHUNKS`
    /// comparison have 13 candidate cargo-mutants mutations: `%` → `/`
    /// or `*`, `/` → `%` or `*`, `>` → `>=` or `<` or `==`. Each row
    /// below targets a distinct boundary where one of those mutations
    /// would flip the outcome.
    ///
    /// Pattern follows P0373-T2's MAX_FRAME_SIZE boundary: exact, ±1,
    /// and far-from-boundary cases so any single-operator flip breaks
    /// at least one row.
    #[test]
    fn manifest_chunk_boundary_arithmetic() {
        // (body_len, expected). body_len is what follows the version
        // byte. Ok(n) = parsed with n entries; Err variant per the
        // arithmetic path that rejects it.
        #[derive(Debug)]
        enum Expected {
            Ok(usize),
            BadLength,
            TooMany,
        }
        use Expected::*;

        let cases = [
            // Divisibility boundary: `% ENTRY_SIZE != 0`.
            // ENTRY_SIZE - 1 → BadLength (% catches non-multiple)
            (ENTRY_SIZE - 1, BadLength),
            // ENTRY_SIZE → 1 entry (% == 0, / == 1)
            (ENTRY_SIZE, Ok(1)),
            // ENTRY_SIZE + 1 → BadLength (catches `%` → `/` mutant:
            //   37/36=1, 37%36=1; if % became / the check is `1 != 0`
            //   still BadLength — but the `/` count becomes 1 not 0,
            //   so this row ALSO guards the count computation)
            (ENTRY_SIZE + 1, BadLength),
            // 2*ENTRY_SIZE → 2 entries (proves `/` not `%` in count:
            //   72/36=2, 72%36=0; if `/` became `%`, count=0, wrong)
            (2 * ENTRY_SIZE, Ok(2)),
            // 2*ENTRY_SIZE - 1 → BadLength (71%36=35, not 0)
            (2 * ENTRY_SIZE - 1, BadLength),
            // MAX_CHUNKS boundary: `count > MAX_CHUNKS`.
            // MAX_CHUNKS entries → Ok (at the limit, `>` not `>=`)
            (MAX_CHUNKS * ENTRY_SIZE, Ok(MAX_CHUNKS)),
            // MAX_CHUNKS + 1 → TooMany (1 past the limit; the `>`
            //   vs `>=` mutant is caught by the prior row, this one
            //   catches `>` → `<` or `==`)
            ((MAX_CHUNKS + 1) * ENTRY_SIZE, TooMany),
        ];

        for (body_len, expected) in cases {
            let mut data = vec![0u8; 1 + body_len];
            data[0] = VERSION;
            let got = Manifest::deserialize(&data);
            match expected {
                Ok(n) => {
                    let m = got.unwrap_or_else(|e| {
                        panic!("body_len={body_len} expected Ok({n}), got Err({e:?})")
                    });
                    assert_eq!(
                        m.entries.len(),
                        n,
                        "body_len={body_len} → expected {n} entries"
                    );
                    // Round-trip: serialize(deserialize(x)).len() ==
                    // 1 + n * ENTRY_SIZE (locks the count ↔ length
                    // bidirectional arithmetic).
                    assert_eq!(m.serialize().len(), 1 + n * ENTRY_SIZE);
                }
                BadLength => assert!(
                    matches!(got, Err(ManifestError::BadLength(l)) if l == body_len),
                    "body_len={body_len} expected BadLength, got {got:?}"
                ),
                TooMany => assert!(
                    matches!(got, Err(ManifestError::TooManyChunks(_))),
                    "body_len={body_len} expected TooManyChunks, got {got:?}"
                ),
            }
        }
    }

    #[test]
    fn deserialize_rejects_too_many_chunks() {
        // MAX_CHUNKS + 1 entries. Don't actually allocate 7.2 MB for the
        // test — construct the minimum viable over-limit manifest.
        // Actually, (MAX_CHUNKS + 1) * 36 ≈ 7.2 MB — that IS big but fine
        // for a test; it's transient and doesn't hit PG or S3.
        let over_limit = vec![0u8; 1 + (MAX_CHUNKS + 1) * ENTRY_SIZE];
        let mut data = over_limit;
        data[0] = VERSION;
        assert!(matches!(
            Manifest::deserialize(&data),
            Err(ManifestError::TooManyChunks(n)) if n == MAX_CHUNKS + 1
        ));
    }

    #[test]
    fn total_size_sums_u64() {
        // Two chunks that together exceed u32::MAX. Validates the sum
        // doesn't wrap.
        let m = Manifest {
            entries: vec![
                ManifestEntry {
                    hash: [0; 32],
                    size: u32::MAX,
                },
                ManifestEntry {
                    hash: [0; 32],
                    size: 100,
                },
            ],
        };
        assert_eq!(m.total_size(), u32::MAX as u64 + 100);
    }

    // -----------------------------------------------------------------------
    // Proptest: roundtrip for arbitrary manifests
    // -----------------------------------------------------------------------

    // Strategy for a single entry. Hash is any 32 bytes; size is any u32
    // (we don't clamp to CHUNK_MAX here — the manifest format doesn't
    // enforce that, it's the chunker's job).
    fn arb_entry() -> impl Strategy<Value = ManifestEntry> {
        (any::<[u8; 32]>(), any::<u32>()).prop_map(|(hash, size)| ManifestEntry { hash, size })
    }

    proptest! {
        /// For ANY valid manifest, serialize → deserialize = identity.
        /// 0..=1000 entries: enough to catch edge cases, small enough to
        /// be fast (~36 KB max manifest per iteration).
        #[test]
        fn prop_roundtrip(entries in prop::collection::vec(arb_entry(), 0..=1000)) {
            let m = Manifest { entries };
            let bytes = m.serialize();
            let parsed = Manifest::deserialize(&bytes).unwrap();
            prop_assert_eq!(parsed, m);
        }

        /// Serialize length is ALWAYS 1 + N*36. No variable-width encoding,
        /// no padding. If this fails, seeking in a manifest is broken.
        #[test]
        fn prop_serialize_length(entries in prop::collection::vec(arb_entry(), 0..=1000)) {
            let m = Manifest { entries };
            let bytes = m.serialize();
            prop_assert_eq!(bytes.len(), 1 + m.entries.len() * ENTRY_SIZE);
        }

        /// Deserialize of arbitrary bytes either fails cleanly or roundtrips.
        /// No panics, no UB. This is the "fuzz-lite" property: garbage in
        /// should produce Err, not crash.
        #[test]
        fn prop_no_panic_on_garbage(data in prop::collection::vec(any::<u8>(), 0..=4096)) {
            // Just exercise the parser. If it returns Ok, verify roundtrip;
            // if Err, that's fine (most garbage should error).
            if let Ok(m) = Manifest::deserialize(&data) {
                let reser = m.serialize();
                prop_assert_eq!(reser, data);
            }
        }
    }
}
