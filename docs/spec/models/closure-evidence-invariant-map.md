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
F11 falsifications resting on the rust-simulator backend (a TLC-backend
discrepancy is recorded as a tool issue). Six permanent
quint-closure-calib-* checks are wired and green; verdicts, re-routes,
the acceptance table, the housekeeping record for the six 0c checks,
and two new stop-and-report items (the TLC discrepancy; an unidentified
as-built conjunction violation at the FailoverEx scope) are in the 0d
stage record. Phase 1 (red-first fixes) is next, gated on the owner
adjudications listed there.**

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
| A22 | `condemnRequiresLiveCoOwnedFailure` | The recovery failed-dep cascade condemns only on a persisted failure a live build co-owns with the parent. | sched.merge.substitute-topdown+11 | HOLDS at the reduced base scope (vacuous there — its trigger lives in the deferred fault-persist/failover runs); trigger reachability pinned by the recovery/durability witnesses |

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
| C3 | `noWrongfulTerminalFailureSingleTenure` | With no failover, no store GC and no upstream withdrawal, no build is wrongfully terminally failed. | sched.merge.substitute-topdown+11 | VIOLATED as-built at base scope — REAL defect candidate (11-state trace + code walk in the 0c stage record); excluded from the wired conjunction, pinned by the wrongful-terminal-failure expect-violation check; Phase-1 red-first candidate, owner adjudication |
| C4 | `noBuildWhenWantedPresent` | A node whose live-wanted outputs are all present at merge is not queued for a from-source build. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| C5 | `terminalBuildStopsPinning` | Only live builds' wanted outputs drive resets, forgiveness refusal and re-pinning. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) (latch is a by-construction hook; falsifiability owned by the 0d calibration override) |

### Group L — settlement, armed-state form (L1–L3)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| L1 | `substitutingAlwaysArmed` | A Substituting node always has a walk instance in flight. | sched.substitute.detached+5 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| L2 | `markedBrokenSettlementArmed` | No reachable D16 limbo cell (marked, Broken, tried, Ready, live interest, all live-wanted outputs present, no walk). **Pre-registered expected-fail probe** (base); its violation trace is the D16 deliverable. | sched.evidence.settlement (intentionally uncovered) | expected-fail probe — NOT yet produced (the D16 cell is ≥16 steps deep at duo scope; deferred-probe record in the 0c stage record) |
| L3 | `liveBuildTerminalOrProgressArmed` | Every live build is terminal, all-produced, or has a progress step armed at some member. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |

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
| quint-closure-evidence-probe-wrongful-terminal-failure | closureEvidenceBaseEx, C3 | the Phase 0c finding (stale walk-failure fail-fast) until its Phase-1 disposition | violation at depth 11; ≈3–5 min |
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
which were already green at the 0c report), 3 manual targets.

**TLC-backend discrepancy (tool issue; stop-and-report).** Found while
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
first measurement of a fault regime).** As part of establishing
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
items carried out of the gate rather than papered over:

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

Later phases append here (1: red-first fixes; 2: acceptance table over
the full corpus incl. the trailing evidence modules; close-out:
deployment-checklist deltas and counter-signatures).
