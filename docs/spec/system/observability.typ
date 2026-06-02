#import "/lib/rio.typ": *
#show: rio.with(domains: ("obs", "common", "sched"))


rio-build provides three pillars of observability: logs, metrics, and traces.

= Build Log Storage

Build logs are stored durably for post-build analysis and the dashboard log
viewer. The data plane is owned by rio-store: builders stream log batches to
`LogService.AppendLog`, the store cuts immutable zstd-compressed chunks to S3
with a PostgreSQL line-range manifest, and every reader --- the gateway's
live tail, the dashboard, the CLI --- reads back through `LogService.TailLog`.
The scheduler never receives a log line: the `BuildExecution` stream carries
control messages only, and the scheduler's only log-adjacent responsibility
is the `drv_executions` lifecycle row (one per execution, stamped with the
terminal status and the builder-reported final line count). The normative
requirements on the store's ingest and read paths live in
#xref(<store-log-service>, [the store component spec]); this section covers
the cross-cutting properties: how logs are keyed, batched, and surfaced.

== Storage Format

Build logs are stored in S3 as immutable, append-only chunks:

```
logs/{drv_hash}/{exec_id}/{session_id}/{chunk_seq}.zst
```

`exec_id` is a per-execution UUIDv7 minted by the scheduler at dispatch
(`assign_to_worker`) and recorded in the `drv_executions` lifecycle row.
Time-sortability means "latest execution for a derivation" is a single index
seek (`ORDER BY exec_id DESC LIMIT 1`). `session_id` is minted per
`AppendLog` stream, so a builder reconnect or a store-replica failover opens
a new session whose chunk keys can never collide with a predecessor's. Each
committed chunk has a `drv_log_chunks` manifest row recording its true
worker-line range (`first_line`, `line_count`); reads are driven entirely by
the manifest, fetch one chunk at a time, and deduplicate overlapping session
ranges by line number.

#r("obs.log.exec-keyed+2")[
  Build logs MUST be keyed by per-execution UUIDv7: one logical log per
  execution regardless of how many builds are interested in the derivation,
  materialized as that execution's `drv_log_chunks` manifest rows and the
  immutable chunk objects they reference.
]

A derivation is built once even if N builds want it (`sched.merge.dedup`), so
keying by `(build_id, drv_hash)` would store N copies of the same output.
`build_derivations.exec_id` carries the build↔execution correlation for the
dashboard; the gateway's live tail subscribes per execution and demultiplexes
to its watching clients.

The storage format provides by construction the properties the previous
scheduler-side design (a per-derivation ring buffer periodically re-uploading
a growing mutable `.partial` blob) needed several hundred lines of
failure-mode protocol to approximate: chunks are immutable and session-keyed
(#rref("store.log.chunk-immutable"), #rref("store.log.session-keyed")), so no
writer can overwrite or regress coverage another writer already stored, and a
scheduler failover, a store-replica failover, or a builder reconnect requires
no reconciliation --- at worst it stores one chunk's worth of duplicate lines
that the read path deduplicates. Completeness is computed from the manifest
at read time (#rref("store.log.completeness-gate")), never latched in a row.

#r("obs.log.incomplete-surfaced+2")[
  A log read whose final chunk carries `is_complete = false` MUST be surfaced
  to the user as incomplete: the CLI prints a trailing notice to stderr and
  the dashboard log viewer renders an "incomplete" banner. The lines
  themselves are served as-is --- the flag is display metadata, not a serving
  gate.
]

An incomplete log (the build is still running, the execution was cancelled,
the builder abandoned its final drain after a long store outage, or the last
chunk is still in flight) serves whatever the manifest covers --- strictly
more useful than `NotFound`, but the missing tail is usually the most
interesting part of the log: the build error. Without an explicit indicator
the user reads a truncated log as the whole thing.

#r("obs.log.worker-header")[
  The worker MUST write `rio: exec`, `rio: builder`, `rio: started` lines as
  the first lines of every build log, and a `rio: exec` + `rio: result` footer
  after the build process exits. These lines are display-only and consumers
  MUST NOT parse them for authoritative state.
]

