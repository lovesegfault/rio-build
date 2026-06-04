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
of this map is the `referenceFold` — the Phase-0 specification oracle the
model's `CountersRefineHistory` invariant compares the live counters
against. It was written as `rio-scheduler/src/retry_policy.rs` and the
phase records below cite it by that path; since the Phase-2 kernel
extraction the fold and the decision surface live in the dependency-free
`rio-retry-kernel` crate (so the CBMC harnesses' goto model closes over
the kernel alone), with `retry_policy.rs` remaining as the scheduler's
projection shim and the home of the fold's unit battery.

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

**Status (Phase 1c, T-1c.3): landed.** The collapsed verdict sites fold
the durable suffix (T-1b.2…T-1b.11), recovery rebuilds every loaded
node's retry view from the same seeded fold
(`sched.retry.recovery-projection+2`), and the companion
forgiveness-prose amendments landed with that change
(`sched.timeout.promote-on-exceed+3`,
`sched.retry.per-executor-budget+2`). The rule now carries its
implementation marker on the recovery-time ledger fold rebuild
(`DerivationState::rebuild_retry_view_from_ledger`) and its
machine-checked `verify` on the post-collapse model's failover regime
(`failoverPreservesHistory` in `quint-retry-policy-failover`, checked
exhaustively together with the counter/verdict refinement and
durability invariants); the model-side
`sched.retry.recovery-projection+2` verify marker returned to the same
check with the Phase-1c re-wiring, alongside the Rust recovery/failover
tests that verify the seeded-fold contract end to end.

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
  `062_derivation_wanted_outputs` (harden-subst), `065_leader_generation_claims`,
  `066_log_chunks`, `067_drop_drv_logs`. The retry-formal attempt-ledger
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
  (migration 068). The only substitution-adjacent ledger writes remain the
  `cache_hit_clear` reset rows in the cached-hit / re-probe lanes; their
  trigger predicate now evaluates the live effective set, so *when* such a
  reset fires can shift (a terminal build's wide wants no longer pin a
  node), but the row contents, the one-transaction clear-poison shape, and
  the no-charge semantics are unchanged. The new `never_forgive_paths`
  clears that land beside those lanes are DAG bookkeeping only.

### 2026-05-26 re-validation: re-authored forgiveness-scoping pair, gateway lifecycle hardening

The third harden-subst rebase (origin tip `5b0023a6c`) lands a rewritten
branch history: the two forgiveness-scoping commits — `cd4495f40`
(`never_forgive_paths` scoped to the substitution chain that created it)
and `6e9791cdf` (spent forgiveness cleared when a stale-Completed reset
opens a new chain) — arrive re-authored with new hashes and expanded
messages, but their cumulative content is exactly what the 2026-05-25
subsection already analyzed: the previous-base→new-tip delta contains no
file under `rio-scheduler/`, `rio-common/`, `rio-proto/`, or
`rio-migrations/` (no new migrations; the attempt-ledger migration stays
066). The genuinely new work this round — rio-gateway connection/session
lifecycle hardening (force-close, session capacity, write-path
deadlines), KEDA/infra fixes, and a test-only MockScheduler ResolveTenant
latency knob — touches nothing under `rio-scheduler/src/`. Re-checked
against the merged tree:

- **(a) E5 and the fold's charging semantics: unaffected.** The rebase
  introduces zero scheduler-side change: `actor/executor.rs` (E5),
  `actor/dispatch.rs`, `actor/merge.rs`, `actor/completion.rs`, and
  `retry_policy.rs` in the merged tree are byte-identical to pre-rebase
  `formal-sprint`, with the chain-end `never_forgive_paths.clear()` in
  `handle_completion` still sitting ahead of the verdict dispatch and the
  line-count stamping exactly as resolved at the previous rebase. No new
  event reaches E5; the gateway hardening changes *when* SSH
  sessions/connections end (an existing cancellation pathway), not which
  scheduler events exist or what they charge. The substitution-failure
  carve-out holds.
- **(b) Substitution-delta notes: no new deltas.** The 2026-05-25 bullets
  already record the chain-scoped `never_forgive_paths` (cleared at every
  chain ending, re-cleared by the stale-Completed reset opening the next
  chain) and the live effective wanted set; the re-authored pair changes
  no clear site and adds none, so notes (1)–(7) and the 2026-05-25
  amendments stand as written.
- **(c) Attempt ledger: nothing new recorded.** `handle_substitute_complete`
  still calls none of `append_attempt` / `append_attempts_batch` /
  `record_attempt_with_poison` / `append_attempt_standalone`, and
  `substitution` remains absent from the `OutcomeClass` alphabet. The only
  substitution-adjacent rows remain the `cache_hit_clear` resets already
  described above; their contents and no-charge semantics are untouched.

### 2026-05-29 re-validation: main rebase — the closure-evidence/topdown-prune lifecycle and its substitution-walk changes

The rebase onto main (`dfe9a5569`) brings in the 69 genuinely-new commits of
main's scheduler campaign: persisted `topdown_pruned` (migration 063) and
`closure_hole` (064) markers, the closure-evidence classifier
(`DerivationDag::closure_evidence` → Vouched/Pending/Broken with the
`must_substitute` predicate), the dispatch-time and reap-time fail-fast arms
(`fail_fast_topdown_pruned_root`, the leader-gated survivor re-evaluation in
`handle_cleanup_terminal_build`), the recovery/poison-clear closure-hole
stamps, and the explicitly-requested prune retention. The branch's own carry
adds the pull-admission refusal (`admit_pull` answers NotYetReady for
`must_substitute` nodes) and the reap-time skip of Assigned/Running
survivors. Re-checked against the merged tree:

- **(a) E5 and the fold's charging semantics: unaffected.** The 69-commit
  delta touches none of `rio-retry-kernel/`, `retry_policy.rs`,
  `actor/executor.rs` (the report-intake successor paths), or
  `db/attempts.rs` (`git diff 5b0023a6c origin/main` is empty for all
  four), and conflict resolution introduced no new caller of the ledger
  writers: the only `INSERT INTO drv_attempts` sites remain
  `db/attempts.rs` plus tests, and `poison_and_cascade` keeps exactly its
  three pre-rebase callers (pull intake, report intake, recovery) — none of
  the new fail-fast/reap/poison-clear/recovery arms call it. The fail-fast
  teardown path (`fail_fast_topdown_pruned_root` →
  `cancel_build_derivations` → `transition_build_to_failed`) writes
  derivation/build status, stamps `drv_executions` via the terminal log
  epilogue, clears `never_forgive_paths` and the consumed `topdown_pruned`
  mark — and appends **no** `drv_attempts` row, mutates no `RetryState`,
  writes no backoff, mints no exec, so the cascade it triggers is
  charge-free trivially. Open pull attempts on nodes it cancels are settled
  only by the existing controller-synthesized verdict
  (`close_pull_attempt_uncharged`, charge-free) with the establishment
  sweep as the unchanged backstop. The reap-time arm cannot fire on a
  survivor holding an open attempt: the §4-i widening skips
  Assigned/Running survivors (pinned by
  `cleanup_reap_skips_marked_holed_survivor_with_open_attempt`), and the
  pull-admission refusal keeps a `must_substitute` node from acquiring an
  open from-source attempt in the first place
  (`pull_must_substitute_node_refused_and_settled_by_sweep`).
- **(b) Substitution-delta notes: one addition, no contradictions.** The
  in-flight-walk skip (`status != Substituting` in the reap arm) and the
  closure-evidence routing in `handle_substitute_complete` change *when*
  the established fail-fast/requeue arms run, not what they charge; the
  walk's late `SubstituteComplete{ok}` verdict still lands in the
  not-Substituting guard / `admit_pull` status table, so a late verdict
  cannot race a re-opened attempt for the same node into a double charge
  (the node leaves Substituting only through the verdict itself or a
  cancel, and `admit_pull` refuses non-Ready nodes). The chain-scoping
  notes stand: the fail-fast's `never_forgive_paths.clear()` is a chain
  ending exactly like the pre-existing walk-failure endings, and the
  one-shot `substitute_tried` consumption is unchanged. Notes (1)–(7) and
  the 2026-05-25/26 amendments stand as written.
- **(c) Attempt ledger: nothing new recorded.** `handle_substitute_complete`
  and the new arms call none of `append_attempt` / `append_attempts_batch`
  / `record_attempt_with_poison` / `append_attempt_standalone`;
  `substitution` remains absent from the `OutcomeClass` alphabet
  (Transient/Infra/ExemptInfra/Timeout/Permanent/Cascade/Backstop/
  Disconnected). The closure-hole stamps (`set_closure_hole_by_hashes`)
  and mark clears are derivation-row column writes, not ledger events.

The substitution carve-out therefore holds in the merged tree; the
closure-evidence lifecycle itself (mark/hole stamping, clearing, and the
failover round-trip) remains example-tested rather than modeled and stays
on the VERIFY-LATER list as the closure-evidence/topdown-prune lifecycle
candidate.

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

#### Phase-1c model flip (T-1c.1): the post-collapse encoding and the frozen as-built model

The Stage-B as-built encoding is frozen at
`docs/spec/models/retryPolicyAsBuilt.qnt` (module `retryPolicyAsBuilt`,
no semantic edits; imported only by the Stage-C calibration corpus and
retired with it in Phase 2). The new `retryPolicy.qnt` main encodes the
post-collapse code: every accounting action advances the cached view,
the durable ledger fold (`pg.ledger`) and the reference-fold ghost with
the same `specApply` application — the appending transaction — so
`countersRefineHistory` and `durableMirrorsCharges` are
true-by-construction tripwires there; the four mirror-column bits and
the lost-mirror-write fault are replaced by the ledger abstraction plus
a single `attemptTxFails` fault action (charge nothing, re-deliver the
event); the failover arm rebuilds the view from the durable fold (the
selective-forgiveness projection and its `recoveryIsTheDocumentedProjection`
invariant are deleted with it, superseded by `failoverPreservesHistory`
in T-1c.3 plus the kept `recoveryNeverFabricatesFailures` /
`recoveryPreservesPoisonStatus`).

Abstraction choices recorded for the C2/establishment encoding (T-1c.1
step 1):

