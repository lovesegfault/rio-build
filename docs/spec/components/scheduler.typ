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
plus once per Tick, bounded by the per-tick probe-admission quota
(`DISPATCH_PROBE_TICK_QUOTA` with `probed_generation` stamping --- the
deferred tail carries to the next tick, oldest-first;
#rref("sched.admission.work-per-turn")). The actor-mailbox protection that
motivated the pacing is carried by the
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

#r("sched.trust.report-membership+4")[
  A worker-supplied `output_path` MUST enter `path_tenants` (or any other
  registration sink) ONLY if it is bounded by a scheduler-verifiable path-value // quantifier: census(forged_output_path_never_reaches_path_tenants_on_any_lane)
  law for that assignment, on EVERY report lane and EVERY face: the // quantifier: census(ca_no_upload_report_never_flips_visibility_on_any_lane)
  input-addressed and fixed-output faces check membership in the
  scheduler-authoritative expected set (`expected_output_paths` ---
  dispatch-minted and signed into the `AssignmentClaims` the store enforces on
  upload; admitted lane against the resident node's set, late-report Register
  lane against the durable row's set, so the evicted face is exactly as
  checked as the resident one); the floating-CA face (`is_ca` and not
  fixed-output, the claims-mint predicate --- no dispatch-time expected set
  exists) checks store-recorded production evidence per the corroboration rule
  below, on EVERY cohort INCLUDING the empty one: an untenanted report // quantifier: census(untenanted_floating_ca_report_never_mints_global_realisations)
  (NULL-tenant anon/dev build, or a late lane whose tenant rows aged out of
  the cold resolve) consults with the empty cohort, receives the EMPTY
  evidence set, and the globally-keyed realisation insert refuses every path
  --- the untenanted face is part of the law's domain, never an exemption
  (the cohort-keyed vacuity argument discharges only the tenant-keyed stamp
  reader; see the evidence-scope rule below). Non-membership MUST be a typed
  refusal --- counted
  (#(refs.metric)("rio_scheduler_unexpected_built_output_total") on the
  expected-set faces,
  #(refs.metric)("rio_scheduler_unevidenced_ca_output_total") on the CA face),
  attributed, and non-poisoning (the report's lawful effects proceed).
  Trust-boundary residual pricing for worker report fields MUST be
  per-consumer: every sink of a worker-supplied field either re-derives from
  scheduler-authoritative data, sits downstream of this membership check, or
  carries a priced-residual entry NAMING that sink (the taint-to-consumer
  census is the enforcing witness).
]
The name-membership rule above bounds the report's output *names*; this rule
binds the *paths* --- the axis that reaches tenant visibility. The repaired
hole (bug 138): a report naming another tenant's existing path triggers no
upload, so the store's PutPath `path ∈ claims.expected_outputs` check never
runs, and the stamped row flips the victim path Hidden → Visible for the
forging tenant through `own_built_projection`'s `bool_or(tenant_id)` and the
I-217 verdict. The bug 132 recurrence, one face over: the original form of
this rule EXEMPTED floating-CA on the claim that the store's content recompute
covers that face "exactly as on upload" --- a compensating control that
structurally cannot fire on the no-upload attack path (the recompute lives
inside PutPath; the attack uploads nothing). Exemptions keyed on
submitter-controlled attributes (the CA-ness of one's own drv) convert
per-face residuals into adversary-chosen bypasses; the CA face therefore
joins the law's domain with its own evidence base rather than an exemption.

#r("sched.trust.report-corroboration+4")[
  A worker report's claim MUST NOT move scheduler-persisted state without
  corroboration the scheduler can verify against evidence it (or the store)
  owns. Two faces:
  (1) *Visibility:* a tenant-visibility registration stamp for a floating-CA
  report MUST bind to store-recorded production evidence: the stamp (and the
  realisation insert that feeds later stamp lanes) admits a worker-reported
  `output_path` ONLY if the store's ingest-lane registration (`path_tenants` // quantifier: census(ca_no_upload_report_never_flips_visibility_on_any_lane)
  --- the SAME rows the visibility verdict's `own_built_projection` reads, // quantifier: census(ca_stamp_lanes_consult_production_evidence)
  one source) records the path for at least one tenant of the reporting
  build's attributed cohort: no upload, no stamp. Absent evidence MUST be a
  typed refusal --- counted
  (#(refs.metric)("rio_scheduler_unevidenced_ca_output_total")), attributed,
  non-poisoning, degrading exactly to the pre-registration posture (bytes
  durable, tenant-invisible until a lawful re-stamp); an evidence-consult
  error MUST fail closed.
  (2) *Sizing floors:* a persisted resource-floor bump on a worker-reported
  failure MUST present a typed corroboration witness verified against
  evidence the scheduler owns, on EVERY axis the floor struct carries --- // quantifier: census(floor_mutation_census)
  the carrier is irrelevant to the obligation: claims riding the TYPED
  `failure_classification` wire field (never `error_msg` text) corroborate
  with telemetry consistent with the shape the scheduler itself assigned at
  dispatch (the corroboration anchor a forger cannot choose), and the
  STATUS-borne deadline axis (`TimedOut`) corroborates against the
  scheduler's own attempt clock (attempt-open duration at least half the
  assigned deadline --- `running_since` vs the reconciled dispatch deadline,
  neither mintable by a worker). The demand is enforced INSIDE the floor
  mutation (the witness parameter --- an ungated axis cannot compile) and
  the writer population is MACHINE-DERIVED (the floor-mutation census
  quantifies over mutation sites, never over one wire enum --- the
  carrier-keyed census was the wave-11 evasion's door). Untyped or
  uncorroborated claims are classify-only, counted
  (#(refs.metric)("rio_scheduler_uncorroborated_sizing_claim_total")), and
  never move a floor. The depth bound is POPULATION-denominated: at most one
  doubling per corroborated incident identity (drv_hash, exec_id) --- the
  report admission fold's once-per-exec dedup is the identity law, and each
  ladder step burns a real scheduled attempt at the previously-assigned
  size, so a forger pays the honest path's cost with no amplification and a
  paced forger gains nothing from pacing.
  Residual pricing for any face of this boundary MUST name a firing
  predicate machine-bound in the taint census --- a census row whose
  compensating control cannot fire on the priced path is census-RED, never
  self-reported prose.
]
Granularity, priced: the binding is (path, claims-tenant-cohort), not
(path, exec) --- the store's durable upload trace is the tenant-keyed ingest
stamp (`AssignmentClaims` carries no exec id). The cross-tenant kill is
intact: the attack cohort never uploaded the victim path. The in-tenant
residual (a tenant re-claiming its OWN previously-registered path as a fresh
CA output) carries no confidentiality flip and is integrity-equivalent to a
compromised builder uploading arbitrary CA content, which the builder trust
model already prices. The honest-path residual (a single-output upload whose
best-effort ingest stamp was skipped under PG pressure) refuses until a
lawful re-stamp --- availability-class, store-warn-visible; the batch upload
lane stamps inside its atomic transaction and has no such window.

#r("sched.trust.evidence-scope")[
  A consult that guards a consumer MUST derive its evidence requirement from
  THAT consumer's scope, never from the reporting cohort: a cohort-keyed
  vacuity argument (an empty cohort "has no boundary to guard") discharges
  only cohort-keyed readers, and a GLOBALLY-keyed reader demands evidence on
  every face --- the empty cohort yields the EMPTY evidence set (refuse all),
  never an absent one (skip the law). The consult's READER SET is
  machine-derived (a generator walks the crate enumerating every consumer of
  the evidence), and each reader carries a measure-compatibility witness
  stating what it assumes of the evidence and why the consult's scope
  entails it; a reader whose assumption the consult's scope does not entail
  is census-RED, never prose-priced.
]
The founding instance (bug 155, round-12 HIGH): the CA production-evidence
consult has TWO readers --- the tenant-keyed `path_tenants` stamp (the
cohort-vacuity argument is valid THERE: empty cohort, no stamp, nothing to
guard) and the globally-keyed `realisations` insert (PK
`(modular_hash, output_name)`; its consumers `query_batch` /
`query_prior_realisation` are tenant-unscoped). The wave-11 close guarded
both behind one cohort-keyed `Option` whose `None` arm was sound for the
stamp and a forgery channel for the insert: an untenanted floating-CA report
durably minted a first-writer-wins `modular_hash -> victim_path` row with
zero corroboration --- the exact cross-tenant flip the close shipped to
kill, reachable one arm over. The repair derives the requirement from the
insert's own scope (non-optional typed evidence; empty cohort refuses all)
and pins both readers in the machine-derived census with their witnesses.

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

#r("sched.merge.exec-correlation+8")[
  The scheduler MUST set `build_derivations.exec_id` for every interested
  build that has not already recorded an observation for that derivation
  when a derivation that has been dispatched (and therefore has an
  `exec_id` recorded for it) reaches a terminal state through a path
  where an execution actually ran: `Completed` (success or recovery's
  orphan adoption), `Poisoned` (permanent failure), `Cancelled` reached
  from `Assigned`/`Running`, and any terminal reached by a derivation
  whose prior, reset execution left a stamped log buffer (the
  build-cancel sweep's `Cancelled`/`DependencyFailed` arms and the
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
(`Poisoned` and timeout-exhausted `Cancelled`), and
`cancel_build_derivations` (any path that cancels in-flight derivations:
user cancel, per-build wall-clock timeout, fail-fast --- including the
materialization settlement's resubmit-directing fail-fast), and
recovery's `adopt_orphan_completion` (an orphaned assignment
whose outputs are found in the store --- the execution completed while the
scheduler was down, and an ex-leader re-acquiring the lease may still hold
its unflushed log tail) --- each of which implies the worker ran the build.
The
not-yet-dispatched arms of the same cancel sweep (`Queued`/`Ready`/`Created` →
`DependencyFailed`) and the dependency-failure
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

#r("sched.tenant.authz+3")[
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
  on mismatch while the actor holds the build. Handlers MUST carry tenant
  identity only as the typed witness `require_tenant` produces
  (`CallerTenant`), and every tenant-scoped read of durable build state MUST
  take that witness — a fetch path that skips the gate does not typecheck.
  `ResolveTenant` is exempt: the gateway calls it during SSH key
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

#r("sched.retry.per-executor-budget+4")[
  `BuildResultStatus::InfrastructureFailure` does NOT count toward the poison
  threshold. It routes through a separate `handle_infrastructure_failure`
  handler: `reset_to_ready` + retry WITHOUT inserting into `failed_builders`.
  Executor-local issues (FUSE EIO, cgroup setup fail, OOM-kill of the build
  process) are not the build's fault. `TransientFailure` (build ran, exited
  non-zero, might succeed elsewhere) DOES count. Executor disconnect DOES count
  --- a build that crashes the daemon 3× is poisoned: an executor crash whose
  classifying report never arrives, once its failure is established (the
  pull-mode establishment sweep
  fills `termination_reason='unreported'`), charges the failure budget and
  counts toward the poison threshold; false-positives from
  unrelated executor deaths are cleared by `rio-cli poison-clear`. The
  budget's exclusion key is the attempt's _source node_, and ONLY that: an
  attempt row carrying
  `drv_attempts.source_node` (the column is written
  only from the controller-authoritative binding, never from worker-supplied
  identity) contributes that node as its exclusion/budget key; a row
  without one (a pull attempt whose binding ack has not landed, or a
  pre-pull legacy row) contributes NO exclusion key --- it still charges the
  flat `failure_count`, but it MUST NOT occupy a distinct-source slot in the
  poison threshold and MUST NOT leak a non-schedulable key (a pod name or
  intent id) into the placement exclusion. Small-fleet clause: when
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
The `+4` revision (decision P12, the executor-lifecycle/retry cross-campaign
close-out): the establishment-vehicle list shrinks to the establishment sweep
--- the correlation-TTL sweep and the scheduler-side backstop retired with the
stream dispatch path --- and the legacy pod-name fallback key is dropped: the
`+3` text let a row without `source_node` contribute its executor (pod-name)
key "until the stream path retires"; that path has retired, so identity-less
rows now charge flat counters only. Bounding consequence, stated for clarity:
a derivation whose attempts are never node-attributed is bounded by the
per-cycle transient/infra/timeout caps and the flat `failure_count` mode, not
by the distinct-source threshold.

#r("sched.dispatch.fleet-exhaust+5")[
  The fleet-exhaust verdict is the structural, immediate "every source this
  derivation could run on has already failed it" poison --- evaluated over
  `(excluded_sources, eligible_sources)` by `placeable()`, preserving the
  empty-universe-defers / exhausted-universe-poisons partition: when the
  eligible universe is empty the check MUST NOT poison --- the derivation
  defers, because an empty pool/fleet is a provisioning transient
  (autoscaler lag, a deployment rollout in progress), and poisoning on it
  would brick every build submitted during the rollout.
  Pull-mode evaluation point (the spawn-intent gate, AD2): the
  spawnable-source universe is k8s-side knowledge, so the controller ---
  which holds the node informers and renders the intent's `excluded_nodes`
  as anti-affinity --- evaluates exhaustion over the intent's CANDIDATE
  universe: the `pre` set is every schedulable (non-cordoned) node the
  intent admits ignoring exclusions --- NotReady nodes COUNT (a booting
  node is a provisioning transient; a Ready-only universe manufactures
  poisons out of node restarts) and the intent's own placement axes
  (selector, affinity) narrow it --- and the verdict is `pre` non-empty
  with `pre ∖ excluded_nodes` empty. The verdict MUST persist for
  `NO_ELIGIBLE_SOURCE_PERSIST_TICKS` consecutive reconcile ticks before
  the controller reports it (the spawn is withheld from the first gated
  tick; only the REPORT --- the poison --- is persistence-gated), and the
  report MUST echo the intent's `resubmit_cycle`. The scheduler MUST map
  that report for a still-Ready derivation to the fleet-exhaust poison arm
  (a `fleet_exhaust` marker row plus `Poison(FleetExhausted)`) only when
  the echoed cycle matches the derivation's current `resubmit_cycles`,
  the derivation still carries a non-empty exclusion set, and no spawn
  acknowledged within the defer window covers the intent (a fresh
  `acked_spawned` witness defers --- the verdict raced a spawn); on any
  guard miss and for a derivation that is no longer Ready (already
  poisoned, in flight, or resolved) it MUST acknowledge without
  poisoning, so controller re-ticks and stale verdicts stay idempotent.
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
arithmetic is the reference fold in `rio-scheduler/src/retry_policy.rs`
(kernel crate: `rio-retry-kernel`), and the model-checked statement of
the budget laws is `docs/spec/models/retryPolicy.qnt`.

#r("sched.retry.transient-budget+2")[
  A worker-reported `TransientFailure` (the build ran and exited non-zero;
  an `Unspecified` result status is treated identically) MUST record the
  reporting executor into `failed_builders`, increment `failure_count`, and
  then decide: if the poison threshold is reached
  (#rref("sched.retry.per-executor-budget")) or every eligible worker has
  already failed this derivation (#rref("sched.dispatch.fleet-exhaust")),
  the derivation is poisoned; otherwise, while the per-cycle transient
  count is below `RetryPolicy.max_retries`, the derivation MUST be requeued
  with an exponential backoff (`backoff_until = now + backoff(count)`,
  enforced at pull admission --- the kernel's fresh-mint arm answers
  `NotYetReady` until the window lapses, and spawn intents exclude the
  node --- cleared when the next attempt mints) and the count incremented;
  at or above `max_retries` the derivation is poisoned.
]
The transient budget is per poison cycle: a resubmit reset
(#rref("sched.merge.poisoned-resubmit-bounded")) restores the full
`max_retries` budget and charges `resubmit_cycles` instead. The backoff is
the only one among the failure classes (infra, timeout, disconnect, and
backstop requeues are immediate) --- the asymmetry is a recorded Phase-1
policy decision (divergence A7 of the retry campaign's catalog; the standing
question is the TODO at the retry kernel's `backoff_secs`), not specified
away here.

#r("sched.retry.store-degraded-uncharged+4")[
  An infrastructure failure carrying the builder's
  `BuildResult.store_degraded` flag
  (#rref("builder.outcome.store-degraded")) MUST be ADMITTED before it
  is believed: admission requires corroboration --- at least two
  distinct controller-authoritative node bindings flagging within the
  corroboration window (600 s), or the scheduler's own store RPCs
  failing inside that window --- AND a store-degraded count strictly
  below the kernel bound `STORE_DEGRADED_FREE_RUN` (12) within the
  trailing bounded-uncharged run: the run is the trailing sequence of
  `BOUNDED_UNCHARGED`-registry rows (the union of the uncharged
  classes), so a sibling bounded-uncharged row --- the worker-abort
  free close --- EXTENDS the run without advancing this count, and
  only a build-lane row outside the union (a charged classification,
  a controller row, a reset) breaks it. An admitted report MUST be
  recorded with the dedicated `store_degraded` outcome class and MUST
  be uncharged: the fold advances no transient/infra/timeout/poison
  counter, records no `failed_builders` exclusion, and never produces
  a poison verdict from such rows; the verdict MUST be a requeue paced
  by the derivation backoff curve computed from the store-degraded
  count within that same trailing bounded-uncharged run
  (`backoff_until = at + backoff(count)`, reset by any build-lane row
  outside the union --- folded to an event or not --- so the pacing
  fold and the admission scan consume ONE run law). A NON-admitted report (uncorroborated, or at
  the count bound) MUST be charged as plain infrastructure --- the
  flag is worker-supplied evidence and cannot mint unbounded uncharged
  requeues, alone or composed with the sibling uncharged class.
] The class is attributable to the STORE, not to the build or the node:
charging any per-derivation or per-executor budget would convert a long
store outage into poison verdicts and fleet-wide exclusion churn — the
exact amplification the heartbeat-era capacity flag absorbed and the 1d
collapse traded away (see the builder spec's retired-block note). The
pacing bound (#rref("sched.retry.attempts-bounded+4")'s carve-out) is
the breaker's own evidence threshold compounded with the backoff cap.

#r("sched.retry.attempts-bounded+5")[
  Every failure-driven retry loop MUST be bounded: every counted attempt
  charges at least one of the named budgets --- the per-cycle transient
  count (`max_retries`), the non-exempt infrastructure count
  (`max_infra_retries`), the exempt-infrastructure count
  (`max_exempt_infra_retries`), the timeout count (`max_timeout_retries`),
  the poison threshold (`PoisonConfig.threshold`), or the cross-cycle
  resubmit count (`POISON_RESUBMIT_RETRY_LIMIT`) --- every budget has a
  finite cap whose exhaustion produces a terminal state (`Poisoned` or
  `Cancelled`), no single attempt charges the same budget more than once,
  and an attempt exempted from one budget MUST be charged to another,
  with exactly one carve-out, itself finite: an ADMITTED
  `store_degraded` attempt
  (#rref("sched.retry.store-degraded-uncharged")) charges no count
  budget BY DESIGN and is bounded by pacing --- the backoff curve over
  its count within the trailing bounded-uncharged run, capped at
  `backoff_max_secs` --- while that count stays strictly below
  `STORE_DEGRADED_FREE_RUN` (12) and the outage is corroborated; the
  count is taken over the `BOUNDED_UNCHARGED` union run, so
  interleaving the sibling uncharged class cannot reset it. Past the
  bound, or uncorroborated, the report falls through CHARGED into the
  non-exempt infrastructure budget, so even this carve-out drains a
  finite budget and terminates in an operator-visible poison.
]
// SIGNED 2026-06-04 (owner, bughunt-2 fix-wave §5-S Q5): two-layer
// close for merged_bug_032 — unconditional kernel run bound
// STORE_DEGRADED_FREE_RUN = 12 (mirrors WORKER_ABORT_FREE_CLOSES
// discipline) + corroboration gate (≥2 distinct controller-bound nodes
// in 600 s OR scheduler store-health); bound-exceeded = charged
// fallthrough into the counted infra budget (operator-visible poison
// ~10 attempts later), not instant poison; both ship as documented
// consts, not config (no BLESS); the attempts-bounded carve-out above
// is re-worded accordingly (+4) with this signature as the
// counter-signed authorization.
//
// AMENDED 2026-06-07 (owner, bughunt-3 fix-wave §5 Q1): the
// 'consecutive run' wording this signature authorized is superseded —
// bug_098 showed the per-class break rule lets the two
// bounded-uncharged classes mutually reset each other's runs
// (strict alternation of worker-abort and store-degraded rows kept
// both runs ≤ 1 forever, reproducing the unbounded uncharged mint
// both bounds exist to close). The run is now the BOUNDED_UNCHARGED
// union trailing run with PER-CLASS counts: any registry row extends
// every scan; each class trips on its own count; charged/controller/
// reset rows still reset everything. Both consts keep this
// signature's values (12, 3). Re-worded accordingly: this rule body
// and carve-out (sched.retry.store-degraded-uncharged,
// sched.retry.attempts-bounded) and sched.attempt.worker-abort-bounded
// — versions bumped at this amendment. The round-3 SIGNED block lives
// at the BOUNDED_UNCHARGED registry doc in rio-retry-kernel/src/lib.rs.
//
// AMENDED 2026-06-09 (bughunt-4 S5b, recorded at this anchor per the
// same precedent): bug_182 — the pacing clause's "reset by any folded
// event outside the union" wording forked the run law the body's own
// run definition states ("only a build-lane row outside the union
// breaks it"): rows folded to NO event (Cascade, FleetExhaust,
// controller-reported rows) broke the admission scan but not the
// pacing fold, so a post-break store-degraded report was admitted
// fresh while paced at the dead run's cap. The clause is re-worded to
// the body's run definition and the kernel now derives BOTH consumers
// from one classifier (`run_step`, rio-retry-kernel). No const, bound,
// or admission semantics change; rule bumped +3 -> +4.
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
restarts on every attempt) --- contradiction C2 of the retry campaign's
Stage-A catalog (git history), bounded only by an optional per-build
`build_timeout`, and a pre-registered expected Stage-B falsification. The per-counter fencepost conventions
(whether the cap fires on the Nth or the N+1th attempt) currently differ
between counters; the reference fold reproduces each counter's own
convention and the fencepost unification stays the recorded open question
(the TODO at the retry kernel's `decide()`).

#r("sched.retry.counters-refine-history+4")[
  The per-derivation retry counters (`count`, `resubmit_cycles`,
  `infra_count`, `timeout_count`, `last_infra_failure_at`,
  `exempt_infra_count`, `failed_builders`, `failure_count`, `poisoned_at`,
  `backoff_until`) MUST at every point equal the reference fold of the
  derivation's observed failure-event history: each observed event charges
  the counters its class charges and no others; `infra_count` counts
  CONSECUTIVE infrastructure failures --- it resets ONLY on intervening // quantifier: census(intervening_health_evidence_resets_the_infra_streak)
  health evidence (a different-class attempt outcome of this derivation:
  a transient build failure or a deadline-class outcome, each proof the
  infrastructure delivered the build to a non-infra verdict), NEVER on // quantifier: census(deterministic_infra_carousel_poisons_at_the_cap_regardless_of_cycle_time)
  elapsed time (the poison cap is denominated in the evidence's own clock
  --- the derivation's conduct --- not the enforcer's wall-window); a
  non-exempt infrastructure failure charges `infra_count` and stamps
  `last_infra_failure_at` (a diagnostic anchor, driving no reset); a
  floor-promoted or CONCURRENT_PUTPATH infrastructure failure charges
  `exempt_infra_count` instead of `infra_count` and neither charges nor
  forgives the streak; the cache-hit and resubmit resets are themselves
  history events that zero the per-cycle counters; and no code path
  mutates a counter outside the fold's event alphabet.
]
The fold lives in the dependency-free `rio-retry-kernel` crate (consumed
through the `rio-scheduler/src/retry_policy.rs` projection shim, whose
hand-computed-history battery is the equivalence oracle); the
model-checked form quantifies over observation orderings and is deferred
to the `retryPolicy.qnt` model. The consecutive denomination appears here
because no other rule states it --- live059-c retired the I-127 300 s
wall-window reset, whose forgiveness keyed on the ENFORCER's clock: any
deterministic failure with cycle time above the window oscillated
`infra_count` 0↔1 forever (`max_infra_retries` unreachable --- the
live_059 carousel, 520 requeues across 128 derivations at INFO-level
silence), while the protection it was minted for (the leaked-PutPath
burst) is exempt-class today. Sparse GENUINE infra failures remain
protected through the health-evidence arm: independent incidents are
separated by the build actually progressing (a different outcome class),
which is the evidence of independence the wall-clock gap only proxied;
`timeout_count` carried this no-elapsed-reset form first and is the
in-tree precedent.

#r("sched.retry.verdict-channel-invariant")[
  For a fixed physical failure history, the budget verdict (requeue,
  poison-on-budget-exhaustion, terminal cancel, or TTL-expire) and the
  counter deltas MUST NOT depend on which observation channel (worker
  completion report, stream disconnect, controller termination report,
  scheduler backstop timer) delivered each physical event or in what order
  the channels delivered them.
]
The pre-Phase-1 code violated this on at least one reachable history,
recorded as divergence D1 of the retry campaign's catalog: the same exhausted timeout
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
run of the same physical history); the retry campaign's catalog records
this as rule-vs-rule contradiction C4 (git history). Phase 1 resolved both as the design
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

#r("sched.retry.recovery-projection+3")[
  After a leader change, each recovered derivation's retry state MUST equal
  the fold of its durable attempt-ledger suffix (the `drv_attempts` rows at
  or after its most recent reset event). `poisoned_at` and the poisoned
  status still come from
  `derivations` (#rref("sched.poison.ttl-persist")), with rows past the
  24 h TTL cleared rather than reloaded; no recovered counter may exceed
  what the durable attempt rows support.
]
This is the Phase-1b recovery contract: the recovered view is the same
fold the live appending transactions compute, so every retry budget,
the 300 s window anchor, and the placement exclusion (including backstop-
and crash-established entries)
survive a leader change per #rref("sched.retry.failover-budget"). The
`+2` revision additionally specified the transitional legacy-column seed
(decision P5: the frozen `derivations.{retry_count, failed_builders,
resubmit_cycles}` mirror columns floored the fold for failure histories
predating the attempt ledger); migration 075 dropped those columns and the
seed machinery with them, so the `+3` revision is the pure ledger fold. The
`+1` revision pinned the pre-ledger selective forgiveness
(4 recovered / 1 derived / 5 defaulted), retired with the Phase-1b
collapse.

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

#r("sched.poison.clear-survivor-reevaluation+2")[
  Both poison-removal paths --- admin `ClearPoison` and the poison-TTL
  sweep --- MUST re-evaluate every surviving parent of the removed child
  after the removal: a `Queued` survivor whose
  remaining dependencies are all satisfied (vacuously so when the removed
  child was its last incomplete dependency) MUST be promoted to `Ready`,
  pushed for dispatch and persisted. A survivor carrying an unresolved
  materialization job needs no extra settling route --- the job's armed
  action covers it (#rref("sched.materialize.settlement")) and the next
  dispatch sweep's probe partition re-classifies the rest. Vanished,
  terminal, and interest-free survivors are skipped.
]
The re-evaluation is what makes the recovery condemnation's co-ownership
scoping (#rref("sched.recovery.failed-dep-cascade")) sound as a pair: a
recovered parent spared by that scoping waits `Queued` above a non-co-owned
within-TTL poisoned child, and the removal of that child --- by the
operator's clear or by TTL expiry --- is its only wake-up edge
(`find_newly_ready` fires on completions, never on removals). Scoping
without re-evaluation reintroduces the build hang the unscoped condemnation
was preventing; re-evaluation without scoping is dead code (the condemnation
leaves no non-terminal survivors). The terminal-build reap's survivor hook
(#rref("sched.merge.substitute-topdown")) runs the same loop at its own call
site.

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

#r("sched.admin.list-executors+3")[
  `AdminService.ListExecutors` MUST return one entry per open pull-mode
  attempt, read from the same durable open-attempt view as
  #rref("sched.admin.list-open-attempts"): the entry's `executor_id` is the
  attempt's executor identity, `busy` is true, `status` is "alive",
  `systems`/`kind` come from the attempt's derivation, and
  `attempt_opened` carries the attempt-open time (the pull) --- set once,
  never advancing mid-build; consumers MUST NOT derive a staleness
  threshold from it (per-pod liveness is the Job/pod phase plus attempt
  age vs deadline, the OA2 alert). The optional `status_filter` matches
  "alive" (or empty) to return the list, any other known historical
  status ("draining"/"degraded"/"connecting") to return an empty list,
  and unknown values leniently (show all). The response's
  `leader_for_secs` keeps the
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

#r("sched.admin.clear-poison+3")[
  `AdminService.ClearPoison` resets both PostgreSQL (`db.clear_poison()`:
  status and `poisoned_at`, joined by the `poison_cleared` ledger reset row
  --- migration 075 dropped the retry-mirror columns, so the attempt ledger
  is the only failure history) and in-memory state (the node is removed
  from the DAG so the next submit re-inserts it fresh); removing the Poisoned
  (by definition un-produced) child MUST run the surviving-parent
  re-evaluation (#rref("sched.poison.clear-survivor-reevaluation")) --- the
  truncation needs no durable breadcrumb, because closure evidence is
  classified from the durable relation at decision time
  (#rref("sched.materialize.routing")). Returns `cleared=true`
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

#r("sched.admin.spawn-intents+2")[
  `AdminService.GetSpawnIntents` returns one `SpawnIntent` per Ready
  derivation, optionally filtered server-side by `{kind, systems, features}`
  and served through a priority-head window: a request with `limit > 0`
  MUST receive the first `limit` intents of the priority-descending order
  with `truncated` set iff the page was cut; `limit == 0` (the proto3
  default and every pre-window client) is unbounded --- the exact
  pre-window behavior. `intent_id == drv_hash`; `(cores, mem_bytes,
  disk_bytes, deadline_secs)` are
  computed by `solve_intent_for` so the controller spawns and the scheduler
  dispatches the SAME shape. `queued_by_system` carries the unfiltered
  FULL-population per-system Ready breakdown (sum ==
  `ClusterStatus.queued_derivations`, window-invariant --- the aggregate is
  the demand truth per #rref("sched.admission.mint-uncapped")) for
  the ComponentScaler's predictive signal.
]

*Retired (1c' spec sweep; machinery deleted by deletion commit A):*
`sched.admin.hung-node-detector`. The scheduler-side hung-node detector
aggregated stale *heartbeats* per node (≥`max(2, ⌈0.5·occupancy⌉)` busy
executors across ≥2 tenants, keyed by the controller-authoritative
`AckSpawnedIntents.bound_intents` binding) and reported the result as the
removed `GetSpawnIntents.dead_nodes`. There are no heartbeats and no
scheduler-side per-pod liveness state left to aggregate: the 1d sweep deleted
the field outright (field 3 is reserved in the proto), and node-wedge
detection moved to the controller ---
#rref("ctrl.nodeclaim.wedge-cluster") clusters expired
open attempts by their controller-authoritative source node, with a
two-derivation floor (tenant-blind --- the successor drops the retired
detector's tenant axis), the per-tick reap cap, and a fail-closed skip when
the open-attempt view cannot be read. `nodeclaim_pool::reap_unhealthy`
consumes that signal as `ReapReason::Dead` exactly as it consumed the
removed `dead_nodes`.

#r("sched.snapshot.binding-presence")[
  `AckSpawnedIntents.binding_snapshot` is presence-preserving: when the
  field is PRESENT the scheduler MUST wholesale-rebuild
  `authoritative_binding` from it --- present-and-empty CLEARS the map (the
  scale-to-zero tick has zero bound pods and says so) --- and when ABSENT
  the scheduler MUST leave the map untouched. The nodeclaim-pool reconciler
  attaches the snapshot on every Ack it sends; per-pool reconcilers never
  do. The legacy non-empty `bound_intents` rebuild remains as a read-side
  back-compat arm for pre-upgrade controllers and is never dual-written by
  a snapshot-capable sender.
]
A bounded rolling-skew window is accepted (new controller + old scheduler:
binding updates pause until both roll); the alternative --- dual-writing
field 5 --- would create a removal obligation later, which the wave's
no-followups directive forbids.

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

#r("sched.merge.wanted-outputs+3")[
  The cache-hit and substitutability classification of a derivation MUST be
  evaluated over its *wanted* outputs only. Every merge MUST durably record,
  per (build, derivation), the union of the output names referenced by any
  consumer's `inputDrvs` entry and the root request's output selection
  (empty = every declared output) in the per-build wanted relation
  (`build_wanted_outputs`), inside the merge transaction; a build's
  re-submission replaces that build's own row and MUST NOT modify any other
  build's row. The *effective* wanted set used for classification is the
  saturating union of the wanted rows of LIVE (non-terminal) interested
  builds, with a live interested build that has no row contributing every
  declared output (the conservative branch); a terminal build's row stops
  counting. The assignment-token output allowlist, the GC pin set, and the
  client-facing output report MUST continue to cover every declared output.
  A wanted set that resolves to no verifiable concrete path MUST take a
  conservative branch --- fall back to the all-declared criterion, or treat
  the derivation as unavailable/unclassifiable --- rather than vacuously
  classifying it as available. The in-memory contribution view is a
  droppable cache of the relation: rebuilt from it at recovery, never
  reconciled, never written back.
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
live asks for --- the incident class that motivated live-scoping. The
same-build replace semantics is the B5 supersession (counter-signed per the
F5/PP-5 calibration record): the durable relation makes the post-failover
effective union exact --- recovery rebuilds the contribution cache from
`build_wanted_outputs`, so a recovered build's width survives the failover.
The conservative no-row branch covers only the pre-relation residue (builds
merged before the relation existed): their width saturates to all-declared
--- wider-or-equal, never narrower --- until they go terminal, observable via
the saturation counter and warn (the recorded D-prime wanted-width
transition residual). The walk-era stored per-node union column (migration
062) and its only-grows fallback semantics were retired with migration 080.

#r("sched.merge.substitute-probe")[
  The merge-time cache check (`check_cached_outputs`) MUST forward the
  submitting session's JWT (`x-rio-tenant-token`) on its `FindMissingPaths`
  store call, and MUST treat paths in the response's `substitutable_paths` as
  cache hits. Without the JWT, the store's per-tenant upstream probe is skipped
  and `substitutable_paths` stays empty --- the scheduler then dispatches
  builds for paths the store could fetch.
]

#r("sched.merge.substitute-probe-indeterminate+2")[
  The store's upstream HEAD probe MUST report paths it could not classify
  (every upstream returned 429 / 5xx / timed out, or the per-call deadline cut
  the pass short) in `FindMissingPathsResponse.indeterminate_paths`, distinct
  from confirmed-miss. The scheduler --- at BOTH the merge-time check and the
  dispatch-time `batch_probe_cached_ready` re-check --- MUST treat
  indeterminate the same as substitutable: create the materialization job
  (the optimistic creation --- indeterminate is never confirmation of a miss)
  and let the job's consumption routing own the outcome
  (#rref("sched.materialize.routing") --- a genuinely missing path resolves
  from-source there). Treating indeterminate as
  confirmed-miss dispatches builders for paths that ARE in cache.nixos.org
  whenever a fresh-wipe burst trips Fastly's edge rate-limit.
]

*Retired (Phase D-prime --- the eager walk-seed fetch):*
`sched.merge.substitute-fetch` required the scheduler to eagerly issue
`QueryPathInfo` for each substitutable-probed path (the walk-seed fetch) so a
derivation was never marked completed against a phantom HEAD hit. The eager
fetch and its caller died with the walk (Wave D3). The obligation's surviving
carrier: a substitutable-probed node is never completed against a HEAD
verdict at all --- it gets a materialization job, the store executor performs
the actual fetch-and-ingest, and only the Success consumption's coverage
re-check completes the node (#rref("sched.materialize.routing"); creation and
the at-most-one-unresolved dedup are #rref("sched.materialize.job")). The
builder-FUSE tenant-context gap the rule documented is likewise owned by the
executor's tenant re-resolution (#rref("store.materialize.executor")).

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
  store, and otherwise given a materialization job created inside the merge
  transaction (#rref("sched.materialize.job")), with `origin = 'pruned'` for
  kept nodes whose dependency closure the prune dropped (a kept node whose
  dependencies are already produced in the DAG, or one with no closure to
  drop, is not pruned-origin) --- the durable record that from-source
  dispatch of this node is doomed, consumed by the settlement arm of
  #rref("sched.materialize.routing"). A later creation that dedups onto this
  node's unresolved job MUST NOT lose the pruned classification (the
  pruned-wins in-place origin upgrade, armament preserved). The scheduler
  MUST fall through to the full merge and the bottom-up
  `check_cached_outputs` when any demanded node's criterion set contains a
  wanted output that is missing and not substitutable, when a demanded
  node's own selector resolves to no declared output, when a criterion set
  resolves to no verifiable path, or on any other uncertainty (store
  unreachable, floating-CA demanded node).
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
exactly this submission's (possibly bogus) selector. The pruned
classification rides the job row inside the merge transaction, so a rolled
back merge leaves neither a job nor a pruned verdict (in-tx atomicity ---
the predecessor mark needed an explicit stamp-after-commit rule for exactly
this; the job row gets it structurally), it survives leader failover with
the row, and it ends with the row's resolution (success, from-source, the
fail-fast, cancellation, obsolescence) --- there is no clear lifecycle. The
fail-fast obligation itself --- a pruned-origin node whose live-wanted
outputs prove unobtainable MUST fail with the resubmit-directing error
rather than a doomed from-source dispatch, across failover --- moved to (and
is normed by) #rref("sched.materialize.routing"): the routing's settlement
arm reads the durable child relation and the job's origin at decision time,
which replaced the predecessor's persisted mark/closure-hole breadcrumb pair
(migrations 063/064, retired by migration 080) and the recovery-time
clear/restore gates that policed them. HISTORY: through Phase D-prime the
walk-era form of this rule additionally normed the `topdown_pruned` stamp
lifecycle, the closure-hole breadcrumb, the verification-walk settlement,
the `substitute_tried` one-shot and the pull-admission refusal; those
mechanisms were deleted (Waves D3-D6) --- admission refusal is the
unresolved-job arm of #rref("sched.materialize.job"), and reap-survivor
re-evaluation is #rref("sched.poison.clear-survivor-reevaluation")'s loop at
the reap site with jobs as the armed action.

*Retired (Phase D-prime --- the closure-hole breadcrumb):*
`sched.evidence.closure-hole` normed the durable breadcrumb (migration 064)
that recorded "an un-produced child was removed out from under a surviving
parent", so a truncated child set could not launder a from-source dispatch
through the produced survivors. The breadcrumb, its four stamping sites, the
recovery restore, the merge-time heal and the Vouched-keyed clears were
deleted (Wave D5; column dropped by migration 080). The hazard it guarded is
owned by durable-relation classification at decision time
(#rref("sched.materialize.routing")): the settlement arm classifies closure
evidence from the persisted edges, the children's persisted statuses AND the
live co-owning build links --- produced children vouched only by terminal
builds classify Broken, so a reap-truncated or stale child set cannot read
as Vouched and no breadcrumb is needed (the relation read is never a
truncated in-memory view).

#r("sched.evidence.durability+4")[
  A durable scheduler write whose loss would leave PG missing state the
  in-memory view relies on MUST be transactional with the state it
  describes: materialization-job creation rides the merge transaction for
  merge-originated classifications --- the row upsert, the edge and
  `build_derivations` inserts, the per-build wanted rows
  (#rref("sched.merge.wanted-outputs")) and the job rows
  (#rref("sched.materialize.job")), with the Pending→Active build activation
  as the final statement --- so a rejected merge leaves no durable trace and
  a committed merge cannot lose one.

  Every durable scheduler write --- the merge transaction (links, edges,
  wanted rows, job rows, activation), every job-table write (creation,
  claim, resolution, park, the pruned-origin upgrade), every wanted-relation
  write, and every derivation-status / poison persistence (the pool-variant
  writers and every transaction that carries their `_in_tx` bodies) --- MUST
  carry the serving generation of the tenure that built the in-memory state
  issuing the write (claimed by #rref("sched.lease.generation-claim") before
  that tenure's recovery writes run) and MUST be applied only if that
  generation is at or above the durable claims floor (`GREATEST` over
  `assignments.generation` and `leader_generation_claims.generation`), read
  on the same connection inside the same transaction as the write; a
  below-floor write MUST roll back having written nothing. A best-effort
  fenced write's ERROR failure is logged and MUST NOT fail the surrounding
  operation --- a Fenced outcome is the fence working on a deposed replica,
  not an error. For the multi-statement merge transaction the floor is
  re-read immediately before commit; the residual window for every fenced
  write is one floor-read-to-commit round trip (a window-narrowing fence,
  not a serializability proof). The comparison is at-or-above (`>=`): a
  write carrying a generation equal to the floor is the same-epoch
  re-acquire keep that #rref("sched.lease.generation-claim") requires, and
  MUST apply. The fence MUST be a capability, not a recipe: decision-state
  write transactions are constructed exclusively through a single fenced
  constructor whose returned transaction handle is the proof the floor
  check ran on that transaction's own connection (the floor-read SQL and
  the comparison are private to the persistence layer), the pre-commit
  re-check is a method of that handle, and the active `assignments` row
  has exactly one closer — exec-scoped, never derivation-keyed — so an
  open-coded floor compare, a forgotten fence, or a deposed replica
  closing a successor's re-minted assignment row is a compile-time error,
  not a review finding.
]
The in-tx-or-fenced split is the load-bearing durability decision, carried
over from the predecessor evidence design: state whose loss would make the
scheduler PERMISSIVE (a job row or wanted row missing while the in-memory
view dispatches against it) rides the transaction that creates the demand;
state whose staleness only makes it CONSERVATIVE may be best-effort fenced.
HISTORY: through Phase D-prime this rule additionally normed the
`topdown_pruned` stamp's in-tx placement, the OR-on-conflict monotonicity of
both evidence columns, and the best-effort clear/heal/hole-stamp statements;
those columns and statements were deleted (Wave D5, migration 080) --- the
pruned verdict now rides the job row in the same transaction, which is the
identical guarantee with no separate stamp to police.

Fencing posture: the uniform claims-floor fence above is normative as of
Phase 1 of the closure-evidence campaign (the 2026-05-30 owner decision,
"fence everything" / design option D15(b)), replacing the original
entry-time-leader-checks-only posture, and Phase D-prime kept it
column-agnostic: job-table and wanted-relation writes feed the same fence
and the same #(refs.metric)("rio_scheduler_evidence_write_fenced_total")
counter. The
deciding evidence was the campaign's stale-tenure model results (the A17
stale-override and A18 deposed-writer probes; archived record:
`docs/spec/models/closure-evidence-records.md`): entry-time gates
cannot close the deposed-believer window --- a replica deposed after a
handler's entry check (or after the SubmitBuild enqueue check, for the
otherwise-ungated MergeDag handler) keeps issuing durable writes for up to
the lease self-fence interval plus in-flight handler work
(#rref("sched.lease.self-fence")), racing the successor's recovery. What the
fence guarantees: a deposed tenure's durable write never lands once a
successor's claim is durable (narrowed to the one floor-read-to-commit round
trip stated in the rule). What it does not guarantee: serializability across
the residual window, and it does not cover work that is not a PG write at
all --- same-epoch in-flight work (required to survive, per the `>=`
comparison) and the documented Lease-deletion-plus-PG-fault conjunction
(#rref("sched.lease.generation-claim") residuals); the model-level oracle is
`fencedJobWritesOnly` (the materialization stale-tenure regime). The
serving-generation capture is tenure-tracking, not per-command: the value is
stamped at the generation claim that precedes the tenure's recovery writes
and is never re-read from the lease atomic mid-tenure, so a new leader's own
recovery writes always pass (the claim made them the floor) while commands
queued under a deposed tenure keep carrying the deposed generation and are
refused.

Accepted residuals, recorded so operators have the recovery answer (in
every shape it is: resubmit the build --- the resubmitting merge re-probes,
re-prunes, or full-merges as appropriate). (a) GC after vouch: outputs that
justified a from-source routing (durable Vouched evidence) are not pinned at
that moment, so the store GC may remove them later; a materialized node's
own outputs ARE pinned at ingest (#rref("sched.materialize.pinning")), and a
completed node whose outputs are lost is re-detected by the stale-completed
verify at the next merge that touches it
(#rref("sched.merge.stale-completed-verify")) or by a dependent's builder
ENOENT and retry. (b) The D-prime transition residuals: a pre-deletion-era
pruned mark without a job row lost its routing effect when the columns
retired --- such a node re-enters via the probe partition and, if its
upstream later vanished, fails as a normal build failure rather than with
the resubmit-directing error (bounded, self-identifying in logs, recovered
by resubmission); and pre-relation builds' effective wanted width saturates
to all-declared post-failover (wider-only; the saturation counter + warn).

*Retired with transfer (Phase D-prime --- the settlement obligation moved):*
`sched.evidence.settlement` required every `topdown_pruned`-marked
Broken-evidence node with live interest to keep a settling step armed
(inline completion, walk, or fail-fast) instead of parking Ready behind the
pull-admission refusal --- the closure-evidence campaign's D16 finding, in
the armed-state form. The mark, the walk, the `substitute_tried` one-shot
and the refusal arm were all deleted (Waves D3-D5), and with them the
present-but-tried limbo cell the rule existed to close (unconstructible
post-deletion). The obligation TRANSFERS to
#rref("sched.materialize.settlement"): every unresolved materialization job
must have an armed action, which subsumes the old rule's subject --- a
pruned-origin node's settlement is its job's claim/consumption/park
lifecycle, and the wrongful-fail-fast bound is carried by the routing's
same-transaction re-probe (#rref("sched.materialize.routing")).

#r("sched.materialize.job+2")[
  The scheduler MUST express "make this derivation's live-wanted outputs
  present in rio-store" as a durable materialization-job row created
  atomically with the classification that demanded it --- inside the merge
  transaction for merge-originated classifications (merge classification,
  prune, stale-Completed verify), through its own claims-floor-fenced
  transaction for the dispatch-probe partition --- with at most one
  unresolved job per derivation (database-enforced), with the creating
  build's tenant recorded, and with every job-table write fenced by the
  durable claims floor. While a derivation carries an unresolved job, pull
  admission MUST NOT mint a from-source build attempt for it (the JobView
  arm: NotYetReady, no attempt row, no status change). A job MUST
  resolve only through: an exec_id-keyed consumption outcome, obsolescence
  on the node producing by other means, or cancellation when no live
  DAG-interested build remains.
]
This is the substitution-replacement campaign's job lifecycle (design §2.1/§6,
adjudications OQ3/OQ6). HISTORY: Phase A landed the mechanism dormant behind a
coexistence flag, Phase B activated it at the deployment layer, and Phase
D-prime deleted the flag and the predecessor walk --- the job is the only
substitution mechanism and creation is unconditional. The
per-(build, derivation) wanted relation (`build_wanted_outputs`) is written by
every merge for every pair, and materialization interest is
always DERIVED from it by a live-build join --- never registered separately
(review finding AS-1). The dedup mirrors the C3 protection: N builds requesting
the same derivation concurrently produce one job, enforced by the
`materialization_jobs_unresolved` partial-unique index instead of by in-memory
status checks; a pruned merge deduping onto the existing unresolved job
upgrades its origin in place (pruned-wins,
#rref("sched.merge.substitute-topdown")). The unresolved-job admission refusal
is the successor of the walk-era must-substitute pull interlock (kind-aware:
materialization-kind claims are the one exception,
#rref("sched.state.machine")).

#r("sched.materialize.claim-resume")[
  Re-delivery of an open materialization attempt MUST require that the
  pulling identity holds the attempt AND that the pull presents a matching
  re-delivery credential --- the original delivery's exec id
  (`resume_exec_id`) OR the claim's persisted nonce
  (`PullAssignmentRequest.claim_nonce` matching `assignments.claim_nonce`)
  --- where an absent credential matches nothing (absent-presented against
  absent-persisted MUST refuse); the nonce MUST be minted client-side
  before the pull is sent, persisted with the assignment at mint, and
  recovered across leader failover; credential-less or mismatched
  same-identity re-pulls MUST be answered NotYetReady and settle through
  the establishment window.
]
The rule-4b amendment (the pull-contract amendment anchor below, SIGNED
2026-06-04): the exec-id resume token travels only on the RESPONSE, so the one
failure mode re-delivery exists for --- the lost response --- was exactly the
case the token could never cover (bug_251). The client-chosen nonce closes it:
minted BEFORE the pull, it survives response loss by construction; the store
worker's bounded resume ledger re-pulls directly (a minted attempt leaves the
claimable listing forever, so re-listing can never recover it). Crashed
workers (ledger lost with the process) still settle through the charged
establishment window --- the signed residual, now reachable only by real
crashes. Build-kind re-delivery stays credential-less (as-built).

*The pull-contract amendment anchor (rule 4 / rule 4b).* Non-normative
record. Amendment status for the frozen pull-contract addendum's rule 4
lives at exactly THIS anchor (the bug_109 single-record discipline, enforced
by the `amendment-status-coherence` check); every other site points here
instead of restating the state. Relocated verbatim from the retired executor
invariant map's rule-4 anchor block (owner directive 2026-06-12, content
unmodified); the wire-delta rows the amendments ride are carried by the
`build_types.proto` field comments (`resume_exec_id = 5`, `claim_nonce = 6`,
`Aborted = 5`), and the original block is in git history.

*Rule-4 amendment* (§4.4 item 7, the follow-up-ledger row-4 / 2026-06-02
batch procedure; never fabricated): rule 4's re-delivery clause is amended
from "the kernel's open-attempt arm re-delivers to the same composite
identity" to "re-delivers to the same composite identity PRESENTING the
original exec_id resume token (`resume_exec_id`)". Tokenless or mismatched
same-identity re-pulls answer `NotYetReady` and settle through the
establishment window. This is a BEHAVIOR CHANGE for senders without the new
field --- identity agreement alone no longer resumes --- and is therefore
NOT covered by extends-never-modifies; it was recorded as the contract's own
amendment and counter-signed below. Rationale: identity agreement is
forgeable (merged_bug_158 --- a sanitize-fold collision or a restarted pod
re-pulls under the same composite identity without ever having held the
attempt); the token is known only to the puller the original
`WorkAssignment` answered. Pinned:
`check_materialization_redelivery_requires_resume_token` + the widened
`check_kinded_one_winner_arbitration` (CBMC, both re-proven over the widened
domain, 13/13), the kinded unit table, and the failover re-delivery test
(the holder now presents its token; the tokenless same-identity re-pull is
asserted `NotYetReady`).

Owner counter-signature for the rule-4 amendment: SIGNED 2026-06-04
(collected at the bughunt-wave close-out --- the wave's final owner act).
Checked at signing: the resume-token arm is landed and kani-proven both
directions (evidence-kernel 19/19 at the final recount); the
colliding-identity calibration falsifies `atMostOneClaimWinner`; the
failover test pins the token-presenting semantics; the wire-delta table is
complete (second-lander A4: `Aborted=5`, `store_degraded=7`,
`resume_exec_id=5`). The amendment's behavior change for tokenless senders
is accepted as the contract's own term.

*Rule-4b amendment --- the claim-nonce credential* (bughunt2 wave, bug_251;
recorded at this single anchor): rule 4's re-delivery clause is amended from
"re-delivers to the same composite identity PRESENTING the original exec_id
resume token" to the credential disjunction: materialization re-delivery
requires `held_by_puller` AND (`resume_exec_id` match OR persisted
`claim_nonce` match); credential-less same-identity re-pulls remain
`NotYetReady`. The wire delta is `PullAssignmentRequest.claim_nonce = 6`
(client-chosen v4, minted BEFORE the pull rides the wire, persisted at mint
--- `assignments.claim_nonce`, migration 096 --- and recovered across
failover by the recovery join). Rationale: the resume token travels only on
the RESPONSE, so the one failure mode re-delivery exists for (the lost
response) is exactly where no client can hold the token; the nonce survives
the loss by construction. The establishment-window settlement remains the
posture for credential-less senders --- now reachable only by real crashes
(process-lost ledger), not by every lost response. The nonce leg matches
only with BOTH sides present (absent-vs-absent refuses --- the
Option-equality trap is centralized in the kernel's
`redelivery_credential_ok`).

Owner counter-signature for the rule-4b amendment: SIGNED 2026-06-04 (the
§5-S R14 signature packet, collected in-conversation before any bughunt2
worktree branched; transcribed per R14 --- that round's directive is the
recorded authorization).

_Amendment note, 2026-06-07_ (bughunt-3 S5, recorded at this anchor per R3):
four repairs to the close's own client loop --- answered permanent refusals
resolve the ledger entry (bug_119: no immortal entries; the lost-response
lane is reserved for unanswered pulls); the ledger is the sole fresh-mint
authority (merged_bug_096: a live credential is never clobbered, and
fresh-claim `NotYetReady` KEEPS the credential --- the post-mint TOCTOU arm
answers `NotYetReady` after persisting the nonce); the claim pass is
single-exit (bug_116) and budgeted by potential mints (bug_099). None alters
the signed re-delivery clause above; the signed residual is STRENGTHENED ---
the charged establishment window is now reachable only by real crashes, no
longer by the loop's own credential destruction. Modeled as three
claim-plane laws in `openAttempts.qnt` (`noCredentialClobber`,
`noRefusalFiledAsLost`, `confirmNeverMints`), each with a falsify twin.

_Amendment note, 2026-06-08_ (bughunt-4 S5a, recorded at this anchor per the
same R3 precedent): the bug_119 disposition above is NARROWED on the resume
arm (merged_bug_074) --- only MINT-DISPROVING refusals
(InvalidArgument/Unimplemented: the request shape cannot mint) resolve
the ledger entry; auth-layer codes (PermissionDenied/Unauthenticated --- the
scheduler's rotation-skew trace, emitted without consulting attempt state)
file as Unanswered, because a RESUME entry exists precisely where the
ORIGINAL unanswered pull may have committed a mint and the auth answer
judges only the presentation. The fresh arm is unchanged (its gates run
pre-mint, so either flavor disproves a mint there). This restores the
2026-06-07 note's own claim --- the charged establishment window stays
"reachable only by real crashes, not by the loop's own credential
destruction" --- which the recorded bug_119 letter violated exactly during
fleet HMAC rotations. Companion budget repair (merged_bug_072): the
fresh-claim budget derives from the surviving ledger population (claimed +
ledger length at or above slots) and the mint authority REFUSES at capacity
--- eviction of live rule-4b credentials is gone. The SIGNED re-delivery
clause above is unaltered; the claim-resume rule's text is untouched (no
tracey bump arises). Modeled in `openAttempts.qnt`: `answeredRefusalSeat`
and `noRefusalFiledAsLost` re-scoped to the mint-disproving reading with the
existing refusal-as-lost twin re-measured; NEW `authRefusalSeat` + live
`claimRefusedAuthSkew` with the rotation-skew twin targeting
`noFaultNeverCharged`; NEW `openAttemptsBudget` module
(`outstandingBounded`) with the per-pass-overmint twin. Pinned:
`check_materialization_redelivery_requires_credential` (REPLACES --- widens
--- `check_materialization_redelivery_requires_resume_token` over the
(resume OR nonce) domain; CBMC, both directions, the credential-less refusal
preserved as the nonce-less slice) + the widened
`check_kinded_one_winner_arbitration` (the `DeliverExisting` branch now
proves a credential matched), the kinded unit table
(`lost_response_nonce_resumes_claim`; the disjunction delivers past a wrong
token; `colliding_identity_fresh_claim_gets_not_yet_ready` extended ---
nonce agreement never overrides the one-winner refusal), the failover
re-delivery test (`flag_on_recovery_rebuilds_job_view_and_jobs_survive`:
wrong-nonce refused; right-nonce tokenless re-pull re-delivers the same
attempt across failover --- the #(refs.migration)("096_assignments_claim_nonce")
persistence pin), the store
client battery (`timeout_then_resume_recovers_lost_response`,
`resume_ledger_lifecycle`), and the `mat-158-colliding-identity` calibration
(header re-pointed to the credential rule; `atMostOneClaimWinner` still
falsifies --- identity agreement alone remains forgeable, which is exactly
what the credential gate refuses).

#r("sched.materialize.routing+7")[
  A materialization outcome MUST be consumed in exactly one fenced transaction
  keyed by its exec_id, and that transaction MUST re-read live interest and the
  live effective wanted set before acting: a Success outcome completes the node
  only when the reported paths cover the re-read live wanted set (else the job
  re-arms); a RetryLater outcome (raced placeholder / upstream rate-limit)
  MUST close the attempt with no ledger row of any class and defer the job's
  next claim through a view-only deferral that pull admission reads but the
  park decision, the park re-evaluation, and the stalled gauge never do — a
  rate-limit wave must not walk a healthy job toward the from-source
  settlement; an Unobtainable outcome MUST route through, in order: the
  moot-failure arm (no live-wanted path is missing AND no reference path is
  confirmed missing → never fail-fast; a confirmed reference miss is a
  closure hole and never completes — reports from executors predating the
  reference cell partition unattributable missing entries, those outside the
  expected ∪ carried ∪ live-wanted sets, into it), the
  durable-Vouched arm (declared closure produced → from-source), the
  durable-Pending arm (deps still buildable → from-source via normal gating),
  and only then --- after a same-transaction store re-probe of the live wanted
  set confirms a live-wanted path missing-and-unsubstitutable under EVERY
  live interested tenant (the re-probe asks once per live tenant; the
  confirmed-missing verdict is the all-tenant conjunction over a non-empty
  answer set, any failed or indeterminate tenant view re-arms instead ---
  the job fails only when NO interested tenant can obtain), or after the
  per-job re-probe one-shot is spent --- the settlement arm. When the
  outcome carries a typed refusal (`Unobtainable.refusal`, the CLOSED
  `UnobtainableRefusal` alphabet: trust --- present upstream but refused
  by signature policy; content --- present upstream but claiming bytes
  that disagree with the stored row; both; or an unrecognized wire value
  --- a future axis that MUST decode from the raw value, never through
  an accessor that defaults unknowns to the clean lane, and MUST route
  conservatively as a refusal), the settlement MUST consume the alphabet
  typed, end-to-end and match it exhaustively: an Obtainable re-probe
  answer MUST NOT license a re-arm (the re-probe is a presence-blind
  HEAD --- it confirms the presence that was never in question, not
  trust and not content agreement; the consumption does not issue the
  doomed probe round-trip at all) and the verdict MUST NOT be the
  fail-fast even for a pruned origin (the resubmit-directing error sends
  the user into the same deterministic refusal, unbounded) --- a refused
  settlement with anything missing resolves from-source, whatever the
  refusal axis. Otherwise the settlement MUST
  discriminate on the job's pruned origin (`origin = 'pruned'`, set at pruned
  creation or by the pruned-wins dedup upgrade and read from the job row at
  decision time): a PRUNED-origin job fail-fasts every live
  DAG-interested build with the resubmit-directing error (the prune
  deliberately dropped its closure --- from-source is doomed); a non-pruned
  job MUST instead resolve from-source (the predecessor walk never
  fail-fasted unmarked nodes, whatever their evidence --- the recorded
  equivalence, Phase B finding 11, preserved through the deletion). The
  durable closure evidence consulted by the Vouched/Pending arms MUST be
  classified from the persisted relation at decision time --- the declared
  edges, the children's persisted statuses AND a live co-owning build link
  for every produced child (produced children vouched only by terminal
  builds classify Broken --- stale evidence never launders a from-source
  dispatch). An InfraFailure
  outcome MUST never fail-fast and never route from-source; it re-arms the job
  within the materialization budget and parks it on exhaustion. The same
  per-tenant discipline governs the dispatch-time batch probe that routes
  Ready nodes: presence and substitutability are asked once per live
  interested tenant, inline completion requires present-and-visible under
  EVERY interested tenant, and a materialization job is created when every
  wanted path is obtainable under SOME tenant (a failed tenant probe drops
  out of both folds; all probes failing keeps the fail-open dispatch shape).
]
The four-arm routing is the C3-settlement successor (design §2.4; review
findings AS-2/PP-1/PP-3/BC-5). The same-transaction re-probe preserves CE-D4's
recorded contract ("every fail-fast decision point re-probes live obtainability
first") at the single surviving fail-fast site. The pruned-origin
discriminator preserves the predecessor reachability fact: in the walk era
only a marked (pruned) node could reach the resubmit-directing fail-fast, so
the settlement arm reserves the same verdict for pruned-origin jobs (the
C3-class equivalence divergence; Phase B finding 11) --- the origin is the
durable carrier the deleted mark column used to be (Wave D2.1). The
three-part live-links criterion is the F9-class guard (Wave D2.2): PG retains
a terminal build's completed children indefinitely, so without live-build
scoping a previous-generation child set would classify Vouched and launder a
stale closure into a doomed dispatch. Genuine leaves whose evidence is Broken
by structure (childless) release to from-source dispatch and the build
attempt proceeds. The Success coverage re-check
closes the CE-17 class (interest grew between execution and consumption). The
park-not-fail posture preserves B3 ("unknown never demotes"): infra evidence is
never confirmation. Parking is *visible and alertable* --- the
#(refs.metric)("rio_scheduler_materialization_stalled") gauge
(#rref("obs.metric.materialization-stalled")) counts currently-parked jobs from
ground truth at every housekeeping tick --- and *re-evaluable*: the same tick's
re-evaluation arm resolves a parked job from-source the moment its node's
durable closure evidence reads Vouched or Pending (the arm-1/arm-2 disposition
applied outside a consumption), so a dead upstream can only ever stall nodes
with no buildable dependency closure; those stay parked, alertable, and
re-claimable at backoff expiry. (PD-20, discharged in Phase B.)

#r("sched.materialize.reprobe-per-path")[
  The settlement-arm re-probe's confirmed-missing verdict MUST be computed at
  per-(tenant, path) granularity with the quantifier order ∃ path ∀ tenant: a
  fail-fast requires SOME live-wanted path that is missing-and-unsubstitutable
  -and-determinate under EVERY live interested tenant, over a non-empty tenant
  set in which every tenant's probe carried confirming identity. The caller
  MUST pass raw per-path membership cells to the fold --- a per-tenant
  pre-projection (any boolean computed per tenant before the cross-tenant
  fold) is not a valid input, since it erases the path axis the quantifier
  ranges over. Conservative rows MUST fold to obtainable: an empty tenant
  set, any tenant unable to confirm, any indeterminate cell on the candidate
  path, and any malformed (ragged) answer matrix.
]
The quantifier order is the substance (bug_299): the pre-fix fold consumed one
pre-projected boolean per tenant --- each tenant's "∃ path missing under me"
--- which computes ∀ tenant ∃ path. Complementary coverage (tenant A's
upstreams carry X but not Y, tenant B's carry Y but not X) then folded to
confirmed-missing and fail-fasted a job every path of which was obtainable
under SOME tenant --- precisely the owner-Q2 contract the all-tenant
conjunction was built to protect. The path axis must survive to the fold for
the conjunction to mean what the routing rule says it means.

#r("sched.materialize.settlement")[
  Every unresolved materialization job MUST have an armed action: a pending
  unparked job is claimable by any store replica; a claimed job settles
  through outcome report or, on executor crash, the establishment sweep
  (materialization-infra charged, never adopted, never parked at
  establishment); a parked job is covered by the per-tick re-evaluation (a
  Vouched/Pending-evidence park resolves from-source the moment its durable
  closure evidence allows) and by backoff expiry (a Broken-evidence park ---
  the stalled population --- un-parks for another claim cycle without
  resetting the budget counter); and a job whose derivation has no live
  DAG-interested build left is cancelled charge-free. This MUST hold across
  leader failover: recovery rebuilds the job view faithfully (claim holders
  and park expiries mirrored), so no unresolved job is left with no armed
  action by a failover.
]
The armed-action totality is the transferred settlement obligation
(predecessor: the walk-era `sched.evidence.settlement`, retired above ---
its present-but-tried limbo cell is unconstructible post-deletion, and the
job lifecycle's totality is the form the obligation takes when every
substitution is a durable job). The stalled population is deliberately NOT
an exception: parked Broken-evidence jobs stay visible
(#(refs.metric)("rio_scheduler_materialization_stalled"), the MD-D1 alert)
and re-claimable at expiry, so "armed" includes "alertably waiting with a
wake-up edge" --- never "parked Ready with no action", which was the D16
defect class. Model-level verification: `unresolvedJobAlwaysArmed` (view
faithfulness, all five materialization regimes) with the F10/B5(a)
calibration pins as its falsifiability pair; production verification: the
armed-action totality test and the failover VM scenario.

#r("sched.materialize.view-settlement")[
  The in-memory materialization job view is a droppable cache of the durable
  job table, and every per-entry view REMOVAL MUST derive from the settled
  disposition of the durable write it mirrors: a removal is authorized only
  when the fenced resolution applied or was already applied
  (Applied/AlreadyResolved); a Fenced or Failed durable write MUST keep the
  view entry (a deposed believer mutates nothing it no longer owns; a failed
  write's armed action stays level-triggered through the next tick). Every
  companion action of a settlement --- requeue, completion batch, fail-fast,
  re-arm, conversion accounting --- MUST gate on the same disposition. The
  zero-interest cancellation MUST be total over the DAG-absent arm: it
  resolves the job and closes its open materialization-kind attempt in one
  fenced transaction keyed entirely on durable state, never on an in-memory
  exec_id its own trigger arm guarantees may be absent.
]
The structural carrier is the `JobView` wrapper: `remove_settled` is the
only per-entry removal in the type (whole-view `wipe`/`rebuild` belong to
LeaderLost and recovery), so an unconditional removal does not typecheck ---
the discipline is compile-borne, not reviewed-in. Model-level verification:
`viewMatchesDurableUnresolved` and `chargeFreeCancellation`
(materializationJob.qnt, the resolve-faults regime) with the
mat-133-discarded-outcome / mat-276-dag-absent-cancel calibration pins as
the falsifiability pair.

#r("sched.materialize.ack-law")[
  The report intake's answer for one materialization consumption MUST be a
  pure function of the close write's disposition: settled
  (Applied/AlreadyResolved) and Fenced closes acknowledge; a Failed close
  MUST refuse retryably (UNAVAILABLE) so the store's bounded report
  redelivery re-presents the same outcome --- an acknowledged consumption
  whose close never became durable is unrepresentable. When a post-close
  companion write (job resolve, park verdict) fails after a settled close,
  the claim MUST be released uncharged (claimable-but-unparked dominates
  wedged-claimed-forever); a fenced companion mutates nothing it no longer
  owns.
]
Both laws are pure kernel functions (`rio-evidence-kernel/src/settle.rs`:
`consumption_ack`, `companion_follow_up`), CBMC-swept and wired through the
sealed witness pipeline: `close_for_consumption` is the only constructor of
the linear `SettledClose` witness, the five settlement companions are the
only spenders, and the `MatAck` the intake returns is mintable only by those
companions or the fenced close arm --- an ack with the assignment still open
does not typecheck. `Fenced ⇒ Ack` is the signed Q20 posture (deposed
believers ack; the successor's establishment owns the row). The residual:
a deposed-but-serving replica swallows a report and the successor charges
'unreported' ~an hour later --- the population is shrunk by the claim nonce
(#rref("sched.materialize.claim-resume")), and the NACK alternative remains
a one-line change to the Fenced arm of `consumption_ack`, recorded there.

#r("sched.materialize.claim-coherence")[
  Every materialization claim release MUST be a compare-and-clear that
  names the executor it acts for: a release whose named executor no
  longer holds the claim MUST clear nothing and MUST NOT requeue the
  node (it belongs to the new attempt). A view entry observed CLAIMED
  with no open materialization assignment in two consecutive
  housekeeping snapshots MUST be repaired by an uncharged release
  through that same compare-and-clear path.
]
The claim fields are private to the view entry: mint
(strike-resetting), compare-and-clear release, and the two-strike ghost
flag are its only mutators, so an unconditional holder strip cannot be
reintroduced outside the law. The two-strike guard is clock-free --- a
claim minted between the housekeeping rows snapshot and the view
iteration gets one full sweep to appear backed before it can be called
a ghost --- and the repair lane deliberately carries no fatal assert
(the ghost has live producers: crash windows between close-commit and
view update).

#r("sched.materialize.claimability-projection")[
  Pull admission, the claimable-backlog gauge, and the leader job
  listing MUST read one shared four-way claimability classification of
  the job-view entry --- claimed, parked, deferred, claimable-now, in
  that dominance order --- and the listing MUST answer only
  view-tracked claimable-now jobs, answering empty under an
  Unavailable view, so every listed job is admittable by construction.
]
The listing over-fetches the durable query (bounded at
`min(2×limit, 512)`; partitioned callers draw the full 512-row
head-window --- the partition domain must not shrink with one caller's
limit) because the durable axis cannot see view-only armament: the
transient deferral has no column by design (merged_bug_178), and a
fresh claim can commit between the query and the reply. This rule does
NOT re-key #rref("sched.materialize.view-settlement"): entry removal
still gates exclusively on the durable write's settled disposition ---
the privacy change moves reads behind the classification, not the
removal discipline.

#r("sched.materialize.listing-distribution")[
  The leader job listing MUST partition the claimable head-window
  across the live store-worker membership by rendezvous hashing over
  (job, worker member) --- the member unit is the per-worker composite
  identity carried by the verified store-service credential's instance
  claim, the membership is the set of members whose last
  identity-bearing listing call is within the membership TTL, and ONE
  owner function is the single partition source --- and MUST serve each
  identity-bearing caller exactly its owner slice united with the steal
  horizon (jobs whose owner has not listed within the staleness
  horizon), with no wire-visible distinction between the segments; an
  instance-less caller or an empty membership MUST be served the
  unpartitioned listing.
]
The live_041 convoy (2026-06-09): every replica polled the same
deterministic `ORDER BY created_at` head --- N workers raced the head
job, one won, N-1 burned their pass; the fleet advanced at most one
listing window per round regardless of replica count, and KEDA
scale-out during a stall ADDED RACERS (live: 95.6% of claim attempts
wasted; a 29.1 s fleet-wide zero-throughput stall). Identity rides the
verified service-token claims (T-5.1) --- zero wire change; the steal
horizon is computed server-side at the same site from the same
membership map (RULED CF-3 --- no client steal lane, no lane flag), so
duplication is bounded by owner staleness, never asserted away:
exactly-one-LISTING is deliberately NOT a property of this rule (a
stale owner's slice is legitimately double-listed until it returns;
claims still arbitrate one-winner). The member unit is the per-WORKER
`{pod}-w{n}` composite, never the pod (RULED CF-2: `with_worker`'s
sanitize-salt fallback makes suffix-stripping unreliable, so no pod
aggregation exists anywhere); a replica scale event is a batch
join/leave of `executor_concurrency` members. Machine witness:
`docs/spec/models/materializationDistribution.qnt` (owner-map
partition, own-slice disjointness/coverage, staleness-bounded steal,
no-job-unlisted, orphan recovery; convoy and no-steal falsify twins).

#r("sched.materialize.listing-cost+2")[
  The leader listing chokepoint MUST NOT recompute the rendezvous
  partition per poll: partition-scoring work MUST be zero on a poll
  in a stable membership epoch over a warm head-window snapshot,
  scoring MUST be bounded by one window rescore per membership event
  (a join costs at most one score per cached head-window job; an
  owner's leave re-scores only the departed owner's jobs over the
  survivors), and the head-window query MUST run at most once per
  snapshot TTL or job-creation dirty event. The pacing charge MUST be
  per attempt: a failed or slow head-window query charges the same
  envelope, with the pacing anchor sampled at attempt completion, so
  neither failure nor latency re-opens per-poll querying; a failed
  attempt MUST also consume the job-creation dirty event it was
  fired for.
]
The cost law is live_041's missing witness shape (bug_045): the
serving-correctness parity witness passed every gate while the
chokepoint did O(window x members) SipHashes plus a 512-row fetch per
poll per worker --- ~M polls/s with per-worker membership M ~ 1384 at
fleet scale, all serialized on the single-threaded actor turn with
pull mints, completions, and heartbeats (the actor-blocking class the
repo already documents as a bug). The rule's witnesses are OPERATION
COUNTERS minted inside the operations themselves (scores inside the
single scoring source; fetches at the sole head-window call site;
member touches at the membership choke sites) --- wall-clock
witnesses are banned here because the claim is complexity, not
latency. Serving semantics are pinned byte-identical by the
#rref("sched.materialize.listing-distribution") parity and coverage
witnesses; the snapshot TTL (1 s) sits at the worker poll cadence and
far below every lapse scale that feeds the window, and claimability
is still filtered per poll from the live view
(#rref("sched.materialize.claimability-projection") is untouched ---
only park-LAPSE entry into the window waits out the TTL; job creation
rides the view's dirty edge instead).

#r("sched.materialize.conversion-strictness")[
  The parked-job re-evaluation MUST support two independently default-off
  strictness fields on the `[materialization]` config surface ---
  `conversion_requires_worker_charge` (refuse the from-source conversion
  unless worker-reported `materialization_infra` charges alone exhaust
  `max_attempts`; Scheduler-party establishment charges keep counting toward
  PARKING, never toward conversion authorization) and
  `conversion_min_park_dwell_secs` (refuse conversion until the configured
  dwell has elapsed since the job's MOST RECENT park began --- a re-park
  restarts the clock; the anchor is the durable
  `materialization_jobs.park_began_at`, so the dwell is failover-exact).
  With both fields at their defaults the re-evaluation MUST be
  byte-identical to the unconditional form: the park predicate, the
  party-blind budget fold, and the stalled-gauge definition are never
  re-keyed by the knob. A refused job MUST stay parked --- counted by the
  stalled gauge, armed via park-expiry re-claim, accruing further
  worker-reported charges across claim cycles --- and MUST convert at the
  first re-evaluation after its strictness conditions clear.
]

The knob is the Item T strictness half (harden-store reconciliation memo
§6.2(b)); the observability half (the conversion counter and the
#(refs.alert)("RioSchedulerMaterializationConversions") alert) landed first, and flipping
either field's default ON in deployment is an operational act gated on that
alert's evidence (owner ruling 2026-06-02). Knob-ON defers --- never
forecloses --- the settlement rule's "resolves from-source the moment its
durable closure evidence allows" arm: the deferred job remains armed
through park-expiry re-claim, so the armed-action totality above is
unchanged.

#r("sched.materialize.pinning")[
  Every store path a materialization job ingests or verifies present MUST be
  pinned against garbage collection at ingest time, under a pin kind
  distinguishable from build-input pins; materialization pins MUST be released
  only when the job is terminally resolved AND no live DAG-interested build
  remains for the derivation; and no build-input pin lifecycle (terminal-status
  release, recovery sweep) may delete a materialization pin.
]
Pin-at-ingest closes the GC-after-vouch window (B2-strong) for the window
ingest → all-interest-terminal (design §5; adjudication OQ5; review finding
PP-2). The kind discrimination exists because the as-built release machinery
(`unpin_live_inputs` on terminal status, `sweep_stale_live_pins` at recovery)
fires at exactly the wrong time for materialization output pins --- its premise
"terminal drv ⇒ inputs no longer in use" is true for build-input pins and false
for materialization output pins.

#r("sched.dispatch.probe-budget")[
  Every actor-side store-probe sweep (the dispatch-time
  `batch_probe_cached_ready` fan-out and the materialization reprobe)
  MUST be priced by ONE `AttemptBudget` of a single `grpc_timeout`:
  per-tenant attempts are clamped to the budget's remainder, an expired
  budget short-circuits the remaining tenants into the same
  dropped-from-fold (or answer-poisoned) arm as a per-tenant failure,
  and the worst-case actor stall is one timeout regardless of tenant
  count.
]

The pre-budget shape awaited each tenant sequentially under a full
`grpc_timeout` — T hung tenants stalled the single-threaded DAG actor
T x 30 s, unbounded in tenant count; heartbeats and dispatch stalled
behind it. Any future partitioning of the probe groups inherits the
bound by construction because the budget, not the loop shape, owns the
clock.

#r("sched.dispatch.fod-substitute+3")[
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
  A missing-but-obtainable (substitutable or indeterminate) node MUST get a
  materialization job (#rref("sched.materialize.job")) --- never an inline
  completion against the HEAD verdict; only locally-present outputs complete
  inline. The store executor performs the actual fetch with the tenant
  context it re-resolves itself (#rref("store.materialize.executor")) ---
  builders' subsequent `GetPath` calls have no tenant context, so the lazy
  `try_substitute_on_miss` cannot fire there.
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

#r("sched.merge.reconcile-order+2")[
  In `reconcile_merged_state`, all dep-state corrections (cache-hit→Completed,
  stale-Completed reset, the reprobe-Poisoned reset --- `poison_cleared`
  ledger row plus the dep-derived status, the AS-5 6d slot) MUST complete
  before any dependent-verdict computation (reprobe-unlocked Queued→Ready,
  `seed_initial_states`). A reprobed previously-Poisoned node MUST be reset
  out of Poisoned before `seed_initial_states` reads
  `any_co_owned_dep_terminally_failed` for its dependents.
]

#r("sched.admin.snapshot-substituting+4")[
  `ClusterStatus` MUST report `substituting_derivations` (wire-stable field
  name): the count of derivations carrying a CLAIMABLE materialization job
  --- unclaimed, not parked, not deferred:
  #rref("sched.materialize.claimability-projection")'s three axes. Parked
  jobs MUST remain visible via
  #(refs.metric)("rio_scheduler_materialization_stalled");
  deferred jobs (`defer_until` --- the bounded <=300s re-probe window) are
  counted in NEITHER gauge for that window. The snapshot match over
  `DerivationStatus` MUST be exhaustive so future status additions are
  compile-time caught, not silently-zero. Job-counted derivations MUST be
  excluded from `queued_derivations` and `queued_by_system` --- the buckets
  stay disjoint, and their sum never reads zero mid-cascade.
]

*Retired (Phase D-prime --- the detached substitution walk):*
`sched.substitute.detached+5` normed the walk: the `Substituting` status, the
detached BFS over the reference closure (per-path `QueryPathInfo` against the
store), the per-path retry ladder, the unwanted-seed forgiveness and its
chain-scoped never-forgive set, the `SubstituteComplete{ok}` completion
protocol with the `substitute_tried` one-shot fall-through, and the
recovery-time Substituting reset. The entire mechanism was deleted (Waves
D3-D6; the status left the persisted alphabet with migration 080). The job
IS the successor (#rref("sched.materialize.job")): the closure walk runs
inside the store executor against its own substitution machinery
(#rref("store.materialize.executor")), completion is the exec_id-keyed
consumption with its coverage re-check, the one-shot's role is the routing's
per-job re-probe one-shot, and "never Completed against a phantom HEAD hit"
is owned by the Success coverage arm (#rref("sched.materialize.routing")).
Forgiveness has no successor --- the executor ingests the live wanted set
path by path and coverage is re-read at consumption, so there is no
forgiven-set state to police (the C-prime F6 by-construction record).

*Retired (Phase D-prime --- the walk fan-out bound):*
`sched.substitute.fanout-bound` bounded the scheduler's in-flight detached
walk tasks (`RIO_SUBSTITUTE_MAX_CONCURRENT`, scheduler memory only). The walk
and its task pool were deleted (Waves D3-D4; the knob and its env var died
with them --- operator-visible, recorded in the D3 commit). The surviving
store protection is per-replica admission
(#rref("store.substitute.admission")), which the rule already disclaimed as
the real bound; the materialization executor's fetch concurrency is
store-side configuration (#rref("store.materialize.executor")).

#r("sched.admin.spawn-intents.probed-gate+3")[
  `compute_spawn_intents` MUST NOT emit a SpawnIntent for a Ready derivation
  whose `probed_generation == 0`, when a store client is configured AND the
  derivation's `expected_output_paths` are all known
  (`DerivationState::output_paths_probeable`).
  A materialization success's consumption promotes dependents Queued→Ready
  with their dispatch-time substitute probe deferred to the next Tick; a
  `GetSpawnIntents` poll landing in that ≤1s window would otherwise
  spawn pods for derivations that the next probe finds substitutable, which
  `reap_stale_for_intents` then deletes 10s later. With
  #rref("sched.substitute.eager-probe") the merge-time probe covers the whole
  submission, so the layer-by-layer cascade is no longer the primary case; the
  gate still covers dependents promoted by a materialized intermediate that
  was NOT in the original probe_set. `queued_by_system` is intentionally NOT
  gated (it must match `ClusterSnapshot.queued_by_system`). With no store
  client (test-only), `batch_probe_cached_ready` early-returns without
  stamping; the gate is moot and disabled.
]
The spawn-intent producer additionally filters intents whose node carries an
unresolved materialization job (the PD-7 job filter, model-verified by the
spawn-coherence mat-jobs regime): a spawned pod's pull would be refused while
the job is unresolved (#rref("sched.materialize.job")), so the filter removes
the refusal/spawn churn the walk-era gate accepted.

*Retired (Phase D-prime --- the inline completion cascade):*
`sched.dispatch.substitute-complete-inline+2` required
`handle_substitute_complete{ok=true}` to run the Ready-set store
short-circuit inline in the same handler so substitution cascades probed
dependents immediately instead of one Tick per layer. The handler died with
the walk consumption (Wave D3.2). Surviving carriers of the load-bearing
content: dependents of a materialized node are promoted by the success
consumption's completion cascade and probed by the dispatch sweep's probe
partition (#rref("sched.materialize.routing")), and the spawn-intent gate
(#rref("sched.admin.spawn-intents.probed-gate")) keeps the not-yet-probed
window spawn-correct exactly as before --- the cascade-layer latency the rule
existed to bound is owned by the merge-time eager probe
(#rref("sched.substitute.eager-probe")), which probes the whole submission up
front.

*Retired (Phase D-prime --- the walk completion's leader gate):*
`sched.substitute.leader-gate` dropped `SubstituteComplete` on standby
replicas so a deposed tenure's surviving walk task could not split-brain
`derivations.status` against the new leader's recovery. The command and the
walk died (Wave D3.2). Cross-tenure staleness for the surviving mechanism is
owned by exec_id identity plus fenced consumption: a materialization outcome
is consumed in exactly one claims-floor-fenced transaction keyed by its
exec_id (#rref("sched.materialize.routing")), so a deposed tenure's report
is refused by the fence rather than by a mailbox gate (the model's
fencedJobWritesOnly oracle and the F11 calibration pin).

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

Dispatch-order preemption is priority-based:
- Spawn intents are computed each pass over `Ready` nodes in critical-path
  priority order (highest first): higher-priority derivations are bound to
  executors ahead of lower-priority queued (not yet running) work. The
  queue-era interactive boost was retired with the spawn-intent ordering;
  interactive builds compete on critical-path priority.
- _Executor-slot reservation (priority lanes holding a fraction of executors
  for high-priority work) is not implemented. Priority ordering plus
  autoscaling is the current mitigation for starvation._
- Autoscaling is the primary mitigation for all-executors-busy scenarios.

= Derivation State Machine

#r("sched.state.machine+2")[
  Each derivation node in the global DAG follows a strict state machine. All
  transitions are performed inside the DAG actor to ensure serialized access.
  The transition table is kind-blind with exactly one kinded exception:
  materialization-kind pull mints may additionally take `Queued → Assigned`
  (materialization does not wait for dependencies --- the store fetches from
  upstream, so dependency state is irrelevant to the claim). Build-kind mints
  and every non-mint transition use the kind-blind table unchanged.
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
    [Derivation re-enters `Ready` (pull-claimable). See `running → failed` above.],

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

#r("sched.state.terminal-idempotent+2")[
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
    `queued`/`completed` when a merge-time re-probe finds the
    output present or substitutable (I-094: present completes via the
    cache-hit lane; substitutable resets to `queued` with a reprobe-origin
    materialization job --- the AS-5 reset; `failed` is non-terminal so
    technically not a carve-out --- listed for symmetry with the reprobe
    lane)
]

#r("sched.state.poisoned-ttl")[
  The `poisoned → created` transition is gated by a 24h TTL.
]
The poison-TTL sweep that performs the expiry removes the expired node from
the DAG the same way the admin clear does, and runs the same surviving-parent
re-evaluation (#rref("sched.poison.clear-survivor-reevaluation")), exactly as
for #rref("sched.admin.clear-poison").

#r("sched.merge.poisoned-resubmit-bounded+4")[
  When a build merges and finds a pre-existing `poisoned` node in the global
  DAG, the node resets for re-dispatch (same as
  `cancelled`/`failed`/`dependency_failed`) iff its `resubmit_cycles` is below
  `POISON_RESUBMIT_RETRY_LIMIT` (2 cycles). An explicit client re-submission is
  treated as retry intent --- the operator presumably fixed the underlying
  cause --- but bounded so a genuinely-broken derivation cannot loop forever.
  `resubmit_cycles` is incremented on each reset and persisted durably --- the
  `resubmit_reset` attempt-ledger row appended for the reset carries the new
  cycle index --- so the bound accumulates across re-submissions
  and survives scheduler restart. The reset gives the node a fresh per-cycle
  `retry_count = 0` (full `max_retries` budget). At or above the limit the
  node stays `poisoned` and the build fail-fasts (use the 24h TTL or
  `ClearPoison` admin RPC to override).
]
The `+4` revision dropped the frozen `derivations.resubmit_cycles` mirror
column from the durability clause: migration 075 removed the column, and the
attempt-ledger reset row has been the only carrier of the cycle index since
the Phase-1b cutover froze the column.

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

#r("sched.merge.stale-substitutable+3")[
  The stale-completed `FindMissingPaths` is sent with the build's tenant token
  so the store reports `substitutable_paths`. A Completed node with a
  missing-but-substitutable live-wanted output MUST be reset and given a
  `stale_reset`-origin materialization job in the same verify (the node
  re-enters Queued; the executor re-fetches); only outputs that are missing
  AND not obtainable leave the node on the from-source reset
  (#rref("sched.merge.stale-completed-verify")). When the reset destroys
  realized floating-CA output paths (the node's `expected_output_paths`
  slots are the `""` placeholder), the job MUST carry them
  (`materialization_jobs.carried_realized_paths` --- a creation-time
  snapshot of the immutable realized paths, written only by the
  `stale_reset` origin; the wanted NAME set stays live), the executor's
  wanted resolution MUST union the carried paths into its seed set, and
  the success-consumption coverage MUST include them --- scoped to
  carried-path presence, never a general "empty coverage never
  completes" rule, which would collide with the conservative-absent
  all-declared saturation (for floating-CA that saturates back to the
  placeholder and re-opens the same hole).
]

The carrier scope is deliberate: without it, post-GC re-submissions
re-dispatch the entire subtree --- including FOD sources whose origin URLs
may be dead --- for paths cache.nixos.org already has, and a floating-CA
stale reset "re-completes" vacuously with the `""` placeholder, never
re-fetching the realized path (GC retention dropped, clients handed `""`).

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

#r("sched.watch.snapshot-first")[
  Every `WatchBuild` stream MUST begin with a `BuildSnapshot` message
  describing the build's current state — build state, absolute aggregate
  counts, the per-derivation running set with `exec_id`s, and (for terminal
  builds) the outcome payload — computed atomically with the broadcast
  subscription, so the events that follow the snapshot are exactly the
  events emitted after it.
]
A watcher attaching to an already-terminal build learns the outcome from the
snapshot alone; a watcher attaching mid-build reconstructs display state from
the running set instead of from replayed history. This is the whole
reconnect contract: there are no sequence numbers, no persisted event
mirror, and no replay — the WatchBuild resumability layer was deleted in
favor of this snapshot (its `build_event_log` table, per-build sequence
counters, and `since_sequence` replay are gone). `SubmitBuild` streams carry
no snapshot: their receivers are registered before the build's first event
is emitted, so there is no missed state to summarize.

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

#r("sched.build.terminal-status-settled+3")[
  Once a build reaches a terminal state, its externally served progress and
  outcome are settled: no further `BuildProgress` event may be emitted for
  it, its served progress accounting (`cached_derivations`) MUST NOT be
  mutated, and a later failure of a shared derivation MUST NOT rewrite its
  settled error summary or re-run its per-build failure handling ---
  aggregate fan-outs (dispatch-time store hits, completion release, failure
  cascades) MUST skip interested builds that are already terminal. The
  settled surface MUST be a payload captured AT the terminal transition
  (counts, outcome arm, output paths / first failure / cancel reason), and
  every consumer --- the live terminal event, the `WatchBuild` snapshot,
  `QueryBuildStatus`, and the persisted row --- MUST serve that one capture,
  never a recomputation from live state.
]

#r("sched.build.terminal-payload-captured")[
  The terminal payload MUST be capturable only together with the terminal
  transition: marking a build terminal without its settled counts and
  outcome payload, marking it `cancelled` without a reason, or recording a
  build-level failure that names a culprit derivation MUST NOT be
  representable in the scheduler's build-state API.
]
The payload travels as one structure (`SettledBuild`); the failure trio
(summary, culprit, classification) is one struct written through first-wins
or whole-struct-override setters, so partial writes that pair a stale
culprit with a new summary are unrepresentable.

#r("sched.watch.terminal-from-durable-row+2")[
  A `WatchBuild` for a build the actor no longer holds MUST be answered
  from the durable `builds` row when that row records a terminal state: one
  synthesized terminal `BuildSnapshot` carrying the persisted verdict
  (state, settled counts, outcome payload). The durable-row fetch MUST be
  tenant-bound by the caller's attested identity (dev mode binds none): a
  foreign tenant's terminal row is absent, so the caller receives the same
  `NotFound` as for a build Postgres does not know terminal. `NotFound` is
  reserved for builds Postgres does not know terminal for this caller.
]
The terminal arm of the status UPDATE persists the whole settled payload
atomically with the status flip (migration 087), so the synthesized
snapshot is never a half-written verdict. Pre-087 terminal rows degrade to
an empty payload with the correct state. This deliberately upgrades the old
in-memory-only failure-trio posture: post-cleanup and post-failover
watchers get the recorded verdict instead of the gateway's
reconnect-exhaustion fabrication.

#r("sched.pull.kinded-running-surface")[
  The running surface MUST be kinded: the work class of every open
  attempt is captured at the single mint site (with the execution id,
  cleared in lockstep with it, recovered across failover from the
  execution row), the per-derivation running set carries it on the wire,
  display routing for both the live mint event and the snapshot running
  set MUST go through the single kind-to-surface projection, and the
  aggregate running counts MUST count build-class work only.
]
A materialization-claimed node is upstream-fetch activity: listing it as a
running build gave re-attaching watchers phantom builds (and tail
subscriptions against executions that never log). The wire field degrades
to the build display for senders that predate it; the count exclusion is
the owner's display contract (decision Q10, 2026-06-03).
Terminal builds stay resident --- and re-subscribable via `WatchBuild` ---
for the terminal-cleanup window while the global DAG keeps evolving for
other builds that share their nodes (a stale-Completed reset, a re-dispatch,
a dispatch-time store hit). A `BuildProgress` recomputed from that
still-mutating DAG and emitted after `BuildCompleted` would reach live
watchers --- and a re-attaching watcher's snapshot
(#rref("sched.watch.snapshot-first")) --- with totals shrunk by whatever
mutated the DAG since; a late shared-node failure routed through the
per-build failure handler would overwrite the settled error summary of a
build that already succeeded. Per-derivation events (`DerivationCached`,
`DerivationFailed`) still flow to a resident terminal build's channel ---
they are facts about the derivation, not aggregate progress of the finished
build.

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
ledger existed (migration 065 ships no backfill) has no claim rows at all ---
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

#r("sched.recovery.step-down+3")[
  A tenure whose state recovery fails MUST NOT be completed: the replica
  MUST NOT mark recovery complete, MUST NOT treat its in-memory DAG as
  authoritative, and MUST request a cooperative lease step-down so a healthy
  replica can acquire. The step-down request MUST carry the tenure INSTANCE
  that issued it --- a monotone per-acquire stamp never reused across
  re-acquires; the lease transition count is NOT a tenure-instance
  identifier, because a self-fence false alarm followed by a same-epoch
  re-acquire legitimately repeats it. The lease loop MUST serve the request
  at its next BELIEVING tick and only while the issuing instance is still
  current: service releases the lease (holder-guarded, bounded by the renew
  deadline) and fires the full lose-edge effects --- leader-state clear,
  consumer on-lose hook, leader-marks reconciliation --- before candidacy
  resumes on the following tick. A tick on which the replica does not
  believe it leads MUST leave the request armed; the acquire and rebound
  EDGES MUST clear any pending request --- a new tenure instance starts
  clean, and a recovery that fails again under the successor re-files with
  the successor's own stamp; and a request whose instance is no longer
  current at service time MUST be dropped, never served against the
  successor tenure --- the successor never asked to step down.
  The durable generation claim recorded for
  the failed tenure is NOT released: the floor only grows, and an unserved
  claim is a harmless over-claim. Failed recoveries MUST be operator-visible
  --- the recovery-failure outcome alertable and every step-down counted.
]
Failure mode under a persistent PG outage: acquire → recovery fails →
step-down → re-acquire cycles at lease cadence across the replica set. This
is deliberate --- each cycle re-probes PG from a fresh tenure, the monotone
floor growth is harmless, and the cycling is exactly what the step-down
counter and the recovery-failure alert make operator-visible; a bounded
retry-before-step-down knob was considered and deliberately not taken
(per-replica retries add zombie window without adding information --- the
next acquire IS the retry). Non-K8s single-scheduler deployments run no
lease loop, so the request is a recorded dead letter there: the tenure
stays incomplete (dispatch remains gated) and the operator signal is the
same failure counter; there is no healthy peer a step-down could yield to.

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
protocol described here has no scheduler side left: the removed `BuildExecution`/
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
stale-epoch calibration witness and the frozen as-built model
(`executorSessionAsBuilt.qnt`) it ran against were deleted by the
2026-05-29 as-built retirement; git history (the retiring commits and the
Stage-C tables they carry) remains the historical evidence.

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

// TODO: verify-repoint follow-up (recorded at the executor campaign
// close-out): the stream-era tests carrying this rule's verify
// markers were deleted with the machinery and the 1c' sweep
// re-pointed only the impl markers; re-point the verify coverage at
// the surviving spawn-side kind/arch batteries on the next touch of
// this rule (tracey's untested query surfaces it meanwhile).
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

// TODO: verify-repoint follow-up (recorded at the executor campaign
// close-out): the stream-era tests carrying this rule's verify
// markers were deleted with the machinery and the 1c' sweep
// re-pointed only the impl markers; re-point the verify coverage at
// the surviving uncharged-requeue batteries on the next touch of
// this rule (tracey's untested query surfaces it meanwhile).
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
code): the rule retires with the machinery, and the contradiction record stays
as history in the retired campaign records (git history).

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

#r("sched.timeout.per-build+2")[
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
  the primary semantics). Zero means no overall timeout. Wire-supplied
  timeout seconds (`build_timeout`, `max_silent_time`) MUST saturate at the
  shared one-year absurdity ceiling (`rio_common::clamped::WireSecs`) at
  ingestion --- at the scheduler's tenant seam and again at the builder's
  assignment seam; zero stays no-timeout, and a saturated value is
  effectively unbounded but arithmetic-safe (no `Instant + Duration`
  overflow on tenant input).
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
minted by the pull transaction --- the only `drv_executions` writer; the
former `dispatch_mode` coexistence discriminator is dropped (migration 076):
the durable open-attempt row is the scheduler's only per-executor state, the
establishment sweep is its only time-based repair, and the operator surfaces
project that row set (#rref("sched.admin.list-open-attempts"),
#rref("sched.admin.list-executors")).

#r("sched.executor.pull-transaction+2")[
  `PullAssignment(executor_token, intent_id)` MUST be leader-served and MUST
  perform its work as one atomic transaction: validate the token↔intent
  binding (#rref("sec.executor.identity-token") applied per-unary), resolve
  the derivation by intent id, transition it out of Ready, mint `exec_id`,
  insert the `drv_executions` row (with `source_node` when known), write or
  refresh the active `assignments` row carrying the serving generation, pin
  GC live-inputs, and commit only if the serving generation is not below
  the durable claims floor (GREATEST over `leader_generation_claims` and
  `assignments`); a below-floor serving generation MUST abort the
  transaction with no row written and return the same retryable not-leader
  error `ensure_leader` produces. A re-pull while the attempt is open and
  bound to the same pulling identity MUST return the identical payload and
  `exec_id` without writing anything.
]
The fence is transaction-side (the worker-side generation latch has no
distribution channel without the stream); the payload carries no generation
at all --- `WorkAssignment.generation` was removed outright, field 7
reserved. The two-believer pull race (two open
attempts, double charge for one pod death) is closed at the same place the
work-binding authority lives.

#r("sched.executor.pull-gone+1")[
  `PullAssignment` MUST return `Gone` when the derivation is no longer
  wanted by anyone: cancelled, substituted, completed, skipped, failed
  permanently/poisoned, or absent from the DAG. `Gone` MUST NOT write any
  attempt or derivation state; on the keyed build lane the confirm-exit
  fence row is the one permitted write and MUST be durable before the
  answer (#rref("sched.executor.confirm-fence")). `Gone` is terminal for
  the pod: it exits 0, the Job completes, and nothing is charged.
]

#r("sched.executor.pull-not-ready+2")[
  `PullAssignment` MUST return `NotYetReady{retry_after_seconds}` --- never
  `Gone`, never another attempt's payload, and never a write --- when the
  derivation is still wanted but not currently deliverable to the pulling
  pod: its dependencies are not yet built (forecast-spawned pod arrived
  early), it is being substituted, it is awaiting retry, or it is currently
  open/Assigned/Running on a different executor (an open attempt bound to
  another pod). The pod re-pulls after the suggested delay and exits 0
  charge-free if it has received only `NotYetReady` for its idle-timeout
  bound.
]
This is the OA6(a) decision: returning `Gone` for a wanted-but-not-Ready
derivation would produce a reap→respawn→Gone churn loop (the controller's
stale-intent reap deletes the terminal Job for a still-wanted intent every
tick), and delivering while an attempt is open elsewhere would re-point the
active assignment away from the executor actually building it.

#r("sched.executor.confirm-fence")[
  On the keyed build lane (an HMAC-verified executor credential), every
  `PullAssignment` answer that licenses the builder's exit 0 --- `Gone`,
  live or confirm-only, and the confirm-only `NotYetReady` --- MUST have
  the confirm-fence row durable BEFORE the reply is sent (write-ahead; a
  failed fence write withholds the license with a retryable rejection),
  `DeliverNew` MUST screen against the fence before minting, and the
  fence key MUST derive only from the carrier bytes the credential layer
  verified --- never from an unverified present carrier.
]
The fence is what makes a licensed clean exit terminal for the token: a
straggler pull (still in the mailbox or network, or re-sent after a
content-addressed resubmit re-readies the same drv) finds the fence and is
screened to `Gone` instead of minting an attempt no sweep can see. Keying on
unverified carrier bytes would let an untrusted worker de-key its own fence
write (garbage metadata + a valid body token authenticates as the body
identity) and dodge the screen.

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

#r("sched.attempt.synthesized-verdict+3")[
  A controller-synthesized terminal report (reason cancelled, preempted, or
  reaped) MUST name the attempt by `exec_id`: the scheduler refuses
  (acknowledges charge-free, resolves nothing) a synthesized verdict that
  arrives intent- or job-keyed only --- intent resolution is
  newest-open-wins, and a sticky disruption re-fire or stale verdict would
  close a newer attempt the controller never observed. An exec-pinned
  synthesized report for an open pull-mode BUILD attempt that has no
  worker-reported classification row MUST close that attempt charge-free in
  one
  generation-fenced appending transaction --- exactly one uncharged terminal
  row whose `termination_reason` carries the synthesized reason, with the
  assignment row closed --- and MUST requeue a still-wanted derivation at
  that fold, never at the establishment sweep. A worker `ReportOutcome`
  whose result is `Cancelled` for a still-wanted open pull-mode build
  attempt (the AD5 SIGTERM-abort report) MUST resolve the same way ---
  charge-free closure and requeue, never an infrastructure-failure charge
  --- subject to the worker-abort bound
  (#rref("sched.attempt.worker-abort-bounded")). A MATERIALIZATION attempt
  is outside both paths: a controller verdict for one MUST be acknowledged
  charge-free with nothing written --- the consumption transaction stays
  the attempt's only consumer. Neither close path may requeue a derivation
  that is no longer wanted, and other pod-terminal reasons without a worker
  classification remain the establishment sweep's to classify. The AD2c
  source-node fill runs iff the report resolved as a BUILD witness AND
  named the execution directly (exec-resolved) --- an intent-resolved
  report never stamps a node onto an execution row.
]
The synthesized-verdict close is the scheduler half of the AD5/C5/C6
successor (`ctrl.job.synthesize-on-delete`, `ctrl.drain.disruption-target`):
the controller's deletion destroys the only pod-terminal status the unified
report could otherwise fold, so the synthesized report itself must carry the
closure. Pod-initiated aborts of still-wanted work are platform terminations
(preemption, scale-down, controller deletes), not worker faults --- charging
them as infrastructure failures would burn the infra budget on disruptions
the design accepts as charge-free.

#r("sched.attempt.worker-abort-bounded+2")[
  The worker-abort charge-free admission MUST be ledger-bounded: a worker
  `ReportOutcome{Cancelled}` for a still-wanted open build attempt is
  admitted charge-free only while the worker-abort count within the
  attempt history's trailing bounded-uncharged run is strictly below
  `WORKER_ABORT_FREE_CLOSES` (3); a report arriving at or past the bound
  MUST be consumed as a charged infrastructure failure through the
  existing unsolicited-Cancelled classification, advancing the exclusion
  and poison budgets. Materialization-lane rows MUST neither extend nor
  break the run (the kind partition); a sibling bounded-uncharged row
  (any other `BOUNDED_UNCHARGED`-registry class --- the store-degraded
  paced write) EXTENDS the run without advancing this count; any other
  build-lane row breaks it.
]
The worker supplies the `Cancelled` discriminator, so trusting it
unboundedly mints unlimited uncharged requeues (a compromised or looping
builder pins its derivation forever without ever advancing exclusion or
poison --- bug_279's class). The bound preserves the AD5 posture for every
plausible disruption burst --- three platform terminations within one
bounded-uncharged run --- while making the loop finite; the run resets on
any genuine classification, reset, or controller observation, and the
per-class count composes with the sibling uncharged class instead of
being reset by it (bug_098: alternating the two classes is bounded by
the SUM of the per-class headrooms, never unbounded).

#r("sched.attempt.establishment-window+6")[
  The establishment sweep MUST visit every open attempt (active assignment ⋈
  execution, no terminal classification) on every sweep, and MUST establish
  an attempt only after its window has elapsed with no terminal row. For an
  attempt carrying a witnessed-terminal mark
  (#rref("sched.attempt.witnessed-terminal")) the window is the WITNESSED
  clock --- `witnessed_at` plus the configured `establishment_report_slack`
  --- in place of the deadline anchor; for every unmarked attempt the window
  is its deadline plus the same slack, where the deadline is anchored to the
  value the attempt was dispatched with (the solved deadline persisted by
  the pull mint): a sweep-time re-solve may widen an unmarked attempt's
  window but MUST never shrink it below the dispatched deadline while the
  attempt is open. Every expired attempt MUST be dispositioned
  through the single total establishment kernel
  (`establish_expired_attempt`): the store-probe arm adopts the attempt as
  completed when its outputs are verifiably present, stamping EXACTLY the
  verified wanted subset the probe witnessed (the kernel's
  `VerifiedPresent` carrier) and never an unverified expected-paths
  superset; a node already settled terminal
  (completed/poisoned/dependency-failed/skipped) MUST close charge-free —
  no attempt row, no exclusion seed, no establishment metric — the work's
  verdict already exists and is never re-litigated; a live-wanted build
  attempt otherwise establishes exactly one executor-crash/unreported
  classification (charged per the existing C2 discipline) and requeues the
  derivation. The node axis MUST be projected through the kernel's total
  authority-aware projection (`project_node`): while the in-memory DAG is
  not authoritative (pre-recovery, failed recovery), no destructive
  housekeeping — establishment, GC, orphan-cancel, timeout-cancel — may
  run at all, and in particular not-in-the-DAG MUST NOT be read as
  absent. Establishment MUST never fire inside the window, and the
  establishing transaction MUST apply the same generation-floor fence as
  the pull transaction.
]

#r("sched.attempt.witnessed-terminal")[
  The `ReportAttemptOutcome` intake MUST record an in-memory
  witnessed-terminal mark `(exec_id → witnessed_at, witnessed_reason)` for
  every pod-terminal letter that resolves to an open, unclassified build
  attempt, idempotently under the controller's level-triggered re-reports:
  the FIRST witnessing report anchors `witnessed_at`, and a re-report MUST
  NOT advance it (it re-creates an absent mark and otherwise changes
  nothing); the intake itself MUST NOT bump a floor, consume budget, or
  insert rows on the mark path. The establishment sweep MUST expire a
  marked attempt on the witnessed clock (`witnessed_at +
  establishment_report_slack`) without waiting for the dispatch-deadline
  anchor, and at establishment MUST feed the witnessed reason through the
  per-reason disposition table: witnessed `OOMKilled` --- the per-container
  kubelet `containerStatuses` attribution --- is the ONLY promoting letter
  (`bump_resource_floor`, label `witnessed_oom`), gated on the
  establishment transaction's append+decide `won` flag so promotion fires
  at most once per attempt, ever; EVERY other witnessed letter (both
  `EvictedDiskPressure` message shapes included --- the controller folds
  node-condition and pod-attributed evictions into that one letter) MUST
  establish classify-only, leaving the floor untouched. Marks MUST be
  consumed at establishment and pruned against the open-attempt view.
]
The promotion narrowing is the I-199 non-recreation argument: the retired
heuristic's over-fire shape was N pods #sym.times M level-triggered
re-reports promoting N #sym.times M times on ambient signals; this law caps
promotion at once-per-attempt (re-reports refresh nothing; the `won` flag
is the durable dedup) AND restricts the promotion set to the single
per-container-attributed letter, so ambient/node-cause signals sit
structurally outside it on both axes (population and rate). The mark is
in-memory by design --- lost on scheduler failover, re-armed by the next
level-triggered re-report while the pod stays listable; beyond that window
the deadline anchor backstops, so degradation is bounded by the prior
behavior (the priced residual; the durable-column arm is the recorded
rejected alternative under the wave's single-DDL allocation).

#r("sched.config.slack-floor")[
  `establishment_report_slack_secs` MUST be validated at config load against
  the shared floor `rio_common::limits::MIN_ESTABLISHMENT_REPORT_SLACK_SECS`:
  a value below the floor MUST fail scheduler startup. The floor exists for
  the controller's wedge clustering --- its observation grace plus two
  reconcile ticks must fit inside the slack so deadline-expired attempts
  remain observable in the open view before the sweep establishes them; the
  controller compile-time-asserts the same inequality against the same
  constant.
]
The sweep reads durable rows --- not an in-memory claim a one-shot timer can
forget --- so the post-failover "deferred claim forgotten" defect class is
closed structurally. Anchoring the window to the dispatched deadline (072's
`deadline_secs`) keeps a fitted estimate or hw-table change that shrinks
mid-flight from establishing a healthy attempt that is still inside the
deadline its pod really runs under; the residual gap between the Job's
`activeDeadlineSeconds` render and the mint-time solve is covered by the
report slack. The kernel routing makes the decision axes explicit; the
cancelled/absent-node row is normative below.

#r("sched.attempt.cancel-close-driven+3")[
  A cancel-driven attempt close MUST be driven to durability: when the
  status persist that closes the attempt's assignment row fails, the
  scheduler MUST latch the batch — together with the affected
  derivations' ACTIVE exec_ids read from the in-memory DAG at failure
  time — in a leader-scoped outbox and retry it on the housekeeping
  tick; the outbox MUST be cleared on leadership loss. Latching is
  per-derivation newest-wins: the single latch chokepoint MUST strip
  each newly latched derivation from every queued batch, so at most
  one pending status exists per derivation queue-wide (stripped
  batches keep their exec_ids — the close is exec-scoped and
  unconditional, and an emptied batch still flushes close-only). The
  retry is a REPLAY, not a repeat: before re-driving, each latched
  derivation MUST be re-derived against the authoritative in-memory
  DAG — kept when its node still carries the latched status or has
  left the DAG, DROPPED when the node is present with a different
  status (a resubmit reset or later transition made the latch stale;
  replaying it would regress newer state) — and the replay's
  assignment close MUST be scoped to the latched exec_ids (never the
  derivation), so an attempt minted after the latch is untouchable by
  construction. The replay's precedence cut MUST be anchored on
  `status_changed_at` — a column whose only writers are status events
  (migrations 101+102 — the BEFORE UPDATE trigger is the stamp's
  single authority; a status-preserving write MUST NOT refuse a
  latched persist) — with the age sampled at the replay transaction
  boundary, so the realized cut can only trail the enqueue instant (refuse,
  never overwrite), and the replay MUST NOT re-stamp a row already at
  the target status. Each zero-row residual MUST be classified at the
  durability point, in the replay transaction: already-applied (the
  durable status equals the latched truth — a lost-ack retry or an
  equivalent landed write, never counted as a refusal), refused-newer
  (evidenced foreign precedence — the row stands with a different
  status; the ONLY lane that may warn and count a refusal), or
  vanished (the row was GC'd; nothing stands). On a healthy persist
  the flush MUST drain the entire outbox in the same tick (fail-fast
  applies to failures only — one attempt per tick on a dead PG, never a
  one-batch-per-tick trickle on a healed one). And the establishment
  sweep MUST close an expired attempt whose node is cancelled or absent
  from the DAG charge-free: the assignment row closes, no attempt row
  is appended, no exclusion is seeded, and no establishment metric
  increments.
]
The two halves are one liveness property (the `openAttempts.qnt` model's
`openAttemptHasDriver`): an open attempt for cancelled work always has a
driver --- the outbox retry while this leader lives, the charge-free sweep
arm after failover (the new leader's recovery rebuilds no interest for the
cancelled node, so the sweep sees `Absent`). Without the outbox, one failed
persist left the assignment open forever on the happy path; without the
charge-free arm, the sweep then established it as `executor_crash` ---
seeding the exclusion ledger and the OA2 wedge clustering with verdicts
about work nobody wanted.

#r("sched.admin.list-open-attempts+4")[
  `AdminService.ListOpenAttempts` MUST return every open attempt --- an
  active `assignments` row joined to its `drv_executions` row with no
  terminal `drv_attempts` fill. Each entry carries the intent id (drv
  hash), derivation path, `exec_id`, executor identity, source node when
  known, the assignment's generation, its age, its work class
  (`attempt_kind`; the unspecified value reads as build for rolling skew),
  and the dispatched deadline persisted by the pull mint (`deadline_secs`;
  0 = unknown, and consumers MUST treat 0 as not-expirable); the response
  carries `leader_for_secs` with the same fail-closed freshness semantics as
  #rref("sched.admin.list-executors-leader-age+2"), and `recently_closed`
  --- every BUILD-lane attempt whose assignment reached a terminal status
  within the recent window (120s), each entry carrying its close cause
  (`completed`/`failed`/`cancelled`) so consumers select on CAUSE rather
  than re-inferring it from the absence of an open row. The window MUST
  carry only closes whose execution row witnesses
  `attempt_kind = 'build'` (the controller cancel arm's teardown target
  is a builder Job; a materialization close is store-side work, and an
  assignment with no execution row has an unknowable kind --- both are
  denied by default). The RPC is leader-served.
]
The same view feeds the #(refs.metric)("rio_scheduler_open_attempts") gauge
(the busy-fleet gauge; the stream fleet's `workers_active` is retired) and
the establishment sweep, and backs the re-implemented
#rref("sched.admin.list-executors+3") projection.

= Admission

The round-9 Banner-A laws. The live_053 freeze measured the failure
shape they close: a 134.65s Tick (one 16.6s unbatched cancel sweep +
unattributed remainder), admin mints delivered 18s late against a 5s
caller deadline, depth-based backpressure silent at 1--12.8% capacity
through total time-starvation, and a 379-call/12min unpaginated
intent-poll plane. Admission is therefore denominated in
*work-per-turn* --- per-cycle quotas with carried remainders, cost-priced
shedding, windowed serving --- never in raw queue depth or producer caps.

#r("sched.admission.work-per-turn")[
  Every scheduler-side consumer of an unbounded demand population MUST
  bound its work per cycle by a typed quota and carry the unserved
  remainder to later cycles, never re-granting it within the same
  cycle; and admission pressure MUST price projected work-cost (queue
  depth × observed per-turn cost), not queue depth alone.
]
Worked instances: the dispatch-probe tick quota
(`DISPATCH_PROBE_TICK_QUOTA` --- per-generation ledger, oldest-first
next-tick carry), the admin fast lane's per-visit drain quota
(`ADMIN_FAST_LANE_DRAIN_QUOTA` --- also the biased-select fairness cap),
the batched zero-interest cancel sweep (one fenced statement regardless
of population --- the B1 interstitial), and the cost-axis backpressure
signal (projected drain = depth × per-turn EWMA against the 30s/10s
drain bounds, #rref("sched.backpressure.hysteresis+3")).

#r("sched.admission.mint-uncapped")[
  The demand record is the demand truth: the scheduler MUST NOT cap the
  spawn-intent mint or the aggregate demand signals (`queued_by_system`,
  the ice-masked cells); bounds live on the consumer's served slice ---
  the priority-head window of #rref("sched.admin.spawn-intents+2") ---
  never on the mint.
]
Capping the mint is the anti-shape: a capped demand record under-feeds
every downstream sizing decision (the controller's cover deficit, the
ComponentScaler's predictive signal, the KEDA backlog gauge) exactly
when demand is highest, and the consumers cannot detect the
shortfall --- a truncation FLAG on a windowed page is honest, a silently
clipped aggregate is not.

#r("sched.admission.leading-signal-clamped")[
  A leading capacity signal MAY lead demand; its ceiling MUST be
  hostability-clamped (the demand-derived arm taken `min` against what
  the fleet can actually host) and its per-period scale commitment
  bounded.
]
Landed instances (the B-1 interstitial): xtask's `derive_store_ceiling`
takes `min(pg_arm, hostable_arm)` and logs the binding arm at boot ---
the PG-only ceiling once let KEDA commit 4→173 replicas in 75s against
a pool hosting 46 (live_052); and the store ScaledObject's scaleUp
policy bounds per-period commitment (Pods 16/30s, stabilization window
0s preserved) --- the chart half carries no tracey markers (YAML is
unscannable), so it is bound by the helm fragment test
`nix/tests/helm/26-store-scaling.sh` and cited here.

= Backpressure

The scheduler applies @backpressure at multiple layers to prevent overload:

*Pull admission:* work delivery is pod-initiated --- the scheduler pushes
nothing, so there is no per-executor send window to manage. A pull or report
that cannot be served (overload, failover, recovery) surfaces as a retried
unary on the pod side; the actor never queues undelivered work for a slow
consumer.

#r("sched.backpressure.hysteresis+3")[
  *Actor queue pressure:* The DAG actor's `mpsc` channel has a fixed
  capacity (`ACTOR_CHANNEL_CAPACITY` = 10,000 messages; compile-time
  constant). Backpressure MUST engage when EITHER axis is high: queue
  depth at/above 80% of capacity, OR projected drain time (queue depth ×
  the observed per-turn work-cost EWMA) at/above the 30s drain budget ---
  depth is a drain-time proxy only under uniform turn costs, and the
  live_053 turns spanned five orders of magnitude (one 140s command at
  1--12.8% depth starved every queued caller with silent watermarks).
  While engaged:
  + New `SubmitBuild` requests from the gateway receive gRPC
    `RESOURCE_EXHAUSTED` status.
  + The scheduler increments the
    #(refs.metric)("rio_scheduler_queue_backpressure") counter for alerting
    (the projected drain itself is continuously visible on
    #(refs.metric)("rio_scheduler_backpressure_projected_drain_seconds")).
  + Pull-mode report intake keeps using `send_unchecked` --- a completion
    report must never be dropped (a lost completion would leave the
    derivation stuck `Running`); the pod's bounded retry is the relief
    valve, never discard.
  Normal processing resumes only when BOTH axes are low --- depth at/below
  60% AND projected drain at/below 10s (joint hysteresis: a one-axis
  release would re-admit work the other axis is still drowning under).
  (The stream-era intake arms this rule used to
  enumerate --- blocking the removed `BuildExecution` reader on completions and
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

#r("sched.db.derivations-gc+4")[
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
  unboundedly --- I-169.2 observed 1.16M rows. The sweep is coupled to the
  closure-evidence lifecycle: the durable-relation classifier
  (#rref("sched.materialize.routing")) reads the persisted edges, child
  statuses and live co-owning `build_derivations` links at decision time ---
  the no-`build_derivations`-link victim filter MUST be retained, because the
  link is what keeps a child row readable by that classifier until its
  builds rows are deleted, which no in-tree path does for a build that merely
  ran to terminal (the failed-merge rollback is the only production deleter
  of `builds` rows); erasing classification evidence therefore requires an
  external builds-row purge in addition to this sweep, and a truncated row
  set classifies Broken (conservative), never Vouched.
]

#r("sched.db.table-retention+1")[
  Every public scheduler-owned table MUST have a declared row lifecycle in
  `rio-migrations/src/retention.rs` (`RETENTION_REGISTRY`), as a TYPED
  policy: a named sweeper (`SweptBy` --- the symbol MUST define in
  non-test workspace source and a defining file MUST carry the deleting
  statement), a parent cascade (`CascadeFrom` --- the named migration
  MUST carry the `REFERENCES parent … ON DELETE CASCADE` clause; RESTRICT
  does not satisfy), or a written keep-forever rationale (which MAY
  record an honest retention debt). A migration that creates a table
  without a registry row MUST fail CI, and a registry row whose claim
  does not resolve against the code MUST fail CI naming the table.
  Resolved `materialization_jobs` rows MUST be deleted only past the
  forensic horizon and only when no `scheduler_live_pins` row and no
  `materialization_interest` row references the job; `pending` jobs MUST
  never be deleted. `build_wanted_outputs` rows MUST be deleted exactly
  when their build row is gone or has been terminal past the horizon, and
  `delete_build` MUST remove the build row and its wanted rows in one
  transaction.
]

The registry test (`rio-migrations/tests/retention.rs`) diffs `pg_tables`
against the registry in both directions, so an undeclared new table and a
stale registry row are both merge-time failures naming the table — the
structural close of the class where migration 078 shipped two tables with
no deletion lifecycle at all (merged_bug_163). The claim half is
`xtask lint retention-truth` (the `xtask-lint` flake check): bughunt-2
found eight registry rows whose prose attribution grepped to nothing or to
forbidden code — `drv_executions` credited to the store sweep that
`store.log.sweep-ownership` bans from touching it, `jwt_revoked` credited
to a TTL sweep never written, `realisation_deps` claiming a CASCADE that
is declared RESTRICT — and the typed registry makes that whole class a
named CI failure instead of an unbounded-growth surprise
(merged_bug_001/142).

#r("sched.db.exec-stamp-on-close")[
  Closing an `assignments` row MUST stamp the closed row's
  `drv_executions` lifecycle status in the same SQL statement: every
  production assignment close renders through `close_assignments_sql`,
  whose CTE pair updates the assignment rows and stamps each closed
  row's execution status (guarded `status IS NULL` --- first verdict
  wins) atomically, with the assignment-close → execution-status
  mapping at the single site `AssignmentCloseStatus::exec_status`. The
  terminal-log epilogue MUST commute with the stamp on equal status
  (late equal-verdict writes fill `finished_at`/`final_line_count`
  gaps via COALESCE) and MUST match zero rows on a different verdict.
  An execution row whose assignment closed is therefore eventually
  sweepable; closing an assignment without stamping its execution row
  is unwritable through the production surface.
]

Before this family existed, every closer updated `assignments` alone: the
execution row kept `status = NULL` ("still running" to the store's
completeness predicate) forever, `gc_exec_rows`' terminality conjunct never
matched, and the lifecycle row was immortal (bug_047) --- the retention
story's second deleter had nothing it was allowed to delete. The CTE shape
makes the stamp unforgettable rather than remembered-per-callsite;
`db/tests/fence_coverage.rs` pins the renderer as the only production
`UPDATE assignments` site.

#r("sched.db.attempts-gc")[
  `drv_attempts` rows MUST be deleted only by the leader's periodic
  Tick-driven sweep, and the suffix every ledger loader returns MUST be
  unchanged by any sweep. For a live derivation the sweep MUST delete only
  attempt-kind rows strictly before its most recent reset row in
  `(recorded_at, attempt_id)` order, older than the retention horizon, and
  carrying no ACTIVE (`pending`/`acknowledged`) `assignments` row for their
  `exec_id`; reset rows of a live derivation MUST never be deleted. Rows whose
  `derivation_id` has no `derivations` row (orphaned histories) are deletable
  past the horizon regardless of kind. The horizon MUST be at least
  max(`LEDGER_RETENTION_FLOOR`, the LIVE configured
  `infra_retry_window_secs`, `POISON_TTL`). The eligibility predicate ---
  INCLUDING the active-assignment conjunct --- MUST be evaluated in the same
  SQL statement (one MVCC snapshot) as the deletion. A closed
  (terminal-status) `assignments` row MUST never return to
  `pending`/`acknowledged`, and an `exec_id` MUST never be re-bound to an
  active assignment.
]

The sweep is exact, not approximate, because the reset row is the
checkpoint: the latest `resubmit_reset` row carries the cycle index in its
`resubmit_cycle` column (#rref("sched.merge.poisoned-resubmit-bounded")), so
deleting rows strictly below the cut can never lower a cycle count, and both
suffix loaders cut at the same `(recorded_at, attempt_id)` tuple the sweep
complements --- machine-checked by the kernel's sweep-equivalence harnesses
(`decide()` and `materialization_decide()` bit-identical before/after any
structural sweep). The active-assignment conjunct exists for the
report-idempotency record, not the fold: `drv_attempts` rows double as the
"already classified" evidence for late reports, and deleting a terminal
attempt row whose assignment were still active would resurrect the attempt
as open. Two stability premises make that guard hold under concurrency, and
a refactor breaking either re-opens the hazard: (i) every report-idempotency
reader --- `find_attempt_by_exec_id`, `find_open_pull_attempt_by_drv_hash`,
and `list_open_pull_attempts` --- computes assignment-active and
attempt-recorded/terminal in ONE SQL statement, so by snapshot transitivity
no reader can observe "assignment active AND attempt row deleted"; (ii)
assignment closure is monotone and claim upserts always mint a fresh
`exec_id`, so no post-snapshot commit can re-activate a swept candidate's
`exec_id` --- a closure committing after the sweep's snapshot only defers the
row to the next pass. This is why the lock-free single-statement shape is
sound here while rio-store's orphan reaper needs `FOR UPDATE`: re-uploads
make THAT reaper's eligibility unstable mid-transaction, whereas no
resurrection writer exists for a closed assignment. The orphan arm rests on
referential unreachability, not the fold: a re-submitted `drv_hash` mints a
FRESH derivation UUID after GC, so an orphaned history's `derivation_id` can
never be named by any loader again (the active-assignment conjunct is
vacuous there --- the 034_assignments_terminal_backfill cascade removed the assignments rows with
the derivation). Accepted narrowing: `ListPoisoned`'s display aggregates
failed executors over full history, so pre-reset entries older than the
horizon disappear from the operator display (decisions unaffected). The
`POISON_TTL` term of the horizon max is currently dominated by the floor via
the compile-time guard in `db/attempts.rs`; it binds independently only if
the TTL ever becomes configurable or outgrows the floor (which that guard
turns into a compile error first).

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

#r("sched.db.clear-poison-batch+3")[
  `clear_poison` has a `clear_poison_batch(&[DrvHash])` variant using `WHERE
  drv_hash = ANY($1)`. The merge-time resubmit-reset path (`reset_on_resubmit`)
  clears poison for every node a resubmit flipped from terminal to fresh;
  per-hash sequential calls inside the single-threaded actor cost N round-trips
  on the dispatch hot path. Both variants clear only the poison-lifecycle
  state on the derivations row (`poisoned_at`, status) --- every retry/poison
  budget, including the resubmit cycle index, is carried by the attempt
  ledger (the `resubmit_reset` row appended in the same transaction), not by
  derivations columns.
]
The `+3` revision dropped the frozen-mirror-column clause (the batch variant
used to be distinguished by leaving `derivations.resubmit_cycles` untouched);
migration 075 removed the mirror columns, so the two variants now differ only
in call shape, not column set.

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
    -- retry/poison counters live in the drv_attempts ledger (068_drv_attempts), not here
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
  (per-execution lifecycle, `exec_id` PK; the log subsystem's anchor row).
  See `rio-migrations/migrations/` for full schema.
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
  renew proves only that the resourceVersion moved --- the mover may be the
  leader's own cancelled-then-committed write or a foreign metadata-only
  patch --- so the loop defers one round and the next completed read resolves
  it: holder still us → renew; another holder → lose with holder evidence; a
  second consecutive 409 → lose with the deferral exhausted (the bound that
  keeps the fence/steal separation intact). A 409 on steal means another
  standby raced and won.
- *Observed-record expiry:* A standby does not compare the lease's `renewTime`
  against its own wall clock (cross-node skew would make that unreliable).
  Instead, it records the holder-authored spec content --- `holderIdentity`
  plus the `renewTime` BYTES, compared for change only --- with a local
  monotonic `Instant` pair: the observation is stamped at the GET's response
  instant and staleness is measured from the deciding GET's send instant, so
  the confirmed no-write span is understated at both ends (the conservative
  direction for steals). A live leader renews every 5s, moving `renewTime`
  every 5s. If the content stays unchanged for `STEAL_AFTER` (`LEASE_TTL` +
  `FENCE_MARGIN` = 19s) of local time, the holder is not writing --- steal.
  Raw `metadata.resourceVersion` movement deliberately does NOT reset the
  clock: the apiserver bumps rv on every object write, including annotation
  patches by non-protocol tooling, and a periodic foreign mutator must not
  block stealing a dead leader's lease. The rv remains the optimistic-
  concurrency guard on the PUT; it never enters the staleness decision. Only
  the standby's own `Instant` monotonicity matters; the `renewTime` value is
  never compared to any clock.
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
residual is a clock pause longer than the budget: host suspend is now caught
at the first post-resume fence check (the blind-time measurement runs on a
suspend-aware clock --- CLOCK_BOOTTIME on Linux), but hypervisor-level VM
pause (invisible to BOOTTIME too), long stop-the-world stalls, and the
resume-to-first-tick gap remain --- no fence/steal separation closes those,
and #rref("sched.lease.generation-fence+2") is the backstop. The compile-time
assertions on the rio-lease constants pin the derivations, the margin
condition, and the response-anchoring premise (the renew attempt deadline
keeps the response-anchored fence within the commit-anchored bound the
model assumes) so no constant moves without the others.

#r("sched.lease.standby-drops-writes+3")[
  A replica that has lost the lease MUST NOT write scheduler-owned PG state
  (`derivations`, `realisations`, `build_samples`). The
  pull-mode work surfaces are leader-gated at the gRPC layer and the fenced
  transactions re-check the durable floor
  (#rref("sched.lease.generation-fence")). `ProcessCompletion`, `CancelBuild`,
  `AckSpawnedIntents`, `ReconcileAssignments`,
  `SubstituteComplete`, and `Tick` are additionally gated at actor dispatch as
  defense-in-depth.
  `reassign_derivations` --- the requeue tail that can poison a derivation
  and run the terminal log epilogue --- is individually leader-gated at its
  own chokepoint (the stream-era arms it used to ride behind ---
  `ExecutorConnected`/`Disconnected`, plus the
  deleted `DrainExecutor`/`Heartbeat`/`ReportExecutorTermination`/`ForwardPhase` ---
  are deleted with their commands).
]
The build-event stream itself has no PG write path to gate: build events
are broadcast-only (the `build_event_log` mirror, its persister, and its
GC --- the carve-outs earlier revisions of this rule had to acknowledge ---
were deleted with the WatchBuild resumability layer in favor of
#rref("sched.watch.snapshot-first")).

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

#r("sched.lease.fence-statement-guard")[
  The pull mint's active-row upsert MUST carry its generation predicate
  in-statement: the `ON CONFLICT … DO UPDATE` arm is guarded by
  `WHERE assignments.generation <= EXCLUDED.generation`, and a
  predicate-refused upsert MUST abort the mint transaction having written
  nothing (no execution row for an assignment the mint never owned),
  surfacing as the same retryable not-leader refusal as the begin-time
  fence.
]
The begin-time floor read is advisory under READ COMMITTED: a successor's
claim and re-mint can commit between the floor read and the upsert, and the
Rust-side comparison passes on the stale floor. The statement guard is the
authoritative half — PostgreSQL evaluates the conflict-arm `WHERE` against
the row's latest committed version (EvalPlanQual), so the destructive
overwrite of a newer tenure's row updates zero rows regardless of snapshot
age. Equality passes (`<=`): the same-epoch re-acquire keep. The residual —
a fresh INSERT below the floor when no active conflict row exists to
evaluate against — cannot regress any newer row by construction; it is
priced and bounded in `fencedWrites.qnt`
(`activeRowGenMonotonic` holds even with the residual reachable).

#r("sched.lease.tenure-stamp-type")[
  Every fenced evidence write MUST take its generation from the tenure stamp
  recorded by the recovery claim step --- the typed serving-generation
  capability whose sole constructor is the claim-stamp site --- never from a
  fresh read of the live lease atomic. The fenced database entry points MUST
  accept only the stamp type, so an actor-side write path structurally cannot
  relabel an in-flight transaction with a generation produced by a mid-tenure
  lease bump that the claims ledger has not vouched for.
]
The stamp type is the compile-time carrier of the write-ahead claim
discipline (#rref("sched.lease.generation-claim")): the claims floor vouches
for exactly the generation the claim step recorded, and a fresh atomic read
taken between the claim and the write can exceed it (lease bump, rebound),
producing a write the floor never covered. The policy census pins the two
production constructor sites and zero fresh-atomic reads in actor write
paths.

#r("sched.grpc.fence-retryable")[
  Every refusal a fence or leadership guard produces MUST surface to clients
  as a RETRYABLE gRPC code, and the code MUST derive from the refusal's
  retry class: `Retryable ⟺ code ∈ {UNAVAILABLE, RESOURCE_EXHAUSTED}`,
  exhaustively over every refusal surface (`ActorError`, `PullRejection`).
  In particular the claims-floor fence's `StaleGeneration` maps to
  UNAVAILABLE (the `ensure_leader` not-leader family) — never
  FAILED_PRECONDITION, which no client retries. The gateway MUST retry a
  refused `SubmitBuild` boundedly while and only while no `x-rio-build-id`
  metadata has been received (the scheduler sets that header only after the
  merge commits, and a refused merge rolls back, so the re-submit is
  idempotent); any other code, a timeout, or a post-metadata failure
  propagates unchanged.
]
The class law is the structural carrier: the per-variant `retry_class()`
fns are exhaustive matches (a new refusal variant must choose), the status
constructors are derived next to a `debug_assert` of the law, and the
`retry_class_code_consistency` unit test pins the biconditional over every
variant of both enums — so a future refusal surface cannot silently map a
retryable condition to a terminal code (the bug_393 class). Model-level
verification: `fenceRefusalAlwaysRetryable` (fencedWrites.qnt) with the
fence-393-terminal-refusal calibration pin as its falsifiability pair.

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
(its `WorkAssignment.generation` field was removed with it; field 7 is
reserved in the proto), and what a worker could once latch is replaced by
what the durable floor covers.

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
(the retired `HeartbeatResponse.generation = 0` sentinel until recovery
completed) so that workers
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

#r("sched.lease.graceful-release+2")[
  On graceful shutdown (SIGTERM), if the lease loop ever acquired the lease
  and has not since observed another holder (a completed election round
  resolving not-leading), it calls `step_down()` to clear `holderIdentity`
  before the process exits; the local self-fence does NOT clear this gate ---
  fencing is a local belief change that writes nothing to the apiserver, so a
  fence-then-SIGTERM sequence MUST still release the possibly-still-ours
  lease. This is an
  optimization, not a correctness requirement: without it, the next replica
  waits up to `STEAL_AFTER` (19s) for observed-record expiry. With it, the
  next replica's `decide()` sees an empty holder and steals on its next poll
  tick (one `RENEW_INTERVAL`, 5s). The `step_down()` call is itself
  holder-guarded and resourceVersion-guarded (404/409 →
  someone already stole or vacated, treated as success), so a stale gate costs
  one harmless round-trip; `main()` awaits the lease-loop's
  `JoinHandle` after `serve_with_shutdown` returns, ensuring the PUT lands
  before process exit. If `step_down()` fails (apiserver unreachable), the loop
  logs a warning and observed-record expiry is the fallback.
]

