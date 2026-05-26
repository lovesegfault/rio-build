# Retry/poison/cascade invariant ↔ spec-rule map

Working artifact for the retry-formal campaign's Phase 0 (spec audit +
referenceFold). Maps the design's invariants over the derivation
retry/poison/failure-cascade machinery onto the `sched.retry.*` /
`sched.timeout.*` / `sched.poison.*` / cascade / dep-failed rule set in
`docs/spec/components/scheduler.typ`, cross-referenced against the nine
decision sites (E1–E9) and the seventeen `RetryState` mutation sites the
protocol inventory catalogs.

This is the post-audit state: every invariant maps onto at least one rule
whose normative MUST sentence states it (the GAP rows below were closed by
new `#r()` rules whose normative bodies are the design's invariant
definitions), every place the code does not do what a rule says it MUST is a
CONTRADICTION row (recorded, not fixed — adjudication is a Phase-1
disposition), and every place two decision sites disagree such that no
single fold over the attempt history can reproduce both is a DIVERGENCE row
(the fold implements the spec-mandated or judged-intended side; the other
side is the deviation Phase 1 must disposition). The executable counterpart
of this map is `rio-scheduler/src/retry_policy.rs` (the `referenceFold` —
the Phase-0 specification oracle the model's `CountersRefineHistory`
invariant compares the live counters against).

This revision incorporates the Stage-A self-review (the audit of this map
and the fold before the Stage-B model is written): three rule bodies were
amended and version-bumped (`sched.retry.attempts-bounded+2`,
`sched.retry.counters-refine-history+2`, `sched.dispatch.fleet-exhaust+3`),
the fold's E2 exempt arm was corrected to reproduce the as-built
window-reset fall-through, the D2 and C2 consequence narratives were
corrected, the rule-vs-rule contradiction C4 was recorded, and the D1/D3/D4
phase-1 dispositions plus the pre-registered Stage-B falsification list
were added.

## The decision sites (the columns of every row below)

| # | Entry point | Trigger |
|---|---|---|
| E1 | `handle_transient_failure` (`actor/completion.rs`) | worker `CompletionReport{TransientFailure}` / `Unspecified` |
| E2 | `handle_infrastructure_failure` (`actor/completion.rs`) | worker `CompletionReport{InfrastructureFailure}` / unsolicited `Cancelled` |
| E3 | `handle_permanent_failure` (`actor/completion.rs`) | 7 permanent statuses (`PermanentFailure`, `CachedFailure`, …) |
| E4 | `handle_timeout_failure` (`actor/completion.rs`) | worker `CompletionReport{TimedOut}` |
| E5 | `reassign_derivations` (`actor/executor.rs`) | stream disconnect / heartbeat timeout / force-drain / backstop |
| E6 | `handle_executor_termination` (`actor/executor.rs`) | controller `ReportExecutorTermination{OomKilled, EvictedDiskPressure}` |
| E7 | `handle_deadline_exceeded` (`actor/executor.rs`) | controller `ReportExecutorTermination{DeadlineExceeded}` |
| E8 | `tick_process_backstop_timeouts` (`actor/housekeeping.rs`) | scheduler-side Running-too-long timer |
| E9 | dispatch fleet-exhaust backstop (`actor/dispatch.rs`) | `find_executor` returns None ∧ every eligible worker ∈ `failed_builders` |

Adjacent deciders that produce the same terminal outcomes but are not
budget sites: `reset_orphan_to_ready` / `adopt_orphan_completion`
(`actor/recovery.rs`, the post-failover reconcile), `handle_substitute_complete`
(`actor/dispatch.rs`, the substitution-failure revert — excluded from the
fold, see "Out of scope" below), `handle_clear_poison` /
`tick_process_expired_poisons` (the two poison-clear paths), the resubmit
reset (`dag/mod.rs`), and the cache-hit `retry.clear()` sites
(`actor/dispatch.rs`, `actor/merge.rs` ×2).

## The invariant ↔ rule map

Verdict legend: **COVERS** — the rule's normative MUST states the
invariant (or the load-bearing piece of it). **PARTIAL** — the rule states
a piece; the missing piece is named. **GAP** — no rule states it; closed by
a new `#r()` rule in this audit. **CONTRADICTION** — the code does not do
what the rule says it MUST; recorded in the contradiction table below and
not fixed here.