- **The establishment is clock-decoupled.** `establishUnreportedCrash(w)`
  is enabled as soon as the released attempt's `recently_disconnected`
  entry has no deliverable classifying report (`ctrl == NoCtrl` or the
  pending report belongs to a different death), with no 60 s TTL gate —
  an over-approximation of production timing that is conservative for
  the safety invariants; the classification window the TTL protects is
  preserved by the no-deliverable-report precondition. The entry is
  resolved by a classifying report or by establishment, never silently
  dropped (the as-built tick-sweep drop arm is gone, matching the
  post-collapse sweep whose expiry IS the establishment).
- **The un-established re-dispatch window is bounded by identity
  freshness.** A slot whose previous death is still awaiting
  establishment is not placeable for this derivation (production
  replacement pods are fresh identities; the crashed identity never
  re-registers). A slot whose death has a deliverable controller report
  pending stays placeable, so the late-installment /
  re-dispatch-before-classification interleavings remain reachable.
  This is what makes `attemptsBoundedGlobal` a meaningful HOLD in the
  crash regime rather than a restatement of the dispatch ceiling.
- **Crash-regime instantiation:** two slots (`w1`,`w2`), so the
  establishment→charge→terminal route is the production distinct-worker
  threshold (THRESHOLD = 2) rather than the fleet-exhaust fallback a
  one-slot regime would force; `ATTEMPT_BOUND = 2` (= THRESHOLD; the
  un-established window adds no extra attempt because an
  establishment-pending slot is not re-dispatched) with
  `MAX_ATTEMPTS = 4` strictly above it, so the HOLD is carried by the
  charging machinery, not by `dispatchTo`'s own ceiling.
  `quint-retry-policy-crash-unbounded` (expect-violation) flips to the
  `attemptsBoundedGlobal` HOLD in this regime at T-1c.2.
- **Ghost attribution:** the per-slot `deathCharges` ghost tracks the
  slot's most recent death; an establishment for an entry whose death is
  no longer the most recent one (re-dispatch while the report was
  deliverable, then a newer death) does not increment it — the older
  death's charge is structurally at-most-once (its report was
  overwritten, so establishment is its only possible charge), and the
  newer death's accounting stays exact for `noDoubleCount`.

#### Phase-1c re-run record (T-1c.4): regimes, witnesses, calibration

The full post-collapse check set was re-run exhaustively after the flip
(T-1c.2) and the acceptance invariants (T-1c.3); distinct/generated
state counts per regime are recorded in the T-1c.4 commit message and
each check's CI transcript (the as-built baselines stay in the Stage-B
section above for comparison). Verdicts: every invariant in every
regime's HOLD list holds — including the four that were
falsified-as-pre-registered against the as-built encoding
(`verdictMatchesFold` / D1, `countersRefineHistory` / D2+D3,
`durableMirrorsCharges` / D4, `attemptsBoundedGlobal` / C2) and the new
`failoverPreservesHistory` acceptance invariant — and every wired
`quint-retry-policy-witness-*` reachability witness is still violated,
with the two pre-registered re-points (the controller-cap witness now
probes the cap-Cancelled terminal; the lost-mirror-write witness is
replaced by the appending-transaction-failure witness) plus the two new
establishment probes (crash-charge, crash-terminal). The six wired
`quint-retry-calib-*` checks still falsify exactly as the calibration
table records — they import the frozen `retryPolicyAsBuilt.qnt`, so the
freeze is semantics-preserving and the corpus keeps its evidentiary
value until Phase 2 retires it.

Carried forward for Phase 2's acceptance table ("cannot recur by
construction" rows):

- **`countersRefineHistory` and `durableMirrorsCharges` are identities
  in the post-collapse encoding** — every entry point advances the
  cached view, the durable ledger fold and the reference-fold ghost
  with the same fold application, so the per-site counter-mutation bug
  class the Stage-C corpus replays (G1's wrong-path/wrong-counter
  increments, G2's per-cycle/cross-cycle split, the at-cap
  double-count, the D2/D3 channel asymmetries) cannot recur as a
  divergence between a site and the fold; the invariants are kept as
  cheap tripwires against encoding drift, and the residual risk — a
  change to the fold itself — is owned by the referenceFold/decide()
  unit batteries and the named regression runs, which pin concrete
  histories rather than refinement.
- **`noDoubleCount` stays a live invariant** (not true by
  construction): the observation/dedup layer (`recently_disconnected`,
  `last_completed`, the establishment's no-deliverable-report
  precondition) is still explicit in the model, so a re-introduced
  dedup defect would still falsify it; production additionally holds
  the schema half (the exec_id partial-unique index makes the second
  installment an UPDATE, never a second row).
- The poison/clear lifecycle invariants (`poisonIsTerminalUntilCleared`,
  `clearedPoisonClearsDurably`, `clearedPoisonScrubsExclusions`,
  `recoveryPreservesPoisonStatus`) and the cascade invariant remain
  live in the new encoding, unchanged in form.

### Stage-C verify-marker status

No new tracey markers: the calibration checks are regression guards for
the model's encoding (same no-marker policy as every other witness
check), and the two new invariants strengthen checks that already carry
the relevant rules' markers (`sched.retry.recovery-projection` on the
failover regime check; the clear-ordering discipline is part of
`sched.admin.clear-poison`'s existing code-level verification).

## Phase-1 close-out (T-1c.5)

Phase 1 of the retry-formal campaign is complete: the durable attempt
ledger (1a), the nine-site collapse onto `decide()` / `classify()` /
`placeable()` with the adjudicated behavior changes (1b), and the model
flip to the post-collapse encoding with the pre-registered
falsifications proven as invariants (1c). This section is the campaign
record the design's §5 Phase-1 gates ask for; the per-task evidence
lives in the introducing commits and the CI transcripts.

### Verdict table v2 (post-collapse encoding)

Every cell is an exhaustive TLC verdict of the named check against the
post-collapse `retryPolicy.qnt`; cells marked **flipped** were
falsified-as-pre-registered against the as-built encoding and now HOLD.
State counts per regime are in the T-1c.4 commit message and the check
transcripts.

| Design invariant | Model form | worker | dual | crash | failover |
|---|---|---|---|---|---|
| `AttemptsBounded` (charge discipline, now total over the alphabet) | `attemptsChargedOnce` | HOLDS | HOLDS | HOLDS | HOLDS |
| `AttemptsBounded` (boundedness clause) | `attemptsBoundedGlobal` | not checked (no uncharged end in the alphabet) | not checked (subsumed by the crash regime) | **HOLDS (flipped — C2)** | not checked |
| `PoisonIsTerminalUntilCleared` | `poisonIsTerminalUntilCleared` | HOLDS | HOLDS | HOLDS | HOLDS |
| `CascadeReachesExactlyTheDependents` | `cascadeReachesExactlyTheDependents` | HOLDS | HOLDS | HOLDS | HOLDS |
| `CountersRefineHistory` | `countersRefineHistory` | HOLDS | **HOLDS (flipped — D2/D3)** | HOLDS | HOLDS |
| `VerdictIsChannelInvariant` | `verdictMatchesFold` | HOLDS | **HOLDS (flipped — D1)** | HOLDS | HOLDS |
| `PlacementIsAFunctionOfExclusionAndFleet` | `placementSound` | HOLDS | HOLDS | HOLDS | HOLDS |
| `NoDoubleCount` | `noDoubleCount` | HOLDS (vacuous — no deaths) | HOLDS | HOLDS | HOLDS |
| durable completeness (D4's surface) | `durableMirrorsCharges` | HOLDS | **HOLDS (flipped — D4)** | HOLDS | HOLDS |
| `RecoveryNeverFabricatesFailures` | `recoveryNeverFabricatesFailures` | HOLDS | HOLDS | HOLDS | HOLDS |
| `FailoverPreservesHistory` *(new acceptance property)* | `failoverPreservesHistory` | n/a (no failover) | n/a | n/a | **HOLDS (first checked here)** |
| poison-set survival | `recoveryPreservesPoisonStatus` | n/a | n/a | n/a | HOLDS |
| clear durability / scrub | `clearedPoisonClearsDurably`, `clearedPoisonScrubsExclusions` | HOLDS | HOLDS | n/a (no resets) | HOLDS |
| `RecoveryIsTheDocumentedProjection` | — | retired with the as-built encoding (the selective forgiveness it stated is no longer the contract); superseded by `failoverPreservesHistory` + `recoveryNeverFabricatesFailures` + `recoveryPreservesPoisonStatus` |  |  |  |

The four retired expect-violation checks
(`quint-retry-policy-divergence-{verdict,counters,durable}`,
`quint-retry-policy-crash-unbounded`) are replaced by the HOLD cells
above plus the establishment/crash-terminal/tx-failure witnesses; the
deterministic reproducer runs survive in the named-run checks with the
adjudicated outcomes (`d1ControllerTimeoutCapCancelsRun`,
`d2AtCapAnchorRun`, `d3PromotedChargesExemptBudgetRun`,
`d4BackstopDurableRun`, `c2CrashLoopBoundedRun`,
`c2EstablishmentChargesRun`, `lateInstallmentAfterRedispatchRun`).

### Spec rules amended or added in Phase 1

| Rule | Change | Owning task |
|---|---|---|
| `sched.retry.failover-budget` | new at the Phase-0 exit (budgets survive failover); `r[impl]` on the recovery-time ledger fold rebuild and `r[verify]` on the failover regime check landed in 1c | T-1c.3 |
| `sched.termination.deadline-exceeded+2 → +3` | the controller path takes terminal `Cancelled` at `max_timeout_retries` (D1; dissolves contradiction C4, resolves C1) | T-1b.10 |
| `sched.timeout.promote-on-exceed+2 → +3` | the in-memory-only / recovery-resets-to-0 forgiveness prose dropped; the timeout budget survives failover | T-1b.12a |
| `sched.retry.per-executor-budget → +2` | established-crash membership clause (C2) + failover-survival prose replacing the forgiveness prose (the single bump P1 commits to) | T-1b.12a |
| `sched.retry.recovery-projection → +2` | the recovery contract is the ledger fold seeded by the legacy columns (reset-row suffixes ignore the seed; empty suffixes degenerate to the legacy projection); fixes the Stage-B-found poisoned-row `count` over-claim | T-1b.12a |
| `sched.db.clear-poison-batch → +2`, `sched.merge.poisoned-resubmit-bounded → +3` | frozen mirror column / ledger-carried resubmit cycle | T-1b.13 |
| `sched.admin.clear-poison` | prose-only correction of the stale pre-b874e5120 ordering description (no bump) | T-1b.12a |

### C2: the charge, the budget, and the establishment vehicles (P1)

An attempt whose classifying report never arrives is charged once its
failure is **established**: the charge is the threshold/exclusion
budget (`failed_builders[executor]` + `failure_count`, nothing else),
applied by the fold's `EstablishedCrash` arm. The establishment
vehicles are the correlation-TTL sweep (installment + in-transaction
`decide()` + status persist, the same spine as every collapsed site)
and the E8 backstop (which charges and decides at its own site); the
controller's non-promoting report deliberately does **not** establish,
preserving the classification window for a promoting or
DeadlineExceeded report for the same death. Unestablished
`disconnected` rows stay uncharged. The A5 membership question for this
class is folded into `sched.retry.per-executor-budget+2`'s
established-crash clause.

### Deletions taken and not taken

Taken (T-1b.13): the 17 in-place `RetryState` budget-counter mutations
(every E1–E8 arm, the cache-hit clears, the
`record_failure_and_check_poison` helper and its
`FailureOutcome::reached_poison` carrier); the per-counter mirror
writers `increment_retry_count` and `append_failed_worker`;
`clear_poison_batch`'s `resubmit_cycles` increment; the 1a-era
`requeue_after_retry` / `record_attempt_with_status` /
`fill_attempt_termination` shims as their callers collapsed.
`persist_poisoned`'s pool-owning form survives only for the row-less
degraded paths (E5 re-check, recovery enforcement, no-db-id fallbacks);
every row-bearing site persists inside its appending transaction.

Not taken, with rationale:

- **E5's poison-threshold re-check in `reassign_derivations`** (P2,
  the narrowed b09c5b312-X6 disposition): kept, converted into a
  `decide()` caller over the durable suffix + seed. The backstop no
  longer depends on it (E8 decides at its own site since T-1b.6), but
  it remains the disconnect-time and force-drain-time re-poison path
  and the post-failover backstop for a lost `persist_poisoned` write,
  and within a single tenure it is structurally unreachable — exactly
  the probe's claim.
