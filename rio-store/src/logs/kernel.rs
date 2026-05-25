//! Pure decision kernels for the log chunk subsystem.
//!
//! Every function here is total, allocation-free, loop-free (except the
//! bounded manifest fold), and depends on nothing but `core` — no SQL,
//! no S3, no clocks, no `Vec<u8>` payloads. The I/O-shaped callers
//! ([`super::tail::read_chunk`], [`super::ingest::IngestSession::accept`],
//! [`super::gate`]'s completeness predicate) project their inputs into
//! plain integers, delegate the decision here, and apply the returned
//! verdict. The split mirrors `rio-lease`'s `decide()` / `decide_pure()`
//! pair: the kernel is the verifiable core, the caller is the
//! I/O-shaped shim.
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
/// the caller stores back into its [`super::tail::LineCursor`].
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
/// batch. The three `Rejected*` variants map one-to-one onto
/// [`super::ingest::AcceptOutcome`]'s rejection variants; `Accepted`
/// carries the post-truncation exclusive end the caller needs to
/// truncate the batch and advance its high-water mark.
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