#r("sched.lease.rebound+4")[
  ANY completed read of a lease this replica holds, observed while it
  already believes it leads --- a renew round that resolves Leading, or the
  own-commit evidence consumer on an abandoned write --- whose observed
  `leaseTransitions` count differs from the count
  recorded at this replica's most recent acquire edge or rebound, MUST be
  treated as a late-observed holder change: the lease loop MUST re-record the
  observed count, re-derive the generation from it via `fetch_max`, clear
  `recovery_complete`, and fire the dedicated rebound hook (`on_rebound`) so
  the consumer runs its declared rebound effects and recovery re-runs against
  the post-change state; `is_leader` MUST NOT be cleared by this transition.
  Every believing consumer of held-lease read facts MUST route through one
  shared observe-completed-read body that fuses the comparison to the
  observation --- a consumer that records the observation without the
  comparison has no API to call.
  The rebound hook is a REQUIRED member of the hooks contract (no default),
  and each consumer-side leadership-edge effect MUST declare its rebound
  policy explicitly: Compound (lose cell then acquire cell --- the default
  posture, since a rebound is a compressed lose→acquire pair whose standby
  interval was never locally observed) or AcquireOnly with a written
  rationale. The scheduler MUST deliver the rebound as its own actor command
  that runs the Compound members' lose cells and then the full acquire path
  --- never the lost handler's state wipe. The lease loop MUST also mark the
  leader marks dirty: a foreign term that ran to completion inside the
  observation gap guarantees the foreign holder's reconcile swept this pod's
  marks, so the unchanged-polarity argument for skipping the re-patch does
  not apply.
]

