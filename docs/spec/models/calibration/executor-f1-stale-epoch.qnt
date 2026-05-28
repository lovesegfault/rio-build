// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F1 (session
// identity), representative `db457374f` — stream_epoch atomicity +
// heartbeat auth_intent binding (the lifecycle halves; the
// deadline/backstop accounting halves are retry-table rows). Layout per
// calibration/README.md: one file per representative, instantiating the
// as-built executorSession model and replacing ONE action with its
// PRE-FIX variant behind a `calibStep`; the violation latches keep the
// as-built oracle.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF1StaleEpochApplies \
//     --step=calibStep --invariant=staleStreamEventsAreInert \
//     docs/spec/models/calibration/executor-f1-stale-epoch.qnt

// =========================================================================
// CALIBRATION — pre-db457374f (bug_008/010 family, the I-056a class):
// stream_epoch was written before the reconnect-rejection guards, so a
// stream that was never the accepted binding could still have its
// disconnect processed as current — the late/stale disconnect evicted
// the legitimately reconnected worker. At model resolution the
// family-level pre-fix shape is "disconnect delivery has no reliable
// epoch attribution": the override delivers the oldest queued
// reader-task disconnect as if it were current regardless of its epoch
// (covering both the premature-stamp half of db457374f C1 and the
// late-disconnect-after-reconnect half the retry close-out hands over),
// and latches the as-built oracle (a stale-epoch event mutated a live
// binding) at the point the as-built guard would have dropped it.
// PREDICTED: staleStreamEventsAreInert falsifies — the counterexample is
// the I-056a interleaving (connection drop queues a disconnect at epoch
// N, the worker reconnects at epoch N+1, the stale event then evicts the
// re-bound entry).
// Constants are the fault-stream-conn regime's (the regime that
// carries F1's connection-fault content; its witness checks are wired,
// its exhaustive cfg is the open budget stop-and-report); the as-built
// baseline is the Stage-B record's deep-simulation + witness evidence
// (the exhaustive run is not re-attempted here).
// =========================================================================
module executorCalibF1StaleEpochApplies {
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
    HALF_DEATHS = 1,
    STREAM_DROPS = 1,
    HB_DROPS = 1,
    DUP_MSGS = 0,
    STALLS = 1,
    FUSE_FLIPS = 2,
    ADMIN_DRAINS = 1,
    SIGTERMS = 1
  ).* from "../executorSessionAsBuilt"

  /// PRE-FIX deliverDisconnect: the oldest queued disconnect is applied
  /// as if it were current — no epoch attribution. The latch keeps the
  /// as-built oracle: it fires exactly when the as-built I-056a guard
  /// would have dropped the event (a present slot bound to a newer
  /// stream), i.e. when a stale event mutates a live binding.
  action deliverDisconnectAnyEpoch(e: str): bool = {
    val s0 = st.sched.get(e)
    val ep = leastInt(s0.pendingDisc)
    val stale = not(s0.present) or s0.streamEpoch != ep
    val cleared = { ...s0, pendingDisc: s0.pendingDisc.exclude(Set(ep)) }
    all {
      s0.pendingDisc.size() > 0,
      st' = applyDisconnect({ ...st, sched: st.sched.set(e, cleared) }, e),
      viol' = { ...viol,
        staleEpochMutation: viol.staleEpochMutation or (stale and s0.present) },
      reach' = reach
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
      deliverDisconnectAnyEpoch(e),
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
