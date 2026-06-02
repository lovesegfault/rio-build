//! Pure decision kernels for the log chunk subsystem.
//!
//! Every function here is total, allocation-free, loop-free (except the
//! bounded manifest fold and the contiguous-prefix scan), and depends
//! on nothing but `core` — no SQL, no S3, no clocks, no `Vec<u8>`
//! payloads. The I/O-shaped callers (rio-store's `logs::tail::read_chunk`,
//! `logs::ingest::IngestSession::accept`, and `logs::gate`'s
//! completeness predicate) project their inputs into plain integers,
//! delegate the decision here, and apply the returned verdict. The
//! split mirrors `rio-lease`'s `decide()` / `decide_pure()` pair: the
//! kernel is the verifiable core, the caller is the I/O-shaped shim.
//!
//! This crate is dependency-free so the CBMC goto model for the proof
//! harnesses closes over it alone (the rio-retry-kernel template);
//! rio-store re-exports it as `rio_store::logs::kernel`, which keeps
//! every store-side import path unchanged.
//!
//! Each kernel parallels a `pure def` in the formal model
//! (`docs/spec/models/logService.qnt`): [`visit_chunk`] is `visitChunk`,
//! [`accept_verdict`] is `acceptVerdict`, [`manifest_covers_contiguously`]
//! is `manifestCoversUpTo`. When either side changes, update the other.
//!
//! The one numeric precondition shared by every kernel: line numbers and
//! line counts that reached a `drv_log_chunks` row round-tripped through
//! `BIGINT`, so they are at most [`i64::MAX`]. The ingest path enforces
//! it at accept time ([`accept_verdict`]'s overflow arm); the read path
//! re-establishes it at the SQL boundary (`read_manifest_range`'s
//! `u64::try_from`). Under that precondition none of the interval
//! arithmetic below can overflow `u64` — `i64::MAX + i64::MAX < u64::MAX`.

/// What one chunk contributes to a `TailLog` read, given the dedup
/// watermark at the time the chunk is visited.
///
/// The contributed line numbers are the half-open range
/// `[yield_from, yield_until)` — empty (`yield_from == yield_until`)
/// when the chunk is skipped. `next_line` is the post-visit watermark
/// the caller stores back into its `LineCursor` (rio-store's
/// `logs::tail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkVisit {
    /// First line number this chunk contributes:
    /// `max(first_line, next_line)` — the chunk's own start, or the
    /// watermark when an earlier overlapping chunk already served the
    /// chunk's leading lines.
    pub yield_from: u64,
    /// One past the last line this chunk contributes (`first_line +
    /// n_lines` for a visited chunk, `yield_from` for a skipped one).
    pub yield_until: u64,
    /// The post-visit watermark: one past the chunk's last line for a
    /// visited chunk, the input watermark unchanged for a skipped one.
    /// A skipped chunk MUST NOT advance the watermark — a zero-line
    /// chunk starting past the cursor would otherwise swallow every
    /// line between the cursor and its `first_line`.
    pub next_line: u64,
}

impl ChunkVisit {
    /// The chunk contributes nothing (zero lines, or every line is
    /// below the watermark). The caller skips the object GET entirely.
    pub fn is_empty(&self) -> bool {
        self.yield_from == self.yield_until
    }
}

