// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F6 (failover
// convergence), representative `c5c5ccd17` — leader-gate
// reassign_derivations on worker disconnect. One file per
// representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF6DeposedReassign \
//     --step=calibStep --invariant=deposedLeaderSessionEventsAreInert \
//     docs/spec/models/calibration/executor-f6-deposed-reassign.qnt

// =========================================================================
// CALIBRATION — pre-c5c5ccd17: the ExecutorDisconnected arm's shared
// tail reassign_derivations was not leader-gated, so a deposed replica
// that had already observed the loss still wrote PG from its stale DAG
// when a worker-stream reader (tripped by the generation fence at the
// lease flip itself) delivered its disconnect: the reset branch
// persisted Ready over the new leader's recovery, the poison branch
// finalized rows the new leader immediately re-runs. The override keeps
// the as-built epoch handling and in-memory bookkeeping (those arms
// stay ungated in production too) and removes ONLY the leader gate on
// the requeue tail; the latch keeps the as-built oracle (a durable
// status write performed after this replica has observed deposition).
// PREDICTED: deposedLeaderSessionEventsAreInert falsifies — the
// counterexample is the deposed replica requeueing the in-flight claim
// from its stale DAG when the post-loss disconnect is delivered.
// Constants are the documented fault-leader regime's; its as-built
// baseline is the Stage-B record's bounded-exhaustive re-run +
// witness evidence (the regime is a documented manual target on
// per-check cost grounds and is not re-attempted exhaustively here).
// =========================================================================
module executorCalibF6DeposedReassign {
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
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).* from "../executorSessionAsBuilt"

  /// PRE-FIX deliverDisconnect: identical to the as-built action (epoch
  /// staleness handling, entry removal, correlation insert) except that
  /// the reassign tail runs without the leader gate — a replica that
  /// has observed deposition still requeues (persists Ready over) the
  /// disconnected worker's in-flight claim from its stale DAG.
  action deliverDisconnectUngatedReassign(e: str): bool = {
    val s0 = st.sched.get(e)
    val ep = leastInt(s0.pendingDisc)
    val stale = not(s0.present) or s0.streamEpoch != ep
    val cleared = { ...s0, pendingDisc: s0.pendingDisc.exclude(Set(ep)) }
    val midBuild = s0.running != "" and s0.lastCompleted != s0.running
    val dn = s0.running
    val inflight =
      if (dn == "") false
      else st.drv.get(dn).status == DAssigned or st.drv.get(dn).status == DRunning
    all {
      s0.pendingDisc.size() > 0,
      if (stale) all {
        st' = { ...st, sched: st.sched.set(e, cleared) },
        viol' = viol,
        reach' = reach
      } else all {
        st' = {
          val st1 = {
            ...st,
            sched: st.sched.set(e, emptySched),
            drv: st.drv.keys().mapBy(d =>
              val rec = st.drv.get(d)
              if (s0.authIntent == d) { ...rec, cellArmed: false } else rec),
            rd: if (midBuild)
                  st.rd.set(e, {
                    present: true, drv: dn,
                    attempt: st.drv.get(dn).attempt, expired: false })
                else st.rd
          }
          // PRE-FIX (c5c5ccd17): reassign_derivations runs regardless of
          // is_leader() — the requeue (a durable status write) is taken
          // even after this replica observed the loss.
          if (dn != "") requeue(st1, dn) else st1
        },
        viol' = { ...viol,
          durableWriteWhileDeposed: viol.durableWriteWhileDeposed or
            (not(st.believedLeader) and inflight) },
        reach' = reach
      }
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
      deliverDisconnectUngatedReassign(e),
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
