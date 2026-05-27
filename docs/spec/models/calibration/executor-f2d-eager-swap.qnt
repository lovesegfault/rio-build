// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F2 (outcome
// delivery, builder half — Model D), representative `6b6cfcf10` — swap
// the relay target only after build_execution returns Ok
// (merged_bug_020). One file per representative per
// calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF2dEagerSwap \
//     --step=calibStep --invariant=reportSurvivesStreamChurn \
//     docs/spec/models/calibration/executor-f2d-eager-swap.qnt

// =========================================================================
// CALIBRATION — pre-6b6cfcf10 (merged_bug_020): the reconnect loop
// swapped the relay target onto the new stream attempt at request time,
// before the open confirmed. The relay then handed the owed completion
// into a channel whose stream the scheduler never accepted (leader
// settling, balance channel stale); when that open failed, the channel
// — and the report in it — was dropped while the builder lived on,
// convinced it had delivered. The override makes exactly that ordering
// available: the eager swap at request time, the relay hand-off into a
// not-yet-confirmed target, and the open-failure dropping the cell.
// The latches keep the as-built oracle: relayedIntoUnconfirmed (the
// hand-off into a never-confirmed stream) and reportLostWhileOwed (the
// owed report vanished while the process lived — settable only by this
// pre-fix ordering; the as-built loss classes are a confirmed stream's
// death or a pod death, both of which the scheduler observes and
// repairs, which is why the as-built actions never set it).
// PREDICTED: reportSurvivesStreamChurn falsifies (the design row);
// relayOnlyIntoConfirmed falsifies on the same trace one step earlier.
// Constants are the wired Model D fault-stream regime's; the wired
// quint-executor-delivery-fault-stream check is the as-built baseline.
// =========================================================================
module executorCalibF2dEagerSwap {
  import executorDelivery(
    STREAM_CEIL = 3,
    STREAM_DEATHS = 2,
    OPEN_FAILS = 1,
    GEN_MOVES = 1,
    SIGKILLS = 0
  ).* from "../executorDelivery"

  /// PRE-FIX streamOpenStart: the relay target is swapped onto the new
  /// attempt at request time (before the open confirms).
  action streamOpenStartEagerSwap: bool = all {
    not(st.exited),
    st.streamSt == DSNone or st.streamSt == DSDead,
    st.streamsOpened < STREAM_CEIL,
    not(st.draining and st.slot != SlotClaimed and not(st.completionPending)),
    st' = { ...st,
      streamSt: DSOpening,
      streamId: st.streamsOpened + 1,
      streamsOpened: st.streamsOpened + 1,
      relayTarget: st.streamsOpened + 1 },
    viol' = viol,
    reach' = reach
  }

  /// PRE-FIX relayHandoff: the relay pumps into whatever target it
  /// holds, confirmed or not. The as-built latch (hand-off into a
  /// stream that is not confirmed open) is unchanged and now fires.
  action relayHandoffAnyTarget: bool = all {
    not(st.exited),
    st.relayTarget != 0,
    st.relayTarget == st.streamId,
    st.streamSt == DSOpening or st.streamSt == DSOpen,
    st.inFlightStream == 0,
    st.sink or st.relayBuf,
    st' = { ...st,
      sink: false,
      relayBuf: false,
      inFlightStream: st.relayTarget,
      completionPending: false },
    viol' = { ...viol,
      relayedIntoUnconfirmed: viol.relayedIntoUnconfirmed or st.streamSt != DSOpen },
    reach' = reach
  }

  /// PRE-FIX streamOpenFail: an open attempt that fails while the
  /// eagerly-swapped relay already handed it the report drops the
  /// in-flight cell with the channel — the owed report vanishes while
  /// the process lives (the as-built oracle's reportLostWhileOwed
  /// latch).
  action streamOpenFailDropsCell: bool = all {
    st.streamSt == DSOpening,
    st.openFailsLeft > 0,
    val lostCell = st.inFlightStream == st.streamId
    all {
      st' = { ...st,
        streamSt: DSDead,
        relayTarget: 0,
        openFailsLeft: st.openFailsLeft - 1,
        inFlightStream: if (lostCell) 0 else st.inFlightStream },
      viol' = { ...viol,
        reportLostWhileOwed: viol.reportLostWhileOwed or
          (lostCell and st.delivered == 0) },
      reach' = reach
    }
  }

  action calibStep = any {
    streamOpenStartEagerSwap,
    streamOpenOk,
    streamOpenFailDropsCell,
    streamDie,
    assignmentArrive,
    handleAssignment,
    buildEnds,
    enqueueSink,
    slotRelease,
    relayHandoffAnyTarget,
    relayRebuffer,
    endpointReceive,
    sigtermArrives,
    genMove,
    observeGen,
    gracefulExit,
    sigkill
  }
}
