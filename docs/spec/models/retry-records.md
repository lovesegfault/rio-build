# Retry/poison/cascade campaign records (closed-campaign archive)

Archived verbatim from `docs/spec/models/retry-invariant-map.md` @
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

Live carriers for this campaign (not this file): `retryPolicy.qnt` (the
post-collapse model, its header legend and scope notes), the
`quint-retry-*` checks and the 1c' retired-check delegation comment in
`nix/quint.nix`, the `rio-retry-kernel` kani contracts, and the
`sched.retry.*` rules in `docs/spec/components/scheduler.typ`.

---

## 1. Stage-C calibration: the historical-fix corpus replayed against the as-built model

> Origin: retry-invariant-map.md § "Stage-C calibration" through "Phase-0
> exit-gate verdict" (the G1-G8 calibration table of the deleted corpus, the
> HOLDS rows and their dispositions, the permanent expect-violation witnesses,
> and the exit-gate verdict), verbatim. The Stage-C override corpus
> (calibration/retry-*.qnt) and the frozen retryPolicyAsBuilt.qnt these tables
> calibrated were deleted by the Phase-2 retirement recorded in §3 below.

## Stage-C calibration: the historical-fix corpus replayed against the model

The 45-commit fix corpus (inventory §5, eight families G1–G8) replayed
against `retryPolicy.qnt`: for each commit, the pre-fix behavior is either
expressed as an override of the as-built model and shown to falsify an
invariant (the model would re-find that bug), or its non-encodability is
dispositioned. Method per the log campaign's Phase-3 procedure: each
override is a module in `docs/spec/models/calibration/retry-<family>.qnt`
that instantiates the as-built model, replaces ONE entry-point action with
a local PRE-FIX variant, and exposes it as a `calibStep` selected with
`quint verify --step=calibStep` (the retry model has no per-fix const
switches, so the override is an action+step swap rather than a const flip;
the reference-fold ghost keeps the as-built/post-fix semantics in every
override — it is the oracle, not part of the reverted behavior). Where the
distinguishing baseline needs the same restricted alphabet, the module also
carries a `baselineStep` (the as-built actions over the same alphabet) and
the table records its HOLDS verdict. Verdicts below are exhaustive TLC
results (violation runs stop at the first counterexample); depths and
state counts are from the recorded transcripts; wall-clocks live in the
introducing commit's message.

