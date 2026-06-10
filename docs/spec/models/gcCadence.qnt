// gcCadence.qnt — the backstop's ATTEMPT-CADENCE and COMMIT-GATES-OK
// laws (bughunt-4 S4: bug_284 + merged_bug_218,
// store.gc.collect-cadence+4; `rio-store/src/gc/{collect,state}.rs`
// `stamp_attempt` / `CycleCommitted`), extended with the bughunt-5 S7
// COMMIT-CLASSIFICATION axis (merged_bug_022: witness at the
// durability point, three-valued attribution, own-commit recognition).
// quint-policy-latches: ranEarly, falseLost

// ===========================================================================
// CADENCE + COMMIT-WITNESS LAWS (bughunt-4 S4: bug_284 + merged_bug_218,
// store.gc.collect-cadence+4).
//
// Laws of the backstop loop:
//
//  1. ATTEMPT-CADENCE: heavy cycles are spaced at least INTERVAL apart
//     for ALL outcome arms. The attempt stamp is written BEFORE the
//     cycle runs, so a fail-closed abort (persistent corrupt manifest,
//     a condition that waits for a human) cannot be re-attempted on
//     every check tick. The pre-fix due predicate keyed only on the
//     SUCCESS stamp, which an abort never writes: 24 heavy scans/day
//     against the documented once-per-interval cadence.
//
//  2. COMMIT-GATES-OK: the outcome="ok" tick requires a LANDED durable
//     commit (the CycleCommitted witness). Pre-fix the tick ran inside
//     the cycle, before commit_cycle: a lost commit (idle-killed lock
//     session) left metrics green while stamp/cursor/backlog were
//     gone. Strengthened by the round-5 axis: a recognized
//     response-lost commit and a coincidence-recognized foreign commit
//     both tick ok only WITH a landed commit in the same step.
//
//  3. ATTRIBUTION-PARTITION + NO-FALSE-LOST (bughunt-5 S7,
//     merged_bug_022): every drained cycle ticks exactly one of
//     ok | commit_failed | commit_indeterminate, and commit_failed
//     requires PROOF of loss - the as-built 0-row collapse ("another
//     holder committed first", unconditional) and the post-commit
//     release-? shape both reported durably-landed commits as lost.
//
// DEDICATED module (gcDrainPass precedent): the time/cadence axis must
// not multiply the commit-basis or drain-pass regimes.
// ===========================================================================
module gcCadence {
  /// Minimum ticks between heavy-cycle attempts (the backstop
  /// interval; checks fire every tick).
  const INTERVAL: int

  /// Horizon: ticks explored (time is bounded for the exhaustive
  /// regime).
  const MAX_TIME: int

  /// CALIBRATION ONLY (`gcCadenceCalibHourlyRetry`): the pre-fix due
  /// predicate — success-stamp staleness only, no attempt gate.
  const CALIB_NO_ATTEMPT_GATE: bool

  /// CALIBRATION ONLY (`gcCadenceCalibTickBeforeCommit`): the pre-fix
  /// tick placement — ok ticks when the cycle drains, before (and
  /// regardless of) the commit landing.
  const CALIB_TICK_BEFORE_COMMIT: bool

  /// Round-5 axis gate (merged_bug_022): enables the commit step's
  /// full shape alphabet — response loss, foreign-live/foreign-shadow
  /// interleave after lock-session death, payload coincidence — and
  /// the three-valued attribution. Default-FALSE so the pre-extension
  /// regimes stay state-space-identical (their new vars evolve as
  /// pure functions of the legacy trajectory).
  const COMMIT_CLASSIFY: bool

  /// CALIBRATION ONLY (`gcCadenceCalibZeroRowsClaimsForeign`): the
  /// as-built 0-row retry collapse — every 0-row or errored retry is
  /// unconditionally attributed "another holder committed first"
  /// (commit_failed), even when the row carries OUR OWN landed write.
  const CALIB_ZERO_ROWS_FOREIGN: bool

