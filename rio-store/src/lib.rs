//! NAR content-addressable store.
//!
//! PostgreSQL-backed metadata (`narinfo`, `manifests`) with FastCDC
//! chunk deduplication, moka chunk cache, ed25519 narinfo [`signing`],
//! and an axum binary-cache HTTP server. Serves `StoreService` +
//! `ChunkService` gRPC (see [`grpc`]).

pub mod backend;
pub mod cache_server;
pub mod cas;
pub(crate) mod chunker;
pub(crate) mod content_index;
pub mod gc;
pub mod grpc;
// pub (not pub(crate)) so the fuzz target at rio-store/fuzz/ can call
// Manifest::deserialize. The fuzz crate is a separate workspace root.
pub mod manifest;
pub(crate) mod metadata;
pub(crate) mod realisations;
pub mod signing;
pub(crate) mod validate;

/// Shared sqlx migrator for the `migrations/` directory. Embeds
/// migration SQL at compile time via `sqlx::migrate!`.
///
/// `#[cfg(test)]` (not `pub`) — the rio-store/fuzz/ workspace compiles
/// this lib as a dep, and its source filter doesn't include migrations/.
/// sqlx::migrate! reads files at COMPILE time, so even an unused static
/// breaks the fuzz build. cfg(test) means the macro only expands when
/// building the lib's own unit tests (not as a dep). Integration tests
/// in tests/grpc/ keep their own copy — they compile the lib WITHOUT
/// cfg(test), so they can't see this one.
#[cfg(test)]
pub(crate) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Register `# HELP` descriptions for all store metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Descriptions
/// sourced from docs/src/observability.md (the Store Metrics table).
/// See rio_gateway::describe_metrics for rationale.
// r[impl obs.metric.store]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!("rio_store_put_path_total", "Total PutPath operations");
    describe_histogram!("rio_store_put_path_duration_seconds", "PutPath latency");
    describe_counter!(
        "rio_store_put_path_bytes_total",
        "Bytes accepted via PutPath (nar_size on success)"
    );
    describe_counter!(
        "rio_store_get_path_bytes_total",
        "Bytes served via GetPath (nar_size on stream start)"
    );
    describe_counter!(
        "rio_store_integrity_failures_total",
        "GetPath content integrity check failures (bitrot/corruption)"
    );
    describe_gauge!(
        "rio_store_chunks_total",
        "Total chunks in storage (piggybacked on FindMissingChunks)"
    );
    describe_gauge!(
        "rio_store_chunk_dedup_ratio",
        "Per-upload dedup ratio (1.0 - missing/total after chunking)"
    );
    describe_counter!(
        "rio_store_s3_requests_total",
        "S3 API calls (labeled by operation: put_object/get_object/head_object/delete_object)"
    );
    describe_counter!(
        "rio_store_chunk_cache_hits_total",
        "moka chunk cache hits (for cross-instance aggregation)"
    );
    describe_counter!(
        "rio_store_chunk_cache_misses_total",
        "moka chunk cache misses"
    );
    describe_counter!(
        "rio_store_gc_path_resurrected_total",
        "Paths skipped by sweep because a new referrer appeared between mark and sweep (race window)"
    );
    describe_counter!(
        "rio_store_hmac_rejected_total",
        "PutPath rejections by HMAC assignment-token check (labeled by reason)"
    );
    describe_counter!(
        "rio_store_hmac_bypass_total",
        "PutPath HMAC checks bypassed via mTLS cert CN=rio-gateway (labeled by cn)"
    );
    describe_gauge!(
        "rio_store_s3_deletes_pending",
        "Rows in pending_s3_deletes awaiting drain"
    );
    describe_gauge!(
        "rio_store_s3_deletes_stuck",
        "pending_s3_deletes rows at max retry attempts (alert if > 0)"
    );
    describe_counter!(
        "rio_store_gc_chunk_resurrected_total",
        "Chunks skipped by drain because PutPath cleared deleted=false after sweep enqueued (TOCTOU catch)"
    );

    // Pre-register drain gauges at 0. metrics-rs only materializes a gauge
    // on first .set(); describe_gauge! alone doesn't. drain_once (gc/drain.rs)
    // sets these every 30s, but:
    //   - for the first 30s after boot, PromQL `_stuck > 0` can't tell
    //     "0" from "store hasn't reported yet"
    //   - inline-only (non-S3) deployments never run drain_once, so the
    //     gauges stay absent forever without this
    // Zero is the correct initial value (no pending deletes at boot).
    metrics::gauge!("rio_store_s3_deletes_pending").set(0.0);
    metrics::gauge!("rio_store_s3_deletes_stuck").set(0.0);
}
