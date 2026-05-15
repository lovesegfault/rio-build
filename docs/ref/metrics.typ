#import "/lib/rio.typ": *

#show: rio.with(domains: none)

= Metric Reference

Full per-component metric inventory. Naming convention, label rules,
leader-gating, and histogram bucket policy are specified normatively in
#xref(label("r-obs.metric.gateway"), [Observability §Metrics]).

== Gateway <tbl-metrics-gateway>

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Metric], [Type], [Description]),
  (refs.metric)("rio_gateway_connections_total"),
  [Counter],
  [Total SSH connections],

  (refs.metric)("rio_gateway_connections_active"),
  [Gauge],
  [Currently active connections],

  (refs.metric)("rio_gateway_opcodes_total"),
  [Counter],
  [Protocol opcodes handled (labeled by opcode)],

  (refs.metric)("rio_gateway_opcode_duration_seconds"),
  [Histogram],
  [Per-opcode latency],

  (refs.metric)("rio_gateway_handshakes_total"),
  [Counter],
  [Protocol handshakes completed (labeled by result:
    success/rejected/failure)],

  (refs.metric)("rio_gateway_channels_active"),
  [Gauge],
  [Currently active SSH channels],

  (refs.metric)("rio_gateway_errors_total"),
  [Counter],
  [Protocol errors (labeled by type)],

  (refs.metric)("rio_gateway_bytes_total"),
  [Counter],
  [Bytes forwarded to/from SSH client (labeled by `direction`: `rx`/`tx`)],

  (refs.metric)("rio_gateway_quota_rejections_total"),
  [Counter],
  [SubmitBuild rejected because tenant is over store quota (labeled by
    `tenant`)],

  (refs.metric)("rio_gateway_auth_degraded_total"),
  [Counter],
  [SSH auth accepted but tenant identity degraded to single-tenant mode due
    to a malformed `authorized_keys` comment (labeled by `reason`:
    `interior_whitespace`). Alerts on misconfigured multi-tenant keys
    silently falling back to single-tenant.],

  (refs.metric)("rio_gateway_jwt_mint_degraded_total"),
  [Counter],
  [JWT mint failed but `jwt.required=false`, so the request degraded to the
    `tenant_name` fallback. Alert if rate > 0 sustained: mint failures
    indicate signing-key misconfig or clock skew; downstream services lose
    cryptographic tenant proof.],

  (refs.metric)("rio_gateway_jwt_refreshed_total"),
  [Counter],
  [Session JWT re-minted because the cached token was near expiry
    (#rref("gw.jwt.refresh-on-expiry")). Expected to be nonzero under
    `ControlMaster` mux'd workloads or single channels with >65min builds;
    not an error.],

  (refs.metric)("rio_gateway_jwt_refresh_failed_total"),
  [Counter],
  [Session JWT re-mint failed; the stale token was kept and downstream will
    reject with `ExpiredSignature`. Alert if > 0: re-mint uses the same key
    that minted the original, so failure indicates a corrupt signing key.],

  (refs.metric)("rio_gateway_putpath_aborted_retries_total"),
  [Counter],
  [`PutPath` retries on store `Code::Aborted` (labeled by `attempt`:
    `1`..`8`). `attempt=8` means the retry budget was exhausted and the
    error surfaced to the client. Alert if `attempt=8` rate > 0: GC mark is
    outlasting both the store-side and gateway-side retry windows (I-168).],
)

#info(title: [Note on #(refs.metric)("rio_gateway_connections_total")])[
  Incremented on first SSH auth attempt (`result=new`), then again on auth
  outcome (`result=accepted`, `result=rejected`, or `result=rejected_jwt`).
  TCP probes that close before sending SSH bytes (NLB/kubelet health checks)
  do not increment --- russh's `new_client()` fires on TCP accept, so the
  counter is deferred to the first `auth_*` callback. A single successful
  connection still generates two increments; use `result=accepted` +
  `result=rejected` + `result=rejected_jwt` for success/failure rates.
  `rejected_jwt` fires when SSH auth succeeds but the JWT mint fails with
  `jwt.required=true` --- indicates signing-key misconfig or clock skew;
  distinct from `rejected` (SSH auth failure) so dashboards can alert on
  JWT-rejection spikes separately.
]

