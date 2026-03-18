# Plan 0142: Graceful shutdown via CancellationToken + FUSE Drop cleanup

## Design

All five binaries' `main()` ended with a bare `.await` on the server future. SIGTERM → default handler → immediate process termination. No `atexit` handlers ran — including LLVM's coverage profraw flush. VM-test coverage (P0143) needed `main()` to RETURN so `llvm_profile_write_file` fires. This plan threaded graceful shutdown through every binary.

**`rio-common::signal::shutdown_signal()` (`2fe5fb5`):** returns a `CancellationToken` that fires on SIGTERM or SIGINT. The signal handlers are installed SYNCHRONOUSLY before spawning the wait task — eliminating the race where a signal arrives between `return token` and the spawned task's first poll (default handler still installed → process dies). `tokio::signal::unix::signal()` registers the handler on construction.

**rio-store (`97cd8e0`):** `Server::serve_with_shutdown(addr, token.cancelled())`. Orphan scanner and drain task loops get `select! { _ = token.cancelled() => break, _ = interval.tick() => ... }`. In-flight gRPC calls drain naturally — `serve_with_shutdown` stops accepting NEW connections but lets existing ones finish.

**rio-scheduler (`4711d74`):** Same `serve_with_shutdown` pattern. The lease loop, tick loop, and health loop all get token-select. `LeaderAcquired` handler is NOT cancelled mid-recovery — recovery either completes or the process exits (actor task itself isn't select!-wrapped; it drains the mpsc channel naturally when the server drops the handle).

**rio-gateway (`e17b94f`):** `select! { _ = server.run() => ..., _ = token.cancelled() => ... }`. Health server also shut down. The SSH server doesn't have a graceful-drain API, so this is best-effort — in-flight nix clients will see EOF.

**FUSE shutdown saga (`19419a1..ed6e9a0`, 30 commits later):** The worker's FUSE `BackgroundSession` doesn't clean up its mount on `Drop` — only `Mount` does, and `BackgroundSession` owns a `Mount` internally but doesn't expose it. Initial fix: `umount_and_join()` with 5s timeout. Second fix: detached `std::thread` for umount (not `spawn_blocking` — tokio runtime is shutting down). Final fix: REVERT to the original Drop-based approach + `std::thread::sleep(200ms)` before process exit. The 200ms gives the kernel's async umount + LLVM's profraw write enough time. Ugly but reliable — every explicit-umount approach raced with profraw flush in some way.

## Files

```json files
[
  {"path": "rio-common/src/signal.rs", "action": "NEW", "note": "shutdown_signal() CancellationToken — handlers installed sync before spawn"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "pub mod signal"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "tokio-util for CancellationToken"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "serve_with_shutdown(token.cancelled())"},
  {"path": "rio-store/src/gc/drain.rs", "action": "MODIFY", "note": "select! on token in loop"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "select! on token in loop"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "serve_with_shutdown + thread token to lease/tick/health"},
  {"path": "rio-scheduler/src/lease.rs", "action": "MODIFY", "note": "select! on token in renew loop"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "select! on server.run() + token + health shutdown"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "FUSE saga: timeout → detached thread → revert to Drop + 200ms sleep"}
]
```

## Tracey

No tracey markers landed. Graceful shutdown is infrastructure enabling coverage collection — not a spec-tracked feature.

## Entry

- Depends on P0127: phase 3a complete (all binaries' `main.rs`, server patterns, background task patterns exist).

## Exit

Merged as `2fe5fb5..e17b94f` + `19419a1..ed6e9a0` (7 commits, non-contiguous, gap=30). `.#ci` green at merge. Enables P0143's profraw collection — `systemctl stop rio-*` now produces valid profraw files in `/var/lib/rio/cov/`.
