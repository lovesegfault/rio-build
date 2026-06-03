#import "/lib/rio.typ": *
#show: rio.with(domains: ("sched", "scheduler", "admin"))

Receives derivation build requests, analyzes the @dag, and publishes work to
executors via a bidirectional streaming RPC.

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
- Work reassignment: when an executor fails (stream closed, heartbeat timeout),
  reassign its in-flight derivations to another executor. _Slow-executor
  speculative reassignment (actual_time > estimated_time × 3) is not currently
  implemented._
- @poison-derivation tracking: mark derivations that fail on 3+ different
  executors; auto-expire after 24h (see the error taxonomy)

= Concurrency Model

#r("sched.actor.single-owner")[
  The scheduler uses a *single-owner actor model* for the in-memory global DAG.
  A single Tokio task owns the DAG and processes all mutations from an `mpsc`
  channel:
  - `SubmitBuild` → DAG merge command
  - `ReportCompletion` → node completion + downstream release command
  - `CancelBuild` → orphan derivations command
  - Heartbeat → executor liveness + `running_build` reconcile
  - CA early cutoff → edge cutoff + potential cancellation command
]

gRPC handler tasks send commands to the @dag-actor and `await` responses. This
eliminates lock contention, makes operation ordering deterministic, and
simplifies reasoning about correctness. PostgreSQL writes are batched and
performed asynchronously by the actor.

#r("sched.actor.dispatch-decoupled")[
  `dispatch_ready` runs from state-change events (`MergeDag`,
  `ProcessCompletion`, `PrefetchComplete`) and from `Tick` when the
  `dispatch_dirty` flag is set. `Heartbeat` sets `dispatch_dirty` instead of
  dispatching inline --- at N executors / 10s heartbeat interval that is N/10
  dispatch passes per second, and each pass costs one full-DAG batch-FOD scan
  plus a `ready_queue` drain. At 290 executors and a 27k-node DAG (I-163) the
  inline path generated \~5× actor capacity and pushed `actor_mailbox_depth` to
  9.5k. Coalescing to once per Tick bounds the heartbeat-driven dispatch rate
  at 1/s regardless of fleet size.
]

#r("sched.dispatch.became-idle-immediate")[
  A `Heartbeat` that transitions an executor's capacity 0→1 (fresh
  registration, `store_degraded` clear, `draining` clear, phantom drain)
  dispatches inline instead of deferring to `Tick`. This is the carve-out from
  #rref("sched.actor.dispatch-decoupled"): the 0→1 transition is at most once
  per executor per degrade/spawn cycle (not N/10 per second), and deferring it
  adds up to one full tick interval of idle time to every freshly-spawned
  ephemeral builder --- the controller spawned the pod _because_ work is
  queued, so the slot is immediately useful. Steady-state heartbeats from
  already-idle or already-busy executors still only set `dispatch_dirty`.
  Inline dispatches from this carve-out are capped at `BECAME_IDLE_INLINE_CAP`
  (4) per Tick: leader-failover or fleet-wide degrade-clear makes every
  executor's heartbeat a 0→1 edge at once, which would otherwise reintroduce
  the I-163 storm via the back door; past the cap, further 0→1 heartbeats set
  `dispatch_dirty` and coalesce to the next Tick.
]

#r("sched.admin.snapshot-cached")[
  `AdminService.ClusterStatus` reads a `watch::channel` snapshot that the actor
  publishes once per `Tick`, instead of round-tripping
  `ActorCommand::ClusterSnapshot` through the mailbox. The handler itself is
  \~37µs; queuing it behind a saturated mailbox (I-163: 9.5k commands) made it
  time out at exactly the moment the controller's reconcile loop and operators
  need a reading. The cached value is at most one Tick (\~1s) stale.
]

= Scheduling Algorithm

*Implemented:* critical-path priority (BinaryHeap ReadyQueue), per-derivation
SLA sizing (`solve_intent_for` → `(cores, mem, disk, deadline)` per
#rref("sched.admin.spawn-intents")), PrefetchHint (full `approx_input_closure`
before WorkAssignment), @leader-election via Kubernetes Lease gated on
`RIO_LEASE_NAME`, `AdminService.ClusterStatus`/`DrainExecutor`, Pool @crd +
one-shot Job reconciler. Interactive builds get a +1e9 priority boost (dwarfs
any critical-path value).

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
   b. Hard-filter executors: required features present? executor idle (one build
      per pod)? system match? Candidates that fail are excluded entirely.
   c. Assign to the first eligible executor via the bidirectional BuildExecution stream.
      The WorkAssignment carries an HMAC-SHA256-signed assignment token (Claims:
      executor_id, drv_hash, expected_outputs, is_ca, is_fixed_output, tenant,
      expiry_unix — the optional fields use serde defaults). The
      store verifies the token on PutPath and rejects uploads for paths not in
      expected_outputs.
8. As builds complete (reported via BuildExecution stream):
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

#r("sched.log.batch-binding")[
  The `BuildLogBatch` ingestion path MUST drop batches whose `derivation_path`
  does not match an active assignment held by the calling executor's stream. A
  batch for an unsolicited derivation MUST NOT allocate a buffer entry.
]

This is the log-path analogue of #rref("sched.completion.output-membership").
The completion check runs inside the actor with `state.assigned_executor` in
scope; the log batch ingestion path deliberately bypasses the actor (so a
chatty build can't fill the actor's bounded mpsc), so the gate is colocated
with the data the recv task has --- the ring buffer entry, stamped with
`(exec_id, assigned_executor)` at dispatch. Without it, a compromised builder
spamming a fabricated `derivation_path` pollutes that drv's per-execution log
blob, and a late batch from a heartbeat-timed-out executor lands after a
re-dispatch and gets attributed to the *next* execution. Dropped batches
increment #(refs.metric)("rio_scheduler_log_batches_rejected_total").

#r("sched.log.phase-binding")[
  The `BuildPhase` ingestion path MUST drop phase updates whose
  `derivation_path` does not match an active assignment held by the calling
  executor.
]

The third worker-supplied `derivation_path` consumer in the `BuildExecution`
recv loop. Unlike #rref("sched.log.batch-binding"), whose check is colocated
with the ring buffer in the recv task (because the durability write must
bypass the actor's bounded mpsc), phase updates have no recv-task side effect
--- every sink they reach is fed from inside the actor. The gate
therefore runs in the actor against `(status, assigned_executor)`: the same
`Assigned|Running` precondition + executor comparison as the
#rref("sched.completion.idempotent") stale-report guard. The status
precondition is load-bearing: `transition()` never touches
`assigned_executor`, and the worker-completion terminal handlers
(`handle_success_completion`, `terminal_failure_epilogue`) leave it set, so
for the ~60s window before `CleanupTerminalBuild` reaps the DAG node a bare
executor comparison would accept a late phase from the just-finished
executor. Unlike the completion guard, this one also fails closed on
`assigned_executor = None`: a phase for a derivation with no active
assignment has no live build to render to. Without this gate, a compromised
executor sending `BuildPhase` with a fabricated `derivation_path` injects
attacker-controlled text into another tenant's `nix build -L` progress
display via the gateway's `SetPhase` relay (persisted to `build_event_log`
and pinned in the per-build state ring --- `Phase` is not a display-only
event). Dropped phase updates increment
#(refs.metric)("rio_scheduler_phases_rejected_total"), labeled by reason
(`not_active` | `no_assignment` | `executor_mismatch` | `path_too_long` |
`phase_too_long`).

#r("sched.log.path-length")[
  The `BuildExecution` recv loop MUST drop any `BuildLogBatch` or
  `BuildPhase` whose `derivation_path` exceeds 512 bytes, before the path is
  cloned, hashed, or forwarded to the actor.
]

A legitimate Nix store path is at most ~259 bytes (`/nix/store/` + 32-char
hash + `-` + the 211-char name limit + `.drv`); the proto `string` field is
otherwise bounded only by the 256 MiB `max_decoding_message_size`. The
binding gates verify the path's _normalized hash component_ — `drv_log_hash`
collapses `"{hash}-<anything>"` back to `{hash}` — so a
`"{hash}-" + 255 MiB` alias for a legitimately assigned derivation passes
#rref("sched.log.batch-binding") and would otherwise be cloned whole into the
recv task's per-stream `seen_drvs` set (pinning
`MAX_DRVS_PER_STREAM × 255 MiB ≈ 2 GiB` resident per stream) and shipped to
the actor's single-threaded mailbox on disconnect. Rejections increment the
arm's rejection counter with reason `path_too_long`.

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

The round-8 `derivation_path` bound (#rref("sched.log.path-length")) fixed one
of two worker-supplied strings in the `BuildPhase` message; the sibling
`phase` field had the larger blast radius (a `Phase` event is not
display-only: it is prost-encoded into `build_event_log`, pinned in the
per-build state ring, and rendered as `SetPhase` into every interested
tenant's terminal — multiplied by the derivation's interested-build count).
Bounding per field rather than lowering the global decode limit preserves the
per-field semantics: advisory messages are rejected whole, a
`CompletionReport` whose `drv_path` could name a live assignment is never
rejected (a lost completion strands the derivation in `Running`) so its
oversized payload fields are truncated or nulled instead, and a rejected
heartbeat reaps the worker by design. The one completion-side rejection is
the `drv_path` bound itself: a path longer than any valid store path can
never name a live assignment, so dropping the report whole at the recv arm
is the actor's inevitable unknown-derivation discard moved off the
single-threaded event loop — no legitimate completion is lost. Phase
rejections increment #(refs.metric)("rio_scheduler_phases_rejected_total")
with reason `phase_too_long`; the unresolvable-path completion drop
increments #(refs.metric)("rio_scheduler_completions_rejected_total") with
reason `path_too_long`.

`BuildLogBatch.first_line_number` is the motivating numeric field: it keys
the ring buffer's line numbering and the `drv_logs` row span, so out-of-order
or overflowing numbering would otherwise wrap the span subtraction into a
negative `line_count` (corrupting `s3_is_caught_up` and the
physical-vs-claimed re-serve for every interested build of the execution) or
panic the flusher task in debug builds. Ordering violations are rejected
per-batch at ingestion (reasons `non_monotonic` / `line_number_overflow` on
#(refs.metric)("rio_scheduler_log_batches_rejected_total")); magnitude is
deliberately NOT bounded at ingestion (forward gaps are legitimate and
unbounded — see #rref("obs.log.gap-span")) and is instead handled under the
rule's total-arithmetic branch: the flusher's span computation falls back to
the physical line count (tripwire
#(refs.metric)("rio_scheduler_log_flush_span_fallback_total")) and the
`drv_logs` numeric binds clamp at `i64::MAX`, which keeps every recorded
`(first_line, line_count)` pair non-negative and overflow-free for the read
path. The `CompletionReport` resource telemetry persisted to `build_samples`
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
`n/a` in the bounds table. Heartbeat resource numerics and `PrefetchComplete`
counters stay enumerated as `n/a`: they are not folded into row metadata or
ordering state.

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
log a build observed (`GetDerivationLogs(drv, exec_id)`) instead of falling
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

#r("sched.retry.revival-total-reset")[
  A cache-hit revival of a previously-failed derivation
  (`Poisoned`/`DependencyFailed`/`Failed` transitioning to `Completed`
  because the output now exists) MUST reset the COMPLETE
  failure-tracking state --- every retry counter, every capped deferral
  budget, every backoff deadline, and the failure attribution set ---
  not an enumerated subset. The reset MUST be implemented so that
  adding a failure-tracking field without deciding its revival
  disposition fails compilation.
]
The clause exists because the enumerated-subset form regressed twice on
the same shape (round-16 merged_bug_022): `claims_unavailable_count`
and `backoff_until` were silently omitted from `RetryState::clear()`,
so a revived node with a maxed claims budget re-poisoned on its FIRST
post-revival store blip (the charge gate saw the stale counter at cap,
emitting a message that falsely claimed the full ladder had run), and a
stale pre-revival backoff deadline silently deferred the re-probe
dispatch by up to a full backoff window. Exhaustive destructuring in
`clear()` makes the omission a compile error; a future field that must
survive revival gets an explicit no-op arm with rationale, never a
silent omission.

#r("sched.retry.per-executor-budget")[
  `BuildResultStatus::InfrastructureFailure` does NOT count toward the poison
  threshold. It routes through a separate `handle_infrastructure_failure`
  handler: `reset_to_ready` + retry WITHOUT inserting into `failed_builders`.
  Executor-local issues (FUSE EIO, cgroup setup fail, OOM-kill of the build
  process) are not the build's fault. `TransientFailure` (build ran, exited
  non-zero, might succeed elsewhere) DOES count. Executor disconnect DOES count
  --- a build that crashes the daemon 3× is poisoned; false-positives from
  unrelated executor deaths are cleared by `rio-cli poison-clear`. Both knobs
  are configurable via `scheduler.toml`: `threshold` (default 3, the former
  `POISON_THRESHOLD` const), `require_distinct_workers` (default true ---
  HashSet semantics; false = any N failures poison, for single-executor dev
  deployments). The retry backoff curve is likewise a `[retry]` table.
  `failed_builders` persisted to PG; infrastructure retry count is in-memory
  only.
]

#r("sched.dispatch.fleet-exhaust+2")[
  When `find_executor` returns `None` and every _statically-eligible_
  *non-draining* registered worker (matching kind, `system`, and
  `required_features`) is already in `failed_builders`, the derivation is
  poisoned immediately rather than deferring. Draining workers MUST be
  excluded: under one-shot semantics
  (#rref("sched.ephemeral.no-redispatch-after-completion")), a just-failed
  worker is draining but still in the executor map at completion-time; counting
  it poisons a `poolSize=1` (or `required_features`-narrowed) derivation on the
  FIRST transient failure, bypassing `max_retries` and the poison threshold.
  Under one-shot the controller spawns fresh `executor_id`s ∉
  `failed_builders`, so this check returns `false` in practice and
  poison-on-repeated-failure flows through `PoisonConfig::is_poisoned(threshold)`;
  the check remains as defense-in-depth for any future path where a worker
  fails without draining. The fleet filter MUST match the static-eligibility
  subset of `rejection_reason` (#rref("sched.admin.inspect-dag")); a narrower
  filter (e.g. kind-only) lets a drv defer forever in a multi-arch or
  feature-partitioned cluster with no INFO-level signal (the I-065 hang shape
  on the system/features axis).
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

#r("sched.admin.list-executors-leader-age")[
  `ListExecutorsResponse.leader_for_secs` is the seconds since this replica
  acquired leadership (`LeaderState::leader_for()`). Consumers MUST treat the
  executor list as potentially incomplete when `leader_for_secs` is small: on
  2-replica failover the new leader's `self.executors` map starts empty and
  fills incrementally as workers reconnect over a 1--10s spread, so a non-empty
  partial list cannot prove absence. The controller's `orphan_reap_gate`
  fail-closes when `leader_for_secs < ORPHAN_REAP_GRACE` --- see
  #rref("ctrl.ephemeral.reap-orphan-running").
]

#r("sched.admin.list-executors")[
  `AdminService.ListExecutors` returns a point-in-time snapshot of all
  connected executors via an `ActorCommand::ListExecutors` (O(executors) scan,
  `send_unchecked` like `ClusterSnapshot` --- dashboard needs a reading even
  under saturation). Each `ExecutorInfo` includes `executor_id`, `systems`,
  `supported_features`, `busy` (a build is in flight), `status`
  ("alive"/"draining"/"connecting"), `connected_since`, `last_heartbeat`, and
  `last_resources`. `Instant` fields are converted to wall-clock `SystemTime`
  by subtracting elapsed from `SystemTime::now()`. The optional `status_filter`
  matches "alive" (registered + not draining), "draining", or empty/unknown
  (show all).
]

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
  `AdminService.ClearPoison` resets both in-memory state
  (`reset_from_poison()`: Poisoned→Created, clear `failed_builders`, zero
  `retry_count`, null `poisoned_at`) and PostgreSQL (`db.clear_poison()`).
  Returns `cleared=true` only if both succeed. If PG fails after in-mem reset,
  returns `false` so the operator retries --- next recovery would restore
  Poisoned, so in-mem/PG drift is self-correcting. Idempotent: calling on a
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

#r("sched.admin.hung-node-detector+3")[
  `GetSpawnIntents.dead_nodes` is populated from the executor heartbeat table:
  a Node is reported dead when ≥`max(2, ⌈0.5·occupancy⌉)` of its busy executors
  have stale heartbeats AND those executors span ≥2 distinct tenants. The
  two-tenant floor distinguishes a hung Node (kernel softlockup, EBS stall,
  OOM-killed kubelet) from one tenant's misbehaving build. The `2`-floor (was
  `3`) is calibrated for busy-only `occupancy` --- idle executors are skipped
  (no drv to attribute), so a node with ≤2 busy pods would otherwise be
  undetectable; the trade-off (a 2-pod node with a correlated >30s heartbeat
  blip is also flagged) is bounded by `dead_reap_cap`. Executors are grouped by
  `authoritative_binding[auth_intent]`: the *controller-reported*
  kube-authoritative `spec.nodeName` binding (`AckSpawnedIntents.bound_intents`,
  sourced from the controller's pod informer) keyed on the executor's
  HMAC-attested `auth_intent` (the pod's spawn-time `INTENT_ID_ANNOTATION`, set
  once at connect, never mutated by heartbeat) --- NOT any worker-supplied or
  worker-influenceable value. Keying on `running_build` would be wrong: it
  diverges from `auth_intent` under dispatch fall-through and is set
  unconditionally from the heartbeat, so a compromised worker could forge it to
  inflate a victim node's `occupancy` and suppress detection. Executors lacking
  an authoritative binding (controller-lag, ack channel down) are skipped ---
  fail-safe --- and counted in
  #(refs.metric)("rio_scheduler_hung_detect_skipped_no_authoritative_total").
  The controller's `nodeclaim_pool::reap_unhealthy` consumes `dead_nodes` as
  `ReapReason::Dead` --- the only reap path for a `Registered=True` NodeClaim
  that is neither `Empty` nor in-flight.
]