  /// CALIBRATION ONLY (`gcCadenceCalibReleaseFailsCommit`): the
  /// as-built post-commit release-`?` shape — a landed commit whose
  /// lock release fails is reported lost (the witness was minted
  /// after the release instead of at the durability point).
  const CALIB_RELEASE_FAILS: bool

  /// Round-6 axis gate (merged_bug_021): enables the SIBLING attempt
  /// stamp — any replica lawfully writes the shared attempt clock
  /// with no dueness gate (run_gc phase 3: an operator GC must not
  /// abort over cadence bookkeeping) whenever the advisory lock is
  /// free (the dead-session world), both as a first-class action
  /// (siblingStampAttempt, deferring the backstop) and as the
  /// commit-step interposition nondet the classifier consults.
  /// Default-FALSE so the pre-extension regimes stay
  /// state-space-identical (the new var evolves as a pure function of
  /// the legacy trajectory; the action is disabled).
  const SIBLING_STAMPS: bool

  /// CALIBRATION ONLY (`gcCadenceCalibSharedClockAnchor`): the
  /// as-built recognition conjunct reading the SHARED attempt clock —
  /// a sibling's stamp interposing between our applied-but-
  /// response-lost commit and the probe makes our own landed payload
  /// read as "another holder committed first" (the merged_bug_021
  /// forged foreign winner). The fixed law anchors recognition at the
  /// holder's OWN held stamp, which a sibling cannot move.
  const CALIB_SHARED_CLOCK_ANCHOR: bool

  var time: int
  /// Success stamp: last COMMITTED live cycle (-1 = never). The
  /// stalled alert keys on this; aborts never write it.
  var lastLive: int
  /// Attempt stamp (migration 100): last heavy-cycle START (-1 =
  /// never). Written before the cycle, every outcome arm inherits it.
  var lastAttempt: int
  /// Last heavy-cycle start, for the spacing latch (model-side
  /// observation; the code's analogue is the attempt stamp itself).
  var lastHeavyAt: int
  /// A corrupt chunk_list exists: every cycle aborts fail-closed.
  var corrupt: bool
  /// LATCH (seated on backstopFires' own enabling read-set): a heavy
  /// cycle started closer than INTERVAL to the previous one.
  var ranEarly: bool
  /// Aborted heavy runs (fail-closed) — non-vacuity observable.
  var aborts: int
  /// outcome="ok" ticks emitted.
  var okTicks: int
  /// Durable commits that actually landed (cycle_epoch advances) —
  /// OURS and foreign interleaves alike (the epoch counter).
  var committedEpochs: int
  /// OUR commits that did not land (ground truth) — non-vacuity
  /// observable.
  var lostCommits: int
  /// Drained (non-abort) heavy cycles — the attribution domain.
  var drainedCycles: int
  /// outcome="commit_failed" ticks emitted (attribution, distinct
  /// from the lostCommits ground truth).
  var lostTicks: int
  /// outcome="commit_indeterminate" ticks emitted.
  var indeterminateTicks: int
  /// Response-lost commits recognized as our own by payload echo —
  /// non-vacuity observable for the recognition path.
  var responseLostRecognized: int
  /// Response-lost commits recognized as our own WITH a sibling
  /// attempt stamp interposed (merged_bug_021) — the non-vacuity
  /// observable for the own-held-anchor law (frozen 0 at
  /// SIBLING_STAMPS = false).
  var stampInterposedRecognitions: int
  /// Foreign winners PROVEN by row evidence (epoch at expected+1,
  /// mismatched payload) — non-vacuity observable.
  var provenForeign: int
  /// ok ticks earned through the documented payload-coincidence
  /// residual (byte-identical foreign LIVE commit recognized as ours;
  /// globally sound — the foreign commit IS landed — misattributed
  /// per-replica only).
  var coincidenceOks: int
  /// LATCH: a commit_failed tick fired for a cycle whose OWN commit
  /// landed (the false-lost shape merged_bug_022 retires).
  var falseLost: bool

