#import "/lib/rio.typ": *
#show: rio.with(domains: ("sched", "scheduler", "admin"))

Receives derivation build requests, analyzes the @dag, and exposes the work
as spawn intents; one-shot executor pods spawned for those intents pull their
assignment and report their outcome over idempotent unaries.

= Responsibilities

- Parse derivation graphs from gateway requests
- Query rio-store for cache hits (already-built outputs)
- Compute remaining work graph (subtract cached nodes)
- Critical-path priority computation (bottom-up: priority = own_duration +
  max(successor priorities)); recomputed incrementally on completion by walking
  ancestors with dirty-flag propagation
- Duration estimation from the @sla model's `T_min` (per-`(pname, system,
  tenant)` fit; see §Duration Estimation)
- Resource-aware scheduling: match derivation `requiredSystemFeatures` and
  resource needs to executor capabilities (subset matching: all required
  features must be present on the executor)
- Auto-pin live build inputs: on dispatch, `pin_live_inputs` writes the
  derivation's input @closure to the `scheduler_live_pins` table (used by
  rio-store's GC mark phase as a root seed); unpinned on completion
- Proxy `AdminService.TriggerGC` to rio-store, first collecting live-build
  output paths via `ActorCommand::GcRoots` and forwarding them as `extra_roots`
- Priority queue with inter-build priority (CI > interactive > scheduled) and
  intra-build priority (critical path)
- @ifd prioritization: builds that block evaluation get maximum priority
  (detected by protocol sequencing --- `wopBuildDerivation` arriving before
  `wopBuildPathsWithResults` on the same session)
- CA early cutoff: per-edge tracking --- when a CA derivation output matches
  cached content, mark that edge as cutoff and skip downstream only when ALL
  input edges are resolved
- Work requeue: when an executor pod dies or its attempt is closed without a
  worker report, requeue the still-wanted derivation for a fresh pod (the
  verdict arms, the synthesized-verdict close, and the establishment sweep all
  converge on the same requeue chokepoint). _Slow-executor speculative
  reassignment (actual_time > estimated_time × 3) is not currently
  implemented._
- @poison-derivation tracking: mark derivations that fail on 3+ different
  executors; auto-expire after 24h (see the error taxonomy)

= Concurrency Model

#r("sched.actor.single-owner")[
  The scheduler uses a *single-owner actor model* for the in-memory global DAG.
  A single Tokio task owns the DAG and processes all mutations from an `mpsc`
  channel:
  - `SubmitBuild` → DAG merge command
  - `PullAssignment` → admission + fenced attempt mint command
  - `ReportPullOutcome` / `ReportAttemptOutcome` → node completion +
    downstream release / attempt-row fill command
  - `CancelBuild` → orphan derivations command
  - CA early cutoff → edge cutoff + potential cancellation command
]

gRPC handler tasks send commands to the @dag-actor and `await` responses. This
eliminates lock contention, makes operation ordering deterministic, and
simplifies reasoning about correctness. PostgreSQL writes are batched and
performed asynchronously by the actor.

*Retired (1c' spec sweep; machinery deleted by deletion commits A/B):*
`sched.actor.dispatch-decoupled`, `sched.dispatch.became-idle-immediate`.
Both rules paced the stream-era `dispatch_ready` pass against heartbeat
volume (the I-163 mailbox storm: coalesce heartbeat-driven dispatch to one
pass per Tick, with a capped inline carve-out for 0→1 capacity edges so a
freshly-spawned builder did not idle a full tick). There is no scheduler-side
dispatch pass and no heartbeat intake left to pace: work delivery is the
pod-initiated `PullAssignment` unary, and the surviving Ready-set store
short-circuit (`sweep_ready_cached`) runs from the same state-change events
plus once per Tick, bounded by `probed_generation` rather than by a dispatch
cap. The actor-mailbox protection that motivated the pacing is carried by the
unary admission shape itself --- a pull is one bounded actor turn, and
backpressure surfaces as retried pulls (#rref("sched.executor.pull-transaction")).

#r("sched.admin.snapshot-cached")[
  `AdminService.ClusterStatus` reads a `watch::channel` snapshot that the actor
  publishes once per `Tick`, instead of round-tripping
  `ActorCommand::ClusterSnapshot` through the mailbox. The handler itself is
  \~37µs; queuing it behind a saturated mailbox (I-163: 9.5k commands) made it
  time out at exactly the moment the controller's reconcile loop and operators
  need a reading. The cached value is at most one Tick (\~1s) stale.
]

= Scheduling Algorithm

*Implemented:* critical-path priority, per-derivation
SLA sizing (`solve_intent_for` → `(cores, mem, disk, deadline)` per
#rref("sched.admin.spawn-intents")), pull-mode delivery with the
input-closure prefetch carried on the assignment payload, @leader-election
via Kubernetes Lease gated on `RIO_LEASE_NAME`,
`AdminService.ClusterStatus`/`ListOpenAttempts`, Pool @crd + one-shot Job
reconciler. Interactive builds get a +1e9 priority boost (dwarfs any
critical-path value).

```
1. Receive derivation DAG from gateway
2. Merge into global DAG (dedup by store path / derivation hash; see Multi-Build DAG Merging)
3. For each derivation in DAG:
   a. Query rio-store: is output already in CAS? (cache hit)
   b. For CA derivations: check content-indexed CAS for matching output
4. Compute remaining build graph (nodes without cached outputs)
5. If empty -> full cache hit, return results immediately
6. Compute critical path priorities (bottom-up traversal)
7. For each ready node (all deps satisfied):
   a. Solve per-derivation (cores, mem, disk, deadline) via the SLA model
      (solve_intent_for; ADR-023). The controller spawns one-shot pods sized
      to the same SpawnIntent.
   b. Publish a SpawnIntent for the Ready derivation (kind from is_fixed_output,
      features/systems filters server-side); the controller spawns one Job per
      intent with a per-intent HMAC executor token and the solved resources.
   c. The pod pulls its assignment (PullAssignment): one fenced transaction
      validates the token-intent binding, mints exec_id, and returns the
      WorkAssignment. The payload carries an HMAC-SHA256-signed assignment token
      (Claims: executor_id, drv_hash, expected_outputs, is_ca, expiry_unix). The
      store verifies the token on PutPath and rejects uploads for paths not in
      expected_outputs.
8. As builds complete (reported via ReportOutcome, with the controller folding
   pod/Job terminal status through ReportAttemptOutcome):
   a. Upload output to rio-store (executor does this before reporting)
   b. For CA derivations: check if output content matches any existing CAS entry
      - If match -> mark that specific edge as "cutoff"
      - For each downstream node, check if ALL input edges are in one of:
        (a) cached, (b) cutoff, (c) rebuilt but content-hash matches old
      - Only skip a downstream node if ALL its input edges meet these conditions
      - If a downstream node is already running when cutoff is detected: let it finish
        and discard the result (see Preemption below)
   c. Release newly-ready downstream nodes
   d. Record actual (cores, mem_peak, disk_peak, wall) into build_samples for SLA refit
   e. Recompute priorities incrementally: walk up ancestors only, using dirty-flag
      propagation -- only ancestors whose max-successor-priority changed need updating
9. On failure: classify error (see ref/errors.typ), apply retry policy, reassign or mark as failed
```

#r("sched.merge.toctou-serial")[
  The DAG merge and subsequent cache check MUST be performed inside the DAG
  actor (serialized), not by the gRPC handler before sending the merge command.
  A cache check performed by the gRPC handler races with concurrent merges ---
  another build may complete a shared derivation between the handler's cache
  check and the actor's merge, leading to duplicate work. By performing cache
  verification after merge inside the actor, the check reflects the latest
  state.
]

#r("sched.completion.output-membership")[
  `handle_completion` MUST drop any worker-supplied `BuiltOutput` whose
  `output_name` is not in the derivation's scheduler-trusted `output_names`
  (parsed from the `.drv` at DAG-merge time), and MUST drop duplicates by
  `output_name`. Builders are untrusted; without this filter a compromised
  worker reporting on its own assigned drv could write arbitrary worker-chosen
  paths to `path_tenants` (pinning them against GC) and stall the actor via the
  sequential `insert_realisation` loop. After filtering, `built_outputs.len() ≤
  output_names.len()`. Dropped entries increment
  #(refs.metric)("rio_scheduler_undeclared_built_output_total").
]

*Retired (1d proto sweep --- the stream-carried BuildPhase surface):*
`sched.log.phase-binding` and `sched.log.path-length` normed the actor-side
binding gate and the recv-loop length bound for `BuildPhase` updates arriving
on the `BuildExecution` stream. That ingestion path no longer exists: the
stream RPC is gone, the `ForwardPhase` actor command and its
`handle_forward_phase` gate (and the phases-rejected counter that gate
incremented) are deleted with it, and no scheduler code accepts a worker-supplied
phase update any more --- the threat the rules guarded (attacker-controlled
text injected into another tenant's progress display via a fabricated
`derivation_path`) is unrepresentable without the intake. The dashboard/nom
phase column derives from attempt/derivation status (the OA4 disposition);
the `BuildEvent.phase` arm remains in the proto as a producer-less display
event for a possible future carrier, and any such carrier must re-introduce a
binding gate with the same shape (status precondition + executor match). The
log-batch half of the old length bound lives on at rio-store
(#rref("store.log.ingest-bounds")).

#r("sched.executor.input-bounds+2")[
  Every worker-supplied string field on the `ExecutorService` surface MUST be
  either length-bounded before it is accumulated (persisted to PostgreSQL,
  buffered in a broadcast ring, rendered to a client terminal, or stored in
  long-lived actor state) or validated against a scheduler-trusted set, and
  every worker-supplied numeric field that the scheduler folds into persisted
  row metadata or per-execution ordering state MUST be either validated at
  ingestion or consumed only through total (non-wrapping, non-panicking)
  arithmetic. Fields that are decoded and dropped without accumulation MAY
  remain bounded only by the gRPC message-size cap, and MUST be enumerated as
  such at the bounds-constant block in `executor_service.rs`.
]

The enumerated surface is the pull-mode pair: `PullAssignment` carries
`intent_id` and (for clients that cannot set per-call metadata) the HMAC
`executor_token`, both rejected whole past their bounds — an over-long
intent id can never name a real intent, and the token bound only caps how
much input the verifier hashes before rejecting garbage. `ReportOutcome`
carries the `CompletionReport`; its identifier/label fields are nulled and
its `error_msg` truncated rather than the report being rejected, because a
lost completion strands the derivation in `Running`, and its `drv_path` is
dropped before the actor entirely — `exec_id` names the attempt, so there is
no path-resolution step left for an oversized path to abuse. (The stream-era
`BuildExecution`/`Heartbeat` RPCs were removed by the proto sweep; no other
worker-facing surface exists.)

`CompletionReport.final_line_count` is the motivating numeric field: the
scheduler folds it into the `drv_executions` row that rio-store's log
completeness predicate (#rref("store.log.completeness-gate")) compares the
chunk manifest against, so a worker-supplied value past `i64::MAX` would wrap
negative under a bare cast and make the contiguity check vacuously true ---
vouching for an empty log as complete and sealing it against the very replay
that could complete it. The bind site converts via `try_from` and records
out-of-range (and zero, the proto's not-reported sentinel) values as SQL
`NULL`, the same degradation as every other unusable report field.
(`BuildLogBatch` no longer transits the scheduler; its line-number ordering
and magnitude are bounded at rio-store's ingest path per
#rref("store.log.ingest-bounds").) The `CompletionReport` resource telemetry persisted to `build_samples`
(`peak_memory_bytes`, `peak_cpu_cores`, the duration derived from the
`BuildResult` start/stop timestamps) and the `CompletionReport.final_resources`
cgroup snapshot folded into the same row (`cpu_limit_cores`,
`cpu_seconds_total`, `peak_io_pressure_pct`, `peak_disk_bytes`) comply via
validation at the actor's sample-record step in `completion.rs`: integer
magnitudes clamp at `i64::MAX`, worker-supplied floats are kept only when
finite and inside their physical domain (cores limits in
`(0, MAX_CORES_HARD]`, non-negative CPU-seconds, pressure in [0, 100]) and
are recorded as not-reported (SQL `NULL`) otherwise, and the duration sanity
bound rejects out-of-order or >30-day timestamps. The bounds reject the
structurally impossible, not the merely implausible — an in-domain reading is
still self-reported telemetry. The remaining `final_resources` counters are
decoded and dropped without being folded into the row and are enumerated as
`n/a` in the bounds table.

#r("sched.merge.exec-correlation+7")[
  The scheduler MUST set `build_derivations.exec_id` for every interested
  build that has not already recorded an observation for that derivation
  when a derivation that has been dispatched (and therefore has an
  `exec_id` recorded for it) reaches a terminal state through a path
  where an execution actually ran: `Completed` (success or recovery's
  orphan adoption), `Poisoned` (permanent failure), `Cancelled` reached
  from `Assigned`/`Running`, and any terminal reached by a derivation
  whose prior, reset execution left a stamped log buffer (the
  build-cancel sweep's `Cancelled`/`DependencyFailed` arms, the
  failed-substitute revert to `DependencyFailed`, and the
  dependency-failure cascade's `DependencyFailed` ancestors). The column
  MUST stay `NULL` for cache-hit `Completed`, cascade-swept
  `DependencyFailed` ancestors that never left a stamped log buffer,
  `Skipped`, non-terminal derivations, and any other
  terminal where no execution was ever observed (nothing to correlate).
  A `(build, derivation)` observation is written exactly once --- a
  post-completion reset and re-execution of the derivation inside the
  terminal cleanup window MUST NOT revise an observation the build
  already recorded.
]

`Failed` is _not_ a terminal status in the actor's state machine
(`is_terminal()`); it is the transient retry intermediate
(`Running → Failed → Ready`). The shared chokepoint is `terminal_log_epilogue`, called from
`handle_success_completion` (`Completed`), `terminal_failure_epilogue`
(`Poisoned`, timeout-exhausted `Cancelled`, and the failed-substitute
revert to `DependencyFailed`), and
`cancel_build_derivations` (any path that cancels in-flight derivations:
user cancel, per-build wall-clock timeout, fail-fast, top-down substitute
fail), and recovery's `adopt_orphan_completion` (an orphaned assignment
whose outputs are found in the store --- the execution completed while the
scheduler was down, and an ex-leader re-acquiring the lease may still hold
its unflushed log tail) --- each of which implies the worker ran the build.
The
not-yet-dispatched arms of the same cancel sweep (`Queued`/`Ready`/`Created` →
`DependencyFailed`, `Substituting` → `Cancelled`) and the dependency-failure
cascade's swept ancestors call the chokepoint only
when a prior execution was reset and left a stamped log buffer
(`has_buffered_exec_log`, via the shared gated form
`finalize_buffered_exec_log`); the never-dispatched majority skip it.
Inside the epilogue, the carrier
resolution is `exec_id_for_terminal`, which reads `state.exec_id` (set by
`assign_to_worker`, recoverable from `assignments.exec_id` after a leader
failover for a currently-assigned derivation, dropped by `transition()`
when the node is reset out of a terminal) and falls back to the
`LogBuffers` ring-buffer entry's stamped
`exec_id` --- covering poison-while-Ready, where `reset_to_ready` clears
`state.exec_id` but the buffer entry retains the disconnected execution's
stamp through the disconnect→re-dispatch window. The epilogue skips all
three steps (seal, flush, correlate) when *both* carriers are `None`, which
covers every never-dispatched terminal regardless of its enum value.

The build↔exec correlation lets the dashboard's build view fetch the *exact*
log a build observed (`LogService.TailLog(drv, exec_id)`) instead of falling
back to "latest execution for this derivation" --- which can differ if the drv
was rebuilt by a later build. The write is best-effort fire-and-forget: a
failed write degrades the dashboard view, not the build outcome. It runs in
`record_exec_correlation`, called from the same per-build fan-out as
`trigger_log_flush`. The UPDATE is guarded with `AND exec_id IS NULL`
(write-once per `(build, derivation)`): a build's `interested_builds`
membership outlives its completion by the terminal cleanup delay, and a
derivation reset and re-executed inside that window must not overwrite the
observation the finished build recorded at its own completion. The guard is
SQL-side rather than an actor-side terminal-state filter because the build
that a derivation's completion finishes is already terminal in the actor's
map by the time the correlation fires (build completion precedes the log
epilogue in the success path).

#r("sched.completion.idempotent")[
  A `CompletionReport` for an already-completed derivation is accepted and
  ignored (no-op). The actor's state machine treats `completed → completed` as
  an idempotent transition. This handles duplicate reports caused by executor
  retries during scheduler failover, network retransmissions, or race
  conditions with CA early cutoff.
]

#r("sched.tenant.resolve+2")[
  The scheduler's `submit_build` handler derives the tenant UUID primarily from
  the interceptor-attached `TenantClaims.sub` (see #rref("sched.tenant.authz")).
  When no claims are attached (dev mode, no JWT pubkey configured), it falls
  back to resolving `SubmitBuildRequest.tenant_name` --- captured by the gateway
  from the server-side `authorized_keys` comment --- via `SELECT tenant_id FROM
  tenants WHERE tenant_name = $1`. Unknown tenant name → `InvalidArgument`.
  Empty string → `None` (single-tenant mode, no PG lookup). This keeps the
  gateway PostgreSQL-free --- preserving stateless N-replica HA.
]

#r("sched.tenant.authz+2")[
  SchedulerService RPCs (`SubmitBuild`, `WatchBuild`, `QueryBuildStatus`,
  `CancelBuild`) MUST derive tenant identity from the interceptor-attached
  `TenantClaims.sub`, not from any proto body field. When a JWT pubkey is
  configured and no `TenantClaims` are attached (header absent --- the
  interceptor is permissive-on-absent so co-hosted ExecutorService callers
  reach the port), the handler MUST reject with `UNAUTHENTICATED`. When
  `TenantClaims` ARE attached, `require_tenant` MUST additionally reject with
  `UNAUTHENTICATED` if `claims.jti` is present in `jwt_revoked` (see
  #rref("gw.jwt.verify")) --- this is the scheduler-level revocation chokepoint
  and applies to all four RPCs, not only SubmitBuild. `WatchBuild`,
  `QueryBuildStatus`, and `CancelBuild` MUST additionally verify the target
  build's `tenant_id` equals `claims.sub` and reject with `PERMISSION_DENIED`
  on mismatch. `ResolveTenant` is exempt: the gateway calls it during SSH key
  auth before a JWT exists.
]

#r("sched.store-client.reconnect")[
  The scheduler's gRPC channel to rio-store MUST use lazy connection
  (`Endpoint::connect_lazy`) with HTTP/2 keepalive so store pod rollouts do not
  require a scheduler restart. On `Unavailable`, the channel re-resolves DNS
  and reconnects transparently.
]

#r("sched.gc.path-tenants-upsert")[
  On build completion, the scheduler upserts `(store_path_hash, tenant_id)`
  rows into `path_tenants` for every output path × every tenant whose build was
  interested in that derivation (dedup via `interested_builds`). This is
  best-effort: upsert failure warns but does not fail completion --- GC may
  under-retain a path if the upsert fails, but the build still succeeds. The
  upsert is `ON CONFLICT DO NOTHING` (composite PK on `(store_path_hash,
  tenant_id)`); repeated builds of the same path by the same tenant are
  idempotent.
]

#r("sched.poison.ttl-persist")[
  `poisoned_at` is persisted to `derivations.poisoned_at TIMESTAMPTZ` when the
  poison threshold trips. Recovery loads poisoned rows via a separate
  `load_poisoned_derivations` query (since `TERMINAL_STATUSES` includes
  `"poisoned"` and `load_nonterminal_derivations` filters it out). The
  timestamp is converted back to `Instant` via PG-computed `EXTRACT(EPOCH FROM
  (now() - poisoned_at))`, so the 24h TTL check survives scheduler restart.
]

#r("sched.retry.per-executor-budget+3")[
  `BuildResultStatus::InfrastructureFailure` does NOT count toward the poison
  threshold. It routes through a separate `handle_infrastructure_failure`
  handler: `reset_to_ready` + retry WITHOUT inserting into `failed_builders`.
  Executor-local issues (FUSE EIO, cgroup setup fail, OOM-kill of the build
  process) are not the build's fault. `TransientFailure` (build ran, exited
  non-zero, might succeed elsewhere) DOES count. Executor disconnect DOES count
  --- a build that crashes the daemon 3× is poisoned: an executor crash whose
  classifying report never arrives, once its failure is established (the
  correlation-TTL sweep, the backstop, or the pull-mode establishment sweep
  fills `termination_reason='unreported'`), joins `failed_builders` and counts
  toward the poison threshold; false-positives from
  unrelated executor deaths are cleared by `rio-cli poison-clear`. The
  budget's exclusion key is the attempt's _source_: an attempt row carrying
  `drv_attempts.source_node` (a pull-mode attempt --- the column is written
  only from the controller-authoritative binding, never from worker-supplied
  identity) contributes that node as its exclusion/budget key, and a row
  without it (every stream-mode/legacy row) contributes its executor (pod
  name) key --- a mixed-era history therefore carries both key kinds in the
  exclusion set until the stream path retires. Small-fleet clause: when
  `0 < |distinct eligible sources| < threshold`, the exhaustion verdict
  (#rref("sched.dispatch.fleet-exhaust")) MUST still be reachable once every
  existing source has failed --- the effective bound is
  `min(threshold, |sources|)`, so a single-node fleet poisons after that one
  source fails rather than deferring forever. Both knobs
  are configurable via `scheduler.toml`: `threshold` (default 3, the former
  `POISON_THRESHOLD` const), `require_distinct_workers` (default true ---
  HashSet semantics; false = any N failures poison, for single-executor dev
  deployments). The retry backoff curve is likewise a `[retry]` table.
  `failed_builders` and the infrastructure retry counts are folds over the
  durable attempt ledger and survive leader failover
  (#rref("sched.retry.failover-budget")); a leader change does not refresh
  any retry budget.
]

#r("sched.dispatch.fleet-exhaust+4")[
  The fleet-exhaust verdict is the structural, immediate "every source this
  derivation could run on has already failed it" poison --- evaluated over
  `(excluded_sources, eligible_sources)` by `placeable()`, preserving the
  empty-universe-defers / exhausted-universe-poisons partition: when the
  eligible universe is empty the check MUST NOT poison --- the derivation
  defers, because an empty pool/fleet is a provisioning transient
  (autoscaler lag, a deployment rollout in progress), and poisoning on it
  would brick every build submitted during the rollout.
  (The stream-path evaluation points --- the dispatch-time
  `find_executor`/E9 backstop and the completion-time check over the
  in-memory executors map --- retired with that path, per this rule's own
  "until that path retires" scoping; the spawn-intent gate below is the
  evaluation point.)
  Pull-mode evaluation point (the spawn-intent gate, AD2): the
  spawnable-source universe is k8s-side knowledge, so the controller ---
  which holds the node informers and renders the intent's `excluded_nodes`
  as anti-affinity --- detects `excluded_nodes ⊇ spawnable sources` for an
  intent and reports it through the idempotent `ReportAttemptOutcome` with
  the distinct reason `NoEligibleSource` instead of spawning an
  unschedulable Job; the scheduler MUST map that report for a still-Ready
  derivation to the same fleet-exhaust poison arm (a `fleet_exhaust` marker
  row plus `Poison(FleetExhausted)`), and MUST treat it as a no-op for a
  derivation that is no longer Ready (already poisoned, in flight, or
  resolved) so controller re-ticks stay idempotent.
]

```toml
# scheduler.toml — poison + retry knobs. All fields optional; absent
# keys fall through to the Default impl shown in comments.
[poison]
threshold = 3                      # failures before derivation is poisoned
require_distinct_workers = true    # HashSet: N DISTINCT executors must fail
                                   # (false = flat counter; single-executor dev)

[retry]
max_retries = 2                    # retries for transient failures
max_exempt_infra_retries = 50      # high-water terminal for cap-exempt infra
                                   # retries (CONCURRENT_PUTPATH, floor-promote)
