# Closure-evidence lifecycle invariant ↔ spec-rule map

Working artifact for the `closure-evidence-formal` campaign (the
`topdown_pruned` / `closure_hole` lifecycle). Campaign design:
`closure-evidence-formal-design.md` (A2-approved revision, adversarial review
run wf_b88941b2-973 incorporated). Verification subject and fix target:
`formal-sprint` @ `cccb4d778`; calibration source: `origin/main` @
`dfe9a5569`. The executable counterpart of this map is
`docs/spec/models/closureEvidence.qnt`.

**Status: Phase 0a (spec audit) complete; Phase 0b (model construction)
complete; Phase 0c (exhaustive checking, witnesses, CI wiring) complete —
with the C3 as-built finding raised for owner adjudication (since
adjudicated CONFIRMED — 0d stage record), the exhaustive conjunctions
held as manual targets on budget, and the A17 / L2 / C1-strict probes
deferred (records in the 0c stage record). Phase 0d (calibration)
complete — verdict GO: every corpus family that the design marks
encodable falsifies through a wired or evidence-module representative
with a holding baseline, except the C5/CE-7 row (deferred to a
documented manual target, owner sign-off requested) and with the F6 /
F11 falsifications resting on the rust-simulator backend. Six permanent
quint-closure-calib-* checks are wired and green; verdicts, re-routes,
the acceptance table, and the housekeeping record for the six 0c checks
are in the 0d stage record. Post-0d triage complete (its stage record is
the last section): the three C3-adjudication model corrections are
applied and the C3 probe re-pointed to the faithful two-build scope; the
0d FailoverEx stop-and-report item is triaged to an L3 violation — a
REAL as-built finding, the second Phase-1 candidate; and the 0d
"TLC-backend discrepancy" stop-and-report item is re-diagnosed as a
budget/metric issue, not a tool bug — no exhaustive verdict is
downgraded. Phase 1 in progress: Wave 1 (the C3+D16 settlement, red-first)
is landed — `sched.evidence.settlement` is covered and the C3 wrongful
fail-fast class is closed at all three call sites; Wave 2 corrected the
model's recovery-condemnation encoding (review finding RT-2) and REFUTED
the post-0d L3 finding as a model artifact — no L3 fix is needed; Wave 2b
(owner decision 2026-05-30, disposition (b)) fixed the residual
spec-vs-code divergence red-first: production's recovery condemnation is
now co-ownership-scoped AND both poison-removal paths re-evaluate
surviving parents (`sched.poison.clear-survivor-reevaluation`), the model
mirrors both halves, and the L3 re-hunt under the corrected pair keeps the
strand closed (the red half — scoping without the promotion — re-finds it,
the calibration that the pairing is load-bearing). See the Phase-1
Wave-2/2b stage records (the last sections).**

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
| A1 | `noDoomedFromSourceDispatch` | A marked node with Broken evidence never gets a from-source attempt opened on it. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A2 | `stampSoundness` | The durable mark is set only by a committed pruned merge for a kept node whose submitted closure was dropped and that was not Vouched at stamp time. | sched.merge.substitute-topdown+11, sched.evidence.durability | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A3 | `clearSoundness` | The mark goes true→false only on Vouched evidence, the strict durable criterion at recovery, or the fail-fast consume. | sched.merge.substitute-topdown+11, sched.evidence.closure-hole | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A4 | `holeSoundness` | The hole is set only when an un-produced child was removed from a surviving parent. | sched.evidence.closure-hole | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A5 | `holeCompleteness` | Every leader-side removal of an un-produced child stamps every surviving parent in the same step (modulo the named residuals D10/AW1). | sched.evidence.closure-hole | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A6 | `healSoundness` | The hole goes true→false only via a full-merge heal, a Vouched-keyed both-bits clear, or removal of the node. | sched.evidence.closure-hole | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A7 | `pgMonotoneUpsert` | No merge upsert or status write ever lowers a durable evidence bit. | sched.evidence.durability | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A8 | `brokenNeverVouches` | A childless or holed child set never vouches for a closure. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A9 | `failFastConsumesMarkKeepsHole` | A fail-fast consumes the mark, keeps the hole, sets the one-shot and terminally fails every then-interested build. | sched.merge.substitute-topdown+11, sched.evidence.closure-hole | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A10 | `failoverPreservation` | Durable bits true at recovery are carried into the recovered memory except the strict-criterion clear and the poisoned-row carve-out. | sched.merge.substitute-topdown+11, sched.evidence.durability | HOLDS at the reduced base scope (vacuous there — its trigger lives in the deferred fault-persist/failover runs); trigger reachability pinned by the recovery/durability witnesses |
| A11 | `pullRefusalNoMint` | admit_pull never mints for a Ready must-substitute node and the refusal writes nothing. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A12 | `chainTermination` | Every forgiven-now-wanted downgrade strictly grows the chain-scoped never-forgive set. | sched.substitute.detached+5 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A13 | `stampAtomicWithActivation` | Merge stamps are visible iff that merge's activation landed (one all-or-nothing intent). | sched.evidence.durability | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A14 | `terminalIsTerminal` | No terminal status is overwritten by a fail-fast or park. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A15 | `readyImpliesDeclaredDepsProduced` | No node is seeded or promoted Ready above an un-produced declared dependency; the checked half is the recovery gate over the durable relation (CE-47), the promotion half is by-construction. | sched.merge.substitute-topdown+11 | HOLDS at the reduced base scope (the checked recovery-gate half is vacuous there; deferred with the failover run) |
| A16 | `liveInterestRequiredForDispatch` | No probe/dispatch/attempt action fires for a node with no live interested build. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A17 | `noStaleTenureClearOverride` | A clear/heal intent created under tenure g never erases a bit a newer tenure stamped. **Pre-registered expected-fail probe** (stale-tenure); not in any exported conjunction. | sched.evidence.durability (fencing posture record) | expected-fail probe — NOT yet produced (deferred; see the 0c deferred-probe record and the fencing-evidence summary) |
| A18 | `leaderClassEvidenceWrites` | Only the current tenure's evidence intents reach PG. **Pre-registered expected-fail probe** (stale-tenure); not in any exported conjunction. | sched.evidence.durability (fencing posture record) | expected-fail probe — CONFIRMED violated (5-state trace: a deposed tenure's merge transaction lands after the successor's recovery; the D14 leg) |
| A19 | `recoveryClearCompleteness` | Recovery clears every restored mark whose strict durable criterion holds. | sched.merge.substitute-topdown+11 | HOLDS at the reduced base scope (vacuous there — its trigger lives in the deferred fault-persist/failover runs); trigger reachability pinned by the recovery/durability witnesses |
| A20 | `healCompleteness` | A full merge heals every re-declared parent with a persisted hole, not keyed on the in-memory bit. | sched.evidence.closure-hole | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A21 | `chainEndClearsForgiveness` | The chain-scoped forgiveness latch never outlives its chain (state form; dead latches on terminal/absent nodes allowed, see ENC-0b-14). | sched.substitute.detached+5 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| A22 | `condemnRequiresLiveCoOwnedFailure` | The recovery failed-dep cascade condemns only on a persisted failure a live build co-owns with the parent. | sched.merge.substitute-topdown+11, sched.recovery.failed-dep-cascade+2 | HOLDS at the reduced base scope (vacuous there — its trigger lives in the deferred fault-persist/failover runs); trigger reachability pinned by the recovery/durability witnesses. As of Wave 2b the in-DAG recompute — the second recovery condemnation mechanism, previously unscoped (the Wave-2 residual finding) — carries the same co-ownership scoping in production (`any_co_owned_dep_terminally_failed`) and in the model (`pInDagCondemnCriterion`); the latch stays keyed on the cascade arm |

### Group B — missing families (B1–B10)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| B1 | `completedWithoutBuildImpliesWantedPresent` | A non-build Produced entry requires the live-wanted outputs present at decision time. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record); did NOT trip under adversarial-store in the bounded exploration (0b expectation corrected) |
| B2 | `substituteOkImpliesClosureIngested` | A consumed ok walk implies the non-forgiven walked closure was present at some instant before consumption (weak form). | sched.substitute.detached+5 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| B2-strong | `substituteOkClosureStillPresentAtConsume` | …and still present at consumption time. **Pre-registered expected-fail probe** (adversarial-store). | — (GC-after-vouch disposition) | expected-fail probe — CONFIRMED violated (6-state trace: store GC removes a walked output between the walk's finish and its consumption) |
| B3 | `unknownNeverDemotes` | An Indeterminate / failed probe verdict never routes from source, never counts as missing for the prune, never fail-fasts on its own. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| B4 | `noVacuousWantedVerdict` | An empty/unresolvable wanted set never satisfies an availability or forgiveness predicate. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| B5 | `storedWantedMonotone` | The stored wanted union only grows. | sched.evidence.durability | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| B6 | `rollbackRestoresWantedAndEvidence` | A rolled-back merge changes nothing. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| B7 | `pruneNotMorePermissiveThanClassification` | The prune's availability criterion is at least as strict as the dispatch-time classification criterion over the same live wanted set. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| B8 | `dispatchImpliesProbedThisPass` | No from-source attempt opens without a substitutability verdict consumed this pass. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| B9 | `staleProducedNeverUnlocksDependents` | A dependent never advances past a Produced child whose live-wanted outputs are absent (merge adoption / attempt open). | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record); trips under adversarial-store as expected (GC-after-produce trace recorded) |
| B10 | `demandSetSurvivesPrune` | The prune never drops a demand-set member (structural roots ∪ explicitly-requested). | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |

### Group C — permissiveness (C1–C5)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| C1 | `wrongfulFailFastBoundedPerArming` | At most one wrongful fail-fast per (node, arming); re-arming is a new stamp or a recovery restoring the mark. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| C1-strict | `wrongfulFailFastBoundedPerStamp` | The per-(node, stamp) form with no recovery re-arming. **Pre-registered expected-fail probe** (failover/fault-persist); the AW2 deliverable. | — (AW2 disposition) | expected-fail probe — NOT producible within the §2b ceilings (the AW2 loop needs a second availability transition per output); recorded as a deviation, not wired |
| C2 | `noWrongfulFromSourceDemotion` | Outside the genuine-walk-failure one-shot, the probe never routes a node from-source while every missing live-wanted output is available upstream. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| C3 | `noWrongfulTerminalFailureSingleTenure` | With no failover, no store GC and no upstream withdrawal, no build is wrongfully terminally failed. | sched.merge.substitute-topdown+11 | VIOLATED as-built — CONFIRMED defect (C3 adjudication + post-0d triage): the violation needs TWO live builds (the faithful trace is in the post-0d triage record); after the adjudication's model corrections C3 HOLDS at the single-build BaseEx scope (the 0c 11-state trace is refuted). Excluded from the wired conjunctions, pinned by the wrongful-terminal-failure expect-violation check at the closureEvidenceC3Duo probe scope; Phase-1 red-first candidate |
| C4 | `noBuildWhenWantedPresent` | A node whose live-wanted outputs are all present at merge is not queued for a from-source build. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| C5 | `terminalBuildStopsPinning` | Only live builds' wanted outputs drive resets, forgiveness refusal and re-pinning. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) (latch is a by-construction hook; falsifiability owned by the 0d calibration override) |

### Group L — settlement, armed-state form (L1–L3)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| L1 | `substitutingAlwaysArmed` | A Substituting node always has a walk instance in flight. | sched.substitute.detached+5 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| L2 | `markedBrokenSettlementArmed` | No reachable D16 limbo cell (marked, Broken, tried, Ready, live interest, all live-wanted outputs present, no walk). **Pre-registered expected-fail probe** (base); its violation trace is the D16 deliverable. | sched.evidence.settlement (intentionally uncovered) | expected-fail probe — NOT yet produced (the D16 cell is ≥16 steps deep at duo scope; deferred-probe record in the 0c stage record) |
| L3 | `liveBuildTerminalOrProgressArmed` | Every live build is terminal, all-produced, or has a progress step armed at some member. | sched.merge.substitute-topdown+11, sched.poison.clear-survivor-reevaluation | No violation found at the reduced base scope (0c run record); the post-0d triage's failover-scope violation is **REFUTED as a production defect — model artifact of the recovery-condemnation gap (Phase 1 Wave 2)**: the traced strand needed recovery to leave the parent Queued above its still-poisoned child, which pre-Wave-2b production never did (the unscoped `compute_initial_states` condemnation closed it). **Wave 2b narrowed that mechanism** (the residual-finding fix: co-ownership scoping) **and added its replacement closure**: the poison-clear survivor re-evaluation promotes the spared parent when the child is removed. Re-hunt under the scoped pair (FailoverEx + Duo, both backends): no violation; the red half (scoping without promotion) re-finds the 9-state strand under TLC — the pairing, not the unscoped condemnation, now closes the strand. Wave-2/2b stage records |

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
| Fencing posture for evidence writes (D14/D15) | Entry-time leader gates only; no SQL fence on any evidence write; the only fenced statements are the three attempt-ledger transactions; the MergeDag handler is ungated past the SubmitBuild enqueue guard. Recorded as rationale prose after `sched.evidence.durability`. | **RESOLVED — normative fence implemented Phase 1 Wave 3 (owner decision 2026-05-30, FENCE EVERYTHING / D15 option (b))**: every evidence write carries the tenure's serving generation and is applied only at-or-above the durable claims floor; `sched.evidence.durability+2` makes it normative. See the Wave-3 stage record for the statement inventory and residuals. |
| D16 present-but-tried limbo cell | `sched.evidence.settlement` added (owner adopted the obligation); the as-built dispatch probe violates it, so the rule is intentionally uncovered (`tracey query uncovered`) until the Phase-1 fix lands red-first. | Settling arm chosen by the model (L2); fix in Phase 1. |

## Verify-marker status (Phase 0a)

- `sched.evidence.closure-hole`, `sched.evidence.durability`: impl markers at
  the inventoried sites; verify markers on the existing unit tests that
  already pin the behaviors (merge/recovery closure-hole battery, the
  PG-persistence and stamp-rollback tests).
- `sched.evidence.settlement`: no impl, no verify at Phase 0a (intentional —
  see above). **Closed by Phase 1 Wave 1** (the C3+D16 settlement fix): impl
  markers at the dispatch-probe partition's settlement cells, the
  `settle_broken_marked_root` helper and its `handle_substitute_complete`
  Broken-arm routing (`actor/dispatch.rs`), and the reap-survivor hook
  (`actor/build.rs`); verify markers on the D16 limbo test
  (`marked_broken_tried_present_node_settles`), the C3 two-build test
  (`stale_walk_failure_does_not_fail_build_with_present_outputs`), the
  reap-settlement test (`reap_survivor_settles_at_reap_time`), and the
  updated topdown battery in `actor/tests/merge.rs`. The rule no longer
  appears in `tracey query uncovered`.
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

### Phase 0c — exhaustive checking, witnesses, CI wiring (this stage)

**Reduction ladder and measurements (§2b.1 milestone follow-up).** All
measurements: `closureEvidenceSlice` (2 derivations / 1 build) unless
stated, `quint verify --backend=tlc` via the house apalache-server
prelude, TLC `auto` workers on a 192-core builder. The 0b baseline and
each ladder step, in application order:

| Step | Measurement | Outcome |
|---|---|---|
| 0b baseline (unmodified model) | 256 initial states; ≈68 K distinct / 1.95 M generated at 3 min, BFS depth 4, queue 60 K and growing (≈32 K distinct/min) | confirms the 0b verdict: far over the ~1 M-distinct / ~60 s slice budget |
| Step-relation grouping (no state-space change: the transition relation is re-grouped per nondet argument so the checker stops evaluating every action under the full node × build × output product) + ladder step CE-L1: per-derivation initial store/upstream assignment | 16 initial states; ≈1.36 M distinct / 5.9 M generated at 4 min, depth 12, queue 912 K growing; distinct-state throughput ≈10× the baseline | both kept; still non-convergent |
| Ladder step (2) (per-derivation store/upstream environment letters, `STORE_PER_OUTPUT = false`) + step (4) (one outstanding walk per node, `WALKS_PER_NODE = 1`) + forgiven-set narrowed to the spawn-time forgivable snapshot | violated `allInvariants` at depth 11 in 12 s — see the encoding-fix record below; after the fixes: ≈1.94 M distinct / 7.5 M generated at 3 min, depth 16, queue 1.14 M growing | steps kept (the violation was an encoding artifact the steps exposed, not caused); still non-convergent |
| Reach-flag tracking gated off for exhaustive runs (trial) | trajectory indistinguishable from the previous step at the 1–3 min marks | abandoned (reverted): the monotone history flags do not measurably multiply the explored prefix; tracking stays unconditional as in 0b |
| Exhaustive-scope instantiations (`closureEvidence{Base,FaultPersist,Failover}Ex`: 2 derivations, 1 build, 2 submissions, poison 1, cancel 1, walks 1, per-derivation store letters) | `closureEvidenceBaseEx` `allInvariants`: violated at ≈1.77 M distinct / depth 16 after ≈165 s — identified as the C3 falsification (the Phase 0c finding, recorded below); with C3 excluded the closing run found no further violation through 16.4 M distinct states (unconverged — the closing-run record below) | the conjunction is a manual target, not a wired check (budget) |

**Encoding fixes (model-faithfulness corrections found by the first
exhaustive runs; both verified against the production code at
`formal-sprint` @ cccb4d778).**

- CE-FIX-1 (poison cascade over-reach): the model's
  `poisonArrival` cascaded DependencyFailed onto every non-terminal
  in-DAG ancestor, including Substituting/Open/Failed parents.
  Production's `cascade_dependency_failure` (completion.rs:3446–3452)
  cascades only to parents that have not started — Queued/Ready/Created
  — so an in-flight walk or attempt keeps its chance and its own
  completion settles the parent. The model now matches.
- CE-FIX-2 (merge re-probe blocked by stale walk records): the
  pending-substitute classification refused to re-spawn a walk for a
  node still carrying a stale (never-Substituting) walk record, which
  under the walks-per-node ceiling left a re-merged node Queued with
  nothing armed. Production's existing-node re-probe (merge.rs:481–499,
  the I-094 reprobe-substitutable lane at merge.rs:841–851) spawns a
  fresh fetch regardless of an earlier detached task whose late result
  the handler will discard. The model's classification now replaces the
  stale records with the fresh spawn.
