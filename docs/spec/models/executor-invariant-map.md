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
plan tasks T-0e.1–T-0e.8. (Update 2026-05-27: the campaign owner has
since decided OA1, OA2, OA6 and signed the item-10 regime-coverage
statement — the decisions are recorded inline in the per-row DECIDED
blocks of the adjudication subsection, and the go/no-go is
re-evaluated to its unconditional verdicts at the end of the
evaluation subsection. The package texts themselves are left as
written.)

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

#### OA1 — establishment-window numbers and the AD5 latency budget: DECIDED (2026-05-27)

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

**Decision (2026-05-27, campaign owner): Option A — AUTHORIZED.** The
additive histogram pair
(`rio_controller_job_terminal_report_seconds`,
`rio_scheduler_attempt_requeue_seconds`) — the instrument the package
recommends — is authorized now rather than at 1a, and lands in the
same commit set that records this decision (per-reason / per-cause
emission, registration in the owning components' metric registries,
docs regeneration, and the metrics-registration tests, per the
observability checklist). The interval-(i)/interval-(ii) baseline
accumulates from instrument availability forward against the
population statement recorded at 0a (the EKS deployment, all
Builder/Fetcher pools; the controller-outage arm exercised in the VM
suite). Consequences: no-go conditions 4 and 5 lose their conditional
status (re-evaluated below); the AD5 numeric budget remains UNSIGNED
at this cut — it is signed against the accumulating baseline and
re-baselined from the same instrument at the 1b gate, which is the
work item this decision deliberately leaves open.

#### OA2 — hung-node signal owner and shape: DECIDED (2026-05-27)

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

**Decision (2026-05-27, campaign owner, signing jointly as the
controller-campaign owner): Option A with option C as the explicit
canary-window interim — the recommendation as written.** The
controller-side aggregation (per-node clustering of attempt-deadline
expiries / pull-latency failures over the ledger plus the spawn-ack
node binding, feeding the same dead_nodes-shaped input
`reap_unhealthy` consumes today) is committed with a landing slot no
later than 1c, inside the controller campaign's Model N scope. The
interim gap is SIGNED as limited to the 1b canary window (1b→1c,
canary pool only), with option C's bound and compensating controls:
per-build impact ≤ `activeDeadlineSeconds` + the establishment sweep;
no automatic NodeClaim reap of wedged-but-Ready nodes during the gap;
the AD2 node-keyed exclusion, the alert on per-node attempt-deadline
clustering, the manual NodeClaim-reap runbook, and Karpenter
NodeRepair for NotReady-surfacing wedges. Obligation-table row 5's
successor is therefore the controller-side aggregation; no-go
condition 9 is re-evaluated below.

#### OA6 — the pod-arrives-before-Ready pull outcome: DECIDED (2026-05-27)

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

**Decision (2026-05-27, campaign owner, signing jointly as the
controller-campaign owner): Option (a) — the `NotYetReady` third
outcome.** The pull protocol gains a third response — "not yet ready —
retry" (`NotYetReady{retry_after}`, or the bounded long-poll variant
of the same shape): a pod whose drv is not yet Ready waits/retries up
to an explicit idle bound and then exits 0 charge-free, preserving the
§13b speculative pod warm-up (a forecast-warmed pod starts building
the moment its drv goes Ready). The spawn-side ready-filter (option
(b)) is NOT adopted. This is the package's non-recommended option and
is recorded as the owner's call. Consequences, as the package already
prices them: the unary signature carries the third protocol state,
priced into §4.2 rows 13/B7 and the re-targeted Model S at 1c'; the
pod side gains the bounded retry loop, whose idle bound (a named
number or tied to `activeDeadlineSeconds`) becomes an owned number of
the Phase-1 plan; Model J/N obligation-table row 6 resolves to the
option-(a) leg — the as-built no-ready-filter publish stands and
Model J is unchanged; and the Phase-1 spec-consequence queue carries
the pull RPC's response-enum entry (the `NotYetReady` outcome) in
place of the OA6(b) `ctrl.nodeclaim.placeable-gate+5` amendment. With
this rider resolved the T-0e.6 unary signatures are frozen in full;
no-go condition 8 is re-evaluated below.

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

### The frozen replacement contract (T-0e.6)

#### The unary signatures and payload reuse (paper freeze; no proto change in Phase 0)

- `PullAssignment(executor_token, intent_id) → WorkAssignment | Gone`
  — leader-served; one transaction: token↔intent binding check
  (`sec.executor.identity-token` semantics moved per-unary),
  Ready→Running transition, `exec_id` mint, `drv_executions` row,
  GC live-input pin, generation fence (commit only at/above the
  durable claims floor); re-pulls return the identical payload and
  `exec_id`; `Gone` = drv no longer wanted, pod exits 0, charge-free.
  Payload reuse: the existing `WorkAssignment` message (drv path,
  ATerm, PutPath assignment token, resources, traceparent); the
  `generation` field is dropped from the contract (kept at most as
  observability — the fence is transaction-side per T-0e.2).
  **OA6 rider:** whether the signature carries a third outcome
  `NotYetReady{retry_after}` (option a) or stays two-outcome with the
  ready-filter at the spawn side (option b) is the one open hole —
  the signature does not finally freeze until the OA6 row resolves
  (no-go condition 8); everything else in this subsection is frozen
  now and is invariant under either OA6 outcome.
- `ReportOutcome(exec_id, CompletionReport) → Ack` — leader-served,
  retried until acked, idempotent by `exec_id` (row-already-terminal
  ⇒ acknowledged-and-ignored); the report payload is today's
  `CompletionReport` unchanged; ack ⇒ pod exits 0; report cannot land
  within the pod's bounded retry budget ⇒ nonzero exit, Job Failed,
  classification via the pod-terminal path.
- `ReportAttemptOutcome(attempt identity, terminal status)` —
  controller→scheduler, the C4/C5 unification: idempotent column fill
  on the existing open attempt row (`WHERE termination_reason IS
  NULL` unchanged); a report for an identity with no attempt row is
  acknowledged and charges nothing (no insert, no budget, no floor
  bump, no establishment); permitted side effects of the no-attempt
  arm: clear the intent's ICE cell, re-arm the spawn intent. The
  controller synthesizes this report whenever it deletes a Job that
  still has an open attempt (cancel/preempt/reap), and a synthesized
  cancelled/preempted/reaped verdict for an open attempt with no
  worker-reported row closes that attempt charge-free at the same
  fold (uncharged terminal row, assignment closed, still-wanted drv
  requeued — never left to the establishment sweep), per AD5 and
  `sched.attempt.synthesized-verdict` (review fix P1, 2026-05-27).

#### The frozen invariant list (what the replacement must preserve)

One row per invariant of the as-built contract: where it is verified
today (the 0c/0d record above), and what the replacement owes it —
the named successor property, the by-construction argument, or the
explicit retire-with-evidence note. "Wired" = a `checks.x86_64-linux`
entry today; "manual" = the documented manual TLC target from the
Stage-B record; calib = the permanent `quint-executor-calib-*`
witness.