backoff_base_secs = 5.0            # first-attempt backoff
backoff_multiplier = 2.0           # exponential growth
backoff_max_secs = 300.0           # clamp (inf would panic from_secs_f64)
jitter_fraction = 0.2              # ± fractional jitter on each backoff
```

#r("sched.retry.exempt-infra-cap")[
  The `exempt_from_cap` infra-retry path (CONCURRENT_PUTPATH,
  `floor_outcome.promoted`) skips `infra_count++` and the `max_infra_retries`
  poison check by design --- but MUST still terminate. A separate
  `exempt_infra_count` increments on every exempt attempt and poisons at
  `max_exempt_infra_retries` (default 50, well above I-127's 4-in-a-row benign
  ceiling). Without this terminal, a leaked store-side placeholder lock makes
  every honest worker report CONCURRENT_PUTPATH → infinite ephemeral-pod churn
  at `info!` level only with no scheduler-side counter advancing. The cap
  converts a silent livelock into an actionable poison; recovery cost is one
  `ClearPoison` after the underlying condition is fixed.
]

== Retry decision invariants

The nine failure entry points (worker-reported transient / infra /
permanent / timeout completions, the stream disconnect, the
controller-reported termination and deadline-exceeded paths, the
scheduler-side backstop timer, and the dispatch-time fleet-exhaust check)
each consult and mutate a subset of the per-derivation retry counters. The
rules in this subsection state the properties the nine sites must
collectively preserve; the executable specification of the counter
arithmetic is the reference fold in `rio-scheduler/src/retry_policy.rs`,
and the per-site ↔ per-rule cross-reference is
`docs/spec/models/retry-invariant-map.md`.

#r("sched.retry.transient-budget")[
  A worker-reported `TransientFailure` (the build ran and exited non-zero;
  an `Unspecified` result status is treated identically) MUST record the
  reporting executor into `failed_builders`, increment `failure_count`, and
  then decide: if the poison threshold is reached
  (#rref("sched.retry.per-executor-budget")) or every eligible worker has
  already failed this derivation (#rref("sched.dispatch.fleet-exhaust")),
  the derivation is poisoned; otherwise, while the per-cycle transient
  count is below `RetryPolicy.max_retries`, the derivation MUST be requeued
  with an exponential backoff (`backoff_until = now + backoff(count)`,
  applied as a dispatch-time defer, cleared on successful dispatch) and the
  count incremented; at or above `max_retries` the derivation is poisoned.
]
The transient budget is per poison cycle: a resubmit reset
(#rref("sched.merge.poisoned-resubmit-bounded")) restores the full
`max_retries` budget and charges `resubmit_cycles` instead. The backoff is
the only one among the failure classes (infra, timeout, disconnect, and
backstop requeues are immediate) --- the asymmetry is recorded in the
invariant map as a Phase-1 policy decision, not specified away here.

#r("sched.retry.attempts-bounded+2")[
  Every failure-driven retry loop MUST be bounded: every counted attempt
  charges at least one of the named budgets --- the per-cycle transient
  count (`max_retries`), the non-exempt infrastructure count
  (`max_infra_retries`), the exempt-infrastructure count
  (`max_exempt_infra_retries`), the timeout count (`max_timeout_retries`),
  the poison threshold (`PoisonConfig.threshold`), or the cross-cycle
  resubmit count (`POISON_RESUBMIT_RETRY_LIMIT`) --- every budget has a
  finite cap whose exhaustion produces a terminal state (`Poisoned` or
  `Cancelled`), no single attempt charges the same budget more than once,
  and an attempt exempted from one budget MUST be charged to another.
]
The budget values are configuration (`[retry]` / `[poison]` tables above),
not normative numbers. The two clauses that bite: an attempt charged to no
budget is an unbounded retry loop (the 9,748-redispatch incident, the
pre-I-200 cold-start timeout loop, the pre-cap exempt-infra livelock), and
one attempt charged twice to the *same* budget poisons early (the at-cap
OOM double-count was `infra_count` charged twice for one pod death). The
partition is per budget, not across budgets: a single worker-reported
transient failure legitimately charges both the poison threshold
(`failed_builders` / `failure_count`) and the per-cycle transient count ---
#rref("sched.retry.transient-budget") mandates exactly that --- because the
two budgets bound different things (distinct wedged workers vs attempts
this cycle), so a model invariant encoded as "exactly one budget total"
would falsify on the first transient failure. The as-built code violates
the boundedness clause on one reachable history: a worker that hard-crashes
without ever sending a `CompletionReport` produces a disconnect-requeue
loop that charges no budget and never reaches the backstop timer (each
disconnect resets the derivation to `Ready`, so the Running-too-long clock
restarts on every attempt) --- contradiction C2 in the invariant map,
bounded only by an optional per-build `build_timeout`, and a pre-registered
expected Stage-B falsification. The per-counter fencepost conventions
(whether the cap fires on the Nth or the N+1th attempt) currently differ
between counters; the reference fold reproduces each counter's own
convention and the invariant map flags the inconsistency for Phase-1
unification.

#r("sched.retry.counters-refine-history+2")[
  The per-derivation retry counters (`count`, `resubmit_cycles`,
  `infra_count`, `timeout_count`, `last_infra_failure_at`,
  `exempt_infra_count`, `failed_builders`, `failure_count`, `poisoned_at`,
  `backoff_until`) MUST at every point equal the reference fold of the
  derivation's observed failure-event history: each observed event charges
  the counters its class charges and no others; an infrastructure failure
  whose own resource-floor outcome is not at the ceiling, observed more
  than `infra_retry_window_secs` (default 300 s) after the most recent
  counted infrastructure failure, resets `infra_count` before any charge
  --- whether or not the failure itself is cap-exempt; a non-exempt
  infrastructure failure then charges `infra_count` and re-anchors the
  window; a floor-promoted or CONCURRENT_PUTPATH infrastructure failure
  charges `exempt_infra_count` instead of `infra_count` and does not move
  the window anchor; the cache-hit and resubmit resets are themselves
  history events that zero the per-cycle counters; and no code path
  mutates a counter outside the fold's event alphabet.
]
The fold is `rio-scheduler/src/retry_policy.rs` (a pure function with unit
tests against hand-computed histories); the model-checked form quantifies
over observation orderings and is deferred to the `retryPolicy.qnt` model.
The 300 s window appears here because no other rule states it --- it is the
I-127 forgiveness that distinguishes a burst of misclassified permanent
failures from sparse independent incidents, and a fold that omits it
poisons healthy derivations on long builds. The window reset firing on
cap-exempt observations too is the as-built `handle_infrastructure_failure`
fall-through (the exempt arm does not return before the reset block); the
fold reproduces it, so a counter mismatch on that history class reads as a
code defect rather than a fold gap.

#r("sched.retry.verdict-channel-invariant")[
  For a fixed physical failure history, the budget verdict (requeue,
  poison-on-budget-exhaustion, terminal cancel, or TTL-expire) and the
  counter deltas MUST NOT depend on which observation channel (worker
  completion report, stream disconnect, controller termination report,
  scheduler backstop timer) delivered each physical event or in what order
  the channels delivered them.
]
The pre-Phase-1 code violated this on at least one reachable history,
recorded as divergence D1 in the invariant map: the same exhausted timeout
budget landed as `Cancelled` (worker-reported `TimedOut`) or `Poisoned`
(controller-reported `DeadlineExceeded`) depending on which observer
reported the deadline overrun first --- the two reports describe one
physical fact and which arrives first is a race. The rule was added
marker-first so the model run that falsified it was confirming a documented
defect, and adding it marker-first also surfaced a rule-vs-rule tension:
the (since-retired) stream-era deadline-exceeded rule as then written
assigned terminal ownership at the timeout cap exclusively to the worker-side
`TimedOut` path (the controller path "only promotes and counts"), so on the
reachable wedged-worker history where only the controller ever observes the
deadline overrun, no implementation could satisfy both rules --- honoring
the deadline-exceeded clause made the verdict channel-dependent (no
terminal on the controller-observed run, `Cancelled` on the worker-observed
run of the same physical history); the invariant map records this as
rule-vs-rule contradiction C4. Phase 1 resolved both as the design
pre-committed: the timeout cap's terminal `Cancelled` is owned by the
collapsed verdict fold regardless of the observing channel (today the
worker-side `TimedOut` path and the establishment sweep are the observers
that reach it). The related
which-counter-does-a-promoted-OOM-charge inconsistency (divergence D3) is
*not* a channel race --- a cgroup-level OOM and a pod-level OOM are
physically distinct events --- and is recorded as a contradiction of
#rref("sched.retry.exempt-infra-cap") instead; its exemption charge landed
on the controller channel with the same Phase-1 collapse.

#r("sched.retry.no-double-count")[
  One physical executor death MUST produce at most one counted accounting
  event per derivation, regardless of which subset of the observation
  channels (stream close, heartbeat timeout, controller termination report,
  backstop timer) observes the death and in which order their reports
  arrive.
]
The enforcement is the durable attempt row rather than in-memory dedup
state: the fenced mint creates at most one open attempt per intent, the
first classifying observation appends the classification exactly once
(`exec_id`-keyed, the partial-unique index is the arbiter), the
pod-terminal report only fills `termination_reason` on that row under the
`WHERE termination_reason IS NULL` guard, a report for an identity with no
attempt row charges nothing, and the establishment sweep only ever
establishes an attempt that still has no terminal row. The stream-era
in-memory dedup (`recently_disconnected`, `last_completed`) that previously
enforced this retired with the session machinery; the property is unchanged
and is what the re-targeted session model checks.

#r("sched.retry.recovery-projection+2")[
  After a leader change, each recovered derivation's retry state MUST equal
  the fold of its durable attempt-ledger suffix (the `drv_attempts` rows at
  or after its most recent reset event), seeded --- transitionally, until
  the legacy mirror columns are dropped --- by the legacy projection of
  `derivations.{retry_count, failed_builders, resubmit_cycles}` whenever
  those columns are non-empty and the loaded suffix contains no reset row:
  `failed_builders` is the union of the fold's set and the column set,
  `count` and `resubmit_cycles` take the maximum of fold and column, and
  `failure_count` is floored at the merged set's size. A suffix that begins
  with a reset row ignores the columns; an empty suffix degenerates to the
  pure legacy projection (in which poisoned-row recovery never recovers
  `count`). `poisoned_at` and the poisoned status still come from
  `derivations` (#rref("sched.poison.ttl-persist")), with rows past the
  24 h TTL cleared rather than reloaded; no recovered counter may exceed
  what the durable attempt rows and the legacy columns together support.
]
This is the Phase-1b recovery contract: the recovered view is the same
seeded fold the live appending transactions compute, so every retry budget,
the 300 s window anchor, and the placement exclusion (including backstop-
and crash-established entries that never had a per-counter column mirror)
survive a leader change per #rref("sched.retry.failover-budget"). The
legacy-column seed is the transitional mixed-era floor (its union/max
semantics cannot double-count rows the legacy writers --- active until the
T-1b.13 cutover froze the columns --- also mirrored); it is dropped in
Phase 2 together with the columns. The previous
revision of this rule pinned the pre-ledger selective forgiveness
(4 recovered / 1 derived / 5 defaulted), which the as-built Stage-B model
still encodes until the Phase-1c re-encode.

#r("sched.retry.failover-budget")[
  Once the attempt history is durable (the Phase-1 attempt ledger), every
  retry budget --- the per-cycle transient count, the non-exempt and
  exempt infrastructure counts, the timeout count, the poison threshold's
  failed-builders set, and the cross-cycle resubmit count --- MUST be
  accounted per poison cycle and MUST survive a leader failover: the new
  leader's fold over the durable attempt history MUST yield the same
  remaining budgets the old leader would have enforced over the same
  history, and only the explicit reset events (the admin or TTL poison
  clear, the bounded resubmit reset, the cache-hit clear) --- themselves
  durable history events --- refresh a budget. A leader change is not a
  reset event.
]
This is the `sched.retry.failover-budget` decision the design pre-commits
directionally and its Phase-0 gate requires before Phase 1 starts, made
and recorded at the Phase-0 exit (2026-05-25):
per-poison-cycle budgets that survive failover are the only choice
consistent with `FailoverPreservesHistory` as a Phase-1 acceptance
property --- the durable ledger exists precisely so the new leader's fold
matches the old leader's --- and the strict direction (no fresh budget
after every leader flap during a failover storm, at the cost of poisoning
a derivation already at the cap --- `infra_count = 10` of 10, the
non-exempt cap is check-then-increment --- on its next counted infra
failure after a flap instead of granting it a fresh budget). The code-side
half landed with the Phase-1b collapse: the verdict sites fold the durable
suffix and recovery rebuilds the retry view from the same seeded fold
(#rref("sched.retry.recovery-projection")), so a leader change no longer
refreshes any budget --- the implementing site is the recovery-time ledger
fold rebuild (`rebuild_retry_view_from_ledger`). The machine-checked
verification landed with the Phase-1c model flip: the post-collapse
`retryPolicy.qnt` failover regime checks `failoverPreservesHistory` (the
recovered budget counters equal the durable ledger fold --- nothing
forgiven, nothing fabricated) exhaustively, alongside the
counter/verdict refinement and durability invariants. The companion
amendments (with their version bumps) of the two rules whose prose
previously pinned the forgiveness
(#rref("sched.timeout.promote-on-exceed"),
#rref("sched.retry.per-executor-budget")) landed with the Phase-1b
recovery change, as this rule required.

#r("sched.poison.cascade-dependents")[
  When a derivation reaches a failure-terminal state (`Poisoned`,
  `Cancelled` via budget exhaustion, or a permanent failure), every
  ancestor reachable from it through edges whose intermediate nodes are all
  in a not-yet-started state (`Created`, `Queued`, `Ready`) MUST be
  transitioned to `DependencyFailed` and persisted; nodes that have already
  started (`Assigned`, `Running`) or already terminated MUST NOT be
  preempted by the cascade; and every interested build of every cascaded
  node MUST observe the cascade (a per-node terminal event and a build-level
  completion check), including builds interested in a cascaded ancestor but
  not in the trigger.
]
This is the runtime cascade (`cascade_dependency_failure` + the
terminal-failure epilogue's union over interested builds). Its merge-time
counterpart (#rref("sched.merge.dep-failed-transitive")) seeds nodes that
join the DAG after the trigger already failed; its recovery-time
counterpart (#rref("sched.recovery.failed-dep-cascade")) re-runs it for
parents whose cascade was interrupted by a crash. All three exist because a
keep-going build with a poisoned leaf otherwise hangs Active forever ---
`completed + failed` never reaches `total`.

#r("sched.admin.list-executors-leader-age+3")[
  `ListExecutorsResponse.leader_for_secs` is the seconds since this replica
  acquired leadership (`LeaderState::leader_for()`). Consumers MUST treat the
  list as a freshly-acquired leader's view when `leader_for_secs` is small
  and MUST NOT use it alone to prove absence right after a failover.
]
The historical hazard was the in-memory executors map refilling
incrementally over the reconnect window; the open-attempt view behind the
re-implemented surface is durable, so today the field is an operator/CLI
freshness hint --- the controller's orphan reap no longer consults
`ListExecutors` at all (its busy view is `ListOpenAttempts`,
#rref("ctrl.job.busy-from-open-attempts")).

#r("sched.admin.list-executors+2")[
  `AdminService.ListExecutors` MUST return one entry per open pull-mode
  attempt, read from the same durable open-attempt view as
  #rref("sched.admin.list-open-attempts"): the entry's `executor_id` is the
  attempt's executor identity, `busy` is true, `status` is "alive",
  `systems`/`kind` come from the attempt's derivation, and
  `connected_since`/`last_heartbeat` carry the attempt-open time (the pull).
  The optional `status_filter` matches "alive" (or empty) to return the
  list, any other known historical status ("draining"/"degraded"/
  "connecting") to return an empty list, and unknown values leniently
  (show all). The response's `leader_for_secs` keeps the
  #rref("sched.admin.list-executors-leader-age+2") semantics.
]
This is the busy-fleet projection kept for existing CLI/dashboard callers;
spawned-but-not-yet-pulled
pods are the controller's Job census, and there is no scheduler-side
registration, draining, degraded, or connecting state left to report.

#r("sched.admin.list-builds")[
  `AdminService.ListBuilds` paginates via a direct PostgreSQL query with
  `LIMIT/OFFSET` (proto field `offset = 3`). Per-build derivation counts come
  from `LEFT JOIN build_derivations + derivations`; `cached_derivations` uses
  the heuristic "completed with no assignment row" (a cache-hit derivation
  transitions directly to Completed at merge time without dispatch). Optional
  `status_filter` matches the `builds.status` column. `total_count` is from a
  separate `COUNT(*)` query (unaffected by pagination).
  `ClusterStatus.store_size_bytes` is now populated from a 60s background task
  that polls `SUM(nar_size) FROM narinfo` --- kept out of the handler's hot
  path since the controller polls it every reconcile tick.
]

#r("sched.admin.clear-poison")[
  `AdminService.ClearPoison` resets both PostgreSQL (`db.clear_poison()`:
  status, `poisoned_at`, `retry_count`, `failed_builders`, joined by the
  `poison_cleared` ledger reset row) and in-memory state (the node is removed
  from the DAG so the next submit re-inserts it fresh). Returns `cleared=true`
  only if both succeed. PG is cleared FIRST: if the PG clear fails, the
  in-memory state is left untouched (still Poisoned) and `false` is returned,
  so the operator's retry finds the derivation still poisoned and can proceed
  --- the pre-`b874e5120` in-mem-first ordering left the in-memory status
  reset after a PG blip, so the retry hit the not-poisoned guard and the
  clear became a permanent no-op until restart. Idempotent: calling on a
  non-poisoned or non-existent derivation returns `cleared=false` without
  error. The DAG is keyed on the full `.drv` store path; `rio-cli poison-clear`
  validates this client-side and rejects bare hashes (a silent no-match would
  look like "not poisoned" when it's actually "wrong key format").
]

#r("admin.rpc.cancel-build")[
  `AdminService.CancelBuild` gates on `x-rio-service-token` (allowlist:
  `rio-cli`, `rio-controller`) and dispatches
  `ActorCommand::CancelBuild{caller_tenant: None}` --- operator override
  bypasses the tenant-ownership check that `SchedulerService.CancelBuild`
  applies. rio-cli holds a service-HMAC identity, not a tenant-JWT identity, so
  `SchedulerService.CancelBuild` is unreachable from the CLI in JWT mode
  (#rref("sched.tenant.authz")); this RPC is the CLI's path.
]

#r("sched.admin.list-poisoned")[
  `AdminService.ListPoisoned` returns all currently-poisoned derivations from
  PostgreSQL (`status = 'poisoned'`). Each entry includes the full `.drv` store
  path (what `ClearPoison` takes), the list of executor IDs that failed
  building it, and the age in seconds (TTL is 24h). These are the ROOTS that
  cascade `DependencyFailed` --- a single poisoned FOD can block hundreds of
  downstream derivations, which `rio-cli status` surfaces only as `[Failed]
  N/M drv` without naming the culprit.
]

#r("sched.admin.list-tenants")[
  `AdminService.ListTenants` returns all rows from the `tenants` table. Each
  `TenantInfo` includes the UUID, name, GC retention settings, `created_at`,
  and a `has_cache_token` projection (boolean --- does NOT leak the actual
  token value).
]

#r("sched.admin.create-tenant")[
  `AdminService.CreateTenant` inserts a new tenant row. `tenant_name` is
  required (empty → `INVALID_ARGUMENT`). On name collision or `cache_token`
  collision, returns `ALREADY_EXISTS`. On success, returns the created
  `TenantInfo` including the generated UUID.
]

#r("sched.admin.delete-tenant")[
  `AdminService.DeleteTenant` removes a tenant row by name. FK constraints
  handle the rest: `tenant_keys`/`tenant_upstreams`/`path_tenants`/`chunk_tenants`
  `ON DELETE CASCADE`; `builds.tenant_id`/`derivations.tenant_id` `ON DELETE
  SET NULL` (content-addressed, shared across tenants --- they outlive the
  tenant). Unknown name → `NOT_FOUND`. Primary use case is `xtask k8s qa
  --scenarios` ephemeral-tenant cleanup; no soft-delete or audit trail.
]

#r("sched.admin.spawn-intents")[
  `AdminService.GetSpawnIntents` returns one `SpawnIntent` per Ready
  derivation, optionally filtered server-side by `{kind, systems, features}`.
  `intent_id == drv_hash`; `(cores, mem_bytes, disk_bytes, deadline_secs)` are
  computed by `solve_intent_for` so the controller spawns and the scheduler
  dispatches the SAME shape. `queued_by_system` carries the unfiltered
  per-system Ready breakdown (sum == `ClusterStatus.queued_derivations`) for
  the ComponentScaler's predictive signal.
]

*Retired (1c' spec sweep; machinery deleted by deletion commit A):*
`sched.admin.hung-node-detector`. The scheduler-side hung-node detector
aggregated stale *heartbeats* per node (≥`max(2, ⌈0.5·occupancy⌉)` busy
executors across ≥2 tenants, keyed by the controller-authoritative
`AckSpawnedIntents.bound_intents` binding) and reported the result as
`GetSpawnIntents.dead_nodes`. There are no heartbeats and no scheduler-side
per-pod liveness state left to aggregate: the field is now always empty (it
stays in the proto until the 1d sweep), and node-wedge detection moved to the
controller --- #rref("ctrl.nodeclaim.wedge-cluster") clusters expired
open attempts by their controller-authoritative source node, with the same
two-tenant floor, the per-tick reap cap, and a fail-closed skip when the
open-attempt view cannot be read. `nodeclaim_pool::reap_unhealthy` consumes
that signal as `ReapReason::Dead` exactly as it consumed `dead_nodes`.

#r("sched.dispatch.soft-features+2")[
  The scheduler MUST strip every feature listed in `soft_features`
  (scheduler.toml) from each derivation's `requiredSystemFeatures` at
  DAG-insertion time, before any spawn-snapshot decision reads it.
  nixpkgs convention treats `big-parallel` and `benchmark` as capability hints
  --- any builder qualifies --- unlike `kvm` / `nixos-test` which are hardware
  gates. Without stripping, a `{big-parallel}`-only derivation passes the
  #rref("sched.admin.spawn-intents.feature-filter") subset check against the
  kvm pool (the only pool advertising `big-parallel`) and fails it against
  every featureless pool, so the controller spawns `.metal` for
  firefox/chromium while regular builders sit idle (I-204). Empty
  `soft_features` (the default) preserves pre-I-204 behavior.
]

#r("sched.retry.promotion-exempt+3")[
  Any failure path that bumps `resource_floor` (#rref("sched.sla.reactive-floor"))
  and returns `promoted=true` MUST NOT increment `retry_count` and MUST NOT
  record into `failed_builders` / `failure_count`. Doubling is bounded by
  `Ceilings`; once a dimension reaches its ceiling, `bump_floor_or_count`
  returns `promoted=false` and the call-site increments `infra_count` instead,
  so `RetryPolicy.max_infra_retries` becomes a budget for failures AT the
  ceiling. `max_timeout_retries` is different (I-200,
  #rref("sched.timeout.promote-on-exceed")): EVERY timeout consumes budget
  regardless of promotion, so it bounds total timeout attempts, not just
  at-cap. I-213: with promotion consuming the budget, the kubelet-eviction →
  SIGKILL → disconnect path recorded each rung as a poison-threshold failure
  and poisoned firefox-unwrapped before reaching a viable size.
]

#r("sched.admin.inspect-dag+2")[
  `AdminService.InspectBuildDag` returns the actor's in-memory snapshot of a
  build's derivations: per-derivation status, retry/backoff state, the
  executor identity of the open attempt building it (when in flight), and
  the set of executor identities the DAG currently has work assigned to
  (`live_executor_ids`).
]
The stream-era cross-reference fields are vestigial until the 1d proto
sweep: `rejections` is always empty (there is no placement decision to
explain) and `executor_has_stream` simply mirrors "the derivation has an
in-flight assignment" --- a stuck attempt is bounded by the Job's
`activeDeadlineSeconds` plus the establishment sweep, not by stream
liveness.

*Retired (1c' deletion commit C --- the operator surfaces):*
`sched.admin.debug-list-executors`. `DebugListExecutors` snapshotted the
in-memory executor map (`has_stream`/`warm`/`kind` per entry --- the
I-048b/c "PG says alive, actor has no stream" diagnostic). That map is
deleted: pull-mode pods hold no scheduler-side connection state, so the
PG-vs-actor divergence class the RPC existed to expose cannot form. The
RPC remains in the proto until the 1d sweep as an UNIMPLEMENTED stub whose
error names the successors --- `ListOpenAttempts` (the durable in-flight
view) and the controller's Job/pod census.

#r("sched.gc.live-pins")[
  On dispatch, the scheduler writes the assigned derivation's input-closure
  paths to the `scheduler_live_pins` PG table; on completion (success or
  failure) it deletes those rows; a periodic stale-sweep clears rows older than
  the grace period to bound leakage from crashed schedulers. rio-store's GC
  mark CTE reads `scheduler_live_pins` directly as additional roots, so an
  in-flight build's inputs survive a concurrent sweep even if no narinfo
  references them yet. The complementary output side is `AdminQuery::GcRoots`,
  which returns `expected_output_paths ∪ output_paths` for all non-terminal
  derivations as extra mark-phase roots covering outputs the executor hasn't
  uploaded.
]

*Retired (1c' spec sweep; machinery deleted by deletion commit A):*
`sched.heartbeat.adopt`, `sched.heartbeat.phantom-drain`. Both rules
reconciled the scheduler's session belief against the worker's heartbeat
report --- adopting a worker-reported build the scheduler had reset, and
draining (after two consecutive misses) a build the scheduler believed
running that the worker no longer held. With pull-only delivery neither
divergence can form: the binding is durable from the fenced pull mint, the
scheduler never believes a pod holds work it did not pull, and a pod cannot
hold work the scheduler does not have a row for. The lost-completion half of
the phantom case (work pulled, report never arrives) is carried by the
establishment sweep (#rref("sched.attempt.establishment-window")) and the
controller's pod-terminal report (#rref("sched.attempt.no-attempt-no-op"),
#rref("sched.attempt.synthesized-verdict")); post-failover adoption of
already-completed work is carried by recovery's store-probe arm and the
sweep's adopt arm.

#r("sched.breaker.cache-check+3")[
  The merge-time `FindMissingPaths` cache check goes through a circuit breaker
  that opens after 5 consecutive store-side failures and auto-closes after
  100s. While open, each `SubmitBuild` still attempts the cache check as a
  half-open probe; on probe failure the scheduler *rejects `SubmitBuild` with
  `UNAVAILABLE`* rather than queueing a 100%-miss DAG. A successful probe
  closes the breaker immediately and uses the result. Under threshold (failures
  1--4): proceed as if the cache check missed. This fail-*closed* policy applies
  only to new submissions at merge time; the in-flight stale-completed
  re-verify path (#rref("sched.merge.stale-completed-verify")) remains
  fail-*open* so an already-admitted DAG is never retroactively rejected by a
  transient store outage.
]

*Retired (1c' spec sweep; machinery deleted by deletion commit B):*
`sched.freeze-detector`, `sched.dispatch.unroutable-system`. Both were
dispatch-side operator alarms over the registered-stream fleet: the freeze
detector WARNed when work of a kind was deferred while zero streams of that
kind were registered, and the unroutable-system arm WARNed (and gauged) a
Ready derivation whose `system` no registered executor advertised. There is
no registered fleet and no dispatch pass left to evaluate either condition
against. The operator questions they answered are now answered one layer
earlier, where the capacity decision actually lives: a system or kind no
pool covers never produces a spawnable intent (the controller's pool
`systems`/kind matching --- "no pool exists" remains an operator action, e.g.
adding `i686-linux` to an x86_64 pool per #rref("builder.platform.i686")),
the AD2 spawn gate reports `NoEligibleSource` when every eligible source is
excluded (#rref("sched.dispatch.fleet-exhaust")), and queued-but-unspawned
demand is visible in `queued_by_system` /
#rref("sched.admin.spawn-intents") and the controller's Job census rather
than in a scheduler-side stream count.

= Multi-Build DAG Merging

#r("sched.merge.dedup")[
  The scheduler maintains a single global DAG across all concurrent build
  requests. When a new derivation DAG arrives from the gateway, it is merged
  into the global graph:
  - *Input-addressed derivations*: deduplicated by store path
  - *#gls("ca", display: "Content-addressed") derivations*: deduplicated by @modular-hash
    (as computed by `hashDerivationModulo` --- excludes output paths, depends
    only on the derivation's fixed attributes)
]

#r("sched.merge.dep-failed-transitive")[
  When a newly-merged node transitively depends on a node already in a
  failure-terminal state (`Poisoned`/`DependencyFailed`/`Cancelled`), it is
  seeded directly to `DependencyFailed` --- at any depth, not just immediate
  children. Under `keepGoing=true` this is the only path that resolves such
  nodes; without it the build hangs Active.
]

#r("sched.merge.shared-priority-max")[
  Each derivation node tracks a set of interested builds. Shared derivations
  are built once; all interested builds are notified on completion. *A shared
  derivation's priority is `max(priority of all interested builds)`, updated on
  merge.* When a new build raises a shared node's priority, the node's position
  in the priority queue is updated.
]

#r("sched.merge.wanted-outputs+2")[
  The cache-hit and substitutability classification of a derivation MUST be
  evaluated over its *wanted* outputs only. Each submission contributes, per
  node, the union of the output names referenced by any consumer's `inputDrvs`
  entry for it and the root request's output selection, with an empty
  contribution meaning every declared output. Two derived sets MUST be kept
  distinct. The *stored* per-node union MUST only ever grow --- unioned across
  consumers within one submission, across roots of a multi-root submission,
  across concurrent builds merging the same derivation, and across rows in the
  persistence upsert --- and serves as the persistence/recovery fallback. The
  *effective* wanted set used for classification is the saturating union of
  the wanted contributions of LIVE (non-terminal) interested builds: a
  terminal build's contribution stops counting, and classification MUST fall
  back to the stored union when live contributions are unavailable
  (post-failover, pre-feature rows, or no live interested builds). The
  assignment-token output allowlist, the GC pin set, and the client-facing
  output report MUST continue to cover every declared output. A wanted set
  that resolves to no verifiable concrete path MUST take a conservative
  branch --- fall back to the all-declared criterion, or treat the derivation
  as unavailable/unclassifiable --- rather than vacuously classifying it as
  available.
]
A missing output that nothing consumes (the `-debug` output of a multi-output
derivation) must not condemn the derivation --- and, through dependency
gating, its entire build-time closure --- to a from-source rebuild when every
output that is actually consumed is present or substitutable. The probe set
stays all declared paths (probing an unwanted path is harmless and
opportunistically fetches it if the upstream has it); only the classification
predicates filter by the wanted subset. Classification is scoped to live
builds because a terminal or cancelled build's wants must not keep pinning a
shared node: under a never-shrinking classification union, one wide
submission that has long since failed or been cancelled forces every later
narrow re-merge to keep resetting, re-fetching, or rebuilding outputs nothing
live asks for --- the incident class that motivated live-scoping. Per-build
contributions are in-memory only on this branch (nothing per-build is
persisted): after a leader failover the effective set degrades conservatively
to the stored union until the recovered pre-failover builds go terminal ---
recovery rebuilds their interest but not their contributions, and they never
re-merge, so only their terminal cleanup ends the degradation (builds
submitted after the failover record contributions as usual).

#r("sched.merge.substitute-probe")[
  The merge-time cache check (`check_cached_outputs`) MUST forward the
  submitting session's JWT (`x-rio-tenant-token`) on its `FindMissingPaths`
  store call, and MUST treat paths in the response's `substitutable_paths` as
  cache hits. Without the JWT, the store's per-tenant upstream probe is skipped
  and `substitutable_paths` stays empty --- the scheduler then dispatches
  builds for paths the store could fetch.
]

#r("sched.merge.substitute-probe-indeterminate")[
  The store's upstream HEAD probe MUST report paths it could not classify
  (every upstream returned 429 / 5xx / timed out, or the per-call deadline cut
  the pass short) in `FindMissingPathsResponse.indeterminate_paths`, distinct
  from confirmed-miss. The scheduler --- at BOTH the merge-time check and the
  dispatch-time `batch_probe_cached_ready` re-check --- MUST treat
  indeterminate the same as substitutable: route to the detached substitute
  fetch and let its failure path (`SubstituteComplete{ok=false}` →
  `substitute_tried`) fall through to build. Treating indeterminate as
  confirmed-miss dispatches builders for paths that ARE in cache.nixos.org
  whenever a fresh-wipe burst trips Fastly's edge rate-limit.
]

#r("sched.merge.substitute-fetch")[
  Before marking a substitutable-probed derivation as completed, the scheduler
  MUST eagerly trigger the store's NAR fetch for each substitutable path by
  issuing `QueryPathInfo` with the session JWT. `FindMissingPaths`'s probe is
  HEAD-only; the builder's later FUSE `GetPath` calls carry no JWT (`&[]`
  metadata) so the store's `try_substitute_on_miss` short-circuits and the
  build fails with ENOENT on inputs the scheduler claimed were cached. Fetches
  MUST be issued concurrently with a bounded in-flight cap (a DAG can have
  hundreds of substitutable paths; unbounded fan-out saturates the store's S3
  connection pool and causes false demotes), and each fetch bounded by the
  actor's gRPC timeout, since the call blocks the single-threaded actor event
  loop. A fetch that fails or returns NotFound demotes that path from the
  substitutable set --- the derivation falls through to normal dispatch instead
  of being marked completed against a phantom cache hit.
]

#r("sched.merge.ca-fod-substitute")[
  The path-based lane of `check_cached_outputs` MUST cover every probe-set node
  with a non-empty `expected_output_paths` --- IA, fixed-CA FOD, or otherwise.
  The realisations-table lane is for floating-CA only (output path unknown
  until built; `expected_output_paths == [""]`). Partitioning by
  `ca_modular_hash` length is wrong: every FOD has a 32-byte modular hash, so a
  CA filter excludes them from the path-based lane and they never get checked
  for upstream substitutability --- a fixed-CA FOD whose output is in
  cache.nixos.org gets dispatched to a fetcher and hits a (potentially dead)
  origin URL.
]

#r("sched.merge.substitute-topdown+10")[
  Before merging a submission's full DAG, the scheduler MUST first check
  whether the submission's *demand set* --- its structural roots (nodes with
  no parent edge in the submission) ∪ every node the client explicitly
  requested, as marked by the gateway --- is already available (present in
  store or upstream-substitutable), and if so prune the submission to the
  demand set before the merge --- the dependency subgraph is transitively
  unnecessary and never enters the global DAG. Availability MUST be judged
  over *wanted* outputs only: a demanded node's criterion set is the
  submission node's own wanted set, saturating-unioned (empty = all declared)
  with the pre-existing node's live effective wanted set
  (#rref("sched.merge.wanted-outputs")) when the node already exists in the
  DAG. The prune is all-or-nothing over the demand set; when it fires, the
  kept submission is the demand set: kept nodes are merged dep-less,
  completed inline when their wanted outputs are already present in the
  store and otherwise routed to the deferred upstream fetch
  (#rref("sched.substitute.detached"), no inline `QueryPathInfo`); kept
  nodes whose dependency closure the prune dropped (a kept node whose
  dependencies are already produced in the DAG, or one with no closure to
  drop, is not marked) are marked `topdown_pruned` --- a mark that MUST
  be applied only after the merge has committed, MUST be persisted and
  restored at leader-failover recovery, and MUST be cleared (in PG and in
  memory) only once the node's children are all already produced in the
  DAG and no un-produced child has been reaped out from under it since
  (the closure-hole breadcrumb is recorded in memory and persisted
  alongside the mark, is carried across a resubmit retry of the node, and is
  dropped when a later full merge re-declares its edges), or
  when the fail-fast below consumes it --- a merge that gives it only
  unbuilt children leaves the mark in place. The scheduler MUST
  fall through to the full merge and the bottom-up `check_cached_outputs`
  when any demanded node's criterion set contains a wanted output that is
  missing and not substitutable, when a demanded node's own selector
  resolves to no declared output, when a criterion set resolves to no
  verifiable path, or on any other uncertainty (store unreachable,
  floating-CA demanded node). A `topdown_pruned` node whose current DAG
  children no longer cover its pruned input closure --- childless, or left
  with a closure hole because an un-produced child was removed out from
  under it (reaped by a terminal interested build's cleanup, removed by a
  poison clear, or dropped at recovery as a lost edge) --- MUST NOT be
  dispatched as a from-source build: when its deferred fetch fails
  (`SubstituteComplete{ok=false}`), when the reap itself strands it with an
  already-spent walk, or when its wanted outputs can neither be completed
  inline nor routed to substitution at dispatch time, the scheduler MUST
  fail every interested build with a resubmit-directing error --- the
  dependency subgraph was dropped, so the worker cannot resolve `inputDrvs`
  --- and this MUST hold across leader failover.
]
The prune short-circuits the common case where a requested package is already
cached upstream: instead of eager-fetching hundreds of dependency NARs (the
stdenv bootstrap chain), only the demanded nodes enter the DAG --- and their
fetch runs detached because the closure walk for a ghc-sized node takes
minutes and would stall the actor inline. Explicitly requested nodes count as
demand because the gateway folds a multi-target request into ONE submission:
a requested target that lies inside another target's `inputDrvs` closure is
not a structural root of the combined DAG, and a roots-only criterion would
silently drop it --- never merged, classified, substituted, or dispatched.
The criterion is wanted-scoped and unioned with the live effective wanted set
so the prune can never be more permissive than the post-merge classification
of a shared node: a submission-only criterion could prune this build's
dependency closure while another live build's wants keep the node on the
from-source path, leaving this build hostage to that interest staying alive.
The own-selector resolvability guard covers the fallback corner where no
prior interested build is live and post-merge classification degrades to
exactly this submission's (possibly bogus) selector. The `topdown_pruned`
stamp waits for the committed merge because merge rollback does not revert
it: stamping before the fallible cache-check and persist steps would leak a
rejected build's prune verdict onto a shared pre-existing childless node, and
a later routine fetch failure would terminally fail innocent builds through
the fail-fast arm. The flag is persisted (migration 063, OR-on-conflict on
upsert, cleared --- by the post-reconciliation clear pass at merge time when
those children are already produced and verified, by the completion-time
clear when children become produced, by the recovery-time gate that
drops a restored mark whose persisted children are all produced and vouched
for by a still-live build that also owns the parent, or by the
lazy walk-failure clear when a failed detached fetch finds the node's
children already produced ---
otherwise the mark stays until they produce or the fail-fast consumes it)
because the post-failover shape
is exactly where the from-source hazard bites: the recovered node is
childless and re-probed against the stored wanted union (migration 062,
empty = all declared) ---
routinely wider than the prune-time criterion --- so an output the prune
never vouched for can be definitively missing at dispatch time; without the
restored flag the node would be left Ready and handed a doomed from-source
dispatch whose `inputDrvs` were never merged. The closure-hole breadcrumb is
persisted alongside the mark (migration 064, OR-on-conflict, written
best-effort by the leader's terminal-build reap hook, by the poison-clear
paths --- the admin clear and the poison-TTL sweep, whose removal of a
Poisoned child is the same truncation --- and by recovery itself when it
drops an edge to an un-produced terminal child of a restored parent, the
recovery-side analogue of that reap; restored at recovery, and cleared
alongside a produced-children mark clear or by the merge-time heal --- the
fail-fast consumes only the mark and leaves the breadcrumb for the resubmit
it directs) so the recovery-time
gate keeps refusing to treat a reap-truncated persisted child set as
produced-closure evidence: the reaped un-produced child's own row and edge
can be GC'd before the failover, and without the durable breadcrumb the
surviving produced siblings would launder the clear and re-arm exactly that
doomed dispatch.

#r("sched.dispatch.fod-substitute+2")[
  The dispatch-time store-check (`batch_probe_cached_ready` and the
  per-derivation `ready_check_or_spawn` fallback) MUST probe upstream
  substitutability for every Ready input-addressed derivation, not just FODs
  and not just local presence. The merge-time probe
  (#rref("sched.merge.substitute-probe")) only covers derivations in
  `probe_set` (newly-inserted plus `existing_reprobe` statuses), so a
  derivation that was already in the DAG in a non-reprobe status reaches Ready
  without a merge-time verdict; dispatch-time is its remaining substitution
  opportunity. Per-tick Ready count is bounded by DAG width (the current
  eligible layer), not DAG size, so the dispatch-time batch stays under
  `DISPATCH_PROBE_BATCH_CAP`. Because the actor has no per-derivation JWT to
  forward at dispatch time, the scheduler MUST mint an `x-rio-service-token`
  (`ServiceClaims { caller: "rio-scheduler" }`) and set `x-rio-probe-tenant-id`
  to any interested build's tenant; the store MUST honour
  `x-rio-probe-tenant-id` only when the request carries a valid allowlisted
  service-token (an unauthenticated request cannot self-select a tenant).
  Substitutable paths MUST be fetched (`QueryPathInfo` with the same metadata)
  before the derivation is marked Completed --- builders' subsequent `GetPath`
  calls have no tenant context, so the lazy `try_substitute_on_miss` cannot
  fire there.
]

#r("sched.substitute.eager-probe")[
  Every probeable node in a submission MUST receive a substitutability verdict
  at merge time: `find_missing_with_breaker` sends one `FindMissingPaths` for
  the full `probe_set` and the store-side `check_available` does not truncate
  (#rref("store.substitute.probe-bounded+4")). The merge-time call MUST use
  `MERGE_FMP_TIMEOUT` (90s, separate from the 30s `grpc_timeout`): with the
  store-side cap removed, a 153k-uncached probe at 128-wide and 30ms RTT runs
  \~36s. Dispatch-time `FindMissingPaths` and the topdown roots-only probe stay
  on `grpc_timeout` (their batches are small).
  #(refs.metric)("rio_store_check_available_duration_seconds") p99 informs
  whether 90s needs revisiting.
]

#r("sched.merge.reconcile-order")[
  In `reconcile_merged_state`, all dep-state corrections (cache-hit→Completed,
  stale-Completed reset, reprobe-Poisoned→Substituting) MUST complete before
  any dependent-verdict computation (reprobe-unlocked Queued→Ready,
  `seed_initial_states`). A pending-substitute reprobe node MUST transition
  →Substituting before `seed_initial_states` reads `any_dep_terminally_failed`
  for its dependents.
]

#r("sched.admin.snapshot-substituting")[
  `ClusterStatus` MUST report `substituting_derivations`. The snapshot match
  over `DerivationStatus` MUST be exhaustive so future status additions are
  compile-time caught, not silently-zero.
]

#r("sched.substitute.detached+5")[
  The upstream-substitute fetch MUST run outside the actor event loop. Awaiting
  it inline blocks `MergeDag`/dispatch for the duration of the slowest closure
  walk --- a single ghc-sized NAR (1.9 GB) exceeds the 30s `grpc_timeout` and
  the 16-way concurrent fan-out blocked the actor for >100s in production.
  Instead: at each merge-time and dispatch-time substitution call site the
  scheduler MUST transition the derivation to `DerivationStatus::Substituting`,
  spawn a background task that walks the transitive reference closure (BFS over
  `info.references` from the output paths, each node a `QueryPathInfo`
  triggering store-side `try_substitute`) with a separate
  `SUBSTITUTE_FETCH_TIMEOUT` (minutes, not seconds), and post
  `ActorCommand::SubstituteComplete{drv_hash, ok}` back into the mailbox. The
  task posts `ok=true` ONLY if every *wanted* seed and every node discovered
  by the reference BFS was found or substituted. Per-path failure handling
  retries a `NotFound` or transient error up to the attempt budget before
  recording the failure (every path in the walk was either probed as
  available or named in a narinfo the upstream just served, so a `NotFound`
  inside the walk contradicts an earlier observation); a non-transient error,
  a path whose retries exhaust, or the `MAX_SUBSTITUTE_CLOSURE` cap,
  → `ok=false`. A seed that is declared-but-unwanted
  (#rref("sched.merge.wanted-outputs")) is still attempted --- opportunistic
  completeness --- but it is forgiven on its first failure of any kind,
  without consuming the retry budget: logged, not counted as a fetch
  failure. A path recorded in the node's never-forgive set --- one whose
  forgiveness already triggered a forgiven-now-wanted downgrade of a
  completed walk --- MUST NOT be forgiven in later walks of the
  substitution chain that recorded it; the set is cleared when that chain
  ends in any way (success, the genuine-failure demotion to a from-source
  build, or completion through a non-substitution path), so a walk of a
  later chain MAY forgive the path again once no live build wants it.
  The store substitutes ONE path per
  call (no recursion), so this BFS is the only place the runtime closure can be
  completed. `Substituting` is NOT terminal (`all_deps_completed` returns false
  → dependents stay gated); on `ok=true` the handler transitions `Substituting
  → Completed` (safe even if inputDrvs aren't yet Completed in the DAG --- the
  BFS fetched every wanted seed and the reachable reference closure of
  everything it successfully fetched; a forgiven unwanted seed can leave its
  own path absent, including when a wanted sibling's references name it ---
  see the residual-hole caveat at the implementation site); on `ok=false` it
  reverts to
  `Ready`/`Queued` for normal scheduling and sets `substitute_tried` so
  subsequent dispatch passes skip substitution and route to a worker (one-shot
  fall-through --- a `FindMissingPaths` HEAD probe that disagrees with
  `QueryPathInfo` GET would otherwise loop at Tick cadence and never reach
  `find_executor`). On scheduler restart, recovery MUST reset `Substituting`
  nodes via the same dep-walk as `Created`/`Queued` (the spawned task is gone).
  A cancelled build's orphan task is benign: its fetch still populates the
  store, the `SubstituteComplete` is dropped by the not-Substituting guard.
]

#r("sched.substitute.fanout-bound")[
  `RIO_SUBSTITUTE_MAX_CONCURRENT` (default 256) bounds in-flight detached tokio
  tasks for scheduler memory only. It MUST NOT be tuned as a store-protection
  knob; per-replica admission is #rref("store.substitute.admission").
  `ResourceExhausted` from the store is transient and retried.
]

#r("sched.admin.spawn-intents.probed-gate+2")[
  `compute_spawn_intents` MUST NOT emit a SpawnIntent for a Ready derivation
  whose `probed_generation == 0`, when a store client is configured AND the
  derivation's `expected_output_paths` are all known
  (`DerivationState::output_paths_probeable`).
  `handle_substitute_complete{ok=true}` promotes dependents Queued→Ready and
  (past the inline cap) defers their dispatch-time substitute probe to the next
  Tick; a `GetSpawnIntents` poll landing in that ≤1s window would otherwise
  spawn pods for derivations that the next probe finds substitutable, which
  `reap_stale_for_intents` then deletes 10s later. With
  #rref("sched.substitute.eager-probe") the merge-time probe covers the whole
  submission, so the layer-by-layer cascade is no longer the primary case; the
  gate still covers dependents promoted by a substituted intermediate that was
  NOT in the original probe_set. `queued_by_system` is intentionally NOT gated
  (it must match `ClusterSnapshot.queued_by_system`). With no store client
  (test-only), `batch_probe_cached_ready` early-returns without stamping; the
  gate is moot and disabled.
]

#r("sched.dispatch.substitute-complete-inline+2")[
  `handle_substitute_complete{ok=true}` MUST run the Ready-set store
  short-circuit (`sweep_ready_cached` → `batch_probe_cached_ready`) inline in
  the same handler, so dependents promoted Queued→Ready by the completed
  substitution are probed for substitutability immediately instead of waiting
  one Tick per cascade layer. The inline sweep MUST stay bounded:
  `probed_generation` stamping skips nodes already probed this Tick, so a
  fresh-cluster substitution burst re-probes only newly-promoted dependents,
  and #rref("sched.admin.spawn-intents.probed-gate") keeps the spawn-intent
  path correct (no spurious spawn) for any not-yet-probed tail that defers to
  the next Tick.
]

#r("sched.substitute.leader-gate")[
  `SubstituteComplete` MUST be dropped on a standby replica (`!is_leader()`).
  The detached `substitute-fetch` task survives lease loss (`on_lose` only
  flips atomics) and posts to the now-standby's mailbox; the `ok=true` branch
  writes PG (`persist_status(Completed)`, `upsert_path_tenants`) and would
  split-brain `derivations.status` with the new leader's recovery. The new
  leader's `recover_from_pg` resets `Substituting` via the dep-walk and
  re-probes from there.
]

#r("sched.dag.build-scoped-roots")[
  `find_roots(build_id)` MUST treat a derivation as a root for a given build if
  no parent _interested in that build_ depends on it. The global `parents` map
  includes parents from all merged builds; a derivation that is a root for
  build X may have a parent from build Y. Using the unscoped parent set
  incorrectly marks X's root as a non-root, stalling X's dispatch. The filter
  is `parents(d).any(|p| p.interested_builds.contains(build_id))`.
]

= Duration Estimation

Build duration estimates feed into critical-path priority computation. The
estimate is the SLA model's `T_min` (`DurationFit::t_min()`, ref-seconds at
`min(p̄, c_opt)`) for the derivation's `(pname, system, tenant)` key, falling
back to a flat 60-second default when the key is unfitted (cold start, or
`pname` absent). `T_min` is monotone in work size, requires no solve, and is a
single cache lookup --- priority is a relative ordering, not a schedule.

= Preemption

#r("sched.preempt.never-running")[
  Nix builds cannot be paused or resumed, so *running builds are never
  preempted or cancelled* --- including for CA early cutoff. When cutoff is
  detected for an already-running build, the build is allowed to complete and
  the result is simply discarded. This bounds wasted work to one build duration
  per affected executor.
]

*Exception*: the only case where a running build is killed is executor pod
termination (scale-down, node failure). The preStop hook gives the build time
to complete; if it cannot finish within the grace period, it is reassigned.

= CA Early Cutoff

#r("sched.ca.detect")[
  The scheduler MUST distinguish content-addressed derivations from
  input-addressed at DAG merge time. The `is_ca` flag is set from
  `has_ca_floating_outputs() || is_fixed_output()` at gateway translate,
  propagated via proto `DerivationNode.is_content_addressed`, persisted on
  `DerivationState`.
]

#r("sched.ca.cutoff-compare")[
  When a CA derivation completes successfully, the scheduler MUST compare the
  output `nar_hash` against the content index. A match means the output is
  byte-identical to a prior build --- downstream builds depending only on this
  output can be skipped.
]

#r("sched.ca.cutoff-propagate+2")[
  On hash match, the scheduler MUST transition downstream derivations whose
  only incomplete dependency was the matched CA output from `Queued` or `Ready`
  to `Skipped` without running them. `Ready` is allowed for order-independence
  vs `find_newly_ready` (the cascade may race a prior `Queued→Ready`
  promotion). The transition cascades recursively (depth-capped at 1000).
  Running derivations are NEVER killed --- cutoff applies pre-dispatch only
  (see #rref("sched.preempt.never-running")).
]

#r("sched.ca.resolve+3")[
  When a CA derivation's inputs are themselves CA (CA-depends-on-CA), the
  scheduler MUST rewrite `inputDrvs` placeholder paths to realized store paths
  before dispatch. For deferred input-addressed derivations (IA with
  floating-CA inputs --- `("out","","","")` outputs), the scheduler MUST
  additionally compute each output's store path from the resolved derivation's
  hash (`makeOutputPath`) and write it into both the output spec and the build
  environment before dispatch. Each successful `(drv_hash, output_name) →
  output_path` lookup during resolution is inserted into the `realisation_deps`
  junction table as a side-effect --- this table is rio's derived-build-trace
  cache (per ADR-018), populated by the scheduler at resolve time. It never
  crosses the wire; `wopRegisterDrvOutput`'s `dependentRealisations` field is
  always `{}` from current Nix.
]

Queue-level preemption is fully supported:
- High-priority derivations jump ahead of lower-priority queued (not yet
  running) work. Interactive builds receive an `INTERACTIVE_BOOST` of +1e9 to
  their priority score, which dominates any realistic critical-path sum while
  still preserving relative ordering *within* the interactive set.
- _Executor-slot reservation (priority lanes holding a fraction of executors
  for high-priority work) is not implemented. The boost heuristic plus
  autoscaling is the current mitigation for starvation._
- Autoscaling is the primary mitigation for all-executors-busy scenarios.

= Derivation State Machine

#r("sched.state.machine")[
  Each derivation node in the global DAG follows a strict state machine. All
  transitions are performed inside the DAG actor to ensure serialized access.
]

// Mermaid's `note right of` annotations are dropped here — the transition-
// guards table immediately below carries the same content.
#figure(
  automaton(
    (
      created: (completed: "hit", queued: "accept", dependency_failed: "dep"),
      queued: (ready: "deps-ok", dependency_failed: "dep"),
      ready: (assigned: "pick", dependency_failed: "dep"),
      assigned: (running: "ack", ready: "lost"),
      running: (completed: "ok", failed: "err", poisoned: "poison"),
      failed: (ready: "retry"),
      poisoned: (created: "ttl"),
      completed: none,
      dependency_failed: none,
    ),
    initial: "created",
    final: ("completed", "dependency_failed"),
    input-labels: (
      hit: [cache hit],
      accept: [build accepted],
      deps-ok: [all deps complete],
      pick: [executor selected],
      ack: [executor acks],
      lost: [executor lost /\ heartbeat timeout],
      ok: [build succeeded],
      err: [retriable error],
      poison: [poison threshold /\ max retries /\ permanent failure],
      retry: [retry scheduled],
      ttl: [24h TTL expiry],
      dep: [dep poisoned],
    ),
    state-format: name => text(size: 0.85em, name.replace("_", "_\n")),
    style: (
      created: (initial: "DAG merge"),
      state: (radius: 0.9),
      transition: (label: (size: 0.8em)),
      created-completed: (curve: 1.6),
      assigned-ready: (curve: 0.8),
      failed-ready: (curve: 0.8),
      poisoned-created: (curve: -1.8),
    ),
    layout: (
      created: (0, 0),
      queued: (3, 0),
      ready: (6, 0),
      assigned: (9, 0),
      running: (12, 0),
      completed: (12, 3),
      failed: (9, -3),
      poisoned: (12, -3),
      dependency_failed: (3, -3),
    ),
  ),
  caption: [Derivation-node state machine.],
)

#info(title: [Connection direction])[
  The architecture diagram shows arrows FROM the scheduler TO executors for
  work delivery. This reflects data flow direction (the scheduler answers with
  the dispatch payload). The gRPC connection direction is the reverse:
  executor pods are the gRPC clients calling the scheduler's
  `ExecutorService.PullAssignment` / `ReportOutcome` unaries.
]

#r("sched.state.transitions")[
  *Transition guards:*
  #table(
    columns: (auto, 1fr),
    align: (left, left),
    table.header([Transition], [Guard / Condition]),
    [`created → completed`],
    [Output already exists in rio-store (full cache hit)],

    [`created → queued`], [Build is accepted into the scheduler],
    [`queued → ready`], [All dependency derivations are in `completed` state],
    [`ready → assigned`],
    [The pod spawned for the derivation pulls it (the fenced pull
      transaction binds the attempt)],

    [`assigned → running`],
    [Same pull transaction (the mint records the execution row and the
      derivation is running on the pod that pulled it)],

    [`running → completed`],
    [Executor reports success (output uploaded by executor before reporting;
      scheduler does not re-verify at completion time --- but DOES re-verify at
      later merge time, see `completed → ready`)],

    [`running → failed`],
    [Executor reports a retriable error (`TransientFailure` /
      `InfrastructureFailure`); retry count \< `max_retries` (default 2) *and*
      `failed_builders` count \< poisonThreshold. `failed` is a non-terminal
      intermediate state --- it always transitions to `ready` after retry
      backoff (stored in `DerivationState.backoff_until`; `dispatch_ready`
      defers until `Instant::now() >= backoff_until`).],

    [`running → poisoned`],
    [Any of: *(a)* derivation has failed on `poisonThreshold` distinct
      executors (default: 3; poison tracking spans across builds, not just one
      build's retry attempts), *(b)* `retry_count >= max_retries` with
      `failed_builders` below threshold, *(c)* executor reports a
      permanent-class failure (`PermanentFailure`, `OutputRejected`,
      `CachedFailure`, `LogLimitExceeded`, `DependencyFailed`) --- poisoned
      immediately on first attempt, no retry],

    [`assigned → ready`],
    [Assigned executor is lost (heartbeat timeout, pod termination)],

    [`failed → ready`],
    [Derivation re-enters the ready queue. See `running → failed` above.],

    [`created → dependency_failed`],
    [A dependency reached `poisoned` before this node was queued],

    [`queued → dependency_failed`],
    [A dependency reached `poisoned` while this node was waiting],

    [`ready → dependency_failed`],
    [A dependency reached `poisoned` after this node became ready],

    [`completed → ready`],
    [A later build merges this node as a pre-existing dependency, but
      `FindMissingPaths` reports the output is gone from rio-store (GC under
      another tenant's retention). Re-dispatch; dependents stay `queued` until
      re-completion.],
  )
]

#r("sched.state.terminal-idempotent")[
  *Idempotency rules:*
  - `completed → completed`: No-op (duplicate completion reports are accepted
    and ignored)
  - `poisoned → poisoned`: No-op
  - `dependency_failed → dependency_failed`: No-op
  - Any transition from a terminal state (`completed`, `poisoned`) to a
    non-terminal state is rejected, with carve-outs: `poisoned` auto-expiry
    after 24h resets to `created`; `completed`/`skipped` → `ready`/`queued`
    when a merge-time output-existence check finds the output GC'd
    (#rref("sched.merge.stale-completed-verify"));
    `poisoned`/`dependency_failed`/`failed` →
    `queued`/`completed`/`substituting` when a merge-time re-probe finds the
    output present or substitutable (I-094; `failed` is non-terminal so
    technically not a carve-out --- listed for symmetry with the reprobe lane)
]

#r("sched.state.poisoned-ttl")[
  The `poisoned → created` transition is gated by a 24h TTL.
]

#r("sched.merge.poisoned-resubmit-bounded+3")[
  When a build merges and finds a pre-existing `poisoned` node in the global
  DAG, the node resets for re-dispatch (same as
  `cancelled`/`failed`/`dependency_failed`) iff its `resubmit_cycles` is below
  `POISON_RESUBMIT_RETRY_LIMIT` (2 cycles). An explicit client re-submission is
  treated as retry intent --- the operator presumably fixed the underlying
  cause --- but bounded so a genuinely-broken derivation cannot loop forever.
  `resubmit_cycles` is incremented on each reset and persisted durably --- the
  `resubmit_reset` attempt-ledger row appended for the reset carries the new
  cycle index, and the frozen legacy `derivations.resubmit_cycles` column
  floors pre-ledger history --- so the bound accumulates across re-submissions
  and survives scheduler restart. The reset gives the node a fresh per-cycle
  `retry_count = 0` (full `max_retries` budget). At or above the limit the
  node stays `poisoned` and the build fail-fasts (use the 24h TTL or
  `ClearPoison` admin RPC to override).
]

#r("sched.merge.stale-completed-verify+5")[
  When a build merges and finds a pre-existing `completed` or `skipped` node in
  the global DAG, the scheduler batches a `FindMissingPaths` against rio-store
  with that node's `output_paths` before computing initial states for
  newly-inserted dependents. If any *wanted* output is missing, the node
  resets to `ready` (or `queued` if a dependency was also reset --- "ready ⟹
  all deps' outputs available" must hold), clearing `output_paths`, and
  #(refs.metric)("rio_scheduler_stale_completed_reset_total") increments. A
  missing recorded path that no live interested build wants (listed in the
  node's `expected_output_paths` but outside the effective wanted set,
  #rref("sched.merge.wanted-outputs")) is forgiven
  --- it was legitimately never produced or substituted, and resetting on it
  would ping-pong the node `completed → ready` on every re-merge; a build that
  newly wants it gets the one reset that re-opens the node and substitutes the
  delta. A missing recorded path outside the declared set (a realized
  floating-CA output) still resets.
  `skipped` is included because it carries real `output_paths` and unlocks
  dependents the same as `completed`. The reset MUST run before any
  dependent-advancement step that reads `all_deps_completed` (including the
  re-probe-unlocked Queued→Ready advance), and Ready parents of a reset node
  MUST be demoted to Queued. Newly-inserted dependents then correctly compute
  as `queued` rather than `ready`. The same store-existence check applies to
  newly-inserted CA nodes whose `realisations`-table lookup found a hit: the
  realized path is verified before the node counts as a cache hit
  (#(refs.metric)("rio_scheduler_stale_realisation_filtered_total")). Both
  checks are fail-open: store unreachable → skip verification, treat existing
  `completed` (or the realisation) as valid (the GC race is rare; blocking
  merge on store availability would be a worse regression).
]

#r("sched.merge.stale-substitutable")[
  The stale-completed `FindMissingPaths` is sent with the build's tenant token
  so the store reports `substitutable_paths`. Outputs that are
  missing-but-substitutable are eagerly fetched (per
  #rref("sched.merge.substitute-fetch")) and the node stays `completed`; only
  outputs that are missing AND not successfully substituted reset to `ready`.
  Without this, post-GC re-submissions re-dispatch the entire subtree ---
  including FOD sources whose origin URLs may be dead --- for paths
  cache.nixos.org already has.
]

= Build State Machine

#r("sched.build.state")[
  Each build request follows a separate state machine from individual
  derivations. Build status aggregates the status of its constituent
  derivations.
]

#figure(
  automaton(
    (
      pending: (active: "merged", cancelled: "cancel-early"),
      active: (succeeded: "all-ok", failed: "any-fail", cancelled: "cancel"),
      succeeded: none,
      failed: none,
      cancelled: none,
    ),
    initial: "pending",
    final: ("succeeded", "failed", "cancelled"),
    input-labels: (
      merged: [DAG merged,\ scheduling begins],
      cancel-early: [`CancelBuild`\ before merge],
      all-ok: [all derivations\ completed],
      any-fail: [`keepGoing=false`: any\ PermanentFailure/poisoned;\ `keepGoing=true`: all resolved,\ ≥1 failed],
      cancel: [`CancelBuild`\ received],
    ),
    state-format: name => name,
    style: (
      pending: (initial: "SubmitBuild"),
      state: (radius: 1.0),
      pending-cancelled: (curve: -1.2, label: (pos: 0.35, dist: -0.4)),
      active-failed: (curve: 0, label: (dist: 0.5)),
      active-cancelled: (curve: -1.4, label: (pos: 0.7, dist: -0.4)),
    ),
    layout: (
      pending: (0, 0),
      active: (4, 0),
      succeeded: (8.5, 2),
      failed: (8.5, 0),
      cancelled: (8.5, -2),
    ),
  ),
  caption: [Build-request state machine.],
)

#r("sched.event.derivation-terminal")[
  Every derivation transition to a terminal state (`Completed`, `Skipped`,
  `Poisoned`, `DependencyFailed`, `Cancelled`) emits exactly one
  `DerivationEvent` to each interested build's `WatchBuild` stream.
  Cached-equivalent transitions (`Skipped`, pre-existing `Completed`) emit
  `DerivationCached`; failure transitions (including cascade-propagated
  `DependencyFailed`) emit `DerivationFailed`.
]

#r("sched.build.keep-going")[
  *Aggregation rules:*
  - `keepGoing=false` (default): the build fails as soon as any derivation
    reaches `PermanentFailure` or `poisoned`. Remaining derivations are
    cancelled.
  - `keepGoing=true`: the build continues executing independent derivations
    even after a failure. The build is `failed` only when all reachable
    derivations have completed or failed.
  - A build is `succeeded` only if ALL derivations are `completed`.
  - A build is `cancelled` only via explicit `CancelBuild` (client disconnect
    or API call).
]

= Leader Transition Protocol

The scheduler uses a leader-elected model for the in-memory global DAG. On
leadership transitions:

+ *Assignment generation counter*: Derived on each acquire transition from the
  Lease's `leaseTransitions` count (the lease loop's
  `fetch_max(transitions + 1)` on the shared `Arc<AtomicU64>`, floored during
  recovery by the durable PG history ---
  #rref("sched.recovery.fetch-max-seed")); a same-epoch re-acquire keeps its
  generation. Every authority-exercising transaction (the pull mint, the
  establishment charge, the synthesized close) carries this generation,
  persists it on the row it writes, and is admitted against the durable
  claims floor (#rref("sched.lease.generation-fence")), so a deposed
  leader's writes are fenced at the transaction rather than at the worker.
+ *Recovery completion keyed to the acquire-epoch*: The lease acquire
  transition fires a `LeaderAcquired` command to the actor (delivered
  asynchronously and in order --- lease renewal MUST NOT block on recovery
  completing) and does not touch the recovery-completion stamp: completion
  is recorded for the `leaseTransitions` count it was computed under, the
  lose and rebound transitions clear it, and dispatch resumes only while the
  recorded stamp matches the current count --- so a lose followed by a
  re-acquire at the same count keeps an in-flight completion valid until the
  actor processes the loss already queued behind it (the wipe and the stamp
  invalidation land together --- a third invalidation site alongside the
  lose/rebound clears --- and dispatch re-gates until the follow-up recovery
  completes), while a holder change observed at a different count leaves any
  in-flight completion mismatched --- the lose or rebound that delivered the
  observation has already cleared the stamp --- so recovery re-runs before
  dispatch; a holder-change sequence whose observed count lands back on the
  recorded value is the count-coincidence ABA priced in the bump-confirm
  residual list below. A
  still-leading renew round that observes a `leaseTransitions` count
  different from the one recorded at the last acquire edge or rebound is a
  holder change observed late: it re-records the count, re-derives the
  generation, clears the completion stamp, and re-fires `LeaderAcquired`
  without an acquire edge (#rref("sched.lease.rebound")).

#r("sched.lease.non-blocking-acquire+2")[
  The LeaderAcquired send MUST NOT block the renewal tick --- delivery to the
  actor is asynchronous. Blocking on recovery would stall renewals: the
  blocked loop can neither renew nor self-fence while a standby steals after
  `STEAL_AFTER` (19s) of observed staleness → dual-leader.
]

#r("sched.lease.standby-tick-noop+2")[
  On lease loss (or local self-fence) the lease loop sends `LeaderLost` to the
  actor (symmetric with `LeaderAcquired`, same non-blocking asynchronous
  delivery). The
  actor clears in-memory builds/dag/events and zeros the leader-only state
  gauges. `handle_tick` early-returns on `!is_leader` so an ex-leader's
  PG-writing housekeeping (orphan-watcher cancel, build-timeout fail, backstop
  reassign, poison-clear, derivations-gc) cannot race the new leader.
]
The cleared set is not exhaustive: retained ring-buffer log entries survive
the loss --- a still-streaming worker's in-flight execution keeps its lines
across the flap --- and are reconciled (re-armed, restamped, or swept) at the
next acquisition (#rref("sched.recovery.log-buffer-sweep")).

#r("sched.lease.hook-order")[
  Lease hook commands MUST be delivered to the actor in invocation order; in
  particular a `LeaderLost` followed by `LeaderAcquired` from the same renewal
  tick MUST be processed in that order.
]
The actor's same-epoch recovery reasoning and the false-alarm end state (the
tick-time self-fence fires `LeaderLost`, the same tick's successful renew
fires `LeaderAcquired`) rely on that order: the lost arm wipes, the acquired
arm re-recovers. An inverted pair would leave the leader with
`is_leader = true`, an empty DAG, and `recovery_complete = false` (the lost
arm invalidates the completion it orphans) --- dispatch gated and every
non-terminal build stalled until the next leadership transition re-runs
recovery.
Delivery is a single FIFO handoff drained by one forwarder task into the
actor's command channel, so invocation order is preserved end-to-end.

#set enum(start: 3)
+ *State reconstruction*: The actor's `LeaderAcquired` handler invokes state
  recovery (see §State Recovery below), then records the completion for the
  `leaseTransitions` count snapshotted at recovery entry; completion is
  deliberately withheld on the discard paths (epoch moved mid-recovery via a
  lose- or rebound-flap, lapsed leadership, or a discarded unconfirmed bump),
  leaving dispatch gated until a later acquire edge or rebound re-runs
  recovery. Dispatch is a no-op until the recorded completion matches the
  current count.
+ *Executor retries*: in-flight pods notice nothing beyond failed unaries ---
  they keep retrying `PullAssignment`/`ReportOutcome` with backoff until the
  new leader serves them (#rref("builder.pull.retry-loop")).
+ *In-flight attempts*: recovery reloads the open pull attempts (assignment
  rows plus their `exec_id`s) from PostgreSQL, so a re-pull after failover
  returns the identical payload, a report lands on the same attempt row, and
  an attempt whose pod died during the failover is resolved by the
  controller's pod-terminal report or the establishment sweep
  (#rref("sched.attempt.establishment-window")). The store-probe arm adopts
  work that completed while no leader was serving.
#set enum(start: 1)

= Synchronous vs. Async Writes

Not all state changes require synchronous PostgreSQL writes:

#table(
  columns: (auto, 1fr, 1fr),
  align: (left, left, left),
  table.header([Write Type], [Examples], [Behavior]),
  [*Synchronous* (before responding)],
  [Derivation completion state, assignment state transitions, build terminal
    status],
  [Must be durable before acknowledging to executor/gateway],

  [*Async/batched*],
  [`build_samples` inserts, SLA estimator refit, dashboard-facing status
    updates],
  [Batched and flushed periodically (every 1--5s)],
)

On crash, async writes may be lost but are non-critical: @ema re-converges after
a few builds, and status is rebuilt from ground truth (derivation/assignment
tables) during state recovery.

= State Recovery

#r("sched.recovery.fetch-max-seed+4")[
  During recovery the scheduler MUST raise its generation to the claimed
  generation target via `fetch_max`, not `store`. The comparison baseline
  is the generation at recovery entry --- the value the lease loop's
  acquire-edge `fetch_max` left, which is the Lease-derived generation
  (#rref("sched.lease.generation-fence")) unless an earlier recovery's
  PG-floor seed already raised it; the target is therefore never below
  the Lease-derived generation. The target is that entry generation
  unless the durable PG floor --- `GREATEST(MAX(assignments.generation),
  MAX(leader_generation_claims.generation))` --- demands more: one past
  the floor when the floor exceeds the entry generation, or when it
  equals it and the claims ledger does not show this holder's own claim
  row at that generation. In every other case (floor below the entry
  generation, no floor at all, or a floor equal to it where this
  holder's own claim row already sits) the target is the entry
  generation itself --- the same-epoch re-acquire retains its generation
  rather than claiming a new one (#rref("sched.lease.generation-claim")).
  The lease loop writes the same `Arc<AtomicU64>` on acquire; both
  writers only ever raise it, and `store` could regress the generation.
]

The PRIMARY generation source is the Lease's `leaseTransitions` count. The PG
floor is the durable backstop that survives Lease-object deletion: a recreated
Lease restarts its transition count at zero, and the floor puts the first
post-deletion leader back above every generation PostgreSQL has ever seen. It
_bounds_ the post-deletion damage; it does not prevent a collision under a PG
point-in-time restore (both tables regress together, and the only remedy there
is the etcd-style "bump the counter past anything that could have been
issued"). The floor reads the claims ledger as well as the assignment history
because the assignment high-water _decays_ --- terminal-derivation GC cascades
assignment rows away, so `MAX(generation) FROM assignments` regresses toward
`NULL` on a quiescent cluster --- and because a leader deposed before
persisting any assignment leaves no trace in `assignments` at all
(#rref("sched.lease.generation-claim")).

A floor that _ties_ the entry generation is exceeded unless the claims ledger
shows this holder's own row there, because assignment rows carry no
scheduler-holder identity and the assignment history written before the claims
ledger existed (migration 063 ships no backfill) has no claim rows at all ---
so on the first post-upgrade acquisition, and after a predecessor that
proceeded unclaimed, the floor cannot be assumed to be ours. A failed ledger
read counts as "not shown" and is likewise exceeded; the conservative cost is
one burned generation and an idempotent re-dispatch of the holder's own
in-flight work.

#r("sched.recovery.bump-confirm+3")[
  A claim target that exceeds the generation the recovery entered with --- or
  one that retains that entry generation while the durable PG floor, taken as
  zero when no assignment or claim row exists at all, lies more than one
  generation below it (the claims-and-assignments history then cannot vouch
  for the generations in between) --- or one that retains that entry
  generation when the durable PG floor could not be read at all (a floor that
  cannot be read cannot vouch for anything) --- MUST NOT be seeded into the
  in-memory generation, and dispatch MUST NOT be ungated at it, until this
  replica has completed an apiserver round-trip --- initiated after the
  write-ahead claim step completed --- that ended with this replica as the
  Lease holder; absent that confirmation the recovery MUST be discarded.
]

The PG floor cannot distinguish a dead predecessor's claim from a live
successor's. Without the confirmation, a deposed-but-unaware leader whose
recovery outlives its deposal --- a Lease deletion lands mid-recovery, a
standby re-creates the Lease, claims one past the old floor, and starts
dispatching, all before the old leader's lease loop observes anything ---
would compute a target one past the _live_ leader's
(#rref("sched.recovery.fetch-max-seed")), seed it, and exercise authority
with it, inverting the fence (#rref("sched.lease.generation-fence")): the
stale believer's pull mints and establishment charges pass the floor
admission and raise the durable floor above the live leader's generation, so
the live leader's own authority transactions abort as below-floor for the
rest of its term. The
same inversion exists downward: a predecessor that died between its acquire
edge and its claim INSERT leaves a derived-but-never-claimed generation with
no durable trace, so the floor sits more than one generation below the next
believer's entry value; a post-deletion successor seeds one past that stale
floor --- _below_ the deposed believer's entry generation --- and without the
wait the deposed believer completes at its higher retained generation and
inverts the fence the same way (its fenced transactions out-floor the live
successor's). The confirmation keeps apiserver I/O in the
lease loop --- the recovery only observes the renew-round counters the loop
publishes. It does not contradict the proceed-on-PG-failure rationale of
#rref("sched.lease.generation-claim"): the wait applies to bump targets and
to retained entry generations the durable floor cannot vouch for; ordinary
failovers --- whose entry generation the floor reaches to within one --- and
same-epoch re-acquires --- where the floor ties the entry on this holder's
own claim row --- never wait, the wait is bounded by the lease loop's own
renew/fence machinery, and a discarded recovery re-runs on the next acquire
edge --- it does not reintroduce the indefinite-block-on-PG failure mode that
rationale rejects. (The rule keeps its historical `bump-confirm` name while
also covering these non-bump retains; on the retain path the seed clause is
an idempotent no-op and the operative prohibition is recovery-completion
ungating --- and with it the serving of fenced authority transactions at the
unconfirmed generation, #rref("sched.lease.claim-before-advertise").) Residuals that remain: the count-coincidence ABA documented
at the recovery gate's entry-snapshot comment --- an edge-ful re-steal, or a
rebound (#rref("sched.lease.rebound")), whose observed transition count lands
exactly back on the recorded value; in the rebound sub-case no command is
queued, so the stale recovery persists until the next real leadership change
or rebound; the claim-failure conjunction --- a term that proceeded unclaimed
leaves no durable trace of its generation, so a Lease deletion after that
term's confirmation still lets a successor seed below it; and the
adjacent-floor race --- when the never-claimed gap is exactly one generation
wide and the post-deletion successor's claim at the deposed believer's entry
generation minus one lands _before_ that believer's floor read, the floor
looks contiguous, no wait is required, and completing above the live
successor additionally requires that believer's renew rounds to fail from the
deletion through its recovery gate while staying under the self-fence
deadline. Non-K8s single-scheduler deployments construct their leader state
with recovery already complete and never run the lease loop, so no
confirmation is ever required there.

#r("sched.reconcile.leader-gate")[
  The post-recovery reconcile pass (`ReconcileAssignments`) MUST early-return
  when `is_leader()` is false. The 45s reconcile timer is fire-and-forget and
  `on_lose` does not cancel it or clear the in-memory DAG, so an ex-leader's
  timer would otherwise issue PG writes (`persist_status`,
  `increment_retry_count`, `poison_and_cascade`) against a stale DAG,
  overwriting the new leader's state. Same gating discipline as
  `dispatch_ready`.
]

#r("sched.recovery.gate-dispatch")[
  On startup or leadership acquisition, the scheduler reconstructs its
  in-memory state from PostgreSQL. Recovery runs inside the DAG actor (via the
  `LeaderAcquired` command). Dispatch is *gated* on the `recovery_complete`
  flag --- `dispatch_ready` is a no-op until recovery finishes, preventing a
  partially-loaded DAG from issuing assignments.
]

#r("sched.recovery.failed-dep-cascade+2")[
  Recovery loads only non-terminal derivations and edges between them; edges to
  `completed`/`skipped` children are dropped (those dependencies are
  satisfied). A recovered parent with a
  `poisoned`/`dependency_failed`/`cancelled` persisted child vouched for by a
  live (`pending`/`active`) build that also owns the parent via
  `build_derivations` --- the state left by a crash
  mid-`cascade_dependency_failure` --- MUST be transitioned directly to
  `DependencyFailed` and persisted, BEFORE the `compute_initial_states`
  recompute; without that short-circuit the dropped edge makes
  `all_deps_completed` true, the parent is wrongly promoted to Ready, and
  dispatched against a missing input. A parent whose only failed-child
  evidence belongs to dead builds or to builds that never owned it MUST NOT
  be condemned by the cascade --- it recovers normally (childless if the
  edge was dropped) and any genuine problem is re-discovered at dispatch
  time. The set of cascaded parents is loaded via a separate
  `derivation_edges JOIN derivations` query restricted to terminal-failure
  child statuses and to children carrying a `build_derivations` link to a
  live build that also links the parent.
]

Recovery carries no build-log state: the scheduler holds no log buffers, so a
lease change requires no log reconciliation, discard, or re-stamping. (The
log data plane lives in rio-store, whose ingest sessions are leased per
execution, not per scheduler tenure --- see
#xref(<store-log-service>, [the store component spec]).)

Recovery sequence:

+ Load all non-terminal builds from PostgreSQL (`builds` and `derivations`
  tables)
+ Reconstruct DAGs from the derivations table and their edges
+ *Identify nodes in "waiting" state whose dependencies are all complete, and
  transition them to "ready"* (handles the case where the previous scheduler
  crashed between completing a node and releasing downstream)
+ Restore the open pull attempts from the `assignments`/`drv_executions`
  rows (assigned executor identity and `exec_id`), so post-failover re-pulls
  and reports stay idempotent against the same attempt
+ For derivations marked "assigned"/"running" with no live attempt to wait
  on, check rio-store for the outputs (they may have been uploaded before the
  executor died): adopt as completed if present, otherwise leave the open
  attempt to the establishment sweep / the controller's pod-terminal report,
  and reset orphaned rows with no open attempt back to Ready
+ Resume serving pulls and reports from the reconstructed state

Executor pods retry their unaries with backoff: a pull or report that fails
while no leader is serving (failover in progress) simply lands on the new
leader once recovery completes.

#r("sched.recovery.poisoned-failed-count")[
  Recovered builds whose derivations include failure-terminal states (Poisoned,
  DependencyFailed, Cancelled) MUST count those derivations in `failed`, not
  omit them from the denominator. A build whose only non-success-terminal
  derivation was poisoned before the crash transitions to `Failed` after
  recovery, never `Succeeded`. Concretely: `load_poisoned_derivations` rows are
  inserted into the recovery-time `id_to_hash` map so the `build_derivations`
  join resolves them, so `build_summary` counts them, so
  `check_build_completion` sees `failed > 0`.
]

= Executor Registration Protocol

*Retired (1c' deletion commit C --- the operator surfaces; RPCs removed at
the 1d proto sweep):*
`sched.executor.dual-register`. The two-step stream+heartbeat registration
protocol described here has no scheduler side left: the `BuildExecution`/
`Heartbeat` RPCs are gone from the proto and the in-memory executor
entry they used to create is deleted. There is no registration in the pull
protocol --- work binds only at a successful `PullAssignment` (the pull is
both halves), and the never-create half survives as the no-attempt no-op
rule on the report path (#rref("ctrl.report.attempt-outcome")).

*Retired (1c' spec sweep; machinery deleted by deletion commit A):*
`sched.executor.session-epoch`. The rule made session-event attribution
normative: every connect/disconnect/heartbeat was attributed to exactly the
stream epoch that produced it, so a late disconnect from a superseded stream
(the I-056 connect-before-disconnect ordering) could not evict a
freshly-reconnected executor, and a heartbeat could not create session state.
There are no streams, no epochs, and no session events left to attribute ---
the entire event class the rule disciplined cannot occur. The surviving
identity discipline is per-request rather than per-session: every pull and
report is bound to its intent by the HMAC-attested token
(#rref("sec.executor.identity-token") applied per-unary), attempts are keyed
by `exec_id`, and a stale or duplicate report finds the attempt row already
terminal (#rref("sched.executor.report-idempotent")). The retired
stale-epoch calibration witness remains re-runnable against the frozen
as-built model (`executorSessionAsBuilt.qnt`) as historical evidence.

#r("sched.dispatch.fod-to-fetcher+2")[
  Per ADR-019, fixed-output derivations route ONLY to fetcher-kind executors
  and non-FODs ONLY to builder-kind executors. The kind boundary is enforced
  at spawn rather than at a dispatch decision: every spawn intent's
  `ExecutorKind` derives from the derivation's `is_fixed_output` through the
  single `kind_for_drv` chokepoint, the controller requests intents only for
  the kind its pool declares (the `GetSpawnIntents` kind filter,
  #rref("sched.admin.spawn-intents")), and the spawned pod can pull only the
  one intent its executor token is bound to
  (#rref("sec.executor.identity-token"),
  #rref("sched.executor.pull-transaction")) --- so no scheduler code path can
  hand a FOD to a builder pod or a non-FOD to a fetcher pod.
]

#r("sched.dispatch.fod-builtin-any-arch+2")[
  A FOD with `system="builtin"` is eligible on any fetcher pool regardless of
  arch: the spawn path derives no architecture constraint from `"builtin"`
  (#rref("ctrl.pod.arch-selector")), so the intent lands on whichever arch's
  fetcher pool has capacity, and every executor treats `builtin` as a
  supported system (the nix-daemon executes `builtin:fetchurl` internally ---
  no real build process is forked). Arch-specific FODs
  (`system="x86_64-linux"` inherited from stdenv) spawn only on pools
  declaring that system.
]

The kind boundary is absolute: if no fetcher pool covers a FOD's system the
intent stays queued and the @fod waits --- the scheduler NEVER hands a FOD to
a builder under pressure. A queued FOD is preferable to a builder with
internet access. Queued-by-kind visibility comes from `queued_by_system`
(#rref("sched.admin.spawn-intents")) and the controller's Job census rather
than a scheduler-side dispatch queue gauge.

#r("sched.timeout.promote-on-exceed+3")[
  A `BuildResultStatus::TimedOut` completion MUST double
  `resource_floor.deadline_secs` (#rref("sched.sla.reactive-floor")) and reset
  the derivation to `Ready` for re-dispatch, NOT terminal-cancel. The next
  dispatch carries the doubled deadline --- "same inputs → same timeout" no
  longer holds. Bounded by a separate `timeout_retry_count` against
  `RetryPolicy.max_timeout_retries`: a genuinely-infinite build still goes
  terminal (`Cancelled`, retriable on explicit resubmit) after exhausting
  promotions instead of walking forever. `timeout_retry_count` is a fold over
  the durable attempt ledger and survives leader failover
  (#rref("sched.retry.failover-budget")); it stays separate from
  `retry_count` / `infra_retry_count` so timeouts neither consume the
  transient budget nor get masked by the infra time-window reset. I-200:
  before this, `TimedOut` went straight to `Cancelled` and the I-199/I-197
  promotion only fired on the K8s-deadline-kill → disconnect path, not on the
  executor-side `daemon_timeout_secs` → clean `TimedOut` report path.
]

#r("sched.reassign.no-promote-on-ephemeral-disconnect+5")[
  Requeueing a derivation because the executor that held it is gone MUST NOT
  bump `resource_floor` and MUST NOT record into
  `failed_builders`/`failure_count`/`retry_count` at the requeue site. An
  executor loss is ambiguous --- pod kill, node scale-down, preemption,
  store-replica restart and deadline kill are not inherently sizing signals
  (live QA: cmake medium→large→xlarge from a pod-kill +
  store-replica-restart with zero builds run; floor is sticky per M_044) ---
  so the requeue chokepoint (`reassign_derivations`, shared by the
  pull-attempt verdict arms, the synthesized-verdict closes and the
  establishment sweep) re-queues at the current floor, and any charge for the
  loss is appended by the observing classifier at its own site. Only explicit
  resource-exhaustion classifications promote the floor, at their own call
  sites (worker-reported `CgroupOom` and `TimedOut` ---
  #rref("sched.sla.reactive-floor"), #rref("sched.timeout.promote-on-exceed")).
]

*Retired (1c' spec sweep; machinery deleted by deletion commits A/C):*
`sched.termination.deadline-exceeded`. The rule normed the legacy
controller→scheduler `ReportExecutorTermination(DeadlineExceeded)` path:
prefix-match the Job name against the `recently_disconnected` correlation
map, double `resource_floor.deadline_secs` (or charge `timeout_retry_count`
at the cap), and converge on the same terminal `Cancelled` as the
worker-side path so the verdict stayed channel-independent (the C4
resolution). The correlation map and the floor-bumping termination intake
are deleted; the legacy RPC is acknowledged as a no-op until the 1d proto
sweep. As-built, a deadline overrun is classified by the worker's own
`daemon_timeout` → `TimedOut` report when the worker can still report
(#rref("sched.timeout.promote-on-exceed") owns promotion, budget and the
terminal at the cap), and by the controller's
#rref("ctrl.terminated.deadline-exceeded") →
`ReportAttemptOutcome(DEADLINE_EXCEEDED)` second installment when it cannot
--- a reason fill on the attempt row that charges no budget and bumps no
floor (#rref("sched.executor.report-idempotent"),
#rref("sched.attempt.no-attempt-no-op")); a pod too wedged to report at all
is classified by the establishment sweep
(#rref("sched.attempt.establishment-window")). The
#rref("sched.retry.verdict-channel-invariant") obligation is unchanged.

*Retired (1c' spec sweep; machinery deleted by deletion commits A/B):*
`sched.ephemeral.no-redispatch-after-completion`,
`sched.assign.resource-fit`. The first closed the I-188 race (re-dispatching
into a just-freed slot whose process is about to exit) by marking the
executor draining before the same actor turn's dispatch pass; with no
dispatch pass and no slot to re-dispatch into, the race cannot form --- a
pod reports its one attempt and exits, and the next attempt is a fresh pod.
The second rejected pairings whose solved memory exceeded the worker's
actual cgroup limit; there is no pairing decision left --- the pod is sized
*from* the solved intent (`SpawnIntent.{cores,mem_bytes,disk_bytes}`,
#rref("sched.admin.spawn-intents")), so by construction the executor a
derivation runs on was provisioned for that derivation's solved shape.

#r("sched.executor.one-shot+2")[
  Executor pods are single-attempt: an executor MUST run at most one build
  over its process lifetime, and the scheduler MUST NOT bind more than one
  open attempt to one pod. The pod pulls exactly the intent it was spawned
  for; a re-pull while its attempt is open returns the identical payload and
  `exec_id` rather than new work (#rref("sched.executor.pull-transaction"));
  after its report is acknowledged (or on `Gone`) the pod exits instead of
  asking for more work (#rref("sched.executor.pull-gone"),
  #rref("builder.pull.exit-codes")); and the builder-side `BuildSlot` rejects
  a second concurrent claim while one build is in flight.
]

One-shot is what makes the pod the natural attempt boundary: fresh identity
per attempt and zero cross-build state on the executor
(#rref("ctrl.pool.ephemeral")). The stream-era corollaries (the post-completion
draining mark and the dispatch-side capacity bookkeeping) retired with the
placement layer; what remains checkable is the pull-side shape --- one
intent, one open attempt, one report, one exit --- which is exactly the form
the re-targeted session model checks.

*Retired (1c' deletion commit B — the placement layer):* the warm-gate
(initial `PrefetchHint` → `PrefetchComplete` → `ExecutorState.warm` →
`best_executor()` two-pass) existed to order the stream scheduler's
placement decisions toward executors whose FUSE cache was already warm.
Pull-mode delivery has no scheduler-side placement decision — the pod is
spawned for exactly one derivation and pulls it — so there is nothing for
a warm gate to order; the per-assignment input-closure prefetch the
builder performs before building is unchanged and is bounded by the same
build it serves.

*Retired (1c' spec sweep; machinery deleted by deletion commit A):*
`sched.executor.deregister-reassign`, `sched.executor.liveness-window`,
`sched.executor.repair-precedence`, `sched.backstop.timeout`. These four
rules normed the stream session's repair lattice --- when an executor was
deregistered (stream close or 30 s heartbeat timeout, with stall credit so
scheduler congestion never reaped a live fleet), how its in-flight work was
reassigned, the windows the repairs observed (the two-strike phantom
confirmation, the 60 s termination-report correlation TTL, the 45 s
post-failover reconcile delay), the precedence between the many observers of
one divergence (worker report vs heartbeat vs disconnect vs controller
report vs backstop --- exactly one classifier wins, every later observer is
a no-op), and the est×3 backstop timer that caught a wedged-but-heartbeating
daemon. The session state, the heartbeat intake, the correlation map, the
reconcile special case and the backstop timer are all deleted; none of the
windows exist to be composed. This also resolves contradiction C2 (the
deregister rule's unqualified stream-close clause vs the epoch-qualified
code): the rule retires with the machinery, and the contradiction record in
the invariant map stays as history.

What survives, re-keyed onto the durable open-attempt row, is the
*one-classifier-wins* discipline the precedence table existed to guarantee:

- An open attempt always has a repair armed --- the pod's own report retry
  loop while it lives, the controller's pod/Job-terminal
  `ReportAttemptOutcome` when it dies, and the establishment sweep at
  deadline + slack if nothing else arrives
  (#rref("sched.attempt.establishment-window")).
- The first classifying observation wins the row exactly once; duplicates,
  late reports and post-establishment reports find the row terminal and
  change nothing (#rref("sched.executor.report-idempotent"),
  #rref("sched.completion.idempotent"), #rref("sched.retry.no-double-count")).
- A report for an identity with no attempt row charges nothing
  (#rref("sched.attempt.no-attempt-no-op")), and controller-synthesized
  closes of still-wanted work are charge-free
  (#rref("sched.attempt.synthesized-verdict")).
- Liveness bounds come from the platform rather than a scheduler-side
  reaper: the Job's `activeDeadlineSeconds`
  (#rref("ctrl.ephemeral.intent-deadline")) bounds a wedged pod, the
  controller reap graces (#rref("ctrl.ephemeral.reap-excess-pending"),
  #rref("ctrl.ephemeral.reap-orphan-running")) bound Pending and orphaned
  Jobs, and the establishment window bounds an unreported attempt.
- Deposed-leader observers stay inert losers
  (#rref("sched.lease.standby-drops-writes"),
  #rref("sched.lease.generation-fence")).

#r("sched.timeout.per-build")[
  `BuildOptions.build_timeout` (proto field, seconds) is a wall-clock limit on
  the _entire_ build from submission to completion. In `handle_tick`, any build
  with `submitted_at.elapsed() > build_timeout` has its non-terminal
  derivations cancelled and transitions to `Failed`, with `error_summary` set
  to `"build_timeout {N}s exceeded (wall-clock since submission)"`. This is
  distinct from the per-attempt bounds (the Job's `activeDeadlineSeconds`
  plus the establishment window,
  #rref("sched.attempt.establishment-window")) and distinct from the
  executor-side daemon floor (which also receives
  `build_timeout` as a per-derivation `min_nonzero` --- defense-in-depth, NOT
  the primary semantics). Zero means no overall timeout.
]

#warning(title: [Reachability: gRPC-only])[
  `build_timeout > 0` is settable ONLY via gRPC
  `SubmitBuildRequest.build_timeout` (rio-cli, direct API consumers).
  `nix-build --option build-timeout N --store ssh-ng://` is a silent no-op ---
  Nix `SSHStore::setOptions()` is an empty override (since 088ef8175, 2018), so
  `wopSetOptions` never reaches rio-gateway (see
  #rref("gw.opcode.set-options.propagation")). VM integration tests for this
  marker must submit via gRPC, not the ssh-ng CLI.
]