Three invariants were added to the main model as part of the calibration
(the third by the Phase-0 exit review's correction pass):
`clearedPoisonClearsDurably` (the PG-first clear discipline; G4) and
`clearedPoisonScrubsExclusions` (the TTL/admin clear scrubs the durable
exclusion set; G4, the b09c5b312-X13 half) — both genuine **unstated
properties** (no design-§3 invariant or spec rule stated the clear's
durability or scrub discipline before the calibration) — and
`recoveryPreservesPoisonStatus` (the poison set survives failover minus
TTL expiry; G8) — a **Stage-B encoding gap**, not an unstated property:
design §3's `RecoveryIsTheDocumentedProjection` already states "the
poison set is preserved exactly minus TTL-expired entries" and
pre-registers dropped Poisoned nodes as a falsifier, but the Stage-B
encoding (`recoveryIsTheDocumentedProjection`) conditions on the
post-recovery `dStatus` and so structurally cannot see a dropped poison
row; Stage C re-introduced the missing clause as its own invariant (see
the footnote on the Stage-B verdict table and the clause-coverage audit
below). All three were confirmed to HOLD on the unmodified as-built model
before any override was run — worker / dual / failover regimes re-checked
exhaustively with bit-identical distinct-state counts
(376,318 / 3,112,250 / 9,228,949) — so none is a new as-built
falsification. They are wired into the corresponding
`quint-retry-policy-*` regime checks; non-vacuity is guarded by the two
wired calibration witnesses that falsify the first and third, and by the
wired TTL-expiry witness plus the evidence-module override
(`retryCalibG4TtlClearKeepsExclusions`) for the second.

**Hash relocation after the harden-subst rebase.** The rebase replayed
only the formal-sprint commits; the corpus commits that predate the
`d79d63368` merge base kept their hashes (verified by ancestry against the
rebased HEAD). Three corpus entries were formal-sprint work and were
relocated by subject: `bfbe07cfa → 0fce3e697`, `473a6df0f → 43a7df620`,
`e2b5be98b → 0745c2ce4`. All other old↔new pairs are identical.

### Calibration table

Classification legend: **ENC** — encodable, override written and run;
**ENC-A** — encodable, covered by the named sibling override (disposition
by analogy within the family, per design §3); **NOT-ENC** — the model
abstracts the mechanism away (the missing dimension is named); **SUBS** —
the fix's subject no longer exists in the integrated tree; **ORIGIN** —
the feature commit that introduced the machinery (no pre-fix defect to
revert). Verdict format: invariant @ step (depth, states generated /
distinct).

#### G1 — counter incremented on the wrong path / not incremented where needed (10 commits)

| Commit (old → new) | Pre-fix behavior reverted | Class | Override module (calibration/retry-g1.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `8283d4362` (unchanged), half a | the I-127 window reset not gated on the event's at-cap outcome (reset wipes at-cap accounting) | ENC | `retryCalibG1WindowResetUngated` | countersRefineHistory | **FALSIFIES** countersRefineHistory @ calibStep (depth 18, 2,649/1,266) |
| `8283d4362` (unchanged), half b | controller-reported at-cap OOM/DiskPressure never cap-checked (loop at ceiling) | ENC | `retryCalibG1ControllerOomUncapped` | boundsOK | **FALSIFIES** boundsOK @ calibStep (depth 15, 50 gen) |
| `172776b1b` (unchanged) | controller-reported DeadlineExceeded had no cap action (loop at cap; the fix introduced today's D1 poison) | ENC | `retryCalibG1DeadlineUncapped` | boundsOK | **FALSIFIES** boundsOK @ calibStep (depth 15, 61 gen) |
| `9c20d04e3` (unchanged) | E4 timeout charge gated on a floor promotion that a cold start never produces (I-200 infinite retry) | ENC | `retryCalibG1TimeoutChargeSkipped` | countersRefineHistory | **FALSIFIES** countersRefineHistory @ calibStep (depth 4, 4 gen) |
| `db457374f` (unchanged), deadline-accounting half | E7 charge gated on the floor outcome (~free ladder rungs before any counting) | ENC-A | covered by `retryCalibG1TimeoutChargeSkipped` (same charge-gated-on-floor-outcome shape, sibling channel) | countersRefineHistory | by analogy (sibling falsified) |
| `db457374f` (unchanged), backstop half | the wedge backstop recorded nothing and quarantined nothing (unbounded wedge loop) | ENC | `retryCalibG1BackstopUncounted` | countersRefineHistory, attemptsBoundedGlobal | **FALSIFIES** countersRefineHistory @ calibStep (depth 4, 7 gen); **FALSIFIES** attemptsBoundedGlobal @ calibStep (depth 8, 22/21); baseline HOLDS both (29/21, depth 9) |
| `db457374f` (unchanged), stream-epoch + heartbeat-binding halves | stale-stream disconnect / heartbeat re-adopt races | NOT-ENC | — (stream epochs and heartbeat machinery outside the model's scope by design) | — | n/a |
| `8a016a393` (unchanged) | at-cap cgroup-OOM double-counted into infra_count (bump + handler) | ENC | `retryCalibG1AtCapOomDoubleCount` | countersRefineHistory | **FALSIFIES** countersRefineHistory @ calibStep (depth 4, 4 gen) |
| `c13f6a277` (unchanged) | floor-promoted transient failures consumed max_retries (I-213, the E1 promotion exemption) | NOT-ENC | — (the floor outcome exists only on OOM-class events in the model and in the reference fold: `processReport` admits `promoted` for the cgroup-OOM class only and the fold's transient event carries no floor outcome, so the pre-fix "a promoting transient failure still charges `count`" behavior is not expressible as a delta of the as-built model and a re-introduction would not falsify any invariant here. Re-dispositioned from ENC-A in the Phase-0 exit review: the previously named covering override (`retryCalibG1DisconnectCharges`) reverts a different entry point (E5) and different counters (`failed_builders`/`failure_count`), sharing only the I-213 incident. Coverage stays with the `handle_transient_failure` promotion-exempt unit tests (`sched.retry.promotion-exempt+4`, `actor/tests/completion.rs`) — the same non-model-vehicle treatment as G6; the Phase-1 choice is in the NOT-ENCODED dimensions list. The disconnect/eviction half of I-213 remains the `8d38cb999` row below.) | — | n/a |
| `8d38cb999` (unchanged) | the disconnect path charged failed_builders / failure_count for floor-promoted evictions (I-213 premature poison) | ENC | `retryCalibG1DisconnectCharges` | countersRefineHistory, verdictMatchesFold | **FALSIFIES** countersRefineHistory @ calibStep (depth 4, 15 gen); **FALSIFIES** verdictMatchesFold @ calibStep (depth 8, 120/96); baseline HOLDS both (993/489, depth 17) |
| `dc094dd0c` (unchanged) | assigned-only disconnects counted toward poison | ENC-A | covered by `retryCalibG1DisconnectCharges`; the Assigned-vs-Running distinction itself is below the model's resolution (DStatus collapses both) | countersRefineHistory | by analogy (shared override falsified) |
| `a60d58a32` (unchanged) | no CONCURRENT_PUTPATH exemption, no 300 s window (I-127 poison at 99.7 %) | ENC | `retryCalibG1PutPathNotExempt` | countersRefineHistory | **FALSIFIES** countersRefineHistory @ calibStep (depth 4, 4 gen) |
| `699ad52e1` (unchanged), exempt-cap root cause | the exempt path had no budget of its own (leaked-store-lock livelock) | ENC | `retryCalibG1ExemptPathUncapped` | countersRefineHistory | **FALSIFIES** countersRefineHistory @ calibStep (depth 4, 4 gen) |

#### G2 — counter splits (2 commits)

| Commit (old → new) | Pre-fix behavior reverted | Class | Override module | Predicted | Verdict |
|---|---|---|---|---|---|
| `a4bcb5623` (unchanged) | retry.count overloaded as per-cycle and cross-cycle gate: resubmit reset neither restores the per-cycle budget nor advances a cycle counter, the resubmit bound never fires | ENC | `retryCalibG2ResubmitSharedCounter` (calibration/retry-g2.qnt) | countersRefineHistory | **FALSIFIES** countersRefineHistory @ calibStep (depth 12, 114/72) |
| `2f07ea909` (unchanged) | the K8s-aware-retry origin feature (cancel signal, failed_workers placement exclusion, disconnect accounting, backstop) | ORIGIN | — no pre-fix defect to revert. Note for Phase 1: the placement exclusion this commit introduced is exactly why the per-cycle transient cap is unreachable under production defaults (the Stage-B finding above) — removing either mechanism re-opens the other's reachability. | — | n/a |

#### G3 — cascade missing / double / hanging build (8 commits)

| Commit (old → new) | Pre-fix behavior reverted | Class | Override module | Predicted | Verdict |
|---|---|---|---|---|---|
| `af0eb62c6` (unchanged) | poison did not cascade DependencyFailed to dependents at all | ENC | `retryCalibG3PoisonWithoutCascade` (calibration/retry-g3.qnt) | cascadeReachesExactlyTheDependents | **FALSIFIES** cascadeReachesExactlyTheDependents @ calibStep (depth 4, 4 gen) |
| `3973a4f54` (unchanged), recovery-cascade half | recovery did not re-seed DependencyFailed for dependents of recovered failure-terminal nodes | ENC | `retryCalibG3RecoveryNoRecascade` | cascadeReachesExactlyTheDependents | **FALSIFIES** cascadeReachesExactlyTheDependents @ calibStep (depth 12, 130/99 — poison, lost clear_poison_batch write, failover, dependent left Ready) |
| `5b4543c3a` (unchanged), transitive-depfailed half | recovery-time cascade not persisted for depth-≥2 ancestors | ENC-A / NOT-ENC | the recovery-re-cascade behavior is covered by `retryCalibG3RecoveryNoRecascade`; the depth-≥2 / per-dependent persistence half is NOT-ENC (one dependent, no per-dependent durable row) | cascadeReachesExactlyTheDependents | by analogy (shared override falsified) |
| `891a6520d` (unchanged), build-summary half | poisoned drvs missing from recovery's id_to_hash → spurious Succeeded in the build summary | NOT-ENC | — (build-level summary accounting not modeled); the poison-set-preservation half of the same commit is the G8 row below | — | n/a |
| `d91df7e9f` (unchanged) | DAG-removal paths forgot derivation_hashes pruning → keep_going build hung | NOT-ENC | — (build-level totals/derivation_hashes not modeled) | — | n/a |
| `e45f2d966` (unchanged), dep-failed-seed half | merge-time transitive DependencyFailed seeding at depth > 1 missing | NOT-ENC | — (no merge action, single dependent at depth 1) | — | n/a |
| `33b1f855c` (unchanged) | cascaded dependency failures didn't finalize retained exec logs | SUBS | — the in-scheduler log-buffer/`drv_logs` machinery this patched was deleted by harden-logs (LogService owns logs; 067_drop_drv_logs) | — | n/a |
| `699ad52e1` (unchanged), drv_name-cascade-key part | the cascade walk keyed by name instead of hash | NOT-ENC | — (derivation identity is structural in the model; no name/hash distinction) | — | n/a |

#### G4 — poison state desynced between memory and PG, or never cleared (8 commits)

| Commit (old → new) | Pre-fix behavior reverted | Class | Override module | Predicted | Verdict |
|---|---|---|---|---|---|
| `b874e5120` (unchanged) | ClearPoison ran in-mem first, PG second best-effort → a PG blip left the stores permanently disagreeing | ENC | `retryCalibG4ClearInMemFirst` (calibration/retry-g4.qnt) | clearedPoisonClearsDurably (new) | **FALSIFIES** clearedPoisonClearsDurably @ calibStep (depth 5, 6 gen). Disposition of the prior gap: unstated property → invariant added to the main model, HOLDS on the unmodified worker/dual/failover regimes (state counts unchanged), wired into those checks |
| `f9adf3c76` (unchanged) | poison expired during downtime reloaded anyway, with poisoned_at re-stamped to now (fresh 24 h TTL) | ENC | `retryCalibG4ReloadExpiredPoison` | recoveryIsTheDocumentedProjection | **FALSIFIES** recoveryIsTheDocumentedProjection @ calibStep (depth 10, 166/123) |
| `7078da256` (unchanged) | poisoned nodes reset in place on clear/TTL → recovered stub fields (empty outputs/features) wedged the resubmit | NOT-ENC | — (derivation metadata fields are not modeled; the post-fix removal IS the modeled behavior) | — | n/a |
| `b09c5b312` (unchanged), X6 half | reassign_derivations had no threshold check (3 disconnects → deferred forever, pre-2f07ea909-era accounting) | ENC | probe `retryCalibG4DisconnectThresholdProbe` (as-built step, two slots, worker+pod-death+crash+wedge alphabet, no failover, no PG faults) | expected HOLDS | **HOLDS** noDisconnectThresholdPoison @ as-built step (exhaustive, 38,980,303/12,146,371, depth 26). Disposition (narrowed in the Phase-0 exit review): the **disconnect/force-drain-triggered arm never fires as-built** — the worker-reported charging sites poison at record time (record-then-check) and post-I-213 disconnects charge nothing — but the same `should_poison` block in `reassign_derivations` is the ONLY threshold gate on the backstop (E8) path (`tick_process_backstop_timeouts` records `failed_builders`/`failure_count` and explicitly delegates the poison decision to `reassign_derivations`) and also serves the force-drain path; the model encodes that delegated check inside `backstopFires` under OE8, so the OE5-scoped probe is structurally blind to it, and the probe ran with MAX_FAILOVERS=0 / PG_FAULTS=0 (no failover, no lost-PG-write histories). HOLDS therefore means "the disconnect-triggered arm is unreachable in today's structure", not "the code-level check is dead". Deletion is licensed only if the Phase-1 collapse routes the E8 exit verdict through `decide()` (or an equivalent threshold check at the E8 charging site) AND the lost-`persist_poisoned`-then-failover history class is explicitly dispositioned; settling C2 only determines whether the disconnect arm becomes load-bearing again. See the Phase-1 input list. |
| `b09c5b312` (unchanged), X13 half | poison-TTL clear left PG failed_workers (and retry_count) behind — the in-memory reset and the durable row disagreed about the next cycle's starting exclusion set, and a post-clear crash recovered the stale set | ENC | `retryCalibG4TtlClearKeepsExclusions` | clearedPoisonScrubsExclusions (new) | **FALSIFIES** clearedPoisonScrubsExclusions @ calibStep (depth 8, 29/23). Disposition of the prior gap: unstated property → invariant added to the main model as a sibling of `clearedPoisonClearsDurably`, HOLDS on the unmodified worker/dual/failover regimes (state counts unchanged), wired into those checks; the override stays an evidence module (G4's wired representative remains the clear-ordering check; the antecedent's reachability is already pinned by the wired TTL-expiry witness). Re-dispositioned from NOT-ENC in the Phase-0 exit review: the mechanism was representable all along (`pg.failedBuilders` exists and the as-built TTL clear writes a fresh row); only the invariant clause was missing — the same treatment `b874e5120` received — and the previously cited dimension (the post-clear re-merge; DAbsent is a sink) is the harm's downstream reader, not the mechanism. |
| `84a692492` (unchanged) | transient retry persisted Failed instead of Ready → post-recovery hang in the backoff window | NOT-ENC | — (a model-encoding collapse, not a design scope-out: the durable status column exists (`PgRow.status`) and `persist_status` is one of the fault-able mirror writes, but `PgNonTerminal` and `DStatus` both merge the Ready/Failed taxonomy inside non-terminal rows, and the recovery queue-push distinction is not modeled; the harm is also liveness-shaped (a hang, not a wrong charge). Covered by the Phase-1a ledger-schema work — see the recovered-node-metadata bullet in the NOT-ENCODED dimensions list) | — | n/a |
| `cbda4119a` (unchanged) | poison_and_cascade on an unexpected state still wrote Poisoned to PG and cascaded | NOT-ENC | — (the in-mem transition-guard failure mode is not modeled; the fix itself was defense-in-depth for a state all callers already excluded) | — | n/a |
| `ea36f98f2` (unchanged) | poison persistence wrote bytes not text | NOT-ENC | — (SQL/serialization encoding) | — | n/a |
| `01faf80b7` (unchanged) | reset-from-poison kept a stale traceparent | NOT-ENC | — (tracing metadata) | — | n/a |

#### G5 — the same dead executor counted twice (4 commits)

| Commit (old → new) | Pre-fix behavior reverted | Class | Override module | Predicted | Verdict |
|---|---|---|---|---|---|
| `ee9302b86` (unchanged) | the race-ahead termination report did not set last_completed → the disconnect re-inserted the dedup entry and the controller's re-report charged the same death again | ENC | `retryCalibG5RaceAheadKeepsPending` (calibration/retry-g5.qnt) | noDoubleCount | **FALSIFIES** noDoubleCount @ calibStep (depth 15, 63/55); the documented incident shape (race-ahead charge → disconnect inserts entry → re-report charges again) is pinned by the module's `g5RaceAheadDoubleCountRun` |
| `e872b2b49` (unchanged) | a non-promoting termination report consumed the recently_disconnected entry before the reason gate → the same-tick DeadlineExceeded report found nothing (controller-side timeout backstop structurally defeated) | ENC | `retryCalibG5NonPromotingConsumesEntry` | local invariant `pendingReportKeepsItsEntry` | **FALSIFIES** pendingReportKeepsItsEntry @ calibStep (depth 16, 19 gen); baseline HOLDS (61/54, depth 16). The downstream unbounded-wedge consequence is fairness-dependent (a report that simply never arrives produces the same uncounted cycle as-built), so the calibration pins the structural conservation property instead |
| `c5c5ccd17` (unchanged) | reassign_derivations not leader-gated → a deposed leader poisoned/re-queued from a stale DAG | NOT-ENC | — (a second, deposed leader acting concurrently is outside this model; the lease fence and its calibration live in leaderElection.qnt and the rio-lease campaign) | — | n/a |
| `db457374f` (unchanged), stream-epoch half | a late disconnect from the previous stream removed the freshly-reconnected worker | NOT-ENC | — (stream epochs outside the model's scope by design) | — | n/a |

#### G6 — floor ladder vs retry budget (9 commits)

`2acd1b327`, `c55467cbc`, `37c21bb7b`, `1184d1bb8`, `775f19023`,
`a76589e37`, `12b86c285`, `79fa0dbbc`, `2f150c585` (all unchanged):
**NOT-ENCODED**, exactly as pre-registered in design §3 and the model
header — the floor ladder enters the model only as the `{promoted, at_cap}`
outcome each OOM-class event consumes (a bounded promotion budget stands in
for the ladder), so which signals trigger a promotion, the ladder's
persistence/hydration (`79fa0dbbc`), the configuration plumbing
(`a76589e37`), the deadline alignment (`12b86c285`) and the at-cap
comparison baseline (`2f150c585`) are all inside the abstracted oracle.
Coverage stays with `floor.rs`'s unit tests. The charging consequences of
floor outcomes on OOM-class, controller-reported and disconnect events
(what a promoted / at-cap infra attempt charges, and what the pre-I-213
disconnect path charged for promoted evictions) ARE in the model and are
calibrated through the G1 rows above (the at-cap double-count, the
window-reset gate, the promoted-eviction accounting). The floor outcome on
worker-reported *transient* (E1) attempts — the I-213 promotion exemption
from `max_retries` — is NOT in the model or the fold (the `c13f6a277` row
above and the NOT-ENCODED dimensions list below).

#### G7 — fleet-exhaust / placement (3 commits)

| Commit (old → new) | Pre-fix behavior reverted | Class | Override module | Predicted | Verdict |
|---|---|---|---|---|---|
| `a62631c90` (unchanged) | the exhaust check filtered by kind only → mismatched-system workers padded the fleet and a drv deferred forever | NOT-ENC | — (a model-time narrowing of design §3's pre-registered G7 encoding, not a design scope-out: G7 was pre-registered encodable via "the eligibility/draining predicate", and the model implemented only the draining/registered half — `eligibleFleet` keeps kind/system/features uniform across slots, so a static-eligibility mismatch is not representable. The harm is also liveness-shaped (a defer-forever, not a wrong charge). Re-evaluate if Phase-1 placement work adds heterogeneous eligibility — see the NOT-ENCODED dimensions list) | — | n/a |
| `699ad52e1` (unchanged), draining-exclusion root cause | the exhaust check counted draining workers as eligible → a one-shot pool poisoned on its first failure instead of deferring | ENC | `retryCalibG7ExhaustCountsDraining` (calibration/retry-g7.qnt) | noFleetExhaustPoison (as invariant on the restricted no-respawn alphabet) | **FALSIFIES** noFleetExhaustPoison @ calibStep (depth 7, 22/18); baseline HOLDS (19/15, depth 6) — the as-built empty-fleet defer of sched.dispatch.fleet-exhaust+3 |
| `c03d52787` (unchanged) | a resubmitted build joining a pre-existing poisoned node hung instead of failing fast | NOT-ENC | — (multi-build merge interaction; build-level) | — | n/a |

#### G8 — failover loses or fabricates attempt history (6 commits)

| Commit (old → new) | Pre-fix behavior reverted | Class | Override module | Predicted | Verdict |
|---|---|---|---|---|---|
| `891a6520d` (unchanged), poison-set half | the commit's two poison-set mechanisms: (a) `load_poisoned_derivations` skipped `status='poisoned'` rows whose `poisoned_at` was NULL — the crash window of the old non-atomic `persist_status` + `set_poisoned_at` two-call sequence, closed by the atomic `persist_poisoned` — and (b) the `dag::remove_build` reap deleted recovered-poisoned nodes (`interested_builds` empty from birth) on the first build completion after recovery | ENC | `retryCalibG8PoisonedRowNotReloaded` (calibration/retry-g8.qnt) — abstracts both mechanisms as "the poisoned row is not reloaded at failover": a family-level shape anchored on the commit at model resolution, not a literal revert (the model's PG poison write is a single atomic step with no NULL-timestamp window, and it has no build-completion reap; the pre-fix DAG did reload timestamped poisoned rows). The check does NOT cover the commit's id_to_hash/build-summary half (NOT-ENC — the G3 row above) nor the remove_build-reap interaction itself (named in the NOT-ENCODED dimensions list) | recoveryPreservesPoisonStatus (new) | **FALSIFIES** recoveryPreservesPoisonStatus @ calibStep (depth 6, 14/13). Disposition of the prior gap: Stage-B encoding gap (the clause is design-stated, see the calibration intro) → invariant added to the main model, HOLDS on the unmodified failover regime (state count unchanged), wired into that check |
| `5b4543c3a` (unchanged), recovery halves | wrong recovered failed-count / dropped recovery cascade | ENC-A | the cascade half is `retryCalibG3RecoveryNoRecascade` (falsified above); the reconstruction half is the family-level override below | — | by analogy |
| family-level reconstruction row (anchors: `5b4543c3a`, `891a6520d`, the from_poisoned_row gap recorded in `a4bcb5623`'s message) | failure_count not derived from the persisted exclusion set at recovery | ENC | `retryCalibG8FailureCountNotDerived` | recoveryIsTheDocumentedProjection | **FALSIFIES** recoveryIsTheDocumentedProjection @ calibStep (depth 6, 14/13) |
| `f9adf3c76` (unchanged) | expired poison reloaded with a fresh TTL | ENC | (G4 row above — same commit, listed in both families by the inventory) | recoveryIsTheDocumentedProjection | **FALSIFIES** (see G4) |
| `bfbe07cfa → 0fce3e697` | recovery retained the entry generation on an unclaimed PG floor tie | NOT-ENC | — the lease generation fence is outside retryPolicy.qnt (assume–guarantee with the leader-election layer); the behavior is modeled and checked by leaderElection.qnt's deletion/pg-faults regimes and witnesses | — | n/a (covered by the rio-lease campaign's checks) |
| `473a6df0f → 43a7df620` | recovery ungated dispatch without flooring/claiming the generation when the DAG load failed | NOT-ENC | — same scope note as above | — | n/a (covered by the rio-lease campaign's checks) |
| `e2b5be98b → 0745c2ce4` | the floor-unreadable fallback skipped the post-claim confirmation | NOT-ENC | — same scope note as above | — | n/a (covered by the rio-lease campaign's checks) |

### HOLDS rows and their dispositions

Only one calibration target returned HOLDS where a falsification could
have been expected, and it was predicted: the `b09c5b312` X6 probe
(E5's poison-threshold re-check) — carrying the narrowed disposition
recorded in its table row: the disconnect/force-drain-triggered arm never
fires as-built (the probe transcript is the machine-checked evidence for
that claim, over the probe's restricted alphabet — OE5-scoped, no
failover, no lost PG writes), while the same code-level check remains the
only threshold gate on the backstop (E8) path and the force-drain path,
so it is NOT a dead-code finding; the G1 disconnect-charges override is
the demonstration of the historical world in which the disconnect arm
itself was load-bearing. Every other
override falsified its predicted invariant on the first run; the
restricted-alphabet baselines (backstop, disconnect-charges,
non-promoting-consumes, exhaust-draining) all HOLD as required for the
falsifications to be attributable to the reverted behavior rather than to
the alphabet restriction. No new invariant falsified on the unmodified
model (no stop-and-report event).

### Permanent expect-violation witnesses (wired into nix/quint.nix)

Six of the twenty override modules are wired as `quint-retry-calib-*`
checks — one representative per family that has a plausible regression
path in the as-built code and a cheap state space (the same ~5-of-22
proportion the log campaign kept):

| Check | Module | Violated invariant | Guards against |
|---|---|---|---|
| `quint-retry-calib-g1-controller-cap` | `retryCalibG1ControllerOomUncapped` | `boundsOK` | losing the E6 at-cap infra cap check (8283d4362) |
| `quint-retry-calib-g2-resubmit-split` | `retryCalibG2ResubmitSharedCounter` | `countersRefineHistory` | re-merging the per-cycle and cross-cycle counters (a4bcb5623) |
| `quint-retry-calib-g3-cascade` | `retryCalibG3PoisonWithoutCascade` | `cascadeReachesExactlyTheDependents` | decoupling the cascade from the poison transition (af0eb62c6) |
| `quint-retry-calib-g4-clear-ordering` | `retryCalibG4ClearInMemFirst` | `clearedPoisonClearsDurably` | reverting the PG-first clear ordering (b874e5120); also the new invariant's non-vacuity guard |
| `quint-retry-calib-g5-race-ahead` | `retryCalibG5RaceAheadKeepsPending` | `noDoubleCount` | weakening the race-ahead/last_completed dedup (ee9302b86) |
| `quint-retry-calib-g8-poison-reload` | `retryCalibG8PoisonedRowNotReloaded` | `recoveryPreservesPoisonStatus` | the poison set failing to survive failover (the family-level shape, anchored on 891a6520d's poison-set half — the `poisoned_at`-IS-NULL load filter and the `remove_build` reap, abstracted at model resolution as "not reloaded"); also the new invariant's non-vacuity guard |

The remaining fourteen modules are evidence modules: committed, typechecked
with the tree, re-runnable with the command in `calibration/README.md`,
not in CI.

### Phase-0 exit-gate verdict

**Met — as corrected by the Phase-0 exit review (correction pass landed
2026-05-25).** The verdict rests on three things, all now on record: the
falsification record, which the exit review confirmed and the correction
pass did not touch; the corrected dispositions and override attributions
(the c13f6a277 re-disposition to NOT-ENC, the 891a6520d re-attribution to
the poison-set mechanisms, the b09c5b312-X6 narrowing, the X13 encode,
the recoveryPreservesPoisonStatus relabel and the design-§3
clause-coverage audit); and the two spec adjudications the design's §5
gate required, both made and recorded (the `sched.retry.failover-budget`
rule — budgets survive failover — and the C2 charging adjudication; see
the Phase-1 input list).

Every one of the eight families either falsifies at least one
invariant through a representative override (G1: 8 overrides falsify; G2:
1; G3: 2; G4: 3; G5: 2; G7: 1; G8: 3 — all as predicted; the G4 count
includes the X13 override added by the exit review's correction pass), or
carries an
explicit disposition for every commit: G6 is NOT-ENCODED exactly as
pre-registered in design §3 (the floor ladder is priced out of this model;
coverage stays with floor.rs's unit tests), and the per-commit NOT-ENCODED
/ SUBSUMED rows inside the other families name their missing dimension and
what covers them instead. The single ENCODABLE-but-HOLDS row
(b09c5b312-X6) carries the narrowed disposition — the disconnect-triggered
arm is unreachable as-built, while the same code-level check stays
load-bearing for the backstop (E8) and force-drain paths — with
machine-checked evidence for the disconnect-arm claim only.
The invariant list that survives calibration — the eight design
invariants plus `durableMirrorsCharges`, `clearedPoisonClearsDurably`,
`clearedPoisonScrubsExclusions`, `recoveryPreservesPoisonStatus`, and the
per-event charge discipline — is
the replacement's contract going into Phase 1, together with the two
adjudications above and the FailoverPreservesHistory acceptance rule.


---

## 2. Phase 2 — the acceptance table: the historical fix corpus against the post-collapse architecture

> Origin: retry-invariant-map.md § "The acceptance table" (G1-G8 + summary),
> verbatim. This table is the license for deleting the as-built model, the
> calibration corpus, and the six quint-retry-calib-* checks.

### The acceptance table: the historical fix corpus against the post-collapse architecture

Design §4's closing assurance item: "the 45-fix families each get a
'cannot recur by construction' or 'checked by invariant X' verdict."
The Stage-C calibration table above proved the *model* would re-find
each encodable bug in the *as-built* code; this table records, for the
same corpus, what holds the bug class down in the *post-collapse* code
— the architecture every row below now runs on: no site mutates a
counter (the counters are `decide()`'s fold of the durable suffix,
refreshed per append), every charge/verdict/status persist commits or
fails as one appending transaction, classification happens once at
append time (`classify()`), placement consumes the fold's exclusion set
(`placeable()`), recovery rebuilds the view from the same fold, and the
decision arithmetic carries machine-checked Kani contracts
(`kani-rio-retry-kernel`, merge-gated).

Verdict legend, following the log campaign's table: **CONSTRUCTION** —
the state or code path the bug lived in does not exist post-collapse;
the cited mechanism is what replaced it (the residual risk for every
such row is a defect in the shared fold/classifier itself, owned
jointly by the Kani contracts, the fold unit battery, and the
`quint-retry-policy-*` regimes). **CHECKED(...)** — the mechanism is
still live (deliberately kept); the named invariant / Kani harness /
test holds the hazard down. **OUTSIDE** — no footprint in the collapsed
decision path then or now; the named conventional vehicle owns it,
unchanged by this campaign. Kani harness citations refer to the
machine-checked contracts (the merge-gated `kani-rio-retry-kernel`
check — see the assurance-layer verification status above); they are
cited as the precise property statements and are never a row's sole
checker.

#### G1 — counter incremented on the wrong path / not incremented where needed

The family-level verdict is CONSTRUCTION: there is no per-site
increment left to put on a wrong path. Each entry point classifies and
appends; which budget an event charges is decided exactly once, in
`apply()`'s arm for that event class, and `countersRefineHistory` is an
identity in the post-collapse encoding (the cached view, the durable
fold and the spec ghost advance together). Per-row:

| Corpus row | Post-collapse verdict | Mechanism / checker |
|---|---|---|
| `8283d4362` half a (window reset wiped at-cap accounting) | CONSTRUCTION | The I-127 window reset exists in exactly one place — the fold's infra arm, gated on the event's own `at_cap` — and no site re-implements it. Pinned by the fold window-reset unit tests (incl. the exempt fall-through) and the `quint-retry-policy-witness-window-reset` / `-exempt-fallthrough` reachability witnesses. |
| `8283d4362` half b (controller at-cap OOM never cap-checked) | CONSTRUCTION | E6's at-cap charge is the same fold arm E2 uses; a per-channel cap divergence has no site to live in. `check_decide_contract` proves `infra_count ≤ max_infra_retries` over every history; D2's anchor stamp is the same arm (flipped to HOLD in the dual regime). |
| `172776b1b` (E7 had no cap action; its fix introduced the D1 poison) | CONSTRUCTION | The timeout cap has one terminal arm: `Cancel` at the cap on both channels (T-1b.10, the D1 adjudication). `check_decide_contract`'s partition clause ties `Cancel` to the exhausted timeout budget; `verdictMatchesFold` HOLDs in the dual regime; `d1ControllerTimeoutCapCancelsRun` and the E7 cap tests pin the concrete history. The 172776b1b livelock cannot return: the cap arm is terminal by construction. |
| `9c20d04e3` (E4 charge gated on a promotion a cold start never produces) | CONSTRUCTION | The fold's worker-timeout arm charges unconditionally; `classify()` never consults the floor for timeouts (kani `check_classify_contract`). |
| `db457374f` deadline-accounting half (E7 charge gated on the floor outcome) | CONSTRUCTION | Same mechanism as `9c20d04e3`, controller channel (`ControllerDeadlineExceeded` arm charges unconditionally below the cap). |
| `db457374f` backstop half (the wedge backstop recorded nothing) | CONSTRUCTION + CHECKED(`attemptsBoundedGlobal`, crash regime) | E8 appends a `backstop` row and routes its verdict through `decide()` at its own site (T-1b.6); the charge is durable by the appending transaction (`durableMirrorsCharges` identity). |
| `db457374f` stream-epoch + heartbeat-binding halves | OUTSIDE | Executor-lifecycle machinery, untouched by this campaign; transfers to the executor-lifecycle campaign (#1). |
| `8a016a393` (at-cap OOM double-counted by floor bump + handler) | CONSTRUCTION | `observe_resource_floor` still mutates no counter; the handler's charge is one fold arm over one ledger row, and one execution can have at most one attempt row (the 066 `exec_id` partial unique index — the second installment is an UPDATE). `noDoubleCount` stays a live invariant (dual regime). |
| `c13f6a277` (I-213: floor-promoted transients consumed `max_retries`) | OUTSIDE (unchanged vehicle, P4) | The exemption stays infra-class only; `classify()` never marks a transient exempt — now also a Kani-proven clause of `check_classify_contract` — and behavioral coverage stays with `test_transient_failure_promotion_exempt_from_max_retries` (`sched.retry.promotion-exempt+4`). |
| `8d38cb999` (disconnect path charged floor-promoted evictions) | CONSTRUCTION | A bare disconnect appends a `disconnected` row that charges nothing (fold's disconnect arm); only an established crash charges (C2/P1), and establishment requires the classification window to have closed empty. Crash-regime `attemptsChargedOnce` + executor tests. |
| `dc094dd0c` (assigned-only disconnects counted toward poison) | CONSTRUCTION | Same mechanism — the disconnect arm charges nothing regardless of Assigned/Running. |
| `a60d58a32` (no CONCURRENT_PUTPATH exemption, no 300 s window) | CONSTRUCTION | Both are single fold/classifier arms now: the exemption is `classify()`'s predicate (kani-contracted on both channels), the window is the fold's infra arm. Reachability pinned by the window-reset and exempt-cap witnesses. |
| `699ad52e1` exempt-cap root cause (the exempt path had no budget) | CONSTRUCTION | The exemption's own budget is one fold arm (increment-then-check); `check_decide_contract` ties `Poison(ExemptInfraBudget)` to the exhausted cap and proves an exempt charge at the cap never requeues. |

#### G2 — counter splits

| Corpus row | Post-collapse verdict | Mechanism / checker |
|---|---|---|
| `a4bcb5623` (per-cycle vs cross-cycle conflated) | CONSTRUCTION | `count` is per-cycle by the suffix cut (the loader returns rows at-or-after the most recent durable reset row) and `resubmit_cycles` advances only on durable resubmit-reset rows; no site owns either, so they cannot be re-merged at a site. Pinned by the fold resubmit-reset unit tests and the recovery test that the resubmit bound survives failover. |
| `2f07ea909` (ORIGIN: K8s-aware retry feature) | n/a (origin) | The placement exclusion it introduced is now `placeable()` over the fold's exclusion set + `hard_filter` over the fold-derived view (kani `check_placeable_contract`); the P3 keep-and-document note (the transient cap is defaults-shadowed by exactly this exclusion) is recorded at the cap check in `retry_policy.rs`. |

#### G3 — cascade missing / double / hanging build

The cascade deliberately stayed an actor-side graph operation
downstream of a `Poison` verdict (design §4); it is CHECKED, not
by-construction.

| Corpus row | Post-collapse verdict | Mechanism / checker |
|---|---|---|
| `af0eb62c6` (poison did not cascade at all) | CHECKED(`cascadeReachesExactlyTheDependents`, all regimes) | Cascade rows are appended in the same transaction as the trigger's poison row; `sched.poison.cascade-dependents` carries the runtime obligation. |
| `3973a4f54` (recovery did not re-cascade) | CHECKED(`cascadeReachesExactlyTheDependents`, failover regime) | `sched.recovery.failed-dep-cascade` + the recovery-cascade tests. |
| `5b4543c3a` transitive-depfailed half (depth ≥ 2 persistence) | OUTSIDE | Build/DAG-level bookkeeping; keep-going and recovery test suites. |
| `891a6520d` build-summary half (spurious Succeeded) | OUTSIDE | Build-summary accounting; `sched.recovery.poisoned-failed-count` + recovery tests. |
| `d91df7e9f` (derivation_hashes pruning, hung keep-going build) | OUTSIDE | Build-level totals; keep-going tests. |
| `e45f2d966` dep-failed-seed half (merge-time transitive seeding) | OUTSIDE | `sched.merge.dep-failed-transitive` + merge tests. |
| `33b1f855c` (cascade didn't finalize retained exec logs) | SUBS | Subject deleted by the log campaign (LogService owns logs; 067_drop_drv_logs). |
| `699ad52e1` drv_name-cascade-key part | OUTSIDE | Identity plumbing; the cascade walk keys on the DAG node, covered by the cascade tests. |

#### G4 — poison state desynced between memory and PG, or never cleared

The family-level mechanism: there is no second store for the budget
counters to desync against — the mirror writers are gone, and the
charge, the verdict and the status persist commit or fail as one
transaction (`attemptTxFails` in the model is the only failure shape
left, and it charges nothing).

| Corpus row | Post-collapse verdict | Mechanism / checker |
|---|---|---|
| `b874e5120` (clear ran in-mem first, PG best-effort) | CONSTRUCTION + CHECKED(`clearedPoisonClearsDurably`) | Poison clears are durable rows written PG-first inside the reset transaction (`clear_poison_in_tx` joins the reset row's transaction); the invariant stays live in the worker/dual/failover regimes. |
| `f9adf3c76` (expired poison reloaded with a fresh TTL) | CHECKED(`recoveryPreservesPoisonStatus` + the recovery TTL filter) | Recovery still filters expired poisons; `quint-retry-policy-witness-ttl-expiry` pins the expiry as reachable; recovery tests pin the reload behavior. |
| `7078da256` (reset-in-place left stub fields) | CONSTRUCTION | Poisoned nodes are removed and re-merged fresh on clear (the post-fix behavior is the only behavior); resubmit/merge tests. |
| `b09c5b312` X6 half (E5 threshold re-check) | CHECKED (kept by decision P2) | The re-check is now a `decide()` caller over the durable suffix + seed — the same fold as everywhere else — serving the disconnect/force-drain re-poison path and the post-failover backstop for a lost poison persist. |
| `b09c5b312` X13 half (TTL clear left PG exclusions behind) | CHECKED(`clearedPoisonScrubsExclusions`) | The clear scrubs the durable exclusion state in the same transaction; the legacy column is also zeroed by the same clear (frozen-column hygiene until the Phase-2 drop). |
| `84a692492` (transient retry persisted Failed instead of Ready) | CHECKED | The status persist is part of the appending transaction whose verdict it reflects, and recovery re-runs `decide()` for every non-terminal derivation and enforces the verdict (T-1b.12b), so a wrong persisted status converges at the next recovery instead of wedging. Recovery enforcement tests. |
| `cbda4119a` (poison_and_cascade on an unexpected state) | OUTSIDE | Defense-in-depth transition guard, unchanged; state-machine tests. |
| `ea36f98f2` (poison persisted bytes not text) | OUTSIDE | SQL serialization; sqlx compile-checked queries + migration tests. |
| `01faf80b7` (reset kept a stale traceparent) | OUTSIDE | Tracing metadata; observability tests. |

#### G5 — the same dead executor counted twice

The dedup/correlation machinery was deliberately NOT deleted (design
non-goal); the family is CHECKED, with one schema-level CONSTRUCTION
half: one execution can have at most one attempt row.

| Corpus row | Post-collapse verdict | Mechanism / checker |
|---|---|---|
| `ee9302b86` (race-ahead report didn't suppress the re-report) | CHECKED(`noDoubleCount`, dual regime) + CONSTRUCTION (schema half) | The 066 `exec_id` partial unique index makes the controller's installment an UPDATE on the disconnect's row, never a second row; the live dedup state is still modeled and still checked. |
| `e872b2b49` (non-promoting report consumed the correlation entry) | CHECKED | The two-installment correlation is keyed by the released `exec_id` carried in `recently_disconnected`; a non-promoting report deliberately does not establish (P1), preserving the classification window — the property the calibration's `pendingReportKeepsItsEntry` stated is now structural in the row lifecycle and exercised by `lateInstallmentAfterRedispatchRun` + the executor-termination tests. |
| `c5c5ccd17` (deposed leader still poisoned/requeued) | OUTSIDE | The lease fence; owned by `leaderElection.qnt` and the rio-lease campaign (assume–guarantee). |
| `db457374f` stream-epoch half (late disconnect removed the reconnected worker) | OUTSIDE | Executor-lifecycle campaign (#1). |

#### G6 — floor ladder vs retry budget

All nine commits: OUTSIDE (unchanged vehicle), exactly as
pre-registered. The ladder itself (`floor.rs`) is untouched by the
campaign — zero lines changed — and its internals stay with its unit
tests. What the campaign did change is the *charging consequence* of a
floor outcome: it is consumed once, at append time, by `classify()`
(kani-contracted on both channels), so the G1-calibrated charging bugs
around promoted/at-cap outcomes are covered by the rows above, and a
new divergence between channels would have to be introduced inside the
single classifier rather than at two sites.

#### G7 — fleet-exhaust / placement

| Corpus row | Post-collapse verdict | Mechanism / checker |
|---|---|---|
| `a62631c90` (exhaust check filtered by kind only) | CHECKED (split) | The exhaust *predicate* is `placeable()` over (exclusion × eligible fleet) — kani `check_placeable_contract` — but the *eligibility computation* feeding it (kind/system/features) is still dispatch-time code owned by the dispatch tests; heterogeneous-eligibility modeling stays a named NOT-ENC dimension and transfers to the executor-lifecycle campaign. |
| `699ad52e1` draining-exclusion root cause (draining workers padded the fleet) | CHECKED(`placementSound` + kani + `test_fleet_exhaustion_defers_under_one_shot`) | The eligible set excludes draining workers at the call site; the empty-fleet defer is normative (`sched.dispatch.fleet-exhaust+3`) and proven for the predicate by `check_placeable_contract` / `check_fold_fleet_exhaust_arm`. |
| `c03d52787` (resubmitted build joined a poisoned node and hung) | OUTSIDE | Build-level merge interaction; merge/keep-going tests. |

#### G8 — failover loses or fabricates attempt history

The family-level verdict is CONSTRUCTION: the history is durable and
recovery rebuilds the view by running the same fold over it
(`failoverPreservesHistory`, first checked at T-1c.3, HOLDs in the
failover regime); the selective forgiveness this family's bugs lived in
no longer exists.

| Corpus row | Post-collapse verdict | Mechanism / checker |
|---|---|---|
| `891a6520d` poison-set half (NULL-timestamp window; remove_build reap) | CHECKED(`recoveryPreservesPoisonStatus`, failover regime) | The poison stamp is atomic with its transaction; recovery preserves unexpired poisons; the reap-interaction at code resolution stays with the recovery tests (the NOT-ENC note carries forward). |
| `5b4543c3a` recovery halves (wrong recovered failed-count / dropped cascade) | CHECKED(`failoverPreservesHistory` + `cascadeReachesExactlyTheDependents`) | The recovered view is the fold of the reloaded suffix (+ legacy seed); the A-1a-4 recovery battery asserts the suffix reloads identically across a flap. |
| family-level reconstruction row (`failure_count` not derived from durable evidence) | CONSTRUCTION | There is no reconstruction left: the recovered counters ARE the fold of durable rows. The lossy projection survives only as the legacy seed's degenerate case, and `check_decide_contract` / `check_legacy_seed_merge_monotone` prove the merge never drops below what the columns support. `recoveryNeverFabricatesFailures` stays live. |
| `f9adf3c76` (expired poison reloaded) | CHECKED | Same row as G4 above. |
| `0fce3e697`, `43a7df620`, `0745c2ce4` (lease generation floor / claims) | OUTSIDE | Owned by `leaderElection.qnt`, the rio-lease Kani contract, and `mbt-rio-lease` (assume–guarantee composition). |

#### Summary

Of the corpus rows above: the two dominant families the design §1
called out — "counter incremented on the wrong path" (G1) and "the same
dead pod counted twice" (G5's charge half) — are closed by
construction: the first because per-site increments no longer exist,
the second because one execution can produce at most one attempt row
and the second observer's installment is an UPDATE. The G4
memory-vs-PG desync family is closed by the appending transaction (one
commit point for charge + verdict + status). The G8 failover family is
closed by the durable history plus the seeded recovery fold. The
mechanisms that deliberately survived — the cascade walk, the dedup/
correlation layer, the E5 re-check, recovery's poison-TTL filter, the
placement eligibility computation — are each named CHECKED above with
the invariant, Kani harness, or test that owns them. The rows that were
never in the decision path (build-level bookkeeping, serialization,
tracing metadata, the floor ladder's internals, the lease fence) keep
their existing vehicles, unchanged.


---

## 3. Retirement of the as-built model and the calibration corpus

> Origin: retry-invariant-map.md § "Retirement of the as-built model and the
> calibration corpus", verbatim.

### Retirement of the as-built model and the calibration corpus

Design §5's Phase-2 row and the Phase-2 hand-off both schedule the
retirement once the acceptance table exists; with the table above in
place, the frozen Stage-B encoding (`retryPolicyAsBuilt.qnt`), the
Stage-C override corpus (`docs/spec/models/calibration/retry-*.qnt` +
its README), and the six `quint-retry-calib-*` checks are deleted. The
post-collapse `retryPolicy.qnt`, its regime/witness/named-run checks,
and the `kani-rio-retry-kernel` contracts are the live verification stack;
the calibration table and the Stage-B/Stage-C sections above are the
surviving record of what the corpus demonstrated (the per-override
verdicts, depths and state counts remain valid statements about the
retired artifacts as they existed at the Phase-1c freeze, and the
corpus remains recoverable from git history if a future campaign wants
to replay it).

What the retired checks guarded and where that duty now lives:

- The six per-family regression guards existed to prove the *as-built
  model* would re-find each family's representative bug. The as-built
  code they encoded no longer exists; the post-collapse architecture's
  protection per family is the acceptance table above (construction
  mechanisms + the live invariants + the Kani contracts).
- Two of the six doubled as non-vacuity anchors for invariants that
  remain live in the post-collapse model: `clearedPoisonClearsDurably`
  / `clearedPoisonScrubsExclusions` (the G4 check) and
  `recoveryPreservesPoisonStatus` (the G8 check). The clear-discipline
  antecedent stays pinned by the wired TTL-expiry and cache-hit
  witnesses (a clear is reachable). The failover-on-a-poisoned-row
  antecedent had no other wired pin, so the retirement adds one:
  `noFailoverOnPoisonedRow` in `retryPolicy.qnt` (a pure `val`; no
  transition-relation change), wired as
  `quint-retry-policy-witness-failover-poisoned` in the failover
  regime — the witness discipline keeps one expect-violation check per
  contended precondition. The historical falsification evidence stays
  in the calibration table. No live invariant loses its exhaustive
  HOLD check.
