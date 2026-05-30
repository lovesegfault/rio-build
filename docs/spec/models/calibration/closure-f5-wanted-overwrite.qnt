// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, closure-evidence family F5 (wanted-set
// lifecycle; wired family representative re-routed within the family to
// CE-16, ab8cdba18 — persisting wanted sets by overwrite would un-want
// outputs another build needs; the design's named rep CE-18 is the
// rollback-replay structural override, which is inert at this model's
// all-or-nothing merge-intent granularity — the as-built mergeRollback
// is a no-op, so the replay shape adds no signal at 0d and stays a
// trailing Phase-2 evidence target; the re-route is recorded in the
// invariant map's Phase 0d calibration table). Crosswalk rows:
// closure-evidence-formal-design.md §4 "F5 CE-16 → ENC → B5",
// "F5 CE-18, CE-19, CE-68 → ENC (rep: CE-18; structural) → B6".
//
// PRE-FIX behavior modeled: the merge transaction's durable wanted
// write is an overwrite of the stored union with this submission's
// contribution instead of a union (pre-ab8cdba18 shape). The B5 oracle
// is added at the apply site: an applied merge whose post-state stored
// union no longer contains the pre-state union.
//
// PREDICTED: storedWantedMonotone (B5) falsifies — the counterexample
// is a wide submission's stored union overwritten by a later narrow
// resubmission.
// Constants: the reduced exhaustive-base scope (closureEvidenceBaseEx);
// the as-built baseline for these constants is the Phase 0c closing run
// / property-identification run (B5 holds there), re-runnable as the
// same module without --step.
//
// Run (serially):
//   quint verify --backend=tlc --main=closureCalibF5WantedOverwrite \
//     --step=calibStep --invariant=storedWantedMonotone \
//     docs/spec/models/calibration/closure-f5-wanted-overwrite.qnt
// Baseline:
//   quint verify --backend=tlc --main=closureCalibF5WantedOverwrite \
//     --invariant=storedWantedMonotone \
//     docs/spec/models/calibration/closure-f5-wanted-overwrite.qnt
module closureCalibF5WantedOverwrite {
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

  /// PRE-FIX intent apply: identical to pgApplyAny except the merge
  /// transaction's stored-wanted write OVERWRITES the union with this
  /// submission's contribution (no union-on-conflict). The B5 oracle is
  /// recorded at this site; every other intent kind is as built.
  action calibPgApplyOverwriteWanted: bool = {
    nondet pick = intents.union(Set(noopIntent)).oneOf()
    val stale = pick.tenure < lead.gen
    val isEvidence = pick.op != INop
    all {
      intents.contains(pick),
      intents' = intents.exclude(Set(pick)),
      reach' = { ...reach, staleIntentApplied: reach.staleIntentApplied or stale },
      viol' = {
        ...viol,
        staleEvidenceApply: viol.staleEvidenceApply or (stale and isEvidence),
        staleClearOverride: viol.staleClearOverride or (stale and
          match pick.op {
            | IClearBoth(d) => (drv.get(d).pgTp and pick.tenure < drv.get(d).pgTpGen)
                                or (drv.get(d).pgCh and pick.tenure < drv.get(d).pgChGen)
            | IClearMark(d) => drv.get(d).pgTp and pick.tenure < drv.get(d).pgTpGen
            | IHeal(d) => drv.get(d).pgCh and pick.tenure < drv.get(d).pgChGen
            | _ => false
          }),
        pgBitLowered: viol.pgBitLowered,
        // B5 oracle: an applied merge shrank some row's stored union.
        wantedShrunk: viol.wantedShrunk or
          match pick.op {
            | IMerge(m) => m.rows.exists(r =>
                drv.get(r).rowExists and
                not(drv.get(r).pgWanted.subseteq(if (m.narrow) Set(1) else OUTIDXSET)))
            | _ => false
          }
      },
      drv' = match pick.op {
        | IClearBoth(d) =>
            if (drv.get(d).rowExists)
              drv.set(d, { ...drv.get(d), pgTp: false, pgCh: false })
            else drv
        | IClearMark(d) =>
            if (drv.get(d).rowExists) drv.set(d, { ...drv.get(d), pgTp: false }) else drv
        | ISetHole(d) =>
            if (drv.get(d).rowExists and not(drv.get(d).pgCh))
              drv.set(d, { ...drv.get(d), pgCh: true, pgChGen: pick.tenure })
            else drv
        | IHeal(d) =>
            if (drv.get(d).rowExists) drv.set(d, { ...drv.get(d), pgCh: false }) else drv
        | IMerge(m) =>
            m.rows.fold(drv, (acc, r) => {
              val rec = acc.get(r)
              // pre-fix: overwrite instead of union
              val newWanted = if (m.narrow) Set(1) else OUTIDXSET
              acc.set(r, { ...rec,
                rowExists: true,
                pgSt: if (rec.mSt != MAbsent) rec.mSt else if (rec.rowExists) rec.pgSt else MQueued,
                pgTp: rec.pgTp or m.stamps.contains(r),
                pgTpGen: if (not(rec.pgTp) and m.stamps.contains(r)) pick.tenure else rec.pgTpGen,
                pgWanted: newWanted })
            })
        | INop => drv
      },
      pgEdges' = match pick.op {
        | IMerge(m) =>
            if (m.pruned) pgEdges
            else pgEdges.union(REFS.filter(e => m.rows.contains(e._1) and m.rows.contains(e._2)))
        | _ => pgEdges
      },
      blds' = match pick.op {
        | IMerge(m) => blds.set(m.bld, { ...blds.get(m.bld), pgRow: true, pgLinks: m.rows })
        | _ => blds
      },
      store' = store, memEdges' = memEdges, lead' = lead, budget' = budget
    }
  }

  action calibStep = any {
    stepStore,
    stepBuild,
    stepDrv,
    calibPgApplyOverwriteWanted,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }
}