The header/footer are written into the same untrusted byte stream as build
output --- arbitrary build code can emit its own `rio: result ok` lines. The
system's source of truth for `exec_id`, outcome, and sizing is
`drv_executions` and `assignments`, not the log text. The `grep '^rio:'`
extraction is a convenience for humans (the post-failure log tail Nix prints,
the dashboard log viewer), not a protocol. On scheduler-initiated
cancellation the footer may be absent (the worker is torn down before it can
emit one) or may disagree with the recorded outcome; `drv_executions.status`
carries the authoritative verdict. The header and footer consume worker line
numbers like any other output, so `CompletionReport.final_line_count` --- the
post-footer high-water mark the completeness predicate checks the manifest
against --- includes them. Pod and node identity are deliberately excluded
--- the "cluster is one machine" abstraction holds at the log level too.

== Log Lifecycle

#r("obs.log.batch-64-100ms")[
  Log lines are batched (up to 64 lines or 100ms, whichever first) in
  `BuildLogBatch` messages.
]

#figure(
  chronos.diagram({
    import chronos: *
    _par("Executor")
    _par("rio-store")
    _par("S3")
    _seq(
      "Executor",
      "rio-store",
      comment: [`AppendLog` (header, then batches ≤64 lines or 100ms)],
    )
    _note(
      "over",
      [Buffer in memory\ (per-stream ingest buffer)],
      pos: "rio-store",
    )
    _seq("rio-store", "S3", comment: [Cut chunk (zstd) + manifest row])
    _seq("rio-store", "Executor", comment: [Ack `durable_through_line`])
    _note("over", [Trim retransmit buffer], pos: "Executor")
  }),
  caption: [Build-log lifecycle.],
)

+ The builder opens one authenticated `AppendLog` stream per execution
  (#rref("store.log.append-auth")), tees every batch into an in-memory
  retransmit buffer, and sends it on the stream.
+ The store validates each batch (#rref("store.log.ingest-bounds")), buffers
  it, and fans it out to any live-tail subscribers.
+ The store cuts a chunk when the buffer reaches a size threshold, on a
  periodic tick, or at stream end; the chunk object is written before its
  manifest row, and the ack carries the highest line number now durable.
+ The builder trims its retransmit buffer on each ack. On any stream failure
  it reconnects and replays the un-acked tail into a new session; the
  manifest's line-range union and the read path's deduplication absorb the
  overlap. At build completion the upload task detaches from the build (the
  `CompletionReport` is never delayed or failed by log persistence) and keeps
  draining until everything is acked or a bounded deadline expires.

#info(title: [Loss budget])[
  The component holding the only copy of a line determines what a crash
  loses. A scheduler failover loses nothing (the scheduler holds no log
  data). A store-replica failover loses nothing (the builder replays its
  un-acked retransmit buffer to another replica). A builder crash loses only
  the lines emitted but not yet streamed (~100ms). A store outage spanning a
  build's completion loses the un-acked tail only if it outlasts the
  builder's post-completion drain deadline. The previous design's 30-second
  periodic-flush loss window on scheduler failover no longer exists.
]

== Log Serving

#table(
  columns: (auto, 1fr),
  align: (left, left),
  table.header([Build State], [Log Source]),
  [Active (building)],
  [Manifest-selected chunks, then the ingesting replica's live buffer and
    subscription stream (`TailLog` with `follow`)],

  [Completed], [Manifest-selected chunks from S3, deduplicated by line number],
  [Failed], [Same --- the failed build's tail is the highest-value content],
)

The gateway opens a `TailLog` subscription per building derivation of a
watched build and relays the lines to the `nix build -L` client; it owns
re-subscription when a stream ends before the derivation is terminal
(#rref("store.log.tail-reconnect")). The dashboard and the CLI issue one-shot
(non-follow) reads.

= Metrics

Each component exposes a Prometheus-compatible `/metrics` endpoint via
`metrics-exporter-prometheus`.

== Gateway Metrics

#r("obs.metric.gateway")[
  rio-gateway MUST expose the metrics in
  #xref(<tbl-metrics-gateway>, [the gateway metric reference]). All metrics
  MUST follow the `rio_gateway_*` naming prefix.
]

== Scheduler Metrics

#r("obs.metric.scheduler")[
  rio-scheduler MUST expose the metrics in
  #xref(<tbl-metrics-scheduler>, [the scheduler metric reference]). All
  metrics MUST follow the `rio_scheduler_*` naming prefix.
]