  action init = all {
    time' = 0,
    lastLive' = -1,
    lastAttempt' = -1,
    lastHeavyAt' = -1,
    nondet c = Set(true, false).oneOf()
    corrupt' = c,
    ranEarly' = false,
    aborts' = 0,
    okTicks' = 0,
    committedEpochs' = 0,
    lostCommits' = 0,
    drainedCycles' = 0,
    lostTicks' = 0,
    indeterminateTicks' = 0,
    responseLostRecognized' = 0,
    stampInterposedRecognitions' = 0,
    provenForeign' = 0,
    coincidenceOks' = 0,
    falseLost' = false,
  }

  /// A check tick with nothing due (or no check): time advances.
  action tick = all {
    time < MAX_TIME,
    time' = time + 1,
    lastLive' = lastLive,
    lastAttempt' = lastAttempt,
    lastHeavyAt' = lastHeavyAt,
    corrupt' = corrupt,
    ranEarly' = ranEarly,
    aborts' = aborts,
    okTicks' = okTicks,
    committedEpochs' = committedEpochs,
    lostCommits' = lostCommits,
    drainedCycles' = drainedCycles,
    lostTicks' = lostTicks,
    indeterminateTicks' = indeterminateTicks,
    responseLostRecognized' = responseLostRecognized,
    stampInterposedRecognitions' = stampInterposedRecognitions,
    provenForeign' = provenForeign,
    coincidenceOks' = coincidenceOks,
    falseLost' = falseLost,
  }

  /// The human repairs the corrupt manifest.
  action repair = all {
    corrupt,
    corrupt' = false,
    time' = time,
    lastLive' = lastLive,
    lastAttempt' = lastAttempt,
    lastHeavyAt' = lastHeavyAt,
    ranEarly' = ranEarly,
    aborts' = aborts,
    okTicks' = okTicks,
    committedEpochs' = committedEpochs,
    lostCommits' = lostCommits,
    drainedCycles' = drainedCycles,
    lostTicks' = lostTicks,
    indeterminateTicks' = indeterminateTicks,
    responseLostRecognized' = responseLostRecognized,
    stampInterposedRecognitions' = stampInterposedRecognitions,
    provenForeign' = provenForeign,
    coincidenceOks' = coincidenceOks,
    falseLost' = falseLost,
  }

  /// Corruption appears (an aborted upload lands a bad chunk_list).
  action corruptAppears = all {
    not(corrupt),
    corrupt' = true,
    time' = time,
    lastLive' = lastLive,
    lastAttempt' = lastAttempt,
    lastHeavyAt' = lastHeavyAt,
    ranEarly' = ranEarly,
    aborts' = aborts,
    okTicks' = okTicks,
    committedEpochs' = committedEpochs,
    lostCommits' = lostCommits,
    drainedCycles' = drainedCycles,
    lostTicks' = lostTicks,
    indeterminateTicks' = indeterminateTicks,
    responseLostRecognized' = responseLostRecognized,
    stampInterposedRecognitions' = stampInterposedRecognitions,
    provenForeign' = provenForeign,
    coincidenceOks' = coincidenceOks,
    falseLost' = falseLost,
  }

  /// The due predicate. REAL: success-stale AND attempt-stale.
  /// CALIB: success-stale only (the pre-fix hole).
  def due(t: int): bool = and {
    or { lastLive == -1, t - lastLive >= INTERVAL },
    or {
      CALIB_NO_ATTEMPT_GATE,
      lastAttempt == -1,
      t - lastAttempt >= INTERVAL,
    },
  }

