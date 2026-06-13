// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-13 F6 (rio-lease
// leader marks, the repair plane): re-introduces the UNBUDGETED
// reconcile-failure world — pre-F6, reconcileFail was the only
// environment action with no budget, so the spawn→fail livelock was a
// lawful forever-schedule: every state of it satisfied
// marksDivergenceBounded's dirty disjunct ("discovered!") while
// repairing nothing. Discovery-not-repair — the audit's class-e
// shape; production analog: the persistent PATCH 403 the code prices
// as a warn-stream forever-state.
//
// PRE-FIX behavior modeled: nothing is overridden — the live module's
// own alphabet under a failure budget high enough (9) that the
// livelock outlives the convergence bound inside the horizon. The
// regime IS the pre-fix world because the only thing F6 added to the
// transition relation is the budget guard.
//
// PREDICTED falsification: marksConvergeWithinLiveBudget — the LIVE
// convergence regime's bound (9 rounds at its constants), restated
// here as a literal. The parametric marksConvergenceBounded
// self-calibrates with MAX_RECONCILE_FAILS (its repair term consumes
// the budget), so simply raising the budget widens the bound with the
// livelock — the regression this twin must catch is the live
// CERTIFICATE failing, not the formula tracking its inputs. An
// external add diverges the standby; the repair chain
// spawn→fail→spawn→fail… (each failure consuming its one-round
// cycle floor) carries the divergence past 9 rounds with every state
// dirty or in-flight — while marksDivergenceBounded stays GREEN over
// the very same livelock (discovery satisfied forever, repair never).
// Both verdicts are recorded in the introducing commit body — the
// old-form green IS the defect this twin freezes.
//
// This pin is the non-vacuity evidence for
// quint-leader-marks-convergence: if it ever stops falsifying, the
// convergence claim has drifted back into a discovery claim.
//
// Run (the wired pin — the unbudgeted-failure world falsifies):
//   quint verify --backend=tlc --main=leaseCalibF6FailLivelock \
//     --invariant=marksConvergeWithinLiveBudget \
//     docs/spec/models/calibration/lease-f6-fail-livelock.qnt
module leaseCalibF6FailLivelock {
  import leaderMarks(
    VERIFY_EVERY = 3,
    MAX_TIME = 12,
    MAX_STRIPS = 0,
    MAX_ADDS = 1,
    MAX_VERIFY_FAILS = 0,
    MAX_RECONCILE_FAILS = 9,
    MAX_HOLDER_CHANGES = 0,
    MAX_REBOUNDS = 0,
    TASK_TIMEOUT = 2
  ).* from "../leaderMarks"

  /// The LIVE convergence regime's certified bound, as a literal:
  /// (1+0) · ((1+0)·3+2 + (1+1)·2) = 9 at leaderMarksConvergence's
  /// constants. Restated by value because the parametric form would
  /// self-calibrate to THIS module's raised budget and absorb the
  /// livelock.
  val convergenceBoundLive = 9

  /// The live certificate under the unbudgeted-failure world: the
  /// livelock must outrun it.
  val marksConvergeWithinLiveBudget =
    wrongUnder(marks, holder, "us") implies
      clock - wrongSince.get("us") <= convergenceBoundLive
}
