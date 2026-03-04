//! Self-describing bloom filter for worker-locality heartbeats.
//!
//! Workers send this in `HeartbeatRequest.local_paths` to tell the
//! scheduler "here's approximately which store paths I have cached".
//! The scheduler uses it for transfer-cost scoring: a worker that
//! already has most of a derivation's inputs is a better candidate.
//!
//! # Why blake3 (user decision from planning)
//!
//! The bloom filter is a WIRE PROTOCOL: worker builds it, scheduler
//! queries it. Both sides must compute the same bit indices for the
//! same input, or every query is a false negative (scoring becomes
//! noise). This rules out platform-dependent hashes:
//!
//! - gxhash: AES-NI based, different output on ARM vs x86
//! - xxhash: portable but we'd need a separate dep
//!
//! blake3 is portable (deterministic everywhere) AND already a dep
//! (chunk addressing in rio-store). Single 256-bit hash call, split
//! into two u64s for Kirsch-Mitzenmacher double-hashing — no seeded
//! re-hash needed. Cryptographic strength is overkill for a bloom
//! filter, but it's free.
//!
//! # Kirsch-Mitzenmacher
//!
//! k hash functions from two: `index_i = (h1 + i * h2) % m`. The
//! original paper (2006) proves this has the same asymptotic FPR as
//! k independent hashes. In practice it's slightly worse for very
//! small filters but fine at our scale (thousands of items).

/// A bloom filter. Insert strings, query approximate membership.
///
/// False positives possible (says "maybe present" when absent).
/// False negatives impossible (if we inserted it, query says yes).
///
/// NOT resizable after construction — the bit array size is fixed
/// by `new(expected_items, target_fpr)`. If you need more capacity,
/// build a fresh filter.
///
/// `Debug` is custom — printing a 60KB bit array in `{:?}` output
/// would be useless. Shows size + hash count instead.
#[derive(Clone)]
pub struct BloomFilter {
    /// The bit array. Stored as bytes (8 bits per byte); bit i is
    /// `bits[i / 8] & (1 << (i % 8))`.
    bits: Vec<u8>,
    /// Total addressable bits. Usually `bits.len() * 8` but can be
    /// less (the last byte might have unused high bits). We track
    /// this separately so `% num_bits` doesn't accidentally address
    /// those unused bits.
    num_bits: u32,
    /// Number of hash functions (bit positions set per insert).
    /// Clamped to [1, 16] — more than 16 has diminishing returns
    /// and burns CPU; less than 1 is meaningless.
    hash_count: u32,
}

/// Format version. Bumped if the index computation changes (which
/// would invalidate all existing filters on the wire).
const FORMAT_VERSION: u32 = 1;

impl std::fmt::Debug for BloomFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't dump 60KB of bits. Summary is what you actually want
        // in tracing output.
        f.debug_struct("BloomFilter")
            .field("num_bits", &self.num_bits)
            .field("hash_count", &self.hash_count)
            .field("byte_len", &self.bits.len())
            .finish()
    }
}

/// Hard cap on k. Beyond ~16, each extra hash function burns CPU for
/// negligible FPR improvement (the optimal k for 1% FPR is ~7).
const MAX_HASH_COUNT: u32 = 16;

#[derive(Debug, thiserror::Error)]
pub enum BloomError {
    #[error("unsupported hash algorithm: {0} (only BLAKE3_256 supported)")]
    UnsupportedAlgorithm(i32),

    #[error("unsupported format version: {0} (expected {FORMAT_VERSION})")]
    UnsupportedVersion(u32),

    #[error("hash_count {0} out of range [1, {MAX_HASH_COUNT}]")]
    HashCountRange(u32),

    #[error("data too short for num_bits: have {have} bytes, need at least {need}")]
    DataTooShort { have: usize, need: usize },

    #[error("num_bits is zero (empty filter is meaningless)")]
    ZeroBits,
}

impl BloomFilter {
    /// Create a filter sized for `expected_items` at `target_fpr`.
    ///
    /// Standard bloom-filter math:
    /// - `m = -(n * ln(p)) / (ln 2)²` bits
    /// - `k = (m / n) * ln 2` hash functions
    ///
    /// Both rounded: m up (more bits = better FPR), k to nearest (the
    /// FPR curve is shallow near the optimum, ±1 is fine).
    ///
    /// `expected_items = 0` is silently treated as 1 — a zero-capacity
    /// filter is meaningless, and crashing on 0 would be annoying for
    /// callers that build from an empty inventory at startup.
    pub fn new(expected_items: usize, target_fpr: f64) -> Self {
        // Clamp inputs to sane ranges. FPR outside (0, 1) is nonsense;
        // we clamp rather than panic because the caller might compute
        // it from configuration and we'd rather degrade than crash.
        let n = expected_items.max(1) as f64;
        let p = target_fpr.clamp(1e-9, 0.5);

        let ln2 = std::f64::consts::LN_2;
        let m = (-(n * p.ln()) / (ln2 * ln2)).ceil() as u32;
        // m can't be zero after the clamp (n ≥ 1, p ≤ 0.5 → -ln(p) > 0).
        // But be defensive: a zero-bit filter would divide by zero in
        // indices().
        let m = m.max(8);

        let k = ((m as f64 / n) * ln2).round() as u32;
        let k = k.clamp(1, MAX_HASH_COUNT);

        // Round bit count up to a whole number of bytes. We still
        // track num_bits separately — the unused high bits of the
        // last byte should never be addressed.
        let byte_len = m.div_ceil(8) as usize;

        Self {
            bits: vec![0u8; byte_len],
            num_bits: m,
            hash_count: k,
        }
    }

