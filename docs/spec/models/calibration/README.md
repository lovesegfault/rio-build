# Stage-C calibration overrides for the as-built protocol models

One file per fix family of the controller-formal calibration corpus
(G-A/G-B/G-G over `spawnCoherence.qnt`, M1/M2/M3-M4/FFD-cover over
`nodeclaimLifecycle.qnt`; the families whose every member is NOT-ENCODED
have no file), one file per fix family of the refcount corpus
(`refcount-*.qnt` over `chunkLiveness.qnt` / `chunkCollect.qnt`), one
file per fix family of the gateway connection-lifecycle corpus
(`gw-f*.qnt` over `gwConnLifecycle.qnt`), one file per fix family of
the closure-evidence corpus (`closure-f*.qnt` over `closureEvidence.qnt`,
the F1–F14 representatives of the closure-evidence-formal Phase 0d
gate; the families whose members are NOT-ENCODED — F15/F16/F17 — have
no file; the closure-evidence corpus additionally carries two Phase-1
fix regression pins, `closure-c3-no-reprobe.qnt` and
`closure-a17-unfenced.qnt`, which freeze the PRE-Phase-1-fix production
actions — the settlement-less fail-fast and the unfenced PG apply —
rather than a historical origin/main fix, so the wired expect-violation
checks over them keep the fixed defect classes permanently
re-discoverable), one file per
transferred family of the substitution-replacement C-prime corpus
(`mat-*.qnt` over `materializationJob.qnt` — the §9.3 calibration
transfer: the closure-evidence family successors plus the Phase-A/B
fix regression pins B1/B3/B4/B5a, which freeze the pre-fix production
behaviors the Phase-B bug harvest removed; the two liveness rows
`mat-b1-claim-refuses-marked.qnt` / `mat-b3-no-redial.qnt` are
WITNESS-FLIP modules — their calibStep makes the paired reachability
witness UNVIOLABLE instead of falsifying an invariant, and
`mat-f1-no-presence-recheck.qnt` is the by-construction row whose
calibStep explores exactly the pre-fix-only delta space and must NOT
falsify the §9.1 conjunction), and — the executor-lifecycle campaign
— the single re-encoded pull-era
override (`executor-f4-pull-establish-early.qnt`, over the re-targeted
live `executorSession.qnt` rather than a frozen as-built encoding). The
executor corpus's as-built representatives
(`executor-<family>-<slug>.qnt` over `executorSessionAsBuilt.qnt`) were
retired with the as-built model on 2026-05-29, and the
`executor-f2d-*.qnt` pair over `executorDelivery.qnt` was deleted with
Model D at the 1d builder collapse — git history is the archive; the
executor invariant map's Stage-C tables and retirement records hold
their verdicts. Each module instantiates the model it names, defines a
local PRE-FIX variant of one action (the behavior the named
historical fix removed), and exposes it through a `calibStep`. The
violation latches inside the pre-fix action keep the instantiated
model's oracle: the behavior vals are reverted, the violation vals are
not, so a falsification means that model's invariant set re-finds that
bug class.

The gateway files follow the same pattern with one addition forced by
the model's scale: the full-alphabet `step` of `gwConnLifecycle.qnt`
does not exhaust inside the per-check budget (the Stage-B B-measure), so
each `gw-f*.qnt` override restricts its `calibStep` to the owning
family's letters (the same single-rich-dimension principle as the wired
`gwConnLifecycleFam*` checks) and the as-built baseline is an explicit
`baselineStep` in the same file — the same alphabet and constants with
the as-built action(s) restored — rather than the imported full `step`.
T-direction (permissiveness) overrides additionally re-introduce the
over-tight pre-fix guard and are run against the named P-property.

Run an override (serially — the bundled Apalache server port is shared):

```
quint verify --backend=tlc --main=<module> --step=calibStep \
  --invariant=<predicted invariant> docs/spec/models/calibration/controller-<family>.qnt
```

