# Plan 0072: Pre-bulk fixes — FUSE TOCTOU, overlay-leak escalation, log BATCH_TIMEOUT

## Design

Three worker-side fixes the phase doc calls "pre-bulk cleanup" — they had to land before the feature bulk because the log pipeline (P0078) and VM test (P0081) depend on correct worker behavior.

**FUSE eviction TOCTOU** (`a26c51a`): `get_and_touch()` stamps `last_access=now` and returns a path; a concurrent `evict_if_needed()` (called after every fetch) could then select that just-touched entry as the LRU victim and delete it from disk before the FUSE caller's `symlink_metadata()`/`open()` ran. The fallback at `ops.rs:207` covered `getattr` but NOT `lookup` (which replied `ENOENT` directly). A race between "this path exists, here it is" and "evicted, gone" — classic TOCTOU.

Fix: since `get_and_touch()` stamps `last_access` *before* returning, any entry with `last_access >= now - EVICT_GRACE_SECS` (5s) is either in active use or about to be. Filter it out of the eviction SELECT — pure SQL filter, zero syscalls added to the hot path. When total > max but all entries are within grace: break with `debug!` (transient, retry on next insert) vs the existing `warn!` (true accounting drift). Distinguished by `COUNT(*) WHERE last_access >= grace_cutoff`.

**Overlay-leak escalation** (`bb2e7e6`): a leaked mount (`umount2` fails in `OverlayMount::Drop`) usually means the mount is stuck busy — open file handles, zombie `nix-daemon`, FUSE hang. Previously we incremented `rio_worker_overlay_teardown_failures_total` and logged, but kept accepting builds into a degraded environment.

Now `execute_build` checks an `Arc<AtomicUsize>` at **entry**. After `RIO_WORKER_MAX_LEAKED_MOUNTS` leaks (default 3), it returns `Ok(ExecutionResult{InfrastructureFailure})` — `CompletionReport` is sent, the scheduler reassigns to a healthy worker. The check is at entry, not exit: a successful build's result isn't overridden because its own teardown later fails; the *next* build is what gets refused. Counter increment lives in `OverlayMount::Drop` alongside the metric (centralized so `?`-early-returns and panics both count).

**Log `BATCH_TIMEOUT` during silent periods** (`f3b9efb`): `observability.md` guarantees "64 lines or 100ms, whichever comes first." Previously `maybe_flush()` only fired once per stderr message, so a build that went silent for 60s (common: long compile buffering stdout) held a partial batch until the next `STDERR_NEXT` arrived. The gateway saw nothing; the build appeared hung.

The obvious fix — `tokio::time::timeout` around `read_stderr_message` — is **cancel-unsafe**: dropping the read future mid-u64-read leaves partial bytes consumed from the daemon's stdout pipe, desyncing the Nix STDERR protocol on the next read. Fix: spawn the reader into an owned task that pushes each parsed `StderrMessage` onto an mpsc channel, then `select!` on `rx.recv()` + a `BATCH_TIMEOUT` interval. Only `recv()` is cancelled on tick, and mpsc recv is cancel-safe. The reader task returns the reader on exit so the main function can still read `BuildResult` after `STDERR_LAST`. The tick arm uses `has_pending() + flush()` directly (not `maybe_flush()`) because `maybe_flush()` checks `std::time::Instant`, which doesn't advance under `tokio::time::pause()` in tests.

## Files

```json files
[
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "EVICT_GRACE_SECS SQL filter; debug! vs warn! distinction via COUNT(*)"},
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "Arc<AtomicUsize> leak counter in Drop; cfg(test) ctor for failure injection"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "leak-counter entry check → InfrastructureFailure; stderr loop rework call site"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "spawn reader into owned task, mpsc channel, select! on rx.recv() + BATCH_TIMEOUT interval"},
  {"path": "rio-worker/src/log_stream.rs", "action": "MODIFY", "note": "has_pending() helper; BATCH_TIMEOUT exported for interval use"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "RIO_WORKER_MAX_LEAKED_MOUNTS config; leak_counter plumbing"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "leak counter init + threshold plumbing"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[wk.fuse.evict]` (TOCTOU fix), `r[wk.overlay.leak]` (escalation), `r[wk.log.batch-timeout]` (cancel-safe timeout).

## Entry

- Depends on **P0056** (phase-2a terminal): `fuse/cache.rs`, `overlay.rs`, `log_stream.rs` (LogBatcher) are all 2a artifacts.
- Depends on **P0066** (module splits): `executor/daemon.rs` and `executor/mod.rs` were created by the split in P0066.

## Exit

Merged as `a26c51a..f3b9efb` (3 commits). `.#ci` green at merge. Tests: `test_leak_counter_increments_on_drop_failure` (cfg(test) ctor with `mounted=true` + nonexistent path → umount2 fails → counter++), `test_leak_counter_no_increment_when_unmounted`.