#r("sched.backstop.orphan-watcher")[
  *Orphan-watcher sweep:* `handle_tick` checks each Active build's
  `build_events` broadcast channel. If `receiver_count() == 0` (no gateway
  SubmitBuild/WatchBuild stream attached) for longer than `ORPHAN_BUILD_GRACE`
  (5 min), the build is auto-cancelled with reason
  `"orphan_watcher_no_client"`. This is the scheduler-side backstop for the
  cases the gateway's #rref("gw.conn.cancel-on-disconnect") path can't cover:
  gateway crash mid-build (no process left to send CancelBuild),
  gateway→scheduler timeout during the disconnect-cleanup loop, or any future
  leak path. The grace timer resets if a watcher reattaches before it elapses
  (gateway WatchBuild-reconnect retries for \~111s; 5 min covers it). The
  #(refs.metric)("rio_scheduler_orphan_builds_cancelled_total") counter tracks
  these. Nonzero is expected on gateway restarts; sustained nonzero with
  healthy gateways means the gateway-side cancel is not firing.
]

= Pull-Mode Dispatch

The pull/report path is the work-delivery protocol: a pod born knowing its
derivation speaks two idempotent unaries --- `ExecutorService.PullAssignment`
and `ExecutorService.ReportOutcome` --- and the controller folds pod/Job
terminal status through `AdminService.ReportAttemptOutcome`. The stream
session protocol it replaced is deleted (its RPCs are unconditional error
stubs until the 1d proto sweep), so every in-flight build is an attempt
minted by the pull transaction (`drv_executions.dispatch_mode = 'pull'`):
the durable open-attempt row is the scheduler's only per-executor state, the
establishment sweep is its only time-based repair, and the operator surfaces
project that row set (#rref("sched.admin.list-open-attempts"),
#rref("sched.admin.list-executors")).

#r("sched.executor.pull-transaction")[
  `PullAssignment(executor_token, intent_id)` MUST be leader-served and MUST
  perform its work as one atomic transaction: validate the token↔intent
  binding (#rref("sec.executor.identity-token") applied per-unary), resolve
  the derivation by intent id, transition it out of Ready, mint `exec_id`,
  insert the `drv_executions` row (with `dispatch_mode = 'pull'` and
  `source_node` when known), write or refresh the active `assignments` row
  carrying the serving generation, pin GC live-inputs, and commit only if
  the serving generation is not below the durable claims floor (GREATEST
  over `leader_generation_claims` and `assignments`); a below-floor serving
  generation MUST abort the transaction with no row written and return the
  same retryable not-leader error `ensure_leader` produces. A re-pull while
  the attempt is open and bound to the same pulling identity MUST return
  the identical payload and `exec_id` without writing anything.
]
The fence is transaction-side (the worker-side generation latch has no
distribution channel without the stream); `WorkAssignment.generation` stays
on the wire as observability only. The two-believer pull race (two open
attempts, double charge for one pod death) is closed at the same place the
work-binding authority lives.

#r("sched.executor.pull-gone")[
  `PullAssignment` MUST return `Gone` --- and MUST NOT write any state ---
  when the derivation is no longer wanted by anyone: cancelled, substituted,
  completed, skipped, failed permanently/poisoned, or absent from the DAG.
  `Gone` is terminal for the pod: it exits 0, the Job completes, and nothing
  is charged.
]

