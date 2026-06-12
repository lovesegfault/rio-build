// -*- mode: Bluespec; -*-
//
// PHASE D-PRIME RETIREMENT NOTE (substitution-replacement T-D7.1):
// the wired check quint-closure-calib-a17-unfenced was pruned in the
// survivors-core landing; successor oracle:
// quint-materialization-calib-f11-unfenced-resolve
// (fencedJobWritesOnly — the uniform fence's living falsifiability
// record).
// The walk-era mechanism family this corpus calibrated was deleted in
// Waves D3-D6 (the walk, the evidence columns, the Substituting
// status; migration 080). This file stays as the predecessor-system
// evidence record until the A6 archive of closureEvidence.qnt; its
// re-run command remains valid against the retained model text.
//
// Phase-1 calibration override / regression pin, closure-evidence
// A17/A18 claims-floor fence (the Wave-3 production fix at
// rio-scheduler db/batch.rs, db/derivations.rs, actor/merge.rs +
// actor/completion.rs: every evidence write carries the issuing
// tenure's serving generation and applies only at-or-above the durable
// claims floor; spec rule sched.evidence.durability+2).
//
// PRE-FIX behavior modeled (one frozen action): calibPgApplyUnfenced —
// the intent-apply path with NO claims-floor fence. A deposed tenure's
// pending evidence intent (created before its lease was lost, applied
// after the successor recovered) lands in PG: the merge transaction
// re-introduces rows and an Active build the live leader does not know
// about (A18, the D14 orphaned-build leg), and a stale clear/heal can
// erase a bit a newer tenure stamped (A17, the dangerous direction).
// Post-fix the production pgApplyAny DISCARDS stale evidence intents
// (the model form of "below-floor write rolls back having written
// nothing").
//
// PREDICTED falsification: leaderClassEvidenceWrites (A18) — the
// 5-state trace the post-0d triage measured at this scope (mergeCommit
// -> leaderLost -> recoverAsLeader -> stale pgApplyAny): the deposed
// tenure's merge transaction lands after the successor's recovery.
// A17 (noStaleTenureClearOverride) falsifies through the same unfenced
// apply path but needs the deeper clear-then-restamp interleaving that
// no bounded hunt has produced (the 0c deferred-probe record); the A18
// falsification is the wired pin for the shared unfenced-apply
// mechanism — the fence that closes A18 is the same statement-level
// guard that closes A17.
//
// The wired check quint-closure-calib-a17-unfenced pins this
// falsification permanently: if it ever stops falsifying, the model
// has drifted away from the stale-write class it documents and the
// stale-fence-holds check is no longer evidence of anything.
//
// Constants: closureEvidenceStaleDuo (the stale-tenure probe scope).
//
// Run (the wired pin -- pre-fix unfenced apply falsifies A18):
//   quint run --backend=rust --main=closureCalibA17Unfenced --step=calibStep \
//     --invariant=leaderClassEvidenceWrites \
//     --max-samples=500000 --max-steps=15 \
//     docs/spec/models/calibration/closure-a17-unfenced.qnt
// Baseline (production fenced apply HOLDS A18 and A17 at these constants):
//   quint run --backend=rust --main=closureCalibA17Unfenced \
//     --invariant=leaderClassEvidenceWrites \
//     --max-samples=2000000 --max-steps=15 \
//     docs/spec/models/calibration/closure-a17-unfenced.qnt
//   quint run --backend=rust --main=closureCalibA17Unfenced \
//     --invariant=noStaleTenureClearOverride \
//     --max-samples=2000000 --max-steps=15 \
//     docs/spec/models/calibration/closure-a17-unfenced.qnt
//
// VERDICT (Phase-1 Wave 4; relocated from the retired closure-evidence
// invariant map, full tables: docs/spec/models/closure-evidence-
// records.md): falsifies A18 in <2 s at 500 K samples (the 5-state
// D14-leg trace); baseline (production fenced apply) holds at 2 M. The
// A18 falsification is the wired oracle for the shared unfenced-apply
// mechanism (A17's own deeper clear-then-restamp interleaving was never
// produced in any hunted budget; the same fence closes both).
module closureCalibA17Unfenced {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 3,
    FAILOVERS = 2,
    PGDROP_BUDGET = 1,
    STALE_APPLY = true,
    CROSS_TENURE_WALKS = true,
    STORE_GC_BUDGET = 0,
    ROW_GC_BUDGET = 0,
    PURGE_BUDGET = 0,
    POISON_BUDGET = 1,
    CANCEL_BUDGET = 1,
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 1,
    INTENT_CEIL = 5,
    WALKS_PER_NODE = 2,
    STORE_PER_OUTPUT = true
  ).* from "../closureEvidence"

  /// PRE-FIX intent apply: identical to pgApplyAny before the Phase-1
  /// claims-floor fence — a stale (deposed-tenure) evidence intent is
  /// APPLIED to the durable state instead of discarded, and the A17/A18
  /// latches record it. Every intent kind and every other latch is the
  /// frozen pre-fence behavior of the live model.
  action calibPgApplyUnfenced: bool = {
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
        // A17: a stale clear/heal erasing a bit set by a newer tenure
        staleClearOverride: viol.staleClearOverride or (stale and
          match pick.op {
            | IClearBoth(d) => (drv.get(d).pgTp and pick.tenure < drv.get(d).pgTpGen)
                                or (drv.get(d).pgCh and pick.tenure < drv.get(d).pgChGen)
            | IClearMark(d) => drv.get(d).pgTp and pick.tenure < drv.get(d).pgTpGen
            | IHeal(d) => drv.get(d).pgCh and pick.tenure < drv.get(d).pgChGen
            | _ => false
          }),
        // A7/B5 hooks: the merge upsert never lowers a bit / shrinks the union
        pgBitLowered: viol.pgBitLowered,
        wantedShrunk: viol.wantedShrunk
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
              val newWanted = rec.pgWanted.union(if (m.narrow) Set(1) else OUTIDXSET)
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
        // build_derivations links are per build id; the directed resubmit
        // re-uses the slot, so its links REPLACE the failed submission's
        // (a fresh build id in production)
        | IMerge(m) => blds.set(m.bld, { ...blds.get(m.bld), pgRow: true, pgLinks: m.rows })
        | _ => blds
      },
      store' = store, memEdges' = memEdges, lead' = lead, budget' = budget
    }
  }

  /// Production step with the unfenced apply swapped in for pgApplyAny.
  action calibStep = any {
    stepStore,
    stepBuild,
    stepDrv,
    calibPgApplyUnfenced,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }
}
