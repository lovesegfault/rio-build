# Closure-evidence lifecycle invariant ↔ spec-rule map

Working artifact for the `closure-evidence-formal` campaign (the
`topdown_pruned` / `closure_hole` lifecycle). Campaign design:
`closure-evidence-formal-design.md` (A2-approved revision, adversarial review
run wf_b88941b2-973 incorporated). Verification subject and fix target:
`formal-sprint` @ `cccb4d778`; calibration source: `origin/main` @
`dfe9a5569`. The executable counterpart of this map is
`docs/spec/models/closureEvidence.qnt`.

**Status: Phase 0a (spec audit) complete; Phase 0b (model construction)
complete; Phase 0c (exhaustive checks + witness wiring) pending.**

## Phase 0a spec-audit record

New rules: `sched.evidence.closure-hole`, `sched.evidence.durability`,
`sched.evidence.settlement` (the last intentionally uncovered — see the
posture records below). Amended and version-bumped, with annotations
re-pointed in the same commits: `sched.merge.substitute-topdown+11`,
`sched.db.derivations-gc+3`, `sched.admin.clear-poison+2`. Rationale-prose
records (non-normative): the as-built fencing posture for evidence writes,
the accepted residuals (expired-at-load poison, lost-hole-stamp /
builds-row-purge conjunction, GC-after-vouch re-detection shapes), the
spawn-intent refusal churn, and the in-memory-only recovery Substituting
reset. Errata: `M_064` doc-const (frozen-header staleness; GC-erasure
preconditions).

## Invariant ↔ rule map (filled in at Phase 0b/0c)

Verdict legend (house format): COVERS / PARTIAL / GAP / CONTRADICTION.
Phase 0b fills the property rows (predicate name, statement, encoding
status); the verdict column is assigned at Phase 0c when the wired checks
produce their results. "by-construction + latch" means the production guard
exists at every encoded site, so the latch can only be set by a Phase-0d
calibration override — the latch is the override's oracle, exactly the
executor-campaign convention.

### Group A — safety (A1–A22)

