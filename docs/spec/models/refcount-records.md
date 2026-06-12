# Chunk-refcount campaign records (closed-campaign archive)

Archived verbatim from `docs/spec/models/refcount-invariant-map.md` @
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

Live carriers for this campaign (not this file): `chunkCollect.qnt` /
`gcCollectState.qnt` / `gcCadence.qnt` and the archived-as-built
`chunkLiveness.qnt` with their wired `quint-chunk-*` / `quint-gc-*` checks and
calibration twins (`nix/quint.nix`), the `refcount-g*.qnt` header VERDICT
ROWS, the `store.gc.*` / `store.chunk.*` rules in
`docs/spec/components/store.typ`, the gc code-site records in
`rio-store/src/gc/`, and `docs/ops/gc-enablement.typ`.

T-1a future-recording note: any NEW mark-scan / collect-phase measurement
rounds append to this file's §3 (the measurement-gate chain), keeping the
gate-license arithmetic in one place — `mark_scan_bench.rs` names this file
as the recording destination.

---

## 1. Stage-B results (`chunkLiveness.qnt`, the as-built model)

> Origin: refcount-invariant-map.md § "Stage-B results", verbatim. This is
> the section the corrupt-regime reproducer checks in nix/quint.nix point at:
> if a C12 reproducer stops falsifying, revisit THIS record (the model has
> stopped reaching the leak, or the code stopped leaking).

## Stage-B results (`chunkLiveness.qnt`, the as-built model)

The model is `docs/spec/models/chunkLiveness.qnt`: the write-ahead uploader
state machine (claim, the upgrade transaction's manifest_data INSERT +
refcount UPSERT + token capture, the S3 PUT fan-out, the presence commit,
the claim-gated completion), the token-gated rollback (DEC-1), the
claim-gated reap, the heartbeat, the hot-path and scanner stale reclaims,
the path-sweep batch (single-path and by-count two-path forms), the
orphan-chunk sweep split into its outer SELECT and inner UPDATE so the C11
window is a real interleaving, the outbox drain with its `FOR UPDATE`
re-check, the crash windows of inventory §2.2 as the fault alphabet, and
`refs(h)` (the manifest fold) as the recomputed ghost truth. One model
action per SQL transaction (design §3.2); scope boundaries and encoding
decisions (the path-level reachability GC abstracted to an environment
choice per the G4b pre-registration; lock order NOT-ENCODED per G6; the S3
PUT fan-out collapsed to one non-transactional action; the outbox attempts
counter and the parked-row alerting tail out of scope; the relative clock
with saturation and the heartbeat contract as a `tick` precondition) are
documented in the model header. Four exhaustive TLC regimes are wired into
`nix/quint.nix` (`quint-chunk-liveness-{base,crash,contend,corrupt}`),
plus the named-run replays, sixteen non-vacuity witness checks, the two
pre-registered corrupt-regime falsification checks, and the
threshold-ordering inversion check.

### Verdict table

