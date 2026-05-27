// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F2 (outcome
// delivery, builder half — Model D), representative `8201db59b` /
// bug_117 — arm completion_pending before the first await and
// gracefully half-close the build stream before exit. One file per
// representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF2dLateArmNoHalfClose \
//     --step=calibStep --invariant=noExitWithReportOwed \
//     docs/spec/models/calibration/executor-f2d-late-arm.qnt

// =========================================================================
// CALIBRATION — pre-8201db59b (bug_012 / bug_117): completion_pending
// was stored only at send_completion time (after the build task's first
// await), so an exit racing the build task's terminal path — the
// panic-catcher's late completion in particular — found nothing owed
// and left; and the exit path dropped the stream as-is (no graceful
// half-close), so a report already relayed into the stream's channel
// but not yet flushed to the wire left with the process. The override
// removes the early arming from the accept path and the flush from the
// exit path. The latch keeps the as-built oracle (exit while a report
// is owed or unflushed) and adds the term the as-built early arming
// makes unreachable: an accepted build's report that was never even
// enqueued when the exit ran.
// PREDICTED: noExitWithReportOwed falsifies — the counterexample is the
// graceful exit taken between the build's terminal outcome and its
// enqueue (the bug_012 ordering), or with the report parked unflushed
// in the stream channel (the bug_117 ordering).
// Constants are the wired Model D base regime's; the wired
// quint-executor-delivery-base check is the as-built baseline.
// =========================================================================
module executorCalibF2dLateArmNoHalfClose {
  import executorDelivery(
    STREAM_CEIL = 2,
    STREAM_DEATHS = 0,
    OPEN_FAILS = 0,
    GEN_MOVES = 0,
    SIGKILLS = 0
  ).* from "../executorDelivery"

  /// PRE-FIX handleAssignment: acceptance does NOT arm
  /// completion_pending (it is only stored at send_completion time —
  /// the as-built enqueueSink).
  action handleAssignmentNoEarlyArm: bool = all {
    not(st.exited),
    st.asg == AsgInFlight,
    val stale = staleAssign(st.asgGen, st.watermark)
    val accept = and { not(stale), not(st.draining), st.slot == SlotIdle }
    all {
      st' = { ...st,
        asg: if (accept) AsgAccepted else AsgRejected,
        slot: if (accept) SlotClaimed else st.slot },
      viol' = { ...viol,
        staleAssignAccepted: viol.staleAssignAccepted or (accept and stale) },
      reach' = reach
    }
  }

  /// PRE-FIX gracefulExit: no half-close flush — the stream is dropped
  /// as-is on the way out — and the exit gates read the late-armed
  /// completion_pending. The latch keeps the as-built oracle and adds
  /// the never-enqueued term the as-built early arming makes
  /// unreachable.
  action gracefulExitNoHalfClose: bool = all {
    not(st.exited),
    or {
      and { st.buildDone, st.slot == SlotDone, not(st.completionPending) },
      and { st.draining, st.slot != SlotClaimed, not(st.completionPending) },
      and { st.asg != AsgAccepted, st.slot == SlotIdle, not(st.completionPending) }
    },
    st' = { ...st,
      relayTarget: 0,
      streamSt: if (st.streamSt == DSOpen) DSDead else st.streamSt,
      exited: true,
      exitedGracefully: true },
    viol' = { ...viol,
      exitWithReportOwed: viol.exitWithReportOwed or
        st.completionPending or st.sink or st.relayBuf or st.inFlightStream != 0 or
        (st.asg == AsgAccepted and not(st.sinkEverQueued)) },
    reach' = reach
  }

  action calibStep = any {
    streamOpenStart,
    streamOpenOk,
    streamOpenFail,
    streamDie,
    assignmentArrive,
    handleAssignmentNoEarlyArm,
    buildEnds,
    enqueueSink,
    slotRelease,
    relayHandoff,
    relayRebuffer,
    endpointReceive,
    sigtermArrives,
    genMove,
    observeGen,
    gracefulExitNoHalfClose,
    sigkill
  }
}