| # | Property (`closureEvidence.qnt` name) | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| A1 | `noDoomedFromSourceDispatch` | A marked node with Broken evidence never gets a from-source attempt opened on it. | sched.merge.substitute-topdown+11 | encoded (latch at attemptOpen) — 0c pending |
| A2 | `stampSoundness` | The durable mark is set only by a committed pruned merge for a kept node whose submitted closure was dropped and that was not Vouched at stamp time. | sched.merge.substitute-topdown+11, sched.evidence.durability | encoded (by-construction + latch) — 0c pending |
| A3 | `clearSoundness` | The mark goes true→false only on Vouched evidence, the strict durable criterion at recovery, or the fail-fast consume. | sched.merge.substitute-topdown+11, sched.evidence.closure-hole | encoded (latches at every clear site) — 0c pending |
| A4 | `holeSoundness` | The hole is set only when an un-produced child was removed from a surviving parent. | sched.evidence.closure-hole | encoded (latches at H1–H4 sites) — 0c pending |
| A5 | `holeCompleteness` | Every leader-side removal of an un-produced child stamps every surviving parent in the same step (modulo the named residuals D10/AW1). | sched.evidence.closure-hole | encoded (latch at the reap; recovery/poison-clear by construction) — 0c pending |
| A6 | `healSoundness` | The hole goes true→false only via a full-merge heal, a Vouched-keyed both-bits clear, or removal of the node. | sched.evidence.closure-hole | encoded (by-construction + latch) — 0c pending |
| A7 | `pgMonotoneUpsert` | No merge upsert or status write ever lowers a durable evidence bit. | sched.evidence.durability | encoded (by-construction + latch at pgApply) — 0c pending |
| A8 | `brokenNeverVouches` | A childless or holed child set never vouches for a closure. | sched.merge.substitute-topdown+11 | encoded (latches at every clear/promote consumer of Vouched) — 0c pending |
| A9 | `failFastConsumesMarkKeepsHole` | A fail-fast consumes the mark, keeps the hole, sets the one-shot and terminally fails every then-interested build. | sched.merge.substitute-topdown+11, sched.evidence.closure-hole | encoded (latch over the fail-fast effect) — 0c pending |
| A10 | `failoverPreservation` | Durable bits true at recovery are carried into the recovered memory except the strict-criterion clear and the poisoned-row carve-out. | sched.merge.substitute-topdown+11, sched.evidence.durability | encoded (latch in Recover; literal pg→mem form, see stage record) — 0c pending |
| A11 | `pullRefusalNoMint` | admit_pull never mints for a Ready must-substitute node and the refusal writes nothing. | sched.merge.substitute-topdown+11 | encoded (by-construction + latch) — 0c pending |
| A12 | `chainTermination` | Every forgiven-now-wanted downgrade strictly grows the chain-scoped never-forgive set. | sched.substitute.detached+5 | encoded (latch at the downgrade) — 0c pending |
| A13 | `stampAtomicWithActivation` | Merge stamps are visible iff that merge's activation landed (one all-or-nothing intent). | sched.evidence.durability | encoded (by-construction; model-consistency baseline) — 0c pending |
| A14 | `terminalIsTerminal` | No terminal status is overwritten by a fail-fast or park. | sched.merge.substitute-topdown+11 | encoded (latch at the fail-fast arms) — 0c pending |
| A15 | `readyImpliesDeclaredDepsProduced` | No node is seeded or promoted Ready above an un-produced declared dependency; the checked half is the recovery gate over the durable relation (CE-47), the promotion half is by-construction. | sched.merge.substitute-topdown+11 | encoded (recovery latch; see ENC-0b-7) — 0c pending |
| A16 | `liveInterestRequiredForDispatch` | No probe/dispatch/attempt action fires for a node with no live interested build. | sched.merge.substitute-topdown+11 | encoded (latch at probe/attempt sites) — 0c pending |
| A17 | `noStaleTenureClearOverride` | A clear/heal intent created under tenure g never erases a bit a newer tenure stamped. **Pre-registered expected-fail probe** (stale-tenure); not in any exported conjunction. | sched.evidence.durability (fencing posture record) | encoded (latch at pgApply keyed on per-bit setter tenure) — 0c expect-violation wiring pending |
| A18 | `leaderClassEvidenceWrites` | Only the current tenure's evidence intents reach PG. **Pre-registered expected-fail probe** (stale-tenure); not in any exported conjunction. | sched.evidence.durability (fencing posture record) | encoded (latch at pgApply) — 0c expect-violation wiring pending |
| A19 | `recoveryClearCompleteness` | Recovery clears every restored mark whose strict durable criterion holds. | sched.merge.substitute-topdown+11 | encoded (by-construction + latch in Recover) — 0c pending |
| A20 | `healCompleteness` | A full merge heals every re-declared parent with a persisted hole, not keyed on the in-memory bit. | sched.evidence.closure-hole | encoded (latch in the merge) — 0c pending |
| A21 | `chainEndClearsForgiveness` | The chain-scoped forgiveness latch never outlives its chain (state form; dead latches on terminal/absent nodes allowed, see ENC-0b-14). | sched.substitute.detached+5 | encoded (state invariant) — 0c pending |
| A22 | `condemnRequiresLiveCoOwnedFailure` | The recovery failed-dep cascade condemns only on a persisted failure a live build co-owns with the parent. | sched.merge.substitute-topdown+11 | encoded (by-construction + latch in Recover) — 0c pending |