#r("sched.executor.pull-not-ready")[
  `PullAssignment` MUST return `NotYetReady{retry_after_seconds}` --- never
  `Gone`, never another attempt's payload, and never a write --- when the
  derivation is still wanted but not currently deliverable to the pulling
  pod: its dependencies are not yet built (forecast-spawned pod arrived
  early), it is being substituted, it is awaiting retry, or it is currently
  open/Assigned/Running on a different executor (a stream-mode assignment
  during coexistence, or an open attempt bound to another pod). The pod
  re-pulls after the suggested delay and exits 0 charge-free if it has
  received only `NotYetReady` for its idle-timeout bound.
]
This is the OA6(a) decision: returning `Gone` for a wanted-but-not-Ready
derivation would produce a reap→respawn→Gone churn loop (the controller's
stale-intent reap deletes the terminal Job for a still-wanted intent every
tick), and delivering while an attempt is open elsewhere would re-point the
active assignment away from the executor actually building it.

#r("sched.executor.report-idempotent")[
  `ReportOutcome(exec_id, CompletionReport)` MUST be idempotent by
  `exec_id`: the first report for an open attempt runs the existing
  classification path and appends the attempt row exactly once; a duplicate
  report, a report for an attempt already established or otherwise
  terminal, or a report whose `exec_id` matches no open attempt MUST be
  acknowledged and MUST NOT write a second row, a second verdict, or any
  new state. The ack MUST be returned only after the appending transaction
  commits.
]

