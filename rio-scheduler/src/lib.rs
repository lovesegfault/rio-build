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
//! - `queue`: FIFO ready queue
//! - [`db`]: PostgreSQL persistence (sqlx)
//! - [`grpc`]: SchedulerService + ExecutorService gRPC implementations

pub mod actor;
pub mod admin;
pub(crate) mod assignment;
pub(crate) mod ca;
pub(crate) mod critical_path;
pub(crate) mod dag;
pub mod db;
pub(crate) mod domain;
pub mod event_log;
pub mod grpc;
pub mod lease;
pub mod logs;
pub(crate) mod queue;
pub mod sla;
pub mod state;

// Re-exports for PoisonConfig + RetryPolicy: main.rs's `Config`
// struct embeds them as `#[serde(default)]` sub-tables. `state` IS
// pub, but the re-export keeps main.rs's imports uniform
// (crate-root path, no deep-module reach-in).
pub use state::{PoisonConfig, RetryPolicy};
// Default for main.rs Config's `#[serde(default = ...)]` fn.
pub use actor::DEFAULT_SUBSTITUTE_CONCURRENCY;

/// Shared sqlx migrator for the `migrations/` directory. Test-only
/// (`TestDb::new(&MIGRATOR)`) — production goes through
/// `rio_common::migrate::run` in `main.rs`. Same migration set as
/// rio-store; each crate embeds its own copy because `sqlx::migrate!`
/// resolves the path relative to the crate's `CARGO_MANIFEST_DIR` at
/// compile time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Histogram bucket boundaries for `rio_scheduler_critical_path_accuracy`.
///
/// Ratio of actual/estimated build duration. `1.0` = perfect prediction,
/// values above `1.0` = underestimate (build took longer than predicted).
/// Bucket edges are chosen to give resolution around `1.0` and capture
/// long tails on both sides.
const CRITICAL_PATH_ACCURACY_BUCKETS: &[f64] = &[0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 5.0];

/// Histogram bucket boundaries for scheduler assignment latency (seconds).
///
/// Time from a derivation becoming Ready to being assigned to a worker.
/// With a warm static fleet this is sub-second. With ephemeral builders the
/// latency is dominated by node-provision (~60–180s on EKS), so the original
/// `[0.001..5.0]` set put every sample in `+Inf` (I-124). These span
/// 100ms..10min: low buckets catch the warm-fleet path, the 30s..600s range
/// gives resolution across cold-node provision.
const DISPATCH_WAIT_BUCKETS: &[f64] = &[
    0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 180.0, 300.0, 600.0,
];

/// Histogram bucket boundaries for `rio_scheduler_build_graph_edges`.
///
/// Edge COUNT (not seconds) per GetBuildGraph response. Range is 0..~20K
/// (induced subgraph over the 5000-node cap at realistic 4× edge density).
/// Default Prometheus buckets `[0.005..10.0]` are useless here — every
/// sample lands in `+Inf`. These match the suggested buckets in
/// observability.md's Histogram Buckets table.
const GRAPH_EDGES_BUCKETS: &[f64] = &[100.0, 500.0, 1000.0, 5000.0, 10000.0, 20000.0];

/// Histogram bucket boundaries for `rio_scheduler_warm_prefetch_paths`.
///
/// Path COUNT (not seconds) per `PrefetchComplete` ACK — how many paths
/// the worker actually fetched for a warm-gate hint. Hard-capped at 100
/// (scheduler-side `MAX_PREFETCH_PATHS`). 0 = already warm (all cache
/// hits). Small-leaf closures: 1–10. Fat stdenv closures: 30–80.
const WARM_PREFETCH_PATHS_BUCKETS: &[f64] = &[0.0, 1.0, 5.0, 10.0, 25.0, 50.0, 100.0];

/// Histogram bucket boundaries for `rio_scheduler_merge_phase_seconds`.
/// Spans sub-ms (in-mem phases) → minute (PG/store-RPC phases) so a
/// per-phase regression that pushes MergeDag past the 1s actor-stall
/// warn is visible without `RUST_LOG=debug`.
const MERGE_PHASE_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
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
    ("rio_scheduler_dispatch_wait_seconds", DISPATCH_WAIT_BUCKETS),
    ("rio_scheduler_merge_phase_seconds", MERGE_PHASE_BUCKETS),
    ("rio_scheduler_build_graph_edges", GRAPH_EDGES_BUCKETS),
    (
        "rio_scheduler_warm_prefetch_paths",
        WARM_PREFETCH_PATHS_BUCKETS,
    ),
];