The shapes this catches land entirely inside this replica's observation gap
--- a foreign term that ended in a graceful vacate
(#rref("sched.lease.graceful-release")), or a delete/recreate --- so neither
an acquire nor a lose edge ever fires locally; one `kubectl delete lease`
during a renew-blind window shorter than the self-fence deadline suffices.
While this replica holds continuously, only a holder change or a
delete/recreate can move `leaseTransitions` (renews never write it), and a
foreign holder still present at the next completed read resolves through the
holder-evidenced lose edge (a Standby resolution, or a 409 whose one-round
deferral expires against it) --- so an unequal count on a
still-leading round is always a genuine discontinuity, and the cost of acting
on one is a single recovery re-run with dispatch gated during it. The
dedicated hook (rather than a re-fired acquire) is what lets consumers run
the lose-shaped HALF of the transition they actually need --- the
leadership-edge table's Compound lose cells, e.g. the cost-table latch whose
skipped false-store let a post-rebound housekeeping tick persist prices over
the foreign tenure's evolved table --- without a synthesized full lose, which
would force a pointless wipe of state the immediately-following re-recovery
rebuilds (and, if the full lose edge were synthesized, an
`is_leader = false` blip), adding nothing to the dispatch gating the
rebound's own `recovery_complete` clear already provides; hook delivery is
ordered (#rref("sched.lease.hook-order")), so the split is about running the
right effects, not about reordering. The accepted residual is the count
coincidence: an observed count that lands exactly back on the recorded value
is indistinguishable from steady state --- the same coincidence pricing as
the recovery gate's deletion-ABA note --- and in that shape no command is
queued, so a recovery loaded across the foreign tenure persists until the
next real leadership change or rebound. Rebounds are counted on their own
counters (#(refs.metric)("rio_scheduler_lease_rebound_total"),
#(refs.metric)("rio_controller_lease_rebound_total"));
#(refs.metric)("rio_scheduler_lease_acquired_total") counts acquire edges
only.

#r("sched.lease.holder-evidenced-lose+4")[
  A renew 409 observed while this replica believes it leads MUST NOT run the
  lose transition by itself: the 409 proves only resourceVersion movement,
  whose mover may be this replica's own cancelled-then-committed write or a
  foreign non-protocol patch. The loop MUST defer exactly one round, keeping
  belief, the hold, and the cancelled-write ledger intact. The lose
  transition MUST require holder evidence: a completed READ resolving the
  lease to a holder other than this replica --- on whichever arm observes it
  (a Completed round's standby resolution or an act-failed round's completed
  read) --- or a second consecutive believing 409 exhausting the one-round
  deferral. And EVERY believing completed read of a LATER round MUST
  resolve a pending deferral: a read resolving holder=us clears it through
  the observation funnel (own-commit evidence, frozen content, and
  moved-content-without-ledger alike), a read resolving holder=other
  clears it through the evidenced lose --- so two 409s with any completed
  read between them are never consecutive. A conflict-bounced round's OWN
  read is not a resolution of its OWN 409 --- the bounce proves a mover
  acted after that GET --- so only its monotone readings may act
  (own-commit movement, an observed foreign holder, observed absence);
  its non-monotone readings MUST be refused typed rather than funneled,
  which is what keeps the exhaustion bound real, while same-round
  own-commit evidence still resolves a deferral PENDING FROM AN EARLIER
  round before the round's own 409 is adjudicated. A completed read
  observing ABSENCE is deletion-axis evidence with its own routed rows in
  the same total law: the lease resolves to nobody and a re-creator needs
  no steal wait, so a believing replica MUST exit belief at that read ---
  the same lose-class transition, clearing any pending deferral --- and
  the release hold is priced PER CELL: it clears only when no write of
  ours is in doubt (an empty cancelled-write ledger and no act
  transmitted this round --- then there is provably no lease object of
  ours to release), and it MUST take the self-fence posture (kept) when a
  transmitted write may still commit a lease naming us --- the
  zombie-create window, where a cleared hold would skip the
  holder-guarded shutdown release and cost the successor the full steal
  wait the graceful-release pricing forbids --- while a standby replica
  re-baselines only (the Absent baseline is what lets the next read prove
  a transmitted Create committed).
]

The one-round bound is what preserves the fence/steal separation: an
unbounded deferral under every-round CAS bounces would retain belief past a
standby's steal. The deferral applies to the *believing* renew path only ---
a 409 racing a steal resolves as never-led, with nothing to defer. (Owner
decision: bughunt-4 fix-wave §5-S Q3, signed 2026-06-08; the dated SIGNED
block sits at the lose-edge arm in `rio-lease/src/lib.rs`.)

#r("sched.lease.cancelled-write+2")[
  A lease write abandoned by a client-side deadline MUST be treated as
  possibly committed, never as discarded. The renew composition MUST bound
  its read and write phases separately, so that a mutating request is only
  ever transmitted after a completed read and a transmitted request's
  response is always awaited under its own budget; every mutating act that
  fails after possible transmission MUST be recorded as an unconfirmed write
  anchored at the attempt's pre-send clock reading, keeping the OLDEST
  anchor while unconsumed; and the blind-window clock MUST be stamped from
  that ledger only by own-commit evidence --- a later completed read
  observing this replica as holder with holder-authored spec content
  (`renewTime` bytes) unequal to the previous completed read's --- at the
  LEDGER's anchor, never the observing read's time. A completed read whose
  holder-authored content is unchanged MUST stamp nothing --- even when the
  object's `resourceVersion` moved: the apiserver bumps rv on every write,
  including non-protocol metadata patches, and rv movement the protocol did
  not author is not evidence that any write of OURS committed.
]

The regime this closes is the mid-band apiserver: reads answer inside their
budget, writes are too slow to answer but still commit. Pre-fix the single
composition deadline stamped nothing on expiry, so the leader self-fenced at
`SELF_FENCE_AFTER` and stayed out --- while its committed-but-cancelled
renews kept bumping the resourceVersion, re-anchoring every standby's
observed-record clock (#rref("sched.lease.k8s-lease")) --- an unbounded
leaderless livelock with the lease perpetually "live" and nobody believing.
The split phases keep both failure directions honest: a truly blind replica
fails its READ, transmits nothing, and freezes the rv for stealers (so
takeover still works); a replica whose writes commit proves it with the rv
movement its own next read observes, and stays leader. Stamping at the
ledger's anchor --- minted before the send, so anchor ≤ send ≤ apiserver
commit --- keeps the NeverDual arithmetic intact: the window the victim
fences on is never shorter than one anchored at the commit the stealer's
clock starts from (the `leaderElectionDroppedWrite` regime of
`docs/spec/models/leaderElection.qnt` is the machine arbiter; its falsify
twin drops the evidence action and demonstrates the livelock). Read-stamping
--- restarting the window on a bare completed read with a frozen rv --- is
the tempting wrong fix: it would let a read-only replica believe forever
while a healthy peer steals, which is exactly a dual-belief seed; the
frozen-rv companion test and the model twin pin the refusal.

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

#r("sched.lease.deletion-cost+3")[
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
  ex-leader cannot remove its own) --- and that sweep MUST spare the current
  Lease holder: the sweep reads the Lease in the same reconcile pass and
  excludes the holder from its targets, so a peer that re-acquired while this
  reconcile was in flight does not have its fresh label stripped (a failed
  holder read fails the sweep --- keep dirty and retry --- rather than
  sweeping blind); a failed sweep leaves the marks unconverged
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

#r("sched.lease.marks-verify")[
  The leader-marks reconcile MUST be backed by a bounded-cadence
  verification: every `MARKS_VERIFY_EVERY` election rounds (12 renews, ~60s),
  a leading loop whose marks are clean and whose reconcile slot is free reads
  its OWN Pod and compares the stored marks (deletion-cost annotation; leader
  label when configured) against its current leadership; on divergence it
  re-marks the reconcile dirty so the next round-trip repairs them. This
  bounds the staleness from ANY marks falsifier --- a foreign sweep racing a
  re-acquisition (the sweep's read-then-patch TOCTOU), `kubectl label`, or
  any future actor outside the edge-writer set --- to one verify interval
  plus one reconcile, replacing the previous contract in which a divergence
  not caused by an enumerated edge writer persisted until the next leadership
  transition.
]

The verify pass shares the reconcile's single-flight slot (verify and patch
never interleave) and is leader-only: a standby's marks are converged by its
own lose-edge reconcile and swept by the live holder, so verifying them too
would double the fleet's read load for marks nothing routes on. The pass
costs one Pod GET per leader per interval; a GET failure is benign (the next
interval retries) and a divergence verdict is always safe to act on (the
repair is one idempotent merge patch).

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

