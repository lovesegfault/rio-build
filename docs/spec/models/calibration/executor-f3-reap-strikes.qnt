// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F3 (liveness
// calibration), representative `5971778f8` — heartbeat reap at 30 s,
// not 2× (the per-executor reap-bound half; the handle_tick leader-gate
// half of the same commit is dispositioned by analogy with the F6
// representative, whose override removes a leader gate). One file per
// representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF3ReapCounterNotReached \
//     --step=calibStep --invariant=silentSlotReapArmed \
//     docs/spec/models/calibration/executor-f3-reap-strikes.qnt

// =========================================================================
// CALIBRATION — pre-5971778f8: tick_check_heartbeats used the 30 s
// timeout as a per-tick increment gate AND required a separate
// missed-heartbeat counter to reach its threshold before disconnecting,
// applying the ×3 twice — a worker-time-stale slot observed by a reaper
// tick was NOT reaped by that tick (the counter had not accumulated
// yet), so dead workers' claims sat unrepaired for ~2× the normative
// window. At model resolution (bucketed worker time, no per-slot strike
// counter) the pre-fix shape is the observing tick that leaves the
// stale slot in place: the reap pass skips worker-time-stale slots; the
// rest of the tick is as-built. The latch keeps the as-built oracle
// (a worker-time-stale slot survived a leader reaper tick). The
// numeric 30-vs-60 s half is below bucketed-time resolution and is
// covered by `sched.executor.liveness-window` plus the lifecycle VM
// scenario the fix itself extended.
// PREDICTED: silentSlotReapArmed falsifies (the design row's
// enabled-implies-fires alternative; noReapWhileFreshInWorkerTime is
// the family's other half, carried by the 1757790f2 representative).
// Constants are the wired base regime's; the wired
// quint-executor-session-base check is the as-built baseline.
// =========================================================================
module executorCalibF3ReapCounterNotReached {
  import executorSessionAsBuilt(
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
  ).* from "../executorSessionAsBuilt"

  /// PRE-FIX handle_tick: identical to the as-built tick except that the
  /// reap pass does not disconnect worker-time-stale slots (the pre-fix
  /// missed-heartbeat counter has not reached its threshold on the
  /// observing tick). The latch formulas keep the as-built oracle.
  action tickReapCounterNotReached: bool = {
    val staleSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HStale)
    val freshSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HFresh)
    // PRE-FIX (5971778f8): the observing tick reaps nothing.
    val afterReap = st
    val expired = EXECS.filter(e => afterReap.rd.get(e).present and afterReap.rd.get(e).expired)
    val afterSweep = expired.fold(afterReap, (acc, e) => establish(acc, e))
    val wedgedDrvs = DRVS.filter(d =>
      afterSweep.drv.get(d).status == DRunning and afterSweep.drv.get(d).wedged)
    val afterBackstop = wedgedDrvs.fold(afterSweep, (acc, d) => {
      val ex = acc.drv.get(d).assignedExec
      val acc1 =
        if (ex == "") acc
        else if (acc.sched.get(ex).present)
          { ...acc, sched: acc.sched.set(ex, { ...acc.sched.get(ex),
              running: if (acc.sched.get(ex).running == d) "" else acc.sched.get(ex).running,
              drainingAdmin: true }) }
        else acc
      requeue(acc1, d)
    })
    val afterCells = { ...afterBackstop,
      drv: afterBackstop.drv.keys().mapBy(d => {
        val rec = afterBackstop.drv.get(d)
        if (rec.cellArmed and rec.status == DCompleted) { ...rec, cellArmed: false } else rec
      }) }
    val afterDispatch =
      if (not(afterCells.recoveryComplete)) afterCells
      else dispatchRound(dispatchRound(afterCells))
    val final = { ...afterDispatch,
      drv: afterDispatch.drv.keys().mapBy(d => {
        val rec = afterDispatch.drv.get(d)
        if (and {
          rec.status == DReady,
          rec.attempt < DISPATCH_CEIL,
          afterCells.recoveryComplete,
          EXECS.exists(e => eligibleFor(afterDispatch, e, d))
        })
          { ...rec, starved: if (rec.starved <= STARVE_BOUND) rec.starved + 1 else rec.starved }
        else { ...rec, starved: 0 }
      }) }
    all {
      st.believedLeader,
      st' = final,
      viol' = { ...viol,
        reapedFreshSlot: viol.reapedFreshSlot or
          freshSlots.exists(e => not(final.sched.get(e).present)),
        staleSlotSurvivedTick: viol.staleSlotSurvivedTick or
          staleSlots.exists(e => final.sched.get(e).present),
        offeredUnrunnable: viol.offeredUnrunnable or
          DRVS.exists(d => and {
            final.drv.get(d).status == DAssigned,
            st.drv.get(d).status == DReady,
            val ex = final.drv.get(d).assignedExec
            if (ex == "") false else not(eligibleFor(afterCells, ex, d))
          }),
        offeredUnregistered: viol.offeredUnregistered or
          DRVS.exists(d => and {
            final.drv.get(d).status == DAssigned,
            st.drv.get(d).status == DReady,
            val ex = final.drv.get(d).assignedExec
            if (ex == "") false else not(isRegistered(afterCells.sched.get(ex)))
          }),
        starvedPastBound: viol.starvedPastBound or
          DRVS.exists(d => final.drv.get(d).starved > STARVE_BOUND),
        doubleResolution: viol.doubleResolution or
          wedgedDrvs.exists(d => afterSweep.drv.get(d).resolution != RUnresolved) },
      reach' = { ...reach,
        reapHappened: reach.reapHappened or false }
    }
  }

  action calibStep = {
    nondet e = EXECS.oneOf()
    nondet d = DRVS.oneOf()
    nondet ok = Set(true, false).oneOf()
    any {
      connectAccept(e, d),
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
      tickReapCounterNotReached,
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