  // --- the round-5 commit-step shape alphabet (COMMIT_CLASSIFY) -----------
  //
  // shape: 0 appliedClean (primary applied, response received)
  //        1 appliedReleaseFails (applied; post-commit release fails)
  //        2 appliedResponseLost (applied; response lost - session died)
  //        3 notApplied (primary failed before applying)
  // foreignArm (consulted for shapes 2 and 3 - the advisory lock died
  //        with the session, another replica can commit before our
  //        retry): 0 none, 1 live, 2 shadow
  // coincide: the documented benign residual - a byte-identical
  //        foreign LIVE commit whose payload echo matches ours
  // retryWorks: for shape 3 with no foreign winner - does the
  //        epoch-guarded retry apply (true) or error (false)?

  /// Did OUR commit land (ground truth)?
  pure def ourCommitLands(shape: int, foreignArm: int, retryWorks: bool): bool = or {
    shape == 0,
    shape == 1,
    shape == 2,
    and { shape == 3, foreignArm == 0, retryWorks },
  }

  /// Did a FOREIGN commit land in the retry window?
  pure def foreignWins(shape: int, foreignArm: int): bool = and {
    or { shape == 2, shape == 3 },
    foreignArm != 0,
  }

  /// The attribution the code under the active calibration emits:
  /// 0 = ok, 1 = commit_failed, 2 = commit_indeterminate.
  /// Post-fix (all CALIB flags false) this is the merged_bug_022
  /// classifier under the merged_bug_021 own-held-anchor law: witness
  /// at the durability point (shapes 0/1 -> ok), own-echo recognition
  /// at the holder's OWN held attempt stamp (shape 2, no foreign ->
  /// ok REGARDLESS of a sibling stamp interposing — the probe no
  /// longer reads the shared clock), interleaved epochs unprovable
  /// (shape 2 with foreign -> indeterminate), retry-applied -> ok,
  /// retry-error -> indeterminate, proven foreign -> commit_failed on
  /// POSITIVE payload contradiction only (coincidence excepted - the
  /// documented residual). The CALIB_SHARED_CLOCK_ANCHOR twin
  /// restores the as-built shared-clock conjunct: an interposed
  /// sibling stamp forges commit_failed out of our own landed write.
  def classifyAttribution(shape: int, foreignArm: int, coincide: bool, retryWorks: bool, interposedPick: bool): int =
    val interposed = and { SIBLING_STAMPS, interposedPick }
    if (shape == 0) 0
    else if (shape == 1) (if (CALIB_RELEASE_FAILS) 1 else 0)
    else if (shape == 2) (
      if (CALIB_ZERO_ROWS_FOREIGN) 1
      else if (foreignArm == 0) (
        if (and { CALIB_SHARED_CLOCK_ANCHOR, interposed }) 1 else 0
      )
      else 2
    )
    else (
      if (foreignArm == 0) (
        if (retryWorks) 0
        else if (CALIB_ZERO_ROWS_FOREIGN) 1
        else 2
      )
      else if (foreignArm == 1) (
        if (CALIB_ZERO_ROWS_FOREIGN) 1
        else if (coincide) 0
        else 1
      )
      else 1
    )