#r("obs.metric.scheduler-leader-gate+4")[
  Scheduler state gauges (`_builds_active`, `_derivations_queued`,
  `_derivations_running`, `_open_attempts`) are
  published *only by the leader*. The standby's actor is warm (DAGs merge
  for fast takeover per #rref("sched.lease.k8s-lease")), so its counts are
  stale or zero; with `replicas>1`, publishing from both would create
  duplicate Prometheus series with identical labels, and stat-panel
  reducers pick one nondeterministically. Counters and histograms are
  unaffected --- the standby's handlers no-op, so its counters stay at zero
  naturally, and `sum(rate(...))` is the idiomatic query form anyway.
]
The stream-era `_workers_active` gauge (and its connection-state exception
to the leader gate) is retired: it was deprecated and pinned to zero when
the stream session was deleted, kept only as the deletion-gate recording
series, and removed with the proto sweep once that role ended.
`_open_attempts` is the busy-fleet gauge.

#r("obs.metric.scheduler-substituting")[
  The scheduler MUST publish
  #(refs.metric)("rio_scheduler_substituting_derivations") (gauge): the count
  of derivations carrying an unresolved, unclaimed materialization job ---
  the same quantity `ClusterStatus.substituting_derivations` reports
  (#rref("sched.admin.snapshot-substituting")) --- set by the leader at every
  housekeeping tick from the freshly computed cluster snapshot, and zeroed
  once on leadership loss.
]
This gauge is the *leading* rio-store autoscaling signal
(#rref("infra.store.autoscaling")): the backlog is known at merge time,
minutes before the store feels the ingest load, and the underlying jobs are
durable PG rows --- the count survives leader failover instead of resetting
with leader memory. It follows the leader-gate posture above (a state gauge,
published only by the serving leader); publishing from the snapshot
computation makes the scrape surface and the admin RPC agree by
construction.

#r("obs.metric.materialization-stalled")[
  When materialization dispatch is enabled, the scheduler MUST publish
  #(refs.metric)("rio_scheduler_materialization_stalled") (gauge): the count
  of parked materialization jobs, set from ground truth at every
  housekeeping tick by the leader, after the parked-job re-evaluation pass
  has run. The gauge MUST exclude jobs the re-evaluation resolved (a parked
  job whose node's durable closure evidence reads Vouched or Pending is
  resolved from-source in the same pass, so it can never be reported as
  stalled), and MUST NOT be published while materialization dispatch is
  disabled.
]
The gauge follows the leader-gate posture above (a state gauge, published
only by the serving leader). Its operational meaning: every counted job has
*no from-source fallback* (Broken closure evidence --- childless or holed),
so a sustained nonzero value isolates "a dead or misconfigured upstream is
the only thing standing between these builds and progress" --- the design
§2.5 park-visibility obligation (PD-20). The corresponding alert lives in
the helm chart's PrometheusRule.

== Store Metrics

#r("obs.metric.store")[
  rio-store MUST expose the metrics in
  #xref(<tbl-metrics-store>, [the store metric reference]). All metrics MUST
  follow the `rio_store_*` naming prefix.
]

#r("obs.metric.store-pg-pool+2")[
  #(refs.metric)("rio_store_pg_pool_utilization") MUST be self-published by
  the store on a 30 s in-process tick (`(size − num_idle) /
  max_connections`), independent of any `GetLoad` traffic; the `GetLoad`
  handler additionally publishes on call so Prometheus mirrors the values a
  polling controller acted on.
]
PG pool exhaustion is a cliff (I-105: acquire times → 11s → builder FUSE
blocks → circuit trip → all builds fail), not a ramp --- this gauge is the
saturation watch on the store dashboard and `xtask k8s status`. It is no
longer the store's scaler-calibration signal: the store ComponentScaler CR
is retired and the KEDA ScaledObject (#rref("infra.store.autoscaling"))
scales on the backlog/builders/CPU triggers; before the self-publication
tick the gauge was refreshed only as a `GetLoad` side-effect and would have
frozen with the CR gone.

== Builder Metrics

#r("obs.metric.builder")[
  rio-builder MUST expose the metrics in
  #xref(<tbl-metrics-builder>, [the builder metric reference]). All metrics
  MUST follow the `rio_builder_*` naming prefix.
]

