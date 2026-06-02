#![no_main]

//! Fuzz the read-path chunk walk ([`rio_log_kernel::visit_chunk`]) and
//! the completeness fold ([`manifest_covers_contiguously`]) over
//! ARBITRARY-length manifests — beyond the kani pair/triple dedup
//! harnesses (bounded to 2-3 chunks) and the ≤3-chunk soundness-only
//! covers proof. Three phases over one decoded manifest:
//!
//! 1. **Walk**: fold `visit_chunk` over the sorted chunks collecting
//!    the non-empty `[yield_from, yield_until)` intervals; the oracle
//!    is pure interval arithmetic (no per-line materialization — `n`
//!    may be ~i64::MAX): yields are pairwise disjoint and strictly
//!    increasing, their merge equals merge(chunk intervals) ∩
//!    `[since, ∞)` (`servedSpanExact` at arbitrary N), and the final
//!    cursor is the end of the last non-empty yield (else `since`).
//! 2. **Covers fold, full equivalence**: `manifest_covers_contiguously`
//!    agrees with an independent interval formulation — the merged
//!    span containing 0 reaches `up_to` (with the fold's documented
//!    `up_to == 0` edge: a non-empty manifest must still START at 0,
//!    because the gap check precedes the early exit). The kani harness
//!    proves only the soundness direction at ≤3 chunks; this checks
//!    both directions at arbitrary N.
//! 3. **Cut → covers composition** (dense-domain subset, bounded
//!    materialization): build the sorted accepted-line set, repeatedly
//!    cut it with [`contiguous_prefix_len`], assert each run is
//!    maximal-contiguous, then `covers(cut_runs, hw) ⟺ [0, hw) ⊆ set`
//!    for `hw > 0` — the write→seal pipeline composed in pure space.
//!
//! Sibling targets: `log_accept_kernel` (the accept gate composed
//! across a session) and `log_chunk_ingest` (the end-to-end store-path
//! oracle).
//!
//! # Input wire format
//!
//! `[since_sel, up_to_lo, up_to_hi]` then 4-byte chunk ops
//! `[sel, a, b, c]` (≤ [`MAX_CHUNKS`]): `first` from `sel % 4` —
//! dense `u16(a,b) % 600` (twice as likely), raw `u16(a,b)`, or the
//! i64 edge `i64::MAX - b`; `n = c % 17`, clamped so `first + n` keeps
//! the BIGINT precondition (mirroring `read_manifest_range`'s
//! `u64::try_from`). Chunks are then sorted by `first_line` — the
//! kernel's documented validity order, generated as a precondition,
//! not replayed from SQL.

use libfuzzer_sys::fuzz_target;
use rio_log_kernel::{contiguous_prefix_len, manifest_covers_contiguously, visit_chunk};

/// Manifest-length cap: large enough for shapes no bounded proof
/// reaches, small enough that the interval oracles stay trivial work.
const MAX_CHUNKS: usize = 64;