#r("sched.dispatch.soft-features")[
  The scheduler MUST strip every feature listed in `soft_features`
  (scheduler.toml) from each derivation's `requiredSystemFeatures` at
  DAG-insertion time, before any spawn-snapshot or dispatch decision reads it.
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

#r("sched.admin.inspect-dag")[
  `AdminService.InspectBuildDag` returns the actor's in-memory snapshot of a
  build's derivations cross-referenced with the live executor stream pool. Each
  derivation row includes `rejections` --- a per-executor list of
  `{executor_id, reason}` veto strings from `dispatch_ready()` (e.g.
  `at-capacity`, `stream-closed`, `feature-missing`, `system-mismatch`) --- so
  a stuck-Ready node is directly diagnosable without log diving.
  `executor_has_stream=false` for an Assigned derivation means its assigned
  executor's gRPC bidi stream is gone from the actor's map --- dispatch can
  never complete. PG may still show the executor as alive; only the actor knows
  the stream is dead.
]

#r("sched.admin.debug-list-executors")[
  `AdminService.DebugListExecutors` snapshots the in-memory executor map
  (`has_stream`, `warm`, `kind` per entry) --- what `dispatch_ready()` filters
  on, not what PG `last_seen` claims. This RPC is *exempt from the
  leader-guard* by design: a stuck or partitioned standby replica can be
  queried directly to compare its view against the leader's.
]

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

#r("sched.heartbeat.adopt")[
  A heartbeat-reported running build the scheduler doesn't have on record for
  that executor is adopted into BOTH `executor.running_build` (so dispatch sees
  at-capacity) AND the DAG node (so `dispatch_ready` won't re-pop it). Expected
  after scheduler restart: recovery's reconcile may have reset the assignment
  to Ready while the executor still has it in-flight.
]

#r("sched.heartbeat.phantom-drain+2")[
  If the scheduler-kept running build assigned to that executor is missing from
  the executor's heartbeat report across two consecutive heartbeats (past the
  \~10s race window), the scheduler drains the phantom assignment: the
  derivation is reset to Ready and re-queued. A derivation assigned to a
  different executor is never drained from this executor's heartbeat.
]

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

#r("sched.freeze-detector")[
  `dispatch_ready` WARNs once per minute when `kind_deferred[k] > 0 &&
  registered_streams[k] == 0` holds for ≥60s, for each `ExecutorKind k`
  (Builder, Fetcher). The scheduler already surfaces the freeze via gauges, but
  a WARN lands in `kubectl logs` without a port-forward.
]

#r("sched.dispatch.unroutable-system+2")[
  When a Ready derivation's `system` is advertised by zero registered executors
  of the matching kind, dispatch defers it (same as no-capacity) but
  additionally counts it under
  #(refs.metric)("rio_scheduler_unroutable_ready")`{system=…}` and WARNs once
  when the system first becomes unroutable. The WARN re-arms after the system
  has been observed routable. This distinguishes "no capacity right now"
  (autoscaler resolves) from "no pool exists" (operator action: add the system
  to a `Pool`'s `systems` list, e.g. `i686-linux` on an x86_64 pool per
  #rref("builder.platform.i686")). The `system` label is normalized: values not
  matching `[a-z0-9_-]{1,32}` are bucketed as `unknown` (the string is
  tenant-supplied via `drv.platform()`; without bucketing, label cardinality
  would be unbounded).
]

= Multi-Build DAG Merging

#r("sched.merge.dedup+2")[
  The scheduler maintains a single global DAG across all concurrent build
  requests. When a new derivation DAG arrives, it is merged into the global
  graph: nodes are deduplicated by `drv_hash`, which ingress binds to the
  declared `.drv` store path
  (#rref("sched.merge.ingress-identity-binding")), so submissions of the same
  `.drv` share one node regardless of submitter.
]

The DAG key is the `.drv` store path for every node shape — including
#gls("ca", display: "content-addressed") derivations. Equivalent CA content
built from textually different `.drv`s is not collapsed at the DAG: that
dedup happens through realisations keyed by the 32-byte @modular-hash
(`ca_modular_hash`, as computed by `hashDerivationModulo` --- excludes output
paths, depends only on the derivation's fixed attributes), which drives the
CA early-cutoff rules. Earlier revisions of this rule described the CA case
as "deduplicated by modular hash"; that described the realisation layer, not
the DAG key.

#r("sched.merge.ingress-identity-binding")[
  `SubmitBuild` ingress MUST reject any node whose `drv_hash` is not exactly
  the declared `drv_path`, whose `ca_modular_hash` is neither empty nor 32
  bytes, or which sets `is_fixed_output` without `is_content_addressed`.
]

The DAG, the persisted `derivations` row, the HMAC assignment claims, and the
authoritative-content / identity conflict gates are all keyed by `drv_hash`,
while edges, dispatch, and recovery resolve the declared `drv_path`. The
gateway always submits the two equal; only a hostile or buggy direct
submitter can split them — registering a node under someone else's
predictable DAG key while pointing `drv_path` at a decoy the workers would
fetch, or aliasing one path onto two keys to corrupt the reverse index.
Binding them at ingress makes every downstream gate reason about one
identity. The two flag checks are declaration consistency: a malformed-length
`ca_modular_hash` would otherwise be silently dropped at the domain boundary,
and `is_fixed_output` without `is_content_addressed` would skip the CA gates.
Ingress deliberately does NOT require `ca_modular_hash` to be present for CA
nodes (the gateway's hash population is best-effort and FOD-only fallbacks may
omit it) and does NOT forbid it on non-CA nodes (deferred-IA nodes carry one).

#r("sched.merge.ingress-edge-endpoints")[
  `SubmitBuild` ingress MUST reject any edge whose `parent_drv_path` or
  `child_drv_path` is not the `drv_path` of a node in the same request.
]

This gate is request-shape consistency: every endpoint must be a declared
node, so a dangling or typo'd endpoint is a clean `INVALID_ARGUMENT` instead
of an opaque `Internal` at persist time. It deliberately does not decide
whether the submitter may *define* dependencies for those nodes — joining a
resident node and re-declaring it (full-closure resubmission) is legitimate
and routine. Protection of resident nodes' dependency sets is the merge-time
rule #rref("sched.merge.edge-creation-scoped") below. Every legitimate
producer satisfies both: the gateway emits each edge together with its parent
node and includes every child as a node of the same request, and the hook
fallback submits a single node with no edges.

#r("sched.merge.ingress-output-path-shape")[
  `SubmitBuild` ingress MUST reject any node carrying an
  `expected_output_paths` entry that is neither empty nor a valid non-`.drv`
  store path.
]

Declared output paths flow into `FindMissingPaths` cache-hit probing, the
HMAC assignment-claims output allowlist that authorizes worker uploads, and
the merge gate's fixed-output path-agreement evidence. Empty entries are the
legitimate floating-CA / deferred shape (the real path is computed at
resolution time); anything else that is not a store path is either hostile or
garbage, and rejecting it at ingress keeps tenant-controlled non-store-path
strings out of every downstream consumer — the same trusted-plane line the
gateway draws for its own clients (#rref("gw.reject.output-path-mismatch+2")).
The shape check is deliberately weaker than the gateway's binding check (the
scheduler has no derivation bytes for store-backed nodes to re-derive paths
from); content-binding for inline submissions is the authoritative-content
validation, and full evidence-ranked binding for store-backed claims is a
documented follow-up.

#r("sched.merge.ingress-output-names-unique")[
  SubmitBuild ingress MUST reject any node whose `output_names` carry a
  duplicate entry, for every node — bare store-backed nodes included —
  before any DAG state, claims derivation, or store probe consults the
  list.
]
The scheduler's name-keyed views (validator zips over
`output_names ⇄ expected_output_paths`, dispatch `position()` lookups,
the HMAC claims allowlist) assume pairwise-distinct names; a collapsed
duplicate leaves them silently partial over positional storage. The
round-15 fix-genealogy lesson is recorded here deliberately
(merged_bug_072, pattern R2): the rio-nix parse boundary rejects
duplicates inside derivation BYTES (`nix.drv.type-classify+1`), but a
bare proto echo carries no bytes — a cross-crate invariant needs a
chokepoint per ingestion surface, and "the other crate validates it"
is exactly how the Nth surface gets missed. The two layers are
independent and independently tested.

#r("sched.merge.ingress-output-arity")[
  SubmitBuild ingress MUST reject any node whose non-empty
  `expected_output_paths` length differs from its `output_names`
  length, for every node --- bare store-backed nodes included ---
  before any DAG state, claims derivation, or store probe consults
  either list. A fully empty `expected_output_paths` (the no-claims
  form) MUST remain accepted.
]
The arity sibling of #rref("sched.merge.ingress-output-names-unique"),
added by the round-16 sibling-invariants lesson (bug_098, pattern R5):
the same name-keyed consumers that assume distinct names also assume
the two lists are positionally PAIRED --- the
`output_names ⇄ expected_output_paths` zips in the settled-row and
resident identity matchers, the HMAC claims allowlist, and recovery's
deferred-resolve re-derivation all silently TRUNCATE on a misaligned
pair, so a hostile bare submission with a short or long path list
could persist mis-paired path evidence and later manufacture a false
`SettledIdentityConflict` against the honest, correctly-aligned
resubmission of the same derivation. The byte-carrying validators
(authoritative and inline) enforce arity against the parsed
derivation; this rule covers the bare proto echo those validators
never see. Sweeping per CONSUMER (what else do the zip consumers
assume?) rather than per invariant across surfaces is what surfaces
the sibling: distinctness landed in round 15, arity is the same
consumers' second assumption.

