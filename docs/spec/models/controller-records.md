# Controller campaign records (closed-campaign archive)

Archived verbatim from `docs/spec/models/controller-invariant-map.md` @
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

Live carriers for this campaign (not this file): the wired
`quint-spawn-coherence-*` / `quint-nodeclaim-*` / `quint-ice-*` /
`quint-wedge-*` checks and their calibration twins (`nix/quint.nix`), the
model headers (`spawnCoherence.qnt`, `nodeclaimLifecycle.qnt`,
`iceEvidenceAck.qnt`, `wedgeCluster.qnt`), and the `ctrl.*` rules in
`docs/spec/components/controller.typ`.

## Standing obligations carried out of the archive

The two Model-J obligations the cross-campaign entries below left open are
carried live as the TODO in the `spawnCoherence.qnt` header, not here: (1) the
pull-only re-encode of the base regimes — partially discharged by the
bughunt-wave C2 axes (`ENABLE_YOUNG_LEADER_REAP` et al.) and by
merged_bug_221's production leader-age law (`ctrl.job.orphan-leader-age`),
with the stream-era executors-present arm still the base-regime default; and
(2) the Stage-C calibration-table re-validation against the pull-only
mechanism set, which stays scheduled (no later entry executed it).

---

## 1. Stage-C corpus pin, verification runs, calibration tables, exit gate

> Origin: controller-invariant-map.md §§ "Stage-C corpus pin: the calibration
> denominator" through "Phase-0 exit-gate verdict" (denominator, per-family
> hash lists, ENC/ENC-A/NOT-ENC tables, run record incl. the `13806e99a`
> busy-guard-probe HOLDS disposition, permanent-witness rationale, exit-gate
> verdict and the MBT "not now" decision), verbatim.

## Stage-C corpus pin: the calibration denominator

Pinned 2026-05-26 at the worktree base `746164c4f` (the formal-sprint
tip), per the design's Stage-C first-deliverable requirement and the
re-pin protocol above.

### Churn re-pin (run immediately before this pin)

`git log 607a93f3f..HEAD` over the three modeled paths returns three
commits: the Stage-A audit's own map commit, `9283bc450` (a one-line
rule-cross-reference comment bump in `pool/jobs.rs`, retry Phase-1b),
and `782b6155b` (the fetcher-budget stream's successor commit, dated
2026-05-24 — it changes the Simulator-shares-accounting chokepoints in
`pod.rs`/`jobs.rs`, which are the G-C accounting family, outside the
modeled protocol state). Both non-audit commits are ancestors of the
Stage-B model commits, so the models were built against exactly this
tree; the pinned rule versions are unchanged
(`placeable-gate+5`, `consolidate-na+6`, `budget.per-class+2`). No
re-validation trigger fires; Stage A and Stage B stand. The fetcher-
budget successor is recorded here as the design's churn check requires
(its accounting content lands in the G-C rows below as additional
NOT-ENCODED members' context, not as new corpus members — it is a
`feat`, not a `fix`).

### The denominator

The corpus is every `fix(` commit on the modeled tick bodies at the pin:
`pool/jobs.rs` (39 of 63 commits), the `pool/job.rs` lineage followed
through its renames (17 fix commits), and `nodeclaim_pool/` (53). The
design's §3.4 estimate (~101) summed the per-file counts; the pinned
DISTINCT-commit corpus is **95**, because 7 of the job.rs fixes are
jobs.rs multi-file commits (the design assumed 8) and 7 further commits
appear in both the jobs.rs and nodeclaim_pool lists (multi-file commits
the per-file sum double-counts: `f97644a53`, `3f416e02e`, `bcfdc2262`,
`9fd4b6e59`, `b570cdd8d`, `039861b56`, `d5602b3aa`). Each such commit
gets exactly one row, in the family its repair belongs to. The three
incidental cross-crate commits the design named (`3c3062760`,
`c8ca42a91`, `dbc7f7cb2`) are binned in the remainder family.

Excluded by the corpus definition (recorded so the boundary is checked,
not assumed): `pool/pod.rs` commits (39/21 — k8s object construction
outside the modeled tick bodies; G-D-disposition coverage),
`node_informer.rs`-only commits (the M10–M13 / λ-accounting families —
the design lists the M12/M13 pair `ff7f99ab8`/`b80d6f135` for
checker-honesty in the calibration table but outside the denominator),
and ComponentScaler / GC-cron / disruption commits (out-of-scope loops).

### Per-family hash lists (the 95)

| Family | n | Commits |
|---|---|---|
| G-A spawn↔reap↔queued coherence | 10 | `7f04c9d88`, `6a9ba0ef0`, `fb0953870`, `fba9086dc`, `6c4f4983d`, `9123e72d4`, `fd5d7c988`, `5e01a9ff1`, `8b0128f5a`, `004956eeb` |
| G-B ack/ICE protocol | 7 | `cdc78f839`, `5815a7544`, `485e736a2`, `af1383c0e`, `e8bd76451`, `d6bc376d3`, `408a48bcb` |
| G-C resource-accounting parity | 8 | `a415a9a8b`, `286566a57`, `d5602b3aa`, `073170dfb`, `5250a4b9a`, `b25836ef1`, `5c2a83761`, `bcfdc2262` |
| G-D placement derivation | 8 | `80cfcd65c`, `039861b56`, `3f416e02e`, `2f9a3769c`, `9fd4b6e59`, `b570cdd8d`, `015667efa`, `f97644a53` |
| G-E deadline coupling | 2 | `172776b1b`, `f73b98b1f` |
| G-F identity/security plumbing | 3 | `a6697c6b0`, `ea10e1d74`, `acf6d476b` |
| G-G reap delete-propagation & report-path mechanics (job.rs lineage) | 6 | `1779975f6`, `2f04e5432`, `8cbf6d7b3`, `12b86c285`, `2acd1b327`, `6d678ac87` |
| M1 prev_idle / idle model | 7 | `34f37d7e9`, `79f86b888`, `13806e99a`, `a19394346`, `7f91f1892`, `cc2e99887`, `a12c6f9f9` |
| M2 inflight_created / ICE detection | 5 | `0507f9874`, `08d49c52c`, `5935d9122`, `4ece337a4`, `92c2a89f2` |
| M3/M4 sketches lifecycle (lease/PG/seed) | 10 | `92a3dc47d`, `703cbf42a`, `2d62e0b49`, `bd8e57de5`, `6052f84df`, `95fc40fb6`, `9c9bfb7c8`, `b92981881`, `df077d82b`, `3c9aa3919` |
| M5/M6 gauge staleness | 4 | `cab0d2d46`, `d4184cf2b`, `d0c858955`, `e0d504321` |
| FFD/cover ⇄ scheduler-config parity | 16 | `9ff9387f0`, `811489319`, `bd781b004`, `5f754baeb`, `787243ef3`, `45f83cdcd`, `f333ebed5`, `58cd38885`, `c5320b40e`, `e013b2044`, `6c8f13710`, `0fa79fcdf`, `79aa88da2`, `d674f0983`, `4fdf3337b`, `979608619` |
| Remainder (docs/test/alert/infra sweeps + incidental cross-crate) | 9 | `2ad753db9`, `416895e3e`, `3c3062760`, `c8ca42a91`, `dbc7f7cb2`, `a49f78722`, `99a17cd2f`, `f1caa0b60`, `ff5f4e95e` |

