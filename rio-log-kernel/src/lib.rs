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
/// The enum is `#[must_use]` and gap-explicit: a forward jump from the
/// watermark to the chunk's first line — the shape the old struct
/// silently absorbed via `yield_from = max(first_line, next_line)` —
/// is its own variant, so every consumer is compile-forced to write a
/// gap arm and choose a disclosure (serve across a genuine storage
/// hole, back-fill a fan-out drop, re-open at the gap, or render a gap
/// marker). Silent gap absorption is no longer expressible.
///
/// The contributed line numbers are the half-open range
/// `[yield_from, yield_until)`; `next_line` is the post-visit
/// watermark the caller stores back into its cursor (rio-store's
/// `logs::tail::LineCursor`, the gateway's relay floor, the
/// dashboard/CLI mirrors).
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkVisit {
    /// The chunk contributes nothing: zero lines, or every line is
    /// below the watermark. The caller skips the object GET entirely.
    /// A skipped chunk MUST NOT advance the watermark — a zero-line
    /// chunk starting past the cursor would otherwise swallow every
    /// line between the cursor and its `first_line`.
    Skip {
        /// The watermark, unchanged.
        next_line: u64,
    },
    /// The chunk starts at or below the watermark and contributes
    /// `[yield_from, yield_until)` with no jump: the served stream
    /// stays contiguous through this visit.
    Serve {
        /// First line this chunk contributes: the watermark (the
        /// chunk's leading lines below it, if any, were already served
        /// by an earlier overlapping chunk).
        yield_from: u64,
        /// One past the last line this chunk contributes
        /// (`first_line + n_lines`).
        yield_until: u64,
        /// The post-visit watermark: `yield_until`.
        next_line: u64,
    },
    /// The chunk starts strictly past the watermark: the lines
    /// `[gap_from, gap_until)` (`== [watermark, first_line)`) are not
    /// held by this chunk — a genuine storage hole on the manifest
    /// walk, a dropped fan-out batch on the live seam, or a skipped
    /// span on a relayed wire. The chunk itself contributes
    /// `[yield_from, yield_until) == [first_line, first_line +
    /// n_lines)`.
    GapThenServe {
        /// First missing line: the input watermark.
        gap_from: u64,
        /// One past the last missing line: the chunk's `first_line`.
        /// Strictly greater than `gap_from`.
        gap_until: u64,
        /// First line this chunk contributes (`== gap_until`).
        yield_from: u64,
        /// One past the last line this chunk contributes.
        yield_until: u64,
        /// The post-visit watermark: `yield_until`.
        next_line: u64,
    },
}

impl ChunkVisit {
    /// The chunk contributes nothing (zero lines, or every line is
    /// below the watermark).
    pub fn is_empty(&self) -> bool {
        matches!(self, ChunkVisit::Skip { .. })
    }

    /// The post-visit watermark, for cursor stepping that does not
    /// otherwise inspect the verdict (folds, fuzz oracles, tests).
    /// Decision-making consumers should `match` instead — the variants
    /// are the point.
    pub fn next_line(&self) -> u64 {
        match *self {
            ChunkVisit::Skip { next_line }
            | ChunkVisit::Serve { next_line, .. }
            | ChunkVisit::GapThenServe { next_line, .. } => next_line,
        }
    }

    /// Mint the sealed cursor advance for this verdict — the ONLY
    /// constructor of [`CursorAdvance`] (merged_bug_205): a serve loop
    /// can move its watermark exclusively to a post-visit position the
    /// kernel computed. The open-coded `filter(>= cursor)` +
    /// `advance_to(end + 1)` shape — which silently absorbed residual
    /// gaps — no longer typechecks.
    pub fn advance(&self) -> CursorAdvance {
        CursorAdvance {
            to: self.next_line(),
        }
    }
}

/// A sealed watermark movement: produced only by
/// [`ChunkVisit::advance`] (and therefore only from a kernel verdict).
/// The field is private — a caller cannot fabricate an advance past
/// lines no verdict served (merged_bug_205: the silent residual-gap
/// absorption class).
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorAdvance {
    to: u64,
}

impl CursorAdvance {
    /// The verdict-computed post-visit watermark.
    pub fn to(&self) -> u64 {
        self.to
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
    // Variant exactness: the case split is total and each variant
    // carries exactly the ranges its docs promise. Skipped iff nothing
    // at or above the watermark; a contributing chunk is Serve when it
    // starts at or below the watermark and GapThenServe (with the gap
    // exactly `[watermark, first_line)`) when it starts past it.
    let end = first_line + n_lines;
    if n_lines == 0 || end <= next_line {
        matches!(r, ChunkVisit::Skip { .. })
    } else if first_line > next_line {
        matches!(r, ChunkVisit::GapThenServe {
            gap_from, gap_until, yield_from, yield_until, ..
        } if *gap_from == next_line
            && *gap_until == first_line
            && *yield_from == first_line
            && *yield_until == end)
    } else {
        matches!(r, ChunkVisit::Serve { yield_from, yield_until, .. }
            if *yield_from == next_line && *yield_until == end)
    }
}))]
#[cfg_attr(kani, kani::ensures(|r: &ChunkVisit| {
    // Dedup safety: no yielded line is below the watermark (a line
    // already served by an earlier chunk is never served again), the
    // yielded range is well-formed and within the chunk, and a gap is
    // non-empty, starts at the watermark, and abuts the served range.
    match r {
        ChunkVisit::Skip { .. } => true,
        ChunkVisit::Serve { yield_from, yield_until, .. } => {
            *yield_from >= next_line
                && *yield_from <= *yield_until
                && *yield_from >= first_line.min(next_line)
                && *yield_until <= first_line + n_lines
        }
        ChunkVisit::GapThenServe { gap_from, gap_until, yield_from, yield_until, .. } => {
            *gap_from == next_line
                && *gap_from < *gap_until
                && *gap_until == *yield_from
                && *yield_from == first_line
                && *yield_until == first_line + n_lines
        }
    }
}))]
#[cfg_attr(kani, kani::ensures(|r: &ChunkVisit| {
    // The watermark is monotone and lands one past the chunk's last
    // line iff the chunk contributed anything; a skipped chunk leaves
    // it untouched.
    let end = first_line + n_lines;
    r.next_line()
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
        return ChunkVisit::Skip { next_line };
    }
    if first_line > next_line {
        // Forward jump: the span `[next_line, first_line)` is missing
        // from this chunk. Naming it is the whole point of the enum —
        // the caller decides the disclosure.
        return ChunkVisit::GapThenServe {
            gap_from: next_line,
            gap_until: first_line,
            yield_from: first_line,
            yield_until: end,
            next_line: end,
        };
    }
    ChunkVisit::Serve {
        yield_from: next_line,
        yield_until: end,
        next_line: end,
    }
}

/// One keyed chunk visit: the execution axis decided BEFORE the line
/// axis (merged_bug_002). A `TailLog` subscription that follows a
/// derivation can observe chunks from a NEW execution (a retry on
/// another worker) whose line numbers restart at zero — fed straight
/// into [`visit_chunk`], the restart is indistinguishable from a
/// duplicate (`Skip`) or a backwards walk, and the consumer silently
/// splices two different builds' logs into one numbering.
///
/// The enum forces every keyed consumer to write the switch arm: a
/// caller that matches on `KeyedVisit` cannot compile without deciding
/// what an execution switch does to its cursor (reset), its UI (the
/// disclosure row), and its dedup floor. Mirrored byte-for-byte in the
/// dashboard's `lineCursor.ts::visitChunkKeyed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyedVisit {
    /// The chunk belongs to a different execution than the cursor's:
    /// the consumer must reset its cursor to the chunk's view (visit
    /// the SAME chunk again against a zero floor after switching) and
    /// disclose the switch. No lines are classified by this verdict.
    ExecSwitch,
    /// Same execution: the plain line-axis verdict.
    Visit(ChunkVisit),
}

