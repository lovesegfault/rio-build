// -*- mode: Bluespec; -*-
//
// Model C of the rio-store build-log subsystem: the LogService
// session/chunk/dedup protocol (`rio-store/src/logs/{sessions,chunks,
// ingest,gate,tail,sweep,service}.rs` plus the builder-side at-least-once
// uploader `rio-builder/src/log_upload.rs`), driven by an adversarial
// builder that re-sends, fabricates, overlaps, or abandons.
//
// This is the successor to the retired `logBufferLifecycle.qnt` (model A
// of the deleted in-scheduler log subsystem). The architecture it models
// has no lease composition (LogService never reads the scheduler's leader
// election), no reaps, no tenure pins, and no finalizing write — see
// docs/spec/models/log-invariant-map.md's campaign close-out section for
// which of the predecessor's nine invariants transfer, which are vacuous,
// and why. The acceptance test for this model is that document's
// 22-bug calibration table re-run against the new architecture.
//
// WHAT IS MODELED:
//   - the per-execution build + uploader: monotone line production, the
//     ack-trimmed retransmit buffer, reconnect-and-replay, the drain
//     abandonment, the permanent-rejection discard
//     (`log_upload.rs::UploadTask`);
//   - the open-time binding + completeness gate (`gate.rs::
//     check_append_open`): latest-assignment admission, the complete-log
//     seal, the final-line-count handoff to the session;
//   - the per-stream ingest session (`ingest.rs::IngestSession`): the
//     accept predicate (monotone floor, the per-append completeness
//     ceiling, the straddling-batch truncation), the buffer, the
//     contiguous-prefix chunk cut, the abort that drops the buffer;
//   - the chunk manifest: INSERT-only `(exec, session, first_line,
//     line_count)` rows, written only by a cut of the named session;
//   - the ack: `durable_through_line` deliverable to the builder only
//     after the chunk commit, only while the builder is still connected
//     to the cutting session;
//   - the read path (`tail.rs::{read_manifest_range, read_chunk,
//     LineCursor}`): the `(first_line, session_id)`-ordered walk with the
//     watermark dedup, encoded as the code's fold (not as the set union
//     it is supposed to compute) so `servedSpanExact` checks the fold;
//   - the completeness predicate (`gate.rs::log_is_complete`): terminal
//     status, known count, the contiguous-coverage fold;
//   - the scheduler's terminal stamp (`completion.rs`): the 0 -> NULL
//     final-line-count mapping;
//   - the TTL sweep (`sweep.rs::sweep_expired_logs`): the expired-only
//     guard and the chunks-before-execution-row deletion order, split
//     into two actions so the mid-pass crash window is reachable.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced):
//   - Line content and byte sizes. Lines are integers (worker-assigned
//     line numbers); every gate and every fold in the code is interval
//     algebra over those numbers. The price: the content/provenance harm
//     of a builder fabricating DIFFERENT BYTES at a line number its build
//     legitimately produced is invisible (the served line number is
//     "covered" either way). The blast radius of that harm is the
//     builder's own execution's log, whose content it controls anyway.
//     Same price the predecessor model paid and documented.
//   - The byte/line/chunk-count resource bounds (`store.log.
//     ingest-bounds`), the per-line truncation, the stream-count and
//     byte-budget semaphores, and the line-number-overflow rejection.
//     Resource bounds, not protocol; the overflow arm is unrepresentable
//     in the bounded line domain (a Kani/fuzz obligation).
//   - The S3 object behind a manifest row as separate state. The
//     object-before-manifest write order and the loud
//     missing-object read error are unit-tested code branches; modeling
//     the split adds a state dimension whose only constraint is a
//     restatement of that ordering. A manifest row IS its chunk here.
//   - The ingest-session lease's heartbeat clock (`sessions.rs`). The
//     lease is documented as an admission/routing mechanism, not a
//     mutual-exclusion guarantee: the protocol is correct under session
//     overlap, and the model reaches the overlap directly (a detached
//     session still cutting while its successor also cuts) without the
//     30s staleness clock that bounds how long the overlap lasts.
//   - Token expiry. `openSession` requires the execution to be
//     unexpired: the assignment token's lifetime is far shorter than the
//     30-day log-retention TTL, so a fresh stream open for an expired
//     execution is unreachable in production. Without this assumption
//     the model reaches a state where a swept execution accumulates new
//     manifest rows that no later sweep pass re-finds (the sweep's
//     SELECT is over `drv_executions`, whose row is already gone) — a
//     bounded resource leak, not a safety violation. The narrower
//     instance that survives the assumption — a session opened BEFORE
//     expiry that cuts a chunk after the sweep's manifest DELETE but
//     whose execution row the same pass then deletes — is reachable
//     here and violates nothing; it is the same bounded leak.
//   - The cross-replica TailLog proxy, the live-tail subscription
//     fan-out, and the `since_line` reconnect cursor. Read-side
//     plumbing over the same manifest + buffer state the model does
//     constrain; the history-then-live seam invariant is pinned by
//     `subscribe_during_in_flight_cut_sees_drained_lines` and the
//     `log-service` VM scenario.
//   - The HMAC layer, the token-vs-header derivation match, and the
//     executor-identity comparison. The gate's checks collapse to "is
//     the claimed execution the derivation's latest assignment" for
//     tokens the scheduler actually mints (one token per (exec,
//     executor) pair, minted together); forgery is below the model's
//     trust boundary. The writer identity here IS the execution index.
//
// ENCODING DECISIONS:
//   - `delivered(e)` (the conservation-law ghost) is the contiguous
//     prefix `[0, producedEnd(e))` of lines the build has emitted into
//     its uploader. The builder's retransmit buffer is the derived
//     sub-range `[ackedBelow(e), producedEnd(e))` (empty once the
//     uploader abandons) — the code's buffer is batch-granular and can
//     retain already-durable lines slightly longer; the model's
//     line-granular form is a subset, which only makes the conservation
//     law harder to satisfy.
//   - A chunk is its manifest row `{sess, first, count}`; the lines it
//     physically holds are `[first, first+count)` by construction
//     because the cut drains a contiguous run and records its length —
//     the per-chunk half of span exactness is structural in the code
//     (`ingest.rs::contiguous_prefix_len` + `lines.len()`) and therefore
//     also in the model. The non-structural half — the READ path's
//     ordered-walk-with-watermark over overlapping chunks — is encoded
//     as the code's fold and checked against the set union it is
//     supposed to equal (`servedSpanExact`).
//   - The session's "lines accepted at or after the completeness ceiling
//     was learned" set is derived from `hwAtLearn` (the session's
//     high-water mark at the moment the ceiling was learned) rather than
//     carried as a ghost: accepted line numbers are strictly increasing
//     within a session (the monotone floor), so the post-learn accepts
//     are exactly the accepted lines >= hwAtLearn.
//   - One derivation. Entries for distinct derivations share no state;
//     the cross-execution scenarios (supersession, the foreign-token
//     rejection) need a second EXECUTION of the same derivation, not a
//     second derivation.
//
// The invariants, their witnesses, and the per-regime non-vacuity
// procedures are documented at their definitions below. Measured state
// counts, depths, and wall-clocks live in the introducing commits'
// messages and the CI checks' output transcripts, never in this file.
module logService {
  // --- 1. types ---------------------------------------------------------