Distinguishing baselines: where an override runs at non-regime constants
(e.g. `gbCalibAckOnlyNew` at CEILING=2) or pins a module-local invariant
(e.g. `m2CalibInflightDropOnSight`), the as-built baseline is the same
module run WITHOUT `--step` (the imported as-built `step` over identical
constants); it must HOLD the same invariant for the falsification to be
attributable to the reverted behavior. Overrides at standard regime
constants use the wired Stage-B regime checks as their baseline.

The verdict table — every corpus commit, its classification, override
module, predicted vs. actual verdict, depth/state counts, and
disposition — lives at the owning campaign's surviving carrier:
controller and refcount in their records archives
(`docs/spec/models/controller-records.md`,
`docs/spec/models/refcount-records.md`, with the refcount ENC/ENC-A
rows mirrored as VERDICT ROWS in the `refcount-g*.qnt` headers);
closure-evidence in `docs/spec/models/closure-evidence-records.md`
plus the per-file VERDICT blocks in the `closure-*.qnt` headers;
gw-session in the `gw-f*.qnt` VERDICT blocks plus this file's gateway
acceptance section; materialization (substitution-replacement) in this
file's materializationJob verdict section; retry in
`docs/spec/models/retry-records.md`.

Executor archive note: the executor campaign's frozen as-built model
(`executorSessionAsBuilt.qnt`) and its ten Stage-C evidence modules
were retired 2026-05-29 (owner decision, ahead of the deployment-watch
condition) — git history at that retiring commit is the archive and
holds the Stage-C verdict tables; the live stack (the re-targeted
`executorSession.qnt`, its two exhaustive regimes, 12 witnesses, and
the re-encoded `executor-f4-pull-establish-early.qnt` flip) carries
every pin a live invariant needs.
A subset of the overrides is wired into `nix/quint.nix` as permanent
expect-violation checks (`quint-ctrl-calib-*`, `quint-refcount-calib-*`,
`quint-executor-calib-*`, `quint-closure-calib-*`, `quint-gw-calib-*`,
`quint-materialization-calib-*`);
the rest are evidence modules, re-runnable on demand with the command
above.

## materializationJob (substitution-replacement campaign) — the §9.3 calibration-transfer verdicts

> Relocated verbatim from the retired substitution-replacement invariant
> map (Phase C-prime stage record). The campaign archive is
> `docs/spec/models/substitution-replacement-records.md`; the exhaustive
> measurement table lives in the `materializationJob.qnt` header.

### The calibration-transfer table (go/no-go criterion 1 — every §9.3 row dispositioned)

