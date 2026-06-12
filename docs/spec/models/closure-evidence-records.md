# Closure-evidence campaign records (closed-campaign archive)

Archived verbatim from `docs/spec/models/closure-evidence-invariant-map.md` @
`a00957266` (the retirement wave's base; the map is deleted unchanged by the
wave's final commit); append-only; this is a closed-campaign archive, not a
live registry — nothing here maps live artifacts. Relocated by owner directive
2026-06-12 ("can we get rid of the invariant-map.md's now?"), content
unmodified.

References to `*-invariant-map.md` files inside the archived text below are
historical: all 13 per-campaign invariant maps were retired in the same wave.
Their surviving records live in the sibling `*-records.md` archives, the model
and calibration `.qnt` headers, the `nix/quint.nix` check comments, the spec
`.typ` rules, and `docs/ops/` — and the full originals in git history.

Live carriers for this campaign (not this file): the survivors-core wired
checks and the two kept pins (`nix/quint.nix`, closure-evidence section), the
archived-in-place model `closureEvidence.qnt`, the kani decision kernel
(`rio-evidence-kernel`, `nix/kani.nix`), and the spec rules in
`docs/spec/components/scheduler.typ`.

---

## 1. Invariant ↔ rule linkage (final state; the Rule(s)-column record for the retired properties)

> Origin: closure-evidence-invariant-map.md § "Invariant ↔ rule map (filled in
> at Phase 0b/0c)", verbatim. The survivors core (A14/A15/A22/B9/B10/L3) keeps
> live linkage through the wired checks' markers in nix/quint.nix; every other
> row's linkage is this archived record.

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


---

## 2. Phase 0c — the exhaustive-attempt record

> Origin: closure-evidence-invariant-map.md § "Phase 0c", closing-run record,
> verbatim.

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

---

## 3. Phase 0d — calibration verdicts and family acceptance

> Origin: closure-evidence-invariant-map.md § "Phase 0d — calibration", from
> the override-module inventory through the 0d → Phase 1 handoff, verbatim.
> Per-file verdict lines also live in each
> `calibration/closure-*.qnt` header (the VERDICT blocks).

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


---

## 4. Post-0d triage — the L3 manual target and the re-pointed checks

> Origin: closure-evidence-invariant-map.md § "Post-0d triage", the
> FailoverEx-L3 dispositions (with the retired manual-target command block) and
> the wired-check delta of that stage, verbatim.

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

---

## 5. Phase 1 Wave 4 — raised-budget exhaustive measurements (the Tier-3 manual-target record)

> Origin: closure-evidence-invariant-map.md § "Phase 1 Wave 4", stage record
> §5, verbatim. These are the seven retired manual targets' coordinates; the
> CE-D6 runbook row in §6 below carries the re-run command shape.

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

---

## 6. Phase-1 close-out — defect dispositions, property-table final state, deployment rows

> Origin: closure-evidence-invariant-map.md § "Phase-1 close-out", the defect
> dispositions, the property-table final state, and the deployment-checklist
> deltas with their successor cross-references and the A6 archive record,
> verbatim.

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