// r[impl store.log.session-keyed]
/// [`visit_chunk`] with the execution axis in front: `keys_match` is
/// the caller's `current_exec == chunk_exec` (payload-free — the
/// kernel never sees the key representation, only the comparison).
/// `keys_match == false` is an [`KeyedVisit::ExecSwitch`]; the caller
/// resets its cursor and re-visits.
pub fn visit_chunk_keyed(
    keys_match: bool,
    next_line: u64,
    first_line: u64,
    n_lines: u64,
) -> KeyedVisit {
    if !keys_match {
        return KeyedVisit::ExecSwitch;
    }
    KeyedVisit::Visit(visit_chunk(next_line, first_line, n_lines))
}

/// Why one connected `TailLog` stream stopped yielding messages, as
/// the relay's loop observed it. The conflations this enum forbids are
/// the merged_bug_076 class: an `Err` is always
/// [`TransportErr`](TailStopCause::TransportErr) and a clean `None` is
/// always [`NaturalEnd`](TailStopCause::NaturalEnd) — neither means "drained"
/// on its own; only [`tail_next`] decides that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailStopCause {
    /// The stream ended cleanly (the serving session closed).
    NaturalEnd,
    /// The stream (or its open) died with a transport/status error.
    TransportErr,
    /// The open call itself failed — there was never a stream.
    OpenFailed,
    /// The relay observed a forward jump it has not yet accepted (the
    /// gap-discipline path: re-open once at the gap before relaying a
    /// disclosure).
    GapObserved,
    /// The relay itself is orphaned: its consumer-side lifecycle
    /// channel's sender vanished without an abort (the owning set was
    /// dropped on a path that never reached `remove`/`abort_all`).
    /// No consumer remains and no drain signal will ever arrive —
    /// re-opening is the merged_bug_130 hot-loop. The law exits
    /// unconditionally. Gateway-only by design: the dashboard's TS
    /// mirror is deliberately NOT extended (a browser stream owns its
    /// consumer in the same task; orphanhood is unrepresentable there).
    Orphaned,
    /// The stream died with a TYPED-permanent unservable error — the
    /// store stamped `x-rio-log-unservable` on the status (a manifest
    /// hole no row covers, a corrupt oversized row): every future open
    /// will refuse identically, so re-dialing is the merged_bug_164
    /// 1 Hz wedge. The law exits unconditionally. Mirrored in the
    /// dashboard's TS `tailNext` as `'permanentErr'` (unlike
    /// `Orphaned`, a browser consumer CAN observe this — it arrives in
    /// the stream error's metadata).
    PermanentErr,
}

/// The verdict of [`tail_next`].
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailNext {
    /// Re-open the stream (after the caller-owned backoff, capped at
    /// the remaining grace).
    Reopen,
    /// The subscription is finished.
    Exit,
}

/// The relay's exit-decision kernel: may a `TailLog` subscription stop
/// re-opening?
///
/// The law (merged_bug_076, merged_bug_130, merged_bug_164): **Exit
/// iff the grace budget is spent, or the relay is orphaned (no
/// consumer remains), or the failure is typed-permanent (the store
/// said no open can ever serve this), or the stream ended naturally
/// with the derivation terminal and the served log complete.** Every
/// other shape re-opens — a transport error after terminal, an open
/// failure at terminal, a natural end with an incomplete served log, a
/// gap awaiting its second chance: all of these have lines that may
/// still be servable and budget to fetch them with. "Give up with grace unspent and the log
/// incomplete" is unrepresentable by this function — and so is "keep
/// opening streams nobody reads": an orphaned relay has no consumer to
/// lose lines for, so exiting loses nothing and re-opening burns a
/// store connection per backoff tick forever.
///
/// `terminal` is "the derivation reached a terminal status";
/// `grace_expired` is "the armed-once post-terminal grace deadline has
/// passed" (false whenever the deadline is not yet armed);
/// `served_complete` is the most recent final message's `is_complete`
/// — the store's own claim that everything durable was served.
// r[impl store.log.tail-grace-drain+2]
pub fn tail_next(
    cause: TailStopCause,
    terminal: bool,
    grace_expired: bool,
    served_complete: bool,
) -> TailNext {
    if grace_expired {
        return TailNext::Exit;
    }
    match cause {
        TailStopCause::Orphaned | TailStopCause::PermanentErr => TailNext::Exit,
        TailStopCause::NaturalEnd if terminal && served_complete => TailNext::Exit,
        TailStopCause::NaturalEnd
        | TailStopCause::TransportErr
        | TailStopCause::OpenFailed
        | TailStopCause::GapObserved => TailNext::Reopen,
    }
}

/// The divergence between a chunk's manifest claim and what its object
/// actually decompressed to. The manifest row and the object are
/// written from the same line slice in the same call, so any
/// disagreement is corruption-grade — but the two directions have
/// different blast radii and therefore different policies (decided by
/// the caller; classified here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectDivergence {
    /// The object holds FEWER lines than the manifest claims: the span
    /// `[missing_from, missing_until)` exists in no object — data loss
    /// with `NotFound` parity (the manifest promised lines that cannot
    /// be served).
    ShortObject {
        /// First missing line: `first_line + object_count`.
        missing_from: u64,
        /// One past the last missing line: `first_line +
        /// manifest_count`.
        missing_until: u64,
    },
    /// The object holds MORE lines than the manifest claims: `excess`
    /// trailing lines have no manifest identity. Serving them would
    /// attribute them to the NEXT chunk's line numbers and advance the
    /// watermark past lines that chunk genuinely holds — they must be
    /// discarded.
    LongObject {
        /// How many trailing object lines exceed the claim.
        excess: u64,
    },
}

/// One chunk's read verdict when the object's decompressed line count
/// can disagree with its manifest row: the dedup visit, clamped to the
/// manifest claim BY CONSTRUCTION, plus the divergence classification.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectVisit {
    /// The dedup verdict over `min(manifest_count, object_count)`
    /// lines: the served range and the post-visit watermark can never
    /// exceed what BOTH the manifest claims and the object holds.
    pub visit: ChunkVisit,
    /// `None` when the object matches its claim exactly.
    pub divergence: Option<ObjectDivergence>,
}

/// The manifest/object clamp for one fetched chunk
/// (`docs/spec/models/logService.qnt`'s `visitChunk` composed with the
/// served-count clamp): evaluate the dedup over
/// `min(manifest_count, object_count)` so an over-length object can
/// never displace a successor chunk's range (the watermark is bounded
/// by the claim) and an under-length object can never silently shrink
/// the stream (the missing span is named).
///
/// Same `BIGINT` precondition as [`visit_chunk`]; `object_count` is
/// additionally bounded in production by the decompressor's 16 MiB
/// frame budget, far below `i64::MAX`.
#[cfg_attr(
    kani,
    kani::requires(
        first_line <= i64::MAX as u64
            && manifest_count <= i64::MAX as u64
            && object_count <= i64::MAX as u64
    )
)]
#[cfg_attr(kani, kani::ensures(|r: &ObjectVisit| {
    // The visit is exactly the dedup verdict over the clamped count —
    // the composition with visit_chunk is definitional, so the whole
    // per-variant contract of visit_chunk transfers.
    let served = if manifest_count < object_count { manifest_count } else { object_count };
    r.visit == visit_chunk(next_line, first_line, served)
}))]
#[cfg_attr(kani, kani::ensures(|r: &ObjectVisit| {
    // Divergence classification is exact and total.
    match r.divergence {
        None => object_count == manifest_count,
        Some(ObjectDivergence::ShortObject { missing_from, missing_until }) => {
            object_count < manifest_count
                && missing_from == first_line + object_count
                && missing_until == first_line + manifest_count
        }
        Some(ObjectDivergence::LongObject { excess }) => {
            object_count > manifest_count && excess == object_count - manifest_count
        }
    }
}))]
// r[impl store.log.read-divergence+2]
pub fn visit_object(
    next_line: u64,
    first_line: u64,
    manifest_count: u64,
    object_count: u64,
) -> ObjectVisit {
    let served = manifest_count.min(object_count);
    let divergence = if object_count < manifest_count {
        Some(ObjectDivergence::ShortObject {
            missing_from: first_line.saturating_add(object_count),
            missing_until: first_line.saturating_add(manifest_count),
        })
    } else if object_count > manifest_count {
        Some(ObjectDivergence::LongObject {
            excess: object_count - manifest_count,
        })
    } else {
        None
    };
    ObjectVisit {
        visit: visit_chunk(next_line, first_line, served),
        divergence,
    }
}

