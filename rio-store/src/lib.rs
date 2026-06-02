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
pub mod config;
#[cfg(feature = "server")]
pub mod gc;
#[cfg(feature = "server")]
pub mod grpc;
#[cfg(feature = "server")]
pub(crate) mod ingest;
#[cfg(feature = "server")]
pub mod logs;
// pub (not pub(crate)) so the fuzz target at fuzz/rio-store/ can call
// Manifest::deserialize. The fuzz crate is a separate workspace root.
#[cfg(feature = "server")]
pub mod manifest;
// Substitution-replacement campaign (design §2.2/§5): the store-side
// materialization-job executor. Dormant in Phase A — nothing spawns it
// unless a `materialization.scheduler_addr` is configured (PD-D2).
#[cfg(feature = "server")]
pub mod materialize;
#[cfg(feature = "server")]
pub(crate) mod metadata;
#[cfg(feature = "server")]
pub mod signing;
#[cfg(feature = "server")]
pub mod substitute;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_helpers;

/// Re-export of the shared embedded migrator from `rio-migrations`.
///
/// Gated on `test`/`test-utils` so `rio_store::MIGRATOR` stays out of
/// the public API for non-test consumers — production code goes through
/// `rio_migrations::migrate::run` with `rio_migrations::migrator()`. The
/// re-export exists for the ~200 `TestDb::new(&crate::MIGRATOR)`
/// callsites in this crate's `#[cfg(test)]` modules. `crate::MIGRATOR`
/// and `rio_store::MIGRATOR` resolve to the same static.
#[cfg(any(test, feature = "test-utils"))]
pub use rio_migrations::MIGRATOR;

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

/// Histogram bucket boundaries for `rio_store_gc_collect_cycle_seconds`.
///
/// A collect cycle (fail-closed mark + prepare + report/collect) is
/// budgeted at five minutes of GC-lock-held time at the design-point
/// scale (refcount-formal design §5b); small stores finish in seconds.
/// Boundaries at 240/300 straddle the budget so threshold queries are
/// exact, with headroom to 900 s before +Inf so an over-budget cycle's
/// magnitude is still visible.
#[cfg(feature = "server")]
const GC_COLLECT_CYCLE_BUCKETS: &[f64] = &[
    1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 180.0, 240.0, 300.0, 420.0, 600.0, 900.0,
];

/// Histogram bucket boundaries for `rio_store_chunk_upgrade_tx_seconds`.
///
/// The chunked-upgrade transaction is normally milliseconds-to-seconds
/// (one FOR UPDATE probe, one manifest_data INSERT, one chunks upsert).
/// The 150 s and 240 s boundaries are exactly the
/// RioStoreChunkUpgradeTxSlow alert thresholds (grace/2 and
/// grace − 60 s — the chunk collector's collect-soundness assumption),
/// so the alert's bucket arithmetic is exact rather than interpolated:
/// the warning arm reads a p99 over these buckets, the critical arm
/// counts observations above the 240 s boundary directly (an exact
/// per-violation count, not a quantile). 300 s = grace itself, so an
/// outright assumption violation is also countable exactly.
#[cfg(feature = "server")]
const CHUNK_UPGRADE_TX_BUCKETS: &[f64] = &[
    0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 150.0, 240.0, 300.0,
];

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
    (
        "rio_store_gc_collect_cycle_seconds",
        GC_COLLECT_CYCLE_BUCKETS,
    ),
    (
        "rio_store_chunk_upgrade_tx_seconds",
        CHUNK_UPGRADE_TX_BUCKETS,
    ),
];