  /// `drv_executions.final_line_count` (and, by the column's write
  /// contract, the terminal status that accompanies it): `NoCount`
  /// until the scheduler stamps the lifecycle row terminal with a
  /// known builder-reported count. SQL NULL and "no terminal status
  /// yet" collapse here — see the `dbFinalCount` variable's doc.
  type CountOpt = | NoCount | Count(int)

  /// The completeness ceiling as one ingest session knows it
  /// (`IngestSession::final_line_count`). `NoCeiling` until the session
  /// learns the execution's recorded end (at open for an
  /// already-terminal execution, at a heartbeat refresh for a seal that
  /// lands mid-stream). `hwAtLearn` is the session's high-water mark at
  /// the moment it learned the value: accepted lines >= hwAtLearn were
  /// accepted under the ceiling and `completenessGate` requires them to
  /// be below it; accepted lines < hwAtLearn are the disclosed
  /// pre-refresh residual.
  type CeilingState = | NoCeiling | Ceiling({ value: int, hwAtLearn: int })

  /// One ingest session's stream phase.
  ///   - `Open`: the builder's gRPC stream is connected; batches are
  ///     accepted, chunks are cut, acks are deliverable.
  ///   - `Detached`: the builder is gone (clean half-close, transport
  ///     error, or a server-side abort). No more accepts, no more acks.
  ///     A detached session with a non-empty buffer is the driver's
  ///     final drain still cutting (`AppendDriver::drain`) — or a
  ///     deposed same-execution predecessor still committing chunks
  ///     that overlap its successor's. A detached session whose buffer
  ///     was dropped (the abort path) is inert.
  type Phase = | SessOpen | SessDetached

