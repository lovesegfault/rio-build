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
liveness on `is_leader` would kill the standby), the builder's
no-probes rationale (`rio-controller/src/reconcilers/pool/job.rs`,
I-114: with `parallelism: 1` a readiness probe adds nothing the pull
loop's own deadline does not), and the controller counterexample that
killed itself in live-054 (an axum `/healthz` on the shared runtime —
the negation of this rule, repaired by the guard-runtime split). The
controller probe chart block (`controller.yaml`, the D-054-1a
rationale: readiness 5s/5s, liveness 10s/10s with failureThreshold 6,
startupProbe 2s/10s×30) is the chart-side realization.

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