- The TLC counterexample that exposed both (the L3
  `liveBuildTerminalOrProgressArmed` violation at slice scope, depth
  6–11): a poisoned child's cascade flipped a Substituting parent to
  DependencyFailed (not production behavior — CE-FIX-1), the failed
  build's pruned resubmit could not re-arm the parent because its stale
  walk record blocked the re-spawn (not production behavior —
  CE-FIX-2), and the subsequent poison-clear left a childless Queued
  parent with live interest and nothing armed. With both fixes the
  shape is no longer reachable (re-checked through ≈2 M distinct
  states); it is recorded here as a model artifact, not an as-built
  finding.

**Pre-registered observation (b): B1/B9 under adversarial-store.** B9
(`staleProducedNeverUnlocksDependents`) violates as expected: the
4-state counterexample is GC-after-produce — a cache-hit-Produced child
loses its only wanted output to the store GC and a from-source attempt
still opens for its dependent. B1
(`completedWithoutBuildImpliesWantedPresent`) did NOT trip in the
explored prefix (20 K × 60-step simulation): every Produced-entry site
checks presence at decision time as built, and GC after the decision is
B9/B2-strong territory, not B1 — the 0b stage record's "B1 and B9 are
expected to trip" note was over-broad on the B1 half. Disposition
unchanged: neither is wired in the adversarial-store regime; both stay
in the other regimes' conjunctions.

**Expected-fail probes (design §3 / §8 0c gate).**

- A18 `leaderClassEvidenceWrites` — confirmed violated
  (closureEvidenceStaleTenure). Counterexample (5 states): tenure 1's
  pruned merge stamps and activates in one transaction whose apply is
  still pending when the lease is lost; tenure 2 recovers from a PG
  that has no row for the node; the tenure-1 transaction then lands —
  a stamped row plus an Active build the new leader's memory does not
  know about (the D14 orphaned-build leg, exhibited rather than just
  asserted).
- B2-strong `substituteOkClosureStillPresentAtConsume` — confirmed
  violated (closureEvidenceAdversarialStore). Counterexample (6
  states): walk finishes ok and ingests the closure, the store GC
  removes one ingested output before the completion is consumed, the
  consumption still adopts Produced — the GC-after-vouch window the §5
  disposition accepts; the weak form (present at some instant before
  consumption) holds.
- A17 `noStaleTenureClearOverride`, C1-strict
  `wrongfulFailFastBoundedPerStamp`, L2 `markedBrokenSettlementArmed`
  (the D16 cell): being driven at duo probe scope
  (closureEvidence{StaleDuo,FailoverDuo,Duo}) — verdicts and trace
  summaries recorded below once the bounded hunts complete.

**A17/A18 fencing evidence (feeds the §9.1 owner decision; D14/D15).**
The model's evidence-write intents partition exactly as the design
§9.1 recommendation anticipated:

- Late writes that can ERASE a newer tenure's evidence (the dangerous
  direction A17 quantifies): the both-bits clear (W2,
  `clear_topdown_pruned_by_hashes`), the mark-only clear (W3,
  `clear_topdown_pruned_by_hash` — the fail-fast's PG counterpart), and
  the heal (W5, `clear_closure_hole_by_hashes`). W2 and W5 are the two
  that can erase a newer tenure's HOLE — the direction the design
  flags for fencing options (c)+(d); W3 erases only the mark, whose
  stale-true/stale-false asymmetry the durability rule already accepts
  (one more wrongful fail-fast cycle worst-case, the AW2 bound).
- Late writes that cannot erase but can introduce state for a dead
  tenure: the merge transaction (W1 + edges + links + activation,
  OR-monotone on both bits) and the hole stamp (W4). The confirmed A18
  counterexample is exactly the late merge transaction: tenure 1's
  pruned merge (stamp + activation) lands after tenure 2's recovery,
  leaving a stamped row and an Active build the live leader has no
  in-memory knowledge of — the D14 orphaned-build leg, now exhibited
  by the model rather than only argued.
- A17 (a stale W2/W5 erasing a hole or mark a NEWER tenure stamped)
  needs the override interleaving: tenure g emits the clear/heal, the
  apply is delayed past a failover, tenure g+1 re-stamps the same bit
  (recovery hole stamp or a new pruned merge), and only then does the
  tenure-g statement land. The bounded hunt for the concrete trace is
  recorded below when it completes.

**Pre-registered observation (a): the C3 single-tenure counterexample —
REPRODUCED, classified REAL (the Phase 0c finding).** The exhaustive
run of the reduced base scope falsified
`noWrongfulTerminalFailureSingleTenure` with an 11-state counterexample
that needs only ONE build — the wider-wanted "co-build" of the 0b
suspicion is played by the same build's own earlier, wider submission.
The trace (closureEvidenceBaseEx, TLC, depth 11; archived in the run
transcript):

1. `mergeCommit` — b1 submits the full closure {d1→d2} with the wide
   wanted set; nothing is present or upstream, the optimistic
   classification routes both nodes into substitution walks
   (`spawn_substitute_fetches`); d1's walk snapshot has an empty
   forgivable set (everything is wanted).
2. `walkFinishes(d1, ok=false)` — d1's walk fails genuinely: at this
   instant its outputs are missing and unavailable.
3. `storeIngestAt(d1)` — d1's outputs are ingested out-of-band and are
   now PRESENT (and stay present for the rest of the trace).
4. `walkFinishes(d2, ok=false)`, then `consumeWalk(d2)` — d2's failure
   is consumed (unmarked ⇒ revert to Ready, `substitute_tried`).
5. `pgApplyAny` — the merge transaction lands (rows, edges, links).
6. `poisonArrival(d2)` — d2 is poisoned; the cascade skips d1 (it is
   Substituting — production `cascade_dependency_failure` skips parents
   that have started); b1 fails.
7. `mergeCommit` — b1's directed resubmit goes through the topdown
   prune: the demand set {d1} is fully available (d1's outputs are
   present), d2 is dropped, and d1 — still Substituting, still carrying
   the stale walk — is STAMPED `topdown_pruned`.
8. `poisonClear(d2)` — the poison clear removes d2 and stamps d1's
   `closure_hole`; d1 is now marked + holed ⇒ evidence Broken.
9. `consumeWalk(d1)` — the stale ok=false verdict from step 2 is
   finally consumed. The routing re-checks only the forgiven-now-wanted
   direction, sees marked + Broken, and takes the fail-fast:
   `fail_fast_topdown_pruned_root` terminally fails b1 — although every
   output b1 wants has been present in the store since step 3.

Code walk along the trace (formal-sprint @ cccb4d778), step by step:
the detached walk's verdict is a mailbox message consumed arbitrarily
later (dispatch.rs:345–574, 2058+ — step 2 vs step 9 ordering is real);
the resubmit's existing-node re-probe excludes in-flight nodes, so the
Substituting root is neither cache-checked nor re-probed at step 7
while the batch upsert still stamps it (merge.rs:481–499 exclusion,
batch.rs stamp); the poison cascade skips Substituting parents
(completion.rs:3446–3452), which is exactly what keeps d1's stale walk
alive across b1's failure; `handle_substitute_complete`'s ok=false
routing re-checks the live wanted set only for the forgiven outputs
(dispatch.rs:616–667) and then branches on the evidence classifier; and
`fail_fast_topdown_pruned_root` (dispatch.rs:985–1099) parks the node,
consumes the mark and terminally fails every interested build with no
re-check of the store or of the failing build's own wanted-output
availability. Every step is as-built behavior; none of it depends on a
fault, a failover, a GC or an upstream change.

Classification: **REAL as-built defect candidate** (not a model
artifact, not encoding-induced): stale walk-failure evidence, produced
before the outputs became available, terminally fails a resubmitted
build whose wanted outputs are all present at consumption time. Per the
design §8 pre-registered triage rule (a counterexample violating a
property in `base` is a defect and gets a red-first fix) this is a
Phase-1 red-first candidate — candidate fix shapes: re-check the
consuming node's live-wanted presence (or re-run the FMP partition)
before the SubstituteComplete fail-fast arm, or drop stale verdicts
whose spawn-time store snapshot is older than the last reconciliation —
with accepted-with-rationale alongside AW5/C5 as the alternative
disposition; owner adjudication required (stop-and-report raised in the
Phase 0c report). NOT fixed in this phase. Wiring consequences: C3 is
excluded from the wired exhaustive conjunctions (its falsification is
pinned by the expect-violation check
`quint-closure-evidence-probe-wrongful-terminal-failure` instead, the
nodeclaim-campaign precedent for pre-registered as-built defects), and
the per-property table records C3 as "violated as-built (defect
candidate, Phase 1)".

