// -*- mode: Bluespec; -*-
//
// PHASE D-PRIME RETIREMENT NOTE (substitution-replacement T-D7.1):
// this evidence module was never wired; the closure-evidence wired
// family was pruned to the survivors core plus the two kept pins
// (closure-f1-stale-produced, closure-f4-demand-drop).
// The walk-era mechanism family this corpus calibrated was deleted in
// Waves D3-D6 (the walk, the evidence columns, the Substituting
// status; migration 080). This file stays as the predecessor-system
// evidence record until the A6 archive of closureEvidence.qnt; its
// re-run command remains valid against the retained model text.
//
// Stage-C calibration override, closure-evidence family F6
// (forgiveness / never_forgive chain scoping; corpus reps CE-20
// 71e37da5f and CE-21 5113de5c8 — the spent-forgiveness latch survived
// the chain that created it, so a later, unrelated chain refused
// forgiveness and demoted a substitutable node to source). Crosswalk
// row: closure-evidence-formal-design.md §4 "F6 CE-20, CE-21, CE-22 →
// ENC (rep: CE-21) → A21 (+ C2, A12)". The wired falsification goes
// through the chain-end transition that exists at the post-ladder
// scope: the forgiven-now-wanted downgrade's revert arm (a chain end —
// demotion to from-source) keeping the latch alive on a live node,
// which is the CE-20 shape of the same family invariant; CE-21's
// specific completion paths (merge-time cached hit, CA-skip cascade)
// cannot carry a non-empty latch as built because the latch only
// becomes non-empty while a walk chain is in flight, so they are
// covered by this representative per the §4 grouping. The C2
// permissiveness direction for F6 is carried by the inherited-veto
// consequence recorded in the calibration table (the demoted node's
// next probe finds substitute_tried armed without a genuine failure);
// the A21 falsification is the wired check.
//
// PRE-FIX behavior modeled: the downgrade's revert arm ends the chain
// (demotes to the from-source lane, arms the one-shot) but does NOT
// clear the chain-scoped never-forgive set — the latch outlives its
// chain on a live node.
//
// PREDICTED: chainEndClearsForgiveness (A21) falsifies — the
// counterexample is a reverted (Ready) node still carrying the
// never-forgive entry of the chain that just ended.
// Constants: the duo probe scope (closureEvidenceDuo) — the
// forgiven-now-wanted downgrade needs a second build's wider wanted
// contribution to land while the narrow walk is in flight; the as-built
// baseline is the same module without --step at these constants
// (bounded run recorded in the invariant map's Phase 0d calibration
// table).
//
// BACKEND NOTE (Phase 0d finding): this override's falsification is
// produced by the RUST SIMULATOR backend (5-state trace, milliseconds);
// the TLC backend does NOT find it — TLC's first run on this module
// hard-errored at 93 ms, and subsequent runs explore past the
// violation depth (BFS depth 7, 280 K+ distinct states) without
// flagging it, the swallowed-evaluation-exception signature. The
// as-built baseline HOLDS under both backends (rust 100 K x 12-step
// samples; TLC bounded to depth 7). Use the rust commands below; the
// TLC discrepancy is recorded in the invariant map's Phase 0d stage
// record as a tool issue.
//
// Run (rust simulator):
//   quint run --main=closureCalibF6LatchOutlivesChain \
//     --step=calibStep --invariant=chainEndClearsForgiveness \
//     --max-samples=100000 --max-steps=12 \
//     docs/spec/models/calibration/closure-f6-latch-outlives-chain.qnt
// Baseline (must find no violation):
//   quint run --main=closureCalibF6LatchOutlivesChain \
//     --invariant=chainEndClearsForgiveness \
//     --max-samples=100000 --max-steps=12 \
//     docs/spec/models/calibration/closure-f6-latch-outlives-chain.qnt
module closureCalibF6LatchOutlivesChain {
  import closureEvidence(
    DRVS = Set("d1", "d2"),
    BUILDS = Set("b1", "b2"),
    REFS = Set(("d1", "d2")),
    SUBMISSION_BUDGET = 3,
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
    ATTEMPT_CYCLE_CEIL = 1,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = true
  ).* from "../closureEvidence"

