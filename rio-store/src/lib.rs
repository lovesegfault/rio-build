//! NAR content-addressable store.
//!
//! PostgreSQL-backed metadata (`narinfo`, `manifests`) with FastCDC
//! chunk deduplication, moka chunk cache, and ed25519 narinfo
//! [`signing`]. Serves `StoreService` + `ChunkService` gRPC (see
//! [`grpc`]).
//!
//! # Feature flags
//!
//! - `server` (default): the full store — gRPC services, S3 backend,
//!   chunked CAS, GC, signing, substitution. Pulls the heavy dep tree
//!   (aws-sdk-s3, moka, reqwest, tonic, …).
//! - `schema`: lean subset for crates that only need to read/write the
//!   shared PG tables directly (rio-scheduler's CA resolver). Exposes
//!   [`error`] + [`realisations`]; compiles with `sqlx` + `thiserror` +
//!   `tracing` + `hex` only. Exists so the scheduler doesn't carry a
//!   raw-SQL copy of `realisations::query` just to avoid the server dep
//!   cascade.
//! - `test-utils`: test fixtures (`test_helpers`, `MIGRATOR`). Implies
//!   `server`.

// ---------------------------------------------------------------------------
// Always-on (`schema` feature surface)
// ---------------------------------------------------------------------------

pub mod error;
// Per ADR-018 §3 resolution logic belongs in the scheduler, but the
// scheduler accesses the same `realisations` table on the shared pool.
// Exported pub so rio-scheduler can call `rio_store::realisations::query`
// instead of duplicating the raw SQL. Single owner for the table's SQL
// means schema changes touch one crate.
pub mod realisations;

// ---------------------------------------------------------------------------
// Server-only (`server` feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "server")]
pub mod admission;
#[cfg(feature = "server")]
pub mod backend;
#[cfg(feature = "server")]
pub mod cas;
#[cfg(feature = "server")]
pub(crate) mod chunker;
#[cfg(feature = "server")]
pub mod gc;
#[cfg(feature = "server")]
pub mod grpc;
#[cfg(feature = "server")]
pub(crate) mod ingest;
// pub (not pub(crate)) so the fuzz target at rio-store/fuzz/ can call
// Manifest::deserialize. The fuzz crate is a separate workspace root.
#[cfg(feature = "server")]
pub mod manifest;
#[cfg(feature = "server")]
pub(crate) mod metadata;
#[cfg(feature = "server")]
pub mod migrations;
#[cfg(feature = "server")]
pub mod signing;
#[cfg(feature = "server")]
pub mod substitute;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_helpers;

/// Shared sqlx migrator for the `migrations/` directory. Embeds
/// migration SQL at compile time via `sqlx::migrate!`.
///
/// Gated on `test`/`test-utils` — the rio-store/fuzz/ workspace compiles
/// this lib as a dep without the feature, and its source filter may
/// exclude migrations/. sqlx::migrate! reads files at COMPILE time, so
/// even an unused static can break the fuzz build. The cfg-gate means
/// the macro only expands for unit tests or downstream test fixtures
/// (which enable `test-utils` and have migrations/ in scope).
#[cfg(any(test, feature = "test-utils"))]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Histogram bucket boundaries for `rio_store_substitute_duration_seconds`.
///
/// narinfo fetch (~50ms) + NAR download + ingest. A 500MB toolchain at
/// 50MB/s is ~10s download + ~10s ingest; cache.nixos.org's largest paths
/// (chromium, llvm) are ~1-2GB → 60s+. The default 10s top would lose all
/// of those in `+Inf`. 10ms low end for narinfo-only short-circuits; 120s
/// top for the largest paths.
#[cfg(feature = "server")]
const SUBSTITUTE_DURATION_BUCKETS: &[f64] =
    &[0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// Per-crate histogram bucket overrides, passed to
/// `rio_common::server::bootstrap` → `init_metrics`. Every
/// `describe_histogram!` in this crate must have an entry here OR be in
/// the `DEFAULT_BUCKETS_OK` exemption list (`tests/metrics_registered.rs`);
/// histograms not listed fall through to the global `[0.005..10.0]` default.
#[cfg(feature = "server")]
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    (
        "rio_store_substitute_duration_seconds",
        SUBSTITUTE_DURATION_BUCKETS,
    ),
    (
        // Same range: ⌈N_uncached/128⌉ × RTT spans the 0.01-120s
        // envelope (153k paths @ 30ms ≈ 36s; the 60-120s tail is
        // 429-retry sleeps).
        "rio_store_check_available_duration_seconds",
        SUBSTITUTE_DURATION_BUCKETS,
    ),
];

