//! DAG-aware build scheduler for rio-build.
//!
//! Receives derivation build requests, analyzes the DAG, and publishes
//! work to workers via a bidirectional streaming RPC.
//!
//! ## Architecture
//!
//! The scheduler uses a single-owner actor model. All mutable state is owned
//! by a single Tokio task (the DAG actor) that processes commands from a
//! bounded mpsc channel. gRPC handlers send commands and await responses.
//!
//! ## Modules
//!
//! - [`actor`]: DAG actor (single-owner event loop, dispatch)
//! - `dag`: In-memory derivation graph
//! - [`state`]: Derivation and build state machines
//! - [`db`]: PostgreSQL persistence (sqlx)
//! - [`grpc`]: SchedulerService + ExecutorService gRPC implementations

pub mod actor;
pub mod admin;
pub(crate) mod assignment;
pub(crate) mod ca;
pub mod config;
pub(crate) mod critical_path;
pub(crate) mod dag;
pub mod db;
pub(crate) mod domain;
pub mod grpc;
/// Re-export so existing `crate::lease::{LeaderState, LeaseConfig,
/// run_lease_loop}` paths keep working after the B1 extraction.
pub use rio_lease as lease;
pub mod lease_hooks;
pub mod observability;
// The Phase-0 reference fold over a derivation's failure history — the
// executable specification the retryPolicy model's CountersRefineHistory
// invariant compares the live RetryState counters against. Dead code by
// design until Phase 1 collapses the nine failure entry points onto
// decide(); see the module doc for why it is wired (clippy/rustfmt/tests)
// but unreferenced.
#[allow(dead_code)]
pub(crate) mod retry_policy;
pub mod sla;
pub mod state;

// Re-exports for PoisonConfig + RetryPolicy: main.rs's `Config`
// struct embeds them as `#[serde(default)]` sub-tables. `state` IS
// pub, but the re-export keeps main.rs's imports uniform
// (crate-root path, no deep-module reach-in).
pub use state::{PoisonConfig, RetryPolicy};

/// Re-export of the shared embedded migrator from `rio-migrations`.
/// Test-only (`TestDb::new(&MIGRATOR)`) — production goes through
/// `rio_migrations::migrate::run(&pool, rio_migrations::migrator())` in
/// `main.rs`. Same migration set as rio-store; both consume the single
/// `rio-migrations` source of truth.
#[cfg(test)]
pub use rio_migrations::MIGRATOR;

/// Histogram bucket boundaries for `rio_scheduler_critical_path_accuracy`.
///
/// Ratio of actual/estimated build duration. `1.0` = perfect prediction,
/// values above `1.0` = underestimate (build took longer than predicted).
/// Bucket edges are chosen to give resolution around `1.0` and capture
/// long tails on both sides.
const CRITICAL_PATH_ACCURACY_BUCKETS: &[f64] = &[0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 5.0];

/// Histogram bucket boundaries for `rio_scheduler_build_graph_edges`.
///
/// Edge COUNT (not seconds) per GetBuildGraph response. Range is 0..~20K
/// (induced subgraph over the 5000-node cap at realistic 4× edge density).
/// Default Prometheus buckets `[0.005..10.0]` are useless here — every
/// sample lands in `+Inf`. These match the suggested buckets in
/// observability.typ's Histogram Buckets table.
const GRAPH_EDGES_BUCKETS: &[f64] = &[100.0, 500.0, 1000.0, 5000.0, 10000.0, 20000.0];

/// Histogram bucket boundaries for `rio_scheduler_merge_phase_seconds`.
/// Spans sub-ms (in-mem phases) → minute (PG/store-RPC phases) so a
/// per-phase regression that pushes MergeDag past the 1s actor-stall
/// warn is visible without `RUST_LOG=debug`.
const MERGE_PHASE_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Histogram bucket boundaries for `rio_scheduler_attempt_requeue_seconds`
/// (the OA1 interval-(ii) instrument). Spans the worker-report cause
/// (same actor turn — tens of ms, dominated by the appending PG
/// transaction), the pod-terminal cause (the controller's classifying
/// report typically lands one or a few reconcile ticks after the
/// disconnect), and the establishment cause (the TTL sweep fires at
/// `TERMINATION_REPORT_TTL` after the disconnect), with headroom for
/// controller-outage tails.
const ATTEMPT_REQUEUE_BUCKETS: &[f64] = &[
    0.05, 0.25, 1.0, 5.0, 15.0, 30.0, 60.0, 90.0, 120.0, 300.0, 600.0,
];