### Group B — missing families (B1–B10)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| B1 | `completedWithoutBuildImpliesWantedPresent` | A non-build Produced entry requires the live-wanted outputs present at decision time. | sched.merge.substitute-topdown+11 | encoded (latches at cache-hit/inline/walk-ok adoption) — 0c pending; expected to trip under adversarial-store as-built (GC-after-vouch accepted bound) |
| B2 | `substituteOkImpliesClosureIngested` | A consumed ok walk implies the non-forgiven walked closure was present at some instant before consumption (weak form). | sched.substitute.detached+5 | encoded (latch at consumption over everPresent) — 0c pending |
| B2-strong | `substituteOkClosureStillPresentAtConsume` | …and still present at consumption time. **Pre-registered expected-fail probe** (adversarial-store). | — (GC-after-vouch disposition) | encoded — 0c expect-violation wiring pending |
| B3 | `unknownNeverDemotes` | An Indeterminate / failed probe verdict never routes from source, never counts as missing for the prune, never fail-fasts on its own. | sched.merge.substitute-topdown+11 | encoded (latches at attempt-open/fail-fast/prune) — 0c pending |
| B4 | `noVacuousWantedVerdict` | An empty/unresolvable wanted set never satisfies an availability or forgiveness predicate. | sched.merge.substitute-topdown+11 | encoded (latch hook; the model's resolved sets are non-empty by construction) — 0c pending |
| B5 | `storedWantedMonotone` | The stored wanted union only grows. | sched.evidence.durability | encoded (by-construction + latch at the merge-intent apply) — 0c pending |
| B6 | `rollbackRestoresWantedAndEvidence` | A rolled-back merge changes nothing. | sched.merge.substitute-topdown+11 | encoded (by-construction; structural-override hook for 0d) — 0c pending |
| B7 | `pruneNotMorePermissiveThanClassification` | The prune's availability criterion is at least as strict as the dispatch-time classification criterion over the same live wanted set. | sched.merge.substitute-topdown+11 | encoded (latch at the prune decision) — 0c pending |
| B8 | `dispatchImpliesProbedThisPass` | No from-source attempt opens without a substitutability verdict consumed this pass. | sched.merge.substitute-topdown+11 | encoded (guard + latch at attemptOpen) — 0c pending |
| B9 | `staleProducedNeverUnlocksDependents` | A dependent never advances past a Produced child whose live-wanted outputs are absent (merge adoption / attempt open). | sched.merge.substitute-topdown+11 | encoded (latches at merge promote and attempt open) — 0c pending; expected to trip under adversarial-store as-built |
| B10 | `demandSetSurvivesPrune` | The prune never drops a demand-set member (structural roots ∪ explicitly-requested). | sched.merge.substitute-topdown+11 | encoded (by-construction + latch at the prune) — 0c pending |

### Group C — permissiveness (C1–C5)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| C1 | `wrongfulFailFastBoundedPerArming` | At most one wrongful fail-fast per (node, arming); re-arming is a new stamp or a recovery restoring the mark. | sched.merge.substitute-topdown+11 | encoded (per-node counter with reset-at-arming) — 0c pending |
| C1-strict | `wrongfulFailFastBoundedPerStamp` | The per-(node, stamp) form with no recovery re-arming. **Pre-registered expected-fail probe** (failover/fault-persist); the AW2 deliverable. | — (AW2 disposition) | encoded (separate counter) — 0c expect-violation wiring pending |
| C2 | `noWrongfulFromSourceDemotion` | Outside the genuine-walk-failure one-shot, the probe never routes a node from-source while every missing live-wanted output is available upstream. | sched.merge.substitute-topdown+11 | encoded (latch at the probe routing decision; see ENC-0b-9) — 0c pending |
| C3 | `noWrongfulTerminalFailureSingleTenure` | With no failover, no store GC and no upstream withdrawal, no build is wrongfully terminally failed. | sched.merge.substitute-topdown+11 | encoded (latch at the fail-fast + side conditions) — 0c pending; **suspected as-built counterexample pre-registered, see stage record** |
| C4 | `noBuildWhenWantedPresent` | A node whose live-wanted outputs are all present at merge is not queued for a from-source build. | sched.merge.substitute-topdown+11 | encoded (latch at merge classification) — 0c pending |
| C5 | `terminalBuildStopsPinning` | Only live builds' wanted outputs drive resets, forgiveness refusal and re-pinning. | sched.merge.substitute-topdown+11 | encoded (by-construction: every wanted-driven decision evaluates the live effective wanted set; latch hook) — 0c pending |

### Group L — settlement, armed-state form (L1–L3)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| L1 | `substitutingAlwaysArmed` | A Substituting node always has a walk instance in flight. | sched.substitute.detached+5 | encoded (state invariant) — 0c pending |
| L2 | `markedBrokenSettlementArmed` | No reachable D16 limbo cell (marked, Broken, tried, Ready, live interest, all live-wanted outputs present, no walk). **Pre-registered expected-fail probe** (base); its violation trace is the D16 deliverable. | sched.evidence.settlement (intentionally uncovered) | encoded (state predicate) — 0c expect-violation wiring pending |
| L3 | `liveBuildTerminalOrProgressArmed` | Every live build is terminal, all-produced, or has a progress step armed at some member. | sched.merge.substitute-topdown+11 | encoded (state invariant; D16 cell carved out per ENC-0b-6) — 0c pending |

### Non-vacuity witnesses (encoded in 0b, wired as expect-violation checks in 0c)

`canReachStamp`, `canReachHoleFromReap`, `canReachHoleFromAdminClear`,
`canReachHoleFromTtlSweep`, `canReachHoleFromRecovery`, `canReachFailFast`,
`canReachRecoveryClear`, `canReachWalkOkConsumed`, `canReachWalkFailConsumed`,
`canReachForgivenResidual` (the CE-23 residual), `canReachStaleIntentApply`,
`canReachD16Cell` (= the L2 predicate), `canReachWrongfulFfTrigger`,
`canReachTriedDemotionUpstreamHas`, `canReachPrune`, `canReachRollback`,
`canReachCrossTenureWalkConsume`, `canReachDowngradeRespawn` — 18 named
predicates covering the design §4 list of 14 plus the prune/rollback/
cross-tenure/downgrade reachability probes the regimes need.

## Contradiction / posture records

| Item | Record | Disposition |
|---|---|---|
| Fencing posture for evidence writes (D14/D15) | Entry-time leader gates only; no SQL fence on any evidence write; the only fenced statements are the three attempt-ledger transactions; the MergeDag handler is ungated past the SubmitBuild enqueue guard. Recorded as rationale prose after `sched.evidence.durability`. | Owner decision deferred to Phase 0c evidence (the A17/A18 stale-tenure probes); no fencing requirement is pre-committed. |
| D16 present-but-tried limbo cell | `sched.evidence.settlement` added (owner adopted the obligation); the as-built dispatch probe violates it, so the rule is intentionally uncovered (`tracey query uncovered`) until the Phase-1 fix lands red-first. | Settling arm chosen by the model (L2); fix in Phase 1. |

## Verify-marker status (Phase 0a)

- `sched.evidence.closure-hole`, `sched.evidence.durability`: impl markers at
  the inventoried sites; verify markers on the existing unit tests that
  already pin the behaviors (merge/recovery closure-hole battery, the
  PG-persistence and stamp-rollback tests).
- `sched.evidence.settlement`: no impl, no verify (intentional — see above).
- §7.12 zero-verify adjacents — left unannotated, recorded here instead:
  `sched.merge.stale-substitutable` (no existing test exercises the
  stale-Completed-but-substitutable stays-completed path; the nearby tests
  cover the newly-merged substitutable matrix and the reset direction),
  `sched.merge.ca-fod-substitute` (no test pins the FOD-in-path-lane
  partition; the CA tests cover the realisations lane), and
  `sched.recovery.poisoned-failed-count` (recovery tests load poisoned rows
  and bound resubmits, but none asserts the recovered build counts them in
  `failed` via `check_build_completion`). Writing those tests is follow-up
  work for Phase 1/2, not the 0a spec audit.
- Phase 0b adds no markers: model-checked properties get their `r[verify …]`
  markers at the nix/quint.nix wiring entries in Phase 0c, per house
  convention.

## Stage records

### Phase 0b — model construction (this stage)

**Artifact.** `docs/spec/models/closureEvidence.qnt`: one core module
(`closureEvidence`) + six instantiation modules (`closureEvidenceSlice`,
`closureEvidenceBase`, `closureEvidenceFaultPersist`,
`closureEvidenceFailover`, `closureEvidenceStaleTenure`,
`closureEvidenceAdversarialStore`). ~2,200 lines; 10 state variables
(per-derivation record map, per-output store map, per-build map, live and
durable edge relations, the intent set, lease, budgets, violation latches,
reachability flags); 30 actions + init; 40 design properties encoded
(22 A + 10 B + 5 C + 3 L, of which A17, A18, B2-strong, C1-strict and L2
are the pre-registered expected-fail probes kept out of the exported
`allInvariants` conjunction) plus 18 non-vacuity witnesses.
`quint typecheck` passes.

**Vertical-slice and run measurements (§2b.1 milestone).** Vertical slice =
`closureEvidenceSlice` (2 derivations, 1 build, base alphabet).
Random-simulation smoke results (rust simulator, 60-step traces):
slice 20,000 samples ≈ 1.1 s; base / fault-persist / failover 20,000 samples
each ≈ 8.5–9.2 s, no violation of `allInvariants` in any of them.
Exhaustive TLC measurement of the slice (`quint verify --backend=tlc`,
`allInvariants`, 192 workers): 256 initial states; after ~6 min of model
checking the run had explored ≈187 K distinct states (≈4.6 M generated) at
BFS depth 4 with ≈156 K states still queued and the frontier still growing
(observed throughput ≈1 M generated / 30–60 K distinct per minute); no
violation in the explored prefix; the run was stopped unconverged.
**Verdict against the §2b.1 budget: the slice exceeds both the ~1 M-distinct
and the ~60 s thresholds, so the reduction ladder must be applied before
Phase 0c wires the exhaustive regime checks.** The dominant costs are the
merge action's nondeterministic branching (root × wanted-width × explicit ×
prune × classification choices per submission) and the free initial
store/upstream nondeterminism (256 initial states at 2 derivations,
65,536 at 4). Recommended ladder application for 0c, in order: bound the
initial store/upstream assignments (not in the design's ladder but the
cheapest multiplier; needs a §4-preserving argument), then design steps
(2) per-derivation store presence in regimes that do not need output
granularity, (4) one outstanding walk per node outside the dual-walk
regime, (6) drop the second build outside the cross-build regimes, and a
narrowed merge-choice alphabet per regime. Holding the witnesses
falsifiable through the ladder is the §2b.1 contingency's condition; if a
regime still cannot converge, the executor-precedent witness-only
hold-back applies (recorded, never silent).

