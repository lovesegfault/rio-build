#import "/lib/rio.typ": *

#show: rio.with(domains: ("sys",))

This page documents the behavior of rio-build when individual components fail,
including cascading effects and recovery procedures.

// The §Component Failure Matrix table is intentionally omitted from this
// chapter — its rows shard to the per-component chapters in batch-4.

= Partial Failure Scenarios

== S3 Read-Only Degradation

If S3 writes fail but reads succeed (e.g., S3 rate limiting on PUTs):
- *Reads* (cache hits, binary cache serving): continue normally
- *Writes* (build output uploads): fail and retry. If all retries fail, the
  build output on the executor's overlay is lost
- *Impact*: New builds that produce outputs can't persist them, but builds that
  only consume existing cache entries work fine

== Network Partition: Scheduler ↔ Executors

- Executor pods notice nothing beyond failed unaries: an in-flight build keeps
  running and retries `ReportOutcome` until acked; a not-yet-pulled pod
  retries `PullAssignment` for as long as it lives (bounded by the Job's
  `activeDeadlineSeconds`)
- The scheduler resets nothing on the partition itself --- the open attempt is
  resolved by the report that eventually lands, by the controller's
  pod-terminal report if the pod dies first, or by the establishment sweep at
  deadline + report-slack
- Spawned-but-unpulled intents simply re-arm; the controller keeps its Job
  census and reaps only per its own graces

== Network Partition: Gateway ↔ Scheduler

- Gateway's `SubmitBuild` calls fail with `UNAVAILABLE`
- The gateway returns `STDERR_ERROR` to the Nix client, which retries (standard
  Nix behavior)
- Builds already submitted continue in the scheduler; the gateway can re-attach
  via `WatchBuild` after reconnection

== Scheduler Split-Brain Mitigation

Split-brain is closed by the fence/steal asymmetry --- a leader that cannot
renew self-fences at `SELF_FENCE_AFTER` (11s), `2 × FENCE_MARGIN` before any
standby's `STEAL_AFTER` (19s) steal threshold --- and bounded by the
generation-fenced authority transactions for the clock-pause residual:
- The leadership generation derives from the Lease's `leaseTransitions` count
  (the lease loop applies `fetch_max(transitions + 1)` to the shared in-memory
  `Arc<AtomicU64>`), floored during recovery by the durable PG history
  (`assignments` plus the `leader_generation_claims` ledger); a same-epoch
  re-acquire keeps its generation; the claim row is durable before recovery
  completes and work is served (#rref("sched.lease.claim-before-advertise"))
- Every authority-exercising transaction (the pull mint, the establishment
  charge, the synthesized close) carries the serving generation, persists it
  on the row it writes, and aborts when it is below the durable claims floor
  (#rref("sched.lease.generation-fence")), so a deposed leader can neither
  bind new work nor consume outcomes once the new leader's generation is
  durable
- Scheduler writes outside those transactions are not generation-fenced --- a
  deposed leader's in-flight PG writes will succeed. They are idempotent
  (INSERT ON CONFLICT, status-check UPDATEs), which limits the damage

== Cascading FUSE Cache Miss Storm

If rio-store is degraded (slow but not down), all executors' @fuse cache misses
queue up:
- Executors' FUSE read operations block, causing build sandboxes to stall
- The scheduler's @backpressure mechanism (actor queue depth > 80%) rejects new
  builds with `RESOURCE_EXHAUSTED`
- After 5 consecutive `ensure_cached` failures, the FUSE circuit breaker opens
  and `check()` returns `EIO` immediately (fail-fast). The existing
  `WAIT_DEADLINE` timeout on each fetch feeds the failure counter. See
  `r[builder.fuse.circuit-breaker]`.

= Guard Isolation Doctrine

Temporal guards — health probes, lease renewers, RPC deadlines, reap
timers — are only as trustworthy as the failure domain that runs them.
The live-053/054 incident pair measured every shape of the class in one
night: a 5s admin deadline delivered at \~18s whose Err arm spawned 257
unauthenticatable pods (guard expiry producing an irreversible action);
a `/healthz` endpoint served from the starved runtime it reported on,
read by the kubelet as death — five kills of a singleton (mute sensor);
and a lease loop whose compile-asserted scheduling premise (fence-check
latency ≤ one 5s renew tick) was violated 2.5--3× by
the same stall while its named backstop — the consumed generation —
did not exist controller-side. The five rules below make the class
structural. Measured economics for the isolation rule: shared-runtime
guard skew equals the stall length (4.9--9.9s observed); a dedicated
OS-thread watchdog in the same process holds 69--97µs — five
orders of magnitude for one thread and \~24--80 KB.

#r("sys.guard.domain-isolation")[
  A guard MUST NOT share the failure domain of the component it guards:
  the timer, the sensor, and the decision path that conclude "failed"
  MUST remain schedulable when the component is not.
]

In-process form: a dedicated `current_thread` runtime on its own OS
thread hosting the health endpoint, the lease renew loop, and the skew
sentinel, with cross-domain state lock-free (atomics/watch) — any lock
shared across domains makes the isolation fictitious. Out-of-process
enforcers (kubelet, the Job controller's `activeDeadlineSeconds`, the
PG advisory lock) satisfy the rule by construction. Honest residual:
a dedicated thread still dies with the process (the kubelet covers
that face) and still lags under cgroup-level starvation — the skew
sentinel (below) is what attributes that case.

#r("sys.guard.brownout-only")[
  Expiry of any guard that MAY share the guarded domain MUST produce
  only delay or refusal. An irreversible effect — spawn, kill, lease
  steal, scale commitment, data discard — MUST require positive
  evidence and MUST NOT be a timeout's Err arm.
]