#r("sched.sla.hwclass.capacity-types+2")[
  `sla.hwClasses[h].capacityTypes` lists capacity-types $h$ is permitted to
  provision (default `[spot, on-demand]`). `solve_full` and the controller's
  `all_cells`/`fallback_cell` iterate THIS, not `CapacityType::ALL`, so an
  od-only class structurally never generates a `(h, Spot)` cell
  --- preventing the conflicting-requirements ICE loop a requirement-based
  exclusion would cause.
]

(No shipped hw-class is od-only since M1 folded metal into
`[spot, on-demand]` with od failover --- the doctrine pin is
`43-metal-capacity-doctrine.sh`; the mechanism above stays normative for any
operator-narrowed class.)

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

#r("sched.sla.pin-wire")[
  An operator `--capacity` pin MUST survive serialization as typed data
  (`SpawnIntent.capacity_pin`, values from the shared
  `rio_common::cell_wire` alphabet, stamped at the solve chokepoint on
  EVERY emission of pinned demand) and MUST fail closed at every
  consumer: the controller's cold-start fallback lane may only pick
  cells at the pinned capacity, an undecodable pin refuses every class,
  and "no class hosts the pin" is the typed ADVISORY pend
  (`PlacementOutcome::PinGated` --- counted, off the verdict wire) ---
  never the poison-feeding no-hosting-class verdict and never an
  off-pin placement.
]