**Witness reachability spot-check (random simulation, failover regime,
20 K samples × 60 steps).** Reached: stamp, fail-fast, walk-ok consumed,
walk-fail consumed, hole-from-reap, recovery clear, hole-from-recovery,
prune. Not reached in this budget (deep corners needing a specific
store/upstream sequence): the D16 cell (`canReachD16Cell` — also the L2
probe) and the wrongful-fail-fast trigger state
(`canReachWrongfulFfTrigger`); demonstrating these two is a Phase 0c task
for the bounded/exhaustive checkers (both require an output that was
substitutable at stamp time to become unavailable for the walk and then
present afterwards).

**Encoding decisions (where the design under-specified or the slice
measurement forced a choice).**

- ENC-0b-1: outputs are (drv, 1|2) — one always-wanted, one possibly
  unwanted; wanted sets are stored resolved (the SQL `'{}'`-saturating union
  is observationally equivalent at this granularity).
- ENC-0b-2: `persist_status` (and build-phase persistence) is synchronous
  with the in-memory transition rather than an intent; no checked property
  reads the status-lag window, and the evidence bits / durable edges /
  links / wanted union keep the full tenure-tagged intent treatment. The
  D17 in-memory-only Substituting reset is still representable (recovery
  does not touch pgSt).
- ENC-0b-3: the batched evidence statements (W2/W4/W5) are decomposed into
  per-row intents; the merge transaction stays one all-or-nothing intent.