/// Register `# HELP` descriptions for all store metrics.
///
/// Call from `main()` immediately after `init_metrics()`. Descriptions
/// sourced from docs/src/observability.md (the Store Metrics table).
/// See rio_gateway::describe_metrics for rationale.
// r[impl obs.metric.store]
#[cfg(feature = "server")]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "rio_store_put_path_total",
        "Total PutPath operations (per store path; PutPathBatch counts each output)"
    );
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
        "rio_store_get_path_total",
        "Total GetPath operations (incremented on successful whole-NAR verify)"
    );
    describe_histogram!(
        "rio_store_get_path_duration_seconds",
        "GetPath latency (stream_path entry to whole-NAR verify)"
    );
    describe_gauge!(
        "rio_store_get_path_active",
        "GetPath body-stream tasks currently writing (drives SIGTERM stream-drain)"
    );
    describe_counter!(
        "rio_store_integrity_failures_total",
        "GetPath content integrity check failures (bitrot/corruption)"
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
        "rio_store_service_token_accepted_total",
        "PutPath HMAC checks bypassed via x-rio-service-token (labeled by caller)"
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
        "rio_store_gc_orphan_reap_failed_total",
        "Per-row reap_one failures during the orphan scanner's loop \
         (e.g. chunks_refcount_nonneg CHECK violation from a pre-existing \
         accounting bug). The scan continues; sustained nonzero = a poison \
         row needs manual intervention."
    );
    describe_counter!(
        "rio_store_gc_chunk_orphan_swept_total",
        "Chunks left at refcount=0 by an aborted PutPath/PutPathBatch (uploader \
         crashed mid-stream) and reaped by sweep_orphan_chunks after grace-TTL. \
         Sustained nonzero = builder upload path is crashing repeatedly."
    );
    describe_gauge!(
        "rio_store_gc_sweep_paths_remaining",
        "Paths not yet processed by the in-progress GC sweep. Ticks down per \
         batch commit; 0 between sweeps. Long-tail = sweep stalled or PG slow."
    );
    describe_counter!(
        "rio_store_sign_tenant_key_fallback_total",
        "PutPath/PutPathBatch tenant-key lookups that failed (transient PG \
         error) and fell back to the cluster key. The upload still succeeds \
         with a valid cluster signature; tenants that trust ONLY their own \
         key will see verify failures. Alert if sustained nonzero."
    );
    describe_counter!(
        "rio_store_sign_empty_refs_total",
        "SignPath requests for non-CA paths with zero references. Suspicious \
         for non-leaf derivations — GC cannot protect deps without the ref \
         graph. Check worker ref-scanner if sustained."
    );
    describe_counter!(
        "rio_store_substitute_total",
        "Upstream substitution attempts, labeled by result (hit/miss/error) \
         and tenant (UUID). Per-upstream debugging detail is in the \
         debug!/warn! log lines (which carry upstream=<url>); the metric \
         label is bounded by tenant count, not by tenant-supplied URL."
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
        "Upstream substitution NAR hash or size mismatches, labeled by \
         tenant (UUID). Nonzero is a security-relevant signal: upstream \
         served corrupt or tampered bytes / a lying NarSize."
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
         HEAD was issued)."
    );
    describe_counter!(
        "rio_store_substitute_probe_ratelimited_total",
        "Upstream HEAD/GET probes that returned 429, labeled by tenant. \
         The rate-limited subset is retried (≤3 passes) after honoring \
         Retry-After; concurrency is halved when >10% of a pass 429s."
    );
    describe_histogram!(
        "rio_store_check_available_duration_seconds",
        "check_available wall-clock (HEAD-probe phase of FindMissingPaths). \
         ⌈N_uncached/128⌉ × RTT plus any 429 retry sleeps. p99 informs \
         the scheduler's MERGE_FMP_TIMEOUT."
    );

    describe_gauge!(
        "rio_store_substitute_admission_utilization",
        "try_substitute admission-gate utilization: (capacity - available) / \
         capacity. Updated on each acquire AND each GetLoad call. Can saturate \
         independently of pg_pool_utilization (upstream HTTP bottleneck)."
    );
    describe_counter!(
        "rio_store_substitute_admission_rejected_total",
        "try_substitute calls rejected with ResourceExhausted after waiting \
         SUBSTITUTE_ADMISSION_WAIT (25s) for a permit. Sustained non-zero = \
         genuine per-replica overload; ComponentScaler should already be \
         reacting via the GetLoad utilization signal."
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
    // Zero is a placeholder; the first drain tick (≤30s) overwrites
    // with the real count.
    metrics::gauge!("rio_store_s3_deletes_pending").set(0.0);
    metrics::gauge!("rio_store_s3_deletes_stuck").set(0.0);
    // Same pre-register reasoning: until the first GetLoad call (or
    // forever, if no ComponentScaler is deployed) the gauge would be
    // absent. 0.0 is the correct initial value (idle pool at boot).
    metrics::gauge!("rio_store_pg_pool_utilization").set(0.0);
    // Same: until the first try_substitute_on_miss (or forever, if no
    // tenant has upstreams configured). 0.0 = no permits held at boot.
    metrics::gauge!("rio_store_substitute_admission_utilization").set(0.0);
    // Same pre-register reasoning: between sweeps (or on a store that
    // never GCs) the gauge would be absent. 0.0 = no sweep in progress.
    metrics::gauge!("rio_store_gc_sweep_paths_remaining").set(0.0);
}