- **The per-cycle transient-cap poison arm** (P3): keep-and-document.
  Defaults-shadowed (the distinct-worker threshold fires first under
  production defaults) but spec-mandated
  (`sched.retry.transient-budget`'s final clause) and live in
  non-distinct/dev configurations; the shadowing is documented at the
  cap check in `retry_policy.rs`.
- **The mirror columns** (`derivations.{retry_count, failed_builders,
  resubmit_cycles}`) (P5): writers retired, columns kept as the frozen
  transitional legacy seed. The DROP is Phase 2, gated on the drain
  condition recorded at `load_retry_seed_in_tx`: no non-terminal or
  poisoned derivation with non-empty mirror columns and a reset-free
  suffix.

### P4: the floor on transient attempts stays out of the model

The I-213 promotion-exemption regression class
(`c13f6a277`) is carried by `classify()` keeping the
promoted/CONCURRENT_PUTPATH event in the exempt infra class on both
reporting channels and by the existing
`sched.retry.promotion-exempt+3` unit tests
(`test_transient_failure_promotion_exempt_from_max_retries`); the
transient arm gains no exemption, the floor oracle is not extended to
transient events, and the model deliberately stays NOT-ENC there — the
fold and the model would have nothing to encode that the as-built code
does not already refuse to do.

### The transitional legacy floor (P5) and its residual

Wherever the fold runs, a derivation whose mirror columns are non-empty
and whose suffix has no reset row is seeded: union for
`failed_builders`, max for `count` and `resubmit_cycles`,
`failure_count` floored at the merged set size; a reset-row suffix
ignores the seed; an empty suffix degenerates to the pure legacy
projection. Residual, accepted: within a boundary-spanning cycle the
per-cycle `count` is floored at max(column, fold) rather than their sum
— never below what either era supports, never above the true history;
exact summation would require the ledger backfill P5 declines. The
floor and the columns are dropped together in Phase 2 behind the drain
condition above.

### A5 / A7 / A10: carried as-built asymmetries (disposition (b))

Which failure classes join the placement-exclusion set (A5, beyond the
C2 addition), the per-class backoff asymmetry (A7), and the per-counter
fencepost conventions (A10) are all carried exactly as built — the
`Attempt` record's `outcome_class`, flags and timestamps are the
discriminating inputs, `decide()` reproduces each counter's own
convention, and no policy change was taken. This closes the divergence
catalog's open (b)/(c) rows without new spec rules.

### Model-side close-out

- The as-built Stage-B encoding is frozen at `retryPolicyAsBuilt.qnt`,
  imported only by the Stage-C calibration corpus; the six wired
  `quint-retry-calib-*` checks and the fourteen evidence modules are
  unchanged, still falsify/typecheck (re-confirmed at T-1c.1 and
  T-1c.4), and remain re-runnable per `calibration/README.md`.
- The post-collapse `retryPolicy.qnt` main encodes the appending
  transaction as the single advance point for the cached view, the
  durable ledger fold and the reference-fold ghost; the establishment
  (`establishUnreportedCrash`) and the appending-transaction fault
  (`attemptTxFails`) join the alphabet; the crash regime runs two slots
  with `ATTEMPT_BOUND = THRESHOLD = 2 < MAX_ATTEMPTS = 4` so the C2
  boundedness HOLD is carried by the charging machinery, not the
  dispatch ceiling (the establishment abstraction and the
  identity-freshness placement bound are recorded in the T-1c.1
  subsection above).
- Pre-registered witness re-points, executed: `noControllerCapPoison` →
  `noControllerCapCancelled` (the cap-Cancelled terminal on the
  controller channel); `noPgWriteLost` → `noAttemptTxFailure`
  (`quint-retry-policy-witness-tx-failure` replaces
  `-witness-pg-write-lost`). New witnesses: `noEstablishedCrashCharge`
  (crash-charge), `noCrashLoopTerminal` (crash-terminal).
- Retired as-built-only artifacts: `recoveryIsTheDocumentedProjection`,
  `recoveryProjectionNonTerminal` / `recoveryProjectionPoisoned`,
  `failoverForgivenessRun` (replaced by `failoverHistorySurvivesRun`),
  `lostPoisonMirrorFailoverRun` (replaced by
  `txFailureNothingChargedRun` — the lost-`persist_poisoned`
  -as-independent-mirror-write class is structurally impossible for
  post-066 attempts: the charge, the verdict and the status persist
  commit or fail as one transaction), and the live-only
  `RTimeoutBudget` poison reason (no producer once E7-at-cap is
  Cancel).
- The deferred verify markers landed: `sched.retry.failover-budget`
  (impl on `rebuild_retry_view_from_ledger`, verify on the failover
  regime check) and the model-side
  `sched.retry.recovery-projection+2` verify (back on the failover
  regime check). `tracey query untested` shows zero untested rules in
  the `sched.retry.*`, `sched.timeout.*`, `sched.poison.*`,
  `sched.backstop.*` and `sched.termination.*` domains — no retry-rule
  verification is deferred to Phase 2.

### Phase-2 hand-off

Kani on `decide()`/`placeable()` (all histories up to the budget bound);
MBT replaying model traces against the fold and the in-memory list; the
acceptance table built from the calibration corpus, after which
`retryPolicyAsBuilt.qnt`, the calibration corpus and the six
`quint-retry-calib-*` checks are retired (design §5, Phase-2 row); the
mirror-column DROP plus the legacy-floor removal behind the drain
condition (restoring `decide()`'s frozen 3-argument shape); a real
ledger GC policy to replace the P8 retention assertion; and the
deferred policy questions left open on purpose (A7 uniform backoff, A10
fencepost unification, A8 poison-reason strings).

## Phase-2 assurance layer

The Phase-1 close-out above is the campaign record at the moment the
nine-site collapse landed. This section records the Phase-2
deliverables (design §5, Phase-2 row, plus amendment A1's removal
clause): the Kani contracts on the decision kernels, the
model-based-testing decision, the acceptance table over the historical
fix corpus, the mirror-column retirement decision, and the retirement
of the frozen as-built model. The campaign close-out is the final
section of this document.

### Kani contracts on the decision kernels

The decision kernels — the reference fold, `decide()`, `classify()` and
`placeable()` — live in the dependency-free `rio-retry-kernel` crate
(extracted from `rio-scheduler/src/retry_policy.rs`, which is now the
projection shim over it). The kernel carries the function contracts
(`#[kani::ensures]`, instrumented under `cfg(kani)` only) on `decide()`,
`classify()` and `placeable()`, plus seven proof harnesses in its
`#[cfg(kani)] mod proofs`, wired as the `kani-rio-retry-kernel` check
in nix/kani.nix and gated in `checks.*` (run on its own with
`nix build .#checks.x86_64-linux.kani-rio-retry-kernel`, or via the
`.#kani-toolchain.kani-checks.kani-rio-retry-kernel` manual alias;
exact harness-count tripwire pinned at 7).

**Verification status — merge-gated.** CBMC on these harnesses did not
converge within a merge-gate-compatible budget when the contracts were
introduced inside rio-scheduler, and the extraction into the
dependency-free kernel crate — the remediation the earlier deferral
recorded — turned out to be necessary but not sufficient: the dominant
cost was the symbolic execution of the std `BTreeSet`/`Vec`/`str`
machinery inside the fold, the harnesses, and the contract
instrumentation, which travels with the code into any crate (the
contracts-introducing and extraction-follow-up commit messages record
those measurements, including the classification harness's missing
unwind bound). What closed the gap is the proof-time representation
change: under `cfg(kani)` every executor-id set in the kernel is
`BoundedIdSet` (via the `IdSet` alias) — a fixed-capacity array set
whose operations are plain, concretely bounded index loops — the
ledger fold runs without an intermediate event buffer, and the
exemption predicate's substring search is a windowed byte comparison
shared by the implementation and the contract. Production keeps
`BTreeSet` (the alias resolves to it under every non-kani cfg), and
the two representations are pinned to each other by the kernel's
differential unit tests and the `check_bounded_set_models_set_semantics`
harness. With that in place the harnesses converge inside the
merge-gate budget (per-harness wall-clocks are recorded in the
representation change's commit messages), `kani-rio-retry-kernel` is
inherited into `checks.*` alongside `kani-rio-lease` /
`kani-rio-log-kernel` (the former `kani-rio-store`), and the corresponding `r[verify]` markers live at
the wiring point in nix/kani.nix. One mechanism note: `classify()` and
`placeable()` are verified as `proof_for_contract` harnesses, while
`decide()`'s four clauses are asserted by its harness through shared
predicate bodies (the same text the `#[kani::ensures]` attributes
wrap) — kani's contract-instrumented wrapper around the whole fold is
the one shape that still exceeds the gate budget, and the assert form
proves the identical clauses over the identical domain. The properties below are
machine-checked over the stated bounded domains on every merge; the
fold unit battery (which exercises the kernel through the scheduler
shim), the per-site tests, and the `quint-retry-policy-*` regimes
remain the load-bearing coverage for everything outside those domains.

What the contracts state and the harnesses prove, over
every attempt suffix of up to 4 arbitrary ledger rows (arbitrary class /
kind / flag / party / executor / timestamp combinations — a strict
superset of what the appending sites can write), every budget with caps
scaled to 0..=2 (both threshold modes), every clock value in bound, and
every (or no) frozen-mirror-column seed:

- **The verdict partition is total, deterministic, and consistent with
  the counters it is computed from** (`check_decide_contract`,
  `check_decide_deterministic`): no input in the domain panics; two
  calls on the same inputs return the same `Decision`; each terminal
  verdict names a budget that really is at its bound in the final
  counter view (threshold reached for `Poison(Threshold)`, the named
  cap reached for the budget poisons, the timeout cap reached for
  `Cancel`, a stamped expired poison for `TtlExpire`); and
  `Poison(FleetExhausted)` is unreachable from `decide()` — placement
  is `placeable()`'s job, fed by the exclusion set.
- **No counter arithmetic overflows** (same harnesses, CBMC overflow
  checks on, the ensures closures recompute the comparisons): every
  charge is `+1` onto a `u32` and the clock arithmetic is saturating,
  so the harness length bound is a solver budget, not a hidden
  precondition; exceeding `u32` in production would need ~4 × 10⁹ rows
  in one suffix, which the per-cycle suffix bound (≤ ~70 rows) excludes
  structurally.
- **The budget caps are never exceeded by a Requeue verdict**
  (`check_decide_contract`): the per-cycle transient, non-exempt infra
  and timeout counters are at or below their caps after *every*
  history (the seed can lift `count` above `max_retries` only with
  evidence the frozen legacy column already holds), and an
  exempt-infra attempt that reaches the exemption's own cap never
  produces Requeue. The global exempt-cap form additionally relies on
  the writer discipline that poisoned nodes get no further attempt
  rows, which is upstream of the fold (the sites' status guards).
- **The exclusion set contains the executor of every charged threshold
  attempt** (`check_decide_contract`): every post-reset attempt row
  whose class charges `failed_builders` (transient, permanent,
  backstop, executor-crash) has its executor in `Decision::exclusion`,
  plus every member of the legacy seed when the seed applies.
- **The legacy-seed merge never lowers a counter below what the frozen
  mirror columns support** (`check_decide_contract`,
  `check_legacy_seed_merge_monotone`): with a reset-free suffix the
  merge is floored at the legacy projection
  (`Counters::recovery_projection`, cross-checked in the harness so
  the floor and the projection cannot drift apart), preserves the
  unseeded fold's exclusion set / failure count / resubmit cycles, and
  leaves the channel budgets (infra / timeout / exempt) exactly the
  unseeded fold's; a reset-bearing suffix or an empty legacy row
  ignores the seed entirely. The per-cycle `count` is deliberately NOT
  claimed monotone against the *unseeded* fold: a merged exclusion set
  can reach the poison threshold earlier, and the threshold arm
  poisons before the per-cycle charge — the evidence lands in
  `failed_builders` instead. This is the P5 floor semantics as
  specified, not a weakening.
- **The classification partition** (`check_classify_contract`): each
  observed failure maps to exactly the ledger class its entry point
  appends; the exemption predicate is precisely
  promoted-or-CONCURRENT_PUTPATH on the worker channel and promoted on
  the controller channel (the `sched.retry.exempt-infra-cap`
  definition on both channels — D3's adjudicated side); a transient
  failure never classifies as exempt regardless of the floor outcome
  (P4); no reset / cascade / fleet class is ever produced for an
  observed failure.
- **The placement partition** (`check_placeable_contract`,
  `check_fold_fleet_exhaust_arm`): an empty eligible fleet always
  defers and never poisons (the empty-fleet clause of
  `sched.dispatch.fleet-exhaust+3`), exhaustion requires a non-empty
  fleet every member of which has already failed the derivation, and
  the fold-side fleet arm (E1) obeys the same predicate.
- **The proof-time set representation models set semantics**
  (`check_bounded_set_models_set_semantics`): the bounded array set the
  kani cfg substitutes for `BTreeSet` (the `IdSet` alias) reports
  insert newness, membership and distinct-count length exactly as a
  set, is insertion-order-insensitive, and iterates exactly its
  members — the harness half of the representation-equivalence pin
  (the differential unit tests against `BTreeSet` are the production
  half).

Domain honesty: the contracts are stated over bounded suffixes and
scaled budgets (the same scaling the quint regimes use), not over the
production budget values; the no-overflow argument for production
values is the structural one above. The harness domain is a superset
of the writer-reachable row shapes, so malformed rows (which the fold
treats as no-ops) are inside the domain. The classification harness
quantifies the error message over representative shapes (empty,
unrelated, the CONCURRENT_PUTPATH marker verbatim and embedded), with
the kernel's copy of the marker pinned to
`rio_proto::CONCURRENT_PUTPATH_MSG` by a lockstep unit test in the
scheduler shim. The CBMC cost evidence lives in the
contracts-introducing, extraction-follow-up, and bounded-representation
commit messages; per-harness verdicts and check counts live in the
gated check's transcript (`nix log` of the derivation, or the check's
output file).

Layer separation: the model (`quint-retry-policy-*`) checks the
protocol — which observations arrive, what the appending transaction
does with them, failover, establishment, dedup; the Kani contracts
check the decision arithmetic the collapsed sites call, over all
bounded inputs rather than the model's enumerated alphabet; the fold
unit battery pins concrete hand-computed histories (including every
divergence-history reproducer); none of the three substitutes for the
others.

### Model-based testing: reasoned omission

The Phase-2 hand-off lists "MBT replaying model traces against the real
fold and the in-memory list" (design §4's assurance bullet, §5's
Phase-2 row). The decision, made after the Kani contract layer landed: **not
built.** Recorded here with the reasoning rather than implemented,
because a conformance harness that re-derives what stronger layers
already prove would be exactly the "harness that proves nothing new"
the campaign discipline rejects.

What the lease and log MBTs buy is conformance between a model and an
implementation that have substantial independent machinery: the lease
MBT drives the real Lease-object CAS/election code, the log MBT drives
the real PG-backed open-gate / ingest-session / manifest / read-path
code. In both, the implementation can drift from the model in ways
neither the model checker (which only sees the model) nor the unit
tests (which were not derived from the model) would notice.

The retry MBT's subject, as the design itself scopes it, is narrower:
the pure fold plus the in-memory append-only list — explicitly *not*
PG, not the appending transactions, not the actor. That subject is
already covered three ways:

- The load-bearing properties of `decide()` / `classify()` /
  `placeable()` (verdict partition, cap bounds, exclusion superset,
  legacy-seed floor, exemption predicate, placement partition,
  no-overflow) are now proven by the Kani contracts over **all**
  histories up to the harness bound — strictly more histories than a
  trace replay samples for those properties.
- The concrete histories that matter are pinned twice: the 31-test
  fold unit battery covers every counter, fencepost, the window reset
  and its exempt fall-through, the resets, the seed, and every
  documented divergence reproducer; the model's named runs replay the
  same histories and end in the same adjudicated outcomes.
- The protocol-level content of the model — observation fan-out and
  dedup, the two-installment correlation, establishment, the appending
  transaction's atomicity, failover — is exactly the part a fold-only
  MBT cannot exercise, by the design's own scoping.

The residual gap a retry MBT would close is transcription drift between
`retryPolicy.qnt`'s `specApply` and `retry_policy.rs`'s `apply()`: the
model's invariant verdicts silently ceasing to describe the code. That
risk is real but second-order here, and it is bounded by the named-run
reproducers being pinned on both sides, by the unit battery pinning the
fold's arithmetic on the same histories the model's invariants quantify
over, and by review of paired model/code changes (the same discipline
every other model in this repository relies on). Two practical considerations tip the
balance against building the harness anyway: the post-collapse model
does not carry an event-level ledger (it carries the folded counters,
`PgRow.ledger`), so a replay harness would have to reconstruct fold
events from action labels — more projection code than checked property
— and the projection itself would become a third transcription of the
same fold, with its own drift risk.

Reconsideration triggers, recorded so this stays a decision rather than
a default: build the harness if the fold's event alphabet or input
surface grows beyond what the unit battery and contracts pin (e.g. the
executor-lifecycle campaign extends the establishment/correlation
semantics), if the in-memory list maintenance is ever decoupled from
the appending transaction, or if a model re-encode introduces an
event-level ledger that makes the trace projection trivial. The
quint-connect machinery (`#[quint_run]`, the ITF replay pattern in
`rio-store/src/logs/mbt_tests.rs` / `rio-lease/src/mbt_tests.rs`)
transfers directly if so.

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
| `8a016a393` (at-cap OOM double-counted by floor bump + handler) | CONSTRUCTION | `bump_resource_floor` still mutates no counter; the handler's charge is one fold arm over one ledger row, and one execution can have at most one attempt row (the 066 `exec_id` partial unique index — the second installment is an UPDATE). `noDoubleCount` stays a live invariant (dual regime). |
| `c13f6a277` (I-213: floor-promoted transients consumed `max_retries`) | OUTSIDE (unchanged vehicle, P4) | The exemption stays infra-class only; `classify()` never marks a transient exempt — now also a Kani-proven clause of `check_classify_contract` — and behavioral coverage stays with `test_transient_failure_promotion_exempt_from_max_retries` (`sched.retry.promotion-exempt+3`). |
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

### Mirror-column retirement: deferred behind the drain condition

The Phase-2 hand-off names the DROP of the three frozen mirror columns
(`derivations.{retry_count, failed_builders, resubmit_cycles}`) plus
the removal of the legacy floor (`decide()`'s `legacy_seed` argument,
`load_retry_seed_in_tx`, `DerivationState::legacy_retry_floor`, the
`load_poisoned_display` legacy union), restoring the frozen §5a-2
three-argument `decide()`. `poisoned_at` is not part of the drop — it
is the poison-lifecycle carve-out (`sched.poison.ttl-persist`), not a
counter mirror. Decision: **deferred — no migration 069 ships in this
phase, and the legacy-seed code path stays.**

Why: the drop is gated (T-1b.13, decision P5) on the drain condition —
no non-terminal or poisoned derivation has non-empty mirror columns
together with a reset-free attempt suffix. At the release boundary that
ships Phases 1+2 this condition is unmet *by definition*: every
deployment that upgrades through migration 068 still carries pre-068
failure histories whose only record is the mirror columns, and those
histories stay live until each such derivation completes, passes
through a durable reset (resubmit, cache-hit clear, poison clear), or
is removed. Dropping the columns in the same release train would forget
exactly the state the design's §5 1b gate ("no counter that survives
failover today stops surviving") and `sched.retry.failover-budget`
exist to preserve — the upgrade itself would act as the budget refresh
the rule forbids. A guard inside the migration (fail the deploy if
undrained rows exist) was considered and rejected: migrations are
frozen and run unconditionally at startup, so a data-dependent failure
mode would turn routine deploys into outages on exactly the
deployments that most need the seed.

What this phase records instead:

- The columns stay frozen: no writer has touched them since the
  T-1b.13 cutover except the reset paths zeroing them, which is the
  durable counterpart of "a reset-row suffix ignores the seed" and
  must be retired together with the columns, not before.
- The seed semantics are pinned by the fold unit battery and stated
  as Kani contracts (`check_decide_contract`'s seed-floor clause,
  `check_legacy_seed_merge_monotone`), so the transitional argument
  keeps a precisely specified surface while it lives.
- The operational drain probe, so the eventual drop is a measurement
  rather than a guess — run against a deployment considering the drop
  (statuses listed are the non-terminal set plus `poisoned`):

  ```sql
  SELECT count(*)
  FROM derivations d
  WHERE d.status IN ('created','queued','ready','assigned',
                     'running','substituting','poisoned')
    AND (COALESCE(d.retry_count, 0) > 0
         OR COALESCE(array_length(d.failed_builders, 1), 0) > 0
         OR COALESCE(d.resubmit_cycles, 0) > 0)
    AND NOT EXISTS (
          SELECT 1 FROM drv_attempts a
          WHERE a.derivation_id = d.derivation_id
            AND a.event_kind = 'reset');
  ```

  When this returns 0 (or the operator explicitly accepts forgetting
  the residual rows it returns), a later release ships migration 069:
  DROP the three columns; delete `load_retry_seed_in_tx`, the
  `legacy_seed` argument and the P5 floor block in `decide()` (the
  contracts' seed clauses go with them), `legacy_retry_floor` and its
  construction sites, and the `load_poisoned_display` legacy union;
  frozen-migration rules apply (M_069 commentary in
  `rio-migrations/src/migrations.rs`, new PINNED checksum, the regen
  umbrella).

The `// TODO:` at `load_retry_seed_in_tx` (db/derivations.rs) is the
code-side anchor for this decision and now points at this subsection.
*[2026-06-02, follow-up cleanup rider: that anchor no longer exists —
the seed loader and its TODO were retired with the mirror-column drop
(migration 075; the legacy seed itself removed in `186a253c8`), and
the ledger fold is now the only seed path. The coda below records the
full retirement; this sentence is kept as the decision-time record.]*

**Status (retry-campaign coda): landed.** The drop shipped — as
migration 075, not the reserved 069: 070–074 landed while the drop was
deferred, and taking a lower number than already-shipped migrations
would have re-ordered the applied chain relative to the authored one
for no benefit (M_075/M_070 in `rio-migrations/src/migrations.rs`
record the numbering; the 069 gap is permanent). The trigger is the
deployment-model clarification of 2026-05-27 (no staged rollouts, no
existing cluster or live database — every eventual deployment is
fresh), exactly the trigger migration 072's record cites: a fresh
database never carries a pre-068 failure history, so the drain
condition this subsection gated on is satisfied vacuously and the
operational drain probe above is retained as history only. The sweep
took the retirement checklist in full: `load_retry_seed_in_tx` (and
its TODO), the reset paths' column zeroing, the recovery loaders'
column reads, `DerivationState::legacy_retry_floor` and its
construction sites, and the `load_poisoned_display` legacy union are
gone; the seed machinery itself followed in the next commit —
`decide()` is back to the frozen §5a-2 three-argument shape,
`PersistedRetryColumns` / `Counters::recovery_projection` and the
contracts' seed clauses are deleted, the `check_legacy_seed_merge_monotone`
harness is retired (the `kani-rio-retry-kernel` harness-count tripwire
is now 6; all six harnesses VERIFY), and
`sched.retry.recovery-projection+3`,
`sched.merge.poisoned-resubmit-bounded+4` and
`sched.db.clear-poison-batch+3` re-state the rules whose bodies named
the columns. The legacy-seed scope-boundary entry in `retryPolicy.qnt`
is re-worded as a retirement record (the seed was never modeled; the
wired pull-regime check and witnesses re-verified green, state counts
unchanged — figures in the introducing commit messages). The deleted
seed tests' coverage disposition is recorded in the introducing
commits (no silent weakening: the behavior they pinned is the behavior
removed).

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

## Campaign close-out — retry/poison/cascade (campaign #4)

The retry-formal campaign is complete: Phase 0 (the spec audit, the
reference fold, the as-built model, the calibration), Phase 1 (the
durable attempt ledger, the nine-site collapse onto
`decide()`/`classify()`/`placeable()`, the post-collapse model flip),
and Phase 2 (the Kani contracts, the acceptance table, the retirement
of the as-built corpus, and the decisions recorded above). This section
is the campaign-level record, in the same shape as the log campaign's
close-out; the per-phase evidence lives in the sections above, the
introducing commits, and the CI transcripts.

**Outcome against the design's two committed goals (§1).** Both hold.
The failure policy is an exhaustively checked invariant set: every
design-§3 invariant HOLDs over the post-collapse model's four regimes
(verdict table v2), the four pre-registered as-built falsifications
(D1, D2/D3, D4, C2) flipped to HOLDs with the Phase-1b fixes,
`FailoverPreservesHistory` is checked for the first time and HOLDs, and
the decision kernels carry machine-checked Kani contracts (the
merge-gated `kani-rio-retry-kernel` check: seven harnesses over the
verdict partition, the cap bounds, the exclusion superset, the
legacy-seed floor, classification, placement, and the proof-time set
representation; gated once the bounded exclusion-set representation
brought them inside the gate budget — see the assurance-layer
verification status). The machinery
that enforces the policy is one fold over
durable rows: the seventeen in-place counter mutations, the per-counter
mirror writers, and the nine divergent cap-check
implementations are gone; the nine entry points append a classified row
and call the same three pure functions; the counters are a derived
view; recovery is the same fold over the same rows. (The poison-status
persist survives outside the appending transaction only for the
row-less degraded paths — the E5 re-check, recovery enforcement, and
the no-db-id fallbacks — exactly as the Phase-1 deletions-not-taken
record states.)

**What the campaign fixed in production behavior** (all adjudicated,
red-first, spec-amended): failed builds' logs no longer read
incomplete (the report's `final_line_count` is stamped on the failure
path); a wedged-worker derivation that exhausts its timeout budget via
the controller backstop is `Cancelled` (immediately resubmit-retriable)
instead of `Poisoned` for 24 h, independent of which observer reports
first (D1/C1/C4); a controller-counted at-cap OOM run participates in
the 300 s window like any other counted infra failure (D2); a
floor-promoted controller-reported OOM charges the exemption budget
(D3/C3); the backstop's poison-threshold progress survives failover
(D4); a derivation that deterministically kills its worker with no
report is bounded by the per-executor threshold via establishment
(C2/P1); and budgets survive leader failover
(`sched.retry.failover-budget`) instead of refreshing on every flap.

**The design-§5 Phase-2 gate, assessed honestly.** The gate has two
clauses. "The full stack in the gate": substantially met — the
per-regime model checks, the reachability witnesses and the named-run
replays are `checks.*` derivations on the merge gate, and the Kani
contracts now run in the gate too (`kani-rio-retry-kernel` joined the
checks.* kani set once the cfg(kani) bounded exclusion-set
representation brought the harnesses inside the gate budget — the
assurance-layer verification status records the history); the MBT slot
is the documented omission above.
"Net-negative diff for the four core
files": NOT met, and recorded as such rather than reinterpreted. Against the pre-campaign baseline (the
parent of the first Phase-1a commit), measured on the integrated branch
at the close of Phase 2:

| File | Baseline lines | Now | Δ raw | Δ excluding comments/blanks |
|---|---|---|---|---|
| `actor/completion.rs` | 2,709 | 3,615 | +906 | +707 |
| `actor/executor.rs` | 1,413 | 1,930 | +517 | +360 |
| `actor/housekeeping.rs` | 772 | 855 | +83 | +56 |
| `actor/floor.rs` | 269 | 269 | 0 | 0 |

The growth is not decision logic — that contracted into
`retry_policy.rs` (~+1,500 lines vs the same baseline, including its
unit battery, the Kani contracts and the proof harnesses, all pure and
leaf) and `db/attempts.rs` (+513 lines, the ledger access layer) — it
is the durable write discipline the
design chose deliberately: every exit path now opens an appending
transaction, threads the report into it, stamps the execution row,
handles the two-installment correlation and establishment, and persists
the verdict it computed, where it previously mutated RAM counters and
issued zero-to-four best-effort UPDATEs. The deletions the design
predicted did land (the 17 mutation sites, the mirror writers, the
divergent checks, ~471 raw lines removed from the four files), but they
are outweighed by the transaction plumbing added at each of the nine
sites plus the new tests' fixtures. The accurate summary of the §1 goal
is therefore: the *decision surface* shrank from nine divergent
implementations to one checked fold, and the *counter state* shrank
from ten independently-mutated fields to a derived view — but the
*failure-handling code* in the four core files grew, because durability
was added where there was none. This deviation is recorded rather than
explained away; the executor-lifecycle campaign should budget for the
same shape (collapsing decisions does not shrink handler files when
each handler also gains a durable write).

**What transfers to the executor-lifecycle campaign (#1):**

- The stream-epoch / heartbeat-binding halves of `db457374f` and the
  late-disconnect-vs-reconnect race (G5's stream-epoch half) — the
  executor-map lifecycle the retry model treats as environment.
- Heterogeneous static eligibility (`a62631c90`): the exhaust
  *predicate* is now contracted (`placeable()`), but the eligibility
  computation that feeds it (kind/system/features × draining ×
  registration) is executor-lifecycle territory, still NOT-ENC.
- The correlation/dedup state (`recently_disconnected`,
  `last_completed`, the establishment TTL sweep): this campaign
  extended it to carry the released `exec_id` and to establish
  unreported crashes; its lifecycle (when entries are created, expire,
  and are swept) belongs to the executor-lifecycle model.
- The `ExecRec`/slot identity-freshness abstractions in
  `retryPolicy.qnt` (pod-identity scoping, `ctrlStale`, establishment
  preconditions) — reusable as the starting encoding for the executor
  model's slot state.
- The lesson above about file-size expectations under a
  durability-adding collapse.

**What Phase 2 deferred, explicitly:**

- Gate-wiring (and therefore machine-proving) of the Kani contracts:
  the harnesses verify nothing until they run, and they do not run in
  the gate until the counter-arithmetic kernels are extracted into a
  dependency-light context (the log campaign's `kernel.rs` pattern);
  the contracts, the harness suite, the manual target and the
  harness-count tripwire are in place for that follow-up. **Status
  after the extraction follow-up:** the kernels (the reference fold,
  `decide()`/`classify()`/`placeable()`, their contracts and the six
  harnesses) now live in the dependency-free `rio-retry-kernel` crate
  with `rio_scheduler::retry_policy` as the projection shim, and the
  manual target is `kani-rio-retry-kernel`; the extraction sharpened
  the diagnosis (the CBMC cost is dominated by the std set/vector
  machinery in the fold and harnesses, not by the host crate's
  reachable code) but did not bring the set-folding harnesses inside
  the gate budget, so the gate-wiring itself remained deferred at that
  point. **Status after the bounded-representation follow-up: gated.**
  Under `cfg(kani)` the kernel's executor-id sets are the
  fixed-capacity `BoundedIdSet` (the `IdSet` alias swap; equivalence
  pinned by differential unit tests and a set-semantics harness), the
  ledger fold runs without an intermediate event buffer, and the
  exemption predicate's substring search is a windowed byte
  comparison; with that representation the harnesses converge inside
  the gate budget, and `kani-rio-retry-kernel` now runs in `checks.*`
  with the `r[verify]` markers at the wiring point and the
  harness-count tripwire pinned at 7 — this deferral is closed.
- The mirror-column DROP, the legacy-floor removal, and the frozen
  three-argument `decide()` — behind the drain condition, with the
  operational probe recorded (the mirror-column subsection above).
  **Closed by the retry-campaign coda** (migration 075 + the seed
  retirement; see that subsection's status paragraph — the fresh-only
  deployment directive made the drain condition vacuous).
- A real ledger GC policy (P8): the compile-time retention-floor
  assertion and the suffix bound stand in; a sweep is future work and
  must respect the recorded retention floor (≥ the poison TTL).
  **Closed by the TODO-closure wave (2026-06-02): the sweep exists.**
  `tick_gc_attempt_ledger` (leader-only, every 30th tick, batch 1000)
  consults the floor via the kernel's `sweep_horizon_secs(budget,
  LEDGER_RETENTION_FLOOR)` = max(floor, LIVE configured
  `infra_retry_window_secs`, poison TTL), and deletes only the suffix
  complement: attempt-kind rows strictly before the last reset row,
  past the horizon, with no active assignment — plus orphaned
  histories whose derivation row is gone. The deletion is
  machine-checked decide()-invariant: `suffix(sweep(L)) == suffix(L)`
  element-wise, hence `decide()` and `materialization_decide()`
  bit-identical (the `check_sweep_suffix_equivalence` /
  `check_sweep_decide_invariant` harnesses; the harness-count tripwire
  is now 10), with a bounded-exhaustive unit twin and the SQL↔kernel
  cut pinned cross-layer
  (`test_suffix_cut_matches_kernel_ledger_suffix_start`). Spec rule:
  `sched.db.attempts-gc` (scheduler.typ, next to the derivations-GC
  rule).
- The deliberately-open policy questions: A7 uniform backoff, A10
  fencepost unification, A8 poison-reason strings (P6 — unadjudicated
  policy changes, not refactoring debt).
- Model-based testing of the fold: the reasoned omission above, with
  its reconsideration triggers.

## Cross-campaign addendum — the pull-mode environment regime (executor-lifecycle campaign, slice 1b)

Added by the executor-lifecycle campaign (Phase-1 plan T-1b.7, the
T-0e.3 re-derivation recorded in
`docs/spec/models/executor-invariant-map.md`): `retryPolicy.qnt` now
carries an additional regime module, `retryPolicyPull`, that re-derives
the model's *environment* for the pull-path protocol — the attempt
opens at `PullAssignment`, the worker classes arrive over the
`ReportOutcome` unary, the controller's pod-terminal classification is
the idempotent `ReportAttemptOutcome` row fill (with the no-attempt
no-op), the establishment sweep is the only time-based repair, and the
exclusion / fleet-exhaust inputs are re-keyed to source nodes with the
AD2 small-fleet clause. The fold (`specApply`) and every invariant of
this map are imported unchanged; no as-built module or regime was
edited, and the wired as-built checks' state counts are unchanged
(re-verified at the introducing commit). The new wired checks are
`quint-retry-policy-pull` plus the `quint-retry-policy-pull-witness-*`
expect-violation probes in `nix/quint.nix`.

Authority boundary: the as-built channel regimes
(`retryPolicyWorker`/`Dual`/`Crash`/`Failover`) remain the
authoritative verification of the stream dispatch path for as long as
that path exists; the pull regime is additive coverage for the
coexistence window. Their retirement is scheduled with the stream
path's own retirement (executor campaign slice 1c', plan T-1c'.6) and
will be recorded here by the campaign that performs it, exactly as the
`retryPolicyAsBuilt` retirement above was.

## Cross-campaign addendum — the as-built-channel regime retirement (executor-lifecycle campaign, slice 1c', T-1c'.6)

Performed by the executor-lifecycle campaign (Phase-1 plan v3,
T-1c'.6), as the addendum above scheduled, after the 1c' deletion
commits removed the stream dispatch/session machinery the
as-built-channel regimes modeled. Recorded here by that campaign;
the fold, the invariants, the witness vals and this map's
authority over them are unchanged.

What retired in `retryPolicy.qnt`:

- The four as-built-channel regime modules and their named runs:
  `retryPolicyWorker`, `retryPolicyDual`, `retryPolicyCrash`,
  `retryPolicyFailover`. Their HOLD lists are carried by the
  pull-mode regime check (`quint-retry-policy-pull`), whose invariant
  list is the same set over the live event-arrival environment (plus
  `recoveryPreservesPoisonStatus`); the one regime-specific invariant
  not in the pull list, `attemptsBoundedGlobal` (crash regime), keeps
  its bound through `boundsOK` in the pull HOLD list and the
  `sched.retry.attempts-bounded+2` kani harness + fold unit tests.
  The named runs were stream-channel narratives (the documented
  divergence shapes D1–D4/C2 and the dedup walkthroughs); the
  adjudications they replayed stay recorded in this map's divergence
  catalog and are enforced by the same invariants in the pull HOLD
  list. No pull-mode named runs exist (none were added at 1b).
- The as-built-channel environment actions of the core module, now
  unreachable from any wired check and describing deleted code: the
  stream dispatch (`dispatchTo`), the dispatch-time fleet-exhaust arm
  (E9, `dispatchFleetExhaust`), the stream completion-intake wrappers
  (E1–E4, `processReport`/`processReportStale` — their classification
  content lives on in `specApply`, which the pull regime's
  `pullReportOutcome` applies), the disconnect entry point (E5,
  `processDisconnect`), the correlated controller reports (E6/E7,
  `ctrlDeliverOomLate`/`ctrlDeliverOomRaceAhead`/`ctrlDeliverDeadline`),
  the correlation-TTL establishment (`establishUnreportedCrash` — the
  pull regime's `pullEstablish` is the successor), the backstop (E8,
  `backstopFires`), the wedge (`buildWedges`), the executor respawn
  (`respawnExecutor`), the as-built failover action (`leaderFailover`
  — `pullLeaderFailover` is the successor) and the as-built `step`.
  Kept: the clock, the physical attempt-end events, the lost
  controller report, the resets, the shared infra-classification arm
  (`infraArm`) and the appending-transaction fault (`attemptTxFails`,
  disabled at TX_FAULTS = 0 in the pull regime, retained as a
  documented manual/evidence action) — exactly the environment
  `pullStep` composes plus the fold/type/invariant infrastructure.

What retired in `nix/quint.nix` (and what carries each pin now):

| Retired check | What it pinned | Carrier after T-1c'.6 |
|---|---|---|
| `quint-retry-policy-worker` (exhaustive) | the fold over the full worker alphabet | `quint-retry-policy-pull` (same invariants, pull alphabet); fold unit tests + kani unchanged |
| `quint-retry-policy-dual` (exhaustive) | the dedup/terminal discipline over the mixed channels | `quint-retry-policy-pull` (worker + controller-fill + establishment alphabet) |
| `quint-retry-policy-crash` (exhaustive) | the C2 established-crash loop boundedness | `quint-retry-policy-pull` (`pullEstablish` + `boundsOK`); the crash-terminal pin re-wired (below) |
| `quint-retry-policy-failover` (exhaustive) | failover/recovery off the durable fold | `quint-retry-policy-pull` (`pullLeaderFailover`, `failoverPreservesHistory`, `recoveryPreservesPoisonStatus`) |
| `quint-retry-policy-runs-{worker,dual,crash,failover}` | deterministic divergence-reproducer narratives | retired with their regimes; the adjudications stay in the divergence catalog, enforced by the pull HOLD list |
| witness `threshold` | Poison(Threshold) reachable | re-wired: `quint-retry-policy-pull-witness-threshold` |
| witness `infra-cap` / `exempt-cap` | infra / exempt cap poisons reachable | the infra/exempt charge arms are in the pull alphabet (`pullAttemptOutcomeOom`, `pullReportOutcome` infra classes, nondet promoted/atCap); the cap arithmetic keeps its fold unit tests + kani; poison reachability is re-pinned by the threshold witness |
| witness `timeout-cancel` | worker timeout cap ends Cancelled | the Timeout class is in the pull alphabet; `verdictMatchesFold` (D1 adjudication) HOLDs on the pull regime; fold unit tests |
| witness `window-reset` / `exempt-fallthrough` | the I-127 window reset and its exempt fall-through | fold-internal (`specApply` unchanged by the environment swap); fold unit tests + `countersRefineHistory` HOLD on the pull regime |
| witness `cache-hit` | the cache-hit poison clear reachable | re-wired: `quint-retry-policy-pull-witness-cache-hit` |
| witness `ttl-expiry` | the poison-TTL expiry clear reachable | verified violating on the pull regime (exhaustive TLC, figures in the introducing commit message) but demoted to a documented manual target instead of a wired check: its derivation repeatedly hit the documented cold-server conversion flake while identically-shaped siblings built green. Manual recipe: `quint verify --backend=tlc --main=retryPolicyPull --step=pullStep --invariant=noTtlExpiry docs/spec/models/retryPolicy.qnt`. The TTL-clear lifecycle additionally keeps its scheduler unit/VM coverage (`poisonIsTerminalUntilCleared` + `clearedPoison*` HOLD in the wired pull check). Re-wire alongside the siblings when the conversion flake is addressed. |
| witness `controller-cap` | controller deadline cap ends Cancelled | `pullAttemptOutcomeDeadline` (OE7) is in the pull alphabet; `verdictMatchesFold` HOLD + fold unit tests |
| witness `promoted-termination` | promoted controller termination | `pullAttemptOutcomeOom(promoted)` in the pull alphabet; floor arithmetic stays with floor.rs unit tests (G6 NOT-ENCODED stands) |
| witness `atcap-termination` | at-cap controller termination charges | already pinned: `quint-retry-policy-pull-witness-fill-charge` (same witness val) |
| witness `late-installment` | report correlated after the disconnect already requeued | the correlation channel is deleted; the pull-path analog (the reason-only second installment on a recorded row) is pinned by the re-targeted Model S witness `canReachSecondInstallment` and explored by the pull regime |
| witness `race-ahead` | controller report racing the disconnect | the disconnect channel is deleted; the analogous pull-path interleaving (controller fill on a still-open, unrecorded attempt) is in the pull alphabet; the never-opened side is pinned by `quint-retry-policy-pull-witness-no-attempt-noop` |
| witness `fleet-exhaust` | FleetExhausted poison reachable | already pinned: `quint-retry-policy-pull-witness-fleet-exhaust` |
| witness `crash-charge` | establishment charge reachable | already pinned: `quint-retry-policy-pull-witness-establishment` |
| witness `crash-terminal` | the established-crash loop reaches a terminal | re-wired: `quint-retry-policy-pull-witness-crash-terminal` |
| witness `failover-history` | failover on a non-empty under-budget history | re-wired: `quint-retry-policy-pull-witness-failover-history` |
| witness `tx-failure` | a failed appending transaction charges nothing | retired without a wired successor: TX_FAULTS = 0 in the pull regime, so the inversion has no wired producer; the post-066 single-transaction property keeps its code-level coverage (the `append_and_decide_in_tx` error-arm unit tests), and `attemptTxFails` stays in the core module as the documented manual target should a pull fault regime be wired later. Recorded, not silently dropped. |
| witness `failover-poisoned` | failover on a live poisoned durable row | verified violating on the pull regime (exhaustive TLC, figures in the introducing commit message) but demoted to a documented manual target for the same conversion-flake reason as the ttl-expiry pin. Manual recipe: `quint verify --backend=tlc --main=retryPolicyPull --step=pullStep --invariant=noFailoverOnPoisonedRow docs/spec/models/retryPolicy.qnt`. `recoveryPreservesPoisonStatus` itself stays exhaustively HOLD in the wired pull check, and the failover-with-history pin is wired. |

Verify-marker re-points: the model-checked markers held by the
retired regime checks move to `quint-retry-policy-pull`
(`sched.retry.counters-refine-history+2`, `sched.retry.no-double-count`,
`sched.retry.verdict-channel-invariant`,
`sched.poison.cascade-dependents`, `sched.retry.failover-budget`,
`sched.retry.recovery-projection+2`).
`sched.retry.transient-budget` and `sched.retry.attempts-bounded+2`
drop their model markers (their kani harnesses and fold unit-test
markers are unchanged); the rules stay covered.

Bit-identical gate: `retryPolicyPull` itself was not edited (its
actions, constants and `pullStep` are untouched; the deletions are
all in code unreachable from it), and the re-built
`quint-retry-policy-pull` check reports the same distinct-state count
and depth as the pre-retirement build (figures in the introducing
commit message and the check transcripts).

## The retry-campaign coda (the two recorded follow-ups, executed)

Performed by the retry campaign after the executor-lifecycle campaign's
close-out, once the deployment model was clarified as fresh-only
(2026-05-27/28 directives: no staged rollouts, no live databases). Both
deferral entries this map carried are closed; the figures live in the
introducing commit messages and the check transcripts.

1. **Mirror-column drop + legacy-seed retirement (the Phase-2
   deferral).** Migration 073 (not the reserved 067 — see the status
   paragraph in the mirror-column subsection and M_075) drops
   `derivations.{retry_count, failed_builders, resubmit_cycles}`; the
   reader/writer sweep and the seed retirement follow. `decide()` is
   the frozen §5a-2 three-argument surface again; the contracts' seed
   clauses, `check_legacy_seed_merge_monotone` and `any_seed` are
   retired (the `kani-rio-retry-kernel` harness-count tripwire is 6 and
   all six harnesses VERIFY); `sched.retry.recovery-projection+3`,
   `sched.merge.poisoned-resubmit-bounded+4` and
   `sched.db.clear-poison-batch+3` re-state the rules whose bodies
   named the columns; the pull-regime check and witnesses re-verified
   green with unchanged state counts.

2. **P12 — the pod-name exclusion-key drop (the executor campaign's
   deferred item, retry-co-owned).** The kernel's event identity is
   `Option<Id>`: the four threshold-charging arms insert into the
   exclusion set only when an identity is present, and the scheduler
   shim's fold-input projection keys rows on
   `drv_attempts.source_node` alone (the `or_else(executor_id)`
   fallback is deleted). An identity-less row — a pull attempt whose
   binding ack never landed, or a pre-pull legacy row — charges the
   flat `failure_count` but occupies no distinct-source slot and leaks
   no non-schedulable key into placement; such histories are bounded
   by the per-cycle caps (and the flat-count mode), not by the
   distinct-source threshold — the threshold-semantics question the
   blocker note left open is resolved that way and stated normatively
   in `sched.retry.per-executor-budget+4`, which also shrinks the
   establishment-vehicle list to the establishment sweep. Red-first
   coverage: `exclusion_keys_are_source_nodes_only` (the successor of
   the mixed-era both-keys test) plus the five actor-test conversions
   recorded in the introducing commit. The kani contracts re-verified
   over the new alphabet (6/6); the pull-regime model is untouched by
   the change (its exclusion inputs were node-keyed from the start).

The deliberately-open items (A7 uniform backoff, A10 fencepost
unification, A8 poison-reason strings, the ledger GC policy, the MBT
omission with its reconsideration triggers) are unchanged by this coda
and remain the campaign's only open list.

## Cross-campaign addendum — the materialization attempt class (substitution-replacement campaign, Phase A, T-5.1/T-5.2)

Added by the substitution-replacement campaign (Phase A plan T-5.1/T-5.2;
design §2.5/§9.2, OQ1 amendments 1–2). `retryPolicy.qnt` now carries the
materialization attempt class — the kind partition the campaign adds to
the attempt ledger: materialization attempts (store-replica executed,
`drv_executions.attempt_kind = 'materialization'`) charge their own
bounded budget, are invisible to every build budget, and never poison;
the establishment sweep charges a crashed materialization attempt as
`materialization_infra`, never `executor_crash`.

What changed in the model:

- **Structural**: the pull-mode environment definitions moved verbatim
  from the former standalone `retryPolicyPull` module into the core
  `retryPolicy` module (no action text changed), so that regime modules
  can instantiate them with per-regime constants — the one-model-N-
  regimes discipline the rest of the model corpus follows. Two thin
  regime modules now close the file: `retryPolicyPull` (the wired
  build-only regime, `ENABLE_MATERIALIZATION = false`) and
  `retryPolicyPullMat` (the materialization-coexistence regime,
  `ENABLE_MATERIALIZATION = true`).
- **The materialization attempt class** (core sections 11/11b/11c):
  three bounded counters carried as the materialization ledger's fold
  (`matInfraCount`, `matSchedulerInfraCount`, `matUnobtainableCount`,
  each ceilinged by an action-precondition guard and checked by the
  extended `boundsOK` — the SC-1 no-unbounded-variables rule), a
  build-side snapshot ghost (`matObservedBuild`), three actions
  (`materializationReportUnobtainable`, `materializationReportInfra`,
  `establishMaterializationCrash` — the OQ1-amendment-1 channel), two
  partition invariants and one witness.
- **`pullStep`** composes every build-side disjunct with
  `buildLeavesMatUntouched` and adds the three materialization
  disjuncts, enabled only where `ENABLE_MATERIALIZATION = true`.

The two new invariants (both encoded in the pre-state-snapshot tripwire
style — the model's section-11 encoding note records why no predicate
over the (build × materialization) product alone can catch a partition
leak):

| Invariant | Claim | Verdict (wired check) |
|---|---|---|
| `materializationNeverPoisons` | no materialization-kind charge — including establishment-written crash charges — ever produces a Poison/Cancel verdict or touches the cascade, at any budget level including park | HOLD (`quint-retry-policy-pull-materialization`) |
| `materializationInvisibleToBuildBudgets` | materialization charges feed exactly one budget (their own); every build-side budget view (cached counters, reference fold, durable ledger fold, verdict, open attempt, dispatch ghost) is untouched by them | HOLD (`quint-retry-policy-pull-materialization`) |

Wired checks after this addendum:

| Check | Regime | Invariants / witness | Verdict |
|---|---|---|---|
| `quint-retry-policy-pull` | build-only (materialization dormant) | the pre-existing 14, **list unchanged** | HOLD — bit-identical state space to the pre-extension baseline (same generated count, same distinct count, same outdegree distribution; figures in the introducing commit message and the check transcripts) |
| `quint-retry-policy-pull-witness-*` (9) | build-only | unchanged | all still violate (re-built and re-verified at the introducing commit) |
| `quint-retry-policy-pull-materialization` (NEW) | materialization-coexistence | the same 14 **plus** the two partition invariants (16) | HOLD |
| `quint-retry-policy-pull-witness-materialization-crash` (NEW) | materialization-coexistence | `noMaterializationCrashCharge` | violates (the establishment crash channel is reachable and charges the materialization budget) |

The materialization-coexistence regime's alphabet: the worker-report
charge channel (E1–E4), the no-report crash death and its build-side
establishment (the OQ1 adjacency — both establishment channels are
reachable, and the partition invariants pin that the materialization one
never feeds the build fold), dispatch, the spawn-gate exhaust and the
source-universe shrink. The controller-fill machinery, the resets and
leader failover remain the build-only regime's exhaustively-proven
concern; the materialization class is structurally independent of them
(the partition invariants force exactly that). The wiring-decision
record — why the partition invariants live in a separate regime check
instead of growing the build-only check's own alphabet (the state-space
product arithmetic vs the Phase-A stop-condition-8 wall-clock
thresholds) — is in nix/quint.nix at the check definition and in the
introducing commit's message.

Calibration (the working-tree falsification discipline, OQ1 amendment
1's "checked against the channel that can violate them"): performed
before wiring, transcripts in the introducing commit's message and the
Phase-A notes file. Direction materialization→build (scratch edit:
`establishMaterializationCrash` writes `executor_crash` into the build
ledger, full verdict-acting form): `materializationInvisibleToBuildBudgets`
violated immediately, `materializationNeverPoisons` violated once the
poison threshold is crossed, and — load-bearing for the encoding choice —
the pre-existing `countersRefineHistory` does NOT catch it (the leak
advances `live` and `spec` together), which is why the snapshot-ghost
encoding exists. Direction build→materialization (scratch edit: the
worker-report disjunct also bumps `matInfraCount`): the extended
`boundsOK` violated (ceiling overrun — a build action carries no
materialization ceiling guard), in both regimes.

Phase C′ obligations recorded here: the calibration scratch edits become
wired expect-violation pins (the Phase-A plan's T-5.1 step 5 records
them as working-tree calibrations only); `materializationJob.qnt` (the
draft this campaign's T-5.3 added) is completed and wired as the §9.1
check set, at which point the partition invariants are checked in both
models.

## Cross-campaign addendum — the per-lane ledger cut (bughunt wave, A2 kind-partition-completion)

Migration 084 put `attempt_kind` ON every `drv_attempts` row (constructor
parameter in `AttemptRow::new`/`new_reset` — a row that forgets its lane
is uncompilable), and the suffix loaders + GC sweep now cut PER LANE:
each row survives iff it is at-or-after the last reset row of ITS OWN
lane (`rio_retry_kernel::row_survives_load`; SQL transcribed as a
kind-correlated LATERAL). The pre-084 any-kind cut let a build resubmit
reset hide (loader) and then delete (GC) materialization-infra
evidence — recorded reds: loader `left: 0, right: 2`, GC `left: 1,
right: 0` (merged_bug_011); a parked job's budget silently re-opened.
Proof surface: `check_sweep_suffix_equivalence` and
`check_sweep_decide_invariant` re-stated over the loaded view at their
unchanged per-harness bounds (MAX=5/unwind 8; MAX=4/unwind 7), NEW
`check_loader_cut_preserves_materialization_decide` (the view loses no
materialization-decision information relative to the full history),
and the bounded-exhaustive twin re-drawn over an 8-shape two-lane
alphabet.

**Mat-lane reset rows — LIVE (A3 materialization-lifecycle-kernel,
2026-06-03; replaces the documented-absence paragraph this slot
carried).** The production writer is `create_materialization_jobs_in_tx`
(migration 085, outcome class `materialization_reset`): ONE
`AttemptRow::new_reset(.., AttemptKind::Materialization)` row per
genuinely created job, in the SAME transaction as the job INSERT — the
dedup arm writes none, so a found pending job keeps its window. The
row is the per-job budget window (merged_bug_020): the kernel cut is
`(attempt_kind, event_kind)` (`ledger_suffix_start`; the class string
is row data, never the cut predicate), so a successor job's
budget/one-shot/strictness counts start fresh — identically live (the
post-commit `mirror_job_creation_reset` feed), post-failover (the
suffix loaders return the suffix INCLUDING its anchor reset), and
under the GC sweep (which preserves per-lane suffixes). The flat
per-class history counts were deleted with the writers' fusion: every
consumer reads `rio_retry_kernel::materialization_counters` (one
windowed fold; `materialization_decide` is its `infra >= max`
projection, pinned by the `check_materialization_counters_window`
harness and the `materialization_counters_projection_differential`
unit), and every `materialization_infra` charge — worker-reported AND
establishment-written — executes the park verdict through the single
`charge_materialization_infra` chokepoint (bug_067, the owner-signed
Q5 reversal of residual (a): party-blind parking; see the
substitution-replacement map's superseded residual block).

## Cross-campaign addendum — the store-degraded pacing class (bughunt wave, B1 bounded-await-transport)

`OutcomeClass::StoreDegraded` (migration 088; the 17th alphabet
literal) is **pacing, not evidence** — the outcome the FUSE breaker
stamps when the worker's own store connectivity degraded mid-build
(`BuildResult.store_degraded`, both open transitions counted by the
breaker's monotonic `trips`). The kernel's fold answers `row_to_event
→ None`, so `apply()` structurally cannot charge the class: no counter
moves, no exclusion key is minted, no poison threshold sees it, and
the verdict stays whatever the charged history decided. The only
effect is the fold-local consecutive-run backoff
(`store_degraded_run`, never persisted, reset by any folded event) —
the derivation **waits out the outage at the curve's cap** instead of
marching to the poison threshold. Recorded red (bug_408): eleven
flagged reports left `left: Poisoned` — a store outage poisoned the
derivation and excluded every builder that honestly reported it.

The rule (`sched.retry.store-degraded-uncharged`, now **+2**)
carve-out amended `sched.retry.attempts-bounded` to **+4**: the
uncharged store-degraded run is bounded by COUNT
(`STORE_DEGRADED_FREE_RUN = 12`; `admit_store_degraded` walks the
ledger suffix exactly as `admit_worker_abort` does) — bughunt-2 slot 3
(m032) closed round 1\'s worker-trusted unbounded posture: the bound-th
consecutive flagged close is the last uncharged one; past it the
report folds CHARGED as plain worker infra (the RunBound disposition),
breaking the run so a recovering store earns a fresh gate. Intake
side: the scheduler additionally requires CORROBORATION before
believing the flag (a second node\'s sighting inside the 600s window or
the store-health leg — `StoreDegradedDisposition`), so a single lying
worker paces only itself uncharged-bounded and never skips the floor.
The report path skips the floor bump only for believed-store
dispositions and writes the fold\'s deadline through the live-backoff
carve-out (B1-s2 commit 2; bughunt-2 C2).

**Proof surface.** Kani: `check_store_degraded_uncharged_requeue`
(decide over N store-degraded rows ⇒ Requeue, all counters zero,
exclusion empty, never Poison; the classify iff-clause pins
`WorkerStoreDegraded ↔ StoreDegraded`). Quint
(`docs/spec/models/retryPolicy.qnt`): the contract triple
`storeDegradedNeverPoisons` / `storeDegradedDrawsNoBudget` /
`storeDegradedMintsNoExclusion` holds exhaustively in the
`retryPolicyPullStoreDegraded` regime (every other regime binds
`ENABLE_STORE_DEGRADED = false` and stays bit-identical — the dormancy
discipline); the falsifiability pair is the `retry-408-sd-as-infra`
calibration (the pre-fix class-blind fold: budget drawn and exclusion
minted on the first report, poisoned at the cap), and the
fleet-correlated reachability pin is `correlatedStoreOutageRun` (three
consecutive store-degraded closes across both workers leave the
derivation requeued, all counters zero, exclusion empty). The curve
arithmetic itself is below the model's untimed-backoff floor, the same
posture as the establishment window's timing.

**Bughunt-2 slot 3 addendum (m032, the run bound).** Model: `var
storeDegradedRun` (QNT000-framed across the full alphabet: SD closes
increment, every other Build-row write zeroes — including the
uncharged worker-abort rows, which ARE ledger rows — non-row actions
frame, failover frames because the rows are durable);
`pullStoreDegraded` gains the admission guard (`storeDegradedRun <
STORE_DEGRADED_FREE_RUN`, scaled 3); `pullStoreDegradedRunBound` is
the charged fallthrough (FInfra fold, distinct `OStoreDegradedCharged`
observation so the uncharged triple stays exact). Invariant
`boundedStoreDegradedRun` holds exhaustively in
`retryPolicyPullStoreDegraded`; falsify twin
`retry-032-unbounded-degraded` (the pre-bound intake: guard removed,
fallthrough removed) violates it with the trace passing
`storeDegradedRun = 3 → 4` uncharged; witnesses
`canReachStoreDegradedBound` (the bound edge is reachable) and
`canChargePastBound` (the fallthrough actually fires — the liveness
direction the uncharged triple alone could never falsify) are both
violation-wired; scenario pin `sustainedOutageChargesPastBound` (three
uncharged closes exhaust the gate, the fourth charges and breaks the
run). SD-regime exhaustive re-measure with the run plane: 291.6M
states generated / 58.6M distinct / 5min13s (was 260.5M/268s — 1.12×,
the run variable correlates with history instead of multiplying it). Kani: `check_store_degraded_admission_bounded` (symbolic-bound
admission table + growth lemma; rio-retry-kernel 16 harnesses).
**Model-boundary narrowing:** the corroboration gate (the C2
two-sighting/health-leg requirement and its 600s window) is BELOW this
model\'s abstraction floor — the model\'s single-derivation view has no
second node to corroborate with and no wall clock; what the model pins
is the per-derivation run bound and the charged fallthrough, which
hold regardless of the corroboration verdict (corroboration only
narrows which closes are believed, never widens the uncharged run).

**None-sensible record (B1's two formal-delta omissions, per the §2
formal delta items 5–6).** (a) `IdleClock` (the builder's
outage-excluding idle-exit clock, merged_bug_209): kani is
none-sensible — `rio-builder` is a bin crate (`nix/kani.nix`'s
bin-crate exclusion) and the clock is ~30 lines of pure told-time
arithmetic; the proof surface is the proptest
(`idle_for == Σ min(gap, 2·prev_suggested)` over answer-adjacent
pairs; errors never advance) plus the unit battery. (b)
`bounded()`/`GraceBudget` (the transport primitive): none-sensible for
quint — a local two-arm race primitive whose model would restate
tokio `select` semantics; correctness is type-level (`#[must_use]
BoundedOutcome`), the `transport-unary-ban` policy check, and the unit
battery (biased shutdown, timeout, budget arithmetic, `GraceBudget`
const-asserts).
