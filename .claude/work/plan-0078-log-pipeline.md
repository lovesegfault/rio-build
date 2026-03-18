# Plan 0078: Log pipeline — ring buffer, rate-limit, S3 flush, AdminService, E2E

## Design

The scheduler-side half of the log pipeline. Worker side was built in phase-2a (`LogBatcher` in `rio-worker/src/log_stream.rs`) and fixed in P0072 (cancel-safe `BATCH_TIMEOUT`). This plan lands the five pieces that make logs actually usable: in-memory ring buffer, per-build rate/size limits, S3 persistence, admin RPC, and the gateway forward path. Five commits, landed together because they're only unit-testable as a unit.

**Ring buffer + forward** (`eb06829`): `LogBuffers` (`logs.rs`, new) is a `DashMap<drv_path, VecDeque<(line_number, bytes)>>`. Lives **outside the actor** so a chatty build (10k lines/s) can't fill the actor's bounded `mpsc(10_000)` and trip backpressure hysteresis. Written directly by the `BuildExecution` recv task. `RING_CAPACITY=100k` lines (~10 MiB/derivation @ typical 100b/line, ~500 MiB @ 50 concurrent — bounded). Evicts oldest on overflow. Edge case: single batch > capacity keeps the **tail**, not the head (you want to see what the build is doing now, not what it was doing at start). The gRPC `LogBatch` arm in the worker-stream recv task pushes to the ring buffer, then sends `ActorCommand::ForwardLogBatch` — the actor resolves `drv_path → drv_hash → interested_builds`, fans out `BuildEvent::Log` to each build's broadcast channel, the `bridge_build_events` helper (from P0065) bridges to the gateway's `SubmitBuild` stream.

**Per-build rate + size limits** (`751418e`): `LogBatcher` gains `LogLimits{rate_lines_per_sec, total_bytes}` and returns `AddLineResult::{Buffered, BatchReady, LimitExceeded}`. When a limit trips, the stderr loop flushes already-buffered lines (client sees output right up to the limit, not 63 lines dropped on the floor) then returns `BuildStatus::LogLimitExceeded` (Nix-native status 11, terminal, non-retryable — retrying on a different worker would spew the same logs). Size check is prospective: at 99.9 MiB with a 1 MiB line coming in, reject THAT line (prospective 100.9 > 100). `total_bytes` accumulates across flushed batches. Rate is a 1-second tumbling window via `Instant` (monotonic, NTP-proof) — no token bucket, sustained spew is the only case that matters. Both 0 = unlimited. Introduces `ExecutorEnv` struct to bundle `{fuse_mount_point, overlay_base_dir, worker_id, log_limits}` — keeps `execute_build` at 5 args (was 8 → clippy violation; project has zero-allow policy).

