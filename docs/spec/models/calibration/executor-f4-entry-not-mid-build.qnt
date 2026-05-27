// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, executor-lifecycle family F4 (death
// attribution — the dedup-state lifecycle), representative: the I-197
// `last_completed` discriminator on the correlation-entry insert (one
// of the dedup-lifecycle halves the retry close-out hands over to this
// campaign; charging itself stays imported from retryPolicy.qnt). One
// file per representative per calibration/README.md.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF4EntryNotMidBuild \
//     --step=calibStep --invariant=correlationEntryLifecycle \
//     docs/spec/models/calibration/executor-f4-entry-not-mid-build.qnt

// =========================================================================
// CALIBRATION — pre-I-197: the disconnect path inserted a
// recently_disconnected correlation entry whenever the slot still held
// a running build, with no last_completed discriminator — so a death
// the controller's classifying report had already raced ahead of (and a
// one-shot exit whose completion had already landed) still minted an
// entry. The orphaned entry then aged out and was established as an
// unreported executor crash on an attempt that had already been
// classified — the double-charge path attemptsChargedOnce exists to
// exclude (charging is the retry fold's; what Model S owns is that the
// entry is created only by a mid-build disconnect). The override
// removes ONLY the discriminator from the disconnect-delivery path; the
// latch keeps the as-built oracle (an entry inserted where the as-built
// discriminator would have skipped it).
// PREDICTED: correlationEntryLifecycle falsifies — the counterexample
// is the race-ahead classify (controller report lands before the
// disconnect) followed by the disconnect inserting the entry anyway.
// Constants are the documented fault-process regime's (a pod death is
// what produces the classifying report); its as-built baseline is the
// Stage-B record's deep-simulation + witness evidence (the exhaustive
// cfg is the open budget stop-and-report and is not re-attempted here).
// =========================================================================
module executorCalibF4EntryNotMidBuild {
  import executorSession(
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
  ).* from "../executorSession"

  /// PRE-FIX deliverDisconnect: identical to the as-built action (epoch
  /// staleness, entry removal, leader-gated requeue) except that the
  /// correlation-entry insert has no last_completed discriminator — any
  /// disconnect whose slot still records a running build mints an
  /// entry, including one whose death was already classified by the
  /// race-ahead controller report.
  action deliverDisconnectNoDiscriminator(e: str): bool = {
    val s0 = st.sched.get(e)
    val ep = leastInt(s0.pendingDisc)
    val stale = not(s0.present) or s0.streamEpoch != ep
    val cleared = { ...s0, pendingDisc: s0.pendingDisc.exclude(Set(ep)) }
    val dn = s0.running
    // PRE-FIX: insert whenever a build is still recorded on the slot.
    val insertEntry = dn != ""
    // The as-built oracle: the I-197 discriminator would have skipped
    // this insert (the recorded build already completed / was already
    // classified via the race-ahead last_completed stamp).
    val notMidBuild = dn != "" and s0.lastCompleted == dn
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
            rd: if (insertEntry)
                  st.rd.set(e, {
                    present: true, drv: dn,
                    attempt: st.drv.get(dn).attempt, expired: false })
                else st.rd
          }
          if (dn != "" and st.believedLeader) requeue(st1, dn) else st1
        },
        viol' = { ...viol,
          rdInsertNotMidBuild: viol.rdInsertNotMidBuild or (insertEntry and notMidBuild) },
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
      deliverDisconnectNoDiscriminator(e),
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
