# Plan 0031: Worker executor — FUSE + overlay + nix-daemon subprocess

## Context

The worker is where builds actually run. It's the productionization of P0007's spike: a FUSE mount serving `/nix/store` from the remote store's NARs (lazy-fetched on lookup), an overlayfs on top for build scratch space, a synthetic SQLite database so `nix-daemon` believes the store is local, and `nix-daemon --stdio` running the actual build via the client protocol (P0011).

P0007 proved this chain works on the target kernel (AL2023, EKS). This plan turns the spike into a gRPC service that receives `WorkAssignment`s, executes builds, uploads outputs, and reports completions.

**Shared commit with P0030:** `395e826` landed gateway AND worker together. This plan covers the `rio-worker/` half. `ba98d17` adds the in-process integration test.

## Commits

- `395e826` — feat(rio-build): implement gateway refactor and worker executor (worker half)
- `ba98d17` — test(rio-gateway): add distributed integration test and mark Phase 2a complete

## Files

```json files
[
  {"path": "rio-worker/src/executor.rs", "action": "NEW", "note": "execute_build: overlay setup → synth DB → spawn nix-daemon --stdio in CLONE_NEWNS → client protocol (wopBuildDerivation) → upload outputs → CompletionReport"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "NEW", "note": "NixStoreFs: fuser 0.17 Filesystem impl, passthrough mode, multi-threaded (n_threads)"},
  {"path": "rio-worker/src/fuse/lookup.rs", "action": "NEW", "note": "lookup(): top-level store path → QueryPathInfo gRPC (existence check) → synthetic dir attr (BUG — should materialize, fixed P0056)"},
  {"path": "rio-worker/src/fuse/read.rs", "action": "NEW", "note": "read/open: passthrough to local cache_dir file descriptors"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "NEW", "note": "LRU cache with SQLite index: insert, contains, eviction by last_access; NAR extraction to directory trees"},
  {"path": "rio-worker/src/overlay.rs", "action": "NEW", "note": "OverlayMount RAII: per-build upper/work/merged; mount(2) with lowerdir=FUSE; teardown on Drop"},
  {"path": "rio-worker/src/synth_db.rs", "action": "NEW", "note": "synthetic Nix SQLite (schema v10): ValidPaths + Refs from store QueryPathInfo; generated per-build"},
  {"path": "rio-worker/src/upload.rs", "action": "NEW", "note": "scan overlay upper for new store paths, NAR-serialize each, PutPath with retry"},
  {"path": "rio-worker/src/log_stream.rs", "action": "NEW", "note": "BuildLogBatch: 64-line / 100ms batching for log forwarding (phase 2b wires this to scheduler)"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "open BuildExecution stream, send WorkerRegister (NOT YET — bug fixed P0033), heartbeat loop, Semaphore for concurrent builds"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "NEW", "note": "in-process mock store + mock scheduler; handshake, setOptions, QueryValidPaths, IsValidPath, QueryPathInfo, NarFromPath via gRPC stack; FUSE tests #[ignore]"}
]
```

## Design

**FUSE daemon (`NixStoreFs`):** mounted at a worker-local path (e.g., `/var/lib/rio/fuse`). `lookup(parent=ROOT, name)` for a store basename checks: (1) local cache dir — if extracted there, return real attrs; (2) remote store via `QueryPathInfo` — if exists, return synthetic dir attr with 1h TTL (the NAR fetch happens lazily on first `read`/`readdir`). InodeMap assigns inode numbers. FUSE callbacks are sync; gRPC calls inside them use `Handle::block_on`. **No `forget()` impl** — inode map grows forever (fixed P0043). **Root is `mount_point`** — wrong, causes recursive deadlock when VM-mounted (fixed P0055).

**Cache:** extracted NARs live in `cache_dir` as real directory trees. SQLite index tracks `(store_path, local_path, size_bytes, last_access)`. Eviction when `total_size > max` removes LRU entries (both file and index row). Concurrent fetches of the same path: sleep-backoff polling (50ms → 500ms, ~1.4s — replaced with condvar in P0036).

**Overlay:** `setup_overlay` creates per-build `upper/`, `work/`, `merged/` under `overlay_base_dir`. `mount -t overlay -o lowerdir={FUSE},upperdir={upper},workdir={work} none {merged}`. The merged dir is what the nix-daemon sees as `/nix/store`. `OverlayMount`'s `Drop` runs `umount2(merged, MNT_DETACH)` — best-effort. Teardown failures are silent (metric added P0051).

**nix-daemon subprocess:** `spawn_daemon_in_namespace` — `Command::new("nix-daemon").arg("--stdio")` with `pre_exec` that calls `unshare(CLONE_NEWNS)` then sets `NIX_STORE_DIR={merged}` (BUG — derivations hardcode `/nix/store`, env approach doesn't work, fixed P0052). Client protocol over stdin/stdout: handshake, `wopSetOptions`, `wopBuildDerivation` with `BasicDerivation` (from P0011). Read STDERR loop for build logs + terminal `BuildResult`.

**Upload:** after build, `scan_new_outputs` walks `overlay_upper` for store-path-shaped directories. Each gets NAR-serialized (P0009) and `PutPath`'d. Sequential — parallelized in P0036.

**Concurrency:** `Semaphore` gates concurrent builds at `max_builds`. Each build runs in its own `spawn`'d task. Heartbeat loop runs separately, reporting `running_builds` (empty in this commit — tracker added P0034).

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Worker spec text in `docs/src/components/worker.md` was later retro-tagged with `r[worker.fuse.*]`, `r[worker.overlay.*]`, `r[worker.synthdb.*]`, `r[worker.executor.daemon]` markers.

## Outcome

Merged as `395e826..ba98d17` (first commit shared with P0030). 423 tests total, 1 skipped (`#[ignore]`'d FUSE integration — needs `CAP_SYS_ADMIN`). The phase doc marks all tasks complete at this point — prematurely. P0033 discovers the worker-registration bug that makes the whole distributed path non-functional under mocked tests that don't actually dispatch.
