#import "/lib/rio.typ": *
#show: rio.with(domains: ("obs", "common", "sched"))


rio-build provides three pillars of observability: logs, metrics, and traces.

= Build Log Storage

Build logs are stored durably for post-build analysis and the dashboard log
viewer.

== Storage Format

Build logs are stored in S3 as zstd-compressed blobs, keyed by the derivation
execution that produced them:

```
logs/{derivation_hash}/{exec_id}.log.zst          (final flush)
logs/{derivation_hash}/{exec_id}.partial.log.zst  (periodic 30s snapshot)
```

`exec_id` is a per-execution UUIDv7 minted by the scheduler at dispatch
(`assign_to_worker`). Time-sortability means "latest execution for a
derivation" is a single index seek (`ORDER BY exec_id DESC LIMIT 1`).
Metadata (S3 key, line counts, byte offsets, completion status, timestamps)
is stored in the `drv_logs` PostgreSQL table for efficient seeking,
pagination, and TTL retention.

#r("obs.log.exec-keyed")[
  Build logs MUST be stored at `logs/{drv_hash}/{exec_id}.log.zst` keyed by
  per-execution UUIDv7. One blob and one PG row per execution regardless of
  how many builds are interested in the derivation.
]

A derivation is built once even if N builds want it (`sched.merge.dedup`), so
keying by `(build_id, drv_hash)` would write N PG rows pointing at one blob ---
or, in the prior model, N copies of the blob under N keys. Keying by
`(drv_hash, exec_id)` stores the log once and lets `build_derivations.exec_id`
carry the build↔execution correlation. Periodic snapshots get the `.partial`
suffix and a `drv_logs` row with `is_complete = false`; the final flush
overwrites the row, writes the non-`.partial` key, and best-effort deletes the
snapshot. Both are swept by the same TTL.
A final flush whose ring buffer drained empty (a failover left the new
leader holding a re-stamped but never-streamed-to entry) stamps `status` and
`finished_at` in place but leaves `is_complete = false`: the `.partial` blob
--- neither re-keyed nor deleted, and the execution's only stored content ---
is missing everything after the ex-leader's last periodic snapshot, so the
incomplete indicator below stays visible.
When the worker instead reconnects to a later tenure of the same execution
and keeps streaming --- a fresh standby whose ring holds only the
re-streamed suffix, or a re-acquiring ex-leader whose retained ring
overlaps or has interior holes relative to what was stored in between ---
that tenure's flusher fetches the execution's existing `.partial` snapshot
once and folds it into every subsequent flush of that execution, so the
periodic overwrite and the final blob keep covering output recorded by
earlier tenures; stored coverage recorded by an earlier tenure is never
reduced by a later flush. Lines emitted between the prior leader's last
snapshot and the failover remain subject to the 30-second bound, and the
gap between the folded prefix and the resumed stream is marked in-band
with a `[rio: ~N earlier lines lost across scheduler failover]` line;
lines an interim leader received but never flushed are within the same
30-second bound but their absence is not separately marked.

#r("obs.log.gap-span")[
  A `drv_logs` row whose blob folds a recovered pre-failover prefix MUST
  describe the execution's true line-number span --- `first_line + line_count`
  is one past the highest true worker line stored, with the lost range counted
  even though the blob replaces it with a single marker line. A
  `GetDerivationLogs` read with `since_line > 0` MUST NOT skip lines the
  client has not received: when the stored blob's physical line count does not
  match the row's claimed span, the server re-serves the blob from its start
  unless the cursor is at or past the true end.
]

The marker collapses the lost range into one physical line, so blob index and
true line number diverge once a marker stands in for two or more lines.
Keeping the row's range in true line-number space keeps the `since_line`
short-circuit and the next failover's prefix/gap computation honest; the read
path detects the divergence by comparing the decoded blob's physical line
count against the row's span, so gapless blobs (and a marker that replaced a
single lost line) keep exact resume, and any mismatch is served in full ---
bandwidth over silent loss. A client that derives its resume cursor from chunk
labels will re-download such a blob on each poll until the row is finalized;
no first-party client does (the CLI and dashboard both read from line 0).
Per-chunk `first_line_number` labels inside a gap-merged blob remain
physical-index based --- exact labels would need per-segment metadata, which
no current client justifies.

