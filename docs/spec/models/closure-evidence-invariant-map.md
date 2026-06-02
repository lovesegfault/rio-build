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
downgraded. **Phase 1 complete** (the per-wave stage records and the
Phase-1 close-out are the last sections of this map): Wave 1 (the C3+D16
settlement, red-first)
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
the calibration that the pairing is load-bearing); Wave 3 (owner decision
2026-05-30, FENCE EVERYTHING) implemented the uniform claims-floor fence on
every production evidence write (`sched.evidence.durability+2`); Wave 4
encoded both Phase-1 fixes in the model and flipped the wired checks — C3,
the L2 armed form, A17 and A18 all HOLD, each paired with an
expect-violation calibration pin (closure-c3-no-reprobe /
closure-a17-unfenced) that keeps the falsification direction permanently
checkable; the A17/L2 wiring satisfies owner decision 4, the `longChecks`
GHA-exclusion mechanism plus the measured manual-target table satisfies
owner decision 3; Wave 5 (close-out) records the final dispositions, the
acceptance-table deltas, the deployment-checklist deltas, and the
owner-decision provenance. See the Phase-1 Wave-2/2b/3/4 stage records
and the Phase-1 close-out. **Phase 2 complete** (the last section): the
two queued C4-memo fixes (the post-terminal BuildProgress freeze,
red-first, and the standby-drops-writes carve-out), the kani kernel
extraction (rio-evidence-kernel — the closure-evidence classifier AND
the pull-admission decision, 13 CBMC-verified harnesses wired as
checks.kani-rio-evidence-kernel), and the full-corpus CE-1..CE-81
acceptance table (81/81 rows dispositioned, no coverage gap). The
campaign's remaining open work is the close-out counter-signature (A6)
over this map's records plus the standing owner items listed in the
Phase-2 stage record.**

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
| A17 | `noStaleTenureClearOverride` | A clear/heal intent created under tenure g never erases a bit a newer tenure stamped. | sched.evidence.durability+2 | **FIXED Phase 1** (Wave 3: the production claims-floor fence on every evidence write; Wave 4: the model encoding — pgApplyAny discards stale evidence intents). HOLDS in every regime; in `allInvariants`/`asBuiltHoldInvariants`; wired holds check `quint-closure-evidence-stale-fence-holds` (decision 4); falsifiability via the `quint-closure-calib-a17-unfenced` pin (the A18 falsification is the wired oracle — A17's own deeper clear-then-restamp interleaving was never produced in any hunted budget, and the same fence closes both). The 0c "dangerous direction" analysis (W2/W5 erasing a newer tenure's hole) is what the fence now structurally prevents |
| A18 | `leaderClassEvidenceWrites` | Only the current tenure's evidence intents reach PG. | sched.evidence.durability+2 | **FIXED Phase 1** (Wave 3 production fence + Wave 4 model encoding). HOLDS; in the conjunctions; wired via `quint-closure-evidence-stale-fence-holds`; regression-pinned by `quint-closure-calib-a17-unfenced` (frozen pre-fence apply still produces the 5-state D14-leg trace). The stale-write window's continued reachability (deposed intents still survive into the successor's tenure) is the `canReachFencedDiscard` witness — wired as `quint-closure-evidence-witness-fenced-discard`, replacing the now-unreachable `canReachStaleIntentApply` check |
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
| C3 | `noWrongfulTerminalFailureSingleTenure` | With no failover, no store GC and no upstream withdrawal, no build is wrongfully terminally failed. | sched.merge.substitute-topdown+12, sched.evidence.settlement | **FIXED Phase 1** (Wave 1: the settlement re-probe at the three production fail-fast sites; Wave 4: the model encoding — probeSettleTried, the consumeWalk Broken-arm settlement, the reap-survivor settlement with the MQueued exclusion). HOLDS at BaseEx and Duo under the settlement-encoded model (Wave-4 stage record); back in `asBuiltHoldInvariants`/`allInvariants`; wired holds check `quint-closure-evidence-settlement-holds`; falsifiability preserved by the `quint-closure-calib-c3-no-reprobe` regression pin (pre-fix actions still falsify). The reap-survivor path is the second C3 violation family (adjudication OQ2 / review finding MCI-2), closed by the same settlement |
| C4 | `noBuildWhenWantedPresent` | A node whose live-wanted outputs are all present at merge is not queued for a from-source build. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| C5 | `terminalBuildStopsPinning` | Only live builds' wanted outputs drive resets, forgiveness refusal and re-pinning. | sched.merge.substitute-topdown+11 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) (latch is a by-construction hook; falsifiability owned by the 0d calibration override) |

### Group L — settlement, armed-state form (L1–L3)

| # | Property | Statement (one line) | Rule(s) | Status |
|---|---|---|---|---|
| L1 | `substitutingAlwaysArmed` | A Substituting node always has a walk instance in flight. | sched.substitute.detached+5 | No violation found at the reduced base scope (the closing run was capped before convergence — 0c run record) |
| L2 | `markedBrokenSettlementArmed` | **ARMED/settlement form (Phase-1 restatement)**: every reachable D16 limbo cell is covered by the settlement partition (some settling action's availability arm holds for it). The 0b state form ("no reachable cell") is retired — post-fix the cell is a designed transient (reap/poison-clear promotion → next-sweep settlement), so state-unreachability can never hold; the armed form is what the fix guarantees (review findings RT-3/MCI-1/FC-3; orchestrator call within owner decision 4). | sched.evidence.settlement | **FIXED Phase 1 / WIRED (decision 4)**: HOLDS at Duo/C3Duo; in `allInvariants`/`asBuiltHoldInvariants`; wired via `quint-closure-evidence-settlement-holds`; the cell's reachability (the state form's real content) is the now-demonstrable `canReachD16Cell` witness — wired as `quint-closure-evidence-witness-d16-cell` (pre-fix it was unreachable in any hunted budget; post-fix the promotion transient assembles it at shallow depth, the settlement action's non-vacuity proof). The pre-fix partition form (`markedBrokenSettlementArmedPreFix`) lives in the closure-c3-no-reprobe override and falsifies under the production step — the pre/post distinguisher |
| L3 | `liveBuildTerminalOrProgressArmed` | Every live build is terminal, all-produced, or has a progress step armed at some member. | sched.merge.substitute-topdown+11, sched.poison.clear-survivor-reevaluation | No violation found at the reduced base scope (0c run record); the post-0d triage's failover-scope violation is **REFUTED as a production defect — model artifact of the recovery-condemnation gap (Phase 1 Wave 2)**: the traced strand needed recovery to leave the parent Queued above its still-poisoned child, which pre-Wave-2b production never did (the unscoped `compute_initial_states` condemnation closed it). **Wave 2b narrowed that mechanism** (the residual-finding fix: co-ownership scoping) **and added its replacement closure**: the poison-clear survivor re-evaluation promotes the spared parent when the child is removed. Re-hunt under the scoped pair (FailoverEx + Duo, both backends): no violation; the red half (scoping without promotion) re-finds the 9-state strand under TLC — the pairing, not the unscoped condemnation, now closes the strand. Wave-2/2b stage records |

### Non-vacuity witnesses (encoded in 0b, wired as expect-violation checks in 0c; Phase-1 additions marked)

`canReachStamp`, `canReachHoleFromReap`, `canReachHoleFromAdminClear`,
`canReachHoleFromTtlSweep`, `canReachHoleFromRecovery`, `canReachFailFast`,
`canReachRecoveryClear`, `canReachWalkOkConsumed`, `canReachWalkFailConsumed`,
`canReachForgivenResidual` (the CE-23 residual), `canReachStaleIntentApply`,
`canReachD16Cell` (its own predicate as of Phase 1 — no longer an alias of
the L2 val, which is now the armed form; demonstrable post-fix and wired),
`canReachVerificationWalkConsumed` (Phase 1: the WALK_CONSUME_CEIL headroom
pin, review finding FC-5), `canReachWrongfulFfTrigger`,
`canReachTriedDemotionUpstreamHas`, `canReachPrune`, `canReachRollback`,
`canReachCrossTenureWalkConsume`, `canReachDowngradeRespawn` — 19 named
predicates covering the design §4 list of 14 plus the prune/rollback/
cross-tenure/downgrade/ceiling-headroom reachability probes the regimes
need.

## Contradiction / posture records

| Item | Record | Disposition |
|---|---|---|
| Fencing posture for evidence writes (D14/D15) | Entry-time leader gates only; no SQL fence on any evidence write; the only fenced statements are the three attempt-ledger transactions; the MergeDag handler is ungated past the SubmitBuild enqueue guard. Recorded as rationale prose after `sched.evidence.durability`. | **RESOLVED — normative fence implemented Phase 1 Wave 3 (owner decision 2026-05-30, FENCE EVERYTHING / D15 option (b)); model-encoded Wave 4**: every evidence write carries the tenure's serving generation and is applied only at-or-above the durable claims floor; `sched.evidence.durability+2` makes it normative; the model's `pgApplyAny` mirrors it (stale evidence intents discarded), flipping A17/A18 to holds. See the Wave-3 stage record for the statement inventory and residuals, and the Wave-4 stage record for the model flips and wiring. |
| D16 present-but-tried limbo cell | `sched.evidence.settlement` added (owner adopted the obligation); the as-built dispatch probe violated it, so the rule was intentionally uncovered until the Phase-1 fix. | **RESOLVED — fixed Phase 1**: Wave 1 landed the production settlement red-first (rule covered, impl+verify markers); Wave 4 encoded it in the model (probeSettleTried + the consumeWalk/reap settlements), restated L2 in the armed form, and wired the holds check + the D16-cell witness + the c3-no-reprobe regression pin. See the Wave-4 stage record. |

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
encoding) landed in Wave 4 — the next stage record.

### Phase 1 Wave 4 — model flips, calibration sync, A17/L2 CI wiring, raised-budget exhaustive measurements (this stage)

**Headline: every Phase-1 fix is now encoded in the model, every flip is wired in CI, and every
flip is paired with a falsifiability pin.** The Wave-1 settlement (C3+D16) and the Wave-3
claims-floor fence (A17/A18) have model-level mirrors; C3, the L2 armed form, A17 and A18 all
HOLD and sit in the exported conjunctions; each flip's pre-fix behavior is frozen in an
expect-violation calibration override that must keep falsifying — red-first at model level
means the same scopes and budgets that produced each violation pre-fix now produce none, and
the overrides prove they still would against the pre-fix actions. Owner decision 4 (A17/L2
wired before close-out) is delivered through holdsInSim+pin pairs; owner decision 3 (the raised
merge-gate budget) is implemented as the `longChecks` GHA-exclusion mechanism plus the measured
manual-target table below. The four commits: the settlement encoding, the fence encoding, the
longChecks mechanism, this record.

**1. The flips (before → after, with evidence).**

| Property | Before Wave 4 | After Wave 4 | Falsifiability pin |
|---|---|---|---|
| C3 `noWrongfulTerminalFailureSingleTenure` | VIOLATED as-built; expect-violation probe `quint-closure-evidence-probe-wrongful-terminal-failure` (Duo, rust sim, ~1/150K per-trace hit rate, 5M-sample budget); excluded from `asBuiltHoldInvariants` | HOLDS: BaseEx 50K samples, Duo 5M×14 (~80 s, the retired probe's exact scope and budget — the red→green flip evidence); back in `asBuiltHoldInvariants`/`allInvariants`; wired holds check `quint-closure-evidence-settlement-holds` (Duo, 5M×14, paired with the pin) | `quint-closure-calib-c3-no-reprobe`: pre-fix consumeWalk + reapTerminalBuild + probeFailFastTried frozen at Duo constants; falsifies C3 6/6 independent 1M-sample runs (the MCI-2 second-family trace: pre-fix reap fail-fasts a marked+tried Queued survivor whose outputs are obtainable); baseline (production step, same constants) holds C3 at 2M |
| L2 `markedBrokenSettlementArmed` | Expected-fail probe in the 0b STATE form (`not(exists d16Cell)`); never produced — the cell sat ≥16 steps deep (0c deferred-probe record); not in any conjunction | Restated in the ARMED form (every reachable D16 cell is covered by the settlement partition — the guard defs are shared with the settling actions so invariant and actions cannot drift); HOLDS at Duo (2M×14) and C3Duo; in the conjunctions; wired via the same settlement-holds check | Same pin; additionally `markedBrokenSettlementArmedPreFix` (the pre-fix partition coverage, defined in the override) FALSIFIES under the override's baseline (production) step — the pre/post distinguisher: the cell the post-fix model reaches is exactly what the pre-fix partition fails to cover |
| D16 cell reachability `canReachD16Cell` | Unreachable in any hunted budget (the deferred L2 probe; no checker ever assembled the cell) | Reachable at shallow depth via the promotion transient (merge wide / walk fails / consume reverts Queued+tried / out-of-band ingest / prune-merge stamps / cancel / reap holes+promotes → the cell); 12/12 hits at C3Duo and 6/6 at Duo in 1M-sample runs; WIRED as `quint-closure-evidence-witness-d16-cell` (C3Duo, 5M×14) — the settlement's non-vacuity proof | n/a (a witness: its violation IS the pass condition) |
| A18 `leaderClassEvidenceWrites` | VIOLATED as-built; expect-violation probe `quint-closure-evidence-probe-stale-evidence-write` (StaleDuo, rust sim, ~1/2.5K hit rate) | HOLDS at StaleDuo (2M×15, ~44 s); in the conjunctions; wired holds check `quint-closure-evidence-stale-fence-holds` (StaleDuo, 2M×15) | `quint-closure-calib-a17-unfenced`: frozen pre-fence `pgApplyAny` at StaleDuo constants; falsifies A18 in <2 s at 500K samples (the 5-state D14-leg trace); baseline (production fenced apply) holds at 2M |
| A17 `noStaleTenureClearOverride` | Expected-fail probe; never produced (the clear-then-restamp interleaving needs ≥14 steps; 0c deferred-probe record) | HOLDS (the same fence closes it — the latch sits on the apply path that fenced intents never reach); in the conjunctions; wired via the same stale-fence-holds check | Same pin (the A18 falsification is the wired oracle for the shared unfenced-apply mechanism) |
| `canReachStaleIntentApply` | Wired witness (StaleDuo, sim) — the stale-APPLY window | UNREACHABLE in the production model (the fence discards before the apply); check REMOVED and replaced by `quint-closure-evidence-witness-fenced-discard` (`canReachFencedDiscard`, <2 s hit): the same window observed at the fence's discard — A17/A18 hold because the fence fires, not because deposed intents stopped arriving | The a17-unfenced override's frozen apply still sets the old flag (the predicate stays meaningful as the override's oracle) |
| `canReachVerificationWalkConsumed` (new) | n/a (the predicate did not exist) | The WALK_CONSUME_CEIL headroom witness (review finding FC-5): a second walk consumption on one node is reachable (~1/70K at C3Duo); WIRED as `quint-closure-evidence-witness-verification-walk` — if a future ceiling edit drops a holds-regime ceiling back to 1, the settlement can spawn but never complete and this check goes red | n/a |

**2. Model changes (closureEvidence.qnt; the introducing commits carry the per-edit rationale).**

- Settlement: `probeSettleTried` (new dispatch-partition settlement action), `probeFailFastTried`
  narrowed to confirmed-unobtainable, the consumeWalk Broken-arm settlement branch
  (untried+obtainable → spend the one-shot + verification respawn; the model's atomic respawn
  abstracts the code's revert-for-respawn, CF-1/FC-1), the reapTerminalBuild survivor settlement
  with the MQueued exclusion (exact code mirror — review finding MCI-2: without it the model
  wrongfully fail-fasts the Queued+tried survivor the code promotes-and-completes). Shared
  settlement-arm guard defs (`settleArmAvailable` / `failFastArmConfirmedMissing` /
  `settlementEnabled`) couple the actions to the L2 armed form structurally.
- Fence: `pgApplyAny` discards stale evidence intents — zero new state variables (review finding
  MCI-4): `lead.gen` IS the durable claims floor at this abstraction because `recoverAsLeader`
  claims atomically with recovery; the code earns the same equivalence via the Wave-3 T-3.1
  claim-before-recovery reorder (the model cannot represent the unfixed ordering, which is why
  that reorder is the fence's production prerequisite).
- Ceilings (review finding FC-5): WALK_CONSUME_CEIL 2→3 at
  BaseEx/FaultPersistEx/FailoverEx/Duo/StaleDuo/AdversarialStoreEx and 1→2 at C3Duo — the
  settlement adds one verification-walk consumption per arming; design-scale regimes stay at 2
  (original + verification fits; the third slot is only needed where a downgrade re-spawn can
  also occur within the trace budget).
- New Reach fields `secondWalkConsumed` / `fencedDiscard` (record fields, not state variables —
  every frozen calibration copy constructs `reach'` by spread, so all 17 override modules stay
  valid without edits).
- Conjunctions: `asBuiltHoldInvariants` is again the FULL set (identical to `allInvariants`,
  now with C3 + L2-armed + A17 + A18); new `asBuiltHoldInvariantsAdversarialStore` (minus B9,
  the documented GC-after-produce trip) for the adversarial-store measurement.

**3. Calibration sync (RT-5 same-commit discipline; no frozen copy retro-fitted).**

- New override modules: `closure-c3-no-reprobe.qnt` (three pre-fix actions, Duo constants),
  `closure-a17-unfenced.qnt` (frozen pre-fence apply, StaleDuo constants).
- Seven existing modules' `calibStepDrv` alphabets gain `probeSettleTried` so each stays
  "production minus exactly the frozen action(s)": f2, f3-indet, f3-subst, f6, f8, f9, f13. The
  five modules that reference production `stepDrv`/`stepBuild` wholesale (f1-stale, f1-skip,
  f4-demand, f4-vacuous, f7) and the two recoverAsLeader-copying modules (f10, f14) needed no
  edit.
- Frozen-copy BODIES are intentionally not retro-fitted with the settlement/fence (the f10/f14
  precedent from the Wave-2 plan): each copy freezes the consumeWalk/poisonClear/pgApplyAny of
  the era whose defect it documents. f6's header records that its frozen consumeWalk predates
  the Phase-1 settlement and why that is intentional.
- Scope deviation from the plan: the C3 pin and the settlement-holds check run at DUO constants,
  not C3Duo. The C3Duo restriction (poison/attempts off, extra intent slot) was designed to
  cheapen TLC BFS (the post-0d triage's purpose for that module); under the rust simulator it
  DILUTES the per-trace hit rate by roughly an order of magnitude — measured this wave: the
  pre-fix C3 falsification hits 6/6 at 1M samples at Duo constants vs ~1-in-2-to-5M at C3Duo
  constants; the D16-cell witness hits 12/12 at 1M at C3Duo (where the cell needs no poison)
  but the C3 violation needs the wider alphabet. Wiring follows the measured hit rates, not the
  plan's scope guess.

**4. Calibration re-validation (T-4.4) — every falsification still falsifies, every baseline holds.**

Wired checks: all 29 `quint-closure-*` checks built green TWICE (after the settlement commit,
again after the fence commit) — including the six 0d calibration checks, every witness, the
B2-strong probe, and the four new Phase-1 entries. Unwired evidence modules, re-run via their
header commands against the both-flips model:

| Module | Property | Verdict (Wave-4 model) |
|---|---|---|
| f1-skip-store-recheck | C4 | still falsifies — TLC, 3.2 s, 171 distinct states (16 workers) |
| f3-indet-failfast | B3 | still falsifies — TLC, 41 s, 77 K distinct (16 workers) |
| f3-substitutable-demoted | C2 | still falsifies — TLC, 38 s, 73 K distinct (16 workers) |
| f4-vacuous-prune | B4 | still falsifies — TLC, 1.7 s, 138 distinct (16 workers) |
| f5-wanted-overwrite | B5 | still falsifies — TLC, 40 s, 53 K distinct (16 workers) |
| f6-latch-outlives-chain | A21 | still falsifies — rust sim, 1.4 s (100 K × 12) |
| f10-recovery-vouch-unscoped | A3 | still falsifies — TLC, 567 s, 4.73 M distinct (60 workers; consistent with its 0d depth-17 / 5.15 M / ~8.4 min-at-192-workers record) |
| f13-unprobed-dispatch | B8 | still falsifies — TLC, 39 s, 72 K distinct (16 workers) |
| f14-recovery-keeps-substituting | L1 | still falsifies — TLC, 15 s, 21 K distinct (16 workers) |

Stop-and-report conditions 3 (a calibration stops falsifying) and 4 (a wired witness becomes
unreachable, beyond the three planned flips) did not trigger.

**5. Exhaustive measurements at the raised budget (T-4.5).**

Conditions: 60 TLC workers per run, 35-minute wall-clock cap, -Xmx48G heap, the 192-core
reference builder (the same host class as the 0c/0d/Wave-2/2b records), quint 0.32.0 /
Apalache 0.56.1, dedicated warm apalache-server per run. Invariant: `asBuiltHoldInvariants`
(the full post-flip conjunction) except where noted.

| # | Conjunction × scope | Pre-fix reference (0c/0d/W2b records) | Post-fix measurement (this wave) | Verdict / Tier |
|---|---|---|---|---|
| 1 | `closureEvidenceBaseEx` × asBuiltHoldInvariants | 0c closing run: 16.4 M distinct / 70.1 M generated / depth 21 / ~15.5 min, unconverged (C3 excluded; full-width workers) | NO violation; stopped unconverged at the 35-min cap: 31.46 M distinct / 142.96 M generated / Progress(16), queue 10.94 M still growing | Tier 3 — manual target (post-fix prefix is ~1.9× the 0c pre-fix exploration with zero violations) |
| 2 | `closureEvidenceFailoverEx` × asBuiltHoldInvariants (FULL, L3 included) | W2b re-hunt: 23.7 M distinct / 107.7 M generated / Progress(14) at 35-min cap, unconverged (60 workers) | NO violation; stopped unconverged at the 35-min cap: 18.80 M distinct / 84.69 M generated / Progress(14), queue 10.05 M still growing | Tier 3 — manual target (the full conjunction, L3 included, stays violation-free over a prefix comparable to the Wave-2b re-hunt) |
| 3 | `closureEvidenceFaultPersistEx` × asBuiltHoldInvariants | never measured | NO violation; stopped unconverged at the 35-min cap: 25.51 M distinct / 114.84 M generated / Progress(15), queue 10.49 M still growing | Tier 3 — manual target (first-ever measurement of this scope) |
| 4 | `closureEvidenceDuo` × asBuiltHoldInvariants | 0d/post-0d: C3 alone 540 K distinct / ~15 min unconverged (60 workers) | NO violation; unconverged at the 35-min cap: 889,030 distinct / 6,429,992 generated / Progress(8), queue 735,964 still growing (60 workers) | Tier 3 — manual target |
| 5 | `closureEvidenceC3Duo` × asBuiltHoldInvariants | post-0d: C3 alone 573 K distinct / 4.0 M generated / ~15 min unconverged (60 workers; pre-raise ceiling) | NO violation; unconverged at the 35-min cap: 1,215,490 distinct / 8,404,107 generated / Progress(8), queue 954,546 still growing (60 workers) | Tier 3 — manual target |
| 6 | `closureEvidenceStaleDuo` × A17+A18 (`leaderClassEvidenceWrites,noStaleTenureClearOverride`) | post-0d: pre-fence A18 violation found at 776 K distinct / 5.1 M generated / 14.8 min full-step at 100 workers | NO violation; unconverged at the 35-min cap: 1,260,071 distinct / 7,921,461 generated / Progress(8), queue 1,054,556 still growing (60 workers) | Tier 3 — manual target |
| 7 | `closureEvidenceAdversarialStoreEx` × asBuiltHoldInvariantsAdversarialStore | B2-strong probe (expect-violation): depth 7 / ~40 s; the hold conjunction never measured | NO violation; unconverged at the 35-min cap: 4,801,915 distinct / 20,431,752 generated / Progress(11), queue 3,206,783 still growing (60 workers) | Tier 3 — manual target |

Verdicts and tier assignment (adjudication OQ6's three-tier rule):

| Tier (OQ6) | Definition | Members after this campaign |
|---|---|---|
| Tier 1 — GHA-swept | exhaustive TLC conjunction converging ≤ ~5 min at 60 workers → wired `mkQuintCheck` with `workers` pinned | none of the seven candidates qualified |
| Tier 2 — [LONG], local gate only | converging in 5–30 min → wired into `checks.*` and named in `longChecks` (excluded from the GHA formal matrix) | none qualified — the `longChecks` list ships empty (the mechanism is implemented, verified, and dormant) |
| Tier 3 — documented manual target | not converged at 30 min / 60 workers | ALL SEVEN candidates (table above); each remains re-runnable via the manual-target commands recorded at the nix/quint.nix closure-evidence section comments, now against the post-flip conjunctions |

Decision-4 note: the A17/L2 GHA-wired deliverables do not depend on these verdicts — the
holdsInSim+pin pairs are wired regardless (the OQ6 fallback chain: "the holdsInSim pair IS the
deliverable; TLC conjunctions are additive when they fit a tier"). Decision-3 note: the raised
15–30-minute local budget is implemented (the mechanism + this measurement record); what the
budget could not buy at this state-space scale is convergence — the bounded-prefix evidence
(every conjunction explored 3–20+ M distinct states with zero violations) is the merge gate's
exhaustive posture until Phase 2 or a Track E bare-metal lane changes the budget class.

Measurement-conditions note: runs 1–3 (BaseEx/FailoverEx/FaultPersistEx) ran as a clean
3-concurrent batch (180 threads / 192 cores). Runs 4–7 (Duo/C3Duo/StaleDuo/AdversarialStoreEx)
ran as a 4-concurrent batch (240 threads / 192 cores, ~1.25× thread oversubscription) — their
figures are conservative lower bounds on a contention-free 35-minute exploration; the verdicts
(unconverged, no violation) are unaffected by the contention.

No measurement reported a violation: stop-and-report condition 7 (a violation other than the
expected flips) did not trigger — the post-fix conjunctions hold over every explored prefix.

Pre-excluded from measurement (review finding SD-4, recorded for the decision-3 close-out): the
five design-scale regime conjunctions (closureEvidence{Base,FaultPersist,Failover,StaleTenure,
AdversarialStore} × their conjunctions), on the recorded 0b/0c evidence — slice non-convergence
and 40+ minutes to BFS depth 2–3 at design scale. They remain documented manual targets;
re-opening them is an owner decision, not a budget increment.

**6. Wired-check inventory delta (25 → 29; all Tier 1 / GHA-swept).**

| Change | Check | Replaces / replaced by |
|---|---|---|
| REMOVED | quint-closure-evidence-probe-wrongful-terminal-failure (C3 expect-violation, Duo sim) | replaced by the settlement flip set below |
| REMOVED | quint-closure-evidence-probe-stale-evidence-write (A18 expect-violation, StaleDuo sim) | replaced by the fence flip set below |
| REMOVED | quint-closure-evidence-witness-stale-intent-apply (StaleDuo sim) | replaced by witness-fenced-discard (the flag is unreachable post-fence) |
| NEW | quint-closure-calib-c3-no-reprobe (expect-violation pin, Duo constants, 5M×14) | the C3/L2 falsifiability half |
| NEW | quint-closure-evidence-settlement-holds (holdsInSim: C3 + L2-armed, Duo, 5M×14) | the C3/L2 holds half; carries r[verify sched.evidence.settlement] + r[verify sched.merge.substitute-topdown+12] |
| NEW | quint-closure-evidence-witness-d16-cell (C3Duo, 5M×14) | the L2 armed form's non-vacuity (the cell is reachable) |
| NEW | quint-closure-evidence-witness-verification-walk (C3Duo, 2M×14) | the FC-5 ceiling-headroom pin |
| NEW | quint-closure-calib-a17-unfenced (expect-violation pin, StaleDuo constants, 500K×15) | the A17/A18 falsifiability half |
| NEW | quint-closure-evidence-stale-fence-holds (holdsInSim: A17 + A18, StaleDuo, 2M×15) | the A17/A18 holds half; carries r[verify sched.evidence.durability+2] |
| NEW | quint-closure-evidence-witness-fenced-discard (StaleDuo, 500K×15) | the fence's non-vacuity (deposed intents still reach the apply site) |
| unchanged | the remaining 22 checks (10 BaseEx TLC witnesses, downgrade-respawn, hole-reap, hole-recovery, recovery-clear, cross-tenure-walk, B2-strong probe, 6 calib checks) | rebuilt green against the Wave-4 model |

Harness additions: `mkQuintSimHoldsCheck` (the bounded-simulation holds constructor — the dual
of mkQuintSimWitnessCheck; every instance must name its falsifiability pin), the optional
`workers` argument on mkQuintCheck/mkQuintWitnessCheck (MCI-6 worker-count pinning; the null
default renders byte-identical scripts, verified no existing check rehashes), and the
`longChecks` export + flake.nix `removeAttrs` exclusion (the Tier-2 mechanism — currently an
empty list, see §5; verified end-to-end with a temporary entry: the named check leaves
ciMatrix.formal and ciMatrix.checks while staying in checks.*).

GHA-pod runtime caveat (recorded for the integration gate): the "Tier 1 / GHA-swept"
assignment of the sim-backed checks follows the post-0d precedent (the retired 5M-sample C3
probe occupied the same cost class), but no closure-evidence check has yet RUN in the GHA
formal lane — the campaign lives on `formal-sprint`, which has not been pushed — so the
per-check pod wall-clock at rio-ci vCPU counts is unmeasured. If a sim check exceeds the lane
budget at integration time, the `longChecks` mechanism is the designated relief valve (move the
check's name into the list — no flake.nix edit needed); the reference-builder wall-clocks of
every wired check are in the introducing commits.

**7. Deviations from the plan (recorded per the Wave-1 correction precedent).**

1. The C3 pin and settlement-holds check run at Duo constants, not C3Duo (§3; measured hit-rate
   justification).
2. The holds checks landed in the same commits as their model flips (T-4.1/T-4.3) rather than in
   a separate T-4.6 commit — the RT-5 self-contained-commit discipline takes precedence over the
   plan's task-to-commit map. T-4.6's surviving separate content (the `workers` argument) landed
   with T-4.7's mechanism commit.
3. `mkQuintSimHoldsCheck` conjoins invariants with `" and "` (a quint expression) rather than
   `","` — `quint run --invariant` rejects the comma form (`quint verify` accepts it).
4. T-4.2 (the L3 model fix + pin) did not execute — Wave 2 took outcome B (L3 refuted as a
   production defect) and Wave 2b's scoping+promotion pair already carries the L3 closure; the
   FailoverEx measurement below runs the FULL conjunction (L3 included) per the Wave-2b record.
5. The T-4.4 evidence-module re-runs used 16 TLC workers for the eight shallow falsifications
   (they complete in seconds-to-a-minute; the worker count is irrelevant at that depth) and 60
   for the deep f10 run; the 0d records they are compared against used 8–192 workers.

**8. Updated owner sign-off items (supersedes the Wave-2b list; renumbered).**

1. The C5 / CE-7 deferral — unchanged (0d item; Phase 2).
2. The F6 falsifications resting on the rust simulator — unchanged.
3. The CE-45 (F10) evidence-module-only falsification — unchanged (re-validated this wave:
   567 s at 60 workers).
4. ~~The mkQuintWitnessCheck rust-backend extension funding question~~ — superseded: the
   sim-witness constructor exists since the post-0d triage and Wave 4 added its holds dual;
   both are in production use with five wired instances.
5. ~~A17/L2 wiring + the raised-budget exhaustive wiring (decisions 3+4)~~ — DELIVERED by this
   stage: the holds+pin pairs are wired (decision 4), the exhaustive conjunctions are measured
   at the raised budget and the `longChecks` mechanism implements the OQ6 adjudication
   (decision 3). Residual for close-out sign-off: the Tier-3 manual-target table (§5) — every
   exhaustive conjunction remains unconverged at 30 minutes / 60 workers, so the merge gate's
   exhaustive evidence stays bounded-prefix rather than converged; the owner counter-signs this
   as the accepted decision-3 residual, or commissions the Track E bare-metal formal-long lane
   (out of Phase-1 scope per OQ6).
6. The 0c carry-overs — unchanged: the design-scale regimes (§5 pre-exclusion) and the
   C1-strict probe.

## Phase-1 close-out — defect dispositions, deployment deltas, Phase-2 handoff (Wave 5)

Phase 1 of the closure-evidence campaign is complete: 18 commits on the `ce-phase1`
worktree (Waves 1, 2, 2b, 3, 4 and this close-out), red-first at every production
behavior change, every commit boundary green for clippy (stable) / rio-scheduler
nextest / tracey-validate / treefmt / the wired quint battery. The branch is handed to
integration only behind a green full gate (`/nixbuild --checks`); the gate run record
and the rebase-onto-`formal-sprint` notes live with the orchestrator. The campaign's
defect classes are fixed or refuted-with-record; the model, the wired checks and the
spec rules now describe the FIXED system; and every flip is paired with a permanent
expect-violation pin so each pre-fix defect class stays re-discoverable by CI.

### Defect dispositions (final)

| Defect | Found | Final disposition | Commits |
|---|---|---|---|
| **C3** — wrongful terminal failure on a stale walk verdict (`noWrongfulTerminalFailureSingleTenure`) | 0c model trace; adjudicated CONFIRMED as-built at 0d (two-build variant) | **FIXED red-first, Wave 1** — the settlement re-probe at all three production fail-fast sites (the dispatch-probe partition, the `handle_substitute_complete` Broken arm, the reap-survivor hook); spec amended to `sched.merge.substitute-topdown+12`. **Model flip HOLDS, Wave 4** (BaseEx + Duo at the retired probe's exact scope and budget). | `7c2d8ea31`, `2351a35ba`, `7b4a2e2d2`, `ae615fc49` (production + spec); `7e776cb0f` (model flip + wiring) |
| **D16** — present-but-tried limbo cell | A1/A2 design window; settlement obligation owner-adopted 2026-05-29 | **FIXED, Wave 1** under the adopted obligation — `sched.evidence.settlement` implemented at the dispatch-probe partition (the same settlement decision as C3); the rule left `tracey query uncovered` at Wave 1 and stays covered (impl + verify). The L2 armed form + the now-demonstrable D16-cell witness are the model-side proof (Wave 4). | `7c2d8ea31`, `ae615fc49`; `7e776cb0f` (L2 armed form + `witness-d16-cell`) |
| **L3** — failover + ClearPoison strands a parent under a live build | post-0d triage (the FailoverEx conjunction violation) | **REFUTED as a production defect, Wave 2** — a model artifact of the recovery-condemnation faithfulness gap (review finding RT-2): the corrected model cannot reach the strand (re-hunt explored ~9.4× the violation's coordinates with no violation, while the same setup re-finds it on the pre-correction model in 8 minutes), and the production code walk closes every path to the strand state. No production fix was needed for L3 as premised. | `55dd15105` |
| **L3-residual** — production's unscoped recovery condemnation (the C3-class wrongful failure at the recovery decision point that the L3 refutation exposed; spec-vs-code divergence against `sched.recovery.failed-dep-cascade+2`) | Wave-2 stage record §5 | **FIXED red-first, Wave 2b** (owner disposition (b), 2026-05-30) — co-ownership scoping of the in-DAG recompute AND poison-clear survivor re-evaluation, both halves together; new rule `sched.poison.clear-survivor-reevaluation`. The red half (scoping without promotion) re-finds the 9-state strand under TLC — the recorded proof the pairing is load-bearing. | `7750a4d45` (production + spec), `e56a1b73c` (model) |
| **D14/D15** — the deposed-believer evidence-write windows (entry-time gates only; no SQL fence on any evidence write) | A1 inventory HIGH findings; FENCE EVERYTHING owner-ratified 2026-05-30 | **CLOSED, Wave 3** — uniform claims-floor fence on every production evidence write (10 statements + the merge transaction + 8 owning transactions; tenure-tracking `serving_generation` capture; no migration); `sched.evidence.durability+2` makes the fence normative. **The A17/A18 model flips prove it, Wave 4**: both HOLD post-fence, and the `a17-unfenced` pin re-finds the pre-fence violation in <2 s. | `498db7410`, `889a575e6`, `55a1fa6cb`, `865b85dbe`, `26ad50144`, `c47de3ccc` (production + spec); `f004ca600` (model flips + wiring) |

D17 (the in-memory-only Substituting reset) keeps its Phase-0 accepted-with-rationale
disposition — no Phase-1 change.

### Property-table final state

Properties that flipped to HOLDS during Phase 1 (none of these held pre-fix), each with
its permanent falsifiability pin:

| Property | Pre-Phase-1 | Post-Phase-1 | Permanent pin |
|---|---|---|---|
| C3 `noWrongfulTerminalFailureSingleTenure` | VIOLATED as-built (wired expect-violation probe) | HOLDS (BaseEx 50 K, Duo 5 M×14); back in `asBuiltHoldInvariants`/`allInvariants`; wired `quint-closure-evidence-settlement-holds` | `quint-closure-calib-c3-no-reprobe` |
| L2 `markedBrokenSettlementArmed` (armed form) | 0b state form never producible; probe deferred at 0c | HOLDS (Duo, C3Duo); wired via the same settlement-holds check; the cell's reachability is `quint-closure-evidence-witness-d16-cell` | same pin (`markedBrokenSettlementArmedPreFix` falsifies under the override) |
| A17 `noStaleTenureClearOverride` | Expect-fail probe, never produced (deferred at 0c) | HOLDS (StaleDuo); wired `quint-closure-evidence-stale-fence-holds` | `quint-closure-calib-a17-unfenced` |
| A18 `leaderClassEvidenceWrites` | VIOLATED as-built (wired expect-violation probe) | HOLDS (StaleDuo 2 M×15); wired via the same stale-fence-holds check | same pin |
| L3 `liveBuildTerminalOrProgressArmed` | VIOLATED in the 0d model (FailoverEx) | No violation under the Wave-2-corrected + Wave-2b-scoped model (both backends; 21–24 M-state TLC prefixes); the survivor-promotion pairing is owned by `sched.poison.clear-survivor-reevaluation` | not a wired pin — the strand class needs TLC depth; the Wave-2b red half (scoping without promotion) is the recorded falsification setup |

Every other Group A/B/C/L row keeps its 0c verdict (no Phase-1 change was needed; the
full battery re-validated green at every model edit — Waves 2, 2b, 4). The witness set
is 19 named predicates, all reachable: `canReachStaleIntentApply` is retired
(unreachable post-fence, by design — replaced by `canReachFencedDiscard`), and
`canReachD16Cell` / `canReachVerificationWalkConsumed` are the two Phase-1 additions.

### Wired-check inventory: 25 → 29 (all Tier 1 / GHA-swept)

Three checks removed (each replaced in the same commit as the flip that made it
obsolete), seven added; the full delta table is the Wave-4 stage record §6:

- REMOVED: the C3 expect-violation probe (`probe-wrongful-terminal-failure`), the A18
  expect-violation probe (`probe-stale-evidence-write`), the stale-intent-apply witness
  (its flag is unreachable post-fence).
- ADDED: two holdsInSim checks (`settlement-holds`, `stale-fence-holds` — the decision-4
  C3/L2/A17/A18 wiring), two expect-violation regression pins (`calib-c3-no-reprobe`,
  `calib-a17-unfenced`), three witnesses (`witness-d16-cell`,
  `witness-verification-walk`, `witness-fenced-discard`).
- Harness additions: `mkQuintSimHoldsCheck` (every instance must name its
  falsifiability pin), the optional `workers` argument (worker-count pinning for any
  future wired TLC check), and the `longChecks` Tier-2 exclusion mechanism.

### Exhaustive coverage — the honest statement

**No exhaustive conjunction converges at the raised budget.** All seven post-fix
conjunctions (BaseEx, FailoverEx, FaultPersistEx, Duo, C3Duo, StaleDuo,
AdversarialStoreEx) were measured at 35 minutes / 60 TLC workers / 48 G heap on the
192-core reference builder (Wave-4 stage record §5): every run stops unconverged at the
cap with **zero violations** over prefixes of 0.9 M – 31.5 M distinct states, each
strictly larger than the prefix where its scope's pre-fix violation (if any) was found.
The five design-scale regimes stay pre-excluded on the recorded 0b/0c evidence (review
finding SD-4). Stated plainly:

- The campaign's exhaustive evidence is **bounded-prefix, not converged** — "no
  violation found within N states", never "property proved over the scope".
- All seven conjunctions are **documented manual targets**: command shapes in the
  `nix/quint.nix` closure-evidence section comments, recorded coordinates in the Wave-4
  measurement table, deployment-time runbook in checklist row CE-D6 below.
- The `longChecks` Tier-2 mechanism (owner decision 3's wiring) **exists, is verified
  end-to-end, and ships with an empty list** — empty by physics, not by omission:
  nothing fits the 5–30-minute convergence window. A future conjunction that converges
  (smaller scope, better reduction, bigger budget class) wires in by adding one name to
  one list.

### Acceptance-table deltas (the 0d rows Phase 1 changes)

The Phase-0d acceptance table is a historical record and is not edited in place; these
deltas supersede the named rows:

| 0d row | 0d verdict | Phase-1 delta |
|---|---|---|
| C-group | "C3 is the confirmed as-built finding (cannot serve as a calibration baseline; Phase-1 fix candidate)" | C3 is **fixed and verified**: red tests at three sites (Wave 1, `7c2d8ea31`/`2351a35ba`/`7b4a2e2d2`), model HOLDS (Wave 4, `7e776cb0f`), regression-pinned by `quint-closure-calib-c3-no-reprobe`. C3 now also serves as a calibration baseline (the pin's baseline run holds C3 at 2 M samples). C1/C2/C4 unchanged; C5 deferral unchanged (Phase 2). |
| F9 (hole stamping on poison-clear) | MET (re-routed rep) | **Phase-1 interaction re-validated**: Wave 2b (`7750a4d45`/`e56a1b73c`) adds the survivor-promotion arm to production poison-clear and the model `poisonClear`; `quint-closure-calib-f9-poison-clear-no-stamp` stays green and its falsification stays attributable (re-validated after Waves 2, 2b, and 4). |
| F14 (recovery / Substituting) | MET (re-routed shape) | **Phase-1 interaction re-validated**: Waves 2/2b (`55dd15105`/`e56a1b73c`) change the model `recoverAsLeader` condemnation arms; the f14 evidence module still falsifies L1 (Wave-4 T-4.4: TLC, 15 s) and calib-f8 (which consumes production `recoverAsLeader`) stays green at every model edit. |
| F11 (leader gating / stale tenure) | MET by regime comparison (A18 falsified on the rust simulator only; the TLC-backend discrepancy kept it un-wired) | **Superseded by the Wave-3/4 fence work**: the falsification direction is now permanently wired (`quint-closure-calib-a17-unfenced`, expect-violation, <2 s) and the production direction HOLDS as a wired check (`quint-closure-evidence-stale-fence-holds`, `f004ca600`). The F11 evidence no longer rests on an unwired simulator record. |

Every other family row (F1–F8, F10, F12, F13, F15–F17) is unchanged by Phase 1; their
representatives were re-validated against the post-fix model in the Wave-4 T-4.4 sweep
(every falsification still falsifies, every baseline still holds).

### Deployment-checklist deltas (the operator handoff for the one-time deployment)

The closure-evidence rows handed to the deployment-time checklist (the house D0–D7
pattern; CE-D1…CE-D6 are new or changed by Phase 1, CE-D7…CE-D9 are Phase-0 residuals
carried unchanged):

| ID | Item | What changes / what to do | Source |
|---|---|---|---|
| CE-D1 | **Fenced-write metric semantics + the leader alert** | `rio_scheduler_evidence_write_fenced_total` counts evidence writes refused by the claims-floor fence. On a replica that just lost the lease, nonzero during failover IS the fence working — deposed-leader evidence writes are now no-ops (pre-Phase-1 they landed; that was the D14/D15 window). On the CURRENT leader it must be ZERO: **alert on any sustained nonzero rate on the leader** (it means a capture bug or a PG floor regression). | Wave 3 |
| CE-D2 | **Failover-time PG-flap alerts** | `rio_scheduler_generation_claim_failed_total` and `rio_scheduler_generation_floor_read_failed_total`: sustained nonzero means PG is flapping exactly at failover time and the affected term serves unclaimed — the deposed-before-persist collision window re-opens for that term. Alert and investigate PG availability at failover. | Wave 3 |
| CE-D3 | **Merge `FAILED_PRECONDITION` during failover** | A SubmitBuild merge racing a failover can now surface gRPC `FAILED_PRECONDITION` (`StaleGeneration`) instead of silently landing the deposed leader's merge. Guidance: re-submit against the current leader (the normal client retry path); this is the fence refusing a stale-tenure write, not a data fault. | Wave 3 |
| CE-D4 | **The wrongful-fail-fast class is gone; resubmit guidance changes** | Pre-Phase-1, a build could terminally fail with the resubmit-directing error while its wanted outputs were present or substitutable (the C3 class). Post-Phase-1, every fail-fast decision point re-probes live obtainability first, so a fail-fast now means a CONFIRMED missing-and-unsubstitutable output or a failed verification walk. Resubmit-after-fail-fast is no longer "try again, it may have been spurious": without an upstream/store change, a resubmitted build reaches the same (now genuine) verdict. Residual (FC-7): wrongful failures are bounded to ≤1 per (node, arming) and eliminated only in the no-failover / no-store-fault / no-withdrawal envelope; the verdict is per-node (a node with genuinely unobtainable wanted outputs still fails every interested build). | Wave 1 |
| CE-D5 | **Poison-clear wakes spared parents** | Admin ClearPoison and the poison-TTL sweep now re-evaluate surviving parents (promotion / settlement) instead of leaving them parked, and recovery condemnation is co-ownership-scoped (a build no longer fails at recovery because ANOTHER build's poisoned child sits under its parent). Operationally: clearing a poison can immediately dispatch waiting work — expect build progress after a clear, not just state cleanup. | Wave 2b |
| CE-D6 | **Manual-target runbook (pre-deployment formal sweep)** | Before the one-time deployment — and after any future change to scheduler evidence handling — run the seven exhaustive conjunctions manually at the largest affordable budget: `quint verify --backend=tlc --main=closureEvidence<Scope> --invariant=asBuiltHoldInvariants docs/spec/models/closureEvidence.qnt` for Scope ∈ {BaseEx, FailoverEx, FaultPersistEx, Duo, C3Duo}; AdversarialStoreEx uses `asBuiltHoldInvariantsAdversarialStore`; StaleDuo uses the A17+A18 invariant pair. TLC workers sized to the host. The Wave-4 §5 coordinates (35 min / 60 workers) are the floor to beat; zero violations at-or-past those coordinates is the deployment-time formal posture. | Wave 4 |
| CE-D7 | KEEP: the AW1 lost-hole-stamp ∩ builds-row-purge bound | Unchanged Phase-0 accepted bound. | 0c |
| CE-D8 | KEEP: the GC-after-vouch bounds | Unchanged (B2-strong stays an expect-violation probe; pin-at-vouch is deferred to the A3 substitution replacement design). | 0c |
| CE-D9 | KEEP: the D10 expired-at-load poison residual | Unchanged. | 0a |

> **Successor cross-reference (substitution-replacement Phase B, 2026-06).**
> The rows above are this campaign's record as written and are not edited.
> The substitution-replacement campaign's Phase B (flag-on cutover)
> re-verified CE-D1..D5 against the materialization path and recorded the
> dispositions — including the CE-D1 alert that now ships in the chart
> (`RioSchedulerEvidenceWriteFenced`), the CE-D4 fail-fast site's flag-on
> successor (the §2.4 consumption settlement), and the CE-D8 evidence
> delivered by pin-at-ingest — in
> `substitution-replacement-invariant-map.md` § "Phase B deployment
> checklist (Wave 6, T-6.3)", together with the new materialization rows
> (MD-D1..D4). CE-D6's conjunction re-run is deferred to that campaign's
> Phase C′ go/no-go.

> **Successor cross-reference (substitution-replacement Phase D′,
> 2026-06-02).** The rows above remain this campaign's record as written.
> Phase D′ (the deletion phase) removed the verification subject: the
> walk, the `topdown_pruned`/`closure_hole` columns (migration 080), the
> `Substituting` status and the coexistence flag are deleted; the
> store-owned materialization job is the only substitution mechanism.
> Dispositions of the rows that referenced this campaign's machinery:
> **CE-D7 closes vacuous-by-construction** (the lost-hole-stamp ∩
> builds-row-purge bound has no hole to lose — durable-relation
> classification at decision time replaced the breadcrumb);
> **CE-D8 splits per the D′ plan §5.4**: shape (a) — GC between vouch and
> use — is delivered by pin-at-ingest
> (`pinCoversIngestUntilAllInterestTerminal`, wired, with the
> mat-b2-no-pin calibration re-finding the GC trace), and shape (b) — the
> stale-Produced direction — narrows into the kept B9 guard
> (`quint-closure-calib-f1-stale-produced`, still wired); **CE-D9 is
> unchanged** (the expired-at-load poison residual survives, re-homed to
> the surviving recovery-time fenced write). The wired check family here
> shrank to the survivors core (A14/A15/A22/B9/B10/L3 over the
> post-deletion alphabet) plus the two kept pins; the 27 retired checks'
> dispositions and the model's archive plan (A6, post-soak) are in
> `substitution-replacement-invariant-map.md` § "Phase D′ stage record".

### Owner-decision provenance

Every owner decision this campaign executed under, with date and Phase-1 outcome:

| Date | Decision (owner) | Phase-1 outcome |
|---|---|---|
| 2026-05-29 (design checkpoint) | D16 settlement obligation ADOPTED (normative MUST at 0a, red-first fix in Phase 1); fencing decision deferred until post-0c evidence; GC-after-vouch accepted as a known bound; calibration target = the formal-sprint tip | `sched.evidence.settlement` added at 0a (intentionally uncovered); FIXED Wave 1; covered (impl + verify) since. |
| 2026-05-30 (Phase-0 checkpoint, decision 1) | Fix BOTH C3 and L3, red-first, model traces as oracles | C3: FIXED Wave 1. L3: the oracle itself was found defective (review finding RT-2), so Wave 2 repaired the model first; the re-derivation REFUTED L3 as premised, and the refuted premise went back to the owner instead of being silently re-scoped — the oracle-discipline reading of decision 1. The residual it exposed became the Wave-2b fix (next row). |
| 2026-05-30 (Wave-2b checkpoint) | The Wave-2 residual finding takes disposition (b): spec-conformance fix, both halves together (co-ownership scoping AND poison-clear survivor re-evaluation) | FIXED Wave 2b red-first; new rule `sched.poison.clear-survivor-reevaluation`; the red-half TLC run (scoping without promotion re-finds the strand) is the recorded proof the halves are jointly load-bearing. |
| 2026-05-30 (Phase-0 checkpoint, decision 2) | FENCE EVERYTHING — uniform claims-floor on every evidence write (~10 statements), not just the merge tx + W2/W5 | Implemented Wave 3: 10 statements + the merge tx + 8 owning transactions (the execution found 5 more owning transactions than the plan's enumeration — the Phase-1b failure-classification handlers; identical fence pattern). A17/A18 flipped to HOLDS, Wave 4. |
| 2026-05-30 (Phase-0 checkpoint, decision 3) | RAISE THE MERGE-GATE BUDGET — 15–30 min checks allowed; wire every exhaustive conjunction that converges at 30 min; the rest stay documented manual targets | **The "wire every converging conjunction" set is empty — by physics, not by omission.** All seven candidates were measured at 35 min / 60 workers; none converges (Wave-4 §5). The pre-registered contingency (adjudication OQ6) applied: the `longChecks` Tier-2 mechanism is implemented, verified end-to-end, and ships empty; all seven conjunctions are documented manual targets with zero-violation bounded-prefix records. The owner counter-signs this as the accepted decision-3 residual, or commissions the Track E bare-metal formal-long lane. |
| 2026-05-30 (Phase-0 checkpoint, decision 4) | A17 and L2 MUST be wired CI checks before close-out (long-budget TLC, or the rust-simulator constructor) | Delivered Wave 4 via the rust-simulator route the decision's text authorizes: holdsInSim + expect-violation-pin pairs (`stale-fence-holds` + `calib-a17-unfenced` for A17/A18; `settlement-holds` + `calib-c3-no-reprobe` for C3/L2). **L2 is wired in the ARMED form** — an orchestrator call within decision-4 intent (the 0b state form is unsatisfiable under the owner's own chosen fix design — findings RT-3/MCI-1/FC-3); flagged for owner counter-signature. **Counter-signed: owner, 2026-06-01 (round-2 final decision gate; signature line applied at the substitution-replacement A6 close-out).** |

Orchestrator calls made within owner-decision intent, flagged for counter-signature at
the Phase-2 / campaign close-out: (1) the L2 armed form (above); (2) the Wave-1 battery
enumeration correction (8 tests / 9 cases updated vs the plan's 5, all sharing the
pre-fix-pinning signature); (3) the Wave-3 fence enumeration correction (+5 owning
transactions); (4) the Wave-4 C3-pin scope deviation (Duo constants instead of C3Duo,
on measured hit rates).

**Counter-signed: owner, 2026-06-01 (round-2 final decision gate)** — all four
orchestrator calls above, as flagged (the L2 armed form additionally carries its
per-ruling signature at the decision-4 row). Signature lines applied at the
substitution-replacement A6 close-out; see
`substitution-replacement-invariant-map.md` § "Campaign close-out".

### What Phase 1 does NOT claim

- **No serializability proof.** The fence narrows the deposed-believer window to one
  floor-read-to-commit round trip per write (the merge transaction via its
  commit-adjacent re-read); it does not eliminate the window.
- **No exhaustive convergence** (above): every TLC "no violation" is bounded-prefix
  evidence; the wired holds checks are bounded random simulation.
- **The status/poison fence has no model coverage** (those writes are not modeled as
  intents — ENC-0b-2); its verification is the Wave-3 db test pairs plus the actor-level
  deposed-actor test.
- **Walk-completion consumption is not fenced** (not a PG evidence write; model-covered
  residual — `canReachCrossTenureWalkConsume` stays reachable by design), and the
  documented Lease-deletion + PG-fault conjunction stands.
- **No VM scenario exercises the closure-evidence lifecycle** (the A0 coverage gap is
  unchanged); Phase-1 verification is unit/db/actor tests + the model + the wired
  sim/TLC checks.
- **Phase 2 is not done** — see the handoff below.

### Phase-2 handoff (the open items)

1. The C5 / CE-7 deferred manual target (0d).
2. The F6 falsifications resting on the rust-simulator backend (0d).
3. The CE-45 (F10) evidence-module-only falsification (0d; re-validated Wave 4).
4. The Tier-3 manual-target posture — the decision-3 residual: owner counter-signature,
   or a Track E bare-metal formal-long lane.
5. The trailing structural evidence modules (CE-18, CE-25, CE-31, CE-40, CE-43) and the
   full-corpus acceptance table.
6. The design-scale regime conjunctions (the SD-4 pre-exclusion) and the C1-strict
   probe.
7. Campaign counter-signatures (the executor / gw-session close-out workflow pattern)
   over this map's records.

Phase 2 appends here.

---

## Phase 2 — assurance: queued fixes, the kani kernel, the full-corpus acceptance table (this stage)

Phase 2 of the closure-evidence campaign (the design §8 "Assurance" row), executed on the
`ce-phase2` worktree on top of the C3 dispatch-mode retirement. Deliverables, in the order
they landed: the two small queued fixes the C4 retirement memo commissioned into this
worktree (the post-terminal BuildProgress freeze, red-first, and the standby-drops-writes
spec carve-out); the kani kernel extraction the design named as the Phase-2 candidates
(the `closure_evidence` classifier AND `admit_pull` — both extracted, neither omitted);
and the full-corpus acceptance table over CE-1..CE-81 below — the campaign's final
accounting. Every commit boundary in this stage is green for stable clippy, rio-scheduler
+ rio-evidence-kernel nextest, tracey-validate, and treefmt; the kani check was built and
verified through the production nix pipeline at each wiring change.

### The two queued fixes (the C4-memo items riding this worktree)

#### Fix 1 — post-terminal BuildProgress freeze (red-first; commit `fcb7ff271`)

The latent defect the build-event-sourcing rescope memo's adversarial review found and
disclosed (memo §3.5; its preconditions verified against this tree): post-terminal
`BuildProgress` events were sequenced after `BuildCompleted`, persisted to
`build_event_log`, and replayed to re-subscribers with totals recomputed from a DAG the
finished build no longer describes; the dispatch store-hit and Skipped emit loops kept
incrementing a terminal build's `cached_count`; and `handle_derivation_failure` rewrote a
settled build's error summary when a shared node failed after the build finished (a
SUCCEEDED build gained an error summary).

Red-first execution: two regression tests staged the defect through the production
pull/merge surfaces (a stale-Completed reset of a shared node under a later build, then a
dispatch-time store hit / a permanent failure fanning out to the resident terminal build)
and FAILED for exactly the characterized reasons before any production change —
`test_terminal_build_frozen_on_dispatch_store_hit` red on `cached_derivations` drift (1 ≠
0), `test_terminal_build_outcome_not_rewritten_by_late_shared_node_failure` red on the
rewritten error summary. The guard then landed: a DagActor-level terminal freeze
(`build_progress_frozen` + an actor-level `emit_progress_with` wrapper) covering the
debounced `emit_progress`, the three precomputed-summary emit sites
(dispatch.rs store-hit loop, completion.rs release loop, completion.rs failure loop), the
two `cached_count` writers, and `handle_derivation_failure`. Both tests green; the full
1106-test scheduler suite green. New spec rule `sched.build.terminal-status-settled`
(impl ×5, verify ×2) makes the freeze normative. The memo's third commissioned item — the
trigger-1 breadcrumb at the event-bus seq-reuse branch — rides the same commit.

#### Fix 2 — the standby-drops-writes carve-out (spec-only; commit `9d48e0044`)

The C4 memo §3.4 contradiction: `sched.lease.standby-drops-writes` flatly forbids standby
writes to `build_event_log`, but two deliberate ex-leader write paths remain in the code
(the persister's bounded in-flight backlog, collision-resolved first-writer-wins by `ON
CONFLICT (build_id, sequence) DO NOTHING`; and the per-build event-log GC DELETE on the
ungated `CleanupTerminalBuild` arm) — the acknowledgment paragraph that priced them was
over-deleted by the executor campaign's stream-era removal (7e60437a1) along with the dead
ForwardPhase exception it was attached to. The carve-out is restored inside the rule body,
narrowed to the two surviving paths with their idempotency rationale (the lossy-by-design
display stream; acknowledge-without-persist on a standby is permitted for it). Rule bumped
to `sched.lease.standby-drops-writes+2`; all impl/verify markers and bare references
re-pointed; tracey validate green. Code fencing of the persister/GC stays rejected per the
memo §4.4 (display-only, already idempotent; revisit only inside trigger-1 work).

### Kani: the closure-evidence decision kernel (`rio-evidence-kernel`)

The design §8 Phase-2 row named two kani candidates: the `closure_evidence` classifier and
`admit_pull`, with the decision rule "recommend kani for admit_pull only if the classifier
proof is not subsumed by the model (decide then)". Both were extracted; the decision
record:

- **The classifier proof is NOT subsumed by the model.** The model's classifier is a
  Quint definition (`evidence()` over model state); its fidelity to the production Rust
  (the early-return order, the `is_some_and` child lookup, the short-circuiting fold) was
  carried by the Phase-0a single-classifier code audit — a manual re-audit obligation.
  The kani proof replaces that manual obligation with a machine-checked one over the
  production code itself, and makes the single-classifier discipline structural (below).
- **admit_pull extracts cleanly — extraction, not reasoned omission.** The pure decision
  was DESIGNED for this lift (decision P10: "Pure — no clocks, no IO, no `&self` — so it
  can be … lifted into a Kani harness without refactoring"); its unit-test module was
  literally named `kernel_tests`. The C3 dispatch-mode retirement (this worktree's base)
  removed its last stream-coexistence references, and the SQL/async entanglement the
  round-1 precedent warns about sits entirely in the CALLER (`pull_assignment_inner` loads
  the inputs; the fenced mint transaction runs after the decision) — the decision itself
  is a case analysis over already-loaded values. The round-1 "load-bearing logic is SQL"
  omission rationale therefore does not apply to the admission decision; it applies to the
  mint transaction, which stays in the scheduler and keeps its existing fencing test
  coverage (`sched.lease.generation-fence+3` db tests).

#### Extraction shape (commits `5965e4b0f`, `994a07fd6`)

`rio-evidence-kernel` is a dependency-free workspace member (no `[dependencies]` at all),
following the `rio-retry-kernel` precedent: hakari final-excluded, crate2nix-built, and
compiled by `crateBuildKani` so the kani goto model closes over the kernel alone.

| Kernel surface | Moved from | Scheduler shim left behind |
|---|---|---|
| `ClosureEvidence` enum + `closure_evidence(present, closure_hole, children)` (generic over a child produced-ness iterator; short-circuiting fold preserved) | `rio-scheduler/src/dag/mod.rs` | `DerivationDag::closure_evidence` — projects (node presence, breadcrumb bit, lazy per-child produced-ness) out of the node/edge maps; `crate::dag::ClosureEvidence` is a re-export |
| `must_substitute(topdown_pruned, evidence)` / `closure_vouched(evidence)` | `rio-scheduler/src/actor/merge.rs` | `DagActor::must_substitute` / `closure_vouched` call the kernel; the dispatch probe partition's Broken arm (`deferred_settlement`) calls `rio_evidence_kernel::must_substitute` directly |
| `pull::PullNodeStatus` (12-variant `DerivationStatus` mirror), `pull::PullAdmission<ExecId>`, `pull::PullRequest`, `pull::admit_pull` | `rio-scheduler/src/actor/pull.rs` | `actor::pull::admit_pull` becomes the projection shim; `PullDecision` = `PullAdmission<Uuid>` type alias; the status mirror is pinned variant-for-variant by an exhaustive `match` (compile error on drift, the retry-kernel db-enum convention) |
| `pull::pull_refused_for_evidence` (the A11 composition: classifier → must_substitute → admission) | new (Phase 2) | consumed by the proofs and available to future callers |

Behavior is unchanged by the move: the projections preserve the original lookup order and
the short-circuiting child fold, and the full scheduler suite (1106 tests pre-existing + 8
kernel + 5 pull-kernel) passes unmodified. `.config/tracey/config.styx` gains the kernel
crate in the impl include set; the kernel functions carry the `r[impl]` markers for the
clauses they now implement (`sched.evidence.closure-hole`,
`sched.merge.substitute-topdown+12`, `sched.executor.pull-gone`,
`sched.executor.pull-not-ready+2`, `sched.lease.generation-fence+3`).

#### Proof inventory and measured budgets

Thirteen harnesses, all verified. Local run: `cargo kani -Z function-contracts` inside
`rio-evidence-kernel/` — 14.8 s wall including the kani-compiler build, ~3.9 s total CBMC
time. Nix pipeline: `/nixbuild .#checks.x86_64-linux.kani-rio-evidence-kernel` — built and
verified green (~15 s wall on a warm remote builder; "Complete - 13 successfully verified
harnesses, 0 failures"). Wired into `checks.*` and promoted into the CI formal lane (the
`kaniChecks` inherit in flake.nix; gen_matrix clusters kani-* checks as singletons), with
`expectedHarnesses = 13` as the silent-drop tripwire.

| Harness | Property (invariant-map name) | CBMC time |
|---|---|---|
| `check_classifier_exhaustive_case_analysis` | the classifier's five-case partition — exact, total, panic-free (the Phase-0a single-classifier audit, mechanized) | 0.61 s |
| `check_marked_broken_must_substitute` | marked + Broken ⇒ must_substitute (A1's predicate level; the substitute-topdown MUST-NOT-dispatch clause) | 0.44 s |
| `check_vouched_never_must_substitute` | Vouched/Pending ⇒ ¬must_substitute | 0.39 s |
| `check_unmarked_evidence_inert` | ¬marked ⇒ ¬must_substitute (closure-hole inert-on-unmarked clause) | 0.38 s |
| `check_hole_breaks_and_never_vouches` | hole ⇒ Broken, never vouches, never un-sets must_substitute (A8/brokenNeverVouches + the OR-monotonicity / stale-true-is-safe asymmetry) | 0.55 s |
| `check_vouched_iff_nonempty_all_produced` | Vouched ⟺ present ∧ ¬hole ∧ non-empty ∧ all produced (the clear/stamp-exemption criterion) | 0.44 s |
| `check_must_substitute_contract` | proof_for_contract over the `#[kani::ensures]` clause | 0.05 s |
| `check_closure_vouched_contract` | proof_for_contract over the `#[kani::ensures]` clause | 0.05 s |
| `pull::check_admit_pull_partition` | the admission's exhaustive decision table (13 statuses × token × fence × flag × identity) | 0.14 s |
| `pull::check_admit_pull_refuses_must_substitute` | A11 code half: Ready + must_substitute is parked, never minted; AW5 re-delivery is the only delivery a flagged node can receive | 0.13 s |
| `pull::check_admit_pull_rejections_dominate` | the load-bearing check order (token ≻ fence ≻ node state) | 0.11 s |
| `pull::check_admit_pull_identity_match` | DeliverExisting only to the open attempt's own identity, carrying its exec id | 0.11 s |
| `pull::check_pull_refusal_chain` | the end-to-end A11 chain through BOTH kernels: any classifier input judged must-substitute makes an authenticated Ready-node pull park | 0.43 s |

What the proofs do and do not claim: they prove the decision predicates over their full
(bounded-children, full-status-alphabet) input domains as implemented in the kernel crate;
the scheduler's projections into those predicates (the DAG map lookups, the
DerivationStatus mirror) are pinned by the exhaustive shim matches (compile-time) and the
existing scheduler unit tests (runtime), not by CBMC. The model (quint) continues to own
the lifecycle protocol AROUND the predicates — when stamps/holes/clears happen; the kernel
owns what the predicates SAY. MBT remains not planned (unchanged from the design §8: the
in-process actor battery + the model cover the surface; Phase 1 did not change handler
structure).

### The full-corpus acceptance table (CE-1..CE-81)

The campaign's final accounting: every corpus row's disposition, with what carries its
assurance on the as-fixed tree. Disposition vocabulary (the close-out categories):

- **FIXED-P1** — the defect class was repaired by a Phase-1 wave (cite wave/commit).
- **BY-CONSTRUCTION** — the current architecture makes the defect unrepresentable (cite
  the mechanism). Where the mechanism is a model-abstraction argument (the all-or-nothing
  merge intent), the production mechanism is cited alongside.
- **WIRED-CHECK** — a permanent CI check guards the defect class (cite the check). Wired
  calibration checks guard by falsification (removing the production guard class re-finds
  the violation); holds checks and kani harnesses guard the property directly.
- **TEST** — named unit/db/actor test coverage (the design §4 NOT-ENC alternative coverage
  or the fix's own regression tests).
- **RESIDUAL** — an explicitly accepted, recorded bound (cite the record).

A row may cite secondary evidence after the primary. EM-(name) = committed evidence module
under `docs/spec/models/calibration/` (re-runnable falsification record, not wired).

| CE | Disposition | What carries the assurance now |
|---|---|---|
| CE-1 | BY-CONSTRUCTION | Merge-time classification re-checks the store unconditionally (`check_cached_outputs`); C4 falsification recorded as EM-closure-f1-skip-store-recheck; family guard WIRED `quint-closure-calib-f1-stale-produced` |
| CE-2 | WIRED-CHECK | `quint-closure-calib-f1-stale-produced` (B9); production verify covered by `test_reprobe_unlocked_deferred_past_stale_reset` (I-047 class) |
| CE-3 | TEST | JWT forwarding under `sched.merge.substitute-probe` + its merge tests; trichotomy guard EM-closure-f3-indet-failfast (B3) |
| CE-4 | TEST | CA realisation lane (NOT-ENC, design §4): `reprobe_substitute_floating_ca…` tests + `sched.merge.ca-fod-substitute` |
| CE-5 | WIRED-CHECK + TEST | Reset/Queued-gating half: `quint-closure-calib-f1-stale-produced` + `test_reprobe_unlocked_deferred_past_stale_reset` (the parent-stays-Queued-past-stale-reset pin); Skipped half: `test_stale_skipped_output_reset` (**re-added pull-mode THIS STAGE** — the design §2e noted the stream-era original was deleted) + dag/tests.rs H1/H2 |
| CE-6 | WIRED-CHECK | Family rep CE-2 (`quint-closure-calib-f1-stale-produced`); single verify path by construction (`verify_preexisting_completed` is the one reset/routing decision site) |
| CE-7 | RESIDUAL | The C5/CE-7 deferred manual target (0d record: falsifiability plan at 3-build constants; owner sign-off item). Production behavior covered by the effective-wanted unit tests (77c98e01b's regression suite) |
| CE-8 | WIRED-CHECK | Family rep CE-9 (`quint-closure-calib-f2-seed-only-walk`); production fixed-point (`apply_cached_hits` gated on `all_deps_completed`) |
| CE-9 | WIRED-CHECK | `quint-closure-calib-f2-seed-only-walk` (B2) |
| CE-10 | WIRED-CHECK | Family rep CE-9 (same check); walk error propagation tests |
| CE-11 | WIRED-CHECK | Family rep CE-9 + wired witness `quint-closure-evidence-witness-tried-demotion` (the `substitute_tried` one-shot) |
| CE-12 | TEST | `verifiable_wanted_paths` ∃-guard (one shared predicate, 6e6fe5b8a) + B4 falsification EM-closure-f4-vacuous-prune |
| CE-13 | TEST | Own-selector resolvability guard + its merge tests; B4 falsification EM-closure-f4-vacuous-prune |
| CE-14 | TEST | Family rep CE-13 (same EM); both-guards tests (97fae90f5) |
| CE-15 | TEST | Model property B7 (holds, 0c record); production prune/classification share the wanted-criterion helpers |
| CE-16 | TEST | Union-on-conflict stored wanted (B5); falsification EM-closure-f5-wanted-overwrite; production union tests |
| CE-17 | TEST | Walk-completion re-check against current wanted (model consumeWalk; B1/B2); production re-check tests (6609ce4fe) |
| CE-18 | BY-CONSTRUCTION | Model: the all-or-nothing merge intent makes rollback-replay divergence unrepresentable (0d re-route record); production: `rollback_merge` restores wholesale snapshots; rollback tests in actor/tests/merge.rs |
| CE-19 | BY-CONSTRUCTION | Same mechanism as CE-18 (+ the resubmit-reset wholesale restore); 2999c1bea regression tests |
| CE-20 | TEST | Chain-scoped `never_forgive` + chain-end clears; A21 falsification EM-closure-f6-latch-outlives-chain (rust-simulator backend, the recorded TLC discrepancy); 71e37da5f tests |
| CE-21 | TEST | Family rep (CE-21 co-rep with CE-20, same EM); setter-coverage tests (5113de5c8) |
| CE-22 | WIRED-CHECK | `quint-closure-evidence-witness-downgrade-respawn` (the downgrade re-spawn arm is reachable); A12/A1 model properties; 29b5322e0 tests |
| CE-23 | RESIDUAL | The accepted forgiveness residual hole — wired witness `quint-closure-evidence-witness-forgiven-residual` (the model REACHES it, negative calibration by design); 9bc7be84a documentation; deployment-checklist CE-D8-adjacent |
| CE-24 | BY-CONSTRUCTION | The `topdown_pruned` state machine exists end-to-end (migration 063 + the mark lifecycle); WIRED `quint-closure-calib-f8-dispatch-no-evidence` (A1) + `kani-rio-evidence-kernel` (THIS STAGE) |
| CE-25 | BY-CONSTRUCTION | Production: the stamp is a statement of the merge transaction itself (`persist_merge_to_db`, `sched.evidence.durability+2`); model: all-or-nothing intent (the 0d structural-override-inert record). The deferred structural override is closed as inert-at-abstraction — the abstraction IS the mechanism |
| CE-26 | BY-CONSTRUCTION | Activation is the merge transaction's last statement (A13); wired witness `quint-closure-evidence-witness-rollback` |
| CE-27 | TEST | Stamp gate requires dropped closure (`markImpliesClosureDropped`, A2 holds 0c); 03ff900e6 merge tests |
| CE-28 | WIRED-CHECK | `quint-closure-calib-f7-clear-unbuilt` (co-rep with CE-30) |
| CE-29 | FIXED-P1 (ii) + TEST (i) | (ii) the fail-open dispatch arm: Wave 1 settlement re-probe (every fail-fast/dispatch decision point now requires a definitive verdict; `7c2d8ea31`); (i) fail-fast consumes the mark: A9 (holds 0c) + c0431eb20 tests |
| CE-30 | WIRED-CHECK | `quint-closure-calib-f7-clear-unbuilt` (A3) |
| CE-31 | BY-CONSTRUCTION | The recovery clear gate is the strict SQL criterion over the durable relation (`load_parents_with_all_children_produced`, db/recovery.rs); A19 (holds; trigger pinned by recovery witnesses); db/tests/recovery.rs. The A19-direction structural override stays a recorded deferred target (0d) |
| CE-32 | BY-CONSTRUCTION | Migration 063 + OR-on-conflict persistence; wired witness `quint-closure-evidence-witness-stamp`; recovery restore tests |
| CE-33 | WIRED-CHECK | `quint-closure-calib-f8-dispatch-no-evidence` (A1) + `kani-rio-evidence-kernel` (`check_marked_broken_must_substitute`, `check_pull_refusal_chain` — THIS STAGE) |
| CE-34 | TEST | A14 `terminalIsTerminal` (holds 0c); f09d16611 idempotence regression tests |
| CE-35 | BY-CONSTRUCTION | **Strengthened THIS STAGE**: the single classifier is now a dependency-free pure kernel (`rio_evidence_kernel::closure_evidence`) and every scheduler site projects through `DerivationDag::closure_evidence` — a guard cannot bypass the classifier without bypassing the only function that exists; WIRED `kani-rio-evidence-kernel` (exhaustive case analysis) |
| CE-36 | WIRED-CHECK | Wired witness `quint-closure-evidence-witness-hole-reap` + `kani-rio-evidence-kernel` (`check_hole_breaks_and_never_vouches`); A4 (holds 0c) |
| CE-37 | FIXED-P1 (pairing) | The reap-time survivor re-evaluation (8c13186fa) plus Wave 2b's poison-clear survivor re-evaluation (`7750a4d45`, `sched.poison.clear-survivor-reevaluation`) — the L3 re-hunt proves the pairing closes the strand; reap-survivor tests |
| CE-38 | FIXED-P1 | Wave 1 settlement (the fail-fast skips in-flight-walk survivors and re-probes; `7c2d8ea31`/`2351a35ba`/`7b4a2e2d2`); a6550006e's original in-flight-walk skip; Wave-1 battery tests |
| CE-39 | BY-CONSTRUCTION | Migration 064 + OR-on-conflict; wired witnesses hole-reap/hole-recovery |
| CE-40 | WIRED-CHECK | `quint-closure-evidence-witness-hole-recovery` (the recovery hole-stamp fires); recovery tests. The deferred structural override (A4-direction recovery copy) closed as covered-by-witness + tests |
| CE-41 | WIRED-CHECK | `quint-closure-calib-f9-poison-clear-no-stamp` (A5) + wired witnesses hole-admin-clear / hole-ttl-sweep |
| CE-42 | TEST | `test_closure_hole_survives_completion_and_stale_completed_reset` + A4–A8 model properties (resubmit carry encoded) |
| CE-43 | TEST | Round-23 bug_006 regression test (2791da787) + A9 `failFastConsumesMarkKeepsHole` (holds 0c). The deferred structural override closed as covered-by-test + model property |
| CE-44 | TEST | A20 `healCompleteness` (holds 0c); PG-side total heal filter; 6799b70b5 tests |
| CE-45 | BY-CONSTRUCTION | The strict SQL vouch gate (live co-ownership joins); A3 falsification at the unscoped variant recorded as EM-closure-f10-recovery-vouch-unscoped (depth-17 TLC, evidence-module-only per 0d) |
| CE-46 | FIXED-P1 | Wave 2b co-ownership scoping of the in-DAG recompute (`7750a4d45` production + `e56a1b73c` model; A22 + `pInDagCondemnCriterion`); the condemn direction was the Wave-2 residual finding |
| CE-47 | BY-CONSTRUCTION | The recovery failed-dep cascade (`sched.recovery.failed-dep-cascade+2`) + Wave 2b scoping; A15 declared-children form; recovery tests |
| CE-48 | TEST (i) + TEST (ii) | (i) recovered-Substituting reset: EM-closure-f14-recovery-keeps-substituting (L1 falsified) + recovery reset tests; (ii) orphan-Ready interest gate: A16 (holds 0c) + recovery interest-gate tests |
| CE-49 | BY-CONSTRUCTION | All-or-nothing merge transaction (A13/B6); ENC-A CE-26; 8b22d0594 regression tests |
| CE-50 | FIXED-P1 | Wave 3 FENCE EVERYTHING (uniform claims-floor fence on every evidence write; `498db7410`..`c47de3ccc`); WIRED `quint-closure-evidence-stale-fence-holds` + `quint-closure-calib-a17-unfenced` regression pin. Supersedes the 0d F11 rust-simulator-only posture |
| CE-51 | WIRED-CHECK | `quint-closure-evidence-witness-tried-demotion` (the one-shot demotion is reachable, so the loop-suppression arm exists); L1 (holds 0c) |
| CE-52 | TEST | The 0d absorption record (the re-probe lane cannot strand a node at this abstraction — merge-time resetNodes owns resurrection); `test_resubmit_poisoned_at_limit_substitutable` (the Poisoned→Substituting transition-table pin); L1 via EM-closure-f14 |
| CE-53 | TEST | `test_resubmit_poisoned_at_limit_substitutable` (the pull-mode successor of c9107fc1e C5's stream-era test, both lanes) + the same family absorption record |
| CE-54 | TEST | Revert-target completeness tests (6875c3769 C4); A15-adjacent model coverage |
| CE-55 | TEST | Build-accounting (NOT-ENC F17): merge-over-poisoned-leaf verdict tests + `tick_recheck_stuck_completions` |
| CE-56 | TEST | d91df7e9f regression (poison-removal updates interested builds' totals) |
| CE-57 | TEST | `test_reprobe_completion_fans_out_to_earlier_build` (c9107fc1e C4) |
| CE-58 | TEST | Probe-coverage union (merge-time + dispatch-time); B8 falsification EM-closure-f13-unprobed-dispatch; dispatch_time_substitutable_completes tests |
| CE-59 | TEST | Spawn-intent probed gate (`sched.admin.spawn-intents.probed-gate+2` tests) — NOT-ENC (pods out of model) |
| CE-60 | TEST | Probe trichotomy: EM-closure-f3-indet-failfast (B3) + EM-closure-f3-substitutable-demoted (C2); store cap-truncation tests |
| CE-61 | TEST | Family rep (same EMs); 429/5xx-as-indeterminate store tests |
| CE-62 | TEST | ENC-A CE-61 (same EMs); retry-before-demote (15aa844d7) walk tests |
| CE-63 | TEST | Store probe-cache tenant keying tests — NOT-ENC (store-side) |
| CE-64 | TEST | Store singleflight tests — NOT-ENC (store-side) |
| CE-65 | TEST | Store placeholder reclaim tests (2d7e4f9fd) — NOT-ENC (store-side) |
| CE-66 | WIRED-CHECK | `quint-closure-calib-f4-demand-drop` (B10) + `test_topdown_explicit_target_*` (85213119d, actor/tests/merge.rs) |
| CE-67 | TEST | Gateway result verification (cb3f6bfbb/73bcad709 tests; `gw.dag.reconstruct+3`) — NOT-ENC (different component) |
| CE-68 | BY-CONSTRUCTION | Duplicate-drv contribution union (production) + all-or-nothing intent (model); 8c594a527 tests |
| CE-69 | WIRED-CHECK | ENC-A CE-30: `quint-closure-calib-f7-clear-unbuilt` (the clear-before-reconciliation weakening IS the F12 ordering shape); `sched.merge.reconcile-order` tests |
| CE-70 | BY-CONSTRUCTION | The detached-walk asynchrony IS the design response to the stall family (NOT-ENC by design); the model's asynchronous walk encodes it; perf fixes' own tests |
| CE-71 | TEST | Recovery preserves each build's full derivation set (891a6520d regression); recovery tests — NOT-ENC (build accounting) |
| CE-72 | TEST | Recovery interest gate (998df909b, I-058/I-059) + A16 (holds 0c); recovery tests |
| CE-73 | BY-CONSTRUCTION | The produced-status set is {Completed, Skipped} everywhere (model: the Produced collapse absorbs it; production: `all_deps_completed` / the verify candidates / the classifier projections all match on both); dag/tests.rs H1/H2 + `test_stale_skipped_output_reset` (re-added THIS STAGE) |
| CE-74 | TEST | CA-cutoff candidate identity tests (d7cf1a4ce) — NOT-ENC (CA lane out of model) |
| CE-75 | BY-CONSTRUCTION | ENC-A CE-18 (all-or-nothing + wholesale restore); a91d63026 rollback-restore tests |
| CE-76 | TEST | `sched.merge.dep-failed-transitive` tests (e45f2d966 C4) — NOT-ENC (F17) |
| CE-77 | TEST | Completion fan-out + completion-authenticity tests (4d20e7c28) — NOT-ENC (F17) |
| CE-78 | TEST | `tick_recheck_stuck_completions` (71a7c8a9b) — NOT-ENC (F17) |
| CE-79 | TEST | Recovery verdict-state restoration tests (5b4543c3a) — NOT-ENC (F10/F17 accounting); round-1 retry campaign owns the ledger-side restore |
| CE-80 | TEST | Retry-lane persistence tests (84a692492); round-1 retry campaign (the ledger fold) — NOT-ENC here |
| CE-81 | TEST | Recovery completion sweep tests (04581fcbb) — NOT-ENC (F17) |

**Tally.** 81 rows, all dispositioned — no row without a disposition (no coverage gap; the
stop-and-report condition did not fire). By primary category: 17 WIRED-CHECK, 15
BY-CONSTRUCTION, 6 FIXED-P1 (CE-29ii, CE-37, CE-38, CE-46, CE-50 — plus CE-33/CE-35's
Phase-2 kani strengthening counted under WIRED-CHECK/BY-CONSTRUCTION), 41 TEST, 2 RESIDUAL
(CE-7, CE-23). Cross-checks against the design §4 sheet: the 19 NOT-ENC rows all land in
TEST with their pre-registered named coverage (no NOT-ENC row was silently re-dispositioned);
the 62 in-model rows land in WIRED-CHECK / BY-CONSTRUCTION / FIXED-P1 / TEST according to
whether their falsification is wired, their mechanism is structural, their fix was Phase 1,
or their record is an evidence module + production test.

**Coverage actions taken by this stage** (rows whose disposition the table itself forced):

1. CE-5 (Skipped half) / CE-73-adjacent: `test_stale_skipped_output_reset` re-added in
   pull mode (the stream-era original was deleted with the session machinery; the design
   §2e and the 0d ENC sheet both noted the re-add for Phase 2).
2. CE-18 / CE-25 trailing structural overrides: closed as **inert-at-abstraction** (the
   all-or-nothing merge intent is the model's mechanism; writing a "structural override"
   would mean re-introducing behavior the abstraction excludes — the 0d re-route record
   already establishes this; this stage makes it the final disposition rather than a
   deferral).
3. CE-31 / CE-40 / CE-43 trailing overrides: closed as covered (CE-31 by the strict
   durable-relation gate + A19 + recovery tests; CE-40 by the wired hole-recovery witness;
   CE-43 by the bug_006 regression test + A9) — the falsification-shaped overrides remain
   unwritten and are no longer carried as open items; re-opening them requires a new
   campaign decision, not a standing deferral.

### Phase-1 handoff items — Phase-2 dispositions

| # | Handoff item (Phase-1 close-out) | Phase-2 disposition |
|---|---|---|
| 1 | C5 / CE-7 deferred manual target | Stays deferred; carried as the CE-7 RESIDUAL row + owner sign-off line item below. The falsifiability plan (3-build constants, pgWanted-keyed override) remains recorded in the 0d record; not executed this stage (model work, not assurance work) |
| 2 | F6 falsifications resting on the rust-simulator backend | Unchanged (the TLC-discrepancy record stands); CE-20/CE-21 dispositioned with the rust-sim EM cited explicitly |
| 3 | CE-45 evidence-module-only falsification | Final disposition: BY-CONSTRUCTION (strict SQL gate) + the EM as the falsification record; no longer an open item |
| 4 | Tier-3 manual-target posture (decision-3 residual) | Not Phase-2's to resolve — owner counter-signature item (below), unchanged |
| 5 | Trailing structural EMs (CE-18/25/31/40/43) + the acceptance table | **Done this stage**: the acceptance table is above; the five trailing EMs have final dispositions (coverage actions 2–3 above) |
| 6 | Design-scale regime conjunctions (SD-4) + C1-strict probe | Unchanged posture (pre-excluded on recorded 0b/0c evidence; C1-strict stays a recorded deviation — see the C1-strict row in Group C) |
| 7 | Campaign counter-signatures | Line items prepared below; the counter-signature itself is the campaign close-out workflow (A6), not this stage |

### Validation sweep and commits

Per-commit gates (every commit in this stage): stable clippy `--deny warnings` for the
touched crates, `cargo nextest run -p rio-scheduler -p rio-evidence-kernel` (1119 tests +
the new ones green), `tracey query validate` (0 errors), treefmt, and the pre-commit hook
battery (crate2nix-check / hakari-check on the workspace-touching commits). The kani check
was additionally built through the production nix pipeline
(`/nixbuild .#checks.x86_64-linux.kani-rio-evidence-kernel`) after each wiring change.
The full `/nixbuild --checks` gate is run by the orchestrator at integration (per the
worktree contract; not run here).

| Commit | Subject |
|---|---|
| `fcb7ff271` | fix(rio-scheduler): freeze progress emission and served accounting for terminal builds |
| `9d48e0044` | docs(spec): restore the standby event-log write carve-out under standby-drops-writes |
| `5965e4b0f` | feat(rio-evidence-kernel): extract the closure-evidence classifier into a CBMC-verified kernel |
| `994a07fd6` | feat(rio-evidence-kernel): lift the pull-admission decision into the verified kernel |
| (this commit) | docs(spec): closure-evidence Phase-2 stage record + the CE-1..81 acceptance table; pull-mode re-add of test_stale_skipped_output_reset |

### Counter-signature line items (for the campaign close-out / A6)

Carried forward from Phase 1, plus this stage's additions:

1. The L2 armed form (Phase-1 orchestrator call within owner decision 4).
2. The Wave-1 battery enumeration correction (8 tests / 9 cases vs the plan's 5).
3. The Wave-3 fence enumeration correction (+5 owning transactions).
4. The Wave-4 C3-pin scope deviation (Duo constants).
5. The decision-3 residual (the empty `longChecks` Tier-2 set / manual-target posture).
6. **(Phase 2)** The C5/CE-7 deferral accepted as a RESIDUAL acceptance-table row rather
   than executed (the only corpus row whose model falsifiability is still owned by a plan
   rather than a run).
7. **(Phase 2)** The CE-18/CE-25 inert-at-abstraction closure and the CE-31/40/43
   covered-by-existing-evidence closure (this stage's coverage actions 2–3) — the five
   "trailing structural evidence modules" are closed without writing the overrides.
8. **(Phase 2)** The admit_pull extraction decision (extract rather than the design's
   conditional "only if not subsumed") and the kernel's scope: the decision predicates are
   CBMC-proven; the projections are compile-pinned + test-covered; the mint transaction
   stays SQL with its existing fencing tests.

Phase 2 is complete. The campaign's remaining open work is the close-out itself
(counter-signatures over this map's records) plus the standing owner items above.

### Successor-campaign cross-reference (substitution replacement, Phase A landed 2026-06-01)

The **substitution-replacement campaign** (Phase A landed 2026-06-01,
additive-dormant) is the successor owner of the substitution-related rows in
this map: the topdown-prune / substitution-walk machinery this campaign's
properties protect (A1/A2/A11, the C3 settlement, the B-family walk shapes)
is scheduled for replacement by store-owned materialization jobs in that
campaign's Phases B–D′, with every property's successor home named in its
design §10 / FP-6 disposition table. Until that campaign's Phase D′ executes,
every row in this map remains authoritative and its wired checks remain the
merge gate; nothing is retired by Phase A (which is dormant by construction —
its dormancy criterion 5 requires this map's checks to stay untouched and
green). See `substitution-replacement-invariant-map.md` for that campaign's
stage records, the §9.1 successor property skeleton, and the Phase B entry
criteria.