  /// A backstop check finds the cluster due and runs the heavy cycle.
  /// The attempt stamp is written FIRST (every arm inherits it); the
  /// outcome splits on corruption and, for drained cycles, on the
  /// commit fate.
  action backstopFires = all {
    due(time),
    time < MAX_TIME,
    // the spacing latch: seated on this action's own enabling
    // condition and read-set (time, lastHeavyAt) — a heavy run
    // starting inside the previous run's interval window.
    ranEarly' = or {
      ranEarly,
      and { lastHeavyAt != -1, time - lastHeavyAt < INTERVAL },
    },
    lastHeavyAt' = time,
    lastAttempt' = if (CALIB_NO_ATTEMPT_GATE) lastAttempt else time,
    time' = time + 1,
    corrupt' = corrupt,
    if (corrupt) all {
      // Fail-closed abort: no success stamp, no commit, no ok tick
      // (the parse_failure counter is out of scope here).
      aborts' = aborts + 1,
      lastLive' = lastLive,
      okTicks' = okTicks,
      committedEpochs' = committedEpochs,
      lostCommits' = lostCommits,
      drainedCycles' = drainedCycles,
      lostTicks' = lostTicks,
      indeterminateTicks' = indeterminateTicks,
      responseLostRecognized' = responseLostRecognized,
      stampInterposedRecognitions' = stampInterposedRecognitions,
      provenForeign' = provenForeign,
      coincidenceOks' = coincidenceOks,
      falseLost' = falseLost,
    } else all {
      aborts' = aborts,
      drainedCycles' = drainedCycles + 1,
      if (COMMIT_CLASSIFY) {
        // Round-5 axis (merged_bug_022): the full commit-step shape
        // alphabet. Unconsulted nondets for a shape are still picked
        // (TLA+ style) but do not change the updates.
        nondet shape = Set(0, 1, 2, 3).oneOf()
        nondet foreignArm = Set(0, 1, 2).oneOf()
        nondet coincide = Set(true, false).oneOf()
        nondet retryWorks = Set(true, false).oneOf()
        // merged_bug_021: a sibling's attempt stamp interposing
        // between our apply and the diagnostic probe (consulted only
        // under SIBLING_STAMPS; an unconsulted pick changes nothing).
        nondet stampInterposedPick = Set(true, false).oneOf()
        all {
          committedEpochs' = committedEpochs
            + (if (ourCommitLands(shape, foreignArm, retryWorks)) 1 else 0)
            + (if (foreignWins(shape, foreignArm)) 1 else 0),
          lastLive' = if (or {
            ourCommitLands(shape, foreignArm, retryWorks),
            and { foreignWins(shape, foreignArm), foreignArm == 1 },
          }) time else lastLive,
          lostCommits' = if (ourCommitLands(shape, foreignArm, retryWorks))
            lostCommits else lostCommits + 1,
          okTicks' = if (classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 0)
            okTicks + 1 else okTicks,
          lostTicks' = if (classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 1)
            lostTicks + 1 else lostTicks,
          indeterminateTicks' = if (classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 2)
            indeterminateTicks + 1 else indeterminateTicks,
          responseLostRecognized' = if (and {
            shape == 2,
            foreignArm == 0,
            classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 0,
          }) responseLostRecognized + 1 else responseLostRecognized,
          stampInterposedRecognitions' = if (and {
            SIBLING_STAMPS,
            stampInterposedPick,
            shape == 2,
            foreignArm == 0,
            classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 0,
          }) stampInterposedRecognitions + 1 else stampInterposedRecognitions,
          provenForeign' = if (and {
            shape == 3,
            foreignArm != 0,
            classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 1,
          }) provenForeign + 1 else provenForeign,
          coincidenceOks' = if (and {
            shape == 3,
            foreignArm == 1,
            coincide,
            classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 0,
          }) coincidenceOks + 1 else coincidenceOks,
          falseLost' = or {
            falseLost,
            and {
              classifyAttribution(shape, foreignArm, coincide, retryWorks, stampInterposedPick) == 1,
              ourCommitLands(shape, foreignArm, retryWorks),
            },
          },
        }
      } else {
        // Legacy lane (pre-round-5 regimes): binary commit fate; the
        // round-5 vars evolve as PURE FUNCTIONS of the legacy
        // trajectory, so the pre-extension regimes stay
        // state-space-identical (the F16 byte-identity convention,
        // bughunt4-fix-specs.md).
        nondet commitLands = Set(true, false).oneOf()
        all {
          committedEpochs' = if (commitLands) committedEpochs + 1 else committedEpochs,
          lastLive' = if (commitLands) time else lastLive,
          lostCommits' = if (commitLands) lostCommits else lostCommits + 1,
          okTicks' =
            if (CALIB_TICK_BEFORE_COMMIT) okTicks + 1
            else if (commitLands) okTicks + 1
            else okTicks,
          lostTicks' = if (commitLands) lostTicks else lostTicks + 1,
          indeterminateTicks' = indeterminateTicks,
          responseLostRecognized' = responseLostRecognized,
          stampInterposedRecognitions' = stampInterposedRecognitions,
          provenForeign' = provenForeign,
          coincidenceOks' = coincidenceOks,
          falseLost' = falseLost,
        }
      },
    },
  }

  /// merged_bug_021: a SIBLING replica's attempt stamp — written with
  /// NO dueness gate (run_gc phase 3's unconditional stamp) whenever
  /// the lock is free. Defers the backstop's due predicate exactly as
  /// production does (the cadence law quantifies attempts per
  /// holder-SET, not per replica: a sibling stamp only ever WIDENS
  /// spacing, so attemptCadenceBounded is unaffected by construction).
  /// Disabled at SIBLING_STAMPS = false (pre-extension byte-identity).
  action siblingStampAttempt = all {
    SIBLING_STAMPS,
    time < MAX_TIME,
    lastAttempt' = time,
    time' = time,
    lastLive' = lastLive,
    lastHeavyAt' = lastHeavyAt,
    corrupt' = corrupt,
    ranEarly' = ranEarly,
    aborts' = aborts,
    okTicks' = okTicks,
    committedEpochs' = committedEpochs,
    lostCommits' = lostCommits,
    drainedCycles' = drainedCycles,
    lostTicks' = lostTicks,
    indeterminateTicks' = indeterminateTicks,
    responseLostRecognized' = responseLostRecognized,
    stampInterposedRecognitions' = stampInterposedRecognitions,
    provenForeign' = provenForeign,
    coincidenceOks' = coincidenceOks,
    falseLost' = falseLost,
  }

  action step = any {
    tick,
    repair,
    corruptAppears,
    backstopFires,
    siblingStampAttempt,
  }

  // --- invariants (store.gc.collect-cadence+4) ----------------------------

  /// Heavy-cycle attempts are spaced at least INTERVAL apart, for ALL
  /// outcome arms. The hourly-retry twin falsifies it: with the
  /// attempt gate removed, a persistent abort re-runs on every check.
  val attemptCadenceBounded = not(ranEarly)

  /// outcome="ok" requires a landed commit: the tick count never
  /// exceeds the committed epoch count. The tick-before-commit twin
  /// falsifies it on the first lost commit. Strengthened by the
  /// round-5 axis: recognition oks and coincidence oks both pair with
  /// a landed commit (ours and the foreign winner's, respectively) in
  /// the same step.
  val okTicksRequireCommit = okTicks <= committedEpochs

  /// merged_bug_022: every drained (non-abort) cycle ticks EXACTLY one
  /// of ok | commit_failed | commit_indeterminate. The
  /// tick-before-commit shape (ok early AND lost on the same cycle)
  /// breaks the partition.
  val attributionPartition = okTicks + lostTicks + indeterminateTicks == drainedCycles

  /// merged_bug_022: commit_failed requires PROOF of loss — in any
  /// trace free of the documented payload-coincidence residual, a
  /// lost tick never fires for a cycle whose own commit landed. The
  /// zero-rows-claims-foreign and release-fails-commit twins both
  /// falsify it.
  val noFalseLost = or { coincidenceOks > 0, not(falseLost) }

  /// Non-vacuity witness (must VIOLATE in the main regime): the
  /// fail-closed abort path is reachable — the cadence law is only
  /// meaningful if aborted attempts actually occur.
  val noAbortedAttempt = aborts == 0

  /// Non-vacuity witness (must VIOLATE in the main regime): the
  /// lost-commit arm is reachable — the commit-gates-ok law is only
  /// meaningful if commits actually get lost.
  val noLostCommit = lostCommits == 0

  /// Non-vacuity witness (must VIOLATE in the main regime): the
  /// applied-but-response-lost RECOGNITION path is reachable — the
  /// no-false-lost law is only meaningful if recognition actually
  /// happens.
  val noRecognizedResponseLost = responseLostRecognized == 0

  /// Non-vacuity witness (must VIOLATE in the main regime): the
  /// proven-foreign-winner path is reachable — commit_failed's
  /// evidence requirement is only meaningful if foreign winners
  /// actually occur.
  val noProvenForeignWinner = provenForeign == 0

  /// Non-vacuity witness (must VIOLATE in the main regime,
  /// merged_bug_021): own-commit recognition WITH a sibling attempt
  /// stamp interposed is actually explored — the own-held-anchor
  /// law's noFalseLost verdict is about a space where the
  /// interposition really happens.
  val noStampInterposedRecognition = stampInterposedRecognitions == 0
}