#r("obs.log.finalize-immutable")[
  Once an execution's `drv_logs` row has `is_complete = true`, its stored final
  blob and row content MUST NOT be overwritten or regressed by a later flush of
  in-memory buffer state for that `exec_id`; a flusher holding retained lines
  for an already-finalized execution MUST discard them instead of
  re-finalizing.
]

Across an A→B→A lease flap the re-acquired ex-leader can still hold a ring
entry stamped with an execution the interim leader already finalized --- the
entry is pre-failover residue retained so that genuinely-abandoned executions
can be finalized by the cancel/dependency-failure sweep. In-memory state alone
cannot distinguish the two, so the flusher consults the row before uploading:
an already-complete row means the durable record is authoritative, and any
retained lines it lacks fall within the accepted periodic-flush failover-loss
bound. The UPSERT latch alone is not sufficient --- it freezes a finalized
row (any later flush write against it is refused, downgrade and
re-finalization alike), but the S3 PUT precedes the row write entirely, so a
stale re-finalization would still overwrite the final blob in place while the
frozen row keeps pointing at it.
When the row cannot be consulted at all (the lookup itself fails), the flusher
fails closed: nothing is uploaded on that attempt. A deferral with a non-empty
buffer is retained and retried (below); a deferral that finds only an empty
restamped entry reaps it instead of retaining it (bookkeeping --- nothing any
retry could upload, and `GetDerivationLogs` probes the execution's stored
`.partial` whenever the ring entry it finds holds zero lines, so the reap does
not gate reads). Either way the
execution's row may remain `is_complete = false` (surfaced per
`obs.log.incomplete-surfaced`) until a retry lands.

#r("obs.log.deferred-final-retry+3")[
  A final flush deferred because the finalize guard could not consult
  `drv_logs` MUST be retained by the flusher --- up to a bounded retention
  cap --- and retried while the execution's sealed ring-buffer entry remains
  in memory, and terminal cleanup MUST NOT discard an entry whose final
  flush is still pending --- enqueued at the terminal epilogue and not yet
  resolved by the flusher, or retained for retry; a deferral beyond the cap
  drops that execution's buffered entry instead of retaining it. A final
  flush request --- first attempt or retained retry --- MUST NOT finalize
  the execution under a leadership tenure other than the one that enqueued
  it: the request carries the scheduler-lease generation at enqueue time and
  the flusher MUST drop it, uploading nothing, when the replica no longer
  holds the lease or its generation has moved on; the live tenure's own
  terminal processing owns that execution's finalization.
]