Distinct-state counts are as measured at the introducing commit (also in
that commit's message and the CI transcripts): base 7,791 distinct
(735,841 generated, depth 14), crash 2,964,717 (145,054,657, depth 25),
contend 1,332,821 (53,843,425, depth 23), corrupt 3,307,725 (142,073,313,
depth 26). Every regime's HOLDS column is an exhaustive TLC result over
that regime's full reachable space.

| Design invariant (§3.3) | Model form | base | crash | contend | corrupt |
|---|---|---|---|---|---|
| CR-1 `NoLiveChunkCollected` (state + action form) | `cr1NoLiveChunkCollected` | HOLDS | HOLDS | HOLDS | HOLDS |
| CR-2 `BoundedGarbageRetention` (as-built structural form) | `cr2NoStrandedGarbage` | HOLDS | HOLDS | HOLDS | **FALSIFIES-AS-PRE-REGISTERED** — the C12 stranded-garbage shape (`quint-chunk-liveness-corrupt-c12-stranded`); the carved form below is this regime's stated form |
| CR-2, corrupt-regime carved form (the rule's carve-out clause) | `cr2CarvedCorrupt` | — (base form checked) | — | — | HOLDS |
| CR-3 `CounterRefinesManifestFold` | `cr3CounterRefinesFold` | HOLDS | HOLDS | HOLDS | **FALSIFIES-AS-PRE-REGISTERED** — the C12 sanctioned permanent over-count (`quint-chunk-liveness-corrupt-c12-overcount`) |
| CR-3, corrupt-regime carved form (counter = fold + observably-skipped decrements) | `cr3CarvedCorrupt` | — | — | — | HOLDS |
| CR-4 `PresenceNeverInferredFromCounter` | `cr4PresenceFromConfirmedUpload` | HOLDS | HOLDS | HOLDS | HOLDS |
| S4 `OwnerOnlyMutation` | `s4OwnerOnlyMutation` (admission-predicate form) | HOLDS | HOLDS | HOLDS | HOLDS |
| S5 `LiveOwnerNeverReaped` | `s5LiveOwnerNeverReaped` | HOLDS | HOLDS | HOLDS | HOLDS |
| L3 `PlaceholderConvergence` (safety support: no foreign freshen) | `l3NoForeignFreshen` | HOLDS | HOLDS | HOLDS | HOLDS |
| M_023 `CHECK (refcount >= 0)` | `m023NonNegative` | HOLDS | HOLDS | HOLDS | HOLDS |
| structural bounds / self-consistency | `boundsOK` | HOLDS | HOLDS | HOLDS | HOLDS |

Forms and qualifications, relative to design §3.3's statements:

- **CR-3 needed no C1–C7 carve-out.** The design sanctioned "transient
  over-counts after C1–C7 crashes, repaired within the scanner threshold";
  at transaction granularity those windows leave the `'uploading'`
  manifest row (and its `manifest_data`) in place, so the fold still
  counts the abandoned references and the counter never diverges — the
  crash regime HOLDS on the exact equality at every reachable state, a
  stronger verdict than the gate required. The only sanctioned deviation
  that exists as built is C12's permanent over-count, carved exactly by
  `cr3CarvedCorrupt` (counter = fold + per-chunk skipped-decrement count).
- **CR-2 is encoded structurally, not clocked.** The rule's bound mixes
  protocol obligations with background-loop cadences (15-minute scanner,
  hourly orphan-chunk sweep, 30 s drain); the cadences are scheduling
  facts about `spawn_periodic` loops, not protocol state, so the model
  checks the protocol half: garbage never becomes *unreclaimable by the
  standing machinery* — an unreferenced, not-yet-soft-deleted chunk is
  always at refcount 0 (still eligible for the zero-detect / orphan-chunk
  sweep predicates), and a soft-deleted chunk whose object still exists
  is always enqueued. As built that is exactly the conditionality on the
  counter the Stage-A row recorded: a refcount stuck above zero with no
  references is permanent garbage, which is why the unconditional form
  falsifies in the corrupt regime and only there. The wall-clock tail of
  the bound (cadence + drain lag + the attempts-cap parking) stays with
  the existing loop tests under `store.gc.pending-deletes` and the
  stale-reclaim rules.
- **CR-1's action form** is carried by the `deletedWhileReferenced` ghost
  recorded at the only two backend-delete sites (the drain transaction
  and its C10 crash variant); the state form is the `'complete'`-manifest
  presence clause. Both hold in all four regimes; the corrupt regime's
  HOLDS is the design's "C12 errs toward retention, never toward
  data loss" claim, machine-checked.
- **S5 holds against the production threshold ordering, and the ordering
  is load-bearing**: the `chunkLivenessThresholdOrder` regime lowers the
  hot-path threshold to the heartbeat deadline and S5 falsifies there
  (`quint-chunk-liveness-threshold-order` passes only while it does).
  S4/L3 are encoded over the admission predicates the actions themselves
  use (the logService authGate pattern), so dropping a claim/token/status
  conjunct from a cleanup or heartbeat guard falsifies them.
- **L4 `RepairLoopLiveness` is not encoded** (per the design's "encode
  only if cheap"): pagination, poison-row isolation and per-row
  transaction shapes are below this model's granularity; the G7 family
  stays with the existing loop tests. L3's liveness half ("every
  abandoned row is eventually reaped") is carried as the no-foreign-
  freshen safety support plus the scanner/hot-path reachability
  witnesses; the eventuality itself is a fairness property outside an
  interleaving safety model.

### Witness results

All sixteen non-vacuity witnesses are violated (the contended states are
reachable) in the regime each is wired against, and the three
expected-falsification probes reproduce; every row below is a CI check.

| Witness (model `val`) | Regime | Probes | Result |
|---|---|---|---|
| `noCompleteUpload` | base | a complete chunked upload exists (CR-1's state form is non-vacuous) | violated |
| `noBackendDelete` | base | a backend DeleteObject actually fires (CR-1's action form is non-vacuous) | violated |
| `noUnconfirmedReferencedChunk` | base | the M_033 precondition (refcount ≥ 1, `uploaded_at` NULL) | violated |
| `noStaleTokenRollbackNoop` | base | the C4 own-heartbeat token no-op | violated |
| `noHeartbeatReset` | base | a heartbeat resets non-zero staleness (the heartbeat is load-bearing) | violated |
| `noCrashAtClaimed` | crash | C1 | violated |
| `noCrashAfterUpgrade` | crash | C2 | violated |
| `noCrashBeforeReap` | crash | C5 (the state C3/C7 collapse onto) | violated |
| `noDoubleCrashStaged` | crash | C6 (both writers staged, both dead) | violated |
| `noAbandonedAccounting` | crash | the as-built leak shape: abandoned `'uploading'` manifest, chunks still counted | violated |
| `noHotpathReclaim` | crash | the 300 s hot-path reclaim fires | violated |
| `noScannerReap` | crash | the 15-minute scanner reaps an abandoned row | violated |
| `noSharedByCountDecrement` | contend | one batch decrements a shared chunk by ≥ 2 (the adfd303d7-C2 clause) | violated |
| `noDrainResurrectSkip` | contend | the drain re-check skips a resurrected chunk (G4a) | violated |
| `noOrphanRecheckSave` | contend | the orphan-sweep inner re-check excludes a resurrected candidate (C11) | violated |
| `noLateCleanupNoop` | contend | an owner-side cleanup no-ops against a foreign/missing row (the G1 contention) | violated |
| `cr3CounterRefinesFold` (pre-registered falsification) | corrupt | the C12 permanent over-count | violated, as pre-registered |
| `cr2NoStrandedGarbage` (pre-registered falsification) | corrupt | the C12 stranded garbage | violated, as pre-registered |
| `noCorruptLeak` | corrupt | the literal leak shape (refcount > 0, zero referencing manifests) | violated |
| `s5LiveOwnerNeverReaped` (threshold-ordering inversion) | threshold-order | a live, progressing owner reaped once heartbeat-deadline ≥ hot-path threshold | violated |

One §3.5 witness was corrected against the code rather than encoded as
written: the design asks for "refcount > 0 with zero manifests" to be
reachable in the **crash** regime before the reaper runs. As built that
state is unreachable outside the corrupt regime — both decrement
statements run in the same transaction as the manifest deletion that
justifies them, and the C1–C7 windows leave the manifest row in place, so
crashes leave *accounted* garbage (`noAbandonedAccounting`), not a
counter/fold divergence. The literal leak shape is reachable exactly where
a decrement is skipped against a surviving reference record's deletion —
the corrupt regime — and is pinned there by `noCorruptLeak`. This is a
design-§3.5 wording correction, not a model gap; the same observation is
why CR-3 needs no crash-regime carve-out.

### Notes for the Phase-0 exit gate (what the model run established)

- The full encoded invariant set HOLDS on the unmodified as-built model in
  every regime, in each regime's stated form (the corrupt regime's stated
  forms being the carved CR-2/CR-3), with the two unconditional-form
  falsifications in the corrupt regime exactly matching this map's
  pre-registered as-built deviations (CR-2 conditional on CR-3; C12's
  sanctioned permanent over-count). No unexpected falsification occurred.
- Encoding observations worth carrying into Stage C and Phase 1:
  `upgrade_manifest_to_chunked`'s ownership guard is the `FOR UPDATE` on
  the `'uploading'` row plus its existence — there is no claim_id filter
  in that statement; the model encodes it faithfully and no invariant
  falsifies, because a reaped-then-re-claimed path can only reach that
  state after the original owner stopped heartbeating, and a stopped
  owner never reaches its upgrade step. Calibration should keep this in
  mind when reverting heartbeat/claim mechanisms (a G1/G5-family revert
  may surface it).
- The Stage-C calibration corpus (design §3.4) is the next stage and is
  NOT part of this section's claims; its overrides will import this model
  the way `calibration/retry-g*.qnt` import `retryPolicyAsBuilt.qnt`.


---

## 2. Stage-C calibration: the corpus, the G1-G7 tables, findings, exit gate, Phase-1 inputs

> Origin: refcount-invariant-map.md § "Stage-C calibration" through "Phase-1
> input list", verbatim (the ENC/ENC-A rows also live as VERDICT ROWS blocks
> in the calibration/refcount-g{1,2,3,4a,5}.qnt headers; the G6/G7 NOT-ENC
> and SUBS dispositions live only here). The Findings subsection carries the
> late-mark discovery narrative.

## Stage-C calibration: the historical-fix corpus replayed against the model

The ~35-fix corpus (inventory §5, families G1–G7, plus the design's
pre-registered G2×G3 joint-revert row) replayed against
`chunkLiveness.qnt`: for each corpus commit the pre-fix behavior is either
expressed as an override of the as-built model and shown to falsify an
invariant (the model would re-find that bug), or its non-encodability is
dispositioned with the missing dimension named. Method per the retry
campaign's Stage C (and the design's §3.4 model-side-override correction):
each override is a module in `docs/spec/models/calibration/refcount-g*.qnt`
that instantiates the as-built model, replaces ONE owner-side entry point
with a local PRE-FIX variant, and exposes the swapped transition relation
as `calibStep` selected with `quint verify --step=calibStep`. The reference
fold `refs(h)`, the ghost sensors, and every invariant keep their as-built
definitions — they are the oracle, not part of the reverted behavior.
Where a module restricts the alphabet below a Stage-B regime's constants,
the distinguishing baseline (the as-built actions over the same
restriction — the module's imported default `step`, or an explicit
`baselineStep` where the restriction itself changes an action) was run
against the same invariant and is recorded as HOLDS; modules that reuse a
Stage-B regime's constants verbatim cite that regime's exhaustive Stage-B
verdict as their baseline. No main-model file was touched by this stage:
no invariant was added to `chunkLiveness.qnt`, the four regime checks are
bit-identical to Stage B, and the only new invariant anywhere is one
module-local ownership restatement (`completionRequiresCurrentOwner`,
below), per the retry campaign's local-invariant precedent.

Verdicts are exhaustive TLC results (violation runs stop at the first
counterexample); depth = transitions in the counterexample, states =
generated/distinct at the point TLC stopped, both from single-worker runs
so the counterexamples are the deterministic shallowest ones; wall-clocks
live in the introducing commit's message. Re-run command shape (the local
apalache-server prelude of `nix/quint.nix` applies):

```
quint verify --backend=tlc --main=<module> --step=calibStep \
  --invariant=<invariant> docs/spec/models/calibration/refcount-g<N>.qnt
```

The S4/L3 encoding caveat, recorded once: the main model's
`s4OwnerOnlyMutation` / `l3NoForeignFreshen` are admission-predicate
regression guards (they quantify over the as-built admission predicates
themselves — the logService authGate pattern), so they bind edits to the
main model's predicates and structurally cannot falsify from an additive
override module. The ownership content of a G1 revert therefore falsifies
here through its state-level consequences (CR-3 / M_023 / CR-4 / CR-1) or
through a module-local restatement over the pre-fix admission predicate —
which is where the design's §3.4 prediction ("S4, then CR-3") lands at
this model's resolution.

### Calibration table

Classification legend: **ENC** — encodable, override written and run;
**ENC-A** — encodable, covered by the named sibling override (disposition
by analogy within the family, design §3.4); **NOT-ENC** — the model
abstracts the mechanism away (the missing dimension is named); **SUBS** —
the fix's subject no longer exists in the tree. Verdict format:
invariant @ step (depth, generated/distinct).

#### G1 — a late or foreign cleanup clobbered someone else's upload (ownership/identity)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g1.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `1cd975b90` | DEC-1 rollback carries no PlaceholderToken / generation gate and takes no FOR UPDATE ownership lock — it deletes whatever 'uploading' row the path has and decrements the roller-back's own hash set against it | ENC | `refcountCalibG1RollbackPreToken` | S4-content via consequences: CR-3, then M_023 | **FALSIFIES** cr3CounterRefinesFold @ calibStep (depth 8, 208,974/12,056) and m023NonNegative @ calibStep (depth 8, 208,974/12,056) — the late rollback erases the successor's placeholder and re-decrements an already-reclaimed reference below zero; the incident shape is pinned by `g1PreTokenDoubleDecrementRun`. Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `937a9c928` (completion half) | completion not claim-gated — a stale uploader that resumes after its placeholder was reaped and re-claimed flips the successor's in-flight placeholder to 'complete'; reaching that state also needs the contract-free clock of the progress-driven-heartbeat era (a stalled owner missed heartbeats while alive) | ENC | `refcountCalibG1CompletionUnclaimGated` | S4-content via the local ownership form, then CR-1 | **FALSIFIES** local `completionRequiresCurrentOwner` @ calibStep (depth 9, 53,278/5,798) and cr1NoLiveChunkCollected @ calibStep (depth 10, 142,842/15,174 — the foreign flip makes the successor's still-uploading manifest readable). Baselines over the same relaxed-clock alphabet: the local invariant **HOLDS exhaustively** (38,684,545/1,608,957) — the falsification is attributable to the claim gate; CR-1 does NOT hold on that baseline (depth 13, 2,458,154/187,657) via the independent late-mark window recorded under Findings, so the CR-1 run is supporting evidence, not the attribution |
| `937a9c928` (heartbeat half) | heartbeat not claim-gated — a stale uploader keeps a foreign placeholder artificially fresh | NOT-ENC | — | — | the harm is an eventuality (the foreign freshen delays reaping; nothing is corrupted), outside this safety model; the ownership content is the same claim-gate discipline the completion half falsifies, and `l3NoForeignFreshen` guards the main model's heartbeat admission predicate. Coverage stays with the claim-gated heartbeat unit tests (store.put.placeholder-claim+2) |
| `bf7e516e4` C1 | the owner-side reap (drop-guard / abort / complete-failure cleanup) matches on the path alone, not the claim — a late drop-guard reaps the successor's in-flight manifest and chunk accounting | ENC | `refcountCalibG1ReapPathMatched` | S4-content via consequences: CR-4 | **FALSIFIES** cr4PresenceFromConfirmedUpload @ calibStep (depth 11, 17,722/6,084) — the foreign reap soft-deletes and enqueues the successor's chunk; the successor commits presence on the soft-deleted row and the drain removes the just-uploaded object. Baseline (as-built step over the same one-path/one-hash restriction): HOLDS (185,649/35,161) |
| `ae5f3190b` | hash/size length validation on the rollback path | NOT-ENC | — | — | input validation; pre-registered per-commit exception (design §3.4); existing unit tests |
| `31bd9c512` | orphan scanner re-checks staleness inside the reap transaction | ENC-A | covered by `refcountCalibG1ReapPathMatched` (reap acting on a stale view of the row) | — | by analogy (sibling falsified); the literal pre-fix mechanism (the re-check moved inside the transaction) is an intra-transaction read/write split below the one-action-per-SQL-transaction granularity |
| `539c2be7c` | reap re-checks status inside the transaction (reap-then-reupload race) | ENC-A | covered by `refcountCalibG1ReapPathMatched` | — | by analogy (same shape: a reap admitted against a row that changed under it) |
| `31ce52b14` | reap re-reads chunk_list inside the transaction (stale-chunk-list double decrement) | ENC-A | covered by `refcountCalibG1RollbackPreToken` (a decrement justified by a stale view, double-charging a generation) | — | by analogy (sibling falsified) |

#### G2 — a cleanup path forgot the chunks (leaked refcounts)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g2.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `e5bdbff1b` (I-040) | the owner-side reap uses the inline-only delete: manifest rows deleted, chunk accounting never touched | ENC | `refcountCalibG2ReapInlineOnly` | CR-3, then CR-2 (the leak) | **FALSIFIES** cr3CounterRefinesFold @ calibStep (depth 4, 25/20) and cr2NoStrandedGarbage @ calibStep (depth 4, 25/20). Baseline (as-built step over the same one-uploader restriction): HOLDS both (1,072/398) |
| `dbb42232a` | abort_upload and the batch drop path still inline-only | ENC-A | covered by `refcountCalibG2ReapInlineOnly` — the model's writers funnel every owner-side cleanup through the same reap action, so this is the same revert at model resolution | CR-3 | by analogy (sibling falsified) |
| `adfd303d7` C2 | the path-sweep batch decrements a chunk shared by N dying manifests once, not N times | ENC | `refcountCalibG2SweepCollapsedCount` | CR-3, then CR-2 | **FALSIFIES** cr3CounterRefinesFold @ calibStep (depth 11, 3,348,835/146,119) and cr2NoStrandedGarbage @ calibStep (depth 11, 3,348,835/146,119) — the by-count clause of store.chunk.refcount-decrement, exercised end-to-end. Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `d617bf3e5` | the M_023 `CHECK (refcount >= 0)` plus wiring the standalone orphan-chunk sweep | split | — | — | CHECK half: a passive schema constraint whose model image (`m023NonNegative`) is an invariant in every regime, not a mechanism an override can revert; the under-count class it detects is demonstrated by the `1cd975b90` override driving a counter to −1. Sweep-wiring half: **NOT-ENC** — the existence/cadence of a background collection loop is below the structural CR-2 encoding (the same pre-registered treatment as the 15-minute/hourly cadences); coverage stays with the sweep unit tests and the wired orphan-sweep witnesses |
| `8d93ce6c1` | chunk_tenants junction cleanup | SUBS | — | — | the table was dropped by migration 035; the subject no longer exists |

#### G3 — the counter was used as an S3-presence signal (data loss)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g3.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `dd5c11376` (M_033) | the needs-upload verdict is keyed on the liveness record (row already exists ⇒ "someone else uploaded") instead of `uploaded_at` | ENC | `refcountCalibG3CounterAsPresence` | CR-4, then CR-1 (the production data-loss trace) | **FALSIFIES** cr4PresenceFromConfirmedUpload @ calibStep (depth 4, 1,602/145) and cr1NoLiveChunkCollected @ calibStep (depth 7, 115,050/7,014) — two concurrent writers of the same content; the loser skips the PUT nobody confirmed and completes. Baseline: crash-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `b1c7a9497` | the dedup verdict read in a separate statement after the upsert (re-query race) | ENC-A | covered by `refcountCalibG3CounterAsPresence` — the re-query race only loses data under the counter-as-presence semantics; both shapes produce the same harm state (a PUT skipped for an unconfirmed chunk) | CR-4 | by analogy (sibling falsified); the atomicity content is what the as-built `upgradeManifest` encodes and `store.cas.upsert-inserted+2` pins |
| `127168477` | FastCDC duplicate hashes in one UNNEST batch crash the upsert | NOT-ENC | — | — | set-collapsed `chunk_list` and SQL-error granularity; pre-registered per-commit exception (design §3.4); covered by the upsert dedup unit test and the `manifest_deserialize` fuzz target |
| `00fd5b12d` | the PutChunk RPC did not set `uploaded_at` | SUBS | — | — | the RPC was deleted (`c5bb34612`); the subject no longer exists |
| G2×G3 joint revert (design §3.4 pre-registered row) | inline-only reap leaves a stale refcount behind a deleted manifest; counter-as-presence dedup then trusts it and skips the needed re-upload (the I-040 stale-skip trace) | ENC | `refcountCalibG3JointStaleSkip` | CR-4, then CR-1 | **FALSIFIES** cr4PresenceFromConfirmedUpload @ calibStep (depth 4, 1,262/121) and cr1NoLiveChunkCollected @ calibStep (depth 7, 76,630/4,969); BFS reports the two-concurrent-writers variant as the shallowest counterexample, and the documented I-040 reap-then-stale-skip shape is pinned deterministically by the module's `g3JointStaleSkipRun`. Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |

#### G4 — collect raced a concurrent re-reference (G4a chunk-level / G4b path-level)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g4a.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `aa738a5d7` (M_006) | the drain deletes the backend object with no same-transaction re-check of the chunk's current state (the resurrect arm is left as built — the minimal delta) | ENC (G4a) | `refcountCalibG4aDrainNoRecheck` | CR-1 in the contend regime | **FALSIFIES** cr1NoLiveChunkCollected @ calibStep (depth 7, 63,334/4,030) — a re-upload resurrects the enqueued chunk and the pre-fix drain deletes its object while referenced (the action-form ghost). Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `a2d4c6cd8` (drain re-check half) | the drain re-check ran without FOR UPDATE — its verdict could be stale at DeleteObject time | ENC-A | covered by `refcountCalibG4aDrainNoRecheck`: at one-action-per-SQL-transaction granularity the lockless re-check's staleness window collapses onto "the delete does not observe the concurrent resurrect" | CR-1 | by analogy (sibling falsified); the missing dimension for a literal encoding is an intra-transaction read/write split |
| `a2d4c6cd8` (path_tenants + cycle-reclaim halves) | sweep deletes path_tenants; cycle reclaim via temp-table anti-join | NOT-ENC (G4b) | — | — | path-level reachability GC, pre-registered NOT-ENCODED; covered by `store.gc.sweep-path-tenants`, `store.gc.sweep-cycle-reclaim` and the sweep tests |
| `2b68855c5`, `261e78c9d`, `7d5ff71dc` | the mark-vs-PutPath story (advisory lock, then placeholder-references + re-check) | NOT-ENC (G4b) | — | — | path unreachability is an abstract environment choice in `chunkLiveness.qnt` (design §3.2); covered by `store.gc.two-phase`, `store.put.placeholder-refs` and the mark/sweep tests |
| `62851c73d`, `132446e7e`, `5ba946682`, `adfd303d7` C1/C3, `bf7e516e4` C5 | sweep resurrection transitivity, referrer-first ordering, settle-before-delete, path_tenants re-check | NOT-ENC (G4b) | — | — | same disposition: `store.gc.sweep-recheck+2`, `store.gc.sweep-referrer-order`, `store.gc.sweep-cycle-reclaim`, `store.gc.tenant-retention` and their tests; the replacement leaves this layer untouched (design §4.3, §8) |

#### G5 — the repair loops reaped live uploads (heartbeat/liveness)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g5.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `a1b49b4a3` | no heartbeat exists — an upload that outlives the stale threshold is reapable mid-flight | ENC | `refcountCalibG5NoHeartbeat` | S5 | **FALSIFIES** s5LiveOwnerNeverReaped @ calibStep (depth 4, 152/35) — the hot-path reclaim reaps a live, guard-armed owner. Baseline (as-built step over the same constants): HOLDS (9,705/981). Complements the Stage-B threshold-order check: that one inverts the ordering, this one removes the heartbeat itself |
| `064ceadbd` | wall-clock-driven guard heartbeat + claim plumbing for inline/slow ingests | ENC-A | heartbeat-existence content covered by `refcountCalibG5NoHeartbeat` (the model does not distinguish progress-driven from wall-clock heartbeats); the inline-ingest plumbing is outside the chunked-upload scope | S5 | by analogy (sibling falsified) |
| `2d7e4f9fd` (I-207) | no hot-path stale reclaim — a stale placeholder blocks every re-claim of its path until the 15-minute scanner | witness-form (pre-registered) | `refcountCalibG5NoHotpathReclaim` | the `noHotpathReclaim` witness becomes unviolable; no safety falsification | **AS PRE-REGISTERED**: noHotpathReclaim HOLDS under calibStep (268,869/46,623 — the repair path is gone) while boundsOK, m023NonNegative, CR-1, CR-2, CR-3, CR-4 and S5 all HOLD over the same alphabet (268,869/46,623) — the revert's harm is latency, not safety. The with-mechanism half of the pair is the wired Stage-B witness `quint-chunk-liveness-witness-hotpath-reclaim` |
| `da351aaff`, `f6bf0a546` | heartbeat/reap tasks moved to spawn_monitored | NOT-ENC | — | — | operability; pre-registered per-commit exception (the G7 treatment) |

#### G6 — lock order (4 commits)

`595b7ed9b`, `d64dbc4b0`, `5ad99b458`, `bf7e516e4` C4: **NOT-ENCODED**,
exactly as pre-registered (design §3.4) — PG row-lock acquisition order is
below the model's transaction-atomic granularity. Coverage stays with
`store.chunk.lock-order`, `with_sorted_retry`, and the existing tests; the
replacement shrinks the rule's site list but does not retire it.

#### G7 — background-loop operability (6 commits)

`bf7e516e4` C2/C3/C6/C7/C9, `adfd303d7` C4, `660825f19`, `947aaba79`,
`468fd725a`, `a97af109b`: **NOT-ENCODED**, as pre-registered — pagination,
per-row transaction isolation, gauge resets, SKIP LOCKED multi-replica
behavior and poison-row livelocks are below this model's granularity. The
design's "encode L4 only if cheap" option was evaluated and declined: a
faithful L4 would need per-row error injection and loop iteration
structure the model deliberately lacks. Coverage stays with the existing
loop tests; the replacement's collector inherits exactly this obligation
(design §7, the single-point-of-non-collection risk).

### HOLDS rows and their dispositions

No override predicted to falsify returned HOLDS: every ENC row above
falsified its predicted invariant on the first run, so none of the three
HOLDS dispositions (model gap / unstated property / redundancy candidate)
was triggered. The two rows that record HOLDS verdicts are both
by-construction: the `2d7e4f9fd` witness-form row (pre-registered by the
design as a liveness/latency property, demonstrated by the witness pair
plus the safety-intact run) and the restricted-alphabet baselines (HOLDS
is their required outcome, and all of them hold — with the one CR-1
baseline exception documented as a finding below, which is why that row's
attribution rests on the module-local ownership invariant instead). No
new invariant falsified on the unmodified model; there is no
stop-and-report event.

### Findings

- **The late-mark window (found by the `937a9c928` baseline run, walked
  against the code).** `mark_chunks_uploaded` is
  `UPDATE chunks SET uploaded_at = now() WHERE blake3_hash = ANY($1) AND
  uploaded_at IS NULL` — no `deleted` guard, no claim/generation gate
  (metadata/chunked.rs). If an owner's row is stale-reclaimed while the
  owner is still alive between its S3 PUTs and its mark (the reclaim
  decrements, soft-deletes, clears `uploaded_at`, enqueues), the owner's
  late mark re-asserts `uploaded_at` on the soft-deleted row; the drain's
  re-check (`deleted AND refcount = 0`) then passes and deletes the
  object, leaving confirmed-presence metadata with no backend object; the
  next writer of the same content trusts `uploaded_at` (CR-4-compliant)
  and skips the PUT — the M_033 harm shape without consulting the
  counter. As built this interleaving is excluded **solely by the
  heartbeat contract** (a live owner heartbeats every 30 s, so its row
  cannot reach the 300 s reclaim threshold between PUT and mark): the
  Stage-B crash regime holds CR-1 because a crashed owner never marks and
  a live owner is never reaped. The model run that exposes it relaxes
  exactly that contract (the calibration's contract-free clock), so this
  is **not** an as-built falsification and not a stop-and-report event —
  it is a documented dependency: `uploaded_at`-as-presence (CR-4(b),
  CR-1) leans on the heartbeat contract, not only S5. The dependency
  survives the replacement unchanged (mark, the drain re-check, and the
  reapers all survive — design §4.2), so it is carried into the Phase-1
  input list rather than being priced here.

### Permanent expect-violation witnesses (wired into nix/quint.nix)

Five of the ten override modules are wired as `quint-refcount-calib-*`
checks — one representative per encodable family with a plausible
regression path and a cheap state space (the retry campaign's
proportion). Each passes only while the checker still falsifies the
invariant under the module's `calibStep`.

| Check | Module | Violated invariant | Guards against |
|---|---|---|---|
| `quint-refcount-calib-g1-token-rollback` | `refcountCalibG1RollbackPreToken` | `cr3CounterRefinesFold` | losing the PlaceholderToken / generation gate on the in-process rollback (1cd975b90) |
| `quint-refcount-calib-g2-inline-reap` | `refcountCalibG2ReapInlineOnly` | `cr3CounterRefinesFold` | a cleanup path reverting to the inline-only delete (e5bdbff1b / I-040) |
| `quint-refcount-calib-g3-counter-presence` | `refcountCalibG3CounterAsPresence` | `cr4PresenceFromConfirmedUpload` | re-keying the needs-upload verdict on the liveness record (dd5c11376 / M_033) |
| `quint-refcount-calib-g4a-drain-recheck` | `refcountCalibG4aDrainNoRecheck` | `cr1NoLiveChunkCollected` | dropping the drain's same-transaction re-check before DeleteObject (aa738a5d7 / M_006) |
| `quint-refcount-calib-g5-no-heartbeat` | `refcountCalibG5NoHeartbeat` | `s5LiveOwnerNeverReaped` | losing the heartbeat that keeps live uploads below the reclaim thresholds (a1b49b4a3) |

The remaining five modules (`refcountCalibG1CompletionUnclaimGated`,
`refcountCalibG1ReapPathMatched`, `refcountCalibG2SweepCollapsedCount`,
`refcountCalibG3JointStaleSkip`, `refcountCalibG5NoHotpathReclaim`) are
evidence modules: committed, typechecked with the tree, re-runnable with
the command above, not in CI. G1/G2's wired checks guard machinery the
campaign intends to delete (the token, the decrement family); they stay
in CI until Phase 1c removes that machinery and are then retired or
re-pointed exactly as the retry campaign's were in its Phase 2; the
G3/G4a/G5 checks guard mechanisms that survive the replacement and are
re-pointed at the counter-free model of record in Phase 2 (design §3.4).

### Phase-0 exit-gate verdict (calibration criterion)

**Met for the calibration clause of design §5 / §5a.** Every encodable
family (G1, G2, G3, G4a, G5) falsifies at least one campaign invariant
through a representative override — G1: 3 overrides falsify (plus the
module-local ownership form); G2: 2; G3: 2 (including the pre-registered
joint-revert row); G4a: 1; G5: 1 plus the pre-registered witness-form
row — all as predicted, each row recording the falsified invariant,
depth, and state count, with restricted-alphabet overrides carrying their
baseline HOLDS and regime-verbatim overrides citing the Stage-B
exhaustive verdicts. Every non-encodable row (G4b, G6, G7, and the
per-commit NOT-ENC / ENC-A / SUBS rows inside G1–G5) carries its
pre-registered disposition with the missing dimension and the covering
rule/test named. No encodable-family representative failed to falsify
without an accepted explanation (§5a bullet 1 not tripped); no §3.3
invariant falsified on the unmodified as-built model (§5a bullet 4 not
tripped — the late-mark finding arises only under a relaxed clock and is
dispositioned above); the Stage-A consumer audit already established §5a
bullet (a) clean and the `'uploading'`-as-live spec check (§5a bullet 3)
closed. The calibration input to the go/no-go is therefore green. The
remaining Phase-0 gate items are outside this stage and still open: the
mark-scan cost measurement on production-scale data (and the junction
fallback decision it prices), the collect-soundness enforcement choice
(timeout vs monitored assumption), and the drafting of the replacement
`#r()` rules (`store.chunk.liveness-derived`, `store.gc.chunk-collect`).

### Phase-1 input list

What the calibration adds to the Phase-1 plan beyond the design's
existing commitments:

- **The late-mark window is a named dependency to keep or close.** The
  replacement keeps `mark_chunks_uploaded`, the drain re-check, and the
  stale reclaims verbatim, so CR-1/CR-4(b) keep leaning on the heartbeat
  contract through the window described under Findings. Phase 1a's
  replacement model (design §4.6) should add a late-mark witness pair
  alongside the mark-stale-race pair (reachability under a relaxed
  contract; excluded under the kept contract), and Phase 1 should either
  add `AND deleted = FALSE` to the mark statement (a one-conjunct change
  that closes the window structurally — the resurrect path already
  forces a re-upload after any soft-delete, so the narrowing costs
  nothing) or carry "the wall-clock heartbeat task outlives the PUT
  fan-out" as a named, monitored assumption next to the §4.1
  writer-transaction bound. Decision belongs to Phase 1a, not here (no
  Rust is touched in Phase 0).
- **The §4.6 acceptance re-run set gains one member.** Beyond the
  design's G4a/G5 re-runs, the `937a9c928` completion-clobber override
  should be re-pointed at the replacement model: the completion claim
  gate survives as a path-row janitor and its falsifiable content
  (premature visibility of an in-flight successor) is unchanged by the
  counter's removal.
- **G1/G2 acceptance rows flip to "cannot recur by construction"** once
  the counter, the token, and the decrement family are deleted (design
  §4.6); their wired calibration checks are retired or re-pointed in
  Phase 2. Until Release B ships, they stay in CI guarding the as-built
  machinery.
- **Keep the dedup verdict atomic with the upsert when the touch lands.**
  The `b1c7a9497` subsumption note: Phase 1a adds `last_referenced_at`
  to the same upsert statement; the §4.5 amendment of
  `store.cas.upsert-inserted+2` must keep the RETURNING-atomic wording so
  the pre-`b1c7a9497` re-query shape cannot reappear alongside the new
  column.
- **Loop-existence obligations stay outside the model.** The structural
  CR-2 encoding cannot see a missing background loop (the `d617bf3e5`
  sweep-wiring half), so the replacement's collector-existence and
  backstop-cadence obligations are carried by the runtime metrics and
  alerts the design already specifies plus the L4-style operability
  tests — not by the model. Do not claim model coverage for them in the
  Phase-1 exit gates.
- **The S4/L3 admission-predicate caveat carries over.** The replacement
  model should keep ownership gates falsifiable at the consequence level
  (the calibration override pattern), since its admission-predicate forms
  will have the same structural blindness to additive overrides.
- **I-207 stays latency-only.** The hot-path reclaim's chunk-awareness
  can be deleted in Phase 1c without a safety re-run (the witness-form
  row shows safety is untouched by its absence); what must be preserved
  is the path-level latency obligation (a stale placeholder yields within
  the 300 s threshold), which it keeps as a path-row janitor.


---

## 3. Phase-1a measurements: the T-1a.1 / T-1a.1b / T-1a.1c gate chain and T-1a.4

> Origin: refcount-invariant-map.md §§ "Phase 1a measurements and
> adjudications" (T-1a.1 NO-GO, the set-based follow-up spike, the T-1a.1b
> anti-join, the T-1a.1c capped-cycle confirmation that licenses
> COLLECT_CYCLE_VICTIM_CAP=500_000 and the server-side mark, T-1a.4
> monitored-assumption decisions, and the Wave-A1 review findings), verbatim.

#### T-1a.1 — mark-scan cost: NO-GO; the §5a junction fallback is triggered

The §5a go/no-go measurement (plan T-1a.1, sign-off item 5) was run
against the prescribed collector mark shape — one connection, keyset
pages over `manifest_data JOIN manifests`, the fallible per-manifest
parse, batched `INSERT … ON CONFLICT DO NOTHING` into
`TEMP TABLE live_chunks(blake3_hash BYTEA PRIMARY KEY)` — using the
`#[ignore]`d, env-tunable harness at
`rio-store/src/gc/mark_scan_bench.rs` (the entry-count mix and sharing
model are documented in its module doc: median a few dozen entries, a
tail at the 10 GB-NAR / ~160 k-entry class, cross-manifest dedup factor
≈ 2.4×), at three scale points — 15 k, 150 k, and the production-scale
1.5 M chunked paths — on the most production-like hardware available to
the campaign (a 192-core EPYC dev box, ephemeral PostgreSQL 18 with
fsync off and tmpfs-backed storage; faster than the production database
class on both I/O and clock, so the measurement is a lower bound on
production cost). Raw figures live in the introducing commit's message
and the run transcripts; the verdict-relevant magnitudes are below.

**Verdict: NO-GO** against the sign-off item 5 threshold (full mark scan
≤ 5 minutes at the ~1.5 M-path scale; linear-or-better growth;
temp-table-bounded memory):

- the production-scale scan takes roughly seven to eight times the
  five-minute budget (tens of minutes, not minutes);
- growth is super-linear — each 10× increase in path count costs
  roughly fourteen to fifteen times the scan wall-clock, because
  per-reference throughput degrades as the mark-set btree grows far
  past the session's local buffer pool — so the growth clause fails
  independently of the absolute-time clause;
- only the memory clause holds: the working set is bounded by the temp
  table (the client holds one page of manifests and one bounded insert
  buffer).

The dominant cost is the per-reference `ON CONFLICT` probe into a
temp-table btree that is orders of magnitude larger than PostgreSQL's
session-local `temp_buffers`. That cost is intrinsic to the prescribed
stream-parse-insert shape on a single backend — not an artifact of batch
size, page size, hardware, or the synthetic mix: at the measured
throughput, fitting the five-minute budget would require the store's
total reference volume to be several times smaller than any mix
consistent with the inventory's §3.3 shape (the known 10 GB-NAR-class
paths alone rule that out), and throughput keeps degrading as the mark
set grows.

