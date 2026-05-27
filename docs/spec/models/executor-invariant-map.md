# Executor-lifecycle / session-protocol invariant ↔ spec-rule map

Campaign #1 of the simplification arc: verify the as-built
scheduler⇄executor session machinery (registration, heartbeat,
assignment push, completion delivery, disconnect/reap/reconcile,
draining, the controller's Job/pod lifecycle and termination
reporting), then — only on a 0e "go" — replace it with the
"assignment IS the pod" pull protocol. The contract for this map is
`executor-formal-design.md` (DRAFT v2) §3, §5, §6; the evidence base
is `executor-inventory.md` (snapshot `e650f23a4`, 2026-05-25),
re-pinned below. The methodology is the one proven on rio-lease, the
log subsystem, retry-formal, refcount, and the controller campaign.

This file is the campaign artifact. Stage 0a (landed) added the churn
pin, the inventory re-pin, the corpus pin with its pre-registered
partition, the encodability pre-registration, and the
open-adjudication tracking. Stage 0b (this section set, directly
below) adds the invariant ↔ rule map proper (one subsection per F1–F8
invariant), the new spec rules, the contradiction records, and the
witness / as-built-falsification pre-registrations. Stage 0c adds the
Stage-B model verdicts (`executorSession.qnt`, `executorDelivery.qnt`).
Stage 0d adds the Stage-C calibration table against the corpus pinned
here. Stage 0e adds the frozen replacement contract and the go/no-go
record.

## The decision sites (the columns of every row below)

The unit of audit is the repair mechanism / decision site, not the
file: the rows below cite the 0a-pinned anchors of the inventory's
mechanism tables — scheduler #1–#22, builder B1–B9, controller C1–C6
(see "Inventory re-pin: anchor re-verification" below for the
file:line of every one at the pin) — plus the non-repair decision
sites of the dispatch/intake path:

| Site | What decides there | At the 0a pin |
|---|---|---|
| connect / accept-gate | stream accept, hijack/intent rejection, epoch + `auth_intent` stamp, stale-flag clear | `grpc/executor_service.rs:208-239`, `actor/executor.rs:116-247` (#5, #6) |
| heartbeat intake | entry-existence drop (I-048b), spoof guard, field refresh, running-build reconcile (keep / adopt / two-strike), capacity edge, registration edge | `actor/executor.rs:1444` (handler), `:1680` (reconcile), `:1860` (adopt), `:1803` (drain_phantoms) (#7–#10, #22) |
| eligibility / placement | `rejection_reason` clause chain, `has_capacity`, `statically_eligible`, warm two-pass, intent-match | `assignment.rs:25/:136/:175`, `dispatch.rs:1631+` (#11) |
| assignment push | 4-phase `assign_to_worker` (transition, persist + exec_id mint + pin, `try_send`, events) and its rollback | `dispatch.rs:1851+`, rollback `:2077` (#18) |
| completion intake | slot free hoist, `last_completed`, one-shot draining, output validation, idempotency / staleness guards, retry-fold routing | `completion.rs:898-1110` (#12, #13) |
| disconnect / reap / report | stale-epoch filter, mid-build discriminator, reassign, termination-report dedup + prefix-match, TTL sweep / establishment, backstop | `actor/executor.rs:347/:565/:676/:1168`, `housekeeping.rs:296/:314` (#1–#4, #14–#17) |
| failover edges | `clear_persisted_state`, leader gates, 45 s reconcile sweep | `actor/mod.rs:872/:1190`, `recovery.rs:1779/:2153` (#19, #21) |
| builder delivery / exit | send chokepoint, sink + relay swap, half-close, drain gate, idle exit, generation fence, slot claim | `runtime/result.rs:239`, `runtime/mod.rs:907-912/:1054/:985`, `runtime/drain.rs:37-120`, `runtime/slot.rs:63` (B1–B9) |
| controller Job view | spawn / reap / ack tick, orphan + excess reaps, termination reports, disruption watcher | `pool/jobs.rs:785+`, `pool/job.rs:373/:496/:990/:1090`, `pool/disruption.rs` (C1–C6) |

## The invariant ↔ rule map (Stage A)

Verdict legend: **COVERS** — the rule's normative sentence states the
invariant (or the load-bearing piece of it). **PARTIAL** — the rule
states a piece; the missing piece is named. **GAP** — no rule stated
it; closed by a new `#r()` rule in this audit (the new rules carry the
design §3.1/§3.3 invariant definitions as their normative bodies).
**CONTRADICTION** — the code does not do what the rule says it MUST;
recorded in the contradiction table below, never fixed and never
modeled around in Phase 0.

The five new rules added by this audit (T-0b.2):
`sched.executor.session-epoch`, `sched.executor.liveness-window`,
`sched.executor.repair-precedence`, `sched.executor.one-shot`
(`docs/spec/components/scheduler.typ`, Executor Registration Protocol
section), and `builder.completion.exactly-once-or-death`
(`docs/spec/components/builder.typ`, Stream Relay & Reconnect
section). Existing rule text is NOT amended in Phase 0; no tracey
bumps were needed.

### F1 — Session identity (Model S)

#### `AtMostOneLiveStreamPerExecutor`

*At most one live `BuildExecution` stream is bound to an executor id
at any time; an accepted reconnect replaces the binding, a rejected
one changes nothing.*

Sites: #6 (accept-gate `executor_service.rs:226-239`, actor reject
`actor/executor.rs:148-176`), #5 (reconnect reuse). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sec.executor.identity-token+2` | **COVERS** (the reject half) | A reconnect whose token intent differs from the stored `auth_intent` *or whose existing stream is still live* MUST be rejected, and the handler MUST learn the actor's accept/reject decision before spawning the reader — the hijack and duplicate-stream arms are already normative here. |
| `sched.executor.session-epoch` *(new)* | **COVERS** (the replace half) | An accepted reconnect replaces `stream_tx` and `stream_epoch` together, so the entry never holds two live streams; events from the replaced stream become stale by construction. Was a GAP. |
| `sched.executor.dual-register` | PARTIAL | States the two-step registration; says nothing about second streams or replacement. The mechanism-description staleness in its prose is a near-miss (below), not load-bearing for this invariant. |

#### `StaleStreamEventsAreInert`

*A disconnect / flag write / session event from epoch N mutates
nothing once the slot is at epoch M>N.*

Sites: #4 (`actor/executor.rs:363-371`), #1 (reaper synthesizes at
current epoch, `housekeeping.rs:304-310`), #5 (clear-on-reconnect
scopes flags to the new session). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.session-epoch` *(new)* | **COVERS** | Was a GAP — the I-056a stale-disconnect filter existed only as code + incident comment. The rule states attribution (epoch carried by reader-task and reaper-synthesized disconnects) and inertness (no removal, no reassign, no gauge decrement). |
| `sched.executor.deregister-reassign` | CONTRADICTION (C2) | Its unqualified "removed when the stream is closed" does not hold for a stale-epoch close — recorded in the contradiction table; the code is correct and the qualification is now normative in the new rule. |

#### `RegistrationRequiresBothHalves`

*No dispatch to a slot that lacks either the stream or a first
heartbeat; heartbeats never create entries.*

Sites: #7 (I-048b drop, `actor/executor.rs:1446-1467`),
`is_registered` (`state/executor.rs:229`), the registration edge in
`handle_heartbeat`. Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.dual-register` | **COVERS** (the both-halves requirement) | Dispatchable only once both the stream and the first heartbeat have landed. |
| `sched.executor.session-epoch` *(new)* | **COVERS** (the never-create half) | "A heartbeat for an `executor_id` with no live entry MUST NOT create session state" — the I-048b zombie-prevention arm, previously code-only. |

### F2 — Outcome delivery (Models S + D)

#### `ClaimedSlotResolvesAtMostOnce` (S)

*An accepted assignment is never counted as both a leader-observed
completion and a repair return-to-Ready, and no second resolution is
recorded after either.*

Sites: #12 (idempotency / staleness guards `completion.rs:1059-1110`),
#15 (termination dedup `actor/executor.rs:710-752`), #10 (phantom
ownership check), #17 (backstop acts only on still-Running). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.completion.idempotent` | **COVERS** (the report-side half) | Already-completed, cancelled-expected, not-Assigned/Running, and stale-executor reports are accepted-and-ignored. |
| `sched.executor.repair-precedence` *(new)* | **COVERS** (the cross-mechanism half) | After any one mechanism resolves a claim, every later observer MUST find a terminal status / non-owner mismatch and change nothing. Was a GAP as a composition statement. |
| `sched.retry.no-double-count` | **COVERS** (the accounting projection) | Retry-campaign rule; imported, not re-stated. |

#### `UnresolvedClaimHasRepairArmed` (S, armed safety)

*In every state where an accepted assignment is neither completed nor
returned to Ready, at least one repair mechanism is enabled for it.*

Sites: #1/#3 (disconnect → reassign), #10 (phantom two-strike), #17
(backstop), #19 (post-failover sweep), B1–B4 (the builder still owes
the report). Model S (with the builder obligation discharged by Model
D).

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.repair-precedence` *(new)* | **COVERS** (the armed-safety statement) | "While the report has not arrived, at least one repair MUST remain armed for the claim" — previously each arm was specified alone and nothing stated that the disjunction is exhaustive. Was a GAP. |
| `sched.executor.deregister-reassign` | **COVERS** (its arm) | Stream-close / heartbeat-timeout → reassign. |
| `sched.heartbeat.phantom-drain+2` | **COVERS** (its arm) | Two-strike phantom → reset to Ready. |
| `sched.backstop.timeout+3` | **COVERS** (its arm) | Heartbeating-but-wedged → cancel + quarantine + requeue. |
| `builder.completion.exactly-once-or-death` *(new)* | **COVERS** (the peer obligation) | The builder delivers the report or dies trying; pod death re-arms the scheduler-side repairs via the death channels. |
| `sched.executor.liveness-window` *(new)* | **COVERS** (the arming bounds) | The windows after which each arm is enabled are now normative values. |

#### `NoFabricatedCompletion` (S)

*No mechanism invents a completion for a build the worker did not
report; repair paths return work to Ready (or adopt store-present
outputs) without synthesizing worker outcomes.*

Sites: #19 (store-probe adopt vs reset, `recovery.rs:1779+`), #10
(phantom drain charges nothing), the gRPC empty-result synthesis
(`executor_service.rs:380-395` — a worker-sent report with a missing
payload becomes InfrastructureFailure; documented bound, not
fabrication: the worker did report). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.repair-precedence` *(new)* | **COVERS** | The post-failover class: the store probe decides adopt-as-completed vs reset-to-Ready and MUST NOT fabricate a completion or charge an attempt for a derivation it merely resets; the phantom drain MUST NOT charge. Was a GAP. |
| `builder.completion.exactly-once-or-death` *(new)* | **COVERS** (worker side) | The builder MUST NOT fabricate a report for an assignment it did not accept. |

#### `ReportSurvivesStreamChurn` (D)

*A terminal outcome queued for delivery survives stream loss, leader
failover, and relay swaps until it reaches a live leader.*

Sites: B1 (`runtime/mod.rs:814+`), B2 (swap-after-Ok `:907-912`), B4
(`completion_pending` + sink, `result.rs:227-253`,
`drain.rs:37-120`). Model D.

| Rule | Verdict | Audit finding |
|---|---|---|
| `builder.relay.reconnect` | **COVERS** | Permanent sink, parked relay, biased re-resolve, in-transit message recovery. |
| `builder.completion.pending-armed-early` | **COVERS** (the panic path) | The flag means "owed", armed before the first await. |
| `builder.relay.graceful-exit-close` | **COVERS** (the exit edge) | Flush before stream drop. |
| `builder.completion.exactly-once-or-death` *(new)* | **COVERS** (top-level obligation) | The end-to-end exactly-once-or-death statement Model D checks; previously implicit in the four mechanism rules. Was a GAP as a composition statement. |

#### `NoExitWithReportOwed` (D)

*No graceful exit path runs while a completion is owed and not yet
flushed to a confirmed-open stream.*

Sites: B3 (half-close `runtime/mod.rs:1054+`), B4 (drain gate
`drain.rs:85-120`), the idle fast-path gate. Model D.

| Rule | Verdict | Audit finding |
|---|---|---|
| `builder.shutdown.idle-no-reregister+2` | **COVERS** (the drain/idle fast-path gate) | Break only when draining ∧ slot idle ∧ no completion pending; reconnect-once-to-flush otherwise. |
| `builder.relay.graceful-exit-close` | **COVERS** (the flush) | Park + drain to server-close before dropping the stream. |
| `builder.completion.exactly-once-or-death` *(new)* | **COVERS** | The general "no graceful exit while `completion_pending`" MUST. |
| `builder.idle-exit+2` | adjacent | The idle exit takes the same flushed BuildComplete exit path; no separate verdict needed. |

### F3 — Liveness calibration (Model S)

#### `NoReapWhileFreshInWorkerTime`

*A slot whose worker heartbeated within the timeout bucket (net of
stall credit) is never reaped, never marked phantom-suspect, and never
excluded from dispatch by bookkeeping alone.*

Sites: #1 (`housekeeping.rs:296-312`), #2 (stall credit `:240-248`),
B8 (heartbeat RPC bound `runtime/heartbeat.rs:24-36`), #10 (two-strike
discipline). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.liveness-window` *(new)* | **COVERS** | Worker-time measurement (stall credit) + the 30 s value + the two-strike phantom window. Was a GAP (numbers and discipline were code constants + incident comments). |
| `sched.executor.deregister-reassign` | PARTIAL | States the 30 s derivation but not the worker-time discipline. |
| `builder.heartbeat.rpc-timeout` | **COVERS** (builder half) | One stalled RPC must not consume the whole missed-heartbeat budget (bug_044). |

#### `SilentSlotReapArmed` (enabled-implies-fires)

*Once a slot is past the timeout bucket measured in worker time, the
reaper action is enabled for it, a reaper tick taken in that state
reaps it, and no non-heartbeat action disables it.*

Sites: #1, #2. Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.deregister-reassign` | **COVERS** | Timeout ⇒ deregister + reassign is already normative. |
| `sched.executor.liveness-window` *(new)* | **COVERS** (the value + the only-credit-disables discipline) | Stall credit is the only thing that legitimately moves `last_heartbeat` forward without a heartbeat. |

### F4 — Death attribution (Model S; the lifecycle the retry model takes as given)

`OnePodDeathAtMostOneCharge` and `TrueReasonWins` are imported from
`retryPolicy.qnt` (`attemptsChargedOnce`, channel invariance) and are
NOT re-stated here; what Model S owns is the lifecycle of the dedup
state.

#### `CorrelationEntryLifecycle`

*A `recently_disconnected` entry is created only by a mid-build
disconnect, consumed by exactly one of {classifying report,
establishment}, and swept only after the TTL.*

Sites: #14 (insert `actor/executor.rs:408-440`, sweep `:1168-1290`),
#15 (consume `:718-752`, race-ahead arm), #13 (`last_completed`
discriminator `completion.rs:1000-1007`). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.reassign.no-promote-on-ephemeral-disconnect+4` | **COVERS** (create + TTL + controller authority) | Mid-build-only insert, 60 s TTL, controller-authoritative reason, no entry after `last_completed == running_build`. |
| `sched.retry.no-double-count` | **COVERS** (consume-once) | Retry-campaign rule; its rationale prose names first-report-wins, race-ahead, and the non-promoting early return. |
| `sched.executor.repair-precedence` *(new)* | **COVERS** (the winner/loser form) | First classifying observation wins; a non-promoting report MUST NOT consume the entry. |
| `sched.executor.liveness-window` *(new)* | **COVERS** (the TTL value) | 60 s as a normative number. |

#### `EstablishmentOnlyAfterWindowCloses`

*An unreported executor crash is established (charged, classified
`unreported`) only when the correlation window closes with no
classifying report — never earlier, and a non-promoting report does
not establish.*

Sites: #14's sweep (`actor/executor.rs:1168-1290`,
`TERMINATION_REPORT_TTL`), #15's non-promoting gate (`:692-708`).
Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.liveness-window` *(new)* | **COVERS** | "Establishment … MUST fire only when the window closes with no classifying report, never earlier." Was a GAP. |
| `sched.retry.per-executor-budget+2` | PARTIAL | Names the establishment vehicle and that established crashes count toward the threshold; does not state the window discipline. |
| `sched.executor.repair-precedence` *(new)* | **COVERS** (the non-promoting clause) | A non-promoting report neither consumes the entry nor establishes. |

### F5 — Eligibility coherence (Model S)

#### `NeverOfferUnrunnableWork`

*No assignment is pushed to a slot that is draining, degraded, at
capacity, closed-stream, kind/system/feature-mismatched, or in the
drv's exclusion set.*