    /// Insert an item. Sets k bits.
    ///
    /// Takes `&str` because our items are store paths (strings).
    /// `AsRef<[u8]>` would be more general but we don't need it —
    /// YAGNI, and `&str` makes call sites cleaner.
    pub fn insert(&mut self, item: &str) {
        // Free function (not &self method) so the iterator doesn't
        // borrow self — we need &mut self.bits inside the loop.
        // Copying the two u32 fields out is free.
        for idx in indices(item, self.num_bits, self.hash_count) {
            let byte = (idx / 8) as usize;
            let bit = (idx % 8) as u8;
            self.bits[byte] |= 1 << bit;
        }
    }

    /// Query membership. `true` = maybe present (could be a false
    /// positive). `false` = definitely absent.
    ///
    /// Named `maybe_contains` not `contains` because the "maybe" is
    /// the whole point of a bloom filter. A caller treating this as
    /// exact containment would be a bug; the name makes that harder
    /// to do by accident.
    pub fn maybe_contains(&self, item: &str) -> bool {
        indices(item, self.num_bits, self.hash_count).all(|idx| {
            let byte = (idx / 8) as usize;
            let bit = (idx % 8) as u8;
            self.bits[byte] & (1 << bit) != 0
        })
    }
}

/// Compute k bit indices for an item. Free function so `insert()` can
/// iterate while mutating `self.bits` (a `&self` method would conflict).
///
/// Kirsch-Mitzenmacher: `idx_i = (h1 + i * h2) % m`. One blake3 call
/// gives 256 bits; we take the first 16 bytes as two u64s. The
/// remaining 16 bytes are wasted — fine, blake3 is fast and we only
/// need 128 bits of entropy for k≤16 hashes.
fn indices(item: &str, num_bits: u32, hash_count: u32) -> impl Iterator<Item = u32> {
    let hash = blake3::hash(item.as_bytes());
    let bytes = hash.as_bytes();

    // First 8 bytes → h1, next 8 → h2. LE is an arbitrary choice
    // but MUST match between worker (insert) and scheduler (query).
    // If this ever changes, bump FORMAT_VERSION.
    let h1 = u64::from_le_bytes(bytes[0..8].try_into().expect("blake3 is 32 bytes"));
    let h2 = u64::from_le_bytes(bytes[8..16].try_into().expect("blake3 is 32 bytes"));

    let m = num_bits as u64;
    (0..hash_count).map(move |i| {
        // Cast to u32 AFTER the mod: `% m` where m is u32-range
        // guarantees the result fits. Casting before would truncate
        // h1+i*h2 to 32 bits first, losing entropy.
        //
        // `wrapping_*`: the mod handles overflow correctness; wrapping
        // just silences debug-assert-on-overflow. For large i*h2, the
        // wrap is expected and benign.
        (h1.wrapping_add((i as u64).wrapping_mul(h2)) % m) as u32
    })
}

impl BloomFilter {
    /// Number of bits in the filter (for sizing metrics).
    pub fn num_bits(&self) -> u32 {
        self.num_bits
    }

    /// Number of hash functions.
    pub fn hash_count(&self) -> u32 {
        self.hash_count
    }

    /// Serialized byte length (for heartbeat size budgeting).
    /// This is just the bit array; the proto wrapper adds a few
    /// more bytes for the metadata fields.
    pub fn byte_len(&self) -> usize {
        self.bits.len()
    }

    // ------------------------------------------------------------------------
    // Wire protocol conversion
    // ------------------------------------------------------------------------