**Consequences (design §5a; plan T-1a.1 step 4).** The junction fallback
— `chunk_refs(store_path_hash, blake3_hash)` maintained in the upgrade
transaction with `ON DELETE CASCADE`, mark as an indexed anti-join — is
triggered, NOT built: §5a requires re-deriving §4.1 and re-entering
design review to price the write-amplification and blob-vs-junction
drift obligations before any collector code exists. The collector tasks
of the Phase-1 plan (T-1a.3 onward) are void pending that re-entry.
Migration 068 / the upsert touch (T-1a.2) was also deliberately not
shipped ahead of the re-entry: whether `last_referenced_at` survives the
junction design, and in what shape, is a re-entry question, and shipped
migrations are frozen. The implied cadence/GC-lock-hold budget for the
as-designed shape — a scan of tens of minutes holding `GC_LOCK_ID` in
every `run_gc` invocation and every backstop run — is far outside what
phase-3-of-run_gc or a daily backstop can absorb, which is the same
no-go stated as an operational cost rather than a threshold breach.

The other two still-open Phase-0 items absorbed into Wave A1 — the
collect-soundness enforcement choice (T-1a.4's histogram + alert) and
the replacement `#r()` rule drafts with `chunkCollect.qnt` (T-1a.5) —
remain open: both are shaped by the mark mechanism the re-entry
chooses (the soundness condition's form and the model's mark action
both change under a junction-maintained mark), so recording a choice
now would prejudge the re-entry. The Wave A1 stop-and-report record is
`refcount-a1-blocker-T-1a.1.md` (campaign working notes, alongside the
design and plan documents).

#### T-1a.1 follow-up — set-based mark formulations (measurement spike, not a design decision)

Before the §5a re-entry prices the junction fallback, two set-based
reformulations of the SAME scan-time mark — liveness still derived
from `manifest_data` at collect time, no new write-path state, the
live set still rebuilt per cycle into a session temp table — were
measured with the same harness, fixture, scale points, and hardware as
the NO-GO record above (`gc::mark_scan_bench`, release profile, single
backend, session `work_mem = 4GB` for the set-based dedup, PostgreSQL
defaults otherwise; raw transcripts in the introducing commit
message):

- **copy + GROUP BY** (`mark_scan_bench_copy_groupby`) — the
  prescribed keyset scan and fallible client-side parse, unchanged,
  but references stream into an unindexed temp table via binary `COPY`
  and are deduplicated once at the end by a single set-based
  `GROUP BY` into `live_chunks` — no per-row `ON CONFLICT` probe, no
  btree maintained during the scan.
- **server-side expansion** (`mark_scan_bench_server_side`) — no
  client round-trip at all: a fail-closed validation pass (version
  byte, 36-byte entry alignment, `MAX_CHUNKS`; any violation aborts
  the cycle, preserving the §4.4 polarity), then one
  `CREATE TEMP TABLE … AS SELECT DISTINCT` statement that expands
  every `chunk_list` inside the server (`generate_series` +
  `substring` over a once-per-manifest detoasted copy) and
  deduplicates in the same statement.

Both formulations produce exactly the prescribed mark product (a
session temp table holding the distinct live hashes) and are pinned to
it: at every scale they reproduce the mark-set cardinality the
prescribed shape produced, and a known manifest's hashes are asserted
present in the result (a slicing/encoding-misalignment check). The
collect-phase anti-join stays outside the measured window, exactly as
in the NO-GO record.

| Formulation | 15 k paths | 150 k paths | 1.5 M paths | growth per 10× paths | spill at 1.5 M |
|---|---|---|---|---|---|
| Prescribed (per-row `ON CONFLICT`; the NO-GO above) | 11.1 s | 163.0 s | 2293.8 s (38 min 14 s) | 14.7×, 14.1× | n/a (20 GB PK btree) |
| Copy + `GROUP BY` | 1.9 s | 29.0 s | 374.3 s (6 min 14 s) | 15.0×, 12.9× | 2.5 GB |
| Server-side expansion | 1.5 s | 18.8 s | 219.7 s (3 min 40 s) | 12.4×, 11.7× | 2.5 GB |

Verdict against the sign-off item 5 threshold (full mark scan ≤ 5
minutes at the ~1.5 M-path scale; linear-or-better growth; bounded
memory):

- **Copy + GROUP BY: does not meet.** Roughly 1.25× the five-minute
  budget at 1.5 M (clause 1 fail); growth still super-linear
  (clause 2 fail); only the memory clause holds (the client holds one
  page plus one ~38 MB COPY buffer, the server side is
  `work_mem`-bounded and spills to temp files). Most of its cost is
  the single-connection round-trip itself — about half the wall-clock
  goes to pulling ~12 GB of blobs out and pushing ~12.5 GB of
  references back over one connection before dedup even starts.
- **Server-side expansion: meets the time and memory clauses with
  margin on this hardware.** 3 min 40 s is ~73 % of the budget
  (clause 1 pass); memory is bounded by `work_mem` (≈2.5 GB of
  temp-file spill observed) plus the ~9.4 GB result table (clause 3
  pass). Growth is 12.4× then 11.7× per 10× paths — ≈1.2× per decade
  against the reference volume, so not strictly linear (clause 2
  marginal), but per-reference throughput falls only 2.4 → 1.5 M
  refs/s across two decades versus 306 k → 144 k refs/s for the
  prescribed shape; the degradation is attributable to the dedup hash
  outgrowing CPU caches and to the spill at the largest point, not to
  a per-row probe that keeps getting more expensive. A further 10×
  store (15 M chunked paths) extrapolates to ~40 minutes — graceful
  degradation, not unbounded headroom.

Caveats carried from the NO-GO record: same dev-box hardware
(tmpfs-backed PostgreSQL, fsync off, EPYC clocks), so absolute times
remain a lower bound on production cost and the ~27 % margin shrinks
on a slower database host; the dominant cost is single-core CPU in the
expansion + aggregate, and `workers_planned = 0` throughout (no
parallel query was used — headroom exists there if the statement's
target is made non-temporary). Other shapes were not pursued: the
threshold was met with margin by a same-architecture formulation
(the spike's stop-early condition), and the "anti-join `chunks`
directly against the aggregated references" variant needs a populated
production-scale `chunks` table in the fixture to mean anything. An
incremental mark (only manifests changed since the last cycle) was
likewise not measured: it changes the §4.1 correctness argument (the
live set would no longer be re-derived from scratch each cycle) and is
out of scope for a measurement spike.

What this changes for the §5a re-entry: the junction fallback is no
longer the only priced option. The scan-time architecture has a
formulation that fits the stated budget at the design-point scale with
no new write-path state; the re-entry can weigh the junction's
write-amplification and blob-vs-junction drift obligations against a
server-side mark whose remaining risks are production-hardware margin,
the strictness of the linear-growth clause, and plan-shape sensitivity
across PostgreSQL versions, rather than against a 38-minute scan.

#### T-1a.1b — collect-phase anti-join (re-entry gate (c))

Both records above cover the mark phase only; design §5b gate (c)
requires the collect-phase anti-join priced on a populated
production-scale `chunks` table, recorded before any Wave A2 task
starts. The bench gained a third entry point
(`mark_scan_bench_collect_phase`) that seeds `chunks` to match the
manifest fixture and then runs the full cycle of the adopted
formulation on one connection, timing three phases separately: the
server-side mark (validation pass + set-based expansion into
`live_chunks`), a one-time prepare step on the mark product (a unique
index plus ANALYZE — what makes the per-batch anti-join an index probe
instead of a per-batch hash or sort of the whole mark set), and the
batched collect loop itself in the orphan-chunk-sweep skeleton (per
batch, one transaction: a candidate scan with the `NOT EXISTS`
anti-join and the `GREATEST(created_at, last_referenced_at)` grace
term, then a sorted `= ANY` soft-delete `RETURNING`), with a keyset
cursor on the candidate scan, mirrored into the anti-join's inner
side, so each `chunks` row and each mark-set entry is examined once
across the whole loop. Without the cursor the loop is quadratic in
batch count at the design point — every batch re-probes all marked
rows that precede its candidates (and the as-written single-statement
`IN (… LIMIT …)` form additionally seq-scans `chunks` once per batch
on the UPDATE side, which the first 15 k probe run surfaced) — so the
live arm (T-1a.8) is expected to adopt the same candidate-scan +
sorted-`ANY` + cursor shape, and the bench's EXPLAIN guard pins it.

Fixture: one `chunks` row per distinct referenced hash (refcount 1,
`uploaded_at` set, `last_referenced_at` NULL), plus a 10 %
unreferenced population — 90 % of it old and untouched (the expected
victims), 5 % younger than grace, 5 % old but freshly touched via
`last_referenced_at` — so the grace term and the migration-068 touch
column both do real filtering work and the victim volume is a
deliberately generous bound on one cycle's garbage (a store turning
over a tenth of its references between collect cycles). Same hardware,
PostgreSQL, `work_mem`, and synthetic mix as the mark records; collect
batch LIMIT 10,000. Raw figures are in the introducing commit message
and the run transcripts; the verdict-relevant magnitudes:

| paths | mark | prepare | collect (victims) | combined |
|---|---|---|---|---|
| 15 k | 1.6 s | 0.5 s | 4.1 s (136 k) | 6.3 s |
| 150 k | 18.5 s | 4.3 s | 35.3 s (1.26 M) | 58.1 s |
| 1.5 M | 3 min 24 s | 51 s | 7 min 40 s (12.4 M, 1,243 batches) | 11 min 54 s |

**Verdict against the amended budget (the five-minute-class lock-held
window of §5b, graceful-growth qualification): exceeded.** The mark
phase alone stays within the budget (3 min 24 s here, consistent with
the adopted record); adding the prepare step and the collect pass —
every existing chunk row probed once against the mark product plus the
soft-delete writes for the victim volume — brings the lock-held window
for a full cycle to ~12 minutes at the design point, ~2.4× the
five-minute-class budget, growing ~1.0–1.3× per decade on top of the
mark's own growth (combined 6.3 s → 58.1 s → 714 s across the three
points). Per the plan's T-1a.1b step 4 and the rollback-story abort
criterion, this routes to an explicit adjudication by the campaign
owner — an accepted cadence/lock-budget relaxation recorded here, or a
further design re-entry — and Wave A2 does not start until that
adjudication is recorded. Scan/collect duration does not enter the
collect-soundness condition (§4.1), so the breach is a cost/cadence
question, not a correctness one; the candidate levers the §5b record
already names (backstop-only cadence, parallel-query headroom —
`workers_planned = 0` throughout these runs too) apply to the combined
cycle as much as to the mark.