File-boundary resolutions the design left to this pin: the FFD/cover
row absorbs the three nodeclaim_pool-touching commits the inventory had
not grouped (`d674f0983` NodeClaim construction, `4fdf3337b` ffd.rs
arch-matching, `979608619` cover.rs ceilings chokepoint); `3c9aa3919`
(sketch persistence serialization) joins M3/M4; `99a17cd2f` (the
scheduler-side authoritative-binding fix whose controller-side touch is
the dead-node reap path — consumed input, out of model) and
`ff5f4e95e`/`f1caa0b60`/`a49f78722` (config/alert plumbing) sit in the
remainder. The inventory's FFD/cover "~13" members that live only in
`node_informer.rs` are outside the corpus by the definition above; all
13 listed there touch `nodeclaim_pool/` and are in.

### Per-family encodability plan (pre-registered → pinned)

The design's §3.4 pre-registration carried over per family, with the
per-commit corrections the pin surfaced (each correction is argued in
the calibration table's rows): G-A encodable via representatives
`fba9086dc`, `6c4f4983d`, `8b0128f5a` (predictions: reapSafety ×2,
gateFailClosed); G-B encodable via `cdc78f839`, `5815a7544`
(ackSoundness, ackCoversPending), the proto/back-compat and
scheduler-side members NOT-ENCODED; G-C / G-D / G-E / G-F NOT-ENCODED
as pre-registered; G-G NOT-ENCODED except the `1779975f6`
census-predicate half (prediction: ackSoundness on today's re-ack
chain); M1 encodable via `79f86b888` (idleReapSafety) with the
`13806e99a` busy-guard half downgraded to a redundancy probe (the
within-tick observe-before-reap ordering covers it at model
resolution); M2 encodable via the two halves of `08d49c52c`
(iceMarkSoundness; the module-local inflight-conservation invariant),
`5935d9122` re-dispositioned NOT-ENCODED (LIST-vs-delete race below
tick atomicity), `0507f9874` treated as the mechanism's introduction;
M3/M4 split exactly as pre-registered — `703cbf42a`
(reloadLatchRespected) and `92a3dc47d`'s recency half
(noMassClearAfterFailover) encodable, content/cell-key members
NOT-ENCODED; M5/M6 NOT-ENCODED (the model carries no cleanup-set state
— a deviation from the pre-registration, recorded); FFD/cover: the
per-class clamp is encoded as a family-level reconstruction
(provisioningBudget), `5f754baeb` itself re-dispositioned to its
sizing content (NOT-ENCODED), `4ece337a4` re-dispositioned NOT-ENCODED
(within-tick per-create granularity below the tick-global create-fault
bit); remainder N/A.

## Stage-C verification runs (serial) and dispositions

Protocol: every override ran serially with the same TLC invocation the
CI checks use (`quint verify --backend=tlc --main=<module>
--step=calibStep --invariant=<predicted>`); violation runs stop at the
first counterexample, baselines and the probe run to exhaustion. The
per-run depths and state counts are recorded in the calibration table
below; wall-clocks live in the introducing commit message and the
transcripts.

Outcome summary:

- **Twelve of twelve pre-fix overrides falsify exactly the invariant
  their module header predicts**, on the first run, with no module
  corrections needed after the corpus-pin commit.
- **The one predicted-HOLDS probe holds**: `m1CalibReapBusyGuardProbe`
  (the `13806e99a` reap_idle busy-guard half) explores the same
  reachable state count as the as-built base regime and finds no
  idle-reap violation. Three-way disposition: this is not a missing
  model dimension to fix and not an incomplete invariant list — the
  busy-guard's trigger state (a live prev_idle entry on a currently
  busy claim at reap time) is unreachable at the model's tick-internal
  ordering resolution because the same tick's observation prunes the
  entry first. It is recorded as defense-in-depth below the per-read
  snapshot abstraction, NOT as a §4(b) redundancy candidate: the real
  loop is not atomic between the observation and the reap, which is
  exactly the window the guard defends. Coverage: the consolidate.rs
  reap_idle unit tests; the windowed-lambda half of the same commit is
  NOT-ENCODED (threshold arithmetic).
- **Both distinguishing baselines hold**: the as-built step at
  CEILING=2 holds `ackCoversPending` (so the `5815a7544` falsification
  is attributable to the missing re-ack, not to the widened ceiling),
  and the as-built step at base constants holds the module-local
  `inflightKeptWhileInFlight` (so the `08d49c52c` KEEP-arm
  falsification is attributable to the drop-on-first-sight prune).
  Overrides at standard regime constants use the wired Stage-B regime
  checks as their baseline (each predicted invariant HOLDS there).
- **The two deterministic reproducer runs pass** (`quint test`:
  `m1AcquireClearOkOnlyRun`, `m2NoConsolidatePruneRun`), pinning the
  documented incident shapes (the failed-reload over-reap and the
  consolidate-only spurious-ICE chain).
- **No stop-and-report event**: no invariant — existing or added —
  falsified on the unmodified as-built models; the calibration added
  no invariant to the main models at all (the one new property,
  `inflightKeptWhileInFlight`, is module-local to the calibration and
  holds on the as-built baseline). The main models and their wired
  Stage-B regime checks are untouched by Stage C, so the Stage-B
  verdict table and state counts above stand as recorded.

## Stage-C calibration: the historical-fix corpus replayed against the models

The 95-commit corpus pinned above, replayed against `spawnCoherence.qnt`
(Model J) and `nodeclaimLifecycle.qnt` (Model N): for each commit the
pre-fix behavior is either expressed as an override of the as-built
model and shown to falsify an invariant (the model would re-find that
bug), or its non-encodability is dispositioned with the missing
dimension and the covering vehicle named. Method per the prior
campaigns: each override is a module in
`docs/spec/models/calibration/controller-<family>.qnt` that instantiates
the as-built model, replaces ONE tick action with a local PRE-FIX
variant, and exposes it as a `calibStep`; the violation latches keep the
as-built oracle. Verdicts are TLC results (violation runs stop at the
first counterexample); verdict format: invariant @ step (depth, states
generated / distinct). Wall-clocks live in the introducing commits'
messages and the run-record section above.

Classification legend: **ENC** — encodable, override written and run.
**ENC-A** — encodable, dispositioned by analogy: the mechanism is
encoded in the as-built model and the named sibling override (or wired
check) exercises the same tick-arm machinery; not separately run.
**NOT-ENC** — the model abstracts the mechanism away (missing dimension
named). **ORIGIN** — the commit introduced the mechanism the model now
encodes; no pre-fix defect of the modeled protocol to revert. **N/A** —
remainder (docs/test/alert/infra sweeps, incidental cross-crate).