This is what keeps a transient PG failure at final-flush time from losing
buffered log content while S3 stays healthy. The retention is a fixed cap of
in-flight deferrals and does not survive process exit; the retry --- like
the first attempt of a queued final --- is pinned to the leadership tenure
that enqueued it: a queued or deferred final is evidence PG was unreachable
at terminal time, the same window in which the terminal-status persist fails
and the lease lapses, so after a leadership change the execution may still
be live and being extended by the tenure that now owns it, and a late stale
final must not freeze that row (`obs.log.finalize-immutable`,
`obs.log.stored-coverage-preserved`). Requests orphaned by a leadership
change are dropped and counted
(#(refs.metric)("rio_scheduler_log_flush_stale_tenure_total")); the
execution then either gets finalized by the live tenure's own terminal flush
or remains at its `.partial` coverage (surfaced per
`obs.log.incomplete-surfaced`).
The tenure check runs before the finalize guard's stored-row consult and is
re-checked after the guard's stored-row consult and the stored-prefix
reconcile --- the awaited steps that precede any destructive arm --- so a
request orphaned before its attempt or during those awaits triggers no S3
work, no drain, no row freeze, and no `.partial` delete; at most it reaps the
seal-guarded residue of an entry whose execution the in-hand guard row
already shows finalized. The destructive arms it is kept away from are
execution-scoped but not tenure-scoped and could otherwise touch a ring
entry that the new tenure's recovery has restamped onto the still-live
execution. The post-drain window (compression, blob PUT, row upsert,
`.partial` delete) is deliberately not re-checked --- the drained ring is the
only copy of the terminal-observed lines; the accepted residual is a lease
move during the upload freezing the row while the live tenure keeps extending
the same execution elsewhere. The drop itself performs no PG work. When the orphaned
request's entry is still sealed for that execution (no restamp in the current
tenure adopted it), the drop reaps an empty entry outright (the terminal
persisted under the old tenure, so nothing else will ever resolve it; reads do
not depend on the reap --- `GetDerivationLogs` probes the execution's stored
`.partial` whenever the ring entry it finds holds zero lines --- so removing
the entry is memory hygiene); a sealed non-empty entry is left in place and
reaped by the periodic
flush instead --- once another tenure has finalized the execution, the
snapshot's row UPSERT is refused by the frozen-row latch and the flusher
discards the still-sealed entry on that refusal (the durable finalized record
is authoritative and the retained lines are superseded by it), within one
periodic tick of PG and leadership recovery. A sealed non-empty entry whose
execution no tenure ever finalizes keeps being snapshotted at `.partial`
coverage --- its ring lines are the best data available --- unless the
per-tenure stored-coverage reconcile finds a prior tenure's row covering past
the retained ring and empties it: an empty ring is never uploaded, so the
refused-UPSERT chokepoint can no longer observe that entry, and the periodic
flush instead reaps the sealed, now-empty entry at its empty-snapshot
early-return as bookkeeping --- reads are already served from that stored
`.partial`. Any other
entry --- unsealed or restamped --- is left for its real owner: the live
tenure's own final, the next dispatch discard, or process exit. (None of these
reaps gates read availability: `GetDerivationLogs` probes the execution's
stored `.partial` whenever the ring entry it finds holds zero lines, falling
back to the empty re-poll chunk only when nothing is stored for that execution
yet, or when the stored side cannot be consulted at all (PG query / S3 fetch
failure) --- the read warns and degrades to the re-poll answer rather than
erroring.)
The protection starts at enqueue: a final still queued behind earlier stalled
flushes during the same outage is protected exactly like one already
attempted and deferred, and stays pinned until the flusher resolves the
request (or the process exits --- the dead-flusher residual; or, for an entry
that is empty --- it never held a line, or the stored-coverage reconcile
emptied it --- the periodic sealed-empty reap may remove it first, the final
then resolving via the no-entry arm with only the empty drain's
status/finished_at stamp lost).

#r("obs.log.incomplete-surfaced")[
  A `GetDerivationLogs` response whose final chunk carries
  `is_complete = false` MUST be surfaced to the user as incomplete: the CLI
  prints a trailing notice to stderr and the dashboard log viewer renders an
  "incomplete" banner. The lines themselves are served as-is --- the flag is
  display metadata, not a serving gate.
]

A `.partial`-only row (leader failover before the final flush, a dropped
completion `FlushRequest`, a final flush deferred because its `drv_logs`
lookup failed, an abandoned execution) serves the periodic
snapshot --- strictly more useful than `NotFound`, but the missing tail is
usually the most interesting part of the log: the build error. Without an
explicit indicator the user reads a truncated log as the whole thing.

#r("obs.log.stored-coverage-preserved")[
  Log content recorded in an execution's `drv_logs` row by a prior scheduler
  tenure and not contiguously covered by the current tenure's in-memory ring
  MUST NOT be overwritten by a later flush of that execution: the flusher MUST
  fold the stored blob into the outgoing upload (superseding any overlapping
  in-memory lines), and when the stored blob cannot be re-read it MUST skip the
  periodic snapshot or preserve the `.partial` blob on the final flush.
]