== Scheduler <tbl-metrics-scheduler>

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Metric], [Type], [Description]),
  (refs.metric)("rio_scheduler_builds_total"),
  [Counter],
  [Total builds at terminal state (labeled by `outcome`:
    `success`/`failure`/`cancelled`)],

  (refs.metric)("rio_scheduler_builds_active"),
  [Gauge],
  [Currently active builds],

  (refs.metric)("rio_scheduler_derivations_queued"),
  [Gauge],
  [Derivations waiting for assignment],

  (refs.metric)("rio_scheduler_derivations_running"),
  [Gauge],
  [Derivations currently building],

  (refs.metric)("rio_scheduler_actor_cmd_seconds"),
  [Histogram],
  [Per-`ActorCommand` handling latency (labeled by `cmd`). The DAG actor is
    single-threaded --- a slow command head-of-line blocks every queued
    RPC. Alert on p99 > 1s sustained.],

  (refs.metric)("rio_scheduler_merge_phase_seconds"),
  [Histogram],
  [Per-phase MergeDag latency (labeled by `phase`). Decomposes
    `actor_cmd_seconds{cmd=MergeDag}`; a single phase >1s is the I-139
    signal (N sequential PG awaits in the actor).],

  (refs.metric)("rio_scheduler_build_duration_seconds"),
  [Histogram],
  [Total build duration],

  (refs.metric)("rio_scheduler_cache_hits_total"),
  [Counter],
  [Derivations served from cache (labeled by `source`: `scheduler`=TOCTOU
    check, `reprobe`=Poisoned reprobe, `existing`=pre-existing completed,
    `dispatch`=dispatch-time substitute hit)],

  (refs.metric)("rio_scheduler_cache_hit_deferred_total"),
  [Counter],
  [Cache-hit drvs deferred to Queued because their inputDrvs are not yet
    Completed (closure-race guard).],

  (refs.metric)("rio_scheduler_cache_check_failures_total"),
  [Counter],
  [Scheduler cache check (store FindMissingPaths) failures. Alert if rate >
    0 sustained: indicates store connectivity issue, every submission
    treated as 100% cache miss.],

  (refs.metric)("rio_scheduler_substitute_fetch_failures_total"),
  [Counter],
  [Substitutable-path eager fetches (QueryPathInfo) that failed. Path
    demoted to cache-miss; derivation falls through to normal dispatch.
    Alert if rate > 0 sustained: upstream reported path available but fetch
    failed --- upstream lying or transient network.],

  (refs.metric)("rio_scheduler_substitute_fetch_retries_total"),
  [Counter],
  [Transient substitute-fetch errors that triggered a backoff retry
    (`SUBSTITUTE_FETCH_BACKOFF`, up to 8 attempts). High rate without
    matching `_failures` = store load absorbed; high rate WITH `_failures`
    = backoff insufficient or store genuinely degraded.],

  (refs.metric)("rio_scheduler_substitute_spawned_total"),
  [Counter],
  [Derivations transitioned to `Substituting` (detached upstream fetch
    spawned, #rref("sched.substitute.detached")). Pairs with
    `substitute_fetch_failures_total` to derive success rate.],

  (refs.metric)("rio_scheduler_topdown_substitute_fail_total"),
  [Counter],
  [Top-down-pruned roots whose deferred substitute fetch failed
    (#rref("sched.merge.substitute-topdown")). Build failed fast with a
    resubmit-directing error; alert if rate > 0 sustained --- upstream HEAD
    probe is reporting substitutable for paths whose GET fails.],

  (refs.metric)("rio_scheduler_queue_backpressure"),
  [Counter],
  [Backpressure activations (queue reached 80% capacity)],

  (refs.metric)("rio_scheduler_workers_active"),
  [Gauge],
  [Fully-registered executors (stream + heartbeat)],

  (refs.metric)("rio_scheduler_assignments_total"),
  [Counter],
  [Total derivation→executor assignments],

  (refs.metric)("rio_scheduler_cleanup_dropped_total"),
  [Counter],
  [Terminal-build cleanup commands dropped due to channel backpressure.
    Alert if rate > 0 sustained: indicates memory leak under load.],

  (refs.metric)("rio_scheduler_transition_rejected_total"),
  [Counter],
  [State-machine transition rejections in the actor (labeled by `to` target
    state). Alert if rate > 0: these are defense-in-depth guards that
    should never fire; any non-zero rate indicates a race or logic bug.],

  (refs.metric)("rio_scheduler_malformed_built_output_total"),
  [Counter],
  [Worker-supplied `BuiltOutput.output_path` that failed `StorePath::parse`
    and was dropped at the `handle_completion` proto→domain boundary. Alert
    if rate > 0: indicates a buggy or compromised builder.],

  (refs.metric)("rio_scheduler_undeclared_built_output_total"),
  [Counter],
  [Worker-supplied `BuiltOutput.output_name` that is not in the
    derivation's scheduler-trusted `output_names` and was dropped at the
    `handle_completion` membership filter. Alert if rate > 0: indicates a
    buggy or compromised builder.],

  (refs.metric)("rio_scheduler_log_lines_forwarded_total"),
  [Counter],
  [Log lines forwarded via `BuildEvent::Log` (executor → scheduler → actor
    → gateway broadcast). Direct signal that the log pipeline's internal
    plumbing is live.],

  (refs.metric)("rio_scheduler_log_flush_total"),
  [Counter],
  [Successful S3 log flushes (labeled by `kind`: `final`/`periodic`).],

  (refs.metric)("rio_scheduler_log_flush_failures_total"),
  [Counter],
  [Failed log flushes (labeled by `phase`: `compress`/`s3`/`pg`,
    `is_final`: `true`/`false`). Alert if `is_final=true` rate > 0
    sustained: build logs are being lost (final flushes already drained the
    buffer).],

  (refs.metric)("rio_scheduler_log_flush_dropped_total"),
  [Counter],
  [Final-flush requests dropped due to flusher channel backpressure.
    Periodic tick will snapshot instead.],

  (refs.metric)("rio_scheduler_log_forward_dropped_total"),
  [Counter],
  [Live log forwards dropped due to actor channel backpressure. Lines
    remain in the ring buffer (serveable via AdminService) but the gateway
    misses the live stream. Sustained non-zero → actor is saturated.],

  (refs.metric)("rio_scheduler_log_unknown_drv_dropped_total"),
  [Counter],
  [LogBatch messages dropped because a single BuildExecution stream
    exceeded `MAX_DRVS_PER_STREAM` distinct `derivation_path` values.
    Non-zero → buggy or hostile worker (one drv per pod is the
    invariant).],

  (refs.metric)("rio_scheduler_executor_reconnect_rejected_total"),
  [Counter],
  [`BuildExecution` reconnects rejected by the stream-hijack guard. Label
    `reason ∈ {live_stream, intent_mismatch}`. Non-zero `intent_mismatch` →
    compromised or misconfigured builder presenting another intent's
    token.],

  (refs.metric)("rio_scheduler_heartbeat_rejected_total"),
  [Counter],
  [Heartbeats dropped by the actor-side identity binding. Label `reason ∈
    {intent_mismatch}`: token-attested `intent_id` ≠ target executor's
    stored `auth_intent` (#rref("sec.executor.identity-token")). Non-zero →
    compromised builder heartbeating as another executor.],

  (refs.metric)("rio_scheduler_critical_path_accuracy"),
  [Histogram],
  [Predicted vs. actual completion ratio (actual/estimated; 1.0 = perfect,
    >1.0 = underestimate)],

  (refs.metric)("rio_scheduler_resource_floor_bumps_total"),
  [Counter],
  [`resource_floor` doublings on explicit resource-exhaustion signals (D4;
    labeled `reason` = `oom_killed` | `disk_pressure` | `cgroup_oom` |
    `timeout` | `deadline_exceeded`; #rref("sched.sla.reactive-floor")).
    This IS the under-provisioning resize-retry signal (`_prediction_ratio`
    is blind to censored samples); the previously-documented
    `_sla_resize_retry_total` was never emitted and is removed. Frequent
    firing for one pname = raise `[sla].probe` defaults.],

  (refs.metric)("rio_scheduler_queue_depth"),
  [Gauge],
  [Deferred Ready derivations per executor kind (labeled `kind` = builder |
    fetcher; snapshot per dispatch pass). Sustained nonzero → scale the
    matching pool.],

  (refs.metric)("rio_scheduler_utilization"),
  [Gauge],
  [Fraction of executors currently running a build (`busy/total`; labeled
    `kind` = builder | fetcher; per dispatch pass).],

  (refs.metric)("rio_scheduler_unroutable_ready"),
  [Gauge],
  [Ready derivations whose `system` is advertised by zero registered
    executors of the matching kind (labeled by `system`; snapshot per
    dispatch pass). Nonzero = no pool exists for that system --- add it to
    a `Pool`'s `systems` list. The scheduler also WARNs once when a system
    first becomes unroutable (#rref("sched.dispatch.unroutable-system")).
    Label values not matching `[a-z0-9_-]{1,32}` are bucketed as `unknown`
    (tenant-supplied; bounds cardinality).],

  (refs.metric)("rio_scheduler_cache_check_circuit_open_total"),
  [Counter],
  [Circuit-breaker open transitions (store unreachable for 5 consecutive
    cache checks). Alert if rate > 0: scheduler is rejecting SubmitBuild
    with `UNAVAILABLE` until a half-open probe succeeds or 100 s elapse
    (#rref("sched.breaker.cache-check")).],

  (refs.metric)("rio_scheduler_phantom_assignments_drained_total"),
  [Counter],
  [Phantom-assignment drains: scheduler-kept running build absent from the
    executor's heartbeat report across two consecutive heartbeats → reset
    to Ready (#rref("sched.heartbeat.phantom-drain")). Non-zero after a
    scheduler restart is normal; sustained non-zero on a stable leader =
    executor/heartbeat desync.],

  (refs.metric)("rio_scheduler_heartbeat_adoptions_total"),
  [Counter],
  [Heartbeat-reported running builds the scheduler had no record of →
    adopted into both `executor.running_build` and the DAG node
    (#rref("sched.heartbeat.adopt")). Expected non-zero immediately after
    recovery; sustained non-zero otherwise = scheduler is dropping its own
    assignment state.],

  (refs.metric)("rio_scheduler_heartbeat_adopt_conflicts_total"),
  [Counter],
  [Heartbeat adoption rejected because the derivation is already Running on
    a _different_ executor. Alert if > 0: split-brain dispatch (two
    executors building the same drv).],

  (refs.metric)("rio_scheduler_prefetch_hints_sent_total"),
  [Counter],
  [PrefetchHint messages sent (one per assignment with paths to warm).
    Missing from a dispatch = leaf drv (no DAG children to prefetch).],

  (refs.metric)("rio_scheduler_prefetch_paths_sent_total"),
  [Counter],
  [Total paths in sent PrefetchHints. Divide by `hints_sent` for avg
    paths-per-hint.],

  (refs.metric)("rio_scheduler_warm_gate_fallback_total"),
  [Counter],
  [`best_executor()` fell back to cold executors because no warm executor
    passed the hard filter. Expected nonzero on single-executor clusters
    and mass scale-up; sustained high rate = executors never warming
    (prefetch broken).],

  (refs.metric)("rio_scheduler_warm_prefetch_paths"),
  [Histogram],
  [Paths fetched per initial warm-gate PrefetchHint (from the executor's
    PrefetchComplete ACK). `0` = executor already warm (cache hit on
    everything); high = fresh executor cold-fetched everything.],

  (refs.metric)("rio_scheduler_event_persist_dropped_total"),
  [Counter],
  [BuildEvents dropped from PG persister (channel backpressure). Broadcast
    still live; only mid-backlog reconnect loses it. Alert if rate > 0
    sustained.],

  (refs.metric)("rio_scheduler_lease_acquired_total"),
  [Counter],
  [Kubernetes Lease acquire transitions (standby → leader). _Internal ---
    primary use is VM test observability._],

  (refs.metric)("rio_scheduler_lease_lost_total"),
  [Counter],
  [Kubernetes Lease loss transitions (leader → standby). _Internal ---
    non-zero on a single-replica deployment is a bug._],

  (refs.metric)("rio_scheduler_recovery_total"),
  [Counter],
  [State recovery runs (on LeaderAcquired). Labeled by `outcome`:
    `success`/`failure`/`discarded_flap`.],

  (refs.metric)("rio_scheduler_recovery_duration_seconds"),
  [Histogram],
  [Time to reload non-terminal builds/derivations from PostgreSQL. Labeled
    by `outcome`: `success`/`failure`.],

  (refs.metric)("rio_scheduler_reconcile_dropped_total"),
  [Counter],
  [Post-recovery `ReconcileAssignments` command dropped because the actor
    channel was full. Assigned-but-executor-gone derivations leak until the
    next recovery pass. Rare (channel is 1024-deep); alert if > 0.],

  (refs.metric)("rio_scheduler_backstop_timeouts_total"),
  [Counter],
  [Running derivations reset to Ready by the backstop timeout
    (running_since > max(est_duration×3, daemon_timeout+10m)). Non-zero
    indicates wedged executors.],

  (refs.metric)("rio_scheduler_build_timeouts_total"),
  [Counter],
  [Builds failed by per-build wall-clock timeout
    (`BuildOptions.build_timeout` seconds since submission). Distinct from
    `backstop_timeouts_total` (per-derivation heuristic).],

  (refs.metric)("rio_scheduler_worker_disconnects_total"),
  [Counter],
  [BuildExecution stream closures (executor gone). Triggers reassignment.],

  (refs.metric)("rio_scheduler_cancel_signals_total"),
  [Counter],
  [CancelSignal messages successfully delivered to the executor stream (Ok
    on `try_send`). Excludes drops (counted in `cancel_signal_dropped_total`)
    and skipped attempts. Sources: explicit CancelBuild, backstop timeout,
    per-build timeout, finalizer drain.],

  (refs.metric)("rio_scheduler_cancel_signal_dropped_total"),
  [Counter],
  [CancelSignal `try_send` drops (executor stream full/closed under
    backpressure). Best-effort: the transition to Cancelled is
    scheduler-authoritative regardless; the executor's next heartbeat
    reconcile cleans it up. Alert if rate > 0 sustained.],

  (refs.metric)("rio_scheduler_sla_refit_total"),
  [Counter],
  [SLA estimator refresh ticks (≈60s cadence). _Internal --- VM test sync
    signal._],

  (refs.metric)("rio_scheduler_derivations_gc_deleted_total"),
  [Counter],
  [Orphan-terminal `derivations` rows deleted by the periodic Tick sweep
    (I-169.2). Nonzero rate is normal; a sustained 1000/tick saturation
    means the backlog hasn't drained yet.],

  (refs.metric)("rio_scheduler_build_graph_edges"),
  [Histogram],
  [Edge count per `GetBuildGraph` response. Bounded by the induced subgraph
    over the node-cap (≤5000 nodes); a high p99 (>10k) means unusually
    dense DAGs. Suggested buckets: `[100, 500, 1000, 5000, 10000, 20000]`.],

  (refs.metric)("rio_scheduler_ca_hash_compares_total"),
  [Counter],
  [CA cutoff-compare output-hash lookups against the content index on
    completion (labeled by `outcome`: `match`, `miss`, `skipped_after_miss`,
    `malformed`, `error`). High match ratio → CA derivations rebuilding
    identical content. `skipped_after_miss` counts outputs NOT looked up
    because an earlier output in the same derivation missed (short-circuit);
    the compare loop breaks early since the AND-fold result is already
    false. `malformed` = executor sent an empty output path; `error` = PG
    lookup failed or timed out (alert if rate>0).],

  (refs.metric)("rio_scheduler_ca_cutoff_saves_total"),
  [Counter],
  [Derivations skipped via CA early-cutoff (Queued→Skipped transitions).
    Each increment is one build that did NOT run because a CA dep's output
    matched the content index.],

  (refs.metric)("rio_scheduler_ca_cutoff_seconds_saved"),
  [Counter],
  [Sum of `est_duration` of skipped derivations. Lower-bound estimate of
    wall-clock saved (est_duration is SlaEstimator T_min; a never-run
    derivation has the fallback, not actual). Divide by `saves_total` for
    avg-seconds-per-save.],

  (refs.metric)("rio_scheduler_actor_mailbox_depth"),
  [Gauge],
  [`ActorCommand` mpsc queue depth, sampled once per dequeued command. The
    actor is single-threaded --- depth growth means commands arrive faster
    than the loop retires them. Pair with `actor_cmd_seconds` to localize a
    wedge: high depth + one slow `cmd` label = head-of-line block; high
    depth + uniformly fast cmds = sustained burst.],

  (refs.metric)("rio_scheduler_dispatch_wait_seconds"),
  [Histogram],
  [Time from a derivation entering Ready to being Assigned (fed from
    `DerivationState.ready_at`). With ephemeral builders, dominated by
    node-provision (\~60--180s on EKS).],

  (refs.metric)("rio_scheduler_broadcast_lagged_total"),
  [Counter],
  [BuildEvent broadcast events skipped by lagging subscribers (sum of
    `RecvError::Lagged(n)` across all bridge tasks). Non-zero under
    sustained event burst --- large DAG initial dispatch, or many
    concurrent drvs emitting Log lines, and the gateway can't drain the
    1024-slot ring fast enough. The bridge continues post-lag (I-144); the
    gap is recoverable via S3 logs / WatchBuild reconnect.],

  (refs.metric)("rio_scheduler_ca_cutoff_depth_cap_hits_total"),
  [Counter],
  [CA cutoff cascade walks that hit `MAX_CASCADE_NODES` (1000). Non-zero →
    cascades truncated; pathological DAG shape or cap too low.],

  (refs.metric)("rio_scheduler_sla_prediction_ratio"),
  [Histogram],
  [ADR-023: actual/predicted, labeled `dim` = `wall` | `mem`. 1.0=perfect;
    >1.0=under-predicted.],

  (refs.metric)("rio_scheduler_sla_envelope_result_total"),
  [Counter],
  [ADR-023: SLA envelope hit/miss per tier (labeled `tier`, `result` =
    `hit` | `miss`).],

  (refs.metric)("rio_scheduler_sla_infeasible_total"),
  [Counter],
  [ADR-023: infeasible-at-any-tier (labeled `reason` = `serial_floor` |
    `mem_ceiling` | `disk_ceiling` | `core_ceiling` | `interrupt_runaway` |
    `capacity_exhausted`). The reason names which constraint bound at the
    loosest tier: `serial_floor` = `S` alone breaches the bound (no `c`
    helps); `{mem,disk,core}_ceiling` = the respective `sla.max*` ceiling
    rejected the feasible `c*`; `interrupt_runaway` = preemption rate `λ`
    makes *every spot cell* infeasible (OD may have failed for an unrelated
    reason --- that reason is reported separately via `classify_ceiling`);
    `capacity_exhausted` = all `(hw_class, cap)` cells ICE-masked at the
    terminal tier.],

  (refs.metric)("rio_scheduler_sla_suspicious_scaling_total"),
  [Counter],
  [ADR-023: exploration froze at `max_cores` still saturated (labeled
    `tenant`). The build wants more cores than the cluster offers.],

  (refs.metric)("rio_scheduler_sla_outlier_rejected_total"),
  [Counter],
  [ADR-023: MAD-rejected `build_samples` rows (labeled `tenant`). Row stays
    in PG (`outlier_excluded=TRUE`) for forensics; refit excludes it
    (#rref("sched.sla.outlier-mad-reject")).],

  (refs.metric)("rio_scheduler_sla_mem_fit_weak_total"),
  [Counter],
  [ADR-023: `M(c)` Koenker-Machado pseudo-R¹ < 0.7 → fell back to
    independent recency-weighted p90 (labeled `tenant`).],

  (refs.metric)("rio_scheduler_sla_prior_divergence"),
  [Gauge],
  [ADR-023: fleet-median prior parameter ÷ operator-probe basis (labeled
    `param`); outside `[0.5, 2.0]` ⇒ clamped to the band edge
    (#rref("sched.sla.prior-partial-pool")). Set every refit, so an in-band
    value clears the alert.],

  (refs.metric)("rio_scheduler_sla_hw_cost_stale_seconds"),
  [Gauge],
  [ADR-023: age of the hw-band `$/vCPU·hr` snapshot. Climbs when the
    lease-gated spot-price poller is failing or this replica is standby.
    Not emitted under `sla.hwCostSource: static` / unset (no live source to
    be stale relative to).],

  (refs.metric)("rio_scheduler_sla_hw_ladder_exhausted_total"),
  [Counter],
  [ADR-023: ICE-mask hardware ladder exhausted at the terminal tier with no
    admissible `(hw_class, cap)` cell left (labeled `tenant`, `exit` = the
    cell the ladder gave up on). Replaces the retired
    `_ice_backoff_total`.],

  (refs.metric)("rio_scheduler_sla_hw_cost_unknown_total"),
  [Counter],
  [ADR-023: solve hit a `(hw_class, cap)` cell the cost table has no
    `$/vCPU·hr` for; the cell is dropped from the admissible set (labeled
    `tenant`). Sustained nonzero ⇒ `sla.hwClasses` config drifted from the
    cost-poller's instance-type menu.],

  (refs.metric)("rio_scheduler_sla_hw_cost_fallback_total"),
  [Counter],
  [ADR-023: cost-poller fell back from a live spot-price source (labeled
    `reason` = `api_error` | `empty_history` | `parse` | `stale`). `stale`:
    `_hw_cost_stale_seconds > 6 × pollInterval` → `price_ratio[h]` clamped
    to the helm seed.],

  (refs.metric)("rio_scheduler_sla_als_round_cap_hit_total"),
  [Counter],
  [ADR-023: ALS alternation hit the round cap (`ALS_MAX_ROUNDS`, currently
    100) without `‖Δα‖₁ < ALS_DELTA_TOL` (currently `10⁻³`) convergence
    (labeled `tenant`). The α/`T_ref(c)` joint fit failed to converge --- α
    is the final iterate, not a fixed point. Sustained ⇒ investigate
    fixture/rank-gate degeneracy, NOT cap tuning (the cap is already a
    worst-case budget, see `alpha.rs`). _Replaces `_als_cap_hit_total`
    (removed; pre-production, no alias)._],

  (refs.metric)("rio_scheduler_sla_keys_evicted_total"),
  [Counter],
  [ADR-023: per-tenant LRU evicted a `(pname, system)` key at
    `sla.maxKeysPerTenant` (labeled `tenant`). Nonzero ⇒ a tenant is
    exhausting the fit-cache cap; check for random-pname submissions
    (#rref("sched.sla.threat.corpus-clamp")).],

  (refs.metric)("rio_scheduler_sla_class_ceiling_uncatalogued"),
  [Gauge],
  [§13c-2: 1 per hwClass with no boot-derived catalog ceiling (labeled
    `hw_class`). Always 1 for every class under `hwCostSource: static` (no
    AWS API, expected). Under `spot` a persistent 1 ⇒
    `describe_instance_types` failed at boot (check IRSA), or the class's
    `requirements` match 0 types in the deployment region. Class falls to
    the global ceiling --- over-permits, never over-strips
    (#rref("scheduler.sla.ceiling.uncatalogued-fallback")).],

  (refs.metric)("rio_scheduler_sla_forecast_dropped_total"),
  [Counter],
  [§13b: forecast-pass intent dropped before emit, labeled `reason` ∈
    {`lead_horizon`, `tenant_budget`}. Debounced once per
    `(drv_hash, reason)` per LRU residency, so the rate tracks _unique drop
    events_ --- not poll cadence. `lead_horizon`: the intent's
    dep-completion ETA exceeds its *per-intent* forecast horizon
    (`max(lead_time_seed[(h,cap)])` over hwClasses `class_routes` admits
    pre-solve, over `intent.hw_class_names` post-solve) --- the scheduler's
    _seed-based approximation_ of the controller's `a_open` would drop
    every cell (`eta ≥ lead_time(c)`), so the intent was unactionable. The
    controller reads a learned per-cell DDSketch quantile with no return
    channel to the scheduler, so when learned drifts above the seed this
    metric over-counts (the controller would have pre-warmed).
    `tenant_budget`: `max_forecast_cores_per_tenant` exhausted by Ready
    cores + higher-priority forecast intents this poll. Sustained
    `lead_horizon` is usually fine --- deps complete far ahead of any seed
    lead, and the forecast pass saved the `solve_intent_for` work. With a
    sustained pre-warm latency regression, check if a routable hwClass is
    missing from `lead_time_seed`. Sustained `tenant_budget` ⇒ Ready
    frontier saturates the per-tenant cap; the forecast pass cannot
    pre-warm.],

  (refs.metric)("rio_scheduler_sla_residual_multimodal_total"),
  [Counter],
  [ADR-023: Hartigan dip test rejected unimodality (`p<0.05`) on a key's
    log-residuals (labeled `tenant`). The single-curve `T(c)` model is
    wrong --- likely two workloads sharing a `pname`.],

  (refs.metric)("rio_scheduler_hung_detect_skipped_no_authoritative_total"),
  [Counter],
  [#rref("sched.admin.hung-node-detector+3"): busy executors skipped by
    `detect_hung_nodes` because no controller-reported `spec.nodeName`
    binding exists yet (controller-lag, `AckSpawnedIntents` channel down).
    Sustained nonzero with `_nodeclaim_reaped_total{reason=dead}` flat ⇒
    check rio-controller `nodeclaim_pool` reconcile loop.],

  (refs.metric)("rio_scheduler_unroutable_features_total"),
  [Counter],
  [§13c: `solve_intent_for` found NO hwClass whose `provides_features`
    hosts the drv's `required_features` (labeled `tenant`). Debounced once
    per `(tenant, required_features)` edge. The intent is unroutable until
    a `[sla.hw_classes.$h]` with matching `providesFeatures` is added; the
    companion `WARN` names which features. The metric carries no
    per-feature label --- `requiredSystemFeatures` is tenant-controlled and
    unbounded.],

  (refs.metric)("rio_scheduler_features_stripped_total"),
  [Counter],
  [§13e+r35: `EffectiveFeatures::derive` stripped the declared
    `requiredSystemFeatures` at the FOD↔Fetcher chokepoint (labeled `reason
    ∈ {non_fod_fetcher, fod_declared_features}`). `non_fod_fetcher`: a
    non-FOD declared `fetcher` (rio-internal routing tag, not a
    tenant-declarable system feature) --- the drv routes to a Builder Pool.
    `fod_declared_features`: a FOD declared extraneous features (e.g.
    `kvm`) --- the drv routes to a Fetcher Pool regardless. The strip
    produces correct routing; the companion `WARN` (debounced once per
    `(pname, reason)` edge) names the declared vs effective set so the
    tenant can fix the misconfig. The metric carries no `pname`/`feature`
    label --- both are tenant-controlled and unbounded.],
)

== Store <tbl-metrics-store>

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Metric], [Type], [Description]),
  (refs.metric)("rio_store_put_path_total"),
  [Counter],
  [Total PutPath operations (per store path; PutPathBatch counts each
    output)],

  (refs.metric)("rio_store_putpath_retries_total"),
  [Counter],
  [PutPath retriable rejections (labeled by `reason`: `serialization`,
    `deadlock`, `placeholder_missing`, `connection`, `resource_exhausted`,
    `concurrent_upload`). Client retries on `aborted`/`unavailable`.
    Sustained high `deadlock`/`connection` rate = PG-side problem. GC no
    longer blocks PutPath (I-192).],

  (refs.metric)("rio_store_put_path_duration_seconds"),
  [Histogram],
  [PutPath latency],

  (refs.metric)("rio_store_integrity_failures_total"),
  [Counter],
  [GetPath content integrity check failures (bitrot/corruption)],

  (refs.metric)("rio_store_chunk_dedup_ratio"),
  [Gauge],
  [Per-upload dedup ratio (1.0 - missing/total after chunking)],

  (refs.metric)("rio_store_s3_requests_total"),
  [Counter],
  [S3 API calls (labeled by operation)],

  (refs.metric)("rio_store_chunk_cache_hits_total"),
  [Counter],
  [moka chunk cache hits (for cross-instance aggregation)],

  (refs.metric)("rio_store_chunk_cache_misses_total"),
  [Counter],
  [moka chunk cache misses],

  (refs.metric)("rio_store_hmac_rejected_total"),
  [Counter],
  [PutPath calls rejected by HMAC verifier (bad signature, expired, path
    not in `expected_outputs`). Alert if rate > 0: indicates
    misconfiguration or compromise attempt.],

  (refs.metric)("rio_store_service_token_accepted_total"),
  [Counter],
  [PutPath calls that skipped HMAC verification via `x-rio-service-token`
    (labeled by `caller`). Transport-agnostic replacement for the CN
    bypass.],

  (refs.metric)("rio_store_gc_sweep_paths_remaining"),
  [Gauge],
  [Paths not yet processed by the in-progress GC sweep. Ticks down per
    batch commit (100 paths); `0` between sweeps. Long-tail at non-zero =
    sweep stalled or PG slow.],

  (refs.metric)("rio_store_gc_path_resurrected_total"),
  [Counter],
  [Paths skipped by GC sweep because a reference appeared between mark and
    sweep (sweep's per-path reference re-check caught it).],

  (refs.metric)("rio_store_gc_chunk_resurrected_total"),
  [Counter],
  [S3 deletes skipped by the drain task because chunk refcount re-check
    found the chunk back in use (TOCTOU guard via
    `pending_s3_deletes.blake3_hash`).],

  (refs.metric)("rio_store_gc_path_swept_total"),
  [Counter],
  [Paths deleted by GC sweep (`narinfo` DELETE + CASCADE). Monotonic over
    store lifetime; `rate()` ≈ GC throughput. Not incremented on dry-run.],

  (refs.metric)("rio_store_gc_s3_key_enqueued_total"),
  [Counter],
  [S3 keys enqueued to `pending_s3_deletes` by GC sweep (chunks that hit
    refcount=0). Gap vs #(refs.metric)("rio_store_s3_deletes_pending") gauge
    decreasing = drain keeping up.],

  (refs.metric)("rio_store_gc_chunk_orphan_swept_total"),
  [Counter],
  [Standalone chunks reaped by `sweep_orphan_chunks` after the grace-TTL
    expired (PutChunk at refcount=0, no subsequent PutPath claimed them).
    Nonzero indicates executors crashing mid-upload; sustained high
    suggests a client-side chunker bug.],

  (refs.metric)("rio_store_sign_empty_refs_total"),
  [Counter],
  [SignPath requests for non-CA paths with zero references. Suspicious for
    non-leaf derivations --- GC cannot protect dependencies without the ref
    graph. Check executor ref-scanner if sustained.],

  (refs.metric)("rio_store_s3_deletes_pending"),
  [Gauge],
  [Rows in `pending_s3_deletes` with `attempts < 10`. Normal operation:
    near-zero.],

  (refs.metric)("rio_store_s3_deletes_stuck"),
  [Gauge],
  [Rows in `pending_s3_deletes` with `attempts >= 10` (max retries
    exhausted). Alert if > 0: manual investigation needed.],

  (refs.metric)("rio_store_put_path_bytes_total"),
  [Counter],
  [Bytes accepted via PutPath (nar_size on success)],

  (refs.metric)("rio_store_get_path_bytes_total"),
  [Counter],
  [Bytes served via GetPath (nar_size on stream start)],

  (refs.metric)("rio_store_get_path_total"),
  [Counter],
  [Total GetPath operations (incremented on successful whole-NAR verify)],

  (refs.metric)("rio_store_get_path_duration_seconds"),
  [Histogram],
  [GetPath latency (stream_path entry to whole-NAR verify)],

  (refs.metric)("rio_store_get_path_active"),
  [Gauge],
  [GetPath body-stream tasks currently writing (drives SIGTERM
    stream-drain)],

  (refs.metric)("rio_store_substitute_total"),
  [Counter],
  [Upstream substitution attempts, labeled by `result` (hit/miss/error) and
    `tenant` (UUID). Per-upstream debugging detail is in the
    `debug!`/`warn!` log lines.],

  (refs.metric)("rio_store_substitute_integrity_failures_total"),
  [Counter],
  [Upstream substitution NAR hash or size mismatches, labeled by `tenant`.
    Nonzero is security-relevant: upstream served corrupt/tampered bytes or
    a lying `NarSize`.],

  (refs.metric)("rio_store_substitute_probe_cache_hits_total"),
  [Counter],
  [`check_available` HEAD-probe cache hits (positive or negative cached
    result; no upstream HEAD made for this path).],

  (refs.metric)("rio_store_substitute_probe_cache_misses_total"),
  [Counter],
  [`check_available` HEAD-probe cache misses (path was uncached; an
    upstream HEAD was issued).],

  (refs.metric)("rio_store_substitute_probe_ratelimited_total"),
  [Counter],
  [Upstream HEAD/GET probes that returned 429, labeled by `tenant` (NOT by
    upstream URL --- tenant-supplied URLs are unbounded cardinality). The
    rate-limited subset is retried (≤3 passes) after honoring
    `Retry-After`; concurrency halves when >10% of a pass 429s.],

  (refs.metric)("rio_store_check_available_duration_seconds"),
  [Histogram],
  [`check_available` wall-clock (HEAD-probe phase of `FindMissingPaths`).
    `⌈N_uncached/128⌉ × RTT` plus any 429 retry sleeps. p99 informs the
    scheduler's `MERGE_FMP_TIMEOUT`.],

  (refs.metric)("rio_store_substitute_stale_reclaimed_total"),
  [Counter],
  [Stale `'uploading'` placeholders reclaimed on the substitution hot path
    (crashed prior fetch left the placeholder; `try_substitute` deleted +
    re-inserted rather than waiting for the 15-minute orphan sweep).
    Nonzero is expected under network churn; sustained high suggests
    upstream instability or aggressive pod rollouts.],

  (refs.metric)("rio_store_substitute_admission_utilization"),
  [Gauge],
  [`try_substitute` admission-gate utilization:
    `(capacity − available_permits) / capacity`. Updated on each acquire
    and each `GetLoad` call. Can saturate independently of
    `pg_pool_utilization` (upstream HTTP bottleneck --- permit held across
    the NAR fetch, PG connection released per-query). Folded into the
    ComponentScaler's per-pod load via `max(pg, this)`.],

  (refs.metric)("rio_store_substitute_admission_rejected_total"),
  [Counter],
  [`try_substitute` calls rejected with `RESOURCE_EXHAUSTED` after waiting
    `SUBSTITUTE_ADMISSION_WAIT` (25 s) for a permit. Sustained non-zero =
    genuine per-replica overload; the ComponentScaler should already be
    reacting via the utilization gauge.],

  (refs.metric)("rio_store_pg_pool_utilization"),
  [Gauge],
  [PG connection-pool utilization: `(size - num_idle) / max_connections`.
    Updated on each `StoreAdminService.GetLoad` call (ComponentScaler 10s
    tick). Sustained > 0.8 = under-provisioned store replicas (I-105 cliff
    approaching); the ComponentScaler reacts at 0.8 with an immediate +1
    and ratio decay.],
)

== Builder <tbl-metrics-builder>

#info[
  Per ADR-019 §Observability, the former `rio_worker_*` metrics are now
  `rio_builder_*`. The scheduler-side
  #(refs.metric)("rio_scheduler_queue_depth")`{kind}` and
  #(refs.metric)("rio_scheduler_utilization")`{kind}` gauges track the
  builder/fetcher split.
]

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Metric], [Type], [Description]),
  (refs.metric)("rio_builder_builds_total"),
  [Counter],
  [Total builds executed (labeled by `outcome`: `success`, `failure`,
    `cancelled`, `timed_out`, `log_limit`, `infra_failure`)],

  (refs.metric)("rio_builder_builds_active"),
  [Gauge],
  [Currently running builds on this executor],

  (refs.metric)("rio_builder_uploads_total"),
  [Counter],
  [Output uploads (labeled by `status`)],

  (refs.metric)("rio_builder_build_duration_seconds"),
  [Histogram],
  [Per-derivation build time],

  (refs.metric)("rio_builder_fuse_jit_lookup_total"),
  [Counter],
  [Top-level FUSE lookup outcomes under JIT fetch (labeled by `outcome`:
    `reject` = not in input set, fast ENOENT, no store contact; `fetch` =
    registered input materialized; `eio` = registered input fetch failed →
    EIO so overlay can't negative-cache). `reject`/`fetch` ratio ≈ closure
    utilization; `eio` nonzero = store degraded.],

  (refs.metric)("rio_builder_jit_inputs_registered"),
  [Gauge],
  [Size of the JIT FUSE allowlist (`known_inputs.len()`) at daemon spawn.],

  (refs.metric)("rio_builder_log_lines_suppressed_total"),
  [Counter],
  [Log lines dropped by `log_rate_limit` suppression. Build continues with
    a `[rio: N lines suppressed …]` marker. Nonzero is normal for bursty
    builds (kernel `oldconfig`, autoconf); sustained high rate =
    pathological producer.],

  (refs.metric)("rio_builder_cgroup_oom_total"),
  [Counter],
  [Builds killed by the cgroup OOM watcher (`memory.events` `oom_kill`
    incremented during build → `cgroup.kill` + `InfrastructureFailure` for
    scheduler reactive-floor promotion, I-196). Nonzero = the SLA model's
    mem fit is undersized for this `pname`.],

  (refs.metric)("rio_builder_input_materialization_failures_total"),
  [Counter],
  [Daemon `MiscFailure` reclassified as `InfrastructureFailure` because the
    missing path is in the build's input closure (I-178 safety net).
    Sustained nonzero = `JIT_MIN_THROUGHPUT_BPS` is set above actual
    store→builder throughput.],

  (refs.metric)("rio_builder_fuse_cache_hits_total"),
  [Counter],
  [FUSE cache hits],

  (refs.metric)("rio_builder_fuse_cache_misses_total"),
  [Counter],
  [FUSE cache misses],

  (refs.metric)("rio_builder_fuse_fetch_duration_seconds"),
  [Histogram],
  [Store path fetch latency],

  (refs.metric)("rio_builder_fuse_fallback_reads_total"),
  [Counter],
  [Successful userspace `read()` callbacks. Near-zero when passthrough is
    on (kernel handles reads directly); nonzero when `fuse_passthrough=false`
    or passthrough failed for specific files.],

  (refs.metric)("rio_builder_fuse_index_divergence_total"),
  [Counter],
  [FUSE cache index/disk divergences self-healed. Nonzero = something rm'd
    cache files under the SQLite index (debugging, interrupted eviction).
    Investigate if sustained.],

  (refs.metric)("rio_builder_overlay_teardown_failures_total"),
  [Counter],
  [Overlay unmount failures (leaked mount). Alert if rate > 0: indicates
    resource leak on executor.],

  (refs.metric)("rio_builder_prefetch_total"),
  [Counter],
  [PrefetchHint outcomes (labeled by `result`: `fetched`, `already_cached`,
    `already_in_flight`, `not_input`, `size_cap`, `error`, `malformed`,
    `panic`).],

  (refs.metric)("rio_builder_prefetch_filtered_total"),
  [Counter],
  [PrefetchHint paths skipped before fetch by the warm-gate filter (labeled
    by `reason`: `not_input` = JIT allowlist armed and path not declared;
    `size_cap` = unarmed warm-gate batch and `nar_size > 256 MiB`). I-212:
    stops speculative pull of multi-GB sibling outputs the scheduler
    over-includes.],

  (refs.metric)("rio_builder_upload_bytes_total"),
  [Counter],
  [Bytes uploaded to store via PutPath (nar_size on success)],

  (refs.metric)("rio_builder_upload_skipped_idempotent_total"),
  [Counter],
  [Outputs skipped before upload because `FindMissingPaths` reports them
    already-present in the store. Idempotency short-circuit --- nonzero is
    healthy (repeat builds of cached paths).],

  (refs.metric)("rio_builder_fuse_circuit_open"),
  [Gauge],
  [FUSE circuit-breaker open state (1 = open/tripped, 0 = closed/healthy).
    Set to 1 when store fetch error rate exceeds threshold; FUSE ops return
    EIO instead of blocking. Reset to 0 on successful probe. Alert if
    sustained 1.],

  (refs.metric)("rio_builder_upload_references_count"),
  [Histogram],
  [Reference count per output upload (`references.len()` after NAR scan).
    Distribution of dependency fan-out. Zero-heavy = mostly leaves; high
    p99 = wide transitive closures. Buckets:
    `[1, 5, 10, 25, 50, 100, 250, 500]`.],

  (refs.metric)("rio_builder_fuse_fetch_bytes_total"),
  [Counter],
  [Bytes fetched from store via FUSE cache misses],

  (refs.metric)("rio_builder_cpu_fraction"),
  [Gauge],
  [Executor cgroup CPU utilization: delta `cpu.stat usage_usec` / wall-clock
    µs. 1.0 = one core fully used; >1.0 on multi-core. Directly comparable
    to cgroup `cpu.max` limits.],

  (refs.metric)("rio_builder_memory_fraction"),
  [Gauge],
  [Executor cgroup memory utilization: `memory.current` / `memory.max`. 0.0
    if `memory.max` is `"max"` (unbounded).],

  (refs.metric)("rio_builder_stale_assignments_rejected_total"),
  [Counter],
  [WorkAssignments rejected by the generation fence (assignment.generation
    < latest heartbeat-observed generation). Nonzero only during leader
    failover split-brain; sustained nonzero = deposed scheduler replica
    still dispatching.],

  (refs.metric)("rio_builder_cgroup_leak_total"),
  [Counter],
  [Per-build cgroup `rmdir` failures on Drop (typically `EBUSY` ---
    processes still in the tree). Leaked cgroups are harmless empty
    directories; pod restart clears the whole subtree. Alert if rate > 0
    sustained: indicates process-kill sequencing bug.],
)

#info(title: [Note on ratio metrics])[
  For aggregatable cache metrics, use counter pairs (e.g.,
  #(refs.metric)("rio_store_chunk_cache_hits_total") +
  #(refs.metric)("rio_store_chunk_cache_misses_total")) and compute ratios at
  query time with PromQL's `rate()`. Pre-computed gauge ratios lose meaning
  when averaged across instances. Exception:
  #(refs.metric)("rio_store_chunk_dedup_ratio") is a per-upload event gauge
  (last-written-wins, not averaged) --- useful for eyeballing recent PutPath
  dedup effectiveness but NOT for cross-instance aggregation.
]

== Controller <tbl-metrics-controller>

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Metric], [Type], [Description]),
  (refs.metric)("rio_controller_reconcile_duration_seconds"),
  [Histogram],
  [Reconcile loop latency (labeled by reconciler)],

  (refs.metric)("rio_controller_reconcile_errors_total"),
  [Counter],
  [Reconcile errors (labeled by reconciler)],

  (refs.metric)("rio_controller_scaling_decisions_total"),
  [Counter],
  [Scaling decisions (labeled by direction: up/down)],

  (refs.metric)("rio_controller_ephemeral_jobs_reaped_total"),
  [Counter],
  [Excess Pending ephemeral Jobs deleted (labeled by `pool`). Non-zero rate
    = queued dropped after spawn (user cancel, gateway disconnect); zero
    rate with stuck Pending pods = reap not firing (check RBAC `delete` on
    `batch/jobs`).],

  (refs.metric)("rio_controller_orphan_jobs_reaped_total"),
  [Counter],
  [Running ephemeral Jobs deleted after orphan grace with no scheduler
    assignment (labeled by `pool`). Non-zero rate = builders stuck unable
    to self-exit (I-165 D-state FUSE wait, OOM-loop); investigate
    node/kernel health.],

  (refs.metric)("rio_controller_gc_runs_total"),
  [Counter],
  [GC cron runs. `result = success | connect_failure | rpc_failure`.
    `connect_failure` = store unreachable (pod down, stale IP);
    `rpc_failure` = TriggerGC error or progress stream aborted.],

  (refs.metric)("rio_controller_disruption_drains_total"),
  [Counter],
  [DisruptionTarget watcher DrainExecutor calls. `result = sent | timeout |
    rpc_error`. Zero rate while evictions occur = watcher dead, falling
    back to SIGTERM self-drain.],

  (refs.metric)("rio_controller_component_scaler_learned_ratio"),
  [Gauge],
  [ComponentScaler learned `builders_per_replica` (labelled by
    `cs=ns/name`). EMA-adjusted against observed PG-pool load; persisted in
    `.status.learnedRatio`.],

  (refs.metric)("rio_controller_component_scaler_desired_replicas"),
  [Gauge],
  [ComponentScaler desired replica count (labelled by `cs=ns/name`). What
    was last patched onto `deployments/scale`.],

  (refs.metric)("rio_controller_component_scaler_observed_load"),
  [Gauge],
  [ComponentScaler max of pg-pool utilization and substitute-admission
    utilization across `loadEndpoint` pods at the last tick (labelled by
    `cs=ns/name`).],

  (refs.metric)("rio_controller_nodeclaim_reaped_total"),
  [Counter],
  [nodeclaim_pool NodeClaim deletions (labeled by `reason` × `cell`).
    `reason=idle`: NA-consolidate break-even; `reason=ice`: `Launched=False`
    (timeout or terminal `LaunchFailed` reason); `reason=boot-timeout`:
    `Launched=True ∧ Registered=False` past timeout; `reason=dead`:
    scheduler-reported hung node; `reason=vanished`: in-flight claim
    Karpenter-GC'd between ticks.],

  (refs.metric)("rio_controller_nodeclaim_created_total"),
  [Counter],
  [nodeclaim_pool NodeClaim `Api::create` successes (labeled by `cell`).
    `Σrate(created) − Σrate(reaped)` over a window ≈ fleet growth;
    sustained created with zero `placeable_intents` =
    FFD/kube-scheduler-packed mismatch.],

  (refs.metric)("rio_controller_nodeclaim_tick_duration_seconds"),
  [Histogram],
  [nodeclaim_pool `reconcile_once` latency. Recorded on success and error
    (⊥-tick, apiserver 5xx). p99 approaching `ADMIN_RPC_TIMEOUT` (5s) =
    scheduler stalled; approaching `TICK` interval = reconciler can't keep
    up.],

  (refs.metric)("rio_controller_nodeclaim_live"),
  [Gauge],
  [Owned NodeClaims at the last tick (labeled by `cell` × `state`).
    `state=registered`: `Registered=True` and not terminating
    (FFD-placeable); `state=inflight`: created but not yet Registered;
    `state=terminating`: `metadata.deletionTimestamp` set (Karpenter
    finalizer draining, \~60-90s --- excluded from FFD placement, still in
    the `max_fleet_cores` budget). The three states partition the owned
    set. `inflight` stuck high → check
    `reaped_total{reason=ice|boot-timeout}`; `terminating>0` with
    `registered=0` and `ffd_unplaced_cores>0` → a node draining out from
    under a queue, replacement minted next tick.],

  (refs.metric)("rio_controller_nodeclaim_inflight_age_max_seconds"),
  [Gauge],
  [Oldest in-flight NodeClaim per `cell` (`now − creationTimestamp`; 0 when
    none in-flight). The per-claim age
    #(refs.alert)("RioNodeclaimPoolStuckPending") keys on ---
    `nodeclaim_live{state=inflight}` count never touches 0 under sustained
    scale-up.],

  (refs.metric)("rio_controller_nodeclaim_terminating_age_max_seconds"),
  [Gauge],
  [Oldest terminating NodeClaim per `cell` (`now − deletionTimestamp`; 0
    when none terminating). The per-claim age
    #(refs.alert)("RioNodeclaimPoolStuckTerminating") keys on ---
    `nodeclaim_live{state=terminating}` count never touches 0 under
    sustained scale-down churn.],

  (refs.metric)("rio_controller_ffd_unplaced_cores"),
  [Gauge],
  [`Σ SpawnIntent.cores` per `cell` left unplaced by FFD at the last tick.
    `cover_deficit`'s per-cell input. Non-zero with `created_total` flat =
    `max_fleet_cores` or per-tick cap throttling.],

  (refs.metric)("rio_controller_ffd_placeable_intents"),
  [Gauge],
  [SpawnIntents FFD-placed at the last tick (labeled by `state`).
    `state=registered` ⇒ on a `Registered=True` claim (Jobs created this
    tick); `state=inflight` ⇒ on a not-yet-Registered claim (held by
    placeable-gate). `registered/(registered+inflight)` is the forecast
    warm-hit proxy.],

  (refs.metric)("rio_controller_nodeclaim_lead_time_seconds"),
  [Gauge],
  [Per-`cell` provisioning lead-time: `lead_time_q`-quantile of the
    `z=boot−eta_error` DDSketch. What `cover_deficit` provisions ahead by.
    Stuck at the seed value = no `Registered=True` transitions recorded
    yet.],

  (refs.metric)("rio_controller_nodeclaim_ice_timeout_seconds"),
  [Gauge],
  [Per-`cell` boot/ICE reap threshold the reaper acts at:
    `max(2×lead_time_seed, q_0.99(boot))`, floored at 2×seed until 100 boot
    samples. The #(refs.alert)("RioNodeclaimPoolStuckPending") alert
    anchors on this (×2). Distinct from `lead_time_seconds`
    (q_0.9(boot−eta) --- what cover_deficit provisions ahead by; learns
    DOWN).],

  (refs.metric)("rio_controller_nodeclaim_consolidate_threshold_seconds"),
  [Gauge],
  [Per-`cell` idle-NodeClaim reap threshold from the NA model (last node
    evaluated this tick; 0 when no idle nodes).
    `max(boot_median/2, min_consolidation_time[cell])` floored. NA-extends
    past the floor ONLY for cells packing \~1 intent/node
    (`E[c_fit] > cores/2`); for bin-packed cells (the §13b MostAllocated
    builder default) the floor is a hard bound. `fetcher-*` cells should
    sit ≥ 600s; builder cells ≥ the 60s `*` floor. A cell at
    `boot_median/2` (\~9--25s) when its `minConsolidationTime` floor says
    60/600s = the prefix-glob didn't match (r35 bug_023, bug_050).],

  (refs.metric)("rio_controller_ddsketch_seed_fallback_total"),
  [Counter],
  [Per-`cell` seed injections at `CellSketches::seed()`. Once per
    cold-start cell whose `z_active` AND `z_shadow` were both empty after
    PG load. >1 over controller lifetime = sketch persist failing.],

  (refs.metric)("rio_controller_nodeclaim_intent_dropped_total"),
  [Counter],
  [nodeclaim_pool intents dropped by `reason`. `reason=no_pool_covers`: no
    configured Builder or Fetcher `Pool` covers the intent's
    `(kind, system, effective_features)` --- no Job will ever be created,
    so provisioning would mint a permanently-idle NodeClaim; add a Pool or
    remove the hwClass advertising the feature. `reason=no_hosting_class`:
    no configured hw-class can host the intent _even with no ICE-masking_
    --- wrong arch, footprint exceeds every arch-matching class's
    `max_cores`/`max_mem`, `required_features` unmatched (no
    `provides_features` entry), or featureless arch-unmappable system (no
    constraint axis to route on; r35 B1). Persistent until
    `[sla.hw_classes]` changes;
    #(refs.alert)("RioNodeclaimPoolNoHostingClass") alerts on it.
    `reason=all_cells_ice_masked`: a class CAN host the intent but every
    hosting cell is ICE-masked --- NodeClaim launches are failing in the
    cloud (capacity, quota, IAM --- e.g. a missing
    `AWSServiceRoleForEC2Spot`). Self-heals on ICE TTL expiry if the cloud
    recovers; persistent if structural.
    #(refs.alert)("RioNodeclaimPoolAllCellsIceMasked") alerts on it; check
    `nodeclaim_reaped_total{reason=~"ice|vanished"}` and the Karpenter log.
    The two reasons need opposite operator actions (config vs cloud) ---
    `no_hosting_class` and `all_cells_ice_masked` together are the ONLY
    signals for the "no NodeClaim was ever minted" failure class (a
    never-minted NodeClaim emits no other series).
    `reason=exceeds_cell_cap`: intent's pod footprint exceeds the assigned
    cell's per-class ceiling (override-bypass producer hole).
    `reason=unknown_hw_class`: scheduler stamped a hwClass not yet in the
    controller's GetHwClassConfig --- config skew; self-heals within ≤300s,
    persistent rate = controller's hw_refresh RPC failing.],
)
