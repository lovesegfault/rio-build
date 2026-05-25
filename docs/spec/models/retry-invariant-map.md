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
| `sched.retry.recovery-projection` *(new)* | **COVERS** | Was a GAP. The projection was scattered across four rules' prose — `sched.poison.ttl-persist` (`poisoned_at`), `sched.merge.poisoned-resubmit-bounded+2` (`resubmit_cycles`), `sched.retry.per-executor-budget` ("`failed_builders` persisted to PG; infrastructure retry count is in-memory only"), `sched.timeout.promote-on-exceed+2` ("`timeout_retry_count` is in-memory only, recovery resets to 0, conservative") — and no rule stated the complete 4-recovered / 1-derived / 5-defaulted split or the no-fabrication bound. The new rule states the as-built documented projection; whether the forgiveness should survive the Phase-1 ledger is the separate `sched.retry.failover-budget` spec decision the design's Phase-0 gate requires before Phase 1 starts (not made here). |
| `sched.recovery.poisoned-failed-count` | **COVERS** (the build-summary half) | Recovered poisoned derivations count toward the build's `failed`, never `Succeeded`. |
| `sched.poison.ttl-persist` | **COVERS** (`poisoned_at`) | Including the expired-poison filter at reload. |

### `RecoveryNeverFabricatesFailures`
*No recovered counter exceeds what the durable rows support.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.retry.recovery-projection` *(new)* | **COVERS** | The projection rule's equality form subsumes the inequality form: the recovered counters are exactly the projection of the persisted columns, and the projection reads only persisted evidence (no counter is invented). Note the projection is *not* a refinement of the live state in either direction: `failure_count := failed_builders.len()` both forgets same-worker repeats (under-count) and counts the permanent path's diagnostics-only `failed_builders` insert as a poison-threshold failure that the live `failure_count` never charged (over-count, see the divergence table's A6). Both directions are the documented lossy reconstruction; the invariant bounds the recovered value by the durable evidence, not by the lost live value. |

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
  premises), it intersects the pending `sched.retry.failover-budget`
  decision the design's Phase-0 gate requires, and Phase 1a's
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
  builders. The `noTransientCapPoison` witness is therefore not wired,
  and the arm is a Phase-1 simplification/clarification candidate.
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