/// Registers prometheus metric descriptions. The help strings here are
/// the source for `docs/ref/metrics.typ` — see
/// `xtask/src/regen/docs_data.rs::metrics()` for the data-flow.
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
        "Chunks skipped by drain because PutPath cleared deleted=false after a collect batch enqueued them (TOCTOU catch)"
    );
    describe_counter!(
        "rio_store_gc_path_swept_total",
        "Paths deleted by GC sweep (narinfo + CASCADE). Monotonic over store lifetime."
    );
    describe_counter!(
        "rio_store_gc_s3_key_enqueued_total",
        "S3 keys enqueued to pending_s3_deletes by the chunk-collect cycle's \
         batches (soft-deleted chunks awaiting backend deletion)."
    );
    describe_counter!(
        "rio_store_gc_orphan_reap_failed_total",
        "Per-row reap_one failures during the orphan scanner's loop \
         (e.g. a transient DB error against one placeholder row). The scan \
         continues; sustained nonzero = a poison row needs manual \
         intervention."
    );
    describe_gauge!(
        "rio_store_gc_sweep_paths_remaining",
        "Paths not yet processed by the in-progress GC sweep. Ticks down per \
         batch commit; 0 between sweeps. Long-tail = sweep stalled or PG slow."
    );

    // Lazy chunk collector (gc::collect) — the live collect arm runs as
    // run_gc phase 3 and from the daily backstop; a dry-run GC keeps
    // phase 3 in shadow (report-only) mode.
    describe_gauge!(
        "rio_store_gc_chunks_live",
        "Distinct chunk hashes referenced by at least one existing manifest \
         (any status) at the last collect cycle's mark snapshot (mark-set size)."
    );
    describe_gauge!(
        "rio_store_gc_chunks_would_collect",
        "Chunks a shadow (report-only) collect cycle would soft-delete: not \
         deleted, absent from the mark set, and older than grace measured \
         from GREATEST(created_at, last_referenced_at). Emitted by shadow \
         cycles only (a dry-run GC's phase 3); live cycles do not re-run \
         this full anti-join count — their backlog visibility is \
         rio_store_gc_collect_backlog_chunks."
    );
    describe_counter!(
        "rio_store_gc_chunks_collected_total",
        "Chunks soft-deleted (and, when a chunk backend exists, enqueued to \
         pending_s3_deletes) by live collect cycles. Incremented per \
         committed collect batch. The expected one-time reclamation of \
         historical chunk leaks shows up here after the cutover."
    );
    describe_gauge!(
        "rio_store_gc_collect_backlog_chunks",
        "Estimate of eligible-but-not-yet-collected chunks (drain visibility \
         for the capped collect). Shadow mode: equals would-collect. Live \
         mode: decremented by each cycle's collected count and re-anchored \
         when a pass completes."
    );
    describe_histogram!(
        "rio_store_gc_collect_cycle_seconds",
        "Chunk-collect cycle duration (snapshot + fail-closed mark + prepare \
         + report/collect) — the GC-lock-held window of run_gc phase 3 and \
         the daily backstop. Completed cycles only; aborted cycles count \
         toward rio_store_gc_collect_parse_failures_total instead."
    );
    describe_counter!(
        "rio_store_gc_collect_cycles_total",
        "Chunk-collect cycles by outcome (ok | parse_failure | error). A \
         cycle that stops at the per-cycle victim cap counts as ok; error = \
         the cycle failed against PostgreSQL (counted by the caller: run_gc \
         phase 3 or the backstop). Staleness of ok cycles (summed across \
         replicas) drives the RioStoreGcCollectStalled alert. Cycles run as \
         phase 3 of every GC run and from each replica's daily backstop \
         timer, which arms one full interval after boot (pod boot never \
         triggers a cycle) and skips its tick when another cycle holds the \
         GC advisory lock."
    );
    describe_counter!(
        "rio_store_gc_collect_parse_failures_total",
        "Chunk-collect cycles aborted by the fail-closed mark validation (an \
         existing manifest's chunk_list failed validation). While this is \
         increasing and unremediated, ALL chunk collection is suspended; \
         path GC is unaffected."
    );
    describe_counter!(
        "rio_store_gc_collect_cycles_capped_total",
        "Chunk-collect cycles that stopped at COLLECT_CYCLE_VICTIM_CAP with \
         backlog remaining (the keyset cursor carries the remainder to the \
         next cycle). Sustained increments mean a backlog is draining."
    );
    describe_histogram!(
        "rio_store_chunk_upgrade_tx_seconds",
        "upgrade_manifest_to_chunked transaction duration (begin to commit) — \
         the single chunk-referencing write transaction. Monitors the chunk \
         collector's collect-soundness assumption that no such transaction \
         outlives the collect grace window (alerts at grace/2 and grace-60s)."
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
        "rio_store_substitute_skipped_total",
        "Substitution requests short-circuited WITHOUT contacting any \
         upstream, labeled by reason (no_upstreams: the tenant has zero \
         tenant_upstreams rows; no_http_client: the reqwest client failed \
         to build at startup so this replica cannot reach any upstream). \
         Every skip degrades to a from-source build at the scheduler, so a \
         sustained nonzero no_http_client rate means \
         cache.nixos.org-cached paths are being compiled. Distinct from \
         rio_store_substitute_total{result=miss}, which counts attempts \
         that DID reach the upstreams and definitively missed. Granularity: \
         per singleflight leader (same as result=hit|miss). (The walk-era \
         per-request no_tenant/disabled reasons died with the \
         SubstitutePath RPC; the materialization executor reports a \
         missing tenant as an InfraFailure outcome instead.)"
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
        "rio_store_materialization_executions_total",
        "Materialization job executions finished by this replica's executor, \
         labeled by outcome (success | unobtainable | infra). The store-side \
         half of the substitution-replacement lifecycle rates: pairs with the \
         scheduler's rio_scheduler_materialization_{claims,jobs_resolved}_total. \
         A rising infra share means upstream/network trouble (executions are \
         retried within the scheduler's materialization budget); a rising \
         unobtainable share means requested paths genuinely left the upstreams."
    );
    describe_counter!(
        "rio_store_materialization_pinned_paths_total",
        "Store paths pinned at materialization ingest (pin_kind='materialization', \
         design §5.1 pin-at-ingest). Pairs with the scheduler's §5.3 release \
         lifecycle: a sustained pinned-paths rate with no matching pin releases \
         after jobs resolve and interest goes terminal means materialization \
         pins are accumulating (GC pressure)."
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

    // Build-log ingest (logs::ingest). Emitted by the IngestSession
    // state machine; the AppendLog handler drives it.
    describe_counter!(
        "rio_store_log_ingest_lines_total",
        "Log lines accepted into ingest buffers (post-truncation, pre-cut). \
         The write-side twin of the chunk manifest's line_count sum."
    );
    describe_counter!(
        "rio_store_log_ingest_bytes_total",
        "Log bytes accepted into ingest buffers (post-truncation, uncompressed)."
    );
    describe_counter!(
        "rio_store_log_ingest_rejected_total",
        "Log batches dropped at the ingest input gates, by reason: \
         non_monotonic / line_number_overflow (malformed worker numbering, \
         per-batch, stream stays open), past_final_line_count (lines at or \
         past the execution's recorded end after the completeness seal \
         lands; per-batch, stream stays open, also counted once per \
         straddling batch that is truncated rather than dropped whole) or \
         byte_cap (per-execution cap, stream-fatal). Sustained non-zero = \
         a misbehaving or hostile builder."
    );
    describe_counter!(
        "rio_store_log_chunks_written_total",
        "Log chunks durably committed (S3 object + drv_log_chunks manifest row)."
    );
    describe_counter!(
        "rio_store_log_chunk_write_failures_total",
        "Failed chunk-cut attempts (compression, S3 PUT, or manifest INSERT). \
         Each failure burns a chunk_seq and restores the lines to the buffer; \
         3 consecutive failures abort the stream so the builder fails over. \
         Alert on sustained non-zero: log durability is degraded."
    );
    describe_counter!(
        "rio_store_log_tail_dropped_total",
        "Live-tail fan-out batches dropped because a TailLog subscriber's \
         queue was full. Lossy by contract (a slow reader must never \
         backpressure ingest); the reader recovers the lines from the \
         manifest on its next reconnect."
    );
    describe_counter!(
        "rio_store_log_ingest_streams_aborted_total",
        "AppendLog streams aborted server-side, by reason: cut_failures \
         (3 consecutive failed chunk commits), stale_buffer (buffered \
         lines older than 2x the cut interval), lease_lost (another \
         replica stole the ingest session), or chunk_cap (the \
         per-execution chunk-count bound). The builder reconnects and \
         replays its un-acked tail to another replica. Alert on \
         sustained non-zero: this replica cannot durably store logs."
    );
    describe_counter!(
        "rio_store_log_read_data_loss_total",
        "TailLog reads that hit a drv_log_chunks manifest row whose S3 \
         object is missing. Each increment is a hole in a stored build \
         log that cannot be recovered. Alert on ANY increment."
    );
    describe_counter!(
        "rio_store_log_tail_proxied_total",
        "TailLog requests relayed to the replica holding the execution's \
         live ingest session (the reader landed on a different replica). \
         Proportional to (replicas - 1) / replicas of live-tail opens; a \
         sudden drop to zero with >1 replica suggests the peer URL \
         template no longer resolves."
    );
    describe_counter!(
        "rio_store_log_tail_proxy_failures_total",
        "Cross-replica TailLog relays that failed to reach the owning \
         replica (connect timeout, DNS, or the forwarded call erroring). \
         Each one degraded a reader to the history-only view. Sustained \
         non-zero with healthy replicas means the log_peer_url_template \
         does not match the Service topology."
    );
    describe_counter!(
        "rio_store_log_sweep_executions_deleted_total",
        "Expired drv_executions rows deleted by the hourly build-log TTL \
         sweep (executions older than log_retention_days)."
    );
    describe_counter!(
        "rio_store_log_sweep_chunks_deleted_total",
        "drv_log_chunks manifest rows deleted by the build-log TTL sweep."
    );
    describe_counter!(
        "rio_store_log_sweep_objects_deleted_total",
        "Chunk objects deleted from the backend by the build-log TTL \
         sweep. Lags ..._chunks_deleted_total when a backend delete \
         fails; the difference is orphaned objects bounded by the S3 \
         lifecycle rule on logs/."
    );
    describe_gauge!(
        "rio_store_log_active_ingest_sessions",
        "AppendLog streams currently open on this replica. Each holds a \
         2x-cut-threshold reservation of log_bytes_budget and one \
         log_max_streams permit."
    );
    describe_gauge!(
        "rio_store_log_tail_subscribers",
        "Live TailLog follow-subscriptions currently attached to this \
         replica's ingest sessions."
    );
    // Pre-register at 0 so PromQL can tell "no sessions" from "store
    // hasn't reported yet" (same reasoning as the drain gauges below).
    metrics::gauge!("rio_store_log_active_ingest_sessions").set(0.0);
    metrics::gauge!("rio_store_log_tail_subscribers").set(0.0);

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
    // Chunk-collect gauges: zero until the first cycle (or forever on
    // a store that never GCs and whose backstop hasn't ticked) — same
    // pre-register reasoning as the drain gauges above.
    metrics::gauge!("rio_store_gc_chunks_live").set(0.0);
    metrics::gauge!("rio_store_gc_chunks_would_collect").set(0.0);
    metrics::gauge!("rio_store_gc_collect_backlog_chunks").set(0.0);
    // Chunk-collect counters: pre-register at 0 so the staleness alert
    // (sum(increase(rio_store_gc_collect_cycles_total{outcome="ok"}[25h]))
    // == 0, aggregated across replicas with for: 30m) and the
    // parse-failure alert have a series to evaluate from boot instead
    // of returning empty until the first cycle/failure. The error
    // outcome is pre-registered for the same reason: a store whose
    // every cycle fails against PostgreSQL surfaces immediately instead
    // of staying invisible until the stalled alert's 25h window.
    metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "ok").absolute(0);
    metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "parse_failure")
        .absolute(0);
    metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "error").absolute(0);
    metrics::counter!("rio_store_gc_collect_parse_failures_total").absolute(0);
    metrics::counter!("rio_store_gc_collect_cycles_capped_total").absolute(0);
    metrics::counter!("rio_store_gc_chunks_collected_total").absolute(0);
}