Protocol: falsification first (the override must VIOLATE its predicted
property under `calibStep`), then the baseline (the as-built step at
the SAME constants must HOLD it). Falsifications run under BOTH
backends (rust simulator at 400 K × 14, then TLC first-violation —
verdict @ depth/states/wall below; development-host runs at quint's
default TLC width — the WIRED budgets are each pin's CI-builder
transcript, which is the width the check runs at by construction, e.g.
F8 8.5 s / F4 2.2 s in the build logs); baselines under the rust
simulator at 400 K × 15 (the bounded-coverage form) — the exhaustive
baseline conjunctions are the same unconverged manual targets as the
regime exhaustives. Worker counts are labeled on every bounded-prefix
figure (the orchestrator's comparability rule); the 60-worker entries
are the reference-budget coordinates. Override modules: `docs/spec/models/calibration/mat-*.qnt`
(21 modules); wired pins: `quint-materialization-calib-*` (19 checks).

| §9.3 family / rep | Override (pre-fix behavior) | Predicted property | Falsification (TLC @ calibStep) | Baseline (as-built step) | Disposition |
|---|---|---|---|---|---|
| F8 (CE-33→A1) + F13 (CE-58→B8) | builder pull ignores the job view + the A11 judgment | `noFromSourceWhileJobUnresolved` | **VIOLATED** — depth 12, 10,832 distinct, 17 s | HOLDS (sim 400 K) | MET (the anchor; WIRED). F13 shares the model's single pull site per the design's own "same check as F8" |
| F10 L1-half (CE-48(i)→L1) | failover drops the view, no rebuild | `unresolvedJobAlwaysArmed` | **VIOLATED** — depth 10, 1,260 distinct, 13 s | HOLDS | MET (WIRED) |
| F10 A3-half (CE-45→A3) | — | — | — | — | BY-CONSTRUCTION: no recovery clear gate exists in the job architecture (failover clears no marks; the only mark consumers are the resolution sites and the fail-fast). Argument recorded against `failover`'s frame |
| B5(a) (`4e57180fd`) | dedup re-feed overwrites armament state | `unresolvedJobAlwaysArmed` | **VIOLATED** — depth 7, 382 distinct, 13 s | HOLDS | MET (WIRED) |
| CE-17 / F6-hazard / F2(a) | success consumption skips the coverage re-check | `successConsumptionCoversLiveWanted` | **VIOLATED** — depth 10, 703 distinct, 13 s | HOLDS | MET (WIRED; the CE-17 corpus anchor). F6's latch state is by-construction gone (no never-forgive bookkeeping exists); the hazard class (wanted growth racing consumption) is this row |
| F2(b) / BC-3 | establishment adopts on output presence | `successConsumptionCoversLiveWanted` | **VIOLATED** — depth 14, 3,179 distinct, 15 s | HOLDS | MET (WIRED) |
| F3 soundness (CE-61→B3) (i) | InfraFailure charged as Unobtainable (unverified) | `noWrongfulTerminalFailure` | **VIOLATED** — depth 15, 41,494 distinct, 27 s | HOLDS | MET (WIRED). Re-point note: the design predicted `noWrongfulFromSourceRouting`; at this model the corrupted charge surfaces through the spent-one-shot fail-fast (the `unobChargeBacked` oracle) — within-family re-route, recorded |
| F3 soundness (ii) / E5 | budget exhaustion fails the node (no park) | `materializationNeverPoisons` | **VIOLATED** — depth 12, 1,532 distinct, 14 s | HOLDS | MET (WIRED) |
| F3 permissiveness (CE-60/3→C2) | report's missing paths unverified upstream | `noWrongfulFromSourceRouting` | **VIOLATED** — depth 12, 1,580 distinct, 14 s | HOLDS | MET (WIRED). The by-construction half (routing requires a completed execution) is structural: every consumption requires an open mat-kind attempt |
| F7 (CE-30/28→A3) | un-keyed resolution (no attempt, no exec id) | `jobResolutionSound` | **VIOLATED** — depth 7, 281 distinct, 13 s | HOLDS | MET (WIRED) |
| F9 (CE-41→A5 re-pointed) | routing trusts a divergent in-memory child view | `routingRequiresDurableVouchOrFailFast` | **VIOLATED** — depth 11, 1,660 distinct, 14 s | HOLDS | MET (WIRED). The hole-stamp half is by-construction (no holes; no DAG edges in-model; production routing reads the durable relation) |
| F11 (CE-50→A18, extended) | stale-tenure job resolve APPLIES (no fence) | `fencedJobWritesOnly` | **VIOLATED** — depth 9, 1,059 distinct, 15 s | HOLDS | MET (WIRED; the a17-unfenced analogue for job-table writes). The regime-comparison half: the no-stale-alphabet regimes hold by construction (`ENABLE_STALE_TENURE`) |
| F1 soundness (CE-2→B9) | — | — | — | — | KEPT GUARD: the stale-Produced verify is untouched production machinery; its oracle (`quint-closure-calib-f1-stale-produced`) stays wired and green; the new model carries the stale-reset SHAPE (Δ4: `OStaleReset` reset-from-Completed, `staleResetRun` + witness) |
| F1 permissiveness (CE-1→C4) | creation without the presence re-check (the covered-creation delta space) | **predicted NO-falsify** | The §9.1 conjunction over `calibStep`: sim 500 K **[ok]**; TLC bounded zero-violation prefix of 546,655 distinct / depth 14 / 15-min cap at 60 workers (unconverged — recorded as bounded coverage). Reachability: `noCoveredCreationJob` **VIOLATED** (TLC depth 9, 655 distinct — the space is real) | conjunction HOLDS as-built (the regime evidence) | MET, BY-CONSTRUCTION + kept guard re-pointed: a covered-creation cannot produce the CE-1 harm (no build attempt exists for an unresolved-job node; the job resolves by Success coverage). The OLD-model override re-run (`closure-f1-skip-store-recheck` × C4): **still VIOLATED** (5,013 distinct, 55 s) — the kept guard's falsifiability re-validated |
| F4 B4-half (CE-13→B4) | coverage over the dead-inclusive stored union | `interestUnionLiveOnly` | **VIOLATED** — depth 11, 1,432 distinct, 2.2 s (the wired check's transcript) | HOLDS (sim 400 K) | MET (WIRED). The Success-coverage override half is the CE-17 row |
| F4 B10-half (CE-66→B10) | — | — | — | — | KEPT GUARD: the prune demand set is untouched; `quint-closure-calib-f4-demand-drop` stays wired and green |
| F5 (CE-16→B5) PP-5(i) | a build's relation write replaces the other builds' rows | `crossBuildWantedIsolation` | **VIOLATED** — depth 11, 1,352 distinct, 13 s | HOLDS | MET (WIRED). Re-point note: the design predicted `interestUnionLiveOnly`; the per-build write-history ghost is the direct PK-isolation oracle (the live-union latch reads decisions, not rows) — recorded |
| F5 PP-5(ii) (same-build narrowing) | — | — | `noWantedRewrite` witness **VIOLATED** (reachable) | — | DOCUMENTED-INTENDED (the sign-off evidence): the as-built upsert allows same-build narrowing; recorded as a reachable behavior, not a violation — the draft's write-once guard was corrected to the as-built upsert |
| F14 (CE-48(i)/CE-52→L1) | — | — | — | — | BY-CONSTRUCTION: no Substituting status exists in the model (fresh flag-on work never reaches the walk — the T-5.3 zero-walk audit + `flag_on_fresh_work_never_walks` own that boundary; legacy-state absorption is VM-tested). The L1 successor is the F10 row |
| C5 / CE-7 (the 0d deferred manual target) | (the F4 dead-union override IS the re-introduced behavior) | `interestUnionLiveOnly` | the F4 row's falsification | the F4 row's baseline | **CLOSED BY CONSTRUCTION, with the falsification the 0d deferral asked for**: no stored union exists (the §6 join is live-only and derived; `buildTerminal` drops rows atomically), AND the dead-union override demonstrates the latch re-finds exactly the stored-union behavior if re-introduced. Owner sign-off flagged (closes a standing 0d open item) |
| B2-strong / GC-after-vouch (a) | ingest without the pin INSERT | `pinCoversIngestUntilAllInterestTerminal` | **VIOLATED** — depth 13, 1,438 distinct, 14 s (the GC trace shape re-found) | HOLDS — after the encoding correction recorded above (the release-window artifact; the FIRST baseline run violated and was triaged to the model, not the product) | MET (WIRED; the §9.3 flip executed: the old expect-violation probe's class is now a holds-property + this pin). Shape (b) narrows into B9 (kept guard row) |
| C1-strict (PP-6) | — | — | — | — | UPGRADED + WIRED: `wrongfulFailFastBoundedPerJob` (≤1 per drv-lifetime, justified) sits in the holds conjunction; non-vacuity via the fail-fast witness; closes the AW2 open item favorably. The bound is per-DRV-history (the as-built `count_materialization_rows_in_history` read — stricter than the design's per-job phrasing; the resubmit lane is out of the §9.1 alphabet, recorded) |
| PP-4 (i) | mat establishment writes executor_crash | `materializationInvisibleToBuildBudgets` | **VIOLATED** — depth 10, 579 distinct, 14 s | HOLDS | MET (WIRED) |
| PP-4 (ii) | establishment closes charge-free | `materializationCrashChargedOnce` | **VIOLATED** — depth 12, 1,307 distinct, 14 s | HOLDS | MET (WIRED). Re-point note: the design's "falsifies unresolvedJobAlwaysArmed (never settled)" is a liveness shape; its safety-checkable core is the charge-once discipline — recorded |
| dedup (the partial-unique index) | creation without the one-unresolved-job arbitration | `atMostOneUnresolvedJobPerDrv` | **VIOLATED** — depth 12, 1,427 distinct, 14 s | HOLDS | MET (WIRED; the C′ handoff's slot-widening executed via the dupJob second-slot ghost) |
| B4 / Δ2b (`056bfc9b6`) | probe creation without the backfill | `creationLeavesTenantResolvable` | **VIOLATED** — depth 10, 1,487 distinct, 15 s | HOLDS | MET (WIRED) |
| B1 / Δ6 (`7c9b9f949`) | claim refuses marked nodes (the pre-B1 A11 arm) | **witness flip** (liveness) | as-built direction: `noMarkedClaim` **VIOLATED** (TLC depth 13, 12,720 distinct, 17 s — wired witness); pre-fix direction: NOT violated — sim 300 K [ok]; TLC bounded zero-violation prefix of 2,338,887 distinct states / depth 18 / 24.5 min at auto(192) workers (unconverged — the [ok] direction cannot early-exit; the prefix is the bounded-coverage record) | §9.1 conjunction green under the override (a refusal strands, never corrupts) — sim 300 K [ok] | MET (witness-flip form; the B1 regression guard is the wired marked-claim witness) |
| B3 / Δ2a (`ce17c6445`) | redial removed from the alphabet | **witness flip** (liveness) | as-built direction: `noPostFailoverClaim` **VIOLATED** (TLC depth 9, 808 distinct, 14 s — wired witness); pre-fix direction: NOT violated — sim 300 K [ok]; TLC bounded zero-violation prefix of 426,036 distinct / depth 14 / 10-min cap at 60 workers — the dead-end made visible as unreachability | §9.1 conjunction green under the override — sim 300 K [ok] | MET (witness-flip form; the B3 liveness guard is the wired post-failover-claim witness) |

**Headline: zero transfer failures.** Every row predicted to falsify
falsified (18/18, both backends); every baseline held (after one
model-side recalibration, recorded above); every by-construction row
carries a structural argument against the final code shape; both
liveness rows flipped exactly as predicted. No stop-and-report
condition fired.

## Gateway lifecycle (gw-session campaign) — Stage-C scope protocol and acceptance

> Relocated verbatim from the retired gw-session invariant map (Stage-C
> calibration record). Per-module verdicts and trace walks live in the
> nine `gw-f*.qnt` headers; the campaign close-out banner is in
> `gwConnLifecycle.qnt`.

**Scope and baselines (a documented deviation from the §5 letter).** §5
planned the overrides on the full-alphabet regime modules; the Stage-B
B-measure showed those do not exhaust inside the per-check budget, so the
falsification direction would have been fine (TLC stops at the first
violation) but the matching baseline ("with the guard restored the
violation disappears at the same scope") would not have been provable.
Each override's `calibStep` therefore restricts the alphabet to its
family's letters — the same single-rich-dimension principle as the wired
`gwConnLifecycleFam*` checks — and the as-built baseline is an explicit
`baselineStep` in the same file (same constants, same alphabet shape, the
as-built action(s) restored) run to exhaustion. No §2e structural or
environment bound is reduced, and the §2e policy-cap table's
pre-registered override values are used where required (GW-6/GW-7 at
`MAX_CHANNELS_PER_CONN = 1`, GW-12 at `MAX_CONNECTIONS = 1`). Verdict
format: property @ depth (states generated / distinct); wall-clocks are
from the same serial `quint verify --backend=tlc` protocol as the Stage-C
check measurements (24–32 workers on the shared builder, ≈20–30 s of
JVM/conversion overhead included in every figure).

**Acceptance verdict (the §5 criterion, per corpus family).**
"Calibrated" = the family falsifies its predicted property through at
least one trace-walked representative, both directions where a T half
exists, with the as-built baseline holding at the same scope.

| Family | Verdict | Evidence |
|---|---|---|
| F1 capacity conservation | **calibrated** | GW-1 (V: L2), GW-12 (V: S1), GW-18 (V: S2; T: P1/P2/P3); GW-2's release-ordering member also lands in F1 via S15 |
| F2 progress-bounded occupancy | **calibrated** | GW-3 (V: S20 + L1; constituent arm L3) |
| F3 decision-to-enforcement | **calibrated** | GW-4 (V: S12); the 2f11bb8f5 constituent dispositioned by structural argument onto the wired l1-no-inactivity probe (note 2) |
| F4 channel/session bookkeeping integrity | **calibrated** | GW-5 (V: S7), GW-6 (V: S7), GW-7 (V: S14) |
| F5 egress flow control / bounded queues | **calibrated** | GW-2 (V: S15, L2; T: P5), GW-8 (V: S13) |
| F6 teardown obligations | **calibrated** | GW-9 (V: S16, S10), GW-10 (V: S10), GW-11 (V: L8, S3), GW-20 (V: S19) |
| F7 drain/shutdown ordering | **calibrated** | GW-13 (V: L4, S17; T: P6) |
| F8 upload bounding & wire-position integrity | **pre-registered out-of-model (test-only)** | GW-14: the `wire_opcodes` upload tests (`opcodes_write.rs` — `test_add_multiple_streaming_early_ok_preserves_wire_position` is the fac58554b regression test, plus the multi-entry batch/mixed permissiveness tests) and `functional/nar_roundtrip.rs`; GW-15: store-side half owned by the store campaign, gateway permissiveness half by the same `wire_opcodes` multi-entry tests + `functional/nar_roundtrip.rs` (owner decision §8 Q5). Corrected at close-out: both halves were previously attributed to "golden-conformance" / "golden/integration" upload tests (wording inherited from design §3.4) — the golden-conformance suite has no upload tests; the dispositions themselves are unchanged |
| F9 anti-hang deadlines on upstream waits | **calibrated** | GW-16 (V: L6); the budget-sizing members stay non-candidates per the corpus |
| F10 accept-path availability | **calibrated** | GW-19 (V: S18) |
| F11 session credential freshness | **pre-registered out-of-model (test-only)** | GW-17: `session_jwt_token_refreshes_per_access` + the I-129 regression tests (no clock in the model; owner decision §8 Q5) |
| F12 result-integrity vs store | **out of corpus (pre-registered exclusion)** | not a lifecycle-machine family (corpus §3, design §1 out-of-scope table); coverage: the 85118ecdf / cb3f6bfbb output-verification paths and their build-result tests; a separate data-integrity model would own it if commissioned |
| F13 adjacent state machines (scheduler-watch reconnect, STDERR framing, startup/readiness) | **out of corpus (pre-registered exclusion, §8 Q6)** | reconnect boundedness: `test_build_paths_reconnect_exhausted_returns_failure` + the C4 snapshot-resync tests (`test_build_paths_reconnect_snapshot_resumes_state`, `test_build_paths_reconnect_terminal_snapshot_short_circuits`) — see the WatchBuild environment-assumption row above for the C4 deletion note; STDERR framing: wire/golden conformance tests; startup/readiness: the vm-lifecycle scenarios |
| F14 per-session ingress memory budget | **pre-registered out-of-model (test-only; named Phase-2 Kani candidate)** | GW-21: drv_cache cap unit tests (`MAX_TRANSITIVE_INPUTS`, `insert_drv_bounded`, the occupancy-aware gate tests) |

No family is NOT MET: every encodable family (F1–F7, F9, F10) falsifies
its predicted property through at least one trace-walked representative
with the baseline holding at the same scope; the three T halves
(GW-2/GW-13/GW-18) falsify their named permissiveness properties; the
four OOM candidates (GW-14/15/17/21) carry their pre-registered
dispositions. No falsification contradicted the §3.4 crosswalk (the one
partial-prediction case, GW-1's S2/S3, falls through the predicted L2
and is documented in note 1); no unlisted falsification of the as-built
model surfaced (every baseline holds `allInvariants`); there is no
stop-and-report item.