- ENC-0b-4: a current-tenure merge intent cannot be dropped (the handler
  observes its commit before the in-memory effects); a deposed tenure's
  pending merge intent can (its transaction may never have committed).
- ENC-0b-5: a walk finishes ok=false only with a genuinely
  missing-and-unavailable non-forgivable output at finish time
  (transient-only failures are below the model's resolution); ok=true
  ingests the REFS-closure and requires it locally-present-or-upstream.
- ENC-0b-6: the D16 cell counts as armed for L3 so L2 alone owns it.
- ENC-0b-7: A15 is split — the promotion-time half is by-construction at
  every promotion site; the checked half is the recovery-time gate over the
  durable relation (the CE-47 direction). A child re-queued or cancelled
  for another build's wider wanted set does not retroactively flag an
  already-Ready dependent (its inputs still exist); that shape is reachable
  as-built through the stale-verify demotion, the reassign path and
  cross-build cancellation, and the design's literal A15 wording would have
  flagged all of them.
- ENC-0b-8: probe-cache staleness is modeled in the positive polarity only
  (a cached hit may outlive an upstream withdrawal); the negative polarity
  (a cached miss outliving an upstream publication) is not modeled — it
  would make C2/C3 flag the documented 1 h negative-cache conservatism.
- ENC-0b-9: C2's wrongful-demotion latch is evaluated at the probe routing
  decision and requires every missing live-wanted output to be available
  upstream (the per-node fully-substitutable form); post-decision upstream
  publication races are not demotions. The design's literal "some missing
  output upstream" reading flags as-built-correct mixed-availability builds.
- ENC-0b-10: the Recover action includes the `finalize_recovered_builds`
  tail (a recovered live build with an un-produced-terminal member fails at
  recovery); the `enforce_recovered_verdicts` poison side stays with the
  environment poison action.
- ENC-0b-11: a resubmitted build slot's durable links replace the failed
  submission's links (a fresh build id in production).
- ENC-0b-12: pins exist only while a parent's attempt is open (children
  outputs); the walk-ingest grace window is not modeled (more adversarial
  GC, the direction the GC-after-vouch questions need).
