# Plan 0194: Rem-15 — Shutdown-signal cluster: worker adopts shutdown_signal(), SIGINT drain, no exit(1) mount leak

## Design

**P1 (HIGH).** Cluster of three shutdown-path bugs sharing root cause: infinite loops without cancellation arm. Touches worker + controller.

**Worker SIGTERM-only handler (`rio-worker/src/main.rs`):** replaced hand-rolled SIGTERM-only handler with `rio_common::signal::shutdown_signal()` (SIGTERM OR SIGINT). Before: Ctrl+C during local dev hit the default SIGINT handler — immediate termination, no Drop, no atexit, FUSE mount leaked, profraw lost. `StreamEnd::Sigterm` → `StreamEnd::Shutdown` (covers both signals).

**Reconnect blind spot:** retry sleeps wrapped in `select!{shutdown.cancelled()=>break}`. Closes the window where scheduler dying first trapped the worker in sleep→continue→await until systemd SIGKILL. The `collectCoverage` Pass-1/Pass-2 ordering in `common.nix` is no longer load-bearing for profraw.

**Heartbeat death `exit(1)` → `bail!`:** `exit()` skips stack unwind — `fuse_session` Drop never runs → `Mount::drop` never runs → `fusermount -u` never runs → next worker start `EBUSY` → CrashLoopBackOff. `bail!` unwinds through `drop(fuse_session)`, then systemd `Restart=on-failure` recovers cleanly.

**Controller (`ea76b0d`):** same pattern in `scaling.rs` — infinite loop with sleep, no cancellation arm.

Remediation doc: `docs/src/remediations/phase4a/15-shutdown-signal-cluster.md` (567 lines). **Partial landing:** the proposed VM test fragment (`# r[verify worker.shutdown.sigint]` in lifecycle.nix) was NOT found in the codebase at phase-4a. `worker.shutdown.sigint` shows UNTESTED in `tracey query untested`.

## Files

```json files
[
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "shutdown_signal() (SIGTERM|SIGINT); StreamEnd::Shutdown; retry sleep select! on cancel; heartbeat bail! not exit(1)"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "same cancellation pattern"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "infinite loop sleep \u2192 select! on cancel"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "r[worker.shutdown.sigint] spec marker"}
]
```

## Tracey

- `r[impl worker.shutdown.sigint]` — `d7af66b` (**UNTESTED at phase-4a** — VM fragment proposed in remediation doc §Test Plan but NOT found in codebase; shows in `tracey query untested`)

1 marker annotation (impl only; verify deferred).

## Entry

- Depends on P0174: scheduler token-aware shutdown (same `shutdown_signal()` adoption pattern)

## Exit

Merged as `62ade73` (plan doc §2.9) + `d7af66b`, `ea76b0d` (2 fix commits). `.#ci` green. **Partial:** VM test fragment not landed; `worker.shutdown.sigint` UNTESTED.
