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

**Status: Phase A complete (this map's stage record). Every mechanism the
design's §8-A scopes — plus the scheduler-side dormant wiring PD-19 records —
is landed, dormant, behind `materialization.enabled = false` on both the
scheduler and the store. The dormancy criteria (1–7) all hold with named
artifacts; the flag-on smoke battery proves the machinery composes end-to-end
in-process (dormant ≠ vestigial); the full CI gate is green at the Phase A
tip. Phase B owns activation; nothing in this phase changes deployed
behavior.**

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
| `routingRequiresDurableVouchOrFailFast` | The Unobtainable routing lands From-Source only on durable Vouched/Pending evidence, FailFast only on the three-conjunct corner. | encoded; prod-tested (`routing_fail_fast_requires_all_three_conjuncts`) |
| `unresolvedJobAlwaysArmed` | An unresolved unparked job is always claimable or claimed (never stranded). | encoded; prod-tested (re-arm paths in T-3.5/T-3.6 batteries) |
| `noWrongfulTerminalFailure` | No build terminally fails while its wanted set is obtainable. | encoded; prod-tested (moot-arm C3 replay, T-6.2) |
| `noWrongfulFromSourceRouting` | No from-source routing while upstream still offers the wanted set. | encoded |
| `successConsumptionCoversLiveWanted` | Success consumption completes a node only when ingested+verified ⊇ live wanted. | encoded; prod-tested (`success_consumption_coverage_check`, keystone stage 6) |
| `interestUnionLiveOnly` | The effective wanted union ranges over LIVE builds only. | encoded; prod-tested (moot-arm replay: cancelled build's wants leave the join) |
| `pinCoversIngestUntilAllInterestTerminal` | Materialization pins survive until job resolved AND no live interest (PD-10/DF-3 upward re-kind). | encoded; prod-tested (pin-kind db battery, T-4.2) |
| `failoverPreservesJobs` | Jobs survive leader failover (PG-authoritative). | placeholder in draft (recovery rebuild is Phase B) |
| `fencedJobWritesOnly` | Every job write carries the claims-floor fence. | placeholder in draft; prod-tested (fenced db batteries T-1.3/T-1.4: below-floor → Fenced) |
| `kindMatchesWorker` | Materialization attempts execute only on store replicas; build attempts only on builders. | encoded; prod-tested (grpc authorization battery: executor tokens never authorize the kind) |
| `materializationNeverPoisons` | No materialization charge ever produces a Poison verdict. | encoded; **checked** (`quint-retry-policy-pull-materialization`, T-5.1/T-5.2) + CBMC `check_materialization_never_poisons` |
| `materializationInvisibleToBuildBudgets` | Materialization charges feed exactly one budget (their own). | encoded; **checked** (`quint-retry-policy-pull-materialization`) + CBMC `check_materialization_rows_invisible_to_build_decision` + keystone stage 7 |
| `atMostOneUnresolvedJobPerDrv` | The partial-unique-index dedup. | placeholder in draft; prod-tested (`flag_on_concurrent_interest_creates_one_job`, db contract tests) |
| `atMostOneClaimWinner` | One-winner claim arbitration per job (BC-1 composite identity). | encoded; prod-tested (CBMC `check_kinded_one_winner_arbitration` + keystone one-winner stage) |
| `wrongfulFailFastBoundedPerJob` | At most one wrongful fail-fast per job (the one-shot). | encoded |

Draft witnesses (non-vacuity probes, wired at C′): `noSuccessResolution`,
`noFailFast`, `noPark`, `noCrashEstablishment`, `noBuildAttempt`.

Three of the seventeen draft properties violate in pre-wiring simulation for
a documented draft-encoding reason (current-state reads where at-decision-time
ghosts are needed) — recorded in the T-5.3 commit (`420e7c1c7`) as C′
completion work, not as design findings.

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

## Cross-references

- `closure-evidence-invariant-map.md` — the predecessor campaign's map; its
  Phase-2 handoff section cross-references this campaign as the successor
  owner of the substitution-related rows (see that map).
- `retry-invariant-map.md` — the "Cross-campaign addendum" section is the
  retry-side record of the kind partition (T-5.1/T-5.2).
- `executor-invariant-map.md` — the frozen pull-protocol contract
  (:2361–2536); Phase A's proto extension is a recorded addendum, never a
  mutation (criterion 4).