#r("sched.attempt.no-attempt-no-op")[
  A pod-terminal report (`ReportAttemptOutcome`) for an attempt identity
  with no attempt row --- a pod that died, was reaped, or hit its deadline
  without ever completing a pull --- MUST be acknowledged and MUST charge
  nothing: no attempt row is inserted, no retry budget is consumed, no
  resource floor is bumped, and no establishment is triggered; its only
  permitted side effects are clearing the intent's ICE cell and re-arming
  the spawn intent.
]
This carries forward the as-built rule that never-assigned/assigned-only pod
deaths never count (the `recently_disconnected` no-entry no-op arms), re-keyed
onto the durable open-attempt view.

#r("sched.attempt.synthesized-verdict")[
  A controller-synthesized terminal report (reason cancelled, preempted, or
  reaped) for an open pull-mode attempt that has no worker-reported
  classification row MUST close that attempt charge-free in one
  generation-fenced appending transaction --- exactly one uncharged terminal
  row whose `termination_reason` carries the synthesized reason, with the
  assignment row closed --- and MUST requeue a still-wanted derivation at
  that fold, never at the establishment sweep. A worker `ReportOutcome`
  whose result is `Cancelled` for a still-wanted open pull-mode attempt (the
  AD5 SIGTERM-abort report) MUST resolve the same way: charge-free closure
  and requeue, never an infrastructure-failure charge. Neither path may
  requeue a derivation that is no longer wanted, and other pod-terminal
  reasons without a worker classification remain the establishment sweep's
  to classify.
]
The synthesized-verdict close is the scheduler half of the AD5/C5/C6
successor (`ctrl.job.synthesize-on-delete`, `ctrl.drain.disruption-target`):
the controller's deletion destroys the only pod-terminal status the unified
report could otherwise fold, so the synthesized report itself must carry the
closure. Pod-initiated aborts of still-wanted work are platform terminations
(preemption, scale-down, controller deletes), not worker faults --- charging
them as infrastructure failures would burn the infra budget on disruptions
the design accepts as charge-free.