#r("obs.metric.input-materialization-failures")[
  #(refs.metric)("rio_builder_input_materialization_failures_total")
  (counter): incremented each time a daemon `MiscFailure` is reclassified as
  `InfrastructureFailure` under #rref("builder.result.input-enoent-is-infra").
  Sustained nonzero rate indicates `JIT_MIN_THROUGHPUT_BPS` is set above
  actual store→builder throughput.
]

#r("obs.metric.transfer-volume")[
  Transfer-volume byte counters (`*_bytes_total`) are emitted at each hop:
  gateway (#(refs.metric)("rio_gateway_bytes_total")`{direction}`), store
  (`rio_store_{put,get}_path_bytes_total`), executor
  (`rio_builder_{upload,fuse_fetch}_bytes_total`). Summing these across the
  topology gives a full picture of data movement --- e.g.,
  `rate(rio_builder_fuse_fetch_bytes_total[5m])` vs
  `rate(rio_builder_upload_bytes_total[5m])` shows whether an executor is
  input-bound or output-bound.
]

#r("obs.metric.builder-util")[
  Builder utilization gauges (`rio_builder_{cpu,memory}_fraction`) are polled
  from the builder's parent cgroup every 10s by `utilization_reporter_loop`.
  The same loop publishes a `ResourceSnapshot` that the heartbeat reads for
  `HeartbeatRequest.resources` --- one sampling site means Prometheus and
  `ListExecutors` always agree. These capture the whole builder tree
  (rio-builder + per-build sub-cgroups + all subprocesses). CPU fraction >1.0
  on multi-core is expected under full load. Memory fraction stays 0.0 if
  `memory.max` is unbounded --- only meaningful when the pod has a memory
  limit configured.
]

== Controller Metrics

#r("obs.metric.consolidate-threshold")[
  #(refs.metric)("rio_controller_nodeclaim_consolidate_threshold_seconds")
  reports the NA-model idle reap threshold per cell. For cells where intents
  pack ≥2 per node (`E[c_fit] ≤ cores/2`, the §13b MostAllocated default for
  builders), the policy floor is a hard bound the model cannot exceed
  regardless of arrival rate; the model NA-extends past the floor only for
  cells packing \~1 intent per node. See `consolidate_after()`.
]

#r("obs.metric.controller")[
  rio-controller MUST expose the metrics in
  #xref(<tbl-metrics-controller>, [the controller metric reference]). All
  metrics MUST follow the `rio_controller_*` naming prefix.
]

== Histogram Buckets

`metrics-exporter-prometheus` defaults to
`[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]` --- tuned
for HTTP request latencies. Build durations span seconds to hours, so
`rio-common::observability::init_metrics` installs per-metric overrides via
`PrometheusBuilder::set_buckets_for_metric`:

