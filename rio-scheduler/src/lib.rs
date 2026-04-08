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
//! - [`dag`]: In-memory derivation graph
//! - [`state`]: Derivation and build state machines
//! - `queue`: FIFO ready queue
//! - [`db`]: PostgreSQL persistence (sqlx)
//! - [`grpc`]: SchedulerService + ExecutorService gRPC implementations

pub mod actor;
pub mod admin;
pub(crate) mod assignment;
pub mod ca;
pub(crate) mod critical_path;
pub mod dag;
pub mod db;
pub(crate) mod estimator;
pub mod event_log;
pub mod grpc;
pub mod lease;
pub mod logs;
pub(crate) mod queue;
pub mod rebalancer;
pub mod state;

// Re-export for main.rs — `assignment` is pub(crate) but the config struct
// is part of the binary's TOML schema.
pub use assignment::{FetcherSizeClassConfig, SizeClassConfig, SoftFeature};
// Same pattern for PoisonConfig + RetryPolicy: main.rs's `Config`
// struct embeds them as `#[serde(default)]` sub-tables. `state` IS
// pub, but the re-export keeps main.rs's imports uniform with
// `SizeClassConfig` (crate-root path, no deep-module reach-in).
pub use state::{PoisonConfig, RetryPolicy};
// Default for main.rs Config's `#[serde(default = ...)]` fn.
pub use actor::DEFAULT_SUBSTITUTE_CONCURRENCY;
// ADR-020 headroom default. main.rs's `default_headroom_multiplier()`
// and `DagActor::new` both read this so the dispatch-time resource-fit
// filter and the manifest RPC agree on the baked-in default.
pub use estimator::DEFAULT_HEADROOM_MULTIPLIER;