Sites: the `rejection_reason` chain (`assignment.rs:25-113`),
`has_capacity` (`state/executor.rs:240-249`, incl. the I-095
`is_closed` arm, #11), `statically_eligible` (`assignment.rs:136`),
B6 (builder-side reject). Model S (the clause *content* — the
kind/system/feature arithmetic — is pre-registered NOT-ENCODED; the
model carries the flags).

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.dispatch.fod-to-fetcher` | **COVERS** (kind clause) | FODs only to fetchers, non-FODs only to builders. |
| `sched.assign.resource-fit` | **COVERS** (resource clause) | Solved memory must fit the worker's cgroup limit. |
| `sched.executor.dual-register` | **COVERS** (registered clause) | No dispatch before both registration halves. |
| `sched.ephemeral.no-redispatch-after-completion` / `sched.executor.one-shot` *(new)* | **COVERS** (draining-after-completion clause) | The freed slot is never re-offered. |
| `builder.heartbeat.store-degraded` | **COVERS** (degraded clause) | Degraded executor excluded from assignment. |
| `sched.retry.per-executor-budget+2` | **COVERS** (exclusion-set clause) | `failed_builders` membership excludes; retry-owned. |
| `sched.dispatch.fleet-exhaust+3` | **COVERS** (the statically-eligible fleet definition + draining exclusion) | |
| — | named gap (deliberate) | The closed-stream (I-095) and at-capacity clauses are code-defined (`has_capacity`) with no rule of their own; Model S encodes them as the HalfDead stream state and the running_build slot, and the design adds no new rule for them in Phase 0. Recorded so the omission is explicit. |

#### `EligibleWorkOfferedWithinBound` (bounded safety)

*If a Ready drv has had ≥1 eligible registered slot with free capacity
continuously for STARVE_BOUND dispatch ticks, an offer or an explicit
rejection_reason has been recorded for it.*

Sites: dispatch pacing (`dispatch.rs:113/268/354`,
`actor/mod.rs:1148+`), the became-idle carve-out, the freeze/
unroutable observables. Model S (STARVE_BOUND is a model constant,
not a spec number — recorded as a model-side bound).

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.dispatch.became-idle-immediate` | **COVERS** (the 0→1 edge) | Inline dispatch on capacity appearance, capped. |
| `sched.actor.dispatch-decoupled` | **COVERS** (the trigger discipline) | Heartbeats coalesce to ≤1 dispatch pass per Tick — the deferral is bounded by one tick. |
| `sched.dispatch.unroutable-system+2` | **COVERS** (the no-pool observable) | |
| `sched.dispatch.fleet-exhaust+3` | **COVERS** (empty-fleet defers, never poisons) | |
| `sched.freeze-detector` | **COVERS** (the queue-with-no-stream observable) | |

#### `RollbackRestoresExactly`

*A failed push leaves no half-recorded assignment.*

Sites: #18 (`dispatch.rs:2077+`). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.executor.repair-precedence` *(new)* | **COVERS** | The failed-push class: same-actor-turn rollback of status, rows, pins, `running_build`; no attempt charged. Was a GAP (rollback was code-only). |

### F6 — Failover convergence (Model S)

#### `DeposedLeaderSessionEventsAreInert`

*A deposed leader's session events mutate nothing durable; non-vacuity
(the deposed-believer window stays reachable) is pinned by an
expect-violation witness in the fault-leader regime.*

Sites: #21 (`actor/mod.rs:872/:1190` + per-handler gates),
`reassign_derivations`' leader gate (`actor/executor.rs:565+`), the
gRPC reader fence, B5 (worker-side generation latch). Model S
(fault-leader regime; lease guarantees imported per the 0c checklist,
not re-modeled).

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.lease.standby-drops-writes` | **COVERS** | The PG-write ban, the generation-fenced reader, the gated actor arms (ProcessCompletion, ReportExecutorTermination, ReconcileAssignments, Tick, …) and the individually-gated sub-calls (reassign, drain_phantoms, dispatch). |
| `sched.reconcile.leader-gate` | **COVERS** (the reconcile arm) | The 45 s timer early-returns when not leader. |
| `sched.lease.generation-fence+2` | **COVERS** (the executor-side fence) | Workers reject assignments from older generations. |
| `sched.lease.claim-before-advertise` | **COVERS** (the advertise gate) | No generation advertised before recovery completes. |
| `sched.recovery.gate-dispatch` | **COVERS** (the dispatch gate) | No dispatch from a partially-recovered DAG. |

#### `ConvergenceToGroundTruth` (per-action form)

*Every recovery/reconcile action on a PG-Assigned/Running drv either
adopts it or resets it to Ready; no drv is both adopted and reset; no
attempt is fabricated; after the sweep no such drv remains
unresolved.*

Sites: #19 (`recovery.rs:1779+`, RECONCILE_DELAY `:2153`), #9 (adopt
`actor/executor.rs:1860+`), #8 (TOCTOU keep). Model S.

| Rule | Verdict | Audit finding |
|---|---|---|
| `sched.heartbeat.adopt` | **COVERS** (the adopt half) | Worker-authoritative re-learn into both the slot and the DAG. |
| `sched.executor.repair-precedence` *(new)* | **COVERS** (the arbitration) | Live reconnection + adopt win over the timed sweep; deferral of stream-but-no-heartbeat workers; store probe decides; never fabricate. Was a GAP. |
| `sched.executor.liveness-window` *(new)* | **COVERS** (the 45 s) | |
| `sched.retry.recovery-projection+2` | adjacent (imported) | The ledger/counter projection on recovery is the retry campaign's rule; not re-stated. |

### F7 — Fleet supply coherence (NOT re-modeled)

`spawnCoherence.qnt` (controller campaign, Stage B all-HOLD) owns the
controller half; Model S states the scheduler-side obligations it
imports as guarantees, carried by existing rules — no new rule needed:

| Obligation (imported by Model J/N) | Carried by | Verdict |
|---|---|---|
| `ListExecutors` busy-accuracy + freshness caveat the orphan-reap gate consumes | `sched.admin.list-executors`, `sched.admin.list-executors-leader-age` | **COVERS** |
| Ack / ICE arming on the registration edge (`dispatched_cells` lifecycle, #22) | `sched.sla.hw-class.ice-mask` (scheduler-side ICE state), `ctrl.pool.ack-spawned-soundness` (controller half, that campaign's map) | **COVERS** (split across the two maps) |
| Termination-report idempotency | `sched.completion.idempotent`, `sched.retry.no-double-count`, the `drv_attempts` fill guard (retry campaign) | **COVERS** |
| `dead_nodes` hung-node signal for Model N's health reap | `sched.admin.hung-node-detector+3` | **COVERS** (its successor is OA2) |

The G7 scheduler-side in-family rows (ICE arm/clear semantics) are
calibrated at 0d against these guarantees; the controller half is
cross-referenced to the controller campaign's calibration table, not
re-run.

### F8 — Input hardening (NOT-ENCODED by design)

Bounds and binding gates at the gRPC boundary; no protocol state.
Verification stays with the existing unit tests and bounds checks.

| Rule | Verdict |
|---|---|
| `sched.executor.input-bounds+2` | **COVERS** |
| `sched.completion.output-membership` | **COVERS** |
| `sched.log.phase-binding`, `sched.log.path-length+2` | **COVERS** |
| `sec.executor.identity-token+2` | **COVERS** (also load-bearing for F1) |

## Contradiction records (Stage A)

The code does not do what a rule (or another spec source) says it
MUST. Recorded with their adjudication; production code and existing
rule text are unchanged in Phase 0 (amendments are Phase-1 spec
consequences). **None of the four looks like a live production
defect** — in every row the as-built behavior is the deliberate,
correct one and the spec text is stale, over-broad, or pending an
already-tracked adjudication.

| # | Spec source(s) | What the spec says | What the code does | Adjudication |
|---|---|---|---|---|
| C1 | `sla-sizing.typ` `@alg-pool` ("create pods for placed ∧ eta=0", annotated *bind-ready only*) and its §13b prose ("Builder pods are created only for Ready intents that the FFD sim placed on a Registered node") vs `ctrl.nodeclaim.placeable-gate+5` | Pod/Job creation is gated on the intent being Ready (eta=0) at placement time. | The placeable gate publishes every FFD-placed-on-Registered Builder intent with **no ready filter** (`pool/jobs.rs:353-397`), so forecast (ready=false) intents reach the Job spawner; such a pod registers, has its reservation downgraded, and idles to the I-116 exit if its drv never becomes Ready (the OA6 bookkeeping note in the 0a section). | Spec-vs-spec-vs-code tension already tracked as OA6 (0e-blocking, jointly owned with the controller campaign). Recorded here regardless of which OA6 option 0e takes; neither source is amended in Phase 0. Cost today is forecast-pod churn (idle exits), not a correctness defect. |
| C2 | `sched.executor.deregister-reassign` | "An executor is removed from the scheduler's state when: the `BuildExecution` stream is closed …" — unqualified. | A stale-epoch stream close (the I-056a late-disconnect half) removes nothing: `handle_executor_disconnected` drops events whose epoch differs from the entry's current `stream_epoch` (`actor/executor.rs:363-371`). | Code is correct (removing on the stale close evicts a freshly-reconnected worker). The epoch qualification is now normative in `sched.executor.session-epoch`, which composes with this rule by cross-reference; amending the deregister rule's own sentence is a Phase-1 consequence, deliberately not done now. |
| C3 | `controller.typ` Executor Lifecycle prose (the SIGTERM-drain step list: "Send `AdminService.DrainExecutor` (best-effort exit deregister) and exit 0") vs `builder.ephemeral.exit-aborts-heartbeat+2` | The builder calls `DrainExecutor` on its way out. | The builder does NOT call `DrainExecutor` — the service-token gate allowlists controller and rio-cli only, and the builder is intentionally excluded (the rule states this; the as-built deregistration is stream-close → `ExecutorDisconnected`). | Two spec sources contradict; the builder rule + code are authoritative, the controller-side prose is stale (it predates the service-token gating). Prose amendment deferred (it is narrative text outside any rule body); recorded so the staleness is on the record. |
| C4 | `sla-sizing.typ` `@alg-pool` `dead_nodes` annotation ("≥ max(3, ⌈0.5·occupancy⌉) executors … reset when all live executors on n heartbeat") vs `sched.admin.hung-node-detector+3` | Hung-node floor of 3 stale executors; signal reset when the node's executors heartbeat again. | Floor is `max(2, ⌈0.5·occupancy⌉)` (the rule's deliberate `2`-floor recalibration for busy-only occupancy) and the repeat signal is retained by TTL only (`HUNG_NODE_REPEAT_TTL`, mb_001a) — recovered nodes age out rather than being reset by heartbeats. | The rule (+3) and the code agree; the `@alg-pool` annotation is stale on both counts. Spec-vs-spec, no behavior question; amendment of the sla-sizing annotation deferred to its next substantive edit (Phase 1 at the latest). |

Near-misses recorded for a later prose pass, not classified as
contradictions of a MUST:

- `sched.executor.dual-register`'s mechanism description: the entry is
  created at stream-connect (`handle_worker_connected`), not at the
  first heartbeat, and `executor_id` is the pod *name* (downward API),
  not "derived from pod UID". The normative content (dispatchable only
  after both halves) is unaffected.
- `sched.executor.deregister-reassign`'s "all derivations in
  `assigned` state … transitioned back to `ready`": the as-built
  reassign also covers Running, re-checks the poison threshold, and
  may poison instead of requeue (the retry fold's verdict); the
  sentence under-describes rather than contradicts.
- `ctrl.pool.ephemeral+1`'s ClusterStatus-polling sentence — already
  recorded as a near-miss in `controller-invariant-map.md`; not
  repeated here.

## Expected as-built falsifications (pre-registered): none

The design (§3.1) expects no §3.3 invariant to falsify against the
as-built code at the §3.2 bounds — the known defect classes in this
subsystem were fixed as they were found (the 50-commit in-family
corpus is the evidence), and the four contradictions above are
spec-text findings whose adjudication keeps the code as-is. The list
is therefore **empty**, which makes any Stage-B (0c) falsification a
stop-and-report by definition: work pauses, the counterexample is
written up (the `phase2-falsification-*` format), and the campaign
owner adjudicates "model encoding bug" vs "real as-built defect"
before Stage B resumes. A real defect found that way is recorded here
as a known-defect row and handed to the normal fix process — never
fixed inside Phase 0 and never modeled around. (Stage B produced
exactly one such falsification — `unresolvedClaimHasRepairArmed` in
the fault-leader / fault-persist regimes — adjudicated a real defect
and fixed; the known-defect row and the verdict flip are in the
Stage-B record's adjudication section below.)

## Witness pre-registration (the §3.5 non-vacuity obligations)

Every contended precondition gets an expect-violation witness at 0c;
a witness that stops violating after any later bound change is a red
check by construction. Pre-registered now so 0c wires exactly this
list:

Model S (`executorSession.qnt`):

1. a phantom is constructible (assignment accepted, report never
   arrives, two heartbeat misses);
2. a half-dead stream is reachable (stream open, sends fail);
3. a stale-epoch disconnect arrives after a reconnect;
4. an adopt happens (worker reports a build the scheduler lost);
5. a reap fires only after stall credit (scheduler stall alone never
   reaps);
6. one pod death is observed by ≥2 channels;
7. a failover occurs with an in-flight build;
8. a drain coexists with a pending completion;
9. the deposed-believer window is reachable (F6 non-vacuity, pinned in
   the fault-leader regime).

Model D (`executorDelivery.qnt`):

10. a relay swap happens while a report is owed;
11. an in-flight cell is dropped by a stream that fails to confirm;
12. the half-close flush path is reachable;
13. exit is blocked while the sink is non-empty.

## Corpus partition counts (T-0b.4 fold)

The per-bucket and per-family counts pre-registered at 0a (50
in-family / 21 retry-owned / 43 controller-owned / 56 out-of-scope of
170, and the per-family G1–G8 split) stand unchanged as the Stage-A
record and remain the 0d denominators; see "Stage-C corpus pin: the
calibration denominators" below. No re-partitioning was needed during
the audit.

## Rules in the neighborhood not load-bearing for any invariant above

Grouped, with the reason they stay outside the Stage-B models:

- **Placement preference / SLA content** (abstracted to opaque
  eligibility in Model S; the static-eligibility *content* is
  pre-registered NOT-ENCODED): `sched.assign.warm-gate`,
  `sched.sla.intent-match`, `sched.dispatch.fod-builtin-any-arch`,
  `sched.dispatch.soft-features`, `sched.dispatch.fod-substitute+2`,
  `sched.dispatch.substitute-complete-inline`.
- **Cancel / preemption delivery** (best-effort optimization today —
  correctness never depends on `CancelSignal` delivery; becomes
  AD5's subject only in the replacement): `builder.cancel.cgroup-kill`,
  `builder.cancel.pre-cgroup-deferred`, `ctrl.pool.disruption`,
  `ctrl.drain.disruption-target`, `ctrl.drain.sigterm`,
  `ctrl.pod.tgps-default`.
- **Exit mechanics that are not protocol state**:
  `builder.shutdown.sigint+2`, `builder.shutdown.fuse-abort`,
  `builder.ephemeral.exit-aborts-heartbeat+2` (zombie hygiene; the
  protocol-visible half is already carried by F1's entry-existence
  rules), `builder.timeout.no-reassign` (retry-classification content,
  retry campaign).
- **Admin/observability surfaces**: `sched.admin.debug-list-executors`,
  `sched.admin.snapshot-cached`, `sched.admin.list-builds`,
  `sched.admin.clear-poison`, `sched.admin.list-poisoned`,
  `sched.admin.spawn-intents` (supply-side bookkeeping; Model J's
  input, not a session invariant), `sched.backstop.orphan-watcher`
  (gateway-client orphan, different protocol).
- **Controller fleet rules** (`ctrl.pool.*`, `ctrl.ephemeral.*`,
  `ctrl.terminated.*`): owned by the controller campaign's map; only
  the graces named in `sched.executor.liveness-window` and the F7
  obligations table touch them here.
- **Lease machinery beyond the five rules cited under F6**
  (`sched.lease.{k8s-lease,at-most-one-leader,self-fence,
  generation-claim,graceful-release,rebound,deletion-cost,
  non-blocking-acquire,standby-tick-noop,hook-order}`,
  `sched.recovery.{fetch-max-seed,bump-confirm,…}`): the rio-lease
  campaign's subject; Model S imports the lease guarantees through the
  0c assume-guarantee checklist instead of mapping them.

## Verify-marker status (Stage-A snapshot)

The five new rules carry `r[impl]` markers at the decision sites named
above (23 markers: 4 session-epoch, 7 liveness-window, 6
repair-precedence, 3 one-shot, 3 exactly-once-or-death) and **no
`r[verify]` markers**: their verification is deliberately deferred to
the Stage-B models, so they appear in `tracey query untested` until 0c
wires the checks — the marker-first signal working as intended, not a
debt to silence.

Planned verification (recorded now, wired at 0c — markers go at the
`nix/quint.nix` wiring points, never in `.qnt` files or scenario
headers; VM-test markers stay at the `nix/tests/default.nix` subtests
entries):

| Rule | Planned check |
|---|---|
| `sched.executor.session-epoch` | `quint-executor-session-*` (F1 invariants; fault-stream regime) |
| `sched.executor.liveness-window` | `quint-executor-session-*` (F3/F4 invariants; the window composition) — the numeric values themselves stay code-reviewed constants |
| `sched.executor.repair-precedence` | `quint-executor-session-*` (F2/F4/F6 invariants: at-most-once, armed-safety, convergence) |
| `sched.executor.one-shot` | `quint-executor-session-*` (the one-shot flag semantics in Model S) plus the existing `ephemeral-pool` VM scenario already covering the I-188 behavior (no marker moved in 0b) |
| `builder.completion.exactly-once-or-death` | `quint-executor-delivery-*` (Model D: `ReportSurvivesStreamChurn`, `NoExitWithReportOwed`) |

Existing rules mapped above keep their existing verification
unchanged; nothing was re-pointed and no rule text was amended, so
`tracey query stale` has nothing to show for this audit.

## Phase 0a — churn pin and re-pin protocol

Pin date: 2026-05-26. Base: the `formal-sprint` lineage at
`277618342` ("test(rio-retry-kernel): bound the classify harness and
record the post-extraction CBMC findings"). This tip is **after** the
retry campaign's Phase 1b–2 and close-out (the durable attempt
ledger, the establishment vehicle, the legacy counter retirement),
after the third harden-subst rebase, after the in-scheduler build-log
deletion / LogService cutover (`f1c758bb5`, `73b727732`), and after
the controller campaign's Stage C — i.e. all the churn the design
flagged as having moved past the inventory snapshot is included in
the pin.

### What is pinned

The in-scope file set (the churn set; the corpus query below uses the
narrower nine-path set):

| In-scope path | Last commit touching it (at the pin) | Last `fix(…)` commit |
|---|---|---|
| `rio-scheduler/src/state/executor.rs` | `001cf0eeb` 2026-05-26 (retry Stage-A self-review corrections) | `001cf0eeb` 2026-05-26 |
| `rio-scheduler/src/actor/executor.rs` | `bcfa87ef8` 2026-05-26 (legacy retry counter retirement) | `7d5646105` 2026-05-26 |
| `rio-scheduler/src/actor/housekeeping.rs` | `bcfa87ef8` 2026-05-26 | `125feb450` 2026-05-26 |
| `rio-scheduler/src/grpc/executor_service.rs` | `f1c758bb5` 2026-05-26 (build-log subsystem deletion) | `8f6190df7` 2026-05-26 |
| `rio-scheduler/src/assignment.rs` | `cde21963a` 2026-05-26 (fleet-exhaust onto placeable()) | `001cf0eeb` 2026-05-26 |
| `rio-builder/src/runtime/` | `73b727732` 2026-05-26 (LogService cutover) | `0ea9bd701` 2026-05-26 |
| `rio-builder/src/main.rs` | `fa3bc53d5` 2026-04-08 | `96056b318` 2026-04-07 |
| `rio-builder/src/health.rs` | `fdbe38517` 2026-04-08 | none (no fix commit in its history) |
| `rio-controller/src/reconcilers/pool/` | `d7cce02ae` 2026-05-26 (controller Stage-A markers) | `be2f50e9e` 2026-05-21 (docs-only); last behavior fix `f97644a53` 2026-05-11 |

Shared actor files in scope for this churn table and the
repair-mechanism audit only (NOT part of the corpus query — see the
corpus pin):

| Shared path (session-relevant slices) | Last commit | Last `fix(…)` |
|---|---|---|
| `rio-scheduler/src/actor/mod.rs` | `66e73569a` 2026-05-26 | `4f12a7ffa` 2026-05-26 |
| `rio-scheduler/src/actor/dispatch.rs` | `ea3b1c078` 2026-05-26 | `44d4235b8` 2026-05-26 |
| `rio-scheduler/src/actor/completion.rs` | `ea3b1c078` 2026-05-26 | `7d5646105` 2026-05-26 |
| `rio-scheduler/src/actor/recovery.rs` | `f512516f9` 2026-05-26 | `44d4235b8` 2026-05-26 |
| `rio-scheduler/src/actor/snapshot.rs` | `7f7a19b8a` 2026-05-23 | `421d674b5` 2026-05-11 |

Peer files whose interfaces the models will treat as environment
(same role as the controller map's peer table):

| Peer path | Why it is a peer | Last commit at the pin |
|---|---|---|
| `rio-scheduler/src/retry_policy.rs` + `rio-retry-kernel/` | the fold/decide()/placeable() kernels Model S imports as guarantees (campaign #4, closed) | `277618342` 2026-05-26 |
| `rio-scheduler/src/db/attempts.rs` | the durable attempt ledger (two-installment rows, `fill_termination`) the termination/establishment path now lands on | `bcfa87ef8` 2026-05-26 |
| `rio-scheduler/src/lease_hooks.rs`, `rio-lease/` | leader environment (imported abstract action; hook ordering) | `125feb450` 2026-05-26 |
| `rio-scheduler/src/sla/cost.rs` | ICE backoff ladder (controller-campaign peer state) | `8026d5f2b` 2026-05-15 |
| `rio-controller/src/reconcilers/nodeclaim_pool/` | Model N's subject (controller campaign); consumes the `dead_nodes` signal this subsystem produces | `782b6155b` 2026-05-24 |
| `rio-proto/proto/{builder,build_types,admin}.proto` | the wire surface (§1.1) | `17856466d` 2026-05-26 |

### Design-hash → on-lineage cross-walk

Every design-named hash (the §3.5 / T-0d.2 representatives and the
cross-campaign exemplars) was resolved against the pin. All are
HEAD-reachable identity rows except the two below (subject-line
match, same content):

| Design / inventory hash | On-lineage hash at the pin | Subject |
|---|---|---|
| `5c47af5ad` | `0ea9bd701` | fix(rio-scheduler): advertise only the post-recovery generation in heartbeat replies |
| `f1902fe63` (inventory G6 "lease hooks in order") | `125feb450` | fix(rio-scheduler/rio-lease): deliver lease hooks to the actor in invocation order |

Identity rows (verified ancestors of the pin, no substitution
needed): `db457374f`, `a6697c6b0`, `0127cf854`, `be3ad068e`,
`6b6cfcf10`, `8201db59b`, `1353d3224`, `29222884e`, `5971778f8`,
`1757790f2`, `96d8092b8`, `a62631c90`, `20afe5154`, `c5c5ccd17`,
`ee9302b86`, `8283d4362`, `172776b1b`, `c13f6a277`, `8d38cb999`,
`e872b2b49`, `dc094dd0c`, `6a9ba0ef0`, `7f04c9d88`, `fba9086dc`,
`9123e72d4`, `4f8f68ff8`, `3082598a3`, `451f2dc80`, `9a2dbc873`,
`83e0b338f`, `ea10e1d74`. The T-0d.2 representative table and the
partition below are written against on-lineage hashes from the
start; T-0d.1 only re-checks for drift after 0a.

### Inventory re-pin: anchor re-verification

The inventory cites `e650f23a4`, which is off-lineage after the third
harden-subst rebase (merge base `d79d633685`); 302 commits separate
it from the pin (cross-lineage superset), 29 of them touching the
churn set above. The scheduler-side files absorbed the retry
campaign's Phase 1b–2 and the build-log deletion since the snapshot,
so the §2/§3 anchors were re-verified one by one rather than carried
over. Verdict: **every §2 state-inventory anchor and §3.1/3.2/3.3
mechanism anchor re-confirms at the pin** — no mechanism was removed
or absorbed — with the line shifts and the four content deltas
recorded below. Inventory references should be read against this
section from here on.

Content deltas (mechanism behavior that moved since the snapshot —
the design anticipated all four):

1. **`recently_disconnected` carries the released execution
   identity.** The map is now `HashMap<ExecutorId,
   DisconnectedAttempt>` (`actor/mod.rs:292-300`, map at `:364`),
   carrying `drv_hash`, `derivation_id`, the released `exec_id`, and
   the observation instant — not the snapshot's `(DrvHash, Instant)`
   pair. Inventory §2.2's purpose/mutator/failover columns otherwise
   stand (insert on mid-build disconnect, first-classifier-wins
   consumption, cleared on leader transition).
2. **The 60 s TTL sweep is now the establishment vehicle.** The sweep
   (`actor/executor.rs:1143-1290`, TTL constant
   `TERMINATION_REPORT_TTL` = 60 s at `:31`) no longer just drops
   expired entries: an entry whose classifying report never arrived
   is established as a durable `executor_crash`/unreported attempt
   fill on the released `(derivation_id, exec_id)` row and charged
   through `decide()` (retry campaign T-1b.11). Mechanism #14's
   detection role is unchanged; its repair action gained a durable
   write.
3. **The termination/establishment path lands on durable attempt
   rows.** `handle_executor_termination` (`actor/executor.rs:676`)
   and the disconnect path append/fill `drv_attempts` rows (migration
   066; `db/attempts.rs::fill_termination` at `:386`, idempotent via
   `WHERE termination_reason IS NULL`); the first-report-wins dedup
   of mechanism #15 is now also a schema property
   (`drv_attempts_exec_id_uniq`). The in-memory dedup
   (`recently_disconnected.remove()`, race-ahead `last_completed`)
   still exists and is what Model S owns (F4's lifecycle).
4. **The legacy retry counter mutations are retired and the
   dispatch-time fleet-exhaust verdict is `placeable()`.**
   (`bcfa87ef8`, `cde21963a`.) The backstop (#17), disconnect (#3),
   and termination (#15/#16) charging paths now flow through the
   retry kernels; `dispatch_fleet_exhausted`
   (`actor/dispatch.rs:1789`) consults the kernel. This does not
   change which repair mechanisms exist; it changes what they write.

Anchor re-verification (new locations at the pin; "≈" means the
function/region begins at the cited line):

§2 state inventory:

| Inventory anchor | At the pin |
|---|---|
| §2.1 `ExecutorState`, 18 fields (`state/executor.rs:28-186`) | re-confirmed — struct at `state/executor.rs:28`, same 18 fields; `new()` ≈ `:192`, `is_registered` `:229`, `has_capacity` `:240` (incl. the I-095 `is_closed` check), `is_draining` `:255` |
| §2.2 actor maps (`actor/mod.rs`) | `executors` `:315`; `hung_nodes` `:327`; `authoritative_binding` `:353`; `recently_disconnected` `:364` (type delta 1 above); `dispatched_cells` `:456`; pacing fields `dispatch_dirty`/`probe_generation`/`became_idle_inline_this_tick` `:642-662` |
| §2.3 DAG-side per-drv session fields | re-confirmed; `transition_to_assigned` ≈ `dispatch.rs:1921`, exec_id mint inside `assign_to_worker` ≈ `:1851`, rollback ≈ `:2077` |
| §2.4 builder runtime state | re-confirmed; `BuilderRuntime` fields ≈ `runtime/mod.rs:779-805` (`relay_target_tx`, `completion_pending`, `latest_generation`, `idle_timeout`), `BuildSlot` `runtime/slot.rs` (`try_claim` `:63`, cgroup path `:28-32`) |
| §2.5 controller cross-tick state | re-confirmed (per-tick recomputation; Jobs/pods + annotations are the durable state) |
| §2.6 durable PG state | re-confirmed for `assignments` / `drv_executions` / status mirror; **new**: `drv_attempts` (066) — the scheduler-owned attempt ledger (delta 3); the executor fleet still has no PG table |
| §2.7 what failover loses | re-confirmed — `clear_persisted_state` ≈ `actor/mod.rs:872`; clears `recently_disconnected`, `dispatched_cells`, `hung_nodes`, `authoritative_binding`; retains `executors` (the `executors: _` bind); standby drops `ProcessCompletion` (`actor/mod.rs:1190`) |

§3.1 scheduler-side mechanisms (1–22):

| # | At the pin |
|---|---|
| 1 heartbeat-timeout reaper | ≈ `housekeeping.rs:291-312` (`HEARTBEAT_TIMEOUT_SECS`) |
| 2 stall credit | `credit_heartbeats_for_stall` `housekeeping.rs:240` (call sites unchanged) |
| 3 stream-close → disconnect → reassign | `executor_service.rs:507-516`, `actor/executor.rs:347`, `reassign_derivations` ≈ `:565` |
| 4 stream-epoch stale-disconnect filter | `actor/executor.rs:363-371` (unchanged) |
| 5 reconnect stale-flag clear | inside `handle_worker_connected` ≈ `:116-247` (region unchanged) |
| 6 reconnect hijack + intent-mismatch rejection | `actor/executor.rs:148-176`; accept-gate `executor_service.rs:226-239` |
| 7 unknown-executor heartbeat drop | inside `handle_heartbeat` ≈ `:1444` (I-048b arm) |
| 8 heartbeat running-build TOCTOU keep | inside `handle_heartbeat` / `reconcile_running_build` ≈ `:1680` |
| 9 heartbeat adopt | `adopt_heartbeat_build` ≈ `:1860` |
| 10 phantom two-strike + drain_phantoms | `drain_phantoms` ≈ `:1803`; suspect marking in `reconcile_running_build` |
| 11 closed-stream exclusion + WARN | `assignment.rs:53`, `state/executor.rs:240-249` |
| 12 completion capacity-free hoist + stale-report guard | `completion.rs:898` (handler), hoist + `last_completed` `:1001-1007`, stale guard `:1096-1106` |
| 13 one-shot draining-on-completion + last_completed | `completion.rs:1009-1019` (I-188/I-197 comments intact) |
| 14 `recently_disconnected` correlation map + TTL sweep | map `actor/mod.rs:364`; sweep ≈ `actor/executor.rs:1143-1290` (deltas 1–2) |
| 15 termination-report dedup | `handle_executor_termination` ≈ `:676`; `fill_termination` `db/attempts.rs:386` (delta 3) |
| 16 DeadlineExceeded job-name prefix-match | second half of `handle_executor_termination` (info line "DeadlineExceeded backstop fired" ≈ `:1132`) |
| 17 backstop timeout | `tick_scan_dag` ≈ `housekeeping.rs:314` + `tick_process_backstop_timeouts` (charging now via decide(), delta 4) |
| 18 dispatch rollback | `rollback_assignment` ≈ `dispatch.rs:2077` |
| 19 post-failover reconcile (45 s) | `handle_reconcile_assignments` ≈ `recovery.rs:1779`; `RECONCILE_DELAY` = 45 s `recovery.rs:2153` |
| 20 hung-node detector | `snapshot.rs::detect_hung_nodes` `:51`; `tick_hung_nodes` `housekeeping.rs:276-288` |
| 21 leader-transition hygiene | `clear_persisted_state` ≈ `actor/mod.rs:872`; leader gates at command dispatch (`actor/mod.rs:1190` and the per-handler gates) |
| 22 dispatched_cells sweep + ICE heartbeat-edge clear | `tick_sweep_dispatched_cells` `housekeeping.rs:741`; registration-edge clear in `handle_heartbeat` |

§3.2 builder-side (B1–B9): B1 `'reconnect` loop ≈ `runtime/mod.rs:814/:840`;
B2 swap-after-Ok `:907-912`; B3 graceful half-close ≈ `:1054-1061`;
B4 drain gate `runtime/drain.rs:37-120` + `completion_pending`
set-before-sink `runtime/result.rs:227-244`; B5 generation fence
`runtime/mod.rs:75` + heartbeat `fetch_max` `runtime/heartbeat.rs:207`;
B6 slot-busy / draining rejection in `handle_assignment` ≈
`runtime/mod.rs:1186`; B7 idle-timeout exit ≈ `:985`; B8 heartbeat RPC
timeout `runtime/heartbeat.rs:24-36`; B9 panic-catcher completion
`runtime/result.rs:132` + heartbeat-task liveness `runtime/mod.rs:915`
+ teardown abort ≈ `:1133-1139`. All re-confirmed.

§3.3 controller-side (C1–C6): C1 `reap_stale_for_intents`
`pool/jobs.rs:785`; C2 `reap_excess_pending` `pool/job.rs:373`
(`REAP_PENDING_GRACE` = 10 s `:270`, `is_pending_job` `:223`); C3
`select_orphan_running`/orphan reap `pool/job.rs:496`
(`ORPHAN_REAP_GRACE` = 300 s `:256`, leader-age fail-closed arm ≈
`:539-562`); C4 `report_terminated_pods` ≈ `pool/job.rs:990-1050`; C5
`report_deadline_exceeded_jobs` ≈ `:1090-1120`; C6
`pool/disruption.rs` (whole file). K8s-native backstops unchanged
(`JOB_TTL_SECS` = 600 `pool/job.rs:50`, `JOB_REQUEUE` = 10 s `:40`,
`activeDeadlineSeconds` floor in `pool/jobs.rs`). All re-confirmed.

§4 decision-inventory anchors follow the same shifts as their host
mechanisms above (the predicate chain `rejection_reason`
`assignment.rs:25`, `statically_eligible` `:136`, `best_executor`
`:175`, dispatch pacing `dispatch.rs:113/268/354`); no decision was
added or removed by the churn except the fleet-exhaust collapse
(delta 4), which replaces the open-coded exhaust check with the
kernel's `placeable()` — same decision, one implementation.

### OA6 bookkeeping (design §5.2, recorded at the 0a re-pin)

Inventory §1.9 and §4.4 omit the §13b placeable-gate's
no-ready-filter behavior: the gate (`pool/jobs.rs:353-397`,
`ctrl.nodeclaim.placeable-gate+5`) publishes every FFD-placed
Builder intent with **no ready filter**, so forecast (ready=false)
intents reach the Job spawner and a pod's first pull under the
replacement can land before its drv is Ready. The neighbouring
comment "`queued` counts only Ready intents" (`pool/jobs.rs:458`) is
stale in exactly this sense. Recorded here so the OA6 adjudication
(0e) and the 0b contradiction row (sla-sizing.typ `@alg-pool` vs the
gate) start from the corrected description, not the inventory's.

### Repair-mechanism completeness audit

Audit inputs: `git log e650f23a4..277618342` over the churn set (29
commits) plus a grep of the in-scope production files for incident
markers (`I-[0-9]{3}[a-z]?`, `bug_[0-9]{3}`), diffed against the §3
tables. Result: **no repair mechanism is missing from the §3
tables.** The 29 commits are: the retry campaign's Phase 1b–2 ledger
work and close-out fixes (deltas 1–4 above — they extend mechanisms
#3/#14/#15/#17, none new), the build-log deletion / LogService
cutover (removes scheduler-side log relay code; no session mechanism
touched), the lease hook-ordering fix (`125feb450`, ordering
infrastructure for #21), the controller Stage-A marker commit and
fetcher-budget split (no mechanism change), and doc/comment sweeps.
Incident markers present in the in-scope files all map to mechanisms
already in the table; no marker names a repair behavior outside it.
The table is therefore NOT extended and no new F-family is needed at
0b beyond the design's F1–F8.

### Cross-campaign sequencing and standing directives

- **Retry campaign (#4): closed.** `retry-invariant-map.md` carries
  the campaign close-out ("Campaign close-out — retry/poison/cascade
  (campaign #4)"). The hand-off items this campaign now owns (from
  its "What transfers to the executor-lifecycle campaign (#1)"
  list): the stream-epoch / heartbeat-binding halves of `db457374f`
  and the late-disconnect-vs-reconnect race; heterogeneous static
  eligibility (`a62631c90` — the eligibility computation feeding
  `placeable()`); the correlation/dedup-state lifecycle
  (`recently_disconnected` with the released `exec_id`,
  `last_completed`, the establishment TTL sweep); the
  `ExecRec`/slot identity-freshness encodings in `retryPolicy.qnt`
  as a starting point for Model S's slot state; and the
  file-size-expectation lesson for any durability-adding collapse.
- **Controller campaign: Phase 0 complete, Phase 1 not started.**
  Verified at the pin: `controller-invariant-map.md` carries its
  Stage-C corpus pin (95 commits at `746164c4f`), the calibration
  table, the Stage-C run record, six wired `quint-ctrl-calib-*`
  witnesses, and a "Phase-0 exit-gate verdict: Met" — and no Phase-1
  record or close-out. The design's 0a ordering decision is
  therefore recorded as: **controller Stage C closed — option (a)'s
  ordering precondition (Stage C before this campaign's 1b touches
  `pool/{jobs,job}.rs`) is met.** This is NOT a discharge of that
  map's own constraints; the named-owner-and-ordering entry with the
  start/green-light obligations lives in
  `controller-invariant-map.md`'s in-flight-work section (added in
  this same commit set), and its obligations — the affected-section
  re-audit (J11, the orphan-reap rows, F1/F3, the I12 out-of-model
  entry) at this campaign's 1b/1d, the F1/F3 prerequisite review at
  0e if that campaign is still mid-campaign, and the Stage-C
  calibration-table delta pass — are carried into this campaign's 0e
  and Phase-1 gates (T-0e.6, T-0e.8). G7 cross-references at 0d are
  unblocked (the controller table exists).
- **Standing rebase directive (harden-subst follow-ups).** The
  formal-sprint lineage is periodically rebased onto harden-subst
  follow-ups; the third such rebase already rewrote one
  representative hash (`5c47af5ad` → `0ea9bd701`). A rebase that
  rewrites the pinned base or any hash named in this map triggers
  the re-pin protocol below — re-locate by subject, record old→new
  rows, re-run the corpus query — never a silent re-anchor.

### Re-pin protocol

Run immediately before Stage B (0c) starts and again immediately
before Stage C (0d) freezes the calibration corpus:

```
git log --oneline 277618342..HEAD -- \
  rio-scheduler/src/state/executor.rs rio-scheduler/src/actor/executor.rs \
  rio-scheduler/src/actor/housekeeping.rs rio-scheduler/src/grpc/executor_service.rs \
  rio-scheduler/src/assignment.rs rio-builder/src/runtime rio-builder/src/main.rs \
  rio-builder/src/health.rs rio-controller/src/reconcilers/pool
```

Stage A/B artifacts MUST be re-validated (re-run the affected slice
of this audit, update the affected rows, re-pin this section) if any
of:

1. any commit in that range changes an in-scope file beyond
   comments, doc-comments, or tracey markers;
2. the formal-sprint lineage is rebased so that the pinned base or
   any hash recorded in this map is rewritten (the standing
   directive above);
3. a behavior-relevant change lands on a peer file that an
   assume-guarantee checklist names (retry kernels, lease hooks,
   nodeclaim_pool's dead-nodes consumer);
4. the corpus query below returns new `fix(` commits at the 0d
   freeze (those are bucketed the same way and the delta recorded —
   the 0d gate then audits against the updated counts).

Changes that do NOT trigger re-validation: comment-only or
marker-only commits, test-only commits, spec prose outside the rules
this campaign adds at 0b, and peer-file changes outside the named
checklists (they move the peer table at the next re-pin but not the
audit).

## Stage-C corpus pin: the calibration denominators (pre-registered at 0a)

Pinned 2026-05-26 at `277618342`, before any model or override
exists, per design §3.5 ("partitioned commit-by-commit, with counts,
before any reverting"). These counts are the denominators the 0d
gate is audited against.

### The corpus query

The corpus file set is exactly inventory §5's nine paths — scheduler
`state/executor.rs`, `actor/executor.rs`, `actor/housekeeping.rs`,
`grpc/executor_service.rs`, `assignment.rs`; builder `runtime/`,
`main.rs`, `health.rs`; controller `reconcilers/pool/` — and the
query is, verbatim (one row per distinct commit; a multi-file commit
gets one row):

```
git log --format='%h %s' 277618342 -- \
  rio-scheduler/src/state/executor.rs rio-scheduler/src/actor/executor.rs \
  rio-scheduler/src/actor/housekeeping.rs rio-scheduler/src/grpc/executor_service.rs \
  rio-scheduler/src/assignment.rs rio-builder/src/runtime rio-builder/src/main.rs \
  rio-builder/src/health.rs rio-controller/src/reconcilers/pool \
  | awk '$2 ~ /^fix[(:]/'
```

Query-shape verification: the same query evaluated at the
inventory's snapshot commit (`git log e650f23a4 -- …`) reproduces
the inventory §5 headline figures exactly (334 commits touching the
set, 168 `fix`, 64 `feat`), so the denominators below are continuous
with the figures the design's §3.5 partition was sized against. Two
recorded properties of this query shape, kept deliberately:

- **No `--follow`.** The five shared actor files are excluded by
  design (below), and pre-rename history (`rio-worker/`,
  `reconcilers/{worker,builder,fetcher}pool/`, `worker.rs`-era
  scheduler files, all before 2026-04-06) is NOT part of the corpus
  — the inventory's 334/168 basis did not include it either (the
  `--follow` phrase in inventory §5's method line applied to its
  per-file deep-dives, not the headline count, as the reproduction
  above shows). Widening to the renamed-path union would add ~130
  worker-era fix commits that the design's denominators were never
  built on; if a later stage wants that history it is a recorded
  corpus change, not a silent widening.
- **The five shared actor files** (`actor/{mod,dispatch,completion,
  recovery,snapshot}.rs`) are NOT in the corpus query —
  "session-relevant slices" is not a git-expressible filter, and
  including the whole files would roughly double the corpus with
  retry-/log-/SLA-owned fixes. Lifecycle-owned fixes that live only
  in those files (the establishment-window and dedup-lifecycle
  halves the retry close-out hands over) enter the 0d calibration
  table as the explicitly-listed hand-off rows (T-0d.4), never by
  widening the query.

### The denominator

At the pin the query returns **170 fix commits** (the design 0a
row's "re-pinned fix count"; the snapshot's was 168). Every one of
the 170 is assigned exactly one bucket below; the bucket counts sum
to 170.

| Bucket | Count |
|---|---|
| in-family (owned by this campaign, G1–G8) | **50** |
| cross-campaign-owned — retry table / retry campaign artifacts | **21** |
| cross-campaign-owned — controller Stage-C table | **43** |
| out-of-scope (adjacent subsystems on shared files) | **56** |
| **Total** | **170** |

Per-family in-family counts (the 0d denominators per family):

| Family | n | Notes |
|---|---|---|
| G1 session identity (→ F1) | 8 | |
| G2 outcome delivery (→ F2, scheduler + builder halves) | 11 | |
| G3 liveness calibration (→ F3) | 12 | 6 per-executor + 6 hung-node/node-aggregation sub-family |
| G4 death attribution (→ F4) | 0 | no standalone in-family commit: the charging halves are all retry-owned (cross-campaign rows below); F4's in-family content enters 0d as the hand-off halves (the `db457374f` heartbeat-binding/stream-epoch halves counted under G1, the dedup-entry lifecycle, the late-disconnect-vs-reconnect race) per T-0d.4 |
| G5 eligibility coherence (→ F5) | 7 | |
| G6 failover convergence (→ F6) | 3 | |
| G7 fleet-supply scheduler-side obligations (→ F7) | 3 | the controller half of G7 is cross-campaign (43 rows) |
| G8 input hardening (→ F8) | 6 | |
| **In-family total** | **50** | |

### In-family rows (50)

| Hash | Family | Note |
|---|---|---|
| `db457374f` | G1 | design F1 representative (stream_epoch atomicity + heartbeat auth_intent binding); split row: the deadline/backstop accounting halves are retry-table G1 rows (cross-referenced at 0d) |
| `a6697c6b0` | G1 | design F1 representative (reconnect hijack accept-gate, ExecutorClaims kind binding); split: also a controller G-F row (auth/identity halves) and log-relay halves |
| `ea10e1d74` | G1 | identity chain (ExecutorClaims HMAC) + per-stream caps; split: also a controller G-F row; build-events-bridge half is log-relay |
| `4f8f68ff8` | G1 | adopt-conflict + stream_epoch (I-056 family) |
| `3082598a3` | G1 | clear draining/degraded on reconnect (I-056a) |
| `451f2dc80` | G1 | I-048 zombie guards |
| `9a2dbc873` | G1 | skip re-register on SIGTERM-reconnect when slot idle |
| `83e0b338f` | G1 | dup-Register handling; split: LogBuffers bound + step_down halves are log/lease content |
| `0127cf854` | G2 | design F2 representative (phantom two-strike drain) |
| `be3ad068e` | G2 | design F2 representative (heartbeat adopt of reconnecting worker's build) |
| `6b6cfcf10` | G2 | design F2(D) representative (relay swap-after-Ok) |
| `8201db59b` | G2 | design F2(D) representative (completion_pending before first await + graceful half-close) |
| `1353d3224` | G2 | design F2(D) representative (drain gated on completion-delivered) |
| `29222884e` | G2 | design F2(D) representative (relay watches target change) |
| `aaa08721d` | G2 | decouple dispatch from heartbeat (lost-assignment prevention); split: FUSE-warm bound + build_id sanitize halves |
| `41bc8dd97` | G2 | unsolicited-Cancelled completion left drv stuck after slot freed (C3 half); split: batch-persist/event-emission halves out of model |
| `cc1ca02a7` | G2 | BuildSlot state under one mutex (single-build occupancy coherence); split: upload-scan half store-owned |
| `d653222cf` | G2 | early graceful-drain (builder half); split: gateway drain + FUSE xattr halves |
| `e4ed7b6a9` | G2 | builder DrainExecutor retry on not-leader (exit choreography; mechanism since deleted by `fb3ea232d`) |
| `5971778f8` | G3 | design F3 representative (reap at 30 s + handle_tick leader gate) |
| `1757790f2` | G3 | design F3 representative (stall credit at all 8 FMP sites) |
| `44a55a224` | G3 | stall-credit early-return removal |
| `e7b8ee91a` | G3 | heartbeat RPC timeout < interval (bug_044) |
| `d12b31027` | G3 | abort heartbeat + DrainExecutor timeout on ephemeral exit (I-142) |
| `f9c89bb92` | G3 | ephemeral idle-timeout exit (I-116) |
| `99a17cd2f` | G3 (hung-node) | authoritative_binding map for detect_hung_nodes; controller table carries it as a Remainder (out-of-its-model) row |
| `468900350` | G3 (hung-node) | tenant_of keyed on auth_intent |
| `9699ac8b2` | G3 (hung-node) | key on auth_intent, floor 2, TTL-only retain |
| `6b152ee22` | G3 (hung-node) | repeats across ticks; clear_persisted_state exhaustive half noted |
| `b9a131ded` | G3 (hung-node) | group by controller-authoritative node binding |
| `b6d26c001` | G3 (hung-node) | computed before stale-reap in handle_tick |
| `a62631c90` | G5 | design F5 representative (fleet-exhaust system/feature-aware); split: retry-table G7 row records it NOT-ENC there and hands the eligibility half to this campaign |
| `20afe5154` | G5 | design F5 representative (intent-matched pod resource-fit self-rejection); split: IceBackoff/solve_full halves are SLA/ICE content |
| `96d8092b8` | G5 | design F5 representative (skip closed-stream executors in dispatch); inventory listed it under G3 — re-grouped to match the design's F5 row |
| `9ce1bcf1b` | G5 | PrefetchComplete routed through became_idle inline cap |
| `c9382fd63` | G5 | became_idle inline dispatch cap per tick |
| `a52c3ec80` | G5 | builder-side features derivation from executor_kind (eligibility input) |
| `6fb244337` | G5 | PrefetchHint contents (inputSrcs) — warm-path correctness |
| `c5c5ccd17` | G6 | design F6 representative (leader-gate reassign_derivations); retry-table G5 row records it NOT-ENC there |
| `0ea9bd701` | G6 | design F6 representative (advertise only post-recovery generation); old hash `5c47af5ad` |
| `374280877` | G6 | leader gates on ProcessCompletion/ReportExecutorTermination/Tick + clear_persisted_state per-generation maps; split: breaker/gc-roots/log-seq halves out of scope |
| `445928288` | G7 | scheduler-side ICE/ack arming (arm-on-ack + dag sweep + single edge-reload owner) |
| `461f6c661` | G7 | scheduler-side ICE clear semantics (registered_cells/heartbeat, not pending ack) |
| `2c8abc9b6` | G7 | scheduler-side ICE-attempt orphan reap keyed on dag state |
| `9917c384d` | G8 | bound every worker-supplied string at the boundary |
| `2143845d6` | G8 | bound derivation_path length at ingestion |
| `d40b3ee86` | G8 | worker-supplied float validation |
| `6b0de6e4e` | G8 | worker log line-number ordering + span totality |
| `7ffbf1415` | G8 | BuildPhase ingestion gated on (executor, drv) binding |
| `496e6fb14` | G8 | (executor, drv) binding check in recv task |

### Cross-campaign-owned rows: retry (21)

Each row links the owning retry artifact it will be cross-referenced
against at 0d (no re-run here).

| Hash | Owning retry row / artifact |
|---|---|
| `ee9302b86` | retry calibration table G5 (race-ahead report keeps pending entry; permanent witness `quint-retry-calib-g5-race-ahead`) |
| `e872b2b49` | retry calibration table G5 (non-promoting report preserves the correlation entry) |
| `dc094dd0c` | retry calibration table G1 (assigned-only disconnects; covered by `retryCalibG1DisconnectCharges`) |
| `8d38cb999` | retry calibration table G1 (I-213 disconnect-path exemption) |
| `c13f6a277` | retry calibration table G1 (I-213 max_retries exemption; NOT-ENC there, P4 vehicle) |
| `8283d4362` | retry calibration table G1 (window-reset gate + controller-OOM cap-check, two falsifying rows) |
| `172776b1b` | retry divergence catalog C1/C4 + controller Stage-C G-E row (deadline-exceeded ownership) |
| `2acd1b327` | retry calibration table G6 (floor-ladder family, NOT-ENC) + controller Stage-C G-G row |
| `c55467cbc` | retry calibration table G6 (floor-ladder family) |
| `37c21bb7b` | retry calibration table G6 (floor-ladder family) |
| `1184d1bb8` | retry calibration table G6 (floor-ladder family) |
| `12b86c285` | retry calibration table G6 (deadline alignment) + controller Stage-C G-G row |
| `a76589e37` | retry calibration table G6 (configuration plumbing) |
| `8a016a393` | retry calibration table G1 (at-cap OOM single-count) |
| `a60d58a32` | retry calibration table G1 (PutPath exemption / window) |
| `699ad52e1` | retry calibration table G1 (exempt-cap) + G7 (draining-exclusion); the output-membership half stays with completion-intake unit tests |
| `d91df7e9f` | retry calibration table G3 (NOT-ENC build-level keep_going) |
| `3973a4f54` | retry calibration table G3 (recovery re-cascade half) |
| `5b4543c3a` | retry calibration table G3/G8 (recovery halves); non-retry halves are observability bookkeeping |
| `7d5646105` | retry campaign Phase-1b/2 implementation fix (floor outcome on controller-classified attempt rows) — owned by the retry close-out, post-dates its calibration corpus |
| `001cf0eeb` | retry campaign Stage-A self-review corrections — owned by the retry Stage-A record |

### Cross-campaign-owned rows: controller Stage-C table (43)

All 43 are members of the controller campaign's pinned 95-commit
corpus; each links its family row there (the G7 fleet-reconciliation
family of inventory §5 is owned by that table, per design §3.5).

| Controller family row | Hashes (this corpus ∩ that table) |
|---|---|
| G-A spawn↔reap↔queued coherence | `7f04c9d88`, `6a9ba0ef0`, `fb0953870`, `fba9086dc`, `6c4f4983d`, `9123e72d4`, `fd5d7c988`, `5e01a9ff1`, `8b0128f5a`, `004956eeb` |
| G-B ack/ICE protocol | `cdc78f839`, `5815a7544`, `485e736a2`, `af1383c0e`, `e8bd76451`, `d6bc376d3`, `408a48bcb` |
| G-C resource-accounting parity | `a415a9a8b`, `286566a57`, `d5602b3aa`, `073170dfb`, `5250a4b9a`, `b25836ef1`, `5c2a83761`, `bcfdc2262` |
| G-D placement derivation | `80cfcd65c`, `039861b56`, `3f416e02e`, `2f9a3769c`, `9fd4b6e59`, `b570cdd8d`, `015667efa`, `f97644a53` |
| G-E deadline coupling | `f73b98b1f` (and `172776b1b`, `12b86c285`, `2acd1b327` listed under retry above — one row each, the retry link is primary, the controller link noted) |
| G-F identity/security plumbing | `acf6d476b` (`a6697c6b0`, `ea10e1d74` are in-family G1 rows above with the G-F cross-reference noted) |
| FFD/cover ⇄ scheduler-config parity | `f333ebed5`, `c5320b40e`, `e013b2044` |
| Remainder | `2ad753db9`, `416895e3e`, `3c3062760`, `c8ca42a91`, `dbc7f7cb2` |

### Out-of-scope rows (56)

Fixes on the corpus files whose repaired behavior belongs to an
adjacent subsystem; each names the subsystem (the 0d table carries
these as OUTSIDE rows, not dispositioned here).

| Subsystem | n | Hashes |
|---|---|---|
| log relay / banner / LogService data plane (incl. the since-deleted in-scheduler log subsystem) | 13 | `44d4235b8`, `8f6190df7`, `849fce331`, `2c301438d`, `c04b5e2a4`, `77a08ec14`, `649b89b81`, `32cd79bec`, `7868d46f2`, `c638fe449`, `7beb1ca00`, `1d51bc845`, `5be205ebc` (gateway log rendering); log halves of split in-family rows are noted in-row above |
| SLA solver / hw-class sampling / estimator | 14 | `bce30573b`, `82f0e9fde`, `20bfb3bee`, `054e8083c`, `90fbf5b52`, `bd41e23ea`, `93ce060f0`, `c6163485a`, `13acff94f`, `b81da271f`, `a9a2e6fc1`, `827b56255`, `077854387`, `c967d75d6` |
| FUSE / overlay / store / FMP probe | 7 | `d5b99450d`, `9c85bcfe5`, `bf7e516e4`, `96056b318`, `8f917db2c`, `77f628ddb`, `702b9ea00` |
| controller pod-spec construction (pool/pod.rs and friends; excluded from the controller corpus by its definition, G-D-disposition coverage there) | 4 | `cda4ad612`, `5b4db724d`, `54ec6d079`, `3ec9120af` |
| auth / service-token gating | 3 | `e36a645cc`, `a92c03ddf`, `fb3ea232d` |
| build/DAG bookkeeping (build-level transitions, client-orphan sweep, cancel bookkeeping) | 3 | `71a7c8a9b`, `a54ac4650`, `1dd32cc10` |
| builder execute-loop / cgroup resources | 2 | `34a4c40be`, `a6b72bf94` |
| controller pool status/census + ComponentScaler (outside both pinned corpora) | 2 | `b19164959`, `e89b89110` |
| observability / tracing | 2 | `475b79eee`, `81963379a` |
| test / spec-annotation hygiene | 3 | `785288a3b`, `0dbd5f2af`, `f005fa55c` |
| CA-derivation dispatch chain | 1 | `6434a2f45` |
| retry-policy configuration validation | 1 | `002effbab` |
| lease / leader-election machinery (rio-lease campaign) | 1 | `125feb450` |
| **Total** | **56** | |

Three rows in this partition re-disposition an
inventory §5 grouping, recorded here so the deviation is explicit:
`849fce331`/`2c301438d` (inventory G6) are out-of-scope log-relay
content — the gated object is log-buffer state owned by the log
campaign and since deleted from the scheduler; `96d8092b8`
(inventory G3) is in-family G5 to match the design's F5
representative row; `f1902fe63` → `125feb450` (inventory G6) is
out-of-scope lease machinery — the hook-ordering forwarder belongs
to the rio-lease campaign's model, not Model S.

### Per-family encodability pre-registration (design §3.5, carried into the partition)

Pre-registered now so every 0d verdict is a checked prediction; an
encodability prediction that fails at 0d is a stop-and-report, never
a silent re-disposition.

| Family (in-family bucket) | Pre-registered encodability |
|---|---|
| F1 (G1) | Model S |
| F2 scheduler half (G2: phantom drain, adopt, dispatch-decouple, unsolicited-Cancelled, slot coherence) | Model S |
| F2 builder half (G2: swap-after-Ok `6b6cfcf10`, half-close `8201db59b`/bug_117, drain-on-delivery `1353d3224`, relay-watch `29222884e`, exit choreography rows) | Model D at await-point granularity |
| F3 per-executor liveness (G3: reap bound, stall credit, RPC timeout, idle/ephemeral exit) | Model S |
| F3 hung-node / node-aggregation sub-family (G3: the six hung-node rows) | node-regime cfg only (2 slots, 1 node, 2 tenants); else NOT-ENCODED with `chaos.nix` and `lifecycle/recovery.nix` named as the coverage |
| F4 lifecycle half (no standalone commits; the hand-off halves of T-0d.4) | Model S (needs the exec_id-carrying `recently_disconnected` map and the establishment action) |
| F5 (G5) | Model S; the static-eligibility *content* (kind/system/features arithmetic, warm/prefetch internals) is expected NOT-ENCODED with the dispatch/assignment unit tests named |
| F6 (G6) | Model S (fault-leader regime; deposed-believer window kept reachable) |
| F7 (G7 scheduler-side rows) | NOT re-modeled — covered by `spawnCoherence.qnt` (controller campaign) plus Model S's stated guarantees (busy-accuracy, ack arming, report idempotency); expected NOT-ENCODED rows naming that coverage |
| F8 (G8) | NOT-ENCODED by design (bounds checks + existing unit tests at the gRPC boundary) |
| G7 controller half (cross-campaign bucket) | controller campaign's table (already calibrated there) |

## Stage-B record (0c) — Models S and D

Executed in worktree `executor-p0c` (base `f3bf70c0d`). The 0c re-pin
check (the re-pin protocol's pre-Stage-B run) found two kinds of
post-pin commits only: the 0b spec-rule commit (comment-only markers +
new rules) and rio-retry-kernel proof-path-representation work
(cfg(kani) only, production semantics unchanged) — neither triggers
re-validation, so the 0a/0b artifacts stand unchanged under this
record.

### The models

- `executorSession.qnt` (Model S): the scheduler's session state
  machine — registration/dual-register and the hijack guards, the
  heartbeat reconcile (keep / adopt / two-strike phantom), dispatch
  push + rollback, completion intake (capacity hoist, one-shot drain,
  idempotency/staleness guards), disconnect with the I-056a
  stale-epoch filter, the worker-time reaper and stall credit, the
  recently_disconnected correlation/establishment lifecycle, draining
  flags, the dispatched_cells bookkeeping, and the failover
  convergence machinery (depose / observed loss / re-acquire /
  recovery / 45 s reconcile). Regime modules: base, fault-stream,
  fault-process, fault-leader, fault-persist.
- `executorDelivery.qnt` (Model D): the builder's delivery
  choreography at await-point granularity — completion_pending armed
  before the first await, the permanent sink, the relay
  swap-after-confirmed-open and its one-message buffer, the
  stream-local in-flight cell distinct from endpoint receipt, the
  half-close flush, the SIGTERM drain gate and idle exit, the B5
  generation watermark. Regime modules: base, fault-stream,
  fault-process. **Model D regime-split decision (T-0c.3):** no
  separate fault-leader cfg — the serving-generation move is an
  environment action inside fault-stream, and the
  generation-moved-vs-holder-changed distinction needs no regime of
  its own because nothing in the builder reacts to a generation move
  except the stale-assignment fence (checked there).

### Bounds (final values; design §3.2 constants as encoded)

| Bound | Design §3.2 | Wired value | Note |
|---|---|---|---|
| executor slots | 2 | 2 | |
| derivations | 1–2 | 1 (wired regimes) | within the design range; the 2-drv instantiation remains available to override modules; lowered from 2 during Stage B for state-space cost, with every witness re-verified violated at the lower bound |
| epoch / reconnect ceiling | 2 | 2 (per slot) | epochs numbered per slot (the stale-disconnect guard only compares same-slot epochs), so the production-global STREAM_EPOCH_SEQ ordering is not part of the state |
| in-flight heartbeat per slot | ≤1 | 0 explicit | heartbeat content is read from the builder's current truth at the arrival step; staleness arises from interleaving (no stored message copy) |
| pod-death budget | 1–2 | 1 (fault-process, fault-persist) | within the design range; lowered from 2 during Stage B for per-check cost with every fault-process witness re-verified violated |
| failover budget | 1 | 1 (fault-leader, fault-persist) | |
| persist-fault budget | 1 | 1 (fault-persist) | restricted to divergences a lost write can actually leave (PG keeps the previous value) |
| tick ceiling | 2–3 buckets | structural | bucketed effects only; a tick that changes nothing is a self-loop, so no numeric tick counter is needed |
| STARVE_BOUND | named const | 2 | starved-ticks counter capped at STARVE_BOUND+1 |
| fault-stream loss/dup/delay | 1 each | 1 each | loss = in-flight WorkAssignment loss; dup = duplicate assignment delivery; delay = heartbeat drop; plus half-death 1, full connection drop 1, try_send failure (rollback path) 1. The regime is wired as TWO cfgs (message faults / connection faults — the plan's pre-registered demote-or-split fallback, taken for per-check cost); budgets per class are unchanged and witnesses were re-verified violated in their split regime |
| dispatch ceiling (added) | — | 2 per drv | bounds the attempt counter (re-dispatch after one repair stays reachable) |
| structural budgets (added) | — | SIGTERM 1, admin-drain 1, FUSE flips 2 (base) / 1 (fault), actor stall 1 | keep always-on environment toggles from multiplying interleavings; each recorded here because they bound real-world event counts per behaviour |
| voluntary exit | — | requires a reason (post-report or SIGTERM) | the bare I-116 idle exit is folded into the SIGTERM exit path (same scheduler-side observable: a clean disconnect of an idle worker); `builder.idle-exit` stays covered by its existing tests |

### Verdict table (exhaustive TLC, allInvariants per regime)

Distinct/generated state counts and wall-clocks are in the introducing
commit messages (volatile-figures discipline); this table records the
verdicts.

| Model / regime | Wired as | Verdict |
|---|---|---|
| S base | `quint-executor-session-base` | HOLD (search depth 27) |
| S fault-stream-msg | `quint-executor-session-fault-stream-msg` | HOLD (search depth 30) |
| S fault-stream-conn | NOT wired (stop-and-report: budget) | held in deep simulation; the exhaustive run did NOT converge within a gate-compatible budget at the recorded bounds (still expanding past 19 M distinct states at 31 min) — owner adjudication, witnesses wired |
| S fault-process | NOT wired (stop-and-report: budget) | held in deep simulation; the exhaustive run was not driven to completion after the sibling regime's non-convergence (same state-space class) — owner adjudication, witnesses wired |
| S fault-leader | NOT wired (budget; documented manual target) | **falsified pre-fix** (`unresolvedClaimHasRepairArmed`) — adjudicated a real defect and fixed (see the adjudication record below); the re-run on the fixed encoding finds **no violation**, with the bounded-exhaustive sweep clearing well past the falsifying trace class's depth before being stopped over the gate budget (figures in the model-flip commit message) |
| S fault-persist | NOT wired (budget; documented manual target) | **falsified pre-fix** (same root cause) — adjudicated and fixed with fault-leader; the re-run on the fixed encoding finds **no violation**, with the bounded-exhaustive sweep likewise clearing well past the falsifying trace class's depth before being stopped over the gate budget (figures in the model-flip commit message) |
| S node (optional) | NOT attempted | pre-registered NOT-ENCODED fallback taken: the F3 hung-node / node-aggregation sub-family keeps its named coverage (`nix/tests` `chaos`, `lifecycle/recovery` scenarios + the detector unit tests); recorded here, not silently unwired |
| D base | `quint-executor-delivery-base` | HOLD (search depth 12) |
| D fault-stream | `quint-executor-delivery-fault-stream` | HOLD (search depth 20) |
| D fault-process | `quint-executor-delivery-fault-process` | HOLD (search depth 17) |

### Stop-and-report (campaign owner): budget non-convergence of the demotion-floor regimes

Of the two 0c stop-and-report items, the falsification one is
resolved (see the adjudication record below); this budget item remains
open. The
fault-stream-conn and fault-process exhaustive cfgs do not converge
within a gate-compatible per-check budget at the recorded
witness-preserving bounds (fault-stream-conn was still expanding past
19 M distinct states after 31 min at 32 TLC workers; fault-stream-msg,
the converging sibling, needed ~14 min for 11.4 M distinct states —
an order of magnitude past the design's retryPolicy yardstick). The
plan's demotion floor forbids moving these regimes to `packages.*` or
silently shrinking them further, and makes a budget failure at the
witness-preserving floor an owner adjudication. Until adjudicated
their exhaustive cfgs are not wired; their witness checks are wired
and green; base and fault-stream-msg carry the wired exhaustive
coverage. Possible owner outcomes: accept multi-ten-minute checks,
authorize a further split (one fault class per cfg), authorize a
coarser re-encoding of Model S, or accept
representative-revert-only calibration for the affected slices.

### Falsification adjudication and fix record (`unresolvedClaimHasRepairArmed`)

**The falsification (Stage B, 0c).** The as-built encoding falsified
`unresolvedClaimHasRepairArmed` (F2's armed-safety form) in the
fault-leader and fault-persist regimes: a PG-Assigned derivation whose
executor entry is recreated around a failover, deferred by the
one-shot 45 s reconcile because its first heartbeat had not yet
landed, and never revisited afterwards (the reaper and the disconnect
path requeue only the entry's own running_build; the backstop scans
Running only; the heartbeat reconcile and the two-strike phantom key
off the entry, never the DAG-side binding; the correlation map was
wiped at recovery) — no repair mechanism armed until the next leader
transition. The write-up, the trace and the model-bug-vs-code-bug
analysis are in
`~/tmp/rio-formal-verification/executor-0c-falsification-unresolvedClaimHasRepairArmed.md`,
with the adversarial confirmation (every load-bearing claim verified
against the code, the trigger window, the consequence class) in
`executor-0c-falsification-review.md` alongside it. No other regime
falsified anything.

**Adjudication: real as-built defect (option 1 of the report).** Per
the pre-registration above ("Expected as-built falsifications: none"),
the falsification was a stop-and-report; the campaign owner
adjudicated it a real defect and routed it to the normal fix process
— the known-defect row below is the record. Phase 0 itself made no
production change; the fix landed as ordinary scheduler work.

**Known-defect row (fixed).**

| Defect | Consequence | Fix |
|---|---|---|
| A claim deferred by the post-recovery reconcile (worker stream-connected, first heartbeat not yet accepted) was never revisited: the reconcile timer was one-shot and no other mechanism consults the DAG-side `assigned_executor` binding for an entry whose `running_build` is empty. | A silently stuck build (derivation parked Assigned to a live, idle executor) until the next leader transition / scheduler restart or an external cancel; contagious to every later build wanting the same drv; no double-charge, no double-dispatch, no incorrect result. | `handle_reconcile_assignments` now re-arms `schedule_reconcile_timer` whenever its collection pass deferred at least one claim, so each deferral grants one more full reconcile window and the follow-up sweep resolves the claim (cross-check, orphan arm, or defer-and-re-arm again). The repair stays on the established no-charge reset path (`reset_orphan_to_ready`); the new `rio_scheduler_reconcile_deferred_total` counter makes the defer path observable. Red-first actor test: `test_deferred_assigned_claim_revisited_by_rearmed_reconcile` (rio-scheduler). |

**Model and verdict flip.** `executorSession.qnt`'s `reconcileSweep`
now encodes the fixed behavior (`reconcilePending` stays set while any
claim was deferred, mirroring the re-arm), and the invariant no longer
falsifies in either regime's re-run (the fault-leader and fault-persist
rows of the verdict table above; counts and wall-clocks in the
model-flip commit message). The two exhaustive cfgs remain unwired on
per-check cost grounds only — at the witness-preserving bounds they
exceed the gate budget, the same class as the budget stop-and-report
above — so they are documented manual targets
(`quint verify --backend=tlc --main=executorSessionFaultLeader|executorSessionFaultPersist --invariant=allInvariants docs/spec/models/executorSession.qnt`),
and their witness checks stay wired and green so the contended states
remain pinned reachable. The falsification report and its review stay
in `~/tmp/rio-formal-verification/` as the historical trace and
confirmation record; their "awaiting adjudication" status is
superseded by this record.

### Witness results (every §3.5 pre-registered witness + extras)

All wired witness checks violate (= the contended state is reachable)
in their named regime:

| Witness (val) | Regime | Pre-registered # |
|---|---|---|
| noPhantomDrain | base | 1 |
| noHalfDeadStream | fault-stream-conn | 2 |
| noStaleEpochDisconnect | fault-stream-conn | 3 |
| noAdopt | fault-leader | 4 |
| noReapAfterStall | fault-process | 5 |
| noDeathByTwoChannels | fault-process | 6 |
| noFailoverWithInflight | fault-leader | 7 |
| noDrainWithPendingCompletion | base | 8 |
| noDeposedBeliever | fault-leader | 9 |
| noEstablishment | fault-process | extra |
| noRollback | fault-stream-msg | extra |
| noRaceAheadReport | fault-process | extra |
| noSwapWithReportOwed (D) | fault-stream | 10 |
| noInFlightCellDropped (D) | fault-stream | 11 |
| noHalfCloseFlush (D) | base | 12 |
| noExitBlockedWhileOwed (D) | base | 13 |
| noStaleAssignmentRejected (D) | fault-stream | extra |

The fault-leader witnesses (noAdopt, noFailoverWithInflight,
noDeposedBeliever) are wired against the unwired-for-invariants
fault-leader regime module — exactly the demotion-floor rule that
witnesses never leave the gate with their cfg.

### Encoding notes (what is by-construction vs checked)

Checked (the model can structurally reach the violating shape and the
checker excludes it): `claimedSlotResolvesAtMostOnce`,
`unresolvedClaimHasRepairArmed`, `noFabricatedCompletion`,
`noReapWhileFreshInWorkerTime`, `silentSlotReapArmed`,
`eligibleWorkOfferedWithinBound`, `neverOfferUnrunnableWork` (the
flag/exclusion clauses), `rollbackRestoresExactly` (the structural
half), `convergenceToGroundTruth` (the no-fabrication and
no-unresolved-after-sweep halves), and Model D's
`reportSurvivesStreamChurn`, `noExitWithReportOwed`,
`atMostOnceDelivery`, `staleAssignmentsRejected`.

By construction in the as-built encoding (the production guard is
encoded as the action's precondition, so the latch can only be set by
a Stage-C override that removes it): `atMostOneLiveStreamPerExecutor`
(the live-stream hijack guard), `staleStreamEventsAreInert` (the
I-056a epoch guard), the never-create half of
`registrationRequiresBothHalves` (I-048b), the mid-build-only insert
and non-promoting no-op halves of `correlationEntryLifecycle`,
`establishmentOnlyAfterWindowCloses` (the sweep consumes only expired
entries), `deposedLeaderSessionEventsAreInert` (every PG-writing arm
is leader-gated), and Model D's `relayOnlyIntoConfirmed`
(swap-after-Ok). These are exactly the pre-fix orderings the Stage-C
representative reverts re-introduce.

Deliberate over-approximations (each priced in the model headers):
CancelSignal delivery/ordering is abstract (a force-drained or
backstopped build may still be reported by the worker — the
as-built first-to-resolve guards absorb it); channel-full try_send
failure is represented by the fault-stream send-failure budget;
best_executor's scoring/warm preference is collapsed to "any eligible
slot".

### Convergence record and check-budget impact

- Model D regimes verify exhaustively well inside the per-check
  guidance.
- Model S regimes are substantially heavier than the design's
  retryPolicy-failover yardstick at the same §3.2 bounds even after
  the witness-preserving reductions recorded above (the per-slot
  session vector times two slots, the six observation channels and the
  repair interleavings dominate). Measured figures are in the
  introducing/wiring commit messages; the base and fault-stream-msg
  cfgs are wired (minutes-class checks), and the
  fault-stream-conn / fault-process budget non-convergence is the
  open stop-and-report item above (the plan's demotion floor forbids
  moving them out of `checks.*` by executor decision, so the wiring
  waits on the owner). The fault-leader / fault-persist re-runs on the
  fixed encoding are in the same budget class (they exceed the
  ~15 min gate guidance at the witness-preserving bounds — figures in
  the model-flip commit message), so they are documented manual
  targets per the adjudication record above.
- New checks wired by this stage: 22 (5 exhaustive regime cfgs — S
  base, S fault-stream-msg, D base, D fault-stream, D fault-process —
  plus 17 witness checks); the S fault-stream-conn / fault-process
  cfgs (budget stop-and-report), the fault-leader / fault-persist
  cfgs (falsified pre-fix, now resolved and held back on budget — see
  the adjudication record) and the optional node regime add 0.

## Stage-C corpus freeze (0d, T-0d.1)

Frozen 2026-05-27 at the formal-sprint tip `2f92ba735`, immediately
before any override module or calibration run, per the re-pin
protocol's pre-Stage-C trigger.

- **Churn re-pin re-run** (`git log 277618342..HEAD -- <the nine
  churn/corpus paths>`): exactly one commit touches the in-scope set —
  `48c00e9d1` (the 0b spec-rule commit: comment-only `r[impl]` markers
  plus the five new rules). Per the re-pin protocol's exclusion list
  (comment-only / marker-only changes) it does not trigger
  re-validation; the 0a/0b artifacts stand unchanged under this
  freeze. The post-0c commits on the shared actor files
  (`1bbad1ee7` recovery re-arm fix, `5e2867a40` actor submodule
  refactor) touch no corpus path and are already accounted for in the
  Stage-B adjudication record above.
- **Corpus query re-run at the freeze tip**: returns the same **170**
  fix commits as the 0a pin — zero new corpus commits landed during
  0b/0c, so the 0a/0b denominators (50 in-family / 21 retry-owned /
  43 controller-owned / 56 out-of-scope) carry into 0d with **no
  delta**. The 0d gate is audited against exactly the counts
  pre-registered above.
- **Representative-hash drift check**: every design-named
  representative and cross-campaign exemplar hash re-verified as a
  HEAD ancestor at the freeze tip; no rebase has rewritten any pinned
  hash since 0a, so the 0a cross-walk (`5c47af5ad` → `0ea9bd701`,
  `f1902fe63` → `125feb450`) stands and no new old→new substitution
  is needed.
- **Known-defect record**: the `unresolvedClaimHasRepairArmed` defect
  found at 0c was fixed by `1bbad1ee7` (re-arm the post-recovery
  reconcile when its sweep defers a claim) on `actor/recovery.rs` — a
  shared actor file outside the nine-path corpus — so it does not
  enter the corpus or move the denominators; its calibration-table
  treatment is the known-defect cross-reference row in the Stage-C
  section below, citing the fix and the now-holding invariant rather
  than a re-derivation.

## Stage-C verification runs (serial) and outcome summary (0d)

Protocol: every override ran serially against the same TLC invocation
the CI checks use (`quint verify --backend=tlc --main=<module>
--step=calibStep --invariant=<predicted>`, 32 TLC workers); violation
runs stop at the first counterexample, the two HOLDS probes and the
two distinguishing exhaustive runs ran to exhaustion. Depths and state
counts are in the calibration table below; wall-clocks live in the
introducing commit messages and the run transcripts.

- **Eleven of eleven falsifying overrides falsify on the first run.**
  Ten falsify exactly the invariant the design's §3.5 table
  pre-registered for them; the eleventh (`0127cf854`, the phantom
  two-strike drain) falsifies the calibration-added
  `confirmedPhantomIsDrained` rather than the pre-registered
  `unresolvedClaimHasRepairArmed` — the recorded checked-prediction
  correction below.
- **The two analysis-predicted HOLDS rows hold non-vacuously.** The
  `0127cf854` override holds `unresolvedClaimHasRepairArmed`
  exhaustively (the slot-binding disjunct masks the drain's absence —
  the reason the new invariant was needed), and the `be3ad068e`
  no-adopt override holds `allInvariants` exhaustively while its
  module-local `canReachOrphanedInflight` witness violates (the
  contended state is reachable, so the HOLDS verdict is about the
  guards, not vacuity).
- **One invariant was added to the main model** as part of the
  calibration (the retry-campaign precedent):
  `confirmedPhantomIsDrained` (executorSession.qnt) — by construction
  on the as-built encoding (the confirming heartbeat always clears the
  binding), latched via `confirmedPhantomRetained`, falsified only by
  the 0127cf854 override. The wired base and fault-stream-msg checks
  re-verify green with the new conjunct at unchanged distinct-state
  counts (figures in the introducing commit message), so the Stage-B
  verdict table above stands.
- **No new invariant falsified on the unmodified model** — no
  stop-and-report event was raised by Stage C. The two regimes already
  documented as over-budget (fault-stream-conn, fault-process) were
  not re-attempted exhaustively; their overrides' baselines cite the
  Stage-B deep-simulation + witness evidence per the campaign plan.
- **Baselines.** Overrides at wired-regime constants (base, Model D
  base / fault-stream) use the wired Stage-B checks as their as-built
  baseline. Overrides at the documented-but-unwired regime constants
  (fault-stream-conn, fault-process, fault-leader) cite the Stage-B
  record's evidence for those regimes (deep simulation, wired
  witnesses, the fault-leader bounded-exhaustive re-run). The one
  override at non-regime constants (`be3ad068e`, fault-leader narrowed
  to the failover budget alone) needed no falsification-attribution
  baseline because it produced a HOLDS verdict.

## Stage-C calibration: the historical-fix corpus replayed against Models S and D

The 50-commit in-family slice of the 170-commit corpus pinned above,
replayed against `executorSession.qnt` (Model S) and
`executorDelivery.qnt` (Model D); the 21 retry-owned and 43
controller-owned commits are cross-referenced (not re-run) and the 56
out-of-scope commits carry their owning subsystem, per T-0d.4. Method
per the prior campaigns: each override is a module in
`docs/spec/models/calibration/executor-<family>-<slug>.qnt` (one file
per representative — this campaign's layout variant, recorded in the
calibration README) that instantiates the as-built model, replaces ONE
action with a local PRE-FIX variant, and exposes it as a `calibStep`;
the violation latches keep the as-built oracle. Verdict format:
invariant @ step (counterexample depth in states, states
generated / distinct).

Classification legend: **ENC** — encodable, override written and run.
**ENC-A** — encodable, dispositioned by analogy: the mechanism is
encoded in the as-built model and the named sibling override (or wired
check/witness) exercises the same machinery; not separately run.
**NOT-ENC** — the model abstracts the mechanism away (missing
dimension named, covering vehicle named). **N/A** — no modeled
protocol content (docs/test/superseded). Splits noted in-row follow
the 0a partition's one-row-per-commit rule.

### G1 — session identity (8) → F1

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `db457374f` | stream_epoch written before the reconnect-rejection guards — a stale/rejected stream's disconnect evicted the legitimate worker (the I-056a class; the deadline/backstop accounting halves are retry-table rows, cross-referenced below) | ENC | `executorCalibF1StaleEpochApplies` (executor-f1-stale-epoch.qnt) | **FALSIFIES** staleStreamEventsAreInert @ calibStep (depth 5, 738/223) — as predicted |
| `a6697c6b0` | the reader was spawned before the actor's accept/reject decision — a rejected (hijack/spoofed) connect still served the slot (controller G-F and log halves cross-referenced) | ENC | `executorCalibF1HijackAccepted` (executor-f1-hijack-accept.qnt) | **FALSIFIES** atMostOneLiveStreamPerExecutor @ calibStep (depth 3, 214/67) — as predicted |
| `ea10e1d74` | ExecutorClaims HMAC chain + per-stream caps | NOT-ENC | token/claims plumbing and wire bounds below Model S's session resolution; coverage: token-mode/auth unit + VM tests, controller G-F row, G8 bounds tests | n/a |
| `4f8f68ff8` | adopt-conflict + stream_epoch hardening (I-056 family) | ENC-A | the epoch-attribution machinery is what `executorCalibF1StaleEpochApplies` reverts; the adopt-conflict half is the same heartbeat-reconcile arm the F2 `be3ad068e` row covers | by analogy |
| `3082598a3` | stale draining/degraded flags not cleared on reconnect (I-056a flag half) | ENC-A | the reconnect stale-flag clear is encoded in `connectAccept`; its loss is an eligibility/liveness regression below the safety set (the slot stays unofferable until the next heartbeat refresh) | by analogy (mechanism encoded; no safety latch) |
| `451f2dc80` | I-048 zombie guards (heartbeats creating session state for unknown executors) | ENC-A | the I-048b entry-existence drop is the heartbeat's `present` precondition; its loss is exactly the `heartbeatCreatedEntry` latch of registrationRequiresBothHalves (by-construction list, Stage B) | by analogy (latch exists) |
| `9a2dbc873` | SIGTERM-reconnect re-registered an idle slot (I-195 fast-path) | ENC-A | the idle fast-path is Model D's `streamOpenStart` drain-gate precondition and `gracefulExit` idle disjunct; its loss is reconnect churn, no safety latch | by analogy (mechanism encoded; no safety latch) |
| `83e0b338f` | duplicate-Register handling (LogBuffers bound + step_down halves are log/lease content) | NOT-ENC | wire-level duplicate-message handling below the model's stream-open resolution; coverage: executor_service unit tests | n/a |

### G2 — outcome delivery (11) → F2 (scheduler + builder halves)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `0127cf854` | phantom running_builds never drained — the slot kept a binding the worker never reported and the claim never resolved (I-035) | ENC | `executorCalibF2PhantomNoDrain` (executor-f2-phantom-no-drain.qnt) | **FALSIFIES** confirmedPhantomIsDrained @ calibStep (depth 6, 6,177/921). **Checked-prediction correction:** the design predicted unresolvedClaimHasRepairArmed; that invariant HOLDS exhaustively over the same override (depth 25, 15,614,761/1,009,472) because its slot-binding disjunct counts the retained binding as an armed repair. Disposition: unstated property → `confirmedPhantomIsDrained` added to executorSession.qnt (by construction as-built, latch `confirmedPhantomRetained`), wired regimes re-verified green at unchanged state counts, falsified by this override. The new invariant is what makes the slot-binding disjunct of the armed-safety form a real arm. |
| `be3ad068e` | a reconnecting worker's still-running build was not re-adopted into the DAG (I-066 lifecycle half; the I-062 FOD resource-fit half is SLA content, out of scope) | ENC (HOLDS probe) | `executorCalibF2NoAdopt` (executor-f2-no-adopt.qnt), fault-leader constants narrowed to the failover budget | **HOLDS** allInvariants @ calibStep, exhaustive (depth 33, 14,561,387/1,063,710); module-local witness canReachOrphanedInflight violated (depth 7, 1,115/224) so the orphaned-in-flight state is reachable. **Checked-prediction correction:** the design predicted unresolvedClaimHasRepairArmed / convergenceToGroundTruth; neither falsifies because the orphaned build's derivation is Ready (not a claim) and the first-to-resolve guards absorb the duplicate outcome. Disposition: redundant at model resolution — the adopt arm's protection is economy (the in-flight build is re-learned instead of re-run) and execution correlation, not a safety invariant; recorded as a Phase-1 / 0e-contract input (consistent with the replacement design's §4.2 deletion of the adopt mechanism). The pre-fix "recorded as phantom-failed" charging half is the retry campaign's attemptsChargedOnce surface (cross-campaign). |
| `6b6cfcf10` | relay target swapped at request time, before the open confirmed (merged_bug_020) | ENC | `executorCalibF2dEagerSwap` (executor-f2d-eager-swap.qnt, Model D) | **FALSIFIES** reportSurvivesStreamChurn @ calibStep (depth 11, 2,496/1,125) — as predicted; relayOnlyIntoConfirmed falsifies on the same trace one step earlier |
| `8201db59b` | completion_pending armed only at send_completion + no graceful half-close (bug_012 / bug_117) | ENC | `executorCalibF2dLateArmNoHalfClose` (executor-f2d-late-arm.qnt, Model D) | **FALSIFIES** noExitWithReportOwed @ calibStep (depth 8, 66/44) — as predicted |
| `1353d3224` | drain not gated on completion-delivered | ENC-A | the drain gate is Model D's gracefulExit drain disjunct (¬completionPending) plus the noExitBlockedWhileOwed witness; same latch class as the `8201db59b` override | by analogy (sibling falsified exitWithReportOwed) |
| `29222884e` | relay did not watch the target change (report stranded on a parked relay) | ENC-A | the relay park/swap/re-buffer machinery is the as-built relayTarget encoding; its loss is the same report-stranded-while-owed class the `6b6cfcf10` override falsifies | by analogy |
| `aaa08721d` | dispatch coupled to heartbeat arrival (lost-assignment prevention; FUSE-warm bound + build_id sanitize halves out of scope) | NOT-ENC | the heartbeat-inline dispatch pacing is below the model's tick-granularity dispatch pass; coverage: `sched.actor.dispatch-decoupled` + dispatch pacing unit tests | n/a |
| `41bc8dd97` | unsolicited-Cancelled completion left the drv stuck after the slot freed (batch-persist/event halves out of scope) | ENC-A | the completion-intake guards and the capacity-free hoist are the builderComplete encoding (accepted check before terminal transition); the Cancelled-classification content is the retry fold's | by analogy (mechanism encoded; verdict content retry-owned) |
| `cc1ca02a7` | BuildSlot state not under one mutex (upload-scan half store-owned) | NOT-ENC | intra-process atomicity below Model D's atomic single slot; coverage: builder runtime unit tests | n/a |
| `d653222cf` | early graceful-drain missing on the builder (gateway/FUSE halves out of scope) | ENC-A | the SIGTERM drain transition + drain gate are Model D's sigtermArrives/draining machinery; same latch class as `8201db59b` | by analogy |
| `e4ed7b6a9` | builder DrainExecutor retry on not-leader | N/A | the mechanism was deleted by `fb3ea232d` (the builder no longer calls DrainExecutor — contradiction record C3); nothing to revert in the as-built protocol | n/a |

### G3 — liveness calibration (12) → F3

Per-executor sub-family (6):

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `5971778f8` | the reap applied the ×3 twice (per-tick increment gate + counter), so the observing tick never reaped a worker-time-stale slot (the handle_tick leader-gate half is the F6 gate family) | ENC | `executorCalibF3ReapCounterNotReached` (executor-f3-reap-strikes.qnt) | **FALSIFIES** silentSlotReapArmed @ calibStep (depth 6, 774/191) — the design row's enabled-implies-fires alternative; the leader-gate half is by analogy with the F6 override; the 30-vs-60 s numeric half is below bucketed-time resolution (covered by `sched.executor.liveness-window` + the lifecycle VM scenario) |
| `1757790f2` | six of eight FMP stall sites uncredited — the reaper measured scheduler time, not worker time (I-178 family) | ENC | `executorCalibF3StallNoCredit` (executor-f3-stall-no-credit.qnt) | **FALSIFIES** noReapWhileFreshInWorkerTime @ calibStep (depth 6, 1,186/285) — as predicted |
| `44a55a224` | stall-credit early-return removal | ENC-A | the same credit discipline the `1757790f2` override reverts (one more uncredited exit arm) | by analogy |
| `e7b8ee91a` | heartbeat RPC timeout ≥ interval (bug_044) | NOT-ENC | builder-side RPC timeout arithmetic below the model's heartbeat-arrival abstraction; coverage: `builder.heartbeat.rpc-timeout` + heartbeat unit tests | n/a |
| `d12b31027` | heartbeat task + DrainExecutor not aborted on ephemeral exit (I-142) | NOT-ENC | exit-mechanics hygiene outside the protocol state (the rule is in the Stage-A not-load-bearing list); coverage: builder shutdown tests | n/a |
| `f9c89bb92` | no ephemeral idle-timeout exit (I-116) | ENC-A | the idle exit is folded into Model S's voluntary-exit path and Model D's gracefulExit idle disjunct; its loss is occupancy cost, no safety latch | by analogy (mechanism encoded; no safety latch) |

Hung-node / node-aggregation sub-family (6): `99a17cd2f`,
`468900350`, `9699ac8b2`, `6b152ee22`, `b9a131ded`, `b6d26c001` —
**NOT-ENC**, exactly as pre-registered (the optional node regime was
not attempted at 0c; the fallback was recorded there). Coverage: the
`chaos` and `lifecycle/recovery` VM scenarios, the
`detect_hung_nodes` unit tests, `sched.admin.hung-node-detector+3`;
`99a17cd2f`'s controller-side touch is carried by the controller
table's Remainder row; the signal's replacement-era successor is OA2.

### G4 — death attribution (0 standalone) → F4: the hand-off rows

The 0a partition records no standalone in-family G4 commit (the
charging halves are retry-owned). F4's in-family content is the two
dedup-lifecycle halves the retry close-out hands over, calibrated here
as pre-registered (Model S, the exec_id-carrying map and the
establishment action):

| Hand-off half | Pre-fix behavior reverted | Class | Override | Verdict |
|---|---|---|---|---|
| the I-197 `last_completed` discriminator (and the late-disconnect-vs-reconnect race it closes) | a correlation entry minted for an already-classified death (race-ahead report) — the double-charge precondition | ENC | `executorCalibF4EntryNotMidBuild` (executor-f4-entry-not-mid-build.qnt) | **FALSIFIES** correlationEntryLifecycle @ calibStep (depth 7, 29,705/4,543) — as predicted |
| the establishment-window guard (the 60 s TTL discipline of the establishment sweep) | establishment fired before the classifying report's window closed | ENC | `executorCalibF4EstablishBeforeWindow` (executor-f4-establish-early.qnt) | **FALSIFIES** establishmentOnlyAfterWindowCloses @ calibStep (depth 7, 15,114/2,454) — as predicted |

### G5 — eligibility coherence (7) → F5

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `96d8092b8` | closed-stream executors stayed dispatchable until phantom detection (I-095) | ENC | `executorCalibF5OfferClosedStream` (executor-f5-offer-closed-stream.qnt) | **FALSIFIES** neverOfferUnrunnableWork @ calibStep (depth 5, 1,025/313) — as predicted |
| `a62631c90` | fleet-exhaust verdict not system/feature-aware (the eligibility-content half handed over by the retry table's G7 row) | NOT-ENC | **checked-prediction correction:** the design's §3.5 table named this an F5 representative, but the 0a/0b encodability carve-out already pre-registers static-eligibility *content* (kind/system/feature arithmetic) as NOT-ENCODED — Model S carries the flags, not the arithmetic. Re-dispositioned to NOT-ENC naming that carve-out; coverage: `sched.dispatch.fleet-exhaust+3`, the placeable()/eligibility unit tests, the retry-table G7 cross-reference | n/a |
| `20afe5154` | intent-matched pod resource-fit self-rejection (IceBackoff / solve_full halves are SLA content) | NOT-ENC | **checked-prediction correction:** same carve-out as above — resource-fit arithmetic is pre-registered NOT-ENCODED (the model's builder reject carries no resource dimension); coverage: `sched.assign.resource-fit`, assignment resource-fit unit tests, the freeze-detector observable | n/a |
| `9ce1bcf1b` | PrefetchComplete not routed through the became-idle inline cap | NOT-ENC | inline-dispatch pacing below tick granularity (the model's deliberate latency carve-out); coverage: `sched.dispatch.became-idle-immediate` + pacing unit tests | n/a |
| `c9382fd63` | became-idle inline dispatch uncapped per tick | NOT-ENC | same as above | n/a |
| `a52c3ec80` | builder-side features derivation from executor_kind | NOT-ENC | eligibility content (features arithmetic); coverage: assignment/features unit tests | n/a |
| `6fb244337` | PrefetchHint contents (inputSrcs) | NOT-ENC | warm-path content outside the session protocol; coverage: prefetch unit tests | n/a |

### G6 — failover convergence (3) → F6

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `c5c5ccd17` | reassign_derivations not leader-gated — a deposed leader requeued/poisoned from its stale DAG | ENC | `executorCalibF6DeposedReassign` (executor-f6-deposed-reassign.qnt) | **FALSIFIES** deposedLeaderSessionEventsAreInert @ calibStep (depth 7, 7,346/1,132) — as predicted |
| `0ea9bd701` (design hash `5c47af5ad`) | heartbeat replies advertised the generation before recovery completed | NOT-ENC | **checked-prediction correction:** the design predicted DeposedLeaderSessionEventsAreInert / ConvergenceToGroundTruth, but the pre-fix ordering is not expressible at Model S's resolution — the model imports the leader environment as an abstract action whose re-acquire is atomically "generation moves ∧ recovery completes", and the harm (a worker fenced against the actual leader) is a multi-replica observable Model S deliberately does not carry. The advertise-before-recovery ordering is the rio-lease campaign's surface: `sched.lease.claim-before-advertise`, the leaderElection.qnt recovery-completion machinery (recovery keyed to the acquire epoch), and its wired checks own it. Recorded as a per-commit re-disposition, not a family-level encodability failure (F6's falsifying representative stands above) | n/a |
| `374280877` | leader gates missing on ProcessCompletion / ReportExecutorTermination / Tick + per-generation map hygiene (breaker/gc-roots/log-seq halves out of scope) | ENC-A | the standby-drops-writes gate family is encoded as the believedLeader preconditions on completion intake, termination reports and the tick; the `c5c5ccd17` override falsifies the shared latch (durableWriteWhileDeposed) for the same gate class | by analogy (sibling falsified the shared latch) |

### G7 — fleet-supply scheduler-side obligations (3) → F7

`445928288` (ICE/ack arming), `461f6c661` (ICE clear semantics),
`2c8abc9b6` (ICE-attempt orphan reap keyed on DAG state): **NOT-ENC**,
exactly as pre-registered — F7 is not re-modeled; the
`dispatched_cells` lifecycle Model S carries (armed at init, cleared
at disconnect and the DAG-state sweep) is stated as a guarantee to
Model J/N, not checked as an invariant here. Coverage: the F7
obligations table (Stage A), `spawnCoherence.qnt` and the controller
campaign's G-B calibration rows for the controller half, the §13a ICE
unit tests and `sched.sla.hw-class.ice-mask` for the scheduler half.

### G8 — input hardening (6) → F8

`9917c384d`, `2143845d6`, `d40b3ee86`, `6b0de6e4e`, `7ffbf1415`,
`496e6fb14`: **NOT-ENC by design** (the 0a/0b pre-registration) —
bounds and binding gates at the gRPC boundary, no protocol state.
Coverage: `sched.executor.input-bounds+2`,
`sched.completion.output-membership`, `sched.log.phase-binding`,
`sched.log.path-length+2`, `sec.executor.identity-token+2` and their
unit tests at the boundary.

### Known-defect cross-reference (outside the corpus, listed for checker honesty)

The Stage-B falsification of `unresolvedClaimHasRepairArmed`
(fault-leader / fault-persist) was adjudicated a real as-built defect
and fixed by `1bbad1ee7` (outside the nine-path corpus; see the
corpus-freeze section). Its calibration treatment is this
cross-reference, not a re-derivation: the defect class (a
post-recovery deferred claim never revisited) is exactly what the
armed-safety invariant now checks on the fixed encoding — the
fault-leader / fault-persist re-runs find no violation (Stage-B
adjudication record above), and the F2/F6 calibration rows above
exercise the same armed-safety and deposed-writes latches from the
historical-fix direction. No override re-introduces the defect: the
fix post-dates the corpus, so a revert module would calibrate the
model against a defect the corpus never contained.

### Cross-campaign-owned and out-of-scope rows (T-0d.4)

- **Retry-owned (21):** the rows listed in the corpus pin above carry
  their owning retry-table links unchanged; the 0d table does not
  re-run them. The split halves of in-family rows (`db457374f`
  deadline/backstop accounting, `a62631c90` exhaust verdict,
  `c5c5ccd17` poison-branch content, `be3ad068e` phantom-failed
  charging) stay with the retry table's G1/G5/G6/G7 rows as recorded
  there.
- **Controller-owned (43):** the controller Stage-C table exists and
  is closed (verified at 0a), so the G7 controller half and the listed
  G-A…G-G/FFD/Remainder rows are cross-referenced to it; none are
  blocked-on-controller-Stage-C.
- **Out-of-scope (56):** carried as OUTSIDE rows naming the owning
  subsystem, exactly as partitioned at 0a (log relay/banner 13, SLA
  solver 14, FUSE/overlay/store 7, controller pod-spec 4, auth 3,
  build/DAG bookkeeping 3, builder execute-loop 2, pool
  status/ComponentScaler 2, observability 2, test/spec hygiene 3, CA
  chain 1, retry-config validation 1, lease machinery 1).

### Closing tally vs the pre-registered denominators

50 in-family rows above (8 G1 + 11 G2 + 12 G3 + 0 G4 + 7 G5 + 3 G6 +
3 G7 + 6 G8) + 21 retry-owned + 43 controller-owned + 56 out-of-scope
= **170 = the 0a/0b denominator**, with zero freeze delta. Per-family
verdict shape: F1 2 falsifying representatives + 4 ENC-A + 2 NOT-ENC;
F2 2 falsifying + 1 falsifying-after-property-addition + 1 HOLDS-probe
+ 4 ENC-A + 2 NOT-ENC + 1 N/A (Model D carries the builder half); F3 2
falsifying + 2 ENC-A + 2 NOT-ENC + 6 NOT-ENC (hung-node, as
pre-registered); F4 2 falsifying hand-off halves; F5 1 falsifying + 6
NOT-ENC (2 of them recorded prediction corrections); F6 1 falsifying +
1 ENC-A + 1 NOT-ENC (prediction correction); F7/G8 9 NOT-ENC as
pre-registered. Every NOT-ENC row names its covering vehicle; every
prediction-vs-verdict mismatch is recorded in-row as a
checked-prediction correction (none silently).

### Permanent expect-violation witnesses (wired into nix/quint.nix)

Six of the twelve override modules are wired as `quint-executor-calib-*`
checks — one per falsifying family with a plausible regression path in
the as-built code and a cheap counterexample (the refcount/controller
proportion); the rest stay evidence modules, re-runnable with the
calibration README recipe.

| Check | Module | Violated invariant | Guards against |
|---|---|---|---|
| `quint-executor-calib-f1-stale-epoch` | `executorCalibF1StaleEpochApplies` | `staleStreamEventsAreInert` | losing the stream-epoch attribution on disconnect delivery (db457374f / I-056a) |
| `quint-executor-calib-f2-phantom-drain` | `executorCalibF2PhantomNoDrain` | `confirmedPhantomIsDrained` | losing the two-strike phantom drain (0127cf854 / I-035) |
| `quint-executor-calib-f2d-exit-owed` | `executorCalibF2dLateArmNoHalfClose` | `noExitWithReportOwed` | arming completion_pending late / dropping the half-close flush (8201db59b, bug_012/bug_117) |
| `quint-executor-calib-f3-stall-credit` | `executorCalibF3StallNoCredit` | `noReapWhileFreshInWorkerTime` | losing the FMP stall credit (1757790f2 / I-178) |
| `quint-executor-calib-f4-correlation-entry` | `executorCalibF4EntryNotMidBuild` | `correlationEntryLifecycle` | losing the I-197 last_completed discriminator on the correlation insert |
| `quint-executor-calib-f5-closed-stream` | `executorCalibF5OfferClosedStream` | `neverOfferUnrunnableWork` | losing the I-095 closed-stream exclusion in dispatch (96d8092b8) |

The remaining six (the F1 hijack-accept, the F2 no-adopt HOLDS probe,
the F2D eager-swap, the F3 reap-strikes, the F4 establish-early and
the F6 deposed-reassign modules) stay evidence modules: their
regression paths are either guarded by a wired sibling on the same
machinery (hijack-accept by the wired stale-epoch + the Stage-B
identity-token coverage; reap-strikes by the wired stall-credit check
on the same reap pass; establish-early by the wired correlation-entry
check on the same sweep), are HOLDS evidence rather than a regression
guard (no-adopt), exercise the same latch class as a wired sibling
(eager-swap vs the wired exit-owed check and the Stage-B
relay/in-flight-cell witnesses), or sit on the costliest regime where
the leader-gate family is already covered by the wired
standby-drops-writes machinery and the lease campaign's checks
(deposed-reassign). Check-budget impact: +6 first-counterexample
checks (each terminates at its violation; transcript wall-clocks are
seconds-class), no new exhaustive cfgs.

### Phase-1 / 0e-contract inputs from the dispositions

- `confirmedPhantomIsDrained` is now part of the as-built contract the
  replacement must preserve or consciously retire: under the pull
  protocol the phantom class disappears with the push channel (no
  scheduler-recorded binding the worker never saw), which the 0e
  disposition table should state explicitly when it retires the
  two-strike mechanism.
- The `be3ad068e` HOLDS row is the calibration's evidence that the
  heartbeat adopt arm is economy/correlation, not safety, at session
  resolution — consistent with (and usable by) the §4.2 row that
  deletes it; the 0e disposition table can cite this row instead of
  asserting it.
- The `0ea9bd701` re-disposition places the advertise-gating
  obligation with the rio-lease campaign's claim-before-advertise
  surface; the 0e lease-seam note (T-0e.2) should carry it as an
  explicit dependency rather than an executor-map obligation.
- The two F5 content re-dispositions keep the static-eligibility
  arithmetic outside the model; the 0e contract's AD2 re-keying work
  should treat the placeable()/eligibility unit-test suite as the
  binding coverage for that content (no model backstop exists).
- The over-budget regimes (fault-stream-conn, fault-process) remain
  the open 0c stop-and-report; Stage C neither widened nor narrowed
  that item — the calibration evidence for the families they carry
  came from first-counterexample runs and wired witnesses, which is
  the coverage the 0e regime-coverage input (plan T-0e.7 item 10)
  must weigh.

## Open adjudications (0a tracking)

Owner for every entry: B. Meurer (campaign owner; also the
controller-campaign owner, so the cross-campaign asks below are
recorded as self-issued and tracked here rather than negotiated).
Status values: open / data-pending / decided-at-0e. The entries below
are the 0a record; the 0e dispositions (decision packages for the
owner-blocking entries, carry records for the rest) are in the
Stage-0e section's "Open adjudications at 0e" subsection below.

### OA1 — establishment-window instrument (decision recorded at 0a)

Decision needed at 0e: the pull-mode establishment deadline + slack
and the no-report degradation mode, signed against an as-built
baseline of (i) Job/pod-terminal → `ReportExecutorTermination`
acked, and (ii) terminal/death observation → derivation back to
Ready, per cause (worker-report / pod-terminal / establishment).

**Instrument decision at 0a: option (b) — the documented log/DB join
— was audited first per the plan's default; the audit (below) shows
interval (i) cannot be reconstructed from existing sources at
production retention, so the option-(a) authorization request
(additive histogram pair, the design's single sanctioned Phase-0
production change) is escalated to the campaign owner now, at 0a.**
Until that authorization is granted or refused, option (b)'s partial
query (below) is the standing instrument and starts accumulating
what it can measure; the controller-outage arm is exercised in the
VM suite either way. If authorization is refused and (b) cannot be
extended, no-go condition 5 ("the OA1 baseline cannot be obtained")
is the live risk to record at 0e — not a reason to soften the gate.

Bounded source audit (the option-(b) feasibility evidence, verified
at the pin):

- Interval (i) endpoint A — Job/pod terminal: exists only as k8s
  object state (pod `containerStatuses[].state.terminated`
  / Job `Failed/DeadlineExceeded` conditions). Jobs and their
  terminal conditions are TTL-reaped 600 s after finish
  (`JOB_TTL_SECS`, `pool/job.rs:50`); pods can be deleted earlier by
  the Job controller. The controller does not log the observation
  per se; `report_terminated_pods` / `report_deadline_exceeded_jobs`
  log at `debug!` for skips and at `warn!` for RPC errors.
- Interval (i) endpoint B — report acked: the controller logs
  `info!` ONLY when the scheduler reply says the floor was promoted
  ("reported pod termination → scheduler bumped resource_floor",
  `pool/job.rs` ≈ `:1034`, and the DeadlineExceeded twin ≈ `:1106`);
  non-promoting acks are silent at info. On the scheduler side the
  second-installment classification is persisted by
  `fill_termination` (`db/attempts.rs:386`), which updates
  `termination_reason`/`outcome_class`/floor flags and **writes no
  timestamp** — the attempt row's `occurred_at`/`recorded_at` are
  set when the row is appended (at disconnect/dispatch time), so
  report/ack time is not recoverable from the DB.
- Interval (ii) endpoint A — death/terminal observation: the
  disconnect is countable (`rio_scheduler_worker_disconnects_total`,
  `actor/executor.rs:452`) but not timestamped per-executor in any
  durable store; the pod-terminal cause's true start (the pod's
  death) is the same k8s-side timestamp as interval (i) endpoint A.
- Interval (ii) endpoint B — derivation back to Ready: recoverable.
  The requeue happens in the same actor turn as the terminal
  observation's attempt-row append, so `drv_attempts.recorded_at`
  per `outcome_class ∈ {disconnected, executor_crash, backstop,
  infra, timeout, …}` (cause label = `outcome_class` ×
  `reporting_party`) is a faithful end-point; `derivations.updated_at`
  (001) is overwritten by later transitions and is only a weak
  cross-check. Establishment-cause rows are exactly the
  `fill_termination` calls made by the TTL sweep
  (`actor/executor.rs:1143-1290`).
- `build_event_log` (003) is prost-encoded BYTEA, filtered to
  state-machine events, and GC'd on terminal cleanup
  (`actor/build.rs:730`) plus a 24 h sweep
  (`housekeeping.rs:767`) — not a usable latency source.

Conclusion: interval (ii) is measurable end-to-end only for the
worker-report cause (observation and requeue are the same actor
turn) and measurable as "scheduler-side processing time" for the
other causes; interval (i) — the number OA1 actually sizes the
establishment slack against — has neither endpoint durably
timestamped on the rio side, and the k8s-side endpoint ages out in
≤600 s. A log join could only work in an environment that retains
debug-level controller and scheduler logs, which the named target
environment does not guarantee.

Committed option-(b) query (what (b) can measure today; per-cause
requeue/processing latency, NOT interval (i)):

```sql
-- per-cause attempt-terminal events at the scheduler (end-points of
-- interval (ii)); join key for any log-side start-point is
-- (executor_id, exec_id).
SELECT outcome_class,
       reporting_party,
       date_trunc('hour', recorded_at)            AS bucket,
       count(*)                                   AS n,
       percentile_cont(0.5) WITHIN GROUP (ORDER BY recorded_at - occurred_at)  AS p50_record_lag,
       percentile_cont(0.99) WITHIN GROUP (ORDER BY recorded_at - occurred_at) AS p99_record_lag
FROM drv_attempts
WHERE event_kind = 'attempt'
  AND outcome_class IN ('disconnected','executor_crash','backstop','infra','exempt_infra','timeout')
GROUP BY 1, 2, 3
ORDER BY 3, 1, 2;
```

Environment / population the baseline accumulates from (named so AD5
and no-go conditions 4/5 are evaluated against a stated population):
the EKS deployment described by `infra/eks` + `infra/helm/rio-build`
(the only standing non-VM environment), all Builder/Fetcher pools,
window = from instrument availability to the 0e cut; the
controller-outage arm is exercised at least once in the VM suite
(`nix/tests` lifecycle/chaos scenarios) and recorded alongside, per
the plan. A change of population at 0e is a recorded deviation.

Status: **escalated** (option-(a) authorization request open with
the campaign owner; decision due before 0b closes so the histograms
— if authorized — accumulate through 0c/0d). 0e-blocking via no-go
condition 5.

### OA2 — hung-node aggregation owner and shape (0e-blocking)

The ask to the controller-campaign owner is issued as part of this
0a engagement (same owner; the ordering entry added to
`controller-invariant-map.md` in this commit set is the venue): pick
the replacement signal shape for the multi-tenant stale-node signal
— L10 health reap, node conditions / NotReady-age, per-node
Job-deadline/pull-latency clustering, or an interim scheduler-side
ledger sweep over open attempts + spawn-ack node binding — and
either commit a landing slot no later than 1c or sign the accepted
1b→1d coverage gap with its bound and named compensating controls
(per design §5.2). Requested decision-by: 0e (target 2026-06-06), so
0e records a decision rather than opening the negotiation.
Status: open, 0e-blocking.

### OA3 — fetcher pull cardinality (data request)

Data needed: fetcher pool churn/cost (pod creations per FOD fetch,
fetch duration distribution vs pod cold-start, I-116 idle-exit rate
for fetchers). Source: existing pool/Job metrics + `drv_attempts`
fetcher-kind rows in the OA1 environment. Default absent data:
one-pull. Owner: campaign owner; due: before 0e (target 2026-06-06).
Status: data-pending.

### OA4 — BuildPhase fate (dashboard owner ping)

Ask issued to the dashboard owner (same person at present): drop
BuildPhase or keep it as a fire-and-forget unary in the replacement.
Inventory §1.11 records it as cosmetic (dashboard phase column
only). Due: 0e (target 2026-06-06). Status: open.

### OA5 — operator-facing fleet view and controls

Inventory of the surfaces that go blind for pull-mode pods at 1b:
`ListExecutors` / `DebugListExecutors` (admin/executors.rs, CLI),
the `workers_active` gauge, the dashboard fleet view, and the
operator controls `DrainExecutor` (per-executor drain / force-evict)
and the fleet-wide stop. 0e must record the open-attempts +
Job-census successor surface, what the dashboard loses (per-pod
heartbeat age), the sign-off owner, and the O1–O3 control
successors; the sign-off itself happens against the running
replacement at 1b. Owner: campaign owner + dashboard/operator owner.
Due: surface + owner recorded by 0e (target 2026-06-06). Status:
open.

### OA6 — forecast-spawn data query (0e-blocking, jointly owned with the controller campaign)

The 0e choice (a third `NotYetReady` pull outcome vs a ready filter
at the placeable gate / spawn pass) is data-driven. Data query
issued at 0a, due before 0d closes (target 2026-06-03), shared with
the controller-campaign owner:

- fraction of spawned Builder Jobs whose intent was ready=false at
  spawn (the §13b no-ready-filter path recorded above);
- how often such a pod registers before its drv becomes Ready, and
  the registration→Ready latency distribution;
- the I-116 idle-exit rate for those pods;
- the cold-start side: what a forecast-warmed, already-registered
  pod saves vs spawn+register (pod creation→registration latency).

Sources: `rio_scheduler_sla_forecast_dropped_total`,
`rio_controller_nodeclaim_forecast_hit_ewma` and the §13b SLI set
where they suffice; otherwise a documented log/DB join defined the
same way as the OA1 option-(b) instrument and committed alongside
this map before 0d closes. Owner: campaign owner (joint sign-off
with the controller campaign at 0e). Status: data-pending,
0e-blocking.

## Stage 0e — the frozen replacement contract and the go/no-go

Written against the 0a–0d record above (the design's 0b/0c/0d gates
are green; the one open 0c item — the budget stop-and-report on the
fault-stream-conn / fault-process exhaustive cfgs — is carried into
the no-go evaluation as plan item 10). Paper only: no production
change, no model change, no proto change. The owner-decision items
(OA1, OA2, OA6, the regime-coverage acceptance) are PREPARED here as
decision packages and marked AWAITING OWNER; nothing in this section
records a decision on the owner's behalf, and the go/no-go therefore
closes conditionally (see the evaluation subsection). Contract
references: executor-formal-design.md (DRAFT v2) §3.4, §4, §5, §6;
plan tasks T-0e.1–T-0e.8.

### The Model J/N obligation table (T-0e.1)

One row per scheduler-side obligation Model J (`spawnCoherence.qnt`)
and Model N (`nodeclaimLifecycle.qnt`) import from this subsystem,
dispositioned **meetable-by-the-replacement** (successor signal named)
or **unmeetable**. The "what the model actually does with it" column
is written against the models' own headers and invariants as wired
today, not against the design's wishes; the mechanical checklist
re-derivations and check re-runs stay at 1c'/1d (design §6) — this
table is what the 0e no-go condition 6 evaluates.

| # | Obligation (imported by) | What the importing model actually does with it today | As-built provider | Replacement successor signal | Disposition |
|---|---|---|---|---|---|
| 1 | `ListExecutors` busy view consulted by the orphan-Running reap, including freshness and fail-closed semantics (Model J) | The executor list + leader age are environment inputs to the 3-arm `orphan_reap_gate` (RPC error / leader younger than grace / empty list ⇒ no reap); `reapSafety` checks no orphan-reap of a busy executor or outside a passed gate; the busy-but-never-registered residual is a documented bound of that invariant (controller map F1) | `executors` map → `ListExecutors` (busy = `running_build.is_some()`), `sched.admin.list-executors`, `sched.admin.list-executors-leader-age` | Busy = an open pull-mode attempt exists, served by the §4.5 bridge chosen below (the ledger-backed open-attempt view consulted by `reap_orphan_running`, stream view OR'd in during coexistence); RPC-error and unanswerable arms stay fail-closed; only the leader-age arm is a retirement candidate, and only once the busy source is durable (1c'/1d) | **Meetable.** Freshness improves (durable, survives failover); the gate's fail-closed posture is retained per re-derivation item (i) below |
| 2 | Registration-edge ICE clear (Models J + N) | Model J arms `dispatched_cells` via the ack chain (`ackSoundness`/`ackCoversPending`) and treats the scheduler-side clear as environment; Model N's `iceMarkSoundness` covers the controller mark half and defers the clear ladder to `sched.sla.hw-class.ice-mask` | The ICE cell clears at the heartbeat registration edge (mechanism #22 half) | The ICE cell clears on first pull (or the controller's pod-Ready observation) — §4.2 row 22; same first-contact semantics, no heartbeat needed | **Meetable.** Equivalent edge (first contact of the pod), strictly later than Job creation, exactly what the mark/clear pairing needs |
| 3 | `dispatched_cells` ack-arming semantics (Model J) | `ackSoundness`: an intent is acked spawned only with a Job behind it; `ackCoversPending`: a surviving Pending Job is re-acked — the ack is what arms the scheduler-side cell | Armed on `AckSpawnedIntents`, cleared at the registration edge, disconnect, and the DAG-state sweep (mechanism #22) | `AckSpawnedIntents` and the arming are untouched (Model J's subject survives); only the clear trigger renames per row 2; the DAG-state sweep (cancel/substitute before the pod ever pulls) is unchanged | **Meetable.** No semantic change on the arming side at all |
| 4 | Termination-report idempotency (Model J) | Model J keeps termination reports OUT of its model on the strength of the scheduler-side dedup ("the scheduler-side dedup … makes the controller's re-report idempotent" — its header checklist); the obligation is that re-reports stay no-ops so that exclusion stays sound | In-memory first-report-wins (`recently_disconnected` consume) + the durable fill guard (`WHERE termination_reason IS NULL`, `drv_attempts_exec_id_uniq`) | The idempotent `ReportAttemptOutcome` column fill keyed by attempt identity; no-attempt rows are no-ops, never inserts (§4.1); the in-memory map goes, the durable guard stays | **Meetable.** Strictly stronger than today (no 60 s TTL race, survives failover); the no-attempt no-op rule is re-derivation input (iii) below |
| 5 | `dead_nodes` hung-node signal (Model N) | Consumed input only and OUT of Model N's checked invariants (its header: the dead_nodes arm's protection is the controller campaign's gate tests, not the model); flows `hung_nodes` → `GetSpawnIntents` response → `reap_unhealthy`'s Dead arm | Scheduler hung-node detector (#20) aggregating heartbeat staleness per node | Per OA2: a k8s-side aggregation (deadline/pull-latency clustering, node conditions, L10 health reap) or the interim ledger sweep — owner and shape AWAITING OWNER (decision package below); or the signed accepted 1b→1d gap with its bound and compensating controls | **Meetable conditional on OA2.** Two viable successor shapes exist and the consuming arm is out-of-model (an accepted gap invalidates no Model N invariant); the row cannot be marked plain-meetable until OA2 names one, and is unmeetable only if OA2 is left unresolved |
| 6 | Placeable-set input distribution: ready=false (forecast) intents reach the Job spawner today (Model J peer-behavior fact, OA6) | Model J's queue abstraction is a Ready-set; the as-built gate publishes with no ready filter (contradiction C1); the model's input distribution is therefore wider in production than in the model | `ctrl.nodeclaim.placeable-gate+5` publish, `pool/jobs.rs:353-397` | OA6(a): the pull gains a `NotYetReady{retry_after}` outcome priced into the re-targeted Model S and §4.2 rows 13/B7 — Model J unchanged; OA6(b): the gate/spawn pass re-gains the ready filter — Model J's abstraction becomes exact, `ctrl.nodeclaim.placeable-gate+5` amended (tracey bump) with the controller campaign's sign-off | **Meetable under either OA6 option** (decision package below, AWAITING OWNER); the re-derivation at 1c'/1d covers whichever side is taken |
| 7 | Cancellation/preemption read: "attempt closed, Job still active" consulted every tick, no age gate (new Model J obligation the replacement introduces) | Not in Model J today — the as-built cancel path is scheduler-side (CancelSignal/force-drain); the replacement adds a controller reconcile action whose read feeds the AD5 deletion arm | n/a (new) | The same ledger-backed open-attempt view as row 1's successor, read every tick; its read latency is a named AD5 budget component; modeled as a new action in the Model J re-derivation at 1c'/1d | **Meetable.** Same view as row 1, no extra signal needed; the latency number is signed with AD5 (awaits the OA1 baseline) |

Re-derivation work items recorded for 1c'/1d (design §3.4 item 3 — the
mechanical re-runs are NOT 0e deliverables; listed here so the
obligation table and the Phase-1 plan read the same scope):

- (i) `reapSafety` is re-checked with the orphan-reap gate's
  fail-closed posture on open-attempt-RPC error retained and only the
  leader-age arm removed;
- (ii) `ORPHAN_REAP_GRACE` (300 s, creation-age) is re-validated
  against worst-case container-start → first-successful-pull under
  leader failover + recovery gating + the §4.1 pull-retry backoff,
  with the accepted miss consequence recorded as pod respawn/churn —
  never a mid-build reap (those have open attempts) and never a
  charge (the no-attempt rule);
- (iii) the no-attempt no-op rule of §4.1 is an input the
  reapSafety/degradedPolarity re-derivation may rely on.

Summary for the no-go evaluation: 7 obligations; 5 meetable with
named successors, 2 meetable conditional on an AWAITING-OWNER
adjudication (row 5 on OA2, row 6 on OA6), 0 unmeetable.

### The lease-seam and dual-belief successor note (T-0e.2)

The §3.4 item-1 checklist re-stated against the replacement
direction; the full re-derivation is Phase 1 (1c', alongside the
Model J/N checklists). This note is the evidence for no-go
condition 7.

- **What the lease model actually exports** (verified in
  `leaderElection.qnt`): `atMostOneCASWinner`, `boundedDualLeadership`,
  `staleLeaderHasStaleGeneration`, and the regime-scoped `neverDual`
  (not imported by the executor models' fault-leader regime — the
  deposed-believer window stays reachable by design, pinned by the
  wired `noDeposedBeliever` witness).
- **The consumer that moves.** The as-built consumer of
  `staleLeaderHasStaleGeneration` is the worker-side B5 latch
  (heartbeat `fetch_max` + `is_stale_assignment`) plus the
  `WorkAssignment.generation` field. The replacement deletes both with
  the stream (1c'); the successor consumer is the §4.1
  transaction-side fence: the pull and establishment transactions
  carry the serving replica's generation, persist it on the row they
  create, and commit only if it is not below the durable claims floor
  (GREATEST over `leader_generation_claims` and `assignments` — the
  existing max_known_generation arms). `ReportOutcome` needs no
  independent check (it fills only the row whose `exec_id` a fenced
  pull minted; row-already-terminal otherwise).
- **Checked successor for the clock-pause/suspend dual-belief
  residual.** Today the residual is bounded by
  `boundedDualLeadership` and NOT closed by the executor-side fence
  (the fence/steal asymmetry); Model S explicitly does not claim
  deposed-but-unaware writes inert (its header prices this). The
  replacement closes it at the authority-exercising transactions, and
  the checked successor exists in the plan of record: the re-targeted
  Model S at 1c' retains a fault-leader regime and checks
  `StaleAuthorityWritesAreInert` (a pull/establishment transaction
  whose serving generation is below the claims floor mutates nothing)
  plus `AtMostOneOpenAttemptPerJob`, and the `admit_pull` Kani
  contract carries the same below-floor rejection (design §4.4).
  Verdict: a checked successor is named and scheduled — condition 7's
  first clause is not triggered.
- **No lease guarantee becomes unconsumable.**
  `atMostOneCASWinner`: consumed unchanged (acquisition untouched).
  `boundedDualLeadership`: still consumed — it is what bounds the
  window the transaction fence must cover, exactly as it bounds the
  B5 window today. `staleLeaderHasStaleGeneration`: still consumed —
  generation-ordering of concurrent believers is what makes the
  claims-floor comparison meaningful. `neverDual`: not consumed
  today, not consumed by the replacement (no change).
  `sched.lease.claim-before-advertise`: its successor is
  claim-before-serve (pulls served only after the acquired
  generation's claim row is durable — already recovery's ordering);
  the heartbeat-distribution clause retires with the stream (AD4).
  Verdict: condition 7's second clause is not triggered.
- **Carried dependency from the 0d re-disposition.** The
  `0ea9bd701` (advertise-only-post-recovery) row was re-dispositioned
  to the rio-lease campaign's claim-before-advertise surface; the
  replacement keeps that obligation with the lease campaign (the
  serve gate above is its analogue), and the 1c' lease-checklist
  re-derivation must restate it as claim-before-serve rather than
  drop it.
- **Acquire-epoch vs generation.** The replacement's fence compares
  against the durable claims floor, never against "the generation
  moved", so the imported distinction (a holder change need not move
  the generation once the PG floor saturates) is preserved by
  construction in the successor; the 1c' re-derivation keeps the log
  campaign's record as the citation, exactly as Model D's header does
  today.

### The retryPolicy.qnt environment re-derivation plan (T-0e.3)

The §3.4 item-2 named deliverable, written at 0e and executed at 1b
(design §6 row 1b: the pull-mode environment regime lands and is
wired into nix/quint.nix at 1b, not 1d). No retryPolicy.qnt edit
happens in Phase 0.

What changes in the retry model's environment, action by action:

- **`dispatchTo` → pull.** The attempt opens at `PullAssignment`
  (Ready→Running + `exec_id` mint + row insert in one transaction);
  the dispatch-time eligibility/exclusion precondition moves to the
  spawn-intent gate per AD2 (the intent carries excluded nodes; the
  controller renders anti-affinity). The pull-mode regime encodes
  "open attempt" as the post-pull state, not the post-push state.
- **E6/E7 (controller channel) → `ReportAttemptOutcome`.** The two
  controller report actions and their `recently_disconnected`
  correlation/dedup preconditions are replaced by the single
  idempotent column fill keyed by attempt identity; the
  second-installment semantics (`WHERE termination_reason IS NULL`)
  are unchanged; the no-attempt no-op rule replaces the no-entry
  no-op arm.
- **`establishUnreportedCrash` precondition.** Becomes "open attempt
  past deadline + report-slack with no terminal row" (the OA1 number)
  instead of "released attempt with no deliverable classifying
  report"; the non-promoting-report-does-not-establish clause
  carries over unchanged.
- **E9 / fleet-exhaust and the exclusion set re-key per AD2.** The
  exclusion set carries the attempt's source node
  (controller-authoritative, migration ≥067 column); the
  exhausted-universe verdict is evaluated at the spawn-intent gate
  (excluded-sources ⊇ spawnable-sources → NoEligibleSource →
  Poison(FleetExhausted)); the small-fleet clause (threshold
  effectively min(threshold, |sources|)) is part of the re-keyed
  encoding so the exhaustion verdict stays reachable on 1–2-node
  fleets.
- **Identity freshness becomes structural.** A fresh pod per attempt;
  the crashed-identity-never-re-registers assumption stops being an
  environment fact the executor map must provide and becomes the
  shape of the protocol.
- **What stays imported unchanged:** `attemptsChargedOnce`,
  `verdictMatchesFold`, `failoverPreservesHistory`, and the atomic
  appending transaction — the re-derivation changes the environment
  actions that feed the fold, never the fold's own invariants.
- **Coexistence.** The pull-mode encoding lands as an additional
  regime cfg (`retryPolicy` pull-mode environment) wired at 1b while
  the as-built-channel regimes stay authoritative for the stream path
  until 1c'; the as-built regimes are retired with the stream path
  (the `retryPolicyAsBuilt` freeze precedent), and the retirement is
  recorded in retry-invariant-map.md by the campaign that performs
  it.

### Open adjudications at 0e (T-0e.4)

Status verbs used below: **AWAITING OWNER** — the decision package is
prepared, the options and recommendation are recorded, and no decision
is recorded here (the campaign owner decides; for OA2/OA6 jointly with
the controller-campaign owner — the same person, but the signature is
that campaign's to give). **CARRIED** — not 0e-blocking; carried into
Phase 1 with a named owner and the pre-registered default standing.

#### OA1 — establishment-window numbers and the AD5 latency budget: AWAITING OWNER

Where it stands at this cut: the 0a source audit showed interval (i)
(Job/pod-terminal → report acked) has neither endpoint durably
timestamped at production retention, so the option-(b) join can only
measure interval (ii)'s per-cause requeue/processing lag; the
option-(a) authorization request (the additive histogram pair, the
design's single sanctioned Phase-0 production change) was escalated
to the campaign owner at 0a with a decide-by of "before 0b closes".
No authorization or refusal has been recorded; no interval-(i)
baseline exists; the controller-outage arm has been exercised only as
the VM-suite behavior (delayed report → establishment), not measured
against a production population. Consequence: the AD5 composite
budget (death→requeue, cancel/preempt) cannot be signed at this cut,
and no-go conditions 4 and 5 cannot be discharged.

The decision package:

- **Option A — authorize the histogram pair** (`rio_controller_job_terminal_report_seconds`,
  `rio_scheduler_attempt_requeue_seconds`), landed as the additive
  observability change the design sanctions, either immediately or
  co-located with the 1a handlers (the 1a gate already requires the
  new handlers to emit the OA1 instrument). Costs: one small
  production change + the observability checklist + red-first
  registration tests; the baseline then accumulates only from
  authorization forward, so the 0e signing of AD5 happens late
  (against the first weeks of data) rather than now. Unblocks: a
  stated-population interval-(i) baseline, the AD5 budget signature,
  no-go conditions 4 and 5, and the like-for-like 1b canary
  comparison the design requires.
- **Option B — accept the measurement gap** and sign the AD5 budget
  against what option (b) can measure (interval (ii) per cause from
  `drv_attempts`) plus the VM-suite controller-outage arm. Costs: the
  budget is signed against a population the design explicitly says is
  not sufficient ("a VM-suite run alone does not satisfy this"), so
  the owner is signing a known-weaker basis; condition 5 is then
  discharged only by that explicit signature, not by data. Unblocks:
  nothing else — Phase 0 stays paper-clean, but the 1b gate's
  "death→requeue latency within the budget signed at 0e" criterion
  inherits the weak baseline.
- **Option C — neither** (refuse A, do not sign B): no-go condition 5
  stands and Phase-1 planning is blocked on the OA1 row alone.

Recommendation: **Option A**, authorized now rather than at 1a. The
gap is structural — the rio side writes no timestamps for interval
(i) and the k8s side ages out in ≤600 s, so no amount of
query-writing recovers it; the instrument is small, additive,
checklisted, and the design pre-sanctioned exactly this exception;
and every later gate (0e AD5 signature, 1b canary latency criterion,
1c fleet-wide criterion) reads the same instrument, so landing it
early is what makes those gates comparable. Option B is recorded as
viable only if the owner judges the schedule cost of waiting for
data to outweigh gating the cutover on a measured baseline — the
design's own no-go wording leans against it.

#### OA2 — hung-node signal owner and shape: AWAITING OWNER

Where it stands: the ask was issued at 0a (decision-by 0e, target
2026-06-06) with the four candidate shapes from design §5.2; no
shape, owner, landing slot, or signed gap has been recorded. The
as-built detector cannot cover the transition (pull-mode pods never
feed it; 1c' deletes its substrate), so "keep the code" is not an
option that needs pricing.

The decision package:

- **Option A — controller-side aggregation, landing ≤1c**: per-node
  clustering of attempt-deadline expiries / pull-latency over the
  ledger plus the spawn-ack node binding (the controller already
  holds the node informers and the Job census), feeding the same
  dead_nodes-shaped input `reap_unhealthy` consumes today. Costs: a
  new controller-side input and its tests, inside the controller
  campaign's Model N scope (its checklist re-derivation is already a
  planned 1c'/1d item); the canary window 1b→1c still needs an
  interim statement (alert + runbook). Unblocks: obligation-table row
  5 becomes plain meetable; no-go condition 9 discharged with no
  coverage gap beyond the canary window; node-wedge detection keeps
  roughly today's latency class for the Ready-but-wedged failure mode
  (EBS/kernel/D-state) that the k8s-native backstops do not see.
- **Option B — rely on node conditions / NotReady-age + Karpenter
  NodeRepair + the L10 health reap only** (no rio-side aggregation).
  Costs: the Ready-but-wedged class regresses to deadline-bound
  detection (activeDeadlineSeconds + establishment sweep) — the exact
  class the as-built detector was built for; per-node blast radius is
  bounded only by AD2's node-keyed exclusion re-routing future
  attempts. Unblocks: condition 9 only via the signed-gap clause —
  this IS the accepted-gap option in permanent form, and the design
  treats it as acceptable only with the bound and compensating
  controls named and signed.
- **Option C — signed accepted gap for 1b→1d only** (defer the
  decision on the permanent shape to Phase 1, accept the gap for the
  transition): bound = per-build impact ≤ activeDeadlineSeconds + the
  establishment sweep; no automatic NodeClaim reap of wedged-but-Ready
  nodes during the gap; compensating controls = the AD2 node-keyed
  exclusion, an alert on per-node attempt-deadline clustering, the
  manual NodeClaim-reap runbook, Karpenter NodeRepair for
  NotReady-surfacing wedges. Costs: the gap is real for every pool
  from its flip until the permanent signal lands; the controller
  campaign must still take the re-derivation work later. Unblocks:
  condition 9 via the recorded-gap clause; 1b is not blocked on new
  controller work.

Recommendation: **Option A with C as the explicit canary-window
interim** — i.e. commit the controller-side deadline/pull-latency
clustering aggregation with a landing slot no later than 1c, and sign
the narrow 1b→1c canary-pool gap with option C's controls. This keeps
the only detection capability for Ready-but-wedged nodes (the failure
mode with real incident history behind the detector), puts the
aggregation where the informers and the Job census already live, and
matches the design's stated preference that the signal land before
the deletion wave removes the heartbeat substrate. Option B is the
fallback if the controller campaign refuses the scope — but it should
be signed as what it is (a permanent regression for one failure
class), not slid into.

#### OA6 — the pod-arrives-before-Ready pull outcome: AWAITING OWNER

Where it stands: the data query was issued at 0a (due before 0d
closes, target 2026-06-03) and the results have not been recorded in
this map; the choice is data-driven and jointly owned with the
controller campaign, so no side is taken here. The unary signatures
below are frozen except for this outcome (the design: they do not
freeze without it; Gone-on-not-Ready is not a neutral default — it
produces a reap→respawn→Gone churn loop).

The decision package:

- **Option (a) — a third pull outcome `NotYetReady{retry_after}`**
  (or a bounded long-poll): the pod waits/retries up to an explicit
  idle bound (the B7/I-116 successor; a named number or tied to
  `activeDeadlineSeconds`) and then exits 0 charge-free. Costs:
  re-introduces a (small) wait state that must be priced into §4.2
  rows 13/B7 and the re-targeted Model S; the idle bound is a new
  number to own; the signature gains a third outcome. Unblocks: keeps
  the §13b forecast warm-start (a forecast-warmed pod starts building
  the moment its drv goes Ready, no spawn latency); no controller
  spec change.
- **Option (b) — forecast intents stop minting Jobs** (the
  ready-filter at the placeable-gate publish or the pool spawn pass;
  NodeClaim pre-provisioning continues): Builder Jobs are created
  only for ready=true intents. Costs: the recorded cold-start
  regression — the first build per forecast-warmed node pays ≤1
  controller tick + pod cold start instead of dispatching to an
  already-registered pod; amends `ctrl.nodeclaim.placeable-gate+5`
  (tracey bump, gate-retain unit tests and the kwok
  forecast-provisioning VM wiring re-pointed); needs the controller
  campaign's sign-off because Model J/N's input distribution changes.
  Unblocks: the pull protocol has exactly two outcomes; the
  not-yet-Ready state is unrepresentable instead of priced; the spec
  contradiction C1 (sla-sizing.typ @alg-pool vs the gate) resolves in
  the direction the design book already documents; Model J's
  Ready-set abstraction becomes exact.
- Decision rule for when the data lands (so the owner can decide
  without re-opening the analysis): take (a) if a material fraction
  of forecast-spawned pods see their drv go Ready within the idle
  bound AND the measured registration→Ready latency saving is
  material against typical build duration; take (b) if
  forecast-spawned pods mostly idle-exit (low hit rate) or the
  cold-start saving is marginal.

Recommendation: **lean (b)** unless the OA6 data shows a material
warm-start win. The spec already promises ready-gating (contradiction
C1), Model J's verified abstraction assumes it, and (b) deletes a
protocol state instead of pricing one in — the same simplification
direction as the rest of the campaign; its cost is a bounded
cold-start regression on exactly the pods that today mostly idle-exit
at I-116. Option (a) is the right call only if the forecast-hit data
contradicts that picture, which is precisely what the outstanding
query measures.

#### OA3 — fetcher pull cardinality: CARRIED

The churn/cost data (pod creations per FOD fetch, fetch duration vs
cold-start, fetcher I-116 idle-exit rate) had not been recorded at
this cut. Carried into Phase 1 with the campaign owner; the
pre-registered default stands — **fetchers are one-pull** — and a
multi-pull exception, if the data later justifies it, gets its own
small model in Phase 1 (never silent session retention). Not
0e-blocking; no no-go condition reads it.

#### OA4 — BuildPhase fate: CARRIED

No dashboard-owner decision recorded (same person as the campaign
owner). Inventory §1.11 records BuildPhase as cosmetic (dashboard
phase column only). Carried with the dashboard owner; the
recommendation on record is **drop it** and derive the phase column
from attempt-row status, with the fire-and-forget unary as the
fallback if the dashboard owner objects when the OA5 surface is
reviewed at 1b. Not 0e-blocking.

#### OA5 — operator-facing fleet view and controls: surface and owner RECORDED, sign-off carried to 1b

The 0e deliverable is the surface, the owner, and the sign-off plan
(the sign-off itself happens against the running replacement at the
1b gate):

- **Successor surface:** open attempts (the `drv_attempts` open rows:
  derivation, exec_id, source node from the ≥067 column, age,
  deadline) joined with the controller's Job census (Job name, pod
  phase, node, age) — served as the replacement fleet/admin view and
  the source for the `workers_active` successor gauge (open-attempt
  count) and ClusterStatus. Pull-mode pods appear in this view from
  1a (the 1a gate requires it) so the canary is never blind.
- **What the dashboard loses, named:** per-pod heartbeat age,
  store_degraded / capacity flags, and live stream state — none of
  which exist for pull-mode pods; the per-pod liveness proxy becomes
  the Job/pod phase plus attempt age.
- **Operator controls (O1–O3) successors acknowledged:** O1
  per-executor drain → k8s cordon + AD2 node exclusion (no dispatch
  decision exists to steer); O2 force-evict → cancel verdict on the
  open attempt + controller Job deletion under the AD5 budget; O3
  fleet-wide stop → pause spawn-intent emission (maxConcurrent=0 /
  pools-paused switch) + bulk cancel of open attempts; expected stop
  latency = one controller tick + the AD5 abort bound.
- **Sign-off owner:** the operator/dashboard owner (B. Meurer at
  present); sign-off is a 1b gate criterion against the running
  canary view, and 1c'/deletion remains gated on it (the old surfaces
  are not deleted before the successor is signed off).

### The §4.5 choices (T-0e.5)

Recorded as the contract's choices; they take effect with the
owner's overall 0e go signature (they are design choices inside the
campaign's own scope, not external adjudications, so they are decided
here rather than packaged — but a no-go or an owner override at the
go review reopens them).

- **Busy-signal bridge vehicle: option (b)** — `reap_orphan_running`
  switches to the ledger-backed open-attempt query at 1a, treating a
  Job as busy when EITHER the stream view says busy OR an open
  pull-mode attempt exists for it, with the leader-age and RPC-error
  fail-closed arms retained unchanged until the cleanup slice.
  Reasoning: (b) is the §4.2 C3 successor pulled forward — built
  once, verified at 1a/1b, and still the consumer at 1c'/1d — whereas
  option (a)'s synthesized `ListExecutors` entries are throwaway
  bridge code that 1c' deletes, put synthetic rows into an
  operator-facing view during exactly the window operators are
  watching the canary (OA5 already gives pull-mode pods their own
  honest view), and would re-point the Model J busy-view re-derivation
  twice. The 1a red-first test (mixed fleet: Running Job older than
  the grace backed only by an open pull-mode attempt is NOT selected;
  the same Job with no open attempt IS selected — the I-165 reap
  preserved) is written against (b).
- **Post-deletion rollback posture: revert-clean deletion-wave
  commits kept rebased until the Phase-2 close-out.** Deletion wave
  1c' lands as a small set of named, revert-clean commits whose
  reverts are kept rebased and re-tested until the campaign close-out;
  forward-fix-only is explicitly NOT adopted at this cut. Reasoning:
  the wave deletes ~14 mechanisms, the session maps, and operator
  surfaces in one slice; the families whose fault-regime coverage is
  thinnest (the open 0c budget item) are exactly the ones whose
  unmodeled-property risk the standing rule warns about, and the VM
  scenarios that back their NOT-ENC rows run at gate cadence, not
  continuously — keeping the reverts warm is cheap insurance over the
  one window where a latent gap can still surface, and the cost is
  bounded by the campaign's existing rebase discipline.
- **Deletion-gate observable: the live stream-registration gauge
  (`workers_active`, or its successor name if the OA5 surface renames
  it) read in every production environment and in the VM suite, at
  zero for a full deadline horizon defined as
  max(`activeDeadlineSeconds` over all pools' live intents) + the
  builder idle-timeout + report-flush slack.** The fixed-conservative-
  number alternative is rejected: `activeDeadlineSeconds` is
  intent/operator-controlled, so any fixed number either over-waits
  the common case or silently under-covers a long-deadline pool. The
  horizon is computed at gate-evaluation time from the live intents;
  it is falsifiable and observable, and it is checked at the 1c'
  gate alongside the model re-target.
