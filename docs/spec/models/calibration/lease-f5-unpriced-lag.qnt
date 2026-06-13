// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-13 F5 (rio-lease
// leader marks, the observation-lag plane): re-introduces the
// UNPRICED-ENVIRONMENT posture — the pre-F5 world in which the
// partition-case repair of a deposed ex-leader's stale label (the
// NEW holder's peer sweep; the ex-leader's own lagged observation)
// was "declared environment": repairs EXIST and stay ENABLED, but
// nothing obliges either to fire. Repair-enabled is not
// repair-happening — the same enabledness-as-liveness shape the
// audit's class-e findings name.
//
// LIVE COUNTERPART: leaderMarksObservationLag (docs/spec/models/
// leaderMarks.qnt). Quint instances cannot be shared across files
// (an instantiated module's vars do not re-export), so this pin
// restates the lag machinery against its OWN leaderMarks instance —
// the same shape every leaseCalib* override restates its target
// action. Drift between this file and the regime is caught by the
// pin itself: a regime change that invalidates the restated world
// stops the falsification, and this check goes red.
//
// PRE-FIX behavior modeled:
//   - calibLagTickUnpriced carries NO repair obligation (the pre-F5
//     model priced the lag window not at all);
//   - the sweep (the regime's otherSweepsUs) is ABSENT — it was
//     environment prose, not a transition;
//   - observeEdge stays in the step, enabled and unforced — the
//     checker finds the trace where it never fires.
//
// PREDICTED falsification: lagDivergenceBounded. The world moves
// away from a converged leader (no local mark — the fused edge is
// exactly what the lag plane unfuses); the stale leader label —
// load-bearing for the leader-only Service — ages past REPAIR_BOUND
// with the generations clean, no task, observation never landing,
// and no sweep in the alphabet.
//
// This pin is the non-vacuity evidence for the
// quint-leader-marks-lag regime: if it ever stops falsifying, the
// repair obligation has drifted out of the lag plane and the window
// pricing is decoration again.
//
// Run (the wired pin — the unpriced-lag world falsifies):
//   quint verify --backend=tlc --main=leaseCalibF5UnpricedLag \
//     --step=calibStep --init=lagInit --invariant=lagDivergenceBounded \
//     docs/spec/models/calibration/lease-f5-unpriced-lag.qnt
module leaseCalibF5UnpricedLag {
  import leaderMarks(
    VERIFY_EVERY = 4,
    MAX_TIME = 10,
    MAX_STRIPS = 0,
    MAX_ADDS = 0,
    MAX_VERIFY_FAILS = 0,
    MAX_RECONCILE_FAILS = 2,
    MAX_HOLDER_CHANGES = 2,
    MAX_REBOUNDS = 0,
    TASK_TIMEOUT = 2
  ).* from "../leaderMarks"

  /// The stale-label repair budget the LIVE regime prices
  /// (leaderMarksObservationLag::REPAIR_BOUND) — restated so the
  /// violated bound is the same number.
  val REPAIR_BOUND = 3

  /// An our-leadership-affecting world move not yet locally observed.
  var obsLag: bool

  /// The world's holder moves with NO local mark (the unfused half).
  // Environment: the Lease changing hands at the apiserver, unfused from this pod's observation (the F5 window; the live counterpart is leaderMarksObservationLag::lagHolderChange)
  action lagHolderChange(h: HolderState): bool = all {
    holderChanges < MAX_HOLDER_CHANGES,
    h != holder,
    holder' = h,
    obsLag' = (obsLag or leadingOf("us", h) != leadingOf("us", holder)),
    marked' = marked, cleared' = cleared,
    marks' = marks, task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges + 1, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, h, wrongSince, clock),
    wrongCause' = stampCause(marks, h, wrongSince, wrongCause, ByEdge),
  }

  // Environment: the world's move, any target (nondet wrapper)
  action lagHolderChangeAny = {
    nondet h = Set(NoLeader, Leader("us"), Leader("other")).oneOf()
    lagHolderChange(h)
  }

  /// The deferred local edge — enabled, and in THIS world never
  /// forced.
  // Mirrors: rio-lease/src/lib.rs:observe_held_while_believing
  action observeEdge = all {
    obsLag,
    obsLag' = false,
    marked' = marked + 1,
    cleared' = cleared,
    holder' = holder, marks' = marks, task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// PRE-FIX tick: no repair obligation — the lag window is
  /// unpriced. The task-residency cap is KEPT (API deadlines existed
  /// pre-fix too).
  // Environment: the pre-F5 posture restored as a regression pin — the partition-case repair "declared environment", unpriced (no live transition carries it)
  action calibLagTickUnpriced = all {
    clock < MAX_TIME,
    (task != NoTask) implies (clock + 1 - taskSince <= TASK_TIMEOUT),
    clock' = clock + 1,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, task' = task,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased, obsLag' = obsLag,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock + 1),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// Reconciles run observation-current only (the live regime's
  /// scoping, restated).
  // Mirrors: rio-lease/src/lib.rs:maybe_spawn_leader_marks
  action lagReconcileSpawn = all {
    not(obsLag),
    reconcileSpawn,
    obsLag' = obsLag,
  }
  // Mirrors: rio-lease/src/lib.rs:spawn_patch_leader_marks
  action lagReconcilePatched = all {
    reconcilePatched,
    obsLag' = obsLag,
  }
  // Mirrors: rio-lease/src/lib.rs:clear_through
  action lagReconcileClear = all {
    reconcileClear,
    obsLag' = obsLag,
  }
  // Mirrors: rio-lease/src/lib.rs:spawn_patch_leader_marks
  action lagReconcileFail = all {
    reconcileFail,
    obsLag' = obsLag,
  }

  // Environment: the regime's init — the live init plus the lag flag down
  action lagInit = all {
    init,
    obsLag' = false,
  }

  /// The live regime's bound, restated verbatim (the violated
  /// proposition must be the same one the regime holds).
  val lagDivergenceBounded =
    wrongUnder(marks, holder, "us") implies or {
      dirty,
      task != NoTask,
      clock - wrongSince.get("us") <= REPAIR_BOUND,
    }

  /// The lag step with the repair obligation REMOVED and the sweep
  /// ABSENT — observeEdge enabled and unforced, exactly the
  /// enabledness-as-liveness world.
  action calibStep = any {
    calibLagTickUnpriced,
    lagHolderChangeAny,
    observeEdge,
    lagReconcileSpawn,
    lagReconcilePatched,
    lagReconcileClear,
    lagReconcileFail,
  }
}