- ENC-0b-13: `stampEpoch` is not a state variable; the wrongful-fail-fast
  counters are reset directly at the stamp/restore sites (same semantics).
- ENC-0b-14: A21's state form tolerates a dead latch on terminal/absent
  nodes (the resubmit-reset re-creates fresh state); it still forbids a
  live node carrying one.
- ENC-0b-15: a submission is the REFS-closure of one nondeterministic root;
  DAG-shape freedom comes from the root choice, the REFS constant, and
  overlapping/repeated builds. Contributions are uniform per merge (narrow
  `{1}` vs all). Merge-time classification, the stale-Completed verify and
  the prune availability are all judged over the live effective wanted set
  (the union including the new submission), matching the production
  `effective_wanted` semantics.

**Pre-registered observations for Phase 0c (found during 0b smoke
simulation; not model defects).**

- C3 has a suspected as-built counterexample: a substitution walk spawned
  while a wider-wanted co-build was live fails genuinely on the
  wider-only output after that co-build is cancelled; the consumption
  fail-fasts the surviving narrow build although its own wanted outputs are
  all present. C1's per-arming bound still holds; C3's zero-wrongful claim
  does not. 0c should check C3 separately and bring the trace to
  adjudication (accepted-with-rationale alongside AW5/C5, or a Phase-1
  candidate such as re-checking failed-now-unwanted paths at consumption).
- In the stale-tenure regime the D14 orphaned-build leg (a deposed
  believer's merge activating a build the live leader does not know about)
  makes durable-relation-scoped properties (notably A15's recovery gate at
  the *next* recovery) interact with state the design says is not
  model-quantified; 0c should keep A15 out of the stale-tenure invariant
  list or treat its trips there as part of the A17/A18/D14 evidence.
- B1 and B9 are expected to trip under `adversarial-store` as-built
  (GC-after-produce / GC-after-vouch — the §5 accepted bound); wire them in
  the other regimes only.

**Deviations from the design text (with reasons).** The A15 restructuring
(ENC-0b-7), the A21 widening (ENC-0b-14), the C2 wrongful-definition
refinement (ENC-0b-9), the C3 regression definition (upstream withdrawal,
not any flip), and the dropped `stampEpoch` variable (ENC-0b-13) are the
substantive deviations; each keeps the design's calibration target
falsifiable through the named latch. Everything else follows §2–§3 of the
design directly.

Later phases append here (0c: exhaustive checks, expected-fail witnesses
incl. the L2 / A17 / A18 traces; 0d: calibration verdict table; 1:
red-first fixes; 2: acceptance table; close-out: deployment-checklist
deltas and counter-signatures).