/// Register `# HELP` descriptions for all scheduler metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Descriptions
/// sourced from docs/src/observability.md (the Scheduler Metrics table).
/// See rio_gateway::describe_metrics for rationale.
// r[impl obs.metric.scheduler]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "rio_scheduler_builds_total",
        "Total builds at terminal state (labeled by outcome: success/failure/cancelled)"
    );
    describe_gauge!("rio_scheduler_builds_active", "Currently active builds");
    describe_gauge!(
        "rio_scheduler_derivations_queued",
        "Derivations waiting for assignment"
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
        "rio_scheduler_substitute_fetch_failures_total",
        "Substitutable-path eager fetches (QueryPathInfo) that failed; path demoted to cache-miss"
    );
    describe_counter!(
        "rio_scheduler_substitute_fetch_retries_total",
        "Transient (Unavailable/Aborted/ResourceExhausted) substitute-fetch errors that \
         triggered a backoff retry. High rate without matching failures = store load \
         absorbed by retry; high rate WITH failures = backoff insufficient."
    );
    describe_counter!(
        "rio_scheduler_substitute_spawned_total",
        "Derivations transitioned to Substituting (detached upstream fetch spawned). \
         Pairs with substitute_fetch_failures_total to derive success rate."
    );
    describe_counter!(
        "rio_scheduler_topdown_prune_total",
        "Submissions pruned to roots-only by the top-down substitution pre-check"
    );
    describe_counter!(
        "rio_scheduler_topdown_substitute_fail_total",
        "Top-down-pruned roots whose deferred substitute fetch failed (build failed fast; resubmit re-probes)"
    );
    describe_counter!(
        "rio_scheduler_queue_backpressure",
        "Backpressure activations (queue reached 80% capacity)"
    );
    describe_gauge!(
        "rio_scheduler_workers_active",
        "Fully-registered workers (stream + heartbeat)"
    );
    describe_counter!(
        "rio_scheduler_assignments_total",
        "Total derivation-to-worker assignments"
    );
    describe_counter!(
        "rio_scheduler_prefetch_hints_sent_total",
        "PrefetchHint messages sent (one per assignment with paths to warm). \
         Missing from a dispatch = leaf drv (no children)."
    );
    describe_counter!(
        "rio_scheduler_prefetch_paths_sent_total",
        "Total paths in sent PrefetchHints. Divide by hints_sent for avg \
         paths-per-hint."
    );
    describe_counter!(
        "rio_scheduler_warm_gate_fallback_total",
        "best_executor() fell back to cold workers because NO warm worker \
         passed the hard filter. Single-worker clusters and mass scale-up \
         expect nonzero; sustained high rate = workers never warming \
         (PrefetchComplete not arriving — check worker logs)."
    );
    describe_histogram!(
        "rio_scheduler_warm_prefetch_paths",
        "Paths fetched per initial warm-gate PrefetchHint (from the worker's \
         PrefetchComplete ACK). 0 = worker was already warm (cache hit on \
         everything); high = fresh worker cold-fetched everything."
    );
    describe_counter!(
        "rio_scheduler_cleanup_dropped_total",
        "Terminal-build cleanup commands dropped due to channel backpressure; alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_reconcile_dropped_total",
        "Post-recovery ReconcileAssignments command dropped (actor channel full). \
         Assigned-but-worker-gone derivations leak until NEXT recovery. Rare (channel \
         is 1024 deep); alert if > 0."
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
        "rio_scheduler_log_lines_forwarded_total",
        "Log lines forwarded via BuildEvent::Log (worker -> scheduler -> gateway broadcast)"
    );
    describe_counter!(
        "rio_scheduler_log_flush_total",
        "Successful S3 log flushes (labeled by kind: final/periodic)"
    );
    describe_counter!(
        "rio_scheduler_log_flush_failures_total",
        "Failed log flushes (labeled by phase: compress/s3/pg, is_final: true/false); \
         alert on is_final=true rate > 0 sustained"
    );
    describe_counter!(
        "rio_scheduler_log_flush_dropped_total",
        "Final-flush requests dropped due to flusher channel backpressure"
    );
    describe_counter!(
        "rio_scheduler_log_forward_dropped_total",
        "Log batches dropped (actor channel backpressure). Lines are still in the ring buffer."
    );
    describe_counter!(
        "rio_scheduler_log_unknown_drv_dropped_total",
        "LogBatch dropped: stream exceeded MAX_DRVS_PER_STREAM distinct derivation_path values"
    );
    describe_counter!(
        "rio_scheduler_executor_reconnect_rejected_total",
        "BuildExecution reconnects rejected by the stream-hijack guard (label: reason)"
    );
    describe_histogram!(
        "rio_scheduler_critical_path_accuracy",
        "Predicted vs actual completion ratio (actual/estimated; 1.0=perfect, >1.0=underestimate)"
    );
    describe_gauge!(
        "rio_scheduler_queue_depth",
        "Deferred Ready derivations waiting for an executor of the matching \
         kind (snapshot per dispatch pass; labeled by kind=builder|fetcher). \
         Sustained nonzero → scale the matching pool."
    );
    describe_gauge!(
        "rio_scheduler_unroutable_ready",
        "Ready derivations whose `system` is advertised by zero registered \
         executors of the matching kind (labeled by system; snapshot per \
         dispatch pass). Nonzero = no pool exists for that system; add it \
         to a Pool's `systems` list."
    );
    describe_gauge!(
        "rio_scheduler_utilization",
        "Fraction of executors currently running a build (busy/total; labeled \
         by kind=builder|fetcher). Emitted per dispatch pass alongside \
         queue_depth."
    );
    describe_counter!(
        "rio_scheduler_cache_check_circuit_open_total",
        "Circuit-breaker open transitions (store unreachable for 5 consecutive checks); alert if > 0"
    );
    describe_counter!(
        "rio_scheduler_event_persist_dropped_total",
        "BuildEvents dropped from PG persister (channel backpressure). \
         Broadcast still live; only mid-backlog reconnect loses it. Alert if rate > 0 sustained."
    );
    // The following metrics are emitted from actor internals.
    describe_counter!(
        "rio_scheduler_backstop_timeouts_total",
        "Derivations reset to Ready after running-since exceeded backstop (worker went silent)"
    );
    describe_counter!(
        "rio_scheduler_phantom_assignments_drained_total",
        "running_build entries drained after two consecutive heartbeats reported \
         empty (lost completion / dead-stream-post-send). The slot was dead capacity \
         until drained. Nonzero is the signal to look for I-032-class bugs upstream — \
         the drain is the safety net, not the fix."
    );
    describe_counter!(
        "rio_scheduler_heartbeat_adoptions_total",
        "Builds re-claimed in the DAG from a reconnecting worker's heartbeat. \
         Expected after scheduler restart: recovery's reconcile may have reset \
         to Ready before the worker reconnected (I-063 keeps the stream alive \
         during drain). Adoption prevents re-dispatch of work already in flight."
    );
    describe_counter!(
        "rio_scheduler_heartbeat_adopt_conflicts_total",
        "Heartbeat-adoption found the DAG already Assigned to a DIFFERENT worker \
         (reconcile re-dispatched before this worker reconnected). Both run; first \
         to complete wins. Nonzero suggests reconcile delay is too short for the \
         observed worker-reconnect time."
    );
    describe_counter!(
        "rio_scheduler_build_timeouts_total",
        "Builds failed by per-build wall-clock timeout (BuildOptions.build_timeout \
         seconds since submission). Distinct from backstop_timeouts_total \
         (per-derivation heuristic — worker went silent)."
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
        "Scheduler state recoveries from PG after LeaderAcquired \
         (labeled by outcome=success|failure|discarded_flap)"
    );
    describe_histogram!(
        "rio_scheduler_recovery_duration_seconds",
        "Time to reconstruct actor state from PG on LeaderAcquired \
         (labeled by outcome=success|failure)"
    );
    describe_counter!(
        "rio_scheduler_worker_disconnects_total",
        "Worker stream disconnects (graceful and ungraceful)"
    );
    describe_counter!(
        "rio_scheduler_cancel_signals_total",
        "CancelSignal messages successfully delivered to the executor stream (Ok on try_send). \
         Excludes drops (counted in cancel_signal_dropped_total) and skipped attempts."
    );
    describe_counter!(
        "rio_scheduler_cancel_signal_dropped_total",
        "CancelSignal try_send drops (worker stream full/closed under backpressure). \
         Best-effort: the transition to Cancelled is scheduler-authoritative regardless; \
         worker disconnect reassign still fires. Alert if rate > 0 sustained."
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
        "rio_scheduler_dispatch_wait_seconds",
        "Time from a derivation entering Ready to being Assigned (fed \
         from DerivationState.ready_at). With ephemeral builders, \
         dominated by node-provision (~60–180s on EKS)."
    );
    describe_counter!(
        "rio_scheduler_broadcast_lagged_total",
        "BuildEvent broadcast events skipped by lagging subscribers \
         (sum of RecvError::Lagged(n) across all bridge tasks). Non-zero \
         under sustained event burst (large DAG, many concurrent drvs \
         emitting Log lines) — gateway can't drain fast enough (I-144)."
    );
    crate::sla::metrics::describe_all();
}