The verify population for this rule is the controller timeout-Err
census: every `tokio::time::timeout` Err arm enumerated and classified
`{delay, refusal, irreversible}`, with zero rows in the irreversible
class. A new timeout whose Err arm commits an irreversible effect is a
census red, not a code-review judgment call. The live-053 shape — an
expired token mint substituting an empty map and spawning anyway — is
the incident this rule generalizes; its point fix (no token evidence
→ zero spawns this tick) predates the rule.

#r("sys.guard.kill-wired-isolated")[
  A kill-wired surface (liveness) MUST be served from a failure domain
  that stays responsive under worst-case admitted load, or be a blind
  aliveness signal (tcp/process check). A shed-wired surface
  (readiness) MAY share the working domain.
]

This was repo folklore in three places before it was a rule: the
scheduler chart's tcpSocket-probe rationale
(`infra/helm/rio-build/templates/scheduler.yaml`, the standby block —
liveness on `is_leader` would kill the standby), the builder's probe
rationale (`rio-controller/src/reconcilers/pool/job.rs`, I-114), and
the controller counterexample that killed itself in live-054 (an axum
`/healthz` on the shared runtime — the negation of this rule,
repaired by the guard-runtime split). The
controller probe chart block (`controller.yaml`, the D-054-1a
rationale: readiness 5s/5s, liveness 10s/10s with failureThreshold 6,
startupProbe 2s/10s×30) is the chart-side realization.

The builder's landed posture (live-056-b) is the rule's dual-face
exemplar, KILL-WIRED BY EXIT: liveness probes are deliberately ABSENT
on executor pods (I-114's liveness half stands — a CPU-pegged build
must never be probe-SIGKILLed), and the kill-wired surface is the
process's own exit instead — a cold-start connect that exceeds its
typed budget exits NONZERO, so the platform's escalation alphabet
(Job backoff, CrashLoopBackOff) carries the failure without any
probe sharing the build's failure domain. READINESS is shed-wired
and MAY share the working domain per this rule: the `/servingz`
serving-state endpoint (200 iff the serving file exists — written
post-connect, pre-first-pull) feeds the Job's readiness probe, and
steady-state reconnects keep the infinite posture (a claim-state
holder must not die on upstream outage), so established pods do not
flap Ready on a correlated upstream blip.

#r("sys.guard.correlated-readiness-brownout")[
  On a horizontally scaled fleet, a shed-wired readiness surface MUST
  NOT gate on a dependency whose failure is correlated across the
  fleet; a correlated-dependency brownout MUST surface as in-band
  refusal or degraded service, never as fleet-wide endpoint removal.
]

The store fleet is the motivating population: every replica shares the
same PG and the same object store, so a readiness check that consulted
either would convert a dependency brownout into simultaneous
NotReady across the fleet — zero endpoints, a total outage strictly
worse than the brownout it reported. The store chart's blind tcp
readiness plus typed in-band refusals (budget parks, watchdog
takeovers) is the conforming shape.