/// Per-crate histogram bucket overrides, passed to
/// `rio_common::server::bootstrap` → `init_metrics`. Every
/// `describe_histogram!` in this crate must have an entry here OR be in
/// the `DEFAULT_BUCKETS_OK` exemption list (`tests/metrics_registered.rs`);
/// histograms not listed fall through to the global `[0.005..10.0]` default.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    (
        "rio_scheduler_build_duration_seconds",
        rio_common::observability::BUILD_DURATION_BUCKETS,
    ),
    (
        "rio_scheduler_critical_path_accuracy",
        CRITICAL_PATH_ACCURACY_BUCKETS,
    ),
    ("rio_scheduler_merge_phase_seconds", MERGE_PHASE_BUCKETS),
    ("rio_scheduler_build_graph_edges", GRAPH_EDGES_BUCKETS),
    (
        "rio_scheduler_attempt_requeue_seconds",
        ATTEMPT_REQUEUE_BUCKETS,
    ),
];

/// Registers prometheus metric descriptions. The help strings here are
/// the source for `docs/ref/metrics.typ` — see
/// `xtask/src/regen/docs_data.rs::metrics()` for the data-flow.
// r[impl obs.metric.scheduler]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "rio_scheduler_builds_total",
        "Total builds at terminal state (labeled by outcome: success/failure/cancelled)"
    );
    describe_gauge!("rio_scheduler_builds_active", "Currently active builds");
    describe_gauge!(
        "rio_scheduler_status_outbox_depth",
        "Failed terminal-status persist batches awaiting the housekeeping tick's \
         re-drive. Nonzero means PG persists are failing; a sustained nonzero \
         depth means cancelled derivations' attempts stay open (their close \
         rides the persist) until the flush succeeds."
    );
    describe_gauge!(
        "rio_scheduler_derivations_queued",
        "Ready derivations waiting for a worker (DAG-status count; the per-system \
         split is queued_by_system on ClusterStatus/GetSpawnIntents)"
    );
    describe_gauge!(
        "rio_scheduler_derivations_running",
        "Derivations currently building"
    );
    describe_histogram!(
        "rio_scheduler_merge_phase_seconds",
        "Per-phase MergeDag latency (labeled by phase: 0-topdown-roots..6f). \
         Decomposes rio_scheduler_actor_cmd_seconds{cmd=MergeDag}. A single \
         phase >1s is the I-139 signal — N sequential PG awaits in the actor."
    );
    describe_histogram!(
        "rio_scheduler_actor_cmd_seconds",
        "Per-ActorCommand handling latency (labeled by cmd variant); \
         the actor is single-threaded so a slow command head-of-line \
         blocks every queued RPC"
    );
    describe_histogram!(
        "rio_scheduler_build_duration_seconds",
        "Total build duration"
    );
    describe_counter!(
        "rio_scheduler_cache_hits_total",
        "Derivations served from cache (labeled by source: scheduler/reprobe/existing/dispatch)"
    );
    describe_counter!(
        "rio_scheduler_cache_hit_deferred_total",
        "Cache-hit derivations deferred to Queued because their inputDrvs are not yet Completed"
    );
    describe_counter!(
        "rio_scheduler_cache_check_failures_total",
        "Scheduler cache check (store FindMissingPaths) failures; alert if rate > 0 sustained"
    );
    describe_counter!(
        "rio_scheduler_derivations_gc_deleted_total",
        "Orphan-terminal derivations rows deleted by the periodic Tick sweep (I-169.2)"
    );
    describe_counter!(
        "rio_scheduler_store_degraded_requeues_total",
        "Worker-flagged store-degraded infrastructure reports by gated disposition \
         (merged_bug_032): paced = corroborated + inside the kernel free run \
         (uncharged requeue); uncorroborated = single-node flag charged as plain \
         infra; run_bound = corroborated but past STORE_DEGRADED_FREE_RUN, charged \
         fallthrough into the counted infra budget"
    );
    describe_counter!(
        "rio_scheduler_attempts_gc_deleted_total",
        "drv_attempts rows deleted by the periodic attempt-ledger retention sweep \
         (sched.db.attempts-gc): suffix-complement rows past the retention horizon plus \
         orphaned histories. The decision suffix is provably unchanged by every increment"
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_gc_total",
        "Resolved materialization_jobs rows deleted by the periodic retention sweep \
         (D1/A6, merged_bug_163): past the forensic horizon, unpinned, interest-free. \
         Pending jobs are never deleted"
    );
    describe_counter!(
        "rio_scheduler_wanted_outputs_gc_total",
        "build_wanted_outputs rows deleted by the periodic retention sweep (D1/A6, \
         merged_bug_163): builds long-terminal, plus orphan rows whose build is gone"
    );
    describe_counter!(
        "rio_scheduler_exec_rows_gc_deleted_total",
        "drv_executions lifecycle rows deleted by the execution-row GC \
         (store.log.sweep-ownership second deleter): terminal, no active \
         assignment, referenced by no drv_attempts row, older than \
         exec_retention_days. The store's log TTL sweep never deletes these."
    );
    describe_counter!(
        "rio_scheduler_resource_floor_bumps_total",
        "resource_floor doublings on explicit resource-exhaustion signals (D4, labeled \
         reason=oom_killed|disk_pressure|cgroup_oom|timeout|deadline_exceeded). Reactive \
         upsize: a derivation that OOMs at mem=N retries at mem=2N. Frequent firing for \
         one pname = raise [sla].probe defaults."
    );
    describe_counter!(
        "rio_scheduler_poison_fleet_exhausted_total",
        "Derivations poisoned because failed_builders excluded every registered worker \
         of the matching kind (I-065). Nonzero rate with small fleet = poison threshold \
         unreachable; the build would otherwise defer forever."
    );
    describe_counter!(
        "rio_scheduler_stale_completed_reset_total",
        "Pre-existing Completed nodes reset to Ready at merge because output was \
         GC'd from store. Nonzero rate = GC retention shorter than DAG node lifetime."
    );
    describe_counter!(
        "rio_scheduler_stale_completed_substituted_total",
        "Pre-existing Completed nodes whose GC'd output was repopulated via upstream \
         substitution at merge (instead of reset-to-Ready re-dispatch)."
    );
    describe_counter!(
        "rio_scheduler_stale_realisation_filtered_total",
        "CA realisations dropped from cache-hit set because the realized path was \
         GC'd from store. Same operator signal as stale_completed_reset; this counts \
         the newly-inserted-CA path, that one counts the pre-existing-completed path."
    );
    describe_counter!(
        "rio_scheduler_topdown_prune_total",
        "Submissions pruned to roots-only by the top-down substitution pre-check"
    );
    describe_counter!(
        "rio_scheduler_topdown_substitute_fail_total",
        "Top-down-pruned roots that could not complete via substitution (a wanted output \
         was definitively missing/unsubstitutable at settlement) — build failed fast; \
         resubmit re-probes"
    );
    describe_counter!(
        "rio_scheduler_queue_backpressure",
        "Backpressure activations (queue reached 80% capacity)"
    );
    describe_gauge!(
        "rio_scheduler_open_attempts",
        "Open pull-mode BUILD attempts (active assignment + execution pair minted \
         by PullAssignment, no terminal classification yet) — one attempt per \
         builder pod. The busy-fleet gauge (successor of the retired stream-era \
         workers_active). Store materialization claims are counted by \
         rio_scheduler_open_materialization_attempts instead (A2.4)."
    );
    describe_gauge!(
        "rio_scheduler_open_materialization_attempts",
        "Open store materialization claims (kind=materialization attempts: active \
         assignment + execution pair, no terminal classification yet). Store-side \
         work — never a builder slot; the build lane is rio_scheduler_open_attempts."
    );
    describe_counter!(
        "rio_scheduler_assignments_total",
        "Total derivation-to-worker assignments"
    );
    describe_counter!(
        "rio_scheduler_cleanup_dropped_total",
        "Terminal-build cleanup commands dropped due to channel backpressure; alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_transition_rejected_total",
        "State-machine transition rejections (labeled by target state); alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_malformed_built_output_total",
        "Worker-supplied BuiltOutput.output_path that failed StorePath::parse \
         (dropped at handle_completion boundary); alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_undeclared_built_output_total",
        "Worker-supplied BuiltOutput.output_name not in derivation's output_names \
         (dropped at handle_completion membership filter); alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_pull_rejected_total",
        "Pull-mode unaries rejected (labels: rpc = \
         pull_assignment|report_outcome, reason = unauthenticated|token_mismatch|\
         kind_unauthorized — the executor token verified but its kind is not \
         allowed to take this attempt kind; consumption_not_durable — the \
         report's consumption close failed to commit durably, the NACK rides \
         UNAVAILABLE and the store's report redelivery re-presents the SAME \
         outcome, so a sustained rate here is a PG brownout trace, not an \
         identity fault). \
         A sustained rate on the identity reasons means a pod fleet holds \
         mis-bound/expired executor tokens or an HMAC rotation skew; an \
         occasional unauthenticated blip is the documented mint-skip race \
         (drv left Ready between GetSpawnIntents and MintExecutorTokens — \
         see mint_executor_tokens). The rejected pods exit nonzero and \
         their logs are ephemeral, so this counter is the alertable trace."
    );
    describe_counter!(
        "rio_scheduler_pull_establishments_total",
        "Open pull-mode attempts established as unreported executor crashes \
         by the establishment sweep (the C2 charge arm; the store-probe adopt \
         arm does not count). Feeds the OA2 interim hung-node tripwire — the \
         attempt_requeue histogram's establishment cause is shared with the \
         stream-mode correlation-TTL sweep and is unsuitable for that alert."
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_created_total",
        "Materialization jobs created (substitution-replacement campaign), \
         labeled by origin (pruned|cache_opportunity|stale_reset|reprobe). \
         Dedup-found existing jobs do not count; origin upgrades are counted \
         by ..._origin_upgraded_total."
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_origin_upgraded_total",
        "Pruned-wins origin upgrades on the creation dedup arm (PD-D1): a \
         topdown prune landed on a node with an existing unresolved \
         non-pruned job and upgraded that row's origin to 'pruned' in the \
         same transaction. Not a creation (jobs_created_total is untouched); \
         psql origin distributions diverge from creation-rate dashboards by \
         exactly this counter."
    );
    describe_counter!(
        "rio_scheduler_wanted_width_saturated_total",
        "The conservative-absent wanted-width arm fired (DQ-2): a live \
         interested build's wanted contributions could not be resolved \
         (no in-memory cache entry / no build_wanted_outputs rows — the \
         legacy pre-relation shape), so the effective wanted width \
         degraded to ALL DECLARED outputs (maximal width, never a vacuous \
         narrow set). Sustained increments mean pre-relation builds are \
         still live; the population shrinks to zero as they terminate."
    );
    describe_counter!(
        "rio_scheduler_materialization_no_verifiable_wanted_total",
        "A materialization re-arm found NO verifiable wanted path set \
         (width ZERO — empty set or empty-string placeholder paths, even \
         after the realized-path carrier union), so the consumption \
         closed uncharged and deferred instead of verifying vacuously. \
         The opposite end of the width axis from \
         ..._wanted_width_saturated_total (bug_282: the two event \
         classes share one typed chokepoint and never one counter)."
    );
    describe_gauge!(
        "rio_scheduler_substituting_derivations",
        "Derivations carrying a CLAIMABLE materialization job (unclaimed AND not \
         parked) — the substitution backlog (the same quantity \
         ClusterStatus.substituting_derivations reports). Parked jobs are pacing, \
         not claimable demand: they leave this gauge (bug_252 — KEDA drains while \
         the backlog parks) and stay visible via rio_scheduler_materialization_stalled. \
         Leader-published every housekeeping tick from the freshly computed cluster \
         snapshot. The leading rio-store KEDA scaling signal: backlog is known at \
         merge time, minutes before the store feels the ingest load."
    );
    describe_gauge!(
        "rio_scheduler_materialization_stalled",
        "Parked materialization jobs (park budget exhausted — worker-charged \
         infra failures or establishment charges; waiting on upstream recovery or \
         park-backoff expiry). Leader-published every housekeeping tick from ground \
         truth. The PD-20 re-evaluation converts parked jobs with buildable \
         dependency closures from-source ONLY when the conversion-strictness knobs \
         admit it (conversion_requires_worker_charge: establishment-only-parked \
         jobs never convert while ON; conversion_min_park_dwell_secs: minimum dwell \
         since the most recent park). A sustained nonzero value therefore means \
         either a dead/misconfigured upstream stalling work with no from-source \
         fallback, OR convertible work the knobs are deliberately holding — check \
         the strictness knobs before cancelling builds."
    );
    describe_counter!(
        "rio_scheduler_materialization_claims_total",
        "Materialization claims delivered to store replicas (one per minted open \
         attempt). Pairs with jobs_created_total (supply) and jobs_resolved_total \
         (drain) for the lifecycle rates: created-vs-claimed divergence means store \
         executors are not keeping up (or are partitioned from the leader); \
         claimed-vs-resolved divergence means executions are failing or reports are \
         not landing."
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_resolved_total",
        "Materialization jobs terminally resolved, labeled by outcome \
         (success|from_source|unobtainable|cancelled|obsolete). At-most-once per \
         job (re-resolution no-ops never double-count). success = the wanted set \
         was materialized from upstream; from_source = the job released the node \
         to normal from-source dispatch (durable Vouched/Pending evidence or the \
         PD-20 park re-evaluation); unobtainable = the fail-fast settlement; \
         cancelled = zero live interest remained. A high infra-failure or \
         unobtainable rate is the upstream-health signal."
    );
    describe_counter!(
        "rio_scheduler_materialization_converted_total",
        "PD-20 park re-evaluation conversions: parked materialization jobs \
         resolved from_source because their infra budget exhausted while \
         from-source stayed viable — the TIME-driven subset of \
         jobs_resolved_total{outcome=from_source}, labeled by job origin \
         (pruned|cache_opportunity|stale_reset|reprobe). origin=cache_opportunity \
         means upstream-available content converted to a build: expected as \
         bounded noise during cold-start waves, alertable when sustained \
         (RioSchedulerMaterializationConversions — the wedged-ingest churn-loop \
         signature). At-most-once per applied resolution."
    );
    describe_counter!(
        "rio_scheduler_materialization_view_node_skew_total",
        "Split-release wedge tripwire (merged_bug_307 rider): a pending-unclaimed materialization job whose node is still Assigned/Running with no open assignment — release_claim should have requeued the node in the same step that dropped the claim. Always zero in a healthy fleet; any increment is a re-introduced split release (fatal under debug builds)."
    );
    describe_counter!(
        "rio_scheduler_materialization_carrier_dropped_total",
        "Realized-path carriers dropped from the leader-scoped retry stash because the node went terminal/gone before the stale-reset job row applied (merged_bug_257). The carrier had no consumer left; the paths are reproducible from source if interest returns."
    );
    describe_histogram!(
        "rio_scheduler_critical_path_accuracy",
        "Predicted vs actual completion ratio (actual/estimated; 1.0=perfect, >1.0=underestimate)"
    );
    describe_counter!(
        "rio_scheduler_cache_check_circuit_open_total",
        "Circuit-breaker open transitions (store unreachable for 5 consecutive checks); alert if > 0"
    );
    // The following metrics are emitted from actor internals.
    describe_counter!(
        "rio_scheduler_build_timeouts_total",
        "Builds failed by per-build wall-clock timeout (BuildOptions.build_timeout \
         seconds since submission)."
    );
    describe_counter!(
        "rio_scheduler_orphan_builds_cancelled_total",
        "Active builds auto-cancelled by the orphan-watcher sweep: no \
         build_events receiver (gateway SubmitBuild/WatchBuild stream) for \
         >ORPHAN_BUILD_GRACE (5min). Backstop for gateway crash / \
         gateway→scheduler timeout during P0331 disconnect cleanup. Nonzero \
         is expected on gateway restarts; sustained nonzero with healthy \
         gateways means the gateway-side cancel is not firing."
    );
    describe_counter!(
        "rio_scheduler_recovery_total",
        "Scheduler state recoveries from PG after LeaderAcquired; exactly one \
         increment per attempt: outcome=success|failure when the loaded result \
         is applied (or the load failed), outcome=discarded_flap|\
         discarded_unconfirmed when the post-recovery gate throws the result \
         away (discard outcomes take precedence over the load result)"
    );
    describe_counter!(
        "rio_scheduler_recovery_step_down_total",
        "Cooperative lease step-downs requested because a tenure's state \
         recovery FAILED (sched.recovery.step-down): the replica never \
         completes the tenure, clears partial state, and yields the lease so \
         a healthy replica can serve. Under a persistent PG outage this \
         cycles at lease cadence across the replica set (acquire → fail → \
         step down) — deliberate and operator-visible; pair with \
         rio_scheduler_recovery_total{outcome=\"failure\"} (the alerting \
         series). In always-leader (non-K8s) deployments the request is a \
         dead letter: the counter still increments, the tenure stays \
         incomplete, and dispatch remains gated."
    );
    describe_counter!(
        "rio_scheduler_generation_claim_failed_total",
        "Generation-claim INSERTs that failed during recovery (PG error between the \
         seed read and the claim write). The leader proceeds unclaimed: dispatch is \
         not blocked, but that term's generation is absent from the \
         leader_generation_claims ledger, re-opening the deposed-before-persist \
         collision window for that one term. Sustained nonzero means PG is flapping \
         exactly at failover time."
    );
    describe_counter!(
        "rio_scheduler_generation_floor_read_failed_total",
        "PG generation-floor reads that failed during recovery. The term proceeds \
         unclaimed at the recovery-entry generation after the post-claim leadership \
         confirmation — dispatch is not blocked, but that term's generation is absent \
         from the leader_generation_claims ledger and may sit below the durable floor. \
         Sustained nonzero means PG is flapping exactly at failover time."
    );
    describe_counter!(
        "rio_scheduler_evidence_write_fenced_total",
        "Evidence writes (materialization job creation/resolution, wanted-relation \
         rows, derivation status/poison persists, merge transactions) refused by the claims-floor \
         generation fence: the writing replica's serving generation sat below the \
         durable floor (GREATEST over assignments.generation and \
         leader_generation_claims.generation). On a replica that just lost the lease \
         this is the fence working — the successor owns the evidence now; on the \
         current leader it should be ZERO (a nonzero rate there means a capture bug \
         or a PG floor regression and needs investigation)."
    );
    describe_histogram!(
        "rio_scheduler_recovery_duration_seconds",
        "Time to reconstruct actor state from PG on LeaderAcquired \
         (labeled by outcome=success|failure)"
    );
    describe_counter!(
        "rio_scheduler_attempt_record_retries_total",
        "Re-deliveries of failure completions whose attempt-recording transaction \
         (drv_attempts append + decide() + status persist) failed. Each increment is one \
         bounded re-push of the completion event onto the actor mailbox; the derivation \
         stays in its pre-report state until a re-delivery lands. Sustained rate > 0 means \
         PG is rejecting the failure-accounting write path (the backstop sweep is the \
         fallback once the per-derivation re-delivery budget is exhausted)."
    );
    describe_counter!(
        "rio_scheduler_lease_acquired_total",
        "Successful K8s Lease acquisitions (leader elections won)"
    );
    describe_counter!(
        "rio_scheduler_lease_lost_total",
        "K8s Lease losses (stepped down, partition, or preempted)"
    );
    describe_counter!(
        "rio_scheduler_lease_rebound_total",
        "Lease rebounds: holder changes observed late on a still-leading round \
         (a foreign term ran entirely inside our observation gap). Each runs the \
         Compound leadership-edge lose cells and then a full re-recovery."
    );
    describe_counter!(
        "rio_scheduler_sla_refit_total",
        "SLA estimator refresh ticks (≈60s cadence; VM-test sync barrier — \
         increments regardless of [sla] gate)"
    );
    describe_histogram!(
        "rio_scheduler_build_graph_edges",
        "Edge count per GetBuildGraph response. High p99 (>10k) = unusually \
         dense DAG approaching the implicit subgraph bound."
    );
    describe_counter!(
        "rio_scheduler_ca_hash_compares_total",
        "CA early-cutoff output-hash lookups against the content index on \
         successful completion (labeled by outcome=match|miss|skipped_after_miss|\
         malformed|error). High match ratio → CA derivations rebuilding identical \
         content; cutoff-propagate will skip downstream work. skipped_after_miss \
         counts outputs short-circuited after an earlier miss in the same \
         derivation. malformed = executor sent empty output_path; error = PG \
         lookup failed/timed out (alert if rate>0)."
    );
    describe_counter!(
        "rio_scheduler_ca_cutoff_saves_total",
        "Derivations skipped via CA early-cutoff (Queued→Skipped transitions). \
         Each increment is one build that did NOT run because a CA dep's \
         output matched the content index. Direct measure of CA cutoff \
         efficacy."
    );
    describe_counter!(
        "rio_scheduler_ca_cutoff_seconds_saved",
        "Sum of est_duration of skipped derivations, in hw-normalized \
         ref-seconds (r[sched.sla.hw-ref-seconds]; NOT wall-clock per-build — \
         skipped builds were never assigned, so no hw_factor exists to \
         denormalize; divide by fleet min hw_factor for a wall-clock lower \
         bound). est_duration is the Estimator's EMA, not actual — a \
         derivation that's never run has no actual. Paired with saves_total \
         for avg-seconds-per-save."
    );
    describe_counter!(
        "rio_scheduler_ca_cutoff_depth_cap_hits_total",
        "CA cutoff cascade walks that hit MAX_CASCADE_NODES (1000). \
         Node-count cap, not tree-depth — for wide DAGs (fanout>1), 1000 \
         nodes hit well before depth>3. Non-zero = cascades truncated; \
         operator should review if non-zero."
    );
    describe_gauge!(
        "rio_scheduler_actor_mailbox_depth",
        "ActorCommand mpsc queue depth, sampled once per dequeued command. \
         Growth = commands arriving faster than the single-threaded loop \
         retires them. Pair with actor_cmd_seconds to localize a wedge."
    );
    describe_histogram!(
        "rio_scheduler_attempt_requeue_seconds",
        "OA1 interval (ii): scheduler-side terminal/death observation → \
         the event that closes the released attempt and (re)queues the \
         derivation, by cause. cause=worker-report: the worker's own \
         failure report and the requeue, same actor turn (the sample is \
         the in-turn processing latency). cause=pod-terminal: disconnect \
         observation → the controller's classifying termination report \
         consumed. cause=establishment: disconnect observation → the \
         TTL-sweep establishment fill. cause=synthesized: a \
         controller-synthesized cancelled/preempted/reaped verdict closed \
         an open pull attempt charge-free (in-turn latency, AD5). \
         cause=worker-abort: the builder's SIGTERM-abort report closed a \
         still-wanted pull attempt charge-free (in-turn latency, AD5). \
         In stream mode the requeue itself \
         happens at the disconnect, so the pod-terminal/establishment \
         samples measure how long the released attempt waited for its \
         classification — the controller-report-slack input the \
         executor-replacement establishment window is sized against \
         (executor-lifecycle campaign, OA1)."
    );
    describe_counter!(
        "rio_scheduler_broadcast_lagged_total",
        "BuildEvent broadcast events skipped by lagging subscribers \
         (sum of RecvError::Lagged(n) across all bridge tasks). Non-zero \
         under sustained event burst (large DAG, many concurrent drvs \
         emitting Log lines) — gateway can't drain fast enough (I-144)."
    );
    crate::sla::metrics::describe_all();

    // Series birth (C3 metric-ownership): every alert-referenced
    // counter and every leader-family gauge exists from the first
    // scrape on every replica. Tail of describe_metrics() because
    // rio_common::server::run installs the real exporter
    // (init_metrics) immediately before calling this fn — the seeds
    // bind to the exporter, not a pre-init noop recorder. NOT in
    // DagActor::new: boot scrape-surface is a process property, and
    // the standby gauge tests' touch-sets must stay actor-clean.
    // r[impl obs.metric.alert-counter-seeded]
    // r[impl obs.metric.scheduler-leader-gate+5]
    crate::observability::seed_alert_counters();
    crate::observability::seed_leader_gauges();
}
