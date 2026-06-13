// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-13 F1 (the audit's
// one confirmed HIGH; rio-lease leader marks): re-introduces the
// LEADER-ONLY verify world — the pre-F1 machinery in which the verify
// pass was gated on `now_leading` (rio-lease/src/lib.rs pre-fix spawn
// gate + the `leading=true` hardcode in
// maybe_spawn_verify_leader_marks), so a NON-leading pod re-examined
// its own marks NEVER. The falsifier polarity F1 added to the
// alphabet (externalAdd — `kubectl label pod <standby> …role=leader`,
// a foreign controller, any future actor) is ENABLED here, which is
// exactly the pre-F1 production world: the state was reachable in
// reality and bounded by nothing — while the pre-F1 MODEL could not
// even construct it (no add action), keeping marksDivergenceBounded
// green over a space that omitted the live falsifier class.
//
// PRE-FIX behavior modeled:
//   - calibVerifyLeaderOnly restores the `leadingOf("us", holder)`
//     gate on the verify pass;
//   - calibTickPreF1 restores the leader-gated verify-cadence cap and
//     carries NO peer-loop obligation (the pre-F1 model had none —
//     the peer's repair existed only as prose beside the sweep);
//   - externalAddAny joins the step (the falsifier was always
//     reachable in production; only the model was blind).
//
// PREDICTED falsification: nonLeaderDivergenceBounded. An external
// add lands on the standby; no edge fires, no generation moves, the
// leader-gated verify never reads this pod, the peer sweep is dormant
// (steady state spawns nothing) — the divergence ages past
// VERIFY_EVERY + TASK_TIMEOUT with the generations clean and no task,
// which production paid as a silently mis-routed leader-only Service
// (un-retryable Trailers-Only UNAVAILABLE on pinned dashboard
// sessions) until the next leadership transition.
//
// This pin is the non-vacuity evidence for quint-leader-marks's
// nonLeaderDivergenceBounded verdict: if it ever stops falsifying,
// the model has drifted away from the falsifier polarity the standby
// self-check exists to bound.
//
// Run (the wired pin — the leader-only-verify world falsifies):
//   quint verify --backend=tlc --main=leaseCalibF1StandbyAdd \
//     --step=calibStep --invariant=nonLeaderDivergenceBounded \
//     docs/spec/models/calibration/lease-f1-standby-add.qnt
module leaseCalibF1StandbyAdd {
  import leaderMarks(
    VERIFY_EVERY = 3,
    MAX_TIME = 10,
    MAX_STRIPS = 0,
    MAX_ADDS = 1,
    MAX_VERIFY_FAILS = 0,
    MAX_RECONCILE_FAILS = 2,
    MAX_HOLDER_CHANGES = 2,
    MAX_REBOUNDS = 0,
    TASK_TIMEOUT = 4
  ).* from "../leaderMarks"

  /// PRE-FIX verify: the leader-only gate restored — a standby never
  /// re-examines its own marks. (Multiple-keyed like production and
  /// the live model since F4 — the deleted mechanism here is the
  /// polarity gate only.)
  // Environment: the pre-F1 machinery restored as a regression pin — the deleted `now_leading` spawn gate / `leading=true` hardcode (rio-lease/src/lib.rs history; no live transition carries it)
  action calibVerifyLeaderOnly = all {
    leadingOf("us", holder),
    not(dirty),
    task == NoTask,
    clock % VERIFY_EVERY == 0,
    lastVerify != clock,
    lastVerify' = clock,
    marked' = if (wrongUnder(marks, holder, "us")) marked + 1 else marked,
    cleared' = cleared,
    holder' = holder, marks' = marks, task' = task, clock' = clock,
    strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails, holderChanges' = holderChanges,
    rebounds' = rebounds, clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// PRE-FIX tick: the verify-cadence cap gated on leading (a standby
  /// loop owed no verify), and NO peer-loop obligation (the pre-F1
  /// model carried the peer's repair as prose, not structure). The
  /// task-residency cap is KEPT: API deadlines existed pre-fix too;
  /// the multiple-keyed schedule is the live model's (F4) — the
  /// deleted mechanism here is the polarity gate only.
  // Environment: the pre-F1 loop schedule restored as a regression pin — the leader-gated cadence obligation (rio-lease/src/lib.rs history; the live loop verifies at both polarities)
  action calibTickPreF1 = all {
    clock < MAX_TIME,
    (leadingOf("us", holder) and not(dirty) and task == NoTask and clock % VERIFY_EVERY == 0) implies
      lastVerify == clock,
    (task != NoTask) implies (clock + 1 - taskSince <= TASK_TIMEOUT),
    clock' = clock + 1,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, task' = task,
    lastVerify' = lastVerify, strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock + 1),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// The live step with the standby self-check REMOVED (verify
  /// leader-gated, cadence cap leader-gated, no peer obligation) and
  /// the F1 falsifier ENABLED — everything else the live model's,
  /// including the fixed clear-through arithmetic. otherLoopConverge
  /// stays enabled-but-unforced: the pre-F1 peer still ran a loop;
  /// nothing obliged it to examine standby marks, and the checker
  /// finds the trace where it never does.
  action calibStep = any {
    calibTickPreF1,
    holderChangeAny,
    externalAddAny,
    otherLoopConverge,
    calibVerifyLeaderOnly,
    reconcileSpawn,
    reconcilePatched,
    reconcileClear,
    reconcileFail,
  }
}
