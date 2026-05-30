// -*- mode: Bluespec; -*-
//
// Phase-1 calibration override / regression pin, closure-evidence C3+D16
// settlement (the Wave-1 production fix at rio-scheduler dispatch.rs /
// build.rs: settle_broken_marked_root + the dispatch-probe partition's
// settlement cells + the reap-survivor settlement; spec rules
// sched.evidence.settlement and sched.merge.substitute-topdown+12).
//
// PRE-FIX behavior modeled (three frozen actions, the same three sites
// the production fix touched):
//   - calibConsumeWalkNoReprobe: the SubstituteComplete Broken arm
//     consumes a stale ok=false verdict as a terminal verdict — direct
//     fail-fast, no settlement re-probe (the C3 defect: the verdict was
//     fixed at the walk's own check time; the wanted outputs may have
//     become obtainable since).
//   - calibReapNoSettle: the terminal-build reap's survivor pass
//     fail-fasts every marked+Broken+TRIED survivor regardless of status
//     (no MQueued exclusion, no settlement re-probe) and skips untried
//     ones entirely.
//   - calibProbeFailFastTriedWide: the dispatch-probe tried arm fires on
//     ANY locally-missing wanted output (not(allWantedPresent)) — even
//     one the upstream still offers — and the probe partition has no
//     settlement cell (no probeSettleTried), so the all-present tried
//     cell gets no action at all (the D16 limbo).
//
// PREDICTED falsification: noWrongfulTerminalFailureSingleTenure (C3) —
// the second C3 family (review finding MCI-2): b1 full-merges {d1->d2}
// wide; d1's walk genuinely fails on the wide-only output; the verdict
// consumption reverts d1 Queued+tried; the output is ingested
// out-of-band; b2's narrow pruned merge stamps d1; b1 is cancelled and
// reaped — the pre-fix reap fail-fasts the Queued+tried survivor d1
// although every output b2 wants is present (the post-fix reap promotes
// it and the next sweep settles it). The wired check
// quint-closure-calib-c3-no-reprobe pins this falsification permanently:
// if it ever stops falsifying, the model has drifted away from the
// defect class it documents.
//
// This module also carries the PRE-FIX form of the L2 armed invariant
// (markedBrokenSettlementArmedPreFix): the pre-fix settlement partition
// covered only not-all-present cells, so the form is violated wherever a
// D16 cell is reachable. Against this module's BASELINE step (the
// production actions at these constants — no --step flag) the cell is
// reachable through the post-fix promotion transient and the pre-fix
// form falsifies; under calibStep the pre-fix reap fail-fasts the
// would-be cell first (the cell needs the >=16-step 0c assembly there).
// That asymmetry is the machine-checked record that the armed form
// distinguishes the pre-fix partition from the post-fix one.
//
// Constants: closureEvidenceDuo (the cross-build investigation scope).
// The Duo constants -- not the C3Duo TLC-probe constants -- because this
// pin runs under the rust simulator and the falsification's per-trace hit
// rate at Duo is the one the post-0d record measured and the retired
// quint-closure-evidence-probe-wrongful-terminal-failure check ran at;
// the C3Duo restriction (poison/attempts off, extra intent slot) was
// designed to cheapen TLC's BFS, and empirically it DILUTES the sim hit
// rate by an order of magnitude (Wave-4 measurement record in the
// invariant map).
//
// Run (the wired pin -- pre-fix actions falsify C3):
//   quint run --backend=rust --main=closureCalibC3NoReprobe --step=calibStep \
//     --invariant=noWrongfulTerminalFailureSingleTenure \
//     --max-samples=5000000 --max-steps=14 \
//     docs/spec/models/calibration/closure-c3-no-reprobe.qnt
// Pre/post distinguisher (BASELINE step reaches the D16 cell, which the
// pre-fix partition does not cover):
//   quint run --backend=rust --main=closureCalibC3NoReprobe \
//     --invariant=markedBrokenSettlementArmedPreFix \
//     --max-samples=20000000 --max-steps=14 \
//     docs/spec/models/calibration/closure-c3-no-reprobe.qnt
// Baselines (production actions at these constants HOLD both):
//   quint run --backend=rust --main=closureCalibC3NoReprobe \
//     --invariant=noWrongfulTerminalFailureSingleTenure \
//     --max-samples=2000000 --max-steps=14 \
//     docs/spec/models/calibration/closure-c3-no-reprobe.qnt
//   quint run --backend=rust --main=closureCalibC3NoReprobe \
//     --invariant=markedBrokenSettlementArmed \
//     --max-samples=2000000 --max-steps=14 \
//     docs/spec/models/calibration/closure-c3-no-reprobe.qnt
module closureCalibC3NoReprobe {
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
    // Phase 1 (FC-5): admits original walk + the settlement's verification
    // walk + one downgrade re-spawn; canReachVerificationWalkConsumed pins it.
    WALK_CONSUME_CEIL = 3,
    ATTEMPT_CYCLE_CEIL = 1,
    INTENT_CEIL = 3,
    WALKS_PER_NODE = 1,
    STORE_PER_OUTPUT = true
  ).* from "../closureEvidence"

  /// The PRE-FIX settlement partition coverage over a tried marked-Broken
  /// cell: only the un-narrowed fail-fast arm existed, keyed on LOCAL
  /// presence — an all-present cell was covered by nothing (the D16
  /// limbo hole the Phase-1 settlement closes).
  def settlementEnabledPreFix(d: str): bool = not(allWantedPresent(d))

  /// L2 in the armed form, restated over the PRE-FIX partition. Violated
  /// exactly where a D16 cell is reachable (the cell is all-present; the
  /// pre-fix coverage requires not-all-present). See the header for the
  /// baseline-vs-calibStep asymmetry this documents.
  val markedBrokenSettlementArmedPreFix =
    DRVS.forall(d => d16Cell(d) implies settlementEnabledPreFix(d))

  /// PRE-FIX terminal-build reap (Wave-1 T-1.6's subject): the survivor
  /// pass fail-fasts marked+Broken+TRIED survivors with no MQueued
  /// exclusion and no settlement re-probe; untried survivors are skipped.
  /// Identical to the production reapTerminalBuild before the Phase-1
  /// settlement; the violation latches keep the production oracle.
  action calibReapNoSettle(b: str): bool = all {
    leaderActive,
    blds.get(b).cleanup,
    blds.get(b).phase == BpDoneOk or blds.get(b).phase == BpFailed or blds.get(b).phase == BpCancelled,
    val members = blds.get(b).interest
    val removalSet = members.filter(m => BUILDS.forall(b2 => b2 == b or not(blds.get(b2).interest.contains(m))))
    val holedParents = DRVS.filter(p => and {
      not(removalSet.contains(p)),
      drvOf(p).mSt != MAbsent,
      removalSet.exists(c => memEdges.contains((p, c)) and drvOf(c).mSt != MProduced)
    })
    val memEdges1 = memEdges.filter(e => not(removalSet.contains(e._1)) and not(removalSet.contains(e._2)))
    val dmRemoved = DRVS.mapBy(d => {
      val r = drv.get(d)
      if (removalSet.contains(d)) { ...r, mSt: MAbsent, walks: [], nf: Set(), attemptOpen: false }
      else if (holedParents.contains(d)) { ...r, mCh: true }
      else r
    })
    val bm1 = blds.set(b, { ...blds.get(b), interest: Set(), cleanup: false })
    // survivor verdicts on the post-removal state
    val survivors = DRVS.filter(p => not(removalSet.contains(p)) and dmRemoved.get(p).mSt != MAbsent and
      removalSet.exists(c => memEdges.contains((p, c))))
    val ffTargets = survivors.filter(p => and {
      pMustSub(dmRemoved, memEdges1, p),
      dmRemoved.get(p).tried,
      not(isTerminal(dmRemoved.get(p).mSt)),
      dmRemoved.get(p).mSt != MSubstituting, dmRemoved.get(p).mSt != MOpen,
      pLiveInterest(bm1, BUILDS, p)
    })
    val afterFf = ffTargets.fold({ dm: dmRemoved, bm: bm1, wrongful: false, fired: false }, (st, p) => {
      val one = pFailFast(st.dm, st.bm, BUILDS, memEdges1, store, p)
      { dm: one.dm, bm: one.bm, wrongful: st.wrongful or one.wrongful, fired: st.fired or one.fired }
    })
    val dmFinal = DRVS.mapBy(d => {
      val r = afterFf.dm.get(d)
      if (survivors.contains(d) and not(ffTargets.contains(d)) and r.mSt == MQueued and
          pKids(memEdges1, d).forall(c => afterFf.dm.get(c).mSt == MProduced))
        { ...r, mSt: MReady }
      else r
    })
    val newIntents = holedParents.map(p => { op: ISetHole(p), tenure: lead.gen })
      .union(ffTargets.map(p => { op: IClearMark(p), tenure: lead.gen }))
    all {
      intents.size() + newIntents.size() <= INTENT_CEIL,
      drv' = dmFinal,
      blds' = afterFf.bm,
      memEdges' = memEdges1,
      intents' = intents.union(newIntents),
      store' = store, pgEdges' = pgEdges, lead' = lead, budget' = budget,
      viol' = { ...viol,
        // A4/A5 hooks: holes exactly for surviving parents of un-produced removals
        holeUnsound: viol.holeUnsound or holedParents.exists(p =>
          not(removalSet.exists(c => memEdges.contains((p, c)) and drv.get(c).mSt != MProduced))),
        holeMissed: viol.holeMissed or DRVS.exists(p => and {
          not(removalSet.contains(p)), drv.get(p).mSt != MAbsent,
          removalSet.exists(c => memEdges.contains((p, c)) and drv.get(c).mSt != MProduced),
          not(dmFinal.get(p).mCh or ffTargets.contains(p))
        }),
        // A14: the fail-fast never overwrites a terminal survivor (guard above)
        terminalOverwritten: viol.terminalOverwritten or ffTargets.exists(p => isTerminal(dmRemoved.get(p).mSt)),
        wrongfulTerminalFailure: viol.wrongfulTerminalFailure or afterFf.wrongful
      },
      reach' = { ...reach,
        holeReap: reach.holeReap or holedParents != Set(),
        failFast: reach.failFast or afterFf.fired }
    }
  }

  /// PRE-FIX dispatch-probe tried arm (Wave-1 T-1.2's subject): fires on
  /// any locally-missing wanted output (not(allWantedPresent)) — the
  /// un-narrowed guard whose substitutable cell was the wrongful half —
  /// and there is no probeSettleTried in this module's alphabet, so the
  /// all-present tried cell gets no action (the D16 limbo).
  action calibProbeFailFastTriedWide(d: str): bool = all {
    leaderActive,
    drvOf(d).mSt == MReady,
    liveInterest(d),
    mustSub(d),
    drvOf(d).tried,
    not(allWantedPresent(d)),
    intents.size() + 1 <= INTENT_CEIL,
    val ff = pFailFast(drv, blds, BUILDS, memEdges, store, d)
    all {
      ff.fired,
      drv' = ff.dm,
      blds' = ff.bm,
      intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
      store' = store, memEdges' = memEdges, pgEdges' = pgEdges, lead' = lead, budget' = budget,
      viol' = { ...viol,
        wrongfulTerminalFailure: viol.wrongfulTerminalFailure or ff.wrongful,
        ffBadEffect: viol.ffBadEffect or ff.dm.get(d).mTp or (drvOf(d).mCh and not(ff.dm.get(d).mCh)),
        terminalOverwritten: viol.terminalOverwritten or isTerminal(drvOf(d).mSt),
        dispatchNoInterest: viol.dispatchNoInterest or not(liveInterest(d))
      },
      reach' = { ...reach, failFast: true }
    }
  }

  /// PRE-FIX walk-completion consumption (Wave-1 T-1.4's subject): the
  /// Broken arm consumes a stale ok=false verdict as a terminal verdict —
  /// direct fail-fast with no settlement re-probe. Identical to the
  /// production consumeWalk before the Phase-1 settlement; the violation
  /// latches (wrongfulTerminalFailure among them — the C3 oracle) are
  /// kept.
  action calibConsumeWalkNoReprobe(d: str): bool = {
    nondet idx = Set(0, 1).oneOf()
    val w = pWalkAt(drvOf(d).walks, idx)
    val res = match w.res { | WDone(r) => r | WPending => { ok: false, forgiven: OUTIDX.exclude(OUTIDX), genuine: false } }
    val crossTenure = w.tenure < lead.gen
    val baseRec = { ...drvOf(d), walks: drvOf(d).walks.select(ww => ww != w), walkConsumed: drvOf(d).walkConsumed + 1 }
    val forgivenNowWanted = res.forgiven.intersect(effWanted(d))
    val revertTo = pRevertTarget(drv, memEdges, d)
    val ev = evidence(d)
    // downgrade ingredients
    val grownNf = baseRec.nf.union(forgivenNowWanted)
    val respawn = revertTo == MDepFailed or mustSub(d)
    // ok-completion ingredients
    val descendants = pClosure(REFS, pKids(REFS, d))
    val walkedOuts = OUTIDX.exclude(res.forgiven).map(i => (d, i)).union(tuples(descendants, OUTIDX))
    val dmProduced = drv.set(d, { ...baseRec, mSt: MProduced,
      pgSt: if (baseRec.rowExists) MProduced else baseRec.pgSt, nf: OUTIDX.exclude(OUTIDX) })
    val cascade = pPromoteAndClear(dmProduced, memEdges, DRVS, Set(d))
    val okIntents = cascade.cleared.map(p => { op: IClearBoth(p), tenure: lead.gen })
    // broken fail-fast ingredients
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
        // downgrade: grow the chain-scoped never-forgive set; re-spawn when
        // the revert target is DepFailed or the node is must-substitute,
        // else revert and arm the one-shot (no genuine-failure exemption;
        // the stale probe verdict is dropped so a fresh probe must precede
        // any from-source attempt)
        drv' = drv.set(d, if (respawn)
          { ...baseRec, nf: grownNf,
            walks: baseRec.walks.append({ forgivable: OUTIDXSET.exclude(effWanted(d)).exclude(grownNf), tenure: lead.gen, res: WPending }) }
          else { ...baseRec, nf: OUTIDX.exclude(OUTIDX), mSt: revertTo, tried: true, triedGenuine: false,
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
        // ok: complete the node, end the chain, cascade C2 clears + promotes
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
        // failed walk on Vouched evidence: the C4 lazy clear (mark only), revert
        intents.size() + 1 <= INTENT_CEIL,
        drv' = drv.set(d, { ...baseRec, mTp: false, mSt: revertTo, tried: true, triedGenuine: res.genuine,
          nf: OUTIDX.exclude(OUTIDX), probedKind: PKMissing }),
        intents' = intents.union(Set({ op: IClearMark(d), tenure: lead.gen })),
        blds' = blds,
        viol' = { ...viol, clearUnsound: viol.clearUnsound or not(vouchedNow(d)) },
        reach' = { ...reach, walkFailConsumed: true,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else if (ev == EvPending or not(drvOf(d).mTp)) all {
        // failed walk, Pending evidence (keep the mark) or unmarked node:
        // revert and arm the one-shot; the genuine failure is itself a
        // missing-confirmed observation
        drv' = drv.set(d, { ...baseRec, mSt: revertTo, tried: true, triedGenuine: res.genuine,
          nf: OUTIDX.exclude(OUTIDX), probedKind: PKMissing }),
        intents' = intents,
        blds' = blds,
        viol' = viol,
        reach' = { ...reach, walkFailConsumed: true,
          crossTenureWalkConsumed: reach.crossTenureWalkConsumed or crossTenure }
      } else all {
        // failed walk on a marked Broken node: fail-fast
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

  /// Production stepDrv with the two per-derivation pre-fix actions
  /// swapped in and probeSettleTried absent (the pre-fix partition has no
  /// settlement cell).
  action calibStepDrv = {
    nondet d = DRVS.oneOf()
    any {
      probeInline(d),
      probeWalk(d),
      probeFailFastMissing(d),
      calibProbeFailFastTriedWide(d),
      probeMissingUnmarked(d),
      walkFinishes(d),
      calibConsumeWalkNoReprobe(d),
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

  /// Production stepBuild with the pre-fix reap swapped in.
  action calibStepBuild = {
    nondet b = BUILDS.oneOf()
    any {
      mergeCommit(b),
      mergeRollback(b),
      cancelBuild(b),
      buildCompletes(b),
      buildFails(b),
      calibReapNoSettle(b),
      buildsRowPurge(b)
    }
  }

  action calibStep = any {
    stepStore,
    calibStepBuild,
    calibStepDrv,
    pgApplyAny,
    pgDropAny,
    leaderLost,
    recoverAsLeader
  }
}