Rationale (bug_121, R25): a pin-gated emission folded to empty cells and was
byte-identical on the wire to an hw-agnostic one --- the controller's
`fallback_cell` arch/size-matched it and ran the on-demand-pinned build on
spot at the class's first configured capacity, with the only disclosure a
debounced scheduler-side warn. Two letters sharing a wire image re-create the
silent-empty population the emission alphabet was minted to kill, so the pin
either survives as a typed field or the letters must map injectively onto
consumer dispositions; this rule demands the field. The pend is deliberately
OFF the verdict wire: the pin disposition is the scheduler's own knowledge
(it minted `CellEmission::PinGated` and disclosed at the mint site), and the
two populations sharing the empty-cells-plus-pin image (pin-gated vs pinned
genuinely-unhostable) are split by the consumer's own pin-stripped
re-derivation --- the injectivity census (`W10-Z`) pins both mint premises to
that predicate's branches, and the wire-mapping census pins `PinGated` into
the no-verdict set. Absence of the field decodes as "no pin" (the Q6
read-side-first law: pre-pin-wire schedulers only ever emitted unpinned or
affinity-constrained intents).

== Catalog-derived per-class ceilings (ADR-023 §13c-2)

Per-class `(max_cores, max_mem)` ceilings are derived *at scheduler boot* from
the AWS instance-type catalog rather than hand-maintained config --- the prior
§13c-1 design's hand-pinned values drifted from what each class's
`requirements` actually permit @karpenter to launch (the `cover::sizing` STRIKE
rounds). Boot-time derivation removes the operator-side staleness step
entirely: a `requirements` edit takes effect on the next rollout.

