# Observability

rio-build provides three pillars of observability: logs, metrics, and traces.

## Build Log Storage

Build logs are stored durably for post-build analysis and the [dashboard](./components/dashboard.md) log viewer.

### Storage Format

Build logs are stored in S3 as gzipped blobs:

```
logs/{build_id}/{derivation_hash}.log.gz
```

Metadata (byte offsets, timestamps, line counts) is stored in PostgreSQL for efficient seeking and pagination.

### Log Lifecycle

r[obs.log.batch-64-100ms]
Log lines are batched (up to 64 lines or 100ms, whichever first) in `BuildLogBatch` messages.

r[obs.log.periodic-flush]
The scheduler flushes buffers to S3 periodically (every 30s) during active builds, not only on completion --- bounds log loss to at most 30s on failover.

```mermaid
sequenceDiagram
    participant Worker
    participant Scheduler
    participant S3

    Worker->>Scheduler: BuildLogBatch (batched, <=64 lines or 100ms)
    Note over Scheduler: Buffer in memory<br/>(per-derivation ring buffer)
    Worker->>Scheduler: CompletionReport
    Scheduler->>S3: Async flush (gzip + upload)
    Note over Scheduler: Write metadata to PG
```

1. Workers stream log lines to the scheduler via `BuildLogBatch` messages in the `BuildExecution` stream. Lines are batched (up to 64 lines or 100ms, whichever comes first) for efficiency.
2. The scheduler buffers logs in an in-memory ring buffer per active derivation.
3. On derivation completion, the scheduler asynchronously flushes the buffer to S3 as a gzipped blob and writes metadata (byte offsets, timestamps) to PostgreSQL.

> **Periodic flush:** Logs are also flushed to S3 periodically (every 30s) during active builds, not only on completion. This bounds log loss to at most 30s of output if the scheduler fails over.

> **Log durability tradeoff:** The 30-second flush interval is a deliberate tradeoff between write amplification and data loss. Flushing more frequently increases S3 PUT costs and scheduler CPU usage; flushing less frequently increases the window of log loss on crash. For most builds, 30s of lost logs is acceptable --- the build itself will be retried and new logs will be generated. For long-running builds where the final 30s of output is critical for debugging, consider a future enhancement: workers could write a local log file as a write-ahead log (WAL) that survives scheduler restarts, with the scheduler draining the WAL on recovery. Not currently planned.
4. The `AdminService.GetBuildLogs` RPC reads from the in-memory buffer for active builds and from S3 for completed builds.

### Log Serving

| Build State | Log Source |
|-------------|-----------|
| Active (building) | In-memory ring buffer on scheduler |
| Completed | S3 blob (gzipped), seekable via PG metadata |
| Failed | S3 blob (flushed on failure as well) |

## Metrics

Each component exposes a Prometheus-compatible `/metrics` endpoint via `metrics-exporter-prometheus`.

### Gateway Metrics

r[obs.metric.gateway]
| Metric | Type | Description |
|--------|------|-------------|
| `rio_gateway_connections_total` | Counter | Total SSH connections |
| `rio_gateway_connections_active` | Gauge | Currently active connections |
| `rio_gateway_opcodes_total` | Counter | Protocol opcodes handled (labeled by opcode) |
| `rio_gateway_opcode_duration_seconds` | Histogram | Per-opcode latency |
| `rio_gateway_handshakes_total` | Counter | Protocol handshakes completed (labeled by result: success/rejected/failure) |
| `rio_gateway_channels_active` | Gauge | Currently active SSH channels |
| `rio_gateway_errors_total` | Counter | Protocol errors (labeled by type) |
| `rio_gateway_bytes_total` | Counter | Bytes forwarded to/from SSH client (labeled by `direction`: `rx`/`tx`) |
| `rio_gateway_quota_rejections_total` | Counter | SubmitBuild rejected because tenant is over store quota (labeled by `tenant`) |
| `rio_gateway_auth_degraded_total` | Counter | SSH auth accepted but tenant identity degraded to single-tenant mode due to a malformed `authorized_keys` comment (labeled by `reason`: `interior_whitespace`). Alerts on misconfigured multi-tenant keys silently falling back to single-tenant. |

