// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F1 (session
// identity), representative `a6697c6b0` — the reconnect hijack
// accept-gate (ExecutorClaims kind binding + the oneshot accept/reject
// reply that keeps a rejected stream's reader from ever serving the
// slot). One file per representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF1HijackAccepted \
//     --step=calibStep --invariant=atMostOneLiveStreamPerExecutor \
//     docs/spec/models/calibration/executor-f1-hijack-accept.qnt

// =========================================================================
// CALIBRATION — pre-a6697c6b0 (bug_038/081): build_execution spawned the
// stream reader before learning the actor's accept/reject decision, so a
// connect that the live-stream hijack guard should have rejected still
// ended up with a live reader bound to the executor's session — two live
// streams serving one executor id. At model resolution the accepted
// connect IS "the reader is live and events flow", so the pre-fix shape
// is the accept path with the live-stream hijack guard removed; the
// as-built latch inside the action (an accept while the existing binding
// is still live) is the oracle and stays unchanged.
// PREDICTED: atMostOneLiveStreamPerExecutor falsifies — a second accept
// lands on a slot whose stream is still live.
// Constants are the wired base regime's; the wired
// quint-executor-session-base check is the as-built baseline.
// =========================================================================
module executorCalibF1HijackAccepted {
  import executorSession(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 0,
    WEDGES = 0,
    FAILOVERS = 0,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 0,
    DUP_MSGS = 0,
    STALLS = 1,
    FUSE_FLIPS = 2,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).* from "../executorSession"

  /// PRE-FIX connectAccept: the live-stream hijack guard is gone — a new
  /// stream is accepted even while the existing binding is still live
  /// (the pre-fix reader was spawned before the reject decision). The
  /// intent-mismatch guard and the rest of the accept path are the
  /// as-built ones; the latch formula is unchanged (it records exactly
  /// the accept-while-live case the guard exists to prevent).
  action connectAcceptNoLiveGuard(e: str, presented: str): bool = {
    val s0 = st.sched.get(e)
    val isReconnect = s0.present
    val newEpoch = s0.connects + 1
    all {
      st.bld.get(e).alive,
      s0.connects < EPOCH_CEIL,
      st.believedLeader,
      // PRE-FIX (a6697c6b0): no `s0.stream != SOpen` hijack guard.
      or { s0.authIntent == "", presented == "", presented == s0.authIntent },
      st' = { ...st,
        sched: st.sched.set(e, { ...s0,
          present: true,
          stream: SOpen,
          streamEpoch: newEpoch,
          connects: s0.connects + 1,
          authIntent: if (s0.authIntent == "") presented else s0.authIntent,
          intentId: if (presented != "") presented else s0.intentId,
          drainingAdmin: if (isReconnect) false else s0.drainingAdmin,
          storeDegraded: if (isReconnect) false else s0.storeDegraded,
          warm: if (s0.systemsKnown) true else s0.warm }) },
      viol' = { ...viol,
        secondLiveStream: viol.secondLiveStream or (isReconnect and s0.stream == SOpen) },
      reach' = reach
    }
  }

  action calibStep = {
    nondet e = EXECS.oneOf()
    nondet d = DRVS.oneOf()
    nondet ok = Set(true, false).oneOf()
    any {
      connectAcceptNoLiveGuard(e, d),
      streamHalfDie(e),
      streamDrop(e),
      builderExit(e),
      deliverDisconnect(e),
      heartbeat(e),
      hbAge(e),
      actorStall,
      builderRecvAssign(e),
      loseAssignMsg(e),
      dupAssignDeliver(e),
      rollbackAssign(e),
      builderComplete(e, ok),
      builderWedge(e),
      builderSigterm(e),
      builderFuseFlip(e),
      podDie(e),
      controllerReport(e),
      rdExpire(e),
      adminDrain(e, false),
      adminDrain(e, true),
      tick,
      drvStartsRunning(d),
      persistFault(d),
      depose,
      workerObservesNewLeader(e),
      observeLoss,
      reacquire,
      reconcileSweep
    }
  }
}
