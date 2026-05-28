// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F2 (outcome
// delivery, scheduler half), representative `be3ad068e` — adopt the
// reconnecting worker's in-flight build into the DAG (the I-066
// lifecycle half; the I-062 FOD resource-fit half is SLA content,
// outside Model S). One file per representative per
// calibration/README.md.
//
// Run (serially; this override's predicted-falsification check runs to
// exhaustion because the analysis expects HOLDS — see the header note):
//   quint verify --backend=tlc --main=executorCalibF2NoAdopt \
//     --step=calibStep --invariant=allInvariants \
//     docs/spec/models/calibration/executor-f2-no-adopt.qnt
//   quint verify --backend=tlc --main=executorCalibF2NoAdopt \
//     --step=calibStep --invariant=canReachOrphanedInflight \
//     docs/spec/models/calibration/executor-f2-no-adopt.qnt   # expect violation
//
// Note on the oracle: the design pre-registered
// unresolvedClaimHasRepairArmed / convergenceToGroundTruth as this
// row's falsification. With the adopt arm removed the orphaned
// in-flight build's derivation is READY (recovery already reset it), so
// it is not a claim the armed-safety invariant ranges over, and the
// as-built first-to-resolve guards absorb the duplicate outcome the
// re-dispatch produces — the safety set is expected to HOLD. What the
// adopt arm protects at model resolution is economy (the still-running
// build is re-learned instead of re-run) and execution correlation, not
// a safety invariant; the prediction-vs-verdict correction and the
// resulting disposition are recorded in the calibration table.

// =========================================================================
// CALIBRATION — pre-be3ad068e (I-066): a worker that reconnects while
// still running a build the scheduler no longer tracks (recovery /
// reconcile reset it after the disconnect) was not re-adopted into the
// DAG: the scheduler re-dispatched the derivation while the original
// worker kept building it, and the original worker's eventual report
// found no claim to resolve. The override removes ONLY the DAG-side
// adopt arm of the heartbeat reconcile (the slot-side learn and the
// rest of the accepted-heartbeat path stay as-built).
// PREDICTED (design): unresolvedClaimHasRepairArmed /
// convergenceToGroundTruth. EXPECTED (analysis, recorded before the
// run): allInvariants HOLDS over this alphabet; the module-local
// canReachOrphanedInflight witness pins the contended state (a Ready
// derivation still being built by a live worker) reachable so the HOLDS
// verdict is about the guards, not about vacuity.
// Constants: the fault-leader regime narrowed to the failover budget
// alone (admin-drain / SIGTERM / stall / FUSE budgets zeroed) so the
// exhaustive HOLDS verdict is reachable inside the calibration budget;
// the as-built baseline at these constants is subsumed by the Stage-B
// fault-leader evidence (same alphabet, strictly smaller budgets).
// =========================================================================
module executorCalibF2NoAdopt {
  import executorSessionAsBuilt(
    EXECS = Set("e1", "e2"),
    DRVS = Set("d1"),
    EPOCH_CEIL = 2,
    DISPATCH_CEIL = 2,
    STARVE_BOUND = 2,
    POD_DEATHS = 0,
    WEDGES = 0,
    FAILOVERS = 1,
    PERSIST_FAULTS = 0,
    MSG_LOSS = 0,
    SEND_FAILS = 0,
    HALF_DEATHS = 0,
    STREAM_DROPS = 0,
    HB_DROPS = 0,
    DUP_MSGS = 0,
    STALLS = 0,
    FUSE_FLIPS = 0,
    ADMIN_DRAINS = 0,
    SIGTERMS = 0
  ).* from "../executorSessionAsBuilt"

  /// The contended state the adopt arm exists for: a derivation the
  /// scheduler holds Ready while a live worker is still building it.
  /// Expect-violation (the state must stay reachable in this module's
  /// alphabet for the HOLDS verdict above to be meaningful).
  val canReachOrphanedInflight =
    not(DRVS.exists(d => EXECS.exists(e => and {
      st.drv.get(d).status == DReady,
      st.bld.get(e).alive,
      st.bld.get(e).claimed == d
    })))

  /// PRE-FIX heartbeat: the accepted-heartbeat path with the DAG-side
  /// adopt arm removed — a worker-reported build whose derivation is
  /// Ready is NOT re-claimed into the DAG. The slot-side reconcile
  /// (keep / suspect / two-strike drain) and everything else stay
  /// as-built.
  action heartbeatNoAdopt(e: str): bool = {
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
    val reconciled = if (prevKept != "") prevKept else reported
    val suspect = if (reconciled != "" and reported != reconciled) reconciled else ""
    val confirmedPhantom = suspect != "" and s0.phantomSuspect == suspect
    val newRunning = if (confirmedPhantom) "" else reconciled
    // PRE-FIX (be3ad068e): no adopt_heartbeat_build — the DAG never
    // re-learns the orphaned in-flight build.
    val drainPhantom = and {
      confirmedPhantom,
      st.believedLeader,
      st.drv.get(suspect).assignedExec == e,
      st.drv.get(suspect).status == DAssigned or st.drv.get(suspect).status == DRunning
    }
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
          phantomSuspect: if (confirmedPhantom) "" else suspect,
          warm: if (not(wasRegistered) and s0.stream != SNone) true else s0.warm }),
        drv:
          if (drainPhantom)
            st.drv.set(suspect, { ...st.drv.get(suspect),
              status: DReady, pgStatus: DReady, assignedExec: "",
              resolution:
                if (st.drv.get(suspect).resolution == RUnresolved) RReturned
                else st.drv.get(suspect).resolution })
          else st.drv,
        bld: st.bld.set(e, { ...b,
          observedGen: if (st.generation > b.observedGen) st.generation else b.observedGen })
      },
      viol' = { ...viol,
        doubleResolution: viol.doubleResolution or
          (drainPhantom and st.drv.get(suspect).resolution != RUnresolved) },
      reach' = { ...reach,
        phantomDrained: reach.phantomDrained or confirmedPhantom }
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
      heartbeatNoAdopt(e),
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