The durable record of what other tenures did is the `drv_logs` row, so the
"is this overwrite lossy?" decision consults the row --- not the shape of the
local ring, whose latches and line ranges encode conclusions reached in a
previous tenure. Three carve-outs bound the rule:

- Same-tenure ring eviction (the ring's head outruns the periodic flush within
  one tenure) is the pre-existing, accepted `RING_CAPACITY`-bounded loss and is
  outside this rule --- the row in that shape was produced by this tenure from
  this very ring after its reconciliation.
- Lines an interim leader received but never flushed are not "stored content";
  they remain subject to the 30-second periodic-flush bound and their absence
  is not separately marked in-band.
- The fetch-failure fallback preserves the *blob*; the row's terminal stamping
  in that degraded case is unchanged from the pre-existing behavior.

#r("obs.log.worker-header")[
  The worker MUST write `rio: exec`, `rio: builder`, `rio: started` lines as
  the first lines of every build log, and a `rio: exec` + `rio: result` footer
  after the build process exits. These lines are display-only and consumers
  MUST NOT parse them for authoritative state.
]

The header/footer are written into the same untrusted byte stream as build
output --- arbitrary build code can emit its own `rio: result ok` lines. The
system's source of truth for `exec_id`, outcome, and sizing is `drv_logs` and
`assignments`, not the log text. The `grep '^rio:'` extraction is a convenience
for humans (the post-failure log tail Nix prints, the dashboard log viewer),
not a protocol. On scheduler-initiated cancellation the footer may be absent or
may disagree with the row. Cancelling an in-flight (`Assigned`/`Running`)
execution seals and finalizes its log before the worker receives the
`CancelSignal`, so the late footer is dropped and the log normally ends without
a `rio: result` line. Cancelling a build whose derivation was already reset off
a lost or force-drained worker --- or sweeping such a derivation into
`DependencyFailed` when one of its dependencies permanently fails --- finalizes
that prior execution's retained
buffer, which may already hold the footer the worker pushed on its way out ---
possibly `rio: result ok` when the success report was lost to the disconnect
--- so the stored line, if present, can disagree with the row. `drv_logs.status`
carries the authoritative outcome in both cases. Pod and node
identity are deliberately excluded --- the "cluster is one machine" abstraction
holds at the log level too.

== Log Lifecycle

#r("obs.log.batch-64-100ms")[
  Log lines are batched (up to 64 lines or 100ms, whichever first) in
  `BuildLogBatch` messages.
]

#r("obs.log.ring-byte-cap")[
  The scheduler-side per-derivation ring buffer is bounded by both line count
  (`RING_CAPACITY`) and bytes (`RING_BYTE_CAP`, 16 MiB). Individual lines are
  truncated to `MAX_LINE_LEN` (64 KiB) before storage. Scheduler-side defense
  against an untrusted worker pushing few-but-huge lines that the line-count
  cap alone would not evict.
]

#r("obs.log.periodic-flush")[
  The scheduler flushes buffers to S3 periodically (every 30s) during active
  builds, not only on completion --- bounds log loss to at most 30s on
  failover.
]

#r("obs.log.failure-reason-persisted")[
  When a derivation reaches terminal failure (poisoned), the scheduler MUST
  persist the builder-reported error text and the originating execution id
  alongside the poison mark, and MUST clear both whenever the poison state is
  reset (admin clear, TTL expiry, resubmit reset).
]

The persisted reason (`derivations.failure_msg` / `failure_exec_id`,
migration 073) is what a later build's fail-fast surfaces to the client
(#rref("sched.merge.failfast-culprit")) --- the original execution's log may
have expired or contain nothing, but the reason text survives with the poison
mark and dies with it.

#figure(
  chronos.diagram({
    import chronos: *
    _par("Executor")
    _par("Scheduler")
    _par("S3")
    _seq(
      "Executor",
      "Scheduler",
      comment: [`BuildLogBatch` (batched, ≤64 lines or 100ms)],
    )
    _note(
      "over",
      [Buffer in memory\ (per-derivation ring buffer)],
      pos: "Scheduler",
    )
    _seq("Executor", "Scheduler", comment: [`CompletionReport`])
    _seq("Scheduler", "S3", comment: [Async flush (zstd + upload)])
    _note("over", [Write metadata to PG], pos: "Scheduler")
  }),
  caption: [Build-log lifecycle.],
)

