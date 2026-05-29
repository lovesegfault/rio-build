// -*- mode: Bluespec; -*-
//
// Calibration override, executor-lifecycle family F4 (death
// attribution — the dedup-state lifecycle), re-encoded against the
// RE-TARGETED pull-protocol Model S at 1c' (the as-built F4
// representatives executor-f4-entry-not-mid-build.qnt and
// executor-f4-establish-early.qnt were evidence modules over the
// frozen executorSessionAsBuilt.qnt until the 2026-05-29 as-built
// retirement deleted them — git history is the archive, the invariant
// map's Stage-C tables hold their verdicts). The analogous contended
// state in the pull protocol is the establishment window: the establishment
// sweep (tick_sweep_open_pull_attempts) may charge an open attempt as
// an unreported executor crash only once the attempt's dispatch-time
// deadline plus the report slack has elapsed — never while the
// classifying report (the worker's own ReportOutcome or the
// controller's pod-terminal classification) still has its window to
// arrive. Losing that gate re-creates the F4 bug class (the I-197
// double-charge precondition) on the pull path: the same death is
// charged by the early establishment and then classified again by the
// report that was still in flight.
//
// Run (serially):
//   quint verify --backend=tlc --main=executorCalibF4PullEstablishEarly \
//     --step=calibStep --invariant=establishmentOnlyAfterWindowCloses \
//     docs/spec/models/calibration/executor-f4-pull-establish-early.qnt
//
// =========================================================================
// CALIBRATION — pre-window-guard establishment: the C2 charge arm of
// the establishment sweep fires on any open, unrecorded attempt
// regardless of whether its window has closed. The latch keeps the
// production oracle (an attempt established while its window was still
// open). Everything else — the pull admission, the report intakes, the
// adopt arm, the fence — is the live model's encoding.
// PREDICTED: establishmentOnlyAfterWindowCloses falsifies — the
// counterexample is an open attempt established by the sweep before
// windowExpires ever fired for it.
// Constants are the wired base regime's; the as-built baseline for this
// family is the wired quint-executor-session-base /
// quint-executor-session-fault-leader checks, which HOLD the same
// invariant with the gate in place.
// =========================================================================
module executorCalibF4PullEstablishEarly {
  import executorSession(
    DRVS = Set("d1"),
    PULL_CEIL = 2,
    ATTEMPTS_CEIL = 2,
    PODS_CEIL = 3,
    POD_DEATHS = 2,
    CANCELS = 1,
    FAILOVERS = 0,
    STALE_TRIES = 0
  ).* from "../executorSession"

  /// PRE-FIX establishment charge arm: identical to establishCharge
  /// except the openWindowClosed precondition is dropped. The latch
  /// records every charge that fired while the window was still open.
  action calibEstablishChargeAnyTime(d: str): bool = all {
    drv(d).openRows == 1,
    not(drv(d).openRecorded),
    not(drv(d).built),
    drv(d).dag == DOpen,
    drv(d).charged < ATTEMPTS_CEIL,
    val r = drv(d)
    st' = { ...st, drvs: st.drvs.set(d, { ...closeOpen(r, false),
      dag: DReady,
      charged: r.charged + 1
    }) },
    viol' = { ...viol,
      establishedBeforeWindow: viol.establishedBeforeWindow or not(drv(d).openWindowClosed)
    },
    reach' = { ...reach, established: true }
  }

  /// The live step with the establishment charge arm replaced by the
  /// pre-fix window-less variant; every other action is the live
  /// model's.
  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet uploadedFirst = Set(true, false).oneOf()
    any {
      drvBecomesReady(d),
      drvCancelled(d),
      spawnPod(d),
      pullNotYetReady(d),
      pullDeliverNew(d),
      pullRedeliver(d),
      pullGone(d),
      idleExit(d),
      podDies(d, uploadedFirst),
      workerReportSuccess(d),
      workerReportFailure(d),
      workerAbortReport(d),
      lateWorkerReport(d),
      ctrlReportNoAttempt(d),
      ctrlSecondInstallment(d),
      ctrlSynthesizedClose(d),
      windowExpires(d),
      establishAdopt(d),
      calibEstablishChargeAnyTime(d),
      leaderFailover,
      staleAuthorityAttempt(d)
    }
  }
}