// r[impl store.log.session-keyed]
/// One step of the read path's overlap-dedup walk
/// (`docs/spec/models/logService.qnt::visitChunk`): which of a chunk's
/// `n_lines` lines starting at `first_line` lie at or above the
/// watermark `next_line`, and where the watermark lands afterwards.
///
/// Valid only for chunks visited in ascending `first_line` order (the
/// `read_manifest_range` `ORDER BY`): under that order the set of
/// already-yielded line numbers is always a prefix of the line domain
/// (minus genuine storage gaps), so "skip lines already yielded"
/// reduces to "skip lines below the watermark".
///
/// The arithmetic saturates so the function is total over all of `u64`;
/// under the `BIGINT` precondition (`first_line` and `n_lines` at most
/// [`i64::MAX`]) the saturation never engages and every quantity below
/// is exact.
//
// ── Kani contracts ───────────────────────────────────────────────────
// The requires clause is the manifest BIGINT round-trip invariant: both
// columns came out of `read_manifest_range`'s `u64::try_from(i64)` (or,
// for the post-decompress call, out of `decompress_lines`'s 16 MiB
// bound). `next_line` is unconstrained — the cursor seeds from the
// client-supplied `since_line`, which is any u64. Under the requires,
// `first_line + n_lines` fits in u64 (i64::MAX + i64::MAX < u64::MAX),
// so the ensures closures may compute it exactly; CBMC additionally
// rejects any overflow inside the closures themselves, so the contract
// doubles as the no-overflow proof for the range arithmetic.
//
// The case split mirrors `logService.qnt::visitChunk` exactly: a chunk
// is skipped iff it has no lines at or above the watermark; a visited
// chunk contributes `[max(first, cursor), first + count)` and lands the
// watermark one past its last line. Verified by
// `check_visit_chunk_contract` in `#[cfg(kani)] mod proofs`; the
// fold-level dedup properties (each line at most once, served set ==
// union) are the `check_dedup_{pair,triple}_*` harnesses.
#[cfg_attr(
    kani,
    kani::requires(first_line <= i64::MAX as u64 && n_lines <= i64::MAX as u64)
)]
#[cfg_attr(kani, kani::ensures(|r: &ChunkVisit| {
    // The contribution is exactly the chunk's range above the
    // watermark: empty when the chunk has nothing at or above it,
    // `[max(first_line, next_line), first_line + n_lines)` otherwise.
    let end = first_line + n_lines;
    if n_lines == 0 || end <= next_line {
        r.yield_from == r.yield_until
    } else {
        r.yield_from == if first_line > next_line { first_line } else { next_line }
            && r.yield_until == end
    }
}))]
#[cfg_attr(kani, kani::ensures(|r: &ChunkVisit| {
    // Dedup safety: no yielded line is below the watermark (a line
    // already served by an earlier chunk is never served again), the
    // yielded range is well-formed, and every yielded line is one the
    // chunk actually holds.
    r.yield_from >= next_line
        && r.yield_from <= r.yield_until
        && (r.yield_from == r.yield_until
            || (r.yield_from >= first_line && r.yield_until <= first_line + n_lines))
}))]
#[cfg_attr(kani, kani::ensures(|r: &ChunkVisit| {
    // The watermark is monotone and lands one past the chunk's last
    // line iff the chunk contributed anything; a skipped chunk leaves
    // it untouched.
    let end = first_line + n_lines;
    r.next_line
        == if n_lines == 0 || end <= next_line {
            next_line
        } else {
            end
        }
}))]
pub fn visit_chunk(next_line: u64, first_line: u64, n_lines: u64) -> ChunkVisit {
    // One past the chunk's last line, and the last line itself. The
    // `n_lines == 0` guard below keeps the `end - 1` underflow
    // unreachable (`saturating_sub` makes it total anyway).
    let end = first_line.saturating_add(n_lines);
    let last_line = end.saturating_sub(1);
    // A degenerate zero-line chunk has nothing to contribute, and a
    // chunk whose last line is below the watermark is a same-range
    // chunk from an earlier-visited overlapping session whose lines
    // were all already yielded. Both leave the watermark untouched.
    if n_lines == 0 || last_line < next_line {
        return ChunkVisit {
            yield_from: next_line,
            yield_until: next_line,
            next_line,
        };
    }
    ChunkVisit {
        yield_from: first_line.max(next_line),
        yield_until: end,
        next_line: end,
    }
}

