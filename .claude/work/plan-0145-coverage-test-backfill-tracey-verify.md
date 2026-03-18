# Plan 0145: Coverage test backfill + tracey verify annotations

## Design

`.#coverage-full` (P0143) produced its first report: 78% unit + 8% VM-only = ~86% combined. The gap was testable-but-untested code — not VM-exclusive, just never covered. This plan closed the biggest gaps, adding ~40 tests and wiring 16 `r[verify]` annotations for spec rules that had `r[impl]` but no test.

**Phase 9 — Controller + gateway pure-function tests:** `apply_event_cancelled_sets_condition`, `apply_event_progress_bumps_pending_to_building` (controller build reconciler state machine); `first_event_cancelled_short_circuit`, `empty_event_stream_failure` (gateway build handler edge cases).

**Phase 10 — MockScheduler stream-error injection:** `rio-test-support/src/grpc.rs` extended: `MockSchedulerOutcome.error_after_n`, `watch_scripted_events`, `watch_fail_count`. `submit_build` now sends `Err(Status)` after N events; `watch_build` supports scripted event sequences + controlled failure count. This unblocks reconnect testing.

**Phase 11 — Gateway reconnect loop tests (`r[verify gw.reconnect.backoff]`):** `test_build_paths_reconnect_on_transport_error` (stream errors after 1 event, `WatchBuild` succeeds with completion → build succeeds); `test_build_paths_reconnect_exhausted_returns_failure` (WatchBuild fails 6× > MAX_RECONNECT → MiscFailure).

**Phase 12 — FUSE fetch error-path tests:** `MockStore.fail_get_path`, `get_path_garbage` fault-injection flags. Tests: `fetch_extract_insert_success_roundtrip` (via `prefetch_path_blocking`), `not_found_returns_enoent`, `store_unavailable_returns_eio`, `nar_parse_error_returns_eio`. `Errno` comparison via `i32::from` (no PartialEq).

**Phase 13 — Prior-phase verify backfill:** 9 tracey rules had `r[impl]` (from phases 1b-2c) but no `r[verify]`. Added: `sched.merge.dedup`, `sched.build.keep-going`, `sched.critical-path.incremental`, `sched.estimate.ema-alpha`, `store.put.wal-manifest`, `store.inline.threshold`, `store.put.idempotent`, `worker.daemon.timeout-wrap`. Plus `sched.backpressure.hysteresis` and `sched.recovery.gate-dispatch` from phase 3a/3b. Each required either a new test or annotation on an existing test that already verified the behavior.

