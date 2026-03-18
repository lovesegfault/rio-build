# Plan 0044: Observability completion — 4 missing metrics, dropped-cmd counter, outcome labels

## Context

P0035 landed the initial metrics. This plan fills gaps: 4 metrics spec'd in `observability.md` but not implemented, plus two new metrics for operational alerting (dropped cleanup commands, cache-check failures), plus the outcome-label fix that makes SLIs actually computable.

The outcome-label fix: `rio_scheduler_builds_total` and `rio_worker_builds_total` incremented at *submission/start* with no outcome label. The SLI spec expects `outcome=success / total`, but you can't compute that from a start-time counter. Moved to terminal transition with `outcome={succeeded|failed|cancelled}` label.

## Commits

- `2dcbcfb` — feat(observability): implement 4 missing Phase 2a metrics
- `fa98d7f` — feat(rio-scheduler): log and count dropped CleanupTerminalBuild commands
- `0145056` — feat(rio-scheduler): add cache check failure metric + circuit-breaker TODO
- `be15769` — fix(workspace): misc diagnostics and robustness cleanup
- `0748563` — feat(workspace): add outcome labels to builds_total metrics

(Discontinuous — commits 57–60, 117.)

## Files

```json files
[
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "build_duration_seconds histogram at terminal transition (via submitted_at:Instant); cleanup_dropped_total on try_send fail; cache_check_failures_total on FindMissingPaths error; builds_total moved to terminal with outcome label"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "BuildInfo.submitted_at: Instant"},
  {"path": "rio-store/src/backend/s3.rs", "action": "MODIFY", "note": "s3_requests_total{operation=put_object|get_object|delete_object|head_object}"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "build_duration_seconds via scopeguard (fires on all paths); daemon.wait() timeout/fail logged; explicit StderrMessage variant match (no catch-all)"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "fuse_fetch_duration_seconds around GetPath stream loop; readlink preserves actual errno (was: always EINVAL)"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "builds_total at CompletionReport with outcome={success|failure}"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "log resolve_derivation failure in wopQueryMissing"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "document cleanup_dropped_total + cache_check_failures_total with alert guidance"}
]
```

## Design

**4 missing metrics:** `rio_scheduler_build_duration_seconds` (histogram, recorded at `transition_build` terminal), `rio_store_s3_requests_total` (counter per SDK call), `rio_worker_build_duration_seconds` (histogram via scopeguard — fires on success/error/panic), `rio_worker_fuse_fetch_duration_seconds` (histogram around `GetPath` loop).

**Outcome labels:** removed the unlabeled submission-time increment. Scheduler: `complete_build` → `outcome="succeeded"`, `transition_build_to_failed` → `"failed"`, `handle_cancel_build` → `"cancelled"`. Worker: at `CompletionReport` construction, `proto status == Built` → `"success"`, else `"failure"`. Total = `sum by () (rio_*_builds_total)` in PromQL.

**Cleanup drop counter:** `CleanupTerminalBuild` uses `try_send` (can't block the delayed-spawn task). Under sustained load, these drop → unbounded memory growth (P0039's fix defeated). `rio_scheduler_cleanup_dropped_total` makes this visible; alert guidance in docs.

**Cache-check failure counter:** store unreachable → every submission treated as 100% cache miss → rebuild avalanche. `rio_scheduler_cache_check_failures_total` makes the systemic issue visible. `TODO(phase2c)` for circuit breaker.

**Diagnostic fixes (`be15769`):** bundled small items — readlink preserves errno, daemon zombie-wait logged, explicit `StderrMessage` match (no `_ => {}`), unknown `BuildResultStatus` fallback logged.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Metric definitions later retro-tagged in `docs/src/observability.md`.

## Outcome

Merged as `2dcbcfb..be15769` + `0748563` (5 commits, discontinuous). observability.md spec now matches implementation. SLIs computable.
