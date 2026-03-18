# Plan 0174: Scheduler token-aware shutdown — profraw flush on SIGTERM

## Design

Found during P0173's lcov analysis: **`collectCoverage`'s `systemctl stop` times out when worker streams are active → SIGKILL → no atexit → no profraw.** Every test that ran builds showed 0/360 coverage for `rio-scheduler/src/actor/mod.rs`. Only the old `phase1a` (idle scheduler, no builds) ever collected it.

**Root cause (`a5b06ef`):** `serve_with_shutdown` deadlocks on open `BuildExecution` bidi streams. The `output_tx` sender lives in the build-exec-bridge task, which exits when `actor_rx` closes, which closes when all `ActorHandle` senders drop — but `SchedulerGrpc` holds a sender and is itself held by the server's handler registry inside `serve_with_shutdown`. **Circular wait** → `systemctl stop` times out → SIGKILL → no atexit → no LLVM profraw.

**Fix:** thread the shutdown token into `DagActor` via a `with_shutdown_token` builder (default never-cancelled, test callsites unchanged). `run_inner` `select!`s on `token.cancelled()` with `biased` ordering so SIGTERM drains workers (drops all `stream_tx`) immediately, cascading the bridge exit → `ReceiverStream` close → `serve_with_shutdown` returns.

Observable: lcov for `actor/mod.rs` went from 0/360 to normal. Also fixes the production k8s rollout case where scheduler pods were waiting out the full 30s termination grace period.

**`common.nix` (`57180ec`):** stop workers before scheduler in `collectCoverage` — workers reconnecting to a dying scheduler would re-open streams.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "with_shutdown_token builder; run_inner select! on token.cancelled() biased"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "pass shutdown.clone() to spawn_with_leader"},
  {"path": "rio-scheduler/src/actor/tests/mod.rs", "action": "MODIFY", "note": "unit test for token-aware shutdown drain"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "collectCoverage: stop workers before scheduler"}
]
```

## Tracey

No new markers. Fixes a coverage-collection infrastructure bug.

## Entry

- Depends on P0165: scheduler step_down (same shutdown path)

## Exit

Merged as `a5b06ef`, `57180ec`, `7326f6d` (3 commits). `.#ci` green. `actor/mod.rs` coverage collected for the first time in any build-running test.