### `AttemptsBounded`
*Every failure-driven retry loop is bounded: every counted attempt charges
at least one finite budget, and no attempt charges the same budget twice.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.attempts-bounded+2` *(new; amended in the Stage-A review)* | **COVERS** (the conjunction) | Was a GAP. The per-counter cap rules below each bound one budget; no rule stated the conjunction (every retry loop bounded; every counted attempt charges at least one budget; no attempt charges the same budget twice). The new rule states it and names the two budgets that had no rule at all: the transient cap (`max_retries`, E1) and the non-exempt infra cap (`max_infra_retries`, E2/E6 — previously mentioned only in `sched.retry.promotion-exempt+3`'s prose, and only for the at-ceiling case). The partition is per budget, not across budgets: a single worker-reported transient failure charges both the poison threshold and the per-cycle transient count (`sched.retry.transient-budget` mandates exactly that); the bug class the rule excludes is one attempt charged twice to the *same* budget (the 8a016a393 at-cap OOM double-count). The rule's original wording ("each counted attempt charges exactly one of the named budgets") would have made a Stage-B `AttemptsBounded` encoding falsify on the first transient failure and the falsification be mis-triaged as a code defect — amended (`+2`) in this review. The as-built code falsifies the boundedness clause on the C2 no-report hard-crash loop (see the contradiction table and the expected-falsifications list). |
| `sched.retry.transient-budget` *(new)* | **COVERS** (E1) | Was a GAP (the inventory's named spec hole): no rule covered E1's decision — transient failures charge `count` + `failed_builders` + `failure_count`, requeue with exponential backoff while under both `max_retries` and the poison threshold, poison at either. `sched.retry.per-executor-budget` describes what *infra* failures don't do, not what transient failures do. |
| `sched.retry.per-executor-budget` | **PARTIAL** + **CONTRADICTION C2** | Covers the poison threshold (3 distinct workers / N flat failures) and the transient-vs-infra budget split. Does not state the caps as numbers (config defaults; the TOML example omits `max_infra_retries` and `max_timeout_retries`). The "Executor disconnect DOES count" sentence contradicts E5 (see C2). |
| `sched.retry.exempt-infra-cap` | **COVERS** (the exempt arm) | The exemption's own terminal (`exempt_infra_count` / `max_exempt_infra_retries`). Note the off-by-one convention: the exempt arm increments *before* its cap check (the cap fires *on* the Nth exempt attempt) while the non-exempt infra arm checks *before* incrementing (the cap fires on the N+1th failure). Both are reproducible by the fold; the asymmetry in what `max_X_retries = N` means is a Phase-1 unification candidate, not a divergence. |
| `sched.timeout.promote-on-exceed+2` | **COVERS** (the timeout budget) | `timeout_count` vs `max_timeout_retries`, terminal `Cancelled`. The controller-path divergence is D1. |
| `sched.merge.poisoned-resubmit-bounded+2` | **COVERS** (the cross-cycle budget) | `resubmit_cycles` vs `POISON_RESUBMIT_RETRY_LIMIT`; the per-cycle `count = 0` reset. |
| `sched.retry.promotion-exempt+3` | **COVERS** (the exemption gating) | A promoted attempt consumes no failure budget; an at-ceiling attempt always consumes budget; timeouts consume budget regardless of promotion. The controller-path deviation from "every exempt attempt charges `exempt_infra_count`" is D3. |
| `sched.backstop.timeout+3` | **COVERS** (E8's accounting) | The backstop charges `failed_builders` + `failure_count` so the no-report *wedge* loop (worker still heartbeating, derivation stays Running long enough for the scan to see it) is bounded by the poison threshold; the no-report *hard-crash* loop never stays Running long enough to reach the backstop at all (see C2). The missing PG mirror of the backstop's charge is D4. |

### `PoisonIsTerminalUntilCleared`
*A poisoned derivation stays poisoned until the TTL expires, an admin
clears it, a cache hit moots the failure history, or a bounded resubmit
resets it.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.state.terminal-idempotent` | **COVERS** | Terminal → non-terminal is rejected with an exhaustively-enumerated carve-out list (TTL expiry, GC'd-output reset, re-probe cache hit). The carve-outs are exactly the "until cleared" clause. |
| `sched.state.poisoned-ttl` | **COVERS** | The `poisoned → created` transition is gated by the 24 h TTL. |
| `sched.poison.ttl-persist` | **COVERS** | `poisoned_at` survives failover, so the TTL gate survives failover. |
| `sched.admin.clear-poison` | **COVERS** (one stale-prose note) | The normative obligations (reset both stores, `cleared=true` only if both succeed, idempotent, full-drv-path key) hold. The mechanism prose describes the pre-`b874e5120` in-mem-first ordering and claims the resulting drift is "self-correcting" — the fix exists because it was not; the code is now PG-first. Phase-1 rule-prose amendment, not a contradiction of a MUST. |
| `sched.merge.poisoned-resubmit-bounded+2` | **COVERS** | The bounded-resubmit carve-out. |

### `CascadeReachesExactlyTheDependents`
*poison(n) ⟹ every ancestor of n reachable through non-terminal nodes
eventually reaches `DependencyFailed`; no node outside that set is
cascaded; every interested build of every cascaded node observes a
terminal build state.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.poison.cascade-dependents` *(new)* | **COVERS** (the runtime cascade) | Was a GAP. The merge-time seeding (`sched.merge.dep-failed-transitive`) and the recovery-time re-cascade (`sched.recovery.failed-dep-cascade`) each had a rule; the runtime cascade they both exist to backstop (`cascade_dependency_failure`: BFS over parents, transition Queued/Ready/Created ancestors to `DependencyFailed`, never Assigned/Running) had none. |
| `sched.merge.dep-failed-transitive` | **COVERS** (the merge-time half) | A newly-merged node transitively depending on a failure-terminal node is seeded `DependencyFailed` at any depth. |
| `sched.recovery.failed-dep-cascade` | **COVERS** (the recovery-time half) | A recovered parent with a failure-terminal child is transitioned before `compute_initial_states`. |
| `sched.preempt.never-running` | **COVERS** (the "exactly" half) | The cascade never preempts Assigned/Running nodes. |
| `sched.event.derivation-terminal` | **COVERS** (the observability half) | Every cascaded node emits exactly one `DerivationFailed{DependencyFailed}` to each of its interested builds. |
| `sched.build.keep-going` | **COVERS** (the build-level half) | The cascade is what lets a keep-going build with a poisoned leaf terminate. |

### `CountersRefineHistory`
*At every state the 10 `RetryState` counters equal
`referenceFold(observedHistory, now, budget)`.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.counters-refine-history+2` *(new; amended in the Stage-A review)* | **COVERS** | Was a GAP — necessarily: the spec had no concept of an attempt history, so no rule could state that the counters are a pure function of one. The new rule's normative body is the invariant; the executable definition of the fold is `rio-scheduler/src/retry_policy.rs`. The 300 s sliding-window reset, previously stated nowhere in the spec (not even in the TOML example), is normative here as a clause of the fold — including its as-built fall-through: an under-cap *exempt* infra observation past the window also resets `infra_count` (E2's exempt arm does not return before the reset block, and the reset is gated only on the event's own at-cap outcome). The rule's first wording said only non-exempt failures reset, and the fold's first transcription returned early on the exempt arm; both were corrected in the Stage-A review (`+2`, plus a fold unit test pinning the counted-then-stale-exempt history) so the fold reproduces the code and a counter mismatch on that history class reads as a code defect, not a fold gap. Verification is deferred to the Stage-B model (`retryPolicy.qnt`); the fold's own unit tests pin the fold against hand-computed histories. |

### `VerdictIsChannelInvariant`
*For a fixed physical failure history, every observation subset/order the
environment can produce yields the same budget verdict (requeue /
poison-on-budget / cancel / TTL-expire) and the same counter deltas.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.verdict-channel-invariant` *(new)* | **COVERS** (the statement) — **falsified by the as-built code** | Was a GAP. The rule states the invariant the design mandates; the as-built code violates it on at least one reachable history: D1 (the same exhausted timeout budget lands as `Cancelled` via E4 or `Poisoned` via E7 depending on whether the worker's `daemon_timeout` or the Job controller's `activeDeadlineSeconds` observer reports the same deadline overrun first). This is the expected Stage-B falsification, recorded here so the model run that finds it is confirming a documented defect rather than discovering a new one. The rule is added marker-first; the code is not changed in Phase 0. Adding it marker-first also puts the rule in direct conflict with `sched.termination.deadline-exceeded+2` on the wedged-worker history — recorded as rule-vs-rule contradiction C4 below, with the design-pre-committed deadline-exceeded amendment as the resolution. The G5 double-counts falsify the counter-delta half of this invariant whenever the dedup fails (the `NoDoubleCount` rows). |

### `PlacementIsAFunctionOfExclusionAndFleet`
*Whether a derivation can still be placed — and the fleet-exhaust poison —
is a function of (the per-executor exclusion set × the live eligible
fleet) only.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.dispatch.fleet-exhaust+3` | **PARTIAL → COVERS** (the exhaust-poison half; `+3` adds the empty-fleet clause) | The original audit over-claimed COVERS: the `+2` text stated the exhaustion predicate (every statically-eligible non-draining registered worker ∈ `failed_builders` → poison, kind/system/features-aware, draining excluded) but never adjudicated the empty fleet, and under a vacuous-truth reading mandated poison there — the opposite of the code, which deliberately peeks the eligible-worker iterator and returns "not exhausted" when zero statically-eligible non-draining workers are registered (`failed_builders_exhausts_fleet`: an empty pool is a provisioning transient; poisoning would brick builds during a deployment rollout). A Stage-B `Placement` invariant encoded from the `+2` text and quantified over fleet states — the design quantifies over the empty fleet explicitly — would have diverged from the code on that basic case while this map claimed the spec covers it. Amended in the Stage-A review: `+3` states the empty-fleet defer as a MUST NOT poison; `test_fleet_exhaustion_defers_under_one_shot` already exercises the empty-non-draining-fleet defer and now carries the `+3` verify marker. |
| `sched.retry.per-executor-budget` | **PARTIAL** (the exclusion half) | `failed_builders` drives `best_executor` exclusion (`assignment.rs::hard_filter` rejects workers in the set) — the rule names the set and its persistence but never states the placement-exclusion obligation as a MUST. The missing piece is rationale-grade (the exclusion is the mechanism behind the distinct-workers threshold and the fleet-exhaust predicate, both of which are normative elsewhere); recorded here, not bumped. Phase 1's `placeable(&ExclusionSet, &EligibleFleet)` extraction is the natural point to make it normative. |

### `NoDoubleCount`
*One physical executor death produces at most one accounting event per
derivation, regardless of which subset of {stream-close, heartbeat-timeout,
controller-report, backstop} observes it and in which order.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.no-double-count` *(new)* | **COVERS** (the statement) | Was a GAP. The dedup machinery (`recently_disconnected` first-report-wins removal, the `last_completed` discriminator, the race-ahead `last_completed` write, the 60 s TTL sweep, the non-promoting-reason early return) exists solely to enforce this and had no unifying rule. `sched.reassign.no-promote-on-ephemeral-disconnect+4` states one piece (a disconnect after the running build's `CompletionReport` records no `recently_disconnected` entry); the conservation statement — at most one counted accounting event per physical death — was unstated. Whether the dedup actually achieves it under every interleaving is the Stage-B model's job (the G5 fix family is the evidence it has failed at least four times). |
| `sched.reassign.no-promote-on-ephemeral-disconnect+4` | **COVERS** (the disconnect-vs-completion dedup piece) | The I-188 race: an expected one-shot exit must not be double-counted as a mid-build death. |

### `RecoveryIsTheDocumentedProjection`
*Post-failover retry state equals the documented projection of the
persisted rows: 4 counters recovered, 1 derived, 5 defaulted, the poison
set preserved exactly minus TTL-expired entries.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.recovery-projection` *(new)* | **COVERS** | Was a GAP. The projection was scattered across four rules' prose — `sched.poison.ttl-persist` (`poisoned_at`), `sched.merge.poisoned-resubmit-bounded+2` (`resubmit_cycles`), `sched.retry.per-executor-budget` ("`failed_builders` persisted to PG; infrastructure retry count is in-memory only"), `sched.timeout.promote-on-exceed+2` ("`timeout_retry_count` is in-memory only, recovery resets to 0, conservative") — and no rule stated the complete 4-recovered / 1-derived / 5-defaulted split or the no-fabrication bound. The new rule states the as-built documented projection; whether the forgiveness should survive the Phase-1 ledger is the separate `sched.retry.failover-budget` spec decision the design's Phase-0 gate requires — made at the Phase-0 exit (budgets survive failover; see the `FailoverPreservesHistory` section below), with this rule continuing to pin the as-built contract until the Phase-1b fold lands. |
| `sched.recovery.poisoned-failed-count` | **COVERS** (the build-summary half) | Recovered poisoned derivations count toward the build's `failed`, never `Succeeded`. |
| `sched.poison.ttl-persist` | **COVERS** (`poisoned_at`) | Including the expired-poison filter at reload. |

### `RecoveryNeverFabricatesFailures`
*No recovered counter exceeds what the durable rows support.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.recovery-projection` *(new)* | **COVERS** | The projection rule's equality form subsumes the inequality form: the recovered counters are exactly the projection of the persisted columns, and the projection reads only persisted evidence (no counter is invented). Note the projection is *not* a refinement of the live state in either direction: `failure_count := failed_builders.len()` both forgets same-worker repeats (under-count) and counts the permanent path's diagnostics-only `failed_builders` insert as a poison-threshold failure that the live `failure_count` never charged (over-count, see the divergence table's A6). Both directions are the documented lossy reconstruction; the invariant bounds the recovered value by the durable evidence, not by the lost live value. |

### `FailoverPreservesHistory` *(Phase-1 acceptance property — not an as-built invariant)*

*The post-failover decision is never more permissive than the pre-failover
one.* Deliberately kept off the as-built invariant list (design §3): the
as-built behavior is the documented selective forgiveness, not a bug for
the model to rediscover. It enters in Phase 1 as the ledger's acceptance
property; the Phase-0 adjudication that scopes it is now a spec rule:

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.failover-budget` *(new; the Phase-0 failover-budget adjudication)* | **COVERS** (the Phase-1 contract) | Records design §6's pre-committed adjudication (b), made at the Phase-0 exit: budgets are per-poison-cycle and SURVIVE leader failover — the new leader's fold over the durable attempt history yields the same remaining budgets the old leader would have enforced, and only the explicit reset events (admin/TTL poison clear, bounded resubmit, cache-hit clear), themselves durable history events, refresh a budget; a leader change is not a reset event. The as-built code does NOT satisfy it (the selective forgiveness `sched.retry.recovery-projection` pins is exactly the budget refresh this rule forbids), so the rule deliberately carries no `r[impl]` marker — it is a Phase-1 acceptance rule, not an as-built claim, and it joins the rules whose `verify` arrives with the Phase-1 model re-check over the ledger fold (the Phase-1b gate). The companion amendments + `tracey bump` of the two rules whose prose pins today's forgiveness (`sched.timeout.promote-on-exceed+2`, `sched.retry.per-executor-budget`) land with the Phase-1 change that makes the code satisfy it. Dependents: the D4 disposition, `sched.retry.recovery-projection` (which keeps pinning the as-built contract until then), and the Phase-1a ledger's recovery semantics. |

## Contradiction records

The code does not do what the rule says it MUST. Recorded, not fixed, and
the rule is not weakened — each row is a Phase-1 disposition input
(fix the code red-first, or amend + `tracey bump` the rule with sign-off).

| # | Rule | What the rule says | What the code does | Evidence |
|---|---|---|---|---|
| C1 | `sched.termination.deadline-exceeded+2` | The controller-reported `DeadlineExceeded` path "does NOT `reset_to_ready` --- it only promotes (so the next dispatch goes larger) and counts (so the ladder is bounded). At `max_timeout_retries` the floor is at ceiling; terminal `Cancelled` is owned by the worker-side `TimedOut` path." — i.e. the controller path performs no terminal transition. | `handle_deadline_exceeded` calls `poison_and_cascade` when `timeout_count >= max_timeout_retries` — a terminal transition, and to `Poisoned` (24 h TTL, bounded resubmit) rather than the `Cancelled` the worker-side path and `sched.timeout.promote-on-exceed+2` produce for the same exhausted budget. The off-spec poison was added by `172776b1b` to break the loop-at-cap (the rule as written loops forever when the worker is too wedged to ever send the `TimedOut` report that would own the terminal transition). The rule needs a terminal clause; the design pre-adjudicates it as `Cancelled` (design §6); neither the code nor the rule changes in Phase 0. | `rio-scheduler/src/actor/executor.rs` (`handle_deadline_exceeded`, the `>= max` arm) vs `scheduler.typ` `sched.termination.deadline-exceeded+2` |
| C2 | `sched.retry.per-executor-budget` | "Executor disconnect DOES count --- a build that crashes the daemon 3× is poisoned." | A bare disconnect (E5, `reassign_derivations`) records nothing: no `failed_builders` insert, no `failure_count` increment — it only re-reads `is_poisoned()` over failures recorded by *other* paths, so the "crashes the daemon 3×" property holds only when the worker survives long enough to report the failure before disconnecting. The compensating bound is narrower than this row first recorded (corrected in the Stage-A review): the E8 backstop scans only derivations still `Running` past max(est×3, 7800 s), so it bounds the *wedged-worker* sub-case (worker keeps heartbeating, derivation stays Running) at ≥ 7800 s per counted attempt. A hard crash closes the stream within seconds; E5 resets the node to `Ready`, the next dispatch restarts `running_since`, and the backstop clock never accumulates — and the controller's follow-up termination report for a non-resource crash reason hits the non-promoting early return and charges nothing. A derivation that deterministically kills the worker daemon with no report and no OOM/DiskPressure/DeadlineExceeded reason is therefore bounded by nothing in the retry machinery (only by an optional per-build `build_timeout`), and the as-built code falsifies `sched.retry.attempts-bounded+2`'s boundedness clause on exactly this history (pre-registered Stage-B falsification). | `rio-scheduler/src/actor/executor.rs` (`reassign_derivations`: "Disconnect ... does NOT record into `failed_builders`/`failure_count`/`retry_count`"; `handle_executor_termination`'s non-promoting-reason early return) and `rio-scheduler/src/actor/housekeeping.rs` (`tick_scan_dag` scans Running-with-`running_since` only) vs `scheduler.typ` `sched.retry.per-executor-budget` |
| C3 | `sched.retry.exempt-infra-cap` | "The `exempt_from_cap` infra-retry path (CONCURRENT_PUTPATH, `floor_outcome.promoted`) skips `infra_count++` ... A separate `exempt_infra_count` increments on every exempt attempt and poisons at `max_exempt_infra_retries`." The rule's own definition of an exempt attempt is "CONCURRENT_PUTPATH or `floor_outcome.promoted`". | A floor-promoted *controller-reported* OOM/DiskPressure (E6, `floor_outcome.promoted = true`) increments nothing — the increment block is gated on `at_cap`, and `promoted` and `at_cap` are mutually exclusive. Only the worker-reported exempt path (E2) charges `exempt_infra_count`. The promoted controller path is still bounded — by the floor ladder's own monotone doubling up to the ceiling, past which `at_cap` engages `infra_count` — but it is a different and unstated bound, and the rule's defense-in-depth purpose ("a `bump_resource_floor` bug that always returns `promoted=true`" livelocks) is unenforced on the controller channel. | `rio-scheduler/src/actor/executor.rs` (`handle_executor_termination`, the `if outcome.at_cap` gate) vs `scheduler.typ` `sched.retry.exempt-infra-cap`. Same defect seen from the fold's side: divergence D3. |

The rows above are code-vs-rule contradictions. One **rule-vs-rule**
contradiction is also on record — two normative bodies the spec now
carries that no implementation can satisfy together:

| # | Rules | The conflict | Resolution |
|---|---|---|---|
| C4 | `sched.termination.deadline-exceeded+2` vs `sched.retry.verdict-channel-invariant` | The deadline-exceeded rule assigns terminal ownership at the timeout cap exclusively to the worker-side `TimedOut` path — the controller path "only promotes and counts" — while the channel-invariance rule requires the budget verdict for a fixed physical history not to depend on which channel observed it. On the reachable wedged-worker history where the worker never sends `TimedOut` and only the controller observes the deadline overrun, the two MUSTs are jointly unsatisfiable: obeying deadline-exceeded means the controller-observed run never goes terminal while the worker-observed run of the same physical history goes `Cancelled` (a channel-dependent verdict); obeying channel-invariance means the controller path takes a terminal transition deadline-exceeded forbids. `172776b1b` went off-spec to `Poisoned` at the cap for exactly this reason (C1/D1 are the code-side view of the same knot). | The design (§6, adjudication (a)) pre-commits the resolution: Phase 1 amends `sched.termination.deadline-exceeded+2` (with the corresponding version bump) to permit terminal `Cancelled` at the cap on the controller path, dissolving the conflict in the same red-first change that fixes D1/C1. Until that lands the spec deliberately carries both bodies; Stage B encodes the as-built code (which satisfies neither exactly) and treats the D1 falsification as the documented expected outcome. |

Near-misses recorded for Phase 1 but not classified as contradictions of a
MUST: the `sched.admin.clear-poison` ordering prose (above);
`sched.sla.reactive-floor+2`'s "if the relevant dimension is already at its
ceiling, increment `infra_count` (or `timeout_count` for deadline)" — the
increment lives at the call sites after their own cap checks
(`bump_floor_or_count` never mutates a counter, by design since the at-cap
double-count fix), and for the deadline dimension the increment is
unconditional rather than at-ceiling-only (the I-200 semantics
`sched.retry.promotion-exempt+3` states); the rule describes the effect at
the wrong site and under-states the deadline arm's condition.

## Divergence catalog (the fold's adjudications)

A DIVERGENCE row is a place where two decision sites disagree such that no
single channel-invariant fold over the attempt history can reproduce both.
The fold (`rio-scheduler/src/retry_policy.rs`) implements the side the spec
mandates, or — where the spec is silent — the side judged intended; the
other side is the deviation Phase 1 must disposition (fix red-first, carry
as an `Attempt`-record asymmetry, or sign off a new policy rule). An
ASYMMETRY row is an inventory-§2.3 divergence that *is* reproducible by the
fold (the disagreeing sites handle different event classes, so the fold
encodes both behaviors); it still needs a Phase-1 disposition ((b) carried
asymmetry or (c) policy choice) but it is not a defect in the fold's sense.

| # | Kind | Site A | Site B | The fold does | The deviation |
|---|---|---|---|---|---|
| D1 | **DIVERGENCE** | E4 at `timeout_count >= max_timeout_retries` → terminal `Cancelled` (resubmit-retriable immediately, no cascade lockout beyond this build) | E7 at the same cap → `poison_and_cascade` → `Poisoned` (24 h TTL, `resubmit_cycles`-bounded resubmit) | `Cancel` for both `WorkerTimeout` and `ControllerDeadlineExceeded` cap-exhaustion — `sched.timeout.promote-on-exceed+2` already names `Cancelled` as the timeout-cap terminal state and `sched.termination.deadline-exceeded+2` assigns terminal ownership to the timeout path; the design (§6) pre-adjudicates E7's `Poisoned` as the non-conforming side | E7 produces `Poisoned`. User-visible consequence of the Phase-1 fix: a wedged-worker derivation that exhausts its timeout budget via the controller backstop becomes immediately resubmit-retriable instead of locked out for 24 h. Contradiction C1 is the same defect seen from the rule's side. |
| D2 | **DIVERGENCE** | E2's non-exempt `infra_count += 1` also stamps `last_infra_failure_at = now` (the 300 s window's anchor) | E6's at-cap `infra_count += 1` does not stamp `last_infra_failure_at` | Stamps `last_infra_failure_at` on every `infra_count` increment — the field's own documented meaning ("timestamp of the most recent InfrastructureFailure that incremented `infra_count`") and the window's purpose (measure the gap since the last *counted* failure) | E6's increment leaves the window anchored at the last E2-counted failure (or `None` if there has never been one). Observability corrected in the Stage-A review: the `!at_cap` guard tests the *current* event's floor outcome, and E2's at-cap increments do stamp the anchor — so a run of worker-reported at-cap OOMs followed by a non-at-cap infra failure > 300 s later IS forgiven (stale-but-set anchor → the reset fires and the accumulated at-cap count is wiped), as is a controller-counted run that has any earlier E2-counted failure to anchor on; only the anchor-`None` case (an exclusively controller-counted run) is not forgiven. The divergence is therefore observable today, with no change to E6's gate: the same physical at-cap OOM run followed by a sparse non-at-cap infra failure produces different `infra_count` trajectories — and a different time-to-poison — depending on which channel counted the OOMs. The adjudication is unchanged: the fold stamps the anchor on every increment. |
| D3 | **DIVERGENCE** | E2: a floor-promoted infra failure (worker-reported `CgroupOom` that successfully doubled the floor) is exempt from `infra_count` but charges `exempt_infra_count` (the budget for the budget exemption, `sched.retry.exempt-infra-cap`: "increments on every exempt attempt", where the rule defines an exempt attempt as CONCURRENT_PUTPATH or `floor_outcome.promoted`) | E6: a floor-promoted controller-reported OOM (`promoted=true` ⟹ `at_cap=false` ⟹ the increment block is skipped) charges *nothing* | Charges `exempt_infra_count` for every `floor_outcome.promoted` infra-class attempt regardless of the reporting channel — `sched.retry.exempt-infra-cap`'s "every exempt attempt" is the spec mandate, and `sched.retry.attempts-bounded+2`'s exempted-attempts-charge-the-exemption-budget clause depends on it | E6's promoted arm charges no budget (contradiction C3). Not a channel race — a cgroup-level OOM and a pod-level OOM are physically distinct events — but the two sites disagree about what a `floor_outcome.promoted` infra-class attempt charges, the existing rule mandates the E2 side, and a fold that reproduces the E6 side leaves the exemption bounded on the controller path only by the floor ladder's own length (log₂(ceiling/start) promotions before `at_cap` engages `infra_count`) — a real bound, but a different and unstated one. A `bump_resource_floor` bug that always returns `promoted=true` would livelock the controller path where the worker path poisons at `max_exempt_infra_retries`. `CountersRefineHistory` is expected to falsify on any history containing a promoted controller-reported termination. |
| D4 | **DIVERGENCE** (durable mirror) | E1's and E3's `failed_builders.insert` are mirrored to PG via `db.append_failed_worker` | E8's `failed_builders.insert` (the backstop's accounting) is in-memory only — no `append_failed_worker` call | The fold computes the in-memory set (insert on `TransientFailure`, `PermanentFailure`, `BackstopTimeout`); the durable view is the Stage-B model's PG-mirror ghost, where E8's write is a permanently-lost mirror write rather than a maybe-lost one | A backstop-recorded failure survives neither failover nor the recovery-time poison re-check: the post-failover `failed_builders` (and the derived `failure_count`) under-count by every backstop event since the last E1/E3 failure, so a derivation that wedged 2 of 3 workers pre-failover restarts its poison-threshold progress. Compounds C2's wedged-worker sub-case: the backstop is the only counted bound on the no-report *wedge* loop, and that accounting does not survive the leader change a wedging derivation can outlast; the no-report *hard-crash* loop never reaches the backstop at all (see C2), so D4 neither helps nor hurts it. |
| A5 | ASYMMETRY (inventory §2.3.5) | E1, E8 insert into `failed_builders` and increment `failure_count`; E3 inserts into `failed_builders` only (diagnostics; it poisons unconditionally anyway) | E2, E4, E5, E6, E7 deliberately do not insert | The fold inserts on `TransientFailure` / `BackstopTimeout` / `PermanentFailure` and not on the others — each event class has one consistent behavior | None for the fold. Phase-1 disposition (b)/(c): which failure classes join the placement-exclusion set is a policy choice the `Attempt` record's `outcome_class` carries. |
| A6 | ASYMMETRY (recovery) | Live `failure_count` is incremented by E1 and E8 only (same-worker repeats counted, permanent failures not counted) | Recovered `failure_count := failed_builders.len()` (same-worker repeats forgotten, the permanent path's diagnostics-only insert counted) | The fold computes the live value; `Counters::recovery_projection` computes the recovered value; the two are documented as different functions of the history | The recovered value can be both above and below the live value for the same history. Documented lossy reconstruction (`sched.retry.recovery-projection`); becomes moot when the ledger fold replaces the projection in Phase 1b. |
| A7 | ASYMMETRY (inventory §2.3.6) | Only E1 sets `backoff_until` (exponential, jittered, 5 s → 300 s) | E2/E4/E5/E6/E7/E8 requeue with no backoff (E4's longer deadline and E2's immediate-retry rationale are documented; E5/E8's are not) | The fold sets `backoff_until` on `TransientFailure` only, deterministically (no jitter — the jitter is an implementation freedom the spec permits, and the model compares modulo it) | None for the fold. Phase-1 disposition (c): uniform backoff vs uniform no-backoff is the policy choice the design flags (the 9,748-redispatch incident is the no-backoff hot-loop; the documented mitigation is the cap, not a backoff). |
| A8 | ASYMMETRY (inventory §2.3.7) | E1's poison reason is the synthesized "max_retries=N exhausted after transient failures" | E2/E3 carry the worker's actual `error_msg` | The fold's `Verdict::Poison` carries a `PoisonReason` discriminant (which budget tripped), not the message string — the reason string is diagnostics, not a counter or a verdict | None. The lost-error-message defect is the failed-logs/attempts-are-not-entities hole the Phase-1 ledger closes (the attempt row carries the message). |
| A9 | ASYMMETRY (inventory §2.3.8) | A floor-promoted infra failure is exempt from `max_infra_retries` (charged to `exempt_infra_count`) | A floor-promoted timeout is NOT exempt from `max_timeout_retries` (every timeout consumes budget) | The fold encodes both: the `exempt` flag gates the infra charge; the timeout charge is unconditional | None — deliberate and spec-covered (`sched.retry.promotion-exempt+3` states the asymmetry and its I-200 rationale). |
| A10 | ASYMMETRY (inventory §2.3.2) | `infra_count`: cap-checked before the increment at both E2 and E6 (the cap fires on failure N+1); `count`: checked-then-incremented on the retry arm at E1 (poison on failure N+1); `timeout_count`: checked-then-incremented at both E4 and E7 (terminal on timeout N+1) | `exempt_infra_count`: incremented before the cap check at E2 (poison *on* exempt attempt N) | The fold reproduces each counter's own convention exactly — no two sites disagree about the *same* counter, so this is reproducible | None for the fold. The off-by-one inconsistency in what `max_X_retries = N` means across counters is a Phase-1 unification candidate (the single `decide()` should give every budget the same fencepost). |

The three most consequential divergences are D1 (a 24 h lockout vs an
immediate retry, decided by message arrival order), D4 (the only counted
bound on the no-report wedge loop does not survive failover — and the
no-report hard-crash loop has no bound at all, see C2), and D3 (the
exemption budget that exists to bound the cap-exemption is never charged
on the controller channel).

### Stage-A dispositions for D1 / D3 / D4

The Stage-A self-review confirmed all three headline divergences against
the code and dispositioned each as **phase-1** — no immediate fix; Phase 0
leaves the production code unchanged and Stage B models it as-is.

- **D1 — phase-1.** Pre-adjudicated by the design (§6, adjudication (a)):
  the fix is a behavior change on the losing path that ships red-first
  together with the `sched.termination.deadline-exceeded+2` amendment and
  its version bump (see C4). The as-built consequence is bounded
  (`Poisoned` with a 24 h TTL, bounded resubmit, admin clear-poison), and
  D1 is the designated expected falsification of
  `VerdictIsChannelInvariant` — fixing it now would remove the
  falsification the Stage-B calibration is designed to confirm.
- **D3 — phase-1.** The same defect as contradiction C3. Today's
  controller path is still bounded (the floor ladder's monotone doubling
  until `at_cap` engages `infra_count`); the livelock requires a
  hypothetical second bug in `bump_resource_floor`; and the proper fix
  lands with the E6 rework that the two-installment attempt-correlation
  mechanism forces in Phase 1 anyway. Charging `exempt_infra_count` now
  would be a Phase-0 behavior change with no urgent production exposure.
- **D4 — phase-1.** Confirmed real: the backstop's accounting is
  in-memory only (the only `db.append_failed_worker` call sites are E1's
  `record_failure_and_check_poison` and E3's `handle_permanent_failure`),
  so backstop-recorded poison-threshold progress does not survive
  failover. The practical exposure is a slow-cadence loss of that
  progress — it requires a no-report *wedge* loop (≥ 7800 s per counted
  attempt) plus a leader failover mid-loop, and it costs wasted
  dispatches, not data loss or wrong build outputs. Fixing it now would
  change post-failover accounting mid-campaign (invalidating this row,
  the as-built model the design requires, and the G8 calibration
  premises), it intersects the `sched.retry.failover-budget` decision
  (made at the Phase-0 exit: budgets survive failover, which confirms
  D4's fix direction and folds it into the ledger work), and Phase 1a's
  `drv_attempts` ledger makes every attempt durable at the append site,
  structurally subsuming a tactical mirror write that Phase 1b would then
  delete. Stage B encodes E8's mirror write as permanently lost in the
  durable view.

### Expected Stage-B falsifications (pre-registered)

A model run that falsifies one of these is confirming a documented
as-built defect; a falsification *not* on this list is a stop-and-report
(a model-encoding bug or a new defect — triage before continuing).

- `VerdictIsChannelInvariant` — the D1 history: the timeout budget
  exhausted via E7 (controller-reported) instead of E4 (worker-reported).
- `CountersRefineHistory` — histories reaching D2 (controller-counted
  at-cap OOMs: the missing anchor stamp) or D3 (a promoted
  controller-reported termination: the missing `exempt_infra_count`
  charge); and, in the durable view only, D4 (E8's unmirrored
  `failed_builders`/`failure_count` charge — encoded as a permanently
  lost write, not a fault-injectable one).
- `AttemptsBounded` — the C2 no-report hard-crash loop: an uncounted
  disconnect-requeue cycle that never reaches the backstop, bounded only
  by an optional per-build `build_timeout`.
- The G5 double-count interleavings falsify the counter-delta half of
  `VerdictIsChannelInvariant` (and `NoDoubleCount`) only if the dedup
  encoding admits them — that is a Stage-B question to answer, not a
  pre-registered defect.

## Rules in the failure-handling inventory not load-bearing for any invariant above

- `sched.timeout.per-build`, `sched.backstop.orphan-watcher` — build-level
  wall-clock and watcher-liveness cancellation; they produce `Cancelled`
  outcomes but consult no retry counter and charge no budget.
- `sched.admin.list-poisoned`, `dash.clear-poison` — read-side surfacing of
  the poison set.
- `sched.executor.deregister-reassign`, `sched.heartbeat.phantom-drain+2`,
  `sched.ephemeral.no-redispatch-after-completion` — the executor-lifecycle
  events that *trigger* E5; their own obligations are about the executor
  map, not the retry state.
- `sched.db.assignment-terminal-on-status+2`, `sched.db.clear-poison-batch`
  — the PG write discipline for the mirror columns; load-bearing for the
  Stage-B `fault-persist` regime, not for the in-memory fold.
- `sched.recovery.fetch-max-seed+4`, `sched.lease.*` — the leader-election
  layer the failover action composes with by assume–guarantee.
- `builder.retry.daemon-transient`, the substitute-fetch backoff loop — the
  worker-local and per-path retry loops invisible to the scheduler's
  counters (a fifth and sixth retry loop; out of the fold's scope by
  construction).
- `sched.sla.reactive-floor+2` — the floor ladder itself. The fold consumes
  its `{promoted, at_cap}` outcome as an input on the event (the design's
  `classify(event, floor_outcome)` decision); the ladder's own monotonicity
  and ceiling invariants are G6's subject and stay with the floor's unit
  tests unless the Stage-B model includes the floor regime.

## Out of scope for the fold (recorded so the omission is deliberate)

- **The substitution-failure path** (`handle_substitute_complete(ok=false)`,
  `substitute_tried`, `topdown_pruned`): not a `RetryState` counter, a
  one-shot budget outside the 10, and a moving target under `harden-subst`
  (the design's Phase-0a sequencing decision). The fold's event alphabet
  excludes it; the Phase-1 collapse carves it out until `harden-subst`
  lands.
- **The cancel paths** (`handle_cancel_build`, `cancel_build_derivations`,
  the per-build timeout): they produce `Cancelled` terminal states but are
  not failure-history decisions (no counter is consulted or charged).
- **The poison reason string and the lost `final_line_count`**: the
  attempts-are-not-entities hole (inventory §6). The fold's verdict carries
  the budget discriminant only; the ledger row carries the rest in Phase 1a.

## Verify-marker status

*(Stage-A snapshot; superseded by the Stage-B wiring — see the Stage-B
verify-marker status subsection at the end of this document.)*

New rules whose verification was deliberately deferred to the Stage-B model
(`retryPolicy.qnt`) and therefore expected to appear in
`tracey query untested` until the model's checks were wired:
`sched.retry.verdict-channel-invariant` (expected to *falsify* on the
as-built encoding — D1), `sched.retry.no-double-count` (expected to falsify
or hold depending on the dedup encoding — the G5 family),
`sched.retry.recovery-projection` (the model's failover action),
`sched.poison.cascade-dependents` (the model's cascade action; the existing
keep-going and recovery-cascade tests verify the build-level consequences
but not the reaches-exactly-the-dependents set property). All four are now
wired (`nix/quint.nix`, the `quint-retry-policy-*` checks).

`sched.retry.transient-budget`, `sched.retry.attempts-bounded+2`, and
`sched.retry.counters-refine-history+2` carry `r[verify]` markers on the
referenceFold's unit tests (`rio-scheduler/src/retry_policy.rs`), which pin
the fold against hand-computed histories covering every counter, the window
reset (including the exempt-event fall-through), the exemption, a poison, a
TTL expiry, and a per-executor exclusion.
The model's exhaustive form of `counters-refine-history` — the *live*
counters compared against the fold over every observation ordering — is the
Stage-B `quint-retry-policy-worker` check; the unit tests verify the fold,
the model verifies the code against the fold.

## Stage-B results (`retryPolicy.qnt`, the as-built model)

The model is `docs/spec/models/retryPolicy.qnt`: the nine entry points
encoded as the code implements them (every divergent arm included), the
reference fold carried as a specification ghost advanced at each observed
accounting event, the signal-channel dedup state, the four PG mirror
columns, the leader-failover selective forgiveness, the resets, and the
cascade to one dependent. Scope boundaries and encoding decisions
(the floor ladder abstracted to a bounded promotion budget per design §3's
G6 pre-registration; the substitution and cancel paths out of scope; the
fold ghost re-seeded from the recovered projection at failover so
`countersRefineHistory` is an intra-tenure invariant; dispatch-time and
recovery-time poisons excluded from the fold-refinement comparisons and
covered by their own invariants) are documented in the model header. Four
regimes are checked exhaustively under TLC and wired into `nix/quint.nix`
(`quint-retry-policy-*`): worker-channel (two slots, every failure
worker-reported), dual-channel (pod deaths, controller reports with
race-ahead/late/lost delivery, wedge backstop, crash, one slot), crash
(the C2 loop in isolation), and fault-persist/failover (one failover, one
lost mirror write). Deterministic reproducer runs pin every documented
divergence shape; the named-run checks replay them in CI.

### Verdict table

Distinct-state counts are as measured at the introducing commit (also in
that commit's message and the CI transcripts): worker 376,318 distinct
(916,846 generated), dual 3,112,250 (7,549,719), crash 60 (79), failover
9,228,949 (26,783,975).

| Design invariant | Model form | worker | dual | crash | failover |
|---|---|---|---|---|---|
| `AttemptsBounded` (charge discipline) | `attemptsChargedOnce` | HOLDS | HOLDS | HOLDS | HOLDS |
| `AttemptsBounded` (boundedness clause) | `attemptsBoundedGlobal` | not checked (no uncharged end in the alphabet) | not checked (subsumed by the crash regime) | **FALSIFIES-AS-PRE-REGISTERED** — C2 (`c2CrashLoopRun`) | not checked |
| `PoisonIsTerminalUntilCleared` | `poisonIsTerminalUntilCleared` | HOLDS | HOLDS | HOLDS | HOLDS |
| `CascadeReachesExactlyTheDependents` | `cascadeReachesExactlyTheDependents` | HOLDS | HOLDS | HOLDS | HOLDS |
| `CountersRefineHistory` | `countersRefineHistory` | HOLDS | **FALSIFIES-AS-PRE-REGISTERED** — D2/D3 histories, plus the D1 history's `poisoned_at` (see the deviations note) (`d2AtCapAnchorRun`, `d3PromotedChargesNothingRun`, `d1ControllerTimeoutCapPoisonRun`, `lateInstallmentAfterRedispatchRun`) | HOLDS (vacuously: nothing charges) | not checked (falsifies exactly as in dual) |
| `VerdictIsChannelInvariant` | `verdictMatchesFold` | HOLDS | **FALSIFIES-AS-PRE-REGISTERED** — D1 (`d1ControllerTimeoutCapPoisonRun`) | HOLDS | not checked |
| `PlacementIsAFunctionOfExclusionAndFleet` | `placementSound` (+ the E9 action's empty-fleet-defers guard) | HOLDS | HOLDS | HOLDS | HOLDS |
| `NoDoubleCount` | `noDoubleCount` | HOLDS (vacuous — no deaths in the alphabet) | HOLDS | HOLDS | HOLDS |
| `RecoveryIsTheDocumentedProjection` | `recoveryIsTheDocumentedProjection` | vacuous (no failover) | vacuous | vacuous | HOLDS |
| `RecoveryNeverFabricatesFailures` | `recoveryNeverFabricatesFailures` | HOLDS | HOLDS | HOLDS | HOLDS |
| durable-mirror completeness (D4's surface) | `durableMirrorsCharges` | HOLDS | **FALSIFIES-AS-PRE-REGISTERED** — D4 (`d4BackstopUnmirroredRun`) | HOLDS | not checked (falsifies via D4 and via injected mirror faults) |

The HOLD column entries are exhaustive TLC results over the regime's full
reachable space; the falsification entries are wired as expect-violation
checks (`quint-retry-policy-divergence-*`, `quint-retry-policy-crash-unbounded`)
that pass only while the documented defect is still reproducible, and flip
to HOLD checks when Phase 1 lands the corresponding fix.

**Footnote on the `RecoveryIsTheDocumentedProjection` row** (added by the
Phase-0 exit review): the model form `recoveryIsTheDocumentedProjection`
is the counter-projection *equality* only — it conditions on the
post-recovery `dStatus` (poisoned rows compared against
`recoveryProjectionPoisoned`, Ready rows against
`recoveryProjectionNonTerminal`), so it cannot see a poisoned row that
recovery failed to reload at all. Design §3's invariant additionally
states "the poison set is preserved exactly minus TTL-expired entries"
and pre-registers dropped Poisoned nodes (the `891a6520d` shape) as a
falsifier; that clause was narrowed away by this encoding and its HOLDS
verdict here does not cover it. The clause is covered since Stage C by
`recoveryPreservesPoisonStatus` (added with the calibration, checked in
the failover regime, non-vacuity guarded by
`quint-retry-calib-g8-poison-reload`). The clause-coverage audit below
records the same comparison for every design-§3 invariant.

### Design-§3 clause-coverage audit (added by the Phase-0 exit review)

One pass over the design-§3 invariant prose, clause by clause, against
the Stage-B encodings — run after the exit review found the poison-set
clause above had been silently narrowed at Stage B. For each clause:
the model `val` carrying it, or the explicit note that it is carried
structurally (by the shape of the transition relation rather than a
checked invariant) or not encoded. Findings beyond the poison-set clause
are listed as narrowings with a benign / hides-known-defect verdict;
resolution of all of them is left to Phase 1 — nothing below changes a
Stage-B verdict.

| Design invariant | Clause | Carried by |
|---|---|---|
| `AttemptsBounded` | no attempt charges the same budget twice | `attemptsChargedOnce` (the `doubleCharge` conjunct, every observation kind) |
| `AttemptsBounded` | every counted attempt charges at least one budget | `attemptsChargedOnce` — **narrowed**: the charge-or-terminal conjunct's antecedent covers worker-reported (OE1–OE4) and backstop (OE8) observations only; controller-report (OE6/OE7) and disconnect (OE5) observations are outside it, so the two as-built uncharged arms (D3's promoted controller termination, C2's bare disconnect) do not falsify it. Both are known and surfaced elsewhere (D3 via `countersRefineHistory`, C2 via `attemptsBoundedGlobal`), so the narrowing hides no defect the record does not already carry — but a *new* uncharged arm on those channels would be invisible to this clause. Phase 1: the `decide()` re-check makes the clause total over the event alphabet. |
| `AttemptsBounded` | every retry loop is bounded | `attemptsBoundedGlobal` — checked only in the crash regime (the other regimes' "not checked" cells in the verdict table are deliberate; the falsification is the pre-registered C2 shape) |
| `PoisonIsTerminalUntilCleared` | no charge while poisoned; stamp present | `poisonIsTerminalUntilCleared` |
| `PoisonIsTerminalUntilCleared` | the only exits are the sanctioned ones (TTL, admin clear, cache hit, bounded resubmit) | structural — no checked `val`; no action's effect leaves `DPoisoned` except the sanctioned ones, by construction of the transition relation. Benign narrowing: a model-encoding regression that added an unsanctioned exit would surface only through the named runs / review, not through an invariant. |
| `CascadeReachesExactlyTheDependents` | every not-yet-started dependent cascaded; no node outside the set cascaded | `cascadeReachesExactlyTheDependents` (both conjuncts) |
| `CascadeReachesExactlyTheDependents` | reach at depth ≥ 2 through non-terminal nodes; never preempt started nodes; every interested build observes a terminal state | not encoded / vacuous at model scale (single dependent at depth 1, never dispatched; build-level observability out of scope) — pre-priced in the model header and the G3 NOT-ENC rows; benign |
| `CountersRefineHistory` | the 10 counters equal the fold of the observed history | `countersRefineHistory` — documented encoding decisions, not narrowings: `poisonedAt` excluded when the poison source is outside the fold's event alphabet (dispatch/recovery poisons), and the fold ghost re-seeded at failover (intra-tenure scope; the cross-failover loss is the recovery invariants' subject) |
| `VerdictIsChannelInvariant` | same budget verdict for every observation order | `verdictMatchesFold` (single-trace refinement against the channel-invariant fold), scoped to the terminal budget verdicts; the requeue-side D3 divergence is deliberately left to `countersRefineHistory` (header encoding decision) |
| `VerdictIsChannelInvariant` | same counter deltas for every observation order | not carried by `verdictMatchesFold` — delegated to `countersRefineHistory` + `noDoubleCount` (as the Stage-B pre-registration already noted for the G5 shapes); benign, documented |
| `PlacementIsAFunctionOfExclusionAndFleet` | the fleet-exhaust poison fires only on a non-empty, fully-excluded eligible fleet | `placementSound` + the E9 action's empty-fleet-defers guard (the guard is structural; the draining half is additionally calibrated by the G7 override's restricted-alphabet invariant) |
| `PlacementIsAFunctionOfExclusionAndFleet` | placement excludes `failed_builders` | structural (`placeable` mirrors `hard_filter`); benign |
| `PlacementIsAFunctionOfExclusionAndFleet` | eligibility is kind/system/features-aware | **narrowed** — static eligibility is uniform across slots (`eligibleFleet` implements only the registered/non-draining half of design §3's pre-registered G7 predicate). Hides the known `a62631c90` defect class (heterogeneous-fleet exhaust mis-fire), which is exactly what its NOT-ENC row now records; re-evaluate if Phase-1 placement work adds heterogeneous eligibility. |
| `NoDoubleCount` | one death ⇒ at most one counted accounting event, across channels and orders | `noDoubleCount` (per-slot `deathCharges`); per-derivation and per-death coincide at model scale (one building derivation) |
| `RecoveryIsTheDocumentedProjection` | 4-recovered / 1-derived / 5-defaulted equality | `recoveryIsTheDocumentedProjection` |
| `RecoveryIsTheDocumentedProjection` | TTL-expired poison cleared, not reloaded | structural (the failover action's expired-poison arm) + calibrated by the `f9adf3c76` override falsifying the projection equality; benign |
| `RecoveryIsTheDocumentedProjection` | the poison set is preserved exactly minus TTL-expired entries | **narrowed at Stage B** (the footnote above) — the projection equality conditions on the recovered status and cannot see a dropped row. This is the one narrowing that hid a design-stated clause; covered since Stage C by `recoveryPreservesPoisonStatus`. |
| `RecoveryNeverFabricatesFailures` | no recovered counter exceeds what the durable rows support | `recoveryNeverFabricatesFailures` — **narrowed** to the exclusion set (`failed_builders` ⊆ ever-charged; recovered ⊆ persisted): the other recovered counters are copies of single columns whose only writers mirror live increments, so the bound is structural for them. Benign; no known defect behind it. |

Audit verdict: the poison-set clause is the only Stage-B narrowing that
hid a design-stated clause behind a HOLDS verdict. The other narrowings
found are either documented scope/encoding decisions, structural-by-
construction properties, or correspond to defects already pre-registered
and surfaced through other invariants (D3, C2) or already dispositioned
rows (a62631c90); none requires a Stage-B re-run, and their resolution
(making the charge-discipline clause total over the event alphabet,
deciding whether the sanctioned-exits clause and the static-eligibility
dimension deserve checked encodings) belongs to the Phase-1 model
re-targeting.

### Pre-registered falsifications: confirmed vs not reachable

- `VerdictIsChannelInvariant` via the D1 history — **confirmed**
  (controller-observed timeout-cap exhaustion lands `Poisoned` where the
  fold and the worker-observed path produce `Cancelled`).
- `CountersRefineHistory` on D2 histories — **confirmed** (at-cap
  controller termination charges `infra_count` without stamping
  `last_infra_failure_at`).
- `CountersRefineHistory` on D3 histories — **confirmed** (promoted
  controller termination charges nothing; the fold charges
  `exempt_infra_count`).
- `CountersRefineHistory` on D4, durable view — **confirmed**, surfaced
  through the dedicated `durableMirrorsCharges` invariant (E8's
  `failed_builders` charge never reaches PG; in the fault regime a lost
  `append_failed_worker` produces the same shape).
- `AttemptsBounded` on the C2 no-report crash loop — **confirmed**
  (uncounted dispatch–crash–requeue cycles exceed any budget-justified
  bound; nothing in the retry machinery ever charges).
- `RecoveryIsTheDocumentedProjection` / `NoDoubleCount` via the G8/G5
  shapes — **not falsified**: the as-built dedup
  (`recently_disconnected` first-report-wins removal, `last_completed`
  suppression, the non-promoting early return, pod-identity-scoped
  correlation) holds `NoDoubleCount` over every observation subset and
  order the dual regime explores, and the failover projection matches the
  code's two recovery constructors over the fault-persist regime. This is
  the expected outcome the map's pre-registration left open ("only if the
  dedup encoding admits them"); the G5/G8 fix families are exercised by
  the Stage-C reverts, not falsified as-built.

### Witness results

Reachability is machine-checked by the `quint-retry-policy-witness-*`
expect-violation checks plus the named runs: every budget terminal
(distinct-worker threshold, non-exempt infra cap, exempt-infra cap,
worker-reported timeout `Cancelled`, controller-reported timeout poison,
fleet exhaust), the I-127 window reset and its exempt fall-through, the
TTL expiry, the cache-hit clear, the resubmit cycle, the cascade, the
race-ahead and late-installment controller deliveries, the dual
observation of one death, a failover landing on a non-empty under-budget
history, and a lost best-effort mirror write. One witness is deliberately
NOT wired: `noTransientCapPoison` (see the next section).

### What Stage B found that Stage A's artifacts had wrong or incomplete

- **The poisoned-row recovery projection does not recover `count`.**
  `from_poisoned_row` constructs the retry state from `resubmit_cycles`,
  `failed_builders` and `poisoned_at` only (plus the derived
  `failure_count`); it never reads `derivations.retry_count`. The
  `sched.retry.recovery-projection` rule text and
  `retry_policy.rs::Counters::recovery_projection` both state the
  4-recovered split unconditionally, which is accurate for non-terminal
  rows (`from_recovery_row`) but over-claims for poisoned rows. The model
  encodes the code (two distinct projections); the rule prose and the
  fold's doc comment should be tightened in Phase 1 (no behavioural
  consequence today: a recovered poisoned node's `count` is reset by the
  resubmit path before it can matter).
- **The per-cycle transient cap is unreachable under production defaults.**
  With `require_distinct_workers = true` and `hard_filter` excluding
  `failed_builders` from placement, the same worker is never given the
  derivation twice in a poison cycle, so the distinct-worker threshold
  always fires at or before the moment `count` could reach `max_retries`
  (threshold 3 vs `max_retries` 2 in production; threshold 2 vs 2 at
  model scale). The "max_retries exhausted" poison arm is live only in
  the non-distinct (dev) mode or if placement stops excluding failed
  builders. The `noTransientCapPoison` witness is therefore not wired;
  the arm itself is spec-mandated (the final clause of
  `sched.retry.transient-budget`) and defaults-shadowed rather than
  deletable — see the keep-and-document entry in the Phase-1 input list.
- **The D1 history also falsifies `CountersRefineHistory`.** The as-built
  E7 cap poison stamps `poisoned_at` where the fold's adjudicated verdict
  is `Cancel` (no stamp), so the counter vector diverges on the same
  history that falsifies the verdict invariant. Same documented defect,
  second surface; recorded here so the Stage-B falsification bookkeeping
  is exact (the pre-registered list attributed counter falsifications to
  D2/D3/D4 only).
- **A late controller report can poison a derivation while its next
  attempt is in flight.** The E6/E7 status guard admits
  `Ready|Assigned|Running`, and the `recently_disconnected` correlation is
  per-derivation, not per-execution — so a report for attempt N's death
  charges (and at the cap, poisons) while attempt N+1 is running; the
  in-flight execution becomes a zombie whose eventual completion the
  status guard drops. As-built behaviour (the model's consistency
  invariant deliberately permits it); it is the strongest concrete
  illustration of the two-installment correlation gap the design's
  Phase-1 `exec_id` mechanism closes.
- **Executor-slot identity needed pod-identity scoping to stay faithful.**
  Encoding executors as reusable slots initially allowed a stale
  controller report to be race-ahead-matched to a *later* incarnation of
  the slot and a deadline report to consume an *older* incarnation's
  `recently_disconnected` entry — interleavings impossible in production,
  where every pod has a fresh name. The model marks a pending report
  stale across respawn and requires the entry consumed by a report to be
  the report's own death's; without those two guards `noDoubleCount`
  falsifies on artefact interleavings (a finding about the Stage-A plan's
  "2 executors" framing, not about the code).

### Stage-B verify-marker status

The four rules listed above as deferred to the model are now wired:
`sched.retry.counters-refine-history+2`, `sched.retry.transient-budget`
and `sched.retry.attempts-bounded+2` on `quint-retry-policy-worker`
(the model-checked refinement over the shared alphabet, on top of the
referenceFold unit-test markers), `sched.retry.no-double-count` and
`sched.poison.cascade-dependents` on `quint-retry-policy-dual`,
`sched.retry.recovery-projection` on `quint-retry-policy-failover`, and
`sched.retry.verdict-channel-invariant` on
`quint-retry-policy-divergence-verdict` (an expect-violation check while
the as-built code still carries D1; it flips to a HOLD check with the
Phase-1 fix).

## Post-integration re-validation: harden-subst's substitution-walk changes

Stage A and Stage B were derived from the pre-`harden-subst` tree. This
section records the re-validation of the Stage-A artifacts (the protocol
inventory, this map, the `referenceFold`) and the Stage-B model against the
integrated tree, after `formal-sprint` was rebased onto `harden-subst`
(`ebb0270eb`). Substitution-walk commits checked: `01344dacd` (walk failure
gated on wanted seeds only), `a62fcf7e6` (error / retry-exhaust forgiveness
gates), `317b9cdd3` (forgiven-seed re-check against the post-walk wanted
set), `7489817da` (contradicted-NotFound retry + demotion-reason taxonomy),
`aae914cae` (downgraded-walk re-spawn for terminal / topdown-pruned revert
targets), `d8c82ae7c` (stale-Completed re-substitution routed on the wanted
subset), `a50f1f590` (not_found demotion-label honesty), plus the base
wanted-outputs plumbing they build on (`91e8daae4`, `18d6c257d`,
`01fbc008c`, `99949da37`, `024172387`, `843dac621`).

### Verdict: the fold, the model, and the E1–E9 alphabet are unaffected

- **Which events reach E5.** E5 (`reassign_derivations`,
  `actor/executor.rs`) is byte-identical to the pre-integration code:
  harden-subst touches neither `actor/executor.rs` nor `actor/completion.rs`,
  `actor/housekeeping.rs`, `actor/floor.rs`, `actor/event.rs`,
  `state/executor.rs`, nor `retry_policy.rs`. No substitution event reached
  E5 before the integration and none does after; the substitution-failure
  path remains the *adjacent decider* (`handle_substitute_complete`) this
  map already excludes from the fold ("Out of scope" above).
- **Charging semantics encoded by the fold.** Unchanged and still accurate.
  Inside `actor/dispatch.rs` and `actor/merge.rs` the harden-subst diff
  contains no edits to the fold-relevant sites: no `RetryState` counter
  increments, no `retry.clear()` cache-hit clear sites, no
  `backoff_until` writes, no `insert_drv_execution` / exec_id minting, no
  `poison_and_cascade` callers, no `failed_builders_exhausts_fleet`
  changes. A substitution failure still charges no `RetryState` counter —
  it consumes only the `substitute_tried` one-shot, which the fold's event
  alphabet deliberately excludes. The fold and `retryPolicy.qnt` therefore
  need no re-derivation; the Stage-B verdict table stands.
- **Spec-rule ids.** The rule sets are disjoint: harden-subst
  adds/bumps `gw.conn.*`, `gw.dag.reconstruct+3`, `sched.merge.wanted-outputs`,
  `sched.merge.stale-completed-verify+4`, `sched.substitute.detached+3`,
  `builder.seccomp.localhost-profile+3`; none collides with the
  `sched.retry.*` / `sched.lease.*` / `store.log.*` / `obs.log.*` ids this
  campaign added (base-id intersection is empty; `tracey-validate` is the
  mechanical check).
- **Migration number.** The integrated tree's migration sequence is
  `062_derivation_wanted_outputs` (harden-subst), `063_leader_generation_claims`,
  `064_log_chunks`, `065_drop_drv_logs`. The retry-formal attempt-ledger
  migration (Phase 1a) takes **066**.

### What did change: the substitution-failure adjacent decider

The changes alter *when and how often* `handle_substitute_complete`'s
failure arm fires and *what it observes* — not what it charges. For the
Stage-C executor re-deriving the inventory's substitution rows
(§1.3 `substitute_tried` / `topdown_pruned` / the SUBSTITUTE_FETCH loop,
and the §2.1 adjacent-decider row), the corrections are:

1. **Signature and command shape.** `SubstituteComplete` now carries
   `forgiven: Vec<String>` (the forgivable seeds that actually failed and
   were forgiven), and the handler is
   `handle_substitute_complete(drv_hash, ok, forgiven)`. The handler
   re-checks `forgiven` against the node's *current* wanted paths: if any
   forgiven seed has become wanted since the walk was spawned, an `ok=true`
   completion is downgraded to the failure arm (`forgiven_now_wanted`).
2. **The failure arm fires less often for genuine misses.** A failed seed
   that is *unwanted* (outside the post-merge wanted union) is forgiven on
   its first failure of any kind and no longer fails the walk; NotFound
   responses now consume the same 8-attempt backoff ladder
   (`SUBSTITUTE_FETCH_MAX_ATTEMPTS`) as transient errors before the walk
   gives up. Only wanted-seed failures (after retry exhaustion) and
   reference-BFS holes still produce `ok=false`.
3. **The failure arm fires *more* often for stale-Completed nodes.**
   `verify_preexisting_completed`'s routing now forgives unwanted recorded
   outputs the same way its reset decision does, so a stale Completed node
   whose *wanted* outputs are substitutable is routed to the detached
   re-substitution walk (and therefore to this handler) instead of being
   pushed straight to Ready/from-source dispatch.
4. **The `substitute_tried` one-shot is no longer charged on every
   failure-arm entry.** The `forgiven_now_wanted` downgrade reverts
   *without* setting `substitute_tried` (the next dispatch pass
   re-substitutes with the corrected forgivable set), and when the
   downgrade's revert target is terminal (DependencyFailed) or
   topdown-pruned, the handler re-spawns the walk immediately instead of
   reverting to a terminal state. The inventory's "the substitution-failure
   retry budget is exactly 1" claim needs that qualification: one charged
   failure still flips the one-shot, but downgrade/re-spawn passes do not
   consume it.
5. **The topdown-pruned arm has an exception.** "On substitute failure a
   topdown-pruned root fails the whole build" now holds only for
   non-downgrade failures (`!forgiven_now_wanted`); the downgrade case
   re-spawns instead, precisely to avoid dispatching a root from source
   with an unscheduled dependency closure.
6. **What the failure arm observes.** Demotions now carry a reason
   taxonomy (`rio_scheduler_substitute_demotions_total{reason}` with
   `not_found` / `not_found_infra` / `error` / `exhausted`, classified by
   `demotion_reason()`), and a bare `not_found` no longer implies every
   upstream missed (the store reports the same message for
   skipped-substitution cases counted in
   `rio_store_substitute_skipped_total`).
7. **Line-number citations drift.** The inventory's `dispatch.rs` /
   `state/derivation.rs` line references predate the +541/+122-line
   substitution rewrite and the LogService/lease deltas; re-derive them
   against the integrated tree rather than patching individual numbers.

None of these corrections touches the fold's input alphabet, the ten
`RetryState` counters, the poison/cascade tails, or `drv_executions`
stamping, so they are inventory-row corrections for Stage C, not model or
fold changes.

### 2026-05-25 re-validation: live effective wanted set and chain-scoped never-forgive

The second harden-subst rebase brings in thirteen further commits. The
scheduler-side ones (`71587d8a0`, `aa3131697`, `427129fc9` per-build wanted
contributions + the live effective wanted set; `b8d353bd6` stale-Completed
forgiveness gated on it; `c54a3d585` its spec rule; `a3fa4b6c1`, `abdd7ada3`
chain-scoped `never_forgive_paths` and spent-forgiveness clearing) change the
forgiveness/wanted-set semantics the previous subsection described; the
gateway connection-bounding and KEDA commits touch nothing under
`rio-scheduler/src/`. Re-checked against the merged tree:

- **E5 and the fold's charging semantics: unaffected.** `actor/executor.rs`
  (E5), `actor/housekeeping.rs`, `actor/floor.rs`, `actor/event.rs`, and
  `retry_policy.rs` remain untouched. Unlike the first batch, harden-subst
  now does edit `actor/completion.rs` — but the edit is a single
  DAG-bookkeeping insertion ahead of the verdict dispatch (an accepted
  worker verdict clears the node's `never_forgive_paths` as a chain
  ending); it adds no `RetryState` mutation, no ledger append, and does not
  change which completion arm fires. The new dispatch.rs/merge.rs/dag
  edits likewise add no `RetryState` counter writes, no `retry.clear()`
  sites, no `backoff_until` writes, no exec_id minting, no
  `poison_and_cascade` callers (the one `clear_poison` mention is a
  comment on pre-existing re-spawn behaviour). The `substitute_tried`
  one-shot charging is unchanged: forgiven-now-wanted downgrades still
  revert without setting it. The substitution-failure path therefore stays
  the adjacent decider, outside the fold's event alphabet — the carve-out
  holds.
- **Deltas to the previous subsection's notes (1)–(3).** The wanted-set
  source for the spawn-time forgivable complement, the post-walk
  `forgiven`-seed re-check, and the stale-Completed forgiveness/routing is
  now the LIVE effective wanted set — `effective_wanted` over live
  interested builds' per-build contributions (`wanted_by_build`), falling
  back to the stored node-level union — rather than the post-merge stored
  union; "no interested build wants it" reads "no live interested build
  wants it" throughout. Additionally a path that already triggered a
  forgiven-now-wanted downgrade is excluded from later walks' forgivable
  sets for the rest of that substitution chain (`never_forgive_paths`,
  cleared at every chain ending and re-cleared by the stale-Completed
  reset that opens the next chain), and the downgrade-termination argument
  in note (1) now rests on that set rather than on the union's
  monotonicity. Notes (4)–(7) — including the charging qualification in
  (4) — are unaffected. Stage C inherits these as additional inventory-row
  corrections on top of the Phase-1 "substitution-path corrections" list.
- **Phase-1a ledger rows: nothing new recorded for substitution
  failures.** `handle_substitute_complete` still has no append site, and
  `substitution` is deliberately absent from the `OutcomeClass` alphabet
  (migration 066). The only substitution-adjacent ledger writes remain the
  `cache_hit_clear` reset rows in the cached-hit / re-probe lanes; their
  trigger predicate now evaluates the live effective set, so *when* such a
  reset fires can shift (a terminal build's wide wants no longer pin a
  node), but the row contents, the one-transaction clear-poison shape, and
  the no-charge semantics are unchanged. The new `never_forgive_paths`
  clears that land beside those lanes are DAG bookkeeping only.

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
| `c13f6a277` (unchanged) | floor-promoted transient failures consumed max_retries (I-213, the E1 promotion exemption) | NOT-ENC | — (the floor outcome exists only on OOM-class events in the model and in the reference fold: `processReport` admits `promoted` for the cgroup-OOM class only and the fold's transient event carries no floor outcome, so the pre-fix "a promoting transient failure still charges `count`" behavior is not expressible as a delta of the as-built model and a re-introduction would not falsify any invariant here. Re-dispositioned from ENC-A in the Phase-0 exit review: the previously named covering override (`retryCalibG1DisconnectCharges`) reverts a different entry point (E5) and different counters (`failed_builders`/`failure_count`), sharing only the I-213 incident. Coverage stays with the `handle_transient_failure` promotion-exempt unit tests (`sched.retry.promotion-exempt+3`, `actor/tests/completion.rs`) — the same non-model-vehicle treatment as G6; the Phase-1 choice is in the NOT-ENCODED dimensions list. The disconnect/eviction half of I-213 remains the `8d38cb999` row below.) | — | n/a |
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
| `33b1f855c` (unchanged) | cascaded dependency failures didn't finalize retained exec logs | SUBS | — the in-scheduler log-buffer/`drv_logs` machinery this patched was deleted by harden-logs (LogService owns logs; 065_drop_drv_logs) | — | n/a |
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

### Phase-1 input list

What the calibration adds to the Phase-1 plan beyond the divergence
dispositions already recorded above (D1–D4, C1–C4, A5–A10):

- **Adjudications recorded at the Phase-0 exit (first-class Phase-1
  inputs — both design-§5 gate items, both now made):**
  - **Failover-budget semantics (`sched.retry.failover-budget`, new
    rule): budgets are per-poison-cycle and SURVIVE leader failover.**
    Only the explicit reset events (admin/TTL poison clear, bounded
    resubmit, cache-hit clear) — themselves durable history events —
    refresh a budget; a leader change is not a reset event. This is
    design §6's pre-committed adjudication (b), made and recorded as a
    spec rule at the Phase-0 exit (see the `FailoverPreservesHistory`
    section above): the only choice consistent with
    `FailoverPreservesHistory` as a Phase-1 acceptance property — the
    durable ledger exists precisely so the new leader's fold matches the
    old leader's. The as-built code does NOT satisfy it (the selective
    forgiveness stays the as-built contract, `sched.retry.recovery-projection`,
    until Phase 1b), so the rule has no `r[impl]` today and joins the
    rules whose `verify` arrives with the Phase-1 model re-check over
    the ledger fold. Phase-1 deliverables it pins: the ledger's recovery
    semantics (no budget refresh on a leader flap), the D4 disposition,
    and the amendment + `tracey bump` of
    `sched.timeout.promote-on-exceed+2` and
    `sched.retry.per-executor-budget` in the change that lands the fold.
  - **C2 (the no-report hard-crash / disconnect-only loop):
    adjudicated.** In the replacement, an attempt whose report never
    arrives MUST be charged to a budget once its failure is established
    — by the controller termination report's classification, the
    disconnect classification, or the backstop, whichever arrives first
    — so the loop is bounded. This enforces the existing
    `sched.retry.per-executor-budget` "Executor disconnect DOES count"
    MUST rather than changing policy (no rule amendment for the
    direction; design §4's "charged to no budget" sentence is annotated
    as superseded by this adjudication). It is required before the
    Phase-1b "all invariants hold" gate is meaningful: the as-built loop
    falsifies `attemptsBoundedGlobal` in the crash regime, and
    `quint-retry-policy-crash-unbounded` flips to a HOLD check when
    Phase 1 lands the charging. Which specific budget/cap a
    disconnect-established failure charges is left to the Phase-1 plan,
    which must say (with a rule amendment + `tracey bump` if the choice
    amends a stated budget's definition; it may be folded into the A5
    failed-builders-membership disposition if that disposition answers
    boundedness explicitly). The E5 re-check item below references this
    adjudication; settling it does not by itself license that deletion.
    **Phase-1b record (T-1b.11, decision P1):** the charge is the
    threshold/exclusion budget — `failed_builders[executor]` +
    `failure_count`, nothing else — applied by the fold's
    `executor_crash` arm once the failure is established. The
    establishment vehicles are the correlation-TTL sweep (which now
    follows its installment with the same in-transaction
    `decide()` + status persist as every collapsed site) and the E8
    backstop (which already charges and decides at its own site); the
    controller's non-promoting report deliberately does NOT establish,
    preserving the classification window for a promoting or
    DeadlineExceeded report for the same death. Unestablished
    `disconnected` rows stay uncharged. The A5 membership question for
    this class is folded into the exclusion-membership clause; the spec
    wording that makes the membership explicit ("an established executor
    crash with no report joins `failed_builders` and counts toward the
    poison threshold") is owned by T-1b.12a's single
    `sched.retry.per-executor-budget → +2` amendment (the one bump P1
    commits to) — T-1b.11 itself made no spec edit, relying on the
    existing "Executor disconnect DOES count" MUST.
- **Mechanisms probed for redundancy (neither is a free deletion):**
  - E5's poison-threshold re-check in `reassign_derivations`: the
    machine-checked claim (the b09c5b312-X6 probe) is only that the
    disconnect/force-drain-*triggered* arm never fires as-built, and only
    over the probe's restricted alphabet (OE5-scoped by construction, no
    failover, no lost-PG-write histories). The same `should_poison` block
    is the ONLY threshold gate on the backstop (E8) path —
    `tick_process_backstop_timeouts` records
    `failed_builders`/`failure_count` and explicitly delegates the poison
    decision to `reassign_derivations` — and it also serves the
    force-drain path. Deletion is licensed only if the Phase-1 collapse
    routes the E8 exit verdict through `decide()` (or adds an equivalent
    threshold check at the E8 charging site) AND the
    lost-`persist_poisoned`-then-failover history class is explicitly
    dispositioned (machine-check it after extending the model so the
    charge mirror and the poison-status mirror can be lost independently
    — a bare re-run with failover/PG faults holds vacuously — or accept
    and record the post-deletion degradation). Settling the C2
    adjudication only determines whether the disconnect arm becomes
    load-bearing again; it does not by itself license deletion.
  - The per-cycle transient-cap poison arm ("max_retries exhausted") is
    **defaults-shadowed but spec-mandated — keep-and-document**, not a
    deletable redundancy: it implements the final clause of
    `sched.retry.transient-budget` ("at or above `max_retries` the
    derivation is poisoned"), is tracey-wired (`r[impl]` at
    `handle_transient_failure` and in `retry_policy.rs`; `r[verify]` on
    the fold's unit tests and on `quint-retry-policy-worker`), and
    `sched.retry.attempts-bounded+2` lists the per-cycle transient count
    among the budgets whose exhaustion must produce a terminal state. Its
    unreachability is a property of production defaults only
    (`require_distinct_workers = true`, threshold 3 vs `max_retries` 2,
    the 2f07ea909 placement exclusion, one-shot fresh executor IDs); the
    arm is live in non-distinct/dev configurations or whenever the poison
    threshold exceeds `max_retries + 1`, so removal is a behavior change
    there. Deleting or changing it requires a rule amendment + tracey
    bump, re-review of the impl/verify sites, a deliberate decision about
    the non-default configurations, and red-first treatment. Recommended
    Phase-1 outcome: keep, and document the shadowing next to the
    placement exclusion it is coupled to (remove either mechanism and the
    other becomes reachable; `decide()` should state one behavior
    deliberately).
- **Invariants added during calibration** (now part of the contract):
  `clearedPoisonClearsDurably`, `clearedPoisonScrubsExclusions`,
  `recoveryPreservesPoisonStatus`; plus two
  calibration-local properties worth carrying into the post-refactor
  model: `pendingReportKeepsItsEntry` (the correlation-state conservation
  e872b2b49 protects — subsumed in Phase 1 by the two-installment exec_id
  mechanism, which should make it true by construction) and the
  E5-threshold-unreachable probe (carried forward as the record of the
  disconnect-arm claim only; superseded once the Phase-1 collapse routes
  the E8 exit verdict through `decide()`).
- **NOT-ENCODED dimensions** (the §3.6-style evidence for what, if
  anything, needs a different verification vehicle than this model):
  - The resource-floor ladder's internals (all of G6, 9 commits): which
    signals promote, hydration/persistence, config plumbing, the at-cap
    baseline. Stays with floor.rs unit tests; if Phase 1 wants it
    machine-checked, it is a small separate model of the ladder, not an
    extension of this one (the design's `classify()` split makes that
    natural).
  - The floor outcome on worker-reported transient (E1) events
    (c13f6a277): the I-213 promotion exemption from `max_retries` is not
    expressible — neither the model's transient arm nor the fold's
    `FTransient` event carries a floor outcome (`promoted` exists only on
    OOM-class events). When E1 collapses into `decide()`, this regression
    class needs either a non-model vehicle (the existing
    `handle_transient_failure` promotion-exempt unit tests,
    `sched.retry.promotion-exempt+3` — the current coverage) or a
    deliberate extension of the floor oracle to transient events; Phase 1
    must choose one explicitly.
  - Build-level bookkeeping (d91df7e9f, e45f2d966, c03d52787, the
    891a6520d build-summary half): summaries, derivation_hashes,
    merge-time transitive seeding, multi-build joins. Model-B territory
    (a build/DAG-level model), not retryPolicy.qnt.
  - The 891a6520d poison-set mechanisms at code resolution: the
    `poisoned_at`-IS-NULL load filter (the crash window of the old
    non-atomic two-call persist, closed by the atomic `persist_poisoned`)
    and the `dag::remove_build` reap guard for recovered-poisoned nodes.
    The G8 override abstracts both as "the poisoned row is not reloaded";
    the model has neither a NULL-timestamp window nor a build-completion
    reap, so if Phase 1 reworks the recovery/poison-load path those two
    mechanisms need their own coverage (unit/VM-level), not a reading of
    this model as covering them.
  - Recovered-node metadata and the persisted-status taxonomy
    (7078da256, 84a692492, ea36f98f2, 01faf80b7, cbda4119a): the ledger
    schema work in Phase 1a is the structural fix; specific regression
    tests stay unit-level.
  - Cross-leader concurrency (c5c5ccd17) and the lease generation floor
    (0fce3e697, 43a7df620, 0745c2ce4): owned by leaderElection.qnt and
    the rio-lease campaign; the retry model composes with them by
    assume–guarantee.
  - Stream epochs / heartbeat binding (db457374f halves): outside the
    model's stated scope. Heterogeneous static eligibility (a62631c90): a
    model-time narrowing of design §3's pre-registered G7 encoding (only
    the draining/registered half of the eligibility predicate is
    implemented; kind/system/features are uniform across slots — see
    `eligibleFleet`). Re-evaluate either only if Phase 1's placement work
    touches them.
- **Substitution-path corrections Phase 1 must respect** (from the
  post-integration re-validation section above): the substitution-failure
  decider stays carved out of the collapse; its inventory rows must be
  re-derived against the integrated tree (`forgiven` seed re-check, the
  8-attempt NotFound ladder, `substitute_tried` no longer charged on
  every failure-arm entry, the topdown-pruned re-spawn exception, the
  stale-Completed re-substitution routing, the demotion-reason taxonomy,
  and the line-number drift).
- **Sequencing note for the D1 fix:** 172776b1b added the off-spec E7
  poison precisely to break a livelock; the Phase-1 Cancel-at-cap
  adjudication must keep the loop broken (the calibration's
  `retryCalibG1DeadlineUncapped` shows what the model says about the
  no-terminal-action world: boundsOK falsifies, i.e. the budget itself is
  breached). The fix is "Cancel at the cap", never "no action at the
  cap".

#### Phase-1b decision surface (T-1b.1, frozen)

The §5a-2 contract as implemented in `rio-scheduler/src/retry_policy.rs`:
`decide(&[AttemptRecord], &Budget, now: AbsTime, legacy_seed:
Option<&PersistedRetryColumns>) -> Decision { verdict, exclusion,
backoff_until, counters }`, a thin wrapper that maps the attempt-ledger
suffix onto the referenceFold's event alphabet and folds it;
`classify(&ObservedFailure, FloorOutcomeView) -> OutcomeClass`, the
total append-time classifier carrying E2's `exempt_from_cap` predicate
(promoted-or-CONCURRENT_PUTPATH) for both reporting channels; and
`placeable(&exclusion, &eligible_fleet) -> Placement
{Placeable | FleetExhausted | NoEligibleWorkers}`, the dispatch-time
placement predicate consuming `Decision::exclusion` (the fleet-exhaust
verdict stays out of the fold). The fourth `legacy_seed` argument is the
transitional mixed-era floor (decision P5 / design amendment A1):
applied only when the mirror columns are non-empty and the suffix has no
reset row, union for `failed_builders`, max for `count` and
`resubmit_cycles`, dropped in Phase 2 with the column drop — at which
point the frozen 3-argument shape is restored. The fleet is not an
argument: `decide()` never sees it, and the in-history fleet-exhaust arm
is evaluated against an empty fleet (never exhausted). The per-cycle
transient-cap arm is kept and documented as defaults-shadowed (P3); the
transient arm carries no promotion exemption and `classify()` never
consults the floor for transients (P4 — coverage stays with the
`sched.retry.promotion-exempt+3` unit tests).

### Stage-C verify-marker status

No new tracey markers: the calibration checks are regression guards for
the model's encoding (same no-marker policy as every other witness
check), and the two new invariants strengthen checks that already carry
the relevant rules' markers (`sched.retry.recovery-projection` on the
failover regime check; the clear-ordering discipline is part of
`sched.admin.clear-poison`'s existing code-level verification).
