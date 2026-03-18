# Plan 0035: Observability — scheduler/store/worker metrics per spec (initial landing)

## Context

`docs/src/observability.md` specifies the metric surface for each component. P0003 (phase 1a) wired the Prometheus exporter and registered gateway metrics. This plan lands the scheduler, store, and worker metrics — 11 total — at the call sites.

Design principle enforced here: **gauges use `set()` from ground truth, not increment/decrement.** The scheduler's `Tick` event recomputes `builds_active`, `derivations_queued`, `derivations_running` from the actual DAG/map lengths, not from counters maintained at transition sites. Self-healing: a missed decrement doesn't permanently skew the gauge.

Also includes the `root_span(component)` helper — every service's main() now opens a top-level tracing span with `component="scheduler"` etc. for structured log correlation.

## Commits

- `adea6e2` — feat(observability): add missing scheduler/store/worker metrics per spec
- `76a1561` — fix(rio-gateway): improve error handling and diagnostics
- `637ae08` — test(rio-scheduler): complete Phase 2a test coverage gaps
- `ecf6444` — chore(rio-common): delete unused ServiceAddrs/ServiceError

## Files

```json files
[
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "root_span(component) helper"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "6 metrics: builds_total (counter on merge entry), builds_active/derivations_queued/derivations_running (gauges from ground-truth on Tick), assignment_latency_seconds (histogram Ready→Assigned via ready_at field), cache_hits_total (counter on Completed-during-merge)"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "ready_at: Option<Instant> on DerivationState — set on Ready transition, recorded+cleared on Assigned"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "3 metrics: put_path_total{result=created|exists}, put_path_duration_seconds (histogram on successful create), integrity_failures_total (on GetPath hash mismatch)"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "delete_uploading(): remove placeholder narinfo+nar_blobs rows on validation failure — enables clean retry"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "builds_total (counter on execute_build entry — moved to terminal with outcome label in P0044)"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "fuse_cache_size_bytes gauge set from ground-truth on insert and post-eviction"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "query_path_exists → Result<bool, Errno>: NOT_FOUND=Ok(false), transport error=Err(EIO); bounded backoff for concurrent fetch (6 attempts, 50ms→500ms)"},
  {"path": "rio-common/src/config.rs", "action": "DELETE", "note": "ServiceAddrs/CommonConfig — zero callers"},
  {"path": "rio-common/src/error.rs", "action": "DELETE", "note": "ServiceError — zero callers"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "test_distributed_full_build_with_fuse: todo!()→graceful skip with prerequisites message"}
]
```

## Design

**Scheduler metrics (6):** `rio_scheduler_builds_total` increments on `handle_merge_dag` entry. Three gauges (`builds_active`, `derivations_queued`, `derivations_running`) are set on every `Tick` from `builds.len()` / `ready_queue.len()` / DAG-count-where-status-in-{Assigned,Running}. `assignment_latency_seconds` histogram: `ready_at` timestamp set when a derivation transitions to `Ready`, recorded when it transitions to `Assigned`, then cleared. `cache_hits_total` increments when `compute_initial_states` finds a derivation already `Completed` in the DAG (shared dep from a prior build).

**Store metrics (3):** `put_path_total{result}` — `created` on successful flip, `exists` on idempotent short-circuit, `error` label added later (P0047). `put_path_duration_seconds` only on `created`. `integrity_failures_total` — the `DATA_LOSS` path in `GetPath`.

**Worker metrics (2):** `builds_total` increments at `execute_build` entry (no outcome — P0044 moves it to terminal with label). `fuse_cache_size_bytes` gauge set from SQLite `SUM(size_bytes)` after every insert and eviction.

**delete_uploading (`76a1561`):** P0028's `PutPath` left the placeholder row on validation failure — unique constraint blocks retry. `delete_uploading` removes both `narinfo` (`nar_size=0` marker) and `nar_blobs` (`status='uploading'`) rows. Called on NAR validation failure and backend write failure.

**Test coverage (`637ae08`):** keepGoing false/true branching, transient-retry increments retry_count, build state-machine terminal rejections, store PutPath→GetPath roundtrip, synth-DB Refs FK pairs (not just row count), log batcher 100ms timeout flush.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Metric names were later retro-tagged `r[obs.sched.*]`, `r[obs.store.*]`, `r[obs.worker.*]` in `docs/src/observability.md`.

## Outcome

Merged as `adea6e2..ecf6444` (4 commits). Closes review findings I5, I7, I8, I9–I14, H9. 4 metrics still missing from the spec — landed in P0044. `builds_total` lacks outcome labels — fixed P0044. Test coverage gaps (CancelBuild, WatchBuild, max_retries) — closed P0049.
