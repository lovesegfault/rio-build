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
//! - [`grpc`]: SchedulerService + WorkerService gRPC implementations

pub mod actor;
pub mod admin;
pub(crate) mod assignment;
pub(crate) mod critical_path;
pub mod dag;
pub mod db;
pub(crate) mod estimator;
pub mod grpc;
pub mod logs;
pub(crate) mod queue;
pub mod state;

// Re-export for main.rs — `assignment` is pub(crate) but the config struct
// is part of the binary's TOML schema.
pub use assignment::SizeClassConfig;

/// Shared sqlx migrator for the `migrations/` directory. See
/// rio-store's MIGRATOR for rationale — same pattern.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Register `# HELP` descriptions for all scheduler metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Descriptions
/// sourced from docs/src/observability.md (the Scheduler Metrics table).
/// See rio_gateway::describe_metrics for rationale.
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
         Missing from a dispatch = either leaf drv (no children), or bloom \
         filter says worker already has everything (scoring working)."
    );
    describe_counter!(
        "rio_scheduler_prefetch_paths_sent_total",
        "Total paths in sent PrefetchHints. Divide by hints_sent for avg \
         paths-per-hint. High avg = workers cold (poor locality) or bloom stale."
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
        "rio_scheduler_log_lines_forwarded_total",
        "Log lines forwarded via BuildEvent::Log (worker -> scheduler -> gateway broadcast)"
    );
    describe_counter!(
        "rio_scheduler_log_flush_total",
        "Successful S3 log flushes (labeled by kind: final/periodic)"
    );
    describe_counter!(
        "rio_scheduler_log_flush_failures_total",
        "Failed S3 log flushes (labeled by phase: s3/pg); alert if rate > 0 sustained"
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
    describe_gauge!(
        "rio_scheduler_cutoff_seconds",
        "Duration cutoff per class (labeled by class; set once at config load, static)"
    );
    describe_gauge!(
        "rio_scheduler_class_queue_depth",
        "Deferred derivations per target class (snapshot per dispatch pass)"
    );
    describe_counter!(
        "rio_scheduler_cache_check_circuit_open_total",
        "Circuit-breaker open transitions (store unreachable for 5 consecutive checks); alert if > 0"
    );
}