#r("scheduler.sla.ceiling.catalog-derived+4")[
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
  from the catalog (warn). Ceiling candidacy MUST be grounded in
  LAUNCHABILITY, never bare API existence: the committed, censused
  launch-evidence exclusion (`sla.unlaunchableSizes` --- instance-size tokens
  present in `describe_instance_types` with ZERO launchable capacity in the
  deployment region in either market) is synthesized as an `instance-size
  NotIn` requirement for EVERY class at the derive seam --- one mint, so a
  class whose own `requirements` omit the row cannot re-import a phantom into
  its ceiling or the global --- and the grounding is EXCLUSION-ONLY negative
  evidence, never a cap at the largest observed launch (the ratchet-down
  failure mode the module doc records).
]
Rationale (live_050(d), the 2026-06-10 mechanism revision): the boot log
showed `max_cores=383` --- the gen-8 Intel 96xlarge/metal-96xl rows exist in
the API with zero launchable us-east-2 capacity; every hi claim was sized to
the phantom, Karpenter fleets contained only phantom types, and ICE hit BOTH
markets (the hi→od override iced identically, refuting market exhaustion).
The largest buyable gen-8 c/m/r is 48xlarge (192 vCPU, family proven live).
Runtime ceiling staleness (a shrink between boots re-pinning persisted
demand) is the `scheduler.sla.ceiling.stale-solve-revalidation` law's axis.

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