/// Shared sqlx migrator for the `migrations/` directory. See
/// rio-store's MIGRATOR for rationale — same pattern.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

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
        "rio_scheduler_actor_cmd_seconds",
        "Per-ActorCommand handling latency (labeled by cmd variant); \
         the actor is single-threaded so a slow command head-of-line \
         blocks every queued RPC"
    );
    describe_histogram!(
        "rio_scheduler_assignment_latency_seconds",
        "Time from ready to assigned"
    );
    describe_histogram!(
        "rio_scheduler_build_duration_seconds",
        "Total build duration"
    );
    describe_counter!(
        "rio_scheduler_cache_hits_total",
        "Derivations served from cache (labeled by source: scheduler/existing)"
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
        "rio_scheduler_size_class_promotions_total",
        "size_class_floor promotions on transient failure (I-170/I-177, labeled kind/from/to; \
         kind=fod|builder). Reactive upsize: a derivation that fails on class N retries on \
         class N+1. Frequent firing for one pname = raise the default tiny class's memory limit."
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
        "rio_scheduler_topdown_prune_total",
        "Submissions pruned to roots-only by the top-down substitution pre-check"
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
        "rio_scheduler_log_lines_forwarded_total",
        "Log lines forwarded via BuildEvent::Log (worker -> scheduler -> gateway broadcast)"
    );
    describe_counter!(
        "rio_scheduler_log_flush_total",
        "Successful S3 log flushes (labeled by kind: final/periodic)"
    );
    describe_counter!(
        "rio_scheduler_log_flush_failures_total",
        "Failed S3 log flushes (labeled by phase: gzip/s3/pg); alert if rate > 0 sustained"
    );
    describe_counter!(
        "rio_scheduler_log_flush_dropped_total",
        "Final-flush requests dropped due to flusher channel backpressure"
    );
    describe_counter!(
        "rio_scheduler_log_forward_dropped_total",
        "Log batches dropped (actor channel backpressure). Lines are still in the ring buffer."
    );
    describe_histogram!(
        "rio_scheduler_critical_path_accuracy",
        "Predicted vs actual completion ratio (actual/estimated; 1.0=perfect, >1.0=underestimate)"
    );
    describe_counter!(
        "rio_scheduler_size_class_assignments_total",
        "Assignments per size class (labeled by class name)"
    );
    describe_counter!(
        "rio_scheduler_misclassifications_total",
        "Builds that exceeded 2x their class cutoff duration (triggers penalty EMA overwrite)"
    );
    describe_counter!(
        "rio_scheduler_ema_proactive_updates_total",
        "Mid-build cgroup memory.peak samples that exceeded the EMA and overwrote it proactively. \
         Same penalty-overwrite as misclassifications_total but triggered BEFORE completion — \
         next submit is right-sized without waiting for an OOM->retry cycle."
    );
    describe_counter!(
        "rio_scheduler_class_drift_total",
        "Builds where classify(actual) != assigned_class. Cutoff-drift signal, labeled by \
         assigned_class+actual_class. Distinct from misclassifications_total (penalty trigger, \
         actual > 2x cutoff) — a build can drift without penalty (barely over cutoff, under 2x)."
    );
    describe_gauge!(
        "rio_scheduler_cutoff_seconds",
        "Duration cutoff per class (labeled by class; initialized from \
         config, live-updated hourly by the SITA-E rebalancer)"
    );
    describe_gauge!(
        "rio_scheduler_class_queue_depth",
        "Deferred derivations per target class (snapshot per dispatch pass)"
    );
    describe_gauge!(
        "rio_scheduler_class_load_fraction",
        "Per-class sum(duration)/total from the last rebalancer pass (labeled by class). \
         SITA-E's target is 1/N each — deviation means the EMA hasn't converged yet, or \
         the workload distribution shifted faster than the smoothing can track."
    );
    describe_gauge!(
        "rio_scheduler_fod_queue_depth",
        "FODs deferred waiting for a fetcher (snapshot per dispatch pass). \
         Sustained nonzero → scale FetcherPool.spec.replicas."
    );
    describe_gauge!(
        "rio_scheduler_fetcher_utilization",
        "Fraction of fetchers currently running a build (busy/total). \
         Emitted per dispatch pass alongside fod_queue_depth."
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
        "Scheduler state recoveries from PG after LeaderAcquired"
    );
    describe_histogram!(
        "rio_scheduler_recovery_duration_seconds",
        "Time to reconstruct actor state from PG on LeaderAcquired"
    );
    describe_counter!(
        "rio_scheduler_worker_disconnects_total",
        "Worker stream disconnects (graceful and ungraceful; labeled by reason if available)"
    );
    describe_counter!(
        "rio_scheduler_cancel_signals_total",
        "CancelSignal messages sent to workers (build cancellation propagation)"
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
        "rio_scheduler_estimator_refresh_total",
        "Duration estimator refresh cycles (re-reads build_history EMAs)"
    );
    describe_histogram!(
        "rio_scheduler_build_graph_edges",
        "Edge count per GetBuildGraph response. High p99 (>10k) = unusually \
         dense DAG approaching the implicit subgraph bound."
    );
    describe_counter!(
        "rio_scheduler_ca_hash_compares_total",
        "CA early-cutoff output-hash lookups against the content index on \
         successful completion (labeled by outcome=match|miss|skipped_after_miss). \
         High match ratio → CA derivations rebuilding identical content; \
         cutoff-propagate will skip downstream work. skipped_after_miss counts \
         outputs short-circuited after an earlier miss in the same derivation."
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
        "Sum of est_duration of skipped derivations. Lower-bound estimate of \
         wall-clock saved by CA early-cutoff (est_duration is the Estimator's \
         EMA, not actual — a derivation that's never run has no actual). \
         Paired with saves_total for avg-seconds-per-save."
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
        "Time from a derivation entering Ready to being Assigned. Same \
         measurement as assignment_latency_seconds (both fed from \
         DerivationState.ready_at); this is the dashboard-facing name."
    );
    describe_counter!(
        "rio_scheduler_broadcast_lagged_total",
        "BuildEvent broadcast events skipped by lagging subscribers \
         (sum of RecvError::Lagged(n) across all bridge tasks). Non-zero \
         under sustained event burst (large DAG, many concurrent drvs \
         emitting Log lines) — gateway can't drain fast enough (I-144)."
    );
}