+ Executors stream log lines to the scheduler via `BuildLogBatch` messages in
  the `BuildExecution` stream. Lines are batched (up to 64 lines or 100ms,
  whichever comes first) for efficiency.
+ The scheduler buffers logs in an in-memory ring buffer per active
  derivation.
+ On derivation completion, the scheduler asynchronously flushes the buffer
  to S3 as a zstd-compressed blob and upserts a `drv_logs` row keyed by
  `exec_id` (S3 key, byte offsets, timestamps, completion status).
+ The `AdminService.GetDerivationLogs` RPC reads from the in-memory buffer for
  active builds and from S3 for completed builds, resolving the latest
  execution when the caller does not pin one.

#info(title: [Periodic flush])[
  Logs are also flushed to S3 periodically (every 30s) during active builds,
  not only on completion. This bounds log loss to at most 30s of output if
  the scheduler fails over.
]

#memo(title: [Log durability tradeoff])[
  The 30-second flush interval is a deliberate tradeoff between write
  amplification and data loss. Flushing more frequently increases S3 PUT
  costs and scheduler CPU usage; flushing less frequently increases the
  window of log loss on crash. For most builds, 30s of lost logs is
  acceptable --- the build itself will be retried and new logs will be
  generated. For long-running builds where the final 30s of output is
  critical for debugging, consider a future enhancement: executors could
  write a local log file as a write-ahead log (WAL) that survives scheduler
  restarts, with the scheduler draining the WAL on recovery. Not currently
  planned.
]

== Log Serving

#table(
  columns: (auto, 1fr),
  align: (left, left),
  table.header([Build State], [Log Source]),
  [Active (building)], [In-memory ring buffer on scheduler],
  [Completed], [S3 blob (zstd), seekable via PG metadata],
  [Failed], [S3 blob (flushed on failure as well)],
)

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

#r("obs.metric.scheduler+2")[
  rio-scheduler MUST expose the metrics in
  #xref(<tbl-metrics-scheduler>, [the scheduler metric reference]). All
  metrics MUST follow the `rio_scheduler_*` naming prefix, except the
  shared `rio_pg_iam_*` family (#rref("obs.metric.pg-iam")).
]

#r("obs.metric.scheduler-leader-gate+2")[
  Scheduler leader-state gauges (`_builds_active`, `_derivations_queued`,
  `_derivations_running`, `_queue_depth`) are published *only by the leader*.
  The standby's actor is warm (DAGs merge for fast takeover per
  #rref("sched.lease.k8s-lease")), so its counts are stale or zero; with
  `replicas>1`, publishing from both would create duplicate Prometheus series
  with identical labels, and stat-panel reducers pick one
  nondeterministically. `_workers_active` is *not* leader-state: it tracks
  executors connected to *this pod*, maintained by inc/dec on
  connect/disconnect on the standby too. After a lease loss, the ex-leader's
  `_workers_active` series drains naturally to zero as executors rebalance to
  the new leader (it is never zeroed explicitly --- doing so would desync
  from the retained `executors` map and go negative on subsequent
  disconnects). Counters and histograms are unaffected --- the standby's
  dispatch loop no-ops, so its counters stay at zero naturally, and
  `sum(rate(...))` is the idiomatic query form anyway.
]

== Store Metrics

#r("obs.metric.store+2")[
  rio-store MUST expose the metrics in
  #xref(<tbl-metrics-store>, [the store metric reference]). All metrics MUST
  follow the `rio_store_*` naming prefix, except the shared
  `rio_pg_iam_*` family (#rref("obs.metric.pg-iam")).
]