#table(
  columns: (1fr, 1fr),
  align: (left, left),
  table.header([Metric(s)], [Buckets (seconds unless noted)]),
  [#(refs.metric)("rio_scheduler_build_duration_seconds"),
    #(refs.metric)("rio_builder_build_duration_seconds")],
  [`[1, 5, 15, 30, 60, 120, 300, 600, 1800, 3600, 7200]`],

  (refs.metric)("rio_scheduler_critical_path_accuracy"),
  [`[0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 5.0]` (ratio:
    actual/estimated; 1.0 = perfect)],

  (refs.metric)("rio_controller_reconcile_duration_seconds"),
  [`[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]`],

  (refs.metric)("rio_controller_nodeclaim_tick_duration_seconds"),
  [`[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]`],

  (refs.metric)("rio_scheduler_build_graph_edges"),
  [`[100, 500, 1000, 5000, 10000, 20000]` (count)],

  (refs.metric)("rio_builder_upload_references_count"),
  [`[1, 5, 10, 25, 50, 100, 250, 500]` (count)],

  [#(refs.metric)("rio_builder_fuse_fetch_duration_seconds"),
    #(refs.metric)("rio_store_substitute_duration_seconds"),
    #(refs.metric)("rio_store_check_available_duration_seconds")],
  [`[0.01, 0.05, 0.1, 0.5, 1, 2.5, 5, 10, 30, 60, 120]` (@nar fetch + drain;
    GB-scale paths via I-212 JIT span 60-127s)],
)

Histograms not listed here (e.g.,
#(refs.metric)("rio_gateway_opcode_duration_seconds"),
#(refs.metric)("rio_store_put_path_duration_seconds")) use the default
buckets --- those are genuinely sub-second request latencies.

= Graceful Drain

#r("common.drain.not-serving-before-exit")[
  On SIGTERM, each long-lived server MUST call `set_not_serving()` on its
  tonic-health reporter BEFORE `serve_with_shutdown` returns, and MUST sleep
  for at least `readinessProbe.periodSeconds + 1` seconds between the two.
  This gives kubelet one full probe cycle to observe NOT_SERVING and the
  endpoint-controller time to remove the pod from the Service's Endpoint
  slice, preventing new connections from being routed to a process that is
  tearing down.
]

For the scheduler specifically, whose readinessProbe is `tcpSocket` (not gRPC
health), the drain sleep signals BalancedChannel clients via their
`DEFAULT_PROBE_INTERVAL` (3s) loop --- K8s endpoint routing is unaffected.

The drain grace period is configurable via `drain_grace_secs` (default 6;
`RIO_DRAIN_GRACE_SECS=0` disables drain for tests).

#r("common.task.periodic-biased")[
  Periodic background tasks (interval-driven loops with a shutdown arm) MUST
  use `biased;` ordering in their `tokio::select!` so shutdown cancellation
  wins deterministically over a ready interval tick. Without `biased;`, tokio
  randomizes branch selection for fairness; a task may execute one more
  tick-body after cancellation fires, which delays graceful shutdown by up to
  one interval (seconds to hours depending on the task). The
  `rio_common::task::spawn_periodic` helper encapsulates this pattern.
  Stateful loops that cannot use the helper MUST inline `biased;` at their
  `select!`.
]

= Distributed Tracing

rio-build uses OpenTelemetry for distributed tracing with trace context
propagation via gRPC metadata.

== Trace Structure

A typical build trace spans multiple components:

```
Build (gateway)
├── SubmitBuild (gateway → scheduler)
│   ├── DAG Merge (scheduler)
│   ├── Cache Check (scheduler → store)
│   └── Schedule (scheduler)
│       ├── Assign derivation-A (scheduler → executor-0)
│       │   ├── Fetch inputs (executor-0 → store)
│       │   ├── Build (executor-0, nix sandbox)
│       │   └── Upload output (executor-0 → store)
│       └── Assign derivation-B (scheduler → executor-1)
│           ├── Fetch inputs (executor-1 → store)
│           ├── Build (executor-1, nix sandbox)
│           └── Upload output (executor-1 → store)
└── Return result (gateway → client)
```

== Configuration

OTel config is read from environment variables (NOT the config loader)
because `init_tracing()` runs before config parsing and must not depend on
any crate's config layout.

#table(
  columns: (auto, 1fr),
  align: (left, left),
  table.header([Env var], [Description]),
  [`RIO_OTEL_ENDPOINT`],
  [OTLP gRPC collector endpoint (e.g., `http://otel-collector:4317`). Unset =
    OTel disabled entirely (zero overhead).],

  [`RIO_OTEL_SAMPLE_RATE`],
  [Trace sampling rate 0.0--1.0 (default: 1.0). Clamped.],

  [`RIO_LOG_FORMAT`], [`json` or `pretty` (default: `json`).],
)

The OTel `service.name` resource attribute is set automatically per component
(gateway, scheduler, store, executor, controller) by `init_tracing()`.

== Concurrency tuning

Config-layer env vars (`RIO_<FIELD>`) that bound fan-out at known saturation
points. These interact multiplicatively --- the defaults are tuned together.

#table(
  columns: (auto, auto, auto, 1fr),
  align: (left, left, left, left),
  table.header([Env var], [Component], [Default], [Description]),
  [`RIO_SUBSTITUTE_MAX_CONCURRENT`],
  [scheduler],
  [256],
  [In-flight detached substitution-fetch tokio tasks. Memory bound only ---
    per-replica throughput is #rref("store.substitute.admission").],

  [`RIO_SUBSTITUTE_ADMISSION_PERMITS`],
  [store],
  [`(pg_max × 3).clamp(64, 128)`],
  [Per-replica cap on concurrent `try_substitute_on_miss`. Excess queue
    server-side up to 25s, then `ResourceExhausted` (transient).],

  [`RIO_CHUNK_UPLOAD_MAX_CONCURRENT`],
  [store],
  [8],
  [Max concurrent S3 `PutObject` calls per `put_chunked`. Bounds store→S3
    fan-out within a single large-NAR ingest.],

  [`RIO_S3_MAX_ATTEMPTS`],
  [store],
  [10],
  [aws-sdk retry ceiling per S3 operation. Raised from the sdk default (3) to
    absorb connection churn from S3-compatible backends that recycle idle
    connections aggressively.],
)