**EXPLAIN plan-shape guard (gate (b)) extended to the anti-join.** The
bench now asserts, at and above the 150 k scale point, that the
expansion plan keeps its set-based aggregate over the server-side
expansion and that the per-batch candidate scan is index-driven on
both sides (`chunks_pkey` and the `live_chunks` index; no Seq Scan of
either relation, no Sort) — the plan regression that would silently
reproduce the NO-GO cost class now fails the bench instead of
shipping. The measured candidate-scan shape at the enforced scale
points is a Merge Anti Join over the two indexes with the keyset bound
on both sides; `workers_planned = 0` throughout, so the parallel-query
headroom noted in the mark record remains unexploited here too.

Caveats carried forward: same dev-box hardware as the mark records
(tmpfs-backed PostgreSQL, fsync off, EPYC clocks), so absolute times
are a lower bound on production cost; the victim-write term scales
with the unreferenced ratio (10 % here — a generous steady-state
bound), while the scan term (every existing chunk row probed against
the mark set once) is fixed by store size; the prepare term is paid
once per cycle and could be folded into the expansion statement later
without changing the architecture.

Statement amendment (Wave-A1 review, finding C1/C11): the pinned
soft-delete template (`COLLECT_BATCH_UPDATE_SQL`) now re-checks the
collect predicate's row-local conjuncts — `deleted = FALSE` and the
`GREATEST(created_at, last_referenced_at) < cutoff` grace term — in its
own WHERE clause, as the T-1a.8 consequence note below requires of the
live arm; the EXPLAIN gate-(b) guard pins only the candidate scan
(unaffected), and the cost delta is one `GREATEST` evaluation per
already-locked row, so the recorded gate-(c) and gate-(c)-v4 figures in
this entry and T-1a.1c stand. A behavioral + structural regression test
(`collect_batch_update_rechecks_collect_predicate`) fails if the
conjunct is dropped again.