    /// Serialize to the proto `BloomFilter` message.
    ///
    /// The proto is self-describing (algorithm, version, hash_count,
    /// num_bits all included) so the scheduler can validate before
    /// querying. A mismatch in any of those would produce garbage
    /// results; better to reject upfront.
    ///
    /// Returns the wire struct directly (not a `Vec<u8>` — tonic
    /// handles the final encoding). Tests exercise the roundtrip.
    ///
    /// `rio-common` doesn't depend on `rio-proto` (would be a cycle:
    /// proto → common for limits). So this returns a tuple of the
    /// fields; caller constructs the proto struct. Ugly but correct.
    pub fn to_wire(&self) -> (Vec<u8>, u32, u32, u32) {
        (
            self.bits.clone(),
            self.hash_count,
            self.num_bits,
            FORMAT_VERSION,
        )
    }

    /// Parse from proto `BloomFilter` fields.
    ///
    /// Validates:
    /// - algorithm is BLAKE3_256 (value 3 per the proto enum)
    /// - version is FORMAT_VERSION
    /// - hash_count in [1, 16]
    /// - data is large enough for num_bits
    ///
    /// `hash_algorithm` is the raw i32 from the proto (enums are i32
    /// on the wire). We check against the known value directly to
    /// avoid the proto dep.
    pub fn from_wire(
        data: Vec<u8>,
        hash_count: u32,
        num_bits: u32,
        hash_algorithm: i32,
        version: u32,
    ) -> Result<Self, BloomError> {
        // 3 = BLOOM_HASH_ALGORITHM_BLAKE3_256. Hardcoded because we
        // can't depend on rio-proto. A const with a comment is the
        // next best thing to the actual enum.
        const BLAKE3_256: i32 = 3;
        if hash_algorithm != BLAKE3_256 {
            return Err(BloomError::UnsupportedAlgorithm(hash_algorithm));
        }

        if version != FORMAT_VERSION {
            return Err(BloomError::UnsupportedVersion(version));
        }

        if hash_count == 0 || hash_count > MAX_HASH_COUNT {
            return Err(BloomError::HashCountRange(hash_count));
        }

        if num_bits == 0 {
            return Err(BloomError::ZeroBits);
        }

        // data.len() * 8 must be ≥ num_bits. < means queries would
        // index out of bounds. > is fine (unused high bits in the
        // last byte).
        let needed_bytes = (num_bits as usize).div_ceil(8);
        if data.len() < needed_bytes {
            return Err(BloomError::DataTooShort {
                have: data.len(),
                need: needed_bytes,
            });
        }

        Ok(Self {
            bits: data,
            num_bits,
            hash_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn zero_false_negatives() {
        // THE bloom filter invariant: if you inserted it, query
        // returns true. No exceptions. This is the only hard guarantee.
        let mut f = BloomFilter::new(100, 0.01);
        let items = [
            "/nix/store/aaa-foo",
            "/nix/store/bbb-bar",
            "path-with-特殊-chars",
        ];
        for item in &items {
            f.insert(item);
        }
        for item in &items {
            assert!(
                f.maybe_contains(item),
                "inserted item must be found: {item}"
            );
        }
    }

    #[test]
    fn empty_filter_contains_nothing() {
        let f = BloomFilter::new(100, 0.01);
        assert!(!f.maybe_contains("anything"));
        assert!(!f.maybe_contains(""));
    }

    #[test]
    fn sizing_math_ballpark() {
        // For n=1000, p=0.01: m ≈ 9585, k ≈ 7. These are the textbook
        // values — if they're wildly off, the math is wrong.
        let f = BloomFilter::new(1000, 0.01);
        assert!(
            (9000..10500).contains(&f.num_bits()),
            "num_bits {} should be ~9585",
            f.num_bits()
        );
        assert!(
            (6..=8).contains(&f.hash_count()),
            "hash_count {} should be ~7",
            f.hash_count()
        );
    }

    #[test]
    fn sizing_clamps() {
        // Zero expected items → treated as 1. Shouldn't crash.
        let f = BloomFilter::new(0, 0.01);
        assert!(f.num_bits() >= 8);
        assert!(f.hash_count() >= 1);

        // Absurd FPR → clamped. Shouldn't produce inf/NaN.
        let f = BloomFilter::new(100, 0.0);
        assert!(f.num_bits() > 0);
        let f = BloomFilter::new(100, 2.0);
        assert!(f.num_bits() > 0);
    }

    #[test]
    fn hash_count_clamped_to_max() {
        // Tiny FPR → huge k. Should clamp to 16.
        let f = BloomFilter::new(10, 1e-9);
        assert!(f.hash_count() <= MAX_HASH_COUNT);
    }

    #[test]
    fn deterministic_across_instances() {
        // Same (n, p, items) → same filter bits. If this fails,
        // something platform-dependent is leaking in (which is the
        // whole wire-protocol problem we're avoiding with blake3).
        let build = || {
            let mut f = BloomFilter::new(50, 0.05);
            f.insert("/nix/store/xxx-one");
            f.insert("/nix/store/yyy-two");
            f
        };
        let a = build();
        let b = build();
        assert_eq!(a.bits, b.bits);
    }

    // ------------------------------------------------------------------------
    // Wire protocol
    // ------------------------------------------------------------------------

    #[test]
    fn wire_roundtrip() {
        let mut f = BloomFilter::new(100, 0.01);
        f.insert("test-item");

        let (data, k, m, v) = f.to_wire();
        let parsed = BloomFilter::from_wire(data, k, m, 3, v).unwrap();

        // Same membership results after roundtrip.
        assert!(parsed.maybe_contains("test-item"));
        assert!(!parsed.maybe_contains("not-inserted"));
        assert_eq!(parsed.num_bits(), f.num_bits());
        assert_eq!(parsed.hash_count(), f.hash_count());
    }

    #[test]
    fn wire_rejects_wrong_algorithm() {
        // 99 = unknown variant. Only 3 (BLAKE3_256) is accepted.
        let result = BloomFilter::from_wire(vec![0; 10], 4, 64, 99, 1);
        assert!(matches!(result, Err(BloomError::UnsupportedAlgorithm(99))));
    }

    #[test]
    fn wire_rejects_wrong_version() {
        let result = BloomFilter::from_wire(vec![0; 10], 4, 64, 3, 999);
        assert!(matches!(result, Err(BloomError::UnsupportedVersion(999))));
    }

    #[test]
    fn wire_rejects_bad_hash_count() {
        let too_low = BloomFilter::from_wire(vec![0; 10], 0, 64, 3, 1);
        assert!(matches!(too_low, Err(BloomError::HashCountRange(0))));
        let too_high = BloomFilter::from_wire(vec![0; 10], 99, 64, 3, 1);
        assert!(matches!(too_high, Err(BloomError::HashCountRange(99))));
    }

    #[test]
    fn wire_rejects_short_data() {
        // num_bits=64 needs 8 bytes. Give it 5.
        let result = BloomFilter::from_wire(vec![0; 5], 4, 64, 3, 1);
        assert!(matches!(
            result,
            Err(BloomError::DataTooShort { have: 5, need: 8 })
        ));
    }

    #[test]
    fn wire_rejects_zero_bits() {
        let result = BloomFilter::from_wire(vec![0; 10], 4, 0, 3, 1);
        assert!(matches!(result, Err(BloomError::ZeroBits)));
    }

    // ------------------------------------------------------------------------
    // Proptest
    // ------------------------------------------------------------------------

    proptest! {
        /// Zero false negatives, always. The ONE hard guarantee.
        #[test]
        fn prop_no_false_negatives(
            items in prop::collection::vec("[a-z0-9/_-]{5,50}", 1..100)
        ) {
            let mut f = BloomFilter::new(items.len().max(10), 0.01);
            for item in &items {
                f.insert(item);
            }
            for item in &items {
                prop_assert!(
                    f.maybe_contains(item),
                    "inserted {item} must be found"
                );
            }
        }

        /// FPR should be within 2× of target. Not a hard guarantee
        /// (it's probabilistic) but if we're consistently >2× off,
        /// the sizing math or hashing is broken.
        ///
        /// 256 proptest cases × ~20 false-positive checks each is
        /// enough to catch gross errors without being flaky.
        #[test]
        fn prop_fpr_ballpark(seed: u64) {
            // Fixed-seed deterministic item generation so proptest's
            // shrinking is meaningful (same seed → same items).
            let n = 1000;
            let target_fpr = 0.01;
            let mut f = BloomFilter::new(n, target_fpr);

            // Insert n items derived from seed.
            for i in 0..n {
                f.insert(&format!("item-{seed}-{i}"));
            }

            // Query 10k items we DIDN'T insert. Count false positives.
            let trials = 10_000;
            let mut fp = 0;
            for i in 0..trials {
                if f.maybe_contains(&format!("absent-{seed}-{i}")) {
                    fp += 1;
                }
            }

            let actual_fpr = fp as f64 / trials as f64;
            // 2× tolerance: the theoretical FPR is asymptotic; small
            // filters are noisier. 3× would still be fine; 10× means bug.
            prop_assert!(
                actual_fpr < target_fpr * 2.0,
                "FPR {actual_fpr:.4} exceeds 2× target {target_fpr} (seed={seed}, fp={fp}/{trials})"
            );
        }

        /// Wire roundtrip preserves membership for any valid filter.
        #[test]
        fn prop_wire_roundtrip(
            items in prop::collection::vec("[a-z0-9/_-]{5,50}", 0..50)
        ) {
            let mut f = BloomFilter::new(items.len().max(10), 0.05);
            for item in &items {
                f.insert(item);
            }

            let (data, k, m, v) = f.to_wire();
            let parsed = BloomFilter::from_wire(data, k, m, 3, v).unwrap();

            for item in &items {
                prop_assert!(parsed.maybe_contains(item));
            }
        }
    }
}
