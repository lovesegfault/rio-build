# Plan 0195: Rem-16 — FUSE waiter-loop + fetch-semaphore + self-heal stat (§2.10 timeout arithmetic)

## Design

**P1 (HIGH).** Three independent fixes for wrong "cache miss is rare/fast/consistent" assumption.

**SELF-HEAL (`fuse-index-disk-divergence`):** `ensure_cached` fast path now stats the index-returned path. `ENOENT` → purge stale row + metric + fall through to re-fetch. Previously: an external `rm` (debugging, interrupted eviction) left the path permanently unfetchable — every call returned `Ok(path-that-does-not-exist)`. New `Cache::remove_stale`, `rio_worker_fuse_index_divergence_total`.

**LOOP-WAIT (`fuse-wait-timeout-vs-stream-timeout`):** `WaitFor` arm now loops `wait(30s slices)` up to `GRPC_STREAM_TIMEOUT+30s` instead of giving up at 30s. A 1GiB NAR @ 15MB/s (~70s) previously `EAGAIN`ed waiters at T+30s while the fetcher was still healthy. `FetchGuard::drop` already fires on success/error/panic; slice timeout means "slow", not "dead". New `InflightEntry::is_done`, `wait_for_fetcher`. `WAIT_SLICE=200ms` under `cfg(test)`.

**SEMAPHORE (`fuse-blockon-thread-exhaustion`):** `NixStoreFs` now bounds FUSE-initiated fetches to `fuse_threads-1` (min 1) via Mutex+Condvar semaphore. Without this, N distinct cold paths can saturate the FUSE pool with `block_on` for up to 300s each, starving warm-path ops that would complete in microseconds. Acquired AFTER ensure_cached fast path (so warm hits never block).

Remediation doc: `docs/src/remediations/phase4a/16-fuse-waiter-timeout.md` (787 lines).

## Files

```json files
[
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "ensure_cached stats index path; ENOENT → remove_stale + re-fetch; InflightEntry::is_done"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "wait_for_fetcher loops 30s slices up to GRPC_STREAM_TIMEOUT+30s; fetch-semaphore fuse_threads-1"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "semaphore wiring in NixStoreFs"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "rio_worker_fuse_index_divergence_total metric"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore delay injection for waiter tests"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "new metric"}
]
```

## Tracey

- `r[verify worker.fuse.lookup-caches]` — `6a8174e` (waiters don't spuriously fail)
- `r[verify worker.fuse.cache-lru]` — `6a8174e` (index self-heals on disk divergence)

2 marker annotations.

## Entry

- Depends on P0148: phase 3b complete
- Depends on P0111: phase 3a FUSE prefetch (extends `ensure_cached`)

## Exit

Merged as `040ae23` (plan doc) + `6a8174e`, `1d6127a` (2 fix commits). `.#ci` green. 1GiB NAR fetch: waiters don't EAGAIN mid-fetch.
