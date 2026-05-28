// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F4 (death
// attribution — the dedup-state lifecycle), representative: the
// establishment-window guard (the TTL discipline of the
// tick_sweep_recently_disconnected establishment vehicle — the second
// dedup-lifecycle half the retry close-out hands over). One file per
// representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF4EstablishBeforeWindow \
//     --step=calibStep --invariant=establishmentOnlyAfterWindowCloses \
//     docs/spec/models/calibration/executor-f4-establish-early.qnt

// =========================================================================
// CALIBRATION — pre-establishment-window-guard: the establishment sweep
// consumes correlation entries regardless of the 60 s TTL window — an
// unreported crash is established (charged, classified `unreported`,
// the slot added to the derivation's exclusion set) before the
// classifying report has had its window to arrive, racing the
// controller's report and double-classifying the same death (the charge
// arithmetic is the retry fold's; what Model S owns is that
// establishment fires only when the window closes). The override
// replaces the tick's establishment pass with the window-less variant;
// the latch keeps the as-built oracle (an entry established before its
// TTL expired). Everything else in the tick is as-built.
// PREDICTED: establishmentOnlyAfterWindowCloses falsifies — the
// counterexample is a mid-build pod death whose fresh (non-expired)
// correlation entry is established by the next leader tick.
// Constants are the documented fault-process regime's (a pod death is
// what mints the entry); its as-built baseline is the Stage-B record's
// deep-simulation + witness evidence (the exhaustive cfg is the open
// budget stop-and-report and is not re-attempted here).
// =========================================================================
module executorCalibF4EstablishBeforeWindow {
  import executorSessionAsBuilt(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 1,
    WEDGES = 1,
    FAILOVERS = 0,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 1,
    DUP_MSGS = 0,
    STALLS = 1,
    FUSE_FLIPS = 1,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).* from "../executorSessionAsBuilt"

  /// PRE-FIX handle_tick: identical to the as-built tick except that the
  /// establishment sweep consumes every present correlation entry,
  /// expired or not (no TTL window). The latch keeps the as-built
  /// oracle (an entry established before its window closed).
  action tickEstablishAnyEntry: bool = {
    val staleSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HStale)
    val freshSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HFresh)
    val afterReap = staleSlots.fold(st, (acc, e) => applyDisconnect(acc, e))
    // PRE-FIX: establish every present entry, not only the expired ones.
    val sweepable = EXECS.filter(e => afterReap.rd.get(e).present)
    val earlyEstablished = EXECS.filter(e =>
      afterReap.rd.get(e).present and not(afterReap.rd.get(e).expired))
    val afterSweep = sweepable.fold(afterReap, (acc, e) => establish(acc, e))
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
        establishedBeforeTtl: viol.establishedBeforeTtl or earlyEstablished.size() > 0,
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
          staleSlots.exists(e => {
            val dn = st.sched.get(e).running
            if (dn == "" or st.sched.get(e).lastCompleted == dn) false
            else and {
              st.drv.get(dn).resolution != RUnresolved,
              st.drv.get(dn).status == DAssigned or st.drv.get(dn).status == DRunning
            }
          }) },
      reach' = { ...reach,
        reapHappened: reach.reapHappened or staleSlots.size() > 0 }
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
      tickEstablishAnyEntry,
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