#### T-1a.1c — capped-cycle confirmation (re-entry gate (c), v4 form)

The second re-entry redefined gate (c) to the capped cycle (design §4.1
step 3 v4, `COLLECT_CYCLE_VICTIM_CAP = 500_000`; plan sign-off item 8):
the gate now asks whether mark + prepare + collect-at-cap fits the
combined five-minute (300 s) lock-held budget at the 1.5 M-path design
point, with a backlog larger than the cap draining across cycles by
design rather than stretching one cycle. The bench's collect loop
gained the cap (default = the design value, env-tunable for smoke
runs), a clamped final batch so a cycle never overshoots the cap, a
split of the collect term into its anti-join candidate-scan and
soft-delete halves (so the sparse full-pass scan cost stays visible
separately from the victim-write cost the cap bounds), and the keyset
cursor at the stop point in the report. Mark, prepare, batch shape,
fixture, hardware, PostgreSQL, `work_mem`, and batch LIMIT are
unchanged from the T-1a.1b record; the 12.4 M-victim backlog is
exactly the case the cap must absorb.

Two release-profile runs at the design point were taken: the first
overlapped repository-scan and build activity from the executor's own
session on the shared dev box during the mark window, and is kept on
record as a contended variance data point rather than discarded (the
same identify-the-artifact-and-rerun discipline as the T-1a.1b
bring-up probes); the second ran with the executor quiescent. Raw
figures in the introducing commit message and the run transcripts;
the verdict-relevant magnitudes:

| run | mark | prepare | collect at cap (scan + soft-delete) | combined |
|---|---|---|---|---|
| r1 (executor-contended) | 228.6 s | 53.0 s | 21.0 s (18.5 s + 2.5 s) | 302.6 s |
| r2 (executor quiescent) | 207.6 s | 52.3 s | 18.4 s (16.0 s + 2.4 s) | 278.3 s |

Both runs: 50 batches, exactly 500,000 victims soft-deleted, cap
reached with the keyset cursor reported, plan-shape guard (gate (b))
green, `workers_planned = 0`, mark-set size 138,042,866 (identical to
the T-1a.1b record), and the protected populations (referenced,
younger-than-grace, freshly-touched) untouched — the capped cycle's
soundness shape is the uncapped cycle's, only the stopping rule
differs. The collect-at-cap term measured ≈37 µs/victim in the
quiescent run — the same per-victim cost as the T-1a.1b record and
well inside the 2× allowance the cap derivation budgeted (≈42 µs in
the contended run, still inside it).

**Verdict against the redefined gate (c) (combined ≤ 300 s at the
design point): PASS** — 278.3 s, ≈93 % of the budget, in the
executor-quiescent run; the contended run exceeded the budget by
0.9 % (302.6 s), which is within the run-to-run band of the mark term
on this shared box and is recorded, not adjudicated over the
quiescent run.

What the pair of runs adds to the record: mark + prepare alone is
254–282 s across the three measured cycles of this fixture (T-1a.1b
and the two runs here), i.e. 85–94 % of the budget is consumed before
any collect work happens, and the run-to-run band of those terms on
this shared dev box (~10 %) is the same order as the entire
collect-at-cap allowance. The cap bounds the term it was derived to
bound; the remaining margin is carried almost entirely by mark-phase
variance. That makes re-entry gate (a) — the production-DB-class mark
confirmation from the additive-release window, on the production
database where the cycle runs alone under the GC lock — the
load-bearing check for the budget, exactly as the v3 record already
framed it; the cycle-duration histogram (T-1a.3) is the runtime
monitor for it. Caveats carried forward otherwise unchanged from
T-1a.1b: tmpfs-backed PostgreSQL, fsync off, EPYC clocks (absolute
times are a lower bound on production cost); the sparse full-pass
scan term is bounded by store size, not by the cap, and stays
monitored (cycle-duration histogram + stalled alert), not gated.

#### T-1a.4 — collect-soundness enforcement: the monitored-assumption option (closes the second still-open Phase-0 item)

The §4.1 collect-soundness condition — no chunk-referencing write
transaction outlives `grace − clock slack` — is carried as a **named,
monitored assumption**, not as enforcement (plan P4 / sign-off
item 3):

- `rio_store_chunk_upgrade_tx_seconds` (histogram) measures every
  *committed* `upgrade_manifest_to_chunked` transaction from `begin()`
  to `commit()` — the single chunk-referencing write transaction in
  the system; the histogram is recorded at commit on the success path,
  so an aborted upgrade (which commits no manifest and cannot endanger
  collect soundness) is not recorded. Bucket boundaries are placed at
  the alert thresholds so the threshold queries are exact.
- The wired alert `RioStoreChunkUpgradeTxSlow`
  (infra/helm/rio-build/templates/prometheusrule.yaml) fires at
  warning when the p99 over a 15-minute window exceeds `grace/2`
  (150 s), and at critical when at least one committed transaction
  exceeded `grace − 60 s` (240 s) in the window — an exact
  per-violation count read off the 240 s histogram bucket (the
  Wave-A1 review, findings C5/C10, replaced the original p99-form
  critical arm: a p99 structurally tolerates a single overrun once
  upload volume is non-trivial, and the chunkCollect
  writer-overrun falsification shows a single overrun is sufficient
  once the live arm deletes). This is the runtime carrier of the
  assumption, in the same sense that the READ-COMMITTED re-evaluation
  assumption is carried as a named assumption: a firing alert means
  the soundness margin is eroding (warning) or was within 60 s of
  violated (critical) and grace (or the upload path) needs attention
  before the live collect arm is enabled or kept enabled.
