# Substitution-replacement invariant ↔ spec-rule map

Working artifact for the **substitution-replacement campaign** (store-owned
materialization jobs replacing the scheduler's detached substitution walk).
Campaign design: `substitution-replacement-design.md` (post-adversarial-review
revision, 2026-05-31; owner A4 execution authorization). Phase A plan:
`substitution-phaseA-plan.md` (v2, post-adversarial-review). Verification
subject and fix target: `formal-sprint` @ `297bb6a78` (the post-ce-phase2
integration state). The executable counterparts of this map are
`docs/spec/models/retryPolicy.qnt` (the `retryPolicyPullMat` regime) and
`docs/spec/models/materializationJob.qnt` (draft).

The campaign has four phases: **A** (additive-dormant: schema, ledger kinds,
store executor, scheduler job machinery, dormancy proof — THIS record),
**B** (activation: flags on, VM matrix, consumer re-sourcing), **C′**
(model completion: materializationJob.qnt wired, §9.3 calibration transfer —
the go/no-go gate), **D′** (replacement: walk deletion, spec re-pointing,
migration retirement).

**Status: Phase A complete; Phase B complete (this map carries both stage
records). Phase A landed every §8-A mechanism dormant behind
`materialization.enabled = false`; its dormancy criteria (1–7) hold with
named artifacts. Phase B (the flag-on cutover) is landed: the deployment
layer defaults to materialization ENABLED (helm values + VM fixtures, with
the AS-6 AND-guard; Rust struct defaults stay `false` per PD-B1), the six
equivalence criteria hold with named artifacts (the Phase B stage record
below), the full VM matrix runs in BOTH flag states (every `-walk` attr is
the byte-original as-built oracle), and the full CI gate is green at the
Phase B tip with flipped defaults. Phase C′ owns the model completion and
the §9.3 calibration transfer (the go/no-go); Phase D′ owns deletion.**

The spec rules this campaign added (Phase A, T-1.1):
`sched.materialize.job`, `sched.materialize.routing`,
`sched.materialize.pinning` (docs/spec/components/scheduler.typ),
`store.materialize.executor` (docs/spec/components/store.typ). All four carry
impl AND verify markers as of this record (tracey: covered).

---

## The §9.1 property skeleton (the C′ checklist)

The design §9.1 names the successor property table for the job lifecycle.
Phase A's draft model (`materializationJob.qnt`, T-5.3 — typechecked, NOT
wired) encodes them; the production half of several is already
unit/actor-tested by the Phase A batteries. C′ flips the "model-checked"
column; nothing is claimed checked until then.

