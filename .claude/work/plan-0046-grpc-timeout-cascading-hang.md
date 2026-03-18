# Plan 0046: gRPC timeout helper — cascading-hang prevention across all services

## Context

Missing gRPC timeouts were systemic: gateway→store (5 sites), gateway→scheduler (1), worker→store (3), scheduler actor→store (1), worker FUSE→store (2), store streaming task (1). Every one a hang vector.

The worst: **scheduler actor→store.** `check_cached_outputs` calls `FindMissingPaths` inside the single-threaded actor event loop. Store hangs → entire actor blocks → no heartbeats processed → workers "time out" → builds reassigned → more load on (still-hung) store → cascade.

Second worst: **worker FUSE→store.** `fetch_and_extract` calls `GetPath` inside `Handle::block_on()` from a FUSE callback. Finite FUSE thread pool. A handful of stalled fetches → all FUSE threads blocked → mount frozen → every build on the worker hangs on I/O.

This plan introduces `rio_common::grpc` with three timeout constants and a `with_timeout(name, dur, fut)` helper, then applies them everywhere.

## Commits

- `ec0be0c` — feat(rio-common): add gRPC timeout helper and shared daemon_timeout() reader
- `1d35617` — fix(rio-scheduler): timeout store gRPC call to prevent actor event loop stall
- `5cbcceb` — fix(rio-worker): timeout FUSE gRPC calls to prevent mount deadlock
- `755f497` — fix(workspace): wrap remaining gRPC calls in shared timeouts
- `9935400` — fix(rio-worker): add gRPC timeout to fetch_drv_from_store
- `1b4db75` — fix(rio-gateway): add gRPC timeouts to query_valid_paths and cancel_build on disconnect

(Discontinuous — commits 77–80, 106, 110.)

## Files

```json files
[
  {"path": "rio-common/src/grpc.rs", "action": "NEW", "note": "DEFAULT_GRPC_TIMEOUT=30s (metadata calls), GRPC_STREAM_TIMEOUT=300s (NAR transfer — 4GiB@15MB/s≈270s), DEFAULT_DAEMON_TIMEOUT=2h; with_timeout(name,dur,fut)→anyhow::Error mentioning name on Elapsed; daemon_timeout() OnceLock reads RIO_DAEMON_TIMEOUT_SECS"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "pub mod grpc"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "check_cached_outputs wrapped in DEFAULT_GRPC_TIMEOUT; on Elapsed: warn, increment failure counter, return empty (=100% miss)"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "GetPath wrapped in GRPC_STREAM_TIMEOUT; QueryPathInfo wrapped in DEFAULT_GRPC_TIMEOUT; on Elapsed: error log + EIO"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "fetch_input_metadata QueryPathInfo per-call timeout; compute_input_closure per-BFS-call timeout; fetch_drv_from_store full-body GRPC_STREAM_TIMEOUT; daemon_timeout() from rio-common"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "grpc_query_path_info, grpc_put_path (stream timeout), grpc_get_path, handle_nar_from_path GetPath, SubmitBuild, FindMissingPaths — all wrapped; query_valid_paths FindMissingPaths wrapped"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "cancel_build on disconnect wrapped; timeout logged+continue (best-effort)"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "GetPath streaming task wrapped in GRPC_STREAM_TIMEOUT + spawn_monitored; drain_stream wrapped in DEFAULT_GRPC_TIMEOUT (slowloris defense)"}
]
```

## Design

**Three tiers:** `DEFAULT_GRPC_TIMEOUT=30s` for metadata calls (QueryPathInfo, FindMissingPaths, SubmitBuild). `GRPC_STREAM_TIMEOUT=300s` for NAR transfers — 4GiB at ~15MB/s ≈ 270s, 300s has headroom. `DEFAULT_DAEMON_TIMEOUT=2h` for local nix-daemon build execution (overridable via `RIO_DAEMON_TIMEOUT_SECS`).

**`with_timeout`:** `tokio::time::timeout(dur, fut).await.map_err(|_| anyhow!("{name} timed out after {dur:?}"))?`. The `name` parameter means error messages say "find_missing_paths timed out after 30s", not "timeout elapsed".

**Actor stall fix:** scheduler's `check_cached_outputs` timeout Elapsed → same behavior as gRPC error: warn, `cache_check_failures_total++`, return empty set. Falls back to build-everything. The actor stays responsive.

**FUSE stall fix:** worker FUSE timeout → `EIO`. The build fails with a clear I/O error. Mount stays responsive for other builds.

**Slowloris defense:** `drain_stream` (reads remaining client chunks after a validation error) wrapped in `DEFAULT_GRPC_TIMEOUT`. A slow client can't hold a server handler indefinitely.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[common.grpc.timeout-tiers]`, `r[sched.actor.no-blocking-io]` (the lesson: never unbounded-block inside the actor loop).

## Outcome

Merged as `ec0be0c..755f497` + `9935400`, `1b4db75` (6 commits, discontinuous). 13 gRPC call sites wrapped. Two were missed by the initial sweep (`755f497`), caught in follow-up commits. This pattern became project convention — every cross-service gRPC call should have an explicit timeout.