#r("sched.attempt.establishment-window+2")[
  The establishment sweep MUST visit every open pull-mode attempt
  (`dispatch_mode = 'pull'`, no terminal classification) on every sweep, and
  MUST establish an attempt only after its deadline plus the configured
  `establishment_report_slack` has elapsed with no terminal row, where the
  deadline is anchored to the value the attempt was dispatched with (the
  solved deadline persisted by the pull mint): a sweep-time re-solve may
  widen the window but MUST never shrink it below the dispatched deadline
  while the attempt is open. The store-probe arm adopts the attempt as
  completed when its outputs are present, otherwise the establishment
  appends exactly one executor-crash/unreported classification (charged per
  the existing C2 discipline) and requeues the derivation. Establishment
  MUST never fire inside the window, MUST never visit stream-mode attempts,
  and the establishing transaction MUST apply the same generation-floor
  fence as the pull transaction.
]
The sweep reads durable rows --- not an in-memory claim a one-shot timer can
forget --- so the post-failover "deferred claim forgotten" defect class is
closed structurally. Stream-mode attempts keep the as-built 60 s correlation
machinery as their only establishment vehicle during coexistence. Anchoring
the window to the dispatched deadline (072's `deadline_secs`) keeps a fitted
estimate or hw-table change that shrinks mid-flight from establishing a
healthy attempt that is still inside the deadline its pod really runs under;
the residual gap between the Job's `activeDeadlineSeconds` render and the
mint-time solve is covered by the report slack.

#r("sched.admin.list-open-attempts")[
  `AdminService.ListOpenAttempts` MUST return every open pull-mode attempt
  --- an active `assignments` row joined to its `drv_executions` row with
  `dispatch_mode = 'pull'` and no terminal `drv_attempts` fill --- and MUST
  NOT list stream-mode executors or stream-mode in-flight builds (those
  remain `ListExecutors`' surface). Each entry carries the intent id (drv
  hash), derivation path, `exec_id`, executor identity, source node when
  known, the assignment's generation, and its age; the response carries
  `leader_for_secs` with the same fail-closed freshness semantics as
  #rref("sched.admin.list-executors-leader-age+2"). The RPC is leader-served.
]
The same view feeds the #(refs.metric)("rio_scheduler_open_attempts") gauge
(the busy-fleet gauge; the stream fleet's `workers_active` is retired) and
the establishment sweep, and backs the re-implemented
#rref("sched.admin.list-executors+2") projection.

= Backpressure

The scheduler applies @backpressure at multiple layers to prevent overload:

*Pull admission:* work delivery is pod-initiated --- the scheduler pushes
nothing, so there is no per-executor send window to manage. A pull or report
that cannot be served (overload, failover, recovery) surfaces as a retried
unary on the pod side; the actor never queues undelivered work for a slow
consumer.

#r("sched.backpressure.hysteresis+2")[
  *Actor queue depth limit:* The DAG actor's `mpsc` channel has a fixed
  capacity (`ACTOR_CHANNEL_CAPACITY` = 10,000 messages; compile-time constant).
  If the queue depth exceeds 80% of capacity:
  + New `SubmitBuild` requests from the gateway receive gRPC
    `RESOURCE_EXHAUSTED` status.
  + The scheduler increments the
    #(refs.metric)("rio_scheduler_queue_backpressure") counter for alerting.
  + Pull-mode report intake keeps using `send_unchecked` --- a completion
    report must never be dropped (a lost completion would leave the
    derivation stuck `Running`); the pod's bounded retry is the relief
    valve, never discard.
  Normal processing resumes when the queue depth drops below 60% (hysteresis to
  prevent oscillation). (The stream-era intake arms this rule used to
  enumerate --- blocking the `BuildExecution` reader on completions and
  dropping `LogBatch` forwards --- left with that protocol surface.)
]

*Gateway timeout:* If a `SubmitBuild` request takes longer than 30 seconds to
receive an initial acknowledgement from the DAG actor, the gateway handler
returns gRPC `DEADLINE_EXCEEDED`. This timeout is enforced client-side in
rio-gateway, not in the scheduler. The gateway may retry with exponential
backoff. This prevents unbounded request queueing at the gateway layer.

= State Storage (PostgreSQL)

#table(
  columns: (auto, 1fr),
  align: (left, left),
  table.header([Table], [Contents]),
  [`builds`], [Build requests, status, timing, `tenant_id`],
  [`derivations`], [Derivation metadata, scheduling state, `tenant_id`],
  [`derivation_edges`],
  [DAG edges (`parent_id`, `child_id`) as a separate join table for concurrent
    merge safety],

  [`assignments`],
  [Derivation → executor mapping, status, assignment generation counter],

  [`build_derivations`],
  [Many-to-many mapping: which builds are interested in which derivations],

  [`build_samples`],
  [Per-completion telemetry rows feeding the ADR-023 SLA fit (ring-buffered per
    `(pname, system, tenant)`)],

  [`drv_executions`],
  [Per-execution lifecycle row (`exec_id` PK, `drv_hash`, `status`,
    `final_line_count`) --- written by the scheduler at dispatch and terminal,
    read by rio-store's log completeness predicate and latest-exec resolution],

  [`build_event_log`],
  [Prost-encoded `BuildEvent` per (`build_id`, `sequence`) for gateway
    `since_sequence` replay across failover],

  [`scheduler_live_pins`],
  [Auto-pinned live-build input closures (`store_path_hash`, `drv_hash`).
    Written by `pin_live_inputs` at dispatch; unpinned on completion. Used by
    rio-store's GC mark phase as a root seed.],
)

#r("sched.db.tx-commit-before-mutate")[
  In-memory `DerivationState.db_id` MUST NOT be set until the persisting
  transaction has committed. Edge resolution during `persist_merge_to_db` reads
  the transaction-local `id_map` (returned by `RETURNING`), not `self.dag` ---
  decoupling the two eliminates the phantom-`db_id` class of bug where a
  rollback leaves in-memory state pointing at a `derivation_id` that never
  became durable.
]

#r("sched.db.batch-unnest")[
  Batch INSERTs into `derivations` / `build_derivations` / `derivation_edges`
  MUST use `UNNEST` array parameters (one bind per column, any row count).
  `QueryBuilder::push_values` generates one bind parameter per column per row,
  which hits PostgreSQL's 65535-parameter wire-protocol limit at 7282 rows × 9
  columns --- below the \~30k-derivation size of a NixOS system closure.
]

#r("sched.db.partial-index-literal")[
  Queries that filter by terminal status MUST interpolate the terminal-status
  list as a SQL literal (`NOT IN ('completed', ...)`), not bind it as a
  parameter (`<> ALL($1::text[])`). The partial index `derivations_status_idx`
  has a literal predicate; the planner can only prove a query's `WHERE` implies
  the index predicate at plan time, before bind values are known. A
  parameterized filter is opaque and forces a seq scan. The literal string and
  `DerivationStatus::is_terminal()` MUST stay in sync (drift-tested).
]

#r("sched.db.derivations-gc+2")[
  Terminal `derivations` rows with no `build_derivations` link and no ACTIVE
  (`pending`/`acknowledged`) `assignments` row are deleted by a periodic
  Tick-driven sweep (batched `LIMIT 1000` per pass). The same statement deletes
  `derivation_edges` rows referencing any victim id (migration 028 dropped the
  FKs --- no cascade --- so without this the edges table grows unbounded at
  avg-fanout× the derivation churn rate). Recovery never re-reads terminal rows
  (`WHERE status NOT IN <terminal>`); once the owning build is deleted
  (cascades `build_derivations`), the derivation row is unreachable. Terminal
  `assignments` rows (closed by #rref("sched.db.assignment-terminal-on-status"))
  do not block: migration 034 made the FK `ON DELETE CASCADE`. Without the
  sweep, `dependency_failed` rows from large failed closures accumulate
  unboundedly --- I-169.2 observed 1.16M rows.
]

#r("sched.db.assignment-terminal-on-status+2")[
  Every persist of a terminal `derivations.status` (via
  `update_derivation_status`, `update_derivation_status_batch`, or
  `persist_poisoned`) MUST also transition any active (`pending`/`acknowledged`)
  `assignments` row for that derivation to the mapped terminal status
  (`completed`/`failed`/`cancelled`) and stamp `completed_at`, *in a single
  transaction*. A crash between the two writes leaves the derivation terminal
  but the assignment `pending`, which is permanently un-GC-able
  (#rref("sched.db.derivations-gc") `NOT EXISTS` never matches; recovery's
  `load_nonterminal_derivations` filters it out so no orphan-reconcile path
  reaches it either). I-209/I-210: before this fold, only
  `handle_success_completion` closed the assignment row; every other terminal
  path (poison, cancel, cache-hit-at-merge, orphan recovery, FOD-from-store)
  left it `pending`, and `derivations` leaked --- 12,609 stuck rows on terminal
  derivations observed in production.
]

#r("sched.db.assignment-stale-sweep")[
  On every recovery, `sweep_stale_assignments` closes any
  `pending`/`acknowledged` `assignments` row whose derivation is already
  terminal. Defense-in-depth backstop for
  #rref("sched.db.assignment-terminal-on-status"): repairs rows leaked by older
  binaries (pre-transaction-wrap) and any future caller that bypasses the
  transactional chokepoint. Mirrors `sweep_stale_live_pins`.
]

#r("sched.db.clear-poison-batch+2")[
  `clear_poison` has a `clear_poison_batch(&[DrvHash])` variant using `WHERE
  drv_hash = ANY($1)`. The merge-time resubmit-reset path (`reset_on_resubmit`)
  clears poison for every node a resubmit flipped from terminal to fresh;
  per-hash sequential calls inside the single-threaded actor cost N round-trips
  on the dispatch hot path. The batch variant leaves the frozen
  `resubmit_cycles` mirror column untouched --- the new cycle index is carried
  by the `resubmit_reset` attempt-ledger row appended in the same transaction
  (the scalar zeroes the column: admin/TTL = full reset; resubmit = bound
  accumulates via the ledger).
]

== Schema (pseudo-DDL)

```sql
CREATE TABLE builds (
    build_id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID REFERENCES tenants(tenant_id) ON DELETE SET NULL,  -- nullable; single-tenant mode leaves NULL
    status          TEXT NOT NULL CHECK (status IN ('pending', 'active', 'succeeded', 'failed', 'cancelled')),
    priority_class  TEXT NOT NULL DEFAULT 'scheduled' CHECK (priority_class IN ('ci', 'interactive', 'scheduled')),
    submitted_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ,
    error_summary   TEXT
);
CREATE INDEX builds_status_idx ON builds (status) WHERE status IN ('pending', 'active');

CREATE TABLE derivations (
    derivation_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID REFERENCES tenants(tenant_id) ON DELETE SET NULL,  -- nullable; single-tenant mode leaves NULL
    drv_hash            TEXT NOT NULL,          -- input-addressed: store path; CA: modular derivation hash
    drv_path            TEXT NOT NULL,          -- full /nix/store/...-foo.drv path
    pname               TEXT,
    system              TEXT NOT NULL,
    status              TEXT NOT NULL CHECK (status IN ('created', 'queued', 'ready', 'assigned', 'running', 'completed', 'failed', 'poisoned', 'dependency_failed')),
    required_features   TEXT[] NOT NULL DEFAULT '{}',
    assigned_builder_id TEXT,
    -- assignment_gen lives on assignments table (as generation), not here
    retry_count         INT NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT derivations_drv_hash_uq UNIQUE (drv_hash)
);
CREATE INDEX derivations_status_idx ON derivations (status) WHERE status NOT IN ('completed', 'poisoned', 'dependency_failed');
-- Partial index: most rows NULL in single-tenant mode (migration 009 Part A).
CREATE INDEX derivations_tenant_idx ON derivations (tenant_id) WHERE tenant_id IS NOT NULL;

CREATE TABLE derivation_edges (
    parent_id   UUID NOT NULL REFERENCES derivations (derivation_id),
    child_id    UUID NOT NULL REFERENCES derivations (derivation_id),
    PRIMARY KEY (parent_id, child_id)
);
CREATE INDEX derivation_edges_child_idx ON derivation_edges (child_id);

CREATE TABLE build_derivations (
    build_id        UUID NOT NULL REFERENCES builds (build_id),
    derivation_id   UUID NOT NULL REFERENCES derivations (derivation_id),
    PRIMARY KEY (build_id, derivation_id)
);
CREATE INDEX build_derivations_deriv_idx ON build_derivations (derivation_id);

CREATE TABLE assignments (
    assignment_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    derivation_id       UUID NOT NULL REFERENCES derivations (derivation_id),
    builder_id          TEXT NOT NULL,
    generation          BIGINT NOT NULL,        -- leader generation counter
    status              TEXT NOT NULL CHECK (status IN ('pending', 'acknowledged', 'completed', 'failed', 'cancelled')),
    assigned_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at        TIMESTAMPTZ
);
CREATE UNIQUE INDEX assignments_active_uq ON assignments (derivation_id) WHERE status IN ('pending', 'acknowledged');
CREATE INDEX assignments_builder_idx ON assignments (builder_id, status);
```

#info[
  Auxiliary tables omitted from pseudo-DDL above: `drv_executions`
  (per-execution lifecycle, `exec_id` PK; the log subsystem's anchor row) and
  `build_event_log` (Prost-encoded BuildEvent per sequence for gateway
  replay). See `rio-migrations/migrations/` for full schema.
]

= Leader Election

#r("sched.lease.k8s-lease+2")[
  The scheduler uses a *Kubernetes Lease* (`coordination.k8s.io/v1`) for leader
  election, via an in-house implementation modeled on client-go's
  `leaderelection` package. A background task polls every 5 seconds against a
  15-second lease TTL (3:1 renew ratio, per Kubernetes convention). On the
  acquire transition (standby → leader), the task derives the leadership
  generation from the Lease's `leaseTransitions` count
  (#rref("sched.lease.generation-fence")) and sets `is_leader=true`; on the
  lose transition, it clears `is_leader`. The dispatch loop checks `is_leader`
  and no-ops while standby (DAGs are still merged so state is warm for
  takeover).
]

- *Configuration:* Enabled by setting `RIO_LEASE_NAME`. When unset (VM tests,
  single-scheduler deployments), the scheduler runs in non-K8s mode:
  `is_leader` defaults to `true` immediately and generation stays at 1.
  Namespace is read from `RIO_LEASE_NAMESPACE` or the in-cluster
  service-account mount; holder identity defaults to the pod's `HOSTNAME`.
- *Optimistic concurrency:* All lease mutations (acquire, renew, step-down) use
  `kube::Api::replace()` (HTTP PUT) with `metadata.resourceVersion` from the
  preceding GET. The apiserver rejects with 409 Conflict if the lease changed
  between GET and PUT --- exactly one of N racing writers succeeds. A 409 on
  renew is treated as an immediate lose transition (someone stole the lease
  since our GET); a 409 on steal means another standby raced and won.
- *Observed-record expiry:* A standby does not compare the lease's `renewTime`
  against its own wall clock (cross-node skew would make that unreliable).
  Instead, it records the lease's `metadata.resourceVersion` plus a local
  monotonic `Instant` when that rv was first seen. The apiserver bumps rv on
  every write, so a leader renewing every 5s produces a fresh rv every 5s. If
  rv stays unchanged for `STEAL_AFTER` (`LEASE_TTL` + `FENCE_MARGIN` = 19s) of
  local time, nobody has written --- steal. Only the standby's own `Instant`
  monotonicity matters; the `renewTime` value is never read.
- *Transient API errors:* On apiserver errors, the loop logs a warning and
  retries on the next tick without flipping `is_leader`. If errors persist past
  `SELF_FENCE_AFTER`, the local self-fence (#rref("sched.lease.self-fence+2"))
  flips `is_leader=false` and another replica acquires --- correct behavior for
  a replica with broken K8s connectivity.
- *Split-brain window:* This is a polling loop, not a watch-based fence. The
  self-fence deadline (11s) sits `2 × FENCE_MARGIN` (8s) before the steal
  threshold (19s): a leader that loses apiserver connectivity stops believing
  it leads before any replica that retains connectivity is allowed to steal,
  so a partition does not produce two simultaneous believers unless one
  replica's clock pauses or skews by more than the margin leaves over after
  the renew-polling slack (1.5s). For that residual --- and for the
  pre-asymmetric deployments the degraded-regime modules
  (`leaderElectionBase`, `leaderElectionDeletion` in
  `docs/spec/models/leaderElection.qnt`) describe ---
  work-binding stays safe: DAG merge dedups by `drv_hash`, and the
  authority-exercising transactions are fenced against the durable claims
  floor (#rref("sched.lease.generation-fence")), so a stale believer's mints
  and charges abort once the new leader's generation is durable. Worst case
  inside the window: a derivation builds twice and produces the same
  deterministic output. Wasteful but correct.

#r("sched.lease.at-most-one-leader+3")[
  The Lease MUST be held by at most one scheduler identity at the apiserver
  at any moment. `replace()` is preconditioned on `metadata.resourceVersion`
  from the preceding GET; the apiserver returns 409 Conflict to all but one
  of N concurrent writers. Without the precondition every writer gets HTTP
  200 and last-write-wins --- the `kube-leader-election` 0.43 failure mode
  (see the `rio-lease/src/election.rs` header). This half is hard. A
  replica's _belief_ that it leads (`is_leader=true`) MAY lag the Lease
  state: a deposed replica that retains apiserver connectivity learns it
  lost on its next GET (one `RENEW_INTERVAL`); a partitioned replica
  self-fences after `SELF_FENCE_AFTER` of no apiserver contact, per its own
  monotonic clock (#rref("sched.lease.self-fence+2")) --- `2 × FENCE_MARGIN`
  before any replica's steal threshold, so under clock skew within the
  margin's budget the lag never produces two simultaneous believers. This
  half is soft only against a clock that _pauses_: a process whose clock
  stops cannot self-fence at the moment its deadline passes --- it discovers
  its lateness only when the clock next reads --- and no fence/steal
  separation closes that. The Chubby-style fix for the residual is a fencing
  token at the resource boundary, which #rref("sched.lease.generation-fence+2")
  provides; that rule, not this one, makes a dual-belief window safe rather
  than merely unreachable. The formal model in
  `docs/spec/models/leaderElection.qnt` verifies the three claims
  separately: `atMostOneCASWinner` (the hard half --- two rv-guarded PUT
  actions cannot both succeed at the same `resourceVersion`), `neverDual` (the
  healthy regime, the `leaderElectionAsymmetric` module --- with the
  fence/steal separation exceeding the renew interval plus the round-trip
  clock skew, no two replicas ever simultaneously believe they lead), and
  `boundedDualLeadership` (the degraded regime, the base and deletion modules
  --- when the separation is insufficient, every reachable dual-belief state
  still has a discovery mechanism armed).
]

#r("sched.lease.self-fence+2")[
  If the lease loop believed it was leading but has not had a successful
  apiserver round-trip in over `SELF_FENCE_AFTER` (`LEASE_TTL` −
  `FENCE_MARGIN` = 11s), it MUST flip `is_leader=false` locally
  (`maybe_self_fence`) and emit
  #(refs.metric)("rio_scheduler_lease_lost_total"). The self-fence deadline
  MUST sit `2 × FENCE_MARGIN` before the steal threshold (`STEAL_AFTER` =
  `LEASE_TTL` + `FENCE_MARGIN` = 19s), so a leader deposed by
  unreachability has provably stopped believing before any replica that
  _can_ reach the apiserver is allowed to steal. The self-fence does NOT
  attempt `step_down()` or `pod-deletion-cost` PATCH (the apiserver is
  unreachable). `last_successful_renew` is reset on every Standby/Conflict
  observation as well as on successful renew --- the clock tracks "am I
  blind", not "am I leader" --- but every round-trip that leaves the
  replica _leading_ ends in an rv-bumping write, so a fresh fence clock on
  a believer always coincides with a fresh observation clock on every
  standby.
]

The two deadlines are anchored at different moments: the leader stamps
`last_successful_renew` when its renew _response_ arrives, a standby stamps
its observation when it _sees_ the rv change. Without a margin the standby's
deadline can land first with zero clock skew. The margin condition the
asymmetry must satisfy is `2 × FENCE_MARGIN ≥ RENEW_INTERVAL + 2 ×
clock_skew` --- the renew interval is the victim's fence-check latency (the
loop evaluates `maybe_self_fence` at the top of every tick, before the renew
attempt), and what remains of
the 8s separation after the 5s renew interval is a 1.5s one-sided clock-skew
budget. The formal model verifies this as `neverDual` over the
`leaderElectionAsymmetric` module of `docs/spec/models/leaderElection.qnt`,
with the boundary measured from both sides:
one model tick less separation and a dual-belief state is reachable. The
residual is a clock that pauses for longer than the budget (suspend, a long
GC, a frozen VM) --- no fence/steal separation closes that, and
#rref("sched.lease.generation-fence+2") is the backstop. The compile-time
assertions on the rio-lease constants pin the derivations, the margin
condition, and the response-anchoring premise (the renew attempt deadline
keeps the response-anchored fence within the commit-anchored bound the
model assumes) so no constant moves without the others.

