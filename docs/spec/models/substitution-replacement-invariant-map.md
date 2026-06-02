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

**Status: Phase A complete; Phase B complete; Phase C′ complete (this
map carries all three stage records — the C′ record at the end is the
go/no-go evidence). Phase A landed every §8-A mechanism dormant behind
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
a wired quint check proves it (none in Phase A — C′'s gate). **As of the
Phase C′ stage record (end of this map): every row below is CHECKED** —
bounded-simulation holds checks per regime with TLC calibration pins as
the falsifiability pairs (the three-tier contingency; the per-row
verdicts, the re-encodes, and the wired-check names are in the C′
record's property and calibration tables, which supersede this column's
Phase-A/B status).

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
| **PD-15b** (proposed, Wave 2) | T-2.1's two new RPCs force tonic-generated REQUIRED trait methods on `executor_service.rs` — the tonic-service analog of dormancy-1/RB-1/RB-2 that the adversarial review enumerated for three mechanisms but missed for the fourth. Resolution: both handlers implemented as the dormant Phase-A arms the plan's own proto comments specify (list → empty; progress → ack-and-drop), in the forcing commit (`319050570`). T-3.3 later replaced the listing stub with the real handler per its task spec. | Ratified by execution (the orchestrator accepted Wave 2's record; the resolution is what the plan's wire-contract text requires). **Counter-signed: owner, 2026-06-01 (round-2 final decision gate; signature line applied at the A6 close-out).** |
| **PD-21** (proposed, Wave 5) | The plan-literal quint wiring (extend `quint-retry-policy-pull`'s own step + invariant list) is arithmetically incompatible with stop-condition 8: the materialization counters multiply the 39.7M-state space ≥6× (minimum ceilings) vs the 2× threshold. Adaptation: ENABLE_MATERIALIZATION regime split — the existing check's attr/list/space stay untouched (stricter dormancy than the plan's exception clause); the 16-invariant list lives in the NEW coexistence-regime check where the partition invariants are non-vacuous. | Ratified by execution (Wave 5's record; preserves every Wave-5 acceptance obligation within budget; the literal form cannot meet stop-condition 8 as written). **Counter-signed: owner, 2026-06-01 (round-2 final decision gate; signature line applied at the A6 close-out).** |

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

> **Phase D′ disposition (2026-06-02).** The rows above are the Phase B
> record as written and are not edited. Phase D′ deleted the coexistence
> flag and the walk: **MD-D1 survives verbatim** (the park
> instrumentation, gauge, alert and runbook are untouched — Item T builds
> on them); **MD-D2 and MD-D3 RETIRE** — there is no flag to flip and no
> mixed-flag window (rollback through D′.1 is binary rollback; migration
> 080 is roll-forward only). The replacement operator posture is the
> Phase D′ stage record's "Deployment and rollback posture" section
> below. MD-D4's skew posture is unchanged (the binding check is
> unconditional post-D′).

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

> **Counter-signed: owner, 2026-06-01 (round-2 final decision gate).**
> The mark-discriminator ruling stands as written — no overrule; the B2
> fix and `sched.materialize.routing+2` are final. This signature
> satisfies C′ entry criterion 2, and the C′ record's GO condition 1
> below is resolved by it. Post-D′ the discriminator is carried in the
> pruned-ORIGIN form (`sched.materialize.routing+3`, PD-D1) — the same
> ruling re-keyed when migration 080 deleted the mark column. Signature
> line applied at the A6 close-out.

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

## Phase C′ stage record (model completion + the §9.3 calibration transfer; landed 2026-06-01)

### Identity

- Branch: `a4-cprime`, 4 commits ahead of the Phase B integration tip
  `1070ebc77`: `ede734b95` (the model re-target, deltas 1–6),
  `cc21b3bfa` (the 21 calibration override modules + the spawnCoherence
  PD-7 filter), `c8dc93c15` (the 43-check wiring), and this record.
- Scope: model + calibration + wiring + this record ONLY — zero
  rio-scheduler/rio-store production changes (the C′ mandate; a
  model-revealed defect would have been a stop-and-report, and none was
  found); `nix/quint.nix` is the one wiring file touched.
- The dormancy oracle: `retryPolicy.qnt` untouched (zero diff);
  `quint-retry-policy-pull`'s definition is byte-untouched in
  `nix/quint.nix` (drv identity — the same derivation green at the
  Phase B tip).

### The delta-encoding table (go/no-go criterion 3)

The six C′ handoff deltas, each encoded against the behavior-bearing
commits (not the Phase A draft, not the design text):

| Δ | Encoding (state/action) | As-built citation | Draft-wrong-verdict note |
|---|---|---|---|
| 1 | Per-drv `topdownPruned` ghost set by `createJob(OPruned)`; `consumeUnobtainable` arm selection split marked/unmarked per the 6-row B2 table (arm-3 FailFast = the four-conjunct corner; unmarked → `JResolvedFromSource`); the mark consumed by success/from-source resolution (the §4 clear-mirror), by the park re-evaluation, AND by the fail-fast itself; `ffJustifiedAll` carries the four-conjunct justification at decision time | `route_unobtainable` (materialize.rs:530–579, the `_ if inputs.topdown_pruned` arm), `clear_pruned_mark_on_job_resolution` (:979), `fail_fast_topdown_pruned_root` (dispatch.rs:1126 — the fail-fast clear the handoff table did not list; encoded as found in code) | The draft's arm 3 fail-fasted ANY node on confirmed-missing/spent — against the as-built system its `routingRequiresDurableVouchOrFailFast` would VIOLATE on every unmarked-leaf release (the B2 fix's exact disposition) and its `noFailFast` witness would violate through traces the production system routes from-source: wired as drafted it would have verified the pre-B2 system |
| 2a | Per-replica `channelStale` (failover stales every channel — the kube-proxy ClusterIP pinning); claims and report consumptions gate on a fresh channel; `redialChannel` is the abandon-and-redial (the UNAVAILABLE trigger RPC collapsed into the action); `claimAfterFailover` ghost + the `noPostFailoverClaim` witness make the redial's liveness checkable | `MaterializeTransport::abandon_connection` / `inspect_outcome` (rio-store/src/materialize/client.rs:338–365); the two transport unit pins (`poll_abandons_connection_pinned_to_standby_replica`, `report_abandons_…`) | The draft's atomic direct claim could not represent the dead-end at all — the B3 class was unfalsifiable, so the transition VM scenario's bug had no model-level guard |
| 2b | `legacyInterest` per (build, drv) (in-memory interest with NO wanted rows — the FP-4(b) window), bounded by `MAX_LEGACY`; the standalone creation sites (`OProbe`, `OStaleReset`) backfill the relation for every live legacy build; the in-tx origins require the creating tenant's rows (the same-tx batch); `creationLeavesTenantResolvable` is the new invariant; the dispatch-probe `dedupRefeed` carries the as-built lazy-heal backfill | `create_materialization_job_if_enabled`'s `live_interested` loop (materialize.rs:184–217 — the loop runs on the dedup arm too, which is the lazy heal the invariant's creation-scoped form prices in) | The draft's `liveWanted(d) != Set()` creation guard made the no-wanted-rows state UNREPRESENTABLE — the B4 bug class could not be expressed, falsified, or guarded |
| 3 | The park rides the InfraFailure consumption at the budget (`reportInfra`: `newCount >= MAT_BUDGET` ⟹ parked in the same action — the draft's separate `parkOnBudgetExhaustion` transition deleted as not-as-built); `parkReevaluate` (Vouched/Pending → `JResolvedFromSource`, mark cleared, no exec id) vs `parkBackoffExpires` (un-park, count NOT reset); the establishment never parks (`establish_materialization_attempt` leaves the job pending claimable at any count); `stalledJobs` derived val = parked ∧ Broken (the gauge population) | `consume_materialization_outcome` InfraFailure arm (materialize.rs:826), `tick_reevaluate_parked_materialization_jobs` (:1285–1334), `establish_materialization_attempt` (:1243), `park_materialization_job` (:1119 — backoff derives from the SURVIVING count) | The draft's `housekeepingReevaluatePark` unconditionally un-parked Broken-evidence jobs AND reset `matInfraCount` to 0 — against the as-built system it would have verified a park cycle that cannot stall (the MD-D1 alert population would be model-empty, and the budget-reset would mask the as-built exponential-backoff posture) |
| 4 | `OReprobe` added (the AS-5 6d reset → NQueued in the creation action); `OStaleReset` requires `NCompleted` ∧ a live-wanted output absent and resets to NQueued in the same action (the draft's `nodeStatus != NCompleted` precondition REVERSED for this origin); the 3-in-tx/2-standalone posture is below model atomicity — encoded as the wanted-rows-precondition (in-tx) vs backfill (standalone) split; `poison_cleared` recorded out-of-model (no poison state) | merge.rs:919–934 (`apply_reprobe_reset_in_memory`, the flag-gated 6d slot), merge.rs:1670 (`verify_preexisting_completed`), the T-5.2 five-site posture table | The draft BLOCKED the as-built stale-verify shape (a Completed node with vanished outputs is exactly what creates `stale_reset` jobs) — its `staleResetRun` analogue was unwritable, and `noStaleResetCreation` would have been permanently unviolated (vacuous) |
| 5 | An explicit `view: str -> ViewState` layer (`VNone`/`VPending(parked)`/`VClaimed(holder)`) — the kernel admission's input; `claimJob` and `builderPull` read the VIEW while the invariants read PG truth; `failover` rebuilds eagerly and faithfully (`projView` over jobs+attempts — holders and park expiries mirrored); `dedupRefeed` is or_insert (armament-preserving); `unresolvedJobAlwaysArmed` re-encoded as view-faithfulness (`view == projView(jobs, attempts)` per drv) | `rebuild_materialization_job_view` (materialize.rs:428, recovery.rs:197), the `entry().or_insert()` dedup feed (materialize.rs:160–176), `materialization_job_view` (:302) | The draft had `failoverPreservesJobs = true` (a placeholder constant) and no view at all — the F10/L1 strand class and the B5(a) overwrite class were both unfalsifiable |
| 6 | `claimJob` admits NQueued|NReady with NO mark conjunct (the claim IS the substitution; the B1 upgrade); `builderPull` carries the A11 `mustSubstitute` refusal (marked ∧ Broken ⟹ refused — the JobView::None dual-predicate window); `noMarkedClaim` is the B1 regression witness | `admit_pull_kinded`'s two upgraded NotYetReady→DeliverNew cells (rio-evidence-kernel/src/pull.rs:783–810), the as-built `must_substitute` arm (:188) | The draft anticipated PD-6 (Queued claims) but had no mark at all — with Δ1's mark imported naively (a mark conjunct on the claim), the model would have re-introduced the B1 admission gap and verified the pre-B1 system |

Encoding corrections found by the protocol itself (model artifacts, not
product defects — each verified against the code before correcting):

1. **The one-shot reads the DRV-level history** — the draft's
   `oneShotSpent` read the per-JOB counter; as built,
   `count_materialization_rows_in_history` counts the derivation's
   materialization_unobtainable rows across jobs. The re-encode reads
   the drv-level ledger. (A resubmit-shaped trace would have
   mis-verdicted under the draft: re-arm where the production system
   fail-fasts on the spent drv-level shot.)
2. **The B2 ingest-guarantee window closes at the §5.3 release** — the
   first baseline run of the no-pin calibration VIOLATED
   `pinCoversIngestUntilAllInterestTerminal` under the AS-BUILT step:
   the `ingested` ghost outlived a legitimate release+GC and re-armed
   against later-arriving interest. Code walk: the release
   (job-resolved ∧ all-interest-terminal) is the documented END of the
   pin guarantee; re-arrival is the stale_reset re-materialization lane
   (T-1.6), not a pin hole. The ghost now clears with the release; the
   release witness re-encoded on the present-unpinned-resolved
   footprint. This is the baseline-protocol working exactly as designed
   (a calibration row that cannot hold its baseline is recalibrated,
   never waved through).

### Property verdicts (go/no-go criterion 2)

The §9.1 successor table: 21 named invariants (the 17 original rows —
three re-encoded as at-decision-time ghost latches — plus `boundsOK`
and the three delta-derived invariants `creationLeavesTenantResolvable`,
`materializationCrashChargedOnce`, `crossBuildWantedIsolation`).

- **Bounded-simulation HOLDS** (the GHA-wired deliverable): the full
  21-invariant conjunction holds in EVERY design-scale regime (base /
  failover / adversarialStore / staleTenure / crashLoop) at 500 K
  samples × 15 steps during development and 2 M × 15 in the wired
  checks (`quint-materialization-holds-*`).
- **Exhaustive TLC**: no modeled scope's full conjunction converges
  inside a gate-compatible budget (the measurement table below) — the
  pre-registered closure-evidence contingency applies: the wired
  deliverable is the sim-holds + pins + witnesses tier; the Ex-scope
  conjunctions are documented manual targets, each with a
  zero-violation bounded prefix at the recorded coordinates. **No
  violation was found in any prefix** — the as-built-faithful traces
  TLC explored never falsified a §9.1 invariant (the stop-and-report
  trigger that did not fire).
- **Witnesses**: all 20 reachability witnesses VIOLATE (reachable) in
  their named regimes under the rust simulator; the wired set
  (14 `quint-materialization-witness-*` checks) covers every §9.1
  property's contended scenario and every delta's new behavior. Two
  witness encodings were corrected during the sweep (the store
  short-circuit needed the as-built Queued cache-hit shape; the release
  witness the footprint form) — both then violated.
- **Named runs**: 10 deterministic scenario pins green
  (`quint-materialization-runs-{base,failover,adversarial}`).

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

### CE-D6 re-run (go/no-go criterion 4)

The deferred manual-target runbook (closure-evidence checklist row
CE-D6: "before any change to scheduler evidence handling, run the
seven exhaustive conjunctions"; deferred by Phase B to this gate),
re-run at the C′ tip — the dual-coverage instant: the predecessor
model's conjunctions against the tree whose successor checks are
simultaneously green. Coordinates: 35-minute caps at 60 workers (the
Wave-4 floor coordinates); every run was still mid-frontier at its
cap, matching the recorded "none converges" posture — the
deployment-time formal posture is zero violations at-or-past those
coordinates, and that is what every row shows. **Zero violations in
all seven.**

| Conjunction (closureEvidence) | Verdict @ 35 min / 60 workers | Bounded coverage |
|---|---|---|
| BaseEx × asBuiltHoldInvariants | zero violation (unconverged) | 22,099,308 distinct / depth 15 — past the Wave-4 16.4 M floor |
| FailoverEx × asBuiltHoldInvariants | zero violation (unconverged) | 13,577,423 distinct / depth 13 |
| FaultPersistEx × asBuiltHoldInvariants | zero violation (unconverged) | 19,057,679 distinct / depth 15 |
| Duo × asBuiltHoldInvariants | zero violation (unconverged) | 1,531,949 distinct / depth 8 |
| C3Duo × asBuiltHoldInvariants | zero violation (unconverged) | 1,519,758 distinct / depth 8 |
| AdversarialStoreEx × asBuiltHoldInvariantsAdversarialStore | zero violation (unconverged) | 6,233,351 distinct / depth 11 |
| StaleDuo × {leaderClassEvidenceWrites, noStaleTenureClearOverride} | zero violation (unconverged) | 2,219,869 distinct / depth 8 |

The closure-evidence wired check set itself is untouched and green at
this tip (criterion: nothing pruned before D′); the F1-permissiveness
kept-guard override re-run is in the transfer table above.

### Check inventory (wired this phase)

| Check | Kind | Subject |
|---|---|---|
| `quint-materialization-holds-{base,failover,adversarial-store,stale-tenure,crash-loop}` | sim-holds (2 M × 15) | the 21-invariant §9.1 conjunction per design-scale regime |
| `quint-materialization-runs-{base,failover,adversarial}` | named-run pins | the 10 delta scenario narratives |
| `quint-materialization-witness-*` (14) | sim expect-violation | every §9.1 property's contended scenario + every delta behavior (incl. the B1 marked-claim and B3 post-failover-claim liveness guards) |
| `quint-materialization-calib-*` (19) | TLC expect-violation pins | the §9.3 falsification corpus (18 property pins + the F1 covered-creation reachability pin) |
| `quint-spawn-coherence-mat-jobs` + `quint-spawn-coherence-witness-mat-filtered` | TLC exhaustive + witness | the PD-7 GetSpawnIntents job filter — CONVERGED exhaustively: 15,176,448 distinct states, depth 34, ~2 min on the CI builder class (the one new exhaustive TLC check this phase wires); the six pre-existing spawnCoherence regimes carry `ENABLE_MAT_JOBS = false` (constant-false var; base regime re-verified green at 2,013,280 distinct states) |

Formal-check invariance: `retryPolicy.qnt`, `nix/kani.nix` zero diff;
`quint-retry-policy-pull` and `quint-retry-policy-pull-materialization`
definitions byte-untouched (drv identity); zero closure-evidence checks
removed or modified (the dual-coverage instant — both generations green
against the same tree).

### Exhaustive measurement table (the budget record)

Coordinates: TLC BFS, 60 workers (the house-budget worker count),
10-min caps for the Ex measurements (the 15-min-hard rule's
determination form: every run below was still mid-frontier with a
growing queue at its cap — none is within reach of a 30-min
convergence either, per the B1-flip 24.5-min/192-worker corroboration
at the BaseEx scope). Manual-target command shape:

    quint verify --backend=tlc --main=materializationJob<Scope>Ex \
      --invariant=<the 21-name §9.1 conjunction> \
      docs/spec/models/materializationJob.qnt

| Scope (as-built step, full conjunction) | Verdict @ cap | Bounded coverage |
|---|---|---|
| materializationJobBaseEx | unconverged @ 10 min | zero violation through 435,251 distinct / depth 14 |
| materializationJobFailoverEx | unconverged @ 10 min | zero violation through 500,327 distinct / depth 14 |
| materializationJobAdversarialStoreEx | unconverged @ 10 min | zero violation through 406,878 distinct / depth 16 |
| materializationJobStaleTenureEx | unconverged @ 10 min | zero violation through 475,795 distinct / depth 14 |
| materializationJobCrashLoopEx | unconverged @ 10 min | zero violation through 400,001 distinct / depth 14 |
| (corroboration) matCalibB1ClaimRefusesMarked @ calibStep — a strict SUBSET of the as-built BaseEx space | unconverged @ 24.5 min, auto(192) workers | zero violation through 2,338,887 distinct / depth 18, queue still growing — the superset (as-built BaseEx) therefore cannot converge at 30 min either |

Design-scale (2-drv) regimes are strict supersets of their Ex scopes
and are not separately measured (the executor/closure precedent: a
scope whose reduction does not converge is recorded once). The
`longChecks` Tier-2 mechanism stays empty — no conjunction fits the
15-30-min window; pressure to change shard knobs remains
stop-and-report per the standing rule.

Width posture (the orchestrator resource directive, applied): the
60-worker rows above are the reference-budget coordinates (comparable
to the house budgets and the Wave-4 floor); the directive's
wide-instance allowance was additionally exploited for a deepening
wave on the two most-cited bounded-coverage rows — scale-annotated
below their reference entries:

| Deepened row | @ 90 workers (15-min cap) |
|---|---|
| matCalibF1NoPresenceRecheck × the §9.1 conjunction (calibStep) | zero violation through 818,905 distinct / depth 15 (unconverged @ 15-min cap) |
| materializationJobBaseEx × the §9.1 conjunction (as-built) | zero violation through 919,952 distinct / depth 16 (unconverged @ 15-min cap) |

### What Phase C′ does NOT claim

- **No exhaustive proof of the §9.1 conjunction at any scope** — the
  wired holds checks are bounded simulation evidence with falsifiability
  pins (the closure-evidence Phase-1 three-tier precedent); the Ex-scope
  conjunctions are documented manual targets with zero-violation bounded
  coverage at the recorded coordinates.
- **No production code was changed and none needed to be** — the model
  matched the as-built system at every delta; the two corrections the
  protocol forced were model-side (the drv-level one-shot read, the
  release-window ghost).
- **The B1/B3 pre-fix flips rest on bounded evidence** for their
  "NOT violated" half (the [ok] direction cannot early-exit); their
  as-built halves are wired TLC-violated witnesses.
- **The finding-11 owner counter-signature** (entry criterion 2) is
  STILL PENDING at this record — the model encodes the orchestrator's
  ruling (`sched.materialize.routing+2`, the mark discriminator); if the
  owner overrules, Δ1's encoding and the F8/F9/B1 rows re-open with B2.
- **D′ is not authorized by this record** — the go/no-go recommendation
  below is the owner's input, not the decision.

### The go/no-go recommendation

**RECOMMENDATION: GO for Phase D′**, conditional on the two items
outside this phase's authority:

1. **The finding-11 owner counter-signature** (entry criterion 2,
   still pending) — the model encodes the orchestrator's ruling
   (`sched.materialize.routing+2`); an overrule re-opens Δ1's
   encoding and the F8/F9/B1 calibration rows together with B2.
   **Resolved: counter-signed, owner, 2026-06-01 (round-2 final
   decision gate — the same decision authorized D′).** No overrule;
   Δ1's encoding and the F8/F9/B1 rows stand. Signature line applied
   at the A6 close-out (the Phase B record's finding-11 note above).
2. **The orchestrator's full-gate run at integration** (criterion 5's
   gate half) — every new check built green individually at this tip
   (43/43), tracey-validate and treefmt are green, and zero existing
   check definitions changed (drv identity for the dormancy oracle);
   the single-command gate is the integrator's step per the C′
   mandate.

The three-line evidence summary the recommendation rests on:

- **The model is the as-built system**: all six handoff deltas
  encoded against the behavior-bearing commits, with two
  draft-wrong-verdict classes caught and corrected (the delta table)
  — wiring the draft un-re-targeted would have verified the pre-B1/
  pre-B2/pre-B3/pre-B4/pre-T-4.3 system.
- **The §9.3 transfer is complete with zero failures**: 18/18
  predicted falsifications VIOLATED under both backends and re-held
  under the as-built baseline at the same constants (one model-side
  recalibration executed per protocol and recorded); both liveness
  rows flipped exactly as predicted; every by-construction row
  carries its structural argument plus reachability or falsifiability
  evidence; the C5/CE-7 standing open item closes WITH the
  falsification the 0d deferral asked for.
- **Every §9.1 property is checked and non-vacuous**: the 21-invariant
  conjunction holds in all five regimes (bounded simulation at 2 M
  samples, wired with 19 TLC falsifiability pins and 14 reachability
  witnesses per the established three-tier contingency; exhaustive
  conjunctions documented as manual targets with the measured
  zero-violation bounded coverage), and the CE-D6 runbook re-run is
  recorded above.

NO-GO triggers checked and not fired: no transferred row held without
falsifying; no §9.1 property needed a system change to hold; the
six-delta review found no behavior the model verifies that the system
lacks.

## Phase D′ stage record (the deletion phase; landed 2026-06-02)

**Authorization:** owner FINAL DECISION GATE 2026-06-01 ("D′ GO"), all five
orchestrator rulings counter-signed (finding-11 mark discriminator among
them). **Plan:** `substitution-phaseD-plan.md` (revised, adversarial review
round 1 incorporated). **Worktree:** `a4-dprime` off `bf15fe57f`
(formal-sprint @ Phases A+B+C′ integrated). **Result:** the walk machinery,
the `Substituting` status, the wanted-set lossy semantics, both evidence
columns, the coexistence flag and the superseded verification surface are
DELETED; the store-owned materialization job is the only substitution
mechanism; migration 080 retired the columns and narrowed the status CHECK;
every commit boundary green; the full gate green at the phase exit.

### Commit ledger (25 commits, bf15fe57f..668b5a229 + the close-out)

| Wave | Commit | Subject (abbreviated) | Net |
|---|---|---|---|
| D1 | 65d285cb9 | retire the flag-off walk oracles + transition scenario | −5 VM attrs |
| D1 | ea1e8e912 | retire the walk-path unit batteries | test-only |
| D1 | cd4b9f30a | retire the executor dormancy pins | test-only |
| D2 | d3aa9efec | settlement fail-fast keyed on job origin (PD-D1, red-first) | behavior |
| D2 | 1b3eb9f39 | durable-relation settlement classifier (PD-D4, three-part) | behavior |
| D2 | c922ee5c6 | wanted contributions rebuilt at recovery (PD-D5, DQ-2) | behavior |
| D3 | aab018f87 | delete the walk spawn machinery (+588/−3112) | deletion |
| D3 | 7d047f5a1 | delete the walk consumption machinery (+49/−883) | deletion |
| D3 | daf36d3c2 | remove the Substituting status (+170/−298) | deletion |
| D4 | 8ac576614 | materialization dispatch unconditional (+339/−961) | deletion |
| D4 | 89e3198ab | store executor spawn-iff-addr (+70/−63) | deletion |
| D4 | f513937c1 | drop the helm coexistence flags (+59/−218) | deletion |
| D5 | 151df20a4 | delete the evidence-column machinery (+484/−2687) | deletion |
| D5 | e07f9c22d | kernel reduced to the durable child relation (+118/−391) | deletion |
| D5 | 504a20534 | retire the SubstitutePath RPC (+28/−433) | deletion |
| D5 | 2ad710ddb | drop the stored wanted-union dual-write (+64/−428) | deletion |
| D5 | 522e7597a | docs-data regen rider (+3/−3) | regen |
| D6 | 41d714b04 | migration 080 + M_080 + PINNED + decode-arm removal (+119/−31) | migration |
| D7 | 94996482b | survivors core wired + 27 closure checks pruned (FP-1) | verification |
| D7 | ad23341ae | materializationJob re-targeted (delta-6 shrink + PD-D1 encoding) | verification |
| D7 | 0f632aeb8 | stale walk-era narration re-worded at surviving sites | hygiene |
| D7 | 06afc9409 | retire the walk-era substitution rules (R half) | spec |
| D7 | 4d8478920 | re-state the substitution rules around materialization (S half) | spec |
| D7 | 64939a7e0 | absence-gate narration stragglers | hygiene |
| D7 | 668b5a229 | docs-lint metric-helper fixup (gate #4 finding) | hygiene |
| D7 | (this commit) | the stage record | docs |

### Deletion totals (measured, `git diff bf15fe57f..668b5a229`)

134 files, **+5,409 / −21,484 = net −16,075** (the §3.10 estimate was net
−9,000..−10,500 over a different counting basis — the estimate excluded the
D1 test deletions' true breadth and the spec/model deltas land smaller than
budgeted). By area (net): rio-scheduler/src −13,099 (52 files);
nix/tests −1,770; rio-evidence-kernel −612; nix/quint.nix −473;
rio-store/src −160; rio-test-support −100; rio-proto −91; docs/spec
components −94; infra/helm −55; nix/kani.nix −25; rio-dashboard −3;
docs/spec/models +449; rio-migrations +96; gateway/controller/common ≈0
(comment/marker hygiene only); .github/codecov.yml 51→46.

### Disposition ledgers (locations)

- **5 VM attrs + subtests:** 65d285cb9 commit body (attr table verbatim).
- **Unit batteries (~100 tests):** ea1e8e912 / cd4b9f30a bodies (per-test
  class → successor-by-name or retired-with-mechanism); D1-escape fold-ins
  recorded in aab018f87 (stop-1 carve-out, 9 tests).
- **27 quint-closure checks:** 94996482b body (the 29-attr table: 2 KEPT +
  27 retired, each with named successor or mechanism).
- **26 spec rule IDs (the plan's 22-row table):** 06afc9409 (2 R) +
  4d8478920 (4 R + 1 R→S transfer + 14 S re-statements + bumps) — final
  tally below.
- **Kernel harnesses 17→14→9:** 8ac576614 (D4.1, 17→14) and e07f9c22d
  (D5.2, 14→9) bodies; kani.nix expectedHarnesses re-pinned in lockstep.

### T-D7.2 verdict-identical re-run (the DS-1 tier protocol)

All 43 materialization checks rebuilt after the model shrink (the .qnt edit
rehashes every check). **43/43 verdict-identical; zero flakes; the
reproduce-3× rule was never invoked; stop #3 not fired.**

| Tier | Count | Result |
|---|---|---|
| Deterministic TLC (19 calib pins + spawn-coherence exhaustive + its witness) | 21 | 19× `[violation]` (pins still falsify), 2× spawn pair as expected (`[ok]` + `[violation]`) |
| Deterministic run replays (runs-base ×7, runs-failover ×2, runs-adversarial ×1) | 3 | 7+2+1 passing, no run edits |
| Unseeded sim witnesses (incl. noMarkedClaim — the B1 guard — and the F8/F13 anchor class) | 14 | 14× `[violation]` (all reachable post-shrink) |
| Unseeded sim holds (5 regimes × 21-invariant conjunction) | 5 | 5× `[ok]` |

Model deltas verified by the run: builderPull's mustSubstitute conjunct
dropped (window-empty equivalence in the model header — every resolution
arm, now including cancellation and obsolescence, ends the topdownPruned
ghost with the row, so ghost ⟹ unresolved job ⟹ JobView non-None ⟹
builderPull disabled by its existing guard); the PD-D1 dedup upgrade encoded
as `dedupPrunedUpgrade` (armament preserved, the DQ-1 record) — this closes
the recorded C′ delta-1 under-encoding scope note; spawnCoherence's
ENABLE_MAT_JOBS=false regimes re-documented as the job-free trace subspace
(PD-D8, kept).

### T-D7.1 survivors core (and the recorded scope finding)

`closureEvidenceSurvivors` (Duo constants) + `closureEvidenceSurvivorsFailover`
(FailoverEx constants): the six surviving invariants
A14/A15/A22/B9/B10/L3 over the restricted post-deletion alphabet (no
probeWalk / walkFinishes / consumeWalk / dropStaleWalkResult /
probeFailFastTried / probeSettleTried; MSubstituting unreachable), wired
sim-holds tier (2 M × 15; 23 s / 15 s — minutes-class, stop #7 not
approached), falsifiability pair = the two kept pins (f1-stale-produced ×
B9, f4-demand-drop × B10).

**Scope finding (follow-up candidate, predecessor encoding):** the first
wiring attempt at the FailoverDuo constants (two builds AND failover)
violated B9 within 2 M samples — REPRODUCED under the FULL alphabet at the
same constants (trace inclusion; seed `0xffbfc9ac0c85df5b`, transcript in
the T-D7.1 commit body and the module's scope note). The trace is the
documented predecessor degradation (post-failover stored-union widening:
narrow co-build → child Produced under width {1}; failover drops memory-only
contributions; union widens to {1,2}; dependent opens from source above the
stale Produced child) at a scope no holds record ever covered (FailoverDuo
was a C1-strict probe scope). NOT a D′ regression — production deleted the
lossy fallback in Wave D2.3 (durable relation rebuilt at recovery); the
model corner is history. Recorded for the A6 archive decision: the corner
documents WHY the 062 semantics had to die.

Build note: the survivors instantiations grew the closureEvidence
quint→TLA+ conversion past the 4 GiB Apalache server heap; both kept pins
re-pinned at `serverHeapMb = 8192` (the gw-f1/f6 precedent; OOM reproduced
and the fix validated locally).

### T-D7.3 spec sweep (final tally; 26 plan IDs + 6 forced additions)

- **Retired (6):** detached+5, fanout-bound, leader-gate,
  substitute-complete-inline+2, evidence.closure-hole,
  merge.substitute-fetch — house retirement notes in place, successors
  named.
- **Retired with transfer (1):** evidence.settlement →
  `sched.materialize.settlement` (PD-D6: armed-action totality across
  failover; impl markers on the park-reevaluation/cancellation ticks;
  verify on `flag_on_every_job_state_has_armed_action`, the failover VM
  attr, and the failover holds check).
- **Re-stated + bumped (14 from the table):** substitute-topdown+13,
  wanted-outputs+3, evidence.durability+3 (fence kept column-agnostic),
  materialize.job+2 (admission-refusal arm added to the body),
  materialize.routing+3 (pruned ORIGIN discriminator + the three-part
  durable classification normed), substitute-probe-indeterminate+2,
  stale-substitutable+2, reconcile-order+2, fod-substitute+3,
  snapshot-substituting+3, spawn-intents.probed-gate+3,
  state.terminal-idempotent+2 (the actual carrier of the stale status
  alphabet — the plan's table placed this edit on state.machine+2, whose
  body/figure/guards were already clean: recorded deviation),
  exec-correlation+8, store.materialize.executor+3 (PD-D2),
  ctrl.scaler.signal-substituting+3.
- **Re-pointed only (4 + 1):** eager-probe, substitute-probe,
  ca-fod-substitute, gw.activity.subst-progress (rref moved in 06afc9409);
  ctrl.scaler resolved as S (body named the status).
- **Forced additions (6, the dangling-ref/stale-mechanism class the plan's
  R7 predicted):** poison.clear-survivor-reevaluation+2, admin.clear-poison+3,
  db.derivations-gc+4 (victim-filter rationale re-keyed to the durable
  classifier), store.substitute.probe-429-retry+3,
  store.substitute.admission+2, sched.materialize.pinning untouched
  (mechanism-clean) — and sched.materialize.job picked up the
  admission-refusal sentence (the walk-era interlock's successor home).
- tracey: 20 rules bumped in one staged-diff pass; every annotation site
  re-pointed in the same commit (67 files); validate green at both
  boundaries; `tracey query uncovered` = 11 pre-existing non-substitution
  rules, ZERO stranded by D′.

### T-D7.4 absence gate (the §11 patterns + both predecessors' additions)

Final transcript (per-file hit counts; the full line list is reproducible
by re-running the §11 grep at 64939a7e0):

```
    50 rio-scheduler/src/actor/tests/materialize.rs
    18 rio-scheduler/src/dag/tests.rs
    15 rio-scheduler/src/actor/tests/merge.rs
    11 rio-scheduler/src/db/tests/wanted.rs
    9 rio-scheduler/src/actor/merge.rs
    8 rio-scheduler/src/db/wanted.rs
    5 rio-store/src/materialize/executor.rs
    5 rio-evidence-kernel/src/pull.rs
    3 rio-scheduler/src/dag/mod.rs
    3 rio-scheduler/src/actor/tests/dispatch.rs
    3 nix/quint.nix
    2 rio-scheduler/src/grpc/tests/pull_tests.rs
    2 rio-scheduler/src/domain.rs
    2 rio-scheduler/src/actor/dispatch.rs
    2 nix/kani.nix
    1 rio-scheduler/src/state/derivation.rs
    1 rio-scheduler/src/db/tests/live_pins.rs
    1 rio-scheduler/src/actor/tests/recovery.rs
    1 rio-scheduler/src/actor/tests/pull.rs
    1 rio-scheduler/src/actor/tests/misc.rs
    1 rio-proto/proto/dag.proto.fields
    1 rio-proto/proto/dag.proto
    1 rio-evidence-kernel/src/lib.rs
```

**146 hits, every one classified; zero unexplained.**
`infra/helm`: 0. `RIO_MATERIALIZATION__ENABLED`: 0.
`substitute_spawned_total`: 0. Classes:

1. `wanted_output_names` ×105 — the dag.proto submission field (dag.proto:96
   + the .fields snapshot + domain.rs mirror) and the
   `build_wanted_outputs.wanted_output_names` relation column (db/wanted.rs,
   executor SQL) + their tests. Wire/schema-stable by design.
2. Wire-retained surface — `SubstituteProgress` (build_types.proto, BC-4
   relay), `DerivationEventKind::SUBSTITUTING` + `K::Substituting` event
   asserts, `ClusterStatus.substituting_derivations` (bucket re-sourced,
   field docs re-worded), gateway "substituting" display strings.
3. Deletion-record / history comments at surviving sites (kernel lib/pull
   docs, kani.nix harness notes, quint.nix family + delta narratives,
   merge.rs bug_089/bug_132 archaeology, executor.rs origin note,
   derivation.rs, dispatch.rs walk-era-caller notes, test docs).
4. Surviving test names containing `topdown_prune` (the prune itself
   survives; only column-naming identifiers were renamed — predecessor
   note 5).

Two hygiene commits (0f632aeb8, 64939a7e0) converted every
present-tense straggler into the materialization-lane narration or an
explicit past-tense record. Regen umbrella: idempotent, zero drift (the
conditional ledger commit #22 was not needed); `after_n_builds` = 46
(set at D1, unchanged).

### Final counts (the §7 exit-gate populations)

- VM attrs **32** (37 − 5); codecov-matrix-sync at **46**.
- Quint: **43** materialization (verdict-identical) + **2** kept closure
  pins + **2** survivors-core checks + all non-campaign families (273 → 246
  quint attrs: −29 closure-evidence, +2 survivors).
- Kani: kani-rio-evidence-kernel at **9** harnesses (expectedHarnesses
  17→14→9 in lockstep; closure_vouched KEPT — live merge-time
  pruned-origin gate).
- Scheduler nextest 1113 → 1095 → (D7 marker hygiene leaves counts
  unchanged) — every delta enumerated in the wave commit bodies; store 541;
  kernel 16; migrations 11.
- Spec: 565 rules; 7 retired (incl. the transfer), 1 created
  (sched.materialize.settlement), 20 bumped.

### Deviations (this block + both predecessors', consolidated)

Predecessor blocks (recorded in their notes, restated here for the ledger):
SubstituteProgress KEPT (BC-4 relay — T-D3.2 scope correction); the
semaphore cluster rode T-D3.1 (deny-warnings, DOB-1 pre-authorization);
rio-dashboard touched in T-D3.3 (§11.3 fence noted); PullNodeStatus
alphabet shrink landed in D5.2 with the harness re-pin;
create_materialization_job_if_enabled renamed create_materialization_job;
the legacy decode arm removed in D6 (migrate-before-recovery verified);
predecessor note 8's D5-marker claim about SQL seeds corrected by the
stop-4 sweep (41d714b04).

This block: (1) the survivors core wired as TWO instantiations at
recorded-green scopes instead of one FailoverDuo module — forced by the B9
scope finding above (the plan pre-authorized either form; the
non-negotiable — six properties wired, one-landing swap — held). (2) The
R/S commit partition: only fanout-bound and substitute-complete-inline
retire in the R commit; the other retirements ride the S commit because
their inbound rrefs live in re-stated bodies (no boundary holds a dangling
ref — the green-boundary requirement outranked the table's halves).
(3) state.machine+2 untouched; terminal-idempotent+2 carries the status
alphabet edit (see above). (4) Six forced rule additions (R7 class).
(5) The 15 closure calibration evidence files annotated (the plan named 9;
the 6 newly-unwired get the same note naming successors). (6) Two hygiene
commits outside the 5-task ledger (comment-only; the absence gate's
zero-unexplained bar).

### Follow-up candidates (recorded, NOT D′ work)

1. **Floating-CA stale-reset carrier gap** (D3-D4 note 7): the stale-verify
   reset clears output_paths; job assignments carry expected paths == [""].
   Pre-existing Phase B shape; the walk's stash that papered over it is
   gone. Candidate red-first fix outside D′.
2. **FailoverDuo B9 corner** (this block): the predecessor encoding's
   stored-union widening violates B9 at two-builds+failover scopes (full
   alphabet, reproduced). History — the mechanism is deleted — but the A6
   archive should carry the seed and the corner as part of the model's
   final record.
3. **setup_with_mock_store_materialization_enabled alias** (D3-D4 note 9):
   fold the ~30 call sites onto setup_with_mock_store at leisure.

### Deployment and rollback posture (the §7.5 record; replaces MD-D2/MD-D3)

Through D5 (D′.1): revertable by binary rollback — the flag no longer
exists after D4, so rollback to walk behavior = a pre-D′ BINARY; the DB
schema is unchanged through D5 and pre-D′ binaries run against it
unmodified. D6 (080 on a persistent DB) is roll-forward only (frozen
migrations): a pre-D′ binary against a post-080 DB fails; a post-D′ binary
against a pre-080 DB works (decode arm verified then removed in-wave).
Deployment order: roll D′.1 binaries everywhere FIRST, then ship the
080-carrying release. In-flight state across the upgrade: unresolved jobs
survive (durable); claimed jobs drain through consumption (exec_id
contract); leftover `substituting` rows decode to Queued pre-080 and are
UPDATEd by 080. Transition residuals (bounded, self-identifying,
resubmission-healed): the directed-error downgrade for pre-Phase-B-era
marks whose upstream later vanished, and the pre-relation wanted-width
saturation (counter + warn). MD-D1 (park alerting:
`rio_scheduler_materialization_stalled` + the alert + PD-20 instrumentation)
SURVIVES VERBATIM.

### What D′ does not claim

No production-deployment or soak evidence (house no-soak rule). No new
model verdicts beyond verdict-preservation re-runs and the survivors-core
re-hold. The counter-signature APPLICATION (B5 supersession, E5
supersession, the bits' retirement counter-signature, CE-D8 split,
frozen-contract addendum, finding-11 signature lines) is A6's; this record
delivers the hand-back list. `closureEvidence.qnt` is NOT archived (A6,
after the survivors core soaks one integration). Items S/T/I remain
commissioned post-D′ work; the PD-20 surface they build on is intact.

### Exit gate (the fourth full `--checks` run)

Gate #4 ran on the tree at 64939a7e0 (`.claude/bin/nixbuild --checks`,
nix-fast-build over `checks.x86_64-linux`, ~80 min wall on the dev host +
remote builder; log `/tmp/rio-dev/rio-a4-dprime-39.log`): **392 successes,
1 failure** — `docs-lint` flagged a raw metric name introduced by the
durability re-statement's fencing prose (the only finding of the run; all
32 VM attrs, cov-smoke, the full quint family incl. the survivors core and
the 43 re-run checks, kani at 9, every per-member clippy/doc/nextest,
pre-commit, helm-lint, codecov-matrix-sync at 46 and tracey-validate were
green). The focused fixup is 668b5a229 (`#(refs.metric)` helper). Gate #4b
on the fixed tree: **exit 0** (log `/tmp/rio-dev/rio-a4-dprime-40.log`,
~6 min — only the docs/spec-affected attrs rebuilt; everything else
cache-carried from #4). One transient infra signature during #4 (a window
of `failed to start SSH master connection` to the remote builder; nix
retried and recovered — the same class the D3-D4 block recorded). No flaky
re-runs were needed; no materialization-check verdict change; no
deleted-machinery resurrection.

### A5/A6 handoff (what close-out still owes)

1. **Counter-signature application (A6):** B5 supersession sign-off
   (last-write-wins per build — `sched.merge.wanted-outputs+3`, the F5/PP-5
   record); the E5-supersession record; the evidence-bits retirement
   counter-signature (migration 080 + the §3.7 dispositions, this record);
   finding-11 signature-line application (the pruned-origin discriminator,
   `sched.materialize.routing+3`).
2. **CE-D8 split** per the design §5.4 (the probe-vouched-closure-gone
   flip's two shapes — pointer rows added to
   `closure-evidence-invariant-map.md` in this commit).
3. **`closureEvidence.qnt` archive** (A6, post-soak) — carry the survivors
   core forward and the FailoverDuo B9 corner record (follow-up 2).
4. **Frozen-contract extension note** in `executor-invariant-map.md` (the
   materialization PullAssignment kind/executor_instance addendum's final
   form — Item I pointer).
5. **The acceptance table's final form** (design §8) — this record is the
   D′ row's evidence; A5/A6 own the table.
6. **Items I/S/T pointers:** Item T observability builds on the intact
   PD-20 surface (MD-D1); Item S (store-side admission/throughput) and
   Item I (the pull-contract addendum integration) unchanged by D′.

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