  /// One `AppendLog` ingest session (`ingest.rs::IngestSession` +
  /// `IngestShared`, with `buffer` and `in_flight` collapsed into one
  /// set — a failed cut restores the staged run exactly, so the
  /// staged-but-uncommitted state is observationally the buffer).
  /// `NoSession` = never opened. Fields:
  ///   - `highWater` — `IngestSession::high_water_line`: the lowest
  ///     acceptable `first_line_number` for the next batch. Raised to
  ///     every accepted batch's (post-truncation) end; never lowered;
  ///     not reset by a cut or by the buffer emptying.
  ///   - `ceiling` — `IngestSession::final_line_count` plus the
  ///     learn-time high-water mark.
  ///   - `buf` — `IngestShared::{buffer, in_flight}`: accepted lines
  ///     not yet in a committed chunk.
  type SessionSlot =
    | NoSession
    | Session({
        phase: Phase,
        highWater: int,
        ceiling: CeilingState,
        buf: Set[int],
      })

  /// One committed log chunk's manifest row (`drv_log_chunks`): the
  /// cutting session, and the contiguous run `[first, first + count)`
  /// the object holds. Write-once; the set of chunks per execution only
  /// grows (ingest) or is emptied wholesale (the TTL sweep).
  type Chunk = { sess: int, first: int, count: int }

  /// The per-build-attempt uploader's terminal state
  /// (`log_upload.rs::DrainStatus`). `Uploading` covers the whole live
  /// lifecycle (streaming, reconnecting, draining); `Abandoned` covers
  /// both the drain-deadline expiry (real, counted, error-logged data
  /// loss) and the permanent store rejection (the execution was
  /// superseded or its log is already complete — the buffer is
  /// discarded and the warn at the rejection site is the disclosure).
  type Uploader = | Uploading | Abandoned

  // --- 2. constants (const = overridable per regime) --------------------