#r("sched.merge.ingress-inline-drv-binding+1")[
  `SubmitBuild` ingress MUST validate every node that carries non-empty
  `drv_content` without the authoritative flag (the gateway's
  inline-`.drv` optimization): the bytes MUST be the canonical ATerm
  serialization of a derivation whose text content-address
  (`makeTextPath` over the bytes and the derivation's `inputSrcs` and
  `inputDrvs` references) equals the declared `drv_path`; the node's
  declared `system`, output names, fixed-output flag, and
  content-addressed flag MUST equal the parsed derivation's; declared
  expected output paths MUST be bound per output kind --- fixed-output
  paths to their declared hash (single output named `out`), floating-CA
  and deferred entries empty, and input-addressed paths equal to the
  paths recomputed from the bytes with inputs resolved from sibling
  inline derivations and sibling `ca_modular_hash` declarations; and a
  non-empty `ca_modular_hash` MUST equal the modulo hash recomputed over
  the bytes, with only siblings whose published hash IS the
  input-position (unmasked) form in the seed --- store-backed
  input-addressed, fixed-output, and deferred entries; never inline or
  store-backed floating-CA ones --- and never the node's own
  declaration. Submissions that fail any of these MUST be rejected with
  `INVALID_ARGUMENT`. When the recompute is impossible because a
  transitive input is a store-backed floating-CA derivation (its
  unmasked form is underivable without its bytes), the declared
  `ca_modular_hash` MUST be discarded at ingress rather than forwarded
  unverified --- an unverifiable claim is no claim --- and the
  submission MUST otherwise be accepted.
]
This closes the variant-1 squat for inline content: before the binding, a
direct submitter could attach inline bytes describing one derivation while
declaring another derivation's identity fields (expected output paths,
flags) --- the worker builds what the bytes say, but upload authorization
and the merge gate's evidence comparisons trust the declared fields, so
attacker content could be registered at a victim derivation's
not-yet-built input-addressed path by an honest worker. With the binding,
every declared field is recomputed from (or checked against) the bytes
themselves, and the bytes are bound to the declared `.drv` path by the
text content-address --- forging any of it requires a SHA-256 second
preimage. The sibling-seeded input resolution is what makes the check
feasible without store access (#rref("gw.dag.modulo-hash-all-nodes")); a
forged sibling hash cannot steer a derived path onto a victim's path,
only away from every honest path. Store-backed nodes (no inline bytes)
remain declaration-trusted at ingress --- their binding is the
documented follow-up (store-evidence displacement), and the residual is
exploitable only by a compromised worker, which the trust model already
assumes hostile.

The seed restriction and the discard clause exist because a floating-CA
derivation's *published* modular hash is its masked-subject form
(`mask_outputs = has_ca_floating_outputs()`, oracle parity), while the
recompute's cache consumes input-position (unmasked) digests --- seeding
the masked form poisons every consumer's recompute and false-rejects
legitimate gateway-built CA chains. Discard-not-reject because the warm
gateway shape (inline will-dispatch consumer of an already-realized
floating drv whose bytes are not re-inlined) is honest traffic whose
hash is genuinely unverifiable at ingress; discard-not-accept because an
unverified declaration would otherwise flow into merge-gate identity
evidence, realisation keys, and the persisted row. NO automatic
re-establisher exists yet: a stripped node completes with its
completion-time CA bookkeeping skipped — surfaced, never silent
(#rref("sched.ca.absent-hash-surfaced")) — and the verifying
re-establisher is the staged follow-up F2 (`ModularHashState`); until
it lands, prose anywhere claiming the hash "is re-established" is the
round-15 bug_048 pattern (R6) and must not return.

#r("sched.merge.edge-creation-scoped")[
  The merge MUST attach a submitted dependency edge to its parent node only
  when this submission (re)creates that parent (a newly inserted node, a
  resubmit-reset re-creation, a displacement, or an authority takeover) or
  the resident parent is a topdown-pruned root awaiting its dependency
  top-up. Any other submitted edge that is not an exact re-declaration of an
  existing edge MUST be skipped without failing the merge, MUST NOT enter the
  in-memory DAG, and MUST NOT be persisted to `derivation_edges`.
]

A node's dependency set is intrinsic to its `.drv`, so only the submission
that (re)creates a node may define it. Without this rule any authenticated
submitter could *join* someone else's resident node (deduplication is by
design — #rref("sched.merge.dedup")) and attach a junk child to it: when the
junk fails, the dependency-failure cascade fails the victim's node, a
cross-tenant denial of service the ingress endpoint gate cannot see because
both endpoints are nodes of the attacker's own request. Exact re-declarations
of existing edges stay accepted as silent no-ops — the gateway re-emits each
parent's full edge set on every full-closure join — and skipped foreign
edges are observable (a per-merge warning plus the
#(refs.metric)("rio_scheduler_merge_foreign_edge_skipped_total") counter) rather than a
rejection, matching the edge loop's existing unresolved-endpoint posture.
The topdown-pruned carve-out exists because a pruned root's creating
submission deliberately dropped its dependency edges; the later full merge
that re-adds them joins the resident root rather than re-creating it.
Residual exposure, accepted: the *first* submitter of a predictable
store-backed path still fixes its dependency set (the scheduler never sees
store-backed `.drv` bytes to validate edges against `inputDrvs` — a possible
future gateway-consistency cross-check for nodes that carry inline content),
and a topdown-pruned root accepts dependency top-ups from any submitter
while the flag is set.

#r("sched.merge.heal-accepted-edges+1")[
  Every post-merge consumer of "what this submission's edges did" --- the
  closure-hole heal, the `topdown_pruned` clear-candidate seeding, the
  persisted edge rows, and the edge-skip metrics --- MUST be derived from the
  merge's *accepted*-edge bookkeeping (`MergeResult`), never from the
  submitter's raw declared edge list. The closure-hole breadcrumb of a
  resident node MUST be cleared by a merge only when that node's *entire*
  declared edge set was accepted (each declared edge either attached or an
  exact re-declaration of an existing edge) AND the merge positively covers
  the recorded truncation: every missing child on the node's closure-hole
  witness set MUST be among the node's post-merge children. A declared edge
  skipped by the creation-scoped gate or naming an unresolvable child MUST
  veto the heal for its parent; an accepted re-declaration that does not
  cover the witness set MUST leave the hole (and its fail-fast routing)
  intact and MUST be surfaced rather than silently retried.
]

The closure-hole breadcrumb is reap-truncation *evidence*: it records that a
node's child set is no longer representative of its `.drv` closure. Healing
it on the strength of edges the merge refused to attach would launder that
evidence — a joining submission whose top-up edges are gate-skipped (the
node was not re-created, so the creation-scoped rule rejects them) would
flip the node's closure evidence from Broken to Vouched while its child set
is still truncated, re-arming exactly the doomed from-source dispatch the
breadcrumb exists to prevent. Deriving every consumer from `MergeResult`
(computed by the same loop that enforces admission) makes this structural:
any future tightening of edge admission automatically propagates to the
heal, the clear pass, the persist, and the metrics. Skips whose parent
carries the breadcrumb are the expected rejoin signature and are counted
separately (#(refs.metric)("rio_scheduler_merge_rejoin_edge_skipped_total"))
from hostile-shaped skips
(#(refs.metric)("rio_scheduler_merge_foreign_edge_skipped_total")); the
rejoined node's hole stays set until a submission re-creates the node with
its full dependency set.

#r("sched.evidence.positive-witness")[
  A trusted-plane evidence UPGRADE --- clearing a closure hole, vouching a
  node's closure, displacing settled state, or any future transition that
  widens what the scheduler asserts about a derivation --- MUST be
  authorized by a positive witness whose operands are owned by the
  recording authority: the upgrade names what was previously recorded as
  missing or contradicted, and proves the new state covers it. An upgrade
  MUST NOT be derived from the absence of objection over a
  submitter-controlled set (edges a submission happened to declare,
  children that happened to complete, claims that happened not to
  conflict).
]

The genealogy that forced this rule: 6799b70b5 derived the topdown clear
from "children all produced" over whatever children remained (reap
truncation made the set unrepresentative); 606390ea7 re-keyed the heal to
"every declared edge accepted" (round-15 merged_bug_073: a subset
re-declaration satisfies it); the witnessed heal
(#rref("sched.merge.heal-accepted-edges+1")) closes the family by
demanding coverage of the recorded witness set --- both operands
scheduler-owned (the truncation recorded what it removed; the merge knows
what it attached). Prior art in the same shape: the displacement
primitive's evidence ranks (#rref("sched.merge.store-evidence-displacement+2"))
refuse re-definition unless the incoming claim PROVES rank over the
recorded row, and the born-holed prune stamp
(#rref("sched.merge.substitute-topdown+13")) records the dropped closure
at the only site that knows it. The `HealWitness` token is the mechanical
form: mintable only by the coverage branch, demanded by the only clear
path, so an absence-of-objection upgrade is unwritable rather than merely
unreviewed.

#r("sched.closure.witness-epoch")[
  A closure-hole witness testifies about ONE definition epoch. Any
  definition-changing transition --- an authority takeover through the
  resubmit-reset, a displacement, or a row-only store-evidence
  displacement --- MUST drop the carried witness in memory and clear the
  persisted flag and witness rows in the same transaction that commits
  the new definition. A witness MUST NOT survive onto a definition it
  does not testify about.
]

The witness records children a truncation removed FROM A SPECIFIC
DECLARED CLOSURE; the heal demands the new submission cover exactly that
set (#rref("sched.merge.heal-accepted-edges+1")). A definition change
replaces the closure the witness was recorded against: the new
definition's real `inputDrvs` can never contain a squat's junk children,
so a carried-over witness would refuse every honest re-declaration and
route the node to the bounded fail-fast permanently (round-16 bug_011 ---
the in-memory carry rode `authority_flip` unconditionally while the
upsert's OR semantics preserved the dead epoch's flag and rows for
recovery to resurrect). The mechanical form is
`ClosureHole::carry_across(DefinitionTransition)` (the only way to move a
witness across the resubmit carry) paired with the merge transaction's
definition-change clear, which runs after the creation upsert and before
the born-holed stamp so a re-creating submission that is itself a pruned
stamping parent ends the transaction with its own epoch's witness, never
a union of eras.

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

#r("sched.merge.substitute-topdown+13")[
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
  drop, is not marked) are marked `topdown_pruned` AND born holed: the
  prune MUST record the dropped children on the node's closure-hole
  witness set --- the flag and its witness rows written by ONE paired
  transactional writer covering newly created and merely joined kept
  nodes alike, in the same transaction as the mark --- so the node's
  closure evidence is Broken from birth and a single junk child
  completing cannot vouch it --- a mark that MUST
  be applied only after the merge has committed, MUST be persisted and
  restored at leader-failover recovery, and MUST be cleared (in PG and in
  memory) only once the node's children are all already produced in the
  DAG and no un-produced child has been reaped out from under it since
  (the closure-hole breadcrumb is recorded in memory and persisted
  alongside the mark, is carried across a resubmit retry of the node, and is
  dropped only when a later full merge re-declares its edges, *the merge
  accepts every one of them*, and the re-supply covers the recorded
  witness set --- a skipped edge, an unresolvable child, or an uncovered
  missing child vetoes the heal,
  #rref("sched.merge.heal-accepted-edges+1")), or
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
The +13 paired-writer clause is round-16 bug_045's lesson: the flag rode the
creation-scoped row bind while the witness rows were inserted for both
populations, so a merely-joined pruned parent committed `topdown_pruned =
true, closure_hole = false` plus orphan witness rows --- recovery hydrated it
un-holed, enrolled it as a mark-clear candidate, and re-armed exactly the
doomed from-source dispatch the born-holed witness suppresses. One paired
writer (`set_closure_holes_tx`) makes the two populations congruent by
construction (fix-discipline R2-PAIRED-WRITERS).
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

#r("sched.dispatch.substitute-complete-inline")[
  `handle_substitute_complete{ok=true}` MUST call `dispatch_ready` inline under
  the `BECAME_IDLE_INLINE_CAP` budget so cascade-promoted dependents are probed
  in the same handler instead of waiting one Tick per layer. Past the cap it
  falls through to `dispatch_dirty=true`;
  #rref("sched.admin.spawn-intents.probed-gate") keeps that path correct (no
  spurious spawn), just one Tick slower. The cap is shared with the Heartbeat
  `became_idle` and `PrefetchComplete` carve-outs --- fresh-cluster
  substitution can post thousands of `SubstituteComplete` in a burst, and
  uncapped inline dispatch is the I-163 storm shape.
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

#r("sched.ca.absent-hash-surfaced")[
  Every completion-time CA bookkeeping consumer of a derivation's
  modular hash (realisation registration, early-cutoff comparison and
  skipped-node realisation copy, dependency-trace recording) MUST
  handle the hash's absence EXHAUSTIVELY: when the node's gate
  conditions hold but the hash is `None` — the population the ingress
  strip legalizes (#rref("sched.merge.ingress-inline-drv-binding+1"))
  — the consumer MUST surface the skip with a warning naming the node
  and consumer and an increment of
  #(refs.metric)("rio_scheduler_ca_bookkeeping_skipped_total") (labeled
  by consumer), never skip silently.
]
The strip turned `ca.modular_hash = None` into a legal state for
CA/resolve nodes, and three `if let Some` consumers absorbed it
silently — a warm CA rebuild completed with no realisation row and no
signal (round-15 bug_048, pattern R1: lifecycle changed, readers
unaudited). Surfacing is part 1; the verifying re-establisher that
makes the realisation-insert skip impossible again is the staged
follow-up F2 (`ModularHashState` lifecycle enum) — no prose in this
spec may claim that writer exists until it does (R6).

#r("sched.dispatch.claims-derived+3")[
  When assignment tokens are signed, the scheduler MUST NOT sign
  upload-authorization claims (`expected_outputs`, `is_ca`,
  `is_fixed_output`) or forward worker build instructions for a
  store-backed derivation from submitter-echoed data: for every
  store-backed node whose definition evidence is below
  `path_bound_bytes`, dispatch MUST first fetch the `.drv` the declared
  path names, re-derive its text content-address in the actor, and prove
  the recorded claims against the parsed derivation with the same
  validator SubmitBuild ingress applies to inline content; the
  resolve-need MUST be derived from those verified bytes, never from the
  submitter's `needs_resolve` echo, and deferred output paths MUST come
  only from realisations resolved over those bytes. The byte-derived
  resolve-need MUST be RECORDED on the node in the same motion as every
  evidence raise to `path_bound_bytes` (the verified and
  stripped-verified dispatch arms, and merge-time store-evidence node
  creation), the dispatch resolve gate MUST read only that recorded
  state, and SubmitBuild ingress MUST normalize each inline node's
  `needs_resolve` echo to the value derived from its validated bytes
  (the shared oracle predicate `rio_nix::derivation::should_resolve`).
  The verdict's
  consequence MUST follow its typed permanence, and no arm may retry
  unbounded on deterministic inputs: a contradiction MUST poison the
  node without signing; STORE SILENCE — a transient verdict —
  MUST roll the assignment back with dispatch backoff, bounded by its
  own budget; INSTANT permanence MUST be restricted to CONTENT-BOUND
  reasons (an unparseable declared path, a contradiction, content-bound
  unparseable bytes) — a verification blocked on MISSING INPUT IDENTITY
  MUST NOT be concluded permanent from resident state alone, because
  residency is scheduler-mutated (terminal reap and leader failover
  both erase a completed input's node without touching content):
  the verifier MUST first re-seed from the persisted derivation rows
  (one batched lookup of the missing inputs' recorded input-form
  hashes, under the same not-floating predicate as every other seed
  source, performed at the shared verification chokepoint so the merge
  and dispatch consumers get it uniformly), and only a
  POST-read-through unseeded verdict may have consequences: at
  dispatch, bounded backoff on a dedicated budget whose exhaustion
  poisons with remediation generated from the post-read-through fact
  set; at merge, synchronous refusal with the same generated
  remediation (the submitter is present). A node whose declared
  modular hash cannot be recomputed
  against otherwise-fully-verified store bytes MUST have the
  declaration STRIPPED — cleared in memory and in the persisted row,
  exact ingress-strip parity (an unverifiable claim is no claim) — and
  proceed on the verified bytes at `path_bound_bytes`. Ingress-byte-bound
  nodes (inline or authoritative content) sign their ingress-bound
  recorded values; nodes already at `path_bound_bytes` or higher skip
  the re-fetch. Unsigned dev mode mints no claims and is exempt.
]
The unseeded-input clause is round-16 bug_029 (the +3 delta): the +2
text's "an input neither submitted nor resident" arm typed a fact about
MUTABLE STATE as structural permanence. A guaranteed deploy failover
erases every completed input's residency at once — recovery rehydrates
non-terminal rows only — so the first post-failover dispatch of any
bare deferred-IA node whose inputs had completed instant-poisoned
honest in-flight builds through the claims gate; terminal reap produced
the same shape one node at a time. The persisted row is CONTENT-DERIVED
state that survives both erasers, which is what qualifies it as the
read-through source. Read-through outcomes are observable on
#(refs.metric)("rio_scheduler_claims_row_readthrough_total") (seeded /
miss / error — a PG error defers as transient silence, never a
permanence verdict) and deferrals on
#(refs.metric)("rio_scheduler_dispatch_claims_unseeded_total");
dashboards keyed on
#(refs.metric)("rio_scheduler_dispatch_claims_unverifiable_total") see
the unseeded population move out (it now counts only content-bound
structural poisons).
This is the dispatch half of the worker-claims story (CppNix parity:
`Store::queryPartialDerivationOutputMap` consults the store's own copy of
the derivation, `store-api.cc:396-410` --- never a client's claim about
it). The child-seed trust posture is deliberate: static-IA derivation
seeds child modulo hashes from the DAG's length-checked echoes for direct
submissions, bounded by second-preimage hardness (a forged child hash
moves every derived path AWAY from honest paths --- see the soundness
note on `input_addressed_output_paths` in rio-nix `derivation/hash.rs`);
the byte-bound authority for membership is the STORE-side modulo cache,
which reads only the store's own backend --- which is why the store-side
deriver proof (`store.put.ia-deriver-proof+4`, landing with the store
workstream) is the authoritative parity surface, and this rule is the
trusted-plane signing gate in front of it. A submitter who never uploads
its `.drv` parks in dispatch backoff forever --- correct under the trust
model: an honest worker could not have fetched the derivation either.

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
  The architecture diagram shows arrows FROM the scheduler TO executors for the
  `BuildExecution` stream. This reflects data flow direction (scheduler sends
  assignments). The gRPC connection direction is the reverse: executors are the
  gRPC client calling the scheduler's `ExecutorService.BuildExecution` RPC.
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
    [An executor passes resource-fit check and is selected by the scoring
      algorithm],

    [`assigned → running`],
    [Executor sends acknowledgement on the `BuildExecution` stream],

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

#r("sched.merge.poisoned-resubmit-bounded+2")[
  When a build merges and finds a pre-existing `poisoned` node in the global
  DAG, the node resets for re-dispatch (same as
  `cancelled`/`failed`/`dependency_failed`) iff its `resubmit_cycles` is below
  `POISON_RESUBMIT_RETRY_LIMIT` (2 cycles). An explicit client re-submission is
  treated as retry intent --- the operator presumably fixed the underlying
  cause --- but bounded so a genuinely-broken derivation cannot loop forever.
  `resubmit_cycles` is incremented on each reset and persisted
  (`derivations.resubmit_cycles`), so the bound accumulates across
  re-submissions and survives scheduler restart. The reset gives the node a
  fresh per-cycle `retry_count = 0` (full `max_retries` budget). At or above
  the limit the node stays `poisoned` and the build fail-fasts (use the 24h TTL
  or `ClearPoison` admin RPC to override).
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

#r("sched.build.failure-evidence-at-source")[
  A build's first observed derivation failure MUST be made durable in
  `builds.error_summary` in the same actor turn it is observed (first write
  wins), independent of any later operation on the failed derivation's row.
  If that persist fails, the scheduler MUST retry it on every housekeeping
  tick --- before the poison-TTL eraser runs --- until it succeeds or the
  build terminates; recovery MUST treat PG-restored evidence as already
  durable and MUST persist any failure summary it reconstructs from
  still-linked failed derivations before the new leader serves traffic.
]

