// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F2 (outcome
// delivery, scheduler half), representative `0127cf854` — drain phantom
// running_builds on the second heartbeat miss. One file per
// representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF2PhantomNoDrain \
//     --step=calibStep --invariant=confirmedPhantomIsDrained \
//     docs/spec/models/calibration/executor-f2-phantom-no-drain.qnt
//
// Note on the oracle: the design pre-registered
// unresolvedClaimHasRepairArmed as this row's falsification. The
// as-built encoding counts the slot binding itself as an armed repair
// (the heartbeat-reconcile / phantom / reaper family acts on what it
// binds), so removing only the drain leaves that disjunct satisfied and
// the armed-safety invariant HOLDS — the bug class lives one level
// down, in the obligation that the binding-holding family actually
// resolves what it binds. That obligation is the
// `confirmedPhantomIsDrained` invariant added to the as-built Model S (now frozen as executorSessionAsBuilt.qnt) at
// Stage C (by construction on the as-built model: the confirming
// heartbeat always clears the binding); this override is what falsifies
// it. The prediction-vs-verdict correction is recorded in the
// calibration table.

// =========================================================================
// CALIBRATION — pre-0127cf854 (I-035): a phantom running_build (an
// assignment the scheduler recorded but the worker never reports — a
// push lost to a rejected/never-claimed delivery) was never drained: the
// worker's consecutive heartbeats kept omitting it and the slot kept the
// binding, so the claim never resolved and the slot looked permanently
// full to dispatch. The fix made the second consecutive miss drain the
// phantom (clear the binding and requeue the still-in-flight claim).
// The override keeps the as-built suspect marking (the strike-counting
// bookkeeping the oracle needs — without it "second consecutive miss"
// is not expressible at model resolution) and removes the resolution:
// the confirming heartbeat neither clears the binding nor requeues.
// PREDICTED: confirmedPhantomIsDrained falsifies (the confirming
// heartbeat retains the phantom binding); unresolvedClaimHasRepairArmed
// stays green over the same alphabet (the slot-binding disjunct), which
// is the recorded checked-prediction correction.
// Constants are the wired base regime's (the phantom is constructible
// there via the SIGTERM-rejected push); the wired
// quint-executor-session-base check is the as-built baseline.
// =========================================================================
module executorCalibF2PhantomNoDrain {
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

  /// PRE-FIX heartbeat: the accepted-heartbeat path with the
  /// confirmed-phantom resolution removed — the suspect is still marked
  /// (strike bookkeeping, kept as the oracle's counting device) but the
  /// confirming heartbeat neither clears the slot's running_build nor
  /// requeues the claim. Everything else (registration edge, flag
  /// refresh, adopt, intent downgrade, generation latch) is as-built.
  action heartbeatNoPhantomDrain(e: str): bool = {
    val s0 = st.sched.get(e)
    val b = st.bld.get(e)
    val reported = b.claimed
    val wasRegistered = isRegistered(s0)
    val prevKept =
      if (s0.running == "") ""
      else if (and {
        st.drv.get(s0.running).status == DAssigned or st.drv.get(s0.running).status == DRunning,
        st.drv.get(s0.running).assignedExec == e
      }) s0.running else ""
    val adopting = prevKept == "" and reported != "" and not(s0.drainingAdmin)
    val reconciled = if (prevKept != "") prevKept else reported
    val suspect = if (reconciled != "" and reported != reconciled) reconciled else ""
    val confirmedPhantom = suspect != "" and s0.phantomSuspect == suspect
    // PRE-FIX (0127cf854): no second-strike resolution — the binding is
    // retained on confirmation and nothing is requeued.
    val newRunning = reconciled
    val adoptDrv =
      if (not(adopting)) ""
      else if (st.drv.get(reported).status == DReady) reported else ""
    all {
      st.bld.get(e).alive,
      st.believedLeader,
      s0.present,
      st' = { ...st,
        sched: st.sched.set(e, { ...s0,
          systemsKnown: true,
          hb: HFresh,
          running: newRunning,
          drainingHb: b.sigterm,
          storeDegraded: b.fuseDegraded,
          intentId:
            if (s0.intentId == "") ""
            else if (st.drv.get(s0.intentId).status == DReady) s0.intentId else "",
          phantomSuspect: suspect,
          warm: if (not(wasRegistered) and s0.stream != SNone) true else s0.warm }),
        drv:
          if (adoptDrv == "") st.drv
          else st.drv.set(adoptDrv, { ...st.drv.get(adoptDrv),
            status: DAssigned, assignedExec: e,
            resolution: RUnresolved }),
        bld: st.bld.set(e, { ...b,
          observedGen: if (st.generation > b.observedGen) st.generation else b.observedGen })
      },
      // The as-built oracle: the confirming heartbeat retained the
      // phantom binding.
      viol' = { ...viol,
        confirmedPhantomRetained: viol.confirmedPhantomRetained or
          (confirmedPhantom and suspect != "" and newRunning == suspect) },
      reach' = { ...reach,
        adoptHappened: reach.adoptHappened or adoptDrv != "" }
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
      heartbeatNoPhantomDrain(e),
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