**S3 flush + periodic** (`1b9ec50`): `LogFlusher` task (`logs/flush.rs`): gzip → S3 PUT → PG insert. Two triggers, both outside the actor loop (hybrid model: buffer outside actor, flush **triggered by** actor so it's ordered with state transitions, but flush I/O runs on a separate task). **Completion flush**: actor `try_send`s `FlushRequest` from `handle_completion_{success,permanent_failure}` (failed builds have useful logs, often the MOST useful). `drain()`s the buffer, gzips in `spawn_blocking` (~50ms for 10 MiB, too long for a tokio worker thread), PUTs to `logs/{build_id}/{drv_hash}.log.gz`, UPSERTs `build_logs` rows (one per `interested_build`, same `s3_key` — derivation builds once even if N builds want it). **Periodic flush** (30s tick): `snapshot()`s all active buffers (NON-draining — derivation still running), PUTs to `logs/periodic/{basename}`, NO PG rows (crash-recovery only, not dashboard-servable). `select!` biased toward completion channel: a completion at tick boundary gets the final flush, not a snapshot. Flusher NEVER dies on transient S3/PG errors (if it died, ALL future logs lost). Migration `005_build_logs.sql`: `(build_id, drv_hash) UNIQUE` for UPSERT.

**AdminService.GetBuildLogs** (`19e9b93`): first real `AdminService` impl. Two-source serve: active → ring buffer (freshest), completed → S3 (via PG `build_logs.s3_key`). Ring buffer checked first — if it has lines, derivation is still running or just-completed-not-yet-drained. S3 fallback: gunzip in `spawn_blocking`, chunk into `CHUNK_LINES=256`. Other 6 `AdminService` RPCs return `UNIMPLEMENTED` (phase4 dashboard, phase3a controller) — stubbing them here means tonic server wiring is in place, each later is a pure body-swap.

**E2E test** (`55ac712`): `test_log_pipeline_grpc_wire_end_to_end` exercises every hop: gRPC wire decode → ring buffer push → actor `ForwardLogBatch` resolution → broadcast → `bridge_build_events` → `SubmitBuild` stream delivery.

## Files

```json files
[
  {"path": "rio-scheduler/src/logs.rs", "action": "NEW", "note": "LogBuffers ring buffer (DashMap<drv_path, VecDeque>); RING_CAPACITY=100k; tail-keep on overflow"},
  {"path": "rio-scheduler/src/logs/mod.rs", "action": "NEW", "note": "logs.rs → logs/mod.rs after flush.rs added"},
  {"path": "rio-scheduler/src/logs/flush.rs", "action": "NEW", "note": "LogFlusher: gzip→S3 PUT→PG UPSERT; biased select! completion > periodic; never-die on transient errors"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "ForwardLogBatch command; LogBuffers Arc plumbing"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "try_send FlushRequest from completion_{success,permanent_failure}"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "ring buffer + forward tests"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "BuildExecution recv task: LogBatch arm → ring buffer push + ForwardLogBatch"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "test_log_pipeline_grpc_wire_end_to_end"},
  {"path": "rio-scheduler/src/admin.rs", "action": "NEW", "note": "AdminService.GetBuildLogs (ring buffer → S3 fallback); 6 RPCs UNIMPLEMENTED"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "mod logs, mod admin"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "S3 client, Arc<LogBuffers> to SchedulerGrpc+AdminService+LogFlusher; AdminServiceServer in tonic chain"},
  {"path": "rio-worker/src/log_stream.rs", "action": "MODIFY", "note": "LogLimits{rate,size}; AddLineResult enum; prospective size check; 1s tumbling rate window"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "LimitExceeded → flush remaining → BuildStatus::LogLimitExceeded"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "ExecutorEnv struct {fuse_mount_point, overlay_base_dir, worker_id, log_limits}"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "BuildSpawnContext.log_limits plumbing"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "cfg.{log_rate_limit, log_size_limit} → BuildSpawnContext"},
  {"path": "migrations/005_build_logs.sql", "action": "NEW", "note": "build_logs table, (build_id, drv_hash) UNIQUE for UPSERT"},
  {"path": "nix/modules/scheduler.nix", "action": "MODIFY", "note": "S3 config for LogFlusher"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[sched.logs.ring]`, `r[sched.logs.flush]`, `r[sched.logs.admin]`, `r[wk.log.limits]`.

## Entry

- Depends on **P0077** (UUID v7): `build_id` generation site in `grpc/mod.rs`; log S3 keys are `logs/{build_id}/...` and the v7 time-ordering is why.
- Depends on **P0072** (pre-bulk fixes): worker `log_stream.rs` cancel-safe `BATCH_TIMEOUT` is prerequisite for correct batching.
- Depends on **P0076** (figment config): `cfg.{log_rate_limit, log_size_limit}` come from the new config layer.

## Exit

Merged as `eb06829..55ac712` (5 commits). `.#ci` green at merge. `test_log_pipeline_grpc_wire_end_to_end` validates the full chain in-process.
