# Plan 0055: FUSE deadlocks — recursive InodeMap + spawn↔reactor circular wait (VM-test caught)

## Context

Two distinct deadlocks, both latent under unit tests (which never actually mounted FUSE), both triggered the first time the VM test mounted for real. Both VMs hung silent until the scheduler timed them out on missed heartbeats.

**Deadlock 1 — FUSE self-stat:** `NixStoreFs::new` passed `mount_point` to `InodeMap::new`, so `real_path(ROOT)` returned the FUSE mount point. FUSE callbacks like `getattr(ROOT)` and `lookup(ROOT, name)` then did `stat()`/`symlink_metadata()` on paths INSIDE the FUSE mount — re-entering FUSE recursively. With 4 FUSE threads, all 4 blocked waiting for each other. The VM test triggered it via `overlay::setup_overlay`, which stats the FUSE mount as the overlay lower.

**Deadlock 2 — spawn↔reactor circular wait:** `cmd.spawn()` blocks on the child's CLOEXEC error-pipe until exec or `pre_exec`-error. Our `pre_exec` bind-mounts the overlay merged dir at `/nix/store`; the kernel validates the overlay lower (FUSE) with a `getattr`. FUSE threads use `Handle::block_on(gRPC)` — which needs the tokio reactor driven. With both tokio worker threads blocked in `spawn()`, the reactor isn't driven → FUSE hangs → child's `mount()` never returns → parent's `spawn()` never returns. Circular.

## Commits

- `fa973d2` — fix(rio-worker): root FUSE InodeMap at cache_dir not mount_point to prevent recursive deadlock
- `275a02a` — fix(rio-worker): break spawn()↔FUSE↔tokio-reactor deadlock via spawn_blocking

## Files

```json files
[
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "InodeMap root = cache.cache_dir() (was: mount_point); real_path(ino) now resolves to local cache paths — FUSE callbacks do normal fs ops, no recursion"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "spawn_daemon_in_namespace: cmd.spawn() on blocking pool (tokio::task::spawn_blocking) so worker threads stay free to drive reactor; setup_overlay also spawn_blocking; validate bind TARGETS before spawn for clearer errors"},
  {"path": "nix/modules/worker.nix", "action": "MODIFY", "note": "tmpfiles d /nix/var parent dir chain (tmpfiles 'd' doesn't create parents); bump VM cores to 2"},
  {"path": "nix/tests/milestone.nix", "action": "MODIFY", "note": "journal dumps on test failure for debugging"}
]
```

## Design

**Fix 1 — root at `cache_dir`:** `real_path(ino)` now returns paths under `/var/lib/rio/cache/{hash}-name`, not under `/var/lib/rio/fuse/{hash}-name`. FUSE callbacks stat local disk — no recursion. The mount point is purely what the kernel mounts on; the InodeMap never stores it.

**Fix 2 — `spawn_blocking`:** `tokio::task::spawn_blocking(move || cmd.spawn())` runs `spawn()` on the blocking thread pool (512 threads by default, not the 2-ish worker threads). Tokio worker threads stay free to drive the reactor. The child's `pre_exec` bind-mount triggers a FUSE `getattr`, the FUSE thread does `Handle::block_on(gRPC)`, the reactor is driven, the gRPC completes, FUSE replies, `mount()` returns, `pre_exec` returns, `exec` runs, CLOEXEC pipe closes, `spawn()` returns.

`setup_overlay` also wrapped in `spawn_blocking` — same reason: slow mount (FUSE lower stalled on remote fetch) would starve tokio worker threads.

**Why unit tests missed both:** `InodeMap` tests create the map but never mount. `setup_overlay` tests mock the FUSE lower. Nobody ran the full chain with a real mount until the VM test.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[worker.fuse.root-cache-dir]`, `r[worker.executor.spawn-blocking]`. Both are load-bearing invariants — documented in `docs/src/components/worker.md`.

## Outcome

Merged as `fa973d2..275a02a` (2 commits). After these, the VM test progressed from "both workers hang silent" to "builds start but fail" — the next bugs (P0056) are ENOENT and output-path mismatches, not hangs.
