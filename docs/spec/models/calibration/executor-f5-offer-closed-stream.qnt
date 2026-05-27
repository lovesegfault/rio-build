// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F5
// (eligibility coherence), representative `96d8092b8` — skip
// closed-stream executors in dispatch (the I-095 arm). One file per
// representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF5OfferClosedStream \
//     --step=calibStep --invariant=neverOfferUnrunnableWork \
//     docs/spec/models/calibration/executor-f5-offer-closed-stream.qnt

// =========================================================================
// CALIBRATION — pre-96d8092b8 (I-095): an executor whose assignment
// stream had closed under it (the half-dead binding: stream_tx closed,
// entry retained) stayed dispatchable until phantom detection — every
// dispatch tick re-picked the dead executor for every ready derivation,
// the push went into a closed channel, and the single-threaded actor
// saturated on channel-closed churn. The override gives the tick's
// dispatch pass the pre-fix eligibility (no closed-stream exclusion);
// the latch keeps the as-built oracle (the as-built rejection_reason /
// has_capacity view of the chosen slot, which excludes closed streams).
// PREDICTED: neverOfferUnrunnableWork falsifies — the counterexample is
// a half-dead slot receiving a fresh assignment.
// Constants are the wired fault-stream-conn regime's (the regime that
// makes the half-dead binding reachable); its as-built baseline is the
// Stage-B record's deep-simulation + witness evidence (the exhaustive
// cfg is the open budget stop-and-report and is not re-attempted here).
// =========================================================================
module executorCalibF5OfferClosedStream {
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
    HALF_DEATHS = 1,
    STREAM_DROPS = 1,
    HB_DROPS = 1,
    DUP_MSGS = 0,
    STALLS = 1,
    FUSE_FLIPS = 2,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).* from "../executorSession"

  /// PRE-FIX eligibility: the as-built hard_filter without the I-095
  /// closed-stream exclusion — a half-dead (SClosedTx) binding is still
  /// offered work.
  pure def eligiblePreFix(stx: SState, e: str, dn: str): bool = and {
    isRegistered(stx.sched.get(e)),
    not(isDraining(stx.sched.get(e))),
    not(stx.sched.get(e).storeDegraded),
    stx.sched.get(e).running == "",
    stx.sched.get(e).intentId == "" or stx.sched.get(e).intentId == dn,
    not(stx.drv.get(dn).excluded.contains(e))
  }

  /// One greedy round of the pre-fix dispatch pass (the as-built
  /// dispatchRound with the pre-fix eligibility).
  pure def dispatchRoundPreFix(stx: SState): SState =
    val cands = DRVS.filter(d => dispatchable(stx, d) and EXECS.exists(e => eligiblePreFix(stx, e, d)))
    if (cands.size() == 0) stx
    else
      val d = pickStr(cands)
      val e = pickStr(EXECS.filter(e2 => eligiblePreFix(stx, e2, d)))
      { ...stx,
        drv: stx.drv.set(d, { ...stx.drv.get(d),
          status: DAssigned, pgStatus: DAssigned, assignedExec: e,
          attempt: stx.drv.get(d).attempt + 1, resolution: RUnresolved,
          starved: 0 }),
        sched: stx.sched.set(e, { ...stx.sched.get(e), running: d }),
        bld: stx.bld.set(e, { ...stx.bld.get(e), pendingAssign: d }) }

  /// PRE-FIX handle_tick: identical to the as-built tick except that the
  /// dispatch pass uses the pre-fix eligibility. The latch formulas keep
  /// the as-built oracle (the as-built eligibleFor / isRegistered view
  /// of every slot the pass assigned to).
  action tickDispatchToClosedStream: bool = {
    val staleSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HStale)
    val freshSlots = EXECS.filter(e => st.sched.get(e).present and st.sched.get(e).hb == HFresh)
    val afterReap = staleSlots.fold(st, (acc, e) => applyDisconnect(acc, e))
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
    // PRE-FIX (96d8092b8): the dispatch pass has no closed-stream
    // exclusion.
    val afterDispatch =
      if (not(afterCells.recoveryComplete)) afterCells
      else dispatchRoundPreFix(dispatchRoundPreFix(afterCells))
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
      tickDispatchToClosedStream,
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