// ===========================================================================
// CADENCE MAIN REGIME: real wiring (round-5 classifier ON), all laws
// hold.
// ===========================================================================
module gcCadenceMain {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = false,
    CALIB_TICK_BEFORE_COMMIT = false,
    COMMIT_CLASSIFY = true,
    CALIB_ZERO_ROWS_FOREIGN = false,
    CALIB_RELEASE_FAILS = false,
    SIBLING_STAMPS = true,
    CALIB_SHARED_CLOCK_ANCHOR = false
  ).*

  /// The bug_284 red, deterministically: a corrupt store, two checks
  /// inside one interval — exactly ONE heavy run happens (the second
  /// check is throttled by the attempt stamp), and the success stamp
  /// stays unwritten (the stalled alert's signal).
  run secondCheckInsideIntervalThrottledRun =
    init
      .then(all {
        time' = 0,
        lastLive' = -1,
        lastAttempt' = -1,
        lastHeavyAt' = -1,
        corrupt' = true,
        ranEarly' = false,
        aborts' = 0,
        okTicks' = 0,
        committedEpochs' = 0,
        lostCommits' = 0,
        drainedCycles' = 0,
        lostTicks' = 0,
        indeterminateTicks' = 0,
        responseLostRecognized' = 0,
        stampInterposedRecognitions' = 0,
        provenForeign' = 0,
        coincidenceOks' = 0,
        falseLost' = false,
      })
      .then(backstopFires) // check 1: due (never attempted) -> heavy run, aborts
      .expect(all {
        aborts == 1,
        lastAttempt == 0,
        lastLive == -1,
      })
      .then(tick) // check 2 one tick later: NOT due (attempt stamp)
      .expect(all {
        not(due(time)),
        aborts == 1,
        attemptCadenceBounded,
      })

  /// merged_bug_021 constructor probe: a SIBLING's attempt stamp
  /// (no dueness gate) defers our backstop exactly one interval —
  /// the attempt clock is holder-set-wide, so the cadence law is
  /// unaffected while the recognition anchor must not be.
  run siblingStampDefersBackstopRun =
    init
      .then(all {
        time' = 0,
        lastLive' = -1,
        lastAttempt' = -1,
        lastHeavyAt' = -1,
        corrupt' = true,
        ranEarly' = false,
        aborts' = 0,
        okTicks' = 0,
        committedEpochs' = 0,
        lostCommits' = 0,
        drainedCycles' = 0,
        lostTicks' = 0,
        indeterminateTicks' = 0,
        responseLostRecognized' = 0,
        stampInterposedRecognitions' = 0,
        provenForeign' = 0,
        coincidenceOks' = 0,
        falseLost' = false,
      })
      .then(siblingStampAttempt) // the sibling stamps at t=0 (lock free)
      .expect(all {
        lastAttempt == 0,
        aborts == 0,        // no heavy run happened here
        not(due(time)),     // ...yet our backstop is deferred
        attemptCadenceBounded,
      })
}