/// The verdict of [`accept_verdict`] for one non-empty `AppendLog`
/// batch. The three `Rejected*` variants map one-to-one onto the
/// rejection variants of rio-store's `logs::ingest::AcceptOutcome`;
/// `Accepted` carries the post-truncation exclusive end the caller
/// needs to truncate the batch and advance its high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptVerdict {
    /// `first_line_number + line_count` overflows `u64` or exceeds
    /// `i64::MAX` — the batch's line numbers could not round-trip
    /// through the manifest's `BIGINT` columns.
    RejectedOverflow,
    /// The batch starts below the session's high-water mark.
    RejectedNonMonotone,
    /// Every line in the batch is at or past the recorded
    /// `final_line_count` ceiling.
    RejectedPastFinal,
    /// The batch is accepted. `end` is one past the last accepted line
    /// after clamping to the ceiling: the caller keeps the first
    /// `end - first_line_number` lines and raises its high-water mark
    /// to `end`. `end < first_line_number + line_count` iff the batch
    /// straddled the ceiling and was truncated.
    Accepted {
        /// One past the last accepted line (the new high-water mark).
        end: u64,
    },
}

// r[impl store.log.ingest-bounds]
// r[impl store.log.completeness-gate]
/// The accept/reject verdict for one non-empty batch of `line_count`
/// lines starting at `first_line_number`, against a session whose
/// high-water mark is `high_water_line` and whose completeness ceiling
/// (the execution's recorded `final_line_count`, once known) is
/// `final_line_count`.
///
/// The check order is the code's (and the model's
/// `docs/spec/models/logService.qnt::acceptVerdict`, which omits the
/// overflow arm as unrepresentable in its bounded line domain):
///
/// 1. overflow — the line numbers must round-trip through `BIGINT`;
/// 2. monotone floor — the batch must start at or above the high-water
///    mark (forward gaps are legal, going backwards is not);
/// 3. completeness ceiling — a batch starting at or past the recorded
///    end is dropped whole; one that straddles it is truncated to
///    `[first_line_number, ceiling)`.
///
/// The caller handles the empty batch before calling this (nothing to
/// buffer, nothing to check); the kernel is nevertheless total over
/// `line_count == 0`.
//
// ── Kani contracts ───────────────────────────────────────────────────
// One ensures clause per verdict variant, each an iff: the verdict is X
// exactly when X's condition holds and no earlier check fired. Together
// the four clauses prove the accept verdict rejects exactly the inputs
// the model's accept predicate rejects — the conditions below transcribe
// `docs/spec/models/logService.qnt::acceptVerdict`:
//
//     pure def acceptVerdict(hw, ceiling, lo, hi) =
//       if (lo < hw) RejectNonMonotone
//       else match ceiling {
//         | NoCeiling => Accept(hi)
//         | Ceiling(c) =>
//           if (lo >= c.value) RejectPastFinal
//           else Accept(minInt(hi, c.value))
//       }
//
// (`hi` is the batch's exclusive end `lo + count`.) The overflow arm is
// the one addition: the model's bounded line domain cannot represent an
// unrepresentable batch, so its predicate is conditioned on
// representability — clauses 2-4 carry the same `end <= i64::MAX`
// conjunct. There is no requires clause: the kernel is total and the
// worker-supplied inputs are untrusted, so the contract must hold over
// the full u64 domain. Verified by `check_accept_verdict_contract` in
// `#[cfg(kani)] mod proofs`.
#[cfg_attr(kani, kani::ensures(|r: &AcceptVerdict| {
    // RejectedOverflow iff the batch's exclusive end cannot round-trip
    // through the manifest's BIGINT columns. (The model omits this arm:
    // "the overflow arm is unrepresentable in the bounded line domain".)
    matches!(r, AcceptVerdict::RejectedOverflow)
        == match first_line_number.checked_add(line_count) {
            None => true,
            Some(end) => end > i64::MAX as u64,
        }
}))]
#[cfg_attr(kani, kani::ensures(|r: &AcceptVerdict| {
    // RejectedNonMonotone iff representable AND below the high-water
    // mark (`if (lo < hw) RejectNonMonotone`).
    matches!(r, AcceptVerdict::RejectedNonMonotone)
        == (first_line_number
            .checked_add(line_count)
            .is_some_and(|end| end <= i64::MAX as u64)
            && first_line_number < high_water_line)
}))]
#[cfg_attr(kani, kani::ensures(|r: &AcceptVerdict| {
    // RejectedPastFinal iff representable, monotone, AND the batch
    // starts at or past the known ceiling
    // (`if (lo >= c.value) RejectPastFinal`).
    matches!(r, AcceptVerdict::RejectedPastFinal)
        == (first_line_number
            .checked_add(line_count)
            .is_some_and(|end| end <= i64::MAX as u64)
            && first_line_number >= high_water_line
            && final_line_count.is_some_and(|c| first_line_number >= c))
}))]
#[cfg_attr(kani, kani::ensures(|r: &AcceptVerdict| {
    // Accepted iff no rejection fires (implied by the three iffs above
    // over the four-variant enum); the accepted end is the batch's end
    // clamped to the ceiling (`Accept(minInt(hi, c.value))` /
    // `Accept(hi)`), so no accepted line is ever at or past the
    // recorded final_line_count (store.log.completeness-gate) and the
    // kept-line count never exceeds the batch's.
    match r {
        AcceptVerdict::Accepted { end } => first_line_number
            .checked_add(line_count)
            .is_some_and(|batch_end| {
                batch_end <= i64::MAX as u64
                    && first_line_number >= high_water_line
                    && final_line_count.is_none_or(|c| first_line_number < c)
                    && *end
                        == match final_line_count {
                            Some(c) if c < batch_end => c,
                            _ => batch_end,
                        }
                    && *end >= first_line_number
                    && *end <= batch_end
            }),
        _ => true,
    }
}))]
pub fn accept_verdict(
    high_water_line: u64,
    final_line_count: Option<u64>,
    first_line_number: u64,
    line_count: u64,
) -> AcceptVerdict {
    // -- Check 1: the batch's end must be representable. Everything
    // downstream (the manifest's [first_line, first_line + line_count)
    // arithmetic, the read path's attribution, the completeness fold)
    // relies on line numbers fitting in BIGINT.
    let end = match first_line_number.checked_add(line_count) {
        Some(end) if end <= i64::MAX as u64 => end,
        _ => return AcceptVerdict::RejectedOverflow,
    };
    // -- Check 2: the monotone floor. A batch starting at or below a
    // line number the session already holds or has cut is malformed
    // worker input; renumbering it would corrupt every downstream
    // consumer of the ordering, so it is dropped whole.
    if first_line_number < high_water_line {
        return AcceptVerdict::RejectedNonMonotone;
    }
    // -- Check 3: the completeness ceiling. Once known, the recorded
    // final_line_count is one past the last line the build produced:
    // lines numbered at or past it cannot be part of the log.
    match final_line_count {
        Some(ceiling) if first_line_number >= ceiling => AcceptVerdict::RejectedPastFinal,
        Some(ceiling) => AcceptVerdict::Accepted {
            end: end.min(ceiling),
        },
        None => AcceptVerdict::Accepted { end },
    }
}