  /// Executions minted for the one modeled derivation. Two are required
  /// for the supersession scenarios: the auth gate's
  /// not-the-latest-assignment rejection, the superseded-but-still-open
  /// session writing to its own (old) execution's log, and the
  /// cross-execution contamination invariant's contended state.
  const MAX_EXECS: int
  /// Ingest sessions per execution. Two are required for the
  /// re-sent-batch-after-ambiguous-disconnect case: a chunk is
  /// committed, the ack is lost, the builder replays the same lines to
  /// a fresh session, and the read path must dedup the overlap.
  const MAX_SESSIONS: int
  /// Worker line numbers range over 0..MAX_LINE-1. Three are required:
  /// an overlap that is not total ([0,2) vs [0,3)), a forward gap
  /// ([0,1) + [2,1)), and a batch that straddles the completeness
  /// ceiling are the shapes the dedup fold, the completeness fold, and
  /// the per-append ceiling are about.
  const MAX_LINE: int
  /// Fault/adversary switches. Each gates one branch of `step`; the
  /// regimes enable them in named combinations.
  ///
  /// The bound builder sends batches that are not its uploader's honest
  /// in-order replay: arbitrary forward gaps, re-sends of already-cut
  /// ranges, and lines at or past the recorded final_line_count. This
  /// is what makes the past-final rejection, the straddling-batch
  /// truncation, and the gap-fragmented manifest reachable.
  const ENABLE_FABRICATION: bool
  /// The builder can disconnect and reopen a fresh session (the
  /// at-least-once re-send shape). Off in the base regime so its state
  /// space stays the single-writer core.
  const ENABLE_RECONNECT: bool
  /// Executions can expire and the TTL sweep can run.
  const ENABLE_SWEEP: bool

  // --- 3. pure helpers --------------------------------------------------

  pure def maxInt(a: int, b: int): int = if (a > b) a else b
  pure def minInt(a: int, b: int): int = if (a < b) a else b

  /// The lowest element of a non-empty set (MAX_LINE for the empty set —
  /// callers guard on non-emptiness).
  pure def setMin(s: Set[int]): int = s.fold(MAX_LINE, (a, b) => minInt(a, b))

  /// The longest contiguous run starting at `from` within `s`
  /// (`ingest.rs::contiguous_prefix_len` applied to the buffer's sorted
  /// order): every line `from..from+k-1` is in `s` and `from+k` is not.
  /// Encoded as a left fold over the ORDERED bounded line domain rather
  /// than a recursion (Quint has no general recursion, and `Set.fold`'s
  /// iteration order is unspecified — every ordered walk in this model
  /// folds over a `range(...)` list).
  pure def runFrom(s: Set[int], from: int): Set[int] =
    range(from, MAX_LINE).foldl(Set(), (acc, n) =>
      if (n == from and s.contains(n)) acc.union(Set(n))
      else if (acc.contains(n - 1) and s.contains(n)) acc.union(Set(n))
      else acc)

  /// The half-open interval `[lo, hi)` as a set ([] when lo >= hi).
  pure def interval(lo: int, hi: int): Set[int] =
    if (lo >= hi) Set() else lo.to(hi - 1)

  /// The lines a chunk's manifest row claims (and, by the cut's
  /// construction, physically holds).
  pure def chunkLines(c: Chunk): Set[int] = interval(c.first, c.first + c.count)

  // --- 4. state variables -----------------------------------------------

  /// Executions minted so far (1-based; execution N is the N-th
  /// assignment attempt for the derivation, and the LATEST attempt is
  /// execution `dispatched`). `assignments` rows are never deleted, so
  /// the auth gate's latest-assignment lookup reads this directly.
  var dispatched: int

  /// GHOST, per execution: one past the highest line the build has
  /// emitted into its uploader — `delivered(e)` is `[0, producedEnd(e))`.
  /// The conservation-law subject. The build emits lines contiguously
  /// from 0 (`log_stream.rs` numbers lines monotonically per execution).
  var producedEnd: int -> int
  /// Per execution: the build process has exited and its completion
  /// report (carrying `final_line_count = producedEnd`) is ready. No
  /// more lines are produced; the uploader's drain deadline is armed.
  var buildFinished: int -> bool
  /// Per execution: `drv_executions.{status, final_line_count}` — `Count`
  /// iff the scheduler has stamped the row terminal with a known count.
  /// A terminal row with an unknown count (a cancellation, the 0->NULL
  /// mapping, a lost completion report) is observationally identical to
  /// a non-terminal row for every guard and every invariant in this
  /// subsystem (`gate.rs::sealed_final_line_count` returns None for
  /// both), so the two are not distinguished.
  var dbFinalCount: int -> CountOpt
  /// Per execution: the `drv_executions` row exists (INSERTed at
  /// dispatch, DELETEd by the TTL sweep's last statement).
  var execRowExists: int -> bool
  /// Per execution: `started_at < now() - retention` — the TTL sweep's
  /// guard. Set by the environment, monotone.
  var expired: int -> bool
  /// Per execution: the current sweep pass has deleted the manifest
  /// rows and may now delete the execution row (the program counter
  /// between `sweep.rs`'s two DELETE statements; a cut can interleave).
  var sweepChunksDone: int -> bool