// ===========================================================================
// CADENCE CALIBRATION TWIN (expect-violation): the pre-fix due
// predicate — no attempt gate. A persistent abort re-runs on every
// check tick; attemptCadenceBounded falsifies. Round-5 axis OFF:
// state space identical to the pre-extension regime.
// ===========================================================================
module gcCadenceCalibHourlyRetry {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = true,
    CALIB_TICK_BEFORE_COMMIT = false,
    COMMIT_CLASSIFY = false,
    CALIB_ZERO_ROWS_FOREIGN = false,
    CALIB_RELEASE_FAILS = false,
    SIBLING_STAMPS = false,
    CALIB_SHARED_CLOCK_ANCHOR = false
  ).*
}

// ===========================================================================
// COMMIT-WITNESS CALIBRATION TWIN (expect-violation): the pre-fix tick
// placement — ok before the commit lands; okTicksRequireCommit
// falsifies on the first lost commit. Round-5 axis OFF: state space
// identical to the pre-extension regime.
// ===========================================================================
module gcCadenceCalibTickBeforeCommit {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = false,
    CALIB_TICK_BEFORE_COMMIT = true,
    COMMIT_CLASSIFY = false,
    CALIB_ZERO_ROWS_FOREIGN = false,
    CALIB_RELEASE_FAILS = false,
    SIBLING_STAMPS = false,
    CALIB_SHARED_CLOCK_ANCHOR = false
  ).*
}