This rule closes a four-times-recurring bug class structurally. Rounds 12,
13, and 14 of adversarial review each found another path that erases a
failed derivation's row while an Active `keepGoing` build's only durable
failure evidence was reconstructible from that row (displacement, admin
poison clear, poison-TTL expiry, same-definition resubmit-reset) ---
each previously fixed by adding a pre-erase persist to that path. The root
cause was the dependency itself: evidence durability relied on row state
surviving until the build terminated. With the failure persisted at the
*source* --- the moment it is observed --- no eraser path needs to know
about build evidence at all, and a future eraser cannot reintroduce the
class: losing evidence now requires a PG outage at the failure-observation
moment AND a leader failover before any tick retry or eraser-side backstop
succeeded, instead of a single unfortunate failover. The per-path persists
(#rref("sched.merge.displaced-failure-evidence"),
#rref("sched.poison.clear-failure-evidence")) are retained as
defense-in-depth backstops for exactly that narrowed window. Behavioral
note: `builds.error_summary` is now populated for Active `keepGoing` builds
that have observed a failure, not only for terminal or pruned-evidence
builds --- admin build listings surface the pending failure earlier.

#r("sched.build.terminal-status-settled+2")[
  Once a build reaches a terminal state, the progress and result data
  served for it MUST be the values settled at its terminal transition:
  `QueryBuildStatus` MUST serve the counts frozen at that transition
  rather than recomputing them from live DAG state, a re-sent terminal
  `BuildCompleted` event MUST carry the output paths captured when the
  build completed, and a cancellation MUST refresh the build's counts
  from the DAG once before the terminal transition freezes them. After
  the terminal transition no further `BuildProgress` event may be
  emitted for the build, and its served progress accounting (including
  `cached_derivations` and the sticky failure summary) MUST NOT be
  mutated --- shared-node fan-outs MUST skip interested builds that are
  already terminal.
]
Terminal builds stay resident — and queryable, and re-subscribable via
`WatchBuild` — for the terminal-cleanup window while the global DAG keeps
evolving: shared-node re-probes, later submissions, and displacement of a
node the build was interested in
(#rref("sched.merge.authoritative-conflict")) all mutate the state a live
recompute would read, so a finished build's served progress could shrink
(or its re-sent completion event lose output paths) after the fact.
Serving the settled values keeps a terminal build's externally visible
history immutable, matching the persisted-count freeze at the terminal
transition. The frozen consumers are the `QueryBuildStatus` terminal arm,
the `WatchBuild` terminal re-send, the count-persist path, the
`BuildProgress` emitters (the debounced per-build emitter and the
precomputed-summary fan-outs in completion and dispatch), the dispatch
and CA-cutoff `cached_derivations` writers, and the per-derivation
failure handler — a `BuildProgress` sequenced after `BuildCompleted`
would otherwise be persisted to the event log and replayed to
re-subscribers with totals shrunk by whatever mutated the DAG since.
Per-derivation events (`DerivationCached`, `DerivationFailed`) still
flow to a resident terminal build's channel; they are facts about the
derivation, not aggregate progress of the finished build.

= Leader Transition Protocol

The scheduler uses a leader-elected model for the in-memory global DAG. On
leadership transitions:

+ *Assignment generation counter*: Incremented on each leader election (by the
  lease loop's acquire transition via `fetch_add` on the shared
  `Arc<AtomicU64>`). Each `WorkAssignment` carries this generation number.
  Executors compare it against the generation seen in `HeartbeatResponse` and
  reject stale-generation assignments.
+ *Recovery flag cleared*: The lease acquire transition clears
  `recovery_complete` and fires a `LeaderAcquired` command to the actor
  (fire-and-forget via `tokio::spawn` --- lease renewal MUST NOT block on
  recovery completing).

#r("sched.lease.non-blocking-acquire")[
  LeaderAcquired send is fire-and-forget via `tokio::spawn` --- blocking on
  recovery would let the lease expire (>15s) → another replica acquires →
  dual-leader.
]

#r("sched.lease.standby-tick-noop")[
  On lease loss (or local self-fence) the lease loop sends `LeaderLost` to the
  actor (symmetric with `LeaderAcquired`, same fire-and-forget spawn). The
  actor clears in-memory builds/dag/events and zeros the leader-only state
  gauges. `handle_tick` early-returns on `!is_leader` so an ex-leader's
  PG-writing housekeeping (orphan-watcher cancel, build-timeout fail, backstop
  reassign, poison-clear, derivations-gc) cannot race the new leader.
]

#set enum(start: 3)
+ *State reconstruction*: The actor's `LeaderAcquired` handler invokes state
  recovery (see §State Recovery below), then sets `recovery_complete = true`.
  Dispatch is a no-op while `recovery_complete` is false.
+ *Executor reconnection*: Executors reconnect their `BuildExecution` streams
  to the new leader. Stale completion reports (carrying an old generation
  number) are verified against rio-store for output existence before
  acceptance.
+ *In-flight assignments*: Assignments from the old leader are verified via
  heartbeat. If an executor reports it is still running the assigned
  derivation, the new leader reuses the assignment with the new generation
  number.
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

#r("sched.recovery.fetch-max-seed")[
  Generation seeding uses `fetch_max` not `store`. The same `Arc<AtomicU64>` is
  shared with the lease loop's `fetch_add(1)` on acquire --- `store` would
  clobber that increment.
]

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

#r("sched.recovery.inline-drv-durability+3")[
  A `DerivationNode` submitted with authoritative inline derivation content
  (`drv_content_authoritative`, the content-bound hook fallback) MUST have
  those bytes persisted with the derivation row at merge time and restored
  into the in-memory state on recovery, so a post-failover dispatch carries
  the same content. Before persisting, `SubmitBuild` ingress MUST validate
  the claim: a single-node submission whose bytes parse as a content-bound
  derivation consistent with the node's declared system, output names,
  fixed-output flag, content-addressed flag, expected output paths (exactly
  one entry per output: the hash-derived path for a fixed output, empty for
  a floating output), and `ca_modular_hash` --- submitters are untrusted,
  and unvalidated authoritative bytes would let one tenant poison the
  persisted content rebuilt under another derivation's identity after a
  failover. When any output declares a fixed hash, the derivation MUST
  have exactly one output and it MUST be named `out` (CppNix: "only one
  fixed output is allowed for now" / "single fixed output must be named
  `out`") --- a multi-output or differently-named fixed shape is rejected
  before any per-output binding is checked. The persisted bytes are
  dispatch payload only --- they MUST never be written to any store or
  served as a store object.
]
The column is `NULL` for every other derivation. Refresh and clearing follow
node lifecycle, not submission order: the row is written by the submission
that (re)creates the in-memory node (#rref("sched.persist.creation-scoped")),
so a retriable re-creation (resubmit after failure / cancel / poison reset)
refreshes or clears the bytes, while submissions that merely join a live node
leave its persisted content untouched. Rows written before the column existed
(or by a pre-upgrade scheduler) recover with empty content and keep the
pre-durability failure mode for that one window. Restoration applies to
poisoned-row recovery as well: a recovered `poisoned` node carries the same
bytes, authoritative flag, identity, and recomputed CA modular hash it held
while live, so the authoritative-conflict gate keeps holding across a leader
failover instead of being silently disabled for poisoned nodes.

#r("sched.recovery.inline-drv-ca-hash+3")[
  A recovered derivation carrying authoritative inline content that is
  content-addressed MUST have its CA modular hash rederived from the restored
  bytes during recovery (`hashDerivationModulo` over the parsed content with
  no input resolution --- the same computation `SubmitBuild` ingress
  validated), so a post-failover completion still registers its realisation
  under the key returned to the hook client and merge-time realisation cache
  hits still apply. Every other recovered derivation MUST restore the
  modular hash persisted with its row
  (#rref("sched.persist.ca-modular-hash")) when one is present ---
  content-addressed or not (deferred input-addressed rows carry one); a
  row that persisted none keeps it unset.
]
Recompute-from-bytes stays the source of truth wherever bytes exist: it
keeps the realisation key inseparable from the content actually persisted
(the two cannot drift), and a row whose bytes fail to parse degrades to an
unset hash (the build still completes; only realisation registration is
lost, with a warning). The recompute leg stays scoped to authoritative
content-addressed rows because the no-input-resolution recompute is only
valid for the lifted single-node fallback shape; everything else ---
store-backed CA nodes and deferred input-addressed nodes alike --- has no
bytes to recompute from, so the persisted ingress value is the only
faithful source of the evidence and of the realisation key.

#r("sched.recovery.deferred-resolve+1")[
  The dispatch-time resolve flag MUST be persisted at every site that
  computes it authoritatively --- the creation upsert (the store-evidence
  grant's byte-derived value for verified creations, the gateway echo
  otherwise) and the dispatch-raise writers, in the same statement as the
  evidence rank they accompany --- and a recovered derivation MUST be
  restored with the persisted flag VERBATIM. The expected-output-path
  re-derivation (an empty entry means "unknown until placeholder
  resolution") is permitted ONLY as the fallback for legacy rows whose
  persisted flag is absent; it MUST NOT shadow a persisted value, because
  it cannot see a fixed-output derivation's floating inputs.
]
The +1 verbatim-restore clause is round-16 bug_053's lesson: the
re-derivation under-approximates exactly the FOD-with-floating-input
population (its own expected paths are all concrete), and the prior
posture's "consequence-free --- covered by the content-addressed flag at
the realisation gate" justification conflated completion-time realisation
REGISTRATION with the dispatch-time placeholder REWRITE. A FOD recovered
with the flag false at the persisted `path_bound_bytes` rank skips the
byte re-derivation, dispatches with literal placeholder strings in
env/args, and poisons deterministically --- a build that succeeded before
the failover. M_071 makes the column nullable so NULL ("never persisted",
pre-071 row) is distinguishable from an authoritative false; the COALESCE
raise writers and the always-bound creation snapshot keep every post-071
row authoritative. This supersedes both the earlier wholly-lossy posture
and the re-derivation posture.

#r("sched.merge.authoritative-conflict+6")[
  A node whose in-memory state carries authoritative inline derivation
  content MUST NOT be redefined by a later submission for the same
  `drv_hash`: a submission that itself claims authoritative content with
  bytes that are not identical to the existing node's MUST be rejected
  while the existing node is live or `Failed`, and
  MUST displace it as a fresh node once it sits in a terminal failure
  state (`Poisoned`, `Cancelled`, or `DependencyFailed`); a store-backed
  (non-authoritative) submission MUST only join the node when its
  verifiable identity matches --- the public attributes (system, output
  names, fixed-output flag, content-addressed flag, declared expected
  output paths) AND at least one piece of content-bound evidence: agreement
  on a non-empty fixed-output expected path, or a byte-equal CA modular
  hash. A store-backed submission whose identity conflicts --- or that
  carries no content-bound evidence --- MUST be rejected while the node is
  non-terminal, and MUST
  displace it as a fresh node once it sits in a terminal failure
  state. A node that finished successfully (`Completed`/`Skipped`) MUST be
  rejected unless the displacer presents STRICTLY higher definition
  evidence than the settled node's persisted rank
  (#rref("sched.derivation.evidence-rank")) --- a settled record is never
  erased by an equal-or-lower-ranked claim, and every settled-displacement
  decision MUST be made by the single displacement primitive
  (#rref("sched.merge.evidence-ranked-displacement")), never by a per-arm
  carve-out. A store-backed submission whose identity matches MUST also
  displace the node --- rather than join it --- when the node sits in a
  terminal failure state that is no longer retriable on resubmit, so a
  poison-locked authoritative claim cannot capture later legitimate
  submissions for the remainder of its poison TTL. Displacement MUST NOT carry the displaced
  node's interest or failure accounting into the fresh node, MUST refresh
  the persisted recovery row to the displacing submission's verifiable
  identity with its status reset to the creation snapshot, and MUST remove
  the displaced node from the completion accounting of prior interested
  builds that are still non-terminal at displacement time --- builds
  already terminal at that moment keep their settled accounting --- and
  that removal MUST survive leader failover. All rejections surface as
  `FAILED_PRECONDITION`.
]
The byte-equality arm makes the legitimate producer's behaviour (identical
hook-fallback resubmissions) a no-op while closing the cross-tenant
pre-squat: a predictable `drv_path` cannot be claimed with attacker bytes
and then silently joined by --- or built on behalf of --- the victim's
submission. First-writer-wins for byte-different authoritative content is
deliberately scoped to claims that are live, still owned by the retry
machinery, or already built: the hook-fallback population (the gateway
always submits the content-bound fallback authoritatively) has no
store-backed form to displace a squat with, so without the
terminal-failure displacement arm a pre-squat that fails and parks would
hold those victims out of the hash for the rest of its poison TTL ---
exactly the lockout the store-backed arms already refuse. A successfully
finished claim is never redefined, so an attacker cannot use the arm to
rewrite a built derivation; while the claim is live the rejection is
unchanged, so nothing can yank a definition out from under a running
build. The Completed/Skipped carve-out is uniform across both conflict
arms: both delegate the settled decision to the displacement
primitive's strict rank comparison --- displacement erases the
record (interest accounting, registered outputs, the inline bytes) of a
build that finished, and an unverified submitter claim must never
out-rank verified-built state. Under the rank rule the lockout a
content-bound squat that *completes* before its victim submits can
impose is scoped to displacers that do not outrank it: a bare
store-backed echo (`unverified_claim`) is still rejected, while an
ingress-byte-bound submission (`path_bound_bytes` --- its bytes were
text-CA-bound to the declared `.drv` path at SubmitBuild admission)
strictly outranks the squat's `content_bound_claim` and reclaims the
hash with no store round-trip; merge-time store-evidence displacement
extends the same self-service to bare store-backed resubmissions by
fetching the proof from the store's text-CA-bound `.drv`.
Public attributes alone are copyable from the victim's public
derivation, and floating-CA expected paths are empty by construction, so a
match must additionally rest on content evidence. The two forms guarantee
different things: for a floating-CA derivation the modular hash is
recomputed from the bytes at ingress, so agreement means byte-level
knowledge of the same derivation (forging it requires a SHA-256 preimage);
for a fixed-output derivation the expected path commits only to the
declared output hash --- it is derivable from the public `(name, algo,
hash)` triple --- so agreement guarantees the same content-defined output
(which the store independently verifies on upload), not knowledge of the
same builder. That weaker FOD guarantee is deliberate: covering builder,
arguments, or environment would over-reject legitimate identical-output
joins, and the residual harm is availability-only --- a squat that joins
and fail-fasts can cost a joiner one spurious failure or a bounded wait,
and a genuinely dead FOD at most one extra poison cycle --- which the
displacement clauses bound by refusing to let a failed claim hold the
hash for its TTL, whichever submission shape (store-backed or
authoritative) the later legitimate producer uses. Tenant-scoped
poisoning and stronger FOD
evidence were considered and rejected as disproportionate. Identical
content still joins: the hook receives an already-resolved derivation ---
CppNix resolves CA derivations before hook dispatch (the building goal
asserts `inputDrvs` is empty) --- so the fallback's modular hash equals the
full-form hash a store-backed submission of the same derivation computes.
The gate is evaluated against the existing node in every lifecycle state
(before any resubmit-reset is applied), and terminal includes a
poison-budget-exhausted node, so a squat cannot dodge the rule by parking
in a retriable or poisoned state. Displaced interest is not carried;
instead the displaced hash stops counting toward still-running prior
interested builds (they keep any results already received), so those
builds neither hang nor get silently re-pointed at a definition they never
submitted, while builds that already finished are history and keep their
settled counts. Removal is total: when the pruned result had not been
received yet, the build's absolute totals (in memory and in
`builds.total_drvs`) shrink with the slot in the same transaction, so the
build can still reach `completed == total`; a result already received
keeps both the credit and the total. The removal is made durable by
deleting those prior builds' `build_derivations` links in the same
transaction as the recreate-refresh, so a recovery rebuilt purely from
the database cannot re-point them at the displacing definition.