/// Merge already-sorted (by start) half-open intervals; drops empties.
fn merge_sorted(intervals: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(a, b) in intervals {
        if a >= b {
            continue;
        }
        match out.last_mut() {
            Some((_, end)) if a <= *end => *end = (*end).max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    // Comparable to the dense line numbers (0..=765) so the cursor can
    // land before, inside, or past the log — the e2e target's scheme.
    let since = data[0] as u64 * 3;
    let up_to = u16::from_le_bytes([data[1], data[2]]) as i64;

    // -- Decode + precondition: sorted, BIGINT-clamped chunks.
    let mut chunks: Vec<(u64, u64)> = Vec::new();
    for op in data[3..].chunks_exact(4).take(MAX_CHUNKS) {
        let (sel, a, b, c) = (op[0], op[1], op[2], op[3]);
        let raw = u16::from_le_bytes([a, b]) as u64;
        let first = match sel % 4 {
            0 | 1 => raw % 600,
            2 => raw,
            _ => i64::MAX as u64 - b as u64,
        };
        let n = ((c % 17) as u64).min((i64::MAX as u64) - first);
        chunks.push((first, n));
    }
    chunks.sort_by_key(|&(f, _)| f);

    // -- Phase 1: the walk, against the interval oracle.
    let mut cursor = since;
    let mut yields: Vec<(u64, u64)> = Vec::new();
    for &(f, n) in &chunks {
        let v = visit_chunk(cursor, f, n);
        if !v.is_empty() {
            yields.push((v.yield_from, v.yield_until));
        }
        cursor = v.next_line;
    }
    // (i) pairwise disjoint, strictly increasing.
    for w in yields.windows(2) {
        assert!(
            w[0].1 <= w[1].0,
            "yields overlap or regress: {:?} then {:?}",
            w[0],
            w[1]
        );
    }
    // (ii) merged yields == merged chunk ranges ∩ [since, ∞).
    let chunk_ivs: Vec<(u64, u64)> = chunks
        .iter()
        .filter(|&&(_, n)| n > 0)
        .map(|&(f, n)| (f, f + n)) // exact under the BIGINT clamp
        .collect();
    let expected: Vec<(u64, u64)> = merge_sorted(&chunk_ivs)
        .into_iter()
        .filter_map(|(a, b)| {
            let a = a.max(since);
            (a < b).then_some((a, b))
        })
        .collect();
    assert_eq!(
        merge_sorted(&yields),
        expected,
        "servedSpanExact violated at N={} (since={since})",
        chunks.len()
    );
    // (iii) the resume watermark.
    assert_eq!(
        cursor,
        yields.last().map_or(since, |&(_, until)| until),
        "cursor did not land one past the last served line"
    );

    // -- Phase 2: covers fold ⟺ the merged span containing 0 reaches
    // up_to. The span containing 0 exists iff the first merged
    // interval starts at 0; its end is the contiguous-from-0 extent.
    // The up_to == 0 edge transcribes the fold's order (gap check
    // before early exit): a non-empty manifest must start at 0.
    let chunks_i64: Vec<(i64, i64)> = chunks.iter().map(|&(f, n)| (f as i64, n as i64)).collect();
    let covers = manifest_covers_contiguously(&chunks_i64, up_to);
    let merged_all = merge_sorted(&chunk_ivs);
    let span0_end: u64 = match merged_all.first() {
        Some(&(0, end)) => end,
        _ => 0,
    };
    let model_covers = if chunks.is_empty() {
        up_to <= 0
    } else {
        (up_to <= span0_end as i64) && (up_to > 0 || chunks[0].0 == 0)
    };
    assert_eq!(
        covers,
        model_covers,
        "covers-fold equivalence violated: up_to={up_to} span0_end={span0_end} N={}",
        chunks.len()
    );

    // -- Phase 3: cut → covers composition, dense subset only (bounded
    // materialization: dense lines are < 600 + 16).
    let mut lines: Vec<u64> = chunks
        .iter()
        .filter(|&&(f, n)| n > 0 && f + n <= 700)
        .flat_map(|&(f, n)| f..f + n)
        .collect();
    lines.sort_unstable();
    lines.dedup();
    let mut runs: Vec<(i64, i64)> = Vec::new();
    let mut rest: &[u64] = &lines;
    while !rest.is_empty() {
        let len = contiguous_prefix_len(rest.iter().copied());
        assert!(
            len >= 1 && len <= rest.len(),
            "prefix length out of range: {len} of {}",
            rest.len()
        );
        for w in rest[..len].windows(2) {
            assert_eq!(w[1], w[0] + 1, "run is not contiguous");
        }
        if len < rest.len() {
            assert_ne!(
                rest[len],
                rest[len - 1] + 1,
                "run is not maximal: the cutter stopped mid-run"
            );
        }
        runs.push((rest[0] as i64, len as i64));
        rest = &rest[len..];
    }
    if let Some(&hw) = lines.last() {
        let hw = hw + 1; // one past the highest accepted line; > 0
        let set_covers = lines.len() as u64 == hw && lines[0] == 0;
        assert_eq!(
            manifest_covers_contiguously(&runs, hw as i64),
            set_covers,
            "cut→covers composition violated at hw={hw}"
        );
    }
});