### G-A — spawn↔reap↔queued coherence (10)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `fba9086dc` | excess-pending DELETE on the informer census alone (no live pod-phase re-check) | ENC | `gaCalibNoLiveRecheck` | **FALSIFIES** reapSafety @ calibStep (depth 10, 4,799/1,000) |
| `6c4f4983d` | orphan_reap_gate treated Ok([]) as authoritative (C3); the C2 orphan-pending arm and C6 admin_call chokepoint are as-built mechanisms | ENC | `gaCalibOrphanGateEmptyOk` | **FALSIFIES** reapSafety @ calibStep (depth 12, 19,785/3,399) |
| `8b0128f5a` | CRD-absent arm armed the gate, so reap_excess_pending ran against an ungated queue | ENC | `gaCalibCrdAbsentArmed` (crd-absent cfg) | **FALSIFIES** gateFailClosed @ calibStep (depth 6, 242/132) |
| `7f04c9d88` | want.is_empty() early-return skipped reap_stale entirely at ceiling-saturation; spawn loop 409-churned | ENC-A | the wantEmpty guard, skip-set and 409-dedupe are the encoded mechanisms; a due-but-skipped stale reap is exactly the orphanRemoved (I2 safety form) latch; same reap-arm machinery as the two G-A overrides above | by analogy |
| `6a9ba0ef0` | spawn stopped at queued−active; selector-drift Pending never reaped | ENC-A | the drift-reap arm and the all-intents spawn iteration are encoded; kept reachable by canReachDriftReap / canReach409Dedupe; the starvation half is liveness-shaped | by analogy |
| `fb0953870` | terminal Jobs name-colliding with a re-queued intent never reaped → respawn blocked | ENC-A | the terminal-collision arm is the encoded unblock mechanism (ETerminalReap path); its loss is a liveness regression below the safety set | by analogy (mechanism encoded; no safety latch) |
| `9123e72d4` | orphan gate without the leader-age arm (plus reap/spawn coherence wiring) | ENC-A | dropping any single 3-arm conjunct reaps under a gate the as-built oracle rejects — same shape and same latch as `gaCalibOrphanGateEmptyOk` | by analogy (sibling falsified reapSafety) |
| `fd5d7c988` | freed slots not credited to headroom the same tick | ENC-A | the freedSlotsSpendable invariant is this clause verbatim; the freed-slot credit is in the encoded headroom arithmetic | by analogy (mechanism encoded; latch exists) |
| `5e01a9ff1` | orphan-reap re-deleted Terminating Jobs every tick | NOT-ENC | the JobPhase partition makes the re-delete a no-op at model resolution (API churn); coverage: is_running_job unit tests, the foreground-delete discipline | n/a |
| `004956eeb` | unpinned selector (reaper thrash on softmax re-roll), HashMap-order truncation, ack arming for headroom-gated intents | ENC-A | the fingerprint pin is the encoded drift mechanism; the deterministic idRank order is by construction; the false-arm half is the same ackSoundness shape `gbCalibAckAttempted` falsifies | by analogy (sibling falsified ackSoundness) |

### G-B — ack/ICE protocol (7)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `cdc78f839` | the ATTEMPTED spawn slice was acked (failed creates and 409s included) | ENC | `gbCalibAckAttempted` | **FALSIFIES** ackSoundness @ calibStep (depth 10, 11,881/705) |
| `5815a7544` | only spawned-this-tick intents acked; already-Pending never re-acked after a scheduler restart | ENC | `gbCalibAckOnlyNew` (CEILING=2) | **FALSIFIES** ackCoversPending @ calibStep (depth 11, 9,252/1,776); baseline as-built step HOLDS (depth 32, 81,467,617/4,414,304) |
| `485e736a2` | heartbeat ICE-clear regardless of admissible-cell count | NOT-ENC | scheduler-side clear path (out of J/N per the design); `sched.sla.hw-class.ice-mask` + scheduler tests | n/a |
| `af1383c0e` | intent.ready unwrap back-compat | NOT-ENC | proto-compat surface; unit tests | n/a |
| `e8bd76451` | ready filter + hw_class_names label reconstruction | NOT-ENC | proto-compat surface; unit tests | n/a |
| `d6bc376d3` | trust_threshold single-source | NOT-ENC | §13a bench-gate config plumbing (out of model); hw-bench unit tests + `ctrl.pool.hw-bench-needed+2` | n/a |
| `408a48bcb` | per-dim min_tenants gate | NOT-ENC | same as above | n/a |

### G-C — resource-accounting parity (8)

`a415a9a8b`, `286566a57`, `d5602b3aa`, `073170dfb`, `5250a4b9a`,
`b25836ef1`, `5c2a83761`, `bcfdc2262`: **NOT-ENC**, exactly as
pre-registered — quantity arithmetic across pod spec / FFD / eviction
classification, not protocol state. Coverage: the
Simulator-shares-accounting chokepoint tests in `pool/jobs.rs` /
`pod.rs`, the eviction-classification tests, and the design §3.6 Kani
candidate. The fetcher-budget stream (`2fff4e938`, `782b6155b`) extends
this surface and inherits the same disposition.

### G-D — placement derivation (8)

`80cfcd65c`, `039861b56`, `3f416e02e`, `2f9a3769c`, `9fd4b6e59`,
`b570cdd8d`, `015667efa`, `f97644a53`: **NOT-ENC** (k8s object
construction parity — affinity/toleration/selector/schedulerName).
Coverage: pool construction unit tests, `vm-protocol-*` /
`vm-forecast-provisioning`, helm/CRD drift checks.

### G-E — deadline coupling (2)

| Commit | Class | Coverage |
|---|---|---|
| `172776b1b` | NOT-ENC | the controller half is plumbing (daemon timeout from intent deadline); the at-cap behavior of the report it feeds is the retry campaign's E7 surface (its calibration row `retryCalibG1DeadlineUncapped` already pins the no-cap world) |
| `f73b98b1f` | NOT-ENC | deadline floor constant; unit tests |

### G-F — identity/security plumbing (3)

`a6697c6b0`, `ea10e1d74`, `acf6d476b`: **NOT-ENC** (token/claims
plumbing). Coverage: token-mode unit/VM tests (`vm-token-mode`), auth
tests.