**Pre-registered observation (the D14/A15 stale-tenure interaction).**
Not observed to trip in the bounded stale-tenure exploration (20 K ×
60-step simulation; A15's recovery latch evaluates the same durable
co-ownership criterion the recovery's own condemnation pass uses, so a
trip requires an inconsistency the late-landing merge alone does not
create). The D14 orphaned-build leg itself is exhibited by the A18
counterexample (above) and carried into the §9.1 consequence list; A15
is NOT excluded from any wired conjunction (stale-tenure has no
exhaustive check — see the regime scope record), so the 0b
recommendation ("keep A15 out of the stale-tenure invariant list or
treat its trips as A17/A18/D14 evidence") is moot for the wiring and
recorded here as resolved-by-scope.

**Non-vacuity witness reachability.** Sixteen of the eighteen canReach*
witnesses were demonstrated reachable at design-scale regimes by the
bounded random exploration (4 000 × 45-step traces each, seconds per
witness): stamp, prune, rollback, fail-fast, hole-from-admin-clear,
hole-from-ttl-sweep, walk-ok-consumed, walk-fail-consumed,
forgiven-residual, tried-demotion-upstream-has, downgrade-respawn
(closureEvidenceBase); hole-from-reap, hole-from-recovery,
recovery-clear (closureEvidenceFailover); stale-intent-apply,
cross-tenure-walk-consume (closureEvidenceStaleTenure). The two
pre-identified deep corners — the D16 cell (canReachD16Cell ≡ the L2
predicate) and the wrongful-fail-fast trigger state
(canReachWrongfulFfTrigger) — were not reached by simulation (as in
0b) and are driven by the bounded checker at duo scope; their outcomes
are recorded with the expected-fail probes. The wired witness checks
run against the smallest scope that contains each trigger (the
exhaustive-base scope for the single-tenure ones, duo for the
cross-build ones, failover-Ex for the recovery ones, design-scale
stale-tenure for the stale ones) so each check both stays within the
per-check budget and guards exactly the regime its sibling exhaustive
check explores.

**Expected-fail probe C1-strict — encoding-level finding.** The AW2
loop the design expects C1-strict to exhibit ("lost clear → failover →
restored mark → second wrongful fail-fast … while upstream flaps")
needs the same output to change availability at least twice: once so
the first fail-fast is wrongful (missing locally, available upstream),
and again so a second walk failure can re-arm `substitute_tried` after
the recovery wiped it (the walk can only fail genuinely on an output
that is missing AND unavailable). The §2b ceiling of at most one
`UpstreamChange` per output (and no store GC in the failover regime)
excludes that flap, so within the modeled ceilings the strict
per-(node, stamp) bound appears to HOLD rather than falsify — the
20 000 × 60-step and 200 000 × 45-step bounded hunts found no
counterexample. Recorded as a deviation from the design's §3c
expectation with this analysis; the per-arming bound C1 (the bound the
as-built system documents) is unaffected and stays in the exhaustive
conjunctions. If the owner wants the AW2 loop exhibited in-model, the
ceiling needs a dedicated probe regime that allows a second
availability transition (one more upstream flip, or the
adversarial-store GC), which is a §2b ceilings change and is left to
the owner rather than made silently here.

**Regime scopes after the ladder (what runs where).**

| Module | Scope | Role |
|---|---|---|
| closureEvidenceSlice | 2 drv / 1 build, ladder granularity | §2b.1 measurement vehicle only (regression tracking; not wired) |
| closureEvidenceBase / FaultPersist / Failover / StaleTenure / AdversarialStore | design §2b constants (4 drv / 2 builds, per-output store, dual walks) | witness + expected-fail-probe vehicles; exhaustive checks at this scale did not converge within a gate-compatible budget (the 0b slice measurement and the ladder table above carry the figures) and are held as unwired manual targets — the executor-campaign precedent; owner adjudication at close-out |
| closureEvidenceBaseEx / FaultPersistEx / FailoverEx | 2 drv / 1 build / 2 submissions, ladder granularity | the manual exhaustive targets (`asBuiltHoldInvariants`); held back from CI on budget, run records below |
| closureEvidenceDuo | 2 drv / 2 builds, no faults | cross-build probe/investigation scope: the L2/D16 probe, the H1 reap witness, the wrongful-FF-trigger witness, the C3 investigation |
| closureEvidenceC3Duo (added post-0d) | 2 drv / 2 builds, cancel/reap + per-output store only (no poison/attempts/faults/GC), INTENT_CEIL 4 | the C3 probe scope: the confirmed two-build wrongful-terminal-failure trace inside a wired check's budget (post-0d triage record) |
| closureEvidenceStaleDuo / FailoverDuo | 2 drv / 2 builds + failovers (+ stale apply / + lost writes) | probe scopes for A17 and C1-strict |

What the reduced exhaustive scope gives up relative to the design
constants, and where each loss is covered instead: cross-build
interleavings (reap-survivor verdicts, co-build wanted interactions,
DeliverExisting re-delivery) — covered by the duo-scope witnesses and
probes plus the design-scale simulation evidence, exhaustively only at
Phase 0d calibration scope; the 4-node diamond topology (multi-parent
holes, shared-child heals) — witnesses confirm reachability at design
scale, the 2-node parent/child pair carries the per-site latches; dual
outstanding walks — stale-tenure-only behavior, witnessed there; the
second/third submission interleavings beyond two — the F6/F1
calibration rows already pre-register 3-submission constants for 0d.

**Deferred probes (bounded hunts did not produce the counterexample
within a wired check's budget; recorded, not silently dropped).**

- A17 `noStaleTenureClearOverride`: the override interleaving needs a
  clear-class statement (W2/W3/W5) emitted and left pending under
  tenure g, the same bit durably cleared and then re-stamped by tenure
  g+1, and only then the stale statement landing — by hand-derivation
  ≥14 steps even at the two-node/two-build stale scope
  (closureEvidenceStaleDuo). Hunted: 20 000 × 60-step and 200 000 ×
  45-step random exploration (design-scale and duo-scale stale
  regimes), plus a bounded BFS that reached depth ≈7 of the duo-scale
  space before being stopped for budget. Not produced; the probe check
  is therefore not wired. What the §9.1 decision still gets from 0c:
  the stale-apply window itself is exhibited (the A18 trace), the
  erasing statements are enumerated (W2/W3/W5, with W2/W5 the
  hole-erasing pair the design's options (c)+(d) target), and the
  required interleaving is precisely characterised above —
  the deferred hunt is a longer-budget BFS of closureEvidenceStaleDuo
  (or a Phase-1 db-level injected-stale-write test, which the design
  already sketches as the red-first shape for the fencing fix).
- L2 / D16 `markedBrokenSettlementArmed`: the limbo cell needs the
  mark, the hole, `substitute_tried` and full presence to assemble on
  one node with no intermediate step settling it — by hand-derivation
  ≥16 steps at duo scope (the assembly path mirrors the production
  story: a stale positive availability at the pruned re-merge, a
  poison-clear hole, a tried one-shot from an earlier failed walk, and
  a late ingest). Hunted: 200 000 × 45-step duo simulation plus the 0b
  20 000-sample failover simulation; not reached. The probe check is
  not wired; closureEvidenceDuo remains the documented probe scope and
  the D16 unit-test shape from design §5 (drive a node to
  marked+Broken+tried, report outputs present, assert it settles)
  remains the recommended red-first vehicle for the §9.2 decision.
- canReachWrongfulFfTrigger: same depth class as the D16 cell (the 0b
  record had flagged both as needing checker support); not wired. The
  C-group's non-vacuity at the wired scopes is carried by the C3
  falsification probe (an actual wrongful fail-fast is exhibited there)
  and the tried-demotion witness.

**Stop-and-report items raised by this stage (for the campaign owner /
orchestrator).**

1. The C3 falsification (REAL as-built defect candidate) — the
   single-tenure wrongful terminal failure recorded above. Per the §8
   triage rule a base-regime counterexample is a defect with a
   red-first fix; the fix is NOT made in this phase and the finding is
   raised for owner adjudication (fix in Phase 1 vs
   accepted-with-rationale alongside AW5/C5).
2. Design-scale exhaustive checks held back on budget (the §2b.1
   contingency): the four-node / two-build regimes do not converge
   within a gate-compatible budget on the 0c builder; the wired
   exhaustive coverage is the reduced two-node scope. Owner
   adjudication at close-out, exactly as the executor campaign's
   non-convergent regimes were handled.
3. Three of the design's pre-registered expected-fail probes (A17, L2,
   plus the C1-strict probe whose loop the §2b ceilings exclude) could
   not be produced as wired expect-violation checks within this
   stage's budgets; their records above carry the analysis and the
   deferred-hunt plan. A18 and B2-strong are wired and green.

**The wired checks (nix/quint.nix; per-check budget minutes-class — the
fixed apalache-server/conversion warm-up of ~1.5–2.5 min plus the TLC
time given per check).**

| Check | Module / target | What it pins | TLC time class |
|---|---|---|---|
| (manual target, not wired) closureEvidenceBaseEx × `asBuiltHoldInvariants` | — | the exhaustive single-tenure verdicts of the per-property table; held back from CI on budget (the §2b.1 contingency) | the 0c run record below |
| quint-closure-evidence-probe-stale-evidence-write | closureEvidenceStaleTenure, A18 | the unfenced-evidence-write window (D14 leg) | violation at depth 4–5; seconds |
| quint-closure-evidence-probe-vouched-closure-gone | closureEvidenceAdversarialStore, B2-strong | the GC-after-vouch accepted bound | violation at depth 5–6; seconds |
| quint-closure-evidence-probe-wrongful-terminal-failure | closureEvidenceBaseEx, C3 *(re-pointed to closureEvidenceC3Duo by the post-0d triage — the BaseEx violation was the refuted single-build trace)* | the Phase 0c finding (stale walk-failure fail-fast) until its Phase-1 disposition | violation at depth 11; ≈3–5 min |
| 11 single-tenure witnesses (stamp, prune, rollback, fail-fast, hole-admin-clear, hole-ttl-sweep, walk-ok, walk-fail, forgiven-residual, tried-demotion, downgrade-respawn) | closureEvidenceBaseEx | non-vacuity of the wired base conjunction | violations at depth ≤8; seconds |
| witness-hole-reap | closureEvidenceDuo | H1 (reap of a shared parent's un-produced child) | shallow; seconds-to-minutes |
| witness-hole-recovery, witness-recovery-clear | closureEvidenceFailoverEx | H4 and the strict-criterion recovery clear | shallow; seconds-to-minutes |
| witness-stale-intent-apply, witness-cross-tenure-walk | closureEvidenceStaleTenure | the stale-apply window and the token-less walk consumption | shallow; seconds-to-minutes |

Not wired, with records above: the design-scale exhaustive regimes
(budget), the reduced exhaustive conjunctions (manual targets — budget),
the fault-persist / failover scopes (not yet measured), the A17 / L2 /
C1-strict probes and the wrongful-ff-trigger witness (deferred hunts),
and the slice module (measurement vehicle only). Because witnesses and
expect-violation probes carry no tracey markers by house convention and
the exhaustive conjunction checks are not wired in this stage, Phase 0c
adds no `r[verify …]` markers; the closure-evidence rules keep their
Phase 0a unit-test verify markers, and the model-checked markers land at
the wiring points when the exhaustive checks are wired.

**The reduced-base closing run (the 0c exhaustive-attempt record).**
`closureEvidenceBaseEx` × `asBuiltHoldInvariants` (allInvariants minus
C3), TLC `auto` workers on the 192-core 0c builder (shared for most of
the evening with another campaign's checker jobs): **no violation
found** through 16,413,593 distinct states (70.1 M generated, BFS depth
21, ≈6.9 M states still queued) when the run was stopped unconverged at
≈15.5 min of checker time. Together with the property-identification
run (every conjunct checked individually over 2.87 M distinct states,
only C3 tripping) this is the basis for the table's "no violation found
at the reduced base scope" rows; the converged figure is left to the
manual target's first uncontended run (the command is recorded at the
wiring point in nix/quint.nix). The same run is the §2b.1 budget
verdict in the concrete: even the two-node, one-build scope is a
multi-ten-minute exhaustive check, so no exhaustive conjunction is
wired into CI in this stage — the wired CI surface is the witness +
expect-violation set, and exhaustive convergence (reduced scopes first,
design scale only if the owner re-scopes the budget) joins the
deferred-runs list alongside the A17 / L2 hunts.

**0c → 0d handoff.** The calibration phase consumes: the per-property
table above (each override must falsify against a baseline run at the
same constants as its override, per the calibration README), the
asBuiltHoldInvariants conjunction (the baseline HOLD set for
attributability runs), the C3 exclusion (its falsification is the
as-built behavior, so C3 cannot serve as a calibration baseline until
the Phase-1 disposition), and the deferred-probe records (the A17 / L2
hunts and the C1-strict ceiling question), which 0d may fold into its
own runs if the owner re-scopes them.

### Phase 0d — calibration (this stage)

**Housekeeping: the six 0c checks that were still building at the 0c
report.** Final status (with the wiring-level fixes this stage made):

| Check | 0c wiring | 0d outcome |
|---|---|---|
| witness-hole-reap | closureEvidenceDuo | builds GREEN as wired |
| witness-recovery-clear | closureEvidenceFailoverEx | builds GREEN as wired |
| probe-vouched-closure-gone (B2-strong) | closureEvidenceAdversarialStore (design scale) | design-scale TLC never completes (the 0c "violation at depth 5–6; seconds" projection was wrong — 40+ min reaching BFS depth 2–3 under the parallel gate); **re-pointed** to the new reduced `closureEvidenceAdversarialStoreEx` scope (this stage adds the module), where TLC produces the GC-after-vouch violation at depth 7 in ~40 s; builds GREEN re-pointed |
| probe-stale-evidence-write (A18) | closureEvidenceStaleTenure (design scale) | design-scale TLC never completes (one JVM SIGSEGV + retries reaching only depth 3 in 40+ min); the duo-scale re-point hits the TLC-backend discrepancy below; **unwired** — documented manual target (rust simulator: violation in <1 s at closureEvidenceStaleDuo) |
| witness-stale-intent-apply | closureEvidenceStaleTenure | same as A18: design scale never completes, StaleDuo hits the TLC discrepancy; **unwired** — documented manual target (rust: <1 s) |
| witness-cross-tenure-walk | closureEvidenceStaleTenure | same; **unwired** — documented manual target (rust: ~2 s) |

The re-point and the unwirings are recorded inline in nix/quint.nix at
the affected entries, with the rust-simulator commands. Net wired-check
delta for the 0c set: 6 wired → 4 wired (hole-reap, recovery-clear,
re-pointed B2-strong, plus the C3 probe and the 11 BaseEx witnesses
which were already green at the 0c report), 3 manual targets. *[The 3
manual-target demotions are reversed by the post-0d triage record (the
last section): all three are re-wired through the rust-simulator
expect-violation constructor that stage adds.]*

**TLC-backend discrepancy (tool issue; stop-and-report).** *[Since
re-diagnosed by the post-0d triage record (the last section): not a
tool issue — a budget + progress-metric misreading; TLC finds every
affected violation at its true depth given the budget. The paragraph
below is kept as the 0d-stage observation.]* Found while
fixing the housekeeping wirings and reproducible at will: for the
stale-tenure alphabet (STALE_APPLY / CROSS_TENURE_WALKS) at
closureEvidenceStaleDuo scope, and for the consume-walk downgrade-revert
arm at closureEvidenceDuo scope, the rust simulator (`quint run`)
produces shallow counterexamples (depth 4–5, milliseconds-to-seconds)
that the TLC backend (`quint verify --backend=tlc`) does NOT find — TLC
explores past the violation depth (BFS depth 6–7, 200 K+ distinct
states) and keeps going, the swallowed-evaluation-exception signature;
one run hard-errored at 93 ms instead. The same predicates and branches
ARE TLC-findable at closureEvidenceBaseEx scope (the 0c
downgrade-respawn witness is TLC-green), so the issue is
scope-conditional, not a general encoding error. Affected items and
their dispositions: the three stale-tenure housekeeping checks
(unwired, above), and the F6 calibration falsification (rust-backend
evidence module, below). Everything wired in CI is TLC-validated
end-to-end. Owner follow-up options: file the quint/TLC issue with the
two-module reproducer; or extend mkQuintWitnessCheck with a
rust-simulator backend for expect-violation checks (the bounded
semantics is sufficient for witnesses); or re-encode the affected
corners until TLC finds them. Not resolved in this stage.

**Override modules (docs/spec/models/calibration/closure-*.qnt).** One
module per wired family representative, each importing
`closureEvidence` at the named scope's constants, replacing ONE action
with its pre-fix variant (the behavior the named historical fix
removed) and exposing it through `calibStep`; the violation latches
keep the production oracle, exactly the round-1 convention
(calibration/README.md). Scopes are the post-ladder 0c scopes: the
reduced exhaustive-base constants ("BaseEx"), the reduced failover
constants ("FailoverEx"), and the duo probe constants ("Duo").

| Module | Family / corpus rep | Pre-fix behavior (one guard) | Property | Scope |
|---|---|---|---|---|
| closure-f1-stale-produced | F1 soundness / CE-2 | merge has no stale-Produced verify | B9 | BaseEx |
| closure-f1-skip-store-recheck | F1 permissiveness / CE-1 | merge classification has no local store re-check | C4 | BaseEx |
| closure-f2-seed-only-walk | F2 / CE-9 | walk checks/ingests the seed only, not the closure | B2 | BaseEx |
| closure-f3-indet-failfast | F3 soundness / CE-61 | fail-fast arm fires without a confirmed-miss answer | B3 | FailoverEx |
| closure-f3-substitutable-demoted | F3 permissiveness / CE-60+CE-3 shape | probe routing demotes on any missing output | C2 | FailoverEx |
| closure-f4-vacuous-prune | F4 / CE-13 | empty wanted selection satisfies the prune availability ∀ | B4 | BaseEx |
| closure-f4-demand-drop | F4 / CE-66 | prune demand set = structural roots only | B10 | BaseEx |
| closure-f5-wanted-overwrite | F5 / CE-16 (re-routed, see below) | merge apply overwrites the stored wanted union | B5 | BaseEx |
| closure-f6-latch-outlives-chain | F6 / CE-20+CE-21 shape | downgrade revert keeps the never-forgive latch | A21 | Duo |
| closure-f7-clear-unbuilt-children | F7 (+F12) / CE-30+CE-28 | merge clear pass keys on "has children", not Vouched | A3 | Duo |
| closure-f8-dispatch-no-evidence | F8 / CE-33 | from-source admission consults no evidence | A1 | FailoverEx |
| closure-f9-poison-clear-no-stamp | F9 / CE-41 (re-routed, see below) | poison clear stamps no surviving parent | A5 | BaseEx |
| closure-f10-recovery-vouch-unscoped | F10 / CE-45 | recovery clear gate drops live co-ownership scoping | A3 | FailoverEx |
| closure-f13-unprobed-dispatch | F13 / CE-58 | from-source admission needs no probe verdict this pass | B8 | FailoverEx |
| closure-f14-recovery-keeps-substituting | F14+F10 / CE-48(i) shape | recovery trusts the persisted Substituting status | L1 | FailoverEx |

**Calibration verdict table.** Every override must FALSIFY its
predicted property under `calibStep` while the as-built baseline HOLDS
it at the same constants. Falsification runs: TLC backend (`quint
verify --backend=tlc`), 192/60/40/8 workers on the 0d builder; trace
lengths are TLC trace states (initial state included; where the
full-width run did not emit the trace the length is from an 8-worker
re-capture of the same falsification). The F6 row is the rust-simulator
exception (the TLC discrepancy above). Baselines, three layers: (i)
for latch-backed properties the as-built baseline holds **by
construction** — the latch site requires exactly the condition the
override removes (the 0c "by-construction + latch" convention); (ii)
the BaseEx-constants properties are conjuncts of the 0c
property-identification + closing runs (2.87 M / 16.4 M distinct
states, depth 21, no violation except C3); (iii) bounded as-built runs
made this stage at the non-BaseEx scopes: `closureEvidenceFailoverEx` ×
{A1, B3, B8, C2, L1, A3} HOLDS-BOUNDED to BFS depth 12 / 2.23 M
distinct states (9-min cap, 60 workers); `closureEvidenceDuo` × {A21,
A3} HOLDS-BOUNDED to BFS depth 7 / 280 K distinct states (9-min cap),
plus A21 at Duo under the rust simulator: no violation in 100 K
× 12-step samples.

| Family (direction) | Representative → property | Falsification (calibStep) | Baseline (as-built step) |
|---|---|---|---|
| F1 (soundness) | CE-2 → B9 | VIOLATED — 7-state TLC trace (756 K distinct explored): narrow merge, walk-ok, poison, wider resubmit leaves the parent Ready above the stale-Produced child | HOLDS (0c BaseEx record + by construction) |
| F1 (permissiveness) | CE-1 → C4 | VIOLATED — 5-state TLC trace (3.6 K distinct): a locally-present node is classified missing and queued from source | HOLDS (0c BaseEx record + by construction) |
| F2 | CE-9 → B2 | VIOLATED — 4-state TLC trace (5.5 K distinct): seed-only walk ok, consumption completes the parent with the child closure never present | HOLDS (0c BaseEx record + by construction) |
| F3 (soundness) | CE-61 → B3 | VIOLATED — 6-state TLC trace (62 K distinct): recovered marked root fail-fasted on a substitutable-only answer | HOLDS-BOUNDED (FailoverEx depth 12) + by construction |
| F3 (permissiveness) | CE-60/CE-3 → C2 | VIOLATED — 6-state TLC trace (4.5 K distinct): recovered Ready node with upstream-available outputs routed from source | HOLDS-BOUNDED (FailoverEx depth 12) + by construction |
| F4 | CE-13 → B4 | VIOLATED — 3-state TLC trace (4.4 K distinct): empty wanted selection, prune fires vacuously | HOLDS (0c BaseEx record + by construction) |
| F4 (demand set) | CE-66 → B10 | VIOLATED — 5-state TLC trace (6.6 K distinct): explicitly-requested non-root dropped by the roots-only prune | HOLDS (0c BaseEx record + by construction) |
| F5 | CE-16 → B5 | VIOLATED — TLC violation at search depth 12 (29.8 K distinct; full-width run did not emit the trace): a narrow resubmit's apply overwrites the wide stored union | HOLDS (0c BaseEx record + by construction) |
| F6 | CE-20/CE-21 → A21 | VIOLATED — 5-state RUST-SIMULATOR trace (167 ms; narrow merge → failed walk forgiving the unwanted output → wide co-build merge → downgrade-revert keeps nf on a Ready node); TLC does not find it (the backend discrepancy above) | HOLDS — rust 100 K × 12-step samples find no violation; TLC bounded to depth 7 |
| F7 (+F12) | CE-30/CE-28 → A3 | VIOLATED — 3-state TLC trace (18 K distinct): pruned-merge stamp, then a full re-merge clears the mark over an unbuilt child | HOLDS-BOUNDED (Duo depth 7) + by construction |
| F8 | CE-33 → A1 | VIOLATED — 6-state TLC trace (78.7 K distinct): recovered marked childless root delivered from source by the evidence-blind admission | HOLDS-BOUNDED (FailoverEx depth 12) + by construction |
| F9 | CE-41 → A5 | VIOLATED — 4-state TLC trace (4.4–13.7 K distinct): merge, poison, poison-clear with no surviving-parent stamp | HOLDS (0c BaseEx record + by construction) |
| F10 (vouch scoping) | CE-45 → A3 | VIOLATED — TLC violation at depth 17 (5.15 M distinct, ≈8.4 min full-width; trace not emitted): the unscoped recovery gate clears on a child co-owned only by a superseded submission. Too deep/slow for a wired check — evidence module only | HOLDS-BOUNDED (FailoverEx depth 12) + by construction (the as-built clear gate IS the strict criterion, so the A3 latch at the recovery site is structurally unreachable without the override) |
| F10 (stranded recovery) | CE-48(i) → L1 | VIOLATED — 5-state TLC trace (2 K distinct): merge, intent apply, failover, recovery re-enters the persisted Substituting row with no walk anywhere | HOLDS-BOUNDED (FailoverEx depth 12; L1 is a state-form property — the bounded run is the baseline) |
| F11 | CE-50 → A18 (regime comparison, no calibStep) | VIOLATED as-built in the stale-apply alphabet — rust simulator at closureEvidenceStaleDuo (<1 s) and the 0c design-scale simulation; the wired TLC probe could not be kept (the housekeeping record above) | HOLDS by construction in the no-stale-alphabet regimes (base / fault-persist / failover discard deposed intents at recovery), so the regime split itself is the baseline |
| F13 | CE-58 → B8 | VIOLATED — 6-state TLC trace (6.8 K distinct): recovered, never-re-probed Ready node opened from source | HOLDS-BOUNDED (FailoverEx depth 12) + by construction |
| F14 | CE-48(i)/CE-52 → L1 | same run as the F10 stranded-recovery row above (one module carries both rows) | HOLDS-BOUNDED (FailoverEx depth 12) |

**Permissiveness directions (the §4 both-directions clause).** F1
calibrates in both directions (B9 + C4 above). F3 calibrates in both
directions (B3 + C2 above). The C-group's C4 and C2 are thereby
falsifiable non-vacuously; C5's override is deferred (below). For F6
the safety direction (A21) calibrates above; the permissiveness
consequence the corpus describes for F6 (the inherited veto demotes
substitutable work, CE-20's harm) is **not separately producible at
this model's abstraction**: the model's wrongful-demotion latch (C2,
ENC-0b-9) requires every missing wanted output to be available
upstream while the demotion site additionally requires a
confirmed-miss answer, and with the negative-cache polarity excluded
by ENC-0b-8 those two cannot hold together except through the F3
override's weakened verdict mapping. The F6 permissiveness direction
is therefore carried by (a) the A21 falsification (the stale latch
demonstrably outlives its chain — the precondition of the inherited
veto) plus (b) the C2 falsification through the F3 representative,
with this structural argument recorded in place of a second F6-specific
override. Re-dispositioning this as a full ENC row would need the
negative-cache polarity added to the model (an ENC-0b-8 revisit), which
is left to the owner.

**Re-routes and dispositions inside families (no silent drops).**

- F5: the design's named representative CE-18 (rollback replay, a
  structural override) is inert at this model's granularity — the
  all-or-nothing merge intent makes the as-built `mergeRollback` a
  no-op, so a replay-shaped override would falsify B6 only by
  re-introducing behavior the abstraction already excludes. The family
  falsification is re-routed to CE-16 → B5 (the overwrite-vs-union
  guard, the same family's wanted-lifecycle invariant); CE-18/CE-19/
  CE-68/CE-75 stay listed as trailing structural evidence targets for
  the Phase 2 acceptance table.
- F9: re-routed from CE-40/CE-43 to CE-41 (the poison-clear setter) —
  the minimal single-action delta at the post-ladder scope; CE-40
  (recovery edge-drop stamp) and CE-43 (fail-fast keeps the breadcrumb)
  need recovery/consume-walk structural copies whose falsifications sit
  deeper, and stay trailing evidence targets for Phase 2.
- F14: re-routed from CE-52 to the CE-48(i) stranded-Substituting
  shape. CE-52's exact lane (Poisoned-at-limit re-probe rejected by the
  transition table) is absorbed by the model's merge-time resetNodes
  lane — the re-probe lane cannot strand a node here, so there is no
  state invariant for it to falsify; the family's stuck-state invariant
  (L1) is falsified through the recovery-trusts-Substituting variant
  instead.
- F10: the family falsifies through two representatives (CE-45 and
  CE-48(i)); the CE-45 falsification is evidence-module-only (≈8 min,
  past the wired-check budget).
- F11: no calibStep override — the leader-gate guard is a regime
  constant in this model (STALE_APPLY / CROSS_TENURE_WALKS), not an
  action guard, so the family is calibrated by regime comparison (the
  A18 falsification in the stale-apply alphabet vs the by-construction
  hold without it). With the TLC discrepancy, the falsification side
  of that comparison is rust-simulator evidence.
- F12: covered by the F7 representative (CE-69 → ENC-A CE-30 in the
  design crosswalk; the clear-before-reconciliation weakening is the
  F12 ordering shape).
- C5 / CE-7 (terminal build's stored union drives resets): **NOT
  falsified in 0d — deferred to a documented manual target.** The
  trigger needs a Produced node with a divergent stored-vs-live wanted
  view (stored {1,2}, live {1}, output 2 absent) plus a third
  submission slot to re-run the verify after the wide build went
  terminal; at the duo scope the third slot does not exist (a cancelled
  build's slot cannot resubmit), so the hunt plan is the design's
  non-regime-constants pattern: BUILDS = {b1,b2,b3},
  SUBMISSION_BUDGET = 3, override = the stale-Produced verify keyed on
  `pgWanted` instead of the live effective wanted set, predicted
  falsification C5 (`unionDrivenDecision`), baseline = the same
  constants without `--step`. Until that override exists C5's
  falsifiability remains owned by this deferred target (the 0c
  per-property note stands); owner sign-off required to accept the
  deferral, per the §8 0d gate language.
- CE-25 (stamp-before-commit, the F7 structural rep) stays a trailing
  Phase-2 structural evidence target for the same reason as CE-18 (the
  all-or-nothing intent encoding).
- CE-31 (recovery clear gate keyed on the in-memory child view → A19)
  is covered for the family by the CE-45/CE-48(i) falsifications; its
  own override (an A19-completeness-direction recovery copy) is a
  trailing Phase-2 evidence target.

**Wired permanent calibration checks (nix/quint.nix,
`quint-closure-calib-*`).** Six of the falsifications run in the
minutes class end-to-end under the TLC backend and are wired as
permanent expect-violation checks, one per major property group and
scope — all six built green in this stage's harness run:
quint-closure-calib-f1-stale-produced (B9, BaseEx),
quint-closure-calib-f2-seed-only-walk (B2, BaseEx),
quint-closure-calib-f4-demand-drop (B10, BaseEx),
quint-closure-calib-f7-clear-unbuilt (A3, Duo),
quint-closure-calib-f8-dispatch-no-evidence (A1, FailoverEx),
quint-closure-calib-f9-poison-clear-no-stamp (A5, BaseEx). The
remaining override modules are committed evidence modules, re-runnable
with the command in each file's header (calibration/README.md
pattern); the C5 target above is the one override still to be written.
No tracey markers on calibration checks (house convention).

**The FailoverEx as-built conjunction violation (stop-and-report; the
first measurement of a fault regime).** *[Since triaged by the post-0d
triage record (the last section): the violated conjunct is L3, a REAL
as-built finding — trace, code walk and Phase-1-candidate disposition
there. The paragraph below is kept as the 0d-stage observation.]* As
part of establishing
baselines, this stage ran the held-back manual exhaustive target
`closureEvidenceFailoverEx` × `asBuiltHoldInvariants` (the as-built
step, no override) for the first time: **VIOLATED** at BFS depth 16
after 2.63 M distinct states (≈4.4 min at full builder width); the
full-width run did not emit the trace (the recurring TLC
trace-emission issue at high worker counts). The violated conjunct is
NOT identified yet; what is known: the six calibration-relevant
properties (A1, B3, B8, C2, L1, A3) hold bounded to depth 12 at this
scope in a separate run, and every latch-backed property's as-built
guard argument still applies, so the suspect set is the state-form /
counter properties or a model-encoding issue the failover alphabet
exposes (the CE-FIX-1/2 precedent — both 0c encoding fixes were found
exactly this way). The 0c record's per-property table is NOT
invalidated (it covers the reduced base scope only; the failover scope
was explicitly "not yet measured"). Triage owner: the orchestrator —
either a third encoding fix (model artifact) or a real as-built
finding at the failover scope; the triage needs a trace, which needs
either a low-worker-count re-run (slower but emits traces) or
per-property bisection of the conjunction. Until then the FailoverEx
exhaustive target stays a held-back manual target, now with a recorded
partial result instead of "not yet measured".

**C3 adjudication update (carried in from the concurrent
investigation).** The C3 finding (wrongful terminal failure, the 0c
stop-and-report item 1) was adversarially adjudicated during this
stage: **CONFIRMED** via a two-live-builds variant of the trace; the
original single-build 11-state trace is refuted (its step 7+9
combination is unreachable as-built because the sole-interest cancel
chain would have cancelled the stale walk's node), but the two-build
variant reproduces the same wrongful fail-fast with both builds live.
C3 is therefore a confirmed Phase-1 red-first fix candidate, no longer
"pending adjudication". The model corrections that follow from the
refuted single-build leg are queued AFTER this stage's commits (not
applied here, to keep the calibration evidence and the correction
disjoint); the wired C3 expect-violation probe
(quint-closure-evidence-probe-wrongful-terminal-failure) stays green
against the current model and gets re-validated when those corrections
land.

**Acceptance verdict (the design §4 calibration acceptance protocol /
§8 0d gate), per corpus family.**

| Family | Verdict | Evidence / disposition |
|---|---|---|
| F1 | MET (both directions) | CE-2 → B9 falsified (wired); CE-1 → C4 falsified (evidence module) |
| F2 | MET | CE-9 → B2 falsified (wired) |
| F3 | MET (both directions) | CE-61 → B3 and CE-60/CE-3 → C2 falsified (evidence modules) |
| F4 | MET | CE-13 → B4 falsified; CE-66 → B10 falsified (wired) |
| F5 | MET (re-routed rep) | CE-16 → B5 falsified; CE-18 structural override deferred to Phase 2 |
| F6 | MET for the safety direction (rust-backend evidence); permissiveness direction by recorded argument | CE-20/CE-21 → A21 falsified (rust simulator; TLC discrepancy recorded); C2 carried by the F3 representative + the structural argument above (owner sign-off requested) |
| F7 | MET | CE-30/CE-28 → A3 falsified (wired); CE-25 structural deferred to Phase 2 |
| F8 | MET | CE-33 → A1 falsified (wired) |
| F9 | MET (re-routed rep) | CE-41 → A5 falsified (wired); CE-40/CE-43 deferred to Phase 2 |
| F10 | MET | CE-45 → A3 falsified (evidence module); CE-48(i) → L1 falsified |
| F11 | MET by regime comparison | A18 falsified in the stale-apply alphabet (rust simulator — the TLC discrepancy keeps it un-wired); no-stale-alphabet regimes are the baseline |
| F12 | MET via ENC-A | covered by the F7/CE-30 representative per the design crosswalk |
| F13 | MET | CE-58 → B8 falsified |
| F14 | MET (re-routed shape) | CE-48(i) → L1 falsified; CE-52 absorption argument recorded |
| F15 | NOT-ENC (pre-registered, unchanged) | store-side; rio-store unit/VM tests |
| F16 | NOT-ENC (pre-registered, unchanged) | motivates the detached-walk asynchrony only |
| F17 | NOT-ENC (pre-registered, unchanged) | build accounting; existing unit tests |
| C-group | C2/C4 MET (above); C1 covered by the 0c hold + the deferred C1-strict probe; C3 is the confirmed as-built finding (cannot serve as a calibration baseline; Phase-1 fix candidate); **C5 NOT MET — deferred manual target (owner sign-off requested)** | see the dispositions above |
| §4 UNCLEAR rump (6 commits) | unchanged | no re-disposition was needed |

The NOT-ENC list is unchanged from the design (no row was silently
re-dispositioned; the two re-routes and the two structural deferrals
above are within-family and recorded).

**Go/no-go.** GO for Phase 1, with the following named owner-sign-off
items carried out of the gate rather than papered over. *[This list is
superseded by the "Updated owner sign-off items" list at the end of the
post-0d triage record, which resolves items 4–5 below.]*

1. The C5 / CE-7 deferral — the only §4 ENC row with no falsification
   in hand; manual-target plan recorded above.
2. The F6 permissiveness-direction argument standing in for a second
   F6 override, and the F6 safety falsification resting on the
   rust-simulator backend (the TLC discrepancy).
3. The CE-45 (F10) falsification being evidence-module-only (the
   family is met; that row is slower than the wired budget).
4. The TLC-backend discrepancy itself — three 0c stale-tenure checks
   unwired to manual targets, and one calibration row on rust
   evidence; the fencing-evidence chain (A18/D14, §9.1) now rests on
   simulation rather than a wired check.
5. The FailoverEx as-built conjunction violation — unidentified
   conjunct, needs orchestrator triage (encoding fix vs finding).
6. The 0c carry-overs unchanged by this stage: C3 (now CONFIRMED, a
   Phase-1 red-first fix candidate), the held-back exhaustive
   conjunctions, and the deferred A17 / L2 / C1-strict probes.

Items 1–3 are 0d-scope calls the owner can accept or reverse without
re-opening the model; items 4–5 are new stop-and-report findings of
this stage; item 6 is the existing 0c set. Every §4 encodable family
falsifies through a representative with a holding baseline at the same
constants — under the TLC backend for 13 of 15 representatives, under
the rust simulator for F6 and the F11 regime comparison — which
satisfies the §8 0d gate with the listed caveats made explicit rather
than absorbed.

**0d → Phase 1 handoff.** The calibration evidence base for the
red-first fixes: the C3 expect-violation probe and its trace (now a
confirmed defect), the re-pointed B2-strong probe (the GC-after-vouch
disposition), the A18 rust-simulator record (the fencing disposition),
the six wired calib checks (regression guards for the guard classes
Phase-1 edits are most likely to touch), and the deferred-target list
above (C5, CE-18/CE-25 structural, CE-40/CE-43, CE-31, the A17/L2
hunts). Any Phase-1 change to merge.rs/dispatch.rs/recovery.rs
evidence handling should re-run the corresponding family's evidence
module even where it is not wired; any change to the model file
re-runs all six wired calib checks plus the witness/probe set
automatically (the .qnt fileset is the eval input).

### Post-0d triage — C3-adjudication corrections, FailoverEx conjunction, TLC-backend discrepancy (this stage)

This stage executes the two stop-and-report items the 0d record raised
for orchestrator triage and applies the three model corrections the C3
adjudication queued. Verification subject unchanged (`formal-sprint` @
`cccb4d778`); all measurements in this section: 192-core builder, TLC
via the warm shared apalache-server prelude, worker counts as noted.

**1. The C3-adjudication model corrections (applied).**

The adjudication confirmed the C3 mechanism (a stale ok=false walk
verdict consumed at the topdown fail-fast with no presence re-check)
but refuted the 0c single-build trace: its step 7+9 combination is
unreachable as-built because the sole-interest cancel chain cancels the
stale walk's node and the directed resubmit re-probes it. Three model
corrections were queued (deliberately held until 0d finished) and are
now applied:

- CE-CORR-1 (`poisonArrival` cancel pass): when a poison verdict fails
  builds, the model now runs the `cancel_build_derivations` pass over
  the failing builds' exclusively-owned members — un-hit, non-terminal
  members whose every interested active build is itself failing go
  Cancelled, exactly as `handle_derivation_failure`'s keep_going=false
  arm does (build.rs:cancel_build_derivations; mirrors the model's
  existing cancelBuild / pFailFast cancel passes). A sole-interest
  Substituting member therefore leaves Substituting when its build
  fails, and its in-flight walk is orphaned into the not-Substituting
  drop guard instead of staying consumable.
- CE-CORR-2 (walk staleness re-grounded on walk duration): the walk
  actions are re-documented and re-grounded as check-time vs send-time —
  `walkFinishes` is the walk task's last presence check (the verdict and
  the ok-ingest are fixed mid-flight), `consumeWalk` is the send plus
  the actor's processing as ONE step. The window between them is the
  tail of the walk's own duration; a SubstituteComplete can be stale
  only because the world changed DURING the walk. The actor's FIFO
  mailbox excludes the reordering the refuted trace relied on (a
  resubmit issued in response to the build failure being processed
  before the walk completion that was already enqueued). No transition-
  relation change was needed for this correction — the two-action
  structure already encodes exactly this window; what changes is the
  stated grounding (the 0b/0c reading treated the window as mailbox
  delay) and, with CE-CORR-1/3, which traces survive it.
- CE-CORR-3 (Cancelled/Failed resubmit-reset re-probe): the merge's
  reset of kept terminal/Failed nodes now also discards stale walk
  records (`walks: []`), completing the fresh-state semantics of
  production's remove+reinsert (dag/mod.rs:389-443
  `is_retriable_on_resubmit`, merge.rs:486-507 `existing_reprobe`): a
  reset node re-evaluates presence through THIS merge's classification
  (cache-hit → Produced from store), never through a verdict formed
  before it went terminal. Propagated to the five calibration modules
  that carry frozen `mergeCommit` copies (closure-f1-stale-produced,
  closure-f1-skip-store-recheck, closure-f4-demand-drop,
  closure-f4-vacuous-prune, closure-f7-clear-unbuilt-children) so each
  still differs from as-built by exactly its one intended guard.

Validation after each correction: `quint typecheck` green (core model +
all 15 calibration modules). Behavioral validation:

- C3 at `closureEvidenceBaseEx` (the single-build scope): **no longer
  violated.** Rust simulator: no violation in 30 K samples × 20 steps.
  TLC: no violation through 10.34 M distinct states (43.95 M generated,
  reported diameter 14, 40 workers, stopped at the 30-min cap) — 5.8×
  the distinct-state count at which the 0c run found the old trace
  (1.77 M). The refuted single-build trace is gone from the model, as
  the adjudication requires.
- C3 at two-build scopes: **violated via the faithful trace.** Rust
  simulator at `closureEvidenceDuo`: violation in ≈2.3 s (≈200 K
  traces); the captured 12-state trace is exactly the adjudication's
  surviving variant — b1's narrow pruned merge stamps d1 and cache-hits
  it (output 1 present); b2's wide duplicate submission demotes the
  stale-Produced d1 (wide wanted set not all present) and spawns the
  wide walk; the walk genuinely fails on the wide-only output 2 at
  check time; b2 is cancelled, d2 reaped, d1 holed; the stale verdict's
  delivery fail-fasts b1, whose own (narrow) wanted output is present.
  Single tenure, no GC, no upstream change anywhere in the trace.
- `asBuiltHoldInvariants` still holds under simulation at BaseEx, Duo
  and FailoverEx after the corrections (20–40 K samples × 30 steps
  each).

Wiring consequence: the wired expect-violation check
`quint-closure-evidence-probe-wrongful-terminal-failure` is re-pointed
from `closureEvidenceBaseEx` under TLC (where it would now fail — no
violation exists there) to `closureEvidenceDuo` under the NEW
rust-simulator expect-violation constructor (`mkQuintSimWitnessCheck`,
added to nix/quint.nix by this stage — see item 3's consequences). The
TLC route was measured and rejected on budget at both two-build scopes:

- `closureEvidenceDuo` × C3 under TLC: stopped unconverged at 540 K
  distinct states / 15 min (60 workers) with the violation still ≥2
  BFS levels away — the same per-level cost story as item 3.
- A dedicated reduced probe scope `closureEvidenceC3Duo` was added (the
  two-build universe restricted to exactly the confirmed trace's
  alphabet: duplicate submissions, cancel + reap, per-output store
  letters, walks; poison / attempt / fault / GC off; one extra intent
  slot so the 7-action trace needs no pgApply step) and measured: TLC
  stopped unconverged at 573 K distinct states / 4.0 M generated /
  ≈15 min (60 workers) without reaching the depth-8 violation. The
  module is kept as the documented TLC manual target for C3 (it is the
  cheapest known TLC scope for this violation), but it is not
  gate-compatible either.
- The rust simulator at `closureEvidenceDuo` finds the violation
  reliably (7/7 independent runs at 2 M samples; the violation sits at
  a per-trace hit rate of ≈1/150 K, so the wired check's 5 M-sample
  budget puts the miss probability at ≈e^-33). Per-run wall clock:
  ≈20–30 s.

**2. FailoverEx as-built conjunction violation: triaged — the violated
conjunct is L3, a REAL as-built finding (the second Phase-1
candidate).**

Re-run after the Part-1 corrections (per the triage plan: if the
violation had disappeared, the cause would have been the poisonArrival
encoding gap): **still violated** — `closureEvidenceFailoverEx` ×
`asBuiltHoldInvariants`, violated at 2.06 M distinct states (8.5 M
generated, reported diameter 12) in 8 min 15 s at 60 workers, and this
run emitted the trace (the 0d full-width run had not). The poisonArrival
hypothesis is refuted; the violation is real and survives the
corrections.

Violated conjunct (identified from the trace — no per-property
bisection was needed): **L3 `liveBuildTerminalOrProgressArmed`**. Every
violation latch in the final state is false, so the violated conjunct
is one of the state-form properties, and the final state is exactly
L3's negation: a live build with non-empty interest whose only member
has no progress arm.

The 10-state trace (archived in the triage run transcript;
`/tmp/rio-dev/ce-triage/runC-failoverex/run.log` during the session,
reproducible with the manual-target command below):

1. Environment: nothing present or upstream anywhere.
2. `mergeCommit(b1)` — full merge of {d1→d2}; d1 → Substituting (walk
   spawned), d2 → Ready (confirmed-missing classification).
3. `pgApplyAny` — the merge transaction lands (rows, edges (d1,d2),
   links {d1,d2}).
4. `poisonArrival(d2)` — d2 → Poisoned (persisted); b1 fails; the
   cascade skips Substituting d1; the (new) cancel pass cancels
   sole-interest d1 → Cancelled.
5. `mergeCommit(b1)` — the directed resubmit: prune keeps {d1} (demand
   available via the substitutable answer), d2 dropped; d1 (Cancelled →
   reset, stale walk discarded) re-classified pending-substitute →
   Substituting with a FRESH walk; b1 Active again; the resubmit's
   merge intent replaces b1's links with {d1}.
6. `pgApplyAny` — the resubmit's transaction lands (b1's durable links
   are now {d1}).
7. `leaderLost`.
8. (environment step)
9. `recoverAsLeader` — recovery loads d1 (pgSt Queued — Substituting is
   in-memory-only, D17) and d2 (pgSt Poisoned, within TTL); b1 Active
   with interest {d1} (links ∩ loaded); d1 restored MQueued (its
   durable child d2 is un-produced, so the dependency walk gates it);
   d1's in-flight walk died with the old tenure (CROSS_TENURE_WALKS =
   false), so NOTHING is armed on d1 — but L3 still holds here: d1 is
   Queued above an un-produced child, which counts as armed (the
   child's own settlement will cascade).
10. `poisonClear(d2)` — admin ClearPoison / TTL sweep: d2 removed from
    the DAG, the (d1,d2) edge scrubbed, d1's closure-hole stamped. d1
    is now **Queued, childless, walk-less, attempt-less** — and b1 is
    Active with interest {d1}. No progress arm exists: L3 violated.

Code walk (formal-sprint @ cccb4d778), step by step: the resubmit's
re-classification spawns a detached fetch whose task dies with the
process at failover (no walk identity, no persistence — dispatch.rs
spawn_substitute_fetches); recovery's `seed_ready_queue` re-derives
dispatchability from the recovered child set and never re-arms
substitution walks (the D17 in-memory-only Substituting reset —
recovery.rs); `handle_clear_poison` (completion.rs:2520–2594) removes
the node via `dag.remove_node` and stamps the surviving parents'
closure_hole, and `remove_node`'s own contract (dag/mod.rs:1271–1273,
1359–1390) is explicit that the removal "must not trigger parent
re-evaluation" — the design intent being that the next merge re-inserts
the child fresh; nothing else ever re-evaluates the parent: the
promotion passes run only at merge time and at child-completion time,
the dispatch probe partition only touches Ready nodes, and the parent
is Queued. The recovered build b1 therefore hangs Active forever unless
an external new submission happens to re-merge d1.

Classification: **REAL as-built finding** (not a model artifact): every
step is as-built behavior, and the stranding is the composition of
three individually-documented behaviors (walks die with the tenure;
recovery's dependency gating; ClearPoison's no-re-evaluation removal).
It is the failover-scope sibling of the D16 settlement gap: where D16
is "marked+Broken+tried+present and the dispatch probe refuses to act",
this is "Queued+childless+unarmed and nothing ever looks at it". Per
the design §8 triage rule (a counterexample violating a property in a
fault regime the §2b matrix marks in-scope is a defect candidate) this
is a **Phase-1 candidate** — candidate fix shapes: ClearPoison /
poison-TTL removal runs the same promote-newly-ready pass the
completion path runs for surviving parents (the narrow fix), or
recovery re-probes Queued nodes whose durable children are all
terminal (the wider fix, also closing the recovery half of D16) — with
accepted-with-rationale (operator runbook: directed resubmit after
ClearPoison) as the alternative disposition; owner adjudication
required. NOT fixed in this phase.

Dispositions:

- The model gains `asBuiltHoldInvariantsFailoverEx` (=
  `asBuiltHoldInvariants` minus L3) so the FailoverEx manual exhaustive
  target remains meaningful while the finding is open; the
  single-tenure scopes keep L3 (it holds there — the 0c BaseEx record).
- The L3 falsification at FailoverEx is a **documented manual target**,
  not a wired probe (the violation sits at ≈2 M distinct states /
  ≈8 min at 60 workers — past the wired-check budget; the C3 precedent
  of wiring the falsification applies only when the violation fits the
  budget):

  ```
  quint verify --backend=tlc --main=closureEvidenceFailoverEx \
    --invariant=liveBuildTerminalOrProgressArmed \
    docs/spec/models/closureEvidence.qnt
  ```

  The rust simulator does NOT reach this violation in bounded samples
  (300 K × 14 steps found nothing — the trace needs two specific merge
  payloads, a poison, a failover and a poison-clear in order), so the
  TLC run above and the archived trace are the falsification evidence.
- The 0c per-property table's L3 row is updated (violated as-built at
  the failover scope); the 0c "no violation found at the reduced base
  scope" reading for L3 stands — the violation needs the failover
  alphabet.
- Whether the 0d depth-16 violation was this same L3 shape cannot be
  confirmed (that run never emitted a trace), but it is consistent: the
  violation exists in the pre-correction model along the same path
  (correction CE-CORR-1 appears in the trace at step 4 but is not
  load-bearing — without it d1 stays Substituting through the resubmit
  and the recovery strands it the same way).

**3. TLC-backend discrepancy: resolved — not a tool bug; a budget +
progress-metric misreading. No exhaustive verdict is downgraded.**

Reproduction (same module, same property, both backends): rust
simulator finds the A18 `leaderClassEvidenceWrites` violation at
`closureEvidenceStaleDuo` in 0.7 s; TLC runs capped at 15–40 min do
not (16-worker and 1-worker runs both timed out without flagging it,
matching the 0d observation).

Diagnosis by link bisection — each prefix of the 4-action violation
path (mergeCommit → leaderLost → recoverAsLeader → stale pgApplyAny)
pinned by a throwaway expect-violation witness, all run with the FULL
step relation at StaleDuo:

| Prefix pinned | TLC outcome |
|---|---|
| any intent exists (depth 1, mergeCommit fires) | violated in ≈2 s |
| leader down (depth 2, leaderLost fires) | violated in seconds |
| second tenure exists (depth 3, recoverAsLeader fires) | violated at the true depth-3 trace in ≈35 s (1.9 K distinct states) |
| stale intent survives recovery (depth 4) | violated at the true depth-4 trace in ≈320 s (16 K distinct states) |
| the A18 violation itself (depth 5) | found in 49 s / 7 K distinct states under a restricted step (merge+lease+pgApply only), proving the translation of every involved action is sound; **and found with the FULL step relation** in 14.8 min / 776 K distinct states (5.1 M generated) at 100 workers — the closing confirmation that TLC finds exactly the violation the 0d record said it "explores past" |

So TLC's BFS is CORRECT — every link is taken at its true depth, and
the full violation is found whenever the budget covers its level. The
discrepancy's root causes are:

- (ii) of the candidate list — a depth/throughput accounting issue:
  the per-level distinct-state growth at the stale-tenure scope is
  ≈10× per BFS level (1.9 K → 16 K → ≈160 K+ distinct states for
  levels 3/4/5) with a high per-state successor cost (dual walks,
  per-output store letters, two builds, the stale-apply alphabet), so
  the depth-5 violation needs roughly an hour of TLC at moderate worker
  counts — past every 0c/0d/triage cap.
- TLC's `Progress(N)` diameter metric overstates completed BFS levels:
  it reports the depth of recently dequeued states, and with the
  multi-worker disk queue, deeper states are dequeued before shallower
  levels complete. The 0d reading "TLC explores past the violation
  depth (BFS depth 6–7) without flagging" was this metric, not a
  completed level. (Independent corroboration: the 0c BaseEx C3
  violation is an 11-state trace that TLC reported at diameter 16.)
- The 0d record's "one run hard-errored at 93 ms" is consistent with
  the documented cold-server conversion failure (the empty-details gRPC
  INTERNAL error quint surfaces as a fast hard error), not with a TLC
  evaluation error; no evaluation error was reproduced in any triage
  run, including single-worker runs.

Consequences and dispositions:

- **No verdict downgrade.** Nothing in the map claimed exhaustive
  verification through the affected scopes — the affected items were
  already manual targets / rust-simulator evidence. The "exhaustive
  verdicts stand" branch of the triage deliverable applies.
- The F6 / F11 calibration rows keep their rust-simulator evidence,
  but their classification changes from "tool issue (candidate
  upstream filing)" to "budget" — there is **no upstream issue to
  file**. nix/quint.nix's inline records are updated accordingly.