#r("sys.guard.scheduling-premise")[
  Every formally derived guard deadline MUST state its
  scheduling-latency premise as an enforced budget. Where the premise
  can be violated, an isolated enforcement domain or a
  transaction-side fencing token MUST exist; a lease whose generation
  has no consumer MUST NOT be the sole guard on a mutually exclusive
  effector.
]

The lease NeverDual derivation assumes fence-check latency ≤
one renew tick; the live-054 stall violated that premise on the shared
runtime. The two lawful answers compose: the guard-runtime split
restores the premise for scheduler-induced stalls, and the consumed
generation (minted at acquire, stamped on NodeClaim mutations and
evidence Acks, rejected stale at the consumer side) covers the
restart-overlap and cgroup-starvation residue the split cannot reach.

#r("sys.guard.skew-sentinel")[
  Every long-lived runtime MUST host a monotonic-clock skew sentinel
  on an isolated thread, exporting executor-scheduling delay and
  capturing stacks past a threshold, so a postmortem can distinguish
  guard-fired-late from component-failed.
]

The sentinel is what makes the isolation rule falsifiable in
production: during an admitted overload the main runtime's skew gauge
reads the stall while the sentinel's own skew stays O(ms). It is also
the named prerequisite for sizing the controller FFD chunk quantum —
the 054 freeze primitive (mass-blocking polls vs a shared sync
primitive vs cgroup starvation) is unattributable without it.

= Epilogue Doctrine

A recurring failure class is the fact proven at ENTRY whose effect
lands AFTER awaits, runtime drops, or abort sites: a shutdown epilogue
aborted by its own host runtime, a leadership check stale by the time
the durable write lands, an abort that destroys work a drain would
have saved. The doctrine rules below give deposition, shutdown, and
supersession TYPED drain and fence protocols — an entry check alone
(or an entry check plus a second entry check) is never a conforming
implementation of any of them.

#r("sys.epilogue.drain")[
  A runtime that hosts post-cancellation epilogue work MUST NOT key
  its lifetime to the same cancellation token as its tasks. Spawning
  an epilogue-bearing task MUST return a drain handle carrying the
  epilogue's bounded budget, the host root MUST be the handle's only
  drain site, and the root's shutdown path MUST traverse exactly the
  states cancelled → draining (every adopted handle awaited, each
  bounded by its own budget) → dropped — the runtime drops only after
  the drain state completes, and a budget expiry abandons that one
  epilogue (logged) without re-entering it.
]

The transition algebra is deliberately small: *running* →(token
cancelled)→ *draining* →(all handles drained or expired)→ *dropped*,
with no edge from draining back to running and no edge that skips
draining. The controller guard is the founding instance: its lease
loop owes a graceful-release `step_down()` PATCH after cancellation,
and the discarded-handle form (bug_118) left the dead pod holding the
nodeclaim-pool lease for the full steal threshold on every rollout —
while every loop-level witness stayed green, because the release
DECISION was proven one level below the hosting wiring that aborted
its EXECUTION. The drain budget is derived where the epilogue lives
(the lease crate exports it from its own renew constants), so host
and epilogue cannot drift independently.

#r("sys.epilogue.reconcile")[
  A periodic actor whose arms mint reconciliation obligations (dirty
  marks, pending patches) MUST make the reconcile its tick's
  STRUCTURAL TAIL: every arm exit flows through it by construction —
  branch arms compose as alternatives ahead of the tail, never as
  early exits past it — and an arm-local control-flow escape
  (`continue`, early return) that skips the tail is a structural
  defect held out by a machine census over the loop body, not by an
  enumerated arm list in a comment.
]

The lease loop is the founding instance (merged_bug_072): the
step-down arm's `continue` bypassed the hoisted leader-marks
reconcile — the hoist's own comment enumerated the arms it covered
and the enumeration silently missed the arm added later, leaving dual
leader labels on the load-bearing Service for one renew interval
after every deliberate handover. The structural form replaces the
enumeration: the step-down arm and the renew round are two branches
of one conditional whose join IS the reconcile, and the zero-continue
census pins the loop body. The companion law is conflict-handling
unification: every 409 on the lease plane resolves through ONE
evidence-based re-read resolver (a conflict proves resourceVersion
movement, never holder change), so a retired optimistic inference
cannot coexist with the deferral model one function over.

#r("sys.epilogue.supersession")[
  An owner that replaces a live task with a successor for the SAME
  output channel MUST pass a typed abort disposition to the
  superseded task's disclosure machinery BEFORE aborting it:
  supersession marks must-discard (the successor owns the channel —
  Drop-time disclosure of the dead task's withheld state is
  forbidden: no markers, no stale payload), terminus keeps
  must-disclose, and the supersession site bound-joins the superseded
  task before the successor produces output. A context-free Drop
  backstop that cannot distinguish the two dispositions is never a
  conforming implementation of either.
]