Status legend: **encoded** = present in the draft model; **prod-tested** =
a Phase A unit/actor/wire test pins the production behavior; **checked** =
a wired quint check proves it (none in Phase A — C′'s gate).

| Property (`materializationJob.qnt` name) | Statement (one line) | Status |
|---|---|---|
| `noFromSourceWhileJobUnresolved` | A node with an unresolved job never gets a from-source build attempt. | encoded; prod-tested (kernel `check_kinded_no_build_delivery_while_job_unresolved` CBMC harness + kinded battery) |
| `jobResolutionSound` | A job resolves only by consumption of one of its own attempts' outcomes (exec_id-keyed). | encoded; prod-tested (consumption battery, T-3.5) |
| `routingRequiresDurableVouchOrFailFast` | The Unobtainable routing lands From-Source on durable Vouched/Pending evidence OR on the unmarked re-probe dispositions; FailFast only on the **four-conjunct** corner (Broken evidence ∧ `topdown_pruned` mark ∧ confirmed-missing-or-spent-one-shot — the finding-11 mark discriminator, `sched.materialize.routing+2`). | encoded **STALE vs as-built** (the draft predates finding 11 — its arm 3 fail-fasts unmarked nodes and its property forbids the unmarked from-source disposition; C′ delta 1 re-encodes both); prod-tested (`routing_fail_fast_requires_all_four_conjuncts` exhaustive + the T-4.4 matrix, Phase B `c952e5a51`) |
| `unresolvedJobAlwaysArmed` | An unresolved unparked job is always claimable or claimed (never stranded). | encoded; prod-tested (re-arm paths in T-3.5/T-3.6 batteries; **Phase B**: `flag_on_every_job_state_has_armed_action` — settlement totality over every job state incl. the 5b Vouched-parked arm, T-4.2/T-6.1) |
| `noWrongfulTerminalFailure` | No build terminally fails while its wanted set is obtainable. | encoded; prod-tested (moot-arm C3 replay, T-6.2) |
| `noWrongfulFromSourceRouting` | No from-source routing while upstream still offers the wanted set. | encoded |
| `successConsumptionCoversLiveWanted` | Success consumption completes a node only when ingested+verified ⊇ live wanted. | encoded; prod-tested (`success_consumption_coverage_check`, keystone stage 6) |
| `interestUnionLiveOnly` | The effective wanted union ranges over LIVE builds only. | encoded; prod-tested (moot-arm replay: cancelled build's wants leave the join) |
| `pinCoversIngestUntilAllInterestTerminal` | Materialization pins survive until job resolved AND no live interest (PD-10/DF-3 upward re-kind). | encoded; prod-tested (pin-kind db battery, T-4.2) |
| `failoverPreservesJobs` | Jobs survive leader failover (PG-authoritative). | placeholder in draft; **prod-tested (Phase B)**: T-4.3 recovery view-rebuild battery (pending → DeliverNew, claimed → DeliverExisting same-holder, parked → NotYetReady until backoff lapses) + `vm-materialization-failover-k3s` (10 jobs survive leader force-delete, ids identical) — C′ encodes the pre/post ghost |
| `fencedJobWritesOnly` | Every job write carries the claims-floor fence. | placeholder in draft; **prod-tested (Phase A db batteries; Phase B)**: the T-5.2 five-site posture audit (3 in-tx + 2 standalone-fenced — every creation site fenced) — C′ encodes via the leaseGen/staleWriteDiscarded ghosts |
| `kindMatchesWorker` | Materialization attempts execute only on store replicas; build attempts only on builders. | encoded; prod-tested (grpc authorization battery: executor tokens never authorize the kind) |
| `materializationNeverPoisons` | No materialization charge ever produces a Poison verdict. | encoded; **checked** (`quint-retry-policy-pull-materialization`, T-5.1/T-5.2) + CBMC `check_materialization_never_poisons` |
| `materializationInvisibleToBuildBudgets` | Materialization charges feed exactly one budget (their own). | encoded; **checked** (`quint-retry-policy-pull-materialization`) + CBMC `check_materialization_rows_invisible_to_build_decision` + keystone stage 7 |
| `atMostOneUnresolvedJobPerDrv` | The partial-unique-index dedup. | placeholder in draft; **prod-tested (Phase A + Phase B)**: `flag_on_concurrent_interest_creates_one_job` + the cross-creation-layer race `flag_on_concurrent_probe_and_merge_create_one_job` (T-5.2) + the T-4.1 two-build C3 trace — C′ widens the job slot to a multiset and re-checks |
| `atMostOneClaimWinner` | One-winner claim arbitration per job (BC-1 composite identity). | encoded; prod-tested (CBMC `check_kinded_one_winner_arbitration` + keystone one-winner stage) |
| `wrongfulFailFastBoundedPerJob` | At most one wrongful fail-fast per job (the one-shot). | encoded |

Draft witnesses (non-vacuity probes, wired at C′): `noSuccessResolution`,
`noFailFast`, `noPark`, `noCrashEstablishment`, `noBuildAttempt`.

Three of the seventeen draft properties violate in pre-wiring simulation for
a documented draft-encoding reason (current-state reads where at-decision-time
ghosts are needed): `routingRequiresDurableVouchOrFailFast`,
`noWrongfulTerminalFailure`, `noWrongfulFromSourceRouting` — recorded in the
T-5.3 commit (`420e7c1c7`) as C′ completion work, not as design findings.

**The draft is now also BEHIND the as-built system**: Phase B landed six
behavior deltas the draft predates (the finding-11 mark discriminator, the
finding-18 channel/backfill behaviors, the park re-evaluation split, the
five-origin/posture set, and the always-on §4 clear-mirror + §5.3 pin
release). The complete delta list C′ must encode is in the Phase B stage
record's "C′ handoff" section below — wiring the draft without those deltas
would verify the wrong system.

---

## Phase A stage record (additive-dormant; landed 2026-06-01)

### Identity

- Branch: `a4-phaseA`, 30 commits ahead of baseline `297bb6a78`
  (24 plan task commits + 2 security-fix commits + 2 generated-file pickups +
  1 docs-lint fix + this record).
- Both flags default `false` everywhere: `[materialization] enabled = false`
  in rio-scheduler and rio-store config structs, helm `values.yaml`, every
  `values/*.yaml`, and every VM fixture.
- Full CI gate (`nix-fast-build .#checks.x86_64-linux`): green at the Phase A
  tip (T-6.5).

### What landed (the deliverables table, with commits)

| # | Deliverable | Commits |
|---|---|---|
| 1 | Spec rules `sched.materialize.{job,routing,pinning}` + `store.materialize.executor` | `f03014d67` |
| 2 | Migration 078 (build_wanted_outputs, materialization_jobs, interest view, attempt_kind, pin_kind/job_id) | `3d0616657` |
| 3 | Fenced db modules db/wanted.rs + db/materialization.rs (in-tx core per PDQ-9) | `561161a48`, `a13ea52ef` |
| 4 | Proto: AttemptKind, executor_instance, MaterializationOutcome, ListMaterializationJobs, ReportMaterializationProgress | `319050570` (carries PD-15b) |
| 5 | Migration 079 + OutcomeClass::{MaterializationUnobtainable,MaterializationInfra} lockstep (FP-2) | `955ad503a` |
| 6 | The kind-partitioned fold (kernel skip-arm + materialization_decide + 2 CBMC harnesses; suffix-load JOIN) | `00218cecb`, `9c55c6aee` |
| 7 | Scheduler + store `[materialization]` config tables + helm values/env plumb | `0bfa15cd1`, `2a4d329ac`, `db26927f0` |
| 8 | admit_pull coexistence wrapper (admit_pull_kinded + 4 CBMC harnesses; as-built kernel byte-identical) | `13945c973`, `4cf2d24a1` |
| 9 | Job creation: probe-partition fenced helper + merge new_sub/pruned origins IN the merge tx (PDQ-9) + interest registration + in-memory view | `5cc237d5e` |
| 10 | Consumption: Success coverage + four-arm Unobtainable routing (pure core + handler) | `56d66bda8` |
| 11 | Establishment branch (no adopt, materialization_infra, never executor_crash) + charge-free cancellation closer | `34fa915ee` |
| 12 | ListMaterializationJobs RPC (leader-served) + executor_instance/kind intake (BC-1) | `f4895284d`, `defa7f4a7` |
| 13 | Store executor: claim client (MaterializeTransport/SchedulerTransport) + execute_job (tenant re-resolution → wanted → closure walk → pin-at-ingest → outcome) + flag-gated spawn | `36c5bbf72`, `b640aa7bf` |
| 14 | Pin release rules: pin_kind discrimination on the three build-input release paths + pin_materialized_paths (ON CONFLICT upward re-kind, PD-10) + all-interest-terminal release | `1c02f51d2` |
| 15 | Netpol/helm/spec: store-egress→scheduler:9001 + ingress mirror + fetcher.typ +3 bump + vm-netpol-k3s subtest | `db26927f0` |
| 16 | retryPolicyPull materialization extension, wired (PD-21 regime split; existing checks untouched) | `bc6181e2b` |
| 17 | materializationJob.qnt draft (typechecks; NOT wired — C′) | `420e7c1c7` |
| 18 | The dormancy proof: VM flag-off subtests + flag-on smoke battery + full-battery sweep | `b6c02171b`, `3e45bd925`, T-6.3 (no commit) |
| 19 | This stage record | this commit |
| — | Store-service credential (ServiceClaims caller="rio-store") for materialization operations — the Wave-4 security obligation | `b70742c76` |
| — | Generated-file pickups (docs/gen modules.json; Wave-1/Wave-4 omissions caught by drift checks) | `d29b45a85`, `6fac6f0da` |
| — | docs-lint fix (PD-20 deferred-metric name removed from rationale prose) | `383191777` |

### Wave-by-wave commit record

| Wave | Commits (in order) |
|---|---|
| 1 — spec, schema, db modules | `f03014d67`, `3d0616657`, `561161a48`, `a13ea52ef` |
| 2 — proto + ledger kinds | `d29b45a85`, `319050570`, `955ad503a`, `00218cecb`, `9c55c6aee` |
| 3 — scheduler-side machinery | `0bfa15cd1`, `13945c973`, `4cf2d24a1`*, `f4895284d`, `defa7f4a7`*, `5cc237d5e`, `56d66bda8`, `34fa915ee` |
| 4 — store-side, pins, netpol | `2a4d329ac`, `1c02f51d2`, `b70742c76`*, `36c5bbf72`, `b640aa7bf`, `db26927f0`, `6fac6f0da` |
| 5 — models | `383191777`, `bc6181e2b`, `420e7c1c7` |
| 6 — dormancy proof + close-out | `b6c02171b`, `3e45bd925`, this commit |

\* security-review-driven commits (see the security record below).

### Dormancy evidence (criteria 1–7, the named artifacts)

| # | Criterion | Verdict | Artifact |
|---|---|---|---|
| 1 | Existing-test invariance | **HOLDS** | T-6.3 diff audit (`/tmp/rio-dev/t63-testdiff.txt` + this record): 49 rust files changed vs baseline, 134 deleted lines, every one triaged to a mechanical carve-out (1a/1b/1c) enumerated in its introducing commit — call sites gaining `kind`/`executor_instance`/`materialization_outcome` parameters with flag-off identity values, the T-2.3 kani contract-predicate restatement, import reformats. Zero assertion changes, zero deleted tests. Full battery green at every commit boundary. |
| 2 | Empty-tables invariance | **HOLDS** | VM: `vm-substitute-standalone` subtest `materialization-dormant` (five-table zero-count after the six substitution subtests + systemd env guard, GREEN); `vm-lifecycle-core-k3s` fragment `materialization-dormant` (same count against real builder traffic — 3 executions / 1 attempt row observed, all build-kind; non-vacuity precondition enforced; GREEN). Actor: `flag_off_merge_dispatch_creates_no_materialization_state`. |
| 3 | Kernel invariance | **HOLDS** | `git diff 297bb6a78 -- rio-evidence-kernel/src/pull.rs`: **zero deleted lines**; 815 pure additions below the as-built code (the kinded wrapper + its battery + 4 CBMC harnesses). The as-built `admit_pull`, its battery, and its five harnesses are byte-identical. |
| 4 | Wire invariance | **HOLDS** | T-2.1 back-compat decode tests (`rio-proto/tests/proto_field_presence.rs`: legacy-encoded requests decode to the documented zero-value mapping); recompiled rio-builder sends byte-identical requests (PD-15: prost omits default fields); all builder VM scenarios pass unchanged in the final gate. |
| 5 | Wired-check invariance | **HOLDS** | `nix/quint.nix`: zero deleted lines (additions confined to the new regime checks). `nix/kani.nix`: only the two expectedHarnesses lockstep bumps (6→8, 13→17) + harness-inventory comments. `quint-retry-policy-pull`: invariant list unchanged (14), verdict [ok], state space bit-identical to baseline (39,711,022 distinct / 193,010,980 generated). All 9 witnesses unchanged. Every baseline check attr exists; 2 new check attrs added (`quint-retry-policy-pull-materialization`, `quint-retry-policy-pull-witness-materialization-crash`). |
| 6 | Schema-additivity | **HOLDS** | Migrations 078/079 read-verified: only CREATE TABLE/INDEX/VIEW, ADD COLUMN ... DEFAULT, and DROP CONSTRAINT + ADD CONSTRAINT where the 079 CHECK is a strict superset (13 → 15 literals). Checksum-frozen (`migration_checksums_frozen` pins both); alphabet-lockstep test green. |
| 7 | Config-schema additivity | **HOLDS** | Zero deleted lines in `rio-scheduler/tests/fixtures/config-schema.json`, `rio-store/tests/fixtures/config-schema.json`, `docs/gen/config.json` vs baseline; every added line belongs to the new `[materialization]` sections (2 insertion hunks per file). No existing field's default/description/serde shape changed. |

### Test-count accounting (baseline → Phase A tip)

T-6.3 full-battery sweep: **3405 tests run, 3405 passed, 28 skipped, zero
failures** (the complete workspace; baseline-equivalent count 3330 + the 75
Phase A additions below; no skip exists that was not skipped at baseline).

| Battery | Baseline | Phase A tip | Delta |
|---|---|---|---|
| rio-scheduler | 1107 + 1 skipped | 1153 + 1 skipped | +46 |
| rio-store | 524 + 13 skipped | 537 + 13 skipped | +13 |
| rio-evidence-kernel | 13 | 17 | +4 (kinded battery) |
| rio-retry-kernel | 10 | 14 | +4 (kind partition) |
| rio-proto | 52 + 8 skipped | 56 + 8 skipped | +4 (back-compat decode) |
| rio-migrations | 7 | 11 | +4 (078/079 contracts) |
| **Total** | **3330 + 28 skipped** | **3405 + 28 skipped** | **+75** |
| CBMC harnesses (kani-rio-evidence-kernel) | 13 | 17 | +4 |
| CBMC harnesses (kani-rio-retry-kernel) | 6 | 8 | +2 |
| Wired quint checks (retry-policy-pull family) | 10 | 12 | +2 |
| VM subtests | — | +3 | materialization-dormant ×2 + netpol-store-scheduler-egress |

Zero existing tests were modified beyond the enumerated mechanical carve-outs;
zero were deleted; zero were weakened.

### Flag-on smoke evidence (dormant ≠ vestigial; T-6.2)

All in-process (PD-14 as amended by dormancy-5); the real-deployment flag-on
matrix is Phase B's.

| Test | Level | What it proves |
|---|---|---|
| `flag_on_materialization_job_end_to_end` | actor | THE keystone: merge → probe-partition job creation → listing → claim (one-winner) → InfraFailure charge + re-arm → re-claim → Success consumption → node Completed / job resolved_success / build Succeeded. Plus the budget partition (suffix with one materialization row folds to the empty-history build verdict) and the pin partition (zero build_input pins). |
| `flag_on_moot_unobtainable_never_fail_fasts` | actor | The design §2.4 C3-trace replay (AS-2/PP-1): interest narrows inside the claim→consume window → Unobtainable routes to CompleteForLiveInterest, never a fail-fast. |
| `flag_on_queued_node_refuses_materialization_claim` | actor | The PDQ-6 boundary pin: Ready-only claims; a Queued node refuses NotYetReady with zero minted rows. Phase B flips this red-first. |
| `flag_on_materialization_lifecycle_through_grpc` | wire (real tonic + real HMAC) | The store↔scheduler seam with the real credential (`ServiceClaims{caller="rio-store"}` on `x-rio-service-token`): listing → kinded claim (InvalidArgument on empty instance) → Success report → consumption. Stop condition 9's HMAC assumption is checked fact. |
| Wave 3/4 flag-on batteries | actor/db/unit | Creation (probe/new_sub/pruned origins, dedup), consumption (all four routing arms + infra budget/park), establishment (never executor_crash, never adopt), cancellation (charge-free), store executor (claim loop, execution, B3 classification, dormancy gate). |

### Model state

- `retryPolicy.qnt`: the materialization attempt class is wired and green
  (T-5.1/T-5.2, PD-21 regime split). The build-only regime
  (`quint-retry-policy-pull`, 14 invariants) is bit-identical to baseline; the
  coexistence regime (`quint-retry-policy-pull-materialization`, 16
  invariants) proves `materializationNeverPoisons` +
  `materializationInvisibleToBuildBudgets` over 3.35M states; the crash
  witness violates as expected. Cross-campaign addendum:
  `retry-invariant-map.md` (Wave 5 section).
- `materializationJob.qnt`: drafted (1221 lines: 20 actions, 17 properties,
  5 regimes, 2 named runs), typechecks, named runs pass, NOT wired (C′ gate).
- CBMC: 6 new harnesses across the two kernels (listed under criteria 3/5).

### Plan-decision log — exercised entries and in-execution deviations

The plan's PD-1..PD-20 were all either exercised as written or not reached.
Two **in-execution deviations** were taken and need (or have received)
orchestrator ratification:

| ID | Deviation | Status |
|---|---|---|
| **PD-15b** (proposed, Wave 2) | T-2.1's two new RPCs force tonic-generated REQUIRED trait methods on `executor_service.rs` — the tonic-service analog of dormancy-1/RB-1/RB-2 that the adversarial review enumerated for three mechanisms but missed for the fourth. Resolution: both handlers implemented as the dormant Phase-A arms the plan's own proto comments specify (list → empty; progress → ack-and-drop), in the forcing commit (`319050570`). T-3.3 later replaced the listing stub with the real handler per its task spec. | Ratified by execution (the orchestrator accepted Wave 2's record; the resolution is what the plan's wire-contract text requires). |
| **PD-21** (proposed, Wave 5) | The plan-literal quint wiring (extend `quint-retry-policy-pull`'s own step + invariant list) is arithmetically incompatible with stop-condition 8: the materialization counters multiply the 39.7M-state space ≥6× (minimum ceilings) vs the 2× threshold. Adaptation: ENABLE_MATERIALIZATION regime split — the existing check's attr/list/space stay untouched (stricter dormancy than the plan's exception clause); the 16-invariant list lives in the NEW coexistence-regime check where the partition invariants are non-vacuous. | Ratified by execution (Wave 5's record; preserves every Wave-5 acceptance obligation within budget; the literal form cannot meet stop-condition 8 as written). |

Plan-recorded decisions exercised with notable in-execution detail:

- **PDQ-9 (in-tx merge creation)**: implemented exactly as adjudicated —
  `create_materialization_jobs_in_tx` rides `persist_merge_to_db`'s
  transaction; the in-memory view is fed post-commit; the probe-partition
  site keeps the standalone fenced helper (no enclosing transaction exists —
  design §2.1 row 3 imposes none). **Phase B revisit note**: if Phase B adds
  more creation sites (reprobe/stale_reset per PD-17/PD-18), re-confirm
  whether the probe site should join an enclosing transaction — the in-tx
  vs fenced-helper split is currently per-§2.1-row, not a global rule.
- **PD-6/PDQ-6 (Ready-only claims)**: pinned twice (kernel claim-table case +
  `flag_on_queued_node_refuses_materialization_claim`). Phase B's flip list
  is in the handoff below.
- **PD-15 (rio-builder carve-out)**: exactly the two
  `..Default::default()` extensions + their explanatory comments; the T-6.3
  audit confirms nothing else in rio-builder changed.
- **PD-17/OQ7 (reprobe lane keeps walking flag-on)**: confirmed real by
  T-6.2's moot-test development — a second build merging an existing READY
  job-carrying node routes it through the I-099 reprobe lane, which spawns
  the as-built walk flag-on; the walk completes the node and orphans the
  pending job (the zero-interest closer cancels it). This is the documented
  partial-coexistence posture, now with a concrete observed trace. Phase B's
  PD-17 work (reprobe-lane job creation + AS-5 reset) closes it.

### Security-review record (Wave 3 review; all fixed in-phase)

Four findings from the Wave-3 commit security review, each fixed red-first
and converted into a permanent property/test:

| # | Finding | Fix | Permanent property |
|---|---|---|---|
| 1 (HIGH) | Kernel kinded-wrapper Claimed re-delivery arm bypassed token/fence gates | `4cf2d24a1` — gates dominate every arm | CBMC `check_kinded_rejections_dominate` (full flag-on kind × job-view domain); expectedHarnesses 16→17 |
| 2 (HIGH) | PullAssignment kind authorization: any executor token could request kind=MATERIALIZATION | `defa7f4a7` (Phase-A-viable closed posture) + `b70742c76` (the real credential) | grpc battery: executor tokens → PermissionDenied on both carriers; ServiceClaims{caller="rio-store"} (service-HMAC family) authorizes exactly the materialization operations; wrong-caller/wrong-key/half-configured all rejected |
| 3 (MEDIUM) | ListMaterializationJobs accepted any executor token for fleet-wide listing | same commits | same battery (listing arm) |
| 4 (MEDIUM) | `executor_instance` unattested and @-injectable in the composite identity | `defa7f4a7` | DNS-1123 validation both sides (client `sanitize_dns1123_label`, scheduler `is_dns1123_label`); grpc battery sweeps the malformed-label corpus |

**The Wave-4 ServiceClaims decision** (security obligation 1, resolved):
the kind-attested store credential is `ServiceClaims{caller="rio-store"}`
signed with the **service**-HMAC key (the existing `x-rio-service-token`
family) — the existing rio-auth claims type + the existing
ServiceTokenInterceptor, **zero rio-auth claim-structure changes**. The
scheduler verifies via `SchedulerGrpc.service_verifier` (the same verifier
AdminService uses) with caller allowlist `["rio-store"]`; the store mints
per-request 60s tokens from the serviceHmac key file helm already mounts on
both. T-6.2's wire test uses exactly this credential.

### Discovered-late items (the latent-failure class; all fixed in-phase)

| Item | Discovery | Fix |
|---|---|---|
| docs/gen/modules.json drift (Wave 1's new db modules not picked up) | T-2.1's regen drift check | `d29b45a85`; recurrence after Wave 4's store modules: `6fac6f0da` |
| docs-lint failure latent since Wave 1 (T-1.1 rationale prose named the PD-20 deferred metric as a raw backticked name) | Wave-4 end first full quick gate | `383191777` (prose reworded; normative body untouched, no tracey bump) |
| Lesson recorded | Run the bare quick gate (`.claude/bin/nixbuild`) at every wave end, not only targeted checks. Adopted from Wave 5 on. | — |

### What Phase A does NOT claim

- **No production behavior change was intended, so none is claimed** — no
  performance/latency claims, no resource claims, nothing about the walk
  path's behavior under load.
- **No coverage claim for the four-arm routing beyond unit/actor tests** —
  the arms are exercised in-process; the real store executor against the real
  scheduler is Phase B's VM matrix.
- **Nothing about the §9.3 calibration transfer** — that is C′'s gate; the
  draft model's three known-violating properties are draft-encoding artifacts,
  not verdicts.
- **No claim that the store executor's closure walk is equivalent to the
  scheduler's walk** — outcome-equivalence testing is Phase B (design §8-B).
- **The in-memory job view is not recovery-safe** — it is rebuilt only
  flag-on within one tenure; recovery rebuilding is Phase B work (flag-off
  recovery has nothing to rebuild).

---

## Phase B entry criteria and handoff

### Entry criteria (from design §8)

1. This plan integrated into `formal-sprint` behind a green full gate (done —
   this record rides the integration).
2. The design's Phase B VM matrix scoped (vm-materialization-* scenarios,
   outcome-equivalence, flag-transition restarts, mixed-flag AS-6 scenarios).
3. The PD items below absorbed into the Phase B plan.

### What Phase B needs to know

**Flag locations** (flip dev first; store first ON, last OFF — design §4/AS-6):

| Component | Config | Env | Helm |
|---|---|---|---|
| scheduler | `[materialization] enabled` (rio-scheduler/src/config.rs `MaterializationConfig`) | `RIO_MATERIALIZATION__ENABLED` | `scheduler.materialization.enabled` |
| store | `[materialization] enabled`, `executor_concurrency`, `poll_interval_secs`, `scheduler_addr` (rio-store/src/config.rs) | `RIO_MATERIALIZATION__*` | `store.materialization.*` |

**Smoke-test entry points** (the in-process patterns Phase B's VM scenarios
replicate at deployment level):

- Actor-level: `rio-scheduler/src/actor/tests/materialize.rs` — keystone
  (`flag_on_materialization_job_end_to_end`), C3 replay, Queued pin; the
  helpers (`claim_materialization`, `report_materialization_outcome`,
  `list_materialization_jobs`, `setup_with_mock_store_materialization_enabled`).
- Wire-level: `rio-scheduler/src/grpc/tests/pull_tests.rs`
  (`flag_on_materialization_lifecycle_through_grpc`) — the credential mint +
  header pattern.
- Store-side: `rio-store/src/materialize/` unit batteries (claim loop,
  executor, dormancy gate `executor_not_spawned_flag_off`).

**VM scenario hooks**:

- `nix/tests/scenarios/substitute.nix` — the `materialization-dormant`
  subtest is the flag-off pin; Phase B adds flag-ON scenarios alongside (the
  existing six subtests are the as-built-path oracle for outcome
  equivalence).
- `nix/tests/scenarios/lifecycle/materialization-dormant.nix` — the
  builder-traffic dormancy fragment; its non-vacuity precondition pattern is
  the template for Phase B fragments.
- `nix/tests/scenarios/netpol.nix` — `netpol-store-scheduler-egress` proves
  the edge; Phase B's flag-on scenarios depend on it.
- The k3s fixture renders `RIO_MATERIALIZATION__*` from helm values —
  Phase B flips via `extraValues`.

**Model regime to extend**: `retryPolicyPullMat` in
`docs/spec/models/retryPolicy.qnt` (ENABLE_MATERIALIZATION = true) is where
new materialization-channel actions go; the build-only regime
(`retryPolicyPull`) must stay bit-identical (the dormancy oracle).
`materializationJob.qnt` completion + wiring is C′.

### The PD items Phase B must absorb

| Item | What Phase B does |
|---|---|
| **PD-6 Queued claims** (PDQ-6) | Kernel arm + `Queued→Assigned` transition edge + mint-ordering rework. Flip red-first: the 3 transition-table tests (`test_derivation_valid_transitions`, the explicit negative at derivation.rs:2417, `validate_transition_exhaustive` + its `sched.state.machine` tracey bump), T-3.2's Queued claim-table case, T-6.2's `flag_on_queued_node_refuses_materialization_claim`. |
| **PD-7 GetSpawnIntents filter** | Unresolved-job nodes filtered from spawn intents (controller-facing; needs the Phase B VM matrix). |
| **PD-17 reprobe-lane job creation** | origin=`reprobe` WITH the AS-5 6d-slot status reset + poison_cleared row + a flag-on reprobe-path actor test. Closes the observed walk-vs-job coexistence boundary (see the PD-17 entry above). |
| **PD-18 stale-Completed-verify creation** | origin=`stale_reset`. Schema/enum/proto already carry the literal. |
| **PD-20 park observability** | `rio_scheduler_materialization_stalled` metric + parked-job re-evaluation housekeeping arm. |
| **§2.6 consumer re-sourcing** | Snapshot buckets, scaler signal, gateway events, dashboard, build_summary. |
| (PD-9 was struck by PDQ-9 — merge-origin in-tx creation is done; nothing to absorb.) | |

### The two named Phase B obligations (from the security review)

1. **Instance attestation binding** (security finding 4's full fix): bind
   `executor_instance` INTO the ServiceClaims so the scheduler verifies (not
   trusts) the replica identity. Deferred because ServiceClaims carries
   `deny_unknown_fields` — adding a field is a cross-version wire skew (the
   bug_011 class) = a cross-cutting rio-auth change, out of Phase A's
   additive scope. Until then the identity is DNS-1123-validated but
   client-asserted.
2. **The PDQ-9 probe-site transaction posture revisit**: when PD-17/PD-18 add
   creation sites, re-confirm the per-§2.1-row split (in-tx for merge
   origins, standalone fenced helper for probe origins) still partitions
   cleanly, or whether a uniform in-tx rule should replace it.

---

## Phase B obligation records (Wave 5)

Working records for the two named Phase B obligations (the handoff section
above) plus the OQ7 close-out audit. Wave 7's stage record extends these;
nothing here is a stage claim on its own.

### Obligation 1 — instance attestation binding (T-5.1, discharged)

`ServiceClaims` carries an optional `instance` claim (serde `default` +
`skip_serializing_if`, `deny_unknown_fields` kept — the AssignmentClaims
tenant pattern, adjudication PDB-8). The store's materialization transport
binds its HOSTNAME-derived `executor_instance` into every minted token
(`ServiceTokenInterceptor::with_instance`); the scheduler requires an
instance-bound credential on the materialization WORK surfaces
(kind=MATERIALIZATION PullAssignment, ListMaterializationJobs,
materialization ReportOutcome) and rejects claims whose request
`executor_instance` differs from the token's bound instance
(`PermissionDenied "instance claim mismatch"`). The display-only progress
relay keeps the fleet-level credential. Commit `520fc26fb`.

Wire-compat record (the three legs, each test-pinned):

| Leg | Behavior | Pin |
|---|---|---|
| old token → new verifier | parses, `instance = None` (serde default) | `service_claims_without_instance_round_trips` |
| new instance-less token → old verifier | byte-identical wire shape (skip_serializing_if) | same test + `assignment_claims`-pattern shape assertion |
| instance-bound token → pre-T-5.1 verifier | REJECTED `unknown field 'instance'` — **fail-closed** (no mixed-version window can authenticate an instance-bound token while skipping the binding check; the failure mode is claim retries, never unenforced binding) | `service_claims_instance_forward_skew` |

### Obligation 2 — PDQ-9 creation-site transaction posture (T-5.2)

The Phase A handoff required re-confirming the per-§2.1-row split once
PD-17/PD-18 added creation sites. The audit over all five sites at this
commit:

| Creation site | Origin | Posture (audited) | §2.1 row | Why |
|---|---|---|---|---|
| merge classification (new_sub lane) | `cache_opportunity` | **in-tx** (merge-tx batch, `create_materialization_jobs_in_tx`) | row 1 | the classification IS the merge transaction's content |
| top-down prune | `pruned` | **in-tx** (same batch) | row 2 | the prune verdict commits with the merge |
| reprobe lane (PD-17 / T-1.5) | `reprobe` | **in-tx** (same batch + the AS-5 6d status reset) | reprobe row | the reprobe classification is hoisted pre-tx and rides the same commit |
| dispatch probe partition | `cache_opportunity` | **standalone fenced helper** (`create_materialization_job_fenced`) | row 3 | no enclosing transaction exists at the probe site |
| stale-Completed verify (PD-18 / T-1.6) | `stale_reset` | **standalone fenced helper** | row 4 | the verify runs POST-tx (reconcile 6c — `persist_merge_to_db` committed long before); there is no transaction to join |

**Verdict (PD-B9, kept):** the split is per-position, structural, and total
over all five sites — sites that run inside the merge transaction join it
(one fence per transaction); sites with no enclosing transaction (the probe
partition and the post-tx stale verify) use the standalone fenced helper,
which is itself exactly "a transaction whose only content is the job
INSERT". A uniform in-tx rule would manufacture transactions that already
exist under another name. No change.

**The one property the split could break — pinned:** cross-site dedup. The
dispatch-probe site and a concurrent merge racing to create a job for the
same derivation converge on ONE row (the `materialization_jobs_unresolved`
partial-unique index arbitrates across creation layers; the loser blocks on
the winner's uncommitted row, then takes the dedup arm — both orders).
Test: `flag_on_concurrent_probe_and_merge_create_one_job`
(db/tests/materialization.rs).

### OQ7 close-out — the zero-walk coexistence boundary audit (T-5.3)

**The boundary statement (equivalence criterion 3, scoped per PD-B19):**
flag-on, walks spawn ONLY for nodes carrying flag-off-era evidence state
(`topdown_pruned` marks / `substitute_tried` / `Substituting` status), never
for fresh flag-on work. Legacy-state walks are the documented, correct
transition-window absorption (design §2.3 Substituting row + §4); fresh-work
walks would be a criterion-3 violation (stop condition 2).

**The static audit — every `spawn_substitute_fetches` call site at this
commit (six sites; the count and gating are what the runtime test pins):**

| # | Site | Fed by | Mechanism selection flag-on | Why fresh work cannot reach it |
|---|---|---|---|---|
| 1 | `dispatch.rs:309` (probe-partition spawn) | `to_spawn` pushes at `:251` (D16 present+tried cell), `:278` (substitutable cell), `:290` (tried+substitutable settlement cell) | `:278` is flag-gated (job creation instead — Phase A); `:251`/`:290` require `substitute_tried` ∧ `must_substitute` (mark + Broken evidence) | the gating cells' preconditions are flag-off-era evidence state; fresh flag-on work never sets `substitute_tried` (no walk ever ran for it) |
| 2 | `dispatch.rs:952` (`handle_substitute_complete` downgrade re-spawn) | walk consumption | walk-consumption code | only runs as a consequence of a walk having completed — transitively unreachable for fresh work |
| 3 | `dispatch.rs:1320` (`settle_broken_marked_root` verification walk) | mark-keyed settlement | mark-carrying nodes only | requires the `topdown_pruned` mark + as-built settlement state |
| 4 | `merge.rs:935` (6d reprobe lane) | `reprobe_sub` | **flag-gated (PD-17 / T-1.5)**: `apply_reprobe_reset_in_memory` + in-tx `reprobe` jobs flag-on; walks flag-off | the gate is load-bearing — closed in Wave 1 |
| 5 | `merge.rs:1017` (6g new_sub lane) | `new_sub` | **flag-gated (Phase A)**: in-tx `cache_opportunity` jobs flag-on; walks flag-off | gated since Phase A |
| 6 | `merge.rs:2085` (`verify_preexisting_completed` stale-verify spawn) | `to_spawn` push at `:2001` | **flag-gated (PD-18 / T-1.6)**: standalone-fenced `stale_reset` jobs flag-on; walks flag-off | the gate is load-bearing for criterion 3 — a materialization-Completed node whose outputs are GC'd re-merges through exactly this lane (fresh flag-on work) |

No seventh site exists (`grep -n "spawn_substitute_fetches\|to_spawn.push"
rio-scheduler/src/actor/*.rs`, audited at this commit).

**The runtime pin:** `flag_on_fresh_work_never_walks`
(actor/tests/materialize.rs) — drives all five creation origins
(new_sub, pruned, probe-partition, reprobe, stale_reset) plus Success/
InfraFailure consumption arms from a clean flag-on DB and asserts
`rio_scheduler_substitute_spawned_total == 0`, zero `QueryPathInfo` calls
(the walk's fetch primitive), and one job per driven origin; then forces
flag-off-era marks on a fresh node (the debug forcers) and asserts the D16
settlement cell DOES walk it (metric 0 → 1) — the legacy-state arm is
sanctioned absorption AND the proof that the metric capture is non-vacuous.

**Mechanism-selection summary (which lane selects which mechanism, by flag
state and node history):**

| Lane | Flag-off | Flag-on, fresh node | Flag-on, legacy-state node (marks/tried) |
|---|---|---|---|
| merge new_sub / prune / reprobe / stale-verify | walk | job (in-tx or standalone fenced per the T-5.2 table) | n/a (these lanes classify fresh probe answers) |
| dispatch probe partition | walk | job | D16/settlement cells → walk (absorption) |
| walk-consumption re-spawn / marked-root settlement | walk | unreachable | walk (absorption) |

T-1.7's clear-mirror narrows the legacy-mark population over time (resolved
jobs clear their nodes' marks), but reaped/never-resolved legacy state can
persist — the absorption arms stay documented, not deleted (deletion is D′).

---

## Phase B deployment checklist (Wave 6, T-6.3)

The design §8-B exit-gate re-verification of the closure-evidence deployment
rows (CE-D1..D9 — the predecessor campaign's operator handoff, recorded in
`closure-evidence-invariant-map.md`; that record is history and is NOT
edited — it carries a pointer to this section), plus the NEW
materialization-specific rows (MD-D). The gateway deployment rows
(GW-D1..D3, `gw-session-invariant-map.md`) are untouched by this campaign:
Phase B makes zero gateway production changes (PD-B15), so their premises
are unchanged.

### CE-D re-verification (flag-on)

| Row | As-built meaning | Phase B flag-on disposition | Evidence |
|---|---|---|---|
| CE-D1 (fenced-write metric + leader alert) | evidence-write fence refusals: failover bursts are the fence working; sustained nonzero on the leader = floor regression | **Re-verified + extended.** Materialization job-table and wanted-relation writes carry the SAME claims-floor fence and feed the SAME counter (`rio_scheduler_evidence_write_fenced_total`). The recommended alert now SHIPS in the chart: `RioSchedulerEvidenceWriteFenced` (sum(rate[5m]) > 0 for 15m, critical) — the temporal form discriminates failover bursts (self-clear in seconds) from leader fencing. (The Phase B plan named this `RioSchedulerMaterializationFenced`; the shipped name is generic because the counter is shared — a materialization-only name on a shared counter would misdirect operators. Deviation recorded for plan review.) | Phase A fenced db batteries; T-6.2's alert (commit `034e91c1a`); helm-lint alert-quality fragment |
| CE-D2 (failover PG-flap alerts) | generation claim/floor read failures at failover | **Unchanged.** Same metrics; they now also guard the fencing of job writes (same floor reads). | T-3.3 failover scenario green (jobs survive, ids identical) |
| CE-D3 (merge `FAILED_PRECONDITION` during failover) | stale-tenure merge refusal | **Unchanged.** Job creation and the wanted relation ride the merge transaction (in-tx batch), so the same refusal covers them — a fenced merge creates no jobs (B6). | T-3.3 + the Phase A in-tx tests (`job_create_in_rolled_back_tx_leaves_no_row`) |
| CE-D4 (wrongful-fail-fast gone; resubmit guidance) | every fail-fast decision point re-probes first | **Re-verified flag-on against §2.4.** The single surviving flag-on fail-fast site (consumption settlement, arm 3) re-probes the live wanted set in-transaction AND discriminates on the topdown-pruned mark (finding 11) — only marked roots fail-fast, with the same resubmit-directing error wrapper. Resubmit guidance unchanged: a resubmitted build creates a NEW job with a fresh re-probe one-shot. | T-3.1 routing-fail-fast subtest (+ its `-walk` oracle twin: same verdict both mechanisms); T-4.4 routing matrix |
| CE-D5 (poison-clear wakes spared parents) | survivor re-evaluation at clear/sweep | **Re-verified flag-on.** Survivors carrying unresolved jobs stay armed without any walk (T-4.3's reap gate); the reprobe lane creates `reprobe`-origin jobs WITH the AS-5 reset + `poison_cleared` row instead of walks (T-1.5); the promotion arm itself is untouched. | T-4.3 + T-1.5's reprobe tests + the T-5.3 origin sweep |
| CE-D6 (manual-target runbook: the 7 closureEvidence conjunctions) | run before evidence-handling changes | **Deferred to C′** (design §9.3: re-run as part of the C′ go/no-go). Phase B touches no closure-evidence check; the conjunctions' premises (the as-built walk arms) are intact flag-off. Recorded as a C′ entry criterion in the Wave 7 handoff. | Wave 7 handoff |
| CE-D7 (AW1 lost-hole-stamp ∩ builds-row-purge bound) | Phase-0 residual | **Unchanged** (closes at D′ with the walk deletion). | — |
| CE-D8 (GC-after-vouch bounds) | Phase-0 residual; pin-at-vouch deferred to this campaign | **Evidence delivered, disposition stays D′'s.** Pin-at-ingest (§5.1) + the §5.3 release lifecycle close the GC-after-vouch window for materialized paths (B2-strong over ingest → all-interest-terminal); the CE-D8 row itself is retired only when D′ deletes the walk (whose vouch path keeps the as-built bound). | T-3.1 gc-pin subtest; T-1.8's three-site release wiring |
| CE-D9 (D10 expired-at-load poison residual) | Phase-0 residual | **Unchanged.** | — |

### MD-D — materialization deployment rows (NEW)

| ID | Item | What to do / what it means | Evidence |
|---|---|---|---|
| MD-D1 | **Park alerting + runbook** | A parked materialization job means UPSTREAM trouble, never build failure: builds wait visibly (`rio_scheduler_materialization_stalled` > 0; the `RioSchedulerMaterializationStalled` alert at 15 m). Every parked job has Broken closure evidence (no from-source fallback) — jobs with buildable closures are auto-resolved from-source by the housekeeping re-evaluation arm within one tick. Runbook: check the tenant's upstream cache config/health (`rio_store_substitute_total{result="error"}` rate, store logs); builds resume on upstream recovery (park-expiry re-claim) or can be cancelled. Do NOT restart the scheduler to "fix" a park — the park is durable state and survives restarts by design. | T-6.1 (re-evaluation arm + gauge), T-6.2 (alert), vm-materialization-standalone infra-park subtest |
| MD-D2 | **Flag rollback procedure (ON → OFF)** | Set both helm values false (`scheduler.materialization.enabled`, `store.materialization.enabled`) and roll. The walk serves all new work immediately. Flag-on-era state drains, never strands: pending job rows are inert (nothing claims them); claimed jobs' reports drain through the always-on consumption transaction; flag-on-era marks were cleared at resolution (the §4 clear-mirror), so no wrongful fail-fast can fire on stale marks; flag-on-era pins release through the always-on §5.3 wiring once jobs resolve and interest goes terminal. The chart's AND-guard makes the hazardous persistent state (scheduler on / store off) unrenderable in either direction. | vm-materialization-transition-k3s (both directions, 8/8 subtests incl. flip-off marks + pins); T-1.7 revert-with-state test; T-1.8 always-on release tests |
| MD-D3 | **Mixed-flag window guidance** | During a rollout where the scheduler is flag-on before the store (the transient AS-6 race the AND-guard cannot eliminate): jobs are created but not claimed — a visible wait (`substituting_derivations` > 0 via rio-cli status, builds Active), bounded by the store rollout completing, never a strand (the store's first poll drains the backlog). The reverse window (store on / scheduler off) is a no-op: the store polls empty lists harmlessly. | T-3.3 mixed-flag subtest (45 s observation window, drains on store rollout) |
| MD-D4 | **Mixed-version (rolling upgrade) posture of the instance-bound credential** | The T-5.1 instance binding is FAIL-CLOSED across version skew: a pre-Phase-B scheduler that receives an instance-bound store token REJECTS it (`deny_unknown_fields` — `unknown field 'instance'`), so no mixed-version window exists in which an instance-bound token is authenticated while the binding check is skipped. The failure mode of that (unsupported, unreachable pre-D′) skew direction is store claim retries against the rolling scheduler — self-healing within the rollout window — never unenforced binding and never a wrongful build outcome. No deployment-ordering step is required for security: the store's instance-bound minting is flag-gated and the flag rolls atomically with the pod template (a pod is either old-binary+flag-off or new-binary+flag-on, never a cross). | `service_claims_instance_forward_skew` (the fail-closed pin); T-5.1 wire-compat battery; PD-B8/PDB-8 adjudication record |

---

## Phase B stage record (flag-on cutover; landed 2026-06-01)

### Identity

- Branch: `a4-phaseB`, **31 commits** ahead of baseline `21955a450` (the
  post-Phase-A integration tip + the ReportMaterializationProgress auth fix):
  26 plan task commits (two of which also carry bug fixes B1/B5) + 3
  standalone product-bug-fix commits (B2/B3/B4) + 1 flake-hardening commit +
  this record. All signed, all crate-scoped semantic subjects
  (`/tmp/rio-dev/phaseB-commits.txt`).
- **The cutover posture (PD-B1):** helm `values.yaml` defaults
  `materialization.enabled: true` for BOTH components, with the AS-6
  AND-guard (scheduler env renders `scheduler.enabled ∧ store.enabled`);
  every VM fixture flag-on by default; **Rust struct defaults stay `false`**
  in both crates (the unit battery's regression posture). Rollback is a
  values flip (MD-D2).
- Diff vs baseline: 67 files, +11,473/−624 (net +10,849).
- Full CI gate (`.claude/bin/nixbuild --checks`) green at this tip with the
  flipped defaults (Wave 7; the gate record below).

### What landed (the deliverables table, with commits)

| # | Deliverable (plan #) | Commits |
|---|---|---|
| 1 | §2.6 consumer re-sourcing flag-on: substituting bucket ← pending unclaimed jobs, queued-bucket exclusion, build_summary union, scaler signal (1) | `435987dad` |
| 2 | BC-4 gateway events flag-on: SUBSTITUTING at claim intake, terminal stop at consumption, byte-progress relay (2) | `ef706db31` |
| 3 | PD-6 Queued materialization claims: kinded `Queued→Assigned` mint edge + 144-case exhaustive + the 3 pre-authorized pin flips (3) | `8d5caee2d` |
| 4 | PD-7 GetSpawnIntents unresolved-job filter (4) | `bbb607447` |
| 5 | PD-17 reprobe-lane job creation (origin=`reprobe`, in-tx, AS-5 6d reset + poison_cleared) (5) | `1fc41ff71` |
| 6 | PD-18 stale-Completed-verify creation (origin=`stale_reset`, standalone-fenced) (6) | `09bc947ba` |
| 7 | The §4 consumption-transaction clear-mirror (T-1.7/PD-B16) + the Phase A admission-gap fix (bug B1) (21) | `7c9b9f949` |
| 8 | The §5.3 pin-release wiring, ALWAYS-ON, three sites (T-1.8/PD-B17) (22) | `97f2d4553` |
| 9 | Materialization-surface auth sweep (T-1.9/PD-B18: 4 surfaces × {no token, executor token}) (23) | `bee3c1155` |
| 10 | `vm-substitute-standalone{,-walk}`: parametrized both-state + dormant→`materialization-active` inversion + the PD-B21 store end-state blocks + `substitute-scheduler-owned` (the PD-B2 basic-path proof) (8, 9) | `ee03bf1a9` |
| 11 | `vm-substitute-scale-k3s{,-walk}`: both-state + the CF-1 deep-chain re-key (jobs_created ≥ 45 + walk-metric == 0) (9) | `a04119f79` |
| 12 | THE FLIP: helm defaults + AND-guard + VM fixtures + lifecycle `materialization-boundary` inversion + helm-lint guard fragment (7, 8) | `03b27b109` |
| 13 | Config-docs cutover posture record (BLESS + regen) (7) | `fcf4b518e` |
| 14 | `vm-materialization-standalone{,-walk}`: routing arms + park + gc-pin + no-walks guard, with the flag-off walk oracle (OQ7 sequences 2–4) (10) | `e043d5bcd` |
| 15 | `vm-materialization-failover{,-walk}-k3s`: failover + mixed-flag + store-only-noop, with the walk failover oracle (OQ7 sequence 5) (12) | `4282476e9` |
| 16 | `vm-materialization-transition-k3s`: FP-4 both directions + marks + pins (8/8 subtests) (11) | `ccd62c48f` |
| 17 | C3/D16/L3 flag-on equivalents: two-build dedup trace, settlement totality, recovery job-view rebuild + reap gate, routing matrix (13, 14) | `ec1874d5f`, `64639ba50`, `4e57180fd`, `aa0efe231` |
| 18 | Obligation 1: `executor_instance` bound INTO ServiceClaims (fail-closed skew, three legs pinned) (15) | `520fc26fb` |
| 19 | Obligation 2: PDQ-9 posture revisit + cross-site dedup pin (16) | `5bf827475` |
| 20 | OQ7 close-out: the six-site zero-walk audit + `flag_on_fresh_work_never_walks` (17) | `69ee2263f` |
| 21 | PD-20: stalled gauge + park re-evaluation arm (18) | `642fe9fb7` |
| 22 | Lifecycle metrics + alerts + dashboards (18) | `034e91c1a` |
| 23 | Deployment checklist (CE-D re-verification + MD-D rows) (19) | `223e05c27` |
| — | Bug fixes B2/B3/B4 (the harvest below) | `c952e5a51`, `ce17c6445`, `056bfc9b6` |
| — | infra-park flake hardening (multi-charge claim waves) | `01e405c6c` |
| — | This stage record (20) | this commit |

### Wave-by-wave commit record

| Wave | Commits (in order) |
|---|---|
| 1 — flag-on mechanisms + missing mechanisms + auth audit | `435987dad`, `ef706db31`, `8d5caee2d`, `bbb607447`, `1fc41ff71`, `09bc947ba`, `7c9b9f949`*, `97f2d4553`, `bee3c1155` |
| 2 — both-state adaptation, then the flip | `ee03bf1a9`, `a04119f79`, `03b27b109`, `fcf4b518e` |
| 3 — new-path VM scenarios | `e043d5bcd`, `4282476e9` (T-3.2 deferred to Wave 4 behind finding 18) |
| 4 — findings resolution + unit/actor battery | `01e405c6c`, `c952e5a51`*, `ce17c6445`*, `ec1874d5f`, `64639ba50`, `4e57180fd`*, `aa0efe231`, `056bfc9b6`*, `ccd62c48f` |
| 5 — obligations | `520fc26fb`, `5bf827475`, `69ee2263f` |
| 6 — observability + checklist | `642fe9fb7`, `034e91c1a`, `223e05c27` |
| 7 — exit gate + this record | this commit |

\* carries a product-bug fix (the harvest below).

### Equivalence evidence (criteria 1–6, the named artifacts)

| # | Criterion | Verdict | Artifact |
|---|---|---|---|
| 1 | Outcome equivalence (OQ7), all five sequences, both runs | **HOLDS** | Cache-hit: `vm-substitute-standalone` vs `-walk` (~61 s each) and `vm-substitute-scale-k3s` vs `-walk` (163–177 s) — same verdicts, statuses, store end-state; the §8-B pair-rendering assertion in progress-e2e, `substituting_derivations > 0` in the scale sub_peak poll (PD-B2 split). Unobtainable-from-source / unobtainable-fail-fast / infra-retry: `vm-materialization-standalone` (21.5 s / 13.3 s / 38.4 s) vs `-walk` (37.3 s / 72.4 s / 35.8 s) — same outcome triple per pair (verdict + error class, final statuses, store rows). Failover: `vm-materialization-failover-k3s` (15.4 s) vs `-walk` (14.8 s) — succeeded, 10 paths, no stuck nodes, both mechanisms. Store end-state: IDENTICAL psql/NAR assertion blocks in both branches of every parametrized scenario (PD-B21). The one authoring-time divergence found (unmarked-childless fail-fast) was a real product bug — fixed as B2, not papered over. |
| 2 | Flag-off invariance (revertability) | **HOLDS** | T-4.5 byte-identity audit: ZERO deleted/modified lines in `tests/{dispatch,merge,build,recovery}.rs` vs baseline; tree-wide ZERO deleted test functions (Wave 7 grep); the only existing-test edits are the three pre-authorized PD-6 pin flips, the B2 routing-core conjunct extension (orchestrator-sanctioned), and the three T-5.1 mint updates — each enumerated in its commit body. The `-walk` oracle attrs carry the byte-original Phase A assertions (incl. the relocated five-table dormancy zero-count) and are GREEN at the final tree (Wave 7 matrix). Walk machinery production code: `dispatch.rs` +21/−0 (the T-4.3 reap gate, flag-gated); `spawn_substitute_fetches`/`handle_substitute_complete`/`walk_substitute_closure`/`settle_broken_marked_root` untouched. As-built `admit_pull` + battery + 5 harnesses: byte-identical (all pull.rs deltas are in the Phase A kinded-wrapper region, line ≥715). |
| 3 | Walk unreachability for fresh flag-on work (PD-B19 scope) | **HOLDS** | The T-5.3 six-site audit table (above) — merge lanes closed by T-1.5/T-1.6, dispatch lanes reachable only via flag-off-era state; `flag_on_fresh_work_never_walks` (all 5 origins + consumption arms → `substitute_spawned_total == 0`, zero `QueryPathInfo`; the legacy-state arm walks → metric capture non-vacuous, sanctioned absorption); deployment level: `materialize-no-walks` guard, progress-e2e `spawned_total == 0`, deep-chain walk-metric delta == 0 with `jobs_created` delta ≥ 45. |
| 4 | Settlement totality flag-on | **HOLDS** | `flag_on_every_job_state_has_armed_action` (T-4.2 + the T-6.1 5b Vouched-parked arm): pending⇒claimable, claimed⇒report-or-establishment, parked⇒re-evaluation+backoff expiry, zero-interest⇒cancellation; the D16 limbo cell (marks+tried+refusing probe) is unconstructible flag-on; T-4.3: the armed action survives failover (view rebuild). |
| 5 | Wire and schema freeze | **HOLDS** | `git diff 4ef5be222 -- rio-proto/` → empty (0 lines); `ls rio-migrations/migrations/ | tail -1` → `079_materialization_outcome_classes.sql` (zero new migrations; `migrations/` diff vs baseline = 0 lines); zero config-schema deletions (T-2.4 is description-only + fixture bless). |
| 6 | Formal-check invariance | **HOLDS** | `retryPolicy.qnt`, `materializationJob.qnt`, `spawnCoherence.qnt`, `nix/kani.nix`: ZERO diff vs baseline. `nix/quint.nix`: the only delta is `c952e5a51`'s tracey marker re-point (`r[verify sched.materialize.routing]` → `+2`), a comment line in the MATERIALIZATION-regime check's wiring — zero check-logic change; the build-only `quint-retry-policy-pull` definition is byte-untouched, so its derivation is the same one green at the Phase A tip (14 invariants, 39,711,022 distinct states — bit-identity by drv identity). Coexistence regime + witness green in the Wave 7 gate; kani 17+8 harnesses green (re-built — the kinded wrapper changed — and re-verified at T-1.3, T-1.7, and the gate). |

**The auth-surface record (PD-B18):** all four materialization RPC surfaces
(kind=MATERIALIZATION PullAssignment, materialization ReportOutcome,
ListMaterializationJobs, ReportMaterializationProgress) carry the
store-service credential gate; the T-1.9 table-driven sweep (every surface ×
{no token, executor token} → rejected; dev-mode open) is a permanent test;
T-5.1 narrowed the WORK surfaces further to instance-bound credentials
(progress stays fleet-level, display-only). The motivating class:
`21955a450` (the third Phase A RPC that shipped ungated). No fifth surface
exists at this tip.

### The bug harvest (five product bugs + the finding-11 design correction)

Every fix red-first, with the failing transcript in the commit body; zero
walk-machinery changes.

| # | Bug (where it lived) | Found by | Fix | Spec/design impact |
|---|---|---|---|---|
| B1 | **Phase A admission gap**: the kinded wrapper's materialization Pending arm ran the as-built A11 `must_substitute` refusal, which also refused MATERIALIZATION claims of topdown-pruned roots — parking exactly the jobs that most need claiming, forever | T-1.7's pruned-origin red fixture | `7c9b9f949` — Ready/Queued + must_substitute upgrades to DeliverNew for unparked pending jobs (the claim IS the substitution); build pulls of marked nodes stay refused | none (kernel arm; kani 17/17 unchanged) |
| B2 | **Arm-3 routing fail-fasted unmarked nodes** (finding 11, the stop-condition-2 candidate): flag-on, ANY Broken-evidence node with a confirmed-missing wanted output fail-fasted — including genuine leaves the walk builds from source. Reachable client-visible divergence (probe-blip + 404 + childless ⇒ flag-on FAILS, flag-off builds) | T-3.1 scenario authoring (the `-walk` oracle disagreed) | `c952e5a51` — the topdown-pruned mark discriminator: FailFast requires the four-conjunct corner (marked ∧ Broken ∧ confirmed-missing-or-spent); unmarked → ResolveFromSource (the walk-equivalent disposition); 6-row behavior table in the commit | **THE design correction**: `sched.materialize.routing+2` (22 annotation sites re-pointed in-commit). Implemented per the orchestrator ruling — **flagged for owner counter-signature** |
| B3 | **Executor transport pinned to a standby** (finding 18 gap 1): the store's lazy ClusterIP channel pins per kube-proxy connection; after a scheduler rollout it can pin to the STANDBY, whose UNAVAILABLE answers never break the connection — claims/reports dead-end forever (deployment-level settlement-totality hole) | T-3.2 transition scenario (5-iteration diagnosis; the failover scenario masked it — one pod dying re-dials naturally) | `ce17c6445` — abandon-and-redial on UNAVAILABLE | `store.materialize.executor+2` |
| B4 | **Wanted relation absent for flag-off-era builds** (finding 18 gap 2): builds submitted flag-off have no `build_wanted_outputs` rows; post-flip jobs for their nodes resolve no tenant/wanted (§6 joins empty) → instant InfraFailure("no tenant context") — the OFF→ON transition strands | T-3.2 flip-on direction | `056bfc9b6` — the probe-partition creation backfills the relation for every live interested build | none (the §6 join contract honored at every creation site) |
| B5 | **Job-view armament-state defects at recovery** (two in one commit): (a) the dispatch-probe dedup re-feed OVERWROTE existing view entries with `parked_until = None` — one probe pass wiped a parked job's in-memory park state (premature re-claim eligibility); (b) the reap hook routed marked-Broken survivors CARRYING an unresolved job through the walk settlement — spending the verification one-shot and spawning a flag-on walk for job-armed nodes (a criterion-3 cell) | T-4.3 red-first | `4e57180fd` — `entry().or_insert()` preserves armament state; the reap hook gates on job presence (survivors with an unresolved job need nothing — the job is armed) | none |

**Finding-11 ruling status:** the divergence was reported as a
stop-condition-2 candidate (notes finding 11); the orchestrator ruled the
mark discriminator IS the §2.4 intent (the stricter flag-on behavior was a
plan-text artifact, not a design decision); implemented + spec-bumped in
`c952e5a51`; T-3.1 re-verified green in both branches post-fix. The ruling
record rides this map **pending owner counter-signature** (the design
§10/§2.4 text is owner-controlled).

### The findings/deviations ledger (all 21, with dispositions)

Full prose in `/tmp/rio-dev/subst-phaseB-notes.md`; this table is the
durable record.

| # | Finding / deviation | Disposition |
|---|---|---|
| 1 | `gw.activity.subst-progress` NOT bumped (T-1.2): normative text is emitter-agnostic; a bump would conflict with PD-B15 zero-gateway-changes | Deviation recorded for plan review; events wire-identical |
| 2 | Phase A admission gap (B1) | Fixed `7c9b9f949` |
| 3 | T-1.3 crash test re-scoped to Wave-1-honest assertions (post-recovery re-claim needs the T-4.3 view rebuild) | Closed by `4e57180fd` (T-4.3) |
| 4 | progress-e2e flag-on creates 1 `pruned` + 4 `cache_opportunity` jobs (5, not the plan's 4) — the prune fires for the substitutable root, the exact 1:1 twin of the flag-off pruned-root walk | Enumeration-A row corrected (mechanical gap, triage path 2) |
| 5 | The as-built substitute scenario never reaches the scheduler's substitution machinery for multi-node work (gateway pre-substitutes via wopQueryMissing) | `substitute-scheduler-owned` subtest added, both branches — the PD-B2 `vm-materialization-basic` deployment proof |
| 6 | Both-state realization is Python-level `MATERIALIZATION_ENABLED` branching, not nix `optionalString` (nixfmt re-indents nested splices out of subtest scope) | Realization deviation; both texts live in the file, the parameter selects |
| 7 | `rio_scheduler_materialization_jobs_created_total` carries an `origin` label — bare-name lookups miss it | Scenarios sum across labeled series |
| 8 | T-2.2's first commit had a tracey ImplInTestFile (prose `r[...]` in a comment) | Amended in place pre-merge (`a04119f79`); boundary stayed green |
| 9 | PD-B4 AND-guard/default/rollback rendering proofs made a permanent helm-lint fragment (`26-materialization-and-guard.sh`) | Hardening beyond plan; keeps the guard tested forever |
| 10 | `check_roots_topdown` forwards ONLY the client JWT — gRPC-direct submissions can never fire the prune without a tenant token | All Wave 3+ scenarios mint tenant JWTs and attach to SubmitBuild |
| 11 | OQ7 divergence, unmarked-childless shape (B2) | STOP-AND-REPORT raised → orchestrator ruling → fixed `c952e5a51`; counter-signature pending |
| 12 | Determinism mechanism: store `poll_interval_secs=3600` + restart-per-wave = exactly one claim wave per restart (also clears the 1 h HEAD-probe cache) | Adopted by all standalone materialization scenarios |
| 13 | ARCHITECTURAL: `recover_from_pg` runs only on LeaderAcquired (k8s lease deployments); a standalone scheduler restart starts empty — ANY in-flight build is orphaned, flag flip or not | T-3.2 rewritten as `vm-materialization-transition-k3s` (the FP-4 story is k8s-only by construction); pre-existing as-built behavior, documented, NOT changed |
| 14 | FP-4(a) establishment-drain not VM-testable (the establishment window anchors to SLA deadlines) | §4 inertness covered by the PARKED-jobs form (pending rows inert across the flip); establishment-drain stays unit-level (Phase A battery) |
| 15 | Schema lessons: `drv_attempts` has no kind column (kind implied by outcome class); `drv_executions` keys on the executor-facing CHAR(32) hash | Baked into scenario assertions (deployment-wide zero-build-executions form) |
| 16 | Mixed-flag window: pending-job backlog visible via the re-sourced `substituting_derivations` (≥2 across the 45 s window), drains on store rollout | Asserted in T-3.3 mixed-flag; MD-D3 guidance |
| 17 | Post-failover completion initially via the dispatch-probe dedup re-feed (~1 tick lazy heal) | Closed by T-4.3's eager rebuild; failover scenario re-verified |
| 18 | STOP-AND-REPORT: transition claim stall after OFF→ON flip | Diagnosed over 5 iterations → B3 + B4 + two scenario races fixed (claim-wave/heal race via finding 12; orphan-watcher re-attach); `ccd62c48f` green 8/8 both directions |
| 19 | Alert named `RioSchedulerEvidenceWriteFenced` (generic), not the plan's `RioSchedulerMaterializationFenced` — the fence counter is shared across all evidence writes | Honest-name deviation recorded for plan review |
| 20 | T-5.2 audit corrects the plan's posture table: the stale-verify site is standalone-fenced (post-tx, reconcile 6c), NOT in-tx batch 5 — split is 3 in-tx + 2 standalone-fenced | PD-B9 verdict (split kept) unchanged; table above is authoritative |
| 21 | Park re-evaluation resolves Vouched AND Pending evidence from-source; only Broken-evidence jobs ever appear in the stalled gauge | Makes the alert's "no from-source fallback" description exact (MD-D1) |
| — | Wave 7: `vm-materialization-standalone-walk` `walk-infra-retry` timed out (180 s) under ~30-VM builder contention; green at Wave 3 in 35.8 s | Triage path 1: solo discriminating re-run GREEN (38.6 s subtest, 172 s attr) — contention flake, recorded, criterion 2 intact |

### Test accounting (baseline `21955a450` → Phase B tip)

Wave 7 full-workspace battery: **3442 tests run, 3442 passed, 28 skipped,
zero failures** (124.2 s). Tree-wide added-test-function count (+43) equals
the run-count delta exactly; **zero test functions deleted** (diff grep).

| Battery | Baseline | Phase B tip | Delta |
|---|---|---|---|
| rio-scheduler | 1146 + 1 skipped | 1182 + 1 skipped | +36 |
| rio-store | 537 + 13 skipped | 541 + 13 skipped | +4 |
| rio-auth | 38 | 41 | +3 (instance binding + skew legs) |
| rio-evidence-kernel / rio-retry-kernel / rio-proto / rio-migrations | 17 / 14 / 56 / 11 | unchanged | 0 (kernel deltas live in the existing kinded battery files) |
| **Workspace** | **3399 + 28 skipped** | **3442 + 28 skipped** | **+43** |
| CBMC harnesses | 17 + 8 | 17 + 8 | 0 (kinded-arm changes re-verified under the existing harnesses) |
| Wired quint checks | 12 | 12 | 0 |
| VM check attrs | 30 | **37** | +7 (2 substitution `-walk` oracles + materialization-standalone{,-walk} + transition + failover{,-walk}) |
| codecov `after_n_builds` | 44 | **51** | +7 (= 17 unit + 34 vm coverage entries; gen-matrix verified) |

### The VM matrix at the exit (Wave 7, final tree)

All 37 attrs green (`.#ci.vm-test` aggregate exit 0). 30 pre-Phase-B attrs
run flag-ON — 27 with byte-zero assertion changes (their green-ness is the
criterion 1 zero-delta statement) + lifecycle-core (boundary fragment) + the
2 adapted substitution scenarios; 4 `-walk` attrs run flag-OFF with the
as-built oracle assertions; `vm-materialization-transition-k3s` exercises
both flips in one run (8/8 subtests, ~191 s green run). One contention
flake this wave (ledger row above), green on the discriminating re-run.

### The Wave 7 exit gate

`.claude/bin/nixbuild --checks` (nix-fast-build over the full
`checks.x86_64-linux` — **380 attrs**: per-member clippy/clippy-test/doc/
nextest for every workspace crate, all 37 VM attrs, the quint family
(build-only + coexistence regime + 11 witnesses), 4 kani check families
(17+8 harness counts among them), cov-smoke, fuzz, pre-commit,
tracey-validate, helm-lint incl. the AND-guard fragment, docs-lint, drift/
policy checks): **OK, exit 0**, zero red checks, zero failure events on the
streamed log (`/tmp/rio-dev/rio-a4-phaseB-92.log`).

Wall-clock, stated honestly against stop condition 7(a): the `--checks`
invocation itself ran **542 s** because the 37-attr VM matrix was built
earlier in the same wave (T-7.1) at the same tree and served as cache hits;
the combined Wave 7 verification wall — matrix run (513 s) + the flake's
discriminating solo re-run (172 s) + aggregate certification (684 s) + the
gate (542 s) — is **≈ 32 min**, inside the plan's expected band (~50–65
min) and far under the ~70 min ceiling. The T-1.9 auth sweep and the full
flag-off battery ride `nextest-rio-scheduler`/`nextest-rio-store` inside
the gate; the workspace dev-shell battery (3442/3442) ran separately at
T-7.1.

### Plan-decision log final state (PD-B1..PD-B21)

All 21 entries **exercised as written**; none struck. Entries with
in-execution detail beyond the plan text: PD-B2 (extended by finding 5 —
`substitute-scheduler-owned` realizes the basic-path deployment proof),
PD-B3 (finding 6's Python-level branching realization), PD-B4 (finding 9's
permanent helm fragment), PD-B6 (stop sites build.rs:441/:521 per CF-4),
PD-B9 (finding 20's posture correction — split kept, table corrected),
PD-B19 (the legacy-state absorption arm exercised non-vacuously by T-5.3).
In-execution deviations needing plan-review ratification: findings 1, 6, 19
(all recorded above); the T-3.2 standalone→k3s rewrite (finding 13 — the
plan's standalone+systemd form was unimplementable against the as-built
recovery trigger).

### What Phase B does NOT claim

- **No soak/production evidence** (house no-soak rule; deployment is
  post-D′ operations — MD-D rows are the operator handoff).
- **No model verdicts**: `materializationJob.qnt` remains a draft, NOT
  wired; nothing is model-checked beyond the Phase A
  `retryPolicyPullMat` regime. C′ owns the model and the §9.3 transfer.
- **The walk machinery is intact** and serves flag-off deployments
  bit-identically; the §4 dual-write stamps are still WRITTEN flag-on
  (removal is D′.1); the evidence-bit absorption arms stay documented, not
  deleted.
- **CE-D6's seven-conjunction manual re-run** has not been executed (C′
  go/no-go scope).
- **The §9.3 calibration transfer has not been executed** — the protocol
  below is the handoff, not a result.
- **The finding-11 ruling awaits owner counter-signature** (the harvest
  table); if the owner overrules, B2's discriminator and
  `sched.materialize.routing+2` re-open.

---

## Phase C′ entry criteria and handoff

### Entry criteria

1. This record integrated into `formal-sprint` behind a green full gate.
2. The finding-11 owner counter-signature obtained (or the overrule
   processed) — the model's arm-3 encoding depends on it.
3. The C′ plan absorbs the model-completion deltas below (the as-built
   system moved during Phase B; the draft did not).

### The model-completion delta list (what `materializationJob.qnt` must encode to match the AS-BUILT system)

The draft (1,221 lines, 20 actions, 17 properties, 5 regimes — Phase A
T-5.3, deliberately unwired) describes the Phase A design. Phase B changed
the system in six places the draft predates. **Wiring the draft without
encoding these deltas would verify the wrong system.**

| Δ | As-built behavior (Phase B) | What the draft has | What C′ must encode |
|---|---|---|---|
| 1 | **The finding-11 mark discriminator** (`c952e5a51`, `sched.materialize.routing+2`): arm-3 FailFast requires the four-conjunct corner (Broken ∧ `topdown_pruned` mark ∧ confirmed-missing-or-spent-one-shot); unmarked nodes → ResolveFromSource. Plus the §4 clear-mirror (`7c9b9f949`): the mark CLEARS at `resolved_success`/`resolved_from_source` | No mark state at all; `consumeUnobtainable`'s `arm3fail` fail-fasts ANY node on confirmed-missing/spent; `routingRequiresDurableVouchOrFailFast` forbids the unmarked from-source disposition (it would VIOLATE against the as-built system) | A per-drv `topdownPruned` ghost: set by `createJob(OPruned)`, cleared on both resolution arms (the clear-mirror); arm selection split marked/unmarked per the 6-row B2 table; `lastFailFastJustified` gains the mark conjunct; the property re-encoded to admit the unmarked arm-3 from-source; a pruned-origin trace keeps `noFailFast` non-vacuous |
| 2a | **Finding-18 transport** (`ce17c6445`, `store.materialize.executor+2`): the executor abandons-and-redials its scheduler channel on UNAVAILABLE (standby-pinned ClusterIP after rollouts); without it, claims/reports dead-end forever — a deployment-level settlement-totality hole | `claimJob`/report actions are atomic direct actions against THE scheduler; no channel/leader-target state exists | Either a per-replica channel-target state (leader/standby; standby-targeted claim/report actions blocked until a redial action) with `unresolvedJobAlwaysArmed` re-proven under it, or an explicit recorded abstraction ("transport liveness assumed") citing the two transport unit pins + the transition VM scenario — C′ adjudicates which; the as-built behavior is the redial |
| 2b | **Finding-18 wanted backfill** (`056bfc9b6`): flag-off-era builds have NO `build_wanted_outputs` rows; the probe-partition creation BACKFILLS the relation for every live interested build (else: InfraFailure "no tenant context") | `createJob` precondition `liveWanted(d) != Set()` makes the no-wanted-rows state unrepresentable — the bug class cannot even be expressed | Model the transition-window build (interest without wanted rows) + creation-establishes-wanted at the probe origin; the new invariant: every unresolved job has ≥1 live interested build with wanted rows (tenant-resolvability) |
| 3 | **The park re-evaluation split** (`642fe9fb7`, T-6.1/PD-20): per-tick re-evaluation resolves Vouched AND Pending evidence from-source; **Broken-evidence jobs STAY parked** until the durable backoff lapses (exactly the stalled-gauge/alert population); backoff-expiry re-claim is a separate arm | `housekeepingReevaluatePark` conflates both arms: Vouched/Pending → from-source, ELSE unconditional unpark + `matInfraCount` reset | Two actions: evidence re-evaluation (Vouched/Pending → `JResolvedFromSource`; Broken → no-op, stays parked) and backoff expiry (unpark Broken-evidence jobs for re-claim); decide the budget-reset semantics against the as-built park cycle; optionally a stalled ghost = parked ∧ Broken (the alert population) |
| 4 | **Five origins, split posture** (T-1.5/T-1.6/T-5.2, finding 20): `cache_opportunity` (merge new_sub IN-tx + dispatch probe standalone-fenced), `pruned` (in-tx), `reprobe` (in-tx, AS-5 6d reset + `poison_cleared`), `stale_reset` (standalone-fenced POST-tx, resets FROM Completed) | Four origins (`OProbe`/`OMerge`/`OPruned`/`OStaleReset`), no `OReprobe`; the AS-5 reset is attached to `OStaleReset`; `createJob` precondition `nodeStatus != NCompleted` BLOCKS the as-built stale-verify shape (a Completed node with vanished outputs is exactly what creates `stale_reset` jobs) | Add `OReprobe` (carries the AS-5 reset; `poison_cleared` ghost if poison enters the model); relax the NCompleted precondition for the `stale_reset` origin (reset-from-Completed in the same action); if C′ models merge-tx atomicity, group the three in-tx origins with the wanted-row writes (one fenced commit) vs the two standalone-fenced sites |
| 5 | **Recovery rebuilds the job view** (`4e57180fd`, T-4.3): post-failover, pending → DeliverNew immediately, claimed → DeliverExisting to the SAME holder / NotYetReady to others, parked → NotYetReady until the durable backoff lapses; the view feed preserves armament state (`or_insert`) | `failoverPreservesJobs = true` (placeholder); no in-memory-view state — failover is droppable-view by assumption, not by proof | The pre/post ghost form (jobs identical across `failover`); if C′ adds a view layer, the rebuild action + the or_insert-class property (a re-feed never weakens armament state: park survives, holder survives) |
| 6 | **Queued claims + the must_substitute upgrade** (`8d5caee2d` + `7c9b9f949`/B1): materialization claims legal from Ready AND Queued; `must_substitute` (the A11 mark refusal) upgrades to DeliverNew for unparked pending jobs — the claim IS the substitution | `claimJob` already admits NQueued|NReady (the draft anticipated PD-6) but has no must_substitute concept at all | NO structural change needed for the happy path (record the anticipation); if C′ imports the mark (Δ1), assert the marked-node claim is ENABLED (the B1 regression guard: a marked Ready/Queued node with an unparked pending job is claimable) |

Out-of-model surfaces (recorded, deliberately NOT deltas): BC-4 events,
§2.6 buckets, metrics (display-only — the draft's exclusion list);
the T-5.1 instance binding (auth layer; wire-level pins own it); the
chart-level AND-guard (deployment config, covered by helm-lint + the
mixed-flag scenario).

### The calibration-transfer protocol (§9.3, executable form)

1. **Which rows transfer:** the closure-evidence calibration families
   F1–F14 and C1–C5 (the model header's completion-contract item 3, the
   `closure-evidence-invariant-map.md` vocabulary) plus the
   executor-campaign rows that touch the pull protocol; target table = the
   §9.1 skeleton above (17 properties + 5 witnesses).
2. **Falsification first:** every transferred row must FALSIFY against a
   deliberately broken encoding (the planted-violation discipline) before
   its correct form is trusted — a transferred row that cannot falsify is
   recalibrated or rejected, never waved through.
3. **Then exhaustive:** every §9.1 property holds exhaustively (TLC) in
   every regime it names (base / failover / adversarialStore / staleTenure
   / crashLoop), with the six deltas above encoded; every property keeps an
   expect-violation witness (`mkQuintWitnessCheck` non-vacuity).
4. **The three draft-violating properties** re-encoded with at-decision-time
   ghosts (`routingRequiresDurableVouchOrFailFast` — restructured by Δ1
   anyway —, `noWrongfulTerminalFailure`, `noWrongfulFromSourceRouting`).
5. **The dormancy oracle holds throughout:** `quint-retry-policy-pull`
   (build-only regime) stays bit-identical through all C′ wiring (the
   regime-split rule from Phase A PD-21).
6. **Budget:** minutes-class per check (the 2-build × 2-drv space); the
   spawnCoherence job-filter conjunct (T-1.4's deferral) joins
   `spawnCoherence.qnt` in the same wave.

### The C′ go/no-go criteria (the design §8/§9.3 gate)

GO requires ALL of:

1. Every transferred calibration row falsifies (protocol step 2) and then
   holds (step 3) — the §9.3 row-by-row verdict table complete, one verdict
   per family.
2. Every NEW invariant (the §9.1 successor table incl. the three
   re-encodes and the Δ-derived properties) holds exhaustively in every
   named regime; witnesses all violate as expected.
3. **The model matches the as-built system per the six deltas above** —
   reviewed against `c952e5a51`, `ce17c6445`, `056bfc9b6`, `7c9b9f949`,
   `4e57180fd`, `642fe9fb7`, `1fc41ff71`, `09bc947ba` (the behavior-bearing
   commits), not against the Phase A draft or the original design text.
4. CE-D6's seven closureEvidence conjunctions re-run green (the deferred
   manual-target runbook).
5. `quint-retry-policy-pull` bit-identical; kani counts unchanged; the full
   CI gate green with the new checks wired in `nix/quint.nix`.
6. The finding-11 counter-signature resolved (entry criterion 2).

NO-GO (any of): a transferred row that holds without ever falsifying
(calibration failure); a §9.1 property that needs a system change to hold
(that is a D′-blocking design finding, not a model bug); the model
verifying a behavior the six-delta review shows the system does not have.

---

## Cross-references

- `closure-evidence-invariant-map.md` — the predecessor campaign's map; its
  Phase-2 handoff section cross-references this campaign as the successor
  owner of the substitution-related rows (see that map).
- `retry-invariant-map.md` — the "Cross-campaign addendum" section is the
  retry-side record of the kind partition (T-5.1/T-5.2).
- `executor-invariant-map.md` — the frozen pull-protocol contract
  (:2361–2536); Phase A's proto extension is a recorded addendum, never a
  mutation (criterion 4).