  /// Per execution: the uploader's lifecycle.
  var uploader: int -> Uploader
  /// Per execution: one past the highest line covered by an ack the
  /// builder has received (`UploadTask::last_acked_line + 1`). Monotone.
  /// The retransmit buffer is `[ackedBelow, producedEnd)` while
  /// `Uploading`.
  var ackedBelow: int -> int
  /// Per execution: one past the highest line the uploader has
  /// transmitted on its CURRENT session (`UploadTask::drive`'s `sent`
  /// index, projected onto line numbers). Reset to `ackedBelow` on every
  /// reconnect — that reset IS the at-least-once retransmit.
  var sentBelow: int -> int

  /// Per (execution, session index): the ingest sessions. Session
  /// indices are mint-ordered (UUIDv7), which is the read path's
  /// same-first-line tiebreak order.
  var sessions: int -> (int -> SessionSlot)
  /// Per execution: how many sessions have ever been opened (the next
  /// session index is `sessionsOpened + 1`).
  var sessionsOpened: int -> int
  /// Per execution: the committed chunk manifest (`drv_log_chunks`).
  var chunks: int -> Set[Chunk]
  /// GHOST, per execution: lines accepted into one of the execution's
  /// sessions from a batch that was NOT the uploader's honest in-order
  /// replay of the build's real output. `noCrossExecContamination`
  /// bounds every chunk's lines by `delivered union fabricated` — the
  /// set of lines the execution's own bound builder ever sent.
  var fabricated: int -> Set[int]

  /// History flags (model-only bookkeeping, never read by an action
  /// guard; each is the discriminator for one expect-violation witness).
  ///   - `supersededOpenRejected`: the auth gate rejected a stream open
  ///     whose claimed execution is no longer the latest assignment.
  ///   - `completeOpenRejected`: the gate rejected a stream open because
  ///     the execution's log is already complete.
  ///   - `pastFinalRejected`: the per-append completeness ceiling
  ///     dropped a whole batch or truncated a straddling one.
  ///   - `nonMonotoneRejected`: the monotone floor dropped a batch
  ///     numbered below the session's high-water mark.
  ///   - `sweptChunks`: the TTL sweep deleted a non-empty manifest.
  var wit: {
    supersededOpenRejected: bool,
    completeOpenRejected: bool,
    pastFinalRejected: bool,
    nonMonotoneRejected: bool,
    sweptChunks: bool,
  }

  // --- 5. derived state -------------------------------------------------

  pure val EXECS = 1.to(MAX_EXECS)
  pure val SESSION_IDS = 1.to(MAX_SESSIONS)

  /// The conservation-law subject: the lines execution `e`'s build has
  /// emitted into its uploader.
  def delivered(e: int): Set[int] = interval(0, producedEnd.get(e))

  /// The builder's retransmit buffer: every emitted line not yet covered
  /// by a received ack. Emptied by abandonment (the deadline expiry or
  /// the permanent rejection — `UploadTask::reject_permanently` clears
  /// the deque, and an exited task's heap is gone either way).
  def builderBuf(e: int): Set[int] =
    if (uploader.get(e) == Abandoned) Set()
    else interval(ackedBelow.get(e), producedEnd.get(e))

