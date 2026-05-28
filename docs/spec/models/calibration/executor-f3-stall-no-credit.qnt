// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F3 (liveness
// calibration), representative `1757790f2` — credit all 8 actor-loop
// FMP stall sites. One file per representative per
// calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF3StallNoCredit \
//     --step=calibStep --invariant=noReapWhileFreshInWorkerTime \
//     docs/spec/models/calibration/executor-f3-stall-no-credit.qnt

// =========================================================================
// CALIBRATION — pre-1757790f2 (the I-178 family): six of the eight
// inline FindMissingPaths awaits in the single-threaded actor loop were
// not credited via credit_heartbeats_for_stall, so an actor-side stall
// consumed the fleet's heartbeat window — the next reaper tick measured
// scheduler time, not worker time, and reaped workers that had been
// heartbeating within their own clock ("three uncredited 30 s stalls is
// enough to reap the fleet on a healthy-but-slow store"). In the model
// the worker-time bucket IS the credit discipline (a stall never ages a
// bucket), so the pre-fix shape is the reaper tick that, in a behaviour
// where an actor stall has occurred, also reaps slots that are fresh in
// worker time. The latch keeps the as-built oracle (a worker-time-fresh
// slot was reaped).
// PREDICTED: noReapWhileFreshInWorkerTime falsifies — the
// counterexample is a stall followed by a reaper tick that removes a
// fresh, live, heartbeating worker's slot.
// Constants are the wired base regime's (one stall is in its structural
// budgets); the wired quint-executor-session-base check is the as-built
// baseline.
// =========================================================================
module executorCalibF3StallNoCredit {
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

  /// PRE-FIX handle_tick: identical to the as-built tick except that
  /// when an actor stall has occurred in this behaviour (the credit for
  /// it is missing), the reap pass treats worker-time-fresh slots as
  /// timed out too — scheduler-time measurement instead of worker-time.
  /// The latch formulas keep the as-built oracle (fresh = the as-built
  /// worker-time bucket).
  action tickStallConsumedBudget: bool = {
    val staleSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HStale)
    val freshSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HFresh)
    val stallHappened = st.stallsLeft < STALLS
    // PRE-FIX (1757790f2): an uncredited stall consumed the heartbeat
    // window — fresh slots are reaped along with the stale ones.
    val reapSet = if (stallHappened) staleSlots.union(freshSlots) else staleSlots
    val afterReap = reapSet.fold(st, (acc, e) => applyDisconnect(acc, e))
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
          wedgedDrvs.exists(d => afterSweep.drv.get(d).resolution != RUnresolved) or
          reapSet.exists(e => {
            val dn = st.sched.get(e).running
            if (dn == "" or st.sched.get(e).lastCompleted == dn) false
            else and {
              st.drv.get(dn).resolution != RUnresolved,
              st.drv.get(dn).status == DAssigned or st.drv.get(dn).status == DRunning
            }
          }) },
      reach' = { ...reach,
        reapHappened: reach.reapHappened or reapSet.size() > 0 }
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
      tickStallConsumedBudget,
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
