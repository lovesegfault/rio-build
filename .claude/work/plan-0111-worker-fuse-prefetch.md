# Plan 0111: Worker FUSE prefetch — singleflight-shared hint pipeline

## Design

FUSE is lazy by design: the worker's `/nix/store` mount fetches paths on first `lookup()`. Good for memory, bad for latency — the first `gcc` invocation in a build blocks on a gRPC round-trip + NAR extraction. Three commits added a prefetch pipeline: scheduler sends `PrefetchHint` before `WorkAssignment` with paths the build will probably need; worker warms them via `spawn_blocking` while still parsing the `.drv`.

`84d8d35` was prep refactor: `Cache` became `Arc<Cache>` so `main.rs` can clone it before moving into `mount_fuse_background` (same extract-before-move pattern as `bloom_handle`). `fetch_and_extract` became a free fn `fetch_extract_insert(&Cache, &StoreServiceClient, &Handle, basename)` instead of `&self` — prefetch can call it without a `NixStoreFs` (which is consumed by `fuser::spawn_mount2`). `prefetch_path_blocking` is **sync**, called via `spawn_blocking`. `Cache` methods use `runtime.block_on` internally (designed for FUSE threads); an async caller would nested-runtime panic — the architecture review caught this in planning. Singleflight **shared** with `ensure_cached` (same `Cache.inflight` map) but `WaitFor` semantics differ: `ensure_cached` (FUSE) waits on condvar — the build needs this path, blocking the FUSE thread is correct. `prefetch` returns **early** on `WaitFor` — prefetch is a hint; if FUSE or another prefetch has it in flight, we're done. Waiting would hold a blocking-pool thread + semaphore permit for something already happening.

`6e5f1b7` replaced the stub at the `main.rs` Prefetch arm. One `tokio::spawn` per path, `spawn_blocking` wraps `prefetch_path_blocking`. `prefetch_sem = Semaphore(8)`, **separate** from `build_semaphore` — prefetch is opportunistic warming, not build capacity. A `max_builds=1` worker prefetching serially defeats "get ahead." Permit acquired before `spawn_blocking`: if saturated, task waits cheaply in async scheduler, not holding a blocking thread. Fire-and-forget: no `JoinHandle` tracking. SIGTERM mid-prefetch → tasks abort with runtime, partial fetch in `.tmp-XXXX` sibling (cleaned on next cache init).

`9c7a10f` wired the scheduler side. `approx_input_closure` extracted from `best_worker` (DAG children's `expected_output_paths`) — both scoring and hinting call it. If scoring picks a warm worker, the hint should be small (only what's missing); inconsistent approximations would mean scoring on one set, hinting from another. `send_prefetch_hint` in `dispatch.rs` is called BEFORE building the `WorkAssignment`. Bloom-filtered against `worker.bloom`: skip paths the worker probably has. Bloom false positives → skip a hint we should send → build fetches on-demand (missed optimization, not broken). False negatives impossible. `MAX_PREFETCH_PATHS=100` cap (200 deps × 3 outputs × 80 bytes = 48KB is fine but bound pathological cases). Empty after filter → don't send — common case: `best_worker` picked a warm worker, exactly what bloom-locality scoring is for. `try_send`, failure at debug: hint not contract.

## Files

```json files
[
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "fetch_and_extract → free fn fetch_extract_insert; prefetch_path_blocking sync fn; PrefetchSkip enum (AlreadyCached vs AlreadyInFlight)"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "Cache: Arc; fetch module pub (was mod-private)"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "Prefetch arm: spawn_blocking wrapper; prefetch_sem = Semaphore(8) separate from build_semaphore"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "send_prefetch_hint BEFORE WorkAssignment; bloom-filtered; MAX_PREFETCH_PATHS=100"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "approx_input_closure extracted from best_worker; shared between scoring and hinting"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "PrefetchHint message"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[worker.prefetch.singleflight]`, `r[worker.prefetch.spawn-blocking]`, `r[sched.prefetch.bloom-filter]`, `r[sched.prefetch.before-assignment]`.

## Entry

- Depends on P0096: phase 2c complete — FUSE cache + bloom filter existed.
- Depends on P0101: the `runtime.rs` extraction made the Prefetch arm land in stable code.
- Depends on P0007: the FUSE spike established the `Cache` singleflight pattern this extends.

## Exit

Merged as `84d8d35..9c7a10f` (3 commits). `.#ci` green at merge. 846 tests unchanged (84d8d35 was behavior-neutral refactor). Scheduler tests: `hint_before_assignment` (proves ordering), `bloom_filters` (2-child parent, bloom claims one → hint only other; ORDER MATTERS: heartbeat with bloom BEFORE second completion), `skipped_when_bloom_covers_all`. Metrics: `rio_scheduler_prefetch_hints_sent_total`, `rio_scheduler_prefetch_paths_sent_total`.
