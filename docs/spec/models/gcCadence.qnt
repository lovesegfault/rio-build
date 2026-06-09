// gcCadence.qnt — the backstop's ATTEMPT-CADENCE and COMMIT-GATES-OK
// laws (bughunt-4 S4: bug_284 + merged_bug_218,
// store.gc.collect-cadence+2; `rio-store/src/gc/{collect,state}.rs`
// `stamp_attempt` / `CycleCommitted`).
// quint-policy-latches: ranEarly

// ===========================================================================
// CADENCE + COMMIT-WITNESS LAWS (bughunt-4 S4: bug_284 + merged_bug_218,
// store.gc.collect-cadence+2).
//
// Two laws of the backstop loop:
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
//     gone.
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
  /// Durable commits that actually landed (cycle_epoch advances).
  var committedEpochs: int
  /// Lost commits (drained cycle, commit failed) — non-vacuity
  /// observable.
  var lostCommits: int

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

  /// A backstop check finds the cluster due and runs the heavy cycle.
  /// The attempt stamp is written FIRST (every arm inherits it); the
  /// outcome splits on corruption and, for drained cycles, on the
  /// commit landing.
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
    } else all {
      // Drained cycle: the commit lands or is lost (idle-killed lock
      // session; the epoch-guarded retry also failed).
      nondet commitLands = Set(true, false).oneOf()
      all {
        committedEpochs' = if (commitLands) committedEpochs + 1 else committedEpochs,
        lastLive' = if (commitLands) time else lastLive,
        lostCommits' = if (commitLands) lostCommits else lostCommits + 1,
        okTicks' =
          if (CALIB_TICK_BEFORE_COMMIT) okTicks + 1
          else if (commitLands) okTicks + 1
          else okTicks,
      },
      aborts' = aborts,
    },
  }

  action step = any {
    tick,
    repair,
    corruptAppears,
    backstopFires,
  }

  // --- invariants (store.gc.collect-cadence+2) ----------------------------

  /// Heavy-cycle attempts are spaced at least INTERVAL apart, for ALL
  /// outcome arms. The hourly-retry twin falsifies it: with the
  /// attempt gate removed, a persistent abort re-runs on every check.
  val attemptCadenceBounded = not(ranEarly)

  /// outcome="ok" requires a landed commit: the tick count never
  /// exceeds the committed epoch count. The tick-before-commit twin
  /// falsifies it on the first lost commit.
  val okTicksRequireCommit = okTicks <= committedEpochs

  /// Non-vacuity witness (must VIOLATE in the main regime): the
  /// fail-closed abort path is reachable — the cadence law is only
  /// meaningful if aborted attempts actually occur.
  val noAbortedAttempt = aborts == 0

  /// Non-vacuity witness (must VIOLATE in the main regime): the
  /// lost-commit arm is reachable — the commit-gates-ok law is only
  /// meaningful if commits actually get lost.
  val noLostCommit = lostCommits == 0
}

// ===========================================================================
// CADENCE MAIN REGIME: real wiring, both laws hold.
// ===========================================================================
module gcCadenceMain {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = false,
    CALIB_TICK_BEFORE_COMMIT = false
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
}

// ===========================================================================
// CADENCE CALIBRATION TWIN (expect-violation): the pre-fix due
// predicate — no attempt gate. A persistent abort re-runs on every
// check tick; attemptCadenceBounded falsifies.
// ===========================================================================
module gcCadenceCalibHourlyRetry {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = true,
    CALIB_TICK_BEFORE_COMMIT = false
  ).*
}

// ===========================================================================
// COMMIT-WITNESS CALIBRATION TWIN (expect-violation): the pre-fix tick
// placement — ok before the commit lands; okTicksRequireCommit
// falsifies on the first lost commit.
// ===========================================================================
module gcCadenceCalibTickBeforeCommit {
  import gcCadence(
    INTERVAL = 4,
    MAX_TIME = 12,
    CALIB_NO_ATTEMPT_GATE = false,
    CALIB_TICK_BEFORE_COMMIT = true
  ).*
}