/// The read-path policy for a SHORT object (one that decompressed to
/// fewer lines than its manifest row claims) — bug_233. The two cases
/// have different blast radii and the choice is COVERAGE, not arm
/// ordering: the wedge class was a fully-servable overlap topology
/// (a second session's row covering the missing span) refused behind
/// `Status::internal` forever.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortObjectPolicy {
    /// Some remaining manifest row covers every missing line: serve
    /// the clamped lines and keep walking — the covering rows supply
    /// the rest. Disclosure-only divergence (warn + divergence
    /// counter), never a refusal.
    ServeClamped,
    /// No remaining row covers the missing span: the lines exist
    /// nowhere — corruption-grade loss. The read fails TYPED-permanent
    /// (`LOG_UNSERVABLE_METADATA_KEY`) so the reader exit law stops
    /// re-dialing an unservable hole; the seal/read contradiction for
    /// genuine corruption is disclosed, not repaired (the manifest row
    /// and object were written from one slice — disagreement here has
    /// no in-band repair).
    UnservableHole,
}

/// Decide the [`ShortObjectPolicy`] for a missing span
/// `[missing_from, missing_until)` given the reader's served-prefix
/// cursor and the REMAINING manifest rows (`(first_line,
/// line_count)`, ordered by `first_line` — the manifest read order).
/// Coverage may be stitched from several overlapping rows.
///
/// merged_bug_001: the cursor is an EXPLICIT parameter so the kani
/// oracle and every test are forced to quantify over the
/// served-prefix axis. Lines below `cursor_next_line` were already
/// yielded to THIS reader by earlier (overlapping-session) chunks —
/// the covering row sorts BEFORE the current chunk and is
/// structurally invisible to the remaining-rows walk, so without the
/// clamp a fully-servable prefix-overlap topology pages a false
/// critical hole.
// r[impl store.log.read-divergence+2]
pub fn short_object_policy(
    cursor_next_line: u64,
    missing_from: u64,
    missing_until: u64,
    remaining: &[(u64, u64)],
) -> ShortObjectPolicy {
    // Served-prefix clamp: the cursor is proof of service.
    let needed_from = missing_from.max(cursor_next_line);
    if needed_from >= missing_until {
        return ShortObjectPolicy::ServeClamped;
    }
    let mut covered_to = needed_from;
    for &(first, count) in remaining {
        if covered_to >= missing_until {
            break;
        }
        let end = first.saturating_add(count);
        // Ordered input: a row starting past the frontier leaves a
        // hole no later row can fill (they start even further right).
        if first > covered_to {
            break;
        }
        covered_to = covered_to.max(end);
    }
    if covered_to >= missing_until {
        ShortObjectPolicy::ServeClamped
    } else {
        ShortObjectPolicy::UnservableHole
    }
}

/// Why a live-tail subscriber observed a forward jump in its fan-out
/// stream — the typed provenance that routes the recovery policy
/// (merged_bug_187). The old code treated EVERY jump as a recoverable
/// drop: a worker-admitted hole (lines the store never accepted)
/// triggered the full clone + manifest walk + recovery counter on a
/// span that no view can ever hold — a worker-drivable amplification.
/// And the old vocabulary blamed "(suppressed lines)" (merged_bug_275):
/// builder suppression never skips numbers — the only sources of an
/// admitted hole are worker-side jumps the ingest accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapProvenance {
    /// The whole missing span was never accepted by the store (the
    /// batch's coverage floor is at or below the gap): there is
    /// nothing to recover anywhere. Serve across — no clone, no
    /// manifest read, no recovery counter.
    AdmittedHole,
    /// The span `[gap_from, dropped_until)` WAS accepted (it lies
    /// below the batch's coverage floor) and this subscriber missed it
    /// — the lossy fan-out dropped batches for its queue. ONE counted
    /// backfill from manifest ∪ live buffer, then re-visit the batch;
    /// any remainder at or above the floor is admitted.
    DroppedSpan {
        /// One past the end of the recoverable (accepted) part of the
        /// gap: `min(coverage_floor, gap_until)`.
        dropped_until: u64,
    },
}

/// The verdict of [`visit_fanout_batch`]: the dedup visit plus the gap
/// provenance, when there is a gap.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanoutVisit {
    /// The dedup verdict over the batch (exactly
    /// [`visit_chunk`]`(next_line, first_line, n_lines)`).
    pub visit: ChunkVisit,
    /// `Some` iff the visit is a [`ChunkVisit::GapThenServe`]: how the
    /// missing span came to be missing, computed against the batch's
    /// coverage floor.
    pub provenance: Option<GapProvenance>,
}

/// One live fan-out batch's verdict for a subscriber at watermark
/// `next_line` (`docs/spec/models/logService.qnt::readerDrain`):
/// the dedup visit, with any gap classified against `coverage_floor` —
/// the ingest session's accepted high-water mark at the instant the
/// batch was fanned out. Every line below the floor was accepted by
/// the store (recoverable from manifest ∪ live buffer); every line at
/// or above it was never accepted (nothing anywhere can serve it).
///
/// Same `BIGINT` precondition as [`visit_chunk`].
#[cfg_attr(
    kani,
    kani::requires(
        next_line <= i64::MAX as u64
            && first_line <= i64::MAX as u64
            && n_lines <= i64::MAX as u64
            && coverage_floor <= i64::MAX as u64
    )
)]
#[cfg_attr(kani, kani::ensures(|r: &FanoutVisit| {
    r.visit == visit_chunk(next_line, first_line, n_lines)
}))]
#[cfg_attr(kani, kani::ensures(|r: &FanoutVisit| {
    match (r.visit, r.provenance) {
        // Provenance exists iff there is a gap.
        (ChunkVisit::Skip { .. }, None) | (ChunkVisit::Serve { .. }, None) => true,
        (ChunkVisit::GapThenServe { gap_from, gap_until, .. },
         Some(GapProvenance::AdmittedHole)) =>
            coverage_floor <= gap_from && gap_from < gap_until,
        (ChunkVisit::GapThenServe { gap_from, gap_until, .. },
         Some(GapProvenance::DroppedSpan { dropped_until })) =>
            gap_from < coverage_floor
                && dropped_until == if coverage_floor < gap_until { coverage_floor } else { gap_until }
                && gap_from < dropped_until,
        _ => false,
    }
}))]
// r[impl store.log.gap-provenance]
pub fn visit_fanout_batch(
    next_line: u64,
    first_line: u64,
    n_lines: u64,
    coverage_floor: u64,
) -> FanoutVisit {
    let visit = visit_chunk(next_line, first_line, n_lines);
    let provenance = match visit {
        ChunkVisit::Skip { .. } | ChunkVisit::Serve { .. } => None,
        ChunkVisit::GapThenServe {
            gap_from,
            gap_until,
            ..
        } => Some(if gap_from < coverage_floor {
            GapProvenance::DroppedSpan {
                dropped_until: coverage_floor.min(gap_until),
            }
        } else {
            GapProvenance::AdmittedHole
        }),
    };
    FanoutVisit { visit, provenance }
}

/// A served-stream completeness claim: the ONLY way to say
/// `is_complete = true` on a `TailLog` final message. Private fields —
/// constructible solely through [`final_claim`], which correlates the
/// claim with the SERVED cursor, not just the durable state
/// (merged_bug_063: a seal + manifest commit landing mid-serve made
/// the old uncorrelated predicate stamp `complete = true` on a stream
/// that had served half the lines; the reconnect heal never fired).
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinalClaim {
    complete: bool,
    cursor_next: u64,
}

impl FinalClaim {
    /// Did the served stream deliver everything the sealed witness
    /// recorded? `true` only when the sealed final line count exists,
    /// the manifest covers it contiguously, AND the served cursor has
    /// reached it.
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// The served watermark this claim was minted against — the
    /// `next_line` the final message advertises for the reconnect
    /// contract.
    pub fn cursor_next(&self) -> u64 {
        self.cursor_next
    }
}

