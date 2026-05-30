// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, closure-evidence family F9
// (closure-hole breadcrumb integrity; wired family representative
// re-routed within the family to CE-41, 9097b71cc — the poison-clear
// paths, admin ClearPoison and the TTL sweep, removed an un-produced
// child without stamping the surviving parents, the same laundering as
// CE-40 via a different remover). The design's named reps CE-40
// (recovery edge-drop) and CE-43 (fail-fast keeps the breadcrumb) need
// a recovery-action / consume-walk structural copy whose falsification
// sits deeper than this setter-coverage member at the post-ladder
// scope; the re-route is recorded in the invariant map's Phase 0d
// calibration table and CE-40/CE-43 stay listed as trailing evidence
// targets for Phase 2. Crosswalk row: closure-evidence-formal-design.md
// §4 "F9 CE-36..CE-43 → ENC → A4-A8, A10".
//
// PRE-FIX behavior modeled: the poison-clear removal of an un-produced
// child does not stamp the closure-hole breadcrumb on the surviving
// parents (neither in memory nor as a PG intent) — one setter site
// omitted from the H2/H3 coverage. The A5 latch carries the as-built
// oracle (a leader-side removal of an un-produced child whose surviving
// parent was not stamped in the same step).
//
// PREDICTED: holeCompleteness (A5) falsifies — the counterexample is an
// admin/TTL poison clear that removes a poisoned child and leaves the
// surviving parent's child set looking intact.
// Constants: the reduced exhaustive-base scope (closureEvidenceBaseEx);
// the as-built baseline for these constants is the Phase 0c closing run
// / property-identification run (A5 holds there), re-runnable as the
// same module without --step.
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF9PoisonClearNoStamp \
//     --step=calibStep --invariant=holeCompleteness \
//     docs/spec/models/calibration/closure-f9-poison-clear-no-stamp.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF9PoisonClearNoStamp \
//     --invariant=holeCompleteness \
//     docs/spec/models/calibration/closure-f9-poison-clear-no-stamp.qnt
module closureCalibF9PoisonClearNoStamp {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 2,
    FAILOVERS = 0,
    PGDROP_BUDGET = 0,
    STALE_APPLY = false,
    CROSS_TENURE_WALKS = false,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    WALK_CONSUME_CEIL = 2,
    ATTEMPT_CYCLE_CEIL = 2,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = false
  ).* from "../closureEvidence"

  /// PRE-FIX poison clear (admin or TTL sweep): the poisoned node is
  /// removed and its row reset, but the surviving parents get no
  /// closure-hole breadcrumb and no W4 intent (pre-9097b71cc). The A5
  /// oracle is added at this site: a removal of an un-produced child
  /// with surviving parents that were not stamped in the same step.
  action calibPoisonClearNoStamp(d: str): bool = {
    nondet viaAdmin = Set(true, false).oneOf()
    all {
      leaderActive,
      drvOf(d).mSt == MPoisoned,
      val parents = memParentsOf(d).filter(p => drvOf(p).mSt != MAbsent)
      all {
        drv' = DRVS.mapBy(x => {
          val r = drv.get(x)
          if (x == d) { ...r, mSt: MAbsent, pgSt: if (r.rowExists) MQueued else r.pgSt, walks: [], nf: Set() }
          else r
        }),
        memEdges' = memEdges.filter(e => e._1 != d and e._2 != d),
        intents' = intents,
        store' = store, blds' = blds, pgEdges' = pgEdges, lead' = lead, budget' = budget,
        viol' = { ...viol,
          holeUnsound: viol.holeUnsound or parents.exists(p => drvOf(d).mSt == MProduced),
          // A5 oracle: an un-produced child was removed from surviving
          // parents and none of them was stamped in this step.
          holeMissed: viol.holeMissed or parents.exists(p => not(drvOf(p).mCh))
        },
        reach' = { ...reach,
          holeAdmin: reach.holeAdmin or (viaAdmin and parents != Set()),
          holeTtl: reach.holeTtl or (not(viaAdmin) and parents != Set()) }
      }
    }
  }

  action calibStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeWalk(d),
      probeFailFastMissing(d),
      probeFailFastTried(d),
      probeMissingUnmarked(d),
      walkFinishes(d),
      consumeWalk(d),
      dropStaleWalkResult(d),
      attemptOpen(d),
      attemptCloseSuccess(d),
      attemptCloseFail(d),
      reassignOpen(d),
      requeueFailed(d),
      poisonArrival(d),
      calibPoisonClearNoStamp(d),
      orphanRowGc(d)
    }
  }

  action calibStep = any {
    stepStore,
    stepBuild,
    calibStepDrv,
    pgApplyAny,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }
}