#r("sched.merge.evidence-ranked-displacement")[
  Exactly one displacement primitive MUST exist in the merge gate, and
  every arm that removes a pre-existing node in favor of an incoming
  definition MUST delegate both the decision and the bookkeeping to it.
  The primitive MUST refuse, in order: a store-anchored victim (no
  authoritative bytes --- its definition lives in the store, which holds
  at most one text-CA `.drv` per path), categorically and regardless of
  rank; a non-terminal victim (live or `Failed` --- first-writer-wins
  while a claim is in flight or owned by the retry machinery); and a
  settled victim (`Completed`/`Skipped`) whose definition-evidence rank
  (#rref("sched.derivation.evidence-rank")) is greater than or equal to
  the displacer's. A victim parked in a terminal failure state
  (`Poisoned`, `Cancelled`, `DependencyFailed`) MUST be displaced
  regardless of rank. A displacement MUST apply the full shared
  bookkeeping: prior state recorded for rollback, dependency edges
  scrubbed, and the displaced hash surfaced for the durable link prune,
  accumulator reset, and accounting.
]
The primitive is the structural fix for the per-arm carve-out class: the
settled-protection predicate (`bug_076`) previously existed as two
open-coded `Completed`/`Skipped` matches that had to agree, and a future
arm could ship without either. Centralizing decision AND bookkeeping
means a new merge arm cannot displace at all except through the
primitive, and the rank comparison gives the protection a single
monotone vocabulary instead of a per-arm boolean. The deliberate
strict-inequality consequence: an ingress-validated inline
`path_bound_bytes` submission displaces a settled `content_bound_claim`
squat with NO store fetch --- its bytes were already text-CA-bound to
the declared `.drv` path at SubmitBuild admission, which is exactly the
store-anchoring the squat's self-bound bytes lack. The store-anchored
refusal is categorical rather than ranked because no claim about a
store-backed definition can outrank the store itself; the in-flight
refusal preserves the long-standing guarantee that nothing yanks a
definition out from under a running build; and terminal-failure
displacement stays rank-free because the anti-squat arms exist
precisely so a parked failure cannot lock later legitimate submissions
out of the hash.

#r("sched.merge.authoritative-claim-no-redefine")[
  A submission claiming authoritative inline content that lands on an
  existing store-backed (non-authoritative) node MUST NOT redefine it
  through the resubmit path: when the existing node is eligible for a
  resubmit-reset, the claim MUST be rejected as `FAILED_PRECONDITION`
  unless its verifiable identity matches the existing node's, in which case
  it is admitted through the normal resubmit-reset and its bytes are
  adopted. An authoritative claim MUST NOT displace a store-backed node.
]
A live or non-retriable store-backed node keeps its existing semantics: the
incoming claim joins it and the claimed bytes are ignored. Without this arm
a parked (failed, cancelled, or poison-reset) store-backed definition could
be silently redefined by an attacker's claim through the resubmit-reset,
carrying the prior builds' interest onto attacker-chosen content; with it,
an authoritative claim can only ever adopt a definition it can prove it
shares. The evidence the gate compares survives leader failover --- the
store-backed node's CA modular hash is persisted with its row
(#rref("sched.persist.ca-modular-hash")) and restored at recovery --- so
the legitimate producer's identical resubmission keeps being adopted after
a failover. The remaining fail-closed residual is a store-backed node
whose creating submission never carried any content-bound evidence at
all: an authoritative claim landing on it while it is parked is still
rejected (admitting it would reopen the redefinition attack), and the
rejection's error text points at the remediations (resubmit store-backed
by re-uploading the `.drv`, an administrative poison clear, or waiting
out the retention window).

#r("sched.merge.displaced-failure-reset+2")[
  Any (re)creation of a derivation row whose previously persisted content
  was authoritative and whose incoming content is not byte-identical to it
  --- displacement, an identity-matching store-backed resubmission taking
  over a parked authoritative claim, or a fresh re-creation after the
  prior node was reaped --- MUST reset every failure-derived column of the
  persisted row (poison state, failed-builder set, retry and resubmit
  counters, status, and the reactive resource floors) in the same
  transaction as the recreate-refresh, and the (re)creating definition's
  in-memory node MUST NOT inherit the prior node's resource floors or its
  consumed poison-resubmit budget. Administrative poison clears, TTL
  expiry, and same-definition resubmits keep their floor-preserving
  semantics.
]
Failure attribution and reactive sizing must not cross the definition
boundary: the floors were ratcheted by the prior definition's failures, and
letting the replacing definition inherit them would permanently dispatch
the victim at ceiling sizes (the floors never decay and survive failover);
the consumed resubmit budget and avoid-list would likewise charge the
victim for the squat's deliberate failures. The detector is row-level ---
prior content present and incoming content different --- so it covers every
takeover path uniformly: displacement of a conflicting or poison-locked
claim, the identity-matching resubmit takeover, and a re-creation of a
reaped authoritative row, whether or not the in-memory node still existed.
Doing the reset inside the recreate-refresh transaction (rather than as a
post-commit step) means a leader crash cannot leave the squat's
accumulators paired with the replacing definition's identity. Accepted
cost: a hook-fallback definition later resubmitted store-backed re-learns
its resource floor once (one extra failure-and-resize cycle); same-content
authoritative resubmits and store-origin rows are unaffected.

#r("sched.merge.displaced-edge-scrub+2")[
  Displacement and the authority takeover through the resubmit-reset MUST
  scrub the removed node's dependency (children) edges: the in-memory
  children edges are dropped when the node is removed for re-creation,
  and the persisted `derivation_edges` rows whose parent is that
  derivation MUST be deleted in the same transaction as its
  recreate-refresh, before the replacing submission's own edges are
  inserted. Edges in the dependent (parents) direction MUST be preserved;
  same-definition resubmit-resets and byte-identical authoritative
  retries MUST keep their edges; and a merge that fails after the scrub
  MUST restore the scrubbed edges together with the node.
]
The replacing submission is a different definition: evaluating its
initial state against dependency edges earlier submissions attached to the
hash would hand the squatter a denial-of-service handle that survives the
replacement --- a failing or never-completing attacker child seeds the
fresh node `DependencyFailed` (or parks it `Queued` forever), and the
persisted rows would resurrect the inherited dependency set after a
leader failover. The boundary is the definition change, so it applies
identically whether the new definition arrives by displacing a terminal
squat or by taking over a parked-but-retriable authoritative claim
through the resubmit-reset (the same path
#rref("sched.merge.displaced-failure-reset") already treats as a
definition change for the failure accumulators). Same-definition
resubmit-resets keep their edge-preserving semantics, and nodes that
depend ON the removed hash keep their edges --- they want its output
whichever definition produces it. Accepted trade: a taken-over node's
prior legitimate dependency edges are dropped unless the takeover
submission re-declares them (gateway full-closure submissions always
do). Known residual: a reaped authoritative row later re-created
store-backed has no in-memory node to scrub, so stale persisted
parent-side edges it left behind survive until a future row-level sweep.

#r("sched.merge.displaced-failure-evidence")[
  When the displacement interest prune removes a displaced derivation
  from a still-running prior build that has already observed a failure,
  the build's sticky first-failure summary MUST be persisted in the same
  transaction as the link prune, and recovery MUST seed a recovered
  non-terminal build's sticky failure from that persisted value. A prior
  build with no observed failure MUST NOT be marked failed by the prune.
]
This rule predates #rref("sched.build.failure-evidence-at-source") and is
retained as its defense-in-depth backstop. With evidence persisted at the
source, the deleted `build_derivations` link is normally no longer the only
failover-recoverable trace of the failure --- the in-transaction persist
matters only when the at-source write failed (PG unavailable at the
observation moment) and no tick retry has succeeded since. The same in-tx
backstop also runs for the non-displacement destructive removals --- the
same-definition resubmit-reset and the authority takeover, whose
recreate-refresh resets the row's failure state without pruning interest
--- so every eraser inside the merge transaction carries its own evidence
persist regardless of which removal arm fired. It rides an
existing transaction (atomicity is free), keeps the displacement path's
evidence contract self-contained rather than dependent on another
component's earlier success, and the COALESCE first-write-wins makes the
overlap idempotent. The no-observed-failure clause keeps joining a
later-displaced node from being treated as a failure (the displaced
result, if already received, stays credited per
#rref("sched.merge.authoritative-conflict")). The administrative prune
paths (poison clear, TTL expiry) reset the derivation row to a non-failed
state (`'created'`), so they carry the same residual hazard and persist
the same backstop evidence per
#rref("sched.poison.clear-failure-evidence").

#r("sched.poison.clear-failure-evidence")[
  When the scheduler removes a poisoned derivation while interested
  builds are resident --- via the administrative `ClearPoison` call or the
  poison-TTL sweep --- it MUST persist the sticky first-failure summary of
  every still-running interested build that has already observed a
  failure (first write wins) BEFORE clearing the persisted poison state,
  MUST NOT clear the poison if that persist fails, and MUST NOT mark a
  build with no observed failure as failed.
]
This rule predates #rref("sched.build.failure-evidence-at-source") and is
retained as its defense-in-depth backstop on the poison-clear paths. The
cleared row recovers as `'created'`: it contributes nothing to the
failed-count reconstruction, so `builds.error_summary` is what survives a
leader failover --- normally written at the failure's observation moment
by the at-source chokepoint, with this pre-clear persist covering the
window where that write failed and no retry has succeeded yet. Persisting
evidence first preserves the PG-first retry contract of both prune paths:
a failed persist leaves the node Poisoned in memory and in PG, so the
operator retry (or the next sweep tick) re-runs the whole sequence; the
COALESCE first-write-wins makes the retry idempotent. The formerly
out-of-scope paths --- the recovery-time expired-at-load clear and the
re-probe cache-hit `clear_poison` callers --- are now covered by the
at-source rule itself: the former because recovery re-persists
reconstructed evidence before serving traffic, the latter because they
never erased evidence in the first place.

#r("sched.persist.creation-scoped")[
  The scheduler MUST write a derivation's persisted recovery row only from
  the submission that (re)creates its in-memory node. Submissions that join
  an existing live node MUST NOT rewrite or clear that node's persisted
  recovery columns (declared identity, expected output paths, authoritative
  inline content).
]
This makes persistence follow the in-memory first-writer-wins truth: the SQL
upsert stays last-write-wins, but the only writers are creations, so an
in-flight node's recovery row can no longer be overwritten --- or its
authoritative inline content cleared --- by a submission that did not create
it. The substitution-planning marks (`topdown_pruned`, `closure_hole`) are
not part of that creation-time snapshot: they have their own dedicated
writers (the joined-node stamp inside the merge transaction, the
closure-hole setters, and the produced/vouched clear passes), so persisting
them for a node a submission merely joined does not violate this rule.