#r("sched.lease.standby-drops-writes")[
  A replica that has lost the lease MUST NOT write scheduler-owned PG state
  (`derivations`, `realisations`, `build_samples`, `build_event_log`). The
  pull-mode work surfaces are leader-gated at the gRPC layer and the fenced
  transactions re-check the durable floor
  (#rref("sched.lease.generation-fence")). `ProcessCompletion`, `CancelBuild`,
  `AckSpawnedIntents`, `ReconcileAssignments`,
  `SubstituteComplete`, and `Tick` are additionally gated at actor dispatch as
  defense-in-depth.
  `reassign_derivations` --- the requeue tail that can poison a derivation
  and run the terminal log epilogue --- is individually leader-gated at its
  own chokepoint (the stream-era `ExecutorConnected`/`Disconnected`/
  `DrainExecutor`/`Heartbeat`/`ReportExecutorTermination`/`ForwardPhase` arms
  it used to ride behind are deleted with their commands).
]

- *Terminal-build cleanup:* the `CleanupTerminalBuild` arm also stays ungated
  (in-memory build/event-map removal and the DAG reap run on standby); its
  post-reap survivor re-evaluation --- which can persist derivation status,
  clear the persisted `topdown_pruned` mark, and terminally fail builds via the
  topdown fail-fast --- is individually leader-gated, like the per-sub-call
  gates above.

#r("sched.lease.generation-fence+3")[
  *Authority is generation-fenced at the transactions that exercise it.* The
  leadership generation MUST derive from the Lease's `leaseTransitions` count
  (`generation = leaseTransitions + 1`): the apiserver bumps that field
  atomically with the holder change inside the resourceVersion-guarded PUT, so
  two replicas that both believe they lead can never have acquired at the same
  count --- their generations are distinct without any coordination beyond the
  CAS that already serializes the steal. Every authority-exercising
  transaction --- the pull mint, the establishment charge, and the
  synthesized/uncharged close --- MUST carry the serving replica's generation,
  persist it on the row it writes (`assignments.generation`), and commit only
  if that generation is not below the durable claims floor (`GREATEST` over
  `leader_generation_claims` and `assignments`); a below-floor serving
  generation MUST abort the transaction with nothing written and surface as
  the same retryable not-leader error the leader guard produces, so a
  deposed-but-still-believing leader can neither bind new work nor consume
  outcomes. This rule is the backstop for the clock-pause residual the
  fence/steal asymmetry cannot close (#rref("sched.lease.self-fence+2")).
  Scheduler writes outside these transactions are not generation-fenced; they
  remain idempotent upserts keyed by `drv_hash` with monotone status
  transitions, so the brief dual-writer windows the lease model prices do not
  corrupt state.
]

A local counter cannot provide the distinctness half of this rule: an
incremented-in-memory generation seeded from a high-water mark collides
whenever a leader is deposed before persisting anything (the
generation-collision counterexample preserved in
`docs/spec/models/leaderElection.qnt`'s history). The
transition count is the epoch source only while the Lease object exists;
#rref("sched.lease.generation-claim") extends the distinctness guarantee
across Lease-object deletion. The fence moved with the work-binding
authority: the stream-era form of this rule had executors latch the
generation from heartbeat replies and reject older `WorkAssignment`s, and the
previously memo'd "optional future hardening" (a PG-side generation guard on
the writes) is now the implemented mechanism for exactly the writes that bind
work and consume outcomes --- the worker-side latch retired with the stream
(its `WorkAssignment.generation` field stays on the wire as observability
only), and what a worker could once latch is replaced by what the durable
floor covers.

#r("sched.lease.generation-claim+2")[
  Before completing recovery and ungating dispatch, a newly-acquired leader
  MUST durably record the generation it will dispatch at as a row in the
  `leader_generation_claims` ledger, and the recovery generation floor
  (#rref("sched.recovery.fetch-max-seed")) MUST be computed over both the
  assignment history and that ledger. A holder whose own claim row already
  sits at the claim target defined there MUST retain that generation rather
  than claim a new one --- the ledger gains no new row on a same-epoch
  re-acquire.
]

This is the Chubby-sequencer discipline: a fencing token is only as durable as
the state that allocates it. The Lease's transition count
(#rref("sched.lease.generation-fence+2")) is the primary epoch source, but
`kubectl delete lease` resets it to zero --- and the Kubernetes ecosystem
treats Lease deletion as a routine remedy for a stuck election, not a
disaster. The only store that survives Lease deletion in this architecture is
PostgreSQL, so the generation must be recorded there _before_ it is used, not
as a side effect of the first dispatch: a leader deposed before persisting any
assignment would otherwise leave no trace, and its successor would seed from
the same stale floor and collide. The `generation` PRIMARY KEY doubles as the
CAS: two holders claiming the same generation concurrently resolve by `ON
CONFLICT DO NOTHING` --- the loser re-targets past the claims high-water and
retries, bounded. The `holder_id` column is the load-bearing same-epoch
discriminator: a holder re-acquiring its own epoch (a self-fence false alarm
followed by a successful renew --- the Lease's transition count did not move,
so the epoch did not change) finds its own row at its current generation
and retains it, rather than burning a generation --- and fencing its own
in-flight work --- on every connectivity blip. A row at our generation with a
_different_ `holder_id` is unambiguously a cross-incarnation collision and
forces the bump --- as does the absence of any claim row at a floor that ties
it. A claim-write failure degrades to a logged, counted
(#(refs.metric)("rio_scheduler_generation_claim_failed_total")) proceed-without-claim: blocking
recovery on the claim would turn a PG blip at failover time into a leader that
holds the Lease but never dispatches.

The degradation is safe because the Lease's transition count and the claims
ledger are _redundant_ epoch sources: the model's `leaderElectionPgFaults`
regime (`docs/spec/models/leaderElection.qnt`)
proves every invariant survives a skipped claim write and a PG point-in-time
restore _alone_ (the lease-derived term of the generation still increases
strictly across successive stealers when the floor term lies), and only the
conjunction of a PG fault with a Lease deletion --- both epoch sources
destroyed --- reaches a collision. Those conjunctions are the documented
residuals; that regime's module header records the procedure for re-deriving
them, and the trace evidence lives in that regime's introducing commit
message.

#r("sched.lease.claim-before-advertise+2")[
  A newly-acquired leader MUST durably claim the generation it will exercise
  authority at before it completes recovery and serves authority-exercising
  work (claim-before-serve): the claim INSERT precedes
  `set_recovery_complete()` (#rref("sched.lease.generation-claim")), and the
  fenced transactions that mint pulls, charge establishments and apply
  synthesized closes additionally persist their serving generation on the
  rows they write, so every generation at which authority has ever been
  exercised is covered by the durable floor those same transactions are
  admitted against (#rref("sched.lease.generation-fence")).
]

The rule keeps its historical `claim-before-advertise` name; there is no
advertisement channel left, and the operative ordering is claim-before-serve.
The stream-era form gated the heartbeat-reply sentinel
(`HeartbeatResponse.generation = 0` until recovery completed) so that workers
could never latch an advertised-but-unclaimed generation --- a latch that
only rises would otherwise wedge the fleet against the active leader after a
Lease deletion. With the latch and the heartbeat channel retired, nothing
latches a generation and that failure mode cannot form; what remains
load-bearing is the ordering itself --- recovery claims before it completes,
and the work-serving surfaces come up behind recovery --- which keeps "what
has exercised authority" inside "what the durable floor covers"
(#rref("sched.recovery.fetch-max-seed")). The degraded paths keep their
existing pricing: a claim-write failure or claim-conflict exhaustion proceeds
unclaimed and a floor-unreadable recovery completes at the recovery-entry
generation (both only after the post-claim leadership confirmation,
#rref("sched.recovery.bump-confirm")) --- the one-term residual is unchanged,
and even an unclaimed term's exercised authority becomes durable through the
generation its fenced transactions persist on the rows they write. Non-K8s
single-scheduler deployments construct `LeaderState` with recovery already
complete, so the ordering is trivially satisfied there.

#r("sched.lease.graceful-release")[
  On graceful shutdown (SIGTERM), if the lease loop was leading, it calls
  `step_down()` to clear `holderIdentity` before the process exits. This is an
  optimization, not a correctness requirement: without it, the next replica
  waits up to `STEAL_AFTER` (19s) for observed-record expiry. With it, the
  next replica's `decide()` sees an empty holder and steals on its next poll
  tick (one `RENEW_INTERVAL`, 5s). The `step_down()` call is a
  resourceVersion-guarded PUT (409 →
  someone already stole, treated as success); `main()` awaits the lease-loop's
  `JoinHandle` after `serve_with_shutdown` returns, ensuring the PUT lands
  before process exit. If `step_down()` fails (apiserver unreachable), the loop
  logs a warning and observed-record expiry is the fallback.
]

#r("sched.lease.rebound")[
  A renew round that resolves Leading while this replica already believes it
  leads, but whose observed `leaseTransitions` count differs from the count
  recorded at this replica's most recent acquire edge or rebound, MUST be
  treated as a late-observed holder change: the lease loop MUST re-record the
  observed count, re-derive the generation from it via `fetch_max`, clear
  `recovery_complete`, and re-fire the acquire hook so recovery re-runs
  against the post-change state; `is_leader` MUST NOT be cleared by this
  transition.
]

The shapes this catches land entirely inside this replica's observation gap
--- a foreign term that ended in a graceful vacate
(#rref("sched.lease.graceful-release")), or a delete/recreate --- so neither
an acquire nor a lose edge ever fires locally; one `kubectl delete lease`
during a renew-blind window shorter than the self-fence deadline suffices.
While this replica holds continuously, only a holder change or a
delete/recreate can move `leaseTransitions` (renews never write it), and a
foreign holder still present at the next successful round resolves
Standby/Conflict through the existing lose edge --- so an unequal count on a
still-leading round is always a genuine discontinuity, and the cost of acting
on one is a single recovery re-run with dispatch gated during it. Only the
acquire hook is re-fired: a synthesized lose would force a pointless wipe of
state the immediately-following re-recovery rebuilds (and, if the full lose
edge were synthesized, an `is_leader = false` blip), while
adding nothing to the dispatch gating the rebound's own `recovery_complete`
clear already provides; hook delivery is ordered
(#rref("sched.lease.hook-order")), so the choice is about avoiding wasted
work, not about reordering. The accepted
residual is the count coincidence: an observed count that lands exactly back
on the recorded value is indistinguishable from steady state --- the same
coincidence pricing as the recovery gate's deletion-ABA note --- and in that
shape no command is queued, so a recovery loaded across the foreign tenure
persists until the next real leadership change or rebound. The scheduler's
acquire-hook counter (#(refs.metric)("rio_scheduler_lease_acquired_total"))
counts rebounds too; that is deliberate --- a rebound is operationally an
acquisition-shaped event --- and no separate counter is added.

#r("sched.health.shared-reporter+2")[
  The lease toggle calls `set_not_serving`/`set_serving` on the SAME
  `HealthReporter` the gRPC server was built with (single port). A fresh
  `health_reporter()` would never be toggled → standby always appears Ready →
  cluster split.
]

#r("sched.grpc.leader-guard")[
  Every gRPC handler (SchedulerService, ExecutorService, AdminService) checks
  `is_leader` at entry and returns `UNAVAILABLE` ("not leader") when false.
  This decouples K8s readiness from leadership: both pods are Ready (process
  up, gRPC listening), but only the leader serves RPCs. Clients with a
  health-aware balanced channel discover the leader via
  `grpc.health.v1/Check` (which reports NOT_SERVING on the standby) and route
  accordingly. A client that hits the standby anyway (race during failover, or
  a per-call connect via the ClusterIP Service) gets UNAVAILABLE, which by gRPC
  convention is retryable --- on the health-aware balancer, the retry goes to
  the leader.
]

#r("sched.lease.deletion-cost+2")[
  On the acquire transition, the lease loop reconciles both leader marks onto
  its own Pod in one merge patch: the annotation
  `controller.kubernetes.io/pod-deletion-cost: "1"` and, when configured, the
  leader role label the leader-only `rio-scheduler-leader` Service selects on;
  on the lose transition it writes cost `"0"` and removes the label.
  Kubernetes's ReplicaSet controller sorts pods by the annotation (ascending,
  lower = kill first) when picking which pod to evict during scale-down ---
  including the surge-reconcile phase of RollingUpdate. With cost=1 on the
  leader and cost=0 on the standby, `kubectl rollout restart` kills the standby
  first, new pod comes up, acquires (old leader step_down on SIGTERM), no
  double leadership churn. While leading, the same retried reconcile also
  strips the leader marks from any other Pod still carrying them (a partitioned
  ex-leader cannot remove its own); a failed sweep leaves the marks unconverged
  and the reconcile retried, exactly like a failed own-Pod patch. The reconcile
  is level-triggered, not fire-and-forget: the patch is spawned so the lease
  loop never blocks on the apiserver, at most one marks-reconcile attempt is in
  flight at a time, and each attempt is bounded by a call timeout; the marks
  MUST be re-reconciled on subsequent successful election round-trips ---
  beginning with the first round-trip after the in-flight attempt completes ---
  until they reflect the Pod's current leadership. While reconciliation keeps
  failing, scale-down ordering is arbitrary (the annotation half) and the
  leader-only Service's endpoints are missing or stale --- including a peer's
  stale label the sweep has not yet removed --- degrading or downing the
  dashboard data path that resolves it (the label half); the warning repeated
  on each failed attempt is the operator signal.
]

*Deployment strategy interaction:* Readiness is decoupled from leadership
(#rref("sched.grpc.leader-guard")): both pods are Ready (TCP probe = process
up), RollingUpdate works with `maxUnavailable: 1`, zero-downtime rollouts.
Clients route via a health-aware balanced channel against the headless Service
`rio-scheduler-headless` --- they DNS-resolve to pod IPs, probe
`grpc.health.v1/Check` on each (NOT_SERVING on standby), and only insert the
leader into the tonic p2c balancer. The ClusterIP Service `rio-scheduler` is
kept for per-call connects (controller reconcilers, rio-cli) where a 50% chance
of hitting UNAVAILABLE + retry is acceptable. A third Service,
`rio-scheduler-leader`, selects on the `rio.build/scheduler-role=leader` label
the lease holder reconciles onto its own Pod (and sweeps off any other Pod
still carrying it), so its endpoints converge to the current leader on the
holder's first successful reconcile after acquiring --- for in-cluster proxies
that can neither health-probe nor retry a Trailers-Only UNAVAILABLE (the
dashboard's nginx upstream); until that reconcile lands (an asymmetric
partition delays it, and persistent reconcile failure --- priced by the
deletion-cost rule above --- extends it), requests reaching a stale-labeled,
self-fenced ex-leader fail with that same un-retryable UNAVAILABLE. Combined with `step_down()` and
pod-deletion-cost, a rollout flips leadership exactly once: K8s kills the
standby first (cost=0), new pod comes up as standby, K8s kills the old leader,
old leader step_down releases the lease, the new pod acquires on its next poll
tick (one `RENEW_INTERVAL`, 5s), balanced-channel clients reroute within one
probe tick (\~3s). Executors reconnect in place --- running builds continue, no
pod restarts.

#info(title: [VM test])[
  The 2-replica failover path is covered by
  #src("nix/tests/scenarios/leader-election.nix") (stable leadership,
  ungraceful-kill failover, build-survives-failover). Unit coverage for
  mechanics: `test_not_leader_rejects_all_rpcs` (leader-guard),
  `tick_follows_health_flip` (balance health probe),
  `relay_survives_target_swap` (executor relay buffering). End-to-end "build
  survives rollout" verified against EKS via the recipe in the
  zero-downtime-rollouts plan.
]

= Incremental Critical-Path Maintenance

#r("sched.critical-path.incremental")[
  Critical-path priorities are maintained *incrementally*, not via full O(V+E)
  recomputation on every event:
  - *On derivation completion:* Walk upward from the completed node to its
    ancestors, recalculating priorities only for nodes whose successor
    priorities changed. This is O(affected subgraph), which is typically much
    smaller than the full DAG.
  - *On DAG merge:* New nodes are inserted with initial priorities computed
    bottom-up from the merge point. Existing nodes' priorities are updated only
    if the new subgraph connects to them with a higher-priority path.
  - *Periodic full recomputation:* Every \~60 seconds (on the SLA-refit
    cadence), the DAG actor performs a full bottom-up priority sweep *inline*
    inside `handle_tick`, ensuring consistency even if incremental updates
    accumulate rounding errors or miss edge cases. No separate background task
    or message is involved --- the actor owns the DAG and mutates it directly.
]

This approach keeps per-event processing well under the 1ms budget needed for
1000+ ops/sec throughput.

= SLA hardware-class targeting (ADR-023 §13a)

#r("sched.sla.hw-class.config")[
  `sla.hwClasses: {h → [{key,value}...]}` maps each hardware class to a
  node-label conjunction. A change to `sla.referenceHwClass` MUST be rejected
  at config-load unless `--allow-reference-change` is set (ref-second
  normalization is anchored on it).
]

#r("sched.sla.hwclass.provides")[
  `sla.hwClasses[h].providesFeatures` lists `requiredSystemFeatures` that
  hw-class $h$ can host. `solve_intent_for` partitions `h_all` by this before
  `solve_full` so feature-bearing intents (e.g. `kvm`) get full SLA-solve
  participation on the matching classes only. §13c: replaces the pre-§13c
  static metal NodePool bypass.
]

#r("sched.sla.hwclass.provides.bidir")[
  Feature-match is the bidirectional ∅-guard predicate
  `features_compatible(required, provides)`: `required ⊆ provides` AND
  `required.is_empty() == provides.is_empty()`. The second clause prevents both
  leaks --- a `provides=[kvm]` class rejects featureless intents (metal doesn't
  absorb non-kvm), and a `provides=[]` class rejects `[kvm]` intents (non-metal
  isn't picked for kvm). One canonical `pub fn` serves all callers (T2/T10/D10
  chokepoint + worker `passes_intent_filter`).
]

#r("sched.sla.hwclass.capacity-types")[
  `sla.hwClasses[h].capacityTypes` lists capacity-types $h$ is permitted to
  provision (default `[spot, on-demand]`). `solve_full` and the controller's
  `all_cells`/`fallback_cell` iterate THIS, not `CapacityType::ALL`, so an
  od-only class (e.g. metal) structurally never generates a `(h, Spot)` cell
  --- preventing the conflicting-requirements ICE loop a requirement-based
  exclusion would cause.
]

#r("sched.sla.fod-feature-derivation+3")[
  The scheduler derives a derivation's _effective_ feature set ONCE at DAG-add
  time as a constructor invariant on `DerivationState`
  (`EffectiveFeatures::derive`). The biconditional `is_fixed_output ⟺
  effective_features ∋ fetcher` is enforced in BOTH directions: FODs project to
  `[fetcher]` regardless of the declared `requiredSystemFeatures`; non-FODs
  have `fetcher` STRIPPED from their declared set (a tenant cannot inject the
  rio-internal routing tag to spend fetcher \$/hr on a drv that doesn't fetch).
  EVERY reader --- `passes_intent_filter`, `h_all` partition, `override_hash`
  memo key, `retain_hosting_cells`, `bypass_cells` cold-start,
  `hard_filter`/`rejection_reason`, `statically_eligible`, and the wire
  `SpawnIntent.required_features` --- reads the stored field, NOT the raw
  declaration. The two intentional bypasses (`InspectBuildDag`'s
  `required_features` echo, the dispatch-time `failed_builders` warn) read the
  in-memory normalized set --- post I-204 soft-strip, but PRE the §13e
  FOD↔fetcher derivation, so the operator can spot a misrouted FOD (the
  verbatim declared set lives only in the `derivations.required_features` PG
  column). Post-construction mutation of `required_features` (the soft-feature
  strip) routes through a `set_required_features` write-gate that re-derives
  `effective_features` atomically --- the two fields cannot drift. §13e: this
  routes FODs to the dynamic `fetcher-*` hwClasses (which advertise
  `providesFeatures: [fetcher]`) via the same bidirectional ∅-guard that routes
  kvm to metal. The override is unconditional: a misconfigured FOD declaring
  `requiredSystemFeatures: [kvm]` would otherwise route to a kvm node with no
  fetcher airgap (#rref("builder.netpol.airgap")).
]

== Catalog-derived per-class ceilings (ADR-023 §13c-2)

Per-class `(max_cores, max_mem)` ceilings are derived *at scheduler boot* from
the AWS instance-type catalog rather than hand-maintained config --- the prior
§13c-1 design's hand-pinned values drifted from what each class's
`requirements` actually permit @karpenter to launch (the `cover::sizing` STRIKE
rounds). Boot-time derivation removes the operator-side staleness step
entirely: a `requirements` edit takes effect on the next rollout.

#r("scheduler.sla.ceiling.catalog-derived+3")[
  The scheduler derives a per-hwClass catalog ceiling at boot by calling
  `describe_instance_types`, projecting each type onto Karpenter discovery
  labels (`instance-category`, `instance-generation`, `instance-size`,
  `kubernetes.io/arch`, `instance-local-nvme`, `instance-cpu-manufacturer`),
  evaluating each class's `requirements` against them
  (`In`/`NotIn`/`Gt`/`Lt`/`Exists`/`DoesNotExist`), synthesizing the metal
  `instance-size {In|NotIn} metalSizes` partition from `nodeClass ==
  rio-metal` (mirroring the controller's `cover::build_nodeclaim`), and
  emitting the `(cores − 1, mem × 9/10)` of the *single largest-cores type* in
  the matched set --- both axes reduced for kubelet
  `kubeReserved`/`systemReserved`/eviction overhead and Karpenter's
  `vmMemoryOverheadPercent` (Karpenter binpacks against `Capacity − Overhead`,
  so a request of the raw capacity on either axis never fits any instance);
  never an independent per-axis max (which would phantom a shape no real type
  satisfies and ICE-loop Karpenter). Spot cost source only; Static (vmtest) has
  no AWS API and yields an empty catalog. A class matching 0 types is omitted
  from the catalog (warn).
]

#r("scheduler.sla.ceiling.uncatalogued-fallback")[
  A class with no catalog ceiling --- Static cost source, fetch failure, or 0
  matched types --- falls to the global `sla.maxCores`/`maxMem`. The
  #(refs.metric)("rio_scheduler_sla_class_ceiling_uncatalogued")`{hw_class}`
  gauge is set to 1 for such a class on every solve tick. Falling to global
  *over-permits, never over-strips* --- the bounded failure mode is a Karpenter
  ICE-backoff (no real type fits the over-permitted demand), caught by
  `cover::sizing`'s `exceeds_cell_cap` drop on the controller side.
]

#r("scheduler.sla.ceiling.config-tightens-only")[
  `sla.hwClasses[h].maxCores`/`maxMem` are an OPTIONAL operator override that
  *tightens* below the catalog (or global, if uncatalogued). The effective
  per-class ceiling is `min(catalog_or_global, cfg_or_global)` per axis.
  `validate_shape()` (config-load) rejects `Some(0)`; `validate_resolved()`
  (post-derive) rejects any override above the resolved global. `None` falls
  through. The global cap is the single hard operator-asserted bound --- the
  AWS `describe-instance-types` catalog is fallible input (region SKU list,
  IRSA-gated, no signature chain) bounded by `min(_, global)` so a buggy or
  hostile API response cannot raise the effective ceiling past what the
  operator budgeted.
]

#r("scheduler.sla.ceiling.controller-mirror")[
  The scheduler ships the catalog ceiling --- `min(catalog, cfg)`, each falling
  to global when absent --- to the controller over `GetHwClassConfig`. The wire
  value is always nonzero (`validate_shape()` rejects `Some(0)` overrides; the
  catalog cores axis is `max(1, cores − 1)` and the mem axis is `mem × 9/10` of
  a real instance type's memory, both nonzero); the controller's `ceilings_for`
  `>0` filter (pre-R26-scheduler back-compat) is preserved. Skew is bounded by
  the controller's 300s `HwClassConfig` refresh.
]

#r("scheduler.sla.global.optional")[
  `sla.maxCores`/`maxMem` MAY be unset (the default). When unset under `Spot`
  cost source, the effective global ceiling is derived at boot from the
  catalog. When unset under `Static`, boot fails. The serde default for
  `Option<>` fields is `None` when the key is absent --- no `#[serde(default)]`
  annotation is needed (or wanted: a redundant attribute would imply a
  non-trivial default).
]