> **Note on `rio_gateway_connections_total`:** Incremented on first SSH auth attempt (`result=new`), then again on auth outcome (`result=accepted` or `result=rejected`). TCP probes that close before sending SSH bytes (NLB/kubelet health checks) do not increment — russh's `new_client()` fires on TCP accept, so the counter is deferred to the first `auth_*` callback. A single successful connection still generates two increments; use `result=accepted` + `result=rejected` for success/failure rates.

### Scheduler Metrics

r[obs.metric.scheduler]
| Metric | Type | Description |
|--------|------|-------------|
| `rio_scheduler_builds_total` | Counter | Total builds at terminal state (labeled by `outcome`: `success`/`failure`/`cancelled`) |
| `rio_scheduler_builds_active` | Gauge | Currently active builds |
| `rio_scheduler_derivations_queued` | Gauge | Derivations waiting for assignment |
| `rio_scheduler_derivations_running` | Gauge | Derivations currently building |
| `rio_scheduler_assignment_latency_seconds` | Histogram | Time from ready to assigned |
| `rio_scheduler_build_duration_seconds` | Histogram | Total build duration |
| `rio_scheduler_cache_hits_total` | Counter | Derivations served from cache (labeled by `source`: `scheduler`=TOCTOU check, `existing`=pre-existing completed) |
| `rio_scheduler_cache_check_failures_total` | Counter | Scheduler cache check (store FindMissingPaths) failures. Alert if rate > 0 sustained: indicates store connectivity issue, every submission treated as 100% cache miss. |
| `rio_scheduler_queue_backpressure` | Counter | Backpressure activations (queue reached 80% capacity) |
| `rio_scheduler_workers_active` | Gauge | Fully-registered workers (stream + heartbeat) |
| `rio_scheduler_assignments_total` | Counter | Total derivation->worker assignments |
| `rio_scheduler_cleanup_dropped_total` | Counter | Terminal-build cleanup commands dropped due to channel backpressure. Alert if rate > 0 sustained: indicates memory leak under load. |
| `rio_scheduler_transition_rejected_total` | Counter | State-machine transition rejections in the actor (labeled by `to` target state). Alert if rate > 0: these are defense-in-depth guards that should never fire; any non-zero rate indicates a race or logic bug. |
| `rio_scheduler_log_lines_forwarded_total` | Counter | Log lines forwarded via `BuildEvent::Log` (worker → scheduler → actor → gateway broadcast). Direct signal that the log pipeline's internal plumbing is live. |
| `rio_scheduler_log_flush_total` | Counter | Successful S3 log flushes (labeled by `kind`: `final`/`periodic`). |
| `rio_scheduler_log_flush_failures_total` | Counter | Failed S3 log flushes (labeled by `phase`: `s3`/`pg`). Alert if rate > 0 sustained: build logs are being lost. |
| `rio_scheduler_log_flush_dropped_total` | Counter | Final-flush requests dropped due to flusher channel backpressure. Periodic tick will snapshot instead. |
| `rio_scheduler_log_forward_dropped_total` | Counter | Live log forwards dropped due to actor channel backpressure. Lines remain in the ring buffer (serveable via AdminService) but the gateway misses the live stream. Sustained non-zero → actor is saturated. |
| `rio_scheduler_critical_path_accuracy` | Histogram | Predicted vs. actual completion ratio (actual/estimated; 1.0 = perfect, >1.0 = underestimate) |
| `rio_scheduler_size_class_assignments_total` | Counter | Assignments per size class (labeled by class name) |
| `rio_scheduler_misclassifications_total` | Counter | Fires when `actual_duration > 2× assigned_cutoff`. Penalty trigger — also overwrites the EMA (`r[sched.classify.penalty-overwrite]`). |
| `rio_scheduler_ema_proactive_updates_total` | Counter | Fires when a mid-build `ProgressUpdate` cgroup `memory.peak` sample exceeds the current `ema_peak_memory_bytes`. Same penalty-overwrite semantics as `misclassifications_total` but BEFORE completion — next submit of that `(pname, system)` is right-sized without waiting for an OOM→retry cycle. |
| `rio_scheduler_class_drift_total` | Counter | Fires when `classify(actual) ≠ assigned_class` (labeled by `assigned_class`, `actual_class`). Cutoff-drift signal — decoupled from penalty logic. A build can trigger drift without penalty (actual barely over cutoff, under 2×). |
| `rio_scheduler_cutoff_seconds` | Gauge | Duration cutoff per class (labeled by class; initialized from config, re-emitted hourly after each rebalancer pass — see `r[sched.rebalancer.sita-e]`) |
| `rio_scheduler_class_queue_depth` | Gauge | Deferred derivations per target class (snapshot per dispatch pass) |
| `rio_scheduler_cache_check_circuit_open_total` | Counter | Circuit-breaker open transitions (store unreachable for 5 consecutive cache checks). Alert if rate > 0: scheduler falling back to rejecting SubmitBuild. |
| `rio_scheduler_prefetch_hints_sent_total` | Counter | PrefetchHint messages sent (one per assignment with paths to warm). Missing from a dispatch = leaf drv or bloom filter says worker already has everything. |
| `rio_scheduler_prefetch_paths_sent_total` | Counter | Total paths in sent PrefetchHints. Divide by `hints_sent` for avg paths-per-hint. High avg = workers cold (poor locality) or bloom stale. |
| `rio_scheduler_event_persist_dropped_total` | Counter | BuildEvents dropped from PG persister (channel backpressure). Broadcast still live; only mid-backlog reconnect loses it. Alert if rate > 0 sustained. |
| `rio_scheduler_lease_acquired_total` | Counter | Kubernetes Lease acquire transitions (standby → leader). *Internal — primary use is VM test observability.* |
| `rio_scheduler_lease_lost_total` | Counter | Kubernetes Lease loss transitions (leader → standby). *Internal — non-zero on a single-replica deployment is a bug.* |
| `rio_scheduler_recovery_total` | Counter | State recovery runs (on LeaderAcquired). Labeled by `outcome`: `success`/`failure`. |
| `rio_scheduler_recovery_duration_seconds` | Histogram | Time to reload non-terminal builds/derivations from PostgreSQL. |
| `rio_scheduler_backstop_timeouts_total` | Counter | Running derivations reset to Ready by the backstop timeout (running_since > max(est_duration×3, daemon_timeout+10m)). Non-zero indicates wedged workers. |
| `rio_scheduler_worker_disconnects_total` | Counter | BuildExecution stream closures (worker gone). Triggers reassignment. |
| `rio_scheduler_cancel_signals_total` | Counter | CancelSignal messages sent to workers (explicit CancelBuild, backstop timeout, or finalizer drain). |
| `rio_scheduler_estimator_refresh_total` | Counter | Build-history estimator refresh ticks (60s cadence). *Internal — VM test sync signal.* |
| `rio_scheduler_build_graph_edges` | Histogram | Edge count per `GetBuildGraph` response. Bounded by the induced subgraph over the node-cap (≤5000 nodes); a high p99 (>10k) means unusually dense DAGs. Suggested buckets: `[100, 500, 1000, 5000, 10000, 20000]`. |
| `rio_scheduler_class_load_fraction` | Gauge | Load fraction per size class (adaptive rebalancer input) |
| `rio_scheduler_ca_hash_compares_total` | Counter | CA cutoff-compare output-hash lookups against the content index on completion (labeled by `outcome`: `match`/`miss`). High match ratio → CA derivations rebuilding identical content. |
| `rio_scheduler_ca_cutoff_saves_total` | Counter | Derivations skipped via CA early-cutoff (Queued→Skipped transitions). Each increment is one build that did NOT run because a CA dep's output matched the content index. |
| `rio_scheduler_ca_cutoff_seconds_saved` | Counter | Sum of `est_duration` of skipped derivations. Lower-bound estimate of wall-clock saved (est_duration is the Estimator's EMA; a never-run derivation has the fallback, not actual). Divide by `saves_total` for avg-seconds-per-save. |
| `rio_scheduler_ca_cutoff_depth_cap_hits_total` | Counter | CA cutoff cascade walks that hit `MAX_CASCADE_DEPTH` (1000). Non-zero → cascades truncated; pathological DAG shape or cap too low. |

r[obs.metric.scheduler-leader-gate]
Scheduler state gauges (`_builds_active`, `_derivations_queued`, `_derivations_running`, `_workers_active`, `_class_queue_depth`) are published **only by the leader**. The standby's actor is warm (DAGs merge for fast takeover per `r[sched.lease.k8s-lease]`) but workers do not connect to it (leader-guarded gRPC per `r[sched.grpc.leader-guard]`), so its counts are stale or zero. With `replicas>1`, publishing from both would create duplicate Prometheus series with identical labels; a naked gauge query returns both, and stat-panel reducers pick one nondeterministically. Counters and histograms are unaffected --- the standby's dispatch loop no-ops, so its counters stay at zero naturally, and `sum(rate(...))` is the idiomatic query form anyway.

### Store Metrics

r[obs.metric.store]
| Metric | Type | Description |
|--------|------|-------------|
| `rio_store_put_path_total` | Counter | Total PutPath operations |
| `rio_store_put_path_duration_seconds` | Histogram | PutPath latency |
| `rio_store_integrity_failures_total` | Counter | GetPath content integrity check failures (bitrot/corruption) |
| `rio_store_chunks_total` | Gauge | Total chunks in storage (piggybacked on FindMissingChunks) |
| `rio_store_chunk_dedup_ratio` | Gauge | Per-upload dedup ratio (1.0 - missing/total after chunking) |
| `rio_store_s3_requests_total` | Counter | S3 API calls (labeled by operation) |
| `rio_store_chunk_cache_hits_total` | Counter | moka chunk cache hits (for cross-instance aggregation) |
| `rio_store_chunk_cache_misses_total` | Counter | moka chunk cache misses |
| `rio_store_hmac_rejected_total` | Counter | PutPath calls rejected by HMAC verifier (bad signature, expired, path not in `expected_outputs`). Alert if rate > 0: indicates misconfiguration or compromise attempt. |
| `rio_store_hmac_bypass_total` | Counter | PutPath calls that skipped HMAC verification via mTLS CN bypass (labeled by `cn`). Expected `cn="rio-gateway"` only. |
| `rio_store_gc_path_resurrected_total` | Counter | Paths skipped by GC sweep because a reference appeared between mark and sweep (sweep's per-path reference re-check caught it). |
| `rio_store_gc_chunk_resurrected_total` | Counter | S3 deletes skipped by the drain task because chunk refcount re-check found the chunk back in use (TOCTOU guard via `pending_s3_deletes.blake3_hash`). |
| `rio_store_gc_path_swept_total` | Counter | Paths deleted by GC sweep (`narinfo` DELETE + CASCADE). Monotonic over store lifetime; `rate()` ≈ GC throughput. Not incremented on dry-run. |
| `rio_store_gc_s3_key_enqueued_total` | Counter | S3 keys enqueued to `pending_s3_deletes` by GC sweep (chunks that hit refcount=0). Gap vs `rio_store_s3_deletes_pending` gauge decreasing = drain keeping up. |
| `rio_store_gc_chunk_orphan_swept_total` | Counter | Standalone chunks reaped by `sweep_orphan_chunks` after the grace-TTL expired (PutChunk at refcount=0, no subsequent PutPath claimed them). Nonzero indicates workers crashing mid-upload; sustained high suggests a client-side chunker bug. |
| `rio_store_s3_deletes_pending` | Gauge | Rows in `pending_s3_deletes` with `attempts < 10`. Normal operation: near-zero. |
| `rio_store_s3_deletes_stuck` | Gauge | Rows in `pending_s3_deletes` with `attempts >= 10` (max retries exhausted). Alert if > 0: manual investigation needed. |
| `rio_store_put_path_bytes_total` | Counter | Bytes accepted via PutPath (nar_size on success) |
| `rio_store_get_path_bytes_total` | Counter | Bytes served via GetPath (nar_size on stream start) |

### Worker Metrics

r[obs.metric.worker]
| Metric | Type | Description |
|--------|------|-------------|
| `rio_worker_builds_total` | Counter | Total builds executed (labeled by `outcome`: `success`/`failure`/`cancelled`/`timed_out`/`log_limit`/`infra_failure`) |
| `rio_worker_builds_active` | Gauge | Currently running builds on this worker |
| `rio_worker_uploads_total` | Counter | Output uploads (labeled by `status`) |
| `rio_worker_build_duration_seconds` | Histogram | Per-derivation build time |
| `rio_worker_fuse_cache_size_bytes` | Gauge | FUSE SSD cache usage |
| `rio_worker_fuse_cache_hits_total` | Counter | FUSE cache hits |
| `rio_worker_fuse_cache_misses_total` | Counter | FUSE cache misses |
| `rio_worker_fuse_fetch_duration_seconds` | Histogram | Store path fetch latency |
| `rio_worker_fuse_fallback_reads_total` | Counter | Successful userspace `read()` callbacks. Near-zero when passthrough is on (kernel handles reads directly); nonzero when `fuse_passthrough=false` or passthrough failed for specific files. |
| `rio_worker_fuse_index_divergence_total` | Counter | FUSE cache index/disk divergences self-healed. Nonzero = something rm'd cache files under the SQLite index (debugging, interrupted eviction). Investigate if sustained. |
| `rio_worker_overlay_teardown_failures_total` | Counter | Overlay unmount failures (leaked mount). Alert if rate > 0: indicates resource leak on worker. |
| `rio_worker_prefetch_total` | Counter | PrefetchHint outcomes (labeled by `result`: `fetched`/`already_cached`/`already_in_flight`/`error`/`malformed`/`panic`). Sustained high `already_cached` = scheduler bloom filter stale from heartbeat lag. (Note: SATURATION produces the OPPOSITE signal — see `rio_worker_bloom_fill_ratio`.) |
| `rio_worker_upload_bytes_total` | Counter | Bytes uploaded to store via PutPath (nar_size on success) |
| `rio_worker_upload_skipped_idempotent_total` | Counter | Outputs skipped before upload because `FindMissingPaths` reports them already-present in the store. Idempotency short-circuit — nonzero is healthy (repeat builds of cached paths). |
| `rio_worker_fuse_circuit_open` | Gauge | FUSE circuit-breaker open state (1 = open/tripped, 0 = closed/healthy). Set to 1 when store fetch error rate exceeds threshold; FUSE ops return EIO instead of blocking. Reset to 0 on successful probe. Alert if sustained 1. |
| `rio_worker_upload_references_count` | Histogram | Reference count per output upload (`references.len()` after NAR scan). Distribution of dependency fan-out. Zero-heavy = mostly leaves; high p99 = wide transitive closures. Buckets: `[1, 5, 10, 25, 50, 100, 250, 500]`. |
| `rio_worker_fuse_fetch_bytes_total` | Counter | Bytes fetched from store via FUSE cache misses |
| `rio_worker_cpu_fraction` | Gauge | Worker cgroup CPU utilization: delta `cpu.stat usage_usec` / wall-clock µs. 1.0 = one core fully used; >1.0 on multi-core. Directly comparable to cgroup `cpu.max` limits. |
| `rio_worker_memory_fraction` | Gauge | Worker cgroup memory utilization: `memory.current` / `memory.max`. 0.0 if `memory.max` is `"max"` (unbounded). |
| `rio_worker_stale_assignments_rejected_total` | Counter | WorkAssignments rejected by the generation fence (assignment.generation < latest heartbeat-observed generation). Nonzero only during leader failover split-brain; sustained nonzero = deposed scheduler replica still dispatching. |
| `rio_worker_bloom_fill_ratio` | Gauge | Fraction of bloom filter bits set. Alert ≥ 0.5: at k=7, FPR climbs past 1% nonlinearly. Saturation is SILENT — `prefetch_total{result="already_cached"}` DECREASES under saturation (scheduler skips hints it thinks worker has), indistinguishable from healthy locality. Long-lived STS workers churn past `bloom_expected_items` via eviction; the filter never shrinks. Fix: bump `worker.toml bloom_expected_items` or restart the pod. |

> **Note on ratio metrics:** For aggregatable cache metrics, use counter pairs (e.g., `rio_store_chunk_cache_hits_total` + `rio_store_chunk_cache_misses_total`) and compute ratios at query time with PromQL's `rate()`. Pre-computed gauge ratios lose meaning when averaged across instances. Exception: `rio_store_chunk_dedup_ratio` is a per-upload event gauge (last-written-wins, not averaged) — useful for eyeballing recent PutPath dedup effectiveness but NOT for cross-instance aggregation.

r[obs.metric.bloom-fill-ratio]
The worker emits `rio_worker_bloom_fill_ratio` (gauge, 0.0–1.0) every heartbeat
tick (10s). Alert threshold 0.5 — at k=7 hash functions, fill ≥ 0.5 means FPR
has climbed past the configured 1% nonlinearly. Saturation causes scheduler
locality scoring to silently degrade (`count_missing()` undercounts →
`W_LOCALITY` term → 0) with NO direct symptom in existing metrics. The filter
never shrinks (evicted paths stay as stale positives); only restart clears it.
Operators set `spec.bloomExpectedItems` on the WorkerPool (injects
`RIO_BLOOM_EXPECTED_ITEMS`); the pod restart that applies the CRD edit also
resets the filter.

r[obs.metric.transfer-volume]

Transfer-volume byte counters (`*_bytes_total`) are emitted at each hop: gateway (`rio_gateway_bytes_total{direction}`), store (`rio_store_{put,get}_path_bytes_total`), worker (`rio_worker_{upload,fuse_fetch}_bytes_total`). Summing these across the topology gives a full picture of data movement — e.g., `rate(rio_worker_fuse_fetch_bytes_total[5m])` vs `rate(rio_worker_upload_bytes_total[5m])` shows whether a worker is input-bound or output-bound.

r[obs.metric.worker-util]

Worker utilization gauges (`rio_worker_{cpu,memory}_fraction`) are polled from the worker's parent cgroup every 10s by `utilization_reporter_loop`. The same loop publishes a `ResourceSnapshot` that the heartbeat reads for `HeartbeatRequest.resources` — one sampling site means Prometheus and `ListWorkers` always agree. These capture the whole worker tree (rio-worker + per-build sub-cgroups + all subprocesses). CPU fraction >1.0 on multi-core is expected under full load. Memory fraction stays 0.0 if `memory.max` is unbounded — only meaningful when the pod has a memory limit configured.

### Controller Metrics

r[obs.metric.controller]
| Metric | Type | Description |
|--------|------|-------------|
| `rio_controller_reconcile_duration_seconds` | Histogram | Reconcile loop latency (labeled by reconciler) |
| `rio_controller_reconcile_errors_total` | Counter | Reconcile errors (labeled by reconciler) |
| `rio_controller_workerpool_replicas` | Gauge | WorkerPool replica count (labeled desired vs actual) |
| `rio_controller_scaling_decisions_total` | Counter | Scaling decisions (labeled by direction: up/down) |
| `rio_controller_gc_runs_total` | Counter | GC cron runs. `result=success\|connect_failure\|rpc_failure`. `connect_failure` = store unreachable (pod down, stale IP); `rpc_failure` = TriggerGC error or progress stream aborted. |

### Histogram Buckets

`metrics-exporter-prometheus` defaults to `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` — tuned for HTTP request latencies. Build durations span seconds to hours, so `rio-common::observability::init_metrics` installs per-metric overrides via `PrometheusBuilder::set_buckets_for_metric`:

| Metric(s) | Buckets (seconds unless noted) |
|---|---|
| `rio_scheduler_build_duration_seconds`, `rio_worker_build_duration_seconds` | `[1, 5, 15, 30, 60, 120, 300, 600, 1800, 3600, 7200]` |
| `rio_scheduler_critical_path_accuracy` | `[0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 5.0]` (ratio: actual/estimated; 1.0 = perfect) |
| `rio_controller_reconcile_duration_seconds` | `[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` |
| `rio_scheduler_assignment_latency_seconds` | `[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0]` |
| `rio_scheduler_build_graph_edges` | `[100, 500, 1000, 5000, 10000, 20000]` (count) |
| `rio_worker_upload_references_count` | `[1, 5, 10, 25, 50, 100, 250, 500]` (count) |

Histograms not listed here (e.g., `rio_gateway_opcode_duration_seconds`, `rio_store_put_path_duration_seconds`, `rio_worker_fuse_fetch_duration_seconds`) use the default buckets — those are genuinely sub-second request latencies.

## Graceful Drain

r[common.drain.not-serving-before-exit]

On SIGTERM, each long-lived server MUST call `set_not_serving()` on its tonic-health reporter BEFORE `serve_with_shutdown` returns, and MUST sleep for at least `readinessProbe.periodSeconds + 1` seconds between the two. This gives kubelet one full probe cycle to observe NOT_SERVING and the endpoint-controller time to remove the pod from the Service's Endpoint slice, preventing new connections from being routed to a process that is tearing down.

For the scheduler specifically, whose readinessProbe is `tcpSocket` (not gRPC health), the drain sleep signals BalancedChannel clients via their `DEFAULT_PROBE_INTERVAL` (3s) loop — K8s endpoint routing is unaffected.

The drain grace period is configurable via `drain_grace_secs` (default 6; `RIO_DRAIN_GRACE_SECS=0` disables drain for tests).

r[common.task.periodic-biased]
Periodic background tasks (interval-driven loops with a shutdown arm) MUST use `biased;` ordering in their `tokio::select!` so shutdown cancellation wins deterministically over a ready interval tick. Without `biased;`, tokio randomizes branch selection for fairness; a task may execute one more tick-body after cancellation fires, which delays graceful shutdown by up to one interval (seconds to hours depending on the task). The `rio_common::task::spawn_periodic` helper encapsulates this pattern. Stateful loops that cannot use the helper MUST inline `biased;` at their `select!`.

## Distributed Tracing

rio-build uses OpenTelemetry for distributed tracing with trace context propagation via gRPC metadata.

### Trace Structure

A typical build trace spans multiple components:

```
Build (gateway)
├── SubmitBuild (gateway → scheduler)
│   ├── DAG Merge (scheduler)
│   ├── Cache Check (scheduler → store)
│   └── Schedule (scheduler)
│       ├── Assign derivation-A (scheduler → worker-0)
│       │   ├── Fetch inputs (worker-0 → store)
│       │   ├── Build (worker-0, nix sandbox)
│       │   └── Upload output (worker-0 → store)
│       └── Assign derivation-B (scheduler → worker-1)
│           ├── Fetch inputs (worker-1 → store)
│           ├── Build (worker-1, nix sandbox)
│           └── Upload output (worker-1 → store)
└── Return result (gateway → client)
```

### Configuration

OTel config is read from environment variables (NOT figment) because `init_tracing()` runs before config parsing and must not depend on any crate's config layout.

| Env var | Description |
|---------|-------------|
| `RIO_OTEL_ENDPOINT` | OTLP gRPC collector endpoint (e.g., `http://otel-collector:4317`). Unset = OTel disabled entirely (zero overhead). |
| `RIO_OTEL_SAMPLE_RATE` | Trace sampling rate 0.0–1.0 (default: 1.0). Clamped. |
| `RIO_LOG_FORMAT` | `json` or `pretty` (default: `json`). |

The OTel `service.name` resource attribute is set automatically per component (gateway, scheduler, store, worker, controller) by `init_tracing()`.

### Trace Propagation

r[obs.trace.w3c-traceparent]

Trace context is propagated via gRPC metadata using the W3C `traceparent` header format.

r[sched.trace.assignment-traceparent]

ssh-ng has no gRPC metadata channel, so the scheduler→worker hop cannot use `inject_current`/`link_parent`. Span context also does not cross the scheduler's mpsc actor channel — calling `current_traceparent()` at dispatch time would capture an orphan actor span. Instead, the `SubmitBuild` gRPC handler captures `current_traceparent()` **after** `link_parent()` (inside the scheduler handler span — which has its own trace_id, LINKED to the gateway trace), and carries it as plain data: `MergeDagRequest.traceparent` → `DerivationState.traceparent` → `WorkAssignment.traceparent` at dispatch. The worker extracts it via `span_from_traceparent()` and wraps the spawned build-executor future in `.instrument(span)`. The span is created then `set_parent()` is called **before it is entered** — the tracing-opentelemetry bridge allocates the OTel span lazily on first enter, at which point the stored parent context is available for the OTel SpanBuilder. This produces **parent-child** (same trace_id): the worker span's `parentSpanId` matches a scheduler `spanId`; Tempo shows scheduler→worker as one trace. **First-submitter-wins on dedup:** when two builds merge the same derivation, the existing state's traceparent is preserved unless it is empty (recovery/poison-reset set `""`), in which case the first live submitter upgrades it. Traceparent is not persisted to PG — recovered derivations dispatched before any re-submit get a fresh worker root span. Empty traceparent → fresh root span. This closes the SSH-boundary tracing gap — Tempo shows scheduler→worker as one trace (via the `WorkAssignment.traceparent` data-carry + `span_from_traceparent`), linked to the gateway trace upstream and linked to store traces downstream (the worker→store hop uses `inject_current` + store-side `link_parent`, the same `#[instrument]`-then-`set_parent` pattern proven to produce a LINK at the gateway→scheduler boundary). Injection and extraction are **manual**, not tonic interceptors: `rio_proto::interceptor::inject_current()` copies the current span's context into outgoing request metadata (client side), and `rio_proto::interceptor::link_parent()` adds an OTel span **link** to the incoming traceparent (server side, first line of each handler) — the `#[instrument]` span was already created and entered with its own trace_id before `set_parent()` runs, so the result is a link, not a parent-child edge. The explicit manual call makes propagation points greppable and avoids tonic's `Interceptor` trait (which changes `connect_*` return types and doesn't compose with server-side `#[instrument]`). The W3C `TraceContextPropagator` is registered globally in `init_tracing()` regardless of whether `RIO_OTEL_ENDPOINT` is set — propagation works even when spans aren't exported.

r[obs.trace.scheduler-id-in-metadata]

The scheduler sets `x-rio-trace-id` in `SubmitBuild` response metadata to its handler span's trace_id (captured AFTER `link_parent()`). The gateway emits THIS id in `STDERR_NEXT` (`rio trace_id: <32-hex>`), not its own. Rationale: `link_parent()` + `#[instrument]` produces an orphan — the scheduler handler span has its own trace_id, LINKED to the gateway trace but not parented. The gateway's trace contains only gateway spans; the scheduler's trace is the one extended through worker via the `WorkAssignment.traceparent` data-carry. Operators grepping the emitted id land in the trace that actually spans the full scheduler→worker chain. Header absent (legacy scheduler / no OTel configured) → gateway falls back to its own `current_trace_id_hex()`.

## SLOs, SLIs, and Alerting

### Service Level Indicators (SLIs)

| SLI | Source Metric(s) |
|-----|------------------|
| Gateway connection success rate | `rio_gateway_connections_total` minus connection errors / total |
| Scheduler build completion rate | `rio_scheduler_builds_total` outcome=success / total |
| Store PutPath success rate | `rio_store_put_path_total` minus errors / total |
| Worker build success rate | `rio_worker_builds_total` outcome=success / total |

### Service Level Objectives (SLOs)

| SLO | Target |
|-----|--------|
| Non-PermanentFailure builds complete within 2x estimated duration | 99.9% |
| PutPath success on first attempt | 99.99% |
| Cache-hit latency (p99) | < 1s |

### Alerting

- **Error budget burn rate:** Alert when the error budget consumption rate exceeds 14.4x the allowed rate over 1h (fast burn) or 6x over 6h (slow burn), following the multi-window multi-burn-rate approach.
- **Saturation alerts:** PostgreSQL connection pool utilization > 80%, S3 rate limiting (429 responses), worker queue depth exceeding 2x worker count.
- **Absence alerts:** No worker heartbeat received for > ~50-60s (the scheduler's effective deregistration threshold: 30s staleness + 3-tick confirmation). Indicates a worker has silently died or lost network connectivity.
- **Bloom saturation:** `rio_worker_bloom_fill_ratio >= 0.5` on any worker. FUSE cache bloom filter has crossed the FPR-degradation threshold — scheduler locality scoring is silently undercounting. Remediation: set `spec.bloomExpectedItems` on the WorkerPool (the pod restart that applies the spec edit also resets the filter).

## Structured Logging

r[obs.log.required-fields]
All components emit structured JSON logs via `tracing-subscriber` with the following required fields per log line:

| Field | Type | Description |
|-------|------|-------------|
| `timestamp` | RFC 3339 | Event time |
| `level` | string | Log level (TRACE, DEBUG, INFO, WARN, ERROR) |
| `component` | string | Emitting component (gateway, scheduler, store, worker, controller) |
| `build_id` | string | Build request ID (if applicable) |
| `derivation_hash` | string | Derivation hash (if applicable) |
| `worker_id` | string | Worker instance ID (worker component only) |
| `message` | string | Human-readable log message |

Conditionally present:

| Field | Type | When present |
|-------|------|--------------|
| `trace_id` | string | Only when `RIO_OTEL_ENDPOINT` is set AND the log is emitted within an active span. The default JSON fmt layer does NOT include trace/span IDs — they come from the OTel layer's span context. |
| `span_id` | string | Same condition as `trace_id`. |
| `tenant_id` | string | Tenant identifier. Gateway records `tenant` (name) on the session span (`session.rs`). Scheduler records `tenant_id` (UUID) on the `SubmitBuild` span after `resolve_tenant` succeeds. Persisted to `builds.tenant_id`. |

Optional fields may be added per component as `tracing` span fields. All fields use snake_case. Missing context fields (e.g., `build_id` outside a build context) are omitted rather than set to empty strings.

## Dashboard Data Sources

The [rio-dashboard](./components/dashboard.md) consumes data from two sources:

| Data | Source | Protocol |
|------|--------|----------|
| Builds, workers, logs | `AdminService` | gRPC-Web |
| Metrics, graphs | Prometheus | HTTP (direct or via Grafana) |

The dashboard does NOT query PostgreSQL or S3 directly.