#r("sched.persist.recreate-refresh+2")[
  A submission that (re)creates a derivation's in-memory node MUST refresh
  the persisted row's full creation-time snapshot --- declared identity
  (pname, system, required features), the declared `.drv` store path,
  output names, expected output paths, content flags, inline content, and
  status --- and MUST NOT touch the row's live accumulator columns (poison
  timestamps, failed builders, retry and resubmit counters, resource
  floors), which have their own writers, except for the definition-change
  reset required by #rref("sched.merge.displaced-failure-reset").
]
Same `drv_hash` no longer implies same content: a displacing submission
(#rref("sched.merge.authoritative-conflict")) carries a different verifiable
identity, and without the snapshot refresh a leader failover would rebuild
the node from the displaced squatter's identity, silently undoing the
displacement. The `.drv` store path is part of that snapshot: recovery and
post-failover dispatch read the path from the row, so a squatter-declared
decoy path surviving the refresh would leave the displacing definition
undispatchable (workers would be told to fetch a `.drv` that exists in no
store). Reap-then-resubmit and crash-retry re-creations get the same
refresh for free.

#r("sched.persist.settled-identity-freeze+3")[
  A persisted derivation row whose status is `completed` or `skipped`
  MUST NOT be re-created under a conflicting identity: before any state
  is written for a submission, every submitted hash that has no resident
  DAG node MUST be checked against the settled rows, and a submission
  whose declared identity does not match a settled row's --- the public
  attributes (system, sorted output names, fixed-output flag,
  content-addressed flag, expected output paths declared by both sides)
  plus at least one piece of content-bound evidence --- MUST be
  rejected with `FAILED_PRECONDITION`, unless the conflicting
  re-creation was approved by the store-evidence check
  (#rref("sched.merge.store-evidence-displacement+2")). Admissible
  match bases are: agreement on a shared non-empty expected output
  path; a byte-equal LIVE CA modular hash; a byte-equal PRESERVED
  stripped claim (the segregated column a strip writer moved an
  unverifiable declaration into --- admitted as a positive match basis
  ONLY: it MUST NOT rank, MUST NOT veto, and a differing preserved
  value MUST fall through to the remaining bases rather than reject);
  and, for rows whose persisted evidence rank is byte-anchored
  (`path_bound_bytes` or `verified_built`), the dual byte-anchor of
  the declared path itself --- the row's recorded identity was derived
  from bytes text-CA-bound to the declared path, and a
  NON-authoritative incoming claim of the same path with matching
  public attributes and no contradicting evidence anchors to the same
  definition (an authoritative claim's bytes are bound to themselves,
  not to the declared path, so it has no second anchor and MUST prove
  identity through the classical bases). An undecodable persisted
  rank MUST NOT grant the dual-anchor basis, and on the settled VICTIM
  side an undecodable persisted rank MUST take the refusal arm of the
  displacement arbitration --- never a low-rank floor (flooring a
  victim demotes exactly the protection the persisted rank provides;
  the displacer-conservative lossy decode MUST be unreachable from
  victim-side call sites by construction). The
  persistence layer MUST additionally refuse to update a settled row
  whose public identity conflicts with the incoming re-creation,
  independent of the pre-merge check, admitting only the per-merge
  hash list that check approved --- and its conflict predicate MUST
  cover the same axes with the same semantics as the pre-merge
  matcher: output names compared as sorted sets (a set-equal
  reordered resubmission is NOT a conflict), expected output paths
  compared per output name where both sides declare one, and a
  present-on-both-sides-but-differing live CA modular hash vetoing
  the update. Axis parity between the two implementations MUST be
  pinned by a differential test driving both over the same
  single-axis mutations.
]
The two M_070 bases exist because the strip writers (ingress and
dispatch) leave exactly the rows they processed with NO classical
evidence: a stripped floating-CA / deferred-IA row has every expected
output path empty and a NULL live hash, so the pre-M_070 matcher could
never match ANY resubmission of it --- one successful stripped build
permanently bricked every rebuild-after-GC of that derivation behind a
deterministic `FAILED_PRECONDITION` (round-16 merged_bug_038). The
preserved-claim basis covers re-presentations of the same declaration;
the dual-anchor basis covers resubmissions that (correctly) no longer
declare the unverifiable hash at all. Rejoins through either basis are
counted (#(refs.metric)("rio_scheduler_merge_stripped_rejoin_total"))
--- each increment is a rebuild the previous matcher refused.
The freeze covers the window the merge gate cannot:
#rref("sched.merge.authoritative-conflict") protects a settled node only
while it is resident in the DAG, but terminal cleanup reaps nodes after
`TERMINAL_CLEANUP_DELAY` while their rows --- the durable record that the
derivation was built, what identity it was built under, and (for
content-bound claims) the bytes it was built from --- live on. Without the
freeze, a fresh submission for a reaped hash is indistinguishable from a
first-ever creation: it flows into the creation-scoped upsert and rewrites
settled history, which is exactly the erasure
#rref("sched.merge.authoritative-conflict") forbids while the node is
resident. A matching-identity re-creation (a legitimate rebuild after the
store garbage-collected the outputs) is admitted and refreshes the row
normally. The persistence-layer guard exists because the pre-merge check
and the upsert run at different times in the same merge: a row that
settles between them (racing completion fan-out) or a future caller that
bypasses the check (bug) must still find the row immovable; the upsert
skips such rows entirely, which surfaces as a loud merge failure rather
than silent history rewrite. The store-evidence carve-out does not weaken
the guard's posture: the approved-hash list is computed by the same
pre-merge check inside the same merge, scoped to that one transaction,
and empty for every other writer --- the freeze stays unconditional
except where the store's own bytes (or strictly higher ingress-bound
evidence) proved the settled record is the impostor.

#r("sched.merge.store-evidence-displacement+2")[
  When a store-backed submission's declared identity conflicts with a
  SETTLED record below byte-anchored rank --- a resident settled node
  whether its evidence is authoritative content-bound OR a bare
  store-backed echo, or a settled row whose persisted rank is below
  `path_bound_bytes` --- the scheduler MUST attempt to verify the claim
  against the store's own copy of the derivation before rejecting it
  (rank-uniform: no settled victim form below the byte-anchored ranks
  may be exempted from the check, and in particular a resident settled
  BARE node MUST NOT silently join a conflicting incoming claim):
  fetch the `.drv` the declared path names (subject to a per-merge
  fetch budget), re-derive its text content-address in the actor and
  require it to equal the declared path, and compare the parsed
  derivation against the submission's claimed identity with the same
  validator SubmitBuild ingress applies to inline content. A verified
  claim MUST displace the settled squat --- through the displacement
  primitive for resident victims, and through a per-merge
  approved-hash carve-out of the settled-row freeze (with the old
  row's persisted parent-side dependency edges scrubbed in the same
  transaction) for row-only victims. A verification that succeeds
  EXCEPT for an unverifiable declared modular hash MUST have the SAME
  consequence as at the dispatch consumer (one verdict, one
  consequence): the declaration is STRIPPED --- cleared from the live
  evidence and PRESERVED in the segregated column --- and the
  displacement approved on the verified bytes; the merge consumer MUST
  NOT refuse with remediation the production submission path cannot
  follow. A contradiction MUST reject the submission with
  `FAILED_PRECONDITION`. The row-level decision MUST be an exhaustive
  arbitration over every (row rank, incoming rank) pair: byte-anchored
  rows (`path_bound_bytes`, `verified_built`) refuse every claim
  class; a `content_bound_claim` row is displaced by strictly higher
  ingress-byte-bound evidence with no store fetch, or by a bare
  store-verified resubmission; an `unverified_claim` row is displaced
  by ingress-byte-bound evidence or a bare store-verified
  resubmission, and MUST NOT be displaced by `content_bound_claim`
  rank alone (an authoritative claim's bytes are bound to themselves,
  not to the declared path). Every refusal's remediation text MUST be
  generated from its arbitration arm. Store silence --- fetch failure,
  absent path, or non-canonical / non-text-CA-consistent bytes ---
  MUST NOT count as evidence in either direction AND MUST surface as
  `UNAVAILABLE`, never hardening into the conflict's permanent
  `FAILED_PRECONDITION`; permanent unprovability (an unseedable
  declared-IA input after the persisted-row read-through, or
  content-bound non-derivation bytes) keeps `FAILED_PRECONDITION` with
  the generated remediation. Exhaustion of the per-merge fetch budget
  MUST fail the merge with `RESOURCE_EXHAUSTED` and no partial
  displacement persisted.
]
This is the self-service path for `bug_076`-class squats: the victim of a
content-bound squat on its predictable `drv_path` uploads the genuine
`.drv` (text-CA-enforced at store ingestion, #rref("store.put.drv-text-ca+2"))
and resubmits store-backed; the scheduler verifies the store derivation
and erases the squat --- no operator involved. The verification is
self-contained in the actor because store transport is not part of the
trust boundary for identity decisions: the fetched bytes must re-derive
the declared path as their text content-address before anything is
believed, so a confused or hostile store answer cannot smuggle unrelated
bytes into the comparison. The fetch budget (8 per merge) bounds the
single-threaded actor's exposure to a submission manufactured to carry
many settled conflicts; exhaustion fails the whole merge with
`RESOURCE_EXHAUSTED` and split-the-submission guidance, observable
(`over_budget` on the
#(refs.metric)("rio_scheduler_merge_store_evidence_total") counter) ---
a load condition must never be reported through the conflict's permanent
code, and partial displacement is rejected by design
(#rref("sched.persist.atomic-activation+2")). The `unverified_claim`
arbitration arm restores the advertised self-service for store-backed
victims whose row was squatted by a bare echo --- the previous
categorical refusal of that rank contradicted the remediation text the
rejection itself emitted --- while the reverse-squat guard keeps
hook-fallback-shaped forgeries from erasing genuine store-backed history
by rank alone. The strip-parity clause exists because the two consumers
of the same verification verdict had drifted (round-16 merged_bug_020):
dispatch stripped-and-proceeded while merge refused with "resubmit
WITHOUT the declared ca_modular_hash" --- remediation the gateway makes
unfollowable (`populate_ca_modular_hashes` stamps the hash on every
submission it can compute one for) and the verdict deterministic for
CA-chain/deferred-IA nodes, so a settled squat on such a node was
permanently undisplaceable through the advertised self-service path.
Stripped-displacement approvals are observable as the `stripped` result
label on
#(refs.metric)("rio_scheduler_merge_store_evidence_total"); dashboards
keyed on `result=unavailable` see that population move. The
rank-uniform victim clause closes the resident bare x bare cell
(round-16 bug_072): a DAG-resident settled BARE squat previously
escaped both the row check (resident hashes are skipped) and the
resident scan (which gated on authoritative content), so a
genuine owner's conflicting store-backed submission silently JOINED
the squat and was served its forged outputs as a cache hit. One
residual is deliberate: a CLAIMS-FREE submission (no expected paths,
no declared hash) of a resident store-anchored node still joins ---
it asserts nothing the resident could contradict, a resident join
rewrites no state, and refusing would demand evidence from the shape
every hash-less re-reference of a floating node takes; gateway
submissions always carry a hash or paths and are therefore never in
this population. The rank gate on byte-anchored row-only victims is
uniform with the displacement primitive's settled rule
(#rref("sched.merge.evidence-ranked-displacement")); it is also
unreachable in honest operation --- store bytes cannot contradict an
identity that was itself derived from byte-bound evidence --- which is
exactly why it is enforced in code rather than argued. The displaced
squat's registered store content (if any) remains until garbage
collection; this is harmless because realisations are keyed by
`(modular hash, output name)` and the squat's modular hash differs from
the genuine derivation's, so stale realisations cannot poison the
victim's resolution.

#r("sched.merge.input-form-seed")[
  Modular-hash seeds consumed by store-evidence verification MUST be
  built only through a constructor that owns the input-form predicate:
  a node or resident child contributes its recorded modular hash only
  when it is not floating-content-addressed
  (`is_fixed_output || !is_content_addressed`). A floating derivation's
  published hash is the masked-subject form and MUST NOT be used as an
  input-position digest.
]
A floating-CA derivation's own `hashDerivationModulo` masks its output
slots (oracle parity), so the value the gateway publishes for the node is
NOT the digest its consumers fold over in input position --- those come
from the unmasked recursion. Seeding a masked form silently steers every
derived input-addressed path away from the honest paths: against an
honest submission that is a wrongful contradiction (poison), and against
a crafted one it is a wrongful verification. The predicate was originally
enforced by call-site discipline at three sites; the fourth (dispatch's
DAG-children seed, added hours before the sweep) shipped without it ---
the constructor is the chokepoint that makes a fifth unfiltered site
unwritable rather than merely unlikely. Floating inputs that genuinely
matter to a verification resolve to store-silence (unseedable input)
instead, which is the fail-closed direction under the trust model.

#r("sched.merge.identity-hash-veto")[
  In every identity matcher that compares a submission against a prior
  definition (the resident-node matcher and the settled-row matcher),
  two recorded 32-byte modular hashes that are both present and DIFFER
  MUST veto the match regardless of any other agreement; the comparison
  MUST go through the single shared classifier so the two matchers
  cannot drift on the hash clause.
]
Expected output paths are public, copyable data --- a squatter can echo
the victim's paths verbatim --- while a modular hash is content-derived.
Before this rule the matchers treated a differing hash as merely "no
hash evidence" and could still declare a match on path agreement,
letting a definition that provably differs (its hash exists and
disagrees) join or displace as if identical. The two matchers enforce
the same identity rule at the two places a prior definition lives
(resident DAG node, settled PG row); the previous "MUST stay in sync"
comment pair is exactly the call-site-discipline shape that R2 retires
--- the shared `modular_hash_evidence` classifier is the chokepoint.
Scope note: the bare-vs-bare join consults neither matcher; closing
that channel is the rank-gated-seed work staged behind this rule.

#r("sched.persist.atomic-activation+2")[
  The merge-time persistence of (re)created derivation rows --- including
  the definition-change accumulator reset of
  #rref("sched.merge.displaced-failure-reset") --- build-derivation links,
  edges, and the durable displacement prune MUST commit in the same
  transaction as the owning build's `pending` to `active` status update. A
  merge that fails before that single commit point MUST leave every
  pre-existing derivation row --- including displaced and resubmit-reset
  nodes' status, identity, authoritative inline content, and failure
  accumulators --- exactly as its prior creation persisted it. Best-effort
  accounting resets (same-definition poison clears, the redundant
  displaced-row reset) MUST run only after that commit.
]
With one commit point, "the build was accepted" and "its rows are durable"
are the same event: a submission rejected late (or a leader that dies
mid-merge) leaves either nothing or a `pending` build row that orphan
handling already covers, never a half-committed displacement, a cleared
authoritative blob, or a wiped failure history for a build that was never
activated, and recovery needs no compensating logic.

#r("sched.persist.ca-modular-hash+2")[
  A derivation's ingress-provided CA modular hash --- carried by
  content-addressed nodes and by deferred input-addressed nodes --- MUST be
  persisted with its derivation row by the creation-scoped upsert and
  refreshed on every (re)creation like the rest of the creation-time
  snapshot. The persisted value is declared identity evidence only: it
  MUST NOT relax authoritative-content ingress validation, the
  byte-equality arm, or any displacement predicate of
  #rref("sched.merge.authoritative-conflict").
]
The merge gate accepts a CA modular hash as content-bound identity
evidence, and for a floating-CA derivation it is the only possible
evidence (expected output paths are empty by construction). An
authoritative row regains its hash after failover by recomputing it from
its persisted bytes; a store-backed CA row has no persisted bytes, so
without this column its evidence was simply lost on failover and a
byte-identical authoritative resubmission of the same derivation could
never again be adopted (#rref("sched.merge.authoritative-claim-no-redefine")).
The hash also keys realisation registration --- for deferred
input-addressed nodes it is what lets `wopQueryDerivationOutputMap` answer
with the post-resolve output path, which is why the gateway populates it
for them even though they are not content-addressed --- so persisting and
restoring it keeps post-failover completions registering under the key
clients were given. Scoping the column to `is_ca` rows would silently
regress exactly that failover guarantee. Stored as part of the identity
snapshot --- never inside the definition-change accumulator reset --- and
absent (`NULL`) when the creating submission carried none, which keeps the
fail-closed posture for evidence-less rows.

#r("sched.derivation.evidence-rank")[
  Every derivation MUST carry a definition-evidence rank from the ordered
  lattice `unverified_claim < content_bound_claim < path_bound_bytes <
  verified_built`, computed shape-based at ingress (store-backed
  submission → `unverified_claim`; authoritative inline content →
  `content_bound_claim`; ingress-validated non-authoritative inline
  content → `path_bound_bytes`), persisted with the derivation row under
  creation-snapshot semantics, and restored at recovery (floored at
  `content_bound_claim` when authoritative bytes are present;
  unparseable values floor to `unverified_claim`). Within one node
  lifecycle the rank MUST only upgrade, at exactly two chokepoints: the
  settle transition upgrades `path_bound_bytes` --- and ONLY
  `path_bound_bytes` --- to `verified_built` on `Completed`/`Skipped`,
  and the dispatch-time claims derivation upgrades a store-backed node
  to `path_bound_bytes` after its `.drv` is fetched, text-CA-verified
  against the DAG key, and found to match the recorded claims.
]
The lattice is the single vocabulary for trusted-plane authority
decisions (displacement, settled-row protection, claims signing) ---
rank comparison replaces per-arm carve-outs, so a future merge arm
cannot re-introduce a divergent settled-protection predicate. The
`verified_built`-only-from-`path_bound_bytes` restriction is
load-bearing: a content-bound squat that completes stays
`content_bound_claim` and remains displaceable by store evidence, while
a genuine store-backed build passes dispatch derivation and settles
unreachable (the maximum displacer rank is `path_bound_bytes`).
Monotonicity is scoped PER NODE LIFECYCLE (creation → settle): a
legitimate matching-identity re-creation after store GC or displacement
starts a new lifecycle at its fresh ingress rank --- the persistence
upsert applies creation-snapshot `EXCLUDED` semantics, deliberately not
`MAX` --- so the rank always describes the definition the CURRENT
lifecycle was admitted with. Settle/dispatch upgrades persist
best-effort outside the merge transaction; a lost write degrades to the
persisted ingress rank at recovery, which never weakens a victim's
protection because no displacer outranks `path_bound_bytes`.

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