- The A18 / D14 fencing evidence (§9.1) now rests on: the rust
  simulator (0.7 s reproduction, now WIRED — below), TLC's
  restricted-step confirmation (depth-5 violation, 49 s), TLC's
  full-step confirmation (14.8 min, 776 K distinct states), and the
  link-bisection table above. This is strictly stronger than the 0d
  posture (unwired rust evidence only).
- **Harness extension (done in this stage, not deferred):**
  nix/quint.nix gains `mkQuintSimWitnessCheck`, a rust-simulator
  expect-violation constructor for violations that are real and
  confirmed but whose BFS level sits past a gate-compatible TLC budget.
  Semantics note in the constructor's header: an expect-violation
  check's claim is existential, so a bounded random search that finds
  the violation is exactly as strong as TLC finding it; only the
  "no violation found" outcome is weaker, and that outcome is a check
  FAILURE either way. Flake discipline: maxSamples is sized from the
  measured per-trace hit rate so the miss probability is ≤~1e-11.
- With that constructor, the three 0d-unwired stale-tenure checks are
  **restored to the wired CI surface**: the A18 probe
  (quint-closure-evidence-probe-stale-evidence-write), the
  stale-intent-apply witness and the cross-tenure-walk witness, all at
  closureEvidenceStaleDuo (hit rates ≈1/2.5 K, ≈1/2 K and ≈1/100 K per
  trace; budgets 500 K / 500 K / 2 M samples). The 0d net-wired-check
  delta (6 wired → 4 wired, 3 manual) is thereby reversed to 4 wired →
  7 wired, with the C3 probe also re-pointed onto this constructor.