### G-G — reap delete-propagation & report-path mechanics (6)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `1779975f6` | is_pending_job counted Terminating Jobs as Pending (census, excess set, ack set); background delete propagation | ENC (census half) / NOT-ENC (propagation half) | `ggCalibTerminatingPending`; the propagation half sits below the model's deletion atomicity — ci-failure-patterns' job-tracking-finalizer entry, `vm-lifecycle-*` | **FALSIFIES** ackSoundness @ calibStep (depth 12, 30,838/4,920) — on today's protocol the lost filter resurfaces as a false re-ack |
| `2f04e5432` | orphan-running reap used background delete (second finalizer-orphan callsite) | NOT-ENC | below deletion atomicity; same coverage as above; the foreground-delete discipline is an explicit §4 non-candidate | n/a |
| `8cbf6d7b3` | kubelet eviction-message match | NOT-ENC | report-classification string matching; unit tests | n/a |
| `12b86c285` | daemon_timeout/deadline alignment + DeadlineExceeded report | NOT-ENC | G-E coupling; the scheduler half is the retry campaign's surface | n/a |
| `2acd1b327` | floor promotion gated on OOMKilled/DiskPressure | NOT-ENC | scheduler-side promotion gate (retry G6 family) | n/a |
| `6d678ac87` | consolidated Job constructor; manifest-path protections | NOT-ENC | G-D construction parity; unit + VM tests | n/a |

### M1 — prev_idle / idle model (7)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `79f86b888` | prev_idle cleared only on the reload Ok arm — a failed reload kept the previous tenure's entries (amplify polarity) | ENC | `m1CalibAcquireClearOkOnly` + named run `m1AcquireClearOkOnlyRun` | **FALSIFIES** idleReapSafety @ calibStep (depth 20, 2,355,905/26,339) |
| `13806e99a` (busy-guard half) | reap_idle did not re-check requested>0 at reap time | ENC-probe | `m1CalibReapBusyGuardProbe` — exhaustive | **HOLDS** idleReapSafety @ calibStep (depth 20, 242,933/1,660 — the as-built base state space; the guard never binds at model resolution). Disposition in the run-record section: defense-in-depth below the tick-internal observe-before-reap ordering; not a deletion candidate |
| `13806e99a` (windowed-lambda half) | consolidate_after finite-difference hazard | NOT-ENC | threshold arithmetic inside the abstracted NA model; consolidate.rs unit tests + `ctrl.nodeclaim.consolidate-na+6` | n/a |
| `34f37d7e9` | idle tracking read the dead Karpenter Empty condition | ORIGIN | introduced the controller-side prev_idle mechanism the model encodes as pIdle; the pre-fix world has no idle reaps at all (cost/liveness, no safety latch) | n/a |
| `a19394346` | FFD placed onto terminating NodeClaims | NOT-ENC | the placement-quality consequence is below the safety set; the budget half (terminating still billed) is encoded and checked by provisioningBudget; ffd.rs unit tests + `ctrl.nodeclaim.ffd-exclude-terminating` | n/a |
| `7f91f1892` | builder consolidation floor 60s | NOT-ENC | threshold constant; consolidate.rs tests | n/a |
| `cc2e99887`, `a12c6f9f9` | per-cell context refactor; hold-open clamp | NOT-ENC | refactor / threshold structure inside the abstracted NA model; consolidate.rs tests | n/a |