#r("obs.metric.store-pg-pool")[
  #(refs.metric)("rio_store_pg_pool_utilization") is the *observed* load
  signal the ComponentScaler calibrates its learned `builders_per_replica`
  ratio against. PG pool exhaustion is a cliff (I-105: acquire times → 11s →
  builder FUSE blocks → circuit trip → all builds fail), not a ramp; the
  predictive signal (`Σ(queued+running)` builders) scales the store _ahead_
  of the burst, and this gauge corrects the ratio when the prediction drifts.
]

== Builder Metrics

#r("obs.metric.builder")[
  rio-builder MUST expose the metrics in
  #xref(<tbl-metrics-builder>, [the builder metric reference]). All metrics
  MUST follow the `rio_builder_*` naming prefix.
]

#r("obs.metric.input-materialization-failures")[
  #(refs.metric)("rio_builder_input_materialization_failures_total")
  (counter): incremented each time a daemon `MiscFailure` is reclassified as
  `InfrastructureFailure` under `r[builder.result.input-eio-is-infra]`
  (ADR-022 design overview §13). Sustained nonzero rate means closure inputs
  are failing to materialize from the castore-FUSE lower (store fetch
  errors, integrity failures, or a tripped fetch circuit breaker) ---
  correlate with #(refs.metric)("rio_builder_castore_fuse_eio_total") and
  rio-store health.
]

#r("obs.metric.transfer-volume")[
  Transfer-volume byte counters (`*_bytes_total`) are emitted at each hop:
  gateway (#(refs.metric)("rio_gateway_bytes_total")`{direction}`), store
  (`rio_store_{put,get}_path_bytes_total`), executor
  (`rio_builder_{upload,castore_fuse_fetch}_bytes_total`). Summing these
  across the topology gives a full picture of data movement --- e.g.,
  `rate(rio_builder_castore_fuse_fetch_bytes_total[5m])` vs
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

#r("obs.metric.controller+2")[
  rio-controller MUST expose the metrics in
  #xref(<tbl-metrics-controller>, [the controller metric reference]). All
  metrics MUST follow the `rio_controller_*` naming prefix, except the
  shared `rio_pg_iam_*` family (#rref("obs.metric.pg-iam")).
]

== Shared (rio-common) Metrics

#r("obs.metric.pg-iam")[
  The `rio_pg_iam_*` family is the sanctioned exception to the
  per-component prefix rules: it is emitted by `rio_common::pg_iam`
  and appears identically on every PG-consuming component (store,
  scheduler, controller). Each consumer MUST register the family by
  calling `rio_common::pg_iam::describe_metrics()` from its own
  `describe_metrics()` — registration and emission are separate call
  sites, and rio-common installs no exporter of its own. Members:
  #(refs.metric)("rio_pg_iam_mint_failures_total") (counter) and
  #(refs.metric)("rio_pg_iam_token_minted_timestamp_seconds") (gauge;
  alert on `time() - x` approaching the 900s token TTL — set per
  successful mint, never per refresher tick, so the PromQL age is the
  true token age).
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

  (refs.metric)("rio_scheduler_dispatch_wait_seconds"),
  [`[0.1, 0.5, 1, 5, 10, 30, 60, 120, 180, 300, 600]` (ephemeral builders:
    dominated by node-provision)],

  (refs.metric)("rio_scheduler_build_graph_edges"),
  [`[100, 500, 1000, 5000, 10000, 20000]` (count)],

  (refs.metric)("rio_builder_upload_references_count"),
  [`[1, 5, 10, 25, 50, 100, 250, 500]` (count)],

  [#(refs.metric)("rio_store_substitute_duration_seconds"),
    #(refs.metric)("rio_store_check_available_duration_seconds")],
  [`[0.01, 0.05, 0.1, 0.5, 1, 2.5, 5, 10, 30, 60, 120]` (@nar download +
    ingest; GB-scale substitutions span 60s+)],
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