  /// PRE-FIX walk consumption: identical to consumeWalk except the
  /// forgiven-now-wanted downgrade's REVERT arm keeps the grown
  /// never-forgive set instead of clearing it with the chain
  /// (pre-71e37da5f node-lifetime scoping). Every other branch and
  /// every latch is the live model's AS OF THE FREEZE — this copy also
  /// predates the Phase-1 Broken-arm settlement (its Broken arm is the
  /// pre-settlement direct fail-fast), which is intentional: the copy
  /// freezes the consumeWalk of the era whose A21 defect it documents,
  /// and the A21 falsification (the nf latch on the REVERT arm) does not
  /// touch the Broken arm. The step alphabet carries the production
  /// probeSettleTried so the rest of the partition stays current.
  action calibConsumeWalkKeepNf(d: str): bool = {
    nondet idx = Set(0, 1).oneOf()
    val w = pWalkAt(drvOf(d).walks, idx)
    val res = match w.res { | WDone(r) => r | WPending => { ok: false, forgiven: OUTIDX.exclude(OUTIDX), genuine: false } }
    val crossTenure = w.tenure < lead.gen
    val baseRec = { ...drvOf(d), walks: drvOf(d).walks.select(ww => ww != w), walkConsumed: drvOf(d).walkConsumed + 1 }
    val forgivenNowWanted = res.forgiven.intersect(effWanted(d))
    val revertTo = pRevertTarget(drv, memEdges, d)
    val ev = evidence(d)
    val grownNf = baseRec.nf.union(forgivenNowWanted)
    val respawn = revertTo == MDepFailed or mustSub(d)
    val descendants = pClosure(REFS, pKids(REFS, d))
    val walkedOuts = OUTIDX.exclude(res.forgiven).map(i => (d, i)).union(tuples(descendants, OUTIDX))
    val dmProduced = drv.set(d, { ...baseRec, mSt: MProduced,
      pgSt: if (baseRec.rowExists) MProduced else baseRec.pgSt, nf: OUTIDX.exclude(OUTIDX) })
    val cascade = pPromoteAndClear(dmProduced, memEdges, DRVS, Set(d))
    val okIntents = cascade.cleared.map(p => { op: IClearBoth(p), tenure: lead.gen })
    val withTried = drv.set(d, { ...baseRec, tried: true, triedGenuine: res.genuine, mSt: MReady, nf: OUTIDX.exclude(OUTIDX) })
    val ff = pFailFast(withTried, blds, BUILDS, memEdges, store, d)
    all {
      leaderActive,
      drvOf(d).mSt == MSubstituting,
      drvOf(d).walkConsumed < WALK_CONSUME_CEIL,
      idx < drvOf(d).walks.length(),
      w.res != WPending,
      crossTenure implies CROSS_TENURE_WALKS,
      if (forgivenNowWanted != Set()) all {
        // pre-fix: the revert arm keeps grownNf (the chain-end clear is
        // omitted); the respawn arm is unchanged.
        drv' = drv.set(d, if (respawn)
          { ...baseRec, nf: grownNf,
            walks: baseRec.walks.append({ forgivable: OUTIDXSET.exclude(effWanted(d)).exclude(grownNf), tenure: lead.gen, res: WPending }) }
          else { ...baseRec, nf: grownNf, mSt: revertTo, tried: true, triedGenuine: false,
            probed: false, probedKind: PKNone }),
        intents' = intents,
        blds' = blds,
        viol' = { ...viol,
          downgradeNoGrowth: viol.downgradeNoGrowth or forgivenNowWanted.subseteq(baseRec.nf) },
        reach' = { ...reach,
          walkFailConsumed: true,
          downgradeRespawned: reach.downgradeRespawned or respawn,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (res.ok) all {
        intents.size() + okIntents.size() <= INTENT_CEIL,
        drv' = cascade.dm,
        intents' = intents.union(okIntents),
        blds' = blds,
        viol' = { ...viol,
          walkOkClosureNeverPresent: viol.walkOkClosureNeverPresent or
            walkedOuts.exists(o => not(store.get(o).everPresent)),
          walkOkClosureGoneAtConsume: viol.walkOkClosureGoneAtConsume or
            walkedOuts.exists(o => not(store.get(o).present)),
          producedWithoutPresence: viol.producedWithoutPresence or
            effWanted(d).exists(i => not(store.get((d, i)).everPresent)),
          brokenVouched: viol.brokenVouched or cascade.cleared.exists(p =>
            pKids(memEdges, p) == Set() or cascade.dm.get(p).mCh)
        },
        reach' = { ...reach,
          walkOkConsumed: true,
          walkOkWithForgiven: reach.walkOkWithForgiven or res.forgiven != Set(),
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (ev == EvVouched) all {
        intents.size() + 1 <= INTENT_CEIL,
        drv' = drv.set(d, { ...baseRec, mTp: false, mSt: revertTo, tried: true, triedGenuine: res.genuine,
          nf: OUTIDX.exclude(OUTIDX), probedKind: PKMissing }),
        intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
        blds' = blds,
        viol' = { ...viol, clearUnsound: viol.clearUnsound or not(vouchedNow(d)) },
        reach' = { ...reach, walkFailConsumed: true,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (ev == EvPending or not(drvOf(d).mTp)) all {
        drv' = drv.set(d, { ...baseRec, mSt: revertTo, tried: true, triedGenuine: res.genuine,
          nf: OUTIDX.exclude(OUTIDX), probedKind: PKMissing }),
        intents' = intents,
        blds' = blds,
        viol' = viol,
        reach' = { ...reach, walkFailConsumed: true,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else all {
        intents.size() + 1 <= INTENT_CEIL,
        drv' = ff.dm,
        blds' = ff.bm,
        intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
        viol' = { ...viol,
          wrongfulTerminalFailure: viol.wrongfulTerminalFailure or ff.wrongful,
          ffBadEffect: viol.ffBadEffect or (ff.fired and (ff.dm.get(d).mTp or (baseRec.mCh and not(ff.dm.get(d).mCh)))) },
        reach' = { ...reach, walkFailConsumed: true, failFast: reach.failFast or ff.fired,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      },
      store' = store, memEdges' = memEdges, pgEdges' = pgEdges, lead' = lead, budget' = budget
    }
  }

  action calibStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeWalk(d),
      probeFailFastMissing(d),
      probeFailFastTried(d),
      probeSettleTried(d),
      probeMissingUnmarked(d),
      walkFinishes(d),
      calibConsumeWalkKeepNf(d),
      dropStaleWalkResult(d),
      attemptOpen(d),
      attemptCloseSuccess(d),
      attemptCloseFail(d),
      reassignOpen(d),
      requeueFailed(d),
      poisonArrival(d),
      poisonClear(d),
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