#r("scheduler.sla.global.derive+2")[
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
  lease-acquire reload. The global MUST derive from launchability-grounded
  class ceilings only (the per-class exclusion binds at the derive seam for
  every class, so `max` is honest by composition --- one loose class cannot
  re-import a phantom into the global clamp) and MUST NOT exceed the largest
  class ceiling --- on the catalog arm reachable only through the
  `MIN_CORES`/`MIN_MEM` floor clamp against a degenerate sub-floor catalog;
  an operator override exceeding every class ceiling MUST be disclosed at
  boot (WARN naming the delta and both provenances --- the override is a
  signed operator act, so the doctrine is disclose-don't-wedge, never a boot
  failure).
]
Rationale (live_051(a)/(f2), the cancelled-python-builds verdict):
`resolve_globals` maxes over ALL per-class catalog ceilings, so one loose
class (live: fetcher `Gt 5` pre-rev-4) re-imported the phantom into the
GLOBAL clamp even after every other class was grounded; demand admitted at
that global was hostable by no class, fell through the hw-agnostic emission
silently, and churned as `no_hosting_class` until operators cancelled the
builds. Disclosure on the override arm is the boot-time half; the
emission-time half is the `scheduler.sla.ceiling.stale-solve-revalidation`
law.

#r("scheduler.sla.ceiling.stale-solve-revalidation+2")[
  A demand envelope solved under a catalog ceiling MUST be revalidated
  against the live ceilings at every emission; an envelope no class can
  host MUST re-solve (clamp into the largest live hosting class, with
  the clamp disclosed) or surface typed — never an UNCLASSIFIED empty
  emission for non-agnostic demand. Demand infeasible at every class
  MUST clamp-with-disclosure into the largest mintable class or surface
  as a typed `Unhostable` carrying demand and best-class. The totality
  of the emission alphabet is scoped to the scheduler: a typed
  `Unhostable` serializes as empty `hw_class_names` BY DESIGN (the
  designed feeder of the controller's `no_hosting_class` arm — the
  forced-demand law requires the empty emission so `fallback_cell`
  reaches its own `None`), distinguished controller-side by the
  `IntentVerdict` plane, never by a second wire axis. A persisted resource floor is evidence under the ceiling that
  authorized it: a floor above the live global MUST be consumed clamped
  — at hydrate and at every read — while the durable row PRESERVES the
  witnessed evidence (the `GREATEST`-ratchet writer; a later ceiling
  re-growth re-admits it). A typed no-hosting-class verdict MUST be
  consumed to a terminal, operator-actionable disposition within a
  typed verdict budget — a drv never loops Ready unanswered. The
  widen-only establishment window governs the deadline/reap axis ONLY
  and never pins a demand envelope. Operator-forced dims are pins, not
  stale solver evidence — they are never clamped; their unhostable form
  surfaces typed and is answered by the verdict loop.
]
Rationale (live_050(e), the post-rev-3 journal): after the NotIn overlay
shrank the derivable ceiling, 192 hi intents re-emitted with empty
`hw_class_names` — open attempts' demand envelopes solved under the old
383-core ceiling were never re-solved, the `reference_hw_class_for_system`
None arm emitted empty cells silently, and the controller dropped them as
`no_hosting_class` at \~25/min; \~120 durable attempts carried the stale
envelopes across leader boots (the widen-only sweep re-solve never shrinks
a persisted solve); the owner abandoned the run. Per-emission revalidation
subsumes a shrink EVENT (an event observed only in-memory misses the
cross-boot reload path) at the cost of work already done — the emission
arm already resolves classes per intent. The 3600s attempt-deadline
self-heal EXISTS (deadline+slack expiry eventually reaps and re-mints) and
is PRICED as the recovery floor only — the live run starved past it; it is
never the mechanism. The verdict budget's time envelope is
`NO_HOST_VERDICTS_TO_POISON x the controller ack cadence` (\~10s tick;
30 passes ≈ 5 minutes), violable by const; a hosting-class config reload
resets the count in-band (the verdict detail names the configured
classes, so a reload is observable without a side channel).