The gateway log-tail relay is the founding instance (bug_168): the
re-dispatch path aborted the old execution's relay with no join and
no disposition, so the aborted relay's drop-disclosure backstop —
correct at TERMINUS (merged_bug_111) — fired in the supersession
context too, splicing the superseded execution's withheld lines plus
a false "durable log gap" marker into the retry's client-visible
stream at an arbitrary later poll. The repair is the protocol in the
rule's order: mark must-discard, abort, hand the join to the
successor whose FIRST act is the bounded join — the successor
structurally cannot produce output before the predecessor unwound,
and the disclosure law survives untouched on every terminus path.
The chunk-stamping alternative (tag relayed chunks with the
execution id and filter at the consumer) is recorded REJECTED: it
leaves the splice window open until the filter and obligates every
present and future consumer; the typed disposition kills the splice
at its source.

= Liveness Duals

A safety latch is half a design. Budgets, caps, masks, refusal lanes,
and advisory dispositions all REMOVE behavior from a state — and a
state whose behaviors have all been removed is absorbing unless some
event mints one back. The round-11 corpus carried five independent
instances of the same composition defect: two individually-sound
safety rules whose conjunction left a state with no exit edge (a
gave-up latch whose every reset witness required the pods the latch
forbids; an exhausted outbox row whose re-decision was swallowed by
its own idempotency arm; a capacity dead band whose advisory verdict
never stepped any terminal budget). None of the latches was wrong;
each was missing its dual.

#r("sys.liveness.exit-edge")[
  Every absorbing or latched state MUST ship its exit edge in the
  same change that ships the latch: the close MUST name the reset or
  terminal event AND demonstrate that the event is reachable from
  inside the latched state under that state's own invariants — an
  exit edge gated on evidence the latch itself forbids, or on a
  re-decision the latch's own idempotency arm swallows, is not an
  exit edge. Advisory (non-poisoning) dispositions on a retry loop
  MUST be bounded by a reachable designed terminal; an
  advisory-forever strand is an absorbing state and rejects the same
  way.
]

