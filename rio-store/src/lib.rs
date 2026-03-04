pub mod backend;
pub mod cache_server;
pub mod cas;
pub(crate) mod chunker;
pub(crate) mod content_index;
pub mod grpc;
// pub (not pub(crate)) so the fuzz target at rio-store/fuzz/ can call
// Manifest::deserialize. The fuzz crate is a separate workspace root.
pub mod manifest;
pub(crate) mod metadata;
pub(crate) mod realisations;
pub mod signing;
pub(crate) mod validate;

/// Register `# HELP` descriptions for all store metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Descriptions
/// sourced from docs/src/observability.md (the Store Metrics table).
/// See rio_gateway::describe_metrics for rationale.
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!("rio_store_put_path_total", "Total PutPath operations");
    describe_histogram!("rio_store_put_path_duration_seconds", "PutPath latency");
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
        "S3 API calls (labeled by operation: put_object/get_object/head_object)"
    );
    describe_counter!(
        "rio_store_chunk_cache_hits_total",
        "moka chunk cache hits (for cross-instance aggregation)"
    );
    describe_counter!(
        "rio_store_chunk_cache_misses_total",
        "moka chunk cache misses"
    );
}
