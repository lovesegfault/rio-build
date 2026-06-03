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
//   - the TTL sweep (`sweep.rs::sweep_expired_logs`): the v2
//     eligibility (`rio_retry_kernel::exec_row_sweep_eligible` —
//     terminal AND attempt-ledger-unreferenced AND aged out; the
//     fourth kernel conjunct, no-active-assignment, is scheduler-side
//     state outside this model and its omission only WIDENS the
//     modeled sweep, so every safety verdict transfers) and the
//     row left to the SCHEDULER's `gcExecRow` reclaim — the bughunt
//     wave's ownership split (`store.log.sweep-ownership`);
//   - the execution-kind population (bughunt-wave B2): a
//     materialization attempt of the same derivation can take the
//     latest-assignment slot, and the open gate's kind conjunct
//     (`gate.rs`'s `attempt_kind` read) admits build executions only;
//   - the uploader's loss-disclosure accounting (`log_upload.rs::
//     disclose`, the single chokepoint every abandoned line routes
//     through): an abandonment either counted its real loss or had
//     none to count.
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

  /// The execution's `drv_executions.attempt_kind` (bughunt-wave B2):
  /// the open gate admits `KBuild` only — a materialization execution
  /// never holds an AppendLog stream, and its taking the
  /// latest-assignment slot must not revoke a build execution's
  /// replay (merged_bug_101's displacement class, the kind half).
  type Kind = | KBuild | KMat

  /// The per-build-attempt uploader's terminal state
  /// (`log_upload.rs::DrainStatus` x the `disclose()` chokepoint).
  /// `Uploading` covers the whole live lifecycle (streaming,
  /// reconnecting, draining). `Abandoned({ counted })` covers the
  /// drain-deadline expiry, the permanent store rejection, and the
  /// panic guard — every arm routes through `disclose()`, which
  /// increments `rio_builder_log_drain_abandoned_total` iff real lines
  /// were lost; `counted: false` therefore CLAIMS the abandonment had
  /// no un-stored lines (`ingestLossCounted` checks that claim; the
  /// pre-fix rejected:true suppression that skipped the counter is
  /// `CALIB_LOSS_ESCAPE`).
  type Uploader = | Uploading | Abandoned({ counted: bool })

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

  /// The per-execution accepted-line cap — the model-side collapse of
  /// the durable byte + chunk caps (`store.log.caps-durable`; one line
  /// = one accounted unit). Bound it ABOVE the reachable accept volume
  /// to leave a regime's exploration untouched, or AT `MAX_LINE` to
  /// exercise the boundary (the resend regime does — that is where
  /// reconnects could historically reset the account).
  const CAP: int

  /// CALIBRATION ONLY (`logServiceCalibSessionCaps`): re-encode the
  /// pre-fix per-SESSION cap accounting — every `openSession` resets
  /// the accepted account to zero, exactly merged_bug_207's reconnect
  /// hole. Production regimes bind false; the calibration check pins
  /// that `acceptedWithinCap` falsifies under it.
  const CALIB_SESSION_SCOPED_CAPS: bool

  /// CALIBRATION ONLY (`logServiceCalibGateRow`): drop the
  /// assignment-row conjunct from the open gate — the pre-fix shape
  /// where admission never consulted the claimed exec's own row.
  /// Production regimes bind false; the calibration check pins that
  /// `authGateExcludesUnassignedWriters` falsifies under it.
  const CALIB_GATE_IGNORES_ROW: bool
  /// Executions can expire and the TTL sweep can run.
  const ENABLE_SWEEP: bool

  /// A materialization attempt can be dispatched into the
  /// latest-assignment slot (`dispatchMaterialization`). Off in the
  /// regimes that predate the kind axis — their state spaces carry
  /// `execKind` as a constant-`KBuild` map and are unchanged.
  const ENABLE_MAT_DISPATCH: bool
  /// Upper bound on BUILD-kind dispatches (`dispatch`). The regimes
  /// that predate the kind axis bind it to MAX_EXECS (no behavior
  /// change); the kind-mix regime binds 1 so its second slot is
  /// exclusively the materialization attempt — without the bound the
  /// regime explores the two-build-execs x two-sessions cross product
  /// (the redispatch and resend subjects multiplied; measured past
  /// 300M distinct states), and the displacement scenario needs
  /// exactly one build.
  const MAX_BUILD_EXECS: int

  /// CALIBRATION ONLY (`logServiceCalibLatestOnly`): re-encode the
  /// pre-fix latest-assignment-only open gate — admission requires
  /// `e == dispatched`, so any later dispatch (a re-attempt or a
  /// materialization mint) silently revokes a terminal-but-incomplete
  /// build's late replay (merged_bug_101's displacement hole).
  /// Production regimes bind false; the calibration check pins that
  /// `buildTailAdmittable` falsifies under it.
  const CALIB_GATE_LATEST_ONLY: bool

  /// CALIBRATION ONLY (`logServiceCalibLossEscape`): the pre-fix
  /// rejected:true suppression — the permanent-rejection abandonment
  /// discards the retransmit buffer WITHOUT routing through the
  /// disclosure counter (merged_bug_360's PERMISSION_DENIED hole).
  /// Production regimes bind false; the calibration check pins that
  /// `ingestLossCounted` falsifies under it.
  const CALIB_LOSS_ESCAPE: bool

  /// CALIBRATION ONLY (`logServiceCalibSweepAgeOnly`): the pre-fix
  /// age-only sweep SELECT — eligibility ignores the terminal and
  /// ledger-reference conjuncts (merged_bug_086). Production regimes
  /// bind false; the calibration check pins that
  /// `sweepOnlyTerminalUnreferenced` falsifies under it.
  const CALIB_SWEEP_AGE_ONLY: bool

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

  /// Has the scheduler's ON-CONFLICT in-place rewrite erased this
  /// (non-latest) execution's assignment row? The v2 write-authority
  /// gate keys on the claimed exec's OWN row — supersession by a newer
  /// attempt no longer revokes it (merged_bug_101), but a rewritten
  /// row does, permanently.
  var assignmentRowLost: int -> bool

  /// Ghost: lines accepted for the execution over its LIFETIME (all
  /// sessions; never reset on reconnect — that is the
  /// `store.log.caps-durable` fix). The accept guard charges it; the
  /// open gate refuses an at-cap execution.
  var acceptedTotal: int -> int
  /// Per execution: the store's TTL pass has stripped this execution
  /// (chunks + session registry). Mirrors the SELECT's EXISTS
  /// disjunction keeping a stripped execution out of later passes.
  var sweepChunksDone: int -> bool

  /// Per execution: `drv_executions.attempt_kind`, stamped at dispatch
  /// and immutable (the dispatch INSERT writes it once).
  var execKind: int -> Kind
  /// Per execution: the scheduler's attempt ledger still references
  /// this execution (`drv_attempts` rows whose exec joins here — the
  /// retry kernel's decision suffix). Set at dispatch (the attempt
  /// INSERT), released by the environment when the ledger GC's own
  /// per-lane cut moves past it (`ledgerReleases`); the v2 sweep
  /// refuses to delete a referenced row. Frozen once the row is gone
  /// (a post-deletion release changes nothing this subsystem reads),
  /// which is what makes `sweepOnlyTerminalUnreferenced` checkable
  /// from post-states.
  var ledgerReferenced: int -> bool

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
  ///   - `matThenBuildReplayAdmitted`: a build execution's reopen was
  ///     admitted while a LATER materialization attempt held the
  ///     latest-assignment slot — the replay the pre-fix latest-only
  ///     gate displaced.
  var wit: {
    supersededOpenRejected: bool,
    completeOpenRejected: bool,
    pastFinalRejected: bool,
    nonMonotoneRejected: bool,
    sweptChunks: bool,
    matThenBuildReplayAdmitted: bool,
  }

  // --- 5. derived state -------------------------------------------------

  pure val EXECS = 1.to(MAX_EXECS)
  pure val SESSION_IDS = 1.to(MAX_SESSIONS)

  /// The conservation-law subject: the lines execution `e`'s build has
  /// emitted into its uploader.
  def delivered(e: int): Set[int] = interval(0, producedEnd.get(e))

  /// The uploader has reached a terminal state (any abandonment arm).
  def uploaderAbandoned(e: int): bool = uploader.get(e) != Uploading

  /// The uploader's abandonment routed through the disclosure counter
  /// (`disclose()` with `lost > 0`). False while uploading.
  def uploaderLossCounted(e: int): bool =
    match uploader.get(e) { | Uploading => false | Abandoned(a) => a.counted }

  /// The builder's retransmit buffer: every emitted line not yet covered
  /// by a received ack. Emptied by abandonment (the deadline expiry or
  /// the permanent rejection — `UploadTask::reject_permanently` clears
  /// the deque, and an exited task's heap is gone either way).
  def builderBuf(e: int): Set[int] =
    if (uploaderAbandoned(e)) Set()
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

  /// `rio_retry_kernel::exec_row_sweep_eligible`, projected onto this
  /// model's state (bughunt-wave B2, merged_bug_086): the sweep pass
  /// may target an execution only if its lifecycle row is TERMINAL
  /// (the model's collapsed encoding: a stamped count — see the
  /// `dbFinalCount` doc; a terminal row with an unknown count is
  /// indistinguishable from a non-terminal one here, which only makes
  /// the modeled guard stricter), the attempt ledger no longer
  /// references it, and the retention clock has aged it out. The
  /// kernel's fourth conjunct (no active assignment) is scheduler-side
  /// state this model does not carry; omitting it only widens the
  /// modeled reclaim. Guards `gcExecRow` (the scheduler's row delete);
  /// the store's chunk/session pass is age-only BY DESIGN — it never
  /// touches the row.
  def execRowSweepEligible(e: int): bool = and {
    expired.get(e),
    // CALIBRATION: the pre-fix SELECT was age-only.
    CALIB_SWEEP_AGE_ONLY or and {
      dbFinalCount.get(e) != NoCount,
      not(ledgerReferenced.get(e)),
    },
  }

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

  // --- 7. invariants ----------------------------------------------------
  //
  // The three surviving invariants of the predecessor model
  // (`noCrossExecContamination`, the binding/auth gate, `noSilentLineLoss`)
  // restated over the new state, `lineSpanExact` restated over the
  // manifest + dedup read path as `servedSpanExact`, the completeness
  // gate, and the load-bearing ack-ordering refinement of the
  // conservation law. Each carries the spec rule it verifies; the
  // r[verify ...] markers live at the check wiring points in
  // nix/quint.nix. Defined before the actions (Quint definitions are
  // order-independent).

  /// The runtime residual of TypeOK: the integer ranges the ceiling
  /// preconditions are supposed to enforce, plus the structural facts
  /// the other invariants' encodings rely on (at most one Open session
  /// per execution; session buffers and high-water marks within the
  /// line domain). The acked watermark is deliberately NOT asserted to
  /// trail the transmit cursor: an ack for a fabricated high-numbered
  /// chunk legitimately overtakes the honest replay's cursor (see
  /// `ackImpliesDurable`).
  val boundsOK = and {
    dispatched >= 0 and dispatched <= MAX_EXECS,
    EXECS.forall(e => producedEnd.get(e) >= 0 and producedEnd.get(e) <= MAX_LINE),
    EXECS.forall(e => ackedBelow.get(e) >= 0 and ackedBelow.get(e) <= MAX_LINE),
    EXECS.forall(e => sessionsOpened.get(e) >= 0 and sessionsOpened.get(e) <= MAX_SESSIONS),
    EXECS.forall(e => SESSION_IDS.forall(s =>
      hasSession(e, s) implies and {
        s <= sessionsOpened.get(e),
        sessHw(e, s) >= 0 and sessHw(e, s) <= MAX_LINE,
        sessBuf(e, s).forall(n => n >= 0 and n < MAX_LINE),
      })),
    // At most one Open session per execution: the builder holds one
    // AppendLog stream at a time, and `openSession` requires the
    // previous session to be detached first.
    EXECS.forall(e => SESSION_IDS.forall(s1 => SESSION_IDS.forall(s2 =>
      (sessIsOpen(e, s1) and sessIsOpen(e, s2)) implies s1 == s2))),
    // An execution's lifecycle row exists iff it was dispatched and the
    // sweep has not deleted it.
    EXECS.forall(e => execRowExists.get(e) implies e <= dispatched),
  }

  /// `NoCrossExecContamination` (obs.log.exec-keyed+2,
  /// store.log.append-auth's downstream half): execution E's manifest
  /// holds only lines E's own bound builder sent — its build's real
  /// output plus its own fabrications — never another execution's. The
  /// defense is structural (the chunk key embeds the exec_id the stream
  /// was bound to at open, and a re-dispatch mints a new exec_id and a
  /// new key namespace); this invariant is the model-side regression
  /// guard that the cut action keys its chunk by the cutting session's
  /// execution and nothing else. Its calibration value is the
  /// non-vacuity of the quantifier — the contended state (two
  /// executions of one derivation with concurrently growing logs, the
  /// superseded one still being written by its not-yet-rejected
  /// session) is pinned reachable by `noConcurrentExecWriters`.
  val noCrossExecContamination = EXECS.forall(e =>
    chunks.get(e).forall(c =>
      chunkLines(c).subseteq(delivered(e).union(fabricated.get(e)))))

  /// `BindingGateExcludesUnassignedWriters` (store.log.append-auth+2,
  /// the v2 claimed-exec re-keying): the stream-open predicate admits
  /// a claimed execution only while its OWN assignment row exists — a
  /// rewritten-in-place attempt is permanently excluded. Supersession
  /// alone no longer excludes (merged_bug_101: the builder's own
  /// superseded execution stays writable; containment is exec-keyed
  /// chunks + the durable caps + the ceiling — see
  /// noCrossExecContamination). The invariant falsifies if the
  /// row conjunct is dropped from `openAdmitted`
  /// (`logServiceCalibGateRow` pins exactly that). The
  /// presenter-identity, derivation-match, and kind halves of the gate
  /// are equality checks on signed/scheduler-stamped fields, outside
  /// the model (see the header's scope notes).
  val authGateExcludesUnassignedWriters = EXECS.forall(e =>
    openAdmitted(e) implies not(assignmentRowLost.get(e)))

  /// `store.log.caps-durable`: the durable accepted footprint — every
  /// line the store is holding or has committed for the execution,
  /// across ALL its sessions — never exceeds the per-execution cap.
  /// Measured against the durable state (chunks + buffers), not the
  /// accounting ghost, so the pre-fix per-session encoding (which
  /// resets the GHOST on reconnect; `logServiceCalibSessionCaps`)
  /// falsifies it: a reconnect-reset account admits and accepts past
  /// the cap, and the over-cap lines land in chunks.
  val acceptedWithinCap = EXECS.forall(e =>
    size(
      chunks.get(e).fold(Set(), (acc, c) => acc.union(chunkLines(c)))
        .union(SESSION_IDS.fold(Set(), (acc, s) => acc.union(sessBuf(e, s))))
        .union(fabricated.get(e))
    ) <= CAP)

  /// Witness (negated): some execution actually REACHES the cap
  /// boundary — the cap conjuncts are exercised, not vacuous.
  val noCapBoundary = EXECS.forall(e => acceptedTotal.get(e) < CAP)

  /// `store.log.append-auth+2`'s kind half + the displacement fix
  /// (bughunt-wave B2 item 8, merged_bug_101's kind leg): a
  /// materialization execution is NEVER admitted to AppendLog, and a
  /// build execution whose own row is intact, whose log is incomplete
  /// and unexpired, and whose durable account has headroom IS admitted
  /// — even while a later attempt (in particular a materialization
  /// mint) holds the latest-assignment slot. In the production gate
  /// the second conjunct is the gate's own definition (the calibration
  /// gives it teeth: `CALIB_GATE_LATEST_ONLY` re-adds the pre-fix
  /// `e == dispatched` conjunct and a mat dispatch then falsifies it).
  val buildTailAdmittable = EXECS.forall(e => and {
    execKind.get(e) == KMat implies not(openAdmitted(e)),
    (and {
      e <= dispatched,
      execKind.get(e) == KBuild,
      not(assignmentRowLost.get(e)),
      not(logIsComplete(e)),
      not(expired.get(e)),
      acceptedTotal.get(e) < CAP,
    }) implies openAdmitted(e),
  })

  /// `builder.log.loss-disclosure` at the ingest plane (bughunt-wave
  /// B2 item 8, merged_bug_360): an abandonment that did NOT route a
  /// loss through the disclosure counter must have had nothing real to
  /// lose — every line its build produced is store-held (buffered or
  /// committed). Deliberately WITHOUT `noSilentLineLoss`'s
  /// not-complete and abandoned escapes: the pre-fix rejected:true
  /// suppression (`CALIB_LOSS_ESCAPE`) satisfied those and still
  /// vanished real lines silently — exactly the hole this plane pins.
  val ingestLossCounted = EXECS.forall(e =>
    match uploader.get(e) {
      | Uploading => true
      | Abandoned(a) =>
        not(a.counted) implies
          delivered(e).forall(n => storeBufHolds(e, n) or manifestHolds(e, n))
    })

  /// `store.log.sweep-ownership` (bughunt-wave B2 item 8,
  /// merged_bug_086): a deleted lifecycle row implies the execution
  /// was sweep-eligible — terminal, attempt-ledger-unreferenced, and
  /// aged out. Sound from post-states because all three facts are
  /// frozen or monotone once the row is gone (`recordFinalLineCount`
  /// and `ledgerReleases` both require the row; `expired` never
  /// clears). The age-only calibration (`CALIB_SWEEP_AGE_ONLY`)
  /// falsifies it by sweeping a never-terminal or still-referenced
  /// execution.
  val sweepOnlyTerminalUnreferenced = EXECS.forall(e =>
    (e <= dispatched and not(execRowExists.get(e))) implies and {
      dbFinalCount.get(e) != NoCount,
      not(ledgerReferenced.get(e)),
      expired.get(e),
    })

  /// `NoSilentLineLoss` (the conservation law), the four-disjunct
  /// disclosed-loss form from log-invariant-map.md's campaign close-out:
  /// every line the build emitted into its uploader is, at all times,
  ///   (1) in the builder's ack-trimmed retransmit buffer (it will be
  ///       replayed to the next session), or
  ///   (2) in a store-side ingest buffer (accepted, awaiting a cut), or
  ///   (3) covered by a manifest chunk (durable and servable), or
  ///   (4) the loss is disclosed: the log does not read as complete
  ///       (`is_complete = false` is the reader-facing incomplete
  ///       indicator, and a swept execution's missing lifecycle row
  ///       reads as incomplete too), or the uploader recorded its
  ///       abandonment (the `rio_builder_log_drain_abandoned_total`
  ///       counter / the permanent-rejection warn).
  /// The disjuncts collapse: completeness REQUIRES the manifest to
  /// contiguously cover `[0, final_line_count)` and the count is the
  /// build's own line total, so the bite is concentrated at "a log that
  /// reads complete while its uploader has not given up" — where every
  /// produced line must already be durable. The load-bearing refinement
  /// that does not route through the completeness definition is
  /// `ackImpliesDurable` below.
  val noSilentLineLoss = EXECS.forall(e =>
    delivered(e).forall(n => or {
      builderBuf(e).contains(n),
      storeBufHolds(e, n),
      manifestHolds(e, n),
      not(logIsComplete(e)),
      uploaderAbandoned(e),
    }))

  /// The conservation law's load-bearing refinement FOR THE HONEST
  /// UPLOADER: a line leaves the retransmit buffer only when an ack
  /// covers it, an ack is sent only after the chunk's manifest INSERT
  /// commits, and the honest uploader's batches are contiguous from its
  /// acked watermark — so every line below that watermark is durably in
  /// the manifest until the TTL sweep (the one sanctioned deleter)
  /// removes it. This is the invariant that falsifies if an ack is ever
  /// emitted before the manifest commit, or if anything other than the
  /// sweep deletes a manifest row (an early ack lets the builder trim a
  /// line that a store-side abort then silently drops, with no replay
  /// and no disclosure beyond the log never completing).
  ///
  /// NOT asserted in the fabrication-enabled regimes, by design: the
  /// acked watermark is a max over committed chunk ENDS
  /// (`log_upload.rs::trim` sets `last_acked_line =
  /// max(durable_through_line)` and pops every frame whose last line
  /// the ack covers), so a client that violates the LogBatcher's
  /// monotone-numbering contract — sending a high-numbered fabricated
  /// batch before its real low-numbered lines — gets an ack for the
  /// fabricated chunk that pops the real lines' frames from its own
  /// retransmit buffer without their ever being stored. The model
  /// reaches that state (fabricate [k, k+1), cut, ack -> ackedBelow =
  /// k+1 with [0, k) never durable) and the code does too. The loss is
  /// confined to the contract-violating builder's own log and is
  /// disclosed by the log never reading complete (`noSilentLineLoss`'s
  /// fourth disjunct holds over the same trace in every regime); the
  /// per-ack durability claim is simply not a property the code offers
  /// to a client that mis-numbers its own lines.
  val ackImpliesDurable = EXECS.forall(e =>
    not(expired.get(e)) implies
      interval(0, ackedBelow.get(e)).forall(n => manifestHolds(e, n)))

  /// `ServedSpanExact` (store.log.session-keyed's read half; the
  /// successor of the predecessor's `lineSpanExact`): the read path's
  /// ordered-walk-with-watermark over the manifest — including
  /// overlapping chunks from concurrent or successive sessions — yields
  /// exactly the union of the chunks' line ranges, each line exactly
  /// once. `served == union` is the no-loss half (a wrong visit order
  /// strands a low-numbered chunk behind an already-advanced watermark
  /// and silently drops its lines); `count == size` is the no-duplicate
  /// half (the same line yielded from two overlapping chunks would
  /// double-print in the client's stream and corrupt its reconnect
  /// cursor).
  val servedSpanExact = EXECS.forall(e =>
    val r = readTail(e)
    and {
      r.served == manifestUnion(e),
      r.count == r.served.size(),
    })

  /// The user-facing restatement of the conservation law's bite: a log
  /// that reads COMPLETE serves every line the build produced. Implied
  /// by `servedSpanExact` + the completeness fold's definition + the
  /// honest terminal stamp; stated independently so an encoding bug in
  /// any one of those three is caught by the others' conjunction.
  val completeLogServesAllProduced = EXECS.forall(e =>
    logIsComplete(e) implies delivered(e).subseteq(readTail(e).served))

  /// `CompletenessGate` (store.log.completeness-gate's per-append half):
  /// once an ingest session has learned the execution's recorded
  /// `final_line_count`, every line it subsequently accepts is below
  /// that count — a batch starting at or past the ceiling is dropped
  /// whole, a straddling batch is truncated. Lines accepted BEFORE the
  /// session learned the ceiling (the seal landed mid-stream and the
  /// heartbeat refresh had not yet observed it) are the disclosed
  /// bounded residual: they are over-coverage the completeness fold
  /// already tolerates, not silent loss, and they are excluded here by
  /// the `>= hwAtLearn` filter (accepted line numbers are strictly
  /// increasing within a session, so "accepted after the learn" is
  /// exactly "numbered at or above the learn-time high water"). The
  /// open-time half of the rule (a COMPLETE log rejects the open
  /// outright) is `openAdmitted`'s `not(logIsComplete)` conjunct,
  /// witnessed by `noCompleteOpenRejection`.
  val completenessGate = EXECS.forall(e =>
    SESSION_IDS.forall(s =>
      match sessCeiling(e, s) {
        | NoCeiling => true
        | Ceiling(c) =>
          sessAccepted(e, s).forall(n => n >= c.hwAtLearn implies n < c.value)
      }))

  // --- expect-violation witnesses (non-vacuity probes) ------------------
  //
  // The vals below are NOT invariants and must never be added to a
  // regime check's invariant list. Each states "the interesting thing
  // never happens"; the quint-log-service-witness-* checks in
  // nix/quint.nix verify one each and PASS only when the checker reports
  // a violation — machine-checked evidence that the scenario a regime's
  // headline invariant constrains is actually reachable in that regime's
  // explored space. A witness that stops being violated means the model
  // (or a regime's constants) no longer reaches the behavior the code
  // can exhibit — a model bug, not a reason to delete the witness.

  /// Violated when an execution's log reads complete. The headline
  /// non-vacuity probe: `noSilentLineLoss` and
  /// `completeLogServesAllProduced` only bite for a complete log, and
  /// the open-time seal only fires against one.
  val noCompletedLog = not(EXECS.exists(e => logIsComplete(e)))

  /// Violated when two chunks of one execution from DIFFERENT sessions
  /// overlap in line range — the re-sent-batch-after-ambiguous-
  /// disconnect shape the read path's dedup exists for. A regime that
  /// never reaches it proves `servedSpanExact` only over disjoint
  /// manifests.
  val noOverlappingChunks = not(EXECS.exists(e =>
    chunks.get(e).exists(c1 => chunks.get(e).exists(c2 => and {
      c1.sess != c2.sess,
      chunkLines(c1).intersect(chunkLines(c2)) != Set(),
    }))))

  /// Violated when the read path's watermark actually suppresses a
  /// duplicate (the total yield count over all chunks falls short of
  /// the sum of the chunks' claimed counts) — the dedup is exercised,
  /// not merely encoded.
  val noDuplicateSuppressed = not(EXECS.exists(e =>
    readTail(e).count < chunks.get(e).fold(0, (acc, c) => acc + c.count)))

  /// Violated when a manifest contains a forward gap (two chunks with
  /// uncovered line numbers between them) — the shape the completeness
  /// fold's gap arm and the cut's split-at-gaps behavior exist for.
  val noGappedManifest = not(EXECS.exists(e =>
    chunks.get(e).exists(c1 => chunks.get(e).exists(c2 => and {
      c1.first + c1.count < c2.first,
      not(manifestHolds(e, c1.first + c1.count)),
    }))))

  /// Violated when the auth gate rejects a stream open for a superseded
  /// execution — the foreign-token rejection (a token for an assignment
  /// that is no longer the derivation's latest).
  val noSupersededOpenRejection = not(wit.supersededOpenRejected)

  /// Violated when the open-time seal rejects a stream open for an
  /// already-complete log.
  val noCompleteOpenRejection = not(wit.completeOpenRejected)

  /// Violated when the per-append completeness ceiling drops or
  /// truncates a batch — the post-terminal injection the gate exists to
  /// stop is actually attempted.
  val noPastFinalRejection = not(wit.pastFinalRejected)

  /// Violated when the monotone floor rejects a batch numbered below
  /// the session's high-water mark.
  val noNonMonotoneRejection = not(wit.nonMonotoneRejected)

  /// Violated when a session learns its completeness ceiling AFTER it
  /// has already accepted lines (the seal lands mid-stream and the
  /// heartbeat refresh picks it up) — the case that distinguishes the
  /// per-append gate from the open-time gate, and the source of the
  /// disclosed pre-refresh residual.
  val noMidStreamCeiling = not(EXECS.exists(e => SESSION_IDS.exists(s =>
    match sessCeiling(e, s) { | NoCeiling => false | Ceiling(c) => c.hwAtLearn > 0 })))

  /// Violated when an uploader abandons its drain with un-acked lines —
  /// the disclosed-loss channel (`rio_builder_log_drain_abandoned_total`
  /// / the permanent-rejection discard) actually fires.
  val noAbandonedWithUnacked = not(EXECS.exists(e => and {
    uploaderAbandoned(e),
    ackedBelow.get(e) < producedEnd.get(e),
  }))

  /// Violated when two executions of the derivation hold committed
  /// chunks while neither's sessions are all closed — the concurrent-
  /// writers state `noCrossExecContamination` is about (the superseded
  /// execution's session still committing while the successor's
  /// session also commits).
  val noConcurrentExecWriters = not(EXECS.exists(e1 => EXECS.exists(e2 => and {
    e1 < e2,
    chunks.get(e1) != Set(),
    chunks.get(e2) != Set(),
    SESSION_IDS.exists(s => hasSession(e1, s) and sessBuf(e1, s) != Set()),
  })))

  /// Violated when the TTL sweep deletes a non-empty manifest — the
  /// sweep's deletion arm is reachable, so `ackImpliesDurable`'s
  /// expired carve-out and the conservation law's disclosed-loss
  /// disjunct are exercised against a real deletion.
  val noSweptChunks = not(wit.sweptChunks)

  /// Violated when a build execution's REPLAY open is admitted while a
  /// later materialization attempt holds the latest-assignment slot —
  /// the exact displacement scenario merged_bug_101's fix exists for.
  /// `buildTailAdmittable`'s verdict in the kind-mix regime is about a
  /// state space that actually contains this admission.
  val noMatDispatchThenBuildReplayAdmitted = not(wit.matThenBuildReplayAdmitted)

  /// Violated when some abandonment actually fires the disclosure
  /// counter — `ingestLossCounted`'s counted arm is exercised against
  /// a real counted loss, not merely encoded.
  val noLossCounterFired = not(EXECS.exists(e => uploaderLossCounted(e)))

  /// Violated when an expired execution SURVIVES with its lifecycle
  /// row intact because it is still ineligible (non-terminal or
  /// ledger-referenced) — the v2 guard actually refuses something the
  /// age-only sweep would have taken.
  val noLiveExecSurvivesSweep = not(EXECS.exists(e => and {
    expired.get(e),
    execRowExists.get(e),
    or { dbFinalCount.get(e) == NoCount, ledgerReferenced.get(e) },
  }))

  // --- 6. actions -------------------------------------------------------
  //
  // Each action's precondition is the corresponding code path's guard
  // and its effect is that path's state mutation; the function it
  // mirrors is named in its doc comment. Actions are GUARDED (disabled
  // when their preconditions fail), not thin always-enabled no-ops.
  // Ceilings (MAX_EXECS, MAX_SESSIONS, MAX_LINE) are action
  // preconditions so the TLC backend's state space is finite.

  /// Frame helper: the variables almost every action leaves unchanged.
  /// Quint has no frame conditions; each action lists its unchanged
  /// variables explicitly. (Kept as individual assignments at each
  /// action rather than a helper action — a helper that assigns a
  /// variable the action also assigns would be a double-assignment
  /// error.)

  // --- 6a. dispatch and the build --------------------------------------

  /// The scheduler dispatches the derivation: a new assignment attempt
  /// with a fresh exec_id becomes the LATEST assignment, and its
  /// `drv_executions` lifecycle row is INSERTed.
  // Mirrors: rio-scheduler assign_to_worker -> assignments INSERT +
  //          rio-scheduler/src/db/executions.rs (the dispatch INSERT).
  action dispatch = all {
    dispatched < MAX_EXECS,
    EXECS.filter(e => e <= dispatched and execKind.get(e) == KBuild).size() < MAX_BUILD_EXECS,
    dispatched' = dispatched + 1,
    execRowExists' = execRowExists.put(dispatched + 1, true),
    execKind' = execKind.put(dispatched + 1, KBuild),
    ledgerReferenced' = ledgerReferenced.put(dispatched + 1, true),
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
  }

  /// A materialization attempt of the same derivation takes the
  /// latest-assignment slot (the zeroth-executor mint): a fresh
  /// exec_id with `attempt_kind = 'materialization'` becomes the
  /// LATEST assignment and its lifecycle row is INSERTed. It has no
  /// uploader, produces no log lines, and the open gate's kind
  /// conjunct never admits it — what it CAN do, pre-fix, is displace
  /// a build execution's late replay (the latest-only gate;
  /// `CALIB_GATE_LATEST_ONLY`).
  // Mirrors: rio-scheduler mint_and_deliver (kind = materialization)
  //          observed by gate.rs as a newer assignments row.
  action dispatchMaterialization = all {
    ENABLE_MAT_DISPATCH,
    dispatched < MAX_EXECS,
    dispatched' = dispatched + 1,
    execRowExists' = execRowExists.put(dispatched + 1, true),
    execKind' = execKind.put(dispatched + 1, KMat),
    ledgerReferenced' = ledgerReferenced.put(dispatched + 1, true),
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
  }

  /// ENVIRONMENT: the scheduler's attempt-ledger GC moves its per-lane
  /// suffix cut past this execution's rows — the ledger no longer
  /// references it and the v2 sweep may reclaim the lifecycle row.
  /// Guarded on the row still existing so the reference state is
  /// frozen after deletion (see the var's doc).
  // Mirrors: the drv_attempts per-lane GC cut observed by
  //          sweep.rs's eligibility SELECT as zero referencing rows.
  action ledgerReleases(e: int): bool = all {
    ENABLE_SWEEP,
    e <= dispatched,
    ledgerReferenced.get(e),
    execRowExists.get(e),
    ledgerReferenced' = ledgerReferenced.put(e, false),
    execKind' = execKind,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
  }

  /// The build emits one more stderr line into its uploader's input
  /// channel (and so into the retransmit buffer).
  // Mirrors: rio-builder/src/log_stream.rs (the per-line counter) +
  //          log_upload.rs::UploadTask::accept (the buffer push).
  action produceLine(e: int): bool = all {
    e <= dispatched,
    execKind.get(e) == KBuild,
    not(buildFinished.get(e)),
    producedEnd.get(e) < MAX_LINE,
    producedEnd' = producedEnd.put(e, producedEnd.get(e) + 1),
    dispatched' = dispatched,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The build exits: no more lines, the completion report (carrying
  /// the build's own line total) is ready for the scheduler, and the
  /// uploader's input channel closes (arming the drain deadline).
  // Mirrors: rio-builder/src/runtime/mod.rs (the post-footer
  //          completion report) + log_upload.rs::accept(None).
  action buildFinishes(e: int): bool = all {
    e <= dispatched,
    execKind.get(e) == KBuild,
    not(buildFinished.get(e)),
    buildFinished' = buildFinished.put(e, true),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The scheduler stamps the execution's lifecycle row terminal with
  /// the builder-reported line count. A zero-line build maps to NULL
  /// (the proto's not-reported sentinel), so its log can never read
  /// complete — disclosed, permanent, and harmless (the banner header
  /// guarantees real builds are never zero-line). The stamp is honest
  /// (the count is the build's own total): a scheduler that stamps a
  /// count below the true total would seal real lines out of the log —
  /// that is a trusted-component failure outside this model.
  /// Precondition `not(expired)`: the terminal stamp follows the
  /// completion report by seconds, the TTL by days.
  // Mirrors: rio-scheduler/src/actor/completion.rs
  //          (terminal_log_epilogue's `try_from(...).filter(> 0)`).
  action recordFinalLineCount(e: int): bool = all {
    e <= dispatched,
    buildFinished.get(e),
    execRowExists.get(e),
    not(expired.get(e)),
    dbFinalCount.get(e) == NoCount,
    dbFinalCount' = dbFinalCount.put(e,
      if (producedEnd.get(e) > 0) Count(producedEnd.get(e)) else NoCount),
    // Avoid a self-loop when the count maps to NULL: the row is
    // terminal either way, and a terminal row with an unknown count is
    // observationally identical to a non-terminal one (see the
    // dbFinalCount doc), so re-enabling the action would only add a
    // stuttering step.
    producedEnd.get(e) > 0,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6b. the open gate -----------------------------------------------

  /// `gate.rs::check_append_open`'s admission predicate, restricted to
  /// the checks with model content (v2, the bughunt-wave re-keying):
  /// the claimed execution's OWN assignment row still exists (check 3 —
  /// supersession by a newer attempt no longer revokes write authority
  /// over the old execution; only the in-place rewrite does), its log
  /// is not already complete (check 4), and its durable accepted
  /// account is under the per-execution cap (check 5,
  /// `store.log.caps-durable`). Checks 1-2 (HMAC, the token-vs-header
  /// derivation match) are pure equality on signed fields; the
  /// single-live-session lease is the `openSessionOf(e) == 0`
  /// admission conjunct on the action itself. v3 (bughunt-wave B2
  /// item 8): the kind axis is modeled — the gate's `attempt_kind`
  /// read admits build executions only, and token expiry is part of
  /// the per-exec predicate (the gate's view of an expired row).
  def openAdmitted(e: int): bool = and {
    e <= dispatched,
    execKind.get(e) == KBuild,
    // CALIBRATION: the pre-fix latest-assignment-only gate — any later
    // dispatch (including a materialization mint) revoked the replay.
    not(CALIB_GATE_LATEST_ONLY) or e == dispatched,
    CALIB_GATE_IGNORES_ROW or not(assignmentRowLost.get(e)),
    not(logIsComplete(e)),
    not(expired.get(e)),
    // CALIBRATION: pre-fix there WAS no open-time cap check (check 5
    // did not exist) — the per-session encoding drops this conjunct
    // along with resetting the account.
    CALIB_SESSION_SCOPED_CAPS or acceptedTotal.get(e) < CAP,
  }

  /// The builder opens (or re-opens) an `AppendLog` stream for its
  /// execution and the gate admits it. The new session is born with the
  /// completeness ceiling iff the execution is already terminal with a
  /// known count (the late-replay case); the uploader's transmit cursor
  /// rewinds to its acked watermark so everything un-acked is
  /// retransmitted (the at-least-once replay).
  // Mirrors: service.rs::append_log (steps 3-6) + gate.rs::
  //          check_append_open + sessions::acquire +
  //          log_upload.rs::UploadTask::open / drive's `sent = 0`.
  action openSession(e: int): bool = all {
    openAdmitted(e),
    uploader.get(e) == Uploading,
    // One AppendLog stream per uploader: the previous session must have
    // been detached (the builder cannot hold two streams). The lease's
    // Busy rejection against a still-fresh foreign-replica session is a
    // bounded retry delay, not a distinct safety state.
    openSessionOf(e) == 0,
    sessionsOpened.get(e) < MAX_SESSIONS,
    // A second open is the uploader's reconnect-and-replay; the regimes
    // that do not explore the at-least-once shape disable it (the first
    // open is always allowed).
    sessionsOpened.get(e) == 0 or ENABLE_RECONNECT,
    // Token expiry << the 30-day TTL (see the header's scope notes);
    // the conjunct lives in `openAdmitted` (v3).
    sessions' = sessions.put(e, sessions.get(e).put(sessionsOpened.get(e) + 1, Session({
      phase: SessOpen,
      highWater: 0,
      ceiling: match dbFinalCount.get(e) {
        | NoCount => NoCeiling
        | Count(n) => Ceiling({ value: n, hwAtLearn: 0 })
      },
      buf: Set(),
    }))),
    sessionsOpened' = sessionsOpened.put(e, sessionsOpened.get(e) + 1),
    sentBelow' = sentBelow.put(e, ackedBelow.get(e)),
    // CALIBRATION: the pre-fix per-session account zeroed on every
    // open; production keeps the lifetime account.
    acceptedTotal' = if (CALIB_SESSION_SCOPED_CAPS) acceptedTotal.put(e, 0)
                     else acceptedTotal,
    // Witness ghost: this open is a REPLAY (not the first session) and
    // a later materialization attempt holds the latest slot — the
    // admission the pre-fix latest-only gate displaced.
    wit' = if (sessionsOpened.get(e) > 0 and
               EXECS.exists(m => m <= dispatched and m > e and execKind.get(m) == KMat))
             { ...wit, matThenBuildReplayAdmitted: true }
           else wit,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// The gate rejects a stream open whose claimed execution's
  /// assignment row was rewritten in place (FAILED_PRECONDITION, the
  /// permanent `superseded` class). v2: mere supersession by a newer
  /// attempt no longer rejects — only the erased row does. The
  /// uploader treats it as a permanent rejection: the retransmit
  /// buffer is discarded and the remaining input is drained to
  /// nowhere — the un-stored tail is gone, disclosed by the
  /// loss-disclosure counter and by its log never reading complete.
  // Mirrors: gate.rs::check_append_open (the no-matching-row arm) +
  //          log_upload.rs::reject_permanently.
  action openRejectedSuperseded(e: int): bool = all {
    e <= dispatched,
    execKind.get(e) == KBuild,
    assignmentRowLost.get(e),
    uploader.get(e) == Uploading,
    openSessionOf(e) == 0,
    // v2 (2f975de9a): the permanent-rejection discard routes through
    // the `disclose()` chokepoint like every other abandonment arm.
    // CALIBRATION: pre-fix, rejected:true SUPPRESSED the counter — the
    // buffer vanished silently (merged_bug_360).
    uploader' = uploader.put(e, Abandoned({ counted: not(CALIB_LOSS_ESCAPE) })),
    wit' = { ...wit, supersededOpenRejected: true },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// ENVIRONMENT: the scheduler's ON-CONFLICT upsert rewrites a
  /// superseded execution's assignment row in place — the one path
  /// that permanently revokes the old executor's write authority
  /// under the v2 claimed-exec gate. Only non-latest executions are
  /// rewritable (the live attempt's row is what the rewrite WRITES).
  // Mirrors: the scheduler's assignment upsert observed by
  //          gate.rs::check_append_open as fetch_optional == None.
  action rewriteAssignment(e: int): bool = all {
    e <= dispatched,
    e != dispatched,
    not(assignmentRowLost.get(e)),
    assignmentRowLost' = assignmentRowLost.put(e, true),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    acceptedTotal' = acceptedTotal,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
  }

  /// The gate rejects a stream open because the execution's log is
  /// already complete (FAILED_PRECONDITION). Same uploader reaction as
  /// the superseded rejection, but nothing of value is lost: every line
  /// the buffer still holds is already covered by the (complete)
  /// manifest.
  // Mirrors: gate.rs::check_append_open (check 4) +
  //          log_upload.rs::reject_permanently.
  action openRejectedComplete(e: int): bool = all {
    e <= dispatched,
    e == dispatched,
    execKind.get(e) == KBuild,
    logIsComplete(e),
    uploader.get(e) == Uploading,
    openSessionOf(e) == 0,
    uploader' = uploader.put(e, Abandoned({ counted: true })),
    wit' = { ...wit, completeOpenRejected: true },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6c. batch ingestion ---------------------------------------------

  /// Rebuild session `s` of `e` after accepting the (post-truncation)
  /// batch `[lo, keptHi)`: the lines join the buffer, the high water
  /// rises to the kept end. Only meaningful under a `hasSession` guard.
  pure def sessAfterAccept(slot: SessionSlot, lo: int, keptHi: int): SessionSlot =
    match slot {
      | NoSession => NoSession
      | Session(x) => Session({
          phase: x.phase,
          highWater: maxInt(x.highWater, keptHi),
          ceiling: x.ceiling,
          buf: x.buf.union(interval(lo, keptHi)),
        })
    }

  /// The uploader transmits the next un-sent run of its retransmit
  /// buffer — `[sentBelow, producedEnd)`, the whole remainder — to the
  /// execution's Open session. The accept verdict is a pure function of
  /// the session's state: the batch is accepted (possibly truncated at
  /// the completeness ceiling), rejected as non-monotone (only
  /// reachable after a fabricated batch raised the high water past the
  /// honest cursor), or rejected as past-final (only reachable after a
  /// truncation left the cursor at the ceiling). The transmit cursor
  /// advances regardless — the uploader gets no per-batch verdict, only
  /// acks — and a rejected batch's lines stay in the retransmit buffer
  /// until an ack covers them or the drain is abandoned.
  // Mirrors: log_upload.rs::drive (the send arm) ->
  //          service.rs::drive (the Batch arm) ->
  //          ingest.rs::IngestSession::accept.
  action appendHonest(e: int): bool = all {
    e <= dispatched,
    uploader.get(e) == Uploading,
    val s = openSessionOf(e)
    val lo = sentBelow.get(e)
    val hi = producedEnd.get(e)
    all {
      s != 0,
      lo < hi,
      // The mid-stream cap guard (`store.log.caps-durable`): the kept
      // lines must fit in the execution's lifetime account. The
      // over-cap batch is the stream-fatal FAILED_PRECONDITION arm —
      // modeled by this action being disabled (the abort itself is
      // `sessionAborts`' nondeterminism).
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo, hi) {
        | Accept(keptHi) => acceptedTotal.get(e) + (keptHi - lo) <= CAP
        | RejectNonMonotone => true
        | RejectPastFinal => true
      },
      sentBelow' = sentBelow.put(e, hi),
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo, hi) {
        | Accept(keptHi) => all {
            sessions' = sessions.put(e,
              sessions.get(e).put(s, sessAfterAccept(sessions.get(e).get(s), lo, keptHi))),
            acceptedTotal' = acceptedTotal.put(e, acceptedTotal.get(e) + (keptHi - lo)),
            wit' = if (keptHi < hi) { ...wit, pastFinalRejected: true } else wit,
          }
        | RejectNonMonotone => all {
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            wit' = { ...wit, nonMonotoneRejected: true },
          }
        | RejectPastFinal => all {
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            wit' = { ...wit, pastFinalRejected: true },
          }
      },
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// The bound builder sends a batch that is NOT its uploader's honest
  /// in-order replay: an arbitrary `[lo, hi)` at or above the session's
  /// high water. Accepted lines (post-truncation at the completeness
  /// ceiling) enter the buffer and the `fabricated` ghost; lines at or
  /// past the ceiling are dropped. This is the adversary the per-append
  /// completeness gate exists for ("a builder holding a still-current
  /// token for a terminal-but-incomplete execution can append arbitrary
  /// lines past the build's true end") and the source of forward-gapped
  /// manifests. The uploader's cursors are untouched — the fabrication
  /// is the builder process misbehaving alongside its own uploader, and
  /// its high-water side effect can permanently strand the honest
  /// replay behind the monotone floor (the builder shooting its own
  /// log; the strand is disclosed by the log never completing).
  // Mirrors: ingest.rs::IngestSession::accept (the accept and
  //          truncate-at-ceiling arms) driven by an untrusted
  //          LogBatcher.
  action appendFabricated(e: int, lo: int, hi: int): bool = all {
    ENABLE_FABRICATION,
    e <= dispatched,
    lo < hi,
    hi <= MAX_LINE,
    val s = openSessionOf(e)
    all {
      s != 0,
      lo >= sessHw(e, s),
      // Fabricated lines are accepted (and charged) like honest ones —
      // the cap is an accounting bound, not an honesty check.
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo, hi) {
        | Accept(keptHi) => acceptedTotal.get(e) + (keptHi - lo) <= CAP
        | RejectNonMonotone => true
        | RejectPastFinal => true
      },
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo, hi) {
        | Accept(keptHi) => all {
            sessions' = sessions.put(e,
              sessions.get(e).put(s, sessAfterAccept(sessions.get(e).get(s), lo, keptHi))),
            fabricated' = fabricated.put(e, fabricated.get(e).union(interval(lo, keptHi))),
            acceptedTotal' = acceptedTotal.put(e, acceptedTotal.get(e) + (keptHi - lo)),
            wit' = if (keptHi < hi) { ...wit, pastFinalRejected: true } else wit,
          }
        | RejectNonMonotone => all {
            // Unreachable under the `lo >= hw` precondition; the arm
            // must still assign every variable.
            sessions' = sessions,
            fabricated' = fabricated,
            acceptedTotal' = acceptedTotal,
            wit' = wit,
          }
        | RejectPastFinal => all {
            sessions' = sessions,
            fabricated' = fabricated,
            acceptedTotal' = acceptedTotal,
            wit' = { ...wit, pastFinalRejected: true },
          }
      },
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// A session learns the execution's recorded `final_line_count`
  /// mid-stream: the scheduler stamped the lifecycle row terminal while
  /// the builder was still streaming, and the handler's heartbeat-tick
  /// refresh observed it. The learn-time high-water mark is recorded so
  /// `completenessGate` can separate the pre-refresh residual from the
  /// post-refresh obligation. Only an Open session refreshes (the
  /// heartbeat loop exits with the driver's select loop); the first
  /// observed value wins (the count is stamped once and never changes).
  // Mirrors: service.rs::drive (the heartbeat arm's
  //          sealed_final_line_count refresh) +
  //          ingest.rs::set_final_line_count.
  action refreshCeiling(e: int, s: int): bool = all {
    e <= dispatched,
    sessIsOpen(e, s),
    sessCeiling(e, s) == NoCeiling,
    execRowExists.get(e),
    val n = dbFinalCount.get(e)
    all {
      n != NoCount,
      sessions' = sessions.put(e, sessions.get(e).put(s,
        match sessions.get(e).get(s) {
          | NoSession => NoSession
          | Session(x) => Session({
              phase: x.phase,
              highWater: x.highWater,
              ceiling: match n { | NoCount => NoCeiling | Count(v) => Ceiling({ value: v, hwAtLearn: x.highWater }) },
              buf: x.buf,
            })
        })),
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6d. the chunk cut and the ack -----------------------------------

  /// Cut the longest contiguous prefix of session `s`'s buffer into one
  /// immutable chunk: the run leaves the buffer and a manifest row
  /// describing exactly that run is committed under the cutting
  /// session's `(exec, session)` key. The commit and the manifest
  /// INSERT are one atomic step here — the failure path restores the
  /// staged run exactly (a no-op at this granularity) and burns a
  /// sequence number the model does not track; the abort that three
  /// consecutive failures trigger is `sessionAborts`. Both Open and
  /// Detached sessions cut (the periodic/size-triggered cut and the
  /// final drain respectively) — a detached predecessor session
  /// committing its drain concurrently with its successor's cuts is
  /// the manifest-overlap source.
  // Mirrors: ingest.rs::IngestSession::cut + commit_chunk;
  //          service.rs::do_cut / drain.
  action cutChunk(e: int, s: int): bool = all {
    e <= dispatched,
    hasSession(e, s),
    sessBuf(e, s) != Set(),
    val cutRun = runFrom(sessBuf(e, s), setMin(sessBuf(e, s)))
    all {
      chunks' = chunks.put(e, chunks.get(e).union(Set({
        sess: s,
        first: setMin(cutRun),
        count: cutRun.size(),
      }))),
      sessions' = sessions.put(e, sessions.get(e).put(s,
        match sessions.get(e).get(s) {
          | NoSession => NoSession
          | Session(x) => Session({ ...x, buf: x.buf.exclude(cutRun) })
        })),
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The builder receives the ack for one committed chunk of its
  /// CURRENT Open session and trims every retransmit-buffer frame whose
  /// last line the ack covers. The ack exists only because the chunk's
  /// manifest INSERT committed (`do_cut` sends it after `cut` returns
  /// the durable-through line), and it is deliverable only while the
  /// builder still holds the stream that cut the chunk — a detached
  /// session's drain acks go to a closed response stream. Acks arrive
  /// in cut order on the wire; the trim is a max, so delivering any
  /// single chunk's ack reaches the same watermark set as delivering
  /// the ordered prefix up to it.
  // Mirrors: service.rs::do_cut (the AppendLogAck send) +
  //          log_upload.rs::trim.
  action deliverAck(e: int): bool = all {
    e <= dispatched,
    uploader.get(e) == Uploading,
    val s = openSessionOf(e)
    all {
      s != 0,
      nondet c = chunks.get(e).filter(c => c.sess == s).oneOf()
      all {
        c.first + c.count > ackedBelow.get(e),
        ackedBelow' = ackedBelow.put(e, c.first + c.count),
      },
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6e. disconnects, aborts, abandonment ----------------------------

  /// The builder's stream to its Open session ends without a
  /// server-side abort: the clean half-close at the end of every build,
  /// a transport error, or the builder reconnecting elsewhere. The
  /// driver runs its final drain — the session keeps its buffer and
  /// keeps cutting (as a Detached session) until the buffer is empty.
  /// Un-acked lines stay in the builder's retransmit buffer for the
  /// next session; the drain's chunks for those same lines are the
  /// manifest overlap the read path dedups. Enabled in every regime
  /// (every build's stream ends); whether a SECOND session can then
  /// open is `openSession`'s ENABLE_RECONNECT gate.
  // Mirrors: service.rs::drive (Ok(None) / Err -> ClientFinished) +
  //          AppendDriver::run (the drain arm).
  action builderDisconnects(e: int): bool = all {
    e <= dispatched,
    val s = openSessionOf(e)
    all {
      s != 0,
      sessions' = sessions.put(e, sessions.get(e).put(s,
        match sessions.get(e).get(s) {
          | NoSession => NoSession
          | Session(x) => Session({ ...x, phase: SessDetached })
        })),
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The server tears the stream down and DROPS the in-memory buffer
  /// without draining it: the gray-failure abort (three consecutive
  /// failed cuts, a stale buffer), the per-execution byte/chunk caps,
  /// or the ingest lease observed as lost. Safe by design: every
  /// dropped line is still in the builder's retransmit buffer (it was
  /// never acked) and will be replayed to the next session.
  // Mirrors: service.rs::AppendDriver::run (the LoopExit::Abort /
  //          LeaseLost arms — the drain is skipped and the
  //          deregistration scopeguard drops IngestShared).
  action sessionAborts(e: int, s: int): bool = all {
    e <= dispatched,
    hasSession(e, s),
    // An inert detached session (empty buffer) aborting is a no-op
    // self-loop; require something observable to change.
    sessIsOpen(e, s) or sessBuf(e, s) != Set(),
    sessions' = sessions.put(e, sessions.get(e).put(s,
      match sessions.get(e).get(s) {
        | NoSession => NoSession
        | Session(x) => Session({ ...x, phase: SessDetached, buf: Set() })
      })),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The uploader gives up with lines still un-acked: the post-build
  /// drain deadline expires while the store is unreachable (or keeps
  /// rejecting the stream). The retransmit buffer is dropped; the loss
  /// is disclosed by `rio_builder_log_drain_abandoned_total` and the
  /// error log, and the execution's log never reads complete (the
  /// manifest cannot cover the abandoned tail).
  // Mirrors: log_upload.rs::run_inner (DeadlineExpired -> abandoned()).
  action uploaderAbandons(e: int): bool = all {
    e <= dispatched,
    execKind.get(e) == KBuild,
    uploader.get(e) == Uploading,
    buildFinished.get(e),
    ackedBelow.get(e) < producedEnd.get(e),
    // The drain-deadline arm always routes through `disclose()`; its
    // un-acked lines are real loss, so the counter fires.
    uploader' = uploader.put(e, Abandoned({ counted: true })),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6f. expiry and the TTL sweep ------------------------------------

  /// The retention clock passes the execution's `started_at +
  /// log_retention_days`. Environment action; monotone.
  action executionExpires(e: int): bool = all {
    ENABLE_SWEEP,
    e <= dispatched,
    not(expired.get(e)),
    expired' = expired.put(e, true),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The store's TTL sweep pass for one expired execution
  /// (`sweep.rs::sweep_expired_logs`): every `drv_log_chunks` manifest
  /// row (and transitively the objects). The pass also clears the
  /// `log_ingest_sessions` REGISTRY rows — hygiene this model does not
  /// carry: the `sessions` variable mirrors the IN-MEMORY ingest
  /// sessions (phase, high water, buffer), which the pass never
  /// touches — a still-open session keeps cutting after the pass, and
  /// the SELECT's EXISTS disjunction re-finds the new chunks next
  /// pass. v2 (`store.log.sweep-ownership`): the pass NEVER deletes
  /// the `drv_executions` row — lifecycle state is scheduler-owned
  /// (`gcExecRow`). Age is the pass's only eligibility: safe precisely
  /// BECAUSE the row survives. The guard requires something to strip
  /// (the EXISTS disjunction keeps a stripped execution out of every
  /// later pass).
  // Mirrors: sweep.rs::sweep_expired_logs (the drv_log_chunks DELETE +
  //          delete_batch; the registry DELETE is unmodeled hygiene).
  action sweepChunks(e: int): bool = all {
    ENABLE_SWEEP,
    e <= dispatched,
    expired.get(e),
    execRowExists.get(e),
    not(sweepChunksDone.get(e)),
    or {
      chunks.get(e) != Set(),
      SESSION_IDS.exists(s => hasSession(e, s)),
    },
    chunks' = chunks.put(e, Set()),
    sessions' = sessions,
    sweepChunksDone' = sweepChunksDone.put(e, true),
    wit' = if (chunks.get(e) != Set()) { ...wit, sweptChunks: true } else wit,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// The SCHEDULER's execution-row reclaim
  /// (`SchedulerDb::gc_exec_rows`, the housekeeping tick arm): deletes
  /// the `drv_executions` row behind the kernel's eligibility —
  /// `execRowSweepEligible` (terminal AND ledger-unreferenced AND aged
  /// out; see the def's doc for the priced fourth conjunct). Runs in a
  /// different service from the store's sweep pass: no ordering with
  /// `sweepChunks` — a row reclaimed before its chunks are stripped
  /// leaves the chunks unreachable to the store SELECT (bounded by the
  /// S3 lifecycle backstop, priced in the header), and a row that
  /// outlives its stripped chunks reads incomplete forever (disclosed).
  // Mirrors: rio-scheduler/src/db/attempts.rs::gc_exec_rows behind
  //          rio_retry_kernel::exec_row_sweep_eligible.
  action gcExecRow(e: int): bool = all {
    ENABLE_SWEEP,
    e <= dispatched,
    execRowSweepEligible(e),
    execRowExists.get(e),
    execRowExists' = execRowExists.put(e, false),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  // --- 8. init ----------------------------------------------------------

  action init = all {
    dispatched' = 0,
    producedEnd' = EXECS.mapBy(e => 0),
    buildFinished' = EXECS.mapBy(e => false),
    dbFinalCount' = EXECS.mapBy(e => NoCount),
    execRowExists' = EXECS.mapBy(e => false),
    expired' = EXECS.mapBy(e => false),
    assignmentRowLost' = EXECS.mapBy(e => false),
    acceptedTotal' = EXECS.mapBy(e => 0),
    sweepChunksDone' = EXECS.mapBy(e => false),
    execKind' = EXECS.mapBy(e => KBuild),
    ledgerReferenced' = EXECS.mapBy(e => true),
    uploader' = EXECS.mapBy(e => Uploading),
    ackedBelow' = EXECS.mapBy(e => 0),
    sentBelow' = EXECS.mapBy(e => 0),
    sessions' = EXECS.mapBy(e => SESSION_IDS.mapBy(s => NoSession)),
    sessionsOpened' = EXECS.mapBy(e => 0),
    chunks' = EXECS.mapBy(e => Set()),
    fabricated' = EXECS.mapBy(e => Set()),
    wit' = {
      supersededOpenRejected: false,
      completeOpenRejected: false,
      pastFinalRejected: false,
      nonMonotoneRejected: false,
      sweptChunks: false,
      matThenBuildReplayAdmitted: false,
    },
  }

  // --- 9. step ----------------------------------------------------------
  // Every disjunct of `step` is a NAMED action with its nondet choices
  // bound directly inside it (the MBT tracker records the innermost
  // disjunct's name; an anonymous block makes the whole step anonymous
  // and undrivable).

  action produceLineAny = { nondet e = EXECS.oneOf() produceLine(e) }
  action buildFinishesAny = { nondet e = EXECS.oneOf() buildFinishes(e) }
  action recordFinalLineCountAny = { nondet e = EXECS.oneOf() recordFinalLineCount(e) }
  action openSessionAny = { nondet e = EXECS.oneOf() openSession(e) }
  action openRejectedSupersededAny = { nondet e = EXECS.oneOf() openRejectedSuperseded(e) }
  action rewriteAssignmentAny = { nondet e = EXECS.oneOf() rewriteAssignment(e) }
  action openRejectedCompleteAny = { nondet e = EXECS.oneOf() openRejectedComplete(e) }
  action appendHonestAny = { nondet e = EXECS.oneOf() appendHonest(e) }
  action appendFabricatedAny = {
    nondet e = EXECS.oneOf()
    nondet lo = 0.to(MAX_LINE - 1).oneOf()
    nondet hi = 1.to(MAX_LINE).oneOf()
    appendFabricated(e, lo, hi)
  }
  action refreshCeilingAny = {
    nondet e = EXECS.oneOf()
    nondet s = SESSION_IDS.oneOf()
    refreshCeiling(e, s)
  }
  action cutChunkAny = {
    nondet e = EXECS.oneOf()
    nondet s = SESSION_IDS.oneOf()
    cutChunk(e, s)
  }
  action deliverAckAny = { nondet e = EXECS.oneOf() deliverAck(e) }
  action builderDisconnectsAny = { nondet e = EXECS.oneOf() builderDisconnects(e) }
  action sessionAbortsAny = {
    nondet e = EXECS.oneOf()
    nondet s = SESSION_IDS.oneOf()
    sessionAborts(e, s)
  }
  action uploaderAbandonsAny = { nondet e = EXECS.oneOf() uploaderAbandons(e) }
  action executionExpiresAny = { nondet e = EXECS.oneOf() executionExpires(e) }
  action sweepChunksAny = { nondet e = EXECS.oneOf() sweepChunks(e) }
  action gcExecRowAny = { nondet e = EXECS.oneOf() gcExecRow(e) }
  action ledgerReleasesAny = { nondet e = EXECS.oneOf() ledgerReleases(e) }

  action step = any {
    dispatch,
    dispatchMaterialization,
    produceLineAny,
    buildFinishesAny,
    recordFinalLineCountAny,
    openSessionAny,
    openRejectedSupersededAny,
    rewriteAssignmentAny,
    openRejectedCompleteAny,
    appendHonestAny,
    appendFabricatedAny,
    refreshCeilingAny,
    cutChunkAny,
    deliverAckAny,
    builderDisconnectsAny,
    sessionAbortsAny,
    uploaderAbandonsAny,
    executionExpiresAny,
    sweepChunksAny,
    gcExecRowAny,
    ledgerReleasesAny,
  }
}

// =========================================================================
// THE BASE REGIME: one execution, one session, no reconnects, no sweep.
// The adversaries are the builder's own batch stream (fabricated forward
// gaps, re-sends below the high water, lines at or past the recorded
// final count) and the unbiased interleaving of production, transmission,
// cutting, acking, the terminal stamp, and the mid-stream ceiling
// refresh. A regression in the core ingest/cut/read/complete lifecycle —
// the accept predicate, the per-append completeness ceiling, the
// contiguous-prefix cut, the read fold, the completeness fold — surfaces
// in this small fast check instead of buried in the multi-session
// regimes.
//
// Constants are chosen for COVERAGE — the smallest state space that
// still reaches every interleaving class the invariants are about:
//   - 1 execution: the supersession scenarios need a second execution
//     and live in the redispatch regime.
//   - 1 session: the overlap/dedup scenarios need a second session and
//     live in the resend regime.
//   - 3 line numbers: a forward gap ({0,2}), a batch straddling the
//     completeness ceiling, and a partial ack are the smallest shapes
//     that exercise the cut's gap split, the ceiling truncation, and
//     the conservation law's buffer accounting.
//
// NON-VACUITY at these constants (machine-checked by the
// quint-log-service-witness-* checks in nix/quint.nix): a log reads
// complete, a past-final batch is rejected, a non-monotone batch is
// rejected, a session learns its ceiling mid-stream, a manifest carries
// a forward gap, and an uploader abandons un-acked lines.
// =========================================================================
module logServiceBase {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = true,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*

  // --- deterministic protocol runs --------------------------------------
  // Executable sanity checks of the happy path and the known-critical
  // scenarios; `quint test --main=logServiceBase --match=<run>` replays
  // them. They do not change the verified state space (runs are not
  // part of `step`).

  /// The happy path: dispatch, produce two lines, stream them, cut,
  /// ack, finish, stamp, and the log reads complete with every produced
  /// line served.
  run happyPathRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(deliverAck(1))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .expect(all {
        ackedBelow.get(1) == 2,
        chunks.get(1) == Set({ sess: 1, first: 0, count: 2 }),
        dbFinalCount.get(1) == Count(2),
        logIsComplete(1),
        readTail(1).served == Set(0, 1),
        builderBuf(1) == Set(),
      })

  /// The per-append completeness ceiling: the execution goes terminal
  /// at 1 line while the session is open; the refresh observes it; a
  /// fabricated batch at the recorded end is rejected whole and a
  /// straddling batch is truncated, so the manifest never grows past
  /// the recorded count.
  run pastFinalRejectionRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .then(refreshCeiling(1, 1))
      .expect(sessCeiling(1, 1) == Ceiling({ value: 1, hwAtLearn: 1 }))
      .then(appendFabricated(1, 1, 3))
      .expect(all {
        wit.pastFinalRejected,
        sessBuf(1, 1) == Set(0),
        sessAccepted(1, 1).forall(n => n < 1),
      })

  /// The mid-stream seal residual: lines accepted before the session
  /// learns the ceiling are kept (over-coverage the completeness fold
  /// tolerates), lines at or past it offered afterwards are dropped.
  run midStreamCeilingResidualRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendFabricated(1, 0, 3))
      .expect(sessBuf(1, 1) == Set(0, 1, 2))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .then(refreshCeiling(1, 1))
      // The residual (lines 1 and 2, accepted before the learn) stays;
      // completenessGate excludes it via hwAtLearn = 3.
      .expect(all {
        sessCeiling(1, 1) == Ceiling({ value: 1, hwAtLearn: 3 }),
        completenessGate,
      })
}

// =========================================================================
// THE REDISPATCH REGIME: two executions of one derivation, one session
// each. Execution 1 is superseded by execution 2 mid-stream: its
// already-open session keeps writing to its OWN (old) execution's log,
// its builder's next reconnect is rejected by the auth gate and discards
// its un-stored tail, and the two executions' manifests grow
// concurrently under disjoint key namespaces. This is the regime where
// `authGateExcludesForeignWriters` and `noCrossExecContamination` are
// load-bearing; the witness checks pin the superseded-open rejection and
// the concurrent-writers state as reachable.
//
// Fabrication is DISABLED: the adversarial batch stream is the base
// regime's subject, and none of the supersession scenarios depend on it
// — a fabricated line lands in the fabricating execution's own keyspace
// regardless of whether that execution is the latest assignment, which
// the base regime already proves. Enabling it here multiplies two
// already-independent per-execution state machines by the fabrication
// branching factor squared and pushes the check past the per-check
// budget without adding an interleaving class any invariant observes.
// =========================================================================
module logServiceRedispatch {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 2,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*

  /// The superseded writer, v2: execution 1's session is open and
  /// holding lines when execution 2 is dispatched. Supersession alone
  /// no longer revokes execution 1's write authority (it remains
  /// admitted — its late replay is the merged_bug_101 fix); only the
  /// scheduler's in-place row rewrite does, after which its builder is
  /// rejected at the next open and abandons. Execution 2's log is
  /// untouched by any of it.
  run supersededWriterRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(dispatch)
      // v2: BOTH stay admitted — the old exec's own row still exists.
      .expect(openAdmitted(1) and openAdmitted(2))
      .then(cutChunk(1, 1))
      .expect(all {
        chunks.get(1) == Set({ sess: 1, first: 0, count: 1 }),
        chunks.get(2) == Set(),
        noCrossExecContamination,
      })
      .then(builderDisconnects(1))
      // The one revocation path: the assignment row is rewritten in
      // place; only then is the old executor excluded.
      .then(rewriteAssignment(1))
      .expect(not(openAdmitted(1)) and openAdmitted(2))
      .then(openRejectedSuperseded(1))
      .expect(all {
        wit.supersededOpenRejected,
        uploaderAbandoned(1),
        // v2: the permanent-rejection discard routes through
        // disclose() — the loss is counted, never silent.
        uploaderLossCounted(1),
      })
}

// =========================================================================
// THE RESEND REGIME: one execution, two sessions, reconnects enabled.
// The at-least-once shape: a chunk is committed but its ack never
// reaches the builder (the stream dies first), the builder reopens a
// fresh session and retransmits its whole un-acked buffer, and the
// detached predecessor's final drain commits chunks that overlap the
// successor's. This is the regime where `servedSpanExact`'s dedup and
// `ackImpliesDurable` are load-bearing; the witness checks pin the
// overlapping-chunks state and the suppressed duplicate as reachable.
// Fabrication is disabled — the adversary here is the retransmission
// protocol itself, and the fabricated-batch branching multiplies a
// state space that the two-session product already dominates.
// =========================================================================
module logServiceResend {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    // 4, not MAX_LINE: the at-least-once replay re-charges the
    // retransmitted lines (the durable account is seeded from committed
    // chunks and the new session re-accepts the un-acked tail — the
    // code's exact double-charge on overlap), so the canonical
    // 2-line-cut + reconnect + 2-line-replay trace costs 4. The cap
    // boundary is reached by exactly that trace (noCapBoundary's
    // witness), making the cap conjuncts non-vacuous here.
    CAP = 4,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*

  /// The ambiguous-disconnect overlap: session 1 commits a chunk for
  /// lines [0, 2) but the builder disconnects before the ack arrives;
  /// the builder replays the same lines to session 2, which commits an
  /// overlapping chunk; the read path serves each line exactly once.
  run ambiguousAckOverlapRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .expect(chunks.get(1) == Set({ sess: 1, first: 0, count: 2 }))
      // The ack is never delivered: the builder disconnects first.
      .then(builderDisconnects(1))
      .then(openSession(1))
      .expect(sentBelow.get(1) == 0)
      .then(appendHonest(1))
      .then(cutChunk(1, 2))
      .expect(all {
        chunks.get(1) == Set(
          { sess: 1, first: 0, count: 2 },
          { sess: 2, first: 0, count: 2 }
        ),
        // Two overlapping chunks, two yield opportunities per line,
        // each line served exactly once.
        readTail(1).served == Set(0, 1),
        readTail(1).count == 2,
        servedSpanExact,
      })
}

// =========================================================================
// THE SWEEP REGIME: the resend regime plus expiry and the TTL sweep.
// The sweep's expired-only guard, the chunks-before-execution-row
// deletion order (with a live session's cut interleavable between the
// two DELETEs), and the disclosed-loss accounting for swept lines are
// the load-bearing pieces; the witness check pins the non-empty-manifest
// deletion as reachable. Sessions are capped at one — the sweep's
// interaction is with the manifest and the lifecycle row, not with the
// session overlap, and the two-session product is already covered by the
// resend regime.
// =========================================================================
module logServiceSweep {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = true,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*

  /// The TTL sweep deletes a complete log: the manifest and the
  /// lifecycle row go, the log stops reading complete (the disclosure),
  /// and the conservation law's disclosed-loss disjunct absorbs the
  /// deleted lines.
  run sweepCompleteLogRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(deliverAck(1))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .expect(logIsComplete(1))
      .then(executionExpires(1))
      .then(sweepChunks(1))
      // v2 ownership split: the store pass strips the manifest and the
      // session registry but the lifecycle row SURVIVES it.
      .expect(all {
        chunks.get(1) == Set(),
        not(logIsComplete(1)),
        execRowExists.get(1),
        wit.sweptChunks,
        noSilentLineLoss,
      })
      // The scheduler's reclaim needs the attempt ledger released
      // first (terminal and aged it already is).
      .then(ledgerReleases(1))
      .then(gcExecRow(1))
      .expect(not(execRowExists.get(1)))
}

// =========================================================================
// CALIBRATION: the pre-fix per-SESSION cap accounting (merged_bug_207).
// Every reconnect zeroes the accepted account, so the durable footprint
// grows one cap per session — `acceptedWithinCap` (measured against the
// durable chunks + buffers, not the resettable ghost) falsifies. The
// expect-violation check pins that the invariant actually catches the
// bug class this wave fixed; if a refactor makes it pass here, the
// invariant has gone vacuous and the check fails loudly.
// =========================================================================
module logServiceCalibSessionCaps {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    // 2 < MAX_LINE: the durable footprint is a SET of line numbers
    // (≤ MAX_LINE = 3), so the cap must sit BELOW the line domain for
    // an over-cap footprint to be representable. Pre-fix: session 1
    // accepts {0,1} (= CAP) and cuts; the reconnect resets the
    // account; session 2 accepts line 2 — footprint {0,1,2} = 3 > 2.
    // Post-fix the lifetime account blocks session 2's accept and the
    // reopen itself.
    CAP = 2,
    CALIB_SESSION_SCOPED_CAPS = true,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*
}

// =========================================================================
// CALIBRATION: the open gate without the assignment-row conjunct
// (merged_bug_101's sibling regression class — admission that consults
// nothing about the claimed exec's own row). With the conjunct ignored,
// a rewritten-in-place execution's builder is re-admitted —
// `authGateExcludesUnassignedWriters` falsifies.
// =========================================================================
module logServiceCalibGateRow {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 2,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = true,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*
}

// =========================================================================
// THE KIND-MIX REGIME (bughunt-wave B2 item 8): one build execution and
// one materialization attempt of the same derivation. The mat dispatch
// takes the latest-assignment slot mid-stream; the build's late replay
// (a reconnect after its first session detaches) must still be admitted
// — the displacement merged_bug_101's v2 gate fixed — and the mat
// execution must never be admitted at all. `buildTailAdmittable` is
// load-bearing here; the witness check pins the
// replay-after-mat-dispatch admission as reachable. Fabrication is
// disabled (the adversary is the dispatch interleaving, not the batch
// stream); two sessions are required for the replay open.
// =========================================================================
module logServiceKindMix {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = true,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*

  /// The displacement scenario, fixed: the build streams and cuts a
  /// chunk, disconnects, a materialization mint takes the latest slot,
  /// and the build's replay open is STILL admitted (v2: its own row is
  /// intact) while the mat execution never is.
  run matDispatchThenBuildReplayRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(builderDisconnects(1))
      .then(dispatchMaterialization)
      .expect(all {
        execKind.get(2) == KMat,
        not(openAdmitted(2)),
        openAdmitted(1),
      })
      .then(openSession(1))
      .expect(all {
        wit.matThenBuildReplayAdmitted,
        buildTailAdmittable,
      })
}

// =========================================================================
// CALIBRATION: the pre-fix latest-assignment-only open gate
// (merged_bug_101's displacement hole, the kind-mix face). With
// `e == dispatched` re-added to admission, the materialization mint
// landing after the build revokes the build's replay —
// `buildTailAdmittable` falsifies. Pins that the invariant catches the
// displacement class B2's claimed-exec re-keying fixed.
// =========================================================================
module logServiceCalibLatestOnly {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = true,
    CALIB_GATE_LATEST_ONLY = true,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*
}

// =========================================================================
// CALIBRATION: the pre-fix rejected:true suppression (merged_bug_360's
// PERMISSION_DENIED hole). The permanent-rejection abandonment discards
// the retransmit buffer without the disclosure counter; an execution
// with produced-but-never-stored lines abandons silently —
// `ingestLossCounted` falsifies. Two executions so the in-place row
// rewrite (the rejection's trigger) is reachable.
// =========================================================================
module logServiceCalibLossEscape {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 2,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = true,
    CALIB_SWEEP_AGE_ONLY = false,
  ).*
}

// =========================================================================
// CALIBRATION: the pre-fix age-only sweep SELECT (merged_bug_086). With
// the terminal and ledger-reference conjuncts ignored, the pass deletes
// a never-terminal (or still-referenced) execution's lifecycle row —
// `sweepOnlyTerminalUnreferenced` falsifies. Pins that the invariant
// catches the unguarded-delete class the kernel's
// `exec_row_sweep_eligible` guard fixed.
// =========================================================================
module logServiceCalibSweepAgeOnly {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = true,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = true,
  ).*
}