| Invariant (family) | Where verified today | What the replacement owes it |
|---|---|---|
| `atMostOneLiveStreamPerExecutor` (F1) | Model S by-construction (hijack guard precondition); wired base + fault-stream-msg cfgs; calib evidence module `executor-f1-hijack-accept` (falsifies when the accept gate is removed); `sec.executor.identity-token+2` unit/VM tests | The session-identity class is structural (no streams). The surviving obligation is identity binding: the per-unary token↔intent check (#6 transformed) plus at most one OPEN attempt per (drv, pod) — `AtMostOneOpenAttemptPerJob` in the re-targeted Model S, `drv_attempts_exec_id_uniq` + the pull transaction's row-exists guard in code |
| `staleStreamEventsAreInert` (F1) | Model S by-construction (I-056a epoch guard); wired cfgs; **wired calib witness** `quint-executor-calib-f1-stale-epoch`; `sched.executor.session-epoch` | Structural (no epochs). The surviving obligation is attribution: a stale pod's report can only fill its own attempt row (`exec_id` keying); a report for a superseded/closed attempt is row-already-terminal ⇒ ignored. Carried by ledger idempotency + re-targeted Model S |
| `registrationRequiresBothHalves` (F1) | Model S (never-create half by construction, I-048b; both-halves dispatch precondition checked); wired cfgs; `sched.executor.dual-register` | Structural: there is no dispatch and no registration; work binds only at a successful pull (the pull IS both halves). The never-create analogue survives as the no-attempt no-op rule (a report never creates state) — checked at 1a by the red-first no-attempt test and in the re-targeted model |
| `claimedSlotResolvesAtMostOnce` (F2) | Model S checked; wired base + fault-stream-msg; fault-leader/persist manual re-runs post-fix; `sched.completion.idempotent`, `sched.executor.repair-precedence` | At-most-once resolution keyed by the durable attempt row: terminal fill exactly once (`WHERE termination_reason IS NULL`), `Gone`/establishment/report all converge on the same row, no second resolution after a terminal row. Owed by ledger idempotency (already built) + `fold_report` Kani contract (no transition out of a terminal row) |
| `unresolvedClaimHasRepairArmed` (F2) | Model S checked (armed-safety); wired cfgs; fault-leader/persist manual re-runs (the 0c defect found here is fixed, `1bbad1ee7`); witnesses noPhantomDrain/noFailoverWithInflight | The armed disjunction re-bases onto: pod alive ⇒ report retry loop armed; pod dead ⇒ Job/pod terminal report path armed; nothing arrives ⇒ establishment sweep armed at deadline+slack (every open attempt is swept — durable, so the 0c "deferred claim forgotten" class is closed structurally). Checked again as the re-targeted Model S armed-safety form at 1c' |
| `noFabricatedCompletion` (F2) | Model S checked; wired cfgs; `sched.executor.repair-precedence` | Unchanged obligation: repair paths (establishment, synthesize-on-delete, store-probe adopt) never invent a worker outcome; the no-attempt rule never creates rows; the store-probe adopt arm survives in the establishment sweep. Verified by the re-targeted model + the 1b fold-input assertions |
| `confirmedPhantomIsDrained` (F2, calibration-added) | Model S by-construction; **wired calib witness** `quint-executor-calib-f2-phantom-drain`; wired cfgs re-verified with the conjunct | The phantom class (scheduler-recorded binding the worker never saw) cannot form: there is no push channel, the binding exists only because the worker pulled it. By-construction in the replacement — stated here explicitly as the retirement evidence for the two-strike mechanism (#10), per the 0d contract input |
| `reportSurvivesStreamChurn` (F2, Model D) | Model D checked; wired base + fault-stream + fault-process cfgs; witnesses noSwapWithReportOwed / noInFlightCellDropped; calib evidence module f2d-eager-swap | The delivery choreography is deleted (1d). The obligation becomes: an unacked report is retried until acked or the pod dies; a died-before-ack pod's outcome arrives via the pod-terminal second installment or establishment. Owed by the report retry loop + AD1 path + the establishment sweep; Model D retires only at 1d with the unary idempotency proofs + chaos VM suite named as carriers |
| `noExitWithReportOwed` (F2, Model D) | Model D checked; wired cfgs; **wired calib witness** `quint-executor-calib-f2d-exit-owed`; `builder.completion.exactly-once-or-death` | Exit 0 is reserved for `Gone` and post-ack; a pod that cannot land its report exits nonzero (Job Failed ⇒ pod-terminal classification). The drain-gate semantics invert under AD5 (SIGTERM = abort + one bounded report attempt), and that inversion is priced in AD5, not silently lost |
| `atMostOnceDelivery` (F2, Model D) | Model D checked; wired cfgs | Idempotent `ReportOutcome` by `exec_id`: duplicates acknowledged-and-ignored; never two different outcomes for one attempt (terminal row wins). Kani `fold_report` target |
| `relayOnlyIntoConfirmed` / `staleAssignmentsRejected` (F2/B5, Model D) | Model D (by construction / checked); wired cfgs; witness noStaleAssignmentRejected | Objectless (no relay, no pushed assignments). The stale-assignment fence's property moves to the transaction-side generation fence (T-0e.2); `StaleAuthorityWritesAreInert` at 1c' is the successor check |
| `noReapWhileFreshInWorkerTime` (F3) | Model S checked; wired cfgs; **wired calib witness** `quint-executor-calib-f3-stall-credit`; witness noReapAfterStall; `sched.executor.liveness-window` | No scheduler-side liveness verdict exists to get wrong: nothing in the scheduler kills or deregisters a working pod on its own clock. The only time-based action left (establishment sweep) acts only on attempts past deadline+report-slack with no terminal row — the analogue obligation "never establish while the worker is still inside its budget" is carried by the OA1-sized window and checked in the re-targeted model |
| `silentSlotReapArmed` (F3) | Model S checked (enabled-implies-fires); wired cfgs; calib evidence module f3-reap-strikes | Real death is detected within a bound: kubelet/Job controller observe the pod, `activeDeadlineSeconds` bounds wedges, the pod-terminal report + establishment sweep close the attempt. The bound is the AD5/OA1 budget — signed when OA1 resolves; the 1b VM gate asserts a no-report death is charged exactly once and only after the window |
| `correlationEntryLifecycle` (F4) | Model S checked; wired cfgs; **wired calib witness** `quint-executor-calib-f4-correlation-entry`; `sched.reassign.no-promote-on-ephemeral-disconnect+4` | The correlation map is deleted; the correlation IS the attempt row keyed by `exec_id` (no TTL race, durable). Owed: the second installment fills only the matching open row; a post-completion death creates nothing (no-attempt rule = the `last_completed` discriminator's successor); first-classifier-wins becomes row-already-terminal |
| `establishmentOnlyAfterWindowCloses` (F4) | Model S checked; wired cfgs; witness noEstablishment; calib evidence module f4-establish-early | Establishment fires only when deadline + report-slack passes with no terminal row, never earlier, and a non-terminal/no-op report does not establish. Same invariant, re-keyed window (OA1 number) anchored to the deadline the attempt was dispatched with (072 `deadline_secs`; the sweep-time re-solve may widen, never shrink — `sched.attempt.establishment-window+2`, review fix P4); checked in the re-targeted model and asserted in the 1b VM gate |
| `neverOfferUnrunnableWork` (F5) | Model S checked; wired cfgs; **wired calib witness** `quint-executor-calib-f5-closed-stream` | There is no offer. The surviving obligation is never-spawn/never-admit-unrunnable: kind/system/feature eligibility and the exclusion set move to the spawn-intent gate (AD2); the closed-stream/capacity/draining clauses are structural (no slot state). Static-eligibility content keeps its NOT-ENCODED status — the placeable()/eligibility unit suite is the binding coverage (0d input), re-stated over (excluded_sources, spawnable_sources) per AD2 |
| `eligibleWorkOfferedWithinBound` (F5) | Model S checked (STARVE_BOUND form); wired cfgs | Bounded progress re-bases onto: Ready intent ⇒ spawn intent emitted (Model J ceiling/headroom + queue, already verified) ⇒ pod pulls or the pull-retry/`activeDeadlineSeconds` policy bounds the wait; starvation-by-bookkeeping has no bookkeeping left to starve on. The freeze-detector observable survives re-keyed to open-attempts/queue age |
| `rollbackRestoresExactly` (F5) | Model S checked; wired cfgs; witness noRollback | No half-recorded push can exist: the pull transaction commits atomically or the pod retries; nothing to roll back. Kani `admit_pull` idempotency + the 1a double-pull red-first test carry it |
| `deposedLeaderSessionEventsAreInert` (F6) | Model S checked; fault-leader manual + wired witnesses (noDeposedBeliever, noAdopt, noFailoverWithInflight); calib evidence module f6-deposed-reassign; `sched.lease.standby-drops-writes` + lease-campaign checks | An observed-deposed replica still mutates nothing (the leader gates survive); the deposed-but-unaware residual is now CLOSED at the transaction fence rather than left to the worker-side latch: `StaleAuthorityWritesAreInert` (1c' re-target) + the `admit_pull` floor check. The lease checklist re-derivation (T-0e.2) is the 1c' deliverable |
| `convergenceToGroundTruth` (F6) | Model S checked (per-action form); wired cfgs; fault-leader manual | Recovery converges from durable state: the ledger fold + the same store-probe adopt-or-reset arm (now inside the establishment sweep); no special-case 45 s timer, no not-yet-heartbeated deferral (the 0c defect class is structurally gone — open attempts are durable and swept every tick). Verified by the re-targeted model + recovery tests |
| F7 obligations (busy-accuracy, ack arming, report idempotency, dead_nodes) | Stated as Model S guarantees (header); `sched.admin.list-executors*`, `sched.sla.hw-class.ice-mask`, `sched.completion.idempotent`, `sched.admin.hung-node-detector+3`; consumed by the wired spawnCoherence/nodeclaimLifecycle checks | The Model J/N obligation table above IS the contract for these: busy = open attempt (bridge then ledger view), ICE clear = first pull / pod-Ready, report idempotency = the fill guard, dead_nodes = OA2's successor. Re-derived and re-run at 1c'/1d |
| F8 input hardening | Unit/bounds tests at the gRPC boundary; `sched.executor.input-bounds+2`, `sec.executor.identity-token+2`, etc. | The same hardening applies to the two new unaries (length bounds, token binding, output-membership on reports); no protocol state — stays test-covered, no model obligation |
| Imported: retry fold (`attemptsChargedOnce`, `verdictMatchesFold`, `failoverPreservesHistory`, atomic append) | retryPolicy.qnt wired regimes (campaign #4, closed) | Keep feeding the fold what §4.3 lists: classified worker-report rows, the pod-terminal second installment, established crashes only after the window, the AD2-re-keyed exclusion/fleet inputs; the pull-mode environment regime (T-0e.3) is the re-derivation vehicle at 1b |
| Imported: lease (`atMostOneCASWinner`, `boundedDualLeadership`, `staleLeaderHasStaleGeneration`) | leaderElection.qnt wired regimes + witnesses (rio-lease campaign) | All remain consumed; the consumer of the generation ordering moves to the transaction fence (T-0e.2); claim-before-advertise's successor is claim-before-serve; nothing becomes unconsumable |
| Imported: Model J (`ceilingRespected`, `reapSafety`, `orphanRemoved`, `gateFailClosed`, `ackSoundness`, `ackCoversPending`, `degradedPolarity`) and Model N producer guarantees | spawnCoherence.qnt / nodeclaimLifecycle.qnt wired regimes + witnesses (controller campaign) | Stay imported as the pod-supply environment; single-Job-per-intent stays an assumed-not-proven environment constraint (unchanged); the scheduler-side obligations they import back are the obligation table above |

#### The per-mechanism disposition table, finalized against the 0d calibration

The design §4.2 dispositions, finalized: every "unnecessary" row now
names the by-construction argument or the successor that carries its
family's invariant, citing the 0d evidence where the calibration
sharpened the claim (the AD6 contract runs in both directions — the
introduced-mechanism list follows the table). Economy-vs-safety
findings from 0d are folded in where they change the row's
justification.

Scheduler mechanisms #1–#22:

| # | Mechanism | Disposition | What carries the invariant (0d evidence) |
|---|---|---|---|
| 1 | Heartbeat-timeout reaper | Unnecessary | Liveness belongs to kubelet/Job + `activeDeadlineSeconds`; F3's no-false-reap half becomes structural (no scheduler liveness verdict), the real-death bound moves to the AD5/OA1 budget |
| 2 | Stall credit | Unnecessary with #1 | The worker-time discipline exists to protect workers from the scheduler's own stalls; with no scheduler-side verdict there is nothing to credit. Calib row `1757790f2` shows what it protected — the obligation transfers to the establishment window being measured from durable timestamps, not actor uptime |
| 3 | Stream-close → disconnect → reassign | Detection unnecessary; requeue survives as the fold's verdict arm | Triggered by terminal classification (report / pod-terminal / establishment) instead of a socket event; charging unchanged (retry-owned) |
| 4 | Stream-epoch stale-disconnect filter | Unnecessary | No epochs. Attribution survives as `exec_id` keying (frozen-invariant row 2). Calib witness f1-stale-epoch stays wired as the as-built regression guard until 1c' |
| 5 | Reconnect stale-flag clear | Unnecessary | No reconnect; flags do not exist. 0d found this row's loss was an eligibility/economy regression, not a safety latch (`3082598a3` ENC-A note) — consistent with deletion |
| 6 | Hijack / intent-mismatch rejection | Survives transformed | Same token↔intent binding check, per-unary at pull/report; `sec.executor.identity-token` unchanged |
| 7 | Unknown-executor heartbeat drop | Unnecessary | Never-create becomes the no-attempt no-op rule (reports never create state) |
| 8 | Heartbeat running-build TOCTOU keep | Unnecessary | No competing in-memory view to reconcile; the attempt row is the single record |
| 9 | Heartbeat adopt | Unnecessary | **0d economy-vs-safety finding:** the `be3ad068e` HOLDS probe shows the adopt arm is economy (re-learn instead of re-run) and execution correlation, not a safety invariant, at session resolution — the deletion is evidence-backed, and the economy loss is priced as: a leader that loses the in-memory binding before the report lands re-runs the build (bounded by the deadline), it never double-charges (first terminal row wins) |
| 10 | Phantom two-strike + drain | Split | The lost-assignment half cannot form (no push channel — `confirmedPhantomIsDrained` is by-construction in the replacement; calib row `0127cf854` is the as-built evidence and its wired witness stays until 1c'); the lost-completion half is carried by the establishment sweep + `ReportAttemptOutcome` (#17's successor), per the design's own mapping |
| 11 | Closed-stream dispatch exclusion (I-095) | Unnecessary | No offer path (calib row `96d8092b8` documents what it protected); never-admit-unrunnable moves to the spawn gate (AD2) |
| 12 | Completion capacity-free hoist + stale-report guard | Survives as ledger idempotency | Already built (retry 1b–2); row-already-terminal is the stale-report guard's successor |
| 13 | One-shot draining + `last_completed` | Unnecessary | One-shot is structural (the pod exits); the `last_completed` discriminator's job (post-completion deaths create no correlation entry) becomes the no-attempt rule — calib row F4/I-197 maps to it directly |
| 14 | `recently_disconnected` map + TTL sweep | Unnecessary as in-memory state | The correlation is the attempt row's two installments (durable, no TTL race); the sweep survives only as the establishment sweep; F4's lifecycle invariants re-key onto the row (frozen-invariant rows 13–14) |
| 15 | Termination-report dedup | Survives as the idempotent fill guard | Already built; the in-memory first-report-wins map goes; obligation-table row 4 |
| 16 | DeadlineExceeded Job-name prefix-match | Unnecessary | The Job name is the attempt key by construction (deterministic name = intent id) |
| 17 | Backstop timeout | Shrinks to the establishment sweep | Open attempt past deadline+slack with no terminal row → store-probe → establish per C2; the cancel/quarantine halves go (the Job deadline already killed the pod) |
| 18 | Dispatch rollback | Unnecessary | The pull transaction commits or the pod retries; no half-recorded state (frozen-invariant row 18) |
| 19 | 45 s post-failover reconcile | Absorbed into normal recovery | The ledger fold + the same store-probe arm; the deferral special-case (the 0c defect's home) is structurally gone — open attempts are durable rows visited by every sweep, not an in-memory claim a one-shot timer can forget |
| 20 | Hung-node detector | Moves to the controller (OA2) | AWAITING OWNER for shape/owner/landing or the signed gap; the as-built detector covers only stream-mode pods during the transition (it goes blind per pool at flip), so retention is not coverage — AD6's carried-by-named-successor clause applies |
| 21 | Leader-transition hygiene | Shrinks | The leader gate stays; the generation-fenced pull/establishment transactions replace the cleared maps (which no longer exist); `StaleAuthorityWritesAreInert` is the 1c' check |
| 22 | `dispatched_cells` sweep + ICE clear | Survives with a renamed trigger | ICE clears on first pull / pod-Ready; the DAG-state sweep is unchanged; obligation-table rows 2–3 |

Builder side: B1 reconnect loop, B2 relay swap, B3 half-close, B4
completion-pending drain gate, B6 slot/draining rejection —
**collapse** into "retry the two unaries until acked" plus the
existing `BuildSlot`; their Model D invariants' successors are the
frozen-invariant rows 8–11 (retry loops, nonzero-exit-on-failure,
idempotent report). B5 generation fence — **replaced**, not deleted
as objectless: the comparison moves to the pull/establishment
transactions (T-0e.2); the worker-side latch is deleted with the
stream at 1c'. B7 idle exit — replaced by the `Gone` exit (and by
the OA6(a) idle bound if that option is taken). B8 — survives
trivially as per-unary timeouts. B9 panic-catcher — survives (a
panic must still produce a report or a nonzero exit). The SIGTERM
drain gate inverts to abort-and-report per AD5.

Controller side: C1 stale/collision/drift reap — survives unchanged.
C2 excess-Pending reap — survives, downgraded from correctness to
economics (an excess pod self-terminates via `Gone`). C3
orphan-Running reap — busy becomes "an open attempt exists" via the
chosen §4.5 bridge (the 1a switch), the leader-age arm retires only
at the cleanup slice, RPC-error/unanswerable arms stay fail-closed
(obligation-table row 1). C4+C5 — unify into `ReportAttemptOutcome`
with the synthesize-on-delete rule. C6 DisruptionTarget — synthesize
the terminal report, then foreground-delete with the AD5 abort
semantics; successor must be live for a pool at its flip.
`GetSpawnIntents` / `MintExecutorTokens` / `AckSpawnedIntents` —
untouched (Model J's subject).

Operator controls: O1 per-executor drain → cordon + AD2 exclusion;
O2 force-evict → cancel verdict + Job deletion under the AD5 budget;
O3 fleet stop → pause spawn intents + bulk cancel; all three retire
their RPC/CLI/dashboard surfaces only at 1c' after the OA5 successor
view is signed off (the OA5 record above).

Mechanisms the replacement INTRODUCES (the other direction of AD6's
contract): the pull retry loop, the report retry loop, the controller
cancel/preempt deletion arm with the synthesized terminal report, the
`ReportAttemptOutcome` ingestion path, the establishment sweep, the
transaction-side generation fence, the controller-side node-wedge
signal (OA2), and — during coexistence only — the §4.5 busy-signal
bridge. Each enters the Phase-1 plan with its own red-first tests and
(for the fence and the establishment sweep) its 1c' model coverage.

K8s-native carriers named (rows that delete a mechanism because the
platform already provides it): kubelet/Job-controller pod liveness
(#1/#3 detection), `activeDeadlineSeconds` (#17's wall-clock half,
wedge bound), the deterministic Job name + 409 dedupe (#16, spawn
identity), foreground deletion + TTL (#13's cleanup half, C2),
anti-affinity rendering (AD2's exclusion carrier).

#### AD1–AD6 at 0e

| AD | 0e status |
|---|---|
| AD1 (controller stays the pod-terminal observer) | Confirmed against the 0a–0d record; nothing in calibration contradicts it; the no-attempt no-op rule is its load-bearing companion. Signed with the go |
| AD2 (exclusion re-keys to the controller-authoritative node; exhaustion survives re-keyed) | Confirmed; the 0d F5 re-dispositions make the placeable()/eligibility unit suite the binding coverage for the eligibility *content* (no model backstop), and migration ≥067's source column is the durability requirement. The Kani/placeable re-statement is Phase-1 work. Signed with the go |
| AD3 (pull = Ready→Running, leader-served, idempotent) | Confirmed; the fence rider from T-0e.2 (claims-floor check) is part of the same transaction. Signed with the go |
| AD4 (no periodic worker→scheduler channel; fence moves transaction-side) | Confirmed; the lease-seam note above is the evidence no guarantee becomes unconsumable. Signed with the go |
| AD5 (cancel/preempt = synthesized report + Job deletion; SIGTERM = abort; composite latency budget) | Structure confirmed (components: scheduler-verdict→controller-observation latency + one tick + deletion propagation + min(grace, abort-and-report bound)); **the numeric budget is NOT signed at this cut** — it requires the OA1 baseline (AWAITING OWNER). The cancel-timing/preemption-timing VM re-baselines stay scheduled at 1b |
| AD6 (the disposition table is the deletion-wave contract, both directions) | The finalized table above IS the artifact; signed with the go |

#### The coexistence invariant (1a–1d)

Throughout 1a–1d every controller decision that acts on executor pods
(orphan-Running reap C3, DisruptionTarget preemption C6) MUST be
answerable for both protocols: busy = stream `running_build` OR an
open pull-mode attempt; preemption = force-drain for stream pods,
synthesized-report + Job deletion for pull-mode pods. A pool template
may not flip before the consumers it depends on are rewired — this is
what orders the C3 busy-source change (the §4.5 bridge) into 1a and
the C6 successor into 1b. Mixed-era accounting: attempt rows do not
change shape; the only mixed-era surface is the exclusion key
(pod-name entries from old attempts, node entries from new ones), and
AD2 carries both keys in the exclusion set during the transition,
dropping the pod-name key with the old protocol.

#### Controller-campaign ordering, re-confirmed at 0e

Status at this cut (re-checked in `controller-invariant-map.md`): its
Phase-0 exit-gate verdict is "Met"; **no Phase-1 record or close-out
exists**, so that campaign is still mid-campaign and a 0e go fires
its re-pin clause ("executor-lifecycle replacement green-lit
mid-campaign") in full:

- **F1/F3 prerequisite review (performed now, recorded here).** The
  controller map's F1 rows (SingleJobPerIntent, CeilingRespected,
  ReapSafety, OrphanBounded, TickOrdering) and F3 rows (AckSoundness,
  IceMarkSignalSoundness, NoMassClearAfterFailover) were read against
  the replacement's peer behavior. Findings: SingleJobPerIntent,
  CeilingRespected, TickOrdering, NoMassClearAfterFailover are
  untouched (spawn path unchanged). ReapSafety/OrphanBounded are
  touched exactly where the obligation table's row 1 says (the busy
  source changes; the documented busy-but-never-registered residual
  becomes busy-but-never-pulled and is bounded by ORPHAN_REAP_GRACE
  re-validation, work item (ii)). AckSoundness is untouched on the
  arming side; only the clear trigger renames (row 2).
  IceMarkSignalSoundness's scheduler-side clear ladder gains the
  first-pull trigger (row 2). No controller-map row is invalidated by
  the paper contract; the affected rows are exactly the ones already
  scheduled for re-audit at 1b/1d.
- **Stage-B re-check with the heartbeat-authority assumptions
  removed:** carried by the planned 1c'/1d Model J/N checklist
  re-derivation and check re-runs (obligation table + work items
  (i)–(iii)), not re-scheduled separately.
- **Stage-C calibration-table delta pass** on the controller map:
  added to the Phase-1 input list below as a 1b/1d deliverable signed
  by that campaign's owner.

### The go/no-go evaluation (T-0e.7)

Every design §6 no-go condition (nine bullets) plus the plan's
regime-coverage input (item 10), each with a verdict and its evidence
pointer. Verdict vocabulary: **clear** — the condition is not
triggered by the 0a–0e record; **conditional** — the condition's
discharge depends on a named AWAITING-OWNER row (it clears if that
row resolves either of its prepared ways, and stands as a no-go only
if the row is left unresolved); **triggered** — the condition holds
and the replacement design must be redrawn (none below). Per the
design, the verification artifacts (0b–0d) stand regardless of the
outcome.

| # | No-go condition (design §6) | Verdict | Evidence |
|---|---|---|---|
| 1 | A calibration family protected only by a deleted mechanism with no named successor and no by-construction argument | **Clear** | The finalized disposition table × the 0d calibration table: every "unnecessary" row names its carrier (frozen-invariant list + per-mechanism table above); the two rows the calibration sharpened (#9 economy-not-safety, #10 by-construction `confirmedPhantomIsDrained`) are cited in-row |
| 2 | One pod death cannot be reduced to ≤1 charged attempt from post-replacement signals alone | **Clear** (paper re-derivation; mechanical re-check at 1b/1c') | The two-installment shape survives keyed by `exec_id` (durable, no TTL race); the no-attempt rule prevents charging never-pulled pods; `attemptsChargedOnce` stays the imported oracle; the §4.1 transaction fence closes the two-believer double-pull race; F4's calibration rows map onto the re-keyed lifecycle (frozen-invariant rows 13–14) |
| 3 | AD2 re-keying cannot preserve `sched.retry.per-executor-budget`'s intent (incl. small-fleet and exhausted-universe clauses) | **Clear** | AD2's three-part contract is confirmed at the freeze (exhaustion survives re-keyed and relocated to the spawn gate; small-fleet clause min(threshold, |sources|); durable source attribution via migration ≥067); the 0d F5 re-dispositions name the placeable()/eligibility unit suite as the binding coverage for the eligibility content; the exhausted-universe verdict stays a structural poison, never deadline churn |
| 4 | The OA1 latency model shows death→requeue or cancel latency past the signed budget | **Clear** (re-evaluated 2026-05-27) | No baseline and no signed budget exist at this cut, so the condition can be neither triggered nor discharged; the AD5 component structure shows no structural blowout (every component is tick-, propagation- or grace-bounded), but the design requires the measured baseline. Clears when OA1 resolves and the budget is signed against it. Re-evaluation 2026-05-27: OA1 is decided (option A authorized, instrument landed); no measured data shows a latency past a budget, the structural analysis stands, and the AD5 numeric budget is signed against the accumulating baseline and re-baselined from the same instrument at the 1b gate — the condition is not triggered |
| 5 | The OA1 baseline cannot be obtained | **Clear** (re-evaluated 2026-05-27) | Today the interval-(i) baseline does not exist and cannot be reconstructed retroactively (the 0a source audit); it becomes obtainable the moment the owner authorizes the instrument (option A) or signs the weaker-population basis (option B). Stands as a no-go only if OA1 is left unresolved. Re-evaluation 2026-05-27: option A is authorized and the instrument is landed, so the baseline is obtainable from instrument availability forward against the 0a population statement — discharged |
| 6 | The Model J/N obligation table marks any imported obligation unmeetable | **Clear** (no unmeetable row), with one row conditional | Obligation table above: 5 of 7 rows meetable with named successors; rows 5 (dead_nodes/OA2) and 6 (placeable input/OA6) are meetable under either prepared resolution of their owner rows; none is unmeetable under any prepared option |
| 7 | The dual-belief residual has no checked successor / a lease guarantee becomes unconsumable | **Clear** | T-0e.2: the transaction-side fence + `StaleAuthorityWritesAreInert` (1c' re-target) + the `admit_pull` Kani contract are the named checked successors; all four leaderElection.qnt exports remain consumed; claim-before-advertise's successor (claim-before-serve) is named |
| 8 | OA6 unadjudicated — no committed pull outcome for the not-yet-Ready case | **Clear** (re-evaluated 2026-05-27) | The decision package (both options, costs, decision rule, recommendation) is prepared and AWAITING OWNER; the unary signatures are frozen except for this rider. Clears with either option; stands only if unresolved. Re-evaluation 2026-05-27: OA6 is adjudicated — option (a), so the committed pull outcome for the not-yet-Ready case is `NotYetReady{retry_after}` and the T-0e.6 signature freeze completes with it |
| 9 | OA2 unresolved — no signed-off node-health successor and no recorded accepted gap | **Clear** (re-evaluated 2026-05-27) | The decision package (three options incl. the signed-gap shape with bound and compensating controls) is prepared and AWAITING OWNER. Clears with any of the three; stands only if unresolved. Re-evaluation 2026-05-27: OA2 is resolved — the controller-side clustering successor is committed with a landing slot ≤1c and the 1b-canary-window interim gap is signed with option C's bound and controls |
| 10 | (plan addition) Regime-coverage input: families whose only exhaustive fault-regime coverage was demoted out of `checks.*` or dropped to representative-revert-only | **Signed — accepted** (2026-05-27) | More than two families are affected, so the plan requires the campaign owner's explicit signed acceptance naming the gap and its compensating coverage; absent that signature this item is treated as a no-go for Phase-1 planning. Signed 2026-05-27: the campaign owner accepts the statement prepared below exactly as written — the affected slices keep the manual exhaustive targets plus the wired witnesses / deep-simulation / calibration / VM coverage in the gate, with the exhaustive cfgs runnable on demand — and the same signature disposes of the still-open 0c budget stop-and-report it folds in |

The item-10 statement prepared for the owner's signature (it is the
same adjudication as the still-open 0c budget stop-and-report, not a
new one):

- **The gap.** The exhaustive cfgs for fault-stream-conn and
  fault-process were never wired (budget non-convergence at the
  witness-preserving bounds; the deep-simulation runs are recorded but
  are not exhaustive verdicts), and the fault-leader / fault-persist
  cfgs are documented manual targets whose post-fix bounded-exhaustive
  re-runs were stopped over the gate budget after clearing well past
  the falsifying trace class's depth. The node regime was not
  attempted (pre-registered fallback). Families whose contended fault
  class therefore has no completed exhaustive verdict anywhere: F2's
  pod-death/failover arms, F3's death channel, F4, F6 (F1/F5 and
  Model D's families have wired exhaustive coverage of their
  contended regimes; F2's stream-message and base arms are wired).
- **The compensating coverage on record:** every witness for the
  affected regimes is wired and violating in `checks.*` (the contended
  states stay pinned reachable); the Stage-B deep-simulation evidence
  and the fault-leader/persist bounded re-runs; the 0d
  first-counterexample calibration runs at those regime constants
  (F2 phantom/no-adopt, F4's two falsifying hand-off overrides, F6
  deposed-reassign) plus the six wired `quint-executor-calib-*`
  regression guards; the named VM scenarios (`chaos`,
  `lifecycle/recovery`, `ephemeral-pool`, `reassign`) and the
  detector/establishment unit tests; and the lease campaign's wired
  checks for the failover machinery Model S imports.
- **The owner outcomes available** (from the 0c stop-and-report,
  unchanged): accept multi-ten-minute checks and wire the cfgs;
  authorize a further per-fault-class split; authorize a coarser
  re-encoding; or accept representative-revert-only calibration for
  the affected slices and sign exactly this statement as the item-10
  acceptance.

**Item-10 signature (2026-05-27):** the campaign owner takes the
fourth outcome and signs the statement above as the acceptance,
exactly as prepared — the gap is accepted with the named compensating
coverage standing in the gate (wired witnesses, deep-simulation
record, calibration runs and their wired regression guards, the named
VM scenarios, the lease-campaign checks), the fault-leader /
fault-persist exhaustive cfgs stay documented manual targets runnable
on demand, and no further per-fault-class split or re-encoding is
commissioned for Phase 0. This signature is also the disposition of
the still-open 0c budget stop-and-report (the same adjudication, as
the statement notes).

**Overall verdict at this cut: no condition is triggered — nothing in
the 0a–0e record indicates the replacement design must be redrawn —
but the gate cannot be signed "go" yet, because four owner items
remain open: OA1 (conditions 4 and 5), OA2 (condition 9), OA6
(condition 8), and the item-10 regime-coverage acceptance. The 0e
record is therefore a conditional close: the contract above is frozen
and Phase-1 planning inputs are complete, and the go signature waits
on those four decisions; if any of them is resolved "no" (OA1 refused
with no signed alternative, OA2 left unresolved, OA6 left
unadjudicated, or the item-10 acceptance withheld), the corresponding
condition stands and the design is redrawn before Phase 1 per the
design's own rule.**

**Re-evaluation after the owner decisions (2026-05-27): the Phase-0
exit gate is MET and Phase 0 is complete.** OA1 (option A authorized,
instrument landed), OA2 (controller-side clustering ≤1c plus the
signed 1b-canary-window interim), OA6 (option (a), the `NotYetReady`
third outcome), and the item-10 regime-coverage acceptance are decided
and recorded above, so conditions 4, 5, 8 and 9 read **Clear** and
condition 10 reads **Signed — accepted**; with conditions 1–3 and 6–7
already clear, no condition is conditional and none is triggered. The
go signature this section was waiting on is constituted by those four
recorded decisions, the contract above is the frozen Phase-1 input,
and Phase-1 planning is unblocked. One AD5 work item deliberately
remains open and is carried into Phase 1 without gating this closure:
the AD5 numeric budget is signed against the OA1 instrument's
accumulating baseline and re-baselined from that same instrument at
the 1b gate (the AD5 row's "structure confirmed, number unsigned"
status is unchanged until then).

### Phase-1 input list (T-0e.8)

Everything the Phase-1 planner needs, in one place. The Phase-1 plan
is written as a separate document against this contract only after
the owner resolves the four open items above.

1. **The frozen contract** (this Stage-0e section): the unary
   signatures and payload reuse (with the OA6 rider), the frozen
   invariant list (where verified today / what the replacement owes),
   the finalized per-mechanism disposition table with the
   introduced-mechanism list and k8s-native carriers, the coexistence
   invariant, AD1–AD6 status (AD5 unsigned pending OA1), and the §4.5
   choices: bridge = ledger-backed open-attempt query in
   `reap_orphan_running` (lands 1a, verified 1b), rollback posture =
   revert-clean deletion-wave commits kept rebased to the Phase-2
   close-out, deletion-gate observable = stream-registration gauge at
   zero for the computed deadline horizon.
2. **The owner-decision queue: resolved, decisions inline
   (2026-05-27).** OA1 = option A (the histogram pair authorized and
   landed; the AD5 numeric budget is signed against its accumulating
   baseline and re-baselined at the 1b gate); OA2 = controller-side
   deadline/pull-latency clustering landing no later than 1c, with the
   signed interim gap limited to the 1b canary window under option
   C's controls; OA6 = option (a), the `NotYetReady{retry_after}`
   third pull outcome (spawn-side ready-filter not adopted); the
   item-10 regime-coverage statement is signed as the acceptance,
   which also disposes of the 0c budget stop-and-report it folds
   into. The DECIDED blocks in the adjudication subsection above are
   the record; nothing in this queue blocks Phase-1 planning.
3. **The interface re-derivation plans:** the Model J/N obligation
   table with re-derivation work items (i)–(iii) (1c'/1d); the
   lease-seam note with `StaleAuthorityWritesAreInert` and
   claim-before-serve (1c'); the retryPolicy.qnt pull-mode environment
   plan (lands 1b).
4. **The calibration record to build the acceptance table against:**
   the 0d calibration table (50 in-family rows, the hand-off rows,
   the cross-campaign links), the six wired `quint-executor-calib-*`
   witnesses and six evidence modules, and the Phase-1/0e-contract
   inputs subsection (confirmedPhantomIsDrained retirement statement,
   the be3ad068e economy finding, the 0ea9bd701 lease dependency, the
   F5 unit-suite binding coverage).
5. **The Stage-B record:** both models, their bounds, the verdict
   table (wired cfgs vs manual targets), the witness set, the
   falsification adjudication and its production fix (`1bbad1ee7`),
   and the open budget stop-and-report — including which regimes the
   1c' re-target must re-cover (a fault-leader regime is retained by
   design §4.4) and which retire with the stream path.
6. **The OA carry list with owners:** OA3 (one-pull default stands;
   data owner: campaign owner), OA4 (BuildPhase; dashboard owner),
   OA5 (surface + owner recorded; operator sign-off due at the 1b
   gate; old surfaces deleted only after sign-off at 1c').
7. **The OA1 instrument decision** (whichever option the owner takes)
   and the environment/population statement it carries, so the 1a
   handlers emit the same instrument and the 1b/1c gates read the
   same source.
8. **Spec consequences queued for Phase 1** (none executed in Phase
   0): the C2 deregister-rule epoch qualification, AD2's
   `sched.retry.per-executor-budget+2` re-key and
   `sched.dispatch.fleet-exhaust+3` re-statement, AD4's
   `sched.lease.generation-fence+2` amendment and
   claim-before-advertise successor, AD5's `ctrl.pod.tgps-default` /
   cancel-path / drain-rule amendments, OA6(b)'s
   `ctrl.nodeclaim.placeable-gate+5` amendment if that option is
   taken, the C3/C4 stale-prose fixes, and retiring/re-pointing the
   five 0b rules' verify markers when the checks they cite are
   re-targeted at 1c'.
9. **Migration ≥067** (AD2c): the source-node column on the attempt
   row, written from the controller-authoritative binding
   (`AckSpawnedIntents.bound_intents`) and/or `ReportAttemptOutcome`;
   never an edit to a shipped migration.
10. **The check-budget envelope:** 28 executor checks wired today (5
    exhaustive cfgs, 17 witnesses, 6 calibration witnesses); the
    unwired exhaustive cfgs and their owner adjudication; what 1c'
    re-targets (Model S), what 1d retires (Model D, the as-built
    retry-channel regimes) and what replaces them (pull-mode regimes,
    `StaleAuthorityWritesAreInert`, the Kani kernels).
11. **The controller-map re-validation owed because this campaign is
    green-lit mid-campaign** (carried forward explicitly, signed by
    that campaign's owner): the re-audit of the affected sections
    (J11, the orphan-reap rows, F1/F3, the I12 out-of-model entry) at
    this campaign's 1b and 1d; the delta pass on its Stage-C
    calibration table at the same slices; the F1/F3 prerequisite
    review already performed and recorded above; the Stage-B re-check
    with heartbeat-authority assumptions removed, carried by the
    1c'/1d Model J/N re-derivation rather than re-scheduled.
12. **The Phase-1 gate skeleton:** design §6 rows 1a–1d/2 with the
    0e-fixed items slotted in (the 1a bridge + red-first idempotency
    and orphan-reap tests, the 1b canary gate's fold-input assertions
    and AD5 re-baselined VM scenarios, the 1c deletion-gate
    observable, the 1c' deletion-wave gate items (a)–(d), the 1d
    cleanup and Model D retirement, the Phase-2 acceptance table and
    close-out honesty contract).

## Phase-1a record (Slice 1a, additive — Waves 1a-A and 1a-B)

Recorded at the Wave-1a-B landing. Everything in this slice is
additive and dormant in production: the stream path is untouched, no
pool template changes, the builder's `dispatch_mode` Config flag
defaults to `stream`, and every pull-only consumer keys on the
`drv_executions.dispatch_mode = 'pull'` discriminator that only the
fenced pull transaction writes. Under the 2026-05-27 directive there
is no development-time deploy: the additive production rollout and the
OA1-baseline accumulation against the new handlers are deployment-time
validation checklist rows D0/D1 of the Phase-1 plan, not part of this
record.

**What landed.**

- Wave 1a-A (scheduler/proto/migration): the four unaries
  (`PullAssignment`, `ReportOutcome`, `ReportAttemptOutcome`,
  `ListOpenAttempts`) with the new spec rules; migration 071
  (`source_node` on `drv_attempts`/`drv_executions`,
  `drv_executions.dispatch_mode`); the ledger-backed open pull-attempt
  view and the `rio_scheduler_open_attempts` gauge; the fenced pull
  transaction with the pure `admit_pull` kernel; the idempotent
  `ReportOutcome` intake; the `ReportAttemptOutcome` second-installment
  fill with the no-attempt charge-free rule; the establishment sweep
  for open pull-mode attempts and the `establishment_report_slack`
  Config field.
- Wave 1a-B (controller/builder/VM/process): the §4.5 option-(b) busy
  bridge (`reap_orphan_running` consults `ListOpenAttempts` alongside
  `ListExecutors`, fail-closed on either source); the
  synthesize-on-delete arm (every controller Job-delete call site
  routes through `delete_job_with_synthesized_report`; reason `reaped`
  at this slice); the builder pull-mode client behind
  `dispatch_mode = pull` (no registration/heartbeat/stream; the OA6
  bounded NotYetReady retry reusing `idle_timeout`; exit codes per
  `builder.pull.exit-codes`); the `pull-mode` VM subtest in
  `vm-lifecycle-autoscale-k3s`; the OA2 interim controls (the
  `RioSchedulerAttemptEstablishmentCluster` alert and the
  `docs/ops/hung-node-manual-reap.typ` runbook); this bookkeeping.

**Obligation-table rows now satisfied additively** (the table's
dispositions are unchanged; this records that the successor signals
exist in the tree):

- Row 1 (busy view): the bridge is landed — the orphan-Running reap
  treats a Job as busy when either the stream view or an open
  pull-mode attempt covers it, with the fail-closed posture spanning
  both reads; the leader-age arm is retained as-is (its retirement
  stays a 1c'/1d item).
- Row 4 (termination-report idempotency): the idempotent
  `ReportAttemptOutcome` column fill is live (first classifier wins,
  `WHERE termination_reason IS NULL`), with the no-attempt no-op rule
  red-first tested; `ReportExecutorTermination` keeps serving
  stream-mode pools unchanged (delta 13 of the Phase-1 plan).
- Row 7 (cancel/preempt read): the open-attempt read exists
  (`ListOpenAttempts`, pull-filtered server-side) and is already the
  busy-bridge input; the cancel-arm consumer lands at 1b (T-1b.4).

**OA6 consequence-list status** (the decision block above prices four
consequences; their execution state at 1a):

1. Protocol enum — DONE: `PullAssignmentResponse` carries the
   `NotYetReady{retry_after_seconds}` outcome (T-1a.1) and the
   scheduler returns it for wanted-but-not-deliverable pulls,
   including the open-on-another-executor coexistence arm (T-1a.4).
2. Pod-side bounded retry — DONE: the builder re-pulls after the
   suggested `retry_after` (±20 % jitter) and exits 0 charge-free
   after receiving only `NotYetReady` for `idle_timeout` (T-1a.10;
   idle bound reuses I-116 per Phase-1 plan delta 5 — no new number).
3. Spec-consequence rule — DONE: `sched.executor.pull-not-ready`
   carries the NotYetReady semantics (T-1a.1).
4. Model-S pricing — PENDING until 1c' (T-1c'.5): the re-targeted
   model carries the NotYetReady wait state, its inertness invariant,
   and the reachability witnesses; nothing to re-run at 1a.

**Establishment persist-clause discharge (T-1a.7 step 2).** The
lease-seam note above says the pull and establishment transactions
"persist [the serving generation] on the row they create". As built:
the establishment transaction runs the same claims-floor check as the
pull transaction and closes/updates the `assignments` row that the
fenced pull minted — that row already persists the binding generation
— and the floor itself is computed from `leader_generation_claims` +
`assignments` (the existing `max_known_generation` arms). No
`drv_attempts.generation` column exists or is needed for the clause;
the 1c' lease-checklist re-derivation (T-1c'.7) should read the clause
against the assignments-row carrier, not a per-attempt-row column.

**Carried to 1b (recorded at the 1a-A integration, restated here so
the map is self-contained):** the post-failover reconcile
(`collect_orphaned_assignments`) must learn the
`dispatch_mode = 'pull'` discriminator before any pull-mode traffic
exists (the establishment sweep, not the orphan reconcile, owns pull
attempts); `drv_attempts.source_node` stamping is deferred to T-1b.1;
`OpenAttempt.deadline_secs` returns 0 at this slice (the sweep
computes the deadline from the live solver); `ListOpenAttempts` is
service-token-gated (controller/cli/dashboard callers).

**Model bookkeeping.** `spawnCoherence.qnt` and `executorSession.qnt`
gained coexistence/busy-view header notes only (assumption text; no
transition change); the affected wired checks were re-run at this
landing with bit-identical state counts (recorded in the landing
commit message per the volatile-figures convention).

## Phase-1b record (Slice 1b — verification batch, model/map half)

Recorded by the 1b verification batch (Phase-1 plan T-1b.10) against
the tree carrying the 1a slice and the 1b code batch
(T-1b.1–T-1b.6). Under the 2026-05-27 directive the 1b gate is
verification-only: no pool template flips, no canary deployment, no
soak; the deployment-time validation checklist rows D0–D5 hold every
formerly-gating production observation.

**Obligation-table rows (status at 1b).**

- Row 1 (busy view): unchanged from the 1a record — the option-(b)
  bridge is the OR of the stream view and the durable open pull-mode
  attempt view, fail-closed on either read. The busy-but-never-
  registered residual stays closed for pull-mode pods, documented for
  stream pods until 1c'.
- Rows 2/3 (ICE arming and the DAG-state sweep): untouched by 1b; the
  ICE-clear re-trigger (T-1b.5) moves only the clear edge for
  pull-mode intents to the first successful pull (registration-edge
  clear retained for stream executors), with the red-first ICE
  batteries green.
- Row 4 (termination-report idempotency): the controller report paths
  now speak `ReportAttemptOutcome` (T-1b.3, C4/C5 unification);
  stream-mode identities route through the same internal path as
  before (bit-identical classification), pull-mode attempts get the
  reason-only second-installment fill (`WHERE termination_reason IS
  NULL`); re-report dedup demonstrated against the scheduler handler
  in the unit batteries.
- Row 6 (exclusion re-key): AD2 landed on both halves — source_node
  stamping + node-keyed exclusion + mixed-era both-keys carry
  (T-1b.1), anti-affinity rendering + NoEligibleSource at the spawn
  gate (T-1b.2) — with the small-fleet clause encoded and the spec
  rules bumped (`sched.retry.per-executor-budget+3`,
  `sched.dispatch.fleet-exhaust+4`).
- Row 7 (cancel/preempt read): the consumer landed (T-1b.4) — the AD5
  cancel arm keys on positive closed-edge evidence from
  `ListOpenAttempts` (never bare absence), and the DisruptionTarget
  watcher's pull-mode branch synthesizes `preempted` + foreground Job
  delete with no `DrainExecutor` hop. No extra signal beyond the
  T-0e.6 surface was needed (obligation row 7's wording holds).

**Model bookkeeping (the 1b re-runs).** `spawnCoherence.qnt`,
`nodeclaimLifecycle.qnt` and `executorSession.qnt` are byte-unchanged
at this slice (the busy-view/coexistence header notes were added at
the 1a landing); every wired Model J and Model N exhaustive check was
re-confirmed green at this tree with state counts bit-identical to
the recorded baselines (figures in the recording commit's message and
the check transcripts). The retryPolicy.qnt pull-mode environment
regime (T-0e.3, executed as T-1b.7) is recorded in
`retry-invariant-map.md`'s cross-campaign addendum: one new wired
exhaustive regime (`quint-retry-policy-pull`, all imported invariants
HOLD) plus five wired expect-violation witnesses, with the as-built
regimes' counts unchanged. The controller-map re-audit entry this
slice owes is recorded in `controller-invariant-map.md`
("Executor-campaign 1b re-audit"), owner counter-signature pending at
the close-out review.

**OA2 (restated per the v3 amendment).** The interim gap is a
development-time code-ordering artifact only: the controller-side
deadline/pull-latency clustering aggregation is absent from the tree
between the 1b landing and T-1c.1, with zero production exposure (no
deployment happens during development); it is closed by T-1c.1
landing no later than 1c. The option-C compensating controls (the
T-1a.12 per-node establishment-cluster alert + manual-reap runbook,
the AD2 node-keyed exclusion, Karpenter NodeRepair) are in the tree;
their live observation is deployment-time checklist row D3.

**OA6 consequences.** Items 1–3 (protocol enum, pod-side bounded
retry reusing `idle_timeout`, the `sched.executor.pull-not-ready`
rule) remain DONE as recorded at 1a; the model half now additionally
covers the charge-free nature of the not-deliverable answer (the
pull regime's NotYetReady/Gone reads are charge-free stutters, and
the no-attempt no-op witness is wired). Item 4 (Model-S pricing of
the NotYetReady wait state and its inertness invariant) remains
PENDING until 1c' (T-1c'.5). The end-to-end VM exercise of the
NotYetReady retry loop (planned as canary-scenario step 7) has NOT
been executed in this batch — see the Phase-1b evidence table.

**OA5 surface notes.** The operator surface for the 1b review is
`ListOpenAttempts` (service-token-gated; pull-filtered server-side)
plus the `rio_scheduler_open_attempts` gauge and the existing Job
census, as demonstrated by the `pull-mode` VM subtest and the
admin-surface unit tests; no rio-cli/dashboard code was added this
slice (the thin `list-open-attempts` subcommand remains an
owner-review ask to record if requested). The OA5 surface review and
the OA4 (BuildPhase) call are owner actions at the 1b close-out
review; the live-fleet OA5 confirmation is deployment-time checklist
row D7.

**AD5 / OA1 deferrals.** The AD5 numeric cancel/preempt budget
remains UNSIGNED (deployment-time checklist row D1, signed against
the OA1 baseline once it accumulates); the OA1 latency comparison and
the establishment-slack re-baseline are rows D1/D2; the production
rollback drill is row D5. Development-time stand-ins shipped: the AD5
component structure, the 45 s pull-mode TGPS (P8/delta 9), and the
OA1 emission paths exercised by tests. The T-1b.9 VM-topology
re-baseline is now recorded from the `vm-pull-canary-k3s` scenario
(executor follow-up, single KVM run, test script ~700 s; VM-topology
numbers only, never the production budget): CancelBuild verdict →
Job/pod/build-cgroup gone, attempt closed, drv Cancelled in 9.9 s;
DisruptionTarget patch → pod+Job gone and the attempt closed at the
report fold in 64.1 s; both asserted against the scenario's 90 s
composite bound and structurally below the establishment window. The
unreported-death establishment charge landed at attempt age 307 s
against the fixture's 180 s solved deadline + 120 s report slack
(the scenario asserts the slack as the universal floor and that no
charge lands inside the window). The composite's component figures
remain the constants the plan names — 10 s controller reconcile tick
(`JOB_REQUEUE`), 45 s pull-mode termination grace
(`PULL_MODE_TGPS_SECS`), 10 s SIGTERM best-effort report timeout —
all unit-covered; no production claim is made from any of these
numbers.

**Code-review pass over the 1a+1b pull-path code (T-1b.11) — review
pass recorded + fixes landed (2026-05-27).** The three-reviewer
adversarial pass (protocol/fence, lifecycle/coexistence,
builder/operability) over the 1a+1b pull-path code is recorded in the
campaign workspace (`pull-path-review.md`): 7 confirmed findings (P1
blocking, P2–P7 important), 1 refuted (the "SIGTERM-abort charged as
infra is a blocking contract violation" claim — refuted as *blocking*
against the then-frozen text, but the charge class itself was adopted
with P1 below), and 6 minors. Every confirmed finding was fixed
(red-first where behavior changed) in the pre-cutover hardening batch
on this branch; dispositions:

- **P1 (blocking) + the AD5 abort charge class — fixed.**
  Controller-synthesized cancelled/preempted/reaped verdicts for an
  open, never-worker-reported pull attempt now close it charge-free
  in one fenced appending transaction (uncharged `disconnected`
  terminal row, assignment closed) and requeue the still-wanted
  derivation at that fold — never the establishment sweep; the
  builder's SIGTERM-abort report of still-wanted work resolves the
  same way instead of an infrastructure charge. Genuinely-cancelled
  (no-longer-wanted) work keeps the cancel arm's exact shape. New
  rule `sched.attempt.synthesized-verdict`; the ReportAttemptOutcome
  bullet in the frozen contract above now names the close.
- **P2 — fixed (smallest AD2c-consistent shape).**
  `ReportAttemptOutcome` backfills the open execution row's
  `source_node` (NULL-only) for pull attempts, and the establishment
  charge falls back to the in-memory spawn-ack binding at sweep time,
  so the AD2 node key on establishment charges no longer depends on
  winning the mint-time binding race; a bounded-poison battery proves
  the crash→establish→respawn loop reaches Poison(Threshold) under
  node keys. Adjudication: the review's ack-time execution-row
  backfill (item 2) was NOT taken — the ack handler is synchronous
  and the two writers above already restore the gate property; the
  residual is cosmetic (ListOpenAttempts node display when the
  pod-terminal report is lost), accepted here.
- **P3 — fixed on both sides.** The controller populates `intent_id`
  only for pull-mode pods/Jobs (per-pod `RIO_DISPATCH_MODE`
  discriminator), and the scheduler's unified intake refuses to
  resolve an intent-keyed report against an open pull attempt when
  the reporting job/pod name is a known stream identity — a stream
  pod's report can never be swallowed by another pod's open pull
  attempt during coexistence.
- **P4 — fixed.** Migration 072 persists the dispatched deadline on
  the execution row; the establishment window is now
  `max(persisted, re-solved) + slack`
  (`sched.attempt.establishment-window+2`) so it can widen but never
  shrink while the attempt is open.
- **P5 — fixed.** The AD5 cancel-arm evidence map is keyed and pruned
  per `{ns}/{pool}/` scope, so co-namespaced pull pools no longer
  erase each other's closed→active evidence (dash-prefixed pool names
  covered).
- **P6 — fixed.** Permanent pull/report rejections (Unauthenticated,
  PermissionDenied, Unimplemented, InvalidArgument) terminate the
  builder loops promptly with a nonzero exit and warn/error logs
  (`builder.pull.retry-loop+2`); `rio_scheduler_pull_rejected_total`
  added for alertability.
- **P7 — fixed.** The production pull transport presents the executor
  identity token as call metadata on both unaries
  (`AuthedPullTransport`); scheduler-side wire tests pin the enforced
  HMAC posture (report without header rejected, with header accepted,
  body-only pull accepted). Adjudication: the VM-level HMAC-enabled
  pull/report arm is NOT added in this batch — the posture is pinned
  at the gRPC wire level instead, and the VM arm stays on the 1c-prep
  list with the existing canary carve-outs.
- **Minors.** Taken: the never-pulled ICE-clear ordering in the
  no-attempt arm; the OA2 interim alert re-keyed to a dedicated
  summed pull-establishment counter
  (`rio_scheduler_pull_establishments_total`); the report-loop
  SIGTERM race hardening (in-flight RPC raced against shutdown).
  Adjudicated, not taken: the pull-mode relaxation of the
  orphan-reap empty-list gate (coexistence posture unchanged this
  slice — the all-pull-fleet C3 source is already a named 1c gate
  item); the controller synthesize-on-delete comments needed no
  change (P1 makes them true as written).

Scenario reconciliation: the `pull-mode` killed-mid-build arm and the
`pull-canary` preempt arm now assert the AD5 uncharged close
(`disconnected` / `worker_abort`|`preempted`) instead of the previous
non-success-charge set, and the establishment arm's left-the-view
assertion is keyed to the established exec (structural against the
healthy respawn); the cancel and establishment-window arms stand
unchanged. Both hosting checks were re-run green on the hardened
tree (single re-runs; the canary needed one assertion-hardening
retry). The retryPolicy pull-regime quint models are untouched by
this batch — the synthesized-verdict and abort paths are documented
model carve-outs that carry no retry charge — so no model re-runs
were owed.

### Phase-1b gate evidence table (assembled by the verification batch; close-out input)

Each row is one item of the v3-rescoped 1b gate (Phase-1 plan, slice-1b
header and gate-plan row G3). "Verdict" is the state at this batch's
assembly; rows marked PENDING are inputs the close-out (T-1b.11
landing) must resolve before slice 1c starts. No pool is flipped by
any row; production observations live in deployment-time checklist
rows D0–D5.

| Gate item | Artifact | Verdict at assembly |
|---|---|---|
| Pull-mode end-to-end VM evidence (T-1a.11) | `pull-mode` subtest in `vm-lifecycle-autoscale-k3s` (no-attempt charge-free death, pull build + report + ListOpenAttempts surface, killed-mid-build charged once + requeued + rebuilt under a fresh exec) | GREEN (wired in checks.*; landed at 1a, re-asserted by the 415d15e6f killed-mid-build update) |
| 1b canary VM scenario (T-1b.8: establishment window, busy bridge, preempt, NotYetReady, AD2 node key, small-fleet poison, rollback-by-template-flip; retry-feed/fold-input assertions steps 1–4) | `pull-canary` fragment hosted by the dedicated `vm-pull-canary-k3s` check (lifecycle module on a separate k3s fixture instantiation; `values/vmtest-pull-canary.yaml` pins the probe deadline to the 180 s floor; codecov `after_n_builds` 42→43) | **DELIVERED (executor follow-up, 2026-05-27; check green on its first run)**: retry-feed equivalence over the fold input — the same scripted success+failure sequence on the stream pool and a `dispatchMode: Pull` pool yields the same `permanent` outcome class, exactly one worker-reported charge per failure leg, no double charges, charge-free successes, the same poisoned/client-visible verdict, and the AD2 exclusion-key columns (executor key for stream rows, intent identity for pull rows) — plus the establishment window end-to-end (executor_crash/unreported, only after deadline+slack, requeued and rebuilt), the DisruptionTarget pull-mode preemption, and the CancelBuild cancel successor. NOT covered at VM level (carve-outs in the fragment header): busy bridge, NotYetReady arm, rollback-by-template-flip, the small-fleet NoEligibleSource ending and the node-keyed `source_node` exclusion (the per-pool reconciler ships no controller-authoritative binding in this fixture) — these stay on the T-1b.1/T-1b.2 unit/contract batteries and the close-out list |
| Cancel/preempt timing re-baseline (T-1b.9, VM-topology numbers) | Cancel/preempt timing assertions in `vm-pull-canary-k3s` + the figures recorded next to the AD5/OA1 deferral note in the Phase-1b record above | **RECORDED (executor follow-up, 2026-05-27)**: cancel 9.9 s and preempt 64.1 s against the asserted 90 s composite bound, establishment charge at attempt age 307 s vs the 180 s + 120 s window — VM-topology numbers only; the production budget remains deployment-time row D1 regardless |
| Pull-mode retryPolicy regime (T-1b.7) | `retryPolicyPull` in `retryPolicy.qnt`; wired `quint-retry-policy-pull` (exhaustive, 14 invariants) + 5 `quint-retry-policy-pull-witness-*` checks; as-built regime counts bit-identical | GREEN (all invariants HOLD; all five witnesses violate; figures in the introducing commit) |
| Model J / Model N re-runs (T-1b.10) | Wired `quint-spawn-coherence-*` and `quint-nodeclaim-*` exhaustive checks at this tree | GREEN, counts bit-identical to the recorded baselines (models unchanged at 1b) |
| Controller-map re-audit (T-1b.10) | `controller-invariant-map.md` "Executor-campaign 1b re-audit" entry | RECORDED; controller-campaign owner counter-signature PENDING at the close-out review |
| Unit/integration red-first batteries (T-1b.1–T-1b.6) | Per-crate batteries landed with the code batch (exclusion re-key, anti-affinity/NoEligibleSource, C4/C5 re-point, AD5 SIGTERM-abort + cancel/disruption arms, ICE re-trigger, dispatchMode rendering) | GREEN at the code-batch landing (their gates ran with T-1b.1–T-1b.6; re-confirmed by the per-crate checks at this tree) |
| Code-review pass over the 1a+1b pull-path code (T-1b.11) | `pull-path-review.md` (campaign workspace) + the "Code-review pass … review pass recorded + fixes landed" subsection in the Phase-1b record above | **RECORDED + FIXES LANDED (2026-05-27)**: 7 confirmed findings (P1–P7) fixed on this branch with red-first batteries, 1 refuted, minors taken or explicitly adjudicated; the two affected VM arms moved to the AD5 uncharged-close semantics and both hosting checks re-run green on the hardened tree |
| OA5 surface review + OA4 call | Owner review against `ListOpenAttempts` + the VM-demonstrated fleet view (no running canary) | OA5: SATISFIED by development-time review (v3 re-derivation — the running-canary sign-off was removed as a development-time gate; the surface-review record is in the Phase-1c' record below, written with deletion commit C, whose precondition it was; the live-fleet confirmation stays deployment-time row D7). OA4 (BuildPhase fate / DrainButton repoint): still the dashboard owner's call, due before T-1d.3 |
| AD5 numeric budget / OA1 latency comparison / establishment-slack re-baseline / production rollback drill | Deployment-time validation checklist rows D1/D2/D5 | DEFERRED BY DESIGN (v3 directive); explicitly NOT development-time gate items |

Deferral notes carried with the table: the AD5 numeric budget stays
unsigned until deployment-time row D1; the OA1 baseline only starts
accumulating at deployment; the rollback-by-template-flip production
drill is row D5 (the VM rollback-flip demonstration planned as T-1b.8
step 9 was NOT included in the landed `pull-canary` scenario — it
remains an open close-out item alongside the busy-bridge and
NotYetReady arms); no pool template is flipped during development.
The two executor-follow-up rows above were resolved on 2026-05-27 by
the landed `vm-pull-canary-k3s` scenario (with the carve-outs named in
their verdicts); the two PENDING-owner rows and those named carve-outs
are the open 1b gate items remaining for the close-out; everything
else in the v3 gate list is green in CI-wired form at this tree.

## Phase-1c record (Slice 1c — fleet-wide pull-path readiness, development-time)

Recorded by the slice-1c batch (Phase-1 plan v3, T-1c.1–T-1c.3;
T-1c.2b is a separate task and is explicitly NOT covered by this
record — see the open-items note at the end of the record; the
T-1c.2b batch later added its own record as the per-check disposition
table subsection below). Under the
2026-05-27 directive the production fleet flip and every observation
read from it are deployment-time checklist rows (D0/D2/D3/D6); this
record carries the development-time artifacts the deployment-time
rows will consume, plus the development-time 1c gate evidence.

### The deletion-gate observable and the deadline-horizon formula (T-1c.2; §4.5 choice instantiated)

The §4.5 deletion-gate observable is the stream-registration gauge at
zero for the computed deadline horizon. Instantiation:

- **Gauge:** `rio_scheduler_workers_active` (stream registrations;
  pull-mode pods never touch it). Pre-canned fleet-wide aggregate:
  the recording rule `rio:scheduler_stream_registrations:max`
  (= `max(rio_scheduler_workers_active)` across scheduler replicas),
  added to `infra/helm/rio-build/templates/prometheusrule.yaml` under
  the `rio.deletion-gate.rules` group (recording-only, no alert).
- **Horizon formula:** `max(activeDeadlineSeconds over live intents)
  + builder idle-timeout + report-flush slack`, where each input is
  read at evaluation time as follows:
  - `max(activeDeadlineSeconds over live intents)` — the maximum
    `.spec.activeDeadlineSeconds` over the live builder/fetcher Jobs
    in the executor namespaces (the controller renders it from the
    solved intent deadline; the same basis ListOpenAttempts reports
    as `deadline_secs`). With no live Jobs the term is 0.
  - builder idle-timeout — the builder Config `idle_timeout`
    (I-116; default 120 s, env `RIO_IDLE_SECS`), the bound for how
    long an already-registered idle stream executor may linger
    before exiting on its own.
  - report-flush slack — the scheduler Config
    `establishment_report_slack` (default 120 s), the slack the
    establishment sweep itself grants late reports.
- **Row-D6 evaluation shape:**
  `max_over_time(rio:scheduler_stream_registrations:max[<horizon>]) == 0`
  with `<horizon>` instantiated from the formula above at evaluation
  time, on every environment sharing the scheduler. The observable
  arms at deployment time (once a real fleet runs pull-mode); nothing
  is evaluated during development.

### The C3 busy-source statement (T-1c.2; the 1c gate text made explicit)

Once stream registrations read zero, the busy source C3 (the
orphan-running reap, I-165) consults is the **open-attempt view** —
`AdminService.ListOpenAttempts`, the 1a busy-signal bridge
(`covered_by_open_pull_attempt` in
`rio-controller/src/reconcilers/pool/job.rs`). The `ListExecutors`
arm remains consulted during coexistence but matches nothing once no
executor registers; an **empty executor list does not disable the
reap** — only an RPC failure does (fail-closed, either view), and the
leader-age fail-closed arm also remains until 1d. The I-165
stuck-pod coverage is therefore retained through the bridge rather
than lost to the empty-list arm; the old ListExecutors consultation
and the leader-age heuristic are removed only at 1d (T-1d.2), at
which point the open-attempt view is the sole busy source. No
coverage loss is being accepted at this slice.

### Fetcher-kind pull coverage (T-1c.2; OA3 evidence input)

`pull-fetcher` subtest added to `vm-pull-canary-k3s`
(`nix/tests/scenarios/lifecycle/pull-fetcher.nix`): a `kind: Fetcher`
Pool with `dispatchMode: Pull` builds a network-free fixed-output
derivation end-to-end on the pull path — the FOD routes to the
fetcher pool (FOD → `effective_features=[fetcher]` → only the
Fetcher pool covers it), the spawned pod carries
`RIO_EXECUTOR_KIND=fetcher` + `RIO_DISPATCH_MODE=pull`, exactly one
open pull-mode attempt is minted while it builds, the clean build
charges nothing, and the pod completes after its single report (the
OA3 one-pull shape) instead of retaining a session. The chart-side
plumbing for pool-kind dispatch-mode selection (the
`pools[].dispatchMode` / `poolDefaults.dispatchMode` rendering in
`templates/pool.yaml`) lands with the same task so values-driven
fixtures and the deployment-time per-pool flip (row D0) have a
first-class knob.

### The 1c defaults flip (T-1c.3): pull is the default dispatch mode

Landed development-time (no deployment happens during the workstream;
the per-pool template flip of a *running* fleet remains the
deployment-time operator action of checklist row D0). The flip changes
defaults, not capabilities:

- **Pool CRD** (`rio-crds/src/pool.rs`): an absent `spec.dispatchMode`
  now means `Pull` (`DispatchMode::default()` flipped); the controller
  reads the default through `unwrap_or_default()` so the CRD enum is
  the single source. `dispatchMode: Stream` stays selectable and fully
  functional until the stream path's 1c'/1d deletion.
- **Controller rendering** (`reconcilers/pool/pod.rs`): the pod
  template now renders `RIO_DISPATCH_MODE` explicitly for BOTH modes
  (`pull` for absent/Pull pools — plus the AD5 45 s abort grace —
  `stream` for explicit-Stream pools, which keep the per-kind/spec
  drain grace). The explicit stream value is what keeps Stream pools
  functional now that the builder image's compiled default is pull;
  the pod-level discriminators keep reading the rendered value, so
  pods spawned before a flip stay correctly keyed
  (`ctrl.pod.tgps-default+3`, bumped with re-pointed impl/verify
  sites).
- **Builder Config** (`rio-builder/src/config.rs`): `dispatch_mode`
  default `stream` → `pull` (config-schema fixture re-blessed;
  `docs/gen/config.json` regenerated). A pull-mode builder without
  `RIO_INTENT_ID` fails fast (the 1b hardening), so a mis-deployed
  stream-era environment surfaces as a prompt failed Job, never a
  silently-hung node.
- **Rendered manifests** (`infra/helm/rio-build`): `templates/pool.yaml`
  renders `dispatchMode` from `poolDefaults.dispatchMode`, whose chart
  default is now `Pull`; per-pool overrides remain for a canary-first
  deployment-time flip (row D0) or to keep a pool on the legacy
  protocol while it exists.
- **What stays pinned to Stream during development** (the not-yet-
  re-pointed stream-era machinery, all carrying pointers at the pin
  site): the VM-suite chart values (`values/vmtest-full.yaml`
  `poolDefaults.dispatchMode: Stream`), the standalone fixture's
  systemd workers (`nix/tests/common.nix` `RIO_DISPATCH_MODE=stream` —
  the standalone topology has no controller to inject
  `RIO_INTENT_ID`), and the two scenario-applied Pool CRs whose
  subtests assert registration-era behavior (`ephemeral-pool`,
  `netpol`). Their re-point (or disposition) is T-1c.2b, which is NOT
  part of this batch; until it lands, the pull path's VM coverage is
  the explicitly-Pull pools of the `pull-mode`, `pull-canary`, and
  `pull-fetcher` subtests. (Executed: the T-1c.2b record below carries
  the per-check disposition table and which of these pins were
  removed.)

### The T-1c.2b VM-corpus disposition table (per-check; the deletion-wave contract)

Recorded by the T-1c.2b batch. Every VM check in `nix/tests/default.nix`
is dispositioned below. The table is the contract the 1c' deletion wave
executes against (AD6 both directions, P13): deletion commit A
(T-1c'.2) may not land while any KEEP-STREAM row marked
*pending-harness* remains unresolved, and every KEEP-STREAM row marked
*retires-with-machinery* names the deletion commit that takes the
subtest (and its stream pin) out with it.

Dispositions:

- **RE-POINT** — the check's builder executors run the pull path after
  this task (the Stream pin it relied on is removed); assertions that
  read stream observables were re-pointed to their pull equivalents.
- **KEEP-STREAM** — the check (or the named arm/subtest) deliberately
  keeps exercising the stream path until the deletion wave removes that
  machinery. Two flavors, named per row: *retires-with-machinery* (the
  subtest is itself stream-machinery coverage and is deleted/re-stated
  by the named deletion commit, replacement coverage named) and
  *pending-harness* (the assertions are dispatch-mode-independent but
  the standalone harness cannot deliver work over the pull protocol
  yet — see the standalone-corpus note below the table).
- **MODE-INDEPENDENT** — the check exercises no builder executors (or
  none exist in its fixture), so the dispatch protocol is not part of
  what it verifies; unaffected by the cutover.
- **ALREADY-PULL** — was already exercising the pull path before this
  task (the 1a/1b/1c additions).

| Check | What it exercises (dispatch-relevant) | Disposition | Notes |
|---|---|---|---|
| `vm-lifecycle-core-k3s` | jwt-mount-present / health-shared / pool-lifecycle (no builds); cancel-cgroup-kill + build-timeout build via the chart pool | RE-POINT | Chart default pool is Pull once the `vmtest-full.yaml` pin is gone. cancel-cgroup-kill re-pointed at the AD5 cancel chain (attempt closed → controller foreground-deletes the Job → SIGTERM abort → cgroup.kill): open-attempt observation guard before the cancel, 90 s composite bound, structural `status='cancelled'` assertion, and a longer cancel sleeper so the bound stays below the sleep. build-timeout is unchanged (the worker-side per-drv deadline is mode-independent). Overlaps the pull-canary cancel arm by design — this one runs under the default (3600 s probe-deadline) values. |
| `vm-lifecycle-gc-k3s` | gc-dry-run / gc-sweep / refs-end-to-end build via the chart pool; assertions are PG/store-side | RE-POINT | Delivery flips with the values default; no assertion changes. |
| `vm-lifecycle-recovery-k3s` | recovery (leader kill mid-build), store-rollout | RE-POINT | The post-failover "worker re-registered" wait (`rio_scheduler_workers_active >= 1`) is replaced by a structural pull-path assertion: the in-flight slow build's execution row carries `dispatch_mode='pull'`. Re-registration has no pull analogue (reports are unary and need no session); report acceptance after failover is covered by the existing end-of-subtest drain plus the post-recovery build. |
| `vm-lifecycle-autoscale-k3s` | ephemeral-pool (Pool CR, Job-per-build, runaway guard), pull-mode | RE-POINT (ephemeral-pool) + ALREADY-PULL (pull-mode) | The ephemeral Pool CR's Stream pin is removed — the CR now omits `dispatchMode`, exercising the CRD absent-means-Pull default end-to-end. The `wait_workers_zero` precondition is replaced by the pod-level precondition the pull-mode fragment already uses (pull pods never register). |
| `vm-pull-canary-k3s` | stream-baseline arm; pull equivalence / cancel / preempt / establishment arms; pull-fetcher | ALREADY-PULL (pull arms) + KEEP-STREAM, retires-with-machinery (stream-baseline arm) | The stream-vs-pull retry-feed equivalence needs a stream pool until the stream path is deleted. The pin moves from the corpus default to `values/vmtest-pull-canary.yaml` (`poolDefaults.dispatchMode: Stream`, that check only). Retired by deletion commit A (T-1c'.2) together with the baseline arm itself; the pull arms are the surviving coverage. |
| `vm-lifecycle-prod-parity-k3s` | bootstrap Job, leader guard; no builder pods | MODE-INDEPENDENT | — |
| `vm-le-stability-k3s` | lease lifecycle (anti-affinity, acquire, stability, graceful release, failover, lease deletion); no builds | MODE-INDEPENDENT | — |
| `vm-le-build-k3s` | build-during-failover, sigkill-mid-build via the chart pool | RE-POINT | Assertions are lease/ledger-side; delivery flips with the values default. |
| `vm-cli-k3s` | AdminService round-trips (status incl. the executors summary, tenants, sla); no builds | MODE-INDEPENDENT | The `executors: N total` assertion is shape-only (>= 0) and stays valid when T-1c'.4 re-implements `ListExecutors` over the open-attempt view. |
| `vm-dashboard-k3s` | gRPC-Web framing via the Cilium Gateway; no builds | MODE-INDEPENDENT | — |
| `vm-netpol-k3s` | builder/store egress probes against build pods from the chart pool plus a scenario-applied cross-ns Pool | RE-POINT | Both pools now run Pull (values default; the `misplaced` Pool CR pin removed). The netns-probe pattern only needs a Running build pod, which the pull path provides identically (the 300 s sleepers). |
| `vm-ingress-v4v6-k3s` | v4/v6 ingress + NAT64 egress; two trivial builds via the chart pool | RE-POINT | Delivery-only; no assertion changes. |
| `vm-fetcher-split-k3s` | FOD→fetcher routing, builder airgap, fetcher egress + node dedication; pools inherit poolDefaults | RE-POINT | Builder and fetcher pools now run Pull; routing/netpol/pod-shape assertions are dispatch-mode-independent. |
| `vm-security-nonpriv-k3s` | nonpriv pod admission, seccomp profile, cgroup remount, one e2e build | RE-POINT | Delivery-only; securityContext rendering is mode-independent. |
| `vm-componentscaler-k3s` | store ComponentScaler closed loop driven by a 30-leaf fanout via the chart pool | RE-POINT | Delivery-only; no assertion changes. |
| `vm-substitute-scale-k3s` | substitution cascade + scaler signal; leaves substituted (no builder pods), the unsubstitutable roots build via the chart pool | RE-POINT | Delivery-only for the roots; substitution/scaler assertions unaffected. |
| `vm-sla-sizing-kwok` | nodeclaim_pool forecast provisioning under KWOK fake nodes; builder Jobs never run on a real kubelet | MODE-INDEPENDENT | The executor protocol is never exercised (KWOK Stages fake the nodes), so the rendered dispatch mode is inert here. |
| `vm-nixos-node` | AMI boot path; no rio services | MODE-INDEPENDENT | — |
| `vm-protocol-warm-standalone` | gateway ssh-ng opcodes, exit-status, connection grace; builds delivered by the standalone pull harness | RE-POINTED (was pending-harness) | Assertions are gateway-side; delivery now runs over the pull harness (spawner → `GetSpawnIntents`/`MintExecutorTokens`/`AckSpawnedIntents` → one-shot pull builder). No assertion changes. |
| `vm-protocol-warm-lix-standalone` | the same against a Lix (protocol 1.35) client | RE-POINTED (was pending-harness) | As above. |
| `vm-protocol-cold-standalone` | cold-bootstrap DAG incl. the FOD→fetcher-kind worker split | RE-POINTED (was pending-harness) | As above; the fetcher worker's spawner filters `GetSpawnIntents` by kind=Fetcher, so the FOD→fetcher routing assertion is preserved on the pull path. |
| `vm-ca-cutoff-standalone` | CA chain builds, cutoff saves, IA-on-CA resolution | RE-POINTED (was pending-harness) | As above (withHmac: the spawner presents the controller-role service token; per-intent executor tokens are scheduler-minted). |
| `vm-substitute-standalone` | store-side substitution, tenant gating, ssh-ng read opcodes; zero workers | MODE-INDEPENDENT | No executors in the fixture. |
| `vm-log-service-standalone` | store LogService ingest/tail/dedup; zero workers | MODE-INDEPENDENT | No executors in the fixture. |
| `vm-sla-sizing-standalone` | SLA estimator/explore/cost-solve fed by a scripted-telemetry worker (RIO_BUILDER_SCRIPT intercept is below the dispatch layer) | RE-POINTED (was pending-harness) | Sample delivery now runs over the pull harness; the `workers_active` boot/restore waits are replaced by the spawner-reachable boot gate and the unit-active condition (stop-the-worker-to-keep-it-queued still works — a stopped spawner pulls nothing). |
| `vm-scheduling-core-standalone` | FUSE/overlay/chunks/cgroup builder mechanics on 3 pull-harness workers | RE-POINTED (was pending-harness) | Builder-mechanics assertions unchanged; delivery is pull. The fanout PrefetchHint metric assertion was removed (stream-only mechanism; unit-test coverage carries it until T-1c'.3 retires it) and the fuse-direct overlay-bypass `ls` is best-effort when no builder process is live between pulls (the in-build FUSE coverage is unchanged). |
| `vm-scheduling-disrupt-standalone` | max-silent-time, setoptions-unreachable, cancel-timing (stream 5 s budget), reassign (disconnect→reassign), load-50drv | PARTIALLY RE-POINTED (delivery + delivery-only subtests), mixed | max-silent-time / setoptions-unreachable / load-50drv are RE-POINTED (delivery-only; the max-silent-time per-attempt bound gained documented pull-cycle slack). The stream-machinery subtests still retire with their mechanisms: `reassign` (mechanism #3) and the stream cancel budget at T-1c'.2 (pull successors: the pull-canary cancel arm and the killed-mid-build pull-mode arm). DEVIATION executed at the harness re-point: `warm-gate` (owned by T-1c'.3) and `sigint-graceful` (re-statement owned by T-1d.1) were unwired from this check at the re-point itself because neither can execute against a pull-delivery fixture; their named carriers until their owning commits are the warm-gate/PrefetchComplete unit tests (`rio-scheduler/src/assignment.rs`, `r[verify sched.assign.warm-gate]`) and the pull-loop SIGINT unit test (`rio-builder/src/runtime/pull.rs`, `r[verify builder.shutdown.sigint+3]`); the fragment files remain for T-1c'.3 / T-1d.1 to retire or re-state. The check is buildable green only once deletion commit A removes reassign + cancel-timing. |
| `vm-security-standalone` | HMAC/JWT boundaries, tenant resolve, rate limit, quota, executor identity token + kind-spoof heartbeat | PARTIALLY RE-POINTED (delivery), mixed | Delivery-only subtests are RE-POINTED (HMAC posture preserved: the spawner holds the controller-role service token, executor tokens are scheduler-minted per intent). `executor-kind-spoof` and the static stream executor token are session machinery — they retire with T-1c'.2; the pull side's per-unary token↔intent binding (mechanism #6) is carried by the 1a/1b unit batteries and the pull VM arms. |
| `vm-observability-standalone` | metric presence, log pipeline, trace export and traceparent propagation | RE-POINTED (was pending-harness) | EXPECTED_METRICS now reads `rio_scheduler_open_attempts` (pull-era fleet observable) instead of `workers_active`; the worker endpoint-up probe was replaced by the journald build-count proof (no idle builder process exists between pulls). The traceparent data-carry assertions are unchanged — `build_assignment_proto` embeds the same submit-time traceparent on both paths. |
| `vm-chaos-standalone` | store-RPC fault injection (latency / reset / partition / bandwidth) under live builds on a pull-harness worker | RE-POINTED (was pending-harness) | Assertions are store-client resilience (mode-independent); delivery is pull. Still the named Model-D carrier for T-1d.1/T-1d.4. |

**What RE-POINT concretely changed at this task.** The
`values/vmtest-full.yaml` `poolDefaults.dispatchMode: Stream` pin is
removed (the chart default — Pull since the 1c flip — now reaches every
helm-rendered VM pool); the `ephemeral-pool` and `netpol` scenario Pool
CRs lost their Stream pins; the stream-observable assertions in the
re-pointed k3s scenarios were replaced by pull-path equivalents
(pod-census preconditions instead of `workers_active`, an execution-row
`dispatch_mode='pull'` assertion instead of the re-registration wait,
the AD5 composite cancel bound instead of the stream cancel dispatch
timing). The standalone fixture (`nix/tests/common.nix`
`RIO_DISPATCH_MODE=stream`) kept its pin at that batch, annotated with
this table's disposition; the pin was removed by the standalone
pull-harness batch below.

**The standalone-corpus note (the open half of T-1c.2b — RESOLVED by
the standalone pull-harness batch).** The pull protocol binds work to
an intent-scoped executor (`RIO_INTENT_ID` plus a per-intent token
minted at spawn); the standalone topology has no controller or Job
spawner, so its long-lived systemd workers could not exercise the pull
path by a configuration flip — `run_pull` refuses to start without an
intent id by design. The KEEP-STREAM *pending-harness* rows above
therefore stayed on the stream path at that task. Option (a) of the
recorded choices was executed by the slice-1c' first batch as the
recorded precondition for deletion commit A: the standalone harness
gained a per-intent spawn mechanism (`rio-pull-spawner`,
`nix/tests/common.nix`) that polls the scheduler's real
`GetSpawnIntents` surface, asks the scheduler to mint the per-intent
executor token (`MintExecutorTokens` — the production signing path,
never a locally-forged credential), acks via `AckSpawnedIntents`, and
execs a one-shot pull-mode `rio-builder` for exactly that intent;
systemd's `Restart=always` plays the Job controller's respawn role.
Under withHmac fixtures the spawner authenticates with a
controller-role service token minted by the fixture
(`fixtures/standalone.nix`), mirroring rio-controller's own
credential. The per-row dispositions above were updated in place
(RE-POINTED / PARTIALLY RE-POINTED) by that batch; rows it could not
honestly reproduce carry named replacement coverage instead of a
silent drop (the warm-gate / sigint-graceful deviation recorded in
the scheduling-disrupt row).

#### T-1c.2b verification record (the re-point batch)

- Every k3s check whose inputs changed at this batch (all seventeen —
  the values default reaches every k3s fixture) was rebuilt once,
  serially, and is green on the re-pointed configuration:
  `vm-lifecycle-{core,gc,recovery,autoscale,prod-parity}-k3s`,
  `vm-pull-canary-k3s` (stream-baseline arm still executing
  dispatch_mode=stream via the overlay pin; every pull arm green),
  `vm-netpol-k3s`, `vm-le-{stability,build}-k3s`, `vm-cli-k3s`,
  `vm-dashboard-k3s`, `vm-ingress-v4v6-k3s`, `vm-fetcher-split-k3s`,
  `vm-security-nonpriv-k3s`, `vm-componentscaler-k3s`,
  `vm-substitute-scale-k3s`, `vm-sla-sizing-kwok`. No
  PENDING-REBUILD rows. Wall-clocks are recorded in the re-point
  batch's close-out report, not here.
- The KEEP-STREAM standalone corpus was not rebuilt because its
  derivations are unchanged: the only standalone-side edit is a Nix
  comment at the `common.nix` pin site, and the worktree/base
  `drvPath`s of representative standalone checks
  (`vm-scheduling-core-standalone`, `vm-chaos-standalone`) were
  compared and are bit-identical, so the slice-1c gate's green status
  carries over.
- `driverInteractive` (mypy + pyflakes on the testScript) was built
  for every check whose testScript changed (the five lifecycle
  splits, pull-canary, netpol) before the VM rebuilds.
- `tracey-validate`, `docs-lint`, and treefmt are green at this
  batch; no spec rule text changed and no verify markers moved (the
  re-pointed subtests still demonstrate the rules their wiring
  entries carry).

### Development-time 1c gate evidence (T-1c.3, v3-rescoped gate)

| Gate item (v3 1c slice header) | Artifact | Verdict at this batch |
|---|---|---|
| OA2 aggregation landed and test-covered (T-1c.1) | `nodeclaim_pool/wedge.rs` + the union consumption in `reconcile_once`; rule `ctrl.nodeclaim.wedge-cluster`; red-first unit battery (3 clustering tests run red against a stubbed tracker, then green); `rio_controller_node_wedge_marked_total`; controller-invariant-map 1c entry | GREEN (rio-controller suite 319/319 at the landing tree) |
| Fetcher/pool-kind VM coverage on the pull path (T-1c.2; the pool kinds the canary did not cover) | `pull-fetcher` subtest in `vm-pull-canary-k3s` (kind=Fetcher, dispatchMode: Pull, network-free FOD, OA3 one-pull assertions) | WIRED + check built once green at this batch (wall-clock recorded in the close-out report); OA3 one-pull confirmation for fetcher pools is the campaign-owner sign-off row in the development-time owner table, evidenced by this subtest |
| Deletion-gate horizon formula + recording rule recorded (T-1c.2) | The §4.5 instantiation above; `rio:scheduler_stream_registrations:max` recording rule | RECORDED (helm-lint green); arms only at deployment time (row D6) |
| C3 busy-source statement recorded (T-1c.2) | The statement above (open-attempt view is the busy source at zero stream registrations; empty-list does not disable the reap; no I-165 coverage loss accepted) | RECORDED |
| Pull-path VM suites green with every pool kind covered | Builder-kind: `pull-canary` (vm-pull-canary-k3s, re-exercised by this batch's single check build because the new fetcher subtest extends that check) and `pull-mode` (vm-lifecycle-autoscale-k3s, green since the 1a landing and re-run at the G4 full gate rather than rebuilt inside this batch); Fetcher-kind: `pull-fetcher` (this batch). The full "lifecycle/scheduling/chaos suites green fleet-wide on the pull path" criterion additionally requires the standalone/k3s stream-era corpus re-point — that is T-1c.2b and remains OPEN (not silently absorbed here) | PARTIAL BY CONSTRUCTION (T-1c.2b outstanding); everything in this batch's scope is green. (Later batch: T-1c.2b executed the k3s-corpus half and dispositioned the standalone half — see the disposition table above and its verification record.) |
| Full gate green at the landing | `/nixbuild --checks` at the 1c landing (G4) | Run at the landing by the integrator (per the gate plan); the per-task batteries, helm-lint, tracey-validate, crds/docs-data drift, and the touched VM check were run green inside the batch |

### Deferral note (v3 directive; recorded with dates at this batch)

The fleet-wide template flip of a running fleet is deployment-time row
D0; the fleet-wide latency comparison and the OA2-signal observation
are rows D2/D3; the deletion-gate gauge observation (armed only once a
real fleet runs pull-mode) is row D6; rollback at that point is still
a per-pool template flip back to `dispatchMode: Stream` (rows D0/D5) —
the development-time defaults flip above does not remove that lever,
because Stream remains selectable until the 1c'/1d deletions and the
deletion-stage releases stay separate (row D0 ordering). Recorded
2026-05-27 with the rest of this record.

## Phase-1c' record (Slice 1c' — deletion wave 1 + the model re-target)

### T-1c'.1 — pre-deletion checkpoint (the development-time deletion gate)

Recorded by the slice-1c' first batch (Phase-1 plan v3, T-1c'.1).
Under the 2026-05-27 directive the §4.5 deletion-gate observable —
the production stream-registration gauge at zero over the computed
deadline horizon — is a deployment-time read and is NOT what gates
development-time deletion. The development-time go for deletion wave
1 is re-derived from the verification evidence below; the production
gauge-at-zero read is deployment-time validation checklist row D6 and
remains the precondition for rolling out the **deletion-stage
release** (the deletion slices ship as separate releases at
deployment time, row D0 ordering, so a template flip back to Stream
stays available right up to that release).

#### Verification evidence the deletion wave rests on

| Evidence item | Where recorded | Status at this checkpoint |
|---|---|---|
| T-1c.2b VM-corpus disposition table executed for the k3s corpus | The per-check disposition table + its verification record (Phase-1c record above): all seventeen k3s checks rebuilt once, serially, green on the re-pointed configuration | GREEN |
| Pull-path lifecycle/scheduling/chaos VM coverage at the 1c landing | Development-time 1c gate evidence table (Phase-1c record above): `pull-mode`, `pull-canary`, `pull-fetcher` green; the k3s corpus delivery re-pointed by T-1c.2b | GREEN for everything in the 1c batch's scope; the standalone (non-k3s) corpus is the open half, see the precondition subsection below |
| 1b code-review pass over the 1a+1b pull-path code (T-1b.11) | Phase-1b gate evidence table: `pull-path-review.md`, 7 confirmed findings fixed with red-first batteries, hosting VM checks re-run green | RECORDED + FIXES LANDED (2026-05-27) |
| 1b / 1c gate records in place | Phase-1b record + Phase-1b gate evidence table; Phase-1c record + development-time 1c gate evidence table (both above) | IN PLACE (the two PENDING-owner rows of the 1b table — OA5 surface review sign-off and the controller-map counter-signature — are owner actions at the close-out review; OA5 sign-off remains the explicit precondition for deletion commit C, not for commits A/B) |
| The VM suite's own stream-registration read on the pull path | Pull-mode pods never register: the pull arms (`pull-mode`, `pull-canary`, `pull-fetcher`) prove delivery, attempt mint, and report entirely through `dispatch_mode='pull'` execution rows and `ListOpenAttempts`, with no stream registration required; after T-1c.2b the only k3s consumer of the stream path is the `pull-canary` stream-baseline arm behind its own `values/vmtest-pull-canary.yaml` pin (retiring with deletion commit A) | DEMONSTRATED at the VM level (the production gauge read stays row D6) |

#### The standalone (non-k3s) corpus precondition for deletion commit A

The T-1c.2b disposition table marks the standalone corpus rows
KEEP-STREAM in two flavors. The **pending-harness** rows
(protocol-warm, protocol-warm-lix, protocol-cold, ca-cutoff,
sla-sizing, scheduling-core, observability, chaos, plus the
delivery-only subtests of scheduling-disrupt and security) assert
dispatch-mode-independent behavior and stay on the stream path only
because the standalone fixture cannot mint pull intents (no
controller/Job spawner to inject `RIO_INTENT_ID` + a per-intent
token); the **retires-with-machinery** rows (the pull-canary
stream-baseline arm, `reassign`, the stream cancel-timing budget,
`executor-kind-spoof` + the static stream executor token at deletion
commit A; `warm-gate` at commit B; `sigint-graceful`'s stream-era
re-statement at T-1d.1) test the stream machinery itself and go when
it goes. Deletion commit A may not land while any pending-harness row
remains unresolved: those rows are resolved by the standalone
pull-harness commit of this same batch (option (a) of the
standalone-corpus note — a minimal per-intent spawn mechanism in the
harness driving the scheduler's real `GetSpawnIntents` /
`MintExecutorTokens` / `AckSpawnedIntents` surface), and the
disposition table above is updated row by row as each re-point is
executed and rebuilt green. Any row the harness genuinely cannot
reproduce gets a named replacement-coverage disposition in the same
update — never a silent drop.

#### The actor unit-test corpus precondition for deletion commit A

Discovered while mapping commit A (executor-deletion-a blocker,
2026-05-28; the unit-test analogue of the standalone-corpus
precondition above): the rio-scheduler actor unit-test corpus
delivers work through the stream session as its only vehicle
(`connect_executor`/`setup_with_worker` → `recv_assignment` →
`ProcessCompletion`), so deleting the connect/heartbeat/completion
intake at commit A would break ~258 actor tests, of which only ~111
live in the session-machinery files commit A retires
(tests/executor.rs, recovery.rs's and merge.rs's session parts,
grpc/stream_tests.rs). The other ~159 (completion.rs ~59,
dispatch.rs ~43, misc/build/fault/integration/keep_going/
lifecycle_sweep/wiring, the mixed pull.rs/establishment.rs tests,
plus admin-suite stragglers) test surviving pull-shared behavior —
above all the `handle_completion` classification path the pull
report intake itself calls — and use the session only as a harness.
Adjudicated direction: blocker path (A) — add pull-mode delivery
helpers and re-point those tests so their assertions are unchanged
while delivery goes through the production pull surfaces
(`PullAssignment` admit + fenced mint, `ReportPullOutcome`,
`ReportAttemptOutcome`, the establishment sweep); a test whose
property genuinely requires stream semantics stays on the stream
path and is recorded against the commit that retires the machinery
it tests. Commit A may not land while unconverted harness-only tests
remain, for the same P13 reason as the VM-corpus precondition.

Status (helper batch): the pull delivery helper layer is in
place in `actor/tests/helpers.rs` (`pull_attempt`/`try_pull_attempt`,
`pull_complete_{success,success_empty,ca,failure}`, `pull_report*`,
`bind_intent_node` + `pull_fail_on_nodes`, `report_attempt_terminal`,
`backdate_pull_attempt`, `setup_pull_ca_fixture`), and the
completion.rs retry/budget/poison block (10 test fns / 13 cases) is
converted and green alongside the untouched stream corpus.
Conversion conventions recorded in the helper-batch commit message:
distinct stream workers → distinct bound source nodes
(`bind_intent_node`), same-worker repetition → same intent identity,
fleet-exhaust padding workers drop out (an empty fleet is
NoEligibleWorkers, never a poison), force-assign backoff shims drop
out (pulls are not backoff-gated).

Status (re-point batch, complete): the three follow-up re-point
commits finish the corpus conversion. Per-file outcome, in test fns —
conversions keep assertions unchanged and deliver through the
production pull surfaces; every non-converted test carries an
explicit disposition named in the converting commit's message:

- completion.rs: 51 converted (the 10 retry/budget fns above + 41
  more), 13 leave-stream — 5 retire with commit B
  (test_transient_retry_different_worker's failed_builders placement
  exclusion, and the four `last_intent`-coupled build-sample tests
  whose cpu_limit_cores assertions need the dispatch-time intent
  writer: hw_class_and_intent_cores, infinite_peak_cpu,
  nonfinite_final_resources, out_of_domain_final_resources) and 8
  retire with commit A (the stream CaFixture race meta-test, the
  ProcessCompletion unknown-drv_key / non-running guards, the
  post-cancel Cancelled-report log pin, the unsolicited-Cancelled
  infra charge, the reportless-backstop line-count pin, and the two
  stream-controller-channel tests phase1b_e2_d2/d3) — each naming its
  pull-side replacement coverage
  (report_outcome_unknown_exec_writes_nothing,
  report_outcome_after_establishment_ignored, the AD5 abort/cancel
  pair, the establishment-sweep and second-installment tests).
- misc/build/fault/integration/keep_going/lifecycle_sweep/wiring: 26
  converted, 21 leave-stream. Retire with commit A: fault.rs's seven
  DB-fault-posture tests plus integration's
  test_db_failure_during_completion_logged (under a PG fault the pull
  report intake rejects at the attempt-resolution read and the pod
  retries — the stream intake's in-memory-first / bounded-re-delivery
  posture has no pull equivalent), and the registration / disconnect
  / stale-report / starvation / workers_active / shutdown-drain /
  inspect-stream-pool / cancel-signal tests. Retire with commit B:
  the dispatch leader/recovery gate, the dispatch-wait metric, the
  interactive front-of-queue ordering, and the assign-send-failure
  rollback test.
- pull.rs: pull_completed_drv_returns_gone re-pointed onto a
  pull-completed terminal; the other five mixed tests are the
  coexistence arm itself (a live stream attempt is their subject) and
  retire with commit A. establishment.rs's
  establishment_never_visits_stream_attempts likewise retires with A.
- admin suite: the two ClearPoison tests converted.
  cluster_status_counts_queued_and_running stays on the stream — its
  queued_derivations assertion reads the dispatch-maintained
  ready_queue length, which a pull mint does not dequeue (recorded in
  the unit-repoint findings note as a pull-mode signal-accuracy
  question for the operator-surface re-point) — and it, the
  ListExecutors/executors-summary surface (workers_tests.rs,
  cluster_status_counts_registered_workers) and the two DrainExecutor
  tests are commit C's operator surface, re-pointed or retired there.
- dispatch.rs (~43) is unchanged, as recorded above:
  placement-layer subject, retires with commit B, per-test
  dispositions recorded when commit B processes that file.

The full rio-scheduler suite is green at every commit of the batch
(1277 tests). This precondition is SATISFIED for deletion commit A:
every harness-only test outside dispatch.rs is either re-pointed onto
the production pull surfaces or carries an explicit
retire-with-machinery / covered-by-pull disposition; commit A must
carry those dispositions into its per-test deletion mapping (P13)
when it deletes the leave-stream tests it owns.

#### The deletion commit set and the revert-cleanliness contract

Deletion wave 1 lands as three named, individually revert-clean
commits, in order, each naming in its commit message the
disposition-table rows it discharges (AD6 in both directions, P13 for
every deleted test):

- **Commit A (T-1c'.2)** — the scheduler session machinery:
  per-mechanism rows #1–#5, #7–#11, #13–#14, #16–#19 and the #21
  shrink (heartbeat intake/reaper/stall credit, adopt, phantom
  two-strike, reconnect stale-flag clear, stale-epoch filter,
  `recently_disconnected` + TTL sweep, disconnect→reassign detection,
  the `BuildExecution`/`Heartbeat` handlers reduced to unconditional
  error stubs, `ExecutorState` and the session maps, the 45 s
  post-failover reconcile special case, the backstop's
  cancel/quarantine halves, dispatch rollback, the DeadlineExceeded
  prefix-match), their tests, and the retires-with-machinery VM
  pieces tagged T-1c'.2 in the disposition table (the pull-canary
  stream-baseline arm and its values pin, `reassign`, the stream
  cancel-timing budget, `executor-kind-spoof` + the static stream
  executor token).
- **Commit B (T-1c'.3)** — the placement/eligibility layer
  (dispatch_ready pacing, placement decision, warm-gate two-pass,
  intent-match override, became_idle inline dispatch, closed-stream
  exclusion, the 4-phase assign path; rows #11/#18 dispatch-side
  remnants; `sched.sla.intent-match` and `sched.assign.warm-gate`
  retired; the legacy pod-name exclusion key dropped per P12).
- **Commit C (T-1c'.4)** — operator surfaces O1–O3 + admin plumbing,
  only after the OA5 sign-off (the PENDING owner row above);
  `ListExecutors` re-implemented over the open-attempt view,
  `DebugListExecutors` retired, `DrainExecutor` per the plan's
  default (no-op-with-error until the 1d proto sweep).

Contract: each commit reverts cleanly on its own (`git revert` of
that single commit restores the deleted machinery and its tests
without touching the other commits), and the reverts are kept rebased
and re-tested until the Phase-2 close-out per the frozen §4.5 choice
(forward-fix-only explicitly not adopted). Nothing the disposition
table marks KEEP-STREAM-until-B/C or pending-harness may be taken by
commit A; a later commit may not silently absorb an earlier one.
Larger model re-targeting (T-1c'.5: Model S re-target, witness and
calibration flips) and the as-built-channel retryPolicy regime
retirement (T-1c'.6) follow the deletion commits in the same slice;
the wired as-built executor session checks stay untouched and green
through commits A–C because they model the pre-deletion file as it
was — they are re-wired only at T-1c'.5.

#### Row-D6 deferral note

The §4.5 deletion-gate observable (the
`rio:scheduler_stream_registrations:max` recording rule at zero for
the computed deadline horizon, instantiated in the Phase-1c record
above) arms only once a real fleet runs pull-mode. Its evaluation is
deployment-time validation checklist row D6 and gates the rollout of
the deletion-stage release, not the development-time landing of the
deletion commits. The `workers_active` gauge keeps being emitted
(reading zero on a pull-only fleet) until 1d precisely so that row D6
stays evaluable after commits A–C land.

Successor-signal re-point (recorded with deletion commit C, which
pinned the gauge): on 1c'-era code the gauge is hardwired to zero (the
intake that incremented it is deleted), so the gauge rule remains the
honest D6 read only for the pre-deletion release it is evaluated
against. The successor signal for "stream-mode executors still exist
and are trying to register" on deletion-stage code is the
`rio_scheduler_stream_stub_calls_total` counter (incremented by the
BuildExecution/Heartbeat error stubs), pre-canned as the
`rio:scheduler_stream_attempts:rate5m` recording rule next to the
gauge rule. Row D6 evaluated against an environment already running
deletion-stage code, and row D7's post-deletion "no calls to removed
RPCs" watch, both read that series; the horizon formula is unchanged.

#### Spec-consequence checklist for the 1c' sweep

Queued at 0e (Phase-1 input list item 8), executed across the 1c'
commits — each item lands in the same commit as the code change or
model re-target that licenses it, with `tracey bump` + marker
re-points in that commit:

1. AD4: the `sched.lease.generation-fence+2` amendment (the
   worker-side latch and `WorkAssignment.generation` consumer give
   way to the transaction-side fence) and the
   claim-before-advertise → claim-before-serve successor statement in
   the lease-checklist re-derivation (T-1c'.7, per the T-0e.2 note).
2. C2: the `sched.executor.deregister-reassign` epoch qualification —
   resolved at commit A by retiring/re-stating the rule with the
   session machinery it describes (the contradiction record stays as
   history).
3. C3: the `controller.typ` Executor Lifecycle SIGTERM-drain prose
   (stale `DrainExecutor` step) — fixed no later than commit C, which
   touches the surface the prose misdescribes.
4. C4: the `sla-sizing.typ` `@alg-pool` `dead_nodes` annotation
   (floor and TTL wording) — fixed with the 1c' spec sweep or, at the
   latest, when the detector moves at 1d.
5. Retiring `sched.sla.intent-match` and `sched.assign.warm-gate`
   with their mechanisms (commit B), with retirement records — not
   silent deletion.
6. Re-pointing the five 0b rules' verify markers
   (`sched.executor.{session-epoch,liveness-window,repair-precedence,one-shot}`,
   `builder.completion.exactly-once-or-death`) when the checks they
   cite re-target (T-1c'.5 for the session checks, 1d for Model D).

#### OA5 surface-review record (development-time; precondition for deletion commit C)

Recorded with deletion commit C (T-1c'.4), whose explicit precondition
the OA5 sign-off was. Basis: the v3 re-derivation — the original
running-canary operator sign-off was removed as a development-time
gate; what stands in is a development-time review of the successor
surface against the VM demonstrations and the 1b evidence assembly,
with the live-fleet confirmation deferred to deployment-time checklist
row D7.

What the operator view becomes (the reviewed surface):

- **`AdminService.ListOpenAttempts`** — the attempt-keyed fleet view:
  every open pull-mode attempt with derivation, exec_id, executor
  identity, source node (when known), generation, and age; the
  `rio_scheduler_open_attempts` gauge is the same view as a fleet
  count. Demonstrated by the `pull-mode` lifecycle subtest and the
  `vm-pull-canary-k3s` scenario (the 1b evidence rows above) and by
  the admin-surface unit batteries.
- **`ListExecutors` (re-pointed at commit C)** — the same view
  projected one-entry-per-attempt for existing CLI/dashboard/
  controller callers (busy/alive entries keyed by the attempt's
  executor identity, system/kind from the attempt's derivation,
  attempt-open time in the timestamp fields, `leader_for_secs`
  preserved). `rio-cli status` / `rio-cli workers` and the dashboard
  Executors page keep working against it unchanged at the API level.
- **`ClusterStatus` (re-pointed at commit C)** — executor counts are
  the busy view (open attempts), `queued_derivations` is the
  DAG-Ready count (now equal to `queued_by_system`'s sum by
  construction — the recorded over-count is fixed), so the
  autoscaler/dashboard headline numbers stay meaningful on a
  pull-only fleet.
- **The controller's Job census + dashboard views** — pod phase, node
  and age for spawned-but-not-yet-pulled pods (the half the scheduler
  no longer sees), per the OA5 0e record's "what the dashboard loses"
  list; the dashboard keeps its Executors/Cluster pages against the
  re-pointed RPCs, and the DrainButton/BuildPhase repoints remain the
  recorded OA4/dashboard-owner ask due before T-1d.3.

Operator controls reviewed against their successors (O1–O3, per the
frozen contract): per-executor drain → cordon + AD2 node exclusion;
force-evict → cancel verdict on the open attempt + controller Job
deletion under the AD5 budget (cancel and preempt demonstrated
end-to-end in `vm-pull-canary-k3s`, 9.9 s / 64.1 s VM-topology
figures recorded in the Phase-1b record); fleet-wide stop → pause
spawn-intent emission + bulk cancel. `DrainExecutor` and
`DebugListExecutors` therefore retire to clear-error no-ops at commit
C and their RPCs leave the proto at the 1d sweep.

Verdict: the successor surfaces honestly carry the operator questions
the stream-era surfaces answered ("what is building", "what is the
fleet doing", "how do I stop/evict work"), with the named loss (per-pod
heartbeat age / capacity flags / live stream state) already recorded in
the 0e OA5 decision and re-pointed at the Job/pod census. The
gate-evidence-table OA5 row above is marked satisfied by this record;
the live-fleet OA5 confirmation remains deployment-time row D7.

#### Pre-deletion go (development-time owner record)

Go for deletion wave 1 recorded against the verification evidence
above, per the v3 owner table (T-1c'.1 row). The go covers commits A
and B unconditionally and commit C conditionally on the OA5 surface
sign-off recorded at the 1b close-out review; it is a development-time
record only — no production read is implied, and the deletion-stage
release remains gated by row D6 at deployment time. Recorded
2026-05-28 by the slice-1c' first batch executing the campaign
owner's instruction.

### T-1c'.5 — Model S re-target record (the pull-protocol model, the witness/calibration flips, the check re-wiring)

Recorded by the slice-1c' model-and-spec batch (Phase-1 plan v3,
T-1c'.5), after deletion commits A–C removed the stream session
machinery the Stage-B Model S encoded.

#### The re-target and the freeze (P14)

The Stage-B as-built Model S is frozen as
`docs/spec/models/executorSessionAsBuilt.qnt` — module names carry the
`AsBuilt` prefix, the file is unwired (no nix/quint.nix check builds
it), and it is kept typechecking as the oracle for the Stage-C
calibration evidence modules (whose imports are re-pointed at it) and
as the encoding a deletion-wave revert would re-wire. The re-targeted
model takes the `executorSession.qnt` name: state = the per-intent DAG
readiness (wanted-but-not-Ready vs Ready — the OA6(a) pre-attempt wait
state) × the pod lifecycle (spawn, bounded NotYetReady retry,
charge-free idle exit, delivery, death, replacement) × the durable
open-attempt row and its closers (worker report first installment, the
controller's second installment / synthesized verdict / no-attempt
no-op, the establishment sweep's adopt and charge arms) × the claims
floor and the per-transaction generation fence. The session/heartbeat/
dispatch/disconnect/reap/correlation actions of the as-built encoding
have no successor actions — the machinery is deleted; the pull/report/
establishment/synthesized-verdict actions ARE the model. Every action
cites the post-deletion production function it encodes (admit_pull,
mint_and_deliver / mint_pull_attempt_fenced, fold_report,
handle_report_outcome, close_pull_attempt_uncharged,
report_attempt_outcome_inner, tick_sweep_open_pull_attempts,
establish_open_pull_attempt, the claims-floor GREATEST fence).

Invariants carried by the re-targeted model:
`StaleAuthorityWritesAreInert`, `AtMostOneOpenAttemptPerJob`,
`NotYetReadyIsInert`, the re-based `unresolvedClaimHasRepairArmed`
(in-flight ⇔ open-row coupling, with the cancelled-with-open-row
carve-out whose armed repair is the synthesized-verdict close),
`noFabricatedCompletion`, `establishmentOnlyAfterWindowCloses`
(re-keyed to the dispatch-deadline + report-slack window), and
`attemptResolvesAtMostOnce` (the dedup-state half of
one-charge-per-death; the budget arithmetic stays imported from
retryPolicy.qnt's pull regime), plus `boundsOK`.

#### Verdict table (exhaustive TLC, allInvariants per regime)

Distinct/generated state counts and wall-clocks are in the introducing
commit message (volatile-figures discipline); this table records the
verdicts.

| Model / regime | Wired as | Verdict |
|---|---|---|
| S′ base (pull) | `quint-executor-session-base` | HOLD (search depth 21) |
| S′ fault-leader (pull) | `quint-executor-session-fault-leader` | HOLD (search depth 24) |
| D base / fault-stream / fault-process | unchanged (`quint-executor-delivery-*`) | unchanged — Model D retires at 1d, not here |

Both regimes are exhaustive within the per-check budget (single-digit
seconds of TLC each); no demote-to-manual fallback was needed. The
0c stop-and-report item about the as-built fault-stream-conn /
fault-process budget non-convergence is closed by this re-target: those
regimes modeled machinery that no longer exists, and their exhaustive
cfgs retire with it (see the check disposition table below) rather
than awaiting a budget adjudication.

The unresolvedClaimHasRepairArmed adjudication record above remains
the historical record of the as-built falsification and its fix; the
re-based form of that invariant HOLDS in both wired regimes of the
re-targeted model.

#### OA6(a) Model-S pricing: discharged

The OA6 DECIDED block's consequence "the unary signature carries the
third protocol state, priced into §4.2 rows 13/B7 and the re-targeted
Model S at 1c'" and obligation-table row 6's option-(a) leg are
discharged by this record: the NotYetReady wait state is in the model
state space (pod `PWaiting` with `podSawNyr`), `NotYetReadyIsInert`
HOLDS in the base and fault-leader regimes, and both OA6 reachability
witnesses violate (the warm-start NotYetReady→Ready→Deliver path and
the never-Ready charge-free idle exit are reachable):
`quint-executor-session-witness-warm-start`,
`quint-executor-session-witness-idle-exit`.

#### Wired witness set (all expect-violation, all violating)

Base regime: warm-start (OA6 i), idle-exit (OA6 ii), establishment
charge, store-probe adopt, synthesized close, second installment,
idempotent re-pull, orphaned open attempt, late-report ignored,
no-attempt no-op. Fault-leader regime: stale-authority fenced,
post-failover deliver. Every safety invariant's contended state is
pinned by at least one of these (the at-most-one-open-attempt and
repair-armed contention by the re-pull and orphaned-attempt witnesses,
the NotYetReady inertness by the OA6 pair, the window discipline and
charge dedup by the establishment/adopt/synthesized/second-installment
witnesses, the fence by the stale-fenced witness).

#### Check disposition table (what was retired, replaced, kept)

| As-built check (pre-1c'.5) | Disposition |
|---|---|
| `quint-executor-session-base` (as-built) | replaced — same attr name, now the re-targeted base regime |
| `quint-executor-session-fault-stream-msg` | retired with the stream machinery (message faults on a deleted channel); no successor regime — the pull path has no scheduler-side in-flight message to lose or duplicate (the unary reply is the delivery) |
| as-built fault-stream-conn / fault-process / fault-leader / fault-persist regime modules (unwired manual targets) | retired with the machinery; the budget stop-and-report they were awaiting is closed (see above). The fault-leader CONTENT (failover + stale authority) is re-targeted as `quint-executor-session-fault-leader`; pod death is part of the re-targeted base regime (it is normal pull-path lifecycle, not a fault class) |
| 12 as-built session witnesses (phantom, drain-pending, half-dead, stale-epoch, rollback, adopt, failover-inflight, deposed-believer, reap-after-stall, two-channel-death, establishment, race-ahead) | retired; the contended states they pinned either no longer exist by construction (phantom, half-dead, stale-epoch, rollback, adopt, drain-pending, reap-after-stall, two-channel-death, race-ahead — all keyed to deleted machinery) or are re-pinned by the new witness set (establishment → witness-establishment; failover-inflight / deposed-believer → witness-stale-fenced + witness-post-failover-deliver + witness-orphaned-attempt) |
| `quint-executor-delivery-*` (Model D) + its 5 witnesses + `quint-executor-calib-f2d-exit-owed` | kept unchanged — Model D is the builder's delivery choreography and retires at 1d (T-1d.4), not with the scheduler deletion |

#### Calibration flips (the 6 `quint-executor-calib-*` expect-violation checks)

| Check | Flip | Record |
|---|---|---|
| `quint-executor-calib-f1-stale-epoch` | (a) retired | Stream-epoch attribution: no streams, no epochs, no disconnect events to mis-attribute. Cannot recur by construction on the pull path; the per-unary token↔intent binding plus the transaction-side generation fence are the successor identity discipline. Acceptance-table verdict pre-staged: "cannot recur by construction". |
| `quint-executor-calib-f2-phantom-drain` | (a) retired | Phantom two-strike drain: a phantom (scheduler-believed-running build the worker no longer holds) required the push channel; with pull-only delivery the scheduler never believes a pod holds work it did not pull, and the establishment sweep resolves any open attempt whose pod stopped reporting. Cannot recur by construction. |
| `quint-executor-calib-f3-stall-credit` | (a) retired | Worker-time reaper stall credit: there is no scheduler-side liveness reaper to mis-measure; liveness is the Job's lifecycle plus the establishment window (anchored to the dispatch-time deadline, which never shrinks at sweep time). Cannot recur by construction. |
| `quint-executor-calib-f5-closed-stream` | (a) retired | Closed-stream dispatch exclusion: there is no scheduler-side placement decision left to exclude anything from; a pod pulls only its own intent. Cannot recur by construction. |
| `quint-executor-calib-f4-correlation-entry` | (b) re-encoded | → `quint-executor-calib-f4-establishment-window` over the re-targeted model (`calibration/executor-f4-pull-establish-early.qnt`): dropping the establishment-window gate lets the sweep charge an attempt whose classifying report still has its window — the pull-path analog of the I-197 double-charge precondition. Verified violating (the override falsifies `establishmentOnlyAfterWindowCloses`). |
| `quint-executor-calib-f2d-exit-owed` | kept | Model D / builder half; flips with Model D's retirement at 1d. |

The retired families' override modules stay under
`docs/spec/models/calibration/` as evidence over the frozen as-built
model (imports re-pointed to `executorSessionAsBuilt`), re-runnable
with the README recipe; the Phase-2 acceptance table consolidates or
retires them, exactly as the retry campaign's precedent did.

#### Verify-marker re-points (the five 0b rules; spec-consequence item 6)

| Rule | Marker before | Marker after T-1c'.5 |
|---|---|---|
| `sched.executor.one-shot` | `quint-executor-session-base` (as-built) | re-pointed to the re-targeted `quint-executor-session-base` (the per-intent single open attempt + the one-pull pod lifecycle is the pull-era form of one-shot); the builder-side impl markers are unchanged |
| `sched.executor.liveness-window` | `quint-executor-session-base` (as-built) | dropped from the model wiring — the rule text describes the deleted heartbeat/correlation windows; its successor content is `sched.attempt.establishment-window+2` (now carrying a model verify marker). The rule itself is retired/re-stated by the 1c' spec sweep. |
| `sched.executor.repair-precedence` | `quint-executor-session-base` (as-built) | dropped from the model wiring — the 22-mechanism precedence it normed is gone; the surviving precedence (first classifier wins the durable row) is carried by `sched.executor.report-idempotent` / the establishment rules. Retired/re-stated by the 1c' spec sweep. |
| `sched.executor.session-epoch` | `quint-executor-session-witness-stale-epoch` | dropped with the retired witness — no impl markers remain (deleted with commit A); the rule is retired by the 1c' spec sweep. |
| `builder.completion.exactly-once-or-death` | `quint-executor-delivery-fault-stream` / `-fault-process` | unchanged (Model D, 1d scope). |

New markers carried by the re-targeted wiring:
`sched.executor.pull-not-ready` and
`sched.attempt.establishment-window+2` on
`quint-executor-session-base` (the model now machine-checks the
NotYetReady inertness clause and the establishment-window discipline).

### T-1c'.7 (re-derivation half) — the neighbor-interface re-derivations and the net-diff record

Recorded by the slice-1c' model-and-spec batch. This is the
re-derivation half of T-1c'.7 only: the obligations are re-derived
against the pull-only world and recorded here and in the neighbor
maps. The 1c' landing itself (full gate, campaign close-out) is the
campaign owner's separate step and is NOT performed by this batch.

#### Model J/N obligation rows re-derived (the 0e table, post-deletion)

| Row | Obligation | Re-derivation against the pull-only world |
|---|---|---|
| 1 | busy view for the orphan-Running reap | `ListExecutors` is re-implemented over the durable open-attempt view (deletion commit C reads `list_open_pull_attempts`); busy = an open pull-mode attempt. The orphan-reap gate's fail-closed posture is retained (RPC error / young leader / empty list ⇒ no reap) — re-derivation item (i) discharged; only the leader-age arm remains a 1d retirement candidate, unchanged at this slice. Freshness improves (durable, survives failover). |
| 2 | registration-edge ICE clear | The clear edge is the first successful pull: the fenced mint clears the intent's `dispatched_cells` entry and the ICE cell under the same single-cell discipline as the retired registration edge (`pull_mint_ice_clear_only_at_single_cell` is the dedicated test). The heartbeat ICE-clear edge died with the heartbeat intake (commit C); arming and the DAG-state sweep are untouched. |
| 3 | `dispatched_cells` ack-arming | Unchanged — `AckSpawnedIntents` arming and Model J's subject are untouched by the deletion wave. |
| 4 | termination-report idempotency | Carried by the idempotent `ReportAttemptOutcome` column fill keyed by attempt identity plus the no-attempt no-op; the in-memory `recently_disconnected` first-report-wins map is deleted (commit A) and the durable guard (`WHERE termination_reason IS NULL`, exec-keyed uniqueness) is the only dedup needed. Strictly stronger than the as-built provider (no TTL race, survives failover). |
| 5 | `dead_nodes` hung-node signal | The scheduler-side detector was deleted at commit A (ahead of the 1d slot this map's 1c entry anticipated); the OA2 controller-side node-wedge clustering (live since 1c) is the successor, `GetSpawnIntents.dead_nodes` is now explicitly empty, and the proto field stays until the 1d sweep. Model N consumes the same Dead arm; no model change. |
| 6 | placeable-set input distribution (OA6) | Resolved on the option-(a) leg: the publish keeps no ready filter, Model J is unchanged, and the NotYetReady wait state is priced into the re-targeted Model S (the T-1c'.5 record above is the discharge). |
| 7 | cancellation/preemption read | The controller's disruption/cancel arms read the same open-attempt view and close open attempts via the synthesized-verdict path (charge-free, requeue at the fold); the read-every-tick obligation is carried as environment in this re-derivation, and the explicit Model-J action for it remains with the 1d Model-J pass if that pass finds it load-bearing. |

Re-derivation work items (i)–(iii): (i) discharged as row 1 above;
(ii) `ORPHAN_REAP_GRACE` (300 s, creation-age) re-validated against
worst-case container-start → first successful pull under leader
failover + recovery gating + the 5 s NotYetReady re-pull pacing and
the bounded idle exit — the budget holds with wide margin for the
non-failover case and the accepted miss consequence under a failover
that outlasts the grace is pod respawn/churn only: a never-pulled pod
has no attempt (the no-attempt no-op keeps it charge-free) and a
mid-build pod has an open attempt (the busy view keeps it un-reaped);
never a mid-build reap, never a charge. (iii) the no-attempt no-op
rule is now both spec'd (`sched.attempt.no-attempt-no-op`) and
model-checked (the re-targeted Model S witness `canReachNoAttemptNoop`
plus the retry pull regime's `noNoAttemptReportNoop`), so the
reapSafety/degradedPolarity re-derivations may rely on it as an
assumption.

Models J and N are byte-unchanged at this slice; their wired checks
are unaffected by construction (the green state at this tree is the
standing CI state). The header-checklist prose updates inside
`spawnCoherence.qnt` / `nodeclaimLifecycle.qnt` ride with the
campaign close-out so those files rebuild their checks exactly once;
the re-derivations of record are this section and the controller
map's 1c' delta entry.

#### The lease-seam re-derivation (T-0e.2 executed at 1c')

Recorded in the re-targeted model's assume-guarantee header
(executorSession.qnt) and re-stated here: all four lease exports stay
consumed (`atMostOneCASWinner`, `boundedDualLeadership`,
`staleLeaderHasStaleGeneration`, regime-scoped `neverDual` still not
imported); the consumer moved from the deleted worker-side B5 latch
to the transaction-side fence (the claims-floor GREATEST read inside
the pull mint, the uncharged close and the establishment charge);
claim-before-advertise's successor is claim-before-serve (the new
leader's claim row is durable before it serves pulls — recovery's
ordering, encoded in the model's failover action); the dual-belief
residual's checked successor is `StaleAuthorityWritesAreInert`
(wired, fault-leader regime) plus the Phase-2 `admit_pull` Kani
contract; the acquire-epoch-vs-generation distinction is preserved by
construction (the fence compares against the durable floor, never
against "the generation moved"). The `sched.lease.generation-fence+2`
/ claim-before-advertise spec amendments (AD4, spec-consequence item
1) are NOT made by this batch — they belong to the 1c' scheduler.typ
spec sweep, which remains outstanding, and are recorded there.

#### The retryPolicy environment re-derivation note

Executed as T-1c'.6 (see the retry map's cross-campaign retirement
addendum): the as-built-channel regimes retired, the pull regime is
the wired set, and the per-pin carrier table records what carries
each retired non-vacuity pin.

#### The 1c' net-diff record (deletion commits A–C)

`git diff --stat` over commits A^..C (b5b961866^..94e6e2e4d):
84 files changed, 2289 insertions, 24925 deletions — net −22636
lines. Scoped to `rio-scheduler/src`: 52 files, +1791/−23526 — net
−21735; scoped to the non-test scheduler session core: 29 files,
+763/−7547 — net −6784. Net-negative as required; no deviation.
Per-commit figures are in the deletion commits' messages
(A: −14771, B: −5886, C: −1982).

### T-1c'.8 — the 1c' scheduler.typ spec sweep (the deferred retire/re-state batch)

Recorded by the slice-1c' spec-sweep batch. This is the scheduler.typ
retirement/re-statement work the deletion commits A–C deferred (commit
A's recorded deviation), plus the AD4 lease amendments
(spec-consequence item 1), the C2 resolution (item 2), the C4
sla-sizing fix (item 4), and the as-built protocol/operator-surface
prose. Spec-consequence checklist status after this batch: item 1
(AD4) — done here; item 2 (C2) — done here (the deregister rule
retired with the machinery; the contradiction record stays as
history); item 3 (C3) — done at deletion commit C; item 4 (C4) — done
here; item 5 (warm-gate / intent-match retirements) — done at
deletion commit B; item 6 (the 0b verify-marker re-points) — done at
T-1c'.5. The checklist is fully discharged.

#### Per-rule disposition table

Retired with retirement records in place of the rule blocks (15 — the
records name the surviving carrier of each rule's load-bearing
content):

| Rule | Carrier named in the retirement record |
|---|---|
| `sched.actor.dispatch-decoupled`, `sched.dispatch.became-idle-immediate` | no dispatch pass / heartbeat intake left to pace; pull admission shape + `sweep_ready_cached`'s probed-generation bound |
| `sched.heartbeat.adopt`, `sched.heartbeat.phantom-drain` | binding durable from the fenced mint; establishment sweep + pod-terminal report for the lost-completion half; recovery/sweep store-probe adopt arms |
| `sched.freeze-detector`, `sched.dispatch.unroutable-system` | spawn-side visibility (`queued_by_system`, the AD2 `NoEligibleSource` arm, the controller's Job census) |
| `sched.admin.hung-node-detector` | `ctrl.nodeclaim.wedge-cluster` (OA2); `dead_nodes` empty until the 1d sweep |
| `sched.executor.session-epoch` | per-unary token↔intent binding + `exec_id`-keyed attempt rows + report idempotency |
| `sched.executor.deregister-reassign` (C2), `sched.executor.liveness-window`, `sched.executor.repair-precedence`, `sched.backstop.timeout` | the re-keyed one-classifier-wins discipline: report retry loop / pod-terminal report / establishment sweep arming, report+completion idempotency, no-attempt no-op, synthesized-verdict closes, platform liveness bounds (Job deadline, controller reap graces, establishment window), standby/fence inertness |
| `sched.termination.deadline-exceeded` | worker `TimedOut` path owns promotion/budget/terminal; controller deadline reports are charge-free second-installment fills; establishment sweep classifies the silent case |
| `sched.ephemeral.no-redispatch-after-completion`, `sched.assign.resource-fit` | the race/fit problems cannot form: no dispatch pass, pod sized from its own solved intent |

Re-stated (tracey bump; every impl/verify marker re-pointed in the
same commit): `sched.dispatch.soft-features+2`,
`sched.dispatch.fod-to-fetcher+2` (kind boundary at spawn:
`kind_for_drv` + pool kind filter + per-intent token),
`sched.dispatch.fod-builtin-any-arch+2`,
`sched.dispatch.substitute-complete-inline+2` (inline ready-set store
probe), `sched.executor.one-shot+2` (one intent / one open attempt /
one report / one exit; new impl marker at the builder pull loop),
`sched.reassign.no-promote-on-ephemeral-disconnect+5` (the requeue
chokepoint never bumps the floor), `sched.lease.generation-fence+3`
(AD4: the transaction-side fence — impl markers at the fenced mint,
the establishment charge and the admit_pull advisory check; the
builder latch markers removed; verify on the fault-leader regime
check), `sched.lease.claim-before-advertise+2` (AD4: claim-before-serve
— impl at the recovery claim-target site, verify on the fault-leader
regime check). Unchanged with prose touch-ups only (no bump):
`sched.executor.input-bounds+2` (the bounds-table prose now describes
the pull surface), `sched.timeout.per-build`,
`sched.actor.single-owner`, `sched.lease.standby-drops-writes`,
`ctrl.terminated.deadline-exceeded+3`, `gw.conn.cancel-on-disconnect+2`
(each judged not a bump — normative content unchanged — and recorded
in the introducing commit messages).

Marker re-points executed outside the rules above:
`sched.executor.liveness-window`'s three impl markers moved to the
rules that own the constants they sit on
(`ctrl.ephemeral.reap-orphan-running+3`,
`ctrl.ephemeral.reap-excess-pending+3`, `builder.heartbeat.rpc-timeout`);
`sched.executor.repair-precedence`'s impl on the completed→completed
guard moved to `sched.completion.idempotent`; the retry-kernel
BackstopTimeout fold arm keeps its behaviour as the historical-row arm
but no longer carries the retired rule's marker; gateway/xtask comment
references to retired rules re-pointed at their surviving bounds.

#### Prose brought to the as-built protocol

scheduler.typ's intro/responsibilities/algorithm walkthrough, the
actor message-flow figure, the failure-modes table and partition
prose, the Pull-Mode section (re-titled, coexistence framing dropped),
the backpressure intro, the no-double-count enforcement note, the
Leader Transition Protocol and recovery-sequence steps, and the
split-brain pricing now describe pull-only delivery, the durable
open-attempt row, the establishment sweep, and the transaction-side
fence; failure-modes.typ (system) and controller.typ/fetcher.typ/
gateway.typ cross-references likewise. sla-sizing.typ's `@alg-pool`
unhealthy-node arm cites `ctrl.nodeclaim.wedge-cluster` instead of the
retired `dead_nodes` aggregation (C4 closed).

#### Known leftovers (deliberately not taken by this batch)

- `sched.backpressure.hysteresis` still enumerates the
  BuildExecution/LogBatch intake arms; those surfaces formally leave
  at the 1d proto sweep and the rule should be re-stated there.
- `sched.retry.per-executor-budget+3` (retry campaign) still lists the
  correlation-TTL sweep and the backstop among its establishment
  vehicles; only the establishment sweep exists. Owner: the retry
  campaign's next touch of that rule.
- `sched.log.phase-binding` / `sched.log.path-length+2` still describe
  the stream-carried BuildPhase surface (their impl markers survive on
  the actor-side gate); 1d owns the surface's fate.
- builder.typ's stream-runtime rules (relay/drain/heartbeat family)
  are 1d scope (T-1d.1), untouched here.
- Observed spec↔code divergence flagged for the owner: the
  `sched.sla.reactive-floor+2` clause "controller-reported
  OomKilled/EvictedDiskPressure/DeadlineExceeded MUST call
  bump_floor_or_count" currently has no production caller — the legacy
  `ReportExecutorTermination` intake is a no-op stub and the
  `ReportAttemptOutcome` second-installment fill deliberately changes
  no floor or budget, so a pod-level OOM/eviction/deadline kill no
  longer promotes the floor (the worker-reported `CgroupOom`/`TimedOut`
  arms still do). Either the second-installment path gains the promote
  arms or the rule is re-derived; not adjudicated by this batch.
