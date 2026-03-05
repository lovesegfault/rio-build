//! Build executor with FUSE store for rio-build.
//!
//! Receives build assignments from the scheduler, runs builds using
//! nix-daemon within an overlayfs+FUSE environment, and uploads
//! results to the store.
//!
//! # Architecture
//!
//! ```text
//! rio-worker binary
//! +-- gRPC clients
//! |   +-- WorkerService.BuildExecution (bidi stream to scheduler)
//! |   +-- WorkerService.Heartbeat (periodic to scheduler)
//! |   +-- StoreService (fetch inputs, upload outputs)
//! +-- FUSE daemon (fuse/)
//! |   +-- Mount /nix/store via fuser 0.17
//! |   +-- lookup/getattr -> StoreService.QueryPathInfo
//! |   +-- read/readdir -> SSD cache or StoreService.GetPath
//! |   +-- LRU cache on local SSD (cache.rs)
//! +-- Build executor (executor.rs)
//! |   +-- Overlay management (overlay.rs)
//! |   +-- Synthetic DB generation (synth_db.rs)
//! |   +-- Log streaming (log_stream.rs)
//! |   +-- Output upload (upload.rs)
//! +-- Heartbeat loop (runtime.rs, 10s interval)
//! ```

pub mod executor;
pub mod fuse;
pub mod health;
pub mod log_stream;
pub mod overlay;
pub mod runtime;
pub mod synth_db;
pub mod upload;

// Re-exports for main.rs — keeps `use rio_worker::{...}` imports stable
// after the lib.rs → runtime.rs extraction.
pub use runtime::{BloomHandle, BuildSpawnContext, build_heartbeat_request, spawn_build_task};

/// Register `# HELP` descriptions for all worker metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Descriptions
/// sourced from docs/src/observability.md (the Worker Metrics table).
/// See rio_gateway::describe_metrics for rationale.
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "rio_worker_builds_total",
        "Total builds executed (labeled by outcome: success/failure)"
    );
    describe_gauge!(
        "rio_worker_builds_active",
        "Currently running builds on this worker"
    );
    describe_counter!(
        "rio_worker_uploads_total",
        "Output uploads (labeled by status: success/exhausted)"
    );
    describe_histogram!(
        "rio_worker_build_duration_seconds",
        "Per-derivation build time"
    );
    describe_gauge!(
        "rio_worker_fuse_cache_size_bytes",
        "FUSE SSD cache usage in bytes"
    );
    describe_counter!(
        "rio_worker_fuse_cache_hits_total",
        "FUSE cache hits (local symlink_metadata succeeded)"
    );
    describe_counter!(
        "rio_worker_fuse_cache_misses_total",
        "FUSE cache misses (fetch from remote store required)"
    );
    describe_histogram!(
        "rio_worker_fuse_fetch_duration_seconds",
        "Store path fetch latency (gRPC GetPath + stream drain)"
    );
    describe_counter!(
        "rio_worker_overlay_teardown_failures_total",
        "Overlay unmount failures (leaked mount); alert if rate > 0"
    );
}