**FUSE readdir + destroy (`d07a52b`):** VM test exercises `readdir` (never called before — ls /store in VM) + discovered `destroy()` callback was NEVER running (BackgroundSession didn't call it on Drop). Fixed worker main.rs.

**GC `decrement_and_enqueue` coverage (`86e8a5c`):** the helper extracted in P0138 had no direct tests. Added 2 (`r[verify store.chunk.refcount-txn]` ×2).

**NAR parser bounds-check tests (`ddabe8b`):** malformed-input coverage for the phase-1a NAR parser — truncated length prefixes, bad magic, oversized entries.

**gRPC validation + metadata query tests (`a0f33af`):** store's `find_paths_by_prefix`, realisation queries.

**PutPath HMAC tests (`3852159`, `r[verify sec.boundary.grpc-hmac]`):** dedicated `tests/grpc/hmac.rs` — trailer verification, token-path mismatch, expiry.

**Scheduler HMAC dispatch + GcRoots + backpressure + completion edge (`a963f93`):** 4 tests, 3 verify annotations.

**Recovery seeding + worker backstop + gRPC stream + admin (`8f134d3`):** `r[verify sched.backstop.timeout]`.

## Files

```json files
[
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockScheduler error_after_n + watch_scripted_events + MockStore fault injection"},
  {"path": "rio-store/tests/grpc/hmac.rs", "action": "NEW", "note": "PutPath HMAC verification tests"},
  {"path": "rio-store/tests/grpc/realisations.rs", "action": "NEW", "note": "realisation query tests"},
  {"path": "rio-store/tests/grpc/trailer.rs", "action": "MODIFY", "note": "HMAC trailer tests"},
  {"path": "rio-store/tests/grpc/main.rs", "action": "MODIFY", "note": "mod hmac + mod realisations"},
  {"path": "rio-store/tests/grpc/core.rs", "action": "MODIFY", "note": "r[verify store.put.idempotent] + store.put.wal-manifest"},
  {"path": "rio-store/tests/grpc/chunked.rs", "action": "MODIFY", "note": "r[verify store.inline.threshold]"},
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "test module exposure"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "r[verify store.chunk.refcount-txn] tests"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "test helper"},
  {"path": "rio-store/src/metadata/queries.rs", "action": "MODIFY", "note": "find_paths_by_prefix test"},
  {"path": "rio-scheduler/src/actor/tests/misc.rs", "action": "NEW", "note": "HMAC dispatch + GcRoots + backpressure + completion edge"},
  {"path": "rio-scheduler/src/actor/tests/mod.rs", "action": "MODIFY", "note": "mod misc"},
  {"path": "rio-scheduler/src/actor/tests/build.rs", "action": "MODIFY", "note": "r[verify sched.recovery.gate-dispatch]"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "completion edge tests"},
  {"path": "rio-scheduler/src/actor/tests/fault.rs", "action": "MODIFY", "note": "fault injection"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "recovery seeding tests"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "r[verify sched.backstop.timeout]"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "r[verify sched.merge.dedup]"},
  {"path": "rio-scheduler/src/actor/tests/keep_going.rs", "action": "MODIFY", "note": "r[verify sched.build.keep-going]"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "test exposure"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "test helpers"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "test helpers"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "admin RPC tests"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "stream tests"},
  {"path": "rio-scheduler/src/critical_path.rs", "action": "MODIFY", "note": "r[verify sched.critical-path.incremental]"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "r[verify sched.estimate.ema-alpha]"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "apply_event tests"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "first_event_cancelled + reconnect tests + r[verify gw.reconnect.backoff]"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "fetch error-path tests"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "r[verify worker.daemon.timeout-wrap]"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "fix FUSE destroy() not running"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "bounds-check + malformed-input tests"},
  {"path": "nix/tests/phase2b-derivation.nix", "action": "MODIFY", "note": "FUSE readdir exercise (ls /store)"}
]
```

## Tracey

16 `r[verify]` markers added across 5 commits:
- `86e8a5c`: `r[verify store.chunk.refcount-txn]` ×2 (sweep/orphan decrement tests)
- `3852159`: `r[verify sec.boundary.grpc-hmac]` (dedicated hmac.rs suite)
- `a963f93`: `r[verify sched.recovery.gate-dispatch]`, `r[verify sec.boundary.grpc-hmac]` (additional), `r[verify sched.backpressure.hysteresis]`
- `8f134d3`: `r[verify sched.backstop.timeout]`
- `8950529`: `r[verify gw.reconnect.backoff]`, `r[verify sched.merge.dedup]`, `r[verify sched.build.keep-going]`, `r[verify sched.critical-path.incremental]`, `r[verify sched.estimate.ema-alpha]`, `r[verify store.put.wal-manifest]`, `r[verify store.inline.threshold]`, `r[verify store.put.idempotent]`, `r[verify worker.daemon.timeout-wrap]`

9 of these are prior-phase backfill — `impl` existed since phases 1b-2c, `verify` added here.

## Entry

- Depends on P0143: `.#coverage-full` report identifies gaps.

## Exit

Merged as `d07a52b..8950529` (8 commits). `.#ci` green at merge. ~40 new tests. Coverage rises from ~86% to ~91% combined.
