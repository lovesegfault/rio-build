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
pub mod migrations;
// pub(crate) per ADR-018 §3 — resolution logic belongs in the scheduler,
// which re-implements query/insert directly against PG (both crates share
// the pool + migrations). See rio-scheduler/src/ca/resolve.rs for the
// scheduler-side mirror. Schema changes to the realisations table MUST
// update both sites.
pub(crate) mod realisations;
pub mod signing;
pub mod substitute;
#[cfg(test)]
pub(crate) mod test_helpers;
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
    describe_counter!(
        "rio_store_putpath_retries_total",
        "PutPath retriable rejections (labeled by reason: serialization|\
         deadlock|placeholder_missing|connection|resource_exhausted|\
         concurrent_upload). Client retries on aborted/unavailable. \
         Sustained high deadlock/connection rate = PG-side problem."
    );
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
    describe_counter!(
        "rio_store_gc_path_swept_total",
        "Paths deleted by GC sweep (narinfo + CASCADE). Monotonic over store lifetime."
    );
    describe_counter!(
        "rio_store_gc_s3_key_enqueued_total",
        "S3 keys enqueued to pending_s3_deletes by GC sweep (zeroed-refcount chunks)."
    );
    describe_counter!(
        "rio_store_gc_chunk_orphan_swept_total",
        "Standalone chunks (PutChunk at refcount=0, no PutPath followed) reaped \
         by sweep_orphan_chunks after grace-TTL expired. Nonzero = workers are \
         crashing between PutChunk and PutPath; sustained high = client bug."
    );
    describe_gauge!(
        "rio_store_gc_sweep_paths_remaining",
        "Paths not yet processed by the in-progress GC sweep. Ticks down per \
         batch commit; 0 between sweeps. Long-tail = sweep stalled or PG slow."
    );
    describe_gauge!(
        "rio_store_gc_empty_refs_pct",
        "Percent of sweep-eligible paths with zero references at GC time. \
         High values trigger the 'suspicious GC sweep' error log (threshold \
         configurable); sustained high = upstream ref-scanner likely broken."
    );
    describe_counter!(
        "rio_store_sign_empty_refs_total",
        "SignPath requests for non-CA paths with zero references. Suspicious \
         for non-leaf derivations — GC cannot protect deps without the ref \
         graph. Check worker ref-scanner if sustained."
    );
    describe_counter!(
        "rio_store_substitute_total",
        "Upstream substitution attempts (labeled by result: hit/miss/error, \
         upstream: URL). Hit means ingested; miss means no upstream had it."
    );
    describe_counter!(
        "rio_store_substitute_bytes_total",
        "Bytes ingested via upstream substitution (nar_size on hit)"
    );
    describe_histogram!(
        "rio_store_substitute_duration_seconds",
        "Upstream substitution latency (narinfo fetch + NAR download + ingest)"
    );
    describe_counter!(
        "rio_store_substitute_stale_reclaimed_total",
        "Stale 'uploading' placeholders reclaimed on the substitution hot \
         path. Nonzero expected under network churn; sustained high suggests \
         upstream instability or aggressive pod rollouts."
    );
    describe_counter!(
        "rio_store_substitute_integrity_failures_total",
        "Upstream substitution NAR hash mismatches. Nonzero is a \
         security-relevant signal: upstream served corrupt or tampered bytes."
    );
    describe_counter!(
        "rio_store_putpath_stale_reclaimed_total",
        "Stale 'uploading' placeholders reclaimed on the PutPath hot path \
         (I-207). Nonzero expected under fetcher churn; sustained high \
         suggests under-sized fetcher pods (see I-208)."
    );
    describe_counter!(
        "rio_store_substitute_probe_cache_hits_total",
        "check_available HEAD-probe cache hits (positive or negative cached \
         result; no upstream HEAD made for this path)."
    );
    describe_counter!(
        "rio_store_substitute_probe_cache_misses_total",
        "check_available HEAD-probe cache misses (path uncached; an upstream \
         HEAD was issued, or the batch hit the 4096-uncached cap)."
    );

    // r[impl obs.metric.store-pg-pool]
    describe_gauge!(
        "rio_store_pg_pool_utilization",
        "PG connection-pool utilization: (size - num_idle) / max_connections. \
         Updated on each StoreAdminService.GetLoad call (ComponentScaler 10s tick). \
         Sustained > 0.8 = under-provisioned store replicas (I-105 cliff approaching)."
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
    // Same pre-register reasoning: until the first GetLoad call (or
    // forever, if no ComponentScaler is deployed) the gauge would be
    // absent. 0.0 is the correct initial value (idle pool at boot).
    metrics::gauge!("rio_store_pg_pool_utilization").set(0.0);
    // Same pre-register reasoning: between sweeps (or on a store that
    // never GCs) the gauge would be absent. 0.0 = no sweep in progress.
    metrics::gauge!("rio_store_gc_sweep_paths_remaining").set(0.0);
}