  // Session accessors. Quint's effect system requires every branch of a
  // match used in action position to assign the same variables, so the
  // actions read sessions through these (which return safe defaults for
  // `NoSession`) and guard on `hasSession` exactly where the code guards
  // on the registry lookup.
  def hasSession(e: int, s: int): bool = sessions.get(e).get(s) != NoSession
  def sessPhase(e: int, s: int): Phase =
    match sessions.get(e).get(s) { | NoSession => SessDetached | Session(x) => x.phase }
  def sessHw(e: int, s: int): int =
    match sessions.get(e).get(s) { | NoSession => 0 | Session(x) => x.highWater }
  def sessCeiling(e: int, s: int): CeilingState =
    match sessions.get(e).get(s) { | NoSession => NoCeiling | Session(x) => x.ceiling }
  def sessBuf(e: int, s: int): Set[int] =
    match sessions.get(e).get(s) { | NoSession => Set() | Session(x) => x.buf }
  def sessIsOpen(e: int, s: int): bool = hasSession(e, s) and sessPhase(e, s) == SessOpen

  /// The execution's unique Open session index, or 0 if none. The
  /// builder holds one `AppendLog` stream at a time, so at most one
  /// session per execution is ever Open (asserted by `boundsOK`).
  def openSessionOf(e: int): int =
    SESSION_IDS.fold(0, (acc, s) => if (sessIsOpen(e, s)) s else acc)

  /// The chunks cut by session `s` of execution `e`.
  def chunksOf(e: int, s: int): Set[Chunk] = chunks.get(e).filter(c => c.sess == s)

  /// Every line accepted into session `s` of `e` that is still
  /// observable: buffered or committed. A line accepted and then dropped
  /// by an abort before it was cut is gone from this set — and from
  /// every harm an accepted line can do.
  def sessAccepted(e: int, s: int): Set[int] =
    sessBuf(e, s).union(chunksOf(e, s).fold(Set(), (acc, c) => acc.union(chunkLines(c))))

  /// Is line `n` covered by some manifest chunk of `e`?
  def manifestHolds(e: int, n: int): bool =
    chunks.get(e).exists(c => c.first <= n and n < c.first + c.count)

  /// Is line `n` in some session's not-yet-durable ingest buffer for `e`?
  def storeBufHolds(e: int, n: int): bool =
    SESSION_IDS.exists(s => sessBuf(e, s).contains(n))

  // -- The completeness predicate (`gate.rs::log_is_complete`) ---------

  /// `gate.rs::manifest_covers_contiguously`: does the manifest, walked
  /// in ascending `first_line` order, cover a contiguous `[0, up_to)`?
  /// Overlap extends coverage (`covered = max(covered, first + count)`);
  /// a chunk starting past the covered-through point is a gap. The walk
  /// order is materialized by folding over the bounded `first_line`
  /// domain; same-first chunks fold through `max` and are
  /// order-insensitive, exactly as the SQL `ORDER BY first_line` (no
  /// session tiebreak) leaves them.
  def manifestCoversUpTo(e: int, upTo: int): bool =
    val covered = range(0, MAX_LINE).foldl(0, (cov, f) =>
      chunks.get(e).filter(c => c.first == f).fold(cov, (cov2, c) =>
        // The early-return-on-gap is encoded by freezing `cov` once a
        // gap is found: a negative sentinel never recovers. The inner
        // fold over same-first chunks is a set fold in arbitrary order;
        // it is order-insensitive because every same-first chunk either
        // trips the same gap test or feeds the same max.
        if (cov2 < 0 or c.first > cov2) -1
        else maxInt(cov2, c.first + c.count)))
    covered >= upTo

