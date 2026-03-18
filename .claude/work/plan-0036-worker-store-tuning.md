# Plan 0036: Worker/store tuning — overlay fs check, S3 config, parallel upload, FUSE condvar

## Context

Five small-but-sharp improvements from the second review pass. None critical, all quality-of-life for operators and perf.

**Overlay fs pre-check:** the kernel rejects overlayfs when FUSE lower and upper are on the same filesystem, but the error is `EINVAL` — cryptic. A `st_dev` comparison before `mount(2)` gives a clear message pointing at `--overlay-base-dir`.

**S3 retry/timeout CLI:** the SDK defaults (3 retries, no attempt timeout) were implicit. Now `--s3-max-retries` and `--s3-attempt-timeout-secs` are explicit and tunable.

**Parallel uploads:** `upload_all_outputs` was sequential. Now `buffer_unordered(4)` — up to 4 concurrent PutPath streams. Memory bound: ~4 × max NAR size. Also fixed a pre-existing ordering bug where `built_outputs` zip assumed upload order matched `output_names` (but `read_dir` is unordered).

**FUSE condvar:** when two FUSE threads request the same uncached path, the waiter now blocks on a per-path `Condvar` and wakes immediately when the fetcher completes (or panics — `Drop` notifies). Replaced the sleep-backoff poll (~1.4s worst case) with instant wakeup.

**DAG encapsulation:** `nodes`/`children`/`parents` made private with accessor methods. Pure hygiene.

## Commits

- `ccb67d6` — feat(rio-worker): validate overlay upper/lower are on different filesystems
- `a1cdfe0` — feat(rio-store): make S3 retry/timeout config explicit and CLI-tunable
- `4904d5f` — perf(rio-worker): parallelize output uploads with bounded concurrency
- `decee28` — refactor(rio-worker): replace FUSE fetch sleep-backoff with condvar wait/notify
- `5dd1a87` — refactor(rio-scheduler): encapsulate DerivationDag fields behind accessors

## Files

```json files
[
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "stat().st_dev comparison on upper vs lower before mount; clear error on same-fs"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "--s3-max-retries (default 5), --s3-attempt-timeout-secs (default 30); wired into aws_config RetryConfig + TimeoutConfig"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "buffer_unordered(4); sort scan output for determinism"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "replace positional built_outputs zip with store_path→output_name HashMap lookup"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "InflightEntry{done,cv}; FetchClaim::{Fetch,WaitFor}; FetchGuard with Drop that removes from inflight map + notify_all"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "ensure_cached matches on FetchClaim; 30s timeout as defense against stuck fetcher"},
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "nodes/children/parents private; node(), node_mut(), contains(), iter_nodes(), node_count() accessors"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "32 call sites mechanically updated to accessors"}
]
```

## Design

**st_dev not f_fsid:** `statvfs().f_fsid` is documented as meaningless on Linux. `stat().st_dev` is the actual device number.

**Parallel upload ordering fix:** `scan_new_outputs` uses `read_dir`, which is unordered. The old code zipped `built_outputs` positionally with `assignment.output_names` — wrong if output order differed. Now: build a `HashMap<StorePath, OutputName>` from the parsed derivation, look up each upload's path. Bonus: `scan_new_outputs` sorts its output for deterministic test behavior.

**Condvar pattern:** `FetchClaim::Fetch(FetchGuard)` — this thread fetches; the guard's `Drop` removes the inflight entry and `notify_all`s (fires on panic too). `FetchClaim::WaitFor(Arc<InflightEntry>)` — this thread waits on `entry.cv.wait_timeout(30s)`. After wakeup: re-check `contains()` (the fetcher may have failed; `done` flag distinguishes fetch-finished from timeout).

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[worker.overlay.fs-check]`, `r[worker.upload.parallel]`, `r[worker.fuse.fetch-condvar]`.

## Outcome

Merged as `ccb67d6..5dd1a87` (5 commits). Concurrent-wait test asserts both threads finish in ~200ms (not 1.4s). Timeout test verifies `wait()` returns false when guard is never dropped. `futures-util` added to workspace deps.