#r("scheduler.sla.global.derive")[
  The boot-derived effective global is `max(catalog
  ceilings).clamp(MIN_CORES, MAX_CORES_GLOBAL)` for cores and `max(catalog
  ceilings).clamp(MIN_MEM, MAX_MEM_GLOBAL)` for mem, where
  `MAX_CORES_GLOBAL = 1023` (PriorityClass bucket count − 1),
  `MAX_MEM_GLOBAL = 32 TiB`, `MIN_CORES = 16` (per
  `probe.cpu ∈ [4, max_cores/4]`), `MIN_MEM = 1 GiB`. The resolved
  global lives on `CostTable.resolved_global`, set unconditionally in `main.rs`
  after the catalog fetch, before actor spawn. Every consumer
  (`Ceilings::from_resolved`, `class_ceilings()`, `GetSlaDefaults`,
  `GetHwClassConfig`) reads from there; `carry_catalog` preserves it across the
  lease-acquire reload.
]

#r("scheduler.sla.global.static-requires-some")[
  `hwCostSource=static` boot-fails when `sla.maxCores`/`maxMem` are unset ---
  Static mode has no instance-type catalog to derive from. The check is in
  `validate_shape()` (config-load pass-1), not `validate_resolved()`
  (post-derive pass-2) because pass-2 only runs under `Spot`.
]

#r("scheduler.sla.global.spot-empty-fails")[
  `hwCostSource=spot` + unset `maxCores`/`maxMem` + an empty catalog (IRSA
  failure, region with no matching SKUs, 30s fetch timeout) is a *boot failure*
  with an actionable error naming three fixes: (a) check IRSA
  `ec2:DescribeInstanceTypes` permissions; (b) set `sla.maxCores` explicitly;
  (c) use `hwCostSource=static`. The chicken-and-egg (transient AWS hiccup →
  CrashLoopBackOff) is the explicit contract: an operator who left
  `maxCores=None` opted into auto-derived globals; if derivation can't happen,
  that's a config error, not a fallback.
]

#r("scheduler.sla.global.controller-mirror")[
  The controller is air-gapped (no AWS API) and cannot self-derive a global
  ceiling. The scheduler ships the resolved global over
  `GetHwClassConfig.global_max_cores`/`global_max_mem`. The controller's
  `HwClassConfig::global_ceilings()` returns `None` until the first poll lands
  or against a pre-§13c-3 scheduler (proto3 zero-default, filtered by the `>0`
  gate); `cover_deficit` skips the tick on `None`, fail-closed. Self-heals
  within ≤300s (next `hw_refresh`). `controller.toml` no longer carries
  `max_node_cores`/`max_node_mem`.
]

#r("sched.sla.hw-class.k3-bench")[
  The builder supervisor runs a K=3 microbench (`alu`, `membw` STREAM-triad,
  `ioseq` O_DIRECT) at init when the `rio.build/hw-bench-needed=true`
  annotation is set; the result is appended to `hw_perf_samples(hw_class,
  pod_id, factor jsonb, submitting_tenant)`.
]

#r("sched.sla.hw-class.alpha-als+2")[
  Per-pname mixture $alpha in Delta^(K-1)$ is fitted via bounded heuristic
  alternation: NNLS on $"wall" dot (alpha dot "factor"[h])$ ↔ simplex-LS on
  $exp(-hat(epsilon)) approx alpha dot "factor"[h]$ ridged toward the previous
  iterate, ≤100 rounds, terminating at $norm(Delta alpha)_1 < 10^(-3)$. The
  simplex-LS ridge MUST be anchored at the current iterate (NOT
  $alpha_"prior"$): under $c arrow.l.r h$-correlated designs a fixed
  prior-ridge makes the data optimum a non-fixed-point of the ALS map and the
  iteration converges to a spurious attractor (§Phasing-13a gate-a).
]

#r("sched.sla.hw-class.admissible-set")[
  The admissible set is $A = {(h,"cap"): EE["cost"]^"upper" <= (1+tau) dot
    EE^min}$ with Schmitt deadband $tau_"enter"=tau$, $tau_"exit"=1.3 tau$. The
  emitted allocation is $c^* = max_A c^*_(h,"cap")$; $A$ is then re-filtered by
  type-fit at $c^*$, cost-at-$c^* <= (1+tau) dot EE^min$, and capacity-ratio
  $c^*_(h,"cap") >= c^* slash k$ (default $k=2$). The argmax cell survives all
  three checks so $A' != emptyset$ provably.
]

#r("sched.sla.hw-class.epsilon-explore+6")[
  With probability `sla.hwExploreEpsilon` per intent, the scheduler pins
  $h_"explore" tilde "Unif"(H without A)$ (or $H without {argmin_H "price"}$ on
  cache-miss or $A=H$), restricts the solve to $(h_"explore",*)$, and emits $A'
  subset.eq {h_"explore"} times {"spot","od"}$. The coin is OUTSIDE the
  memoization and deterministic in `drv_hash`; the pin VALUE is seeded from
  `model_key_hash ^ override_hash` --- independent of DAG iteration order ---
  so every drv sharing the storage key agrees on the same $h_"explore"$. The
  drawn $h_"explore"$ is stored in the SolveCache `MemoEntry.pinned_explore`
  and *carried across memo invalidation* --- `inputs_gen` governs memo
  staleness only, not selector identity. The pin is released when the pinned
  class graduates into $A$, is removed from $h_"all"$, becomes the cheapest
  (fallback pool only --- normal mode $H without A$ may include cheapest when
  cheapest $in.not A$), or `solve_full({h})` is `BestEffort` / its $A'$ is
  fully ICE-masked. Release rotates round-robin over `sorted(pool)`: $"next" =
  "pool"[("idx_of"(h)+1) mod abs("pool")]$ --- covers every pool element within
  $abs("pool")$ consecutive misses; deterministic in `(mkh, ovr, prev, pool)`.
  `prev` is re-validated $in "pool"$ each call (≤1 re-draw transition on pool
  change). `inputs_gen` is derived from the `(HwTable, CostTable)`
  solve-relevant projection at poll time; no caller bumps. The cached $A$ is
  never overwritten by an exploration result.
]

#r("sched.sla.hw-class.ice-mask")[
  ICE state is a read-time mask: the memo holds the full-$H$ solve and is never
  overwritten; each dispatch computes $A without "ice_masked"$. Per-cell
  exponential backoff is `60s → 120s → …` capped at `sla.maxLeadTime`, reset on
  first success. ICE state is in-memory (lease-holder only) and NOT in
  `inputs_gen`.
]

#r("sched.sla.hw-class.anchor-slots")[
  The 32-slot ring buffer reserves one anchor slot per distinct `cpu_limit`,
  holding the highest-weight sample at that value, never recency-displaced; the
  anchor weight floor is $w_"anchor" >= 0.5^"vdist" slash n_"anchors"$.
]

#r("sched.sla.hw-class.sample-weight-ordinal")[
  Sample weight is $w_i = 0.5^("ordinal_age" slash 20) dot 0.5^"vdist"$ ---
  ordinal half-life of 20 samples, with no wall-clock arm.
]

#r("sched.sla.hw-class.zq-inflation")[
  The quantile inflation factor is $z_q = t_(q, max(
    3, min(
      n_"eff",
      n_"distinct_c"
    ) - n_"par"
  )) dot sqrt(1+1 slash Sigma w)$; `Quantile()` uses
  $sigma' = sigma dot z_q slash Phi^(-1)(q)$ (with the $q=0.5$ limit branch).
]

#r("sched.sla.hw-class.lambda-gamma-poisson")[
  The per-class spot-interrupt rate estimate is $hat(lambda)[h] =
  ("EMA"("interrupts") + n_lambda dot lambda_"seed") slash ("EMA"("exposure") +
    n_lambda)$ with $n_lambda = 1"day" dot max(
    1,
    "EMA"_"24h"("node_count"_(h,"spot"))
  )$.
]

#r("sched.sla.cost-instance-type-feedback")[
  The per-cell instance-type menu (`CostTable.cells`) is populated by
  controller-observed feedback: `nodeclaim_pool` reports each resolved
  NodeClaim's `node.kubernetes.io/instance-type` (with kubelet
  `allocatable.{cores,mem}`) via `AckSpawnedIntents.observed_instance_types`;
  the scheduler folds these into the menu (union-only, persisted to
  `sla_observed_instance_types`). The menu drives `spot_price_poller`'s
  per-type AWS query and gates `smallest_fitting`'s capacity-reject; the
  returned price is the per-cell EMA, not a per-type field.
]

#r("sched.sla.forecast.one-layer")[
  `compute_spawn_intents` walks the Ready frontier AND a forecast frontier of
  `Queued` derivations whose every incomplete dependency is running with $"ETA"
  < max_((h,"cap") in A) "lead_time"[h,"cap"]$. Each `SpawnIntent` carries
  $(A, c^*, M, D, "eta")$ with $"eta"=0$ for Ready and max-dep-ETA for
  forecast. Forecast lookahead is exactly one DAG layer.
]

#r("sched.sla.forecast.tenant-ceiling")[
  Per-tenant Ready cores MUST be subtracted from the
  `sla.maxForecastCoresPerTenant` budget before forecast intents are admitted,
  so a tenant's wide layer-2 fanout cannot capture shared `maxFleetCores` ahead
  of other tenants' Ready intents (§Threat-model gap d).
]

#r("sched.sla.threat.read-path-auth")[
  All read-path AdminService SLA RPCs (`SlaStatus` / `SlaExplain` /
  `ExportSlaCorpus` / `ListSlaOverrides` / `ListTenants` / `ListPoisoned`) MUST
  gate on `ensure_service_caller`, not only `ensure_leader` (§Threat-model gap
  a).
]

#r("sched.sla.threat.hw-median-of-medians")[
  `HwTable` aggregation MUST be median-of-medians: per-tenant median (with
  $3 dot 1.4826 dot MAD$ reject) then median across tenants.
  `hw_perf_samples.submitting_tenant` is the grouping column;
  `FLEET_MEDIAN_MIN_TENANTS = 5` so two colluding tenants cannot capture the
  cross-tenant median (§Threat-model gap b).
]

#r("sched.sla.threat.corpus-clamp+3")[
  `ImportSlaCorpus` / `sla.seedCorpus` MUST reject entries with non-finite or
  out-of-range params: `ref_factor_vec` per-dim $in [0.1, 10]$, $Q >= 0$,
  $n_"eff" <= 32$, $S, P in [0, "buildTimeout"_"ref"]$, $a in [0,
    ln("MAX_MEM_HARD")]$, $b in [-2, 2]$ (§Threat-model gap c). §13c-3:
  $"buildTimeout"_"ref" = 7"d" times "MAX_CORES_HARD"$ and the $a$ upper bound
  are *constants* (`MAX_CORES_HARD = 1024`, `MAX_MEM_HARD = 32 TiB`), not
  catalog-derived --- corpus validation runs before the catalog fetch and MUST
  NOT depend on the resolved global. A constant also closes restart-shrink: a
  catalog that drops its largest type at boot N+1 must not reject the corpus
  that loaded cleanly at N.
]

= Key Files

- #src("rio-scheduler/src/actor/") --- DAG actor (single-owner event loop,
  dispatch, retry, cleanup; split into mod/merge/completion/dispatch/executor/build
  submodules)
- #src("rio-scheduler/src/dag/") --- DAG representation, merge, cycle
  detection, topological ops
- #src("rio-scheduler/src/state/") --- State machines (DerivationStatus,
  BuildState), transition validation, RetryPolicy, newtypes (DrvHash,
  ExecutorId)
- #src("rio-scheduler/src/queue.rs") --- Priority BinaryHeap ReadyQueue
  (OrderedFloat + lazy invalidation)
- #src("rio-scheduler/src/critical_path.rs") --- Bottom-up priority:
  `est_duration + max(children's priority)`; incremental ancestor-walk on
  completion
- #src("rio-scheduler/src/assignment.rs") --- Hard-filter executor selection
  (`best_executor`)
- #src("rio-scheduler/src/actor/floor.rs") --- D4 reactive `resource_floor`
  doubling (`bump_floor_or_count`)
- #src("rio-scheduler/src/grpc/") --- SchedulerService + ExecutorService gRPC
  implementations
- #src("rio-scheduler/src/db/") --- PostgreSQL persistence (derivations,
  assignments, build_samples telemetry; split into 9 domain modules per P0411)
- #src("rio-lease/src/") --- Kubernetes Lease leader-election loop
  (generation counter, `is_leader` flag, `recovery_complete` gate)
- #src("rio-scheduler/src/actor/recovery.rs") --- State recovery: reload
  non-terminal builds/derivations from PG on LeaderAcquired
- #src("rio-scheduler/src/event_log.rs") --- PostgreSQL-backed
  `build_event_log` writes for gateway `since_sequence` replay
- #src("rio-scheduler/src/admin/") --- AdminService gRPC (ClusterStatus,
  ListExecutors/ListOpenAttempts, TriggerGC)

CA early cutoff is end-to-end: compare (#rref("sched.ca.cutoff-compare") ---
completion-time content-index lookup), propagate
(#rref("sched.ca.cutoff-propagate") --- `Queued`→`Skipped` cascade with
`MAX_CASCADE_NODES=1000`), and resolve (#rref("sched.ca.resolve") ---
dispatch-time placeholder rewrite for CA-on-CA chains). The `Skipped` terminal
state is distinct from `Completed` for metrics
(#(refs.metric)("rio_scheduler_ca_cutoff_saves_total"),
#(refs.metric)("rio_scheduler_ca_cutoff_seconds_saved")) and audit trail.
Resolution uses the gateway-computed `ca_modular_hash` (plumbed via
`DerivationNode.ca_modular_hash` post-BFS) to query the `realisations` table;
each lookup is recorded in `realisation_deps` at completion time after the
parent's own realisation lands (FK ordering).

#figure(
  caption: [Actor message flow.],
  diagram(
    spacing: (16mm, 12mm),
    node-stroke: 0.5pt,
    node((0, 0), [`SubmitBuild`], name: <sb>),
    node((1, 0), [DAG Merge], name: <merge>),
    node((2, 0), [Critical Path\ Computation], name: <cp>),
    node((3, 0), [Publish Ready\ Spawn Intents], name: <disp>),
    node((4, 0), [Fenced Pull\ Mint], name: <assign>),
    node((4, 1), [Executor pod], name: <ex>, fill: accent.lighten(88%)),
    node((3, 1), [Process\ Completion], name: <comp>),
    node((2, 1), [Release\ Downstream], name: <rel>),
    node((1, 1), [Update Duration\ Estimates (async)], name: <upd>),
    edge(<sb>, <merge>, "-|>"),
    edge(<merge>, <cp>, "-|>"),
    edge(<cp>, <disp>, "-|>"),
    edge(<disp>, <assign>, "-|>", [pod's PullAssignment], label-size: 0.75em),
    edge(<assign>, <ex>, "-|>", [WorkAssignment], label-size: 0.75em),
    edge(<ex>, <comp>, "-|>", [ReportOutcome], label-size: 0.75em),
    edge(<comp>, <rel>, "-|>"),
    edge(<rel>, <disp>, "-|>", bend: 25deg),
    edge(<comp>, <upd>, "-|>", bend: -25deg),
  ),
)

= Failure modes

#table(
  columns: (auto, 1fr),
  align: (left, left),
  [*Immediate impact*],
  [No new builds accepted; `SubmitBuild` returns `UNAVAILABLE`.],

  [*Cascading effects*],
  [Executor pods cannot pull or report; in-flight builds keep running and the
    pods retry their unaries with backoff. Gateways return errors to clients.],

  [*Recovery*],
  [New leader acquires the Kubernetes Lease → fire-and-forgets `LeaderAcquired`
    → `recover_from_pg` rebuilds DAG from PG, including the open pull attempts
    and their `exec_id`s, so retried pulls and reports stay idempotent.
    Gateways reconnect via `WatchBuild(build_id, since_sequence=last_seen)`.],
)

Under a network partition between the scheduler and executors, in-flight pods
keep building and retry `ReportOutcome` until acked (the report retry loop is
bounded by the pod's budget, then by the Job deadline); not-yet-pulled pods
retry `PullAssignment` for as long as they live. The scheduler does not reset
anything on the partition itself --- an open attempt is resolved by the report
that eventually lands, by the controller's pod-terminal report if the pod dies
first, or by the establishment sweep at deadline + slack
(#rref("sched.attempt.establishment-window")).

For split-brain mitigation: the Kubernetes Lease prevents two active schedulers
under normal conditions; dual-leader windows are closed by the self-fence/steal
asymmetry (11s vs 19s; empty under bounded clock skew) and, for the clock-pause
residual, bounded by the generation-fenced authority transactions: the pull
mint, the establishment charge and the synthesized close all carry the serving
replica's generation and abort when it is below the durable claims floor
(#rref("sched.lease.generation-fence")), so a deposed leader can neither bind
new work nor consume outcomes. Other PG writes remain idempotent upserts and
tolerate the brief dual-writer window.

= Rationale

== PostgreSQL for scheduler state // supersedes ADR-007
<sched-rationale-pg>

Build DAGs, job assignments, build history, and dashboard data are stored in
PostgreSQL. The scheduler state shares the same PostgreSQL cluster as the store
metadata (separate tables in the default `public` schema) to reduce operational
overhead. PostgreSQL provides ACID transactions for consistent DAG state
updates and rich query capability for the web dashboard (build history,
analytics, worker utilization).

*Alternatives considered.* Redis or etcd are fast for simple key-value lookups
but lack the relational query capability needed for DAG operations and
dashboard analytics --- DAG traversal queries (find all ready-to-build
derivations, compute critical path) are natural in SQL but awkward in key-value
stores. SQLite embedded in the scheduler has no network overhead but does not
support concurrent access from multiple replicas. In-memory state with WAL is
fast but limits state size to available memory and requires custom recovery
logic; PostgreSQL's battle-tested WAL and replication are more reliable.

*Consequences.* PostgreSQL is well-understood with mature operational tooling
(backups, monitoring, replication), and complex dashboard queries (build time
percentiles, cache hit rates, worker utilization) are natural SQL. On the
negative side: it is a stateful dependency that must be provisioned and
managed, and schema migrations must be coordinated across scheduler and store
tables during upgrades.

Two mechanisms originally planned were never built: leader election uses a
Kubernetes Lease instead of PG advisory locks (operationally simpler; the
generation-fence mechanism makes brief dual-leader windows harmless), and
scheduling is tick-based (10s dispatch interval) rather than `LISTEN/NOTIFY`
event-driven --- the actor model's internal message channel handles
intra-process events; PG is polled, not subscribed.

*The hard part: PostgreSQL as bottleneck.* The scheduler and store share a
PostgreSQL cluster. High-throughput builds (e.g., full nixpkgs rebuild)
generate heavy write load from derivation state transitions and chunk manifest
writes. Mitigations: connection pooling via PgBouncer; read replicas for
dashboard queries (AdminService reads can use a read-only endpoint);
async/batched writes for non-critical state (duration estimates, dashboard
status) per §Synchronous vs. Async Writes; and separate PostgreSQL instances
for store vs. scheduler if write contention becomes an issue.

== Predictive cache warming // supersedes ADR-009
<sched-rationale-prefetch>

The scheduler used to drive @fuse cache pre-warming: prefetch hints for the
input closure paths were sent via the bidirectional build execution stream
(the retired warm-gate). With pull-mode delivery the builder warms its own
cache from the assignment payload's input closure before building,
converting serial "fetch then build" into overlapped execution.

Unlike a shared PV approach, each worker manages its own cache independently
with no coordination overhead. The scheduler's hints are best-effort: if
prefetching is incomplete when the build starts, the FUSE daemon falls back to
synchronous on-demand fetching.

*Alternatives considered.* Pure lazy fetch (no prefetching) works correctly but
adds latency proportional to the number of input paths and their sizes ---
particularly painful for cold workers or large closures. Full closure
materialization before build start guarantees no fetch latency during the build
but wastes bandwidth and adds a blocking pre-build phase. Worker-side
prediction based on history requires worker-side state and heuristics; the
scheduler already has the derivation's `inputDrvs` and `inputSrcs`, making
server-side prediction both simpler and more accurate.

*Consequences.* Significantly reduces build start latency, especially for large
closures on cold workers; best-effort design means prefetch failures are not
fatal; no additional infrastructure required (piggybacks on the existing build
execution stream). On the negative side: prefetch hints consume network
bandwidth even if the build is cancelled before starting, and the scheduler
must compute or look up input closures to generate hints, adding scheduling
overhead.

== Build-log data plane lives in rio-store // supersedes ADR-024 and reverses the round-7 "stays in scheduler" decision
<sched-rationale-logs>

`rio-scheduler` holds no build-log data and links no S3 SDK. Builders stream
log batches directly to rio-store's `LogService.AppendLog`; the store owns
chunking, durability, retention, and serving
(#xref(<store-log-service>, [the store component spec])). An earlier revision
of this section *rejected* moving the log path out of the scheduler; that
decision was reversed when the in-scheduler design's maintenance cost became
undeniable --- a majority of all commits to the scheduler's log module were
fixes for one bug class.

*The hard part.* The previous design buffered logs in per-derivation ring
buffers inside the leader-elected scheduler and periodically re-uploaded a
growing, mutable `.partial` blob to S3. Both choices generated the dominant
bug class: the leader's RAM was the system of record for up to 30 seconds,
and a mutable blob means a stale tenure's flush can *reduce* coverage a prior
tenure already stored. Every leadership change therefore required a
reconciliation protocol (re-stamping, sealing, stored-coverage folding,
tenure-pinned flush requests) whose failure modes were the majority of the
log subsystem's bug history.

*Alternatives considered.* Keeping the scheduler as the writer but switching
to immutable append-only chunks fixes the overwrite half but leaves the
leader's RAM as the only copy of un-flushed lines. Routing the flusher
through a `StoreService.PutLog` RPC (the original ADR-024 proposal) was
rejected for channel-mixing reasons that no longer apply: builders already
hold an authenticated bulk-upload channel to the store (NAR uploads), so log
ingest rides a builder→store stream that exists anyway rather than competing
with the scheduler→store cache-lookup channel.

*Consequences.* The scheduler's log subsystem (ring buffers, flusher,
leadership reconciliation, `GetDerivationLogs`) is deleted; the scheduler
drops `aws-sdk-s3` entirely. No log byte ever transits the scheduler --- the
pull/report unaries carry only control and the completion report, so a
chatty build cannot contend with completions for the scheduler's mailbox. Log loss on scheduler failover is zero by
construction (the scheduler holds no log data); the loss budget moves to the
builder's retransmit buffer and the store's ingest path. The cost is that
rio-store gains a position-addressed, TTL-retained object class that does not
participate in the CAS --- scoped to its own `LogService` and the `logs/`
prefix so the content-addressed store's single responsibility is diluted as
little as possible.

== In-memory DAG scalability
<sched-rationale-dag-scale>

*The hard part:* the scheduler maintains the entire global DAG in memory via a
single-owner actor model. A full nixpkgs rebuild has 50,000+ derivation nodes;
multiple concurrent nixpkgs rebuilds (different tenants or branches) multiply
this. Memory consumption (each derivation node carries metadata: hash, pname,
system, status, priority, edges --- a single DAG can consume hundreds of MB),
actor throughput (all mutations through a single `mpsc` channel; critical-path
recomputation across a large DAG could cause head-of-line blocking), and DAG
merge cost (deduplication by `drv_hash` is O(n) per merge) are the scaling
concerns.

Mitigations: profile memory and throughput against a 60K-node DAG target
(\<500MB, actor processes >1000 ops/sec); offload compute-heavy operations
(critical-path recomputation) via dirty-flag coalescing
(#rref("sched.critical-path.incremental")); bound individual submissions at
`MAX_DAG_NODES = 1,048,576` / `MAX_DAG_EDGES = 5,242,880` --- global
compile-time constants in #src("rio-common/src/limits.rs"), not per-tenant
(SubmitBuild rejects DAGs exceeding either limit before merge).