- The F6 (A21-at-Duo) calibration row's diagnosis is extrapolated, not
  re-run: the same scope-cost structure applies (the Duo C3 budget
  record under item 1 independently demonstrates the Duo cost story).
  Re-running F6 under a long-budget TLC, and/or wiring it through the
  sim constructor, is a recorded optional follow-up, not a gate item.

**Wired-check delta of this stage.**

| Check | Before | After |
|---|---|---|
| quint-closure-evidence-probe-wrongful-terminal-failure | closureEvidenceBaseEx, TLC backend (violation = the refuted single-build trace) | closureEvidenceDuo, rust-simulator backend via mkQuintSimWitnessCheck (violation = the confirmed two-build trace); TLC manual target at closureEvidenceC3Duo |
| quint-closure-evidence-witness-downgrade-respawn | closureEvidenceBaseEx, TLC (its reachability rested on the refuted single-build behavior — unreachable post-corrections: rust 500 K × 20-step samples find nothing at BaseEx) | closureEvidenceC3Duo, rust-simulator backend (the faithful two-build trace: narrow pruned merge + wide pruned co-merge + ok-walk with a forgiven output + consume → mustSub respawn; four actions deep, rust ≈3.4 s; the TLC form costs ≈10 min at 60 workers — multi-worker queue-ordering delay — and is the recorded manual target) |
| quint-closure-evidence-probe-stale-evidence-write (A18) | unwired (0d housekeeping demotion to manual target) | WIRED, closureEvidenceStaleDuo, rust-simulator backend |
| quint-closure-evidence-witness-stale-intent-apply | unwired (0d) | WIRED, closureEvidenceStaleDuo, rust-simulator backend |
| quint-closure-evidence-witness-cross-tenure-walk | unwired (0d) | WIRED, closureEvidenceStaleDuo, rust-simulator backend |
| (harness) | mkQuintCheck / mkQuintWitnessCheck / mkQuintRunCheck | + mkQuintSimWitnessCheck (rust-simulator expect-violation constructor) |
| (model) | — | new probe module `closureEvidenceC3Duo` (the C3 TLC manual-target scope + the wired downgrade-respawn scope); new conjunction `asBuiltHoldInvariantsFailoverEx` |
| manual targets | FailoverEx × asBuiltHoldInvariants ("not yet measured" → 0d: violated, conjunct unknown) | FailoverEx × asBuiltHoldInvariantsFailoverEx (the open-finding-free target) + the L3 falsification target (the finding's pin) + C3 × closureEvidenceC3Duo (TLC form of the wired sim pin) |

Net wired closure-evidence checks: 22 (0d) → 25. All other wired
closure-evidence checks (the remaining 10 BaseEx witnesses, the
duo/failover witnesses, the B2-strong probe, the six calib checks) are
unchanged in wiring; they re-run automatically against the corrected
model because the .qnt fileset is their eval input. Re-validation of
the unchanged checks against the corrected model (this stage's harness
runs): every one that completed its build is green — the ten remaining
BaseEx witnesses, hole-admin-clear and hole-ttl-sweep included (the two
poison-path witnesses whose traces the cancel-pass correction
touches), the B2-strong probe at AdversarialStoreEx, hole-recovery at
FailoverEx, and five of the six calib checks (f2/f4/f7/f8/f9; f9 is
the poisonClear copy most exposed to the correction). The stragglers
still building at the report cut (hole-reap, recovery-clear, calib-f1
— the three slowest TLC checks of the set) have unchanged reachability
arguments; their completion is part of the next full gate run.

**Updated owner sign-off items for the Phase-0 checkpoint** (supersedes
the 0d go/no-go list; items renumbered):

1. The C5 / CE-7 deferral — unchanged from 0d (the only §4 ENC row with
   no falsification in hand; manual-target plan in the 0d record).
2. The F6 permissiveness-direction argument and the F6 safety
   falsification resting on the rust simulator — unchanged from 0d,
   with the discrepancy re-diagnosis (budget, not tool bug) attached.
3. The CE-45 (F10) falsification being evidence-module-only — unchanged
   from 0d.
4. ~~The TLC-backend discrepancy~~ — RESOLVED by this triage (budget +
   metric misreading; no tool issue, no downgrades). The residual
   sign-off question is only whether to fund the mkQuintWitnessCheck
   rust-backend extension so the affected manual targets can become
   wired checks.
5. The FailoverEx conjunction violation — TRIAGED to the L3 as-built
   finding. New owner adjudication: Phase-1 fix (ClearPoison promotes
   surviving parents / recovery re-probe) vs accepted-with-rationale
   (operator runbook). This is the second confirmed as-built finding of
   the campaign, alongside C3.
6. C3 — CONFIRMED (adjudication + this stage's faithful-trace
   reproduction); Phase-1 red-first fix candidate; fix authorization
   requested. The model corrections and probe re-point of this stage do
   not change the production-code question.
7. The 0c carry-overs unchanged: the held-back exhaustive conjunctions
   (now with the FailoverEx target split per item 5) and the deferred
   A17 / L2 / C1-strict probes.

### Phase 1 Wave 2 — recovery-condemnation correction, L3 re-hunt, decision gate (this stage)

**Headline: the post-0d triage's L3 finding is REFUTED as a production defect.** The recorded
FailoverEx violation of `liveBuildTerminalOrProgressArmed` was a model artifact of a
recovery-encoding faithfulness gap (the plan's adversarial-review finding RT-2), not an
as-built defect. The model is corrected, the full wired battery stays green, and the re-hunt
under the corrected encoding finds no L3 violation at any hunted scope under either backend —
while the same setup re-finds the violation on the pre-correction model in eight minutes. No
fix is implemented (owner decision 1's L3 half is moot as premised); the genuine residual the
review surfaced — production's unscoped recovery condemnation, a C3-class behavior at a fourth
decision point — is characterized below and routed to the owner. The plan's conditional Wave-2
tasks (T-2.2..T-2.4) did not execute.

**1. The faithfulness gap and the correction (closureEvidence.qnt, recoverAsLeader pass 2).**

Production has TWO recovery condemnation mechanisms; the 0b model encoded only the first:

| Mechanism | Production | Model pre-correction | Model post-correction |
|---|---|---|---|
| R2 cascade pre-pass | `load_parents_with_failed_deps` -> `cascade_failed` short-circuit in `seed_ready_queue` (recovery.rs) — co-ownership-scoped (the bug_009 evidence rule; the `sched.recovery.failed-dep-cascade+2` MUST) | `pCondemnCriterion` (faithful) | unchanged |
| In-DAG recompute | `compute_initial_states` (dag/mod.rs) -> `any_dep_terminally_failed` over the LOADED child set — NO co-ownership scoping; within-TTL poisoned children are loaded with their edges, so a recovered parent above another build's still-poisoned child IS condemned | **missing** | `kidsLoaded.exists(c => isUnprodTerminal(dm1.get(c).mSt))`, disjoined with the cascade arm |

Faithfulness evidence for the second mechanism: `any_dep_terminally_failed` (dag/mod.rs:950–964,
Poisoned|DependencyFailed|Cancelled over in-DAG children, no liveness filter);
`compute_initial_states` (dag/mod.rs:1402–1441, the :1426 condemnation branch, persisted at
recovery.rs:841–849); pinned by the pre-existing production tests
`test_initial_states_with_prepoisoned_dep` (dag/tests.rs:736 — the cross-build case: build1's
poisoned leaf condemns build2's parent with no co-ownership) and
`test_recovery_substituting_with_poisoned_dep_goes_dependency_failed`
(actor/tests/recovery.rs:2271).

Encoding decisions recorded:

- The A22 latch (`condemnUnscoped`) stays keyed on the cascade arm only. A22's spec subject
  (`sched.recovery.failed-dep-cascade+2`) constrains the cascade pre-pass, which production
  does scope by co-ownership; the in-DAG recompute is the divergent mechanism and is the
  residual finding's subject (below), not an A22 violation. A22 therefore keeps holding
  as-built, and the calibration hook for cascade-mis-scoping overrides keeps its meaning.
- The model's new arm is single-level (checks pass-1 statuses) where production's `will_fail`
  set propagates transitively in topo order. Immaterial for verdicts: an intermediate condemned
  child is itself MDepFailed in dm2 (terminal => progressArmed), and a build whose recovered
  interest contains it is failed by the recovery tail (`memberFailed`), exactly like
  production's `finalize_recovered_builds`.
- The model's new arm has no I-059 orphan gate and no Created/Queued/Substituting status
  filter (production's `to_recompute` has both). Immaterial for every checked property: an
  L3-relevant node has live interest by definition, and the over-condemnation of true orphans
  is unobservable by the wired invariant/witness set (verified empirically by the full battery
  below).

**2. Re-validation battery under the corrected encoding — green; no baseline breaks.**

- `quint typecheck`: clean (quint 0.32.0).
- Sim sweeps, `allInvariants`, 40 000 samples, rust backend, per design-scale regime + Duo:
  Base [ok] 8.9 s; FaultPersist [ok] 9.5 s; Failover [ok] 9.4 s; StaleTenure [ok] 9.3 s;
  Duo [ok] 7.9 s; AdversarialStore **[violation] = B9 `staleProducedUnlocked`** — NOT a
  baseline break: B9-under-adversarial-store is the documented pre-registered as-built trip
  (this map's B9 row; the 0b "B1 and B9 are expected to trip under adversarial-store" record;
  the 0c pre-registered observation (b)). Attribution to pre-existing behavior is airtight:
  the violating trace never fires `recoverAsLeader` (lead.gen = 1 and budget.failovers
  undecremented in every state — the correction only changes recoverAsLeader), and a same-seed
  re-run (`--seed=0x5d893ce5c423e2f1`) against the PRE-correction model reproduces the same B9
  violation in 149 ms. The trace consumes the storeGc budget (the GC-after-produce shape).
- All 25 wired `quint-closure-*` checks rebuilt green against the corrected model
  (`nix build`, exit 0 — run twice: once against the semantic correction, once against the
  final committed text), including every recovery-dependent check: the FailoverEx TLC
  witnesses (hole-recovery, recovery-clear), the three StaleDuo sim checks (A18 probe,
  stale-intent-apply, cross-tenure-walk), and calib-f8 (FAILOVERS=1, production
  recoverAsLeader in its calibStep). No wired witness became unreachable (stop-and-report
  condition 4 not triggered); no calibration stopped falsifying (condition 3 not triggered).

**3. The L3 re-hunt (corrected encoding) — no violation under either backend.**

Backend 1 — rust simulator (existential search; the wired sim checks' budget shape):

| Scope | Invariant | Budget | Result |
|---|---|---|---|
| closureEvidenceFailoverEx | liveBuildTerminalOrProgressArmed | 2 000 000 samples x 14 steps | [ok] no violation (36.4 s) |
| closureEvidenceDuo | liveBuildTerminalOrProgressArmed | 2 000 000 samples x 14 steps | [ok] no violation (38.7 s) |

(Calibration of this signal: the simulator also could not find the PRE-correction violation —
the post-0d triage's 300 K x 14 record — so the sims corroborate but are not decisive.)

Backend 2 — TLC exhaustive BFS, 60 workers, 35-minute cap per run (the plan's Wave-2 budget),
all three runs on the same host on the same day with the same quint 0.32.0 / Apalache 0.56.1
distribution:

| Run | Model | Invariant | Result |
|---|---|---|---|
| Baseline (red half) | PRE-correction (Wave-1 tip) | liveBuildTerminalOrProgressArmed | **[violation]** at 2 024 464 distinct / 8 271 444 generated / Progress(12), 8 min 11 s — a 9-state trace ending in the strand (parent MQueued+marked+holed, child removed, build live). Re-confirms the 0d finding (2.06 M / 8.5 M / diameter 12 / 8 min 15 s) and proves this exact setup finds the violation when it exists |
| Re-hunt | POST-correction | asBuiltHoldInvariantsFailoverEx (the L3-free conjunction — "the correction breaks nothing else") | NO violation through 18 928 539 distinct / 85 343 972 generated / Progress(14) at the cap (unconverged) |
| Re-hunt | POST-correction | liveBuildTerminalOrProgressArmed (the L3-bearing form — the re-hunt proper) | NO violation through 19 114 520 distinct / 86 073 903 generated / Progress(14) at the cap (unconverged) |

Reading the unconverged re-hunt runs: both explored ~9.4x the distinct states and ~10.4x the
generated states of the coordinates where the pre-correction violation lives, two BFS progress
levels deeper, without a violation — and the violation's own depth class (a 9–10-state trace)
is fully enumerated well inside that prefix. Full convergence of the FailoverEx scope remains
a Wave-4 (T-4.5) measurement item — the same "convergence borderline" the plan's design table
predicted; the cap-hit here does not weaken the refutation, whose decisive comparison is
baseline-found vs re-hunt-not-found over a strictly larger prefix.

**4. The decision gate: outcome B — L3 as authorized does not exist.**

The defect the owner authorized a fix for (decision 1's L3 half: "failover + ClearPoison
strands a parent Queued/childless/un-armed under a live build forever") is refuted as a
production defect on three independent grounds:

1. **The corrected model cannot reach it** (the re-hunt above).
2. **The code walk** (the plan's design analysis §B, re-verified against this tree): three
   production mechanisms each independently prevent the strand state from forming —
   (i) `cascade_dependency_failure` (completion.rs:3425) condemns every un-started
   (Queued/Ready/Created) in-DAG ancestor when the poison lands, transitively, with no
   ownership scoping; (ii) `compute_initial_states`/`any_dep_terminally_failed`
   (dag/mod.rs:1402–1441/:950–964) condemns a recovered Created/Queued/Substituting parent
   above a loaded terminally-failed child (within-TTL poisoned children are loaded with their
   edges); (iii) `revert_target_for` (dag/mod.rs:979–987) sends a Substituting parent whose
   walk fails above a terminally-failed child to DependencyFailed, not Queued. The traced
   strand requires a parent Queued above a still-poisoned child at ClearPoison time; every
   production path to that configuration is closed by (i)–(iii).
3. **The trace's load-bearing step is the model artifact**: step 9 of the 0d trace (recovery
   restores d1 Queued above within-TTL-poisoned d2) is exactly the missing-arm state;
   production condemns d1 there (mechanism ii), the build fails with an actionable
   DependencyFailed error, and the ClearPoison step finds no live build to strand.

Consequences:

- The L3 row in the per-property table is flipped to REFUTED (model artifact); the header
  status block is updated.
- `asBuiltHoldInvariantsFailoverEx` (the L3-free conjunction) is retained in the model for
  comparison with the 0d record; the FailoverEx manual exhaustive target becomes the FULL
  `asBuiltHoldInvariants` (with L3) — nix/quint.nix's manual-target comment is updated
  accordingly.
- The plan's conditional Wave-2 tasks (T-2.2 strand red test, T-2.3 survivor re-evaluation,
  T-2.4 clear-poison spec amendment) DO NOT EXECUTE: there is no production defect to fix, and
  the T-2.3 hardening alone would be dead code in production (production never strands the
  parent, because it condemns it — the residual below).
- Wave-4 L3-conditional items (T-4.2 model L3 fix + calibration pin; the FailoverEx L3 row
  flip in T-4.5/T-4.6) are likewise moot in their outcome-A form; T-4.5's measurement of
  FailoverEx convergence proceeds with the full conjunction.

**5. The residual finding routed to the owner: production's unscoped recovery condemnation
(a C3-class behavior at a fourth decision point; spec-vs-code divergence).**

What it is: at recovery, `compute_initial_states`'s `any_dep_terminally_failed` branch
condemns a Created/Queued/Substituting parent (with live interest) above ANY in-DAG
terminally-failed child — regardless of build co-ownership. The spec rule
`sched.recovery.failed-dep-cascade+2` (scheduler.typ) mandates the opposite outcome for
non-co-owned failures: "A parent whose only failed-child evidence belongs to dead builds or to
builds that never owned it MUST NOT be condemned by the cascade --- it recovers normally
(childless if the edge was dropped) and any genuine problem is re-discovered at dispatch
time."

Code walk: the rule's "recovers normally (childless if the edge was dropped)" premise holds
for `dependency_failed`/`cancelled`/expired-poisoned children — those rows are not loaded,
their edges are dropped, the parent recovers childless and Ready, and the dispatch sweep
(post-Wave-1: the settlement re-probe) re-discovers any genuine problem. It does NOT hold for
a within-TTL POISONED child: that row IS loaded (required by
`sched.recovery.poisoned-failed-count` for TTL tracking) and keeps its edge, so
`compute_initial_states` sees it and condemns the parent to DependencyFailed (persisted), even
when no live build co-owns the child and even when the parent's own wanted outputs are present
or substitutable. The owning build then fails at the recovery tail
(`finalize_recovered_builds` -> `check_build_completion`). The wrongful-failure window is:
failover x within-TTL poisoned child x parent-owning build that does not co-own that child.
The two production tests named in section 1 PIN this behavior (one is exactly the cross-build
case), so it is deliberate hang-prevention, not an accident.

Wave-1 coverage: NONE. The settlement helper (`settle_broken_marked_root`) is reached from
exactly three sites — the dispatch-probe partition, the `handle_substitute_complete` Broken
arm, and the reap-survivor hook. The recovery condemnation transitions the node to terminal
DependencyFailed before any dispatch sweep runs; no settlement site ever sees it.

Why production is shaped this way (the trade-off the owner must adjudicate): the unconditional
condemnation is precisely what prevents the L3 hang in production. If recovery honored the
spec's scoping for the loaded-poisoned-child case, the parent would recover Queued above the
poisoned child, and when ClearPoison/TTL-sweep later removes that child, nothing re-evaluates
the parent (production's removal paths do not re-evaluate survivors) — the build hangs
forever, which is the refuted L3 trace's shape made real. Production traded a bounded wrongful
failure (terminal, actionable, resubmittable) for hang-freedom.

Disposition options for the owner (this stage takes no action):

- **(a) Accepted bound + spec amendment**: amend `sched.recovery.failed-dep-cascade+2` to
  carve out the loaded-poisoned-child case (production behavior becomes spec-sanctioned),
  record the bound in this map and the deployment checklist. Rationale: the harm is one
  wrongful build failure per (failover x within-TTL-poisoned non-co-owned child) conjunction;
  the failed build is recoverable by directed resubmit, and post-Wave-1 the resubmitted
  build's settlement handles the re-merged node correctly. The hang-freedom this buys is
  exactly what L3 checks.
- **(b) Spec-conformance fix** (NOT authorized by current decisions; needs a new red-first
  plan): scope the in-DAG condemnation by co-ownership (mirror the cascade's evidence rule)
  AND add the survivor re-evaluation on poison-clear removals (the conditional T-2.2..T-2.4
  shape). Both halves are required together: the scoping alone re-introduces the L3 hang; the
  re-evaluation alone is dead code. This option converts the recovery decision point to the
  same settle-don't-condemn shape Wave 1 gave the dispatch/reap decision points, at the cost
  of touching `compute_initial_states` (shared with the merge path) and the recovery sequence.

**6. Updated owner sign-off items (supersedes the post-0d triage list; renumbered).**

1. The C5 / CE-7 deferral — unchanged (post-0d item 1).
2. The F6 falsifications resting on the rust simulator — unchanged (post-0d item 2).
3. The CE-45 (F10) evidence-module-only falsification — unchanged (post-0d item 3).
4. The mkQuintWitnessCheck rust-backend extension funding question — unchanged (post-0d
   item 4 residual).
5. ~~The FailoverEx conjunction violation / L3 as-built finding~~ — **REFUTED by this stage**
   (the recovery-condemnation correction + re-hunt + code walk above). Owner decision 1's L3
   fix authorization is moot as premised. NEW adjudication request in its place: the residual
   finding (section 5 above) — disposition (a) accepted bound + spec amendment vs (b)
   spec-conformance fix.
6. C3 — RESOLVED by Phase 1 Wave 1 (the settlement fix; red-first, landed). The model-side C3
   flip (settlement encoding + calibration pin re-point) is Wave 4 (T-4.1).
7. The 0c carry-overs — unchanged, except the FailoverEx manual target is now the full
   conjunction (consequence 2 of the gate above).

### Phase 1 Wave 2b — the residual-finding fix: co-ownership scoping + poison-clear survivor re-evaluation (this stage)

**Headline: the Wave-2 residual finding (production's unscoped recovery condemnation) is FIXED
red-first, both halves together, per the owner's 2026-05-30 decision (disposition (b)).** The
recovery in-DAG recompute now condemns a recovered parent only for a terminal child a live
build co-owns with it, and both poison-removal paths re-evaluate surviving parents so the
spared parent makes progress when the poison clears. The model mirrors both halves; the L3
re-hunt under the corrected encoding finds no violation at any hunted scope under either
backend, while the red half (scoping without the promotion) re-finds the 9-state strand under
TLC in under six minutes — the calibration evidence that the scoping+promotion PAIR, not
either half alone, closes the strand. All 25 wired checks rebuilt green; the production test
battery needed zero updates.

**1. The owner decision and the fix shape.**

The Wave-2 record routed the residual finding to the owner with two dispositions: (a) accepted
bound + spec amendment, or (b) spec-conformance fix. The owner chose (b) on 2026-05-30, with
the explicit requirement that both halves land together — the Wave-2 record's analysis that
"the scoping alone re-introduces the L3 hang; the re-evaluation alone is dead code" is
confirmed empirically by this stage's red-half TLC run (section 6).

**2. Red-first execution (production) — rio-scheduler commit `7750a4d45`.**

- RED: `test_recovery_cross_build_poisoned_dep_spares_non_co_owning_parent`
  (actor/tests/recovery.rs) stages the exact residual scenario — build A full-merges
  parent→child and is dead at recovery with the child poisoned within TTL; build B owns only
  the parent (the bug_009 pruning-build shape; the child's `poisoned_at` is future-dated so
  the within-TTL load is deterministic under the 100ms cfg(test) TTL). Pre-fix failure
  verified: `assertion failed: ... got DependencyFailed, expected Queued` — the wrongful
  condemnation, exactly the finding's characterization. The companion
  `test_poison_clear_reevaluates_spared_recovered_parent` (admin-clear + ttl-expiry rstest
  cases) pins the second half and was red for the same reason (its fixture premise is the
  spared recovery).
- FIX half 1 (co-ownership scoping): `compute_initial_states`'s condemnation arm now goes
  through the new `DerivationDag::any_co_owned_dep_terminally_failed` — a terminal child
  counts only when `interested_builds` of parent and child intersect (the in-memory mirror of
  `load_parents_with_failed_deps`'s SQL evidence rule; at recovery the in-memory sets ARE the
  durable links of live builds). The `will_fail` transitive propagation carries the same
  scoping. `any_dep_terminally_failed` keeps its unscoped form for `revert_target_for`
  (first-party walk-failure judgments — mechanism (iii) of the Wave-2 code walk, deliberately
  untouched).
- FIX half 2 (survivor re-evaluation): the Wave-1 reap-survivor loop is extracted into
  `reevaluate_removal_survivors` (dispatch.rs) — settlement for marked-Broken survivors,
  promotion (Ready + push + persist) for Queued survivors whose remaining deps are satisfied —
  and is now called from all three removal sites: the terminal-build reap (unchanged
  behavior), admin `ClearPoison` (`handle_clear_poison`) and the poison-TTL sweep
  (`tick_process_expired_poisons`). The two poison sites previously stamped closure holes and
  deliberately did NOT re-evaluate ("must not trigger parent verdicts" — a rationale that
  Wave 2b's scoping invalidates: the spared parent's only wake-up edge is exactly this
  removal).
- GREEN: both new tests pass post-fix; clippy (stable, CI parity), treefmt, tracey validate
  and the full rio-scheduler nextest suite are green at the commit boundary.

**3. The existing-test battery — zero updates needed (under the predicted bound of 2+2).**

1094/1094 tests pass unmodified. The Wave-2 record predicted the two condemnation-pinning
tests would regress; they do not, and the reason is informative:

| Test | Why it stays green |
|---|---|
| `test_initial_states_with_prepoisoned_dep` (dag/tests.rs) | Build 2's merge node list INCLUDES the poisoned leaf, so build 2 co-owns it (`dag.merge` adds the merging build's interest to existing nodes); the scoped condemnation still fires. The test pins the legitimate co-owned case, not the unscoped-ness. |
| `test_recovery_substituting_with_poisoned_dep_goes_dependency_failed` (actor/tests/recovery.rs) | One build owns both nodes, so the parent is condemned by the cascade pre-pass (`load_parents_with_failed_deps` → `cascade_failed`), which was always co-ownership-scoped; the in-DAG recompute never even sees it. |

Every other condemnation-adjacent test (bug_341, bug_051, the transitive-persist test, the
keep_going merge tests, the poison-clear closure-hole tests, the keep_going poison-removal
tests) likewise stages co-owned scenarios or in-flight survivors that the new survivor loop
deliberately skips. The cross-build non-co-owned case had NO pre-existing test — which is
exactly why it survived as a residual finding until the Wave-2 model review surfaced it.

**4. Spec changes.**

- `sched.recovery.failed-dep-cascade+2`: text UNCHANGED, no bump — the rule already mandated
  the scoped outcome (its MUST NOT clause); production now conforms. New impl markers on
  `any_co_owned_dep_terminally_failed` / `compute_initial_states`, new verify marker on the
  red test. (The Wave-2 record's planned "divergence note" amendment is moot under
  disposition (b): there is no divergence left to document.)
- NEW rule `sched.poison.clear-survivor-reevaluation` (scheduler.typ, after the
  cascade-dependents rule): both poison-removal paths MUST re-evaluate surviving parents
  (promotion / settlement / skip rules as implemented), with the rationale prose explaining
  the pairing with the failed-dep-cascade scoping. Impl markers at the shared loop and both
  poison call sites; verify markers on the new rstest pair. tracey validate green; the rule
  is neither uncovered nor untested.

**5. The model update (closureEvidence.qnt) — both halves mirrored.**

- `recoverAsLeader` pass-2 condemnation arm (b) flips from the unscoped
  `kidsLoaded.exists(c => isUnprodTerminal(...))` to the scoped `pInDagCondemnCriterion`
  (new pure def): a loaded un-produced-terminal child condemns only when a live build's
  durable links contain both parent and child — the model form of production's
  interested-builds intersection (identical at recovery, where interest IS the durable-link
  relation). Over the loaded relation the scoped arm (b) is subsumed by arm (a)
  (pCondemnCriterion); it is kept as a separately named criterion so the two production
  mechanisms stay separately visible and separately falsifiable.
- `poisonClear` gains the survivor promotion arm: a MQueued surviving parent whose remaining
  loaded kids are all MProduced (vacuously, when the cleared child was the last) is promoted
  to MReady — the same mSt-only promotion `reapTerminalBuild`'s dmFinal pass applies. The
  settlement arm needs no separate encoding (a promoted/existing MReady marked survivor is
  what the dispatch-partition letters fire on; an MFailed survivor re-arms via requeueFailed).
- The A22 latch (`condemnUnscoped`) stays keyed on the cascade arm (a) — unchanged semantics,
  updated comment. `asBuiltHoldInvariantsFailoverEx` is retained for 0d comparison with an
  updated header.

Encoding decisions recorded: the model promotion is single-step and mSt-only (recovery
re-derives dispatchability from the dep walk, so the durable status is immaterial — the
existing reap-promotion encoding decision, reused); production's settlement arm for
marked-Broken non-Queued survivors maps onto the existing dispatch-partition letters rather
than a new poisonClear branch (same observable: the survivor is ARMED, which is all L3 reads).

**6. The L3 re-hunt — the strand stays closed by the scoping+promotion pair.**

Backend 1 — rust simulator (the wired sim checks' budget shape), 2 000 000 samples × 14 steps,
`liveBuildTerminalOrProgressArmed`:

| Scope | Model | Result |
|---|---|---|
| closureEvidenceFailoverEx | Wave-2b (scoping + promotion) | [ok] no violation (67.4 s, 29 684 traces/s) |
| closureEvidenceDuo | Wave-2b (scoping + promotion) | [ok] no violation (70.8 s, 28 233 traces/s) |
| closureEvidenceFailoverEx | red half (scoping, NO promotion) | [ok] no violation (68.0 s) — the simulator cannot find this strand class (consistent with the Wave-2 calibration note: it also missed the pre-correction violation), so the sims corroborate but are not decisive; TLC below is |
| closureEvidenceDuo | red half | [ok] no violation (70.9 s) — same reading |

Supplementary `allInvariants` sweeps (40 000 samples × 14 steps, the Wave-2 re-validation
battery shape): Base / FaultPersist / Failover / StaleTenure / Duo / FailoverDuo / C3Duo all
[ok]; AdversarialStore reports the documented pre-registered B9 trip
(`staleProducedUnlocked`, GC-after-produce) — NOT a Wave-2b break: the violation reproduces
with the same seed (`0x58b7a35c717a4f85`) against the Wave-2 baseline model (HEAD~1) in
143 ms, and B9-under-adversarial-store is the standing pre-registered as-built observation
(0b/0c records, Wave-2 record).

Backend 2 — TLC exhaustive BFS, 60 workers, same host / same day / same quint 0.32.0 +
Apalache 0.56.1 distribution for all runs:

| Run | Model | Invariant | Result |
|---|---|---|---|
| Red half (the calibration) | scoping WITHOUT the poisonClear promotion | liveBuildTerminalOrProgressArmed | **[violation]** at 1 884 601 distinct / 7 697 354 generated, 5 min 46 s — a 9-state trace: mergeCommit → pgApply → poisonArrival → mergeCommit (the resubmit whose IMerge REPLACES the build's links, making the poisoned child non-co-owned) → leaderLost → pgApply → recoverAsLeader (the SCOPED condemnation now spares d1 → recovers MQueued above MPoisoned d2) → poisonClear (d2 removed, NO promotion) → final state: b1 BpActive interest={d1}, d1 MQueued childless un-armed, d2 MAbsent. This is the 0d strand made real by the scoping — proof that (i) this exact TLC setup finds the violation when it exists, and (ii) half 2 is load-bearing |
| Re-hunt (the fix) | Wave-2b (scoping + promotion) | liveBuildTerminalOrProgressArmed | NO violation through 21 384 877 distinct / 96 719 329 generated / Progress(14) at the 35-minute cap (unconverged) — 11.3× the distinct and 12.6× the generated states of the red-half violation coordinates, with the violation's own depth class (a 9-state trace) fully enumerated well inside that prefix |
| Re-hunt (breaks nothing else) | Wave-2b (scoping + promotion) | asBuiltHoldInvariants (the FULL conjunction, L3 included — the FailoverEx manual-target form) | NO violation of ANY conjunct through 23 695 257 distinct / 107 664 812 generated / Progress(14) at the 35-minute cap (unconverged) — 12.6× the distinct and 14× the generated states of the red-half violation coordinates |

Reading: the red-half violation sits at the same coordinate class as the Wave-2 baseline
violation (2.02 M distinct / 8.27 M generated / 8 min 11 s on the pre-correction model) — the
strand's depth did not move, only its closing mechanism did. The re-hunt runs explore well
past those coordinates without a violation, so the verdict criterion is the same
baseline-found vs re-hunt-not-found comparison over a strictly larger prefix that the Wave-2
record used.

**7. The 25 wired checks — all green against the Wave-2b model.**

`nix build` of all 25 `quint-closure-*` checks (the .qnt fileset is their eval input, so every
one rebuilt against the modified model): exit 0, 25/25. This includes every
recovery/poison-dependent check — the FailoverEx TLC witnesses (hole-recovery,
recovery-clear), the BaseEx hole-admin-clear / hole-ttl-sweep witnesses (poisonClear
reachability, now exercised against the promotion-bearing action), the three StaleDuo sim
checks, the C3 probe at Duo, the downgrade-respawn witness at C3Duo, and calib-f8 / calib-f9
(whose calibSteps consume the production recoverAsLeader / poisonClear respectively). No
witness became unreachable; no calibration stopped falsifying.

**8. Updated owner sign-off items (supersedes the Wave-2 list; renumbered).**

1. The C5 / CE-7 deferral — unchanged (Wave-2 item 1).
2. The F6 falsifications resting on the rust simulator — unchanged (Wave-2 item 2).
3. The CE-45 (F10) evidence-module-only falsification — unchanged (Wave-2 item 3).
4. The mkQuintWitnessCheck rust-backend extension funding question — unchanged (Wave-2
   item 4).
5. ~~The residual finding (unscoped recovery condemnation)~~ — **FIXED by this stage**
   (disposition (b), red-first, both halves; commit `7750a4d45` + the model/map commit). The
   wrongful-failure window (failover × within-TTL poisoned child × non-co-owning
   parent-owner build) is closed; the spared parent's progress is owned by
   `sched.poison.clear-survivor-reevaluation`.
6. C3 — RESOLVED by Phase 1 Wave 1 (unchanged; the model-side C3 flip is Wave 4 / T-4.1).
7. The 0c carry-overs — unchanged: the held-back exhaustive conjunctions (the FailoverEx
   manual target remains the full `asBuiltHoldInvariants`, now also exercised by this
   stage's capped TLC run) and the deferred A17 / L2 / C1-strict probes.

### Phase 1 Wave 3 — uniform claims-floor fencing of every evidence write (this stage)

**Headline: the owner's FENCE EVERYTHING decision (2026-05-30, design §5 D15 option (b)) is
implemented.** Every evidence write the scheduler issues — the merge transaction, the four
batched mark/hole helpers, the five status/poison pool-variant writers, and every owning
transaction that carries their `_in_tx` bodies — now reads the durable claims floor
(`GREATEST` over `assignments.generation` ∪ `leader_generation_claims.generation`) on its own
connection inside its own transaction and rolls back having written nothing when the issuing
tenure's serving generation sits below it. `sched.evidence.durability` is bumped to `+2` with
the fence as a normative MUST. No migration: the floor reads the two existing tables
(the pull-mint pattern, `mint_pull_attempt_fenced`).

**1. The six commits (red-first working-tree ritual; transcripts in commit bodies).**

| Commit | Task | Content |
|---|---|---|
| `498db7410` | T-3.1 | Tenure-tracking `serving_generation` + the LeaderAcquired claim-before-recovery reorder + the saturated-floor tripwire test |
| `889a575e6` | T-3.2/T-3.3 | `FencedWrite`/`claims_floor`/`at_or_above_floor` primitives; the four batch.rs evidence helpers fenced; red test `stale_tenure_clear_does_not_erase_newer_evidence` + positive companions |
| `55a1fa6cb` | T-3.4 | The merge transaction fenced (begin-time + commit-adjacent authoritative re-read); `ActorError::StaleGeneration` → gRPC FAILED_PRECONDITION |
| `865b85dbe` | T-3.5 | The five status/poison pool writers + eight owning transactions fenced; red test `stale_tenure_status_and_poison_writes_are_fenced` |
| `26ad50144` | T-3.6 | Actor-level end-to-end deposed-actor test (`fencing.rs`); the ClearPoison/TTL-sweep `Ok(Fenced)` caller-contract fixes it forced |
| (this commit) | T-3.7 | `sched.evidence.durability+2` normative text; marker re-pointing; this record |

**2. The fenced-statement inventory (final, post-Wave-3 file:line).**

The capture: every statement carries `DagActor::serving_generation` — the tenure-tracking
field (mod.rs:750 init, recovery.rs:1691 claim stamp; **no per-command read of the lease
atomic anywhere**) — through `serving_generation()` at the call site.

Statements (the owner's "~10"):

| # | Statement | Where | Fence form |
|---|---|---|---|
| 1 | merge tx: `batch_upsert_derivations` + `batch_insert_build_derivations` + `batch_insert_edges` + `activate_build_tx` | actor/merge.rs `persist_merge_to_db` (early check ~:2067, authoritative commit-adjacent re-read ~:2161) | two floor checks; below-floor → `ActorError::StaleGeneration` → gRPC FAILED_PRECONDITION |
| 2 | W2 `clear_topdown_pruned_by_hashes` | db/batch.rs:355 | tx-wrapped helper, `FencedWrite` return |
| 3 | W3 `clear_topdown_pruned_by_hash` | db/batch.rs:413 | tx-wrapped helper |
| 4 | W4 `set_closure_hole_by_hashes` | db/batch.rs:466 | tx-wrapped helper |
| 5 | W5 `clear_closure_hole_by_hashes` | db/batch.rs:512 | tx-wrapped helper |
| 6 | `update_derivation_status` | db/derivations.rs:97 | fence inside the existing owned tx |
| 7 | `update_derivation_status_batch` | db/derivations.rs:180 | same |
| 8 | `persist_poisoned` | db/derivations.rs:283 | same |
| 9 | `clear_poison` | db/derivations.rs:365 | same |
| 10 | `clear_poison_batch` | db/derivations.rs:408 | same |

Owning transactions (one floor check after `begin()`; the `_in_tx` variants themselves stay
unfenced by design — their fence lives at the owner):

| # | Transaction | Where | Plan status |
|---|---|---|---|
| 11 | `record_attempt_with_poison` | actor/completion.rs:145 | enumerated |
| 12 | `record_reset_with_clear_poison` | actor/completion.rs:302 | enumerated |
| 13 | `record_resubmit_resets` | actor/completion.rs:339 | enumerated |
| 14 | `handle_transient_failure` | actor/completion.rs:2735 | **enumeration correction** |
| 15 | `handle_infrastructure_failure` | actor/completion.rs:2988 | **enumeration correction** |
| 16 | `handle_permanent_failure` | actor/completion.rs:3247 | **enumeration correction** |
| 17 | `handle_timeout_failure` | actor/completion.rs:3401 | **enumeration correction** |
| 18 | `record_cascade_attempts_and_status` | actor/completion.rs:3662 | **enumeration correction** |

**Enumeration correction (the Wave-1 precedent applied).** The plan's table counted "~10
statements + 4 owning transactions"; rows 14–18 are five additional owning transactions the
enumeration missed — the Phase-1b failure-classification handlers, which persist status/poison
evidence through the same `_in_tx` bodies inside their own appending transactions. They
pre-existed the plan (they are not Wave-1/2b additions); all five take the identical fence
pattern (floor check after `begin()`, below-floor → rollback + `FailureHandling::Handled`,
i.e. drop-the-report — re-delivery would re-hit the fence; the successor re-derives the
failure from the open attempt row via its establishment sweep). Waves 1/2b added new CALL
SITES of already-enumerated statements (the settlement helper's fail-fast/lazy clears, the
survivor re-evaluation's status persists), not new statements; those call sites were swept up
by the parameter threading.

Already fenced before this wave (no change, different rule —
`sched.lease.generation-fence+3`): the pull-mint transaction (db/open_attempts.rs) and the
establishment-charge transaction (actor/housekeeping.rs, which also carries a poison persist).
The establishment charge keeps its per-sweep lease-atomic capture: it implements the
attempt-ledger fence rule, not the evidence rule; its FC-2-style residual is bounded by the
Tick handler's `is_leader` gate and by the fence it already carries.

**3. The capture redesign (CF-2/FC-2/OQ7) and its verification.**

`handle_leader_acquired` now runs floor-read → claim → `serving_generation` stamp →
`recover_from_pg` (the reorder), so a new leader's recovery evidence writes carry the claimed
generation and pass the fence *because the claim made them the floor*. The lease-atomic seed
(`seed_generation_from`) stays at the post-TOCTOU-gate tail (moving it would false-positive
the gate); the fence reads the field, not the atomic.

- **FC-2 verification** — `saturated_floor_recovery_evidence_writes_land`
  (actor/tests/recovery.rs): a fresh leader (lease generation 1) over a foreign claim row at
  200 claims 201 and lands BOTH recovery evidence-write classes (the W4 closure-hole stamp and
  the poisoned-dep DependencyFailed status persist). Green at every Wave-3 commit boundary —
  the tripwire never fired.
- **Permissiveness** — `live_leader_evidence_writes_are_never_fenced` (actor/tests/fencing.rs)
  + `current_tenure_*_apply` db pairs + the full single-tenure batteries: the fence never
  rejects a live single leader (the `evidence_writes_fenced` test counter stays 0 across the
  entire suite except the two deposed-actor tests).
- **Same-epoch keep (OQ4)** — `same_generation_write_at_floor_applies` pins `>=`; the
  comparison can never be tightened to `>` without a red test.
- **End-to-end** — `deposed_actor_evidence_writes_are_fenced`: a real actor + a successor
  claim + admin ClearPoison ⇒ cleared=false, poison survives in PG, fenced-counter increments.
  This test caught a real wiring gap (callers matching the fenced reset with `if let Err`
  treated `Ok(Fenced)` as success); the gap is fixed in the same commit.

**4. Residuals (stated normatively in `sched.evidence.durability+2`).**

- The fence is window-narrowing, not serializability: every fenced write retains a residual of
  one floor-read-to-commit round trip; for the multi-statement merge transaction the
  commit-adjacent re-read is what brings its window down from the whole tx body (FC-4).
- Not covered (not PG evidence writes): same-epoch in-flight work (required to survive),
  walk-completion consumption (model-covered, `canReachCrossTenureWalkConsume` stays
  reachable), the documented Lease-deletion + PG-fault conjunction.
- The status/poison fence has NO model coverage (those writes are not modeled as intents —
  ENC-0b-2): its verification is the T-3.5 db test pairs + the T-3.6 actor-level test (OQ3
  correction).

**5. Battery status.**

Full rio-scheduler suite green at every commit boundary (1096 → 1104 tests, +8 from this
wave); the recovery/floor battery (`test(recovery)`, 66 tests) green with **zero assertion
changes** (the OQ7 acceptance gate); merge battery (97) green; clippy (stable), tracey
validate, treefmt green at every boundary. Model-side flips (A17/A18 → holds, the fence
encoding) are Wave 4 (T-4.3); the A17/A18 rows in the per-property table above stay
"expected-fail probe" until then.

Later phases append here (Wave 4: model + CI updates; Wave 5 / close-out: acceptance table
over the full corpus, deployment-checklist deltas and counter-signatures).