> **A6 archive executed (substitution-replacement follow-up close-out,
> 2026-06-02).** The archive plan named above ran 2026-06-02 under the
> owner's waiver of the post-soak precondition: the archive is the file
> IN PLACE at `docs/spec/models/closureEvidence.qnt` — full pre-D7 model
> text and all 14 legacy regime modules retained verbatim at the
> original path (the wired checks stage it by exact path and the kept
> pins import it; no `archive/` move), with the executed-archival banner
> in the file header. The wired family is unchanged: the survivors core
> + the two kept pins, plus the seeded FailoverDuo B9 corner check
> `quint-closure-corner-failover-duo-b9` (follow-up ledger row 2; module
> `closureEvidenceCornerFailoverDuo`, the discovery configuration) — the
> corner and seed `0xffbfc9ac0c85df5b` ride the archive machine-checked.
> Pre-prune wiring and the 27 retired checks' dispositions: D7 commit
> `94996482b` ("chore(models): wire the closure-evidence survivors core
> and prune superseded checks"; identify by subject after rebases). The
> 15 unwired calibration overrides' retirement notes remain valid as-is.


---

## 7. Owner-decision provenance

> Origin: closure-evidence-invariant-map.md § "Owner-decision provenance",
> verbatim (the applied counter-signature lines at the decision-3/decision-4
> rows and the four-orchestrator-call paragraph).

### Owner-decision provenance

Every owner decision this campaign executed under, with date and Phase-1 outcome:

| Date | Decision (owner) | Phase-1 outcome |
|---|---|---|
| 2026-05-29 (design checkpoint) | D16 settlement obligation ADOPTED (normative MUST at 0a, red-first fix in Phase 1); fencing decision deferred until post-0c evidence; GC-after-vouch accepted as a known bound; calibration target = the formal-sprint tip | `sched.evidence.settlement` added at 0a (intentionally uncovered); FIXED Wave 1; covered (impl + verify) since. |
| 2026-05-30 (Phase-0 checkpoint, decision 1) | Fix BOTH C3 and L3, red-first, model traces as oracles | C3: FIXED Wave 1. L3: the oracle itself was found defective (review finding RT-2), so Wave 2 repaired the model first; the re-derivation REFUTED L3 as premised, and the refuted premise went back to the owner instead of being silently re-scoped — the oracle-discipline reading of decision 1. The residual it exposed became the Wave-2b fix (next row). |
| 2026-05-30 (Wave-2b checkpoint) | The Wave-2 residual finding takes disposition (b): spec-conformance fix, both halves together (co-ownership scoping AND poison-clear survivor re-evaluation) | FIXED Wave 2b red-first; new rule `sched.poison.clear-survivor-reevaluation`; the red-half TLC run (scoping without promotion re-finds the strand) is the recorded proof the halves are jointly load-bearing. |
| 2026-05-30 (Phase-0 checkpoint, decision 2) | FENCE EVERYTHING — uniform claims-floor on every evidence write (~10 statements), not just the merge tx + W2/W5 | Implemented Wave 3: 10 statements + the merge tx + 8 owning transactions (the execution found 5 more owning transactions than the plan's enumeration — the Phase-1b failure-classification handlers; identical fence pattern). A17/A18 flipped to HOLDS, Wave 4. |
| 2026-05-30 (Phase-0 checkpoint, decision 3) | RAISE THE MERGE-GATE BUDGET — 15–30 min checks allowed; wire every exhaustive conjunction that converges at 30 min; the rest stay documented manual targets | **The "wire every converging conjunction" set is empty — by physics, not by omission.** All seven candidates were measured at 35 min / 60 workers; none converges (Wave-4 §5). The pre-registered contingency (adjudication OQ6) applied: the `longChecks` Tier-2 mechanism is implemented, verified end-to-end, and ships empty; all seven conjunctions are documented manual targets with zero-violation bounded-prefix records. The owner counter-signs this as the accepted decision-3 residual, or commissions the Track E bare-metal formal-long lane. **Counter-signed: owner, 2026-06-02 (follow-up-ledger close-out; signature line applied at the follow-up-ledger close-out).** |
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

---

## 8. Phase 2 — the full-corpus acceptance table (CE-1..CE-81)

> Origin: closure-evidence-invariant-map.md § "The full-corpus acceptance
> table (CE-1..CE-81)" and the Phase-2 coverage actions, verbatim.

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


---

## 9. Counter-signature line items and the A6 close-out disposition

> Origin: closure-evidence-invariant-map.md § "Counter-signature line items
> (for the campaign close-out / A6)", verbatim.

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

> **A6 close-out disposition (2026-06-02).** Line items 1–4 above are
> counter-signed: owner, 2026-06-01 (round-2 final decision gate); the
> signature lines are applied at their flags in this map (the
> decision-4 provenance row and the four-call provenance paragraph).
> Items 5–8 were not part of that decision and remain standing owner
> line items, carried in the successor campaign's follow-up ledger. The
> close-out record for the whole Track A arc — this map's verification
> stages plus the substitution replacement — is
> `substitution-replacement-invariant-map.md` § "Campaign close-out
> (A5/A6)", which also carries the final consolidated deployment
> checklist (the CE-D rows' end states, including the CE-D7
> vacuous-close and the CE-D8 split recorded in the Phase D′
> cross-reference note above).

**Counter-signed: owner, 2026-06-02 (follow-up-ledger close-out)** — line
items 5–8 above, as flagged (item 5 additionally carries its per-ruling
signature at the decision-3 provenance row). Signature lines applied at
the follow-up-ledger close-out; see
substitution-replacement-invariant-map.md § "Follow-up ledger" row 8.