// ===========================================================================
// ZERO-ROWS-CLAIMS-FOREIGN CALIBRATION TWIN (expect-violation,
// bughunt-5 S7): the as-built 0-row retry collapse — every 0-row or
// errored retry unconditionally attributed "another holder committed
// first" (commit_failed). An applied-but-response-lost commit (the
// row carries OUR OWN write at expected+1) is reported lost;
// noFalseLost falsifies. Same regime constants as the main regime.
// ===========================================================================
module gcCadenceCalibZeroRowsClaimsForeign {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = false,
    CALIB_TICK_BEFORE_COMMIT = false,
    COMMIT_CLASSIFY = true,
    CALIB_ZERO_ROWS_FOREIGN = true,
    CALIB_RELEASE_FAILS = false,
    SIBLING_STAMPS = false,
    CALIB_SHARED_CLOCK_ANCHOR = false
  ).*
}

// ===========================================================================
// RELEASE-FAILS-COMMIT CALIBRATION TWIN (expect-violation, bughunt-5
// S7): the as-built post-commit release-`?` shape — the witness was
// minted AFTER the release instead of at the durability point, so a
// landed commit whose lock release fails is reported lost;
// noFalseLost falsifies. Same regime constants as the main regime.
// ===========================================================================
module gcCadenceCalibReleaseFailsCommit {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = false,
    CALIB_TICK_BEFORE_COMMIT = false,
    COMMIT_CLASSIFY = true,
    CALIB_ZERO_ROWS_FOREIGN = false,
    CALIB_RELEASE_FAILS = true,
    SIBLING_STAMPS = false,
    CALIB_SHARED_CLOCK_ANCHOR = false
  ).*
}

// ===========================================================================
// SHARED-CLOCK-ANCHOR CALIBRATION TWIN (expect-violation, bughunt-6
// S6 merged_bug_021): the as-built recognition conjunct reads the
// SHARED attempt clock — a sibling's stamp interposing between our
// applied-but-response-lost commit and the diagnostic probe makes our
// own landed payload read as "another holder committed first";
// noFalseLost falsifies. KILL ISOLATION (R16): with the zero-rows and
// release toggles OFF, falseLost (classify == commit_failed AND
// ourCommitLands) is reachable ONLY through the interposed shape-2
// cell — shape 1 is ok (release toggle off), shape 3's commit_failed
// cells all have ourCommitLands = false — so the violation dies
// through THIS conjunct with the sibling lanes proven intact in the
// same trace. Same regime constants as the main regime, axis ON.
// ===========================================================================
module gcCadenceCalibSharedClockAnchor {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = false,
    CALIB_TICK_BEFORE_COMMIT = false,
    COMMIT_CLASSIFY = true,
    CALIB_ZERO_ROWS_FOREIGN = false,
    CALIB_RELEASE_FAILS = false,
    SIBLING_STAMPS = true,
    CALIB_SHARED_CLOCK_ANCHOR = true
  ).*
}