The founding instance is the merged_bug_016 capacity dead band: the
`(ceiling − pad, ceiling]` band's OverCap verdicts were ADVISORY by
design (correct for their intended population, ≤300s config-mirror
skew that self-heals), but the band made the population permanent —
an infinite no-pod/no-NodeClaim/no-retry requeue of exactly the
largest builds, with the designed bounded at-cap poison terminal
unreachable because the at-cap dispatch itself rendered an
unhostable container. The exit-edge proof for that close is
two-armed: the gate/funnel adjunction makes the band EMPTY (no state
enters), and the at-cap retry terminal is re-proven REACHABLE (the
pinned at-cap attempt renders a hostable container, runs, and its
failures are counted against the bounded retry budget). Sibling
instances land with their own closes (the GC outbox reset edge, the
gave-up latch's pod-free decay) and append their rows below this
doctrine; the per-latch exit-edge obligation census is the standing
machine enforcement.

== Instance Register

Each landed liveness-dual close appends its row here: the latch, the reset
or terminal event, and the reachability witness — under the doctrine above
(#rref("sys.liveness.exit-edge")).

- *GC delete outbox (store)*: a `pending_s3_deletes` row that exhausts its
  retry budget (`attempts >= MAX_ATTEMPTS`) is parked outside the drain's
  partial index --- a latch with no in-band retry. Exit edge: the next fresh
  collect decision for the same object resets the budget through the
  enqueue's guarded conflict arm (#rref("store.gc.outbox-reset")); the reset
  event is mintable from inside the latched state because collect decisions
  derive from tombstone state, which the latch does not gate. Reachability
  witnessed end-to-end (injected-outage exhaustion through the production
  drain, then a production re-decision reaching execution past backend
  recovery), with the dedup face co-witnessed: in-budget rows stay swallowed.
- *Pool gave-up latch (controller)*: the verdict-free respawn record at the
  give-up threshold (`RESPAWN_GIVE_UP_DEATHS`, pool/candidate.rs) is
  expiry-immune by design while every reset witness in the prior alphabet
  required pod/attempt evidence the latch itself forbids (gave-up partitions
  spawnable; no pod ever exists to mint from) --- an absorbing state minted
  by two individually-sound rules. Exit edge: the fresh-demand-epoch decay
  --- a wanted intent presenting a strictly newer `resubmit_cycle` than the
  record last observed decays it at the demand seam (the one lane the latch
  does not gate) under the typed `GaveUpReset` receipt; deaths under the new
  epoch re-latch at the full budget
  (#rref("ctrl.pool.respawn-backoff")). Reachability witnessed end-to-end
  through the production fold (give-up, then resubmission, then a same-tick
  spawn) with the quint twin pair pinning both faces (the as-built module
  falsifies the liveness; the fixed module holds it with the re-latch
  safety).

- *Ack-plane whole-request refusal (scheduler)*: the `AckSpawnedIntents`
  whole-request refusal composed with the controller's durable Job-template
  annotation --- `assemble_re_acks` re-derives the poisoned row every tick,
  the consumer refused the whole request on one undecodable entry, and the
  redelivery loop became an absorbing state blacking out every evidence
  plane in the request with no self-heal. Exit edge (double --- defense in
  depth across the version-skew window neither side alone covers): the
  producer round-trips the typed capacity parser and degrades a poisoned
  row to the no-echo skip lane (the mint dies at the source), and refusal
  granularity drops to per-plane at the consumer --- one poisoned entry
  refuses its plane while sibling planes apply, lawful only with the
  redelivery-idempotency obligation discharged per plane (epoch gate,
  upsert, wholesale rebuild; the verdict plane asserted out of the
  redelivery loop) --- so the loop converges instead of wedging
  (#rref("sched.sla.ack-validate-then-commit")). Reachability witnessed by
  the sibling-planes-apply red across three redeliveries and the
  state-equality cell against one clean apply.

= Linear Obligations

#r("sys.obligation.linear-discharge")[
  An obligation computed MUST be discharged or refused through its own
  type: acknowledgements, trims, deletes, retransmit copies, release
  duties, and disclosure duties are LINEAR resources --- minted as a
  value whose only exits are its typed discharge methods, consumed
  exactly once by construction, and droppable only through an explicit
  typed refusal or a named Drop backstop. An obligation returned as
  advisory data that a call site may read, discard, partially execute,
  or lose across an await rejects.
]

Ownership is the enforcement: in Rust, a value that must be moved into
its discharge cannot be silently ignored at a second call site, so the
law's quantifier ("every path discharges") equals the enforced
population by type rather than by enumeration. The round-12 founding
instances are the two coupled-reader highs --- a destructive trim
consuming an ack denominated in a measure weaker than the trim's
assumption, and computed verdicts discarded as plain data. Each landed
linear-obligation close appends its instance row below; advisory-data
shapes (a discarded clearance closure argument, a wildcard struct
pattern over a verdict, a take-then-await window) are review rejects
under this rule.

== Instance Register (Linear Obligations)

- *Builder retransmit copy (store to builder)*: the retransmit buffer
  is the builder's only copy of un-acked log lines; its sole
  destructive exit is the ack-driven trim, so the trim is the copy's
  typed discharge and its argument must be a witnessed
  contiguous-prefix frontier (#rref("store.log.frontier-denominated")).
  The producer-side clamp makes the existing prefix-pop sound; the
  builder-side witness pins the discharge against the producing
  formula, and the store-side carrier census keeps every producer
  routed through it.

- *GC batch authority (store)*: the per-batch hold re-authorization verdict
  was advisory data --- a unit `Authorized` arm a body could match and
  ignore (the log sweep discarded its whole lane clearance as
  `move |_clearance|`), and two of six multi-batch destructive bodies
  shipped outside the hand-wired demand, defeating the operator's emergency
  stop mid-pass (bug_084, merged_bug_006). The obligation is now the linear
  `BatchAuthority` token (#rref("store.gc.batch-authority")): minted solely
  by the boundary consult, non-clonable, demanded BY VALUE at every
  destructive sink, spent exactly once per batch --- a destructive batch
  outside an authorized boundary does not compile, and the body population
  the demand quantifies over is DERIVED by the destructive-body census
  (R31), never author-listed. Witnessed mid-pass at every derived body:
  the path sweep, the log sweep, the collect drain, the post-drain reap,
  the per-row S3 drain, and the orphan scan each stop at their next batch
  boundary under a hold landed between committed batches, and release
  drains the remainder (suspension never converts into lost work).