#r("sched.recovery.log-buffer-sweep+2")[
  On lease acquisition, after re-stamping ring-buffer entries for
  PG-`Assigned|Running` assignments, the scheduler MUST discard every other
  retained, unsealed ring-buffer entry whose derivation is not present in the
  rebuilt DAG in a non-terminal state.
]

An ex-leader re-acquiring the lease retains its ring buffers across the flap
(so a still-streaming worker's in-flight execution keeps accumulating), but a
derivation that reached a terminal state under an interim leader is either not
loaded at recovery at all or loaded only as a Poisoned poison-TTL-tracking
node, and no other cleanup path covers its retained entry --- the stale
pre-flap lines would shadow the execution's stored log in `GetDerivationLogs`
(the ring buffer is probed before S3) and the periodic flusher would re-upload
its `.partial` snapshot every 30 seconds for the process lifetime. Poisoned
derivations' executions were already finalized by whichever leader poisoned
them, they are never re-dispatched while poisoned (a post-clear re-dispatch
mints a fresh `exec_id` and discards the entry first), and the only other
reaper is the 24-hour poison TTL --- so their retained entries are discarded
as well. Entries for derivations the rebuilt DAG tracks in a non-terminal
state survive regardless of which state (a post-reset retained buffer on a
`Ready` node is finalized by the cancel-sweep paths, not discarded). Sealed
entries are exempt: a seal marks a terminal this process already observed,
whose final flush request may still be queued; the flusher owns their removal.
The discarded entries' unflushed tails are accepted loss within the failover
bound of `obs.log.periodic-flush`.

Recovery sequence:

+ Load all non-terminal builds from PostgreSQL (`builds` and `derivations`
  tables)
+ Reconstruct DAGs from the derivations table and their edges
+ *Identify nodes in "waiting" state whose dependencies are all complete, and
  transition them to "ready"* (handles the case where the previous scheduler
  crashed between completing a node and releasing downstream)
+ Discover executors from the `assignments` table and from Kubernetes pod list
+ Query each known executor for current state via Heartbeat
+ For derivations marked "assigned":
  - If the assigned executor reports completion → process the result
  - If the assigned executor is gone → check rio-store for the output (it may
    have been uploaded before the executor died). If found, mark complete.
    Otherwise, reassign.
+ Resume scheduling from the reconstructed state

Executors buffer completion reports with retry logic: if `ReportCompletion`
fails (scheduler unreachable during failover), the executor retries with
exponential backoff until the scheduler accepts it.

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

#r("sched.executor.dual-register")[
  Executor registration is *two-step* --- there is no single registration RPC;
  instead, the scheduler infers registration from two separate interactions:
  + Executor opens a `BuildExecution` bidirectional stream to the scheduler
    (calling `ExecutorService.BuildExecution`).
  + Executor calls the separate `Heartbeat` unary RPC with its initial
    capabilities: `executor_id` (unique, derived from pod UID), `systems`
    (list, e.g., `[x86_64-linux]`; an executor may support multiple target
    systems via emulation), `supported_features` (list of
    `requiredSystemFeatures` the executor supports).
  + When the scheduler receives the first `Heartbeat` from an `executor_id`
    that also has an open `BuildExecution` stream, it creates an in-memory
    executor entry with the reported capabilities and marks the executor as
    `alive`.
  + Scheduler begins sending `WorkAssignment` messages on the stream.
]

#r("sched.dispatch.fod-to-fetcher")[
  Per ADR-019, `hard_filter()` rejects any derivation-executor pairing where
  `drv.is_fixed_output != (executor.kind == Fetcher)`. Fixed-output derivations
  route ONLY to fetcher-kind executors; non-FODs route ONLY to builder-kind
  executors. The `ExecutorKind` is reported via `HeartbeatRequest.kind` and
  stored on `ExecutorState`.
]

#r("sched.dispatch.fod-builtin-any-arch")[
  A FOD with `system="builtin"` is eligible on any registered fetcher
  regardless of arch. Every executor appends `"builtin"` to its advertised
  `systems` unconditionally at startup (before the first heartbeat), so
  `hard_filter()`'s `system-mismatch` clause matches on the union.
  `best_executor()` scores across the flat `executors` map (keyed by
  `ExecutorId`, not pool), so a `builtin` FOD overflows to whichever arch's
  fetchers have capacity. Arch-specific FODs (`system="x86_64-linux"` inherited
  from stdenv) match only fetchers advertising that system.
]

FODs and non-FODs share the same `find_executor()` path: intent-match (ADR-023)
first, else `best_executor()` over the kind-matching pool. The `kind=fetcher`
hard-filter in #rref("sched.dispatch.fod-to-fetcher") is the absolute boundary
--- if no fetcher is available the @fod queues; the scheduler NEVER sends a FOD
to a builder under pressure. A queued FOD is preferable to a builder with
internet access. The #(refs.metric)("rio_scheduler_queue_depth")`{kind}` gauge
tracks queued derivations per kind.

#r("sched.timeout.promote-on-exceed+3")[
  A `BuildResultStatus::TimedOut` completion MUST double
  `resource_floor.deadline_secs` (#rref("sched.sla.reactive-floor")) and reset
  the derivation to `Ready` for re-dispatch, NOT terminal-cancel. The next
  dispatch carries the doubled deadline --- "same inputs → same timeout" no
  longer holds. Bounded by a separate `timeout_retry_count` against
  `RetryPolicy.max_timeout_retries`: a genuinely-infinite build still goes
  terminal (`Cancelled`, retriable on explicit resubmit) after exhausting
  promotions instead of walking forever. `timeout_retry_count` is in-memory
  only (recovery resets to 0, conservative) and separate from `retry_count` /
  `infra_retry_count` so timeouts neither consume the transient budget nor get
  masked by the infra time-window reset. I-200: before this, `TimedOut` went
  straight to `Cancelled` and the I-199/I-197 promotion only fired on the
  K8s-deadline-kill → disconnect path, not on the executor-side
  `build_timeout_secs` → clean `TimedOut` report path.
]

#r("sched.reassign.no-promote-on-ephemeral-disconnect+4")[
  Reassigning a derivation after an executor disconnects MUST NOT bump
  `resource_floor`. Disconnect is ambiguous --- pod-kill, store-replica-restart,
  node failure, deadline kill are all NOT inherently sizing signals (live QA:
  cmake medium→large→xlarge from a pod-kill + store-replica-restart with zero
  builds run; floor is sticky per M_044). The disconnect path re-queues at the
  current floor and records `(executor_id → drv_hash)` into a
  `recently_disconnected` map (60s TTL). The CONTROLLER is authoritative on
  termination reason via `AdminService.ReportExecutorTermination`:
  `OomKilled`/`EvictedDiskPressure`/`DeadlineExceeded` → `bump_floor_or_count`
  (#rref("sched.sla.reactive-floor")); other reasons → no-op. A disconnect
  AFTER `CompletionReport` for the running drv (`last_completed ==
  running_build`) records NO `recently_disconnected` entry --- expected
  one-shot exit (I-188 race). Defense-in-depth with
  #rref("sched.ephemeral.no-redispatch-after-completion"): that closes the
  I-188 race at the source.
]

#r("sched.termination.deadline-exceeded+3")[
  A `ReportExecutorTermination(DeadlineExceeded)` MUST double
  `resource_floor.deadline_secs` (or increment `timeout_retry_count` if already
  at the 24h cap, #rref("sched.sla.reactive-floor")) for the derivation that
  was running on the disconnected executor. The report carries the JOB name
  (the k8s Job controller deletes the Pod when `activeDeadlineSeconds` fires,
  so the controller observes the Job condition `Failed/DeadlineExceeded`
  instead, #rref("ctrl.terminated.deadline-exceeded")); the scheduler
  prefix-matches `recently_disconnected` keys (pod name = `{job}-{5char}`).
  This is defense-in-depth behind the worker-side `build_timeout` →
  `BuildResultStatus::TimedOut` primary path
  (#rref("sched.timeout.promote-on-exceed")): with
  #rref("ctrl.ephemeral.intent-deadline") the scheduler-computed
  `SpawnIntent.deadline_secs` carries 5× headroom over the predicted p99 wall
  time, so this only fires when the worker is too wedged (FUSE deadlock, kernel
  hang) to time itself out. The disconnect path already re-queued, so this does
  NOT `reset_to_ready` --- it only promotes (so the next dispatch goes larger)
  and counts (so the ladder is bounded). At `max_timeout_retries` the floor is
  at ceiling; terminal `Cancelled` is owned by the worker-side `TimedOut` path.
]

#r("sched.ephemeral.no-redispatch-after-completion")[
  When an executor completes a build and its `running_build` slot becomes
  empty, the scheduler MUST mark it `draining=true` immediately --- before the
  same actor turn's `dispatch_ready` runs. `has_capacity()` then rejects it.
  Closes the I-188 race at the source: every executor exits after its one
  build, so re-dispatching to its freed slot guarantees an
  Assigned-never-Running reassign.
]

#r("sched.assign.resource-fit")[
  `hard_filter()` rejects any executor whose `memory_total_bytes <
  drv.sched.last_intent.mem_bytes` as a hard filter, same position as
  `has_capacity()`. `last_intent` is the dispatch-time `solve_intent_for()`
  output (mem clamped at `resource_floor`). An executor reporting
  `memory_total_bytes == 0` (cgroup `memory.max=max`, no k8s limit set ---
  #src("rio-builder/src/cgroup.rs") sends 0 for `None`) is treated as
  unlimited-fit. A derivation with `last_intent == None` (never dispatched:
  cold start / recovery) fits any executor. This rejects a derivation whose
  solved memory exceeds the worker's actual cgroup limit before assignment
  rather than OOM-killing mid-build.
]

#r("sched.assign.warm-gate")[
  A newly-registered executor (step 3 above --- first heartbeat with open
  stream) receives an initial @prefetch-hint before any `WorkAssignment`. The
  executor fetches the hinted paths into its FUSE cache and replies with
  `PrefetchComplete` on the `BuildExecution` stream. The scheduler's
  `ExecutorState.warm` flag starts `false` and flips `true` on receipt.
  `best_executor()` filters out `warm=false` executors from its candidate set
  --- but falls back to cold executors if no warm executor passes the hard
  filter (single-executor clusters and mass-scale-up must not deadlock). Empty
  scheduler queue at registration time → `warm` flips `true` immediately
  (nothing to prefetch for). Hint contents select up to 32 Ready derivations
  sorted by fan-in (interested-builds count) descending, union their input
  closures, sort by occurrence frequency descending, cap at 100 paths --- the
  selection is deterministic for a given queue state. The warm-gate is
  per-executor: a second executor registering while the first is still warming
  does not delay builds that the second (already warm) executor can take.
]

#r("sched.executor.deregister-reassign")[
  *Deregistration:* An executor is removed from the scheduler's state when:
  - The `BuildExecution` stream is closed (graceful shutdown or network
    failure)
  - Heartbeat timeout: the actor's tick (configurable, default 10s) finds
    `last_heartbeat` older than `HEARTBEAT_TIMEOUT_SECS` (=
    `MAX_MISSED_HEARTBEATS × HEARTBEAT_INTERVAL_SECS` = 30s). Effective
    wall-clock timeout: \~30--40s depending on tick phase alignment.
  On deregistration, all derivations in `assigned` state for that executor are
  transitioned back to `ready` for reassignment.
]

#r("sched.backstop.timeout+4")[
  *Backstop timeout:* Separately from executor deregistration, `handle_tick`
  checks each `running` derivation's `running_since` timestamp. If elapsed time
  exceeds `max(est_duration × 3, build_timeout + 10min)` --- where
  `est_duration` is reference-seconds denormalized to wall-clock via the
  slowest fleet `hw_factor` per #rref("sched.sla.hw-ref-seconds") --- the
  scheduler sends a CancelSignal to the executor, marks the executor draining
  (its task is wedged; dispatch must not feed it new work that would sit
  Assigned forever), resets the derivation to `ready`, increments
  `failure_count`, and adds the executor to `failed_builders`. This catches the
  "executor is heartbeating but daemon is wedged" case where no stream-close or
  heartbeat-timeout fires; the accounting bounds the loop at the poison
  threshold. The #(refs.metric)("rio_scheduler_backstop_timeouts_total")
  counter tracks these events.
]

#r("sched.timeout.per-build")[
  `BuildOptions.build_timeout` (proto field, seconds) is a wall-clock limit on
  the _entire_ build from submission to completion. In `handle_tick`, any build
  with `submitted_at.elapsed() > build_timeout` has its non-terminal
  derivations cancelled and transitions to `Failed`, with `error_summary` set
  to `"build_timeout {N}s exceeded (wall-clock since submission)"`. This is
  distinct from #rref("sched.backstop.timeout") (per-derivation heuristic:
  est×3) and distinct from the executor-side daemon floor (which also receives
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

= Backpressure

The scheduler applies @backpressure at multiple layers to prevent overload:

*gRPC flow control:* The `BuildExecution` streams use the default HTTP/2 flow
control window (64 KiB initial, dynamically adjusted). The scheduler does not
send new `WorkAssignment` messages to an executor whose send window is
exhausted, naturally rate-limiting dispatch to slow consumers.

#r("sched.backpressure.hysteresis")[
  *Actor queue depth limit:* The DAG actor's `mpsc` channel has a fixed
  capacity (`ACTOR_CHANNEL_CAPACITY` = 10,000 messages; compile-time constant).
  If the queue depth exceeds 80% of capacity:
  + `CompletionReport` messages from executor `BuildExecution` streams block
    the stream-reader task on the actor channel send (completions must not be
    dropped --- a lost completion would leave the derivation stuck `Running`).
  + `LogBatch` messages are dropped (non-blocking `try_send`) --- the ring
    buffer already holds log lines and live-forward is a nice-to-have.
  + New `SubmitBuild` requests from the gateway receive gRPC
    `RESOURCE_EXHAUSTED` status.
  + The scheduler increments the
    #(refs.metric)("rio_scheduler_queue_backpressure") counter for alerting.
  Normal processing resumes when the queue depth drops below 60% (hysteresis to
  prevent oscillation).
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

  [`drv_logs`],
  [S3 blob metadata per execution (`exec_id` PK, `drv_hash`) --- `s3_key`,
    `line_count`, `is_complete`, `started_at` for log-flush UPSERTs and TTL GC],

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

#r("sched.db.clear-poison-batch")[
  `clear_poison` has a `clear_poison_batch(&[DrvHash])` variant using `WHERE
  drv_hash = ANY($1)`. The merge-time resubmit-reset path (`reset_on_resubmit`)
  clears poison for every node a resubmit flipped from terminal to fresh;
  per-hash sequential calls inside the single-threaded actor cost N round-trips
  on the dispatch hot path. The batch variant additionally increments
  `resubmit_cycles` (the scalar zeroes it: admin/TTL = full reset; resubmit =
  bound accumulates).
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
    drv_hash            TEXT NOT NULL,          -- the declared .drv store path (== drv_path, ingress-enforced); CA identity lives in realisations
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
  Auxiliary tables omitted from pseudo-DDL above: `drv_logs` (S3 blob
  metadata per derivation execution, `exec_id` PK) and `build_event_log`
  (Prost-encoded BuildEvent per sequence for gateway replay). See
  `rio-migrations/migrations/` for full schema.
]