// r[impl store.log.completeness-gate]
/// Does an `ORDER BY first_line` manifest cover a contiguous
/// `[0, up_to)` with no gaps?
///
/// Chunks may overlap (two ingest sessions for one execution after a
/// replica failover) — overlap extends coverage, it never breaks it. A
/// chunk starting *past* the covered-through point is a gap and ends
/// the fold early. Parallels
/// `docs/spec/models/logService.qnt::manifestCoversUpTo`.
///
/// The `(first_line, line_count)` pairs are the raw `BIGINT` column
/// values; for rows the ingest path wrote both are non-negative and
/// their sum fits in `i64`, so the `saturating_add` never saturates.
pub fn manifest_covers_contiguously(chunks: &[(i64, i64)], up_to: i64) -> bool {
    let mut covered = 0i64;
    for &(first_line, line_count) in chunks {
        if first_line > covered {
            // A gap: nothing covers [covered, first_line).
            return false;
        }
        covered = covered.max(first_line.saturating_add(line_count));
        if covered >= up_to {
            return true;
        }
    }
    covered >= up_to
}

/// Length of the longest prefix of `line_numbers` that is consecutive
/// (each number is its predecessor plus one). Zero for an empty
/// iterator — total over every input, unlike the slice predecessor in
/// rio-store's cutter, which documented a non-empty precondition (the
/// production caller still pre-checks emptiness before draining).
///
/// This is the cutter's "what may one chunk contain" decision: a chunk
/// manifest row describes a gap-free `[first_line, first_line +
/// line_count)`, so the cutter drains exactly one maximal contiguous
/// run per chunk. Payload-free per the kernel style — the caller maps
/// its `(line_number, bytes)` buffer down to the line numbers.
///
/// `checked_add` keeps the scan total at `u64::MAX` (the run simply
/// ends there); under the `BIGINT` precondition every production input
/// is far below the edge, so the result is identical to the old
/// wrapping form.
pub fn contiguous_prefix_len(line_numbers: impl Iterator<Item = u64>) -> usize {
    let mut iter = line_numbers;
    let Some(mut prev) = iter.next() else {
        return 0;
    };
    let mut len = 1;
    for n in iter {
        if prev.checked_add(1) != Some(n) {
            break;
        }
        len += 1;
        prev = n;
    }
    len
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Verify [`visit_chunk`] against its `kani::ensures` contracts for
    /// every `(next_line, first_line, n_lines)` triple satisfying the
    /// `kani::requires` BIGINT precondition (which `proof_for_contract`
    /// assumes automatically). The full type domain is a strict
    /// superset of the inputs reachable from production — the cursor
    /// can be any u64 (the client-supplied `since_line` is unclamped),
    /// the manifest columns are bounded by the SQL `try_from`, and the
    /// decompressed line count is bounded by the 16 MiB codec limit.
    /// Proving over the superset is sound: it implies the property over
    /// the reachable subset. `visit_chunk` has no loops, no allocation,
    /// no recursion, so the proof is exhaustive over the domain (not
    /// bounded).
    ///
    /// This establishes that the chunk-interval arithmetic cannot
    /// overflow under the BIGINT precondition (the ensures closures
    /// recompute `first_line + n_lines` with overflow checks on) and
    /// that one visit step behaves exactly like the model's
    /// `visitChunk`. The fold-level dedup property is
    /// `check_dedup_{pair,triple}_serves_union_exactly_once`.
    #[kani::proof_for_contract(visit_chunk)]
    fn check_visit_chunk_contract() {
        let next_line: u64 = kani::any();
        let first_line: u64 = kani::any();
        let n_lines: u64 = kani::any();
        let _ = visit_chunk(next_line, first_line, n_lines);
    }

    /// Verify [`accept_verdict`] against its `kani::ensures` contracts
    /// for every `(high_water_line, final_line_count, first_line_number,
    /// line_count)` quadruple — the kernel has no requires clause, so
    /// the proof covers the full u64/Option<u64> domain, which is
    /// exactly the untrusted-worker input space. The four iff clauses
    /// together prove the verdict partition matches the model's
    /// `acceptVerdict` (see the contract comment on the function).
    #[kani::proof_for_contract(accept_verdict)]
    fn check_accept_verdict_contract() {
        let high_water_line: u64 = kani::any();
        let final_line_count: Option<u64> = kani::any();
        let first_line_number: u64 = kani::any();
        let line_count: u64 = kani::any();
        let _ = accept_verdict(
            high_water_line,
            final_line_count,
            first_line_number,
            line_count,
        );
    }

    /// Is `x` one of the lines chunk `(first_line, n_lines)` holds?
    /// The spec-side membership predicate the dedup harnesses compare
    /// the fold's output against. Computed with exact arithmetic — the
    /// harnesses' assumptions keep `first_line + n_lines` in range, and
    /// CBMC rejects the harness if they did not.
    fn in_chunk(x: u64, first_line: u64, n_lines: u64) -> bool {
        x >= first_line && x < first_line + n_lines
    }

    /// Is `x` in the half-open range a [`ChunkVisit`] yielded?
    fn in_visit(x: u64, v: &ChunkVisit) -> bool {
        x >= v.yield_from && x < v.yield_until
    }

    // (The tracey verify markers for these harnesses live at the
    // `kani-rio-log-kernel` wiring point in nix/kani.nix, not here —
    // same discipline as the VM-test subtests list: a marker in the
    // harness would tell tracey the rule is verified even if the member
    // were never wired into the check set.)
    /// The read path's dedup over TWO possibly-overlapping chunks
    /// visited in ascending `first_line` order (the
    /// `read_manifest_range` `ORDER BY`): every line is served at most
    /// once, and the served set is exactly the union of the chunks'
    /// line ranges restricted to `[since_line, ∞)` — the
    /// `servedSpanExact` invariant of `docs/spec/models/logService.qnt`
    /// (`r.served == manifestUnion(e)` ∧ `r.count == r.served.size()`),
    /// proven here over the full BIGINT-bounded u64 domain instead of
    /// the model's `MAX_LINE = 3`.
    ///
    /// `x` is a universally quantified line number: CBMC checks the
    /// assertions for every value, so `in_a || in_b == in_union` is set
    /// equality and `!(in_a && in_b)` is pairwise disjointness.
    #[kani::proof]
    fn check_dedup_pair_serves_union_exactly_once() {
        let since: u64 = kani::any();
        let (f_a, n_a): (u64, u64) = (kani::any(), kani::any());
        let (f_b, n_b): (u64, u64) = (kani::any(), kani::any());
        // The manifest BIGINT precondition (read_manifest_range's
        // try_from) and the visit order (ORDER BY first_line; the
        // session_id tiebreak only orders equal-first_line chunks,
        // which the symbolic `f_a <= f_b` already covers).
        kani::assume(f_a <= i64::MAX as u64 && n_a <= i64::MAX as u64);
        kani::assume(f_b <= i64::MAX as u64 && n_b <= i64::MAX as u64);
        kani::assume(f_a <= f_b);

        let mut cursor = since;
        let v_a = visit_chunk(cursor, f_a, n_a);
        cursor = v_a.next_line;
        let v_b = visit_chunk(cursor, f_b, n_b);

        let x: u64 = kani::any();
        let served_a = in_visit(x, &v_a);
        let served_b = in_visit(x, &v_b);
        // Each line is served at most once across the walk.
        assert!(!(served_a && served_b));
        // The served set equals the union of the chunks' ranges above
        // the starting watermark: nothing is dropped (no line of any
        // chunk at or past `since` is missing) and nothing is invented.
        let in_union = x >= since && (in_chunk(x, f_a, n_a) || in_chunk(x, f_b, n_b));
        assert!((served_a || served_b) == in_union);
    }

    /// [`check_dedup_pair_serves_union_exactly_once`] over THREE
    /// chunks — enough for the shapes two chunks cannot reach: a chunk
    /// entirely contained in the union of its two predecessors, an
    /// overlap chain (A∩B ≠ ∅ ≠ B∩C), and a gap followed by an overlap.
    #[kani::proof]
    fn check_dedup_triple_serves_union_exactly_once() {
        let since: u64 = kani::any();
        let (f_a, n_a): (u64, u64) = (kani::any(), kani::any());
        let (f_b, n_b): (u64, u64) = (kani::any(), kani::any());
        let (f_c, n_c): (u64, u64) = (kani::any(), kani::any());
        kani::assume(f_a <= i64::MAX as u64 && n_a <= i64::MAX as u64);
        kani::assume(f_b <= i64::MAX as u64 && n_b <= i64::MAX as u64);
        kani::assume(f_c <= i64::MAX as u64 && n_c <= i64::MAX as u64);
        kani::assume(f_a <= f_b && f_b <= f_c);

        let mut cursor = since;
        let v_a = visit_chunk(cursor, f_a, n_a);
        cursor = v_a.next_line;
        let v_b = visit_chunk(cursor, f_b, n_b);
        cursor = v_b.next_line;
        let v_c = visit_chunk(cursor, f_c, n_c);

        let x: u64 = kani::any();
        let served_a = in_visit(x, &v_a);
        let served_b = in_visit(x, &v_b);
        let served_c = in_visit(x, &v_c);
        assert!(!(served_a && served_b));
        assert!(!(served_a && served_c));
        assert!(!(served_b && served_c));
        let in_union =
            x >= since && (in_chunk(x, f_a, n_a) || in_chunk(x, f_b, n_b) || in_chunk(x, f_c, n_c));
        assert!((served_a || served_b || served_c) == in_union);
    }

    /// [`manifest_covers_contiguously`] never reports a gapped manifest
    /// as covering: a `true` verdict means every point of `[0, up_to)`
    /// lies inside at least one chunk's range. This is the soundness
    /// direction of the completeness predicate — the direction whose
    /// failure serves a gapped log as complete and seals it against the
    /// late replay that would fill the gap. Bounded to manifests of at
    /// most three chunks (enough for a gap, an overlap, and an
    /// extension — the same shapes the model's `MAX_LINE = 3` domain
    /// reaches); the completeness direction (a fully-covered manifest
    /// is reported as covering) is pinned by the `contiguity_*` unit
    /// tests in rio-store's `gate.rs`.
    #[kani::proof]
    #[kani::unwind(5)]
    fn check_manifest_covers_no_uncovered_point() {
        const MAX_CHUNKS: usize = 3;
        let all: [(i64, i64); MAX_CHUNKS] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= MAX_CHUNKS);
        let chunks = &all[..len];
        // Rows the ingest path wrote: non-negative, end fits in BIGINT
        // (accept_verdict's overflow arm), ORDER BY first_line.
        for c in chunks {
            kani::assume(c.0 >= 0 && c.1 >= 0 && c.0.checked_add(c.1).is_some());
        }
        for w in chunks.windows(2) {
            kani::assume(w[0].0 <= w[1].0);
        }
        let up_to: i64 = kani::any();
        kani::assume(up_to >= 0);

        if manifest_covers_contiguously(chunks, up_to) {
            // A universally quantified point of [0, up_to): some chunk
            // must hold it.
            let x: i64 = kani::any();
            kani::assume(x >= 0 && x < up_to);
            assert!(chunks.iter().any(|&(f, n)| f <= x && x < f + n));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One visit step per row: `(cursor, first, n) -> (yield_from,
    /// yield_until, next_line)`. The skip rows pin the two skip causes
    /// (zero-line chunk; chunk entirely below the watermark) leaving
    /// the watermark untouched.
    #[test]
    fn visit_chunk_table() {
        type Case = ((u64, u64, u64), (u64, u64, u64));
        let cases: &[Case] = &[
            // Fresh cursor, chunk ahead of it: full yield.
            ((0, 0, 4), (0, 4, 4)),
            ((0, 10, 4), (10, 14, 14)),
            // Cursor inside the chunk: leading lines deduped.
            ((12, 10, 4), (12, 14, 14)),
            // Chunk entirely below the watermark: skipped, no advance.
            ((20, 10, 4), (20, 20, 20)),
            // Last line exactly at the watermark boundary: 10..14 with
            // cursor 14 is fully served already.
            ((14, 10, 4), (14, 14, 14)),
            // Zero-line chunk past the cursor must NOT advance the
            // watermark (the doc-comment's swallow hazard).
            ((3, 50, 0), (3, 3, 3)),
            // BIGINT-edge chunk: exact arithmetic at the precondition
            // boundary (first + n == i64::MAX as u64 + 1 is the largest
            // representable end).
            (
                (0, i64::MAX as u64 - 3, 3),
                (i64::MAX as u64 - 3, i64::MAX as u64, i64::MAX as u64),
            ),
        ];
        for &((cursor, first, n), (from, until, next)) in cases {
            let v = visit_chunk(cursor, first, n);
            assert_eq!(
                (v.yield_from, v.yield_until, v.next_line),
                (from, until, next),
                "visit_chunk({cursor}, {first}, {n})"
            );
            assert_eq!(v.is_empty(), from == until);
        }
    }

    /// The verdict partition in gate order: overflow beats
    /// non-monotone beats past-final beats accept; truncation clamps
    /// the accepted end to the ceiling.
    #[test]
    fn accept_verdict_partition() {
        use AcceptVerdict::*;
        // u64 wrap.
        assert_eq!(accept_verdict(0, None, u64::MAX, 2), RejectedOverflow);
        // Representable in u64 but past the BIGINT ceiling.
        assert_eq!(
            accept_verdict(0, None, i64::MAX as u64, 1),
            RejectedOverflow
        );
        // Exactly at the BIGINT ceiling: still representable.
        assert_eq!(
            accept_verdict(0, None, i64::MAX as u64 - 1, 1),
            Accepted {
                end: i64::MAX as u64
            }
        );
        // Overflow wins even when the batch is also non-monotone.
        assert_eq!(accept_verdict(10, None, u64::MAX, 2), RejectedOverflow);
        // Below the high-water mark.
        assert_eq!(accept_verdict(10, None, 9, 1), RejectedNonMonotone);
        // Non-monotone wins over past-final.
        assert_eq!(accept_verdict(10, Some(5), 4, 1), RejectedNonMonotone);
        // At or past the ceiling.
        assert_eq!(accept_verdict(0, Some(5), 5, 1), RejectedPastFinal);
        assert_eq!(accept_verdict(0, Some(5), 7, 1), RejectedPastFinal);
        // Straddling the ceiling: truncated to it.
        assert_eq!(accept_verdict(0, Some(5), 3, 4), Accepted { end: 5 });
        // Under the ceiling: untouched.
        assert_eq!(accept_verdict(0, Some(5), 3, 2), Accepted { end: 5 });
        assert_eq!(accept_verdict(0, Some(9), 3, 2), Accepted { end: 5 });
        // No ceiling: the batch's own end.
        assert_eq!(accept_verdict(3, None, 3, 4), Accepted { end: 7 });
        // Empty batch (the caller normally pre-handles it): total,
        // accepted, end == first.
        assert_eq!(accept_verdict(0, None, 3, 0), Accepted { end: 3 });
    }

    /// Coverage fold: gaps break it, overlap extends it, the empty
    /// manifest covers exactly the empty range.
    #[test]
    fn covers_contiguously_table() {
        // Exact cover.
        assert!(manifest_covers_contiguously(&[(0, 5), (5, 5)], 10));
        // Overlap extends coverage.
        assert!(manifest_covers_contiguously(&[(0, 6), (4, 6)], 10));
        // Duplicate-range chunks (failover twins) are harmless.
        assert!(manifest_covers_contiguously(&[(0, 5), (0, 5), (5, 5)], 10));
        // Interior gap.
        assert!(!manifest_covers_contiguously(&[(0, 4), (5, 5)], 10));
        // Doesn't reach up_to.
        assert!(!manifest_covers_contiguously(&[(0, 5)], 10));
        // Coverage past up_to is fine.
        assert!(manifest_covers_contiguously(&[(0, 50)], 10));
        // Empty manifest: covers [0, 0) only.
        assert!(manifest_covers_contiguously(&[], 0));
        assert!(!manifest_covers_contiguously(&[], 1));
        // A manifest not starting at zero never covers.
        assert!(!manifest_covers_contiguously(&[(1, 9)], 10));
    }

    /// The cutter's run rule: maximal consecutive prefix, zero on
    /// empty, run ends at the u64 edge instead of wrapping.
    #[test]
    fn contiguous_prefix_len_runs() {
        let lens = |v: &[u64]| contiguous_prefix_len(v.iter().copied());
        assert_eq!(lens(&[]), 0);
        assert_eq!(lens(&[7]), 1);
        assert_eq!(lens(&[7, 8, 9]), 3);
        // Forward gap splits the run.
        assert_eq!(lens(&[7, 8, 10, 11]), 2);
        // Backwards numbering is never consecutive.
        assert_eq!(lens(&[7, 6, 5]), 1);
        // Duplicate line number ends the run.
        assert_eq!(lens(&[7, 7]), 1);
        // The u64::MAX edge: checked_add ends the run; no wrap to 0.
        assert_eq!(lens(&[u64::MAX, 0]), 1);
        assert_eq!(lens(&[u64::MAX - 1, u64::MAX]), 2);
    }
}