- Residual blind spot, accepted for Phase 1: because the histogram is
  recorded only at commit, a still-open transaction is invisible to
  the carrier until the moment it commits — which is exactly when it
  becomes dangerous. In shadow mode nothing is deleted, so the gap has
  no harm reach in Phase 1; before the Wave A2 live arm is enabled the
  campaign owner must either accept the gap explicitly for the live
  arm or close it with a store-DB long-transaction check
  (`max(now() − xact_start)` from `pg_stat_activity`) and/or the §4.1
  collector-side `least(cycle_started_at, min(xact_start))` snapshot
  anchor. Recorded here so the A2 entry decision sees it.
- No `statement_timeout` is set in Phase 1 (the
  enforcement-by-timeout option of §4.1): a timeout would add a new
  writer-failure mode for zero observed need, and the histogram is
  exactly the data that would justify a timeout value later if one is
  wanted. The carrier change above alters only the monitor's
  statistic, not this decision.
- The collector-side alternative — anchoring the collect threshold at
  `least(cycle_started_at, min(xact_start) of transactions open at
  the snapshot)` — is noted as available but not taken (it
  complicates the snapshot for a window the grace term already covers
  whenever the assumption holds).

This closes the second still-open Phase-0 item (the collect-soundness
enforcement choice; the first — the mark-scan cost measurement — was
closed by the T-1a.1 records above, and the third — the replacement
`#r()` rule drafts — lands with the replacement model). The runbook
section in docs/ops/gc-enablement.typ documents the alert's meaning
and remediation; the histogram is live from the additive release, so
the Release-A observation window (re-entry gate (a), T-1a.7) also
produces the empirical upgrade-transaction-duration distribution this
assumption is judged against.

#### Wave-A1 collector code review — recorded findings and dispositions (T-1a.7 step 3 / plan v5 start-condition item 7)

A three-reviewer adversarial review of the Wave-A1 shadow collector
(SQL/data, concurrency/lifecycle, operability/tests; 2026-05-27) was
recorded as the Wave-A2 entry criterion requires, and the confirmed
findings were fixed (or explicitly adjudicated) in the
collector-hardening change set before any Wave-A2 task starts. Eleven
confirmed findings deduplicate to six issues; one further claim was
refuted during verification; two minors were applied. Dispositions:

- **C1/C11 (blocking) — pinned soft-delete template omitted the grace
  conjunct.** Fixed: `COLLECT_BATCH_UPDATE_SQL` re-checks
  `deleted = FALSE AND GREATEST(created_at, last_referenced_at) <
  cutoff` in its own WHERE with a cutoff bind, the rationalizing
  comment is gone, and `collect_batch_update_rechecks_collect_predicate`
  (structural pin + touched-candidate-survives behavior) fails if the
  conjunct is dropped again. Recorded against the gate-(c) records in
  T-1a.1b above.
- **C2 (important) — no single cycle snapshot.** Fixed: the cycle's
  read phase (cutoff, validation, mark expansion, prepare, shadow
  report) runs in one REPEATABLE READ transaction, so the drift
  gauges/would-collect are computed on one MVCC snapshot and the
  validation→expansion TOCTOU is closed structurally. The
  separately-reported TOCTOU claim had been downgraded by the
  reviewers (the fail-closed re-validation inside the expansion's own
  snapshot already bounded it); the transaction closes it regardless.
- **C3/C4/C8 (important) — 4GB work_mem / temp-table session leak into
  the shared pool.** Fixed: `SET LOCAL` for both GUCs and an
  `ON COMMIT DROP` variant of the expansion CTAS inside the cycle
  transaction (shared constant untouched for the bench); leak
  regression tests drain the pool after a completed and a mid-cycle
  failed cycle.
- **C5/C10 (important) — collect-soundness carrier was a p99.** Fixed:
  the critical arm of `RioStoreChunkUpgradeTxSlow` is now the exact
  over-240 s violation count from the histogram buckets; T-1a.4 above
  records the carrier change and the accepted commit-time blind spot.
- **C6 (important) — stalled alert false-fires per pod from boot.**
  Fixed: `RioStoreGcCollectStalled` aggregates across replicas
  (`sum(increase(...))`) with `for: 30m`; the helm alert-quality
  fragment gained a staleness-aggregation bug class so the next
  zero-activity alert cannot regress this.
- **C7/C9 (important) — the daily backstop ran the heaviest cycle at
  every pod boot on every replica.** Fixed for the boot-firing half:
  the backstop ticker is armed one full interval after spawn
  (regression test pins it), the cadence is documented in the spawn
  docs, metric help, and runbook, and cross-replica dedup remains the
  GC advisory lock. The additional C9 proposal — a persisted
  cluster-wide recency gate (new `gc_collect_state` table + migration)
  capping the backstop at one cycle per ~24 h fleet-wide — is
  **adjudicated not adopted** in this batch: with the boot trigger
  removed the worst case is one lock-serialized cycle per replica per
  day (bounded by the autoscaler's replica count), shadow-mode cycles
  are read-only, and adding write-path schema for a cadence question
  is not warranted before the A2 lock-budget pricing; if Release-A
  observation shows the per-replica cadence is still too hot, the
  recency gate is the named follow-up and belongs with the A2
  lock-held budget work.
- **Minors:** DB-error cycles are now visible immediately
  (`outcome="error"` incremented by both callers, pre-registered,
  described) instead of only via the 25 h stalled alert; the stalled
  alert's cluster-wide aggregation (the second minor) is the C6 fix.
- **Out-of-repo records:** the campaign design record's §4.1/T-1a.4
  monitored-assumption wording and the phase-1 plan's T-1a.8 step-1
  soft-delete sketch live outside this repository; aligning them with
  the amended carrier and the predicate-re-checking soft-delete shape
  is flagged to the campaign owner rather than edited here.


---

## 4. Replacement-model encoding notes and findings (chunkCollect.qnt)

> Origin: refcount-invariant-map.md § "Encoding notes and findings"
> (replacement model, Phase 1a), verbatim — the
> quarantine-vs-complete-manifests restriction rationale and the
> replacement-side image of the Stage-C late-mark finding.

### Encoding notes and findings

- **CR-2's structural content shrinks with the counter, and that is the
  honest result.** As-built, clause (a) ("garbage stays eligible")
  carried the real conditionality — a stuck refcount made garbage
  permanently invisible to the standing machinery. Under the
  replacement, liveness is recomputed every cycle, so nothing per-chunk
  can durably exclude garbage from collection; the only durable
  exclusion is the system-wide fail-closed pause while an unparseable
  `chunk_list` exists. The structural form therefore reads "garbage
  implies no corrupt manifest exists" plus the unchanged outbox clause:
  discharged by construction in the fault-free regimes, expectedly
  falsified in the corrupt regime (the §4.4 trade made checkable), with
  the carved form keeping the outbox clause and the
  resumption-after-remediation half pinned by `quarantineResumeRun`.
  The wall-clock half of the rule's bound (cadence, the capped-cycle
  backlog drain, drain lag) stays with runtime metrics per the Phase-1
  input list's loop-existence note.
- **The row-lock / READ-COMMITTED assumption is now explicit in the
  encoding.** The sweep batch cannot fire on a hash staged by an open
  upgrade transaction (the chunk-row lock); the post-commit
  re-evaluation outcome is encoded at the commit action — protected iff
  the backdated touch still postdates the cycle cutoff. This is the
  §3.2 named assumption carried by the as-built model, made visible
  here because the split-upgrade regimes are exactly about the window
  it governs. Consequence, recorded for T-1a.8: the live arm's
  soft-delete UPDATE must re-evaluate the collect predicate (at minimum
  the `deleted = FALSE` and `GREATEST(created_at, last_referenced_at) <
  cutoff` conjuncts) in its own WHERE clause, not only in the candidate
  scan — re-evaluating a hash-only WHERE after a row-lock wait is what
  the design's §4.1 "re-evaluating its predicate" sentence forbids
  relying on, and the model's writer-bounded HOLDS is stated against
  the predicate-re-checking shape.
- **S4/L3 stay admission-predicate forms; the consequence level is
  carried by `noReferencedChunkSwept` and CR-1** (the Stage-C caveat
  carried into the replacement, per the Phase-1 input list). The
  acceptance re-run (T-1a.6) exercises the ownership gates by reverting
  them and falsifying at the consequence level.
- **Quarantine is modeled against 'complete' manifests only.** The
  adjudication-7 trigger (consecutive failed cycles on the same named
  manifest) cannot accumulate against a transient placeholder, and
  corrupt `'uploading'` rows self-heal via the reapers (design §4.4).
  Modeling an unrestricted quarantine admits an operator deleting a
  live, mid-upload placeholder, whose late S3 PUT then recreates an
  object with no scheduled delete — an orphan-object shape that is not
  a property of the design's remediation story; the restriction is
  recorded here rather than silently narrowing the rule text.