= Leader Election

#r("sched.lease.k8s-lease")[
  The scheduler uses a *Kubernetes Lease* (`coordination.k8s.io/v1`) for leader
  election, via an in-house implementation modeled on client-go's
  `leaderelection` package. A background task polls every 5 seconds against a
  15-second lease TTL (3:1 renew ratio, per Kubernetes convention). On the
  acquire transition (standby → leader), the task increments the in-memory
  generation counter and sets `is_leader=true`; on the lose transition, it
  clears `is_leader`. The dispatch loop checks `is_leader` and no-ops while
  standby (DAGs are still merged so state is warm for takeover).
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
  rv stays unchanged for `lease_ttl` of local time, nobody has written ---
  steal. Only the standby's own `Instant` monotonicity matters; the
  `renewTime` value is never read.
- *Transient API errors:* On apiserver errors, the loop logs a warning and
  retries on the next tick without flipping `is_leader`. If errors persist past
  the lease TTL, the local self-fence (#rref("sched.lease.self-fence")) flips
  `is_leader=false` and another replica acquires --- correct behavior for a
  replica with broken K8s connectivity.
- *Split-brain window:* This is a polling loop, not a watch-based fence. During
  a true network partition where the leader cannot reach the apiserver but can
  still reach executors, both replicas may believe they are leader for up to
  `lease_ttl` (15s). This is *acceptable* because dispatch is idempotent: DAG
  merge dedups by `drv_hash`, and executors reject stale-generation assignments
  after seeing the new generation in `HeartbeatResponse`. Worst case: a
  derivation is dispatched twice, builds twice, produces the same deterministic
  output. Wasteful but correct.

#r("sched.lease.self-fence")[
  If the lease loop believed it was leading but has not had a successful
  apiserver round-trip in over `LEASE_TTL` (15s), it MUST flip
  `is_leader=false` locally (`maybe_self_fence`) and emit
  #(refs.metric)("rio_scheduler_lease_lost_total"). At that point any replica
  that _can_ reach the apiserver has already stolen the lease via
  observed-record expiry; the only world where this replica is still rightful
  leader is one where no replica can reach the apiserver, in which case
  dispatch is pointless anyway. The self-fence does NOT attempt `step_down()`
  or `pod-deletion-cost` PATCH (the apiserver is unreachable).
  `last_successful_renew` is reset on every Standby/Conflict observation as
  well as on successful renew --- the clock tracks "am I blind", not "am I
  leader".
]

#r("sched.lease.standby-drops-writes")[
  A replica that has lost the lease MUST NOT write scheduler-owned PG state
  (`derivations`, `realisations`, `build_samples`, `build_event_log`). Open
  `BuildExecution` worker streams are generation-fenced at the gRPC reader: the
  reader captures the lease generation at stream-open and breaks the loop
  (closing the stream) before forwarding `ProcessCompletion` /
  `PrefetchComplete` if `is_leader=false` or the generation has changed --- the
  worker reconnects to the new leader. `ProcessCompletion`, `CancelBuild`,
  `ReportExecutorTermination`, `AckSpawnedIntents`, `ReconcileAssignments`,
  `SubstituteComplete`, and `Tick` are additionally gated at actor dispatch as
  defense-in-depth.
  `ExecutorConnected`/`Disconnected`/`DrainExecutor`/`Heartbeat`/`PrefetchComplete`
  arms stay ungated (they keep `self.executors` accurate for dashboard +
  reconnect-after-reacquire); their PG-touching sub-calls (`drain_phantoms`,
  `dispatch_ready`, and `reassign_derivations` --- the disconnect/force-drain
  tail that can poison a derivation and run the terminal log epilogue) are
  individually leader-gated.
  `ForwardLogBatch` is NOT gated (in-memory ring only). `ForwardPhase` is NOT
  gated either, and is a deliberate exception to the table list above:
  `Event::Phase` is persisted (#rref("sched.log.phase-binding")), so a deposed
  leader whose stale DAG still holds the assignment writes `build_event_log`
  rows. Gating the arm would not seal the table (the event-log persister task
  has no leader gate); the sequence collision with the new leader is resolved
  first-writer-wins by `ON CONFLICT (build_id, sequence) DO NOTHING` inside
  the #rref("sched.lease.generation-fence") dual-writer window.
]

- *Terminal-build cleanup:* the `CleanupTerminalBuild` arm also stays ungated
  (in-memory build/event-map removal, the DAG reap, and log-buffer bookkeeping
  run on standby); its post-reap survivor re-evaluation --- which can persist
  derivation status, clear the persisted `topdown_pruned` mark, and terminally
  fail builds via the topdown fail-fast --- is individually leader-gated, like
  the per-sub-call gates above.

#r("sched.lease.generation-fence")[
  *Generation-based staleness detection is executor-side only.* On each lease
  acquisition, the new leader increments an in-memory `Arc<AtomicU64>`
  generation counter. Executors see the new generation in `HeartbeatResponse`
  and reject any `WorkAssignment` carrying an older generation. *No
  PostgreSQL-level write fencing exists.* A deposed leader's in-flight PG
  writes will succeed; the split-brain window is bounded by the Lease renew
  deadline (default 15s). Because the writes in question are idempotent upserts
  keyed by `drv_hash` and status transitions are monotone, brief dual-writer
  windows do not corrupt state.
]

#memo(title: [Optional future hardening])[
  If stricter at-most-one-writer semantics are needed, add a `scheduler_meta`
  row with a `leader_generation` column and gate all synchronous writes with
  `WHERE leader_generation = $current_gen`. Not currently implemented --- the
  executor-side generation check plus idempotent PG schema is sufficient for
  correctness.
]

#r("sched.lease.graceful-release")[
  On graceful shutdown (SIGTERM), if the lease loop was leading, it calls
  `step_down()` to clear `holderIdentity` before the process exits. This is an
  optimization, not a correctness requirement: without it, the next replica
  waits up to `lease_ttl` (15s) for observed-record expiry. With it, the next
  replica's `decide()` sees an empty holder and steals immediately on its next
  poll (\~1s). The `step_down()` call is a resourceVersion-guarded PUT (409 →
  someone already stole, treated as success); `main()` awaits the lease-loop's
  `JoinHandle` after `serve_with_shutdown` returns, ensuring the PUT lands
  before process exit. If `step_down()` fails (apiserver unreachable), the loop
  logs a warning and observed-record expiry is the fallback.
]

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

#r("sched.lease.deletion-cost")[
  On the acquire transition, the lease loop annotates its own Pod with
  `controller.kubernetes.io/pod-deletion-cost: "1"`; on the lose transition, it
  sets `"0"`. Kubernetes's ReplicaSet controller sorts pods by this annotation
  (ascending, lower = kill first) when picking which pod to evict during
  scale-down --- including the surge-reconcile phase of RollingUpdate. With
  cost=1 on the leader and cost=0 on the standby, `kubectl rollout restart`
  kills the standby first, new pod comes up, acquires (old leader step_down on
  SIGTERM), no double leadership churn. The PATCH is fire-and-forget (the lease
  loop must not block on it) and failure is non-fatal: without the annotation,
  K8s picks arbitrarily, which means 50% of rollouts churn leadership twice
  instead of once. Annoying but correct.
]

*Deployment strategy interaction:* Readiness is decoupled from leadership
(#rref("sched.grpc.leader-guard")): both pods are Ready (TCP probe = process
up), RollingUpdate works with `maxUnavailable: 1`, zero-downtime rollouts.
Clients route via a health-aware balanced channel against the headless Service
`rio-scheduler-headless` --- they DNS-resolve to pod IPs, probe
`grpc.health.v1/Check` on each (NOT_SERVING on standby), and only insert the
leader into the tonic p2c balancer. The ClusterIP Service `rio-scheduler` is
kept for per-call connects (controller reconcilers, rio-cli) where a 50% chance
of hitting UNAVAILABLE + retry is acceptable. Combined with `step_down()` and
pod-deletion-cost, a rollout flips leadership exactly once: K8s kills the
standby first (cost=0), new pod comes up as standby, K8s kills the old leader,
old leader step_down releases the lease, new pod acquires within one poll
(\~1s), balanced-channel clients reroute within one probe tick (\~3s).
Executors reconnect in place --- running builds continue, no pod restarts.

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
- #src("rio-scheduler/src/logs/") --- LogBuffers ring buffer + S3 LogFlusher
- #src("rio-lease/src/") --- Kubernetes Lease leader-election loop
  (generation counter, `is_leader` flag, `recovery_complete` gate)
- #src("rio-scheduler/src/actor/recovery.rs") --- State recovery: reload
  non-terminal builds/derivations from PG on LeaderAcquired
- #src("rio-scheduler/src/event_log.rs") --- PostgreSQL-backed
  `build_event_log` writes for gateway `since_sequence` replay
- #src("rio-scheduler/src/admin/") --- AdminService gRPC (ClusterStatus,
  DrainExecutor, GetDerivationLogs, TriggerGC)

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
    node((3, 0), [Dispatch Ready\ Nodes], name: <disp>),
    node((4, 0), [Assign to\ Best Executor], name: <assign>),
    node((4, 1), [Executor], name: <ex>, fill: accent.lighten(88%)),
    node((3, 1), [Process\ Completion], name: <comp>),
    node((2, 1), [Release\ Downstream], name: <rel>),
    node((1, 1), [Update Duration\ Estimates (async)], name: <upd>),
    edge(<sb>, <merge>, "-|>"),
    edge(<merge>, <cp>, "-|>"),
    edge(<cp>, <disp>, "-|>"),
    edge(<disp>, <assign>, "-|>"),
    edge(<assign>, <ex>, "-|>", [BuildExecution\ stream], label-size: 0.75em),
    edge(<ex>, <comp>, "-|>", [CompletionReport], label-size: 0.75em),
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
  [Executors' `BuildExecution` streams disconnect; executors go idle. Gateways
    return errors to clients.],

  [*Recovery*],
  [New leader acquires the Kubernetes Lease → fire-and-forgets `LeaderAcquired`
    → `recover_from_pg` rebuilds DAG from PG. Executors reconnect;
    `ReconcileAssignments` (45s delayed) checks for orphan completions or
    resets stale assignments. Gateways reconnect via `WatchBuild(build_id,
    since_sequence=last_seen)`.],
)

Under network partition between scheduler and executors, executors detect via
heartbeat-response timeout, stop accepting new work, finish current builds, and
buffer completion reports. The scheduler calls `reset_to_ready()` on
disconnected executors' running builds --- they go directly back to Ready
(increment `retry_count`), no intermediate status classification. After
partition heals, executors reconnect and replay buffered completions.

For split-brain mitigation: the Kubernetes Lease prevents two active schedulers
under normal conditions; brief dual-leader windows are bounded by the lease
renew deadline (\~15s); the assignment-generation counter lets executors ignore
assignments from a deposed leader. PG writes are idempotent upserts. Optional
future hardening: a `scheduler_meta` row with a generation-guard WHERE clause
for strict fencing (current: idempotent writes tolerate the dual-leader
window).

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

The scheduler drives @fuse cache pre-warming. When scheduling a derivation to a
worker, the scheduler sends prefetch hints for the input closure paths via the
bidirectional build execution stream (#rref("sched.assign.warm-gate")). The
worker's FUSE daemon begins fetching these paths into its local SSD cache
before the build starts, converting serial "fetch then build" into overlapped
execution.

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

== Build-log archival stays in scheduler // supersedes ADR-024
<sched-rationale-logs>

`rio-scheduler` writes build logs to S3 directly. The proposal to add a
`StoreService.PutLog(stream LogChunk) → LogRef` RPC --- routing the flusher
through it and dropping `aws-sdk-s3` from the scheduler's dependency tree ---
was *rejected*.

Build logs are scheduler artifacts, not store artifacts. rio-store's domain is
the content-addressed Nix store: @nar chunks, @narinfo, signatures, realisations,
GC by reachability. Build logs are execution-addressed (`exec_id`, not
content-addressed), mutable (periodic snapshots and the final flush UPSERT the
same row), retained on a wall-clock TTL (not reachability), unsigned, and not
deduplicated. They share nothing with `PutPath` except "bytes go to S3." Adding
them to `StoreService` dilutes its single responsibility into "also a generic
blob bucket." The `drv_logs` metadata table is correlated to scheduler state
(`build_derivations.exec_id` records which execution each interested build
observed); a `PutLog` RPC either leaves the PG write scheduler-side anyway or
drags scheduler-private execution identity into store's vocabulary.

Latency is not the problem; channel mixing is. The flush is fully async --- the
actor `try_send`s and moves on --- but the periodic-snapshot bytes would ride
the same scheduler→store gRPC channel that carries `FindMissingPaths` /
`BatchQueryPathInfo`, the calls that I-110 identified as the scaling bottleneck
under builder fan-out. Best-effort bulk log traffic competing for h2 stream
slots and store CPU with latency-sensitive cache lookups is a regression.
Direct scheduler→S3 keeps that traffic on a separate fault domain.

The original concern --- two components hand-rolling AWS SDK config differently
--- was already fixed by `rio_common::s3::default_client` (one config home, one
retry policy, one stalled-stream setting). What remains is "scheduler links
`aws-sdk-s3`," which is a binary-size observation, not an architectural one.

*Consequences.* `rio-scheduler` keeps `aws-sdk-s3` as a direct dependency.
`rio_common::s3` remains the single S3-config home for both scheduler and
store. `StoreService` stays scoped to Nix-store semantics. Log-loss-on-crash
bound stays ≤30s per #rref("obs.log.periodic-flush"). Revisit if: store gains a
generic blob tier for some other reason; multi-region deployment makes
per-component @irsa roles materially expensive; or periodic-snapshot volume
grows enough that scheduler→S3 egress becomes a cost line item (at which point
the fix is delta-upload, not relocation).

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
(#rref("sched.actor.dispatch-decoupled")); bound individual submissions at
`MAX_DAG_NODES = 1,048,576` / `MAX_DAG_EDGES = 5,242,880` --- global
compile-time constants in #src("rio-common/src/limits.rs"), not per-tenant
(SubmitBuild rejects DAGs exceeding either limit before merge). The same
module owns the per-node `drv_content` ingress bound
(`MAX_DRV_CONTENT_BYTES` = 1 MiB), which the gateway's content-bound
hook-fallback cap aliases so the producer and consumer limits cannot
drift apart.