/// Mint the final-message completeness claim
/// (`docs/spec/models/logService.qnt::readerStamp`):
/// **complete ⇔ sealed = Some(n) ∧ cursor_next ≥ n ∧
/// manifest_covers_sealed.**
///
/// `sealed_final_line_count` is the lifecycle row's recorded end (only
/// when terminal); `manifest_covers_sealed` is the contiguity fold up
/// to that count; `cursor_next` is the watermark actually served to
/// THIS reader. A mid-serve commit+seal yields `complete = false` and
/// the client's reconnect contract heals the difference.
#[cfg_attr(kani, kani::ensures(|r: &FinalClaim| {
    r.cursor_next == cursor_next
        && (r.complete
            == matches!(sealed_final_line_count,
                        Some(n) if cursor_next >= n && manifest_covers_sealed))
}))]
// r[impl store.log.served-claim]
pub fn final_claim(
    cursor_next: u64,
    sealed_final_line_count: Option<u64>,
    manifest_covers_sealed: bool,
) -> FinalClaim {
    let complete = matches!(sealed_final_line_count,
                            Some(n) if cursor_next >= n && manifest_covers_sealed);
    FinalClaim {
        complete,
        cursor_next,
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
    /// The batch holds more lines than [`MAX_BATCH_LINES`] — more than
    /// one chunk's charged capacity in a single admission. Per-batch
    /// (the stream stays open, matching the other `Rejected*`
    /// semantics): the bound stops the line-count amplification axis
    /// (millions of near-empty lines in one wire message materializing
    /// as millions of buffered allocations) before any per-line work,
    /// without killing an otherwise-honest session.
    RejectedOversizedBatch,
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
/// 2. admission size — the batch may not hold more lines than
///    [`MAX_BATCH_LINES`] (one chunk's charged capacity; the
///    line-count amplification bound);
/// 3. monotone floor — the batch must start at or above the high-water
///    mark (forward gaps are legal, going backwards is not);
/// 4. completeness ceiling — a batch starting at or past the recorded
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
// one addition: the model's bounded line domain cannot represent an
// unrepresentable batch, so its predicate is conditioned on
// representability — the later clauses carry the same `end <= i64::MAX`
// conjunct. The oversized arm is the other (bug_298's admission axis):
// the model's `MAX_BATCH_LINES` lives far above its bounded line
// domain, so its `acceptVerdict` carries the arm with a small bound
// (`batchLinesBounded` + `CALIB_OVERSIZED_ADMITTED` exercise it); the
// later clauses carry the matching `line_count <= MAX_BATCH_LINES`
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
    // RejectedOversizedBatch iff representable AND over the per-batch
    // admission bound (one chunk's charged capacity).
    matches!(r, AcceptVerdict::RejectedOversizedBatch)
        == (first_line_number
            .checked_add(line_count)
            .is_some_and(|end| end <= i64::MAX as u64)
            && line_count > MAX_BATCH_LINES)
}))]
#[cfg_attr(kani, kani::ensures(|r: &AcceptVerdict| {
    // RejectedNonMonotone iff representable, within the admission
    // bound, AND below the high-water mark
    // (`if (lo < hw) RejectNonMonotone`).
    matches!(r, AcceptVerdict::RejectedNonMonotone)
        == (first_line_number
            .checked_add(line_count)
            .is_some_and(|end| end <= i64::MAX as u64)
            && line_count <= MAX_BATCH_LINES
            && first_line_number < high_water_line)
}))]
#[cfg_attr(kani, kani::ensures(|r: &AcceptVerdict| {
    // RejectedPastFinal iff representable, within the admission bound,
    // monotone, AND the batch starts at or past the known ceiling
    // (`if (lo >= c.value) RejectPastFinal`).
    matches!(r, AcceptVerdict::RejectedPastFinal)
        == (first_line_number
            .checked_add(line_count)
            .is_some_and(|end| end <= i64::MAX as u64)
            && line_count <= MAX_BATCH_LINES
            && first_line_number >= high_water_line
            && final_line_count.is_some_and(|c| first_line_number >= c))
}))]
#[cfg_attr(kani, kani::ensures(|r: &AcceptVerdict| {
    // Accepted iff no rejection fires (implied by the four iffs above
    // over the five-variant enum); the accepted end is the batch's end
    // clamped to the ceiling (`Accept(minInt(hi, c.value))` /
    // `Accept(hi)`), so no accepted line is ever at or past the
    // recorded final_line_count (store.log.completeness-gate) and the
    // kept-line count never exceeds the batch's.
    match r {
        AcceptVerdict::Accepted { end } => first_line_number
            .checked_add(line_count)
            .is_some_and(|batch_end| {
                batch_end <= i64::MAX as u64
                    && line_count <= MAX_BATCH_LINES
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
    // -- Check 2: the per-batch admission bound. One AppendLog message
    // may not carry more lines than one chunk's charged capacity
    // ([`MAX_BATCH_LINES`]): past it the batch is rejected before any
    // per-line allocation, truncation, or fan-out work — the line-count
    // amplification axis (bug_298) is cut at admission, not at the
    // lifetime cap after the allocations already happened.
    if line_count > MAX_BATCH_LINES {
        return AcceptVerdict::RejectedOversizedBatch;
    }
    // -- Check 3: the monotone floor. A batch starting at or below a
    // line number the session already holds or has cut is malformed
    // worker input; renumbering it would corrupt every downstream
    // consumer of the ordering, so it is dropped whole.
    if first_line_number < high_water_line {
        return AcceptVerdict::RejectedNonMonotone;
    }
    // -- Check 4: the completeness ceiling. Once known, the recorded
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

/// Width of the per-line `u32` length prefix in a chunk payload frame.
/// Owned here because BOTH halves of the codec bound depend on it: the
/// cutter's prospective payload arithmetic
/// ([`bounded_contiguous_prefix_len`]) and the store codec's framing.
pub const LINE_LEN_PREFIX_BYTES: u64 = 4;

/// The chunk payload ceiling shared by the write and read paths: the
/// cutter never drains a run whose charged payload
/// ([`charged_line_cost`] + [`LINE_LEN_PREFIX_BYTES`] per line) exceeds
/// this, and the reader refuses to decompress past it. One constant,
/// one owner — the write/read bound asymmetry (a committed chunk the
/// read path rejects) is unrepresentable while both sides consume this
/// value.
pub const MAX_CHUNK_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;

/// Per-line memory overhead charged on top of a line's content bytes
/// by every byte-denominated bound in the log pipeline: the `u64`
/// line-number key (8 bytes) plus the `Vec<u8>` header (ptr + len +
/// cap = 24 bytes on 64-bit).
///
/// Without this charge a stream of near-empty lines holds ~33x its
/// accounted bytes resident before any byte trigger fires — the
/// line-count amplification axis of bug_298. The kernel owns the
/// constant so the ingest buffer accounting (cut trigger, lifetime
/// cap, `buffer_bytes` bookkeeping) and the cutter's chunk budget
/// ([`bounded_contiguous_prefix_len`]) charge the SAME formula; two
/// formulas is exactly the write/read asymmetry this module exists to
/// make unrepresentable.
pub const PER_LINE_OVERHEAD_BYTES: u64 = 32;

/// The charged size of one (post-truncation) line: content bytes plus
/// [`PER_LINE_OVERHEAD_BYTES`]. THE byte-axis formula — the ingest
/// accounting and the cutter budget both consume it, so a bound
/// denominated in "bytes" can never again be defeated by line count.
pub const fn charged_line_cost(content_len: u64) -> u64 {
    content_len.saturating_add(PER_LINE_OVERHEAD_BYTES)
}

/// The most lines one committed chunk can hold post-fix: an all-empty
/// run charges `PER_LINE_OVERHEAD_BYTES + LINE_LEN_PREFIX_BYTES` per
/// line against [`MAX_CHUNK_PAYLOAD_BYTES`], so the cutter's bounded
/// drain ([`bounded_contiguous_prefix_len`]) tops out here (466,033).
/// Documentation + test anchor; the bound itself is enforced by the
/// charge formula, not by counting.
pub const MAX_CHUNK_LINES: u64 =
    MAX_CHUNK_PAYLOAD_BYTES / (PER_LINE_OVERHEAD_BYTES + LINE_LEN_PREFIX_BYTES);

/// The absolute read-side line-count backstop for one chunk:
/// `MAX_CHUNK_PAYLOAD_BYTES / LINE_LEN_PREFIX_BYTES` (4,194,304) — the
/// most lines any decodable payload could EVER have framed (every
/// line, even empty, costs its length prefix), i.e. the pre-fix
/// theoretical ceiling. No legitimately-written chunk, old or new, can
/// exceed it, so refusing past it (the manifest-row pre-GET check and
/// the decoder's parse loop) never rejects grandfathered data; a row
/// claiming more is corrupt by construction and is refused BEFORE the
/// allocation it requests.
pub const MAX_CHUNK_LINES_ABS: u64 = MAX_CHUNK_PAYLOAD_BYTES / LINE_LEN_PREFIX_BYTES;

/// The per-line content ceiling for the whole log pipeline: 64 KiB.
/// The store's ingest truncates every accepted line to this before
/// buffering (the enforcement for non-conforming workers), and the
/// builder's `LogBatcher` pre-truncates to the SAME constant before
/// transmitting — content past it is guaranteed discarded server-side,
/// so shipping it is pure wire cost, and with the log plane's decode
/// cap at [`MAX_CHUNK_PAYLOAD_BYTES`] an untruncated multi-MiB line
/// would make an otherwise-honest flush undecodable (bug_298's
/// regression edge: the daemon protocol admits 64 MiB strings). One
/// constant, one owner — `64 lines x (this + frame) ~ 4 MiB << 16 MiB`
/// is const-asserted by the builder.
pub const MAX_LINE_CONTENT_BYTES: u64 = 64 * 1024;

/// Per-batch admission bound for `AppendLog`: one batch may not hold
/// more lines than one chunk's charged capacity ([`MAX_CHUNK_LINES`]).
/// Enforced by [`accept_verdict`]'s `RejectedOversizedBatch` arm —
/// per-batch, stream stays open. Honest builders sit orders of
/// magnitude below this (the builder flushes at 64 lines); the bound
/// exists so a single wire message cannot materialize millions of
/// buffered allocations before the byte axes notice.
pub const MAX_BATCH_LINES: u64 = MAX_CHUNK_LINES;

// r[impl store.log.write-read-bound+2]
/// The bounded cutter decision: the longest contiguous prefix of
/// `lines` (`(line_number, content_len)` pairs, contiguity on the
/// numbers exactly as [`contiguous_prefix_len`]) whose CHARGED payload
/// — `Σ (charged_line_cost(content_len) + LINE_LEN_PREFIX_BYTES)` —
/// stays at or under `max_payload`.
///
/// Charging [`charged_line_cost`] (not bare content) is bug_298's
/// write-side fix: with bare framing an all-empty run cost 4 bytes per
/// line, so one chunk could legally hold
/// `MAX_CHUNK_PAYLOAD_BYTES / 4 = 4,194,304` lines that the read path
/// then materializes as 4M allocations. Charged, the same run tops out
/// at [`MAX_CHUNK_LINES`] (466,033) — the chunk's line count is
/// bounded by the SAME formula every other byte axis uses.
///
/// Returns at least 1 on non-empty input even if the first line alone
/// exceeds the bound: a single line is always cuttable (the ingest path
/// truncates lines to 64 KiB ≪ the 16 MiB bound — the consuming crate
/// const-asserts that relation), and refusing it would wedge the
/// buffer. Saturating arithmetic keeps the running total defined at the
/// `u64` edge (the prefix simply ends there).
pub fn bounded_contiguous_prefix_len(
    lines: impl Iterator<Item = (u64, u64)>,
    max_payload: u64,
) -> usize {
    let mut iter = lines;
    let Some((mut prev, first_len)) = iter.next() else {
        return 0;
    };
    let mut framed: u64 = charged_line_cost(first_len).saturating_add(LINE_LEN_PREFIX_BYTES);
    let mut len = 1;
    for (n, content_len) in iter {
        if prev.checked_add(1) != Some(n) {
            break;
        }
        let next_framed = framed
            .saturating_add(charged_line_cost(content_len).saturating_add(LINE_LEN_PREFIX_BYTES));
        if next_framed > max_payload {
            break;
        }
        framed = next_framed;
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

    /// Verify [`short_object_policy`] over every sorted 3-row manifest
    /// tail and span (bounded; the function is a left-to-right fold
    /// with a break, so 3 rows exercise stitch, gap, and exhaustion):
    /// the verdict is `ServeClamped` iff a step-through cover check —
    /// written independently as per-line reachability — says every
    /// missing line is held by some row.
    #[kani::proof]
    #[kani::unwind(10)]
    fn check_object_coverage_policy() {
        const B: u64 = 8; // small domain: positions 0..8
        let missing_from: u64 = kani::any();
        let missing_until: u64 = kani::any();
        kani::assume(missing_from < missing_until && missing_until <= B);
        // merged_bug_001: the served-prefix axis, quantified.
        let cursor: u64 = kani::any();
        kani::assume(cursor <= B);
        let rows: [(u64, u64); 3] = kani::any();
        kani::assume(rows.iter().all(|&(f, c)| f <= B && c <= B));
        kani::assume(rows[0].0 <= rows[1].0 && rows[1].0 <= rows[2].0);

        let verdict = short_object_policy(cursor, missing_from, missing_until, &rows);

        // Independent oracle: every missing line NOT already served
        // (below the cursor) individually covered, with rows usable
        // only while the prefix has no gap (the ordered-walk
        // semantics the policy documents).
        let needed_from = if cursor > missing_from {
            cursor
        } else {
            missing_from
        };
        let mut all_covered = true;
        let mut line = needed_from;
        while line < missing_until {
            let mut hit = false;
            let mut frontier_ok = true;
            for &(f, c) in &rows {
                if !frontier_ok {
                    break;
                }
                if f > line && f > needed_from {
                    // A row starting past this line cannot help it,
                    // and ordered input means no later row can either.
                    frontier_ok = f <= line;
                }
                if f <= line && line < f.saturating_add(c) {
                    hit = true;
                    break;
                }
            }
            if !hit {
                all_covered = false;
                break;
            }
            line += 1;
        }
        assert_eq!(
            verdict == ShortObjectPolicy::ServeClamped,
            all_covered,
            "policy iff per-line coverage"
        );
    }

    /// Verify [`visit_fanout_batch`] against its contracts under the
    /// BIGINT precondition: the visit IS the dedup verdict, and the
    /// provenance partition is exact — present iff there is a gap,
    /// `DroppedSpan` iff the gap dips below the coverage floor, with
    /// `dropped_until` clamped to the gap. No loops — exhaustive.
    #[kani::proof_for_contract(visit_fanout_batch)]
    fn check_visit_fanout_batch_contract() {
        let next_line: u64 = kani::any();
        let first_line: u64 = kani::any();
        let n_lines: u64 = kani::any();
        let coverage_floor: u64 = kani::any();
        let _ = visit_fanout_batch(next_line, first_line, n_lines, coverage_floor);
    }

    /// Verify [`final_claim`] against its ensures contract over the
    /// full `(u64, Option<u64>, bool)` domain: complete ⇔ sealed
    /// Some(n) ∧ cursor ≥ n ∧ covers, and the advertised cursor is the
    /// input cursor. No loops, no allocation — exhaustive.
    #[kani::proof_for_contract(final_claim)]
    fn check_final_claim_contract() {
        let cursor_next: u64 = kani::any();
        let sealed: Option<u64> = kani::any();
        let covers: bool = kani::any();
        let _ = final_claim(cursor_next, sealed, covers);
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

    /// Is `x` in the half-open range a [`ChunkVisit`] yielded? A gap
    /// span is NOT served — only the yield range counts, in both
    /// contributing variants (the union/at-most-once equalities below
    /// therefore hold across gap-shaped folds too).
    fn in_visit(x: u64, v: &ChunkVisit) -> bool {
        match *v {
            ChunkVisit::Skip { .. } => false,
            ChunkVisit::Serve {
                yield_from,
                yield_until,
                ..
            }
            | ChunkVisit::GapThenServe {
                yield_from,
                yield_until,
                ..
            } => x >= yield_from && x < yield_until,
        }
    }

    /// [`tail_next`] never exits prematurely: with grace budget
    /// remaining and the served log not complete, the verdict is
    /// Reopen for EVERY retryable cause/terminal combination — the
    /// three premature-exit shapes of merged_bug_076 (Err conflated to
    /// drained, open-failure at terminal, natural end with an
    /// incomplete log) are unrepresentable. `Orphaned` is exempt by
    /// design: with no consumer left there are no lines to lose, so
    /// its exit is not premature (merged_bug_130 — the proof below
    /// pins it as an unconditional exit). `PermanentErr` is exempt the
    /// other way around (merged_bug_164): the store typed the refusal
    /// as forever, so there are no servable lines to wait for and
    /// re-dialing is the 1 Hz wedge. Exhaustive over the 6x2x2 input
    /// domain.
    #[kani::proof]
    fn check_tail_next_no_premature_exit() {
        let cause: u8 = kani::any();
        kani::assume(cause < 6);
        let cause = match cause {
            0 => TailStopCause::NaturalEnd,
            1 => TailStopCause::TransportErr,
            2 => TailStopCause::OpenFailed,
            3 => TailStopCause::GapObserved,
            4 => TailStopCause::Orphaned,
            _ => TailStopCause::PermanentErr,
        };
        let unconditional_exit =
            matches!(cause, TailStopCause::Orphaned | TailStopCause::PermanentErr);
        let terminal: bool = kani::any();
        let served_complete: bool = kani::any();
        // Grace unspent + log not served-complete + a consumer still
        // listening + the store has not said never => never Exit.
        if !served_complete && !unconditional_exit {
            assert!(tail_next(cause, terminal, false, served_complete) == TailNext::Reopen);
        }
        // Grace unspent + not a natural end + retryable => never
        // Exit, even when complete (an erred stream gets its retry).
        if cause != TailStopCause::NaturalEnd && !unconditional_exit {
            assert!(tail_next(cause, terminal, false, served_complete) == TailNext::Reopen);
        }
        // The two unconditional exits ARE unconditional.
        if unconditional_exit {
            assert!(tail_next(cause, terminal, false, served_complete) == TailNext::Exit);
        }
    }

    /// [`tail_next`] always honours the spent grace budget: Exit for
    /// every cause/terminal/completeness combination once
    /// `grace_expired` — the loop is provably finite past the
    /// deadline. And the early-exit cell is exactly
    /// (NaturalEnd && terminal && served_complete) ∨ Orphaned.
    #[kani::proof]
    fn check_tail_next_grace_exit() {
        let cause: u8 = kani::any();
        kani::assume(cause < 6);
        let cause = match cause {
            0 => TailStopCause::NaturalEnd,
            1 => TailStopCause::TransportErr,
            2 => TailStopCause::OpenFailed,
            3 => TailStopCause::GapObserved,
            4 => TailStopCause::Orphaned,
            _ => TailStopCause::PermanentErr,
        };
        let terminal: bool = kani::any();
        let served_complete: bool = kani::any();
        assert!(tail_next(cause, terminal, true, served_complete) == TailNext::Exit);
        // The early-exit cell, exactly.
        let early = tail_next(cause, terminal, false, served_complete) == TailNext::Exit;
        let natural_drained = cause == TailStopCause::NaturalEnd && terminal && served_complete;
        assert!(
            early
                == (natural_drained
                    || cause == TailStopCause::Orphaned
                    || cause == TailStopCause::PermanentErr)
        );
    }

    /// `Orphaned` always exits: with the consumer-side lifecycle
    /// channel gone there is nothing to serve and nothing to lose —
    /// every re-open would be the merged_bug_130 hot-loop (a store
    /// connection per backoff tick, forever, for nobody). Exhaustive
    /// over terminal x grace x completeness.
    #[kani::proof]
    fn check_tail_next_orphan_always_exits() {
        let terminal: bool = kani::any();
        let grace_expired: bool = kani::any();
        let served_complete: bool = kani::any();
        assert!(
            tail_next(
                TailStopCause::Orphaned,
                terminal,
                grace_expired,
                served_complete
            ) == TailNext::Exit
        );
    }

    /// Verify [`visit_object`] against its contracts for every
    /// quadruple under the BIGINT precondition: the visit equals the
    /// dedup verdict over the clamped count, and the divergence
    /// classification is exact. Loop-free, exhaustive over the domain.
    #[kani::proof_for_contract(visit_object)]
    fn check_visit_object_contract() {
        let next_line: u64 = kani::any();
        let first_line: u64 = kani::any();
        let manifest_count: u64 = kani::any();
        let object_count: u64 = kani::any();
        let _ = visit_object(next_line, first_line, manifest_count, object_count);
    }

    /// Composition: an over-length object can NEVER displace a
    /// successor chunk's range. Visiting chunk A through
    /// [`visit_object`] bounds the watermark by A's manifest claim, so
    /// a successor chunk B starting at A's claimed end always serves
    /// its full range — the property the unclamped wiring violated
    /// (advancing by the object count suppressed B's leading lines and
    /// attributed A's excess to B's numbers).
    #[kani::proof]
    fn check_object_clamp_preserves_successor() {
        let since: u64 = kani::any();
        let f_a: u64 = kani::any();
        let mc_a: u64 = kani::any();
        let oc_a: u64 = kani::any();
        let f_b: u64 = kani::any();
        let n_b: u64 = kani::any();
        kani::assume(f_a <= i64::MAX as u64 && mc_a <= i64::MAX as u64);
        kani::assume(oc_a <= i64::MAX as u64);
        kani::assume(f_b <= i64::MAX as u64 && n_b <= i64::MAX as u64);
        // B is the successor row: it starts exactly at A's claimed end
        // (the manifest ORDER BY puts it second), and the reader's
        // cursor began at or before A's claim domain.
        kani::assume(f_b == f_a + mc_a);
        kani::assume(since <= f_b);
        kani::assume(n_b > 0);

        let ov_a = visit_object(since, f_a, mc_a, oc_a);
        let cursor = ov_a.visit.next_line();
        // The watermark after A never exceeds A's claimed end —
        // regardless of how over-length A's object was.
        assert!(cursor <= f_b);
        // Therefore B's visit serves B's entire range: no leading line
        // of B is suppressed by A's excess.
        match visit_chunk(cursor, f_b, n_b) {
            ChunkVisit::Serve {
                yield_from,
                yield_until,
                ..
            }
            | ChunkVisit::GapThenServe {
                yield_from,
                yield_until,
                ..
            } => {
                assert!(yield_from == f_b.max(cursor));
                assert!(yield_until == f_b + n_b);
            }
            ChunkVisit::Skip { .. } => {
                // B is non-empty and starts at or past the cursor, so
                // it can never be skipped.
                assert!(false);
            }
        }
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
        cursor = v_a.next_line();
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
        cursor = v_a.next_line();
        let v_b = visit_chunk(cursor, f_b, n_b);
        cursor = v_b.next_line();
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

    // r[verify store.log.write-read-bound+2]
    /// [`bounded_contiguous_prefix_len`]'s contract over every input of
    /// up to 4 lines with fully symbolic numbers, sizes, and bound:
    /// (1) the result never exceeds the unbounded contiguous prefix;
    /// (2) non-empty input yields ≥ 1, empty yields 0; (3) the framed
    /// payload of the chosen prefix is within `max_payload` UNLESS the
    /// prefix is the single always-cuttable first line; (4) the chosen
    /// prefix is genuinely contiguous.
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_bounded_prefix_contract() {
        const MAX_LINES: usize = 4;
        let all: [(u64, u64); MAX_LINES] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= MAX_LINES);
        let lines = &all[..len];
        let max_payload: u64 = kani::any();

        let bounded = bounded_contiguous_prefix_len(lines.iter().copied(), max_payload);
        let unbounded = contiguous_prefix_len(lines.iter().map(|&(n, _)| n));

        // (1) Never longer than the contiguity alone allows.
        assert!(bounded <= unbounded);
        // (2) Total + non-wedging.
        if len == 0 {
            assert!(bounded == 0);
        } else {
            assert!(bounded >= 1);
        }
        // (3) Within budget — the CHARGED budget (bug_298: every line
        // costs `charged_line_cost` + its frame, so an all-empty run
        // cannot pack `max_payload / LINE_LEN_PREFIX_BYTES` lines) —
        // except the singleton escape.
        if bounded > 1 {
            let mut framed: u64 = 0;
            for &(_, content) in &lines[..bounded] {
                framed = framed.saturating_add(
                    charged_line_cost(content).saturating_add(LINE_LEN_PREFIX_BYTES),
                );
            }
            assert!(framed <= max_payload);
        }
        // (4) Contiguous on the line numbers.
        for w in lines[..bounded].windows(2) {
            assert!(w[0].0.checked_add(1) == Some(w[1].0));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn short_object_policy_coverage_stitching() {
        use ShortObjectPolicy::*;
        // One covering row.
        assert_eq!(short_object_policy(0, 5, 10, &[(5, 5)]), ServeClamped);
        // Stitched from two overlapping rows.
        assert_eq!(
            short_object_policy(0, 5, 10, &[(3, 4), (6, 4)]),
            ServeClamped
        );
        // Covering row starts below the span.
        assert_eq!(short_object_policy(0, 5, 10, &[(0, 12)]), ServeClamped);
        // Gap in the stitch: [7,8) uncovered.
        assert_eq!(
            short_object_policy(0, 5, 10, &[(5, 2), (8, 2)]),
            UnservableHole
        );
        // Nothing at all.
        assert_eq!(short_object_policy(0, 5, 10, &[]), UnservableHole);
        // Row past the span can't help (ordered-walk break).
        assert_eq!(short_object_policy(0, 5, 10, &[(11, 5)]), UnservableHole);
        // Empty rows are no-ops.
        assert_eq!(
            short_object_policy(0, 5, 10, &[(5, 0), (5, 5)]),
            ServeClamped
        );
        // merged_bug_001: the served-prefix axis. The cursor proves
        // [5,12) served; the remaining row covers [12,15).
        assert_eq!(short_object_policy(12, 8, 15, &[(12, 3)]), ServeClamped);
        // Cursor covers the WHOLE span: no coverage rows needed.
        assert_eq!(short_object_policy(15, 8, 15, &[]), ServeClamped);
        // Cursor below the span: no help.
        assert_eq!(short_object_policy(3, 8, 15, &[(12, 3)]), UnservableHole);
    }

    #[test]
    fn fanout_gap_provenance_partition() {
        // No gap: no provenance.
        assert_eq!(visit_fanout_batch(5, 5, 3, 5).provenance, None);
        assert_eq!(visit_fanout_batch(5, 2, 3, 9).provenance, None, "skip");
        // Gap [5,8) entirely at/above floor 5: admitted — nothing to recover.
        assert_eq!(
            visit_fanout_batch(5, 8, 2, 5).provenance,
            Some(GapProvenance::AdmittedHole)
        );
        // Gap [5,8) below floor 8: all accepted — dropped span to 8.
        assert_eq!(
            visit_fanout_batch(5, 8, 2, 8).provenance,
            Some(GapProvenance::DroppedSpan { dropped_until: 8 })
        );
        // Mixed: gap [5,9), floor 7 — recoverable [5,7), admitted [7,9).
        assert_eq!(
            visit_fanout_batch(5, 9, 1, 7).provenance,
            Some(GapProvenance::DroppedSpan { dropped_until: 7 })
        );
        // The visit half is exactly visit_chunk.
        assert_eq!(visit_fanout_batch(5, 8, 2, 7).visit, visit_chunk(5, 8, 2));
    }

    #[test]
    fn cursor_advance_is_verdict_sealed() {
        // The only constructor is ChunkVisit::advance(); the advance
        // carries the post-visit watermark for every variant.
        assert_eq!(visit_chunk(5, 2, 1).advance().to(), 5, "skip holds");
        assert_eq!(visit_chunk(5, 5, 3).advance().to(), 8, "serve moves");
        assert_eq!(visit_chunk(5, 8, 2).advance().to(), 10, "gap-serve moves");
    }

    #[test]
    fn final_claim_law() {
        // complete ⇔ sealed ∧ cursor ≥ sealed ∧ covers
        assert!(!final_claim(10, Some(20), true).complete(), "mid-serve");
        assert!(
            !final_claim(20, Some(20), false).complete(),
            "gapped manifest"
        );
        assert!(!final_claim(20, None, true).complete(), "unsealed");
        assert!(final_claim(20, Some(20), true).complete());
        assert!(
            final_claim(25, Some(20), true).complete(),
            "served past seal"
        );
        assert_eq!(final_claim(7, None, false).cursor_next(), 7);
    }

    use super::*;

    /// One visit step per row: `(cursor, first, n) -> expected
    /// variant`. This is THE shared case table — the dashboard's
    /// `lineCursor.ts` mirror duplicates these vectors verbatim (see
    /// `rio-dashboard/src/lib/lineCursor.ts`); change them together.
    /// The skip rows pin the two skip causes (zero-line chunk; chunk
    /// entirely below the watermark) leaving the watermark untouched;
    /// the gap rows pin the forward-jump split, including a cursor
    /// strictly inside the gap and the BIGINT edge.
    #[test]
    fn visit_chunk_table() {
        use ChunkVisit::*;
        type Case = ((u64, u64, u64), ChunkVisit);
        let cases: &[Case] = &[
            // Fresh cursor, chunk at it: contiguous serve.
            (
                (0, 0, 4),
                Serve {
                    yield_from: 0,
                    yield_until: 4,
                    next_line: 4,
                },
            ),
            // Fresh cursor, chunk strictly ahead: the jump is a gap.
            (
                (0, 10, 4),
                GapThenServe {
                    gap_from: 0,
                    gap_until: 10,
                    yield_from: 10,
                    yield_until: 14,
                    next_line: 14,
                },
            ),
            // Cursor inside the chunk: leading lines deduped, no gap.
            (
                (12, 10, 4),
                Serve {
                    yield_from: 12,
                    yield_until: 14,
                    next_line: 14,
                },
            ),
            // Cursor strictly inside a forward jump: gap is exactly
            // [cursor, first).
            (
                (47, 50, 3),
                GapThenServe {
                    gap_from: 47,
                    gap_until: 50,
                    yield_from: 50,
                    yield_until: 53,
                    next_line: 53,
                },
            ),
            // One-line jump: minimal non-empty gap.
            (
                (9, 10, 1),
                GapThenServe {
                    gap_from: 9,
                    gap_until: 10,
                    yield_from: 10,
                    yield_until: 11,
                    next_line: 11,
                },
            ),
            // Chunk entirely below the watermark: skipped, no advance.
            ((20, 10, 4), Skip { next_line: 20 }),
            // Last line exactly at the watermark boundary: 10..14 with
            // cursor 14 is fully served already.
            ((14, 10, 4), Skip { next_line: 14 }),
            // Zero-line chunk past the cursor must NOT advance the
            // watermark (the doc-comment swallow hazard) — and must
            // NOT report a gap either: it holds no lines to serve
            // after one.
            ((3, 50, 0), Skip { next_line: 3 }),
            // BIGINT-edge chunk reached by a gap: exact arithmetic at
            // the precondition boundary (first + n == i64::MAX as u64
            // is the largest representable end here).
            (
                (0, i64::MAX as u64 - 3, 3),
                GapThenServe {
                    gap_from: 0,
                    gap_until: i64::MAX as u64 - 3,
                    yield_from: i64::MAX as u64 - 3,
                    yield_until: i64::MAX as u64,
                    next_line: i64::MAX as u64,
                },
            ),
            // BIGINT-edge contiguous serve.
            (
                (i64::MAX as u64 - 3, i64::MAX as u64 - 3, 3),
                Serve {
                    yield_from: i64::MAX as u64 - 3,
                    yield_until: i64::MAX as u64,
                    next_line: i64::MAX as u64,
                },
            ),
        ];
        for &((cursor, first, n), expected) in cases {
            let v = visit_chunk(cursor, first, n);
            assert_eq!(v, expected, "visit_chunk({cursor}, {first}, {n})");
            assert_eq!(v.is_empty(), matches!(expected, Skip { .. }));
            assert_eq!(v.next_line(), expected.next_line());
        }
    }

    /// The full tail_next decision table: 6 causes x terminal x
    /// grace_expired x served_complete = 48 rows. Exit cells: every
    /// grace_expired row, every Orphaned row, every PermanentErr row
    /// (recorded red for the variant's introduction: the 8
    /// PermanentErr rows did not exist — the pre-fix law had no
    /// vocabulary for "the store says never", so a typed-permanent
    /// refusal re-dialed at 1 Hz until grace), plus exactly
    /// (NaturalEnd, terminal, !grace_expired, served_complete).
    // r[verify store.log.tail-grace-drain+2]
    #[test]
    fn tail_next_decision_table() {
        use TailNext::*;
        use TailStopCause::*;
        let causes = [
            NaturalEnd,
            TransportErr,
            OpenFailed,
            GapObserved,
            Orphaned,
            PermanentErr,
        ];
        let mut rows = 0;
        for cause in causes {
            for terminal in [false, true] {
                for grace_expired in [false, true] {
                    for served_complete in [false, true] {
                        rows += 1;
                        let got = tail_next(cause, terminal, grace_expired, served_complete);
                        let want = if grace_expired
                            || cause == Orphaned
                            || cause == PermanentErr
                            || (cause == NaturalEnd && terminal && served_complete)
                        {
                            Exit
                        } else {
                            Reopen
                        };
                        assert_eq!(
                            got, want,
                            "tail_next({cause:?}, {terminal}, {grace_expired}, {served_complete})"
                        );
                    }
                }
            }
        }
        assert_eq!(rows, 48, "the R5 composed table: 6 causes x 2^3");
    }

    /// The clamp + classification table: `(cursor, first, manifest,
    /// object) -> (visit-over-min, divergence)`.
    #[test]
    fn visit_object_table() {
        use ChunkVisit::*;
        use ObjectDivergence::*;
        // Exact agreement: no divergence, plain serve.
        let v = visit_object(0, 0, 4, 4);
        assert_eq!(v.divergence, None);
        assert_eq!(
            v.visit,
            Serve {
                yield_from: 0,
                yield_until: 4,
                next_line: 4
            }
        );
        // Over-length object: clamped to the claim, excess named.
        let v = visit_object(0, 0, 10, 15);
        assert_eq!(v.divergence, Some(LongObject { excess: 5 }));
        assert_eq!(
            v.visit,
            Serve {
                yield_from: 0,
                yield_until: 10,
                next_line: 10
            }
        );
        // Short object: served range is what the object holds, the
        // missing span is exactly the claimed remainder.
        let v = visit_object(0, 0, 10, 6);
        assert_eq!(
            v.divergence,
            Some(ShortObject {
                missing_from: 6,
                missing_until: 10
            })
        );
        assert_eq!(
            v.visit,
            Serve {
                yield_from: 0,
                yield_until: 6,
                next_line: 6
            }
        );
        // Divergence is classified even when the dedup skips the chunk
        // entirely (cursor past it).
        let v = visit_object(20, 0, 10, 15);
        assert_eq!(v.divergence, Some(LongObject { excess: 5 }));
        assert_eq!(v.visit, Skip { next_line: 20 });
        // Gap + clamp compose: cursor below a divergent chunk's start.
        let v = visit_object(2, 5, 3, 9);
        assert_eq!(v.divergence, Some(LongObject { excess: 6 }));
        assert_eq!(
            v.visit,
            GapThenServe {
                gap_from: 2,
                gap_until: 5,
                yield_from: 5,
                yield_until: 8,
                next_line: 8
            }
        );
        // Zero-line object against a non-zero claim: everything
        // missing, nothing served.
        let v = visit_object(0, 7, 4, 0);
        assert_eq!(
            v.divergence,
            Some(ShortObject {
                missing_from: 7,
                missing_until: 11
            })
        );
        assert_eq!(v.visit, Skip { next_line: 0 });
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

    // r[verify store.log.write-read-bound+2]
    #[test]
    fn bounded_prefix_len_table() {
        let f = |v: &[(u64, u64)], cap: u64| bounded_contiguous_prefix_len(v.iter().copied(), cap);
        // Empty: 0. Singleton over the cap: still 1 (always cuttable).
        assert_eq!(f(&[], 100), 0);
        assert_eq!(f(&[(0, 1000)], 100), 1);
        // Under the cap end-to-end: equals the unbounded contiguity.
        let run: Vec<(u64, u64)> = (0..5).map(|n| (n, 10)).collect();
        assert_eq!(f(&run, 1000), 5);
        assert_eq!(
            f(&run, 1000),
            contiguous_prefix_len(run.iter().map(|&(n, _)| n))
        );
        // The cap splits a contiguous run: 2 lines of charged 46
        // (10 content + 32 overhead + 4 frame) fit in 92; the third
        // would make 138 > 92.
        assert_eq!(f(&run, 92), 2);
        // A number gap still ends the run before the cap does.
        assert_eq!(f(&[(0, 1), (2, 1), (3, 1)], u64::MAX), 1);
        // Exactly at the cap is in-budget.
        assert_eq!(f(&[(0, 10), (1, 10)], 92), 2);
    }

    // r[verify store.log.write-read-bound+2]
    /// bug_298's write-side red: an all-empty contiguous run against
    /// the production payload bound. Pre-fix (bare `content + 4`
    /// framing) the cutter admitted all 500,000 lines into one chunk
    /// (recorded red: `left: 500000, right: 466033`); charged, the
    /// drain tops out at exactly [`MAX_CHUNK_LINES`] — the read path
    /// can never again be handed a multi-million-line chunk written by
    /// our own cutter.
    #[test]
    fn empty_line_run_bounded_by_charged_cost() {
        let bounded =
            bounded_contiguous_prefix_len((0..500_000u64).map(|n| (n, 0)), MAX_CHUNK_PAYLOAD_BYTES);
        assert_eq!(bounded as u64, MAX_CHUNK_LINES);
        // The const family's arithmetic anchors (the docs cite these
        // numbers; a drive-by constant edit must show up here).
        assert_eq!(MAX_CHUNK_LINES, 466_033);
        assert_eq!(MAX_CHUNK_LINES_ABS, 4_194_304);
        assert_eq!(MAX_BATCH_LINES, MAX_CHUNK_LINES);
        assert_eq!(charged_line_cost(0), PER_LINE_OVERHEAD_BYTES);
        assert_eq!(charged_line_cost(u64::MAX), u64::MAX);
    }

    /// bug_298's admission-axis red: a 4,194,304-empty-line batch (the
    /// pre-fix theoretical chunk maximum) was `Accepted { end: .. }`
    /// before the oversized arm existed (recorded red:
    /// `left: Accepted { end: 4194304 }, right: RejectedOversizedBatch`);
    /// now it is rejected at admission, before any per-line work, and
    /// the stream-open semantics match the other per-batch rejections.
    #[test]
    fn oversized_batch_rejected_at_admission() {
        use AcceptVerdict::*;
        // The exact amplification payload from the finding.
        assert_eq!(
            accept_verdict(0, None, 0, 4_194_304),
            RejectedOversizedBatch
        );
        // Boundary: one chunk's charged capacity is admitted...
        assert_eq!(
            accept_verdict(0, None, 0, MAX_BATCH_LINES),
            Accepted {
                end: MAX_BATCH_LINES
            }
        );
        // ...one more line is not.
        assert_eq!(
            accept_verdict(0, None, 0, MAX_BATCH_LINES + 1),
            RejectedOversizedBatch
        );
        // Check order: overflow still wins (unrepresentable end),
        // oversized wins over the monotone floor and the ceiling.
        assert_eq!(
            accept_verdict(0, None, u64::MAX, u64::MAX),
            RejectedOverflow
        );
        assert_eq!(
            accept_verdict(10, None, 0, MAX_BATCH_LINES + 1),
            RejectedOversizedBatch
        );
        assert_eq!(
            accept_verdict(0, Some(5), 7, MAX_BATCH_LINES + 1),
            RejectedOversizedBatch
        );
    }
}