The per-replica in-flight S3 PutObject ceiling is
`RIO_SUBSTITUTE_ADMISSION_PERMITS × RIO_CHUNK_UPLOAD_MAX_CONCURRENT` --- keep
it under the aws-sdk's \~1024 default connection pool with headroom for other
store traffic. If `DispatchFailure` appears in store logs during large-NAR
ingest, raise `RIO_S3_MAX_ATTEMPTS` first (cheap, retries absorb transient
connection churn); lower `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` only if retries
don't clear it (reduces throughput).

== Trace Propagation

#r("obs.trace.w3c-traceparent")[
  Trace context is propagated via gRPC metadata using the W3C `traceparent`
  header format.
]

#r("sched.trace.assignment-traceparent")[
  ssh-ng has no gRPC metadata channel, so the scheduler→executor hop cannot
  use `inject_current`/`link_parent`. Span context also does not cross the
  scheduler's mpsc actor channel --- calling `current_traceparent()` at
  dispatch time would capture an orphan actor span. Instead, the
  `SubmitBuild` gRPC handler captures `current_traceparent()` *after*
  `link_parent()` (inside the scheduler handler span --- which has its own
  trace_id, LINKED to the gateway trace), and carries it as plain data:
  `MergeDagRequest.traceparent` → `DerivationState.traceparent` →
  `WorkAssignment.traceparent` at dispatch. The executor extracts it via
  `span_from_traceparent()` and wraps the spawned build-executor future in
  `.instrument(span)`. The span is created then `set_parent()` is called
  *before it is entered* --- the tracing-opentelemetry bridge allocates the
  OTel span lazily on first enter, at which point the stored parent context
  is available for the OTel SpanBuilder. This produces *parent-child* (same
  trace_id): the executor span's `parentSpanId` matches a scheduler `spanId`;
  Tempo shows scheduler→executor as one trace. *First-submitter-wins on
  dedup:* when two builds merge the same derivation, the existing state's
  traceparent is preserved unless it is empty (recovery/poison-reset set
  `""`), in which case the first live submitter upgrades it. Traceparent is
  not persisted to PG --- recovered derivations dispatched before any
  re-submit get a fresh executor root span. Empty traceparent → fresh root
  span. This closes the SSH-boundary tracing gap --- Tempo shows
  scheduler→executor as one trace (via the `WorkAssignment.traceparent`
  data-carry + `span_from_traceparent`), linked to the gateway trace upstream
  and linked to store traces downstream (the executor→store hop uses
  `inject_current` + store-side `link_parent`, the same
  `#[instrument]`-then-`set_parent` pattern proven to produce a LINK at the
  gateway→scheduler boundary). Injection and extraction are *manual*, not
  tonic interceptors: `rio_proto::interceptor::inject_current()` copies the
  current span's context into outgoing request metadata (client side), and
  `rio_proto::interceptor::link_parent()` adds an OTel span *link* to the
  incoming traceparent (server side, first line of each handler) --- the
  `#[instrument]` span was already created and entered with its own trace_id
  before `set_parent()` runs, so the result is a link, not a parent-child
  edge. The explicit manual call makes propagation points greppable and
  avoids tonic's `Interceptor` trait (which changes `connect_*` return types
  and doesn't compose with server-side `#[instrument]`). The W3C
  `TraceContextPropagator` is registered globally in `init_tracing()`
  regardless of whether `RIO_OTEL_ENDPOINT` is set --- propagation works even
  when spans aren't exported.
]