#r("sched.sla.ladder-transit")[
  The ladder closure walk MUST separate REACHABILITY from ADMISSION:
  the worklist transits every declared ladder edge of a walked class
  --- whether or not that class minted any cell for THIS demand ---
  and per-rung cell admission (the hosting predicate, the
  capacity-pin filter) gates only which CELLS join the closure, never
  which declared edges are walked. A mid-chain rung minting zero
  cells (a spot-only class under an od pin; a smaller-ceiling rung; a
  wrong-arch rung) MUST NOT sever the declared tail behind it, and
  any future per-cell filter added to the walk MUST compose against
  admission only. Seeds remain the RETAINED producer classes --- a
  stripped producer cell is a regression signal, not a closure seed.
]
Rationale (bughunt-11 merged_bug_015; amends the wave-10 transitive
closure, 7ae2b282f): the worklist enqueued a rung only if it became a
closure MEMBER (≥1 cell admitted), and both per-cell filters before
membership were silent --- so a spot-only g7 under an od pin, or a
small-ceiling g7, severed the operator's declared g8→g7→g6 tail in
exactly the multi-generation capacity event the ladder exists for
(the live_050 strand shape, re-created one filter later). Expanding
the fixpoint over filtered OUTPUTS instead of the declared graph made
reachability demand-dependent: every new per-cell filter composed
multiplicatively against the tail. The documented-intent half of the
old behavior is PRESERVED and now stated precisely: an unhostable
rung's CELLS are skipped --- its EDGES are not (the closure can still
never admit a cell the strip would refuse). The walk's "can never
silently truncate" claim binds to the pin × ceiling × hosting product
table (`ladder_transits_declared_edges_independent_of_rung_admission`,
sla/config.rs) --- the zero-cell mid-rung rows are the cells the
pre-amendment fixpoint test never covered (it quantified over
fully-hostable unpinned chains only).

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

#r("sched.sla.ack-validate-then-commit+2")[
  `AckSpawnedIntents` application MUST be validate-then-commit at PER-PLANE
  granularity: every refusal --- undecodable plane entry, length-skewed
  arming echo --- MUST be computed before the first state mutation; a
  refused plane MUST withhold all of its own evidence (the plane is the
  refusal unit --- the spawned plane's arm decodes and its spawn-ack
  witnesses withhold together) while sibling planes apply, so one poisoned
  durable row cannot black out any other plane's evidence; the reply MUST
  disclose every refused plane as a typed refusal naming the plane and its
  first offending entry, never a silent drop, and an erring reply that
  names refused planes implies that every plane not named LANDED (the
  deposed-drain refusal remains whole-request: nothing landed). A plane
  whose apply is not idempotent under whole-buffer redelivery MUST NOT take
  the per-plane arm --- it refuses whole-with-disclosure or stays out of
  the redelivery loop: cell-event planes ride the per-cell evidence-epoch
  gate, observed types upsert, binding snapshots rebuild wholesale, and the
  verdict plane is level-triggered (minted fresh per cover pass, dropped on
  Ack-Err, never redelivered from the retained buffer); a refused verdict
  plane is a non-event for the pass ordinal.
]

The commit half is infallible by signature (the validated plan carries only
decoded cells, hashes, and rows --- no raw wire type crosses into it; a
refused plane's slot is empty by construction), so partial application is
exactly the disclosed refusal list, never an undisclosed half-apply.
Per-plane refusal is safe producer-side: the controller's
commit-on-Ack buffer is retained on Ack-Err
(#rref("ctrl.nodeclaim.evidence-ack-latch")) and buffered marks keep masking
`cover_deficit` locally until acked, so refusing loses no protection --- the
refusal is a loud skew signal where a drop was silent evidence destruction,
and redelivered landed planes no-op through their own idempotency laws. The
pre-round-11 whole-request form let one undecodable durable row (a Job
annotation re-derived every tick) starve every sibling evidence plane in
its request for the row's life --- the bug_142 wedge this rule now closes.
The closed-cost-gate refusal class is retired: the gate existed solely to
keep observed-type writes off the pre-reload table, and `carry_catalog`'s
menu merge made that window lossless, so consume-once evidence is never
refused for a sibling plane's apply-window anymore.

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

The union-only law makes the lease-acquire reload lossless: `carry_catalog`
merges the outgoing in-memory menus into the fresh PG load (per-`(cell,
name)`, the newer `last_observed` winning wholesale), so an observation
folded before the edge reload survives it --- the menu plane needs no
apply-window gate.

#r("sched.sla.merge-law")[
  The per-`(cell, instance_type)` menu merge law is newest-wins-WHOLESALE
  and MUST have exactly one in-crate implementation consumed by every Rust
  write leg (the controller-observation fold and the lease-edge carry),
  with the PG upsert mirroring it under the same STRICT monotonicity qual:
  a strictly newer `last_observed` replaces the whole entry, ties and older
  observations keep the current holder. The observation intake MUST refuse
  zero-resource observations as a typed, counted letter
  (`_evidence_refused_total{plane="observed_types", reason="zero_resource"}`)
  --- absence of parseable kubelet resources is not a 0-core fact.
]

A durable store with a hand-implemented merge law at $N$ write sites has
$N$ laws: bug_059's observe leg kept-first-with-fresh-timestamp while the
carry and persist legs honored newest-wins, so a producible 0-core first
observation was immortal --- re-stamped fresh, winning every monotonicity
qual, and permanently excluded from the `$/vCPU` fold (which divides by
menu cores). The wholesale overwrite doubles as the store's liveness dual
(the `sys.liveness.exit-edge` instance for this plane): historical junk
rows are healed by the next complete observation rather than re-stamped,
and the intake refusal stops new junk from minting --- the keep-first
composition had no such exit edge.

#r("sched.sla.epoch-domain")[
  Absolute PG epochs in the SLA cost plane MUST cross the sqlx boundary
  through a finite-by-construction typed domain (`Epoch`), minted only at
  the decode boundary: a non-finite stored epoch
  (`'infinity'::timestamptz`, NaN) is a typed, counted, per-row refusal
  (`_evidence_refused_total{reason="nonfinite_epoch"}`) that skips the row
  and leaves siblings unaffected --- never a raw `f64` store. Staleness
  ages, newest-wins maxima, and watermark folds MUST be computed as
  methods of the typed domain, so the non-finite comparison arms
  (`now - inf = -inf` reading eternally fresh; `at > 'infinity'` matching
  nothing) are unrepresentable for the whole family.
]

The family axis (R28) is enforced one level out by the in-crate
`EXTRACT(EPOCH ...)` census: cost.rs is the family's only home (three
query strings, each consumed through a sanctioned constructor), and a new
absolute-epoch read anywhere in `sla/` goes census-red until it joins the
typed domain. R29 context: the wave-10 clamp split (`fix(rio-common):
split the epoch domain out of the age clamp`) hardened one of four reads
in the same function; the type seals the family, not the site. The
poisoned PG row itself is deliberately left in place --- the monotone
upsert qual refuses rewinds, so the row is read-dead (refused at every
load) until operator surgery, and the staleness clamp plus
#(refs.alert)("RioSlaHwCostStale") arm truthfully the moment the stamp stops decoding.

#r("sched.sla.class-membership")[
  An open-string wire grammar feeding closed-domain SLA stores MUST carry a
  MEMBERSHIP SEAM at the consumer's growth boundary: hw classes decoded from
  cell events join the durable observed-types store and the ICE mask's
  TTL-less watermark only by lookup against the configured class set.
  Membership skew MUST be exactly as loud as grammar skew --- a typed,
  counted letter (`_evidence_refused_total{reason="unknown_class"}`) ---
  never silent durable growth. Arm dispositions (recorded): the DURABLE
  observed-types store REFUSES the entry; the in-memory, redelivery-tolerant
  ICE mask WARNS-AND-DROPS per event (whole-request refusal would wedge the
  controller's commit-on-Ack redelivery loop on one skewed entry, and an
  unknown-class mask is read-dead --- dispatch subtracts masks from
  CONFIGURED cells only).
]

The seam makes the prose cardinality bounds enforceable: "Bounded by
`|H|x2`" on the watermark and the catalog-bounded observed-types claim were
producer-honesty prose over a grammar that admits any string before the
last `:` --- a skewed or defective producer grew durable KeepForever state
silently while undecodable entries got the loud refusal lane. The
membership snapshot is installed at actor construction from the
process-immutable configured set and carried across the lease-edge reload
with the other process-lifetime table state; unarmed (`None`) admits ---
the legacy lane for direct test constructions and the pre-construction
boot window, pinned by the wiring regression test.

#r("sched.sla.one-aggregator")[
  Each c-independent scalar axis of the per-pname fit (memory, disk) MUST
  have exactly ONE aggregation function consumed by every arm (full-fit,
  probe, degenerate fallback): the quantile's evidence universe is an
  explicit population parameter --- always the full row set, ring-weighted
  where a row holds a c-axis seat and unit-weighted elsewhere --- never an
  implicit property of which vector an arm happened to collect. Subset
  quantiles are census-red, not comment-policed.
]

bug_070/bug_072 were one law violated per axis: the disk fallback's
emptiness gate dropped every peaked legacy row the moment one c-axis row
carried a peak (p90 of $N+1$ collapsing to 1), and the probe arm
single-sampled memory from the newest row two lines above the aggregated
disk --- estimator quality silently depended on which arm a pname landed
in. The `(axis, arm)` census (W11-BC) is the belt: per-axis evidence
reads outside the chokepoints drift its committed counts.

#r("sched.sla.forecast.one-layer+2")[
  `compute_spawn_intents` walks the Ready frontier AND a forecast frontier of
  `Queued` derivations whose every incomplete dependency is running with a
  fitted-curve $"ETA"$ — or substitution-active with the typed prior
  (#rref("sched.sla.forecast.substituting-dep-eta")) — under $"ETA"
  < max_((h,"cap") in A) "lead_time"[h,"cap"]$. Each `SpawnIntent` carries
  $(A, c^*, M, D, "eta")$ with $"eta"=0$ for Ready and max-dep-ETA for
  forecast. Forecast lookahead is exactly one DAG layer; the substitution
  contribution is job-grounded direct evidence, never layer propagation.
]

#r("sched.sla.forecast.substituting-dep-eta")[
  A dependency carrying a store-ACTIVE materialization job (claimed, or
  claimable now) MUST contribute the typed substitution prior
  (`SUBSTITUTING_DEP_ETA_PRIOR_SECS`, violable) to the forecast dep walk
  through the unchanged lead-horizon gates; a PACING job (parked/deferred)
  MUST yield a typed, counted exclusion
  (`forecast_dropped_total{reason="substituting_pacing"}`); an UNCLAIMED job
  MUST NOT displace a live build attempt's progress-grounded ETA; an
  unhydrated job view MUST fail closed to the status disposition, uncounted.
]

The disposition set is total over status × job armament (`SubstDepEta`,
derived from the one `claimability` source — bug_170): a held claim displaces
a stale build curve (cache hits never builder-dispatched have no curve at
all); terminal dep statuses kill regardless of job state. The prior is static
by design — the scheduler retains neither claim timestamps nor byte progress
(`ReportMaterializationProgress` is a display-only relay), so in-flight decay
is a recorded non-goal; the error rides the same `eta_error` absorption family
as the ref↔wall skew, and the per-cell `lead_time` return channel remains
absent (the seed-based gate approximation is the operative law for BOTH eta
sources — r34 merged_bug_006's caveat carries unchanged).

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
    Gateways reconnect via `WatchBuild(build_id)` and resynchronize from its
    snapshot-first attach (#rref("sched.watch.snapshot-first")).],
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