  /// `gate.rs::log_is_complete`: terminal with a known count, and the
  /// manifest contiguously covers `[0, count)`. A missing
  /// `drv_executions` row (never dispatched, or swept) means the
  /// execution cannot be judged complete. Computed at read time from
  /// these inputs on every call — never latched.
  def logIsComplete(e: int): bool = and {
    execRowExists.get(e),
    match dbFinalCount.get(e) {
      | NoCount => false
      | Count(n) => manifestCoversUpTo(e, n)
    },
  }

  // -- The read path (`tail.rs`) ---------------------------------------

  /// One step of the `LineCursor` walk: visit chunk `c` with the
  /// watermark at `cursor`, returning the post-visit watermark and the
  /// lines this chunk contributed (`tail.rs::read_chunk`). A chunk
  /// whose last line is below the watermark is skipped without
  /// advancing it; otherwise the chunk yields `[max(first, cursor),
  /// first + count)` and the watermark lands one past its last line.
  pure def visitChunk(cursor: int, c: Chunk): { cursor: int, yielded: Set[int] } =
    if (c.count == 0 or c.first + c.count - 1 < cursor)
      { cursor: cursor, yielded: Set() }
    else
      { cursor: c.first + c.count, yielded: interval(maxInt(c.first, cursor), c.first + c.count) }

  /// The full `TailLog` history read from `since_line = 0`: every chunk
  /// of `e` visited in `(first_line, session_id)` order
  /// (`tail.rs::read_manifest_range`'s ORDER BY), each contributing its
  /// lines above the shared watermark. `count` accumulates the total
  /// number of (line, chunk) yield events so `servedSpanExact` can
  /// assert each line was yielded at most once — the half of the dedup
  /// a set-valued result cannot express.
  def readTail(e: int): { served: Set[int], count: int } =
    val r = range(0, MAX_LINE).foldl({ cursor: 0, served: Set(), count: 0 }, (acc, f) =>
      range(1, MAX_SESSIONS + 1).foldl(acc, (acc2, s) =>
        // At most one chunk per (first_line, session): a session's
        // chunks are disjoint runs, so two of them cannot share a
        // first_line and the inner set fold visits at most one element.
        chunks.get(e).filter(c => c.first == f and c.sess == s).fold(acc2, (acc3, c) =>
          val v = visitChunk(acc3.cursor, c)
          { cursor: v.cursor,
            served: acc3.served.union(v.yielded),
            count: acc3.count + v.yielded.size() })))
    { served: r.served, count: r.count }

  /// What the read path is SUPPOSED to serve: the union of every
  /// manifest chunk's line range (each line once). The independent
  /// truth `servedSpanExact` compares the fold against.
  def manifestUnion(e: int): Set[int] =
    chunks.get(e).fold(Set(), (acc, c) => acc.union(chunkLines(c)))

  // -- The accept predicate (`ingest.rs::IngestSession::accept`) -------

  /// The verdict for a non-empty batch `[lo, hi)` against session
  /// state `(hw, ceiling)`, in the code's check order:
  ///   1. `first_line_number < high_water_line` -> RejectedNonMonotone
  ///      (the whole batch, even the lines above the high water);
  ///   2. ceiling known and `first_line_number >= ceiling` ->
  ///      RejectedPastFinal (the whole batch);
  ///   3. ceiling known and `end > ceiling` -> accepted truncated to
  ///      `[lo, ceiling)`;
  ///   4. otherwise accepted whole.
  /// The overflow arm (check 0 in the code) is unrepresentable in the
  /// bounded line domain. The accepted batch raises the high water to
  /// its (post-truncation) end.
  type Verdict = | RejectNonMonotone | RejectPastFinal | Accept(int)
  pure def acceptVerdict(hw: int, ceiling: CeilingState, lo: int, hi: int): Verdict =
    if (lo < hw) RejectNonMonotone
    else
      match ceiling {
        | NoCeiling => Accept(hi)
        | Ceiling(c) =>
          if (lo >= c.value) RejectPastFinal
          else Accept(minInt(hi, c.value))
      }

}