#r("obs.trace.scheduler-id-in-metadata")[
  The scheduler sets `x-rio-trace-id` in `SubmitBuild` response metadata to
  its handler span's trace_id (captured AFTER `link_parent()`). The gateway
  emits THIS id as the `(trace <32-hex>)` suffix on the `rio: build <id>`
  `STDERR_NEXT` preamble, not its own.
  Rationale: `link_parent()` + `#[instrument]` produces an orphan --- the
  scheduler handler span has its own trace_id, LINKED to the gateway trace
  but not parented. The gateway's trace contains only gateway spans; the
  scheduler's trace is the one extended through executor via the
  `WorkAssignment.traceparent` data-carry. Operators grepping the emitted id
  land in the trace that actually spans the full scheduler→executor chain.
  Header absent (legacy scheduler / no OTel configured) → gateway falls back
  to its own `current_trace_id_hex()`.
]

= SLOs, SLIs, and Alerting

== Service Level Indicators (SLIs)

#table(
  columns: (auto, 1fr),
  align: (left, left),
  table.header([SLI], [Source Metric(s)]),
  [Gateway connection success rate],
  [#(refs.metric)("rio_gateway_connections_total") minus connection errors /
    total],

  [Scheduler build completion rate],
  [#(refs.metric)("rio_scheduler_builds_total") outcome=success / total],

  [Store PutPath success rate],
  [#(refs.metric)("rio_store_put_path_total") minus errors / total],

  [Executor build success rate],
  [#(refs.metric)("rio_builder_builds_total") outcome=success / total],
)

== Service Level Objectives (SLOs)

#table(
  columns: (1fr, auto),
  align: (left, left),
  table.header([SLO], [Target]),
  [Non-PermanentFailure builds complete within 2x estimated duration], [99.9%],

  [PutPath success on first attempt], [99.99%],
  [Cache-hit latency (p99)], [< 1s],
)

== Alerting

- *Error budget burn rate:* Alert when the error budget consumption rate
  exceeds 14.4x the allowed rate over 1h (fast burn) or 6x over 6h (slow
  burn), following the multi-window multi-burn-rate approach.
- *Saturation alerts:* PostgreSQL connection pool utilization > 80%, S3 rate
  limiting (429 responses), executor queue depth exceeding 2x executor count.
- *Absence alerts:* No executor heartbeat received for > \~30-40s
  (`HEARTBEAT_TIMEOUT_SECS=30` + ≤1 tick alignment). Indicates an executor
  has silently died or lost network connectivity.

= Structured Logging

#r("obs.log.required-fields")[
  All components emit structured JSON logs via `tracing-subscriber` with the
  following required fields per log line:
]

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`timestamp`], [RFC 3339], [Event time],
  [`level`], [string], [Log level (TRACE, DEBUG, INFO, WARN, ERROR)],
  [`component`],
  [string],
  [Emitting component (gateway, scheduler, store, executor, controller)],

  [`build_id`], [string], [Build request ID (if applicable)],
  [`derivation_hash`], [string], [Derivation hash (if applicable)],
  [`executor_id`],
  [string],
  [Executor instance ID (builder/fetcher components only)],

  [`message`], [string], [Human-readable log message],
)

Conditionally present:

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Field], [Type], [When present]),
  [`trace_id`],
  [string],
  [Only when `RIO_OTEL_ENDPOINT` is set AND the log is emitted within an
    active span. The default JSON fmt layer does NOT include trace/span IDs
    --- they come from the OTel layer's span context.],

  [`span_id`], [string], [Same condition as `trace_id`.],
  [`tenant_id`],
  [string],
  [Tenant identifier. Gateway records `tenant` (name) on the session span
    (`session.rs`). Scheduler records `tenant_id` (UUID) on the `SubmitBuild`
    span after `resolve_tenant` succeeds. Persisted to `builds.tenant_id`.],
)

Optional fields may be added per component as `tracing` span fields. All
fields use snake_case. Missing context fields (e.g., `build_id` outside a
build context) are omitted rather than set to empty strings.

= Dashboard Data Sources

The rio-dashboard consumes data from two sources:

#table(
  columns: (auto, auto, auto),
  align: (left, left, left),
  table.header([Data], [Source], [Protocol]),
  [Builds, executors, logs], [`AdminService`], [gRPC-Web],
  [Metrics, graphs], [Prometheus], [HTTP (direct or via Grafana)],
)

The dashboard does NOT query PostgreSQL or S3 directly.