- **Under the relaxed heartbeat contract only**, two benign bookkeeping
  windows are reachable even with the deleted-guard present: a
  reclaimed-then-collected chunk whose still-alive owner later re-PUTs
  the object (an unreferenced S3 object with no scheduled delete,
  violating CR-2's outbox clause), and a transient
  `uploaded_at`-set/object-absent state when the late mark lands on a
  row a concurrent writer has just resurrected (violating CR-4(b) until
  that writer's own re-upload lands). Neither has data-loss content,
  both vanish under the production heartbeat contract (the four main
  regimes HOLD CR-2/CR-4 exhaustively), and the late-mark holds-half
  therefore checks the pair's actual claim — CR-1 — plus the
  structural/ownership set. This is the replacement-model image of the
  Stage-C late-mark finding: `uploaded_at`-as-presence still leans on
  the heartbeat contract; the T-pre.1 guard closes the data-loss trace,
  not every bookkeeping wrinkle of a deliberately broken contract.
- **No demotions, no stop-and-report events.** Every check in the
  plan's must-stay-wired set is wired and green; the two demotable
  holds-halves are wired exhaustive checks; no invariant falsified
  outside the pre-registered corrupt-regime falsification; per-check
  wall-clocks fit the T-1a.5 step-6 budget with margin (largest regime
  ≈2 minutes of checker time on the campaign dev box, transcripts in
  the introducing commit).


---

## 5. Phase 2 — the acceptance table: the calibration corpus against the replacement architecture

> Origin: refcount-invariant-map.md § "The acceptance table" (G1-G7 against
> the replacement, the M_023/M_033 lessons restated, and the summary),
> verbatim.

### The acceptance table: the calibration corpus against the replacement architecture

Design §4.6/§5's Phase-2 obligation: every family of the Stage-C
corpus (G1–G7, plus the M_023/M_033 lessons called out by the design's
§1) carries a disposition against the architecture the code now runs
on — the collect cycle of `gc/collect.rs` is the only producer of
chunk soft-deletes and outbox rows, eligibility is the manifest fold
recomputed each cycle (server-side fail-closed mark + grace/touch
term), the counter, the `PlaceholderToken`, the decrement/zero/enqueue
family, the chunk-aware reap paths, the path-sweep chunk block, and
the hourly orphan-chunk sweep no longer exist (Release B record), the
M_023 CHECK and `idx_chunks_gc` are dropped (069), and presence is
`uploaded_at` only.

The Stage-C table above proved the *as-built model* would re-find each
encodable bug in the *as-built code*; the family-level re-run at Wave
A2 entry (T-1a.6) proved the *replacement model* still falsifies when
the surviving mechanisms are reverted. This table completes the
obligation per corpus row. Verdict legend, following the retry and log
campaigns' tables: **CONSTRUCTION** — the state or code path the bug
lived in does not exist under the replacement; the cited mechanism is
what replaced it (the residual risk for every such row is a defect in
the collector itself, owned jointly by the chunkCollect regimes, the
collector test set, and the differential/EXPLAIN guards). **CHECKED**
— the mechanism survives deliberately; the named invariant, wired
check, or test holds the hazard down. **OUTSIDE** — no footprint in
the chunk-liveness decision path then or now; the named conventional
vehicle owns it, unchanged by this campaign. Wired-check names are CI
attrs (`nix/quint.nix`); test names are `rio-store` test functions.

#### G1 — a late or foreign cleanup clobbered someone else's upload

The family-level verdict is CONSTRUCTION for the chunk-accounting
content (there is no counter, token, or per-hash decrement left for a
late or foreign cleanup to corrupt) and CHECKED for the surviving
path-row ownership content (the claim gate on reap/completion and the
heartbeat survive as path-row janitors).

| Corpus row | Replacement verdict | Mechanism / checker |
|---|---|---|
| `1cd975b90` (token-less rollback double-decrement) | CONSTRUCTION | DEC-1 and the `PlaceholderToken` are deleted (Release B); the in-process rollback is the claim-gated `reap_one` row delete, so there is no decrement to double-apply and no foreign hash set to charge. The path-row half is CHECKED: `s4OwnerOnlyMutation` HOLDS in all four `quint-chunk-collect-*` regimes, `quint-chunk-collect-witness-late-cleanup-noop` pins the contended late-cleanup state as reachable, and `rollback_after_reap_and_reupload_is_noop` / `rollback_after_reap_and_fresh_reupload_mid_upload_is_noop` pin the no-op behavior. The retired wired guard is recorded in the Release B calibration-check disposition. |
| `937a9c928` (completion not claim-gated) | CHECKED | The completion claim gate survives as a path-row janitor. Re-falsified against the replacement model in the acceptance re-run (`refcountCollectG1CompletionUnclaimGated` falsifies the local ownership form and CR-1; the claim-gated baseline HOLDS exhaustively); behavioral coverage stays with the claim-gated completion unit tests (`store.put.placeholder-claim+2`). |
| `937a9c928` (heartbeat not claim-gated) | CHECKED | `l3NoForeignFreshen` (admission-predicate form) HOLDS in all four chunkCollect regimes; the harm remains an eventuality (delayed reaping), so the claim-gated heartbeat unit tests stay the behavioral pin, exactly as Stage C dispositioned. |
| `bf7e516e4` C1 (reap matched on path alone) | CONSTRUCTION + CHECKED | The chunk consequence (a foreign reap soft-deleting and enqueuing the successor's chunks) is unconstructible: reaps delete path rows only, and a successor's still-referenced chunk is in the next cycle's mark set by definition. The path-row half is CHECKED by `s4OwnerOnlyMutation` and the late-cleanup-noop witness, plus `upgrade_holds_for_update_against_reaper`. |
| `ae5f3190b` (rollback hash/size validation) | CONSTRUCTION | The rollback no longer takes a hash list at all (row delete only); the upload-path input validation that remains is OUTSIDE this campaign and keeps its existing unit tests. |
| `31bd9c512` (scanner staleness re-check inside the reap tx) | CHECKED | The orphan scanner survives as a path-row janitor; reaping a live owner is what `s5LiveOwnerNeverReaped` forbids (HOLDS, all regimes; `quint-chunk-collect-witness-scanner-reap` pins the reap as reachable; `quint-chunk-collect-threshold-order` pins the threshold ordering as load-bearing). The chunk-accounting consequence of a stale-view reap is CONSTRUCTION (nothing to decrement). |
| `539c2be7c` (reap status re-check inside the tx) | CHECKED | Same treatment as `31bd9c512` — the surviving hazard is path-row-only and sits under S4/S5 plus the existing reap tests. |
| `31ce52b14` (stale-chunk_list double decrement) | CONSTRUCTION | Reaps no longer read `chunk_list` and no decrement exists; liveness is recomputed from the durable manifests each cycle. |

#### G2 — a cleanup path forgot the chunks (leaked refcounts)

Family-level verdict: CONSTRUCTION. There is no chunk accounting for a
cleanup path to forget — an unreferenced chunk is an ordinary collect
victim regardless of which path deleted its manifests, which is also
why the historical leaks this family produced become reclaimable
(`live_cycle_collects_unreferenced_chunk_exactly_once`, named
`live_cycle_collects_stale_refcount_leak` until the 070 column drop
made the stale-counter seeding inexpressible).

| Corpus row | Replacement verdict | Mechanism / checker |
|---|---|---|
| `e5bdbff1b` (I-040 inline-only reap) | CONSTRUCTION | A reap that deletes only manifest rows is now the *correct* behavior; the chunks it leaves behind are unmarked next cycle and collected after grace. `quint-chunk-collect-witness-abandoned-upload` pins the crashed-upload garbage shape as reachable, the crash-regime `cr2NoStrandedGarbage` HOLDS structurally, and `live_cycle_collects_unreferenced_chunk_exactly_once` (renamed from `live_cycle_collects_stale_refcount_leak` at the 070 column drop) pins the end-to-end reclamation. The retired `quint-refcount-calib-g2-inline-reap` guard is recorded in the Release B disposition. |
| `dbb42232a` (abort/batch-drop still inline-only) | CONSTRUCTION | Same mechanism; the abort/drop-guard paths are pure path-row janitors (`gt13_batch_chunked_abort_leaves_chunks_unreferenced`, `batch_guard_drop_reaps_placeholders` pin the post-Release-B behavior). |
| `adfd303d7` C2 (shared chunk decremented once, not N times) | CONSTRUCTION | No by-count arithmetic exists; a chunk shared by N dying manifests is simply absent from the mark fold once all N are gone, however they die. |
| `d617bf3e5` (M_023 CHECK + orphan-chunk sweep wiring) | CONSTRUCTION + OUTSIDE | The CHECK was dropped by 069 because the quantity it constrained no longer exists (see the M_023 lesson row below). The sweep-wiring half (a background collection loop must exist and run) is OUTSIDE the model, exactly as the Phase-1 input list pre-registered: collector existence/cadence is carried by `run_gc_phase3_runs_live_cycle`, `backstop_first_cycle_waits_one_interval_after_spawn`, `backstop_skips_when_gc_lock_held`, the `RioStoreGcCollectStalled` alert, and the runbook — not by a model invariant. |
| `8d93ce6c1` (chunk_tenants junction cleanup) | OUTSIDE (subject deleted) | The table was dropped by migration 035 before this campaign began; nothing to disposition. |

#### G3 — the counter was used as an S3-presence signal (data loss)

| Corpus row | Replacement verdict | Mechanism / checker |
|---|---|---|
| `dd5c11376` (M_033: row-exists treated as uploaded) | CHECKED | CR-4 survives verbatim: `cr4PresenceFromConfirmedUpload` HOLDS exhaustively in all four chunkCollect regimes; `store.cas.upsert-inserted+2` / `store.chunk.liveness-not-presence` state it; `upsert_returning_sequential_needs_upload_set`, `upsert_returning_concurrent_both_need_upload`, and `sigkill_race_second_uploader_covers` pin the code path. The wired `quint-refcount-calib-g3-counter-presence` regression guard remains against the as-built model (re-pointing at the model of record is deferred — close-out). |
| `b1c7a9497` (dedup verdict re-queried after the upsert) | CHECKED | The RETURNING-atomic shape is kept and the 068 touch was added to the same statement (the Phase-1 input-list "keep the dedup verdict atomic" item); `store.cas.upsert-inserted+2` pins the wording, `upsert_touch_advances_last_referenced_at` and the upsert-RETURNING tests pin the behavior. |
| `127168477` (duplicate hashes in one UNNEST batch) | OUTSIDE | Unchanged vehicle: `upgrade_duplicate_hashes_pg_rejects` / `upgrade_deduped_hashes_ok` plus the `fuzz-manifest_deserialize` target. |
| `00fd5b12d` (PutChunk RPC missed `uploaded_at`) | OUTSIDE (subject deleted) | The RPC was deleted pre-campaign (`c5bb34612`). |
| G2×G3 joint revert (I-040 stale-skip trace) | CONSTRUCTION + CHECKED | The stale counter the dedup trusted cannot exist (no counter is maintained), and the dedup signal is `uploaded_at` only (CR-4 as above); `i040_inline_delete_stale_row_still_reuploads` pins the historical trace end-to-end. |

#### G4 — collect raced a concurrent re-reference

G4a (chunk-level) mechanisms survive deliberately and stay CHECKED;
G4b (path-level) stays OUTSIDE exactly as pre-registered. The
replacement also *adds* two race surfaces this family did not have —
the mark-snapshot race and the writer-transaction overrun — and both
carry wired falsification pairs: `quint-chunk-collect-no-touch-falsifies-cr1`
/ `quint-chunk-collect-witness-mark-miss-touch-saved` (touch/grace is
load-bearing) and `quint-chunk-collect-writer-overrun-falsifies-cr1` /
`quint-chunk-collect-writer-bounded` (the §4.1 soundness condition),
with `live_cycle_spares_uploading_grace_and_touched` and
`collect_batch_update_rechecks_collect_predicate` as the code-side
pins and the `RioStoreChunkUpgradeTxSlow` alert as the runtime carrier
of the writer bound.

| Corpus row | Replacement verdict | Mechanism / checker |
|---|---|---|
| `aa738a5d7` (M_006: drain deleted without re-checking) | CHECKED | The drain re-check (now `deleted`-only) and the resurrect path survive; re-falsified against the replacement model (`refcountCollectG4aDrainNoRecheck` falsifies CR-1; contend regime HOLDS as baseline); `quint-chunk-collect-witness-drain-resurrect` pins the contended state; `drain_skips_resurrected_chunk` and `live_cycle_resurrected_chunk_survives_drain` pin the code path. The wired `quint-refcount-calib-g4a-drain-recheck` guard remains against the as-built model. |
| `a2d4c6cd8` (drain re-check without FOR UPDATE) | CHECKED | The `FOR UPDATE` re-check survives verbatim in `drain.rs`; at model granularity covered by the same module as above; `drain_for_update_serializes_with_upsert` pins the lock interaction. |
| `a2d4c6cd8` (path_tenants + cycle-reclaim halves) | OUTSIDE (G4b) | Path-level reachability GC, untouched: `store.gc.sweep-path-tenants`, `store.gc.sweep-cycle-reclaim` and the sweep tests. |
| `2b68855c5`, `261e78c9d`, `7d5ff71dc` (mark-vs-PutPath) | OUTSIDE (G4b) | `store.gc.two-phase`, `store.put.placeholder-refs` and the mark/sweep tests; the path mark CTE is untouched by the campaign. |
| `62851c73d`, `132446e7e`, `5ba946682`, `adfd303d7` C1/C3, `bf7e516e4` C5 (sweep resurrection/ordering) | OUTSIDE (G4b) | `store.gc.sweep-recheck+2`, `store.gc.sweep-referrer-order`, `store.gc.sweep-cycle-reclaim`, `store.gc.tenant-retention` and their tests, unchanged. |

#### G5 — the repair loops reaped live uploads

| Corpus row | Replacement verdict | Mechanism / checker |
|---|---|---|
| `a1b49b4a3` (no heartbeat) | CHECKED | The heartbeat survives as the path-row liveness fence; re-falsified against the replacement model (`refcountCollectG5NoHeartbeat` falsifies S5; baseline HOLDS); `s5LiveOwnerNeverReaped` HOLDS in all four regimes and `quint-chunk-collect-threshold-order` pins the ordering. The late-mark dependency this family carries (uploaded_at-as-presence leans on the heartbeat contract) is closed for the data-loss trace by the T-pre.1 guard and its wired pair `quint-chunk-collect-latemark-guarded` / `quint-chunk-collect-latemark-unguarded-falsifies-cr1`, plus `mark_chunks_uploaded_skips_soft_deleted_rows`. The wired `quint-refcount-calib-g5-no-heartbeat` guard remains against the as-built model. |
| `064ceadbd` (wall-clock heartbeat + claim plumbing) | CHECKED | Same coverage as `a1b49b4a3` (the model does not distinguish progress-driven from wall-clock heartbeats); the inline-ingest plumbing half stays outside the chunked-upload scope. |
| `2d7e4f9fd` (I-207: no hot-path reclaim) | CHECKED (latency-only) + CONSTRUCTION (chunk half) | The hot-path reclaim survives as a path-row janitor and its absence is a latency harm, not a safety harm (Stage-C witness-form row); `quint-chunk-collect-witness-hotpath-reclaim` pins that the repair path fires; its former chunk-awareness is deleted (Release B), per the Phase-1 input list's I-207-stays-latency-only item. |
| `da351aaff`, `f6bf0a546` (spawn_monitored moves) | OUTSIDE | Operability; the G7 treatment below. |

#### G6 — lock order

`595b7ed9b`, `d64dbc4b0`, `5ad99b458`, `bf7e516e4` C4: **OUTSIDE**,
unchanged from the pre-registration — PG row-lock acquisition order is
below the model's transaction granularity in both architectures.
`store.chunk.lock-order` survives with its site list narrowed to the
upsert, the collect batch (the candidate scan's ascending order, the
sorted `= ANY` soft-delete, the sorted outbox enqueue — the
`r[impl store.chunk.lock-order+2]` sites in `gc/collect.rs` /
`gc/mod.rs`), and the drain's one-row locks; coverage stays with
`with_sorted_retry`, `upsert_overlapping_no_deadlock`,
`drain_chunk_lock_released_between_rows`, and
`drain_skip_locked_disjoint_batches`.

#### G7 — background-loop operability

`bf7e516e4` C2/C3/C6/C7/C9, `adfd303d7` C4, `660825f19`, `947aaba79`,
`468fd725a`, `a97af109b`: **OUTSIDE**, as pre-registered — and the
obligation transfers whole to the collector, which is now the single
producer of soft-deletes (the design §7 single-point-of-non-collection
risk). The compensating coverage is named: per-batch transaction
isolation (`live_cycle_per_batch_isolation_on_midcycle_failure`),
session-state hygiene after success and mid-cycle failure
(`cycle_leaves_no_session_state_in_pool`,
`failed_cycle_leaves_no_session_state_in_pool`,
`temp_table_does_not_leak_across_cycles`), error-outcome visibility
(`backstop_counts_error_outcome` and the `outcome="error"` counter),
the cap/cursor drain behavior
(`live_cycle_cap_stop_then_cursor_resume_drains_backlog`,
`live_cycle_cap_stop_survives_cursor_loss`,
`live_cycle_multi_batch_below_cap_collects_all`), the fail-closed
abort (`validation_failure_aborts_cycle`,
`live_cycle_parse_failure_collects_nothing`), and the
`RioStoreGcCollectStalled` / `RioStoreGcCollectParseFailure` alerts
with their runbook entries. The poison-row livelock analog (L4) stays
un-modeled, as pre-registered: a poisoned batch fails its own
transaction, prior batches stay committed, and the cycle surfaces as
`outcome="error"` plus the stalled alert rather than silent
non-progress.

#### The M_023 and M_033 lessons, restated against the replacement

| Lesson | Replacement verdict | Mechanism / checker |
|---|---|---|
| M_023 — under-counts are never sanctioned (the CHECK was the only runtime enforcement of the counter's meaning) | CONSTRUCTION | The quantity the CHECK constrained no longer exists; there is no maintained aggregate whose under-count could make a referenced chunk eligible. The equivalent hazard — a referenced chunk missing from the mark set — is what the fail-closed mark forbids: `quint-chunk-collect-parse-skip-falsifies-cr1` shows the skip polarity is the data-loss path, the corrupt-regime CR-1 + `noReferencedChunkSwept` HOLD with fail-closed in place, and `mark_expansion_matches_rust_parser` (the differential pinning test) holds the SQL expansion to the Rust definition of a manifest's chunk set. 071's M_071 commentary records why the CHECK had to go first at deployment time. |
| M_033 — presence is `uploaded_at`, never the liveness signal | CHECKED | `store.chunk.liveness-not-presence` (rule), CR-4 exhaustive in both models, the upsert-RETURNING test set, the i201 probe re-point (Phase 1-pre, consumer-audit row 1), and the i040 selector re-point (Release B, row 2) — no probe or production path infers presence from a liveness signal anywhere in the tree. |

#### Summary

Of the 31 corpus rows above (the 29 Stage-C rows plus the two lesson
rows), counting each row once by its primary verdict: **10 are
CONSTRUCTION** — the counter/token/decrement content of G1 (4 rows),
the forgotten-decrement content of G2 (4 of its 5 rows), the G2×G3
joint revert, and the M_023 lesson — because the state those bugs
lived in (the maintained counter and its write-exactly-once
obligations) no longer exists; **12 are CHECKED** against the
surviving mechanisms, each with a named wired check and test (the
claim/heartbeat path-row janitors of G1/G5, the drain re-check and
resurrect path of G4a, the upsert RETURNING dedup of G3, and the
M_033 lesson); **9 are OUTSIDE** with their compensating coverage
named (G4b's path-level GC, G6 lock order, G7 loop operability, the
two subject-deleted rows, the upsert-batch input-validation row, and
the spawn_monitored operability row). No row is exposed without a
named owner. The
two genuinely new race surfaces the replacement introduces (the
mark-snapshot race and the writer-transaction overrun) are not corpus
rows but carry the §4.6 required-falsification pairs as wired CI
checks, recorded in the chunkCollect section above.


---

## 6. Campaign close-out records

> Origin: refcount-invariant-map.md §§ "The design-§5 exit gates, assessed
> honestly", "Decisions and sign-off items as exercised", "Deferred items,
> owners, and conditions" (with their flip conditions), and "What the
> campaign does NOT claim", verbatim.

### The design-§5 exit gates, assessed honestly

- **Phase 0 / 1a / 1b gates: met**, as recorded in their sections
  (the as-built HOLDS baseline and calibration; the replacement-model
  verdicts, witnesses, falsification pairs, acceptance re-run, gates
  (b)/(c), and the code-review pass; the cutover and deletion
  landings behind green gates with the mixed-fleet construction
  review).
- **Phase 1c "net-negative diff for rio-store/src + schema": NOT
  met**, and recorded as such rather than reinterpreted — see the
  next subsection for the measured numbers and where the growth went.
- **Phase 1c tracey/test hygiene: met.** `tracey query untested` is
  clean for the two collector rules, the two retired rules appear in
  no query output, and every retired or re-pointed test carries its
  P13 justification in the landing commit messages.
- **Phase 2 "Kani proofs in the gate": not met as stated, by
  decision.** The two candidates are reasoned omissions (above) with
  named compensating coverage; nothing is claimed as machine-proved
  for this subsystem.
- **Phase 2 "acceptance table complete": met** (the table above; no
  row without a disposition).
- **Phase 2 "closing bughunter rounds": not executed.** No
  refcount-specific closing bug-sweep rounds were run. The
  adversarial-review evidence for this campaign is the Wave-A1
  collector code review plus the per-landing gates; a closing sweep
  over the landed `rio-store/src/gc` + `metadata/chunked.rs` surface
  remains open work for whoever picks up the deferred Phase-2 items.


### Decisions and sign-off items as exercised

P1–P15 and sign-off items 1–8 shipped as written, with these
exercised outcomes worth restating: item 1 (the parse Kani contract
pulled forward) was attempted, not wired, and is now closed as a
reasoned omission; item 4 (the soak window) was closed by the v5
directive and its suggested window carried into checklist row D2;
item 5 (the mark-scan threshold) was exercised twice — the NO-GO and
the gate-(c) breach — and both re-entries are recorded with their
adjudications; item 7 kept default (B) (no VM-level collector
coverage; the chunkCollect checks, the postgres-backed collect tests,
and the code review carry it); item 8's cap value (500,000) shipped
as derived. P12's retire-at-deletion reading for the G1/G2
calibration guards was taken and recorded in the Release B
disposition.

### Deferred items, owners, and conditions

| Item | Owner | Condition / where recorded |
|---|---|---|
| Migration 072 (`DROP COLUMN chunks.refcount`) — authoring and the seeder/comment sweep | **Landed** (2026-05-27 close-out update) | No longer deferred: the owner clarified (2026-05-27) that there is no staged rollout and no existing cluster or live database — eventual deployments are fresh — so the drop is ordinary development work. The landing carries migration 072 (PINNED), the seeder sweep this row scoped to it (`test_helpers.rs::ChunkSeed`, the admin VerifyChunks test seeds, the bench fixture), and the still-existing-column comment sweep; checklist row D7 reduces to the ordinary "migrations run on deploy" statement. See the migration 072 landing record below. |
| Deployment-time validation checklist D0–D7 | Operator/owner at deployment time | The plan's checklist and its operator copy in `docs/ops/gc-enablement.typ`; D1 (production-class cycle timing, formerly gate (a)), D2 (drift window), D3 (alert quietness), D4 (backlog drain), D5 (integrity spot-checks) precede the Release-B stage; D6/D7 follow it. The Wave-A1 instrumentation is the deliverable that makes these executable. |
| Retiring the as-built `chunkLiveness.qnt` (model-of-record flip) and re-pointing the three surviving `quint-refcount-calib-*` checks at `chunkCollect.qnt` | Whoever picks up the deferred Phase-2 items | Do together, after the deployment-time checklist has validated the live collector (retiring the as-built encoding before then would discard the only model of the still-deployable previous release); the retry campaign's retirement section is the template (preserve non-vacuity anchors when removing checks). |
| MBT-lite trace-derived integration tests (design §5 Phase-2 option) | Same | Optional; revisit only if the collector's PG-side behavior grows beyond what the postgres-backed structural tests pin. |
| Closing bug-sweep rounds over the landed collector surface | Same | The design's Phase-2 closing-discipline item; not executed in this campaign (recorded above). |
| The late-mark heartbeat-contract dependency | Standing assumption, monitored | `uploaded_at`-as-presence still leans on the heartbeat contract under the T-pre.1 guard (Stage-C finding; chunkCollect encoding notes); carried by the S5/threshold-order checks and the latemark pair, not by new work. |
| The upgrade-tx histogram's commit-time blind spot | Operator/owner at deployment time | Recorded at T-1a.4; accept explicitly for the live arm or close with a `pg_stat_activity` long-transaction check / the collector-side snapshot anchor (checklist row D3 is where it surfaces). |
| The sparse full-pass scan term and the 15 M-path mark extrapolation | Operator/owner, monitored | Not cap-bounded; monitored by the cycle-duration histogram and stalled alert; levers are cadence (backstop-only), parallel-query headroom, lowering the cap, and ultimately the junction fallback (design §5b/§7). |
| Impl markers for the three invariant-level rules | Optional follow-up | They are verify-only by design (Stage-A record); if the project later wants `tracey query uncovered` clean of them, the candidate impl sites are the drain re-check (`no-live-collect`), the collect cycle/backstop wiring (`bounded-garbage-retention`), and the upsert RETURNING decision (`liveness-not-presence`). |

### What the campaign does NOT claim

Per the 2026-05-27 directive there was no cluster to deploy to during
the workstream, so nothing in this record is deployment-validated: no
production-scale mark/cycle timing beyond the tmpfs/fsync-off dev-box
bench (whose figures are lower bounds), no observed drift window, no
alert-quietness window, no observed one-time reclamation drain, no
GetPath/VerifyChunks integrity observation, no mixed-fleet rollout
exercised (the §4.5 orderings are reviewed at construction level
only), and no application of migration 072 (landed in-tree at the
close-out, never applied anywhere). Those observations are
exactly rows D0–D7 of the deployment-time validation checklist and
remain open until the completed workstream deploys. The model
verdicts hold at the models' stated bounds (3 hashes, 2 paths, 2
uploaders, scaled clocks), the Kani slot is discharged by reasoned
omission rather than proof, and the bench's EXPLAIN guard pins plan
shape, not production cost.