### M2 — inflight_created / ICE detection (5)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `08d49c52c` (bug_012 half) | consolidate_only never pruned inflight_created — the controller's own reaps read as Karpenter GC on the next full tick | ENC | `m2CalibNoConsolidatePrune` + named run `m2NoConsolidatePruneRun` | **FALSIFIES** iceMarkSoundness @ calibStep (depth 16, 1,085,474/4,039) |
| `08d49c52c` (bug_020 half) | detect_vanished pruned entries on first sight in live — fast-GC'd claims escaped detection | ENC | `m2CalibInflightDropOnSight` (module-local invariant `inflightKeptWhileInFlight` — the harm is a missed mark, completeness-shaped, so the calibration pins the structural conservation half) | **FALSIFIES** inflightKeptWhileInFlight @ calibStep (depth 3, 290/6); baseline as-built step HOLDS (depth 18, 242,933/1,660) |
| `0507f9874` | (pre-fix world: no vanish/LaunchFailed ICE detection at all; cover from rio.build/* labels) | ORIGIN (detection half) / NOT-ENC (requirements half) | the detection mechanism is what the model encodes (kept reachable by canReachVanishMark); the requirements half is construction parity | n/a |
| `5935d9122` | reap_unhealthy 404 arm did not mask; dead-cap counted in-flight | NOT-ENC | the LIST-vs-delete 404 race sits below the tick's atomicity; dead_nodes is consumed input (out of model). Coverage: health.rs unit tests. Re-dispositioned from the design's family-level "encodable" pre-registration | n/a |
| `4ece337a4` | failed creates consumed per-tick budget (under-cover within the round-robin) | NOT-ENC | within-tick per-create granularity is below the tick-global create-fault bit, and the budget is recomputed per tick (no cross-tick consequence); coverage: cover.rs accounting tests, `ctrl.nodeclaim.budget.per-class+2`'s failed-creates clause, canReachCreateFailure keeps the path reachable. Re-dispositioned from the design's pre-registration | n/a |
| `92c2a89f2` | drop-reason metrics split | NOT-ENC | observability only | n/a |

### M3/M4 — sketches lifecycle (10)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `703cbf42a` | reload latch cleared on the load attempt; persist ungated | ENC | `m34CalibLatchClearOnAttempt` | **FALSIFIES** reloadLatchRespected @ calibStep (depth 14, 40,457/815) |
| `92a3dc47d` (recency-gate half) | observe_registered emitted clears (and samples) for stale registrations after an acquire | ENC | `m34CalibNoRecencyGate` | **FALSIFIES** noMassClearAfterFailover @ calibStep (depth 14, 34,086/767) |
| `92a3dc47d` (other halves) | lease hooks doing the unarm/reload work; ack dedup; non-blocking connect_pg | ENC-A / by construction | the hook machinery is the as-built E-acq/E-loss encoding exercised by the fault-lease regime; the per-tick mark dedup is set-valued by construction; connect_pg is plumbing | n/a |
| `2d62e0b49`, `bd8e57de5`, `6052f84df`, `95fc40fb6`, `9c9bfb7c8`, `b92981881`, `df077d82b` | seed/rotate ordering, shadow gates, quantile fallbacks, cell-key aliases | NOT-ENC | sketch contents and key forms are abstracted (the design's pre-registered split); sketch.rs unit tests | n/a |
| `3c9aa3919` | sketch persistence serialization (bincode→postcard) | NOT-ENC | serialization format below the abstract PG cell; sketch.rs round-trip tests | n/a |

### M5/M6 — gauge staleness (4)

| Commit | Class | Coverage |
|---|---|---|
| `d0c858955` | ENC-A (the kube-only-observations sharing half) / NOT-ENC (the trailing-zero gauge half) | the shared observation block is the as-built consolidate-only encoding; the residual unobserved window (the ⊥ early-return) was closed 2026-06-02 by the ⊥-arm fix and its falsification checks flipped per protocol (this commit); gauges are observability |
| `cab0d2d46`, `d4184cf2b`, `e0d504321` | NOT-ENC | observability only (the model carries no cleanup-set / gauge state — recorded as a deviation from the design's "polarity classification encodable" pre-registration); gauge_universe / emit_live_gauges unit tests; since 2026-06-02 the cleanup-set polarity rows (M5/M6) carry their only automated end-to-end coverage in the lifecycle-invariants suite (`lifecycle_tests::acquire_keeps_cleanup_sets_one_trailing_write_then_drop` — survives-acquire + consumed-exactly-once via a local DebuggingRecorder through the real tick) |

### FFD/cover ⇄ scheduler-config parity (16)

| Commit / row | Pre-fix behavior | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| family-level reconstruction (anchors: the cover.rs class_budget mechanism, `ctrl.nodeclaim.budget.per-class+2`; the design's named representative `5f754baeb` turned out to be the per-claim sizing half) | cover sized against the global fleet budget only — no per-class clamp | ENC | `ffdCalibNoClassClamp` | **FALSIFIES** provisioningBudget @ calibStep (depth 8, 2,368/42) |
| `5f754baeb` | per-claim SizingCfg from global max_cores/mem (oversized NodeClaims → InsufficientCapacity → ICE loop) | NOT-ENC | per-claim sizing arithmetic (the design's pre-registered NOT-ENCODED half); cover.rs sizing unit tests; §3.6 Kani candidate | n/a |
| `79aa88da2` | no periodic HwClassConfig refresh (unbounded skew); node-role label | NOT-ENC | the refresh cadence is below time resolution — the model's configuredCells / ceilingsKnown environment actions abstract it; the fail-closed gate it feeds IS encoded (degradedCoverPolarity + canReachCeilingsFailClosed) | n/a |
| `9ff9387f0`, `811489319`, `bd781b004`, `787243ef3`, `45f83cdcd` | sizing predicate / chunking / per-cell cap filtering / anchor sizing / reference-cell assignment | NOT-ENC | FFD/cover sizing arithmetic (pre-registered); cover.rs + ffd.rs unit tests, Kani candidate | n/a |
| `f333ebed5`, `58cd38885`, `c5320b40e`, `e013b2044`, `6c8f13710`, `0fa79fcdf` | feature/arch-axis parity chokepoints shared with the scheduler | NOT-ENC | axis-derivation parity (G-D-shaped); chokepoint unit tests on both sides | n/a |
| `d674f0983`, `4fdf3337b`, `979608619` | NodeClaim spec construction (expireAfter), ffd arch-matching, cover ceilings chokepoint | NOT-ENC | construction/axis parity; unit tests + `vm-forecast-provisioning` | n/a |

### Remainder (9)

`2ad753db9`, `416895e3e` (test/doc sweeps), `3c3062760`, `c8ca42a91`,
`dbc7f7cb2` (incidental cross-crate, named by the design), `a49f78722`,
`f1caa0b60` (helm/alert plumbing), `ff5f4e95e` (configmap/RBAC infra),
`99a17cd2f` (scheduler-side authoritative-binding fix; its
controller-side touch is the dead-node reap path — consumed input, out
of model per the Stage-A out-of-model list): **N/A** — no modeled
protocol content.

### Outside the corpus, listed for checker honesty

`ff7f99ab8`, `b80d6f135` (M12/M13 bound-intent disambiguation,
node_informer family): **NOT-ENC** — Models J/N carry no Pod objects
and `dead_nodes` is consumed input; their protection is the design
§4(a)1 gate tests (the ported two-pods-one-intent derivation tests and
the consequence-chain coverage), not an F1–F5 invariant. Recorded here
so the boundary stays explicit.

### Permanent expect-violation witnesses (wired into nix/quint.nix)

Six of the thirteen modules are wired as `quint-ctrl-calib-*` checks —
one per falsifying family with a plausible regression path in the
as-built code and a cheap state space (the same proportion as the prior
campaigns); the rest are evidence modules, re-runnable with the README
recipe.

| Check | Module | Violated invariant | Guards against |
|---|---|---|---|
| `quint-ctrl-calib-ga-live-recheck` | `gaCalibNoLiveRecheck` | `reapSafety` | losing the live pod-phase re-check before the excess DELETE (fba9086dc) |
| `quint-ctrl-calib-gb-ack-spawned-only` | `gbCalibAckAttempted` | `ackSoundness` | acking attempted instead of created spawns (cdc78f839) |
| `quint-ctrl-calib-m1-acquire-clear` | `m1CalibAcquireClearOkOnly` | `idleReapSafety` | regressing the unconditional prev_idle clear to the Ok arm (79f86b888) |
| `quint-ctrl-calib-m2-consolidate-prune` | `m2CalibNoConsolidatePrune` | `iceMarkSoundness` | losing the consolidate-only inflight prune (08d49c52c) |
| `quint-ctrl-calib-m34-reload-latch` | `m34CalibLatchClearOnAttempt` | `reloadLatchRespected` | clearing the reload latch on attempt instead of on Ok (703cbf42a) |
| `quint-ctrl-calib-ffd-class-clamp` | `ffdCalibNoClassClamp` | `provisioningBudget` | dropping the per-class fleet clamp from cover |

The G-G and the second M1/M2/M3-M4 overrides, the crd-absent and
orphan-gate G-A overrides, the CEILING=2 G-B override and the
busy-guard probe stay evidence modules: their regression paths are
either already guarded by a wired sibling on the same tick arm, pinned
by a deterministic named run, or (the probe) are HOLDS evidence rather
than a regression guard.

### Phase-0 exit-gate verdict

**Met.** Per the design §5 Stage-C gate:

- **Every family has a falsifying representative or an explicit
  NOT-ENCODED disposition naming its coverage.** Falsifying
  representatives exist for G-A (3), G-B (2), G-G (1), M1 (1), M2 (2),
  M3/M4 (2) and FFD/cover (1, family-level); G-C, G-D, G-E, G-F, M5/M6
  and the remainder carry per-commit NOT-ENCODED / N-A rows naming the
  covering vehicle, exactly as the design pre-registered for them.
- **Every falsification trace was walked against the code path** during
  module construction (each override's header cites the decision sites
  and the incident the historical fix recorded; the two deepest traces
  are additionally pinned as deterministic named runs).
- **Permanent `quint-ctrl-calib-*` checks are wired** for the
  regression-worthy subset (six, one per falsifying family).
- **The calibration table is committed next to the invariant map** (this
  section), and the corpus denominator was pinned before any override
  ran.
- **No stop-and-report event occurred**; the main models are untouched,
  so the Stage-B verdicts stand without re-runs.

Honest deltas from the design's pre-registration, all argued in the
rows above: the verdict shape is leaner than the predicted "45–50 of
~101 encodable through a representative" — about 20 of the 95 pinned
commits are ENC/ENC-A, because the per-commit pin shows the family
tails are dominated by sizing arithmetic, content/serialization,
axis-parity and observability commits that the design itself
pre-registered as NOT-ENCODED at the family level; the per-commit
re-dispositions (`5935d9122`, `4ece337a4`, `5f754baeb`'s sizing
content, M5/M6's missing cleanup-set state, the `13806e99a` busy-guard
probe) are recorded as checked-prediction corrections rather than
silent shrinks. The MBT decision the design deferred to this gate:
calibration did NOT keep tripping on model/code mismatch (every
override falsified as predicted on the first run), so the §3.6
recommendation stands — no MBT/trait-extraction prerequisite is imposed
on Phase 1; the decision is recorded as "not now", revisitable if
Phase-1 work surfaces conformance doubts.

---

## 2. Cross-campaign counter-signed entries (1b, 1c OA2, 1c', 1d)

> Origin: controller-invariant-map.md §§ "Executor-campaign 1b re-audit"
> through "Executor-campaign 1d entry", signatures verbatim.

## Executor-campaign 1b re-audit (the contained delta pass this map's cross-campaign entry requires)

Recorded by the executor-lifecycle campaign's 1b verification batch
(Phase-1 plan T-1b.10), against the formal-sprint tree carrying the 1a
additive slice and the 1b code batch (AD2 both halves, the C4/C5
re-point, the AD5 successors, the ICE re-trigger, the Pool
`dispatchMode` field). Scope: exactly the sections the cross-campaign
entry names — J11, the orphan-reap rows, F1/F3 — plus the model
re-runs that license "no transition change needed".

- **J11 (termination reports).** `report_terminated_pods` /
  `report_deadline_exceeded_jobs` keep their selection and
  once-per-terminal-object sampling behavior; what changed is the RPC
  they speak (`ReportAttemptOutcome`, the C4/C5 unification — T-1b.3).
  For stream-mode identities the scheduler routes the unified report
  through the same internal path `ReportExecutorTermination` served,
  so the J11 row's behavioral description stands unchanged; for
  pull-mode attempts the report is the idempotent second-installment
  fill (no new row, no reclassification). No Model J encoding change:
  the model's termination-report environment action abstracts the
  channel, not the RPC name.
- **Orphan-reap rows (J10 / `ctrl.ephemeral.reap-orphan-running+3`).**
  The busy view is now the OR of `ListExecutors.busy` and the durable
  open pull-mode attempt view (`ListOpenAttempts`), fail-closed on
  either read (landed at 1a, T-1a.8; the spawnCoherence.qnt busy-view
  header note from the 1a landing already records the abstraction).
  The "busy but never registered" documented residual is closed in the
  code for pull-mode pods (their busyness is ledger-backed); it
  remains the documented bound for stream pods until 1c'. The 3-arm
  fail-closed gate is unchanged. New at 1b and adjacent to (not part
  of) the reap rows: the AD5 cancel arm
  (`cancel_closed_attempt_jobs`, pull-mode pools only, closed-edge
  evidence required, fail-closed on the view read) and the pull-mode
  preemption branch of the DisruptionTarget watcher
  (synthesize-preempted + foreground Job delete, no `DrainExecutor`).
  Both are pull-pool-scoped, evidence-gated, and covered by their
  red-first unit batteries; neither is encoded in Model J at this
  slice — their model pricing rides the 1c'/1d Model J/N checklist
  re-derivation this map already carries as an obligation.
- **F1/F3 rows.** No change to the same-tick coherence machinery or to
  the ack/ICE protocol as modeled. The ICE-clear re-trigger (T-1b.5)
  moves the *scheduler-side* clear for pull-mode intents from the
  registration edge to the first successful pull; the controller-side
  arming (`AckSpawnedIntents`) and the modeled tick ordering are
  untouched, so the F3 rows' statements stand. The F1/F3 rows remain a
  prerequisite review input to that campaign's deletion slices exactly
  as the re-pin protocol states.
- **Model re-runs (no transition change).** Models J and N are
  byte-unchanged at this slice; every wired exhaustive check was
  re-confirmed green at this tree with state counts bit-identical to
  the recorded baselines (spawn-coherence base/fault-rpc/fault-lease/
  fault-stale/crd-absent/fetcher and nodeclaim base/fault-rpc/
  fault-lease/fault-karpenter; figures in the recording commit's
  message and the checks' transcripts).
- **Stage-C calibration table delta pass:** not triggered by 1b — no
  modeled mechanism's behavior changed; the full delta pass remains
  scheduled with the 1c'/1d re-derivation as already recorded above.

Controller-campaign owner counter-signature for this re-audit entry:
SIGNED 2026-05-28 (collected at the executor campaign's close-out, as
the Phase-1b record's reference to this entry anticipated). Checked at
signing: J11's selection and once-per-terminal-object sampling are
intact in `pool/job.rs::report_terminated_pods` /
`report_deadline_exceeded_jobs` (the seen-map sample gate), now
speaking `ReportAttemptOutcome`, and the pull-mode second-installment /
no-attempt semantics the entry relies on are pinned by the scheduler's
report-idempotency and no-attempt batteries; the AD5 cancel arm
(`cancel_closed_attempt_jobs`, gated on the Pool CR's pull dispatch
mode, closed-edge evidence, fail-closed view read) and the
DisruptionTarget preemption branch are as recorded and stay out of
Model J, their pricing carried by the 1c'/1d re-derivation exactly as
this entry promised; `AckSpawnedIntents` arming and the modeled tick
order are untouched (the ICE clear edge moved scheduler-side only, to
the fenced pull mint's single-cell clear). Models J and N were
byte-unchanged at the 1b slice; the re-run record is commit
`402a459562` (figures in that commit message and the checks'
transcripts), and the same wired exhaustive checks were re-confirmed
green against the unchanged model text at this counter-signature with
distinct-state counts bit-identical to that baseline (figures in the
counter-signature commit message). The busy-view OR-bridge this entry
describes was later narrowed to the durable open-attempt view alone —
that is the 1d entry's signed content below, not a correction to this
one. No Model J/N assumption is invalidated.

## Executor-campaign 1c entry — the OA2 controller-side node-wedge aggregation (Model N input change)

Recorded by the executor-lifecycle campaign's slice 1c (Phase-1 plan
T-1c.1), inside this campaign's Model N scope per the OA2 decision
(executor map, "OA2 — hung-node signal owner and shape: DECIDED
(2026-05-27)", option A with option C as the canary-window interim).

- **What landed.** `reconcilers/nodeclaim_pool/wedge.rs`: per-node
  clustering of pull-mode attempt-deadline expiries over the
  open-attempt ledger view (`AdminService.ListOpenAttempts`), keyed on
  the ledger's `source_node` (the kube-authoritative spawn-ack binding)
  with the controller's own `bound_intents()` map as the fallback
  attribution. A node accumulating expired attempts for ≥2 distinct
  derivations inside a 30-minute window is marked Dead-equivalent
  (`rio_controller_node_wedge_marked_total`) and `reconcile_once`
  passes the union of that set and the scheduler-reported `dead_nodes`
  to `health::reap_unhealthy` — the same Dead arm, the same per-tick
  `dead_reap_cap`, no new reap path. Spec rule:
  `ctrl.nodeclaim.wedge-cluster` (controller.typ); red-first unit
  battery in `wedge.rs` (cluster threshold, single-derivation and
  healthy-pull non-marking, window aging, attribution fallback,
  unknown-deadline exclusion, union composition).
- **Model N impact: input source only.** The dead_nodes arm is consumed
  input, out of Model N's checked invariants (Stage-B encoding notes;
  the model header now carries the input-source note). No transition,
  bound, or invariant changes; the wired `quint-nodeclaim-*` checks are
  unaffected by construction and re-confirmed at the 1c landing gate.
  The Model N checklist re-derivation proper remains the 1c'/1d item
  already recorded in this map.
- **Coexistence posture.** The scheduler-side heartbeat detector and
  the `GetSpawnIntents.dead_nodes` plumbing are untouched and keep
  covering stream-mode pools until 1d (`sched.admin.hung-node-detector`
  unchanged); consolidate-only ticks still perform no Dead reaps from
  either source. The interim alert
  (`RioSchedulerAttemptEstablishmentCluster`) and the manual-reap
  runbook stay as the operator-facing tripwire and confirmation
  procedure (deployment-time checklist row D3).

Controller-campaign owner counter-signature for this entry: SIGNED
2026-06-02 (collected at the follow-up-ledger close-out — the
program's final signature event; b1d3a877d had deliberately left this
entry's landed form open; the scope and landing slot remain exactly
what the jointly-signed OA2 DECIDED block (2026-05-27) committed).
Checked at signing: the wedge.rs battery and union-composition
consumption stand as recorded (r[impl ctrl.nodeclaim.wedge-cluster] at
nodeclaim_pool/mod.rs reconcile_once Dead arm; rule at controller.typ;
rio_controller_node_wedge_marked_total registered+incremented); the
coexistence-posture bullet is read as history — the scheduler
detector's deletion and dead_nodes-always-empty are the signed 1c'/1d
content, not deviations. No Model J/N assumption is invalidated.

## Executor-campaign 1c' entry — deletion wave 1 and the Model J/N obligation re-derivation (delta pass)

Recorded by the executor-lifecycle campaign's slice-1c' model-and-spec
batch (Phase-1 plan v3, T-1c'.7 re-derivation half), after deletion
commits A–C removed the scheduler's stream session/placement machinery
and re-pointed the operator surfaces onto the open-attempt view.

- **What changed that this map's models consume.**
  `ListExecutors` is now served from the durable open-attempt view
  (busy = an open pull-mode attempt; the orphan-reap gate's
  fail-closed arms are unchanged and the leader-age arm survives until
  1d). The scheduler-side hung-node detector was deleted at commit A —
  earlier than the 1d slot the 1c entry above anticipated — so
  `GetSpawnIntents.dead_nodes` is now always empty and the OA2
  controller-side node-wedge clustering (live since 1c) is the only
  Dead-arm feed besides node conditions; the proto field is removed at
  the 1d sweep. The heartbeat registration edge is gone: the ICE-cell
  clear edge is the first successful pull (the fenced mint's
  single-cell clear). Termination-report dedup is the idempotent
  `ReportAttemptOutcome` fill plus the no-attempt no-op (the
  `recently_disconnected` map is deleted). `DrainExecutor` and
  `DebugListExecutors` are retired to clear-error stubs (RPC removal
  at 1d). The legacy pod-name exclusion key is still carried alongside
  the node key (P12 remains unexecuted).
- **Obligation-table re-derivation.** The seven Model J/N obligation
  rows of the executor map's 0e table are re-derived against the
  pull-only world in that map's Phase-1c' record (T-1c'.7 section),
  with re-derivation items (i)–(iii) discharged there: the reapSafety
  gate posture is retained (fail-closed on RPC error, leader-age arm
  unchanged until 1d), ORPHAN_REAP_GRACE re-validated against
  worst-case container-start → first successful pull with the accepted
  miss consequence limited to respawn churn (never a mid-build reap,
  never a charge), and the no-attempt no-op rule available as an
  assumption (spec'd and model-checked).
- **Model impact: none at this slice.** Models J and N are
  byte-unchanged; no transition, bound, or invariant changes; the
  wired `quint-spawn-coherence-*` / `quint-nodeclaim-*` checks are
  unaffected by construction. The header-checklist prose updates in
  the two model files ride with the campaign close-out (one rebuild of
  their checks instead of two). The Stage-C calibration table delta
  pass remains scheduled with the 1d re-derivation as recorded above.

Controller-campaign owner counter-signature for this delta entry:
SIGNED 2026-05-28 (collected with the campaign close-out; the
spec-sweep had landed). Checked at signing: `ListExecutors` is served
from the durable open-attempt view (`admin/executors.rs` — busy ⇔ an
open pull-mode attempt); the heartbeat-fed hung-node detector and the
`GetSpawnIntents.dead_nodes` plumbing are gone from the scheduler
(the proto field is reserved at the 1d sweep), leaving the OA2 wedge
clustering as the only Dead-arm feed besides node conditions, exactly
as stated; the ICE-cell clear edge is the fenced pull mint's
single-cell clear; the termination-report idempotence the
spawnCoherence assume-guarantee imports survives as the attempt-row
idempotency plus the no-attempt no-op (the model header and a
`pool/job.rs` comment still name the deleted `recently_disconnected`
map — prose drift only, covered by the header touch-up this entry
already defers to the model files' next rebuild); the
`DrainExecutor`/`DebugListExecutors` clear-error stubs and the
retained legacy pod-name exclusion key (P12) were as recorded at the
slice. The 0e obligation-row re-derivation items (i)–(iii) are present
in the executor map's Phase-1c' record. Models J and N are
byte-unchanged since the 1c OA2 header note, so the wired checks were
unaffected by construction at this slice; they are green at the
as-landed tree (re-confirmed at this counter-signature). No Model J/N
assumption is invalidated.

## Executor-campaign 1d entry — controller cleanup, proto sweep, Model D retirement (delta pass)

Cross-campaign record for the executor-lifecycle campaign's Slice 1d
(T-1d.1–T-1d.4 on the `executor-1d` branch), the contained delta pass
this map's cross-campaign entry requires. Written by that campaign;
counter-signature collected with the 1d landing.

- **What changed that this map's models consume.**
  The orphan-Running reap consults only the durable open-attempt view:
  the `ListExecutors` call, the leader-age arm and the empty-list arm
  are gone (`ctrl.ephemeral.reap-orphan-running+4`,
  `ctrl.job.busy-from-open-attempts+2`); fail-closed on a failed
  `ListOpenAttempts` read is retained, and a successful read is
  authoritative on its own (durable ledger state). The DisruptionTarget
  watcher's `DrainExecutor` force-drain hop is removed: every
  disruption-targeted pod takes the synthesize-preempted +
  foreground-delete path (`ctrl.drain.disruption-target+4`,
  `ctrl.pool.disruption+2`). The nodeclaim Dead-arm input is the OA2
  wedge clustering alone (`dead_union` removed; the
  `GetSpawnIntentsResponse.dead_nodes` field is reserved at the proto).
  The stream-era admin RPCs (`DrainExecutor`, `DebugListExecutors`,
  `ReportExecutorTermination`) and the `BuildExecution`/`Heartbeat`
  executor RPCs left the proto; the controller's pod-terminal path was
  already `ReportAttemptOutcome`-only since 1b. The builder runtime is
  the pull loop only (Pool `dispatchMode: Stream` still renders its
  template, but the builder ignores the env and always pulls — see the
  1d landing record's deferred-items list).
- **Model impact: none.** Models J and N are byte-unchanged. The
  leader-age retirement is an environment-input change below the
  models' abstraction: `reapSafety`'s subject — no orphan reap of a
  busy executor and no reap outside a passed gate — is preserved with
  the gate now being "view read succeeded" (fail-closed on error), and
  the busy-but-never-registered residual recorded at F1 is narrowed
  (there is no registration; busy is the durable open attempt).
  `quint-spawn-coherence-base` (reapSafety, orphanRemoved) and the
  orphan-reap / excess-reap witnesses were rebuilt green at the 1d
  tree.
- **Stage-C calibration table delta pass.** No controller-campaign
  calibration family's mechanism changed behavior at 1d: G-A/G-B
  (spawn dedupe/headroom), G-G (gate), M-1/M-2 (scaler), FFD and the
  M-3/M-4 rows are untouched; the only controller behavior changes are
  the reap-gate input re-key and the preemption hop removal recorded
  above, neither of which is a calibrated family. The full table
  re-validation stays scheduled with this campaign's own Phase-1 work
  as previously recorded.

Controller-campaign owner counter-signature for this delta entry:
SIGNED 2026-05-28 (collected after the 1d landing, with the campaign
close-out). Checked at signing, against the as-landed tree: the
orphan-Running reap reads only `ListOpenAttempts`, fail-closed on a
failed read, with no leader-age or empty-list arm and foreground
deletes (the `ctrl.ephemeral.reap-orphan-running+4` /
`ctrl.job.busy-from-open-attempts+2` impl sites); the dropped gate
arms are no-reap arms whose hazard — a post-failover in-memory map
that has not refilled — does not exist for the durable ledger view,
so `reapSafety`'s subject (busy ⇒ never reaped; no reap without a
passed gate, the gate now being view-read-success) is preserved and
the F1 busy-but-never-registered residual narrows exactly as stated;
Model J keeps the stream-era 3-arm gate encoding and is therefore
conservative with respect to the deleted arms until this campaign's
own pull-only re-encode (already a recorded obligation above). The
DisruptionTarget watcher takes the synthesize-preempted +
foreground-delete path with no `DrainExecutor` hop; the NodeClaim
Dead arm is fed by the OA2 wedge clustering alone, which Model N
consumes as input only per its header note; `GetSpawnIntentsResponse.
dead_nodes` is reserved and the stream-era executor/admin RPCs are
out of the proto. Models J and N are byte-unchanged; the wired
`quint-spawn-coherence-*` / `quint-nodeclaim-*` checks (all six J
regimes, all four N regimes, and the J witnesses) were re-confirmed
green at this counter-signature via `nix build --no-link` against the
as-landed model text, with distinct-state counts bit-identical to the
`402a459562` baselines (figures in the counter-signature commit
message and the checks' transcripts). The Stage-C delta-pass claim —
no calibrated controller family's mechanism changed at 1d — is
consistent with the per-family tables above. No Model J/N assumption
is invalidated.

---

## 3. Item I entry — store ComponentScaler CR removal (counter-signed)

> Origin: controller-invariant-map.md § "Item I entry", signature verbatim.

## Item I entry — store ComponentScaler CR removal (KEDA ScaledObject takeover) (delta pass)

Cross-campaign coordination record for harden-store work item I
(decision-5 store scaling, commissioned at the reconciliation memo's
ratification), following the C4-deletion precedent ("Track B invariant
map coordinated"). Written by the item-I executor; docs-only —
counter-signature of the landed form remains to be collected (the 1b
precedent: collected at landing review). *(Superseded: collected
2026-06-02 at the follow-up-ledger close-out — see the SIGNED block at
the end of this entry.)*

- **What changed that touches this map's subjects.** The `store`
  ComponentScaler CR is removed from the chart
  (`templates/componentscaler.yaml` deleted; `store.yaml`'s replicas
  gating re-keyed `componentScaler.store.enabled` →
  `store.autoscaling.enabled` with the gateway-style lookup-echo); the
  rio-store Deployment's replica count is now owned by a KEDA
  ScaledObject (`templates/store-scaledobject.yaml`: substitution
  backlog / builders-per-replica / CPU triggers, one-per-node
  topologySpreadConstraints — `infra.store.autoscaling`). The
  ComponentScaler CRD, the controller reconciler, and
  `componentscaler/decide.rs` with its full unit battery are
  UNCHANGED — the chart simply defines no CR. Spec rules re-scoped in
  the same commit: `ctrl.scaler.signal-substituting+3→+4` (rationale
  re-keyed from "the store" to "any ComponentScaler target"),
  `store.admin.get-load+2→+3` (reconciler polling now conditional on a
  CR targeting the store; PG-pool gauge publication store-owned),
  `obs.metric.store-pg-pool→+2` (self-published 30 s tick). The
  `vm-componentscaler-k3s` scenario is retired with the CR (its four
  wiring markers go with it; `ctrl.scaler.{component,ratio-learn}` and
  `store.admin.get-load` / `obs.metric.store-pg-pool` keep their unit
  verify sites in decide.rs / admin.rs); `vm-substitute-scale-k3s` is
  reworked to assert the autoscaling-signal path (the
  `rio_scheduler_substituting_derivations` gauge) instead of the CR
  closed loop.
- **Model impact: none.** The ComponentScaler loop (L2 / I13) is out
  of model for this campaign by the Stage-A scope decision recorded
  above ("out-of-model invariants"); Models J and N consume neither
  the CR nor the store replica count. No modeled tick body changes.
- **Stage-C calibration impact: none mechanical.** The M-1/M-2
  calibration families (and every other family table above) concern
  the prev_idle/ICE NodeClaim machinery, not the ComponentScaler; no
  calibrated mechanism's behavior changes when the chart stops
  defining a store CR. The reconciler code the `ctrl.scaler.*` rules
  govern is byte-unchanged; only its production target population is
  now empty (the loop idles until a future CR names a target).

Controller-campaign owner counter-signature for this delta entry:
SIGNED 2026-06-02 (collected at the item-I landing review). Checked at
signing: decide.rs/mod.rs functionally unchanged — only the r[...]
bumps (+4 impl/verify in decide.rs, store.admin.get-load+3 in mod.rs);
the ComponentScaler CRD ships
(infra/helm/crds/componentscalers.rio.build.yaml) while the chart
defines no CR (componentscaler.yaml deleted, store-scaledobject.yaml
present, store.autoscaling.enabled re-key); annotation sites moved
with their bumps (controller.typ:843/:875, observability.typ:260). No
Model J/N assumption is invalidated.
